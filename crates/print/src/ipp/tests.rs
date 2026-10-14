//! Printer-free tests of the IPP backend: static byte vectors, request builders checked against
//! expected bytes, canned replies through a small scripted transport (a mock transport, not a fake
//! printer: it answers exactly the replies a test queues), and registry-level routing.
//! Tests against a real IPP server live in the pdfcraft-fakeprint harness, outside this repository.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::adapter::{DeviceUrl, Request, Response, Stage, Transport, TransportKind, WireError};
use super::client::Client;
use super::codec::{Attribute, DateTime, Group, GroupTag, Limits, Message, Value};
use super::model::PrinterAttrs;
use super::policy::PrinterUri;
use super::protocol::Operation;
use super::{IppBackend, JobFlow};
use crate::backends::Backend;
use crate::{
    BackendClass, BytesSource, CancelToken, ConvertContext, Document, DocumentConverter, DocumentFormat, Duplex, ErrorKind, Fingerprint, Job,
    JobFilter, JobOptions, JobState, LifecycleState, ManualClock, Outcome, OwnedDocument, Phase, PrintError, PrintJob, Quality, Reconciled, Registry,
    RetryAdvice, RetrySafety, SubmitRequest, Tag, TagExt as _, TagPlacement, TagPolicy, Timestamp, UserConfirmed, WatchSupport,
};

const URI: &str = "ipp://127.0.0.1:631/ipp/print";
const PDF: &[u8] = b"%PDF-1.7\n1 0 obj\n<<>>\nendobj\ntrailer\n<<>>\n%%EOF\n";

// ---- a scripted transport ----------------------------------------------------------------

type Responder = Box<dyn Fn(&Message) -> Message + Send + Sync>;

enum Step {
    Reply(Responder),
    Raw(Vec<u8>),
    Http(u16, Option<Duration>),
    Fail(TransportKind, Stage),
}

#[derive(Clone, Default)]
struct Script {
    steps: Arc<Mutex<VecDeque<Step>>>,
    seen: Arc<Mutex<Vec<Message>>>,
}

impl Script {
    fn new() -> Self {
        Self::default()
    }

    fn push(&self, step: Step) -> &Self {
        self.steps.lock().unwrap().push_back(step);
        self
    }

    fn ok(&self, groups: Vec<Group>) -> &Self {
        self.push(reply(0, groups))
    }

    fn status(&self, code: u16) -> &Self {
        self.push(reply(code, vec![]))
    }

    fn requests(&self) -> Vec<Message> {
        self.seen.lock().unwrap().clone()
    }

    fn ops(&self) -> Vec<u16> {
        self.requests().iter().map(|m| m.code).collect()
    }

    fn last(&self) -> Message {
        self.requests().last().cloned().unwrap()
    }

    fn pending(&self) -> usize {
        self.steps.lock().unwrap().len()
    }
}

impl Transport for Script {
    fn post(&self, url: &DeviceUrl, request: Request) -> Result<Response, WireError> {
        assert_eq!(request.content_type, "application/ipp");
        assert_eq!((url.port(), url.path()), (631, "/ipp/print"));
        let message = Message::decode(&request.body, &Limits::default().with_max_message(usize::MAX)).expect("the request is well-formed IPP");
        // What every conforming request starts with (RFC 8011 section 4.1.4).
        let op = message.group(GroupTag::Operation).expect("an operation group");
        assert_eq!(op.attributes[0].name, "attributes-charset");
        assert_eq!(op.attributes[1].name, "attributes-natural-language");
        assert_eq!(op.attributes[2].name, "printer-uri");
        assert_ne!(message.request_id, 0);
        self.seen.lock().unwrap().push(message.clone());
        let step = self.steps.lock().unwrap().pop_front().unwrap_or_else(|| panic!("no reply queued for operation {:#06x}", message.code));
        match step {
            Step::Reply(f) => Ok(Response::new(200, Some("application/ipp".into()), f(&message).encode().unwrap())),
            Step::Raw(bytes) => Ok(Response::new(200, Some("application/ipp".into()), bytes)),
            Step::Http(status, wait) => Ok(Response::new(status, None, Vec::new()).with_retry_after(wait)),
            Step::Fail(kind, stage) => Err(WireError::transport_at(kind, stage, "scripted")),
        }
    }
}

fn op_group() -> Group {
    Group::new(GroupTag::Operation)
        .with(Attribute::new("attributes-charset", Value::Charset("utf-8".into())))
        .with(Attribute::new("attributes-natural-language", Value::NaturalLanguage("en".into())))
}

fn reply(code: u16, groups: Vec<Group>) -> Step {
    Step::Reply(Box::new(move |request| {
        let mut message = Message::new(request.major, request.minor, code, request.request_id).with_group(op_group());
        message.groups.extend(groups.clone());
        message
    }))
}

fn reply_bytes(groups: Vec<Group>) -> Vec<u8> {
    let mut message = Message::new(1, 1, 0, 1).with_group(op_group());
    message.groups.extend(groups);
    message.encode().unwrap()
}

fn printer_group() -> Group {
    Group::new(GroupTag::Printer)
        .with(Attribute::name_value("printer-name", "Front desk"))
        .with(Attribute::enumeration("printer-state", 3))
        .with(Attribute::keyword("printer-state-reasons", "none"))
        .with(Attribute::boolean("printer-is-accepting-jobs", true))
        .with(Attribute::set("operations-supported", [2, 4, 5, 6, 8, 9, 10, 11].map(Value::Enum).to_vec()))
        .with(Attribute::set(
            "document-format-supported",
            ["application/octet-stream", "application/pdf", "image/pwg-raster", "image/jpeg", "image/urf"]
                .map(|m| Value::MimeMediaType(m.into()))
                .to_vec(),
        ))
        .with(Attribute::keywords("sides-supported", &["one-sided", "two-sided-long-edge", "two-sided-short-edge"]))
        .with(Attribute::keywords("print-color-mode-supported", &["auto", "color", "monochrome"]))
        .with(Attribute::keywords("media-supported", &["iso_a4_210x297mm", "na_letter_8.5x11in"]))
        .with(Attribute::new("copies-supported", Value::Range { lower: 1, upper: 99 }))
        .with(Attribute::new("printer-resolution-supported", Value::Resolution { cross_feed: 600, feed: 600, units: 3 }))
        .with(Attribute::set("print-quality-supported", vec![Value::Enum(3), Value::Enum(4), Value::Enum(5)]))
        .with(Attribute::new("multiple-operation-time-out", Value::Integer(120)))
        .with(Attribute::new("printer-up-time", Value::Integer(5000)))
}

fn jpeg_only_printer() -> Group {
    let mut g = printer_group();
    g.attributes.retain(|a| a.name != "document-format-supported");
    g.with(Attribute::set("document-format-supported", vec![Value::MimeMediaType("image/jpeg".into()), Value::MimeMediaType("image/urf".into())]))
}

