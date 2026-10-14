//! Submit and watch as separate steps, driven through the public API with fake backends: one that
//! reports a job's status and can cancel it, one that cannot report anything (a fire-and-forget
//! spooler), and the retry vocabulary of [`PrintError`].
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use craft_print::backends::Backend;
use craft_print::{
    CancelToken, Document, ErrorKind, Job, JobRef, JobState, JobStatus, JobWatch, ManualClock, Phase, PrintError, PrinterDetails, PrinterRef,
    Registry, RetrySafety, SubmitRequest, Submitted, UserConfirmed, WaitPolicy, WatchOutcome, WatchStep, WatchSupport, wait_for_job,
};

/// What the fake printer does with the job it accepted.
struct Reporting {
    /// Statuses returned by successive `job_status` calls; the last one repeats.
    script: Mutex<Vec<Result<JobStatus, PrintError>>>,
    status_calls: AtomicU32,
    cancels: Mutex<Vec<String>>,
    submitted_at: Mutex<Option<Instant>>,
}

impl Reporting {
    fn new(script: Vec<Result<JobStatus, PrintError>>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            status_calls: AtomicU32::new(0),
            cancels: Mutex::new(Vec::new()),
            submitted_at: Mutex::new(None),
        })
    }
}

struct Shared(Arc<Reporting>);

impl Backend for Shared {
    fn name(&self) -> &'static str {
        "net"
    }
    fn list_printers(&self) -> Result<Vec<PrinterRef>, PrintError> {
        Ok(vec![PrinterRef { id: "p1".into(), name: "P1".into(), default: true, two_sided: None }])
    }
    fn printer_details(&self, native_id: &str) -> Result<PrinterDetails, PrintError> {
        Ok(PrinterDetails { id: native_id.into(), name: native_id.into(), ..PrinterDetails::default() })
    }
    fn submit(&self, request: SubmitRequest<'_>) -> Result<Submitted, PrintError> {
        *self.0.submitted_at.lock().unwrap() = Some(Instant::now());
        let printer = request.job.printer.unwrap_or_default();
        Ok(Submitted::new("ignored-by-registry", "accepted").with_job(JobRef::new(format!("{printer}#7"))))
    }
    fn watch_support(&self) -> WatchSupport {
        WatchSupport::Poll
    }
    fn recommended_poll_interval(&self) -> Option<Duration> {
        Some(Duration::from_millis(1500))
    }
    fn job_status(&self, job: &JobRef) -> Result<JobStatus, PrintError> {
        assert_eq!(job.native, "p1#7", "the backend gets its native handle");
        assert_eq!(job.id, "net:p1#7", "and the namespaced id");
        self.0.status_calls.fetch_add(1, Ordering::SeqCst);
        let mut script = self.0.script.lock().unwrap();
        if script.len() > 1 { script.remove(0) } else { script[0].clone() }
    }
    fn cancel_job(&self, job: &JobRef) -> Result<(), PrintError> {
        self.0.cancels.lock().unwrap().push(job.native.clone());
        Ok(())
    }
}

/// Takes the document and says nothing more, as a fire-and-forget spooler does.
struct Silent;
impl Backend for Silent {
    fn name(&self) -> &'static str {
        "lp"
    }
    fn list_printers(&self) -> Result<Vec<PrinterRef>, PrintError> {
        Ok(vec![PrinterRef { id: "q".into(), name: "Q".into(), default: false, two_sided: None }])
    }
    fn printer_details(&self, native_id: &str) -> Result<PrinterDetails, PrintError> {
        Ok(PrinterDetails { id: native_id.into(), name: native_id.into(), ..PrinterDetails::default() })
    }
    fn submit(&self, _: SubmitRequest<'_>) -> Result<Submitted, PrintError> {
        Ok(Submitted::new("lp", "request id is q-1").with_job(JobRef::new("q-1")))
    }
}

fn status(state: JobState) -> JobStatus {
    JobStatus::new(state)
}

fn request(printer: &str) -> SubmitRequest<'static> {
    SubmitRequest::new(Document::pdf(b"%PDF-1.7"), Job { printer: Some(printer.into()), ..Job::default() })
}

fn registry(backend: &Arc<Reporting>) -> Registry {
    let registry = Registry::new();
    registry.register(Box::new(Shared(Arc::clone(backend)))).unwrap();
    registry.register(Box::new(Silent)).unwrap();
    registry
}

