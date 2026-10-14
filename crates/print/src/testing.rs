//! Test doubles for apps that print through this crate. Cargo feature `testing`; it has no effect
//! on default builds and adds no dependency. Never enable it in a shipped binary.
//!
//! [`FakeBackend`] is a configurable [`Backend`] for a [`Registry`](crate::Registry) of your own:
//! it lists the printers you give it, answers details, takes documents, reports a scripted job
//! status, and records every request so the test can assert on what the app sent (the
//! [`Submission`]s, the cancels, how often the status was asked). Clones share their state, so one
//! clone goes into the registry and the other stays with the test:
//!
//! ```
//! use craft_print::testing::FakeBackend;
//! use craft_print::{Document, Job, JobOptions, JobState, JobStatus, Registry, SubmitRequest, WatchSupport};
//!
//! let fake = FakeBackend::new("fake")
//!     .with_printer("p1", true)
//!     .with_watch(WatchSupport::Poll)
//!     .with_status_script(vec![Ok(JobStatus::new(JobState::Processing)), Ok(JobStatus::new(JobState::Completed))]);
//! let registry = Registry::new();
//! registry.register(Box::new(fake.clone()))?;
//!
//! let request = SubmitRequest::new(Document::pdf(b"%PDF-1.7"), Job::default()).with_options(JobOptions::new().with_expected_pages(2));
//! let submitted = registry.submit_job(request)?;
//! let job = submitted.job.expect("the fake hands out a job id");
//! assert_eq!(registry.job_status(&job)?.state, JobState::Processing);
//! assert_eq!(fake.submissions()[0].options.expected_pages, Some(2));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! For the WS-Print backend see `craft_print::wsd::testing` (an in-memory WS-Print printer; needs
//! the `wsd` feature too).

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::backend::{
    Backend, BackendClass, DocumentFormat, ErrorKind, JobHint, PrintError, PrinterDetails, PrinterRef, PrinterStatus, Progress, SubmitRequest,
};
use crate::job::{JobId, JobState, JobStatus, Submitted, WatchSupport};
use crate::options::JobOptions;
use crate::{Job, PrinterOption};

/// What a [`FakeBackend`] was asked to print.
#[derive(Clone, Debug)]
pub struct Submission {
    /// The native printer id the backend received (`None`: the default printer).
    pub printer: Option<String>,
    pub format: DocumentFormat,
    pub bytes: Vec<u8>,
    /// The whole [`Job`] as the backend saw it (the registry has already removed the prefix from
    /// `printer`).
    pub job: Job,
    pub options: JobOptions,
    /// Whether the request carried a [`crate::UserConfirmed`].
    pub consented: bool,
}

type JobHandle = dyn Fn(Option<&str>, u32) -> Option<String> + Send + Sync;

#[derive(Default)]
struct Recorded {
    submissions: Vec<Submission>,
    status_calls: u32,
    detail_calls: u32,
    cancels: Vec<String>,
    script: Vec<Result<JobStatus, PrintError>>,
}

#[derive(Clone)]
struct Config {
    name: &'static str,
    class: BackendClass,
    formats: Vec<DocumentFormat>,
    formats_per_printer: bool,
    printers: Vec<PrinterRef>,
    details: PrinterDetails,
    list_error: Option<PrintError>,
    details_error: Option<PrintError>,
    submit_error: Option<PrintError>,
    message: String,
    job_handle: Arc<JobHandle>,
    progress: Vec<Progress>,
    watch: WatchSupport,
    poll_interval: Option<Duration>,
    can_cancel: bool,
    submitted_status: Option<JobStatus>,
}

/// A configurable [`Backend`] for tests; see the [module documentation](self). Build it with
/// [`Self::new`] and the `with_*` methods, register a clone, keep a clone to inspect.
///
/// Defaults: a local backend taking PDF, no printers, `submit` accepts and hands out the job handle
/// `<printer>#<n>` (n counts from 1), no progress, [`WatchSupport::None`], and a status that is
/// [`JobState::Unknown`] unless a script is set.
#[derive(Clone)]
pub struct FakeBackend {
    config: Arc<Config>,
    recorded: Arc<Mutex<Recorded>>,
}

