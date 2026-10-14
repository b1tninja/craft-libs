//! [`WsdBackend`]: the WS-Print client as a registry backend.

use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};

use super::adapter::{SharedTransport, Transport, default_transport};
use super::client::Client;
use super::encoder::{ColorMode, PassThrough};
use super::job::{self, JobOutcome, JobResult, PollPolicy, PrintOptions};
use super::model::{JobReason, PrinterElements};
use super::mtom::Transfer;
use super::policy::{NetworkLimits, PrinterUrl};
use super::ticket::{Sides, TicketShape};
use crate::backends::{
    Backend, BackendClass, DocumentFormat, JobHint, PrinterDetails, PrinterRef, PrinterState, PrinterStatus, Receipt, SubmitRequest,
};
use crate::{Duplex, ErrorKind, Job, PrintError, Scaling};

/// The registry name and id prefix.
const NAME: &str = "wsd";

/// What the registry may be handed: the formats a WS-Print printer can list and this crate can name.
/// Whether one printer takes one of them is decided against that printer's own list, in
/// [`Backend::submit`] ([`ErrorKind::UnsupportedFormat`]) and shown by [`Backend::printer_details`].
const FORMATS: &[DocumentFormat] = &[DocumentFormat::Pdf, DocumentFormat::PwgRaster, DocumentFormat::PostScript, DocumentFormat::OctetStream];

/// The `Job` settings this backend applies: copies, duplex and colour (as grey). Collation and
/// scaling have no WS-Print setting, and driver keywords are a CUPS notion.
const HONOURED: &[JobHint] = &[JobHint::Copies, JobHint::Duplex, JobHint::Colour];

#[derive(Clone, Debug)]
struct Entry {
    url: PrinterUrl,
    name: Option<String>,
}

#[derive(Clone, Debug)]
struct Config {
    ticket_shape: TicketShape,
    drop_ticket_settings: bool,
    strict: bool,
    transfer: Transfer,
    poll: PollPolicy,
    user_name: String,
    limits: NetworkLimits,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ticket_shape: TicketShape::Full,
            drop_ticket_settings: false,
            strict: false,
            transfer: Transfer::Mtom,
            poll: PollPolicy::default(),
            user_name: "craft".into(),
            limits: NetworkLimits::default(),
        }
    }
}

/// A WS-Print backend. Registers as `wsd`; its printers are the ones added to it and are named
/// `wsd:<url>`. See the [module documentation](super) for what it does and, above all, does not do
/// on its own.
///
/// The backend is cheap to clone and clones share the printer list: register one clone
/// (`registry.register(Box::new(backend.clone()))`) and keep the other to [`add`](Self::add) or
/// [`remove`](Self::remove) printers later. Configuration is set on a backend before it is cloned.
#[derive(Clone)]
pub struct WsdBackend {
    printers: Arc<RwLock<Vec<Entry>>>,
    transport: SharedTransport,
    config: Arc<Config>,
}

impl fmt::Debug for WsdBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsdBackend").field("printers", &self.urls()).field("config", &self.config).finish_non_exhaustive()
    }
}

impl Default for WsdBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WsdBackend {
    /// A backend with no printers and the plain-HTTP transport. Opens no connection.
    pub fn new() -> Self {
        Self { printers: Arc::default(), transport: default_transport(), config: Arc::default() }
    }

