//! The WS-Print backend through the public registry API, against the in-process fake printer
//! (no sockets: the backend is given an in-memory transport) and, for the hostile cases, loopback
//! servers that play a misbehaving device.
#![cfg(all(feature = "wsd", not(target_arch = "wasm32")))]

mod wsd_support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use craft_print::backends::Backend;
use craft_print::wsd::{PollPolicy, TicketShape, Transfer, WsdBackend};
use craft_print::{
    BackendClass, CancelToken, Document, DocumentFormat, Duplex, ErrorKind, Job, JobHint, PrinterState, Registry, Scaling, SubmitRequest,
    UserConfirmed, namespaced_id,
};
use wsd_support::{Config, Ending, FakePrinter, MemTransport, printer_server, raw_response};

const URL: &str = "http://127.0.0.1:5357/print";
const PDF: &[u8] = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\r\n--not-a-boundary\r\n--\n1 0 obj\n<<>>\nendobj\ntrailer\n<<>>\n%%EOF\n";

fn quick() -> PollPolicy {
    PollPolicy { interval: Duration::from_millis(1), max_wait: Duration::from_secs(5) }
}

fn backend(p: &Arc<FakePrinter>) -> WsdBackend {
    WsdBackend::new().with_transport(MemTransport(p.clone())).with_poll(quick())
}

fn registry(backend: &WsdBackend) -> Registry {
    let registry = Registry::new();
    registry.register(Box::new(backend.clone())).unwrap();
    registry
}

fn id(url: &str) -> String {
    namespaced_id("wsd", url)
}

fn job(url: &str) -> Job {
    Job { printer: Some(id(url)), title: "Report".into(), ..Job::default() }
}

fn request<'a>(job: Job) -> SubmitRequest<'a> {
    SubmitRequest::new(Document::pdf(PDF), job).with_consent(UserConfirmed::interactive())
}

/// A backend over the fake with the printer added, and a registry that holds it.
fn setup(config: Config) -> (Arc<FakePrinter>, WsdBackend, Registry) {
    let p = FakePrinter::new(config);
    let b = backend(&p);
    b.add(URL).unwrap();
    let r = registry(&b);
    (p, b, r)
}

// ---- identity, listing, policy ----

#[test]
fn the_backend_is_a_network_backend_named_wsd_and_lists_only_what_was_added() {
    let b = WsdBackend::new();
    assert_eq!((b.name(), b.class()), ("wsd", BackendClass::Network));
    assert!(b.accepted_formats().contains(&DocumentFormat::Pdf) && b.accepted_formats().contains(&DocumentFormat::PwgRaster));
    assert!(b.list_printers().unwrap().is_empty(), "nothing is listed until the app adds a printer");
    let r = registry(&b);
    assert!(r.list_printers().unwrap().items.is_empty());
    let native = b.add_named(URL, "Front office").unwrap();
    assert_eq!(native, URL);
    b.add("http://127.0.0.1:5358/print").unwrap();
    assert_eq!(b.add(URL).unwrap(), URL, "adding twice keeps one entry");
    let listing = r.list_printers().unwrap();
    assert!(listing.is_complete());
    let ids: Vec<_> = listing.items.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["wsd:http://127.0.0.1:5357/print", "wsd:http://127.0.0.1:5358/print"]);
    assert_eq!(listing.items[0].name, "Front office");
    assert_eq!(listing.items[1].name, "http://127.0.0.1:5358/print");
    assert!(listing.items.iter().all(|p| !p.default && p.two_sided.is_none()));
    assert!(b.remove(URL) && !b.remove(URL) && !b.remove("not a url"));
    assert_eq!(b.urls(), ["http://127.0.0.1:5358/print"]);
}

#[test]
fn building_listing_and_adding_open_no_connection() {
    let p = FakePrinter::new(Config::default());
    let server = printer_server(p.clone());
    let b = WsdBackend::with_printers([server.url("/print")]).unwrap();
    let r = registry(&b);
    r.list_printers().unwrap();
    b.add(&server.url("/other")).unwrap();
    r.list_printers().unwrap();
    assert_eq!(*server.hits.lock().unwrap(), 0);
    assert!(p.log().is_empty());
}

