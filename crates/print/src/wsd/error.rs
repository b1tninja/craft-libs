//! The WS-Print layer's error type, and its mapping onto [`crate::PrintError`].

use std::fmt;
use std::time::Duration;

use super::adapter::WireError;
use super::fault::{Fault, FaultCode};
use crate::{ErrorKind, NativeCode, Phase, PrintError, RetrySafety};

/// Everything that can go wrong, as values an interface can show and an agent can act on.
#[derive(Debug, Clone, PartialEq)]
pub enum WsdError {
    /// The network policy refused (see `policy`): the URL, or a missing confirmation.
    Policy(String),
    /// The transport failed before a usable HTTP reply arrived (connect, timeout, redirect refused, ...).
    Transport(String),
    /// The printer closed the connection without replying (retryable).
    ConnectionClosed,
    /// An HTTP status that is not success, with no SOAP fault in the body.
    Http(u16),
    /// A size cap was hit (what, the limit in bytes or items).
    TooLarge(&'static str, usize),
    /// The reply is not well-formed XML (or uses something refused, such as a DTD).
    Xml(String),
    /// Well-formed, but not the message that was expected.
    Protocol(String),
    /// The printer answered with a SOAP fault.
    Fault(Fault),
    /// An argument or option is invalid (checked before anything is sent).
    Invalid(String),
    /// The printer does not accept any format the document can be produced in.
    NoCommonFormat { offered: Vec<String>, accepted: Vec<String> },
    /// A requested option is outside what the printer lists (strict mode), or cannot be carried by
    /// the chosen ticket shape.
    Unsupported(String),
    /// The document could not be produced.
    Encode(String),
    /// The caller cancelled; nothing further was sent.
    Cancelled,
    /// A deadline passed.
    Timeout,
    /// The printer does not offer what was asked for.
    NotFound(String),
}

impl fmt::Display for WsdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Policy(m) => write!(f, "refused by policy: {m}"),
            Self::Transport(m) => write!(f, "transport: {m}"),
            Self::ConnectionClosed => f.write_str("the printer closed the connection without replying"),
            Self::Http(s) => write!(f, "the printer answered HTTP {s}"),
            Self::TooLarge(what, n) => write!(f, "{what} is larger than the limit of {n}"),
            Self::Xml(m) => write!(f, "malformed XML: {m}"),
            Self::Protocol(m) => write!(f, "unexpected reply: {m}"),
            Self::Fault(fault) => fault.fmt(f),
            Self::Invalid(m) => write!(f, "invalid {m}"),
            Self::NoCommonFormat { offered, accepted } => {
                write!(f, "the printer accepts none of the offered formats (offered: {offered:?}; printer accepts: {accepted:?})")
            }
            Self::Unsupported(m) => write!(f, "the printer does not support {m}"),
            Self::Encode(m) => write!(f, "encoding failed: {m}"),
            Self::Cancelled => f.write_str("cancelled"),
            Self::Timeout => f.write_str("the deadline passed"),
            Self::NotFound(m) => write!(f, "not found: {m}"),
        }
    }
}

impl std::error::Error for WsdError {}

impl From<WireError> for WsdError {
    /// The shared protocol errors map onto the same-named variants.
    fn from(e: WireError) -> Self {
        match e {
            WireError::Policy(m) => Self::Policy(m),
            WireError::Transport(m) => Self::Transport(m),
            WireError::ConnectionClosed => Self::ConnectionClosed,
            WireError::Http(s) => Self::Http(s),
            WireError::Cancelled => Self::Cancelled,
            WireError::Timeout => Self::Timeout,
            WireError::NotFound(m) => Self::NotFound(m),
            WireError::TooLarge(what, n) => Self::TooLarge(what, n),
            WireError::Xml(m) => Self::Xml(m),
            WireError::Protocol(m) => Self::Protocol(m),
            WireError::Fault(f) => Self::Fault(f.into()),
            WireError::Invalid(m) => Self::Invalid(m),
            // The core's error type is non-exhaustive: a future variant is at least a protocol problem.
            other => Self::Protocol(other.to_string()),
        }
    }
}

pub type Result<T, E = WsdError> = std::result::Result<T, E>;

/// Every value [`WsdError::kind`] can return.
pub const KINDS: &[&str] = &[
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
    "cancelled",
    "timeout",
    "not_found",
];

/// What the printer's reply is called in [`WsdError::TooLarge`].
pub(crate) const REPLY_LABEL: &str = "the printer's reply";

/// The [`WsdError::TooLarge`] labels that name something this side sends or produces.
const REQUEST_LABELS: &[&str] = &["the request", "an inline (base64) document", "the raster stream", "a raster page"];