fn job_group(id: i32, state: i32, reasons: &[&str], name: &str) -> Group {
    Group::new(GroupTag::Job)
        .with(Attribute::integer("job-id", id))
        .with(Attribute::enumeration("job-state", state))
        .with(Attribute::keywords("job-state-reasons", if reasons.is_empty() { &["none"] } else { reasons }))
        .with(Attribute::name_value("job-name", name))
        .with(Attribute::name_value("job-originating-user-name", "craft"))
}

fn unsupported_group(names: &[&str]) -> Group {
    names.iter().fold(Group::new(GroupTag::Unsupported), |g, n| g.with(Attribute::new(*n, Value::Unsupported)))
}

fn backend(script: &Script) -> IppBackend {
    let b = IppBackend::new().with_transport(script.clone()).with_poll_interval(Duration::from_millis(1));
    b.add(URI).unwrap();
    b
}

fn registry(b: &IppBackend) -> Registry {
    let r = Registry::new();
    r.register(Box::new(b.clone())).unwrap();
    r
}

fn id() -> String {
    format!("ipp:{URI}")
}

fn job() -> Job {
    Job { printer: Some(id()), title: "Report".into(), ..Job::default() }
}

fn request<'a>(job: Job) -> SubmitRequest<'a> {
    SubmitRequest::new(Document::pdf(PDF), job).with_consent(UserConfirmed::interactive())
}

fn accepted(id: i32) -> Vec<Group> {
    vec![job_group(id, 3, &["job-incoming"], "Report")]
}

// ---- identity, policy, no contact ----------------------------------------------------------

#[test]
fn the_backend_is_a_network_backend_named_ipp_and_lists_only_what_was_added() {
    let script = Script::new();
    let b = IppBackend::new().with_transport(script.clone());
    assert_eq!((b.name(), b.class()), ("ipp", BackendClass::Network));
    assert!(b.formats_per_printer());
    assert!(b.accepted_formats().contains(&DocumentFormat::Pdf) && b.accepted_formats().contains(&DocumentFormat::Jpeg));
    assert!(b.list_printers().unwrap().is_empty());
    assert_eq!(b.add_named("http://127.0.0.1/ipp/print", "Front desk").unwrap(), URI);
    assert_eq!(b.add(URI).unwrap(), URI, "the two spellings are one printer");
    b.add("ipp://[::1]:8631/x").unwrap();
    let r = registry(&b);
    let listing = r.list_printers().unwrap();
    let ids: Vec<_> = listing.items.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["ipp:ipp://127.0.0.1:631/ipp/print", "ipp:ipp://[::1]:8631/x"]);
    assert_eq!(listing.items[0].name, "Front desk");
    let info = r.backend_info(Some(&id())).unwrap();
    assert_eq!(info.tag_support.placement(), TagPlacement::JobNameSuffix);
    assert!(info.query_caps.list && info.query_caps.created_time && info.query_caps.user && info.query_caps.tag && info.query_caps.history);
    assert!(b.remove(URI) && !b.remove(URI));
    assert_eq!(b.urls(), ["ipp://[::1]:8631/x"]);
    assert!(script.requests().is_empty(), "nothing connects until asked");
}

#[test]
fn printer_uris_go_through_the_policy_and_tls_is_refused_plainly() {
    let b = IppBackend::new();
    for bad in ["ipp://printer.example/ipp", "ipp://localhost/", "http://8.8.8.8/ipp", "ipp://192.0.2.1/ipp", "ipp://u@127.0.0.1/", "127.0.0.1", ""] {
        let e = b.add(bad).unwrap_err();
        assert_eq!((e.kind(), e.is_retryable()), (ErrorKind::InvalidRequest, false), "{bad}");
    }
    for tls in ["ipps://127.0.0.1/ipp/print", "https://10.0.0.5:443/ipp/print"] {
        let e = b.add(tls).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::InvalidRequest);
        assert!(e.message().contains("TLS"), "{e}");
    }
    assert!(b.urls().is_empty());
    assert!(IppBackend::with_printers([URI, "ipp://printer.example/"]).is_err());
}

#[test]
fn only_added_printers_can_be_reached_and_nothing_is_sent_for_the_others() {
    let script = Script::new();
    let b = backend(&script);
    let r = registry(&b);
    let other = "ipp:ipp://127.0.0.1:9999/ipp/print";
    assert_eq!(r.printer_details(other).unwrap_err().kind(), ErrorKind::NotFound);
    assert_eq!(r.submit_job(request(Job { printer: Some(other.into()), ..Job::default() })).unwrap_err().kind(), ErrorKind::NotFound);
    assert_eq!(r.submit_job(request(Job { printer: None, ..Job::default() })).unwrap_err().kind(), ErrorKind::NotFound);
    assert_eq!(
        r.submit_job(request(Job { printer: Some("127.0.0.1:631/x".into()), ..Job::default() })).unwrap_err().kind(),
        ErrorKind::UnknownBackend
    );
    let no_consent = SubmitRequest::new(Document::pdf(PDF), job());
    assert_eq!(r.submit_job(no_consent).unwrap_err().kind(), ErrorKind::ConsentRequired);
    assert!(script.requests().is_empty());
    // The backend itself checks consent too.
    let direct = SubmitRequest::new(Document::pdf(PDF), Job { printer: Some(URI.into()), ..Job::default() });
    assert_eq!(b.submit(direct).unwrap_err().kind(), ErrorKind::ConsentRequired);
}

// ---- request bytes ---------------------------------------------------------------------------

#[test]
fn get_printer_attributes_is_exactly_the_rfc_8011_request() {
    let client = Client::new(Script::new(), PrinterUri::parse(URI).unwrap());
    let message = client.request(Operation::GetPrinterAttributes, None, vec![Attribute::keywords("requested-attributes", &["printer-name"])]);
    let mut want: Vec<u8> = vec![1, 1, 0x00, 0x0B, 0, 0, 0, 1, 0x01];
    let mut push = |tag: u8, name: &str, value: &[u8]| {
        want.push(tag);
        want.extend_from_slice(&(name.len() as u16).to_be_bytes());
        want.extend_from_slice(name.as_bytes());
        want.extend_from_slice(&(value.len() as u16).to_be_bytes());
        want.extend_from_slice(value);
    };
    push(0x47, "attributes-charset", b"utf-8");
    push(0x48, "attributes-natural-language", b"en");
    push(0x45, "printer-uri", URI.as_bytes());
    push(0x42, "requesting-user-name", b"craft");
    push(0x44, "requested-attributes", b"printer-name");
    want.push(0x03);
    assert_eq!(message.encode().unwrap(), want);
}