impl std::fmt::Debug for FakeBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeBackend").field("name", &self.config.name).finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl FakeBackend {
    /// A fake registered as `name` (the registry's id prefix: lowercase letters, digits, `-`, `_`).
    pub fn new(name: &'static str) -> Self {
        let config = Config {
            name,
            class: BackendClass::Local,
            formats: vec![DocumentFormat::Pdf],
            formats_per_printer: false,
            printers: Vec::new(),
            details: PrinterDetails::default(),
            list_error: None,
            details_error: None,
            submit_error: None,
            message: "accepted".into(),
            job_handle: Arc::new(|printer, n| Some(format!("{}#{n}", printer.unwrap_or_default()))),
            progress: Vec::new(),
            watch: WatchSupport::None,
            poll_interval: None,
            can_cancel: true,
            submitted_status: None,
        };
        Self { config: Arc::new(config), recorded: Arc::default() }
    }

    fn configured(mut self, change: impl FnOnce(&mut Config)) -> Self {
        // Configuration is set before the fake is cloned; if it was cloned, the clones keep the old one.
        change(Arc::make_mut(&mut self.config));
        self
    }

    /// [`BackendClass::Network`] fakes need a [`crate::UserConfirmed`] on every request.
    #[must_use]
    pub fn with_class(self, class: BackendClass) -> Self {
        self.configured(|c| c.class = class)
    }

    /// The formats [`Backend::accepted_formats`] answers (default PDF).
    #[must_use]
    pub fn with_formats(self, formats: Vec<DocumentFormat>) -> Self {
        self.configured(|c| c.formats = formats)
    }

    /// Whether [`Backend::formats_per_printer`] is `true` (default `false`): the registry then reads
    /// the details' `formats` before it decides on a conversion, like it does for a network backend.
    #[must_use]
    pub fn with_formats_per_printer(self, per_printer: bool) -> Self {
        self.configured(|c| c.formats_per_printer = per_printer)
    }

    /// Shorthand for the `formats` (the printer's own list) of the details.
    #[must_use]
    pub fn with_printer_formats(self, formats: Vec<DocumentFormat>) -> Self {
        self.configured(|c| c.details.formats = formats)
    }

    /// Shorthand for the `resolutions` of the details.
    #[must_use]
    pub fn with_resolutions(self, resolutions: Vec<crate::Resolution>) -> Self {
        self.configured(|c| c.details.resolutions = resolutions)
    }

    /// Add a printer to the listing (a native id; the name is `Printer <id>`).
    #[must_use]
    pub fn with_printer(self, id: &str, default: bool) -> Self {
        self.with_printer_ref(PrinterRef { id: id.into(), name: format!("Printer {id}"), default, two_sided: None })
    }

    /// Add a fully specified printer to the listing.
    #[must_use]
    pub fn with_printer_ref(self, printer: PrinterRef) -> Self {
        self.configured(|c| c.printers.push(printer))
    }

    /// The details every listed printer answers (its `id` and `name` are filled in per printer).
    /// Without this the details are empty ("not published") apart from the id and name.
    #[must_use]
    pub fn with_details(self, details: PrinterDetails) -> Self {
        self.configured(|c| c.details = details)
    }

    /// Shorthand for the `honoured` list of the details.
    #[must_use]
    pub fn with_honoured(self, honoured: Vec<JobHint>) -> Self {
        self.configured(|c| c.details.honoured = Some(honoured))
    }

    /// Shorthand for the driver options of the details.
    #[must_use]
    pub fn with_printer_options(self, options: Vec<PrinterOption>) -> Self {
        self.configured(|c| c.details.options = options)
    }

    /// Shorthand for the printer state of the details.
    #[must_use]
    pub fn with_printer_status(self, status: PrinterStatus) -> Self {
        self.configured(|c| c.details.status = Some(status))
    }

    /// `list_printers` fails with this error.
    #[must_use]
    pub fn failing_list(self, error: PrintError) -> Self {
        self.configured(|c| c.list_error = Some(error))
    }

    /// `printer_details` fails with this error (for every printer).
    #[must_use]
    pub fn failing_details(self, error: PrintError) -> Self {
        self.configured(|c| c.details_error = Some(error))
    }

    /// `submit` fails with this error (after recording the request).
    #[must_use]
    pub fn failing_submit(self, error: PrintError) -> Self {
        self.configured(|c| c.submit_error = Some(error))
    }

    /// The sentence `submit` answers with (default `accepted`).
    #[must_use]
    pub fn with_message(self, message: &str) -> Self {
        self.configured(|c| c.message = message.to_owned())
    }

    /// How `submit` names the job it accepted, from the native printer id and a counter that starts
    /// at 1. Returning `None` hands out no job handle (a spooler that cannot say). The default is
    /// `<printer>#<n>`.
    #[must_use]
    pub fn with_job_handle(self, handle: impl Fn(Option<&str>, u32) -> Option<String> + Send + Sync + 'static) -> Self {
        self.configured(|c| c.job_handle = Arc::new(handle))
    }

    /// Progress events `submit` reports to the request's callback, in order.
    #[must_use]
    pub fn with_progress_events(self, events: Vec<Progress>) -> Self {
        self.configured(|c| c.progress = events)
    }

    /// What the backend says it can do after acceptance. [`WatchSupport::Poll`] and `Events`
    /// default the recommended interval to two and fifteen seconds unless
    /// [`Self::with_poll_interval`] sets one.
    #[must_use]
    pub fn with_watch(self, watch: WatchSupport) -> Self {
        self.configured(|c| c.watch = watch)
    }

    #[must_use]
    pub fn with_poll_interval(self, interval: Duration) -> Self {
        self.configured(|c| c.poll_interval = Some(interval))
    }

    /// Whether `cancel_job` works (default) or answers [`ErrorKind::Unsupported`].
    #[must_use]
    pub fn with_cancel_support(self, can_cancel: bool) -> Self {
        self.configured(|c| c.can_cancel = can_cancel)
    }

    /// The status `submit` returns with the accepted job (a printer that reports up front).
    #[must_use]
    pub fn with_submitted_status(self, status: JobStatus) -> Self {
        self.configured(|c| c.submitted_status = Some(status))
    }

    /// The answers of successive `job_status` calls; the last one repeats. Empty (the default)
    /// answers [`JobState::Unknown`] like a backend that cannot report.
    #[must_use]
    pub fn with_status_script(self, script: Vec<Result<JobStatus, PrintError>>) -> Self {
        lock(&self.recorded).script = script;
        self
    }

    /// Everything `submit` received, oldest first.
    pub fn submissions(&self) -> Vec<Submission> {
        lock(&self.recorded).submissions.clone()
    }

    /// How many times `printer_details` was asked (the registry asks to choose a conversion).
    pub fn detail_calls(&self) -> u32 {
        lock(&self.recorded).detail_calls
    }

    /// How many times `job_status` was asked.
    pub fn status_calls(&self) -> u32 {
        lock(&self.recorded).status_calls
    }

    /// The native handles of the jobs `cancel_job` was asked to cancel.
    pub fn cancels(&self) -> Vec<String> {
        lock(&self.recorded).cancels.clone()
    }
}