    /// A backend with these printer URLs added. A URL the policy refuses is an
    /// [`ErrorKind::InvalidRequest`] naming it, and no backend is made.
    pub fn with_printers<I>(printers: I) -> Result<Self, PrintError>
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        let backend = Self::new();
        for url in printers {
            backend.add(url.as_ref())?;
        }
        Ok(backend)
    }

    /// Add a printer by URL (`http://<numeric local address>[:port]/path`) and return its native
    /// id, the canonical URL (the registry id is `wsd:<that>`). Adding a printer twice keeps one
    /// entry. Opens no connection.
    pub fn add(&self, url: &str) -> Result<String, PrintError> {
        self.add_entry(url, None)
    }

    /// [`Self::add`] with the name a person sees in the printer list (until the printer reports
    /// its own in [`Backend::printer_details`]).
    pub fn add_named(&self, url: &str, name: &str) -> Result<String, PrintError> {
        self.add_entry(url, Some(name.to_owned()))
    }

    fn add_entry(&self, url: &str, name: Option<String>) -> Result<String, PrintError> {
        let url = PrinterUrl::parse(url).map_err(PrintError::from)?;
        let id = url.as_url();
        let mut printers = self.printers.write().unwrap_or_else(PoisonError::into_inner);
        match printers.iter_mut().find(|e| e.url == url) {
            Some(existing) => {
                if name.is_some() {
                    existing.name = name;
                }
            }
            None => printers.push(Entry { url, name }),
        }
        Ok(id)
    }

    /// Remove a printer; `false` when it was not added (or the URL is not one).
    pub fn remove(&self, url: &str) -> bool {
        let Ok(url) = PrinterUrl::parse(url) else { return false };
        let mut printers = self.printers.write().unwrap_or_else(PoisonError::into_inner);
        let before = printers.len();
        printers.retain(|e| e.url != url);
        printers.len() != before
    }

    /// The added printers' native ids (canonical URLs).
    pub fn urls(&self) -> Vec<String> {
        self.snapshot().iter().map(|e| e.url.as_url()).collect()
    }

    fn snapshot(&self) -> Vec<Entry> {
        self.printers.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn configured(mut self, change: impl FnOnce(&mut Config)) -> Self {
        let mut config = (*self.config).clone();
        change(&mut config);
        self.config = Arc::new(config);
        self
    }

    /// Use another transport (an in-memory fake, a wrapper that adds logging). The default is the
    /// plain-HTTP transport of `craft-wsd`. The transport is only ever given validated URLs.
    #[must_use]
    pub fn with_transport(mut self, transport: impl Transport + Send + Sync + 'static) -> Self {
        self.transport = Arc::new(transport);
        self
    }

    /// How much of the ticket to send. Anything but [`TicketShape::Full`] is for printers whose
    /// firmware does not answer tickets with `JobProcessing` or `DocumentProcessing`; the settings
    /// such a ticket cannot carry are refused ([`ErrorKind::InvalidRequest`], before anything is sent)
    /// unless [`Self::with_drop_ticket_settings`] is set. The default is `Full`.
    #[must_use]
    pub fn with_ticket_shape(self, shape: TicketShape) -> Self {
        self.configured(|c| c.ticket_shape = shape)
    }

    /// With a reduced ticket, drop the settings it cannot carry (each one is a warning in the
    /// receipt) instead of refusing the job. Off by default.
    #[must_use]
    pub fn with_drop_ticket_settings(self, drop: bool) -> Self {
        self.configured(|c| c.drop_ticket_settings = drop)
    }

    /// Strict mode: refuse a setting the printer lists as unsupported, and send every setting as
    /// `MustHonor`. Off by default (such settings are left out or lowered, with a warning).
    #[must_use]
    pub fn with_strict(self, strict: bool) -> Self {
        self.configured(|c| c.strict = strict)
    }

    /// How the document travels in `SendDocument`: an MTOM part (default) or inline base64.
    #[must_use]
    pub fn with_transfer(self, transfer: Transfer) -> Self {
        self.configured(|c| c.transfer = transfer)
    }

    /// How long and how often a job is watched. A submit blocks for at most `max_wait` after the
    /// document is sent (the default is five minutes); a job still running then is reported as
    /// accepted and left running on the printer.
    #[must_use]
    pub fn with_poll(self, poll: PollPolicy) -> Self {
        self.configured(|c| c.poll = poll)
    }

    /// The `JobOriginatingUserName` sent with jobs. Defaults to a fixed `craft`, never the account name.
    #[must_use]
    pub fn with_user_name(self, name: &str) -> Self {
        self.configured(|c| c.user_name = name.to_owned())
    }

    /// Timeout and size caps per request.
    #[must_use]
    pub fn with_limits(self, limits: NetworkLimits) -> Self {
        self.configured(|c| c.limits = limits)
    }

    /// The listed printer for a native id: it must be an added printer, not just a valid URL, so a
    /// request can only ever reach what the app chose.
    fn entry(&self, native_id: &str) -> Result<Entry, PrintError> {
        let url = PrinterUrl::parse(native_id).map_err(PrintError::from)?;
        self.snapshot()
            .into_iter()
            .find(|e| e.url == url)
            .ok_or_else(|| PrintError::new(ErrorKind::NotFound, format!("{native_id} is not a WS-Print printer that was added to this backend")))
    }

    fn client(&self, entry: &Entry) -> Client<SharedTransport> {
        Client::new(Arc::clone(&self.transport), entry.url.clone()).with_limits(self.config.limits)
    }

    /// The options of one job. The returned strings are what could not be applied.
    fn options(&self, job: &Job) -> (PrintOptions, Vec<String>) {
        let mut notes = Vec::new();
        let config = &*self.config;
        let sides = job.duplex.map(|d| match d {
            Duplex::Off => Sides::OneSided,
            Duplex::LongEdge => Sides::TwoSidedLongEdge,
            Duplex::ShortEdge => Sides::TwoSidedShortEdge,
        });
        if job.scaling == Scaling::ShrinkToFit {
            notes.push("scaling is not applied: the document is sent as it is".to_owned());
        }
        if !job.options.is_empty() {
            notes.push("driver options are not applied by WS-Print".to_owned());
        }
        let options = PrintOptions {
            job_name: job.title.chars().take(255).collect(),
            user_name: config.user_name.clone(),
            document_name: None,
            // One copy is the printer's default and needs no setting (a reduced ticket has no room for it).
            copies: (job.copies != 1).then_some(job.copies),
            sides,
            color: if job.grayscale { ColorMode::Mono } else { ColorMode::Auto },
            strict: config.strict,
            ticket_shape: config.ticket_shape,
            drop_ticket_settings: config.drop_ticket_settings,
            transfer: config.transfer,
            poll: config.poll,
            ..PrintOptions::default()
        };
        (options, notes)
    }
}