#[test]
fn request_builders_put_the_attributes_in_the_groups_the_rfc_says() {
    let client = Client::new(Script::new(), PrinterUri::parse(URI).unwrap()).with_user("alice");
    let req = super::client::JobRequest {
        job_name: "Report [ref abc12345]".into(),
        document_name: Some("doc".into()),
        document_format: Some("application/pdf".into()),
        fidelity: true,
        attributes: vec![Attribute::integer("copies", 2), Attribute::keyword("sides", "one-sided")],
    };
    let print = client.print_job_message(&req, b"DATA");
    assert_eq!(print.code, 0x0002);
    let op = print.group(GroupTag::Operation).unwrap();
    let names: Vec<_> = op.attributes.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "attributes-charset",
            "attributes-natural-language",
            "printer-uri",
            "requesting-user-name",
            "job-name",
            "ipp-attribute-fidelity",
            "document-name",
            "document-format"
        ]
    );
    assert_eq!(op.first_str("requesting-user-name"), Some("alice"));
    assert_eq!(op.get("document-format").unwrap().values, [Value::MimeMediaType("application/pdf".into())]);
    assert_eq!(print.group(GroupTag::Job).unwrap().attributes.len(), 2);
    assert_eq!(print.data, b"DATA");
    assert_eq!((print.groups[0].tag, print.groups[1].tag), (GroupTag::Operation, GroupTag::Job));

    let create = client.create_job_message(&req);
    assert_eq!(create.code, 0x0005);
    assert!(create.group(GroupTag::Operation).unwrap().get("document-format").is_none() && create.data.is_empty());
    let validate = client.validate_job_message(&req);
    assert_eq!(validate.code, 0x0004);
    assert!(validate.data.is_empty());

    let send = client.send_document_message(7, Some("image/jpeg"), true, b"JPEG");
    assert_eq!(send.code, 0x0006);
    let op = send.group(GroupTag::Operation).unwrap();
    assert_eq!(op.first_i32("job-id"), Some(7));
    assert_eq!(op.first_bool("last-document"), Some(true));
    assert_eq!(op.first_str("document-format"), Some("image/jpeg"));
    assert_eq!(send.data, b"JPEG");
    assert!(send.group(GroupTag::Job).is_none());

    // The request ids count up and are never zero.
    let a = client.request(Operation::CancelJob, Some(3), vec![]).request_id;
    let b = client.request(Operation::CancelJob, Some(3), vec![]).request_id;
    assert_eq!(b, a + 1);
}

// ---- details -------------------------------------------------------------------------------------

#[test]
fn printer_details_are_read_from_get_printer_attributes() {
    let script = Script::new();
    let b = backend(&script);
    script.ok(vec![printer_group().with(Attribute::set(
        "media-size-supported",
        vec![Value::Collection(vec![("x-dimension".into(), vec![Value::Integer(10000)]), ("y-dimension".into(), vec![Value::Integer(15000)])])],
    ))]);
    let d = registry(&b).printer_details(&id()).unwrap();
    assert_eq!(script.ops(), [0x000B]);
    assert!(script.last().group(GroupTag::Operation).unwrap().strings("requested-attributes").len() > 10);
    assert_eq!((d.id.as_str(), d.name.as_str()), (id().as_str(), "Front desk"));
    assert_eq!(d.formats, [DocumentFormat::OctetStream, DocumentFormat::Pdf, DocumentFormat::PwgRaster, DocumentFormat::Jpeg]);
    assert_eq!((d.two_sided, d.colour, d.max_copies), (Some(true), Some(true), Some(99)));
    assert_eq!(d.paper, ["iso_a4_210x297mm", "na_letter_8.5x11in", "custom_100x150mm"]);
    assert_eq!(d.resolutions, [crate::Resolution::new(600, 600)]);
    let status = d.status.unwrap();
    assert_eq!((status.state, status.reasons.len()), (crate::PrinterState::Idle, 0));
    let hints = d.honoured.unwrap();
    for hint in [
        crate::JobHint::Copies,
        crate::JobHint::Duplex,
        crate::JobHint::Media,
        crate::JobHint::Quality,
        crate::JobHint::Resolution,
        crate::JobHint::Orientation,
    ] {
        assert!(hints.contains(&hint), "{hint:?}");
    }
}

#[test]
fn a_stopped_printer_that_is_not_accepting_jobs_says_so() {
    let script = Script::new();
    let b = backend(&script);
    let mut g = printer_group();
    g.attributes.retain(|a| !matches!(a.name.as_str(), "printer-state" | "printer-state-reasons" | "printer-is-accepting-jobs"));
    script.ok(vec![
        g.with(Attribute::enumeration("printer-state", 5))
            .with(Attribute::keywords("printer-state-reasons", &["media-empty-error", "paused"]))
            .with(Attribute::boolean("printer-is-accepting-jobs", false)),
    ]);
    let status = registry(&b).printer_details(&id()).unwrap().status.unwrap();
    assert_eq!(status.state, crate::PrinterState::Stopped);
    assert_eq!(status.reasons, ["media-empty-error", "paused", "not-accepting-jobs"]);
}

// ---- submit ----------------------------------------------------------------------------------------

#[test]
fn a_document_is_submitted_with_print_job_and_the_job_is_named() {
    let script = Script::new();
    let b = backend(&script);
    script.ok(vec![printer_group()]).ok(accepted(7));
    let tag = Tag::random();
    let events = Mutex::new(Vec::new());
    let mut record = |p: crate::Progress| events.lock().unwrap().push(p);
    let job = Job { copies: 2, duplex: Some(Duplex::LongEdge), ..job() };
    let req = SubmitRequest::new(Document::pdf(PDF), job)
        .with_consent(UserConfirmed::interactive())
        .with_options(JobOptions::new().with_quality(Quality::High).with_media("iso_a4_210x297mm"))
        .with_tag(tag)
        .with_progress(&mut record);
    let submitted = registry(&b).submit_job(req).unwrap();
    assert_eq!(script.ops(), [0x000B, 0x0002]);
    assert_eq!(submitted.backend, "ipp");
    assert_eq!(submitted.job.as_ref().unwrap().to_string(), format!("ipp:{URI}#7"));
    assert_eq!(submitted.watch, WatchSupport::Poll);
    assert_eq!(submitted.poll_interval, Some(Duration::from_millis(1)));
    assert_eq!(submitted.tag, Some(tag));
    assert_eq!(submitted.status.as_ref().unwrap().state, JobState::Pending);
    assert!(submitted.warnings.is_empty(), "{:?}", submitted.warnings);
    assert!(submitted.effective.as_deref().unwrap().contains("print-job"));
    let sent = script.last();
    assert_eq!(sent.data, PDF);
    let op = sent.group(GroupTag::Operation).unwrap();
    assert_eq!(op.first_str("job-name"), Some(format!("Report [ref {tag}]").as_str()));
    assert_eq!(op.first_str("document-format"), Some("application/pdf"));
    assert!(op.get("ipp-attribute-fidelity").is_none());
    let template = sent.group(GroupTag::Job).unwrap();
    assert_eq!(template.first_i32("copies"), Some(2));
    assert_eq!(template.first_str("sides"), Some("two-sided-long-edge"));
    assert_eq!(template.first_i32("print-quality"), Some(5));
    assert_eq!(template.first_str("media"), Some("iso_a4_210x297mm"));
    assert_eq!(events.into_inner().unwrap(), [crate::Progress::of(1, 3), crate::Progress::of(3, 3)]);
}

