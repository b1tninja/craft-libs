//! The injectable HTTP layer.
//!
//! The client never opens a socket itself: it hands a [`Request`] to a [`Transport`]. A transport
//! only ever sees a validated [`DeviceUrl`], and must honour the policy numbers in the request:
//! no proxy, no redirects, a timeout, and a response body no larger than `max_response`.

use std::time::Duration;

use crate::error::Error;
use crate::policy::DeviceUrl;

/// One HTTP POST.
#[derive(Clone, Debug)]
pub struct Request {
    pub content_type: String,
    /// The SOAP action URI, to be sent as `SOAPAction`.
    pub soap_action: String,
    pub body: Vec<u8>,
    pub timeout: Duration,
    /// Read at most this many body bytes; more is an error ([`Error::TooLarge`]).
    pub max_response: usize,
}

/// The reply: status, `Content-Type` and the (capped) body.
#[derive(Clone, Debug)]
pub struct Response {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

/// Something that can POST to a device.
pub trait Transport {
    /// Redirects must be refused, proxies must not be used, and the exchange must time out.
    fn post(&self, url: &DeviceUrl, request: Request) -> Result<Response, Error>;
}

impl<T: Transport + ?Sized> Transport for &T {
    fn post(&self, url: &DeviceUrl, request: Request) -> Result<Response, Error> {
        (**self).post(url, request)
    }
}

impl<T: Transport + ?Sized> Transport for Box<T> {
    fn post(&self, url: &DeviceUrl, request: Request) -> Result<Response, Error> {
        (**self).post(url, request)
    }
}

impl<T: Transport + ?Sized> Transport for std::sync::Arc<T> {
    fn post(&self, url: &DeviceUrl, request: Request) -> Result<Response, Error> {
        (**self).post(url, request)
    }
}
