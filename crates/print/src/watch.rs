//! Following an accepted job: [`JobWatch`], a caller-driven state machine, and
//! [`wait_for_job`], the blocking convenience built on it.
//!
//! A `JobWatch` owns no thread and no timer. The caller (a UI frame, a worker loop) calls
//! [`JobWatch::tick`] whenever it likes; the watch does at most **one** bounded request per call
//! and only when one is due, and otherwise says how long until it is. All waiting is measured on an
//! injectable [`Clock`], so a test drives it without sleeping.
//!
//! ```
//! use craft_print::{JobRef, JobWatch, ManualClock, Registry, WaitPolicy, WatchStep};
//! # use std::sync::Arc;
//! let clock = Arc::new(ManualClock::new());
//! let registry = Registry::new();
//! let mut watch = JobWatch::new(JobRef::namespaced("wsd", "job-1"), WaitPolicy::default()).with_clock(clock);
//! // Nothing is registered, so the first poll fails for good; with a backend it would be a status.
//! assert!(matches!(watch.tick(&registry), WatchStep::Done(_)));
//! ```

use std::sync::Arc;
use std::time::Duration;

use crate::backend::{CancelToken, ErrorKind, PrintError};
use crate::job::{JobRef, JobState, JobStatus, Submitted, WatchSupport};
use crate::registry::Registry;
use crate::retry::{CANCEL_SLICE, Clock, Jitter, RetryPolicy, SystemClock};

/// Where a watch gets status from and sends cancels to. The [`Registry`] is one (by job id); a
/// test can supply its own.
pub trait JobSource {
    fn job_status(&self, job: &JobRef) -> Result<JobStatus, PrintError>;
    fn cancel_job(&self, job: &JobRef) -> Result<(), PrintError>;
}

impl JobSource for Registry {
    fn job_status(&self, job: &JobRef) -> Result<JobStatus, PrintError> {
        Registry::job_status(self, &job.id)
    }

    fn cancel_job(&self, job: &JobRef) -> Result<(), PrintError> {
        Registry::cancel_job(self, &job.id)
    }
}

/// How a job is followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitPolicy {
    /// The gap between polls. `None`: the backend's recommended interval, else two seconds.
    pub interval: Option<Duration>,
    /// While the status does not change the gap grows by `growth_percent` per poll up to this.
    pub max_interval: Duration,
    /// 100 keeps the gap constant; 150 multiplies it by 1.5 per unchanged poll. A change resets it.
    pub growth_percent: u32,
    /// Stop following after this long, counted from the watch's creation. The job keeps going on
    /// the printer. Finite on purpose; the default is two minutes.
    pub max_wait: Duration,
    /// How consecutive failed polls are retried (they are idempotent), then given up on.
    pub retry: RetryPolicy,
}

impl Default for WaitPolicy {
    fn default() -> Self {
        Self {
            interval: None,
            max_interval: Duration::from_secs(10),
            growth_percent: 150,
            max_wait: Duration::from_secs(120),
            retry: RetryPolicy { max_attempts: 5, ..RetryPolicy::default() },
        }
    }
}

impl WaitPolicy {
    /// A constant gap, for tests and tight loops.
    #[must_use]
    pub fn every(interval: Duration) -> Self {
        Self { interval: Some(interval), growth_percent: 100, ..Self::default() }
    }

    #[must_use]
    pub fn with_max_wait(mut self, max_wait: Duration) -> Self {
        self.max_wait = max_wait;
        self
    }
}

/// How following ended.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WatchOutcome {
    /// The job reached a terminal state (success or not: look at the state).
    Terminal(JobStatus),
    /// The device no longer knows the job. Finished jobs are forgotten, so it most likely
    /// finished; `last` is the last status seen.
    Forgotten { last: Option<JobStatus> },
    /// The backend cannot report on jobs ([`WatchSupport::None`]): accepted, outcome unknown.
    NoStatus,
    /// `max_wait` ran out with the job not finished; it carries on at the printer.
    TimedOut { last: Option<JobStatus> },
    /// A failure that is not transient, or a transient one that outlasted the retry policy.
    Failed(PrintError),
}

impl WatchOutcome {
    /// The job printed, or (forgotten) most likely did.
    pub fn is_success(&self) -> bool {
        match self {
            Self::Terminal(status) => status.state.is_success(),
            Self::Forgotten { .. } => true,
            _ => false,
        }
    }

