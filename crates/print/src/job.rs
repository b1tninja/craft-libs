//! What a backend hands back once a printer has accepted a document, and how a job's progress is
//! described afterwards. See "Job lifecycle" in the crate README for the reasoning.
//!
//! Submitting and watching are separate calls. [`crate::backends::Backend::submit`] returns a
//! [`Submitted`] as soon as the printer has accepted the document; everything after that (is it
//! printing, did it finish, cancel it) is a [`JobRef`] plus one bounded request at a time
//! ([`crate::backends::Backend::job_status`], [`crate::backends::Backend::cancel_job`]), driven by
//! the caller (see [`crate::JobWatch`]).

use std::time::{Duration, SystemTime};

use crate::backend::Receipt;
use crate::registry::{namespaced_id, split_id};

/// A job that a printer has accepted. `id` is what an app keeps and shows (a registry id,
/// `"<backend>:<native id>"`); `native` is the backend's own handle for it.
///
/// A backend builds one with [`JobRef::new`] and the registry namespaces it, exactly as it does
/// for printer ids. A native handle may contain colons; only the first colon of `id` separates the
/// backend's name. A handle is only meaningful to the backend that made it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct JobRef {
    pub id: String,
    pub native: String,
}

impl JobRef {
    /// A job as a backend names it: `id` equals `native` until the registry namespaces it.
    pub fn new(native: impl Into<String>) -> Self {
        let native = native.into();
        Self { id: native.clone(), native }
    }

    /// The registry's form of a backend's job.
    pub fn namespaced(backend: &str, native: &str) -> Self {
        Self { id: namespaced_id(backend, native), native: native.to_owned() }
    }

    /// Rebuild a job from the registry id an app kept (`None` when it has no backend prefix).
    pub fn from_id(id: &str) -> Option<Self> {
        let (_, native) = split_id(id)?;
        Some(Self { id: id.to_owned(), native: native.to_owned() })
    }

    /// The backend's name, when `id` is namespaced.
    pub fn backend(&self) -> Option<&str> {
        split_id(&self.id).map(|(backend, _)| backend)
    }
}

impl std::fmt::Display for JobRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.id)
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
/// document. It says nothing about the paper: follow the job with its [`JobRef`] if the backend
/// can ([`Submitted::watch`]).
///
/// The call that produced it is bounded by the backend's per-request timeouts only; it never waits
/// for the job to print.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submitted {
    /// The backend's registry name.
    pub backend: &'static str,
    /// The accepted job, when the backend has a handle for it (the built-in spooler has none).
    pub job: Option<JobRef>,
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
        }
    }

    #[must_use]
    pub fn with_job(mut self, job: JobRef) -> Self {
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
        Receipt { backend: self.backend, message: self.message.clone(), job_id: self.job.as_ref().map(|j| j.native.clone()) }
    }
}

/// A receipt becomes an accepted job whose handle is the receipt's job id (so it is only
/// watchable if the backend overrides [`crate::backends::Backend::job_status`]).
impl From<Receipt> for Submitted {
    fn from(receipt: Receipt) -> Self {
        let mut submitted = Submitted::new(receipt.backend, receipt.message);
        submitted.job = receipt.job_id.map(JobRef::new);
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
    fn job_refs_namespace_and_come_back_from_their_id() {
        let native = JobRef::new("http://192.0.2.1:5357/p#7");
        assert_eq!(native.id, native.native);
        let job = JobRef::namespaced("wsd", &native.native);
        assert_eq!(job.id, "wsd:http://192.0.2.1:5357/p#7");
        assert_eq!(job.backend(), Some("wsd"));
        assert_eq!(JobRef::from_id(&job.id), Some(job));
        assert_eq!(JobRef::from_id("no-prefix"), None);
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