#[test]
fn printer_urls_go_through_the_policy() {
    let b = WsdBackend::new();
    for bad in [
        "http://printer.example/print",
        "http://localhost:5357/print",
        "https://127.0.0.1/print",
        "http://192.0.2.10:5357/print",
        "http://198.51.100.20/print",
        "http://127.0.0.1:5357/print?x=1",
        "http://user@127.0.0.1/print",
        "127.0.0.1",
        "",
    ] {
        let e = b.add(bad).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::InvalidRequest, "{bad}");
        assert!(!e.is_retryable());
    }
    assert!(b.urls().is_empty());
    assert!(WsdBackend::with_printers([URL, "http://printer.example/print"]).is_err());
    assert_eq!(WsdBackend::with_printers(["http://10.0.0.5/print", "http://[::1]:5357/print"]).unwrap().urls().len(), 2);
}

#[test]
fn only_added_printers_can_be_reached() {
    let (p, _b, r) = setup(Config::default());
    // A valid URL that was never added: not found, and nothing was sent.
    let other = "http://127.0.0.1:5999/print";
    assert_eq!(r.printer_details(&id(other)).unwrap_err().kind(), ErrorKind::NotFound);
    assert_eq!(r.submit(request(job(other))).unwrap_err().kind(), ErrorKind::NotFound);
    // A URL the policy refuses is refused before it is looked up.
    let e = r.submit(request(job("http://printer.example/print"))).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::InvalidRequest);
    // There is no default printer.
    let e = r.submit(request(Job { printer: None, ..Job::default() })).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::NotFound);
    assert!(p.log().is_empty());
    // A backend without the wsd prefix id is not guessed.
    assert_eq!(r.submit(request(Job { printer: Some(URL.into()), ..Job::default() })).unwrap_err().kind(), ErrorKind::UnknownBackend);
}

// ---- details ----

#[test]
fn details_map_the_printers_capabilities_and_status() {
    let config = Config {
        formats: vec!["application/pdf", "image/pwg-raster", "application/vnd.vendor-pcl", "APPLICATION/POSTSCRIPT", "application/pdf"],
        sides: vec!["OneSided", "TwoSidedLongEdge"],
        max_copies: 25,
        color: true,
        state: "Stopped",
        state_reasons: vec!["MediaEmpty", "DoorOpen"],
        media: vec!["iso_a4_210x297mm", "na_letter_8.5x11in"],
        ..Config::default()
    };
    let (p, _b, r) = setup(config);
    let d = r.printer_details(&id(URL)).unwrap();
    assert_eq!(d.id, "wsd:http://127.0.0.1:5357/print");
    assert_eq!(d.name, "Fake Printer");
    assert!(!d.default);
    assert_eq!(
        d.formats,
        [DocumentFormat::Pdf, DocumentFormat::PwgRaster, DocumentFormat::PostScript],
        "listed order, unknown formats left out, no duplicates"
    );
    assert_eq!((d.two_sided, d.colour, d.max_copies), (Some(true), Some(true), Some(25)));
    assert_eq!(d.paper, ["iso_a4_210x297mm", "na_letter_8.5x11in"]);
    assert_eq!(d.honoured, Some(vec![JobHint::Copies, JobHint::Duplex, JobHint::Colour]));
    let status = d.status.unwrap();
    assert_eq!(status.state, PrinterState::Stopped);
    assert_eq!(status.reasons, ["MediaEmpty", "DoorOpen"]);
    assert!(d.options.is_empty());
    assert_eq!(p.actions(), ["GetPrinterElements"], "details cost one round trip and create no job");
    assert_eq!(p.jobs(), 0);
}

#[test]
fn details_say_unknown_when_the_printer_does_not() {
    let (_p, _b, r) = setup(Config { sides: vec!["OneSided"], color: false, state: "VendorBusy", ..Config::default() });
    let d = r.printer_details(&id(URL)).unwrap();
    assert_eq!((d.two_sided, d.colour), (Some(false), Some(false)));
    assert!(d.status.is_none(), "a vendor state is not guessed");
    let (_p, _b, r) = setup(Config { sides: vec![], ..Config::default() });
    assert_eq!(r.printer_details(&id(URL)).unwrap().two_sided, None);
}

// ---- routing, consent, formats ----