    /// The error that says the job did **not** print: an abort, a cancel, completed-with-errors or
    /// a failed watch. `None` for success and for the open outcomes (forgotten, no status, timed
    /// out): those are not failures, a caller words them itself.
    pub fn error(&self) -> Option<PrintError> {
        match self {
            Self::Failed(error) => Some(error.clone()),
            Self::Terminal(status) => match status.state {
                JobState::Cancelled => Some(PrintError::new(ErrorKind::Cancelled, with_reasons("the job was cancelled", status))),
                JobState::Aborted => Some(PrintError::new(ErrorKind::Rejected, with_reasons("the printer aborted the job", status))),
                JobState::CompletedWithErrors => Some(PrintError::new(ErrorKind::Failed, with_reasons("the job completed with errors", status))),
                _ => None,
            },
            _ => None,
        }
    }
}

fn with_reasons(what: &str, status: &JobStatus) -> String {
    if status.reasons.is_empty() { what.to_owned() } else { format!("{what} ({})", status.reasons.join(", ")) }
}

/// What one [`JobWatch::tick`] or [`JobWatch::poll_status`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WatchStep {
    /// Nothing is due yet; call again after this long (or whenever: it is cheap).
    Pending { next_in: Duration },
    /// A poll was made and the job has not ended. The status may be [`JobState::Unknown`].
    Update(JobStatus),
    /// A poll failed with a transient error and will be repeated after `next_in`.
    Retrying { error: PrintError, attempt: u32, next_in: Duration },
    /// Following is over. Every later call returns the same outcome.
    Done(WatchOutcome),
}

/// A caller-driven follower of one job. Advance it with [`Self::tick`]; see the module docs.
pub struct JobWatch {
    job: JobRef,
    support: WatchSupport,
    base_interval: Duration,
    interval: Duration,
    policy: WaitPolicy,
    clock: Arc<dyn Clock>,
    cancel: CancelToken,
    jitter: Jitter,
    started: Duration,
    next_at: Duration,
    failures: u32,
    polls: u32,
    last: Option<JobStatus>,
    cancel_sent: bool,
    done: Option<WatchOutcome>,
}

impl std::fmt::Debug for JobWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobWatch")
            .field("job", &self.job.id)
            .field("support", &self.support)
            .field("polls", &self.polls)
            .field("failures", &self.failures)
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

/// The shortest gap between polls, whatever the policy says.
const MIN_INTERVAL: Duration = Duration::from_millis(10);
/// The gap when neither the policy nor the backend suggests one.
const FALLBACK_INTERVAL: Duration = Duration::from_secs(2);

impl JobWatch {
    /// Follow `job`, assuming the backend can be polled ([`WatchSupport::Poll`]); the clock is the
    /// system's. Use [`Self::for_submitted`] to take support and interval from the backend.
    pub fn new(job: JobRef, policy: WaitPolicy) -> Self {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        Self::build(job, WatchSupport::Poll, None, policy, clock)
    }

    /// Follow what a backend accepted: its [`WatchSupport`] and recommended interval.
    /// `None` when the backend gave no job handle (nothing to follow).
    pub fn for_submitted(submitted: &Submitted, policy: WaitPolicy) -> Option<Self> {
        let job = submitted.job.clone()?;
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        Some(Self::build(job, submitted.watch, submitted.poll_interval, policy, clock))
    }

    fn build(job: JobRef, support: WatchSupport, recommended: Option<Duration>, policy: WaitPolicy, clock: Arc<dyn Clock>) -> Self {
        let base_interval = policy.interval.or(recommended).unwrap_or(FALLBACK_INTERVAL).max(MIN_INTERVAL);
        let started = clock.now();
        Self {
            job,
            support,
            base_interval,
            interval: base_interval,
            policy,
            jitter: Jitter::from_clock(clock.as_ref()),
            clock,
            cancel: CancelToken::new(),
            started,
            next_at: started,
            failures: 0,
            polls: 0,
            last: None,
            cancel_sent: false,
            done: None,
        }
    }