#[test]
fn ignored_attributes_become_warnings_and_strict_mode_asks_for_fidelity() {
    let script = Script::new();
    let b = backend(&script);
    script.ok(vec![printer_group()]).push(reply(1, [accepted(8), vec![unsupported_group(&["print-quality"])]].concat()));
    let submitted = registry(&b).submit_job(request(job())).unwrap();
    assert!(submitted.warnings.iter().any(|w| w.contains("print-quality")), "{:?}", submitted.warnings);
    assert!(submitted.message.contains("warnings"));

    let strict = backend(&script).with_strict(true);
    script.ok(vec![printer_group()]).ok(accepted(9));
    registry(&strict).submit_job(request(job())).unwrap();
    assert_eq!(script.last().group(GroupTag::Operation).unwrap().first_bool("ipp-attribute-fidelity"), Some(true));
    // A setting the printer lists as unsupported is refused before the job goes out.
    script.ok(vec![printer_group()]);
    let before = script.requests().len();
    let e = registry(&strict).submit_job(request(Job { copies: 500, ..job() })).unwrap_err();
    assert_eq!((e.kind(), e.retry_safety()), (ErrorKind::InvalidRequest, RetrySafety::Safe));
    assert_eq!(script.requests().len(), before + 1, "only the attributes were read");
}

#[test]
fn unlisted_settings_are_dropped_with_a_warning_in_lenient_mode() {
    let script = Script::new();
    let b = backend(&script);
    script.ok(vec![printer_group()]).ok(accepted(2));
    let submitted = registry(&b).submit_job(request(Job { copies: 500, ..job() })).unwrap();
    assert!(submitted.warnings.iter().any(|w| w.contains("lowered to 99")), "{:?}", submitted.warnings);
    assert_eq!(script.last().group(GroupTag::Job).unwrap().first_i32("copies"), Some(99));
}

#[test]
fn a_big_document_goes_through_create_job_and_send_document() {
    let script = Script::new();
    let b = backend(&script).with_large_document(10);
    script.ok(vec![printer_group()]).ok(accepted(11)).ok(vec![job_group(11, 3, &[], "Report")]);
    let events = Mutex::new(Vec::new());
    let mut record = |p: crate::Progress| events.lock().unwrap().push(p);
    let submitted = registry(&b).submit_job(request(job()).with_progress(&mut record)).unwrap();
    assert_eq!(script.ops(), [0x000B, 0x0005, 0x0006]);
    let requests = script.requests();
    let create = &requests[1];
    assert!(create.data.is_empty() && create.group(GroupTag::Operation).unwrap().get("document-format").is_none());
    let send = &requests[2];
    let op = send.group(GroupTag::Operation).unwrap();
    assert_eq!(
        (op.first_i32("job-id"), op.first_bool("last-document"), op.first_str("document-format")),
        (Some(11), Some(true), Some("application/pdf"))
    );
    assert_eq!(send.data, PDF);
    assert_eq!(submitted.job.unwrap().to_string(), format!("ipp:{URI}#11"));
    assert!(submitted.effective.unwrap().contains("create-job"));
    assert_eq!(events.into_inner().unwrap(), [crate::Progress::of(1, 3), crate::Progress::of(2, 3), crate::Progress::of(3, 3)]);
}

#[test]
fn the_flow_follows_the_setting_and_the_operations_the_printer_lists() {
    let script = Script::new();
    let forced = backend(&script).with_job_flow(JobFlow::CreateAndSend);
    script.ok(vec![printer_group()]).ok(accepted(1)).ok(vec![]);
    registry(&forced).submit_job(request(job())).unwrap();
    assert_eq!(script.ops(), [0x000B, 0x0005, 0x0006]);
    let plain = backend(&script).with_job_flow(JobFlow::PrintJob).with_large_document(1);
    script.ok(vec![printer_group()]).ok(accepted(2));
    registry(&plain).submit_job(request(job())).unwrap();
    assert_eq!(&script.ops()[3..], [0x000B, 0x0002]);
    // A printer that does not list Print-Job gets Create-Job and Send-Document in Auto mode.
    let auto = backend(&script);
    let mut g = printer_group();
    g.attributes.retain(|a| a.name != "operations-supported");
    script.ok(vec![g.with(Attribute::set("operations-supported", [5, 6, 8, 9, 10, 11].map(Value::Enum).to_vec()))]).ok(accepted(3)).ok(vec![]);
    registry(&auto).submit_job(request(job())).unwrap();
    assert_eq!(&script.ops()[5..], [0x000B, 0x0005, 0x0006]);
    // Validate first adds one request, and what it would change comes back as warnings.
    let validating = backend(&script).with_validate_first(true);
    script.ok(vec![printer_group()]).push(reply(1, vec![unsupported_group(&["sides"])])).ok(accepted(4));
    let s = registry(&validating).submit_job(request(job())).unwrap();
    assert_eq!(&script.ops()[8..], [0x000B, 0x0004, 0x0002]);
    assert!(s.warnings.iter().any(|w| w.contains("sides")));
    assert_eq!(script.pending(), 0);
}

#[test]
fn a_refused_send_document_removes_the_half_made_job_but_a_lost_reply_leaves_it() {
    let script = Script::new();
    let b = backend(&script).with_job_flow(JobFlow::CreateAndSend);
    let r = registry(&b);
    script.ok(vec![printer_group()]).ok(accepted(5)).status(0x040A).ok(vec![]);
    let e = r.submit_job(request(job())).unwrap_err();
    assert_eq!(script.ops(), [0x000B, 0x0005, 0x0006, 0x0008], "the cleanup cancel");
    assert_eq!(script.last().group(GroupTag::Operation).unwrap().first_i32("job-id"), Some(5));
    assert_eq!((e.kind(), e.retry_safety()), (ErrorKind::UnsupportedFormat, RetrySafety::Safe));

    script.ok(vec![printer_group()]).ok(accepted(6)).push(Step::Fail(TransportKind::ConnectionClosed, Stage::Reply));
    let e = r.submit_job(request(job())).unwrap_err();
    assert_eq!(&script.ops()[4..], [0x000B, 0x0005, 0x0006], "no cancel: the document may have arrived");
    assert!(e.outcome_unknown());
    assert_eq!(e.phase(), Some(Phase::Reply));
}

#[test]
fn a_format_the_printer_does_not_list_is_refused_before_the_job_is_sent() {
    let script = Script::new();
    let b = backend(&script);
    script.ok(vec![printer_group()]);
    let doc = Document::new(DocumentFormat::Png, b"\x89PNG\r\n\x1a\n");
    let e = b
        .submit(SubmitRequest::new(doc, Job { printer: Some(URI.into()), ..Job::default() }).with_consent(UserConfirmed::interactive()))
        .unwrap_err();
    assert_eq!((e.kind(), e.retry_safety(), e.phase()), (ErrorKind::UnsupportedFormat, RetrySafety::Safe, Some(Phase::Prepare)));
    assert_eq!(script.ops(), [0x000B]);
}

