use std::{cell::Cell, rc::Rc, sync::mpsc};

use super::*;

const PDF: &str = "application/pdf";

fn printer() -> PrinterSummary {
    PrinterSummary {
        name: "Office".into(),
        color: Some(true),
        state: "Idle".into(),
        queued_jobs: Some(0),
        reasons: vec![],
        caps: Caps {
            formats: vec![PDF.into()],
            sides: vec!["OneSided".into(), "TwoSidedLongEdge".into()],
            orientations: vec!["Portrait".into()],
            qualities: vec![],
            media_sizes: vec!["iso_a4_210x297mm".into()],
            copies: Some((1, 5)),
        },
    }
}

fn queried(url: &str) -> Draft {
    let mut d = Draft { url: url.into(), ..Default::default() };
    let g = d.begin_query();
    d.finish_query(g, &d.normalized_url(), Ok(printer()));
    d
}

fn labels() -> Labels<'static> {
    Labels {
        address: "Adresse",
        address_hint: "http://example",
        address_note: "Nur lokale Adressen",
        query_printer: "Drucker abfragen",
        querying: "Frage den Drucker",
        query_first: "Zuerst abfragen",
        printer_status: "Status: {state}",
        reasons: "Gruende: {reasons}",
        no_common_format: "Kein gemeinsames Format",
        copies: "Kopien",
        sides: "Zweiseitig",
        sides_off: "Aus",
        sides_long_edge: "Lange Kante",
        sides_short_edge: "Kurze Kante",
        orientation: "Ausrichtung",
        portrait: "Hoch",
        landscape: "Quer",
        reverse_portrait: "Hoch gedreht",
        reverse_landscape: "Quer gedreht",
        quality: "Qualitaet",
        draft: "Entwurf",
        normal: "Normal",
        high: "Hoch",
        photo: "Foto",
        auto: "Automatisch",
        confirm: "Ich bestaetige den Versand",
        sending: "Sende an {printer}",
        canceling: "Abbrechen laeuft",
        cancel: "Abbrechen",
        sent: "Gesendet an {printer}",
        sheets: "Blatt: {n}",
        outcome_warnings: "Mit Warnungen",
        outcome_errors: "Mit Fehlern",
        outcome_canceled: "Abgebrochen",
        outcome_aborted: "Gestoppt",
        outcome_forgotten: "Vergessen",
        outcome_still_running: "Laeuft noch",
        failure_policy: "Adresse abgelehnt",
        failure_transport: "Nicht erreichbar",
        failure_protocol: "Antwort unverstaendlich",
        failure_fault: "Drucker lehnt ab",
        failure_no_common_format: "Kein Format",
        failure_unsupported: "Nicht unterstuetzt",
        failure_encode: "Dokument nicht vorbereitet",
        failure_invalid: "Ungueltig",
        failure_stopped: "Unerwartet beendet",
        failure_other: "Unerwartet",
    }
}

#[test]
fn addresses_get_a_scheme_and_nothing_else() {
    assert_eq!(normalize_url(" 192.0.2.5:5357/s "), "http://192.0.2.5:5357/s");
    assert_eq!(normalize_url("http://198.51.100.2/"), "http://198.51.100.2/");
    assert_eq!(normalize_url("https://198.51.100.2/"), "https://198.51.100.2/");
    assert_eq!(normalize_url("  "), "");
}

#[test]
fn only_listed_options_are_offered() {
    let d = queried("192.0.2.5");
    assert_eq!(d.offered_sides(), [Sides::OneSided, Sides::LongEdge]);
    assert_eq!(d.offered_orientations(), [Orientation::Portrait]);
    assert!(d.offered_qualities().is_empty(), "not stated: nothing offered");
    assert_eq!(d.max_copies(), 5);
    let fresh = Draft::default();
    assert!(fresh.offered_sides().is_empty() && fresh.offered_orientations().is_empty() && fresh.max_copies() == 1);
}

