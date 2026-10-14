//! What a backend hands back once a printer has accepted a document, and how a job's progress is
//! described afterwards. See "Job lifecycle" in the crate README for the reasoning.
//!
//! Submitting and watching are separate calls. [`crate::backends::Backend::submit`] returns a
//! [`Submitted`] as soon as the printer has accepted the document; everything after that (is it
//! printing, did it finish, cancel it) is a [`JobId`] plus one bounded request at a time
//! ([`crate::backends::Backend::job_status`], [`crate::backends::Backend::cancel_job`]), driven by
//! the caller (see [`crate::JobWatch`]).

use std::time::{Duration, SystemTime};

use crate::backend::{ErrorKind, PrintError, Receipt};
use crate::options::JobOptions;
use crate::registry::{namespaced_id, split_id};
use crate::{Duplex, Job};

/// The typed id of a job a printer has accepted: the owning backend's registry name and that
/// backend's own handle for the job (its **native** part).
///
/// The text form is `"<backend>:<native>"` ([`fmt::Display`](std::fmt::Display) and
/// [`FromStr`](std::str::FromStr) round-trip it), the same shape as namespaced printer ids. A
/// native part may contain colons and anything else but must not be empty; only the first colon
/// separates the backend. An app keeps the typed value, or the string form when it must store it,
/// and never concatenates or splits ids by hand; a backend offers a typed constructor for its
/// native part (for example a printer URL and a job number) so that nobody builds that string
/// either.
///
/// A native handle is only meaningful to the backend that made it.
///
/// This type replaces the earlier `JobRef` (`id`/`native` strings, `from_id` returning `Option`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct JobId {
    backend: String,
    native: String,
}

impl JobId {
    /// A job of `backend` with its native handle. Does not validate: use
    /// [`str::parse`] (or [`Self::try_new`]) for text from outside. A backend names its jobs
    /// with its own [`Backend::name`](crate::backends::Backend::name).
    pub fn new(backend: impl Into<String>, native: impl Into<String>) -> Self {
        Self { backend: backend.into(), native: native.into() }
    }

    /// [`Self::new`] that checks what [`FromStr`](std::str::FromStr) checks, so the result always
    /// round-trips through its text form.
    pub fn try_new(backend: impl Into<String>, native: impl Into<String>) -> Result<Self, ParseJobIdError> {
        let (backend, native) = (backend.into(), native.into());
        if backend.is_empty() {
            return Err(ParseJobIdError::new(ParseJobIdErrorKind::EmptyBackend));
        }
        if !valid_backend_name(&backend) {
            return Err(ParseJobIdError::new(ParseJobIdErrorKind::InvalidBackend));
        }
        if native.is_empty() {
            return Err(ParseJobIdError::new(ParseJobIdErrorKind::EmptyNative));
        }
        Ok(Self { backend, native })
    }

    /// The owning backend's registry name.
    pub fn backend(&self) -> &str {
        &self.backend
    }

    /// The backend's own handle for the job.
    pub fn native(&self) -> &str {
        &self.native
    }

    /// The text form, `"<backend>:<native>"`.
    pub fn to_id_string(&self) -> String {
        namespaced_id(&self.backend, &self.native)
    }
}

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.backend, self.native)
    }
}

fn valid_backend_name(name: &str) -> bool {
    name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Why text is not a [`JobId`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParseJobIdErrorKind {
    /// No colon: not `"<backend>:<native>"`.
    MissingSeparator,
    /// Nothing before the first colon.
    EmptyBackend,
    /// The backend part has a character other than lowercase ASCII, digits, `-` and `_`.
    InvalidBackend,
    /// Nothing after the first colon.
    EmptyNative,
}

/// The typed failure of parsing a [`JobId`]; see [`Self::kind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseJobIdError {
    kind: ParseJobIdErrorKind,
}

impl ParseJobIdError {
    fn new(kind: ParseJobIdErrorKind) -> Self {
        Self { kind }
    }

    pub fn kind(&self) -> ParseJobIdErrorKind {
        self.kind
    }
}

impl std::fmt::Display for ParseJobIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.kind {
            ParseJobIdErrorKind::MissingSeparator => "a job id looks like <backend>:<native id>, and this has no colon",
            ParseJobIdErrorKind::EmptyBackend => "the job id has no backend name before its colon",
            ParseJobIdErrorKind::InvalidBackend => "the backend name of the job id has a character other than a-z, 0-9, '-' and '_'",
            ParseJobIdErrorKind::EmptyNative => "the job id has nothing after its colon",
        })
    }
}

impl std::error::Error for ParseJobIdError {}

/// A malformed job id is a bad request for [`PrintError`] consumers.
impl From<ParseJobIdError> for PrintError {
    fn from(error: ParseJobIdError) -> Self {
        PrintError::new(ErrorKind::InvalidRequest, error.to_string())
    }
}

