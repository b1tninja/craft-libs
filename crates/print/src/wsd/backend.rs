//! [`WsdBackend`]: the WS-Print client as a registry backend.

use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use super::adapter::{SharedTransport, Transport, default_transport};
use super::client::Client;
use super::encoder::{ColorMode, PassThrough};
use super::error::{Op, WsdError};
use super::fault::FaultCode;
use super::job::{self, PrintOptions};
use super::model::{self, JobReason, PrinterElements};
use super::mtom::Transfer;
use super::policy::{NetworkLimits, PrinterUrl};
use super::ticket::{self, Sides, TicketShape};
use crate::backends::{
    Backend, BackendClass, DocumentFormat, JobHint, PrinterDetails, PrinterRef, PrinterState, PrinterStatus, Progress, SubmitRequest,
};
use crate::{ColourMode, Duplex, ErrorKind, Job, JobId, JobOptions, JobStatus, PrintError, Scaling, Submitted, WatchSupport};

/// The registry name and id prefix.
const NAME: &str = "wsd";

/// What the registry may be handed: the formats a WS-Print printer can list and this crate can name.
/// Whether one printer takes one of them is decided against that printer's own list, in
/// [`Backend::submit`] ([`ErrorKind::UnsupportedFormat`]) and shown by [`Backend::printer_details`].
const FORMATS: &[DocumentFormat] = &[DocumentFormat::Pdf, DocumentFormat::PwgRaster, DocumentFormat::PostScript, DocumentFormat::OctetStream];

/// The `Job` and `JobOptions` settings this backend applies: copies, duplex, colour (chosen when the
/// picture is made), and the print ticket's media size, orientation, quality and resolution (each
/// still subject to the printer's own capability list and the ticket shape). Collation and scaling
/// have no WS-Print setting, and driver keywords are a CUPS notion.
const HONOURED: &[JobHint] =
    &[JobHint::Copies, JobHint::Duplex, JobHint::Colour, JobHint::Media, JobHint::Orientation, JobHint::Quality, JobHint::Resolution];

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
    poll_interval: Duration,
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
            poll_interval: Duration::from_secs(2),
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
///
/// # Per-job and per-invocation settings
///
/// What varies per job travels in the request: copies, duplex and grey in the [`Job`], and media
/// size, orientation, quality, resolution and an explicit colour choice in the request's
/// [`JobOptions`]. The backend maps them into that job's ticket (settings the printer's capability
/// list rejects are dropped with a warning, or refused in strict mode). The **ticket shape**, strict
/// mode, the transfer encoding, the user name and the limits are backend-wide configuration. An
/// app that wants different ones for one invocation builds a variant of the backend: the `with_*`
/// methods consume a backend and a clone shares the printer list, so
/// `backend.clone().with_ticket_shape(TicketShape::Description).with_drop_ticket_settings(true)` is
/// a cheap, independent variant that still prints to the same printers. Register it in a
/// [`crate::Registry`] of its own, or call [`Backend::submit`] on it directly (it checks the
/// consent itself).
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

    /// The poll interval this backend recommends for [`Backend::job_status`] (default two seconds).
    /// It is advice for [`crate::JobWatch`] and UIs: the backend itself never polls, and `submit`
    /// does not wait for the job.
    #[must_use]
    pub fn with_poll_interval(self, interval: Duration) -> Self {
        self.configured(|c| c.poll_interval = interval)
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
    fn options(&self, job: &Job, per_job: &JobOptions) -> (PrintOptions, Vec<String>) {
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
        let color = match per_job.colour {
            ColourMode::Auto if job.grayscale => ColorMode::Mono,
            ColourMode::Auto => ColorMode::Auto,
            ColourMode::Colour => ColorMode::Color,
            ColourMode::Mono => ColorMode::Mono,
        };
        let options = PrintOptions {
            job_name: job.title.chars().take(255).collect(),
            user_name: config.user_name.clone(),
            document_name: None,
            // One copy is the printer's default and needs no setting (a reduced ticket has no room for it).
            copies: (job.copies != 1).then_some(job.copies),
            sides,
            orientation: per_job.orientation.map(ticket_orientation),
            media_size: per_job.media.clone().filter(|m| !m.is_empty()),
            quality: per_job.quality.map(ticket_quality),
            resolution: per_job.resolution.map(|r| (r.x_dpi, r.y_dpi)),
            color,
            strict: config.strict,
            ticket_shape: config.ticket_shape,
            drop_ticket_settings: config.drop_ticket_settings,
            transfer: config.transfer,
            ..PrintOptions::default()
        };
        (options, notes)
    }

    /// The job id of job number `job_number` on the printer at `printer_url`: the typed way to name
    /// a job this backend made (or one a person typed in), so nothing concatenates strings. The URL
    /// is canonicalised and must pass the printer URL policy ([`ErrorKind::InvalidRequest`]
    /// otherwise); the printer need not be added to any backend. The inverse is
    /// [`Self::parse_native`] (or [`Self::parse_job_id`]).
    pub fn job_id(printer_url: &str, job_number: u32) -> Result<JobId, PrintError> {
        let url = PrinterUrl::parse(printer_url).map_err(PrintError::from)?;
        Ok(JobId::new(NAME, job_native(&url.as_url(), job_number)))
    }

    /// Split the native part of a `wsd` [`JobId`] (`<printer url>#<job number>`) into its printer
    /// URL (canonical) and job number. A malformed handle is [`ErrorKind::InvalidRequest`], never
    /// a panic.
    pub fn parse_native(native: &str) -> Result<WsdJob, PrintError> {
        let bad = |why: &str| {
            PrintError::new(ErrorKind::InvalidRequest, format!("{native:?} is not a wsd job handle: {why}")).with_phase(crate::Phase::Prepare)
        };
        let (url, number) = native.rsplit_once('#').ok_or_else(|| bad("no job number"))?;
        let number: u32 = number.parse().map_err(|_| bad("the job number is not a number"))?;
        let url = PrinterUrl::parse(url).map_err(|e| bad(&PrintError::from(e).to_string()))?;
        Ok(WsdJob { printer_url: url.as_url(), job_number: number })
    }

    /// [`Self::parse_native`] for a whole [`JobId`]; one of another backend is
    /// [`ErrorKind::InvalidRequest`].
    pub fn parse_job_id(job: &JobId) -> Result<WsdJob, PrintError> {
        if job.backend() != NAME {
            return Err(
                PrintError::new(ErrorKind::InvalidRequest, format!("{job} is not a job of the wsd backend")).with_phase(crate::Phase::Prepare)
            );
        }
        Self::parse_native(job.native())
    }
}