#[test]
fn a_cancelled_token_stops_before_anything_is_sent() {
    let script = Script::new();
    let b = backend(&script);
    let token = CancelToken::new();
    token.cancel();
    let e = b.submit(request(Job { printer: Some(URI.into()), ..job() }).with_cancel(token)).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Cancelled);
    assert!(script.requests().is_empty());
}

// ---- conversion through the registry ----------------------------------------------------------------

struct ToImage;

impl DocumentConverter for ToImage {
    fn targets(&self, from: DocumentFormat) -> Vec<DocumentFormat> {
        if from == DocumentFormat::Pdf { vec![DocumentFormat::Jpeg, DocumentFormat::PwgRaster] } else { Vec::new() }
    }

    fn convert(&self, document: Document<'_>, target: DocumentFormat, _: &ConvertContext) -> Result<OwnedDocument, PrintError> {
        let mut bytes = format!("<{}>", target.mime()).into_bytes();
        bytes.extend_from_slice(document.bytes);
        Ok(OwnedDocument::new(target, bytes))
    }
}

#[test]
fn a_document_the_printer_cannot_take_is_converted_to_a_format_it_lists() {
    let script = Script::new();
    let b = backend(&script);
    let r = registry(&b);
    r.set_converter(Arc::new(ToImage));
    // The registry reads the details to decide, the converter runs, the backend reads them again.
    script.ok(vec![jpeg_only_printer()]).ok(vec![jpeg_only_printer()]).ok(accepted(3));
    let submitted = r.submit_job(request(job())).unwrap();
    let conversion = submitted.conversion.unwrap();
    assert_eq!((conversion.from, conversion.to), (DocumentFormat::Pdf, DocumentFormat::Jpeg));
    let sent = script.last();
    assert_eq!(sent.group(GroupTag::Operation).unwrap().first_str("document-format"), Some("image/jpeg"));
    assert!(sent.data.starts_with(b"<image/jpeg>%PDF"));

    // Without a converter the same printer refuses the PDF with a typed error.
    r.clear_converter();
    script.ok(vec![jpeg_only_printer()]);
    let e = r.submit_job(request(job())).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::UnsupportedFormat);
}

// ---- job status and cancel -----------------------------------------------------------------------------

fn status_of(group: Group) -> Result<crate::JobStatus, PrintError> {
    let script = Script::new();
    let b = backend(&script);
    script.ok(vec![group]);
    registry(&b).job_status(&IppBackend::job_id(URI, 7).unwrap())
}

#[test]
fn job_status_maps_states_reasons_and_counters() {
    let counters = |g: Group| {
        g.with(Attribute::integer("job-impressions", 4))
            .with(Attribute::integer("job-impressions-completed", 3))
            .with(Attribute::integer("job-media-sheets", 2))
            .with(Attribute::integer("job-media-sheets-completed", 1))
    };
    let s = status_of(counters(job_group(7, 5, &["job-printing"], "x"))).unwrap();
    assert_eq!(s.state, JobState::Processing);
    assert_eq!((s.pages_done, s.pages_total, s.sheets_done, s.sheets_total), (Some(3), Some(4), Some(1), Some(2)));
    assert_eq!(s.reasons, ["job-printing"]);
    for (state, reasons, want) in [
        (3, vec!["job-incoming"], JobState::Pending),
        (3, vec!["job-hold-until-specified"], JobState::Held),
        (4, vec![], JobState::Held),
        (6, vec!["printer-stopped"], JobState::NeedsAttention),
        (7, vec!["job-canceled-by-user"], JobState::Cancelled),
        (8, vec!["aborted-by-system"], JobState::Aborted),
        (9, vec!["job-completed-successfully"], JobState::Completed),
        (9, vec!["job-completed-with-errors"], JobState::CompletedWithErrors),
        (77, vec![], JobState::Unknown),
    ] {
        assert_eq!(status_of(job_group(7, state, &reasons, "x")).unwrap().state, want, "{state} {reasons:?}");
    }
    let warned = status_of(job_group(7, 9, &["job-completed-with-warnings"], "x")).unwrap();
    assert_eq!((warned.state, warned.warnings.len()), (JobState::Completed, 1));
    assert!(warned.terminal());
    assert_eq!(status_of(job_group(7, 9, &["none"], "x")).unwrap().reasons.len(), 0);
}

#[test]
fn a_job_the_printer_forgot_is_not_found_and_other_failures_are_not() {
    let script = Script::new();
    let b = backend(&script);
    let r = registry(&b);
    let job = IppBackend::job_id(URI, 7).unwrap();
    script.status(0x0406).status(0x0407).status(0x0500).push(Step::Fail(TransportKind::ConnectionClosed, Stage::Reply));
    for _ in 0..2 {
        let e = r.job_status(&job).unwrap_err();
        assert_eq!((e.kind(), e.retry_safety()), (ErrorKind::NotFound, RetrySafety::Safe));
        assert!(e.message().contains("forget"));
    }
    let e = r.job_status(&job).unwrap_err();
    assert_eq!((e.kind(), e.phase()), (ErrorKind::Failed, Some(Phase::Poll)));
    let e = r.job_status(&job).unwrap_err();
    assert_eq!((e.kind(), e.phase(), e.retry_safety()), (ErrorKind::Unavailable, Some(Phase::Poll), RetrySafety::Idempotent));
    // A handle for a printer that was not added is a bad request, not a forgotten job.
    let foreign = IppBackend::job_id("ipp://127.0.0.1:7777/x", 1).unwrap();
    assert_eq!(r.job_status(&foreign).unwrap_err().kind(), ErrorKind::InvalidRequest);
    assert_eq!(r.job_status(&crate::JobId::new("ipp", "garbage")).unwrap_err().kind(), ErrorKind::InvalidRequest);
}

#[test]
fn cancel_is_tolerant_of_jobs_that_already_ended() {
    let script = Script::new();
    let b = backend(&script);
    let r = registry(&b);
    let job = IppBackend::job_id(URI, 7).unwrap();
    script.ok(vec![]);
    r.cancel_job(&job).unwrap();
    assert_eq!(script.last().group(GroupTag::Operation).unwrap().first_i32("job-id"), Some(7));
    script.status(0x0406);
    r.cancel_job(&job).unwrap();
    // not-possible on a completed job is fine; on a running one it is a refusal.
    script.status(0x0404).ok(vec![job_group(7, 9, &[], "x")]);
    r.cancel_job(&job).unwrap();
    script.status(0x0404).ok(vec![job_group(7, 5, &[], "x")]);
    let e = r.cancel_job(&job).unwrap_err();
    assert_eq!((e.kind(), e.native().unwrap().code), (ErrorKind::Rejected, 0x0404));
    script.status(0x0404).status(0x0406);
    r.cancel_job(&job).unwrap();
    script.status(0x0507);
    let e = r.cancel_job(&job).unwrap_err();
    assert_eq!((e.kind(), e.retry_safety()), (ErrorKind::Busy, RetrySafety::Safe));
}