    /// Use another clock (a [`crate::ManualClock`] in tests). Restarts the deadline and schedule
    /// from that clock's now.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.started = clock.now();
        self.next_at = self.started;
        self.jitter = Jitter::from_clock(clock.as_ref());
        self.clock = clock;
        self
    }

    /// Take the recommended interval from the backend (when the policy sets none).
    #[must_use]
    pub fn with_recommended_interval(mut self, recommended: Option<Duration>) -> Self {
        self.base_interval = self.policy.interval.or(recommended).unwrap_or(FALLBACK_INTERVAL).max(MIN_INTERVAL);
        self.interval = self.base_interval;
        self
    }

    /// Declare what the backend can do (default: [`WatchSupport::Poll`]).
    #[must_use]
    pub fn with_support(mut self, support: WatchSupport) -> Self {
        self.support = support;
        self
    }

    /// A token that, once cancelled, sends one cancel to the printer on the next tick and keeps
    /// following until the printer reports how the job ended.
    #[must_use]
    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Replace the jitter source (a fixed seed in tests).
    #[must_use]
    pub fn with_jitter(mut self, jitter: Jitter) -> Self {
        self.jitter = jitter;
        self
    }

    pub fn job(&self) -> &JobRef {
        &self.job
    }

    /// The last status read, if any.
    pub fn last_status(&self) -> Option<&JobStatus> {
        self.last.as_ref()
    }

    /// How many status requests were made (successful or not).
    pub fn polls(&self) -> u32 {
        self.polls
    }

    /// The final outcome, once following is over.
    pub fn outcome(&self) -> Option<&WatchOutcome> {
        self.done.as_ref()
    }

    pub fn is_done(&self) -> bool {
        self.done.is_some()
    }

    /// Time until the next poll is due or the deadline arrives, whichever is first; `None` when
    /// done. Zero means "tick now".
    pub fn next_poll_in(&self) -> Option<Duration> {
        if self.done.is_some() {
            return None;
        }
        let now = self.clock.now();
        if self.cancel.is_cancelled() && !self.cancel_sent {
            return Some(Duration::ZERO);
        }
        let deadline = self.started.saturating_add(self.policy.max_wait);
        Some(self.next_at.min(deadline).saturating_sub(now))
    }

    /// Advance: make the one request that is due (a cancel, else a status poll), or report how long
    /// to wait. Never blocks beyond that one request.
    pub fn tick(&mut self, source: &dyn JobSource) -> WatchStep {
        if let Some(done) = &self.done {
            return WatchStep::Done(done.clone());
        }
        let now = self.clock.now();
        if self.cancel.is_cancelled() && !self.cancel_sent {
            return self.send_cancel(source, now);
        }
        if now >= self.started.saturating_add(self.policy.max_wait) {
            return self.finish(WatchOutcome::TimedOut { last: self.last.clone() });
        }
        if self.support == WatchSupport::None {
            return self.finish(WatchOutcome::NoStatus);
        }
        if now < self.next_at {
            let deadline = self.started.saturating_add(self.policy.max_wait);
            return WatchStep::Pending { next_in: self.next_at.min(deadline).saturating_sub(now) };
        }
        self.poll(source, now)
    }

    /// Ask for the status now, ignoring the schedule (a person pressed Refresh). Still a single
    /// request, and still subject to the retry policy for failures.
    pub fn poll_status(&mut self, source: &dyn JobSource) -> WatchStep {
        if let Some(done) = &self.done {
            return WatchStep::Done(done.clone());
        }
        if self.support == WatchSupport::None {
            return self.finish(WatchOutcome::NoStatus);
        }
        let now = self.clock.now();
        self.poll(source, now)
    }

    fn finish(&mut self, outcome: WatchOutcome) -> WatchStep {
        self.done = Some(outcome.clone());
        WatchStep::Done(outcome)
    }

    fn send_cancel(&mut self, source: &dyn JobSource, now: Duration) -> WatchStep {
        match source.cancel_job(&self.job) {
            Ok(()) => {
                self.cancel_sent = true;
                self.failures = 0;
                // Look at how it ended straight away; if the backend cannot report, that is the end.
                self.next_at = now;
                if self.support == WatchSupport::None {
                    let mut status = JobStatus::new(JobState::Cancelled);
                    status.reasons.push("cancel requested".into());
                    return self.finish(WatchOutcome::Terminal(status));
                }
                WatchStep::Pending { next_in: Duration::ZERO }
            }
            Err(error) => self.failed(error, now),
        }
    }

    fn poll(&mut self, source: &dyn JobSource, now: Duration) -> WatchStep {
        self.polls = self.polls.saturating_add(1);
        match source.job_status(&self.job) {
            Ok(status) => {
                self.failures = 0;
                if status.terminal() {
                    self.last = Some(status.clone());
                    return self.finish(WatchOutcome::Terminal(status));
                }
                let changed = self.last.as_ref().is_none_or(|last| !last.same_as(&status));
                self.interval = if changed { self.base_interval } else { self.grown() };
                self.next_at = now.saturating_add(self.interval);
                self.last = Some(status.clone());
                WatchStep::Update(status)
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                if self.cancel_sent {
                    let mut status = JobStatus::new(JobState::Cancelled);
                    status.reasons.push("cancelled; the printer no longer lists the job".into());
                    return self.finish(WatchOutcome::Terminal(status));
                }
                let last = self.last.clone();
                self.finish(WatchOutcome::Forgotten { last })
            }
            Err(error) => self.failed(error, now),
        }
    }

    fn grown(&self) -> Duration {
        let percent = self.policy.growth_percent.max(100);
        let grown = Duration::try_from_secs_f64(self.interval.as_secs_f64() * f64::from(percent) / 100.0).unwrap_or(self.interval);
        grown.min(self.policy.max_interval.max(self.base_interval))
    }

    /// A request failed: retry after a backoff when the policy allows (the errors of status polls
    /// and cancels are idempotent), else end the watch.
    fn failed(&mut self, error: PrintError, now: Duration) -> WatchStep {
        self.failures = self.failures.saturating_add(1);
        match self.policy.retry.next_delay(&error, self.failures, self.jitter.unit()) {
            Some(next_in) => {
                self.next_at = now.saturating_add(next_in);
                WatchStep::Retrying { error, attempt: self.failures, next_in }
            }
            None => self.finish(WatchOutcome::Failed(error)),
        }
    }

    /// Drive the watch to the end on this thread: tick, sleep on the clock in slices short enough
    /// to notice the cancel token, and hand every step that is not a plain wait to `on_step`.
    /// This is the loop behind [`wait_for_job`]; a UI should tick from its own loop instead.
    pub fn run(&mut self, source: &dyn JobSource, on_step: &mut dyn FnMut(&WatchStep)) -> WatchOutcome {
        loop {
            let step = self.tick(source);
            match &step {
                WatchStep::Pending { next_in } => {
                    // One short slice, then tick again: a set cancel token is noticed by the tick, and
                    // time moves on even after the cancel was sent.
                    if !next_in.is_zero() {
                        self.clock.sleep((*next_in).min(CANCEL_SLICE));
                    }
                }
                WatchStep::Retrying { .. } | WatchStep::Update(_) => on_step(&step),
                WatchStep::Done(outcome) => {
                    on_step(&step);
                    return outcome.clone();
                }
            }
        }
    }
}