/// The printer and job number a `wsd` job id names; see [`WsdBackend::parse_native`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WsdJob {
    /// The canonical printer URL, the native id of the printer.
    pub printer_url: String,
    pub job_number: u32,
}

fn ticket_orientation(o: crate::Orientation) -> ticket::Orientation {
    match o {
        crate::Orientation::Landscape => ticket::Orientation::Landscape,
        crate::Orientation::ReversePortrait => ticket::Orientation::ReversePortrait,
        crate::Orientation::ReverseLandscape => ticket::Orientation::ReverseLandscape,
        _ => ticket::Orientation::Portrait,
    }
}

fn ticket_quality(q: crate::Quality) -> ticket::Quality {
    match q {
        crate::Quality::Draft => ticket::Quality::Draft,
        crate::Quality::High => ticket::Quality::High,
        _ => ticket::Quality::Normal,
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
        let elements = job::printer_info(&self.client(&entry)).map_err(|e| e.print_error(Op::Query))?;
        Ok(details_of(&entry, elements))
    }

    /// Create the job and send the document; returns as soon as the printer has **accepted** it
    /// (the reply to `SendDocument`), never waiting for the job to print. The call makes four bounded
    /// requests at most (capabilities, create, send, and a cleanup cancel when the send is refused),
    /// each limited by the configured request timeout. `request.job.printer` must name an added
    /// printer; there is no default. Progress events are the three protocol phases of the submission
    /// (capabilities read, job created, document accepted: `Progress::of(done, 3)`), not sheets;
    /// the sheets the printer has finished come from [`Backend::job_status`].
    ///
    /// Follow the job with [`Backend::job_status`] (through the registry, or a
    /// [`crate::JobWatch`]). Failures say whether sending again is safe: a refusal is
    /// [`crate::RetrySafety::Safe`]; a connection that closed or timed out **after** the request was
    /// written is [`crate::RetrySafety::OutcomeUnknown`] (the job may exist) and the backend then
    /// leaves any created job alone; call [`WsdBackend::reconcile`] before submitting again.
    fn submit(&self, request: SubmitRequest<'_>) -> Result<Submitted, PrintError> {
        let SubmitRequest { document, job, options: per_job, consent, cancel, progress } = request;
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
        let (options, notes) = self.options(&job, &per_job);
        let encoder = PassThrough::new(document.format.mime(), document.bytes);
        let mut progress = progress;
        let mut report = |done: usize, total: usize| {
            if let Some(callback) = progress.as_deref_mut() {
                let clamp = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
                callback(Progress::of(clamp(done), clamp(total)));
            }
        };
        let mut op = Op::Query;
        let accepted =
            job::submit_document(&self.client(&entry), &encoder, &options, consent, &cancel, &mut report, &mut op).map_err(|e| e.print_error(op))?;
        let mut warnings = accepted.warnings;
        warnings.extend(notes);
        let url = entry.url.as_url();
        let mut message = format!("job {} on {url} ({}) was accepted", accepted.job_id, accepted.printer_name);
        if !warnings.is_empty() {
            message.push_str(&format!(" (warnings: {})", warnings.join("; ")));
        }
        Ok(Submitted::new(NAME, message).with_job(JobId::new(NAME, job_native(&url, accepted.job_id))).with_warnings(warnings))
    }

    /// WS-Print has no event listener here: callers poll.
    fn watch_support(&self) -> WatchSupport {
        WatchSupport::Poll
    }

    fn recommended_poll_interval(&self) -> Option<Duration> {
        Some(self.config.poll_interval)
    }

    /// One `GetJobElements`. A job the printer does not list is [`ErrorKind::NotFound`] and the
    /// message says what that usually means: printers forget finished jobs, so a job that was
    /// accepted and is now unknown most likely finished.
    fn job_status(&self, job: &JobId) -> Result<JobStatus, PrintError> {
        let (entry, id) = self.job_target(job)?;
        match self.client(&entry).job_status(id) {
            Ok(status) => Ok(neutral_status(&status)),
            Err(WsdError::Fault(f)) if f.code == FaultCode::ClientErrorJobIdNotFound => Err(forgotten(&entry, id, &WsdError::Fault(f))),
            Err(e) => {
                let mut error = e.print_error(Op::Status);
                if error.kind() == ErrorKind::NotFound {
                    // Only the printer's own "no such job" means forgotten; anything else is a failure.
                    error = PrintError::new(ErrorKind::Failed, error.message().to_owned())
                        .with_phase(crate::Phase::Poll)
                        .with_safety(crate::RetrySafety::Idempotent);
                }
                Err(error)
            }
        }
    }

    /// `CancelJob`, treating "too late" as success (see [`job::cancel_job_tolerant`]).
    fn cancel_job(&self, job: &JobId) -> Result<(), PrintError> {
        let (entry, id) = self.job_target(job)?;
        job::cancel_job_tolerant(&self.client(&entry), id).map_err(|e| e.print_error(Op::Cancel))
    }
}