#[test]
fn a_request_is_limited_to_what_the_printer_lists() {
    let mut d = queried("192.0.2.5");
    d.copies = 3;
    d.sides = Some(Sides::LongEdge);
    d.quality = Some(Quality::Photo);
    let r = d.request(Some("iso_a4_210x297mm"));
    assert_eq!((r.copies, r.sides, r.quality), (Some(3), Some(Sides::LongEdge), None));
    assert_eq!(r.media_size.as_deref(), Some("iso_a4_210x297mm"));
    assert_eq!(d.request(Some("na_letter_8.5x11in")).media_size, None, "an unlisted size is left to the printer");
    assert_eq!(d.request(None).media_size, None);
    d.copies = 1;
    assert_eq!(d.request(None).copies, None);
}

#[test]
fn choices_the_next_printer_lacks_are_dropped() {
    let mut d = queried("192.0.2.5");
    d.copies = 5;
    d.sides = Some(Sides::LongEdge);
    d.orientation = Some(Orientation::Portrait);
    let mut other = printer();
    other.caps.sides = vec!["OneSided".into()];
    other.caps.orientations.clear();
    other.caps.copies = Some((1, 2));
    let g = d.begin_query();
    d.finish_query(g, "http://192.0.2.5", Ok(other));
    assert_eq!((d.sides, d.orientation, d.copies), (None, None, 2));
}

#[test]
fn the_confirmation_is_never_default_and_never_kept() {
    let mut d = queried("192.0.2.5");
    assert!(!d.confirmed && !Draft::default().confirmed);
    assert_eq!(d.block(true, true, &[PDF]), Some(Block::Unconfirmed));
    d.confirmed = true;
    assert_eq!(d.block(true, true, &[PDF]), None);
    d.url = "192.0.2.6".into();
    d.address_edited();
    assert!(!d.confirmed && d.query == Query::Idle && d.printer().is_none());
    let mut d = queried("192.0.2.5");
    d.confirmed = true;
    d.begin_query();
    assert!(!d.confirmed);
    let mut d = queried("192.0.2.5");
    d.confirmed = true;
    d.job_started();
    assert!(!d.confirmed && d.running());
    d.confirmed = true;
    d.dialog_opened();
    assert!(!d.confirmed && d.running(), "a running job survives reopening");
    d.job_finished(JobView::Idle);
    d.confirmed = true;
    d.dialog_opened();
    assert!(!d.confirmed);
}

#[test]
fn print_waits_for_a_query_of_this_address() {
    let mut d = Draft { url: "192.0.2.5".into(), confirmed: true, ..Default::default() };
    assert_eq!(d.block(false, true, &[PDF]), Some(Block::Address));
    assert_eq!(d.block(true, true, &[PDF]), Some(Block::NotQueried));
    let g = d.begin_query();
    assert_eq!(d.block(true, true, &[PDF]), Some(Block::Busy));
    d.finish_query(g, "http://192.0.2.5", Ok(printer()));
    d.confirmed = true;
    assert_eq!(d.block(true, false, &[PDF]), Some(Block::NoPages));
    assert_eq!(d.block(true, true, &[PDF]), None);
    // A printer that takes none of the formats the host can produce.
    assert_eq!(d.block(true, true, &["image/pwg-raster"]), Some(Block::Format));
    assert_eq!(d.block(true, true, &["image/pwg-raster", "APPLICATION/PDF"]), None, "format match ignores case");
    // A printer listing no formats is left to answer for itself.
    let mut silent = printer();
    silent.caps.formats.clear();
    let g = d.begin_query();
    d.finish_query(g, "http://192.0.2.5", Ok(silent));
    d.confirmed = true;
    assert_eq!(d.block(true, true, &["image/pwg-raster"]), None);
}

#[test]
fn a_late_or_stale_answer_is_dropped() {
    let mut d = Draft { url: "192.0.2.5".into(), ..Default::default() };
    let first = d.begin_query();
    let second = d.begin_query();
    d.finish_query(first, "http://192.0.2.5", Ok(printer()));
    assert!(d.querying(), "the older answer does not end the newer query");
    d.url = "192.0.2.9".into();
    d.address_edited();
    d.finish_query(second, "http://192.0.2.5", Ok(printer()));
    assert!(d.printer().is_none());
    let g = d.begin_query();
    let failure = Failure::new(FailureKind::Transport, "timed out");
    d.finish_query(g, "http://192.0.2.9", Err(failure.clone()));
    assert_eq!(d.query, Query::Failed(failure));
}