/// Follow a job to its end on this thread: the blocking convenience for a CLI, **built on** the
/// non-blocking primitives (never part of `submit`). Waits at most `policy.max_wait` (two minutes
/// by default) and reports [`WatchOutcome::TimedOut`] if the job is still running; the job is not
/// touched. `Err` only when the job id names no registered backend.
pub fn wait_for_job(registry: &Registry, job: &JobRef, policy: WaitPolicy) -> Result<WatchOutcome, PrintError> {
    let mut watch = registry.job_watch(job, policy)?;
    Ok(watch.run(registry, &mut |_| {}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retry::ManualClock;
    use std::sync::Mutex;

    /// A source that plays back a script of poll results.
    struct Script {
        polls: Mutex<Vec<Result<JobStatus, PrintError>>>,
        cancels: Mutex<Vec<Result<(), PrintError>>>,
        seen_polls: Mutex<u32>,
        seen_cancels: Mutex<u32>,
    }

    impl Script {
        fn new(polls: Vec<Result<JobStatus, PrintError>>) -> Self {
            Self { polls: Mutex::new(polls), cancels: Mutex::new(Vec::new()), seen_polls: Mutex::new(0), seen_cancels: Mutex::new(0) }
        }

        fn polls(&self) -> u32 {
            *self.seen_polls.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
        }

        fn cancels(&self) -> u32 {
            *self.seen_cancels.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    impl JobSource for Script {
        fn job_status(&self, _: &JobRef) -> Result<JobStatus, PrintError> {
            *self.seen_polls.lock().unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
            let mut polls = self.polls.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if polls.len() > 1 { polls.remove(0) } else { polls.first().cloned().unwrap_or_else(|| Ok(JobStatus::unknown())) }
        }

        fn cancel_job(&self, _: &JobRef) -> Result<(), PrintError> {
            *self.seen_cancels.lock().unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
            let mut cancels = self.cancels.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if cancels.is_empty() { Ok(()) } else { cancels.remove(0) }
        }
    }

    fn status(state: JobState) -> JobStatus {
        JobStatus::new(state)
    }

    fn watch(policy: WaitPolicy) -> (JobWatch, ManualClock) {
        let clock = ManualClock::new();
        let watch = JobWatch::new(JobRef::namespaced("t", "1"), policy).with_clock(Arc::new(clock.clone())).with_jitter(Jitter::new(3));
        (watch, clock)
    }

    fn busy() -> PrintError {
        PrintError::new(ErrorKind::Unavailable, "down").with_safety(crate::RetrySafety::Idempotent)
    }

    #[test]
    fn a_job_is_followed_on_the_injected_clock_to_its_end() {
        let script = Script::new(vec![
            Ok(status(JobState::Pending)),
            Ok(status(JobState::Processing)),
            Ok(status(JobState::Processing)),
            Ok(status(JobState::Completed)),
        ]);
        let (mut watch, clock) = watch(WaitPolicy::every(Duration::from_secs(2)));
        assert!(matches!(watch.tick(&script), WatchStep::Update(s) if s.state == JobState::Pending));
        // Not due: no request, and it says how long.
        assert_eq!(watch.tick(&script), WatchStep::Pending { next_in: Duration::from_secs(2) });
        assert_eq!(script.polls(), 1, "a tick that is not due makes no request");
        clock.advance(Duration::from_secs(1));
        assert_eq!(watch.next_poll_in(), Some(Duration::from_secs(1)));
        clock.advance(Duration::from_secs(1));
        assert!(matches!(watch.tick(&script), WatchStep::Update(s) if s.state == JobState::Processing));
        let outcome = watch.run(&script, &mut |_| {});
        assert!(matches!(outcome, WatchOutcome::Terminal(ref s) if s.state == JobState::Completed), "{outcome:?}");
        assert!(outcome.is_success() && outcome.error().is_none());
        assert_eq!(script.polls(), 4);
        // Done is sticky and makes no further request.
        assert!(matches!(watch.tick(&script), WatchStep::Done(_)));
        assert_eq!(script.polls(), 4);
        assert_eq!(watch.next_poll_in(), None);
        assert!(clock.now() >= Duration::from_secs(4), "the run slept on the injected clock: {:?}", clock.now());
    }

    #[test]
    fn the_gap_grows_while_nothing_changes_and_resets_on_a_change() {
        let script = Script::new(vec![
            Ok(status(JobState::Processing)),
            Ok(status(JobState::Processing)),
            Ok(status(JobState::Processing)),
            Ok(status(JobState::Stopped)),
            Ok(status(JobState::Completed)),
        ]);
        let policy =
            WaitPolicy { interval: Some(Duration::from_secs(1)), growth_percent: 200, max_interval: Duration::from_secs(3), ..Default::default() };
        let (mut watch, clock) = watch(policy);
        let mut gaps = Vec::new();
        loop {
            match watch.tick(&script) {
                WatchStep::Update(_) => gaps.push(watch.next_poll_in().unwrap_or_default().as_secs()),
                WatchStep::Pending { next_in } => clock.advance(next_in),
                WatchStep::Done(_) => break,
                _ => gaps.push(999),
            }
        }
        // 1st reading is a change (1s); same again doubles (2s), then the cap (3s); a change resets to 1s.
        assert_eq!(gaps, [1, 2, 3, 1]);
    }

    #[test]
    fn a_backend_that_cannot_report_ends_with_no_status_and_no_request() {
        let script = Script::new(vec![Ok(status(JobState::Unknown))]);
        let (watch, _) = watch(WaitPolicy::default());
        let mut watch = watch.with_support(WatchSupport::None);
        assert_eq!(watch.tick(&script), WatchStep::Done(WatchOutcome::NoStatus));
        assert_eq!(script.polls(), 0);
        let (watch2, _) = self::watch(WaitPolicy::default());
        let mut watch2 = watch2.with_support(WatchSupport::None);
        assert_eq!(watch2.poll_status(&script), WatchStep::Done(WatchOutcome::NoStatus));
    }

    #[test]
    fn a_backend_that_polls_but_stays_unknown_times_out_instead_of_spinning() {
        let script = Script::new(vec![Ok(status(JobState::Unknown))]);
        let (mut watch, clock) = watch(WaitPolicy::every(Duration::from_secs(5)).with_max_wait(Duration::from_secs(20)));
        let outcome = watch.run(&script, &mut |_| {});
        assert!(matches!(outcome, WatchOutcome::TimedOut { last: Some(ref s) } if s.state == JobState::Unknown), "{outcome:?}");
        assert!(!outcome.is_success() && outcome.error().is_none());
        assert!(clock.now() >= Duration::from_secs(20) && clock.now() < Duration::from_secs(26), "{:?}", clock.now());
        assert!(script.polls() <= 5, "{}", script.polls());
    }

    #[test]
    fn transient_poll_failures_back_off_then_recover() {
        let script = Script::new(vec![Err(busy()), Err(busy()), Ok(status(JobState::Processing)), Ok(status(JobState::Completed))]);
        let (mut watch, clock) = watch(WaitPolicy::every(Duration::from_secs(1)));
        let mut steps = Vec::new();
        let outcome = watch.run(&script, &mut |s| steps.push(s.clone()));
        assert!(outcome.is_success());
        let waits: Vec<Duration> =
            steps.iter().filter_map(|s| if let WatchStep::Retrying { next_in, .. } = s { Some(*next_in) } else { None }).collect();
        assert_eq!(waits.len(), 2, "{steps:?}");
        assert!(waits[1] > waits[0], "the backoff grows: {waits:?}");
        assert!(clock.now() > Duration::ZERO);
    }

    #[test]
    fn failures_that_outlast_the_policy_or_are_not_transient_end_the_watch() {
        let script = Script::new(vec![Err(busy())]);
        let policy = WaitPolicy {
            retry: RetryPolicy { max_attempts: 3, jitter_percent: 0, ..Default::default() },
            ..WaitPolicy::every(Duration::from_secs(1))
        };
        let (mut watch, _) = watch(policy);
        let outcome = watch.run(&script, &mut |_| {});
        assert!(matches!(outcome, WatchOutcome::Failed(ref e) if e.kind() == ErrorKind::Unavailable), "{outcome:?}");
        assert_eq!(script.polls(), 3, "the third failure is the last try");
        assert!(outcome.error().is_some());

        let script = Script::new(vec![Err(PrintError::new(ErrorKind::Rejected, "no"))]);
        let (mut watch, _) = self::watch(WaitPolicy::default());
        assert!(matches!(watch.run(&script, &mut |_| {}), WatchOutcome::Failed(_)));
        assert_eq!(script.polls(), 1, "not transient: no retry");

        // A poll error that says the outcome is unknown is never retried automatically.
        let script = Script::new(vec![Err(PrintError::new(ErrorKind::Timeout, "t").with_safety(crate::RetrySafety::OutcomeUnknown))]);
        let (mut watch, _) = self::watch(WaitPolicy::default());
        assert!(matches!(watch.run(&script, &mut |_| {}), WatchOutcome::Failed(_)));
        assert_eq!(script.polls(), 1);
    }

    #[test]
    fn a_forgotten_job_is_reported_with_its_last_status() {
        let script = Script::new(vec![Ok(status(JobState::Processing)), Err(PrintError::new(ErrorKind::NotFound, "forgotten"))]);
        let (mut watch, _) = watch(WaitPolicy::every(Duration::from_secs(1)));
        let outcome = watch.run(&script, &mut |_| {});
        assert!(matches!(outcome, WatchOutcome::Forgotten { last: Some(ref s) } if s.state == JobState::Processing), "{outcome:?}");
        assert!(outcome.is_success());
    }

    #[test]
    fn cancel_is_sent_once_and_the_end_state_is_the_result() {
        let script = Script::new(vec![Ok(status(JobState::Processing)), Ok(status(JobState::Cancelled))]);
        let cancel = CancelToken::new();
        let (watch, _) = watch(WaitPolicy::every(Duration::from_secs(1)));
        let mut watch = watch.with_cancel(cancel.clone());
        assert!(matches!(watch.tick(&script), WatchStep::Update(_)));
        cancel.cancel();
        assert_eq!(watch.next_poll_in(), Some(Duration::ZERO));
        let outcome = watch.run(&script, &mut |_| {});
        assert_eq!(script.cancels(), 1, "one cancel, however long the watch goes on");
        assert!(matches!(outcome, WatchOutcome::Terminal(ref s) if s.state == JobState::Cancelled));
        assert_eq!(outcome.error().map(|e| e.kind()), Some(ErrorKind::Cancelled));
    }

    #[test]
    fn a_job_that_vanishes_after_a_cancel_counts_as_cancelled() {
        let script = Script::new(vec![Err(PrintError::new(ErrorKind::NotFound, "gone"))]);
        let cancel = CancelToken::new();
        cancel.cancel();
        let (watch, _) = watch(WaitPolicy::default());
        let mut watch = watch.with_cancel(cancel);
        let outcome = watch.run(&script, &mut |_| {});
        assert!(matches!(outcome, WatchOutcome::Terminal(ref s) if s.state == JobState::Cancelled), "{outcome:?}");
    }

    #[test]
    fn a_cancel_the_backend_cannot_do_ends_the_watch_with_its_error() {
        let script = Script::new(vec![Ok(status(JobState::Processing))]);
        script.cancels.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(Err(PrintError::new(ErrorKind::Unsupported, "no cancel")));
        let cancel = CancelToken::new();
        cancel.cancel();
        let (watch, _) = watch(WaitPolicy::default());
        let mut watch = watch.with_cancel(cancel);
        assert!(matches!(watch.tick(&script), WatchStep::Done(WatchOutcome::Failed(e)) if e.kind() == ErrorKind::Unsupported));
    }

    #[test]
    fn poll_status_ignores_the_schedule() {
        let script = Script::new(vec![Ok(status(JobState::Processing))]);
        let (mut watch, _) = watch(WaitPolicy::every(Duration::from_secs(60)));
        assert!(matches!(watch.tick(&script), WatchStep::Update(_)));
        assert!(matches!(watch.tick(&script), WatchStep::Pending { .. }));
        assert!(matches!(watch.poll_status(&script), WatchStep::Update(_)));
        assert_eq!(script.polls(), 2);
        assert_eq!(watch.polls(), 2);
    }

    #[test]
    fn the_deadline_is_a_finite_default_and_ends_with_timed_out() {
        assert_eq!(WaitPolicy::default().max_wait, Duration::from_secs(120));
        let script = Script::new(vec![Ok(status(JobState::Processing))]);
        let (mut watch, clock) = watch(WaitPolicy::every(Duration::from_secs(1)).with_max_wait(Duration::from_secs(3)));
        let outcome = watch.run(&script, &mut |_| {});
        assert!(matches!(outcome, WatchOutcome::TimedOut { last: Some(_) }));
        assert_eq!(clock.now(), Duration::from_secs(3), "no overshoot");
    }

    #[test]
    fn terminal_failures_map_to_errors() {
        let t = |state, reasons: &[&str]| {
            let mut s = status(state);
            s.reasons = reasons.iter().map(|r| (*r).to_owned()).collect();
            WatchOutcome::Terminal(s).error()
        };
        assert_eq!(
            t(JobState::Aborted, &["format"]).map(|e| (e.kind(), e.message().to_owned())),
            Some((ErrorKind::Rejected, "the printer aborted the job (format)".into()))
        );
        assert_eq!(t(JobState::CompletedWithErrors, &[]).map(|e| e.kind()), Some(ErrorKind::Failed));
        assert_eq!(t(JobState::Completed, &[]), None);
        assert_eq!(t(JobState::CompletedWithWarnings, &["w"]), None);
        assert!(WatchOutcome::Forgotten { last: None }.error().is_none() && WatchOutcome::NoStatus.error().is_none());
    }

    #[test]
    fn a_watch_for_a_submission_takes_support_and_interval_from_it() {
        let mut submitted = Submitted::new("t", "ok");
        assert!(JobWatch::for_submitted(&submitted, WaitPolicy::default()).is_none(), "no job handle, nothing to follow");
        submitted.job = Some(JobRef::namespaced("t", "1"));
        submitted.watch = WatchSupport::Poll;
        submitted.poll_interval = Some(Duration::from_secs(7));
        let mut watch = JobWatch::for_submitted(&submitted, WaitPolicy::default())
            .map(|w| w.with_clock(Arc::new(ManualClock::new())))
            .unwrap_or_else(|| JobWatch::new(JobRef::new("x"), WaitPolicy::default()));
        let script = Script::new(vec![Ok(status(JobState::Processing))]);
        assert!(matches!(watch.tick(&script), WatchStep::Update(_)));
        assert_eq!(watch.next_poll_in(), Some(Duration::from_secs(7)));
    }
}