#[test]
fn a_job_prints_through_the_registry_and_the_receipt_names_it() {
    let (p, _b, r) = setup(Config::default());
    let phases = Mutex::new(Vec::new());
    let mut record = |done: usize, total: usize| phases.lock().unwrap().push((done, total));
    let receipt = r.submit(request(Job { copies: 2, ..job(URL) }).with_progress(&mut record)).unwrap();
    assert_eq!(receipt.backend, "wsd");
    assert_eq!(receipt.job_id.as_deref(), Some("1"));
    assert!(receipt.message.contains("completed") && receipt.message.contains("127.0.0.1:5357"), "{}", receipt.message);
    assert_eq!(*phases.lock().unwrap(), [(1, 4), (2, 4), (3, 4), (4, 4)]);
    let job = p.job(1).unwrap();
    assert_eq!(job.document.as_deref(), Some(PDF), "the bytes arrive intact");
    assert_eq!(job.format.as_deref(), Some("application/pdf"));
    assert!(job.ticket.contains("Report") && job.ticket.contains(">2</"), "{}", job.ticket);
    let seq = p.actions();
    assert_eq!(&seq[..3], ["GetPrinterElements", "CreatePrintJob", "SendDocument"]);
    assert!(seq[3..].iter().all(|a| a == "GetJobElements"));
}

#[test]
fn network_jobs_need_consent_and_nothing_is_sent_without_it() {
    let (p, _b, r) = setup(Config::default());
    let e = r.submit(SubmitRequest::new(Document::pdf(PDF), job(URL))).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::ConsentRequired);
    assert!(p.log().is_empty());
    assert_eq!(UserConfirmed::attest(None).unwrap_err().kind(), ErrorKind::ConsentRequired);
    // A caller holding the backend directly gets the same answer.
    let direct = backend(&p);
    direct.add(URL).unwrap();
    let e = direct.submit(SubmitRequest::new(Document::pdf(PDF), Job { printer: Some(URL.into()), ..Job::default() })).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::ConsentRequired);
    assert!(p.log().is_empty());
    // The attested path works.
    let ok = SubmitRequest::new(Document::pdf(PDF), job(URL)).with_consent(UserConfirmed::attest(Some(true)).unwrap());
    assert!(r.submit(ok).is_ok());
}

#[test]
fn a_format_the_printer_does_not_list_is_refused_before_a_job_exists() {
    let (p, _b, r) = setup(Config::default());
    let ps = SubmitRequest::new(Document::new(DocumentFormat::PostScript, b"%!PS\n"), job(URL)).with_consent(UserConfirmed::interactive());
    let e = r.submit(ps).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::UnsupportedFormat);
    assert!(!e.is_retryable());
    assert_eq!((p.jobs(), p.actions()), (0, vec!["GetPrinterElements".to_owned()]));
    // The raster format the printer lists is sent as that format.
    let raster = SubmitRequest::new(Document::new(DocumentFormat::PwgRaster, b"RaS2...."), job(URL)).with_consent(UserConfirmed::interactive());
    let receipt = r.submit(raster).unwrap();
    assert_eq!(p.job(receipt.job_id.unwrap().parse().unwrap()).unwrap().format.as_deref(), Some("image/pwg-raster"));
}

#[test]
fn the_printers_own_spelling_of_the_format_is_used() {
    let (p, _b, r) = setup(Config { formats: vec!["Application/PDF"], ..Config::default() });
    r.submit(request(job(URL))).unwrap();
    assert_eq!(p.job(1).unwrap().format.as_deref(), Some("Application/PDF"));
}

#[test]
fn a_token_cancelled_before_the_job_starts_sends_nothing() {
    let (p, _b, r) = setup(Config::default());
    let token = CancelToken::new();
    token.cancel();
    let e = r.submit(request(job(URL)).with_cancel(token.clone())).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Cancelled);
    // The backend checks too, for callers that hold it directly.
    let direct = backend(&p);
    direct.add(URL).unwrap();
    let e = direct.submit(request(Job { printer: Some(URL.into()), ..Job::default() }).with_cancel(token)).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Cancelled);
    assert!(p.log().is_empty());
}

// ---- job settings and ticket shapes ----