impl std::str::FromStr for JobId {
    type Err = ParseJobIdError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (backend, native) = split_id(text).ok_or(ParseJobIdError::new(ParseJobIdErrorKind::MissingSeparator))?;
        Self::try_new(backend, native)
    }
}

/// The totals a caller expects for a job, used only where the backend reports none. The registry
/// computes them from the request ([`Self::for_request`]) and puts them in [`Submitted::expected`];
/// a [`crate::JobWatch`] applies them to every reading, and the registry applies the sheet total
/// to progress events. A backend-reported total always wins.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExpectedTotals {
    /// Pages of the document ([`JobOptions::expected_pages`]).
    pub pages: Option<u32>,
    /// Sheets this job should use: pages halved (rounded up) for two-sided jobs, times the copies.
    /// An estimate; collation, booklet layouts and the device's own counting can differ.
    pub sheets: Option<u32>,
}

impl ExpectedTotals {
    /// What a request expects. Zero pages is "unknown".
    pub fn for_request(job: &Job, options: &JobOptions) -> Self {
        let Some(pages) = options.expected_pages.filter(|p| *p > 0) else {
            return Self::default();
        };
        let per_sheet = if matches!(job.duplex, Some(Duplex::LongEdge | Duplex::ShortEdge)) { 2 } else { 1 };
        let sheets = pages.div_ceil(per_sheet).saturating_mul(job.copies.max(1));
        Self { pages: Some(pages), sheets: Some(sheets) }
    }

    /// Fill the totals a reading lacks.
    pub fn apply(&self, status: &mut JobStatus) {
        if status.pages_total.is_none() {
            status.pages_total = self.pages.filter(|p| *p > 0);
        }
        if status.sheets_total.is_none() {
            status.sheets_total = self.sheets.filter(|s| *s > 0);
        }
    }

    /// Whether anything is expected.
    pub fn is_empty(&self) -> bool {
        self.pages.is_none() && self.sheets.is_none()
    }
}

/// Where a job is, in the printer's own words reduced to what every backend can say.
///
/// [`JobState::Unknown`] means "accepted, outcome unknown": the backend cannot (or could not just
/// now) say. It is **not** terminal and not a failure; a UI shows "sent" and stops promising more.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum JobState {
    /// Accepted and waiting (also: the document is still arriving).
    Pending,
    /// Held: waiting for a person or a release (a hold, a PIN, a schedule).
    Held,
    Processing,
    /// Paused by the printer or the queue; needs attention (see the reasons).
    Stopped,
    Completed,
    /// Finished, and the printer says something went slightly wrong.
    CompletedWithWarnings,
    /// Finished, and the printer says it did not print everything.
    CompletedWithErrors,
    Cancelled,
    /// The printer gave up on it.
    Aborted,
    /// The backend cannot say. Not terminal.
    Unknown,
}

impl JobState {
    /// Whether the job has ended for good; a terminal state never changes again.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::CompletedWithWarnings | Self::CompletedWithErrors | Self::Cancelled | Self::Aborted)
    }

    /// Whether the job ended and printed ([`Self::Completed`] or [`Self::CompletedWithWarnings`]).
    pub const fn is_success(self) -> bool {
        matches!(self, Self::Completed | Self::CompletedWithWarnings)
    }

    /// A stable lowercase name for logs and tool results.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Held => "held",
            Self::Processing => "processing",
            Self::Stopped => "stopped",
            Self::Completed => "completed",
            Self::CompletedWithWarnings => "completed_with_warnings",
            Self::CompletedWithErrors => "completed_with_errors",
            Self::Cancelled => "cancelled",
            Self::Aborted => "aborted",
            Self::Unknown => "unknown",
        }
    }
}

/// One reading of a job's status. Counters are `None` when the backend does not publish them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobStatus {
    pub state: JobState,
    /// The device's reasons, as reported (`"JobPrinting"`, `"media-empty"`); empty when none.
    pub reasons: Vec<String>,
    pub sheets_done: Option<u32>,
    pub sheets_total: Option<u32>,
    pub pages_done: Option<u32>,
    pub pages_total: Option<u32>,
    /// When this reading was taken (the caller's wall clock, not the printer's).
    pub updated_at: SystemTime,
}

impl JobStatus {
    /// A reading with this state and no counters or reasons, taken now.
    pub fn new(state: JobState) -> Self {
        Self { state, reasons: Vec::new(), sheets_done: None, sheets_total: None, pages_done: None, pages_total: None, updated_at: SystemTime::now() }
    }

    /// "Accepted, outcome unknown": what a backend that cannot report status answers.
    pub fn unknown() -> Self {
        Self::new(JobState::Unknown)
    }

