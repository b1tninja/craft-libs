//! The WS-Print layer's error type, and its mapping onto [`crate::PrintError`].

use std::fmt;

use super::adapter::WireError;
use super::fault::{Fault, FaultCode};
use crate::{ErrorKind, NativeCode, PrintError};

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

    /// Would trying the same request again later plausibly work? HTTP 5xx and 429, a connection
    /// that failed or that the printer closed, a deadline, and a fault whose code says so. It is
    /// exactly [`ErrorKind::is_retryable`] of [`Self::error_kind`].
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

    /// The neutral kind this error is reported as. `is_retryable()` of the result equals
    /// [`Self::is_retryable`] (see the table test).
    pub fn error_kind(&self) -> ErrorKind {
        match self {
            Self::Policy(_) | Self::Invalid(_) | Self::Unsupported(_) => ErrorKind::InvalidRequest,
            Self::Transport(_) | Self::ConnectionClosed => ErrorKind::Unavailable,
            Self::Http(429) => ErrorKind::Busy,
            Self::Http(s) if (500..600).contains(s) => ErrorKind::Unavailable,
            Self::Http(_) | Self::Xml(_) | Self::Protocol(_) | Self::Encode(_) => ErrorKind::Failed,
            // A cap on something we send is the caller's document being too big; a cap on what came
            // back (the reply, a list, a multipart message) is the printer's doing.
            Self::TooLarge(what, _) if REQUEST_LABELS.contains(what) => ErrorKind::InvalidRequest,
            Self::TooLarge(..) => ErrorKind::Failed,
            Self::Fault(f) => fault_kind(&f.code),
            Self::NoCommonFormat { .. } => ErrorKind::UnsupportedFormat,
            Self::Cancelled => ErrorKind::Cancelled,
            Self::Timeout => ErrorKind::Timeout,
            Self::NotFound(_) => ErrorKind::NotFound,
        }
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

impl From<WsdError> for PrintError {
    fn from(e: WsdError) -> Self {
        let kind = e.error_kind();
        let native = match &e {
            WsdError::Fault(f) => {
                let (code, name) = f.code.native();
                Some(NativeCode { domain: "wsd-fault", code, name })
            }
            _ => None,
        };
        let error = PrintError::new(kind, e.to_string());
        match native {
            Some(n) => error.with_native(n),
            None => error,
        }
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

    #[test]
    fn every_kind_of_ours_maps_to_a_neutral_kind_and_keeps_retryability() {
        let fault = |local: &str| {
            WsdError::Fault(Fault {
                role: Role::Receiver,
                code: FaultCode::from_qname(Some(ns::WPRT), local),
                reason: "r".into(),
                detail: String::new(),
            })
        };
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

    #[test]
    fn every_fault_code_maps_and_the_retryable_ones_stay_retryable() {
        for (name, code) in crate::wsd::fault::WPRT_CODES.iter() {
            let e = WsdError::Fault(Fault { role: Role::Sender, code: code.clone(), reason: String::new(), detail: String::new() });
            let neutral = PrintError::from(e.clone());
            let native = neutral.native().expect("a fault carries its code");
            assert_eq!((native.domain, native.name), ("wsd-fault", Some(*name)));
            assert!(native.code >= 1, "{name}");
            assert_eq!(neutral.is_retryable(), code.is_retryable(), "{name}");
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
        for (s, r) in [(500, true), (502, true), (503, true), (429, true), (404, false), (400, false), (403, false), (301, false)] {
            assert_eq!(WsdError::Http(s).is_retryable(), r, "{s}");
            assert_eq!(PrintError::from(WsdError::Http(s)).is_retryable(), r, "{s}");
        }
        assert!(!WsdError::Protocol(String::new()).is_retryable());
        assert!(WsdError::ConnectionClosed.is_retryable());
    }
}