#[test]
fn copies_duplex_and_colour_reach_the_ticket_and_the_rest_is_reported() {
    let (p, _b, r) = setup(Config::default());
    let j = Job {
        copies: 3,
        duplex: Some(Duplex::LongEdge),
        grayscale: true,
        scaling: Scaling::ShrinkToFit,
        options: vec![("Tray".into(), "2".into())],
        ..job(URL)
    };
    let receipt = r.submit(request(j)).unwrap();
    let t = p.job(1).unwrap().ticket;
    assert!(t.contains(">3</") && t.contains("TwoSidedLongEdge"), "{t}");
    assert!(receipt.message.contains("scaling") && receipt.message.contains("driver options"), "{}", receipt.message);
    // Simplex is forced with `Some(Off)`, and the defaults write neither copies nor sides.
    r.submit(request(Job { duplex: Some(Duplex::Off), ..job(URL) })).unwrap();
    let t2 = p.job(2).unwrap().ticket;
    assert!(t2.contains("OneSided") && !t2.contains("Copies"), "{t2}");
    r.submit(request(job(URL))).unwrap();
    let t3 = p.job(3).unwrap().ticket;
    assert!(!t3.contains("Copies") && !t3.contains("Sides"), "{t3}");
}

#[test]
fn settings_the_printer_does_not_list_are_lowered_with_a_warning_or_refused_in_strict_mode() {
    let (p, _b, r) = setup(Config { max_copies: 10, ..Config::default() });
    let receipt = r.submit(request(Job { copies: 50, duplex: Some(Duplex::ShortEdge), ..job(URL) })).unwrap();
    let t = p.job(1).unwrap().ticket;
    assert!(t.contains(">10</") && !t.contains("TwoSidedShortEdge"), "{t}");
    assert!(receipt.message.contains("warnings"), "{}", receipt.message);
    let p2 = FakePrinter::new(Config::default());
    let strict = backend(&p2).with_strict(true);
    strict.add(URL).unwrap();
    let e = registry(&strict).submit(request(Job { copies: 50, ..job(URL) })).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::InvalidRequest);
    assert_eq!(p2.jobs(), 0, "refused before CreatePrintJob");
    let ok = registry(&strict).submit(request(Job { copies: 3, ..job(URL) }));
    assert!(ok.is_ok());
    assert!(p2.job(1).unwrap().ticket.contains("MustHonor=\"true\""));
}

#[test]
fn a_reduced_ticket_refuses_settings_it_cannot_carry_by_default() {
    for shape in [TicketShape::Description, TicketShape::JobNameOnly, TicketShape::Empty] {
        let p = FakePrinter::new(Config::default());
        let b = backend(&p).with_ticket_shape(shape);
        b.add(URL).unwrap();
        let r = registry(&b);
        let e = r.submit(request(Job { copies: 2, ..job(URL) })).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::InvalidRequest, "{shape:?}");
        assert!(e.message().contains("copies"), "{}", e.message());
        let e = r.submit(request(Job { duplex: Some(Duplex::LongEdge), ..job(URL) })).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::InvalidRequest, "{shape:?}");
        assert_eq!(p.jobs(), 0, "{shape:?}: refused before anything is created");
        // A plain job needs no settings and goes through.
        r.submit(request(job(URL))).unwrap();
        assert_eq!(p.jobs(), 1);
    }
}

#[test]
fn a_reduced_ticket_can_drop_settings_with_a_warning() {
    let p = FakePrinter::new(Config::default());
    let b = backend(&p).with_ticket_shape(TicketShape::JobNameOnly).with_drop_ticket_settings(true);
    b.add(URL).unwrap();
    let receipt = registry(&b).submit(request(Job { copies: 2, duplex: Some(Duplex::LongEdge), ..job(URL) })).unwrap();
    assert!(receipt.message.contains("leaves out copies") && receipt.message.contains("leaves out sides"), "{}", receipt.message);
    let t = p.job(1).unwrap().ticket;
    assert!(
        t.contains("Report") && !t.contains("JobOriginatingUserName") && !t.contains("JobProcessing") && !t.contains("DocumentProcessing"),
        "{t}"
    );
}

