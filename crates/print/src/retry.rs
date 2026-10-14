//! Retry semantics: whether an error is transient ([`crate::PrintError::is_retryable`]), whether
//! repeating the request could print twice ([`RetrySafety`]), where it failed ([`Phase`]) and how
//! long to wait ([`RetryPolicy`]). Also the small clock and jitter sources that make waiting
//! testable.
//!
//! Two questions, kept apart on purpose:
//!
//! * *Is the condition transient?* A busy printer, a dropped connection and a timeout are; a
//!   refused address, a format the printer does not take and a redirect are not.
//! * *Is repeating the request safe?* Only if nothing was accepted, or the request is idempotent.
//!   A connection that closed after the request was written, or a timeout while waiting for the
//!   reply to a create/send, may have been delivered: retrying could print twice. Such an error is
//!   transient **and** [`RetrySafety::OutcomeUnknown`]; [`RetryPolicy`] never retries it on its own,
//!   and a backend that can offers a reconcile step (look for the job before resubmitting).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use crate::backend::{CancelToken, PrintError};

/// Where in an exchange a failure happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Phase {
    /// Before any traffic: validation, policy, consent, encoding the document.
    Prepare,
    /// Making the connection. Nothing was sent.
    Connect,
    /// Writing the request. It may or may not have arrived.
    Request,
    /// The request was sent; the reply is missing, late or unusable.
    Reply,
    /// After the printer accepted the job, while following it.
    Poll,
}

impl Phase {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Prepare => "prepare",
            Self::Connect => "connect",
            Self::Request => "request",
            Self::Reply => "reply",
            Self::Poll => "poll",
        }
    }
}

/// What repeating the failed request would do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RetrySafety {
    /// Nothing was sent or accepted (refused before connect, policy, a connect failure, a 429 or
    /// 503 before the printer processed anything). Repeating cannot duplicate anything.
    Safe,
    /// The request may have been delivered (the connection closed or reset after the request was
    /// written, a timeout waiting for the reply to a create/send). Repeating a submit could print
    /// twice: reconcile first.
    OutcomeUnknown,
    /// The request is a query or a cancel: repeating it is harmless.
    Idempotent,
}

impl RetrySafety {
    /// Whether a retry may be made without asking a person or reconciling first.
    pub const fn allows_automatic_retry(self) -> bool {
        !matches!(self, Self::OutcomeUnknown)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::Idempotent => "idempotent",
        }
    }
}

/// A monotonic clock that can also wait. [`SystemClock`] is the real one; [`ManualClock`] makes
/// tests instant and exact.
pub trait Clock: Send + Sync {
    /// Time since an arbitrary fixed origin; never goes backwards.
    fn now(&self) -> Duration;
    /// Wait. A manual clock just moves its time forward.
    fn sleep(&self, duration: Duration);
}

/// The real clock (`std::time::Instant`, `std::thread::sleep`). Not available on
/// `wasm32-unknown-unknown`: pass another [`Clock`] there.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        static START: OnceLock<Instant> = OnceLock::new();
        START.get_or_init(Instant::now).elapsed()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// A clock a test moves by hand. Clones share the time; `sleep` advances it, so code that waits
/// finishes at once and the total waited can be asserted.
#[derive(Clone, Debug, Default)]
pub struct ManualClock {
    now: Arc<Mutex<Duration>>,
    slept: Arc<Mutex<Vec<Duration>>>,
}

impl ManualClock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn advance(&self, by: Duration) {
        let mut now = self.now.lock().unwrap_or_else(PoisonError::into_inner);
        *now = now.saturating_add(by);
    }

    /// Every duration passed to `sleep`, in order.
    pub fn sleeps(&self) -> Vec<Duration> {
        self.slept.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        *self.now.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn sleep(&self, duration: Duration) {
        self.slept.lock().unwrap_or_else(PoisonError::into_inner).push(duration);
        self.advance(duration);
    }
}

/// Sleep in short slices so a [`CancelToken`] is noticed within [`CANCEL_SLICE`].
pub(crate) fn sleep_cancellable(clock: &dyn Clock, cancel: &CancelToken, mut duration: Duration) {
    while !duration.is_zero() && !cancel.is_cancelled() {
        let step = duration.min(CANCEL_SLICE);
        clock.sleep(step);
        duration -= step;
    }
}

/// The longest a waiting helper sleeps without looking at its cancel token.
pub const CANCEL_SLICE: Duration = Duration::from_millis(100);

/// A tiny deterministic random source for jitter (xorshift64*). Not for anything but spreading
/// retries out.
#[derive(Clone, Debug)]
pub struct Jitter(u64);

impl Jitter {
    /// A source with this seed (a zero seed is replaced; the same seed gives the same sequence).
    pub fn new(seed: u64) -> Self {
        Self(if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed })
    }

    /// A source seeded from a clock reading and a process-wide counter, so two watchers started
    /// together do not wait in step.
    pub fn from_clock(clock: &dyn Clock) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        let salt = COUNTER.fetch_add(0x9E37_79B9, Ordering::Relaxed);
        Self::new((clock.now().as_nanos() as u64) ^ salt.rotate_left(17))
    }

    /// The next value in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        let out = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (out >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// How often and how long to retry. Attempts count tries, the first one included: `max_attempts: 1`