/// The backend's job handle: `<printer url>#<job id>`. A printer URL never contains `#` (the policy
/// refuses fragments), so the last `#` separates.
fn job_native(url: &str, job_id: u32) -> String {
    format!("{url}#{job_id}")
}

/// A job a printer no longer lists. Printers drop finished jobs quickly, so "unknown" after an
/// accepted job means finished, cancelled or lost; the neutral kind is [`ErrorKind::NotFound`] and a
/// [`crate::JobWatch`] reports it as forgotten.
fn forgotten(entry: &Entry, job_id: u32, cause: &WsdError) -> PrintError {
    let phase = cause.print_error(Op::Status).phase().unwrap_or(crate::Phase::Poll);
    PrintError::new(
        ErrorKind::NotFound,
        format!(
            "the printer at {} does not list job {job_id}: printers forget finished jobs, so it most likely finished (or was cancelled)",
            entry.url.as_url()
        ),
    )
    .with_phase(phase)
    .with_safety(crate::RetrySafety::Safe)
}

/// The neutral status for a WS-Print job status.
fn neutral_status(status: &model::JobStatus) -> JobStatus {
    let has = |r: JobReason| status.reasons.contains(&r);
    let state = match &status.state {
        model::JobState::Pending => crate::JobState::Pending,
        model::JobState::PendingHeld => crate::JobState::Held,
        model::JobState::Processing | model::JobState::Started | model::JobState::Terminating => crate::JobState::Processing,
        model::JobState::ProcessingStopped => crate::JobState::Stopped,
        model::JobState::Completed if has(JobReason::JobCompletedWithErrors) => crate::JobState::CompletedWithErrors,
        model::JobState::Completed if has(JobReason::JobCompletedWithWarnings) => crate::JobState::CompletedWithWarnings,
        model::JobState::Completed => crate::JobState::Completed,
        model::JobState::Canceled => crate::JobState::Cancelled,
        model::JobState::Aborted => crate::JobState::Aborted,
        model::JobState::Other(_) => crate::JobState::Unknown,
    };
    let mut out = JobStatus::new(state);
    out.reasons = status.reasons.iter().filter(|r| **r != JobReason::None).map(|r| r.name().to_owned()).collect();
    out.sheets_done = Some(u32::try_from(status.sheets_completed).unwrap_or(u32::MAX));
    out
}