#[test]
fn submit_returns_the_accepted_job_without_waiting_and_the_registry_fills_in_the_rest() {
    let printer = Reporting::new(vec![Ok(status(JobState::Processing))]);
    let registry = registry(&printer);
    let started = Instant::now();
    let submitted = registry.submit_job(request("net:p1")).unwrap();
    assert!(started.elapsed() < Duration::from_secs(5), "submit never waits for the job: the printer keeps it Processing forever");
    assert_eq!(printer.status_calls.load(Ordering::SeqCst), 0, "submit asks nothing after acceptance");
    assert_eq!(submitted.backend, "net", "the registry sets the backend name");
    assert_eq!(submitted.job.as_ref().map(|j| (j.id.as_str(), j.native.as_str())), Some(("net:p1#7", "p1#7")));
    assert_eq!(submitted.watch, WatchSupport::Poll);
    assert_eq!(submitted.poll_interval, Some(Duration::from_millis(1500)));
    assert_eq!(submitted.receipt().job_id.as_deref(), Some("p1#7"));
    // The older receipt entry point is the same call.
    assert_eq!(registry.submit(request("net:p1")).unwrap().message, "accepted");
}

#[test]
fn status_and_cancel_route_by_the_job_ids_prefix() {
    let printer = Reporting::new(vec![Ok(status(JobState::Pending)), Ok(status(JobState::Processing))]);
    let registry = registry(&printer);
    assert_eq!(registry.job_status("net:p1#7").unwrap().state, JobState::Pending);
    assert_eq!(registry.job_status("net:p1#7").unwrap().state, JobState::Processing);
    registry.cancel_job("net:p1#7").unwrap();
    assert_eq!(*printer.cancels.lock().unwrap(), ["p1#7"]);
    assert_eq!(registry.watch_support("net:p1#7").unwrap(), WatchSupport::Poll);
    assert_eq!(registry.recommended_poll_interval("net:p1#7").unwrap(), Some(Duration::from_millis(1500)));
    for bad in ["p1#7", "nope:p1#7"] {
        assert_eq!(registry.job_status(bad).unwrap_err().kind(), ErrorKind::UnknownBackend, "{bad}");
        assert_eq!(registry.cancel_job(bad).unwrap_err().kind(), ErrorKind::UnknownBackend, "{bad}");
    }
    assert!(registry.job_status("p1#7").unwrap_err().message().contains("job id"), "the message names a job, not a printer");
    assert_eq!(Registry::new().job_status("net:x").unwrap_err().kind(), ErrorKind::Unsupported);
}

#[test]
fn a_backend_that_cannot_report_is_accepted_outcome_unknown_and_is_not_polled() {
    let printer = Reporting::new(vec![Ok(status(JobState::Processing))]);
    let registry = registry(&printer);
    let submitted = registry.submit_job(request("lp:q")).unwrap();
    assert_eq!(submitted.watch, WatchSupport::None);
    assert_eq!(submitted.poll_interval, None);
    let status = registry.job_status(&submitted.job.as_ref().unwrap().id).unwrap();
    assert_eq!(status.state, JobState::Unknown);
    assert!(!status.terminal(), "unknown is 'accepted, outcome unknown', not an ending");
    let mut watch = JobWatch::for_submitted(&submitted, WaitPolicy::default()).unwrap();
    assert_eq!(watch.tick(&registry), WatchStep::Done(WatchOutcome::NoStatus));
    assert_eq!(registry.cancel_job("lp:q-1").unwrap_err().kind(), ErrorKind::Unsupported, "a backend with no cancel says so");
}

#[test]
fn a_watch_with_an_injected_clock_follows_the_job_to_a_terminal_state() {
    let printer = Reporting::new(vec![
        Ok(status(JobState::Pending)),
        Ok(JobStatus { sheets_done: Some(1), sheets_total: Some(2), ..status(JobState::Processing) }),
        Ok(status(JobState::Completed)),
    ]);
    let registry = registry(&printer);
    let submitted = registry.submit_job(request("net:p1")).unwrap();
    let clock = ManualClock::new();
    let mut watch =
        JobWatch::for_submitted(&submitted, WaitPolicy { growth_percent: 100, ..WaitPolicy::default() }).unwrap().with_clock(Arc::new(clock.clone()));
    let mut seen = Vec::new();
    let outcome = watch.run(&registry, &mut |step| {
        if let WatchStep::Update(s) = step {
            seen.push((s.state, s.sheet_fraction()));
        }
    });
    assert!(matches!(outcome, WatchOutcome::Terminal(ref s) if s.state == JobState::Completed), "{outcome:?}");
    assert_eq!(seen, [(JobState::Pending, None), (JobState::Processing, Some(0.5))]);
    assert_eq!(clock.sleeps().iter().sum::<Duration>(), Duration::from_millis(3000), "two gaps of the backend's recommended 1.5 s");
    assert_eq!(printer.status_calls.load(Ordering::SeqCst), 3);
}