/// never retries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    /// The wait after the first failure; it doubles per failure up to `max_delay`.
    pub base: Duration,
    pub max_delay: Duration,
    /// Spread each wait by up to this many percent either way (0 to 100).
    pub jitter_percent: u32,
    /// Longest a device's own "come back in" hint is honoured.
    pub max_retry_after: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
            jitter_percent: 20,
            max_retry_after: Duration::from_secs(60),
        }
    }
}

impl RetryPolicy {
    /// A policy that never retries.
    pub fn none() -> Self {
        Self { max_attempts: 1, ..Self::default() }
    }

    /// The wait after failure number `attempt` (1 is the first failure): `base * 2^(attempt-1)`
    /// capped at `max_delay`, spread by `jitter_percent` around its value with `unit` in `[0, 1]`
    /// (a non-finite `unit` counts as the middle, no spread). Never longer than `max_delay`.
    pub fn delay(&self, attempt: u32, unit: f64) -> Duration {
        let doublings = attempt.saturating_sub(1).min(30);
        let backoff = self.base.saturating_mul(1u32 << doublings).min(self.max_delay);
        let unit = if unit.is_finite() { unit.clamp(0.0, 1.0) } else { 0.5 };
        let spread = f64::from(self.jitter_percent.min(100)) / 100.0;
        let factor = 1.0 + (unit * 2.0 - 1.0) * spread;
        Duration::try_from_secs_f64(backoff.as_secs_f64() * factor).unwrap_or(backoff).min(self.max_delay)
    }

    /// What to do after `error` on try number `attempt`: `Some(wait)` to try again, `None` to give
    /// up. `None` when the error is not transient, when repeating could duplicate the job
    /// ([`RetrySafety::OutcomeUnknown`]) or when the attempts are used up. A device hint
    /// ([`PrintError::retry_after`]) makes the wait at least that long, up to `max_retry_after`.
    pub fn next_delay(&self, error: &PrintError, attempt: u32, unit: f64) -> Option<Duration> {
        if attempt >= self.max_attempts || !error.is_retryable() || !error.retry_safety().allows_automatic_retry() {
            return None;
        }
        let hint = error.retry_after().map_or(Duration::ZERO, |d| d.min(self.max_retry_after));
        Some(self.delay(attempt, unit).max(hint))
    }
}