#[test]
fn the_default_ticket_is_the_full_one_and_the_shapes_differ() {
    let ticket_for = |shape: Option<TicketShape>| {
        let p = FakePrinter::new(Config::default());
        let mut b = backend(&p);
        if let Some(shape) = shape {
            b = b.with_ticket_shape(shape);
        }
        b.add(URL).unwrap();
        registry(&b).submit(request(job(URL))).unwrap();
        p.job(1).unwrap().ticket
    };
    let default = ticket_for(None);
    assert_eq!(default, ticket_for(Some(TicketShape::Full)));
    assert!(default.contains("JobOriginatingUserName") && default.contains(">craft<"), "{default}");
    assert!(ticket_for(Some(TicketShape::Description)).contains("JobOriginatingUserName"));
    let name_only = ticket_for(Some(TicketShape::JobNameOnly));
    assert!(name_only.contains("Report") && !name_only.contains("JobOriginatingUserName"));
    assert!(!ticket_for(Some(TicketShape::Empty)).contains("JobDescription"));
    // The user name is configurable, and the transfer too.
    let p = FakePrinter::new(Config::default());
    let b = backend(&p).with_user_name("kiosk").with_transfer(Transfer::InlineBase64);
    b.add(URL).unwrap();
    registry(&b).submit(request(job(URL))).unwrap();
    assert!(p.job(1).unwrap().ticket.contains(">kiosk<"));
    assert!(p.log().iter().any(|l| l.action.ends_with("SendDocument") && l.content_type.starts_with("application/soap+xml")));
    assert_eq!(p.job(1).unwrap().document.as_deref(), Some(PDF));
}

// ---- cancellation and progress ----

#[test]
fn cancelling_after_the_job_is_created_cancels_it_on_the_printer_and_sends_no_document() {
    let (p, _b, r) = setup(Config { ending: Ending::Hangs, ..Config::default() });
    let token = CancelToken::new();
    let t = token.clone();
    let mut progress = move |done: usize, _| {
        if done == 2 {
            t.cancel();
        }
    };
    let e = r.submit(request(job(URL)).with_cancel(token).with_progress(&mut progress)).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Cancelled);
    assert_eq!(p.job(1).unwrap().state, "Canceled");
    assert!(!p.actions().iter().any(|a| a == "SendDocument"), "{:?}", p.actions());
}

#[test]
fn cancelling_while_the_job_runs_sends_cancel_job_once_and_reports_cancelled() {
    let (p, _b, r) = setup(Config { ending: Ending::Hangs, ..Config::default() });
    let token = CancelToken::new();
    let t = token.clone();
    let mut progress = move |done: usize, _| {
        if done == 3 {
            t.cancel();
        }
    };
    let e = r.submit(request(job(URL)).with_cancel(token).with_progress(&mut progress)).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Cancelled);
    assert!(!e.is_retryable());
    assert_eq!(p.job(1).unwrap().state, "Canceled");
    assert_eq!(p.actions().iter().filter(|a| *a == "CancelJob").count(), 1);
}

#[test]
fn a_slow_job_is_accepted_and_left_running() {
    let (p, _b, r) = setup(Config { ending: Ending::Hangs, ..Config::default() });
    let b = backend(&p).with_poll(PollPolicy { interval: Duration::from_millis(2), max_wait: Duration::from_millis(30) });
    b.add(URL).unwrap();
    let receipt = registry(&b).submit(request(job(URL))).unwrap();
    assert!(receipt.message.contains("still running"), "{}", receipt.message);
    assert_eq!(receipt.job_id.as_deref(), Some("1"));
    drop(r);
}

// ---- how a job can end ----

#[test]
fn job_end_states_map_to_receipts_and_errors() {
    let run = |ending| {
        let (_p, _b, r) = setup(Config { ending, ..Config::default() });
        r.submit(request(job(URL)))
    };
    let warn = run(Ending::Completes("JobCompletedWithWarnings")).unwrap();
    assert!(warn.message.contains("warnings"), "{}", warn.message);
    let errors = run(Ending::Completes("JobCompletedWithErrors")).unwrap_err();
    assert_eq!(errors.kind(), ErrorKind::Failed);
    let aborted = run(Ending::Aborts("DocumentFormatError")).unwrap_err();
    assert_eq!(aborted.kind(), ErrorKind::UnsupportedFormat);
    assert!(aborted.message().contains("DocumentFormatError"), "{}", aborted.message());
    assert_eq!(run(Ending::Aborts("PrinterStopped")).unwrap_err().kind(), ErrorKind::Unavailable);
    let other = run(Ending::Aborts("JobPasswordWait")).unwrap_err();
    assert_eq!((other.kind(), other.is_retryable()), (ErrorKind::Rejected, false));
    let gone = run(Ending::Vanishes).unwrap();
    assert!(gone.message.contains("no longer listed"), "{}", gone.message);
}

