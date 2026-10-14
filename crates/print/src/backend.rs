//! The [`Backend`] trait and the neutral types a backend speaks.
//!
//! A backend is one way of getting a document onto paper: the system spooler (CUPS, the Windows
//! spooler) or, later, a network protocol (WS-Print, IPP). The [`Registry`](crate::Registry) holds
//! several at once, namespaces their printer ids (`"<backend>:<native id>"`), aggregates their
//! printer lists and routes details and jobs by that prefix.
//!
//! # Writing a backend
//!
//! * **Object safe, blocking, `Send + Sync`.** The registry stores `Arc<dyn Backend>` and calls
//!   from whatever thread the app uses (a worker, by convention). Every call is **bounded**: a
//!   network backend blocks on its own transport for at most its per-request timeouts, never for
//!   the length of a print. There is no async runtime in this crate.
//! * **Submit and watch are separate.** [`Backend::submit`] returns a [`Submitted`] as soon as the
//!   printer has *accepted* the document. What happens next is an optional capability: declare it
//!   with [`Backend::watch_support`] and answer [`Backend::job_status`] (one request, no waiting)
//!   and [`Backend::cancel_job`]. A backend that cannot say keeps the defaults: the status is
//!   [`JobState::Unknown`], "accepted, outcome unknown". Callers follow a job with a
//!   [`crate::JobWatch`]; [`crate::wait_for_job`] is the blocking convenience for a CLI.
//! * **Cancellation and progress travel in the request**: [`SubmitRequest::cancel`] is polled at
//!   the backend's own checkpoints (between sheets, between network round trips); progress is a
//!   callback of [`Progress`] events whose total is optional. Neither is required:
//!   the built-in spooler backends check the token only before they start.
//! * **Per-job options**: [`SubmitRequest::options`] ([`JobOptions`]) carries media, orientation,
//!   quality, resolution and colour as hints, and the page count the app knows. Apply what you can,
//!   say so in [`PrinterDetails::honoured`], and list what you dropped in [`Submitted::warnings`].
//! * **Ids**: a backend names printers with its own native ids and never adds its prefix; the
//!   registry does. A native id may contain colons; only the first colon separates the prefix.
//! * **Capabilities are lists and may be unknown.** [`PrinterDetails`] uses `Option`/empty lists
//!   for "not published". Options such as duplex, colour and scaling are hints a backend may
//!   ignore; [`PrinterDetails::honoured`] tells a UI which ones it applies.
//! * **Status is optional.** `None` is "cannot say", not "all clear". Spoolers report after the
//!   fact; a network protocol can report state up front.
//! * **Documents are bytes plus a declared format.** A backend declares what it accepts with
//!   [`Backend::accepted_formats`]; the registry refuses anything else before the backend is called
//!   ([`ErrorKind::UnsupportedFormat`]). A backend that converts (for example PDF to raster) does
//!   so inside `submit`.
//! * **Consent.** A backend whose [`Backend::class`] is [`BackendClass::Network`] only receives
//!   requests that carry a [`UserConfirmed`]; the registry enforces it so a backend cannot forget.
//! * **Errors are neutral**: choose an [`ErrorKind`] and attach the native code with
//!   [`PrintError::with_native`]. Say where it failed ([`PrintError::with_phase`]) and whether a
//!   retry could print twice ([`PrintError::with_safety`]; [`RetrySafety::OutcomeUnknown`] after a
//!   request that may have arrived). Offer a way to reconcile if you can.
//! * **Registration**: `registry.register(Box::new(MyBackend::new()))`, once at start-up. The
//!   process-wide registry ([`crate::registry`]) already holds the platform's built-in backend.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::job::{JobId, JobState, JobStatus, Submitted, WatchSupport};
use crate::options::JobOptions;
use crate::remote::{Fingerprint, JobFilter, JobQueryCaps, Reconciled, RemoteJob, reconcile_jobs};
use crate::retry::{Phase, RetrySafety};
use crate::tag::{JobTag, TagSupport};
use crate::{Error, Job, PrinterOption};