/// Run `op` (given the try number, from 1) until it succeeds or [`RetryPolicy::next_delay`] says
/// stop. The waits go through `clock` and a set `cancel` ends the loop with the last error.
///
/// Use it for queries, cancels and anything else whose errors are [`RetrySafety::Safe`] or
/// [`RetrySafety::Idempotent`]. A submit whose error is [`RetrySafety::OutcomeUnknown`] comes back
/// after the first try: reconcile, do not repeat.
pub fn retry<T>(
    policy: &RetryPolicy,
    clock: &dyn Clock,
    cancel: &CancelToken,
    jitter: &mut Jitter,
    mut op: impl FnMut(u32) -> Result<T, PrintError>,
) -> Result<T, PrintError> {
    let mut attempt = 1u32;
    loop {
        let error = match op(attempt) {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        match policy.next_delay(&error, attempt, jitter.unit()) {
            Some(wait) if !cancel.is_cancelled() => sleep_cancellable(clock, cancel, wait),
            _ => return Err(error),
        }
        attempt = attempt.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::ErrorKind;

    fn policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 5,
            base: Duration::from_millis(100),
            max_delay: Duration::from_millis(1000),
            jitter_percent: 0,
            ..Default::default()
        }
    }

    #[test]
    fn delays_double_up_to_the_cap() {
        let p = policy();
        let ms: Vec<u128> = (1..=6).map(|a| p.delay(a, 0.5).as_millis()).collect();
        assert_eq!(ms, [100, 200, 400, 800, 1000, 1000]);
        assert_eq!(p.delay(0, 0.5), Duration::from_millis(100), "attempt 0 is treated like 1");
        assert_eq!(p.delay(u32::MAX, 0.5), Duration::from_millis(1000), "no overflow");
    }

    #[test]
    fn jitter_spreads_within_its_bounds_and_never_exceeds_the_cap() {
        let p = RetryPolicy { jitter_percent: 50, ..policy() };
        assert_eq!(p.delay(1, 0.0), Duration::from_millis(50));
        assert_eq!(p.delay(1, 1.0), Duration::from_millis(150));
        assert_eq!(p.delay(1, 0.5), Duration::from_millis(100));
        assert_eq!(p.delay(5, 1.0), Duration::from_millis(1000), "capped after jitter");
        assert_eq!(p.delay(1, f64::NAN), Duration::from_millis(100));
        assert_eq!(p.delay(1, -3.0), Duration::from_millis(50));
        let huge = RetryPolicy { max_delay: Duration::MAX, base: Duration::MAX, ..p };
        let _ = huge.delay(40, 1.0); // must not panic
        let mut j = Jitter::new(7);
        for _ in 0..1000 {
            let u = j.unit();
            assert!((0.0..1.0).contains(&u));
        }
        assert_eq!(Jitter::new(7).unit(), Jitter::new(7).unit());
        assert_ne!(Jitter::new(7).unit(), Jitter::new(8).unit());
    }

    fn error(kind: ErrorKind, safety: RetrySafety) -> PrintError {
        PrintError::new(kind, "x").with_safety(safety)
    }

    #[test]
    fn only_transient_and_repeatable_errors_are_retried_and_attempts_are_bounded() {
        let p = policy();
        assert!(p.next_delay(&error(ErrorKind::Busy, RetrySafety::Safe), 1, 0.5).is_some());
        assert!(p.next_delay(&error(ErrorKind::Timeout, RetrySafety::Idempotent), 1, 0.5).is_some());
        assert_eq!(p.next_delay(&error(ErrorKind::Timeout, RetrySafety::OutcomeUnknown), 1, 0.5), None, "may have printed: never automatic");
        assert_eq!(p.next_delay(&error(ErrorKind::Rejected, RetrySafety::Safe), 1, 0.5), None, "not transient");
        assert_eq!(p.next_delay(&error(ErrorKind::Busy, RetrySafety::Safe), 5, 0.5), None, "attempts used up");
        assert_eq!(RetryPolicy::none().next_delay(&error(ErrorKind::Busy, RetrySafety::Safe), 1, 0.5), None);
    }

    #[test]
    fn a_device_hint_lengthens_the_wait_up_to_a_cap() {
        let p = RetryPolicy { max_retry_after: Duration::from_secs(10), ..policy() };
        let busy = |secs| error(ErrorKind::Busy, RetrySafety::Safe).with_retry_after(Duration::from_secs(secs));
        assert_eq!(p.next_delay(&busy(5), 1, 0.5), Some(Duration::from_secs(5)));
        assert_eq!(p.next_delay(&busy(3600), 1, 0.5), Some(Duration::from_secs(10)));
        assert_eq!(p.next_delay(&busy(0), 1, 0.5), Some(Duration::from_millis(100)), "backoff still applies");
    }

    #[test]
    fn retry_waits_on_the_clock_and_stops_at_success_or_an_unsafe_error() {
        let clock = ManualClock::new();
        let cancel = CancelToken::new();
        let mut jitter = Jitter::new(1);
        let mut tries = 0;
        let got = retry(&policy(), &clock, &cancel, &mut jitter, |n| {
            tries = n;
            if n < 3 { Err(error(ErrorKind::Unavailable, RetrySafety::Idempotent)) } else { Ok(n) }
        });
        assert_eq!((got, tries), (Ok(3), 3));
        let total: Duration = clock.sleeps().iter().sum();
        assert!(total >= Duration::from_millis(100 + 200) * 8 / 10, "{total:?}");

        let mut calls = 0;
        let got: Result<(), _> = retry(&policy(), &clock, &cancel, &mut jitter, |_| {
            calls += 1;
            Err(error(ErrorKind::Timeout, RetrySafety::OutcomeUnknown))
        });
        assert_eq!(calls, 1);
        assert_eq!(got.map_err(|e| e.retry_safety()), Err(RetrySafety::OutcomeUnknown));

        let mut calls = 0;
        let _: Result<(), _> = retry(&policy(), &clock, &cancel, &mut jitter, |_| {
            calls += 1;
            Err(error(ErrorKind::Busy, RetrySafety::Safe))
        });
        assert_eq!(calls, 5, "max_attempts bounds the loop");
    }

    #[test]
    fn a_cancelled_token_ends_the_loop_after_the_current_try() {
        let clock = ManualClock::new();
        let cancel = CancelToken::new();
        let mut jitter = Jitter::new(1);
        let mut calls = 0;
        let _: Result<(), _> = retry(&policy(), &clock, &cancel, &mut jitter, |_| {
            calls += 1;
            cancel.cancel();
            Err(error(ErrorKind::Busy, RetrySafety::Safe))
        });
        assert_eq!(calls, 1);
    }

    #[test]
    fn the_manual_clock_records_and_shares_time() {
        let clock = ManualClock::new();
        let other = clock.clone();
        clock.sleep(Duration::from_secs(2));
        other.advance(Duration::from_secs(1));
        assert_eq!(clock.now(), Duration::from_secs(3));
        assert_eq!(other.sleeps(), [Duration::from_secs(2)]);
        let cancel = CancelToken::new();
        sleep_cancellable(&clock, &cancel, Duration::from_millis(250));
        assert_eq!(clock.sleeps().len(), 4, "100 + 100 + 50 ms after the 2 s sleep");
    }

    #[test]
    fn the_system_clock_moves_forward() {
        let a = SystemClock.now();
        SystemClock.sleep(Duration::from_millis(2));
        assert!(SystemClock.now() > a);
    }
}