    /// Whether the job has ended for good ([`JobState::is_terminal`]).
    pub const fn terminal(&self) -> bool {
        self.state.is_terminal()
    }

    /// Fraction of the sheets done in `0.0..=1.0`, when both counters are known and the total is
    /// not zero.
    pub fn sheet_fraction(&self) -> Option<f32> {
        match (self.sheets_done, self.sheets_total) {
            (Some(done), Some(total)) if total > 0 => Some((done.min(total) as f32) / (total as f32)),
            _ => None,
        }
    }

    /// Whether two readings say the same thing (ignoring when they were taken); a watcher uses it
    /// to slow down while nothing changes.
    pub fn same_as(&self, other: &Self) -> bool {
        self.state == other.state
            && self.reasons == other.reasons
            && self.sheets_done == other.sheets_done
            && self.sheets_total == other.sheets_total
            && self.pages_done == other.pages_done
            && self.pages_total == other.pages_total
    }
}

/// Whether and how a backend can tell what happens to a job after it was accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum WatchSupport {
    /// It cannot: [`crate::backends::Backend::job_status`] answers [`JobState::Unknown`] and a UI
    /// should not poll. "Accepted, outcome unknown" is the final answer.
    #[default]
    None,
    /// Ask again every [`crate::backends::Backend::recommended_poll_interval`].
    Poll,
    /// The device can push changes. Callers still poll at the (longer) recommended interval as a
    /// safety net; no backend listens for events yet.
    Events,
}

impl WatchSupport {
    /// Whether a caller should ask for the status at all.
    pub const fn is_watchable(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// What [`crate::backends::Backend::submit`] returns once the printer has **accepted** the
/// document. It says nothing about the paper: follow the job with its [`JobId`] if the backend
/// can ([`Submitted::watch`]).
///
/// The call that produced it is bounded by the backend's per-request timeouts only; it never waits
/// for the job to print.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submitted {
    /// The backend's registry name.
    pub backend: &'static str,
    /// The accepted job, when the backend has a handle for it (the built-in spooler has none).
    pub job: Option<JobId>,
    /// When the printer accepted the document (the caller's wall clock).
    pub accepted_at: SystemTime,
    /// The job's state at that moment, when the reply carried it or a read was cheap.
    pub status: Option<JobStatus>,
    /// Settings that were dropped or changed on the way.
    pub warnings: Vec<String>,
    /// A sentence for people ("job 7 on <printer> was accepted").
    pub message: String,
    /// Filled in by the registry from the backend; see [`WatchSupport`].
    pub watch: WatchSupport,
    /// Filled in by the registry from the backend; `None` when there is nothing to poll.
    pub poll_interval: Option<Duration>,
    /// Filled in by the registry from the request's [`JobOptions::expected_pages`]; see
    /// [`ExpectedTotals`]. [`crate::JobWatch::for_submitted`] uses it.
    pub expected: ExpectedTotals,
}

impl Submitted {
    /// An accepted job with no handle and nothing known about it.
    pub fn new(backend: &'static str, message: impl Into<String>) -> Self {
        Self {
            backend,
            job: None,
            accepted_at: SystemTime::now(),
            status: None,
            warnings: Vec::new(),
            message: message.into(),
            watch: WatchSupport::None,
            poll_interval: None,
            expected: ExpectedTotals::default(),
        }
    }

    #[must_use]
    pub fn with_job(mut self, job: JobId) -> Self {
        self.job = Some(job);
        self
    }

    #[must_use]
    pub fn with_status(mut self, status: JobStatus) -> Self {
        self.status = Some(status);
        self
    }

    #[must_use]
    pub fn with_warnings(mut self, warnings: Vec<String>) -> Self {
        self.warnings = warnings;
        self
    }