/// A native error code and the domain that owns it. `domain` names the code space:
/// `"win32"`, `"ipp"`, `"wsd-fault"`, `"cups"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeCode {
    pub domain: &'static str,
    pub code: i64,
    /// The code's symbolic name, when known.
    pub name: Option<&'static str>,
}

/// What went wrong, independent of the backend. The kind and [`ErrorKind::is_retryable`] are the
/// contract callers branch on; the message is for people.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// No backend is registered, or the platform has no print stack.
    Unsupported,
    /// The printer id names a backend that is not registered (or has no prefix).
    UnknownBackend,
    /// The backend does not know the printer, or there is no default printer.
    NotFound,
    /// The request is malformed or asks for something the printer cannot do.
    InvalidRequest,
    /// The printer or backend does not accept the document's format.
    UnsupportedFormat,
    /// A network-class backend was asked to print without a [`UserConfirmed`].
    ConsentRequired,
    /// The caller cancelled.
    Cancelled,
    /// The spooler, device or network is not reachable right now.
    Unavailable,
    /// The device is busy; try again later.
    Busy,
    /// The device did not answer in time.
    Timeout,
    /// The printer or driver refused the job.
    Rejected,
    /// Reading or writing the document failed.
    Io,
    /// Anything else; the message says more.
    Failed,
}

impl ErrorKind {
    /// Whether the condition is transient: trying the same request again later can succeed.
    /// This says nothing about whether a retry is *safe*; see [`PrintError::retry_safety`].
    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::Unavailable | Self::Busy | Self::Timeout)
    }

    /// The retry safety an error of this kind has unless its backend says otherwise.
    ///
    /// Refusals and conditions that arise before anything is accepted are [`RetrySafety::Safe`];
    /// a timeout or a failure of unknown cause may have followed a delivered request, so it is
    /// [`RetrySafety::OutcomeUnknown`]. A backend that knows better (a connect failure, a query)
    /// overrides it with [`PrintError::with_safety`].
    pub const fn default_retry_safety(self) -> RetrySafety {
        match self {
            Self::Timeout | Self::Failed => RetrySafety::OutcomeUnknown,
            _ => RetrySafety::Safe,
        }
    }
}

/// A print failure: a neutral [`ErrorKind`], a message and optionally the backend's native code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrintError {
    kind: ErrorKind,
    message: String,
    native: Option<NativeCode>,
    phase: Option<Phase>,
    safety: Option<RetrySafety>,
    retry_after: Option<Duration>,
}

impl PrintError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), native: None, phase: None, safety: None, retry_after: None }
    }

    /// Where the failure happened ([`Phase`]).
    #[must_use]
    pub fn with_phase(mut self, phase: Phase) -> Self {
        self.phase = Some(phase);
        self
    }

    /// Whether repeating the request could duplicate it ([`RetrySafety`]).
    #[must_use]
    pub fn with_safety(mut self, safety: RetrySafety) -> Self {
        self.safety = Some(safety);
        self
    }

    /// How long the device (or the backend's sensible default for a busy device) says to wait.
    #[must_use]
    pub fn with_retry_after(mut self, wait: Duration) -> Self {
        self.retry_after = Some(wait);
        self
    }

    /// Attach the backend's own error code.
    #[must_use]
    pub fn with_native(mut self, native: NativeCode) -> Self {
        self.native = Some(native);
        self
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn native(&self) -> Option<NativeCode> {
        self.native
    }

    /// Whether the condition is transient (see [`ErrorKind::is_retryable`]). Not a licence to
    /// resend: check [`Self::retry_safety`].
    pub fn is_retryable(&self) -> bool {
        self.kind.is_retryable()
    }

    /// Where it failed, when the backend said.
    pub fn phase(&self) -> Option<Phase> {
        self.phase
    }

    /// Whether a retry could print twice. When the backend did not say, the kind's default
    /// ([`ErrorKind::default_retry_safety`]).
    pub fn retry_safety(&self) -> RetrySafety {
        self.safety.unwrap_or_else(|| self.kind.default_retry_safety())
    }

    /// The wait the device or backend suggests before another try, when there is one.
    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    /// The request may have been delivered: reconcile (find out whether the job exists) before
    /// submitting again.
    pub fn outcome_unknown(&self) -> bool {
        self.retry_safety() == RetrySafety::OutcomeUnknown
    }
}