/// A listed format as a [`DocumentFormat`], when this crate has a name for it.
fn document_format(listed: &str) -> Option<DocumentFormat> {
    FORMATS.iter().copied().find(|f| f.mime().eq_ignore_ascii_case(listed.split(';').next().unwrap_or("").trim()))
}

fn printer_state(state: &str) -> Option<PrinterState> {
    match state {
        "Idle" => Some(PrinterState::Idle),
        "Processing" => Some(PrinterState::Processing),
        "Stopped" => Some(PrinterState::Stopped),
        _ => None,
    }
}

fn details_of(entry: &Entry, elements: PrinterElements) -> PrinterDetails {
    let id = entry.url.as_url();
    let description = elements.description.unwrap_or_default();
    let name = [Some(description.name.as_str()), entry.name.as_deref()].into_iter().flatten().find(|n| !n.is_empty()).unwrap_or(&id).to_owned();
    let mut details = PrinterDetails {
        id: id.clone(),
        name,
        default: false,
        colour: description.color_supported,
        honoured: Some(HONOURED.to_vec()),
        ..PrinterDetails::default()
    };
    if let Some(caps) = elements.capabilities {
        for format in caps.formats.iter().filter_map(|f| document_format(f)) {
            if !details.formats.contains(&format) {
                details.formats.push(format);
            }
        }
        details.two_sided = if caps.sides.is_empty() { None } else { Some(caps.sides.iter().any(|s| s.starts_with("TwoSided"))) };
        details.paper = caps.media_sizes;
        details.max_copies = caps.copies.map(|(_, max)| max);
    }
    details.status = elements.status.and_then(|s| printer_state(&s.state).map(|state| PrinterStatus { state, reasons: s.reasons }));
    details
}