#[test]
fn cancel_is_sent_once() {
    let mut d = queried("192.0.2.5");
    d.job_started();
    assert_eq!(d.job, JobView::Running { canceling: false });
    assert!(d.job_canceling());
    assert!(!d.job_canceling());
    d.job_finished(JobView::Idle);
    assert!(!d.job_canceling());
}

#[test]
fn outcomes_and_failures_map_from_client_names() {
    for (name, outcome) in [
        ("completed", Outcome::Completed),
        ("completed_with_warnings", Outcome::CompletedWithWarnings),
        ("completed_with_errors", Outcome::CompletedWithErrors),
        ("canceled", Outcome::Canceled),
        ("forgotten", Outcome::Forgotten),
        ("still_running", Outcome::StillRunning),
        ("anything else", Outcome::Aborted),
    ] {
        assert_eq!(Outcome::from_name(name), outcome, "{name}");
    }
    assert!(Outcome::CompletedWithWarnings.is_success() && !Outcome::Canceled.is_success());
    let l = labels();
    assert_eq!(l.outcome_text(Outcome::Completed), None);
    assert!(Outcome::from_name("canceled") != Outcome::Completed && l.outcome_text(Outcome::Canceled) == Some("Abgebrochen"));
    let kinds = [
        "policy",
        "transport",
        "connection_closed",
        "http",
        "too_large",
        "xml",
        "protocol",
        "fault",
        "invalid",
        "no_common_format",
        "unsupported",
        "encode",
        "stopped",
        "mystery",
    ];
    let headings: std::collections::BTreeSet<_> = kinds.iter().map(|k| l.failure_heading(FailureKind::from_name(k))).collect();
    assert_eq!(headings.len(), 10, "{headings:?}");
}

fn query_worker(answer: Result<PrinterSummary, Failure>) -> QueryPrinter {
    let answer = std::sync::Mutex::new(Some(answer));
    Box::new(move |_| {
        let (tx, rx) = mpsc::channel();
        if let Ok(mut slot) = answer.lock()
            && let Some(answer) = slot.take()
        {
            let _ = tx.send(answer);
        }
        rx
    })
}

#[test]
fn state_files_a_query_answer_and_drops_a_stopped_worker() {
    let ctx = egui::Context::default();
    let mut state = State::new(Draft::for_address("192.0.2.5"));
    assert!(!state.start_query(&query_worker(Ok(printer())), false), "an address the host refused starts nothing");
    assert!(state.start_query(&query_worker(Ok(printer())), true));
    assert!(state.draft.querying());
    assert!(!state.start_query(&query_worker(Ok(printer())), true), "one query at a time");
    state.poll(&ctx);
    assert_eq!(state.draft.printer().map(|p| p.name.as_str()), Some("Office"));
    // A worker that ends without answering is a failure, not a hang.
    let silent: QueryPrinter = Box::new(|_| mpsc::channel().1);
    assert!(state.start_query(&silent, true));
    state.poll(&ctx);
    assert_eq!(state.draft.query, Query::Failed(Failure::new(FailureKind::Stopped, "")));
    assert!(state.draft.printer().is_none());
}