impl std::fmt::Display for PrintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PrintError {}

/// The legacy spooler error keeps its message; it carries no more detail than that, so the kind
/// is [`ErrorKind::Failed`].
impl From<Error> for PrintError {
    fn from(error: Error) -> Self {
        Self::new(ErrorKind::Failed, error.to_string())
    }
}

/// The format of the bytes in a [`Document`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DocumentFormat {
    /// `application/pdf`, what the apps generate.
    Pdf,
    /// `image/pwg-raster`.
    PwgRaster,
    /// `application/postscript`.
    PostScript,
    /// `application/octet-stream`: the printer's own page description language.
    OctetStream,
}

impl DocumentFormat {
    pub const fn mime(self) -> &'static str {
        match self {
            Self::Pdf => "application/pdf",
            Self::PwgRaster => "image/pwg-raster",
            Self::PostScript => "application/postscript",
            Self::OctetStream => "application/octet-stream",
        }
    }
}

/// Document bytes and their declared format. The crate does not sniff or convert.
#[derive(Clone, Copy, Debug)]
pub struct Document<'a> {
    pub format: DocumentFormat,
    pub bytes: &'a [u8],
}

impl<'a> Document<'a> {
    pub fn new(format: DocumentFormat, bytes: &'a [u8]) -> Self {
        Self { format, bytes }
    }

    pub fn pdf(bytes: &'a [u8]) -> Self {
        Self::new(DocumentFormat::Pdf, bytes)
    }
}

/// Whether a backend stays on this machine or talks to devices over a network.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BackendClass {
    /// The system spooler: the user already chose the queue.
    #[default]
    Local,
    /// Connects to devices itself; every job needs a [`UserConfirmed`].
    Network,
}

/// Proof that a person agreed to this print. It carries no policy of its own: the registry only
/// requires one for [`BackendClass::Network`] backends, and an app decides how a person gets to
/// say yes. It can be obtained only through [`Self::attest`] or [`Self::interactive`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserConfirmed(());

impl UserConfirmed {
    /// The guard for agent and automation callers: pass the `user_confirmed` argument as given.
    /// Absent or false is [`ErrorKind::ConsentRequired`], with a message that says what to do.
    pub fn attest(user_confirmed: Option<bool>) -> Result<Self, PrintError> {
        if user_confirmed == Some(true) {
            Ok(Self(()))
        } else {
            Err(PrintError::new(
                ErrorKind::ConsentRequired,
                "printing to a network printer needs user_confirmed: true, set only after the person has agreed to this print",
            ))
        }
    }

    /// For interactive callers where the person has just pressed Print. Do not use it for anything
    /// an agent or script can reach.
    pub fn interactive() -> Self {
        Self(())
    }
}

/// A cooperative cancellation flag shared between the caller and a running job. Clones share the
/// flag.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// A printer in a list. In a [`Registry`](crate::Registry) result `id` is namespaced
/// (`"<backend>:<native id>"`); a backend returns its native id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrinterRef {
    pub id: String,
    /// What a person sees.
    pub name: String,
    pub default: bool,
    /// Whether the printer takes two-sided jobs; `None` when unknown.
    pub two_sided: Option<bool>,
}

/// The job settings of [`Job`] and [`JobOptions`] a printer may honour. Everything else is a hint
/// it ignores. ([`JobOptions::colour`] is the `Colour` hint, as is `Job::grayscale`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum JobHint {
    Copies,
    Collate,
    Duplex,
    Colour,
    Scaling,
    /// `Job::options`, the driver's own keywords.
    DriverOptions,
    /// [`JobOptions::media`].
    Media,
    /// [`JobOptions::orientation`].
    Orientation,
    /// [`JobOptions::quality`].
    Quality,
    /// [`JobOptions::resolution`].
    Resolution,
}