#[test]
fn watching_and_timing_are_what_the_backend_says() {
    let script = Script::new();
    let b = backend(&script);
    assert_eq!(b.watch_support(), WatchSupport::Poll);
    assert_eq!(b.recommended_poll_interval(), Some(Duration::from_millis(1)));
    assert_eq!(IppBackend::new().recommended_poll_interval(), Some(Duration::from_secs(2)));
    assert_eq!(b.open_job_timeout(), Some(Duration::from_secs(240)), "the RFC's recommended upper bound until a printer says otherwise");
    script.ok(vec![printer_group()]);
    b.printer_details(URI).unwrap();
    assert_eq!(b.open_job_timeout(), Some(Duration::from_secs(125)), "the advertised 120 s plus a margin");
    assert_eq!(b.clone().open_job_timeout(), Some(Duration::from_secs(125)), "clones share what was learned");
}

// ---- errors, retry, hostile replies --------------------------------------------------------------------

#[test]
fn ipp_statuses_surface_with_their_native_code_and_retry_facts() {
    let script = Script::new();
    let b = backend(&script);
    let r = registry(&b);
    script.ok(vec![printer_group()]).status(0x0506);
    let e = r.submit_job(request(job())).unwrap_err();
    assert_eq!((e.kind(), e.retry_safety(), e.is_retryable()), (ErrorKind::Busy, RetrySafety::Safe, true));
    assert_eq!(e.retry_after(), Some(Duration::from_secs(15)));
    let native = e.native().unwrap();
    assert_eq!((native.domain, native.code, native.name), ("ipp", 0x0506, Some("server-error-not-accepting-jobs")));
    // A status message is carried.
    script.ok(vec![printer_group()]).push(Step::Reply(Box::new(|req| {
        let op = op_group().with(Attribute::new("status-message", Value::Text("try later".into())));
        Message::new(req.major, req.minor, 0x0507, req.request_id).with_group(op)
    })));
    let e = r.submit_job(request(job())).unwrap_err();
    assert!(e.message().contains("try later") && e.message().contains("server-error-busy"), "{e}");
    // HTTP-level Retry-After on a 503.
    script.ok(vec![printer_group()]).push(Step::Http(503, Some(Duration::from_secs(33))));
    let e = r.submit_job(request(job())).unwrap_err();
    assert_eq!((e.kind(), e.retry_after(), e.retry_safety()), (ErrorKind::Unavailable, Some(Duration::from_secs(33)), RetrySafety::Safe));
    assert_eq!(e.native().unwrap().domain, "http");
    // An unsupported attribute under fidelity names the attributes.
    script.ok(vec![printer_group()]).push(reply(0x040B, vec![unsupported_group(&["print-quality", "media"])]));
    let e = r.submit_job(request(job())).unwrap_err();
    assert!(e.message().contains("print-quality, media"), "{e}");
    assert_eq!(e.kind(), ErrorKind::InvalidRequest);
}

#[test]
fn transport_failures_are_classified_by_what_was_in_flight() {
    let script = Script::new();
    let b = backend(&script);
    let r = registry(&b);
    // Closed after Print-Job was written: the job may exist.
    script.ok(vec![printer_group()]).push(Step::Fail(TransportKind::ConnectionClosed, Stage::Reply));
    let e = r.submit_job(request(job())).unwrap_err();
    assert!(e.outcome_unknown() && e.is_retryable());
    // Refused to connect: nothing was sent.
    script.push(Step::Fail(TransportKind::Refused, Stage::Connect));
    let e = r.submit_job(request(job())).unwrap_err();
    assert_eq!((e.retry_safety(), e.phase(), e.kind()), (RetrySafety::Safe, Some(Phase::Connect), ErrorKind::Unavailable));
    // A timeout of a read is idempotent.
    script.push(Step::Fail(TransportKind::Timeout, Stage::Reply));
    let e = r.printer_details(&id()).unwrap_err();
    assert_eq!((e.kind(), e.retry_safety()), (ErrorKind::Timeout, RetrySafety::Idempotent));
}

#[test]
fn hostile_replies_are_failures_not_panics() {
    let script = Script::new();
    let b = backend(&script);
    let r = registry(&b);
    let details = || {
        let e = r.printer_details(&id()).unwrap_err();
        assert_eq!(script.pending(), 0);
        e
    };
    let bytes = reply_bytes(vec![printer_group()]);
    for body in [bytes[..bytes.len() / 2].to_vec(), b"<html>no</html>".to_vec(), Vec::new(), vec![0xFF; 64], vec![9, 9, 0, 0, 0, 0, 0, 1, 3]] {
        script.push(Step::Raw(body));
        let e = details();
        assert_eq!(e.kind(), ErrorKind::Failed);
        assert_eq!(e.retry_safety(), RetrySafety::Idempotent, "a read may be repeated");
    }
    // A reply that answers another request.
    script.push(Step::Reply(Box::new(|req| Message::new(req.major, req.minor, 0, 999).with_group(op_group()))));
    assert!(details().message().contains("answers request 999"));
    script.push(Step::Http(404, None));
    assert_eq!(details().kind(), ErrorKind::Failed);
    script.push(Step::Http(401, None));
    let e = details();
    assert_eq!(e.kind(), ErrorKind::Rejected);
    assert!(e.message().contains("authentication"));
    // A printer group is required for Get-Printer-Attributes.
    script.ok(vec![]);
    assert_eq!(details().kind(), ErrorKind::Failed);
    // A Print-Job reply without a job id.
    script.ok(vec![printer_group()]).ok(vec![Group::new(GroupTag::Job).with(Attribute::enumeration("job-state", 3))]);
    let e = r.submit_job(request(job())).unwrap_err();
    assert!(e.outcome_unknown(), "the printer answered ok without naming a job: treat as unknown");
}

#[test]
fn a_reply_over_the_size_limit_is_refused() {
    let script = Script::new();
    let b = backend(&script).with_limits(super::NetworkLimits { max_response: 200, ..super::NetworkLimits::default() });
    let bytes = reply_bytes(vec![printer_group()]);
    assert!(bytes.len() > 200);
    script.push(Step::Raw(bytes));
    let e = registry(&b).printer_details(&id()).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Failed);
}

// ---- job ids ------------------------------------------------------------------------------------------------

#[test]
fn job_ids_are_typed_and_round_trip() {
    let job = IppBackend::job_id("http://127.0.0.1/ipp/print", 42).unwrap();
    assert_eq!(job.to_string(), "ipp:ipp://127.0.0.1:631/ipp/print#42");
    let parsed = IppBackend::parse_job_id(&job).unwrap();
    assert_eq!((parsed.printer_uri.as_str(), parsed.job_number), (URI, 42));
    let v6 = IppBackend::job_id("ipp://[fd00::5]:8631/p", 1).unwrap();
    assert_eq!(IppBackend::parse_job_id(&v6).unwrap().printer_uri, "ipp://[fd00::5]:8631/p");
    assert!(IppBackend::job_id("ipp://example.com/", 1).is_err());
    for bad in [
        "ipp://127.0.0.1:631/ipp/print",
        "ipp://127.0.0.1:631/ipp/print#",
        "ipp://127.0.0.1:631/ipp/print#x",
        "ipp://127.0.0.1:631/ipp/print#0",
        "ipp://127.0.0.1:631/ipp/print#4294967295",
        "#5",
        "ipp://8.8.8.8/x#5",
    ] {
        assert_eq!(IppBackend::parse_native(bad).unwrap_err().kind(), ErrorKind::InvalidRequest, "{bad}");
    }
    assert!(IppBackend::parse_job_id(&crate::JobId::new("wsd", "http://127.0.0.1/x#1")).is_err());
}