/// The result of [`WsdBackend::reconcile`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reconciled {
    /// The printer lists no job with this name from this backend's user: as far as it can tell, the
    /// document did not arrive, and submitting again will not duplicate it.
    NotFound,
    /// Exactly one job matches: the earlier attempt did create it.
    Found(JobMatch),
    /// Several match (the same title was printed before and the printer still lists it). Newest first
    /// (highest job id); look at the states to choose.
    Ambiguous(Vec<JobMatch>),
}

/// A job found by [`WsdBackend::reconcile`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobMatch {
    /// The registry's id for the job (`wsd:<printer url>#<id>`), usable with `job_status` and
    /// `cancel_job` of a [`crate::Registry`] that holds this backend.
    pub job: JobId,
    pub name: String,
    pub status: JobStatus,
}

impl WsdBackend {
    /// The printer and job number of a job handle made by this backend. The printer must still be
    /// added: a request only ever reaches what the app chose. A handle that is not one of ours, or
    /// whose printer was removed, is [`ErrorKind::InvalidRequest`] (not `NotFound`: that means a
    /// forgotten job).
    fn job_target(&self, job: &JobId) -> Result<(Entry, u32), PrintError> {
        let bad = |why: &str| {
            PrintError::new(ErrorKind::InvalidRequest, format!("{job} is not a job of the wsd backend: {why}")).with_phase(crate::Phase::Prepare)
        };
        if job.backend() != NAME {
            return Err(bad("it belongs to another backend"));
        }
        let WsdJob { printer_url: url, job_number: number } = Self::parse_native(job.native()).map_err(|e| bad(e.message()))?;
        let url = url.as_str();
        let entry = self.entry(url).map_err(|e| if e.kind() == ErrorKind::NotFound { bad("its printer is not added to this backend") } else { e })?;
        Ok((entry, number))
    }

    /// After an error that says the outcome is unknown ([`PrintError::outcome_unknown`], for
    /// example a connection that closed with no reply to `SendDocument`), find out whether the
    /// printer made the job **before** sending again. It reads the printer's active jobs and its
    /// history (`GetActiveJobs`, `GetJobHistory`: idempotent reads) and looks for jobs named
    /// `job_title` (the title of the [`crate::Job`], cut to the 255 characters this backend sends)
    /// from this backend's user name.
    ///
    /// WS-Print job summaries carry no creation time, so the window is whatever the printer still
    /// lists; a title that was printed earlier may match too ([`Reconciled::Ambiguous`]). A job that
    /// was created but never received its document shows as pending: cancel it with the registry's
    /// `cancel_job`. Nothing is sent that changes a job.
    pub fn reconcile(&self, printer: &str, job_title: &str) -> Result<Reconciled, PrintError> {
        let entry = self.entry(printer)?;
        let client = self.client(&entry);
        let name: String = job_title.chars().take(255).collect();
        let user = self.config.user_name.as_str();
        let active = client.get_active_jobs().map_err(|e| e.print_error(Op::Query))?;
        // History is best effort when the printer refuses it (a fault), not when the wire fails.
        let history = match client.get_job_history() {
            Ok(rows) => rows,
            Err(WsdError::Fault(_)) => Vec::new(),
            Err(e) => return Err(e.print_error(Op::Query)),
        };
        let url = entry.url.as_url();
        let mut found: Vec<(u32, JobMatch)> = Vec::new();
        for row in active.into_iter().chain(history) {
            if row.job_name != name || row.user_name != user || found.iter().any(|(id, _)| *id == row.status.job_id) {
                continue;
            }
            let job = JobId::new(NAME, job_native(&url, row.status.job_id));
            found.push((row.status.job_id, JobMatch { job, name: row.job_name.clone(), status: neutral_status(&row.status) }));
        }
        found.sort_by_key(|(id, _)| std::cmp::Reverse(*id));
        let mut matches: Vec<JobMatch> = found.into_iter().map(|(_, m)| m).collect();
        Ok(match matches.len() {
            0 => Reconciled::NotFound,
            1 => matches.pop().map_or(Reconciled::NotFound, Reconciled::Found),
            _ => Reconciled::Ambiguous(matches),
        })
    }
}