#[test]
fn the_blocking_convenience_is_built_on_the_polls_and_gives_up_at_max_wait() {
    let printer = Reporting::new(vec![Ok(status(JobState::Processing))]);
    let registry = registry(&printer);
    let submitted = registry.submit_job(request("net:p1")).unwrap();
    let job = submitted.job.unwrap();
    let started = Instant::now();
    let policy = WaitPolicy::every(Duration::from_millis(20)).with_max_wait(Duration::from_millis(200));
    let outcome = wait_for_job(&registry, &job, policy).unwrap();
    assert!(matches!(outcome, WatchOutcome::TimedOut { last: Some(ref s) } if s.state == JobState::Processing), "{outcome:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(printer.status_calls.load(Ordering::SeqCst) >= 2);
    assert_eq!(wait_for_job(&registry, &JobRef::new("no-prefix"), WaitPolicy::default()).unwrap_err().kind(), ErrorKind::UnknownBackend);
    // Finishing is also seen by the convenience.
    let done = Reporting::new(vec![Ok(status(JobState::Processing)), Ok(status(JobState::CompletedWithWarnings))]);
    let registry = self::registry(&done);
    let outcome = wait_for_job(&registry, &JobRef::namespaced("net", "p1#7"), WaitPolicy::every(Duration::from_millis(10))).unwrap();
    assert!(outcome.is_success());
}

#[test]
fn a_watch_can_cancel_through_the_registry() {
    let printer = Reporting::new(vec![Ok(status(JobState::Processing)), Ok(status(JobState::Cancelled))]);
    let registry = registry(&printer);
    let cancel = CancelToken::new();
    cancel.cancel();
    let mut watch = registry
        .job_watch(&JobRef::namespaced("net", "p1#7"), WaitPolicy::every(Duration::from_millis(5)))
        .unwrap()
        .with_cancel(cancel)
        .with_clock(Arc::new(ManualClock::new()));
    let outcome = watch.run(&registry, &mut |_| {});
    assert_eq!(*printer.cancels.lock().unwrap(), ["p1#7"]);
    assert_eq!(outcome.error().map(|e| e.kind()), Some(ErrorKind::Cancelled));
}

#[test]
fn error_kinds_carry_a_default_retry_safety_a_backend_can_override() {
    let table = [
        (ErrorKind::Unsupported, false, RetrySafety::Safe),
        (ErrorKind::UnknownBackend, false, RetrySafety::Safe),
        (ErrorKind::NotFound, false, RetrySafety::Safe),
        (ErrorKind::InvalidRequest, false, RetrySafety::Safe),
        (ErrorKind::UnsupportedFormat, false, RetrySafety::Safe),
        (ErrorKind::ConsentRequired, false, RetrySafety::Safe),
        (ErrorKind::Cancelled, false, RetrySafety::Safe),
        (ErrorKind::Unavailable, true, RetrySafety::Safe),
        (ErrorKind::Busy, true, RetrySafety::Safe),
        (ErrorKind::Timeout, true, RetrySafety::OutcomeUnknown),
        (ErrorKind::Rejected, false, RetrySafety::Safe),
        (ErrorKind::Io, false, RetrySafety::Safe),
        (ErrorKind::Failed, false, RetrySafety::OutcomeUnknown),
    ];
    for (kind, retryable, safety) in table {
        let e = PrintError::new(kind, "x");
        assert_eq!((e.is_retryable(), e.retry_safety(), e.phase(), e.retry_after()), (retryable, safety, None, None), "{kind:?}");
        assert_eq!(e.outcome_unknown(), safety == RetrySafety::OutcomeUnknown);
    }
    let e = PrintError::new(ErrorKind::Timeout, "x")
        .with_safety(RetrySafety::Idempotent)
        .with_phase(Phase::Poll)
        .with_retry_after(Duration::from_secs(3));
    assert_eq!((e.retry_safety(), e.phase(), e.retry_after()), (RetrySafety::Idempotent, Some(Phase::Poll), Some(Duration::from_secs(3))));
    assert!(e.is_retryable() && !e.outcome_unknown());
    assert!(UserConfirmed::attest(Some(true)).is_ok());
}