// ---- listing and reconcile ------------------------------------------------------------------------------------

fn dated(g: Group, when: SystemTime) -> Group {
    g.with(Attribute::new("date-time-at-creation", Value::DateTime(DateTime::from_system_time(when))))
}

fn by(mut g: Group, user: &str) -> Group {
    g.attributes.retain(|a| a.name != "job-originating-user-name");
    g.with(Attribute::name_value("job-originating-user-name", user))
}

#[test]
fn list_jobs_reads_active_and_finished_jobs_with_title_user_state_and_creation_time() {
    let script = Script::new();
    let b = backend(&script);
    let r = registry(&b);
    let t1 = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let t2 = t1 + Duration::from_secs(60);
    script
        .ok(vec![dated(job_group(3, 5, &["job-printing"], "Third [ref abcd1234]"), t2)])
        .ok(vec![dated(job_group(2, 9, &["job-completed-successfully"], "Second"), t1), job_group(3, 5, &[], "dup")]);
    let tag = Tag::find_in("Third [ref abcd1234]").unwrap();
    let listed = r.list_jobs(&id(), &JobFilter::new().marking(tag)).unwrap();
    assert_eq!(script.ops(), [0x000A, 0x000A]);
    let requests = script.requests();
    let which = |m: &Message| m.group(GroupTag::Operation).unwrap().first_str("which-jobs").map(str::to_owned);
    assert_eq!((which(&requests[0]).as_deref(), which(&requests[1]).as_deref()), (Some("not-completed"), Some("completed")));
    assert_eq!(requests[0].group(GroupTag::Operation).unwrap().first_bool("my-jobs"), Some(false));
    assert_eq!(listed.len(), 2, "a job listed twice is one job");
    assert_eq!(listed[0].id.to_string(), format!("ipp:{URI}#3"));
    assert!(listed[0].ours && !listed[1].ours);
    assert_eq!((listed[0].display_title(), listed[1].title.as_str()), ("Third", "Second"));
    assert_eq!((listed[0].state, listed[1].state), (JobState::Processing, JobState::Completed));
    assert_eq!(listed[0].user.as_deref(), Some("craft"));
    assert_eq!((listed[0].created, listed[1].created), (Some(t2), Some(t1)));
    assert_eq!(listed[0].tag, Some(tag));
}

#[test]
fn creation_time_falls_back_to_the_printers_uptime_clock() {
    let script = Script::new();
    let b = backend(&script);
    let r = registry(&b);
    let at = |id, secs| job_group(id, 9, &[], "Old").with(Attribute::integer("time-at-creation", secs));
    script.ok(vec![]).ok(vec![at(2, 4900), at(1, 100)]).ok(vec![Group::new(GroupTag::Printer).with(Attribute::integer("printer-up-time", 5000))]);
    let before = SystemTime::now();
    let listed = r.list_jobs(&id(), &JobFilter::new()).unwrap();
    let after = SystemTime::now();
    assert_eq!(script.ops(), [0x000A, 0x000A, 0x000B]);
    let created = listed[0].created.unwrap();
    assert!(created >= before - Duration::from_secs(100) && created <= after - Duration::from_secs(100), "100 s before now");
    assert!(listed[1].created.unwrap() < created - Duration::from_secs(4000));
}

#[test]
fn a_printer_without_history_or_a_clock_still_lists() {
    let script = Script::new();
    let b = backend(&script);
    let r = registry(&b);
    script.ok(vec![job_group(1, 5, &[], "A")]).status(0x040B);
    let listed = r.list_jobs(&id(), &JobFilter::new()).unwrap();
    assert_eq!((listed.len(), listed[0].created), (1, None));
    // A transport failure is not "no history".
    script.ok(vec![]).push(Step::Fail(TransportKind::ConnectionClosed, Stage::Reply));
    assert!(r.list_jobs(&id(), &JobFilter::new()).is_err());
    // A user filter asks the printer for that user's jobs.
    script.ok(vec![]).ok(vec![]);
    r.list_jobs(&id(), &JobFilter::new().for_user("alice")).unwrap();
    let last = script.last();
    let op = last.group(GroupTag::Operation).unwrap();
    assert_eq!((op.first_str("requesting-user-name"), op.first_bool("my-jobs")), (Some("alice"), Some(true)));
}

