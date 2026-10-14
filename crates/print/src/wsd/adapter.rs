//! The one place this backend names `craft-wsd`.
//!
//! Every other module of [`crate::wsd`] imports the WSD core through here, so a rename or a
//! reshuffle in `craft-wsd` is a change to this file only. Nothing in it opens a socket by itself:
//! the transports are values the backend is given (or builds, see [`default_transport`]).

use std::sync::Arc;

pub use craft_wsd::soap::{Attachment, Call, SoapClient};
pub use craft_wsd::transport::{Request, Response, Transport};
pub use craft_wsd::{DeviceUrl, NetworkLimits, xml};

/// The shared protocol error, as `craft-wsd` reports it.
pub type WireError = craft_wsd::Error;
/// Every value [`WireError::kind`] can return.
pub use craft_wsd::KINDS as WIRE_KINDS;
/// A SOAP fault as the generic layer parses it (the sub-code is classified by [`super::fault`]).
pub type WireFault = craft_wsd::Fault;
pub use craft_wsd::fault::{Role, parse_fault};
pub use craft_wsd::ns::{ANONYMOUS, FAULT_ACTION, SOAP, WSA, WSA_FINAL, XOP};
pub use craft_wsd::{Fault, SubCode};

/// A transport the backend can hold and share between threads.
pub type SharedTransport = Arc<dyn Transport + Send + Sync>;

/// The blocking plain-HTTP transport: numeric local addresses only, no proxy, no redirects, capped
/// replies and a timeout. It is built only when a backend is made, and connects only when a job or a
/// details request asks it to.
pub fn default_transport() -> SharedTransport {
    Arc::new(craft_wsd::http::HttpTransport::new())
}

/// A fresh random UUID (lowercase, no `urn:uuid:` prefix).
pub fn new_uuid() -> String {
    craft_wsd::envelope::new_uuid()
}

/// `xop:Include` for the part with this content id.
pub fn xop_include_ref(cid: &str) -> String {
    craft_wsd::mtom::xop_include_ref(cid)
}

/// Standard base64 with padding.
pub fn base64(bytes: &[u8]) -> String {
    craft_wsd::mtom::base64(bytes)
}

/// Parse and validate a printer URL; `noun` names it in the refusal ("printer").
pub fn parse_device_url(text: &str, noun: &str) -> Result<DeviceUrl, WireError> {
    DeviceUrl::parse_as(text, noun)
}