impl WsdError {
    /// A stable lowercase name for the variant, one of [`KINDS`].
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Policy(_) => "policy",
            Self::Transport(_) => "transport",
            Self::ConnectionClosed => "connection_closed",
            Self::Http(_) => "http",
            Self::TooLarge(..) => "too_large",
            Self::Xml(_) => "xml",
            Self::Protocol(_) => "protocol",
            Self::Fault(_) => "fault",
            Self::Invalid(_) => "invalid",
            Self::NoCommonFormat { .. } => "no_common_format",
            Self::Unsupported(_) => "unsupported",
            Self::Encode(_) => "encode",
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::NotFound(_) => "not_found",
        }
    }

    /// Would trying the same request again later plausibly work? The condition is transient: HTTP
    /// 408, 425, 429 and 5xx except 501 and 505, a connection that failed or that the printer
    /// closed, a deadline, and a fault whose code says so. It is exactly
    /// [`ErrorKind::is_retryable`] of [`Self::error_kind`]. Whether sending again is *safe* depends
    /// on the operation too: see [`Self::classify`].
    pub fn is_retryable(&self) -> bool {
        self.error_kind().is_retryable()
    }

    /// The fault, when the printer sent one.
    pub fn fault(&self) -> Option<&Fault> {
        match self {
            Self::Fault(f) => Some(f),
            _ => None,
        }
    }

    /// The neutral kind this error is reported as. It does not depend on the operation.
    /// `is_retryable()` of the result equals [`Self::is_retryable`] (see the table test).
    pub fn error_kind(&self) -> ErrorKind {
        self.classify(Op::Send).kind
    }

    /// The whole reading of this error for an operation: the neutral kind, where it failed, whether
    /// repeating the request is safe, and how long to wait. The table is in the crate's
    /// `docs/job-lifecycle.md` and pinned by the table tests.
    ///
    /// The principle: **a failure that proves the printer did not act is [`RetrySafety::Safe`]**
    /// (nothing sent, a refused address, a connect failure, a policy refusal, 408/425/429/503, a SOAP
    /// fault, which is a definite answer). **A failure after the request may have arrived is
    /// [`RetrySafety::OutcomeUnknown`] for a non-idempotent operation** (create, send: a closed or
    /// reset connection, a timeout waiting for the reply, a 500/502/504, an unreadable reply) and
    /// [`RetrySafety::Idempotent`] for a query or a cancel, which may be repeated freely.
    pub fn classify(&self, op: Op) -> Class {
        use ErrorKind as K;
        use Phase as P;
        use RetrySafety as S;
        // After the request may have been delivered.
        let reached = if op.is_idempotent() { S::Idempotent } else { S::OutcomeUnknown };
        // A status poll's failures are all "poll" failures, whatever the network step was.
        let net = |phase: Phase| if op == Op::Status { P::Poll } else { phase };
        let class = |kind, phase, safety| Class { kind, phase, safety, retry_after: None };
        match self {
            Self::Policy(_) | Self::Invalid(_) | Self::Unsupported(_) => class(K::InvalidRequest, P::Prepare, S::Safe),
            Self::NoCommonFormat { .. } => class(K::UnsupportedFormat, P::Prepare, S::Safe),
            Self::Encode(_) => class(K::Failed, P::Prepare, S::Safe),
            Self::NotFound(_) => class(K::NotFound, P::Prepare, S::Safe),
            Self::Cancelled => class(K::Cancelled, P::Prepare, S::Safe),
            // A cap on something we send is the caller's document being too big; a cap on what came
            // back (the reply, a list, a multipart message) is the printer's doing.
            Self::TooLarge(what, _) if REQUEST_LABELS.contains(what) => class(K::InvalidRequest, P::Prepare, S::Safe),
            Self::TooLarge(..) | Self::Xml(_) | Self::Protocol(_) => class(K::Failed, net(P::Reply), reached),
            Self::ConnectionClosed => class(K::Unavailable, net(P::Reply), reached),
            Self::Timeout => class(K::Timeout, net(P::Reply), reached),
            Self::Transport(message) => match transport_kind(message) {
                TransportKind::Redirect => class(K::Failed, net(P::Reply), reached),
                TransportKind::ConnectFailed => class(K::Unavailable, net(P::Connect), S::Safe),
                TransportKind::ConnectTimeout => class(K::Timeout, net(P::Connect), S::Safe),
                TransportKind::Timeout => class(K::Timeout, net(P::Reply), reached),
                TransportKind::Other => class(K::Unavailable, net(P::Reply), reached),
            },
            Self::Http(status) => http_class(*status, net(P::Request), net(P::Reply), reached),
            Self::Fault(fault) => {
                let kind = fault_kind(&fault.code);
                let retry_after = fault_retry_after(&fault.code);
                // A fault is a definite answer: the printer declined, so nothing was accepted. The
                // one exception says an earlier send was in fact accepted.
                let safety = match fault.code {
                    FaultCode::ClientErrorLastDocumentAlreadySent => reached,
                    _ => S::Safe,
                };
                Class { kind, phase: net(P::Reply), safety, retry_after }
            }
        }
    }

    /// The neutral error for this one, as seen by a caller of operation `op`: the kind, the message,
    /// the native fault code, [`Phase`], [`RetrySafety`] and [`PrintError::retry_after`].
    pub fn print_error(&self, op: Op) -> PrintError {
        let class = self.classify(op);
        let mut error = PrintError::new(class.kind, self.to_string()).with_phase(class.phase).with_safety(class.safety);
        if let Some(wait) = class.retry_after {
            error = error.with_retry_after(wait);
        }
        if let Self::Fault(f) = self {
            let (code, name) = f.code.native();
            error = error.with_native(NativeCode { domain: "wsd-fault", code, name });
        }
        error
    }
}