#[test]
fn a_job_starts_only_when_nothing_blocks_it_and_spends_the_confirmation() {
    let ctx = egui::Context::default();
    let mut state = State::new(queried("192.0.2.5"));
    let cancelled = Rc::new(Cell::new(0));
    let (tx, rx) = mpsc::channel();
    let slot = std::cell::RefCell::new(Some(rx));
    let seen = Rc::new(std::cell::RefCell::new(None));
    let (seen2, cancelled2) = (seen.clone(), cancelled.clone());
    let send: SendJob = Box::new(move |url, request| {
        *seen2.borrow_mut() = Some((url.to_owned(), request));
        let counter = cancelled2.clone();
        JobHandle { result: slot.borrow_mut().take().unwrap_or_else(|| mpsc::channel().1), cancel: Box::new(move || counter.set(counter.get() + 1)) }
    });
    assert!(!state.start_job(&send, true, true, &[PDF], None), "unconfirmed");
    state.draft.confirmed = true;
    state.draft.copies = 2;
    assert!(state.start_job(&send, true, true, &[PDF], Some("iso_a4_210x297mm")));
    assert!(!state.draft.confirmed && state.draft.running());
    let sent = seen.borrow().clone();
    let (url, request) = sent.unwrap_or_default();
    assert_eq!(url, "http://192.0.2.5");
    assert_eq!((request.copies, request.media_size.as_deref()), (Some(2), Some("iso_a4_210x297mm")));
    assert!(!state.start_query(&query_worker(Ok(printer())), true), "no query while a job runs");
    assert!(state.cancel_job());
    assert!(!state.cancel_job());
    assert_eq!(cancelled.get(), 1, "the host is told once");
    let report = Report { outcome: Outcome::Canceled, printer: "Office".into(), sheets: 0, notes: vec![] };
    tx.send(Ok(report.clone())).unwrap();
    state.poll(&ctx);
    assert_eq!(state.draft.job, JobView::Finished(report));
    assert!(!state.cancel_job());
}

#[test]
fn a_job_worker_that_stops_or_fails_is_reported() {
    let ctx = egui::Context::default();
    let mut state = State::new(queried("192.0.2.5"));
    let (tx, rx) = mpsc::channel();
    let slot = std::cell::RefCell::new(Some(rx));
    let send: SendJob =
        Box::new(move |_, _| JobHandle { result: slot.borrow_mut().take().unwrap_or_else(|| mpsc::channel().1), cancel: Box::new(|| {}) });
    state.draft.confirmed = true;
    assert!(state.start_job(&send, true, true, &[PDF], None));
    drop(tx);
    state.poll(&ctx);
    assert_eq!(state.draft.job, JobView::Failed(Failure::new(FailureKind::Stopped, "")));
    let (tx, rx) = mpsc::channel();
    let slot = std::cell::RefCell::new(Some(rx));
    let send: SendJob =
        Box::new(move |_, _| JobHandle { result: slot.borrow_mut().take().unwrap_or_else(|| mpsc::channel().1), cancel: Box::new(|| {}) });
    state.draft.confirmed = true;
    assert!(state.start_job(&send, true, true, &[PDF], None));
    let failure = Failure::new(FailureKind::Fault, "refused");
    tx.send(Err(failure.clone())).unwrap();
    state.poll(&ctx);
    assert_eq!(state.draft.job, JobView::Failed(failure));
}

type Frames<'a> = (&'a egui::Context, &'a std::cell::RefCell<State>, &'a Validate, &'a QueryPrinter);

fn frame(f: &Frames<'_>, events: Vec<egui::Event>) -> (Response, egui::FullOutput) {
    let (ctx, state, validate, query) = *f;
    let l = labels();
    let mut response = Response::default();
    let mut output = ctx.run_ui(
        egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(420.0, 480.0))), events, ..Default::default() },
        |ui| {
            let host = Host { validate, query, sheets_ok: true, formats: &[PDF] };
            response = state.borrow_mut().show(ui, egui::Id::new("net"), &host, &l);
        },
    );
    output.textures_delta.clear();
    (response, output)
}

fn center_of(output: &egui::FullOutput, label: &str) -> Option<egui::Pos2> {
    let tree = output.platform_output.accesskit_update.as_ref()?;
    let node = tree.nodes.iter().find(|(_, n)| n.label() == Some(label) || n.value() == Some(label))?.1.clone();
    let b = node.bounds()?;
    Some(egui::pos2(((b.x0 + b.x1) / 2.0) as f32, ((b.y0 + b.y1) / 2.0) as f32))
}