    /// The older receipt view: the message and the backend's own job id.
    pub fn receipt(&self) -> Receipt {
        Receipt { backend: self.backend, message: self.message.clone(), job_id: self.job.as_ref().map(|j| j.native().to_owned()) }
    }
}

/// A receipt becomes an accepted job whose handle is the receipt's job id (so it is only
/// watchable if the backend overrides [`crate::backends::Backend::job_status`]).
impl From<Receipt> for Submitted {
    fn from(receipt: Receipt) -> Self {
        let mut submitted = Submitted::new(receipt.backend, receipt.message);
        submitted.job = receipt.job_id.map(|native| JobId::new(receipt.backend, native));
        submitted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_states_are_exactly_the_five_endings() {
        use JobState as S;
        let all = [
            S::Pending,
            S::Held,
            S::Processing,
            S::Stopped,
            S::Completed,
            S::CompletedWithWarnings,
            S::CompletedWithErrors,
            S::Cancelled,
            S::Aborted,
            S::Unknown,
        ];
        let terminal: Vec<_> = all.iter().filter(|s| s.is_terminal()).map(|s| s.name()).collect();
        assert_eq!(terminal, ["completed", "completed_with_warnings", "completed_with_errors", "cancelled", "aborted"]);
        assert!(!S::Unknown.is_terminal(), "unknown is 'accepted, outcome unknown', not an ending");
        assert!(S::Completed.is_success() && S::CompletedWithWarnings.is_success() && !S::CompletedWithErrors.is_success());
        assert!(JobStatus::new(S::Cancelled).terminal() && !JobStatus::unknown().terminal());
    }

    #[test]
    fn job_ids_round_trip_through_their_text_form() {
        let job = JobId::new("wsd", "http://192.0.2.1:5357/p#7");
        assert_eq!((job.backend(), job.native()), ("wsd", "http://192.0.2.1:5357/p#7"));
        assert_eq!(job.to_string(), "wsd:http://192.0.2.1:5357/p#7");
        assert_eq!(job.to_string().parse::<JobId>(), Ok(job.clone()));
        assert_eq!(JobId::try_new("wsd", "http://192.0.2.1:5357/p#7"), Ok(job));
    }

    #[test]
    fn malformed_job_ids_are_typed_errors() {
        use ParseJobIdErrorKind as K;
        let kind = |text: &str| text.parse::<JobId>().map_err(|e| e.kind());
        assert_eq!(kind("no-prefix"), Err(K::MissingSeparator));
        assert_eq!(kind(""), Err(K::MissingSeparator));
        assert_eq!(kind(":7"), Err(K::EmptyBackend));
        assert_eq!(kind("Wsd:7"), Err(K::InvalidBackend));
        assert_eq!(kind("a b:7"), Err(K::InvalidBackend));
        assert_eq!(kind("wsd:"), Err(K::EmptyNative));
        let error: PrintError = "x".parse::<JobId>().map(|_| ()).unwrap_err().into();
        assert_eq!(error.kind(), ErrorKind::InvalidRequest);
    }

    #[test]
    fn expected_totals_fill_only_what_the_backend_did_not_report() {
        let totals = ExpectedTotals { pages: Some(8), sheets: Some(4) };
        let mut status = JobStatus::new(JobState::Processing);
        status.sheets_done = Some(1);
        totals.apply(&mut status);
        assert_eq!((status.sheets_total, status.pages_total), (Some(4), Some(8)));
        assert_eq!(status.sheet_fraction(), Some(0.25));
        let mut reported = JobStatus::new(JobState::Processing);
        reported.sheets_total = Some(10);
        totals.apply(&mut reported);
        assert_eq!(reported.sheets_total, Some(10), "a backend-reported total wins");
        let mut zero = JobStatus::new(JobState::Processing);
        ExpectedTotals { pages: Some(0), sheets: Some(0) }.apply(&mut zero);
        assert_eq!((zero.sheets_total, zero.pages_total), (None, None), "zero is no total");
    }

    #[test]
    fn expected_sheets_follow_duplex_and_copies() {
        use crate::{Duplex, Job};
        let options = JobOptions::new().with_expected_pages(5);
        let simplex = ExpectedTotals::for_request(&Job::default(), &options);
        assert_eq!((simplex.pages, simplex.sheets), (Some(5), Some(5)));
        let duplex = Job { duplex: Some(Duplex::LongEdge), copies: 2, ..Job::default() };
        let both = ExpectedTotals::for_request(&duplex, &options);
        assert_eq!((both.pages, both.sheets), (Some(5), Some(6)));
        assert_eq!(ExpectedTotals::for_request(&Job::default(), &JobOptions::new()), ExpectedTotals::default());
    }

    #[test]
    fn sheet_fraction_needs_both_counters() {
        let mut s = JobStatus::new(JobState::Processing);
        assert_eq!(s.sheet_fraction(), None);
        s.sheets_done = Some(1);
        assert_eq!(s.sheet_fraction(), None);
        s.sheets_total = Some(4);
        assert_eq!(s.sheet_fraction(), Some(0.25));
        s.sheets_total = Some(0);
        assert_eq!(s.sheet_fraction(), None);
        s.sheets_total = Some(1);
        s.sheets_done = Some(5);
        assert_eq!(s.sheet_fraction(), Some(1.0));
    }

    #[test]
    fn a_receipt_converts_and_the_receipt_view_comes_back() {
        let receipt = Receipt { backend: "cups", message: "request id is Office-1".into(), job_id: Some("Office-1".into()) };
        let submitted = Submitted::from(receipt.clone());
        assert_eq!(submitted.job.as_ref().map(|j| j.native.as_str()), Some("Office-1"));
        assert_eq!(submitted.watch, WatchSupport::None);
        assert_eq!(submitted.receipt(), receipt);
    }
}