#[test]
fn find_job_matches_by_tag_and_otherwise_by_title_user_and_time() {
    let script = Script::new();
    let b = backend(&script);
    let r = registry(&b);
    let now = SystemTime::now();
    let tag = Tag::random();
    let titled = format!("Report [ref {tag}]");
    let at = |t: SystemTime| Timestamp::from_millis(u64::try_from(t.duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap());
    let listing = |script: &Script| {
        script.ok(vec![dated(job_group(9, 5, &[], &titled), now)]).ok(vec![
            dated(job_group(4, 9, &[], "Report"), now - Duration::from_secs(3600)),
            dated(job_group(8, 9, &[], "Report"), now - Duration::from_secs(2)),
        ]);
    };
    // By tag: exactly ours, although two others have the same title.
    listing(&script);
    let found = r.find_job(&id(), &Fingerprint::new("Report", at(now - Duration::from_secs(5))).with_tag(tag)).unwrap();
    let Reconciled::Found(job) = found else { panic!("{found:?}") };
    assert!(job.ours);
    assert_eq!(job.id.to_string(), format!("ipp:{URI}#9"));
    // By title and time alone the tagged job (the tag is not part of the title) and the recent
    // untagged one both match, newest first; the hour-old one is out of the window. Never `ours`.
    listing(&script);
    let found = r.find_job(&id(), &Fingerprint::new("Report", at(now - Duration::from_secs(10)))).unwrap();
    let Reconciled::Ambiguous(jobs) = found else { panic!("{found:?}") };
    let ids: Vec<_> = jobs.iter().map(|j| j.id.to_string()).collect();
    assert_eq!(ids, [format!("ipp:{URI}#9"), format!("ipp:{URI}#8")]);
    assert!(jobs.iter().all(|j| !j.ours));
    // Another user's job of the same title is not ours (the backend fills in its own user name).
    script.ok(vec![]).ok(vec![by(dated(job_group(3, 9, &[], "Report"), now), "bob")]);
    let found = r.find_job(&id(), &Fingerprint::new("Report", at(now - Duration::from_secs(10)))).unwrap();
    assert!(matches!(found, Reconciled::NotFound), "{found:?}");
}

// ---- tracked jobs end to end ---------------------------------------------------------------------------------------

fn tracked(script: &Script, clock: &ManualClock) -> (Registry, PrintJob) {
    let b = backend(script);
    let r = registry(&b);
    let job = PrintJob::new(job(), BytesSource::new(DocumentFormat::Pdf, PDF.to_vec()))
        .with_clock(Arc::new(clock.clone()))
        .with_consent(UserConfirmed::interactive());
    (r, job)
}

fn drive(job: &mut PrintJob, r: &Registry, clock: &ManualClock) {
    for _ in 0..200 {
        match job.advance(r).next_in {
            Some(wait) => clock.advance(wait),
            None => return,
        }
    }
    panic!("never settled: {:?}", job.state());
}

#[test]
fn a_tracked_print_completes_and_carries_its_tag_in_the_job_name() {
    let script = Script::new();
    let clock = ManualClock::new();
    let (r, mut job) = tracked(&script, &clock);
    let tag = *job.tag();
    script.ok(vec![printer_group()]).ok(accepted(1));
    script.ok(vec![job_group(1, 5, &["job-printing"], "Report")]).ok(vec![job_group(1, 9, &["job-completed-successfully"], "Report")]);
    drive(&mut job, &r, &clock);
    assert_eq!(job.state(), LifecycleState::Terminal(Outcome::Completed));
    assert_eq!(script.requests()[1].group(GroupTag::Operation).unwrap().first_str("job-name"), Some(format!("Report [ref {tag}]").as_str()));
    let snapshot = job.snapshot();
    assert!(snapshot.tag_applied);
    assert!(snapshot.native.unwrap().effective.unwrap().contains("print-job"));
}

#[test]
fn a_lost_print_job_reply_is_reconciled_by_tag_and_nothing_is_sent_twice() {
    let script = Script::new();
    let clock = ManualClock::new();
    let (r, mut job) = tracked(&script, &clock);
    let tag = *job.tag();
    script.ok(vec![printer_group()]).push(Step::Fail(TransportKind::ConnectionClosed, Stage::Reply));
    drive(&mut job, &r, &clock);
    assert_eq!(job.state(), LifecycleState::NeedsAttention, "{:?}", job.snapshot().last_error);
    let prints = || script.ops().iter().filter(|op| **op == 0x0002).count();
    assert_eq!(prints(), 1);
    // The printer did make the job: the listing finds it by the tag.
    let name = format!("Report [ref {tag}]");
    script.ok(vec![job_group(5, 5, &["job-printing"], &name)]).ok(vec![job_group(2, 9, &[], "Report")]);
    let found = job.reconcile(&r).unwrap();
    let Reconciled::Found(remote) = found else { panic!("{found:?}") };
    assert!(remote.ours);
    assert_eq!(remote.id.to_string(), format!("ipp:{URI}#5"));
    assert_eq!(job.snapshot().advice, RetryAdvice::DoNotRetry);
    job.accept(&r, &remote.id).unwrap();
    script.ok(vec![job_group(5, 9, &["job-completed-successfully"], &name)]);
    drive(&mut job, &r, &clock);
    assert_eq!(job.state(), LifecycleState::Terminal(Outcome::Completed));
    assert_eq!(prints(), 1, "the second copy was never sent");
}

#[test]
fn a_reconcile_that_finds_nothing_allows_a_resend() {
    let script = Script::new();
    let clock = ManualClock::new();
    let (r, mut job) = tracked(&script, &clock);
    script.ok(vec![printer_group()]).push(Step::Fail(TransportKind::Timeout, Stage::Reply));
    drive(&mut job, &r, &clock);
    assert_eq!(job.state(), LifecycleState::NeedsAttention);
    script.ok(vec![job_group(1, 9, &[], "Other")]).ok(vec![]);
    let found = job.reconcile(&r).unwrap();
    assert!(matches!(found, Reconciled::NotFound), "{found:?}");
}

#[test]
fn without_the_tag_a_lost_reply_reconciles_by_title() {
    let script = Script::new();
    let clock = ManualClock::new();
    let (r, job) = tracked(&script, &clock);
    let mut job = job.with_tag_policy(TagPolicy::Never);
    script.ok(vec![printer_group()]).push(Step::Fail(TransportKind::ConnectionClosed, Stage::Reply));
    drive(&mut job, &r, &clock);
    assert_eq!(script.requests()[1].group(GroupTag::Operation).unwrap().first_str("job-name"), Some("Report"));
    script.ok(vec![job_group(3, 5, &[], "Report")]).ok(vec![]);
    let found = job.reconcile(&r).unwrap();
    let Reconciled::Found(remote) = found else { panic!("{found:?}") };
    assert!(!remote.ours);
}

// ---- long names ------------------------------------------------------------------------------------------------------

#[test]
fn a_long_or_multibyte_title_is_cut_so_that_the_tag_survives_255_bytes() {
    let b = IppBackend::new();
    let tag = Tag::random();
    for title in ["x".repeat(400), "\u{e9}".repeat(300), "\u{4e2d}".repeat(200)] {
        let name = b.job_name(&title, Some(&tag));
        assert!(name.len() <= 255, "{} bytes", name.len());
        assert_eq!(Tag::find_in(&name), Some(tag));
    }
    assert_eq!(b.job_name("short", None), "short");
    assert!(b.job_name(&"\u{4e2d}".repeat(200), None).len() <= 255);
}

// ---- a static reply vector ---------------------------------------------------------------------------------------------

#[test]
fn printer_attributes_parse_from_a_static_reply_vector() {
    fn attr(out: &mut Vec<u8>, tag: u8, name: &str, value: &[u8]) {
        out.push(tag);
        out.extend_from_slice(&(name.len() as u16).to_be_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&(value.len() as u16).to_be_bytes());
        out.extend_from_slice(value);
    }
    // The least a printer returns: a version, a status, a request id, the operation group, then
    // the printer group with a few attributes (one with an additional value).
    let mut bytes: Vec<u8> = vec![1, 1, 0, 0, 0, 0, 0, 1, 0x01];
    attr(&mut bytes, 0x47, "attributes-charset", b"utf-8");
    attr(&mut bytes, 0x48, "attributes-natural-language", b"en");
    bytes.push(0x04);
    attr(&mut bytes, 0x42, "printer-name", b"Minimal");
    attr(&mut bytes, 0x23, "printer-state", &[0, 0, 0, 3]);
    attr(&mut bytes, 0x49, "document-format-supported", b"application/pdf");
    attr(&mut bytes, 0x49, "", b"image/jpeg; charset=x");
    attr(&mut bytes, 0x33, "copies-supported", &[0, 0, 0, 1, 0, 0, 0, 9]);
    bytes.push(0x03);
    let m = Message::decode(&bytes, &Limits::default()).unwrap();
    let p = PrinterAttrs::from_group(m.group(GroupTag::Printer).unwrap());
    assert_eq!((p.name.as_deref(), p.state, p.copies), (Some("Minimal"), Some(3), Some((1, 9))));
    assert_eq!(p.formats, ["application/pdf", "image/jpeg; charset=x"]);
    assert_eq!(p.lists_format("image/jpeg"), Some(true));
}
