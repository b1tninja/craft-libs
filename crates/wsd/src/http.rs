//! The blocking HTTP transport (cargo feature `http`), on `ureq` without TLS.
//!
//! Hardened to the network policy: proxies off (even from the environment), redirects not
//! followed, a global timeout, plain `http` only (a [`DeviceUrl`] cannot be anything else), and
//! the body read through a hard size cap.

use std::io::Read;

use ureq::Agent;

use crate::error::Error;
use crate::policy::DeviceUrl;
use crate::transport::{Request, Response, Transport};

/// The `ureq`-backed transport. Cheap to clone.
#[derive(Clone)]
pub struct HttpTransport {
    agent: Agent,
}

impl Default for HttpTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpTransport {
    pub fn new() -> Self {
        let config = Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .max_redirects_will_error(false)
            .http_status_as_error(false)
            .https_only(false)
            .user_agent("craft-wsd")
            .build();
        Self { agent: Agent::new_with_config(config) }
    }
}

/// A connection the peer closed or reset before any reply is `ConnectionClosed` (retryable);
/// everything else is a plain transport failure with `ureq`'s description.
fn map_error(e: ureq::Error) -> Error {
    use std::io::ErrorKind as K;
    match &e {
        ureq::Error::Io(io) if matches!(io.kind(), K::UnexpectedEof | K::ConnectionReset | K::ConnectionAborted | K::BrokenPipe) => {
            Error::ConnectionClosed
        }
        _ => Error::Transport(e.to_string()),
    }
}

impl Transport for HttpTransport {
    fn post(&self, url: &DeviceUrl, request: Request) -> Result<Response, Error> {
        let mut builder = self
            .agent
            .post(url.as_url())
            .config()
            .timeout_global(Some(request.timeout))
            .build()
            .header("Content-Type", &request.content_type)
            .header("SOAPAction", format!("\"{}\"", request.soap_action));
        // Nothing else from the caller reaches the headers.
        builder = builder.header("Accept", "application/soap+xml, multipart/related");
        let mut response = builder.send(&request.body[..]).map_err(map_error)?;
        let status = response.status().as_u16();
        if (300..400).contains(&status) {
            return Err(Error::Transport(format!("the device answered with a redirect (HTTP {status}), which is refused")));
        }
        let content_type = response.headers().get("content-type").and_then(|v| v.to_str().ok()).map(str::to_owned);
        let mut body = Vec::new();
        let cap = request.max_response as u64;
        response.body_mut().as_reader().take(cap.saturating_add(1)).read_to_end(&mut body).map_err(|e| Error::Transport(e.to_string()))?;
        if body.len() > request.max_response {
            return Err(Error::TooLarge("the device's reply", request.max_response));
        }
        Ok(Response { status, content_type, body })
    }
}