fn click(f: &Frames<'_>, at: egui::Pos2) -> Response {
    let button = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
    frame(f, vec![egui::Event::PointerMoved(at), button(true)]);
    frame(f, vec![button(false)]).0
}

#[test]
fn the_section_shows_its_controls_with_labels_and_drives_query_and_confirmation() {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let state = std::cell::RefCell::new(State::new(Draft::for_address("192.0.2.5")));
    let validate: &Validate = &|url| if url.is_empty() { Err(Failure::new(FailureKind::Policy, "empty")) } else { Ok(()) };
    let query = query_worker(Ok(printer()));
    let f: Frames<'_> = (&ctx, &state, validate, &query);
    let (r, output) = frame(&f, Vec::new());
    assert_eq!(r.block, Some(Block::NotQueried));
    let at = center_of(&output, "Drucker abfragen");
    assert!(at.is_some(), "the Query button is reachable by label");
    let r = click(&f, at.unwrap_or_default());
    assert!(r.query_started);
    let (r, output) = frame(&f, Vec::new());
    assert_eq!(r.block, Some(Block::Unconfirmed));
    for label in ["Adresse", "Ich bestaetige den Versand", "Kopien", "Zweiseitig", "Ausrichtung"] {
        assert!(center_of(&output, label).is_some(), "{label} is exposed");
    }
    assert!(center_of(&output, "Qualitaet").is_none(), "an unlisted option is not drawn");
    let at = center_of(&output, "Ich bestaetige den Versand").unwrap_or_default();
    let r = click(&f, at);
    assert!(r.changed);
    assert!(state.borrow().draft.confirmed);
    let (r, _) = frame(&f, Vec::new());
    assert_eq!(r.block, None, "the app may now enable Print");
    // Editing the address voids the answer and the yes.
    state.borrow_mut().draft.url.push('0');
    state.borrow_mut().draft.address_edited();
    let (r, _) = frame(&f, Vec::new());
    assert_eq!(r.block, Some(Block::NotQueried));
    assert!(!state.borrow().draft.confirmed);
}

#[test]
fn a_refused_address_shows_the_hosts_error_and_blocks_query() {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let state = std::cell::RefCell::new(State::new(Draft::for_address("somewhere")));
    let validate: &Validate = &|_| Err(Failure::new(FailureKind::Policy, "not on this network"));
    let query = query_worker(Ok(printer()));
    let f: Frames<'_> = (&ctx, &state, validate, &query);
    let (r, output) = frame(&f, Vec::new());
    assert_eq!(r.block, Some(Block::Address));
    assert!(center_of(&output, "Adresse abgelehnt").is_some());
    assert!(center_of(&output, "not on this network").is_some());
    let tree = output.platform_output.accesskit_update.as_ref();
    let disabled = tree.is_some_and(|t| t.nodes.iter().any(|(_, n)| n.label() == Some("Drucker abfragen") && n.is_disabled()));
    assert!(disabled, "Query printer is disabled for a refused address");
}

#[test]
fn a_running_job_disables_editing_and_offers_cancel() {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let mut draft = queried("192.0.2.5");
    draft.job_started();
    let state = std::cell::RefCell::new(State::new(draft));
    let validate: &Validate = &|_| Ok(());
    let query = query_worker(Ok(printer()));
    let f: Frames<'_> = (&ctx, &state, validate, &query);
    let (r, output) = frame(&f, Vec::new());
    assert_eq!(r.block, Some(Block::Busy));
    let tree = output.platform_output.accesskit_update.as_ref();
    let disabled = |label: &str| tree.is_some_and(|t| t.nodes.iter().any(|(_, n)| n.label() == Some(label) && n.is_disabled()));
    assert!(disabled("Ich bestaetige den Versand") && disabled("Drucker abfragen"));
    let at = center_of(&output, "Abbrechen").unwrap_or_default();
    let r = click(&f, at);
    assert!(r.cancel_requested);
    assert_eq!(state.borrow().draft.job, JobView::Running { canceling: true });
}