impl Backend for FakeBackend {
    fn name(&self) -> &'static str {
        self.config.name
    }

    fn class(&self) -> BackendClass {
        self.config.class
    }

    fn accepted_formats(&self) -> &[DocumentFormat] {
        &self.config.formats
    }

    fn formats_per_printer(&self) -> bool {
        self.config.formats_per_printer
    }

    fn list_printers(&self) -> Result<Vec<PrinterRef>, PrintError> {
        match &self.config.list_error {
            Some(error) => Err(error.clone()),
            None => Ok(self.config.printers.clone()),
        }
    }

    fn printer_details(&self, native_id: &str) -> Result<PrinterDetails, PrintError> {
        {
            let mut recorded = lock(&self.recorded);
            recorded.detail_calls = recorded.detail_calls.saturating_add(1);
        }
        if let Some(error) = &self.config.details_error {
            return Err(error.clone());
        }
        let printer = self
            .config
            .printers
            .iter()
            .find(|p| p.id == native_id)
            .ok_or_else(|| PrintError::new(ErrorKind::NotFound, format!("the fake backend has no printer {native_id}")))?;
        Ok(PrinterDetails { id: printer.id.clone(), name: printer.name.clone(), default: printer.default, ..self.config.details.clone() })
    }

    fn submit(&self, mut request: SubmitRequest<'_>) -> Result<Submitted, PrintError> {
        let number = {
            let mut recorded = lock(&self.recorded);
            recorded.submissions.push(Submission {
                printer: request.job.printer.clone(),
                format: request.document.format,
                bytes: request.document.bytes.to_vec(),
                job: request.job.clone(),
                options: request.options.clone(),
                consented: request.consent.is_some(),
            });
            u32::try_from(recorded.submissions.len()).unwrap_or(u32::MAX)
        };
        if let Some(report) = request.progress.as_deref_mut() {
            for event in &self.config.progress {
                report(*event);
            }
        }
        if let Some(error) = &self.config.submit_error {
            return Err(error.clone());
        }
        let mut submitted = Submitted::new(self.config.name, self.config.message.clone());
        if let Some(native) = (self.config.job_handle)(request.job.printer.as_deref(), number) {
            submitted = submitted.with_job(JobId::new(self.config.name, native));
        }
        if let Some(status) = &self.config.submitted_status {
            submitted = submitted.with_status(status.clone());
        }
        Ok(submitted)
    }

    fn watch_support(&self) -> WatchSupport {
        self.config.watch
    }

    fn recommended_poll_interval(&self) -> Option<Duration> {
        self.config.poll_interval.or(match self.config.watch {
            WatchSupport::None => None,
            WatchSupport::Poll => Some(Duration::from_secs(2)),
            WatchSupport::Events => Some(Duration::from_secs(15)),
        })
    }

    fn job_status(&self, job: &JobId) -> Result<JobStatus, PrintError> {
        let _ = job;
        let mut recorded = lock(&self.recorded);
        recorded.status_calls = recorded.status_calls.saturating_add(1);
        match recorded.script.len() {
            0 => Ok(JobStatus::new(JobState::Unknown)),
            1 => recorded.script.first().cloned().unwrap_or_else(|| Ok(JobStatus::new(JobState::Unknown))),
            _ => recorded.script.remove(0),
        }
    }

    fn cancel_job(&self, job: &JobId) -> Result<(), PrintError> {
        if !self.config.can_cancel {
            return Err(PrintError::new(ErrorKind::Unsupported, format!("the {} backend cannot cancel a job once it is accepted", self.config.name)));
        }
        lock(&self.recorded).cancels.push(job.native().to_owned());
        Ok(())
    }
}
