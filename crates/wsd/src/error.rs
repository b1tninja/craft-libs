//! The protocol-level error type shared by WSD clients.
//!
//! Service-specific crates (print, scan) wrap or convert this into their own error type, adding the
//! failures that only make sense for their service.

use crate::fault::Fault;

/// Everything that can go wrong below the service layer.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The network policy refused (see `policy`): the URL, or a missing confirmation.
    #[error("refused by policy: {0}")]
    Policy(String),
    /// The transport failed before a usable HTTP reply arrived (connect, timeout, redirect refused, ...).
    #[error("transport: {0}")]
    Transport(String),
    /// The device closed the connection (or reset it) without sending a reply. Retryable: it is
    /// what an embedded server does with a request it cannot handle right now.
    #[error("the device closed the connection without replying")]
    ConnectionClosed,
    /// An HTTP status that is not success, with no SOAP fault in the body.
    #[error("the device answered HTTP {0}")]
    Http(u16),
    /// A size cap was hit (what, the limit in bytes or items).
    #[error("{0} is larger than the limit of {1}")]
    TooLarge(&'static str, usize),
    /// The reply is not well-formed XML (or uses something refused, such as a DTD).
    #[error("malformed XML: {0}")]
    Xml(String),
    /// Well-formed, but not the message that was expected.
    #[error("unexpected reply: {0}")]
    Protocol(String),
    /// The device answered with a SOAP fault.
    #[error("{0}")]
    Fault(Fault),
    /// An argument or option is invalid (checked before anything is sent).
    #[error("invalid {0}")]
    Invalid(String),
    /// The caller's [`crate::CancelToken`] was set; nothing further was sent.
    #[error("cancelled")]
    Cancelled,
    /// The caller's overall deadline passed before the exchange could start or finish.
    #[error("the deadline passed")]
    Timeout,
    /// The device does not offer what was asked for (a hosted service of some type, a usable address).
    #[error("not found: {0}")]
    NotFound(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Every value [`Error::kind`] can return.
pub const KINDS: &[&str] =
    &["policy", "transport", "connection_closed", "http", "too_large", "xml", "protocol", "fault", "invalid", "cancelled", "timeout", "not_found"];

impl Error {
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
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::NotFound(_) => "not_found",
        }
    }

    /// Would trying the same request again later plausibly work? HTTP 5xx and 429. Whether a fault
    /// sub-code is retryable is a property of the service, so it is decided by the service crate.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Http(s) => *s == 429 || (500..600).contains(s),
            Self::ConnectionClosed => true,
            _ => false,
        }
    }

    /// The fault, when the device sent one.
    pub fn fault(&self) -> Option<&Fault> {
        match self {
            Self::Fault(f) => Some(f),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_are_listed_and_retryability_follows_the_status() {
        let all = [
            Error::Policy(String::new()),
            Error::Transport(String::new()),
            Error::Http(500),
            Error::ConnectionClosed,
            Error::TooLarge("x", 1),
            Error::Xml(String::new()),
            Error::Protocol(String::new()),
            Error::Invalid(String::new()),
            Error::Cancelled,
            Error::Timeout,
            Error::NotFound(String::new()),
        ];
        for e in &all {
            assert!(KINDS.contains(&e.kind()), "{e}");
        }
        assert_eq!(all.len() + 1, KINDS.len(), "the Fault kind is the one not built above");
        for (s, r) in [(500, true), (502, true), (503, true), (429, true), (404, false), (400, false), (403, false), (301, false)] {
            assert_eq!(Error::Http(s).is_retryable(), r, "{s}");
        }
        assert!(!Error::Protocol(String::new()).is_retryable());
        assert!(Error::ConnectionClosed.is_retryable());
        assert!(!Error::Cancelled.is_retryable() && !Error::Timeout.is_retryable());
    }
}