/// The protocol operation that was in flight when an error happened. It decides whether repeating
/// the request is safe: a query or a cancel may be repeated, a create or a send may not (the first
/// one may have arrived).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Op {
    /// `GetPrinterElements`, `GetActiveJobs`, `GetJobHistory`: reads.
    Query,
    /// `CreatePrintJob`.
    Create,
    /// `SendDocument`.
    Send,
    /// `GetJobElements` while following a job.
    Status,
    /// `CancelJob`.
    Cancel,
}

impl Op {
    /// Repeating the request changes nothing the first one did not.
    pub const fn is_idempotent(self) -> bool {
        matches!(self, Self::Query | Self::Status | Self::Cancel)
    }

    pub const ALL: [Op; 5] = [Op::Query, Op::Create, Op::Send, Op::Status, Op::Cancel];
}

/// How an error reads to a caller; see [`WsdError::classify`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Class {
    pub kind: ErrorKind,
    pub phase: Phase,
    pub safety: RetrySafety,
    /// How long to wait before another try, for the conditions where the printer is merely busy.
    pub retry_after: Option<Duration>,
}

/// What the transport's failure text says. `craft-wsd` reports transport failures as text, so this
/// reads that text; an unrecognised message is [`TransportKind::Other`], which is treated as
/// "the request may have arrived" (the careful reading).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransportKind {
    /// A redirect was answered and refused.
    Redirect,
    /// The connection could not be made (refused, unreachable): nothing was sent.
    ConnectFailed,
    /// The connection attempt timed out: nothing was sent.
    ConnectTimeout,
    /// A deadline passed after the request was sent, or the phase is unknown.
    Timeout,
    Other,
}

fn transport_kind(message: &str) -> TransportKind {
    let m = message.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| m.contains(n));
    if has(&["redirect"]) {
        TransportKind::Redirect
    } else if has(&["connection attempt failed", "timeout: connect", "connect timed out", "connection timed out"]) {
        TransportKind::ConnectTimeout
    } else if has(&["refused", "unreachable", "no route to host", "host is down", "network is down"]) {
        TransportKind::ConnectFailed
    } else if has(&["timeout", "timed out"]) {
        TransportKind::Timeout
    } else {
        TransportKind::Other
    }
}

/// HTTP statuses without a SOAP fault. `request` and `reply` are the phases for "the server
/// refused before processing" and "it answered something unusable"; `reached` is the safety when
/// the request may have been processed.
fn http_class(status: u16, request: Phase, reply: Phase, reached: RetrySafety) -> Class {
    use ErrorKind as K;
    let class = |kind, phase, safety, retry_after| Class { kind, phase, safety, retry_after };
    match status {
        // The server says it did not take the request, or is not ready for it: nothing happened.
        408 => class(K::Timeout, request, RetrySafety::Safe, None),
        425 => class(K::Busy, request, RetrySafety::Safe, Some(Duration::from_secs(2))),
        429 => class(K::Busy, request, RetrySafety::Safe, Some(Duration::from_secs(5))),
        503 => class(K::Unavailable, request, RetrySafety::Safe, Some(Duration::from_secs(5))),
        // Not implemented and unsupported version are permanent, and nothing was done.
        501 | 505 => class(K::Failed, reply, RetrySafety::Safe, None),
        // A server-side failure or a gateway that gave up: the request may well have been processed.
        500..=599 => class(K::Unavailable, reply, reached, None),
        // Redirects and other statuses we do not follow: not transient.
        300..=399 => class(K::Failed, reply, reached, None),
        _ => class(K::Failed, reply, RetrySafety::Safe, None),
    }
}