/// The state a printer reports before a job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PrinterState {
    Idle,
    Processing,
    Stopped,
    Offline,
}

/// A printer's reported state. Absent from [`PrinterDetails`] when the backend cannot say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrinterStatus {
    pub state: PrinterState,
    /// Human-readable reasons the device gives (`"paper empty"`), as reported.
    pub reasons: Vec<String>,
}

/// What a printer can do. Every list may be empty or `None`, which means "not published", not
/// "none supported".
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PrinterDetails {
    pub id: String,
    pub name: String,
    pub default: bool,
    /// The formats this printer takes; empty when the backend publishes only its own list.
    pub formats: Vec<DocumentFormat>,
    /// Driver job options (CUPS: the PPD's). Empty when none or unknown.
    pub options: Vec<PrinterOption>,
    pub two_sided: Option<bool>,
    /// Whether colour is available; `None` when unknown.
    pub colour: Option<bool>,
    /// Paper names the printer offers; empty when not published.
    pub paper: Vec<String>,
    pub max_copies: Option<u32>,
    /// Which [`Job`] settings this backend applies. `None`: not published, treat as unknown.
    pub honoured: Option<Vec<JobHint>>,
    pub status: Option<PrinterStatus>,
}

/// A message about an accepted job and the backend's own job id, if any. The older view of a
/// [`Submitted`] ([`Submitted::receipt`]); receipts are messages, not portable job ids.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    /// The backend's registry name.
    pub backend: &'static str,
    pub message: String,
    /// The backend's job id, when it has one.
    pub job_id: Option<String>,
}

/// One progress event: sheets processed so far and, **when anyone knows it**, how many there will
/// be. A network printer typically reports sheets completed and no total; the registry then fills
/// `total` from [`JobOptions::expected_pages`] (as an estimate of the sheets, see
/// [`crate::ExpectedTotals`]) and a backend-reported total wins. A UI shows a determinate bar when
/// `total` is `Some` and an indeterminate one ("3 sheets") otherwise.
///
/// The older [`crate::submit_with_progress`] keeps its `(processed, sheets)` closure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Progress {
    pub done: u32,
    pub total: Option<u32>,
}

impl Progress {
    pub const fn new(done: u32, total: Option<u32>) -> Self {
        Self { done, total }
    }

    /// An event with a known total.
    pub const fn of(done: u32, total: u32) -> Self {
        Self { done, total: Some(total) }
    }

    /// An event whose total is not known.
    pub const fn done_only(done: u32) -> Self {
        Self { done, total: None }
    }

    /// `done / total` in `0.0..=1.0` when the total is known and not zero.
    pub fn fraction(&self) -> Option<f32> {
        match self.total {
            Some(total) if total > 0 => Some((self.done.min(total) as f32) / (total as f32)),
            _ => None,
        }
    }

    /// Fill a missing (or zero) total from `expected`; a total that is present is kept.
    #[must_use]
    pub fn or_expected(self, expected: Option<u32>) -> Self {
        match self.total {
            Some(total) if total > 0 => self,
            _ => Self { done: self.done, total: expected.filter(|t| *t > 0) },
        }
    }
}

/// The progress callback of a [`SubmitRequest`].
pub type ProgressCallback<'a> = &'a mut dyn FnMut(Progress);