#[test]
fn faults_map_to_neutral_kinds_and_keep_their_code() {
    let (_p, _b, r) = setup(Config { accept_jobs: false, ..Config::default() });
    let e = r.submit(request(job(URL))).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Busy);
    assert!(e.is_retryable());
    let n = e.native().unwrap();
    assert_eq!((n.domain, n.name), ("wsd-fault", Some("ServerErrorNotAcceptingJobs")));
    assert_eq!(n.code, 5);
    let (p, _b, r) = setup(Config { fail_send: true, ..Config::default() });
    let e = r.submit(request(job(URL))).unwrap_err();
    assert_eq!((e.kind(), e.is_retryable()), (ErrorKind::Failed, false));
    assert_eq!(e.native().unwrap().name, Some("ServerErrorInternalError"));
    assert_eq!(p.job(1).unwrap().state, "Canceled", "a failed send cancels the orphan job");
    // A fault on details too.
    let (_p, _b, r) = setup(Config { http_error: Some(503), ..Config::default() });
    let e = r.printer_details(&id(URL)).unwrap_err();
    assert_eq!((e.kind(), e.is_retryable()), (ErrorKind::Unavailable, true));
}

// ---- hostile devices, over real loopback sockets ----

type Responder = Box<dyn Fn(&wsd_support::HttpRequest) -> Vec<u8> + Send>;

fn hostile(respond: impl Fn(&wsd_support::HttpRequest) -> Vec<u8> + Send + 'static) -> (wsd_support::Server, Registry) {
    let server = wsd_support::Server::start(respond);
    let b = WsdBackend::new().with_poll(quick());
    b.add(&server.url("/print")).unwrap();
    let r = registry(&b);
    (server, r)
}

fn details_error(respond: impl Fn(&wsd_support::HttpRequest) -> Vec<u8> + Send + 'static) -> craft_print::PrintError {
    let (server, r) = hostile(respond);
    r.printer_details(&id(&server.url("/print"))).unwrap_err()
}

#[test]
fn hostile_replies_are_typed_errors_and_never_a_panic() {
    let soap = "application/soap+xml";
    let cases: Vec<(&str, Responder, ErrorKind)> = vec![
        ("html 503", Box::new(|_| raw_response(503, "text/html", b"<html><body>maintenance</body></html>")), ErrorKind::Unavailable),
        ("html 500 with a doctype", Box::new(|_| raw_response(500, "text/html", b"\n<!DOCTYPE html><html><body>bad</body>")), ErrorKind::Unavailable),
        ("429", Box::new(|_| raw_response(429, "text/plain", b"slow down")), ErrorKind::Busy),
        ("404", Box::new(|_| raw_response(404, "text/plain", b"nope")), ErrorKind::Failed),
        ("html 200", Box::new(|_| raw_response(200, "text/html", b"<html><body>router login</body></html>")), ErrorKind::Failed),
        ("malformed xml", Box::new(move |_| raw_response(200, soap, b"<s:Envelope><s:Body>")), ErrorKind::Failed),
        ("empty 200", Box::new(move |_| raw_response(200, soap, b"")), ErrorKind::Failed),
        ("binary junk", Box::new(move |_| raw_response(200, soap, &[b'<', 0, 159, 146, 150, 0xff, 0xfe])), ErrorKind::Failed),
        (
            "wrong namespace",
            Box::new(move |_| {
                raw_response(
                    200,
                    soap,
                    b"<s:Envelope xmlns:s=\"urn:not-soap\"><s:Header/><s:Body><x:GetPrinterElementsResponse xmlns:x=\"urn:other\"/></s:Body></s:Envelope>",
                )
            }),
            ErrorKind::Failed,
        ),
        (
            "a DTD",
            Box::new(move |_| {
                raw_response(200, soap, b"<!DOCTYPE s [<!ENTITY a \"aaaa\">]><s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\"/>")
            }),
            ErrorKind::Failed,
        ),
        ("garbage multipart", Box::new(|_| raw_response(200, "multipart/related; boundary=b", b"<html>")), ErrorKind::Failed),
        ("not http at all", Box::new(|_| b"not http at all".to_vec()), ErrorKind::Unavailable),
        ("connection closed with no reply", Box::new(|_| Vec::new()), ErrorKind::Unavailable),
    ];
    for (what, respond, kind) in cases {
        let e = details_error(respond);
        assert_eq!(e.kind(), kind, "{what}: {e}");
        assert_eq!(e.is_retryable(), kind.is_retryable(), "{what}");
        assert!(!e.message().is_empty());
    }
}