/// The wait a busy printer is told to observe when it gives no hint of its own. These are defaults,
/// not measurements: a printer that is not taking jobs is in a mode that lasts, a temporary error
/// clears sooner, and a document still being processed is the shortest wait.
fn fault_retry_after(code: &FaultCode) -> Option<Duration> {
    match code {
        FaultCode::ServerErrorTemporaryError => Some(Duration::from_secs(5)),
        FaultCode::ServerErrorNotAcceptingJobs => Some(Duration::from_secs(15)),
        FaultCode::ServerErrorDocumentActive => Some(Duration::from_secs(2)),
        _ => None,
    }
}

/// How a fault's sub-code reads to a caller. The retryable codes ([`FaultCode::is_retryable`]) are
/// the ones that map to a retryable kind, so the two notions cannot drift apart.
fn fault_kind(code: &FaultCode) -> ErrorKind {
    use FaultCode as C;
    match code {
        C::ServerErrorTemporaryError | C::ServerErrorNotAcceptingJobs | C::ServerErrorDocumentActive => ErrorKind::Busy,
        C::ClientErrorFormatNotSupported | C::ClientErrorCompressionNotSupported => ErrorKind::UnsupportedFormat,
        C::ClientErrorJobIdNotFound => ErrorKind::NotFound,
        C::ServerErrorJobCancelled => ErrorKind::Cancelled,
        C::InvalidArgs
        | C::ClientErrorMultipleDocumentsNotSupported
        | C::ClientErrorDocumentURLSchemeNotSupported
        | C::ClientErrorDocumentURLNotFound
        | C::ClientErrorConflictingRequiredParameters => ErrorKind::InvalidRequest,
        C::ServerErrorInternalError | C::ClientErrorLastDocumentAlreadySent => ErrorKind::Failed,
        // The printer understood and declined: it is a refusal, whatever the vendor calls it.
        C::OperationFailed | C::ActionNotSupported | C::ServerErrorAddDocumentNotSupported | C::Other(..) => ErrorKind::Rejected,
    }
}