/// One print request. [`Job::printer`] is the printer: a namespaced id when it goes to a
/// [`Registry`](crate::Registry), the native id when a backend receives it; `None` is the default.
/// [`SubmitRequest::options`] carries the per-job hints beside the spooler's own [`Job`] settings;
/// see [`JobOptions`].
pub struct SubmitRequest<'a> {
    pub document: Document<'a>,
    pub job: Job,
    /// Per-job hints (media, orientation, quality, resolution, colour, expected page count).
    pub options: JobOptions,
    pub consent: Option<UserConfirmed>,
    pub cancel: CancelToken,
    pub progress: Option<ProgressCallback<'a>>,
    /// The idempotency tag of the print this request belongs to ([`JobTag`]); the same for every
    /// attempt. A backend carries it where its protocol allows ([`Backend::tag_support`]); the
    /// registry removes it for a backend that cannot.
    pub tag: Option<JobTag>,
}

impl<'a> SubmitRequest<'a> {
    pub fn new(document: Document<'a>, job: Job) -> Self {
        Self { document, job, options: JobOptions::default(), consent: None, cancel: CancelToken::new(), progress: None, tag: None }
    }

    /// Put this idempotency tag on the job, where the backend can carry it. See [`JobTag`].
    #[must_use]
    pub fn with_tag(mut self, tag: JobTag) -> Self {
        self.tag = Some(tag);
        self
    }

    /// Per-job hints; see [`JobOptions`].
    #[must_use]
    pub fn with_options(mut self, options: JobOptions) -> Self {
        self.options = options;
        self
    }

    #[must_use]
    pub fn with_consent(mut self, consent: UserConfirmed) -> Self {
        self.consent = Some(consent);
        self
    }

    #[must_use]
    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    #[must_use]
    pub fn with_progress(mut self, progress: ProgressCallback<'a>) -> Self {
        self.progress = Some(progress);
        self
    }
}

/// One way of printing. Ids passed in and returned are the backend's **native** ids; the
/// [`Registry`](crate::Registry) adds and strips the `"<name>:"` prefix. See the module docs.
pub trait Backend: Send + Sync {
    /// The registry name and id prefix: short, lowercase ASCII letters, digits, `-` and `_`
    /// (`"cups"`, `"spooler"`, `"wsd"`, `"ipp"`).
    fn name(&self) -> &'static str;

    /// Whether jobs need a [`UserConfirmed`]. Defaults to [`BackendClass::Local`].
    fn class(&self) -> BackendClass {
        BackendClass::Local
    }

    /// The formats [`Backend::submit`] takes. Defaults to PDF.
    fn accepted_formats(&self) -> &[DocumentFormat] {
        &[DocumentFormat::Pdf]
    }

    /// The printers the backend lists. Opens none of them.
    fn list_printers(&self) -> Result<Vec<PrinterRef>, PrintError>;

    /// What one printer (a native id) can do. [`ErrorKind::NotFound`] for an unknown printer.
    fn printer_details(&self, native_id: &str) -> Result<PrinterDetails, PrintError>;