impl Backend for WsdBackend {
    fn name(&self) -> &'static str {
        NAME
    }

    /// Every job goes over the network, so the registry asks for a [`crate::UserConfirmed`].
    fn class(&self) -> BackendClass {
        BackendClass::Network
    }

    fn accepted_formats(&self) -> &[DocumentFormat] {
        FORMATS
    }

    /// The printers the app added; no discovery, no connection.
    fn list_printers(&self) -> Result<Vec<PrinterRef>, PrintError> {
        Ok(self
            .snapshot()
            .into_iter()
            .map(|e| {
                let id = e.url.as_url();
                PrinterRef { name: e.name.unwrap_or_else(|| id.clone()), id, default: false, two_sided: None }
            })
            .collect())
    }

    /// One `GetPrinterElements` (description, status, capabilities) to the printer.
    fn printer_details(&self, native_id: &str) -> Result<PrinterDetails, PrintError> {
        let entry = self.entry(native_id)?;
        let elements = job::printer_info(&self.client(&entry)).map_err(PrintError::from)?;
        Ok(details_of(&entry, elements))
    }

    /// Create the job, send the document, and watch it to an end state. Blocks for the whole job
    /// (see [`WsdBackend::with_poll`]). `request.job.printer` must name an added printer; there is
    /// no default. Progress is `(done, 4)` protocol phases (capabilities read, job created,
    /// document sent, job ended), not sheets.
    fn submit(&self, request: SubmitRequest<'_>) -> Result<Receipt, PrintError> {
        let SubmitRequest { document, job, consent, cancel, progress } = request;
        // The registry checks this too; a caller that holds the backend directly gets the same answer.
        let Some(consent) = consent else {
            return Err(PrintError::new(
                ErrorKind::ConsentRequired,
                "the wsd backend prints over a network and needs the person's confirmation (UserConfirmed)",
            ));
        };
        let Some(id) = job.printer.as_deref() else {
            return Err(PrintError::new(ErrorKind::NotFound, "the wsd backend has no default printer: name one (wsd:<url>)"));
        };
        let entry = self.entry(id)?;
        let (options, notes) = self.options(&job);
        let encoder = PassThrough::new(document.format.mime(), document.bytes);
        let mut none = |_: usize, _: usize| {};
        let progress: &mut dyn FnMut(usize, usize) = match progress {
            Some(p) => p,
            None => &mut none,
        };
        let result =
            job::print_document_with_progress(&self.client(&entry), &encoder, &options, consent, &cancel, progress).map_err(PrintError::from)?;
        receipt(&entry, result, notes)
    }
}

/// The receipt of a job that was accepted, or the error of one that ended badly.
fn receipt(entry: &Entry, result: JobResult, notes: Vec<String>) -> Result<Receipt, PrintError> {
    let JobResult { job_id, outcome, reasons, printer_name, mut warnings, .. } = result;
    warnings.extend(notes);
    let who = format!("job {job_id} on {} ({printer_name})", entry.url.as_url());
    let reason_text = || reasons.iter().filter(|r| **r != JobReason::None).map(JobReason::name).collect::<Vec<_>>().join(", ");
    let ok = |what: &str| {
        let mut message = format!("{who} {what}");
        if !warnings.is_empty() {
            message.push_str(&format!(" (warnings: {})", warnings.join("; ")));
        }
        Ok(Receipt { backend: NAME, message, job_id: Some(job_id.to_string()) })
    };
    match outcome {
        JobOutcome::Completed => ok("completed"),
        JobOutcome::CompletedWithWarnings => ok("completed with warnings from the printer"),
        JobOutcome::Forgotten => ok("is no longer listed by the printer, which most likely finished it"),
        JobOutcome::StillRunning => ok("was accepted and is still running on the printer"),
        JobOutcome::CompletedWithErrors => Err(PrintError::new(ErrorKind::Failed, format!("{who} completed with errors ({})", reason_text()))),
        JobOutcome::Canceled { at_device } => {
            let by = if at_device { "at the printer" } else { "by request" };
            Err(PrintError::new(ErrorKind::Cancelled, format!("{who} was cancelled {by}")))
        }
        JobOutcome::Aborted { reason } => {
            let kind = match reason {
                Some(JobReason::DocumentFormatError | JobReason::UnsupportedDocumentFormat | JobReason::UnsupportedCompression) => {
                    ErrorKind::UnsupportedFormat
                }
                Some(JobReason::PrinterStopped) => ErrorKind::Unavailable,
                _ => ErrorKind::Rejected,
            };
            Err(PrintError::new(kind, format!("{who} was aborted by the printer ({})", reason_text())))
        }
    }
}
