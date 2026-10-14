//! WS-Print (Web Services for Devices) as a [`crate::backends::Backend`]. Cargo feature `wsd`, on by
//! default; not compiled for `wasm32`.
//!
//! [`WsdBackend`] prints to network printers that speak the WS-Print service, over `craft-wsd`
//! (SOAP 1.2 / WS-Addressing, MTOM, the numeric-local-address URL policy and the plain-HTTP
//! transport). It registers as `wsd`, has [`crate::BackendClass::Network`] (so the registry only
//! hands it requests that carry a [`crate::UserConfirmed`]) and names its printers `wsd:<url>`.
//!
//! # Nothing happens until the app asks
//!
//! * The backend **lists only the printers the app added** ([`WsdBackend::add`],
//!   [`WsdBackend::with_printers`]). It does no discovery and sends no multicast; finding printers
//!   on the network is a separate decision for the app.
//! * Building a backend, adding a printer and listing printers open no connection. A connection is
//!   made by [`Backend::printer_details`](crate::backends::Backend::printer_details) (one
//!   `GetPrinterElements`) and by a job.
//! * Printer URLs must be `http://` with a numeric loopback, link-local or private address; host
//!   names, public addresses, redirects and proxies are refused.
//!
//! # Layers
//!
//! * [`WsdBackend`]: the registry-facing backend (`backend.rs`).
//! * [`client::Client`]: one method per WS-Print operation over any transport; [`job`] is the flow
//!   that negotiates a format, plans the [`ticket::PrintTicket`], creates the job, sends the
//!   document (MTOM or inline base64) and watches it to an end state. [`encoder`] and [`pwg`] make
//!   the document bytes (pass-through, a chain of alternatives, or PWG raster from a renderer).
//! * `adapter.rs`: the only file that names `craft-wsd`.
//!
//! # Printer firmware and the ticket
//!
//! Some printers' firmware does not answer tickets containing `JobProcessing` or
//! `DocumentProcessing`. [`ticket::TicketShape`] lists smaller tickets for them; the default is the
//! full ticket, and a smaller one is opt-in: [`WsdBackend::with_ticket_shape`]. Settings that a
//! smaller ticket cannot carry (copies, sides, ...) are refused before anything is sent, or dropped
//! with a warning in the receipt when [`WsdBackend::with_drop_ticket_settings`] is set.

pub mod adapter;
pub mod client;
pub mod encoder;
pub mod error;
pub mod fault;
pub mod job;
pub mod model;
pub mod mtom;
pub mod ns;
pub mod ops;
pub mod policy;
pub mod pwg;
pub mod ticket;

mod backend;

#[cfg(test)]
mod testutil;

pub use backend::WsdBackend;
pub use client::Client;
pub use encoder::{ColorMode, DocumentEncoder, PassThrough, RasterEncoder};
pub use error::{KINDS, Result, WsdError};
pub use fault::{Fault, FaultCode};
pub use job::{JobOutcome, JobResult, PollPolicy, PrintOptions, print_document, print_document_with_progress, printer_info};
pub use model::{Capabilities, JobState, PrinterElements};
pub use mtom::Transfer;
pub use policy::{NetworkLimits, PrinterUrl};
pub use ticket::TicketShape;