/// The conservative reading: as if the operation were a send, so that an ambiguous failure says
/// [`RetrySafety::OutcomeUnknown`]. Use [`WsdError::print_error`] with the real [`Op`] where it is
/// known (queries and cancels are [`RetrySafety::Idempotent`]).
impl From<WsdError> for PrintError {
    fn from(e: WsdError) -> Self {
        e.print_error(Op::Send)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wsd::adapter::{Role, SubCode};
    use crate::wsd::ns;

    fn wire_samples() -> Vec<WireError> {
        let fault = crate::wsd::adapter::WireFault {
            role: Role::Sender,
            code: SubCode { ns: Some(ns::WPRT.to_owned()), local: "InvalidArgs".into() },
            reason: String::new(),
            detail: String::new(),
        };
        vec![
            WireError::Policy("p".into()),
            WireError::Transport("t".into()),
            WireError::ConnectionClosed,
            WireError::Http(503),
            WireError::TooLarge("x", 1),
            WireError::Xml("x".into()),
            WireError::Protocol("x".into()),
            WireError::Fault(fault),
            WireError::Invalid("x".into()),
            WireError::Cancelled,
            WireError::Timeout,
            WireError::NotFound("x".into()),
        ]
    }

    #[test]
    fn every_core_error_kind_maps_to_one_of_ours() {
        use crate::wsd::adapter::WIRE_KINDS;
        let samples = wire_samples();
        for kind in WIRE_KINDS {
            let e = samples.iter().find(|e| e.kind() == *kind).unwrap_or_else(|| panic!("no sample for the core kind {kind}"));
            let ours = WsdError::from(e.clone());
            assert_eq!(ours.kind(), *kind, "{kind} keeps its name");
            assert!(KINDS.contains(&ours.kind()));
            // And the neutral error exists, with a message.
            let neutral = PrintError::from(ours.clone());
            assert!(!neutral.message().is_empty(), "{kind}");
            assert_eq!(neutral.is_retryable(), ours.is_retryable(), "{kind}");
        }
        assert_eq!(samples.len(), WIRE_KINDS.len());
    }

    fn fault(local: &str) -> WsdError {
        WsdError::Fault(Fault { role: Role::Receiver, code: FaultCode::from_qname(Some(ns::WPRT), local), reason: "r".into(), detail: String::new() })
    }

    /// One of every shape of error, with the transport texts that are classified.
    fn samples() -> Vec<WsdError> {
        let mut all = vec![
            WsdError::Policy(String::new()),
            WsdError::ConnectionClosed,
            WsdError::TooLarge(REPLY_LABEL, 1),
            WsdError::TooLarge("the device's reply", 1),
            WsdError::TooLarge("the request", 1),
            WsdError::Xml(String::new()),
            WsdError::Protocol(String::new()),
            WsdError::Invalid(String::new()),
            WsdError::NoCommonFormat { offered: vec![], accepted: vec![] },
            WsdError::Unsupported(String::new()),
            WsdError::Encode(String::new()),
            WsdError::Cancelled,
            WsdError::Timeout,
            WsdError::NotFound(String::new()),
            fault("ServerErrorNotAcceptingJobs"),
            fault("ServerErrorTemporaryError"),
            fault("ServerErrorDocumentActive"),
            fault("ServerErrorInternalError"),
            fault("ClientErrorLastDocumentAlreadySent"),
            fault("ClientErrorJobIdNotFound"),
            fault("OperationFailed"),
        ];
        for m in [
            "",
            "the device answered with a redirect (HTTP 302), which is refused",
            "io: No connection could be made because the target machine actively refused it. (os error 10061)",
            "io: Connection refused (os error 111)",
            "io: Network is unreachable (os error 101)",
            "timeout: global",
            "timeout: connect",
            "io: A connection attempt failed because the connected party did not properly respond after a period of time",
            "io: something odd",
        ] {
            all.push(WsdError::Transport(m.into()));
        }
        for s in [301, 400, 401, 403, 404, 408, 425, 429, 500, 501, 502, 503, 504, 505] {
            all.push(WsdError::Http(s));
        }
        all
    }

    #[test]
    fn every_kind_of_ours_maps_to_a_neutral_kind_and_keeps_retryability() {
        let all = [
            (WsdError::Policy(String::new()), ErrorKind::InvalidRequest),
            (WsdError::Transport(String::new()), ErrorKind::Unavailable),
            (WsdError::ConnectionClosed, ErrorKind::Unavailable),
            (WsdError::Http(503), ErrorKind::Unavailable),
            (WsdError::Http(429), ErrorKind::Busy),
            (WsdError::Http(404), ErrorKind::Failed),
            (WsdError::TooLarge(REPLY_LABEL, 1), ErrorKind::Failed),
            (WsdError::TooLarge("the device's reply", 1), ErrorKind::Failed),
            (WsdError::TooLarge("the request", 1), ErrorKind::InvalidRequest),
            (WsdError::Xml(String::new()), ErrorKind::Failed),
            (WsdError::Protocol(String::new()), ErrorKind::Failed),
            (fault("ServerErrorNotAcceptingJobs"), ErrorKind::Busy),
            (WsdError::Invalid(String::new()), ErrorKind::InvalidRequest),
            (WsdError::NoCommonFormat { offered: vec![], accepted: vec![] }, ErrorKind::UnsupportedFormat),
            (WsdError::Unsupported(String::new()), ErrorKind::InvalidRequest),
            (WsdError::Encode(String::new()), ErrorKind::Failed),
            (WsdError::Cancelled, ErrorKind::Cancelled),
            (WsdError::Timeout, ErrorKind::Timeout),
            (WsdError::NotFound(String::new()), ErrorKind::NotFound),
        ];
        let mut seen: Vec<&str> = all.iter().map(|(e, _)| e.kind()).collect();
        seen.sort_unstable();
        seen.dedup();
        let mut listed = KINDS.to_vec();
        listed.sort_unstable();
        assert_eq!(seen, listed, "the table covers every entry of KINDS");
        for (e, kind) in &all {
            assert!(KINDS.contains(&e.kind()), "{e}");
            let neutral = PrintError::from(e.clone());
            assert_eq!(neutral.kind(), *kind, "{e}");
            assert_eq!(neutral.is_retryable(), e.is_retryable(), "{e}");
            assert_eq!(neutral.message(), e.to_string());
        }
    }

    /// The retry table: (error, op) -> (kind, phase, safety, retry_after seconds). Every kind in
    /// [`KINDS`] appears, for a non-idempotent and for an idempotent operation where they differ.
    #[test]
    fn the_retry_table_for_every_kind() {
        use ErrorKind as K;
        use Phase as P;
        use RetrySafety as S;
        let transport = |m: &str| WsdError::Transport(m.into());
        let rows: Vec<(WsdError, Op, K, P, S, Option<u64>)> = vec![
            // Refused before anything is sent: not transient, safe.
            (WsdError::Policy("p".into()), Op::Create, K::InvalidRequest, P::Prepare, S::Safe, None),
            (WsdError::Invalid("i".into()), Op::Query, K::InvalidRequest, P::Prepare, S::Safe, None),
            (WsdError::Unsupported("u".into()), Op::Create, K::InvalidRequest, P::Prepare, S::Safe, None),
            (WsdError::NoCommonFormat { offered: vec![], accepted: vec![] }, Op::Query, K::UnsupportedFormat, P::Prepare, S::Safe, None),
            (WsdError::Encode("e".into()), Op::Create, K::Failed, P::Prepare, S::Safe, None),
            (WsdError::NotFound("n".into()), Op::Query, K::NotFound, P::Prepare, S::Safe, None),
            (WsdError::Cancelled, Op::Send, K::Cancelled, P::Prepare, S::Safe, None),
            (WsdError::TooLarge("the request", 1), Op::Send, K::InvalidRequest, P::Prepare, S::Safe, None),
            // The connection closed or timed out: transient, and after a write it may have arrived.
            (WsdError::ConnectionClosed, Op::Create, K::Unavailable, P::Reply, S::OutcomeUnknown, None),
            (WsdError::ConnectionClosed, Op::Send, K::Unavailable, P::Reply, S::OutcomeUnknown, None),
            (WsdError::ConnectionClosed, Op::Query, K::Unavailable, P::Reply, S::Idempotent, None),
            (WsdError::ConnectionClosed, Op::Cancel, K::Unavailable, P::Reply, S::Idempotent, None),
            (WsdError::ConnectionClosed, Op::Status, K::Unavailable, P::Poll, S::Idempotent, None),
            (WsdError::Timeout, Op::Send, K::Timeout, P::Reply, S::OutcomeUnknown, None),
            (WsdError::Timeout, Op::Status, K::Timeout, P::Poll, S::Idempotent, None),
            (transport("timeout: global"), Op::Send, K::Timeout, P::Reply, S::OutcomeUnknown, None),
            (transport("timeout: global"), Op::Status, K::Timeout, P::Poll, S::Idempotent, None),
            (transport("io: something odd"), Op::Create, K::Unavailable, P::Reply, S::OutcomeUnknown, None),
            (transport("io: something odd"), Op::Query, K::Unavailable, P::Reply, S::Idempotent, None),
            (transport(""), Op::Send, K::Unavailable, P::Reply, S::OutcomeUnknown, None),
            // A connection that could not be made: nothing was sent, whatever the operation.
            (transport("io: Connection refused (os error 111)"), Op::Send, K::Unavailable, P::Connect, S::Safe, None),
            (
                transport("io: No connection could be made because the target machine actively refused it. (os error 10061)"),
                Op::Create,
                K::Unavailable,
                P::Connect,
                S::Safe,
                None,
            ),
            (transport("io: Network is unreachable (os error 101)"), Op::Query, K::Unavailable, P::Connect, S::Safe, None),
            (transport("timeout: connect"), Op::Send, K::Timeout, P::Connect, S::Safe, None),
            (
                transport("io: A connection attempt failed because the connected party did not properly respond after a period of time"),
                Op::Create,
                K::Timeout,
                P::Connect,
                S::Safe,
                None,
            ),
            (transport("io: Connection refused (os error 111)"), Op::Status, K::Unavailable, P::Poll, S::Safe, None),
            // A refused redirect is not transient; the request may have reached a server that answered.
            (transport("the device answered with a redirect (HTTP 302), which is refused"), Op::Send, K::Failed, P::Reply, S::OutcomeUnknown, None),
            (transport("the device answered with a redirect (HTTP 302), which is refused"), Op::Query, K::Failed, P::Reply, S::Idempotent, None),
            // HTTP.
            (WsdError::Http(408), Op::Send, K::Timeout, P::Request, S::Safe, None),
            (WsdError::Http(425), Op::Create, K::Busy, P::Request, S::Safe, Some(2)),
            (WsdError::Http(429), Op::Send, K::Busy, P::Request, S::Safe, Some(5)),
            (WsdError::Http(429), Op::Status, K::Busy, P::Poll, S::Safe, Some(5)),
            (WsdError::Http(503), Op::Create, K::Unavailable, P::Request, S::Safe, Some(5)),
            (WsdError::Http(500), Op::Send, K::Unavailable, P::Reply, S::OutcomeUnknown, None),
            (WsdError::Http(502), Op::Send, K::Unavailable, P::Reply, S::OutcomeUnknown, None),
            (WsdError::Http(504), Op::Query, K::Unavailable, P::Reply, S::Idempotent, None),
            (WsdError::Http(504), Op::Status, K::Unavailable, P::Poll, S::Idempotent, None),
            (WsdError::Http(501), Op::Send, K::Failed, P::Reply, S::Safe, None),
            (WsdError::Http(505), Op::Query, K::Failed, P::Reply, S::Safe, None),
            (WsdError::Http(400), Op::Send, K::Failed, P::Reply, S::Safe, None),
            (WsdError::Http(404), Op::Query, K::Failed, P::Reply, S::Safe, None),
            (WsdError::Http(301), Op::Send, K::Failed, P::Reply, S::OutcomeUnknown, None),
            // A reply that cannot be used: the request was probably processed.
            (WsdError::Xml("x".into()), Op::Send, K::Failed, P::Reply, S::OutcomeUnknown, None),
            (WsdError::Xml("x".into()), Op::Query, K::Failed, P::Reply, S::Idempotent, None),
            (WsdError::Protocol("x".into()), Op::Create, K::Failed, P::Reply, S::OutcomeUnknown, None),
            (WsdError::TooLarge(REPLY_LABEL, 1), Op::Send, K::Failed, P::Reply, S::OutcomeUnknown, None),
            (WsdError::TooLarge("the job list", 1), Op::Query, K::Failed, P::Reply, S::Idempotent, None),
            // Faults are definite answers: nothing was accepted.
            (fault("ServerErrorNotAcceptingJobs"), Op::Create, K::Busy, P::Reply, S::Safe, Some(15)),
            (fault("ServerErrorTemporaryError"), Op::Send, K::Busy, P::Reply, S::Safe, Some(5)),
            (fault("ServerErrorDocumentActive"), Op::Send, K::Busy, P::Reply, S::Safe, Some(2)),
            (fault("ServerErrorInternalError"), Op::Send, K::Failed, P::Reply, S::Safe, None),
            (fault("ClientErrorJobIdNotFound"), Op::Status, K::NotFound, P::Poll, S::Safe, None),
            (fault("OperationFailed"), Op::Cancel, K::Rejected, P::Reply, S::Safe, None),
            (fault("ClientErrorFormatNotSupported"), Op::Send, K::UnsupportedFormat, P::Reply, S::Safe, None),
            (fault("InvalidArgs"), Op::Create, K::InvalidRequest, P::Reply, S::Safe, None),
            (fault("ServerErrorJobCancelled"), Op::Status, K::Cancelled, P::Poll, S::Safe, None),
            // An earlier send was in fact accepted: the job may print.
            (fault("ClientErrorLastDocumentAlreadySent"), Op::Send, K::Failed, P::Reply, S::OutcomeUnknown, None),
        ];
        let mut kinds_seen: Vec<&str> = Vec::new();
        for (e, op, kind, phase, safety, wait) in &rows {
            let c = e.classify(*op);
            let what = format!("{e} [{op:?}]");
            assert_eq!(c.kind, *kind, "{what}");
            assert_eq!(c.phase, *phase, "{what}");
            assert_eq!(c.safety, *safety, "{what}");
            assert_eq!(c.retry_after.map(|d| d.as_secs()), *wait, "{what}");
            let neutral = e.print_error(*op);
            assert_eq!(
                (neutral.kind(), neutral.phase(), neutral.retry_safety(), neutral.retry_after()),
                (c.kind, Some(c.phase), c.safety, c.retry_after),
                "{what}"
            );
            kinds_seen.push(e.kind());
        }
        kinds_seen.sort_unstable();
        kinds_seen.dedup();
        let mut listed = KINDS.to_vec();
        listed.sort_unstable();
        assert_eq!(kinds_seen, listed, "the retry table has a row for every kind");
    }

    #[test]
    fn the_retry_invariants_hold_for_every_error_and_operation() {
        for e in samples() {
            for op in Op::ALL {
                let c = e.classify(op);
                let what = format!("{e} [{op:?}]");
                assert_eq!(c.kind.is_retryable(), e.is_retryable(), "{what}: retryability does not depend on the operation");
                assert_eq!(c.kind, e.error_kind(), "{what}: neither does the kind");
                if op.is_idempotent() {
                    assert_ne!(c.safety, RetrySafety::OutcomeUnknown, "{what}: a query or a cancel can always be repeated");
                } else {
                    assert_ne!(c.safety, RetrySafety::Idempotent, "{what}: a create or a send is not idempotent");
                }
                if c.retry_after.is_some() {
                    assert!(c.kind.is_retryable(), "{what}: only a transient condition has a wait");
                }
                if c.kind.is_retryable() {
                    assert_ne!(c.phase, Phase::Prepare, "{what}: a transient failure happened on the wire");
                }
                if c.phase == Phase::Poll {
                    assert_eq!(op, Op::Status, "{what}");
                }
                if op == Op::Status && c.phase != Phase::Prepare {
                    assert_eq!(c.phase, Phase::Poll, "{what}: a status poll's wire failures are poll failures");
                }
                // The conversion keeps it all.
                let neutral = e.print_error(op);
                assert_eq!((neutral.is_retryable(), neutral.retry_safety()), (e.is_retryable(), c.safety), "{what}");
            }
        }
        // Refusals and the redirect are not transient.
        for e in [WsdError::Policy("p".into()), WsdError::Transport("the device answered with a redirect (HTTP 302), which is refused".into())] {
            assert!(!e.is_retryable(), "{e}");
        }
        // The conversion without an operation is the careful one.
        assert_eq!(PrintError::from(WsdError::ConnectionClosed).retry_safety(), RetrySafety::OutcomeUnknown);
    }

    #[test]
    fn every_fault_code_maps_and_the_retryable_ones_stay_retryable() {
        for (name, code) in crate::wsd::fault::WPRT_CODES.iter() {
            let e = WsdError::Fault(Fault { role: Role::Sender, code: code.clone(), reason: String::new(), detail: String::new() });
            let neutral = PrintError::from(e.clone());
            let native = neutral.native().expect("a fault carries its code");
            assert_eq!((native.domain, native.name), ("wsd-fault", Some(*name)));
            assert!(native.code >= 1, "{name}");
            assert_eq!(neutral.is_retryable(), code.is_retryable(), "{name}");
            assert_eq!(neutral.retry_after().is_some(), code.is_retryable(), "{name}: a busy fault carries a wait, others do not");
        }
        let action = PrintError::from(WsdError::Fault(Fault {
            role: Role::Sender,
            code: FaultCode::ActionNotSupported,
            reason: String::new(),
            detail: String::new(),
        }));
        assert_eq!((action.kind(), action.native().map(|n| n.name)), (ErrorKind::Rejected, Some(Some("ActionNotSupported"))));
        let vendor = PrintError::from(WsdError::Fault(Fault {
            role: Role::Receiver,
            code: FaultCode::Other(Some("urn:v".into()), "Odd".into()),
            reason: String::new(),
            detail: String::new(),
        }));
        assert_eq!((vendor.kind(), vendor.native().map(|n| (n.code, n.name))), (ErrorKind::Rejected, Some((0, None))));
        // Codes are distinct.
        let mut codes: Vec<i64> = crate::wsd::fault::WPRT_CODES.iter().map(|(_, c)| c.native().0).collect();
        codes.push(FaultCode::ActionNotSupported.native().0);
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), crate::wsd::fault::WPRT_CODES.len() + 1);
    }

    #[test]
    fn http_retryability_follows_the_status() {
        for (s, r) in [
            (500, true),
            (502, true),
            (503, true),
            (504, true),
            (429, true),
            (408, true),
            (425, true),
            (501, false),
            (505, false),
            (404, false),
            (400, false),
            (403, false),
            (301, false),
        ] {
            assert_eq!(WsdError::Http(s).is_retryable(), r, "{s}");
            assert_eq!(PrintError::from(WsdError::Http(s)).is_retryable(), r, "{s}");
        }
        assert!(!WsdError::Protocol(String::new()).is_retryable());
        assert!(WsdError::ConnectionClosed.is_retryable());
    }

    #[test]
    fn transport_texts_are_read_conservatively() {
        use TransportKind as T;
        for (text, kind) in [
            ("the device answered with a redirect (HTTP 307), which is refused", T::Redirect),
            ("io: Connection refused (os error 111)", T::ConnectFailed),
            ("io: The remote host actively refused it", T::ConnectFailed),
            ("io: No route to host (os error 113)", T::ConnectFailed),
            ("timeout: connect", T::ConnectTimeout),
            ("io: Connection timed out (os error 110)", T::ConnectTimeout),
            ("timeout: global", T::Timeout),
            ("io: read timed out", T::Timeout),
            ("io: something odd", T::Other),
            ("", T::Other),
        ] {
            assert_eq!(transport_kind(text), kind, "{text:?}");
        }
    }
}