#[test]
fn a_connection_closed_with_no_reply_is_retryable() {
    let e = details_error(|_| Vec::new());
    assert_eq!(e.kind(), ErrorKind::Unavailable);
    assert!(e.is_retryable());
}

#[test]
fn a_huge_reply_is_cut_off() {
    let server = wsd_support::Server::start(|_| raw_response(200, "application/soap+xml", &vec![b'<'; 3 << 20]));
    let b = WsdBackend::new().with_limits(craft_print::wsd::NetworkLimits { max_response: 64 << 10, ..Default::default() });
    b.add(&server.url("/print")).unwrap();
    let e = registry(&b).printer_details(&id(&server.url("/print"))).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Failed);
    assert!(e.message().contains("larger than the limit"), "{}", e.message());
}

#[test]
fn a_redirect_is_refused_not_followed() {
    let target = wsd_support::Server::start(|_| raw_response(200, "text/plain", b"never reached"));
    let location = target.url("/elsewhere");
    let (server, r) =
        hostile(move |_| format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes());
    let e = r.printer_details(&id(&server.url("/print"))).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Unavailable);
    assert_eq!(*target.hits.lock().unwrap(), 0, "the redirect target was never contacted");
}

#[test]
fn a_silent_printer_times_out() {
    let server = wsd_support::Server::start(|_| {
        std::thread::sleep(Duration::from_millis(1500));
        Vec::new()
    });
    let b = WsdBackend::new().with_limits(craft_print::wsd::NetworkLimits { timeout: Duration::from_millis(300), ..Default::default() });
    b.add(&server.url("/print")).unwrap();
    let started = std::time::Instant::now();
    let e = registry(&b).printer_details(&id(&server.url("/print"))).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Unavailable);
    assert!(started.elapsed() < Duration::from_millis(1400), "{:?}", started.elapsed());
}

#[test]
fn a_hostile_reply_in_the_middle_of_a_job_leaves_a_typed_error() {
    // The printer answers discovery and job creation properly and then goes bad on SendDocument.
    let p = FakePrinter::new(Config::default());
    let server = wsd_support::Server::start(move |req| {
        let ct = req.header("content-type").unwrap_or("");
        let action = req.header("soapaction").unwrap_or("").to_owned();
        if action.contains("SendDocument") {
            return raw_response(502, "text/html", b"<html>bad gateway</html>");
        }
        let (status, ct, body) = p.handle(ct, &req.body);
        raw_response(status, &ct, &body)
    });
    let b = WsdBackend::new().with_poll(quick());
    b.add(&server.url("/print")).unwrap();
    let e = registry(&b).submit(request(job(&server.url("/print")))).unwrap_err();
    assert_eq!((e.kind(), e.is_retryable()), (ErrorKind::Unavailable, true), "{e}");
}

#[test]
fn a_full_print_over_a_loopback_socket() {
    let printer = FakePrinter::new(Config::default());
    let server = printer_server(printer.clone());
    let b = WsdBackend::new().with_poll(quick());
    b.add(&server.url("/print")).unwrap();
    let r = registry(&b);
    let pdf = b"%PDF-1.4\n\r\n--x\r\n binary \xff\x00\xfe\n%%EOF".to_vec();
    let receipt = r.submit(SubmitRequest::new(Document::pdf(&pdf), job(&server.url("/print"))).with_consent(UserConfirmed::interactive())).unwrap();
    assert!(receipt.message.contains("completed"));
    assert_eq!(printer.job(1).unwrap().document, Some(pdf));
    let d = r.printer_details(&id(&server.url("/print"))).unwrap();
    assert_eq!(d.name, "Fake Printer");
}