    /// Hand the document to the printer and return **as soon as it has accepted it**: the call is
    /// bounded by the backend's own per-request timeouts and never waits for the job to print.
    /// `request.job.printer` is a native id or `None` for the backend's default. The registry has
    /// already checked the format, the consent and that the token is not cancelled.
    ///
    /// Return [`Submitted::job`] when the backend has a handle for the job (`JobId::new(self.name(), native)`;
    /// the registry stamps the backend's name on it either way). An error that follows a request which may have arrived must say
    /// so: [`PrintError::with_safety`]`(`[`RetrySafety::OutcomeUnknown`]`)`.
    fn submit(&self, request: SubmitRequest<'_>) -> Result<Submitted, PrintError>;

    /// Whether and how this backend can report on a job after [`Backend::submit`]. Defaults to
    /// [`WatchSupport::None`]: "accepted, outcome unknown".
    fn watch_support(&self) -> WatchSupport {
        WatchSupport::None
    }

    /// How often a caller should ask [`Backend::job_status`]; `None` when it should not.
    /// Defaults from [`Backend::watch_support`]: 2 s for polling, 15 s as a safety net for events.
    fn recommended_poll_interval(&self) -> Option<Duration> {
        match self.watch_support() {
            WatchSupport::None => None,
            WatchSupport::Poll => Some(Duration::from_secs(2)),
            WatchSupport::Events => Some(Duration::from_secs(15)),
        }
    }

    /// The job's status: **one bounded request**, no waiting. Its errors are idempotent (repeating
    /// is harmless). A job the device no longer knows is [`ErrorKind::NotFound`] (finished jobs are
    /// forgotten; the message says so). The default answers [`JobState::Unknown`]: a backend that
    /// cannot report keeps it, and callers do not poll it ([`WatchSupport::None`]).
    fn job_status(&self, job: &JobId) -> Result<JobStatus, PrintError> {
        let _ = job;
        Ok(JobStatus::new(JobState::Unknown))
    }

    /// Ask the device to cancel an accepted job. Cancelling a job that already ended is not an
    /// error. The default is [`ErrorKind::Unsupported`].
    fn cancel_job(&self, job: &JobId) -> Result<(), PrintError> {
        let _ = job;
        Err(PrintError::new(ErrorKind::Unsupported, format!("the {} backend cannot cancel a job once it is accepted", self.name())))
    }

    /// Whether and where this backend carries a [`SubmitRequest::tag`], and whether
    /// [`Backend::list_jobs`] gives it back. Defaults to [`TagSupport::NONE`]. A backend that
    /// supports a placement applies the tag itself (a name suffix with [`JobTag::apply_to`], within
    /// its own length limit) and exactly once.
    fn tag_support(&self) -> TagSupport {
        TagSupport::NONE
    }

    /// What [`Backend::list_jobs`] can fill in. Defaults to [`JobQueryCaps::NONE`].
    fn job_query_caps(&self) -> JobQueryCaps {
        JobQueryCaps::NONE
    }

    /// The jobs of one printer (a native id): its active jobs and, when the source keeps them,
    /// finished ones ([`JobQueryCaps::history`]). **One bounded, idempotent read.** Foreign jobs
    /// are listed too; [`RemoteJob::ours`] is set from [`JobFilter::tag`] (use
    /// [`crate::filter_jobs`]). The default is [`ErrorKind::Unsupported`]: a spooler that cannot
    /// list keeps it, and says so in [`Backend::job_query_caps`].
    fn list_jobs(&self, printer_native: &str, filter: &JobFilter) -> Result<Vec<RemoteJob>, PrintError> {
        let _ = (printer_native, filter);
        Err(PrintError::new(ErrorKind::Unsupported, format!("the {} backend cannot list jobs", self.name())).with_safety(RetrySafety::Safe))
    }

    /// After an attempt whose outcome is unknown: find the job it may have created. The default
    /// lists jobs ([`Backend::list_jobs`]) and decides with [`reconcile_jobs`]: by tag when the
    /// backend can match it, else by title, user and time, which may be
    /// [`Reconciled::Ambiguous`]. A backend overrides it to fill in what only it knows (its own
    /// user name).
    fn find_job(&self, printer_native: &str, fingerprint: &Fingerprint) -> Result<Reconciled, PrintError> {
        let support = self.tag_support();
        let jobs = self.list_jobs(printer_native, &fingerprint.filter(support.matchable))?;
        Ok(reconcile_jobs(jobs, fingerprint, support))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_totals_are_optional_and_a_reported_one_wins() {
        assert_eq!(Progress::done_only(2).fraction(), None);
        assert_eq!(Progress::of(1, 4).fraction(), Some(0.25));
        assert_eq!(Progress::of(9, 4).fraction(), Some(1.0));
        assert_eq!(Progress::of(1, 0).fraction(), None);
        assert_eq!(Progress::done_only(2).or_expected(Some(5)), Progress::of(2, 5));
        assert_eq!(Progress::of(2, 3).or_expected(Some(5)), Progress::of(2, 3));
        assert_eq!(Progress::of(2, 0).or_expected(Some(5)), Progress::of(2, 5), "a zero total is no total");
        assert_eq!(Progress::done_only(2).or_expected(Some(0)), Progress::done_only(2));
        assert_eq!(Progress::new(1, None), Progress::done_only(1));
    }
}
