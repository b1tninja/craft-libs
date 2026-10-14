//! The registry of print backends. See [`crate::backend`] for how to write one.
//!
//! Printer ids are namespaced `"<backend>:<native id>"`. Listing asks every backend and keeps what
//! answers: a backend that fails does not hide the others' printers, it appears in
//! [`Listing::errors`]. Details and jobs route by the id's prefix. Ids are exact: an id without a
//! registered prefix is [`ErrorKind::UnknownBackend`], never a guess.
//!
//! A job is a printer-side object with an id of the same shape (`"<backend>:<native id>"`, see
//! [`JobId`]). [`Registry::submit_job`] returns once the printer has accepted the document;
//! [`Registry::job_status`] and [`Registry::cancel_job`] are single bounded requests routed by the
//! job id's backend; [`Registry::job_watch`] builds a caller-driven [`JobWatch`] for a job.

use std::sync::{Arc, OnceLock, PoisonError, RwLock};
use std::time::Duration;

use crate::backend::{Backend, BackendClass, DocumentFormat, ErrorKind, PrintError, PrinterDetails, PrinterRef, Progress, Receipt, SubmitRequest};
use crate::convert::{Conversion, ConversionPolicy, ConvertContext, DocumentConverter, OwnedDocument, choose_resolution, pick_target, wants_colour};
use crate::job::{ExpectedTotals, JobId, JobStatus, Submitted, WatchSupport};
use crate::retry::{Phase, RetrySafety};
use crate::watch::{JobWatch, WaitPolicy};

/// Why [`Registry::register`] refused a backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegisterError {
    /// A backend with this name is already registered; it stays in place.
    DuplicateName { name: &'static str },
    /// The name is empty or has a character other than lowercase ASCII, digits, `-` and `_`.
    InvalidName { name: &'static str },
}

impl std::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateName { name } => write!(f, "a print backend named {name:?} is already registered"),
            Self::InvalidName { name } => write!(f, "{name:?} is not a valid print backend name"),
        }
    }
}

impl std::error::Error for RegisterError {}

/// One backend's failure inside a [`Listing`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendError {
    pub backend: &'static str,
    pub error: PrintError,
}

/// What the registered backends found: every printer that was found (namespaced ids) and one
/// [`BackendError`] per backend that failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listing<T> {
    pub items: Vec<T>,
    pub errors: Vec<BackendError>,
}

impl<T> Listing<T> {
    /// No backend failed.
    pub fn is_complete(&self) -> bool {
        self.errors.is_empty()
    }
}

/// `"<backend>:<native id>"`.
pub fn namespaced_id(backend: &str, native_id: &str) -> String {
    format!("{backend}:{native_id}")
}

/// Split a namespaced id at its first colon.
pub fn split_id(id: &str) -> Option<(&str, &str)> {
    id.split_once(':')
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// The print backends of a process. The free functions of this crate's [`registry`] use one global
/// instance; tests and apps that want isolation make their own with [`Registry::new`].
#[derive(Default)]
pub struct Registry {
    backends: RwLock<Vec<Arc<dyn Backend>>>,
    conversion: RwLock<ConversionSetup>,
}

/// The app's converter and the policy around it; see [`crate::convert`].
#[derive(Default)]
struct ConversionSetup {
    converter: Option<Arc<dyn DocumentConverter>>,
    policy: ConversionPolicy,
}

impl Registry {
    /// An empty registry: every call answers [`ErrorKind::Unsupported`] until a backend is added.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the app's [`DocumentConverter`], replacing any earlier one. Without one, a document
    /// the printer does not accept is refused ([`ErrorKind::UnsupportedFormat`]); with one, the
    /// registry converts it when a format in common exists. See [`crate::convert`] for the rules.
    pub fn set_converter(&self, converter: Arc<dyn DocumentConverter>) {
        self.conversion.write().unwrap_or_else(PoisonError::into_inner).converter = Some(converter);
    }

    /// Remove the converter: back to refusing formats the printer does not accept.
    pub fn clear_converter(&self) {
        self.conversion.write().unwrap_or_else(PoisonError::into_inner).converter = None;
    }

    /// Whether a converter is registered.
    pub fn has_converter(&self) -> bool {
        self.conversion.read().unwrap_or_else(PoisonError::into_inner).converter.is_some()
    }

    /// Replace the format preference and target resolution used when converting
    /// ([`ConversionPolicy`]).
    pub fn set_conversion_policy(&self, policy: ConversionPolicy) {
        self.conversion.write().unwrap_or_else(PoisonError::into_inner).policy = policy;
    }

    fn conversion_setup(&self) -> (Option<Arc<dyn DocumentConverter>>, ConversionPolicy) {
        let setup = self.conversion.read().unwrap_or_else(PoisonError::into_inner);
        (setup.converter.clone(), setup.policy.clone())
    }

    fn snapshot(&self) -> Vec<Arc<dyn Backend>> {
        // Clone the handles out so no lock is held while a backend blocks.
        self.backends.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Add a backend. Several can be registered; a name registered twice is
    /// [`RegisterError::DuplicateName`] (the new backend is dropped), never a panic.
    pub fn register(&self, backend: Box<dyn Backend>) -> Result<(), RegisterError> {
        let name = backend.name();
        if !valid_name(name) {
            return Err(RegisterError::InvalidName { name });
        }
        let mut backends = self.backends.write().unwrap_or_else(PoisonError::into_inner);
        if backends.iter().any(|existing| existing.name() == name) {
            return Err(RegisterError::DuplicateName { name });
        }
        backends.push(Arc::from(backend));
        Ok(())
    }

    /// The registered names, in registration order.
    pub fn backend_names(&self) -> Vec<&'static str> {
        self.snapshot().iter().map(|backend| backend.name()).collect()
    }

    /// Whether any backend is registered.
    pub fn supported(&self) -> bool {
        !self.snapshot().is_empty()
    }

    fn names_text(backends: &[Arc<dyn Backend>]) -> String {
        backends.iter().map(|backend| backend.name()).collect::<Vec<_>>().join(", ")
    }

    /// The printers every backend lists, ids `"<backend>:<native id>"`. `Err(Unsupported)` only when
    /// no backend is registered; see [`Listing`] for partial failure.
    pub fn list_printers(&self) -> Result<Listing<PrinterRef>, PrintError> {
        let backends = self.snapshot();
        if backends.is_empty() {
            return Err(unsupported());
        }
        let mut listing = Listing { items: Vec::new(), errors: Vec::new() };
        for backend in &backends {
            match backend.list_printers() {
                Ok(found) => listing.items.extend(found.into_iter().map(|mut p| {
                    p.id = namespaced_id(backend.name(), &p.id);
                    p
                })),
                Err(error) => listing.errors.push(BackendError { backend: backend.name(), error }),
            }
        }
        Ok(listing)
    }

    fn route<'a, 'i>(backends: &'a [Arc<dyn Backend>], id: &'i str) -> Result<(&'a Arc<dyn Backend>, &'i str), PrintError> {
        Self::route_noun(backends, id, "printer")
    }

    fn route_noun<'a, 'i>(backends: &'a [Arc<dyn Backend>], id: &'i str, noun: &str) -> Result<(&'a Arc<dyn Backend>, &'i str), PrintError> {
        if backends.is_empty() {
            return Err(unsupported());
        }
        let Some((prefix, native)) = split_id(id) else {
            return Err(PrintError::new(
                ErrorKind::UnknownBackend,
                format!("{noun} id {id:?} has no backend prefix; ids look like <backend>:<id> (registered: {})", Self::names_text(backends)),
            ));
        };
        match backends.iter().find(|backend| backend.name() == prefix) {
            Some(backend) => Ok((backend, native)),
            None => Err(PrintError::new(
                ErrorKind::UnknownBackend,
                format!("no print backend named {prefix:?} (registered: {})", Self::names_text(backends)),
            )),
        }
    }

    /// One printer's capabilities by namespaced id. The returned id is namespaced.
    pub fn printer_details(&self, id: &str) -> Result<PrinterDetails, PrintError> {
        let backends = self.snapshot();
        let (backend, native) = Self::route(&backends, id)?;
        let mut details = backend.printer_details(native)?;
        details.id = namespaced_id(backend.name(), &details.id);
        Ok(details)
    }

    /// Hand a document to a printer; returns once the printer has **accepted** it, which is a
    /// bounded call (the backend's per-request timeouts, never the length of the print). Follow the
    /// job with [`Self::job_status`], [`Self::job_watch`] or [`crate::wait_for_job`] when
    /// [`Submitted::watch`] says the backend can report. `request.job.printer` picks the backend:
    ///
    /// * `"<backend>:<native id>"` with a registered prefix goes to that backend, prefix removed;
    /// * `None` (the default printer) goes to the only backend, or, with several, to the first one
    ///   that lists a default printer;
    /// * anything else is [`ErrorKind::UnknownBackend`].
    ///
    /// Before the backend is called the registry checks, in this order, that the token is not
    /// cancelled ([`ErrorKind::Cancelled`]), that the printer accepts the document's format or can
    /// be given a converted one ([`ErrorKind::UnsupportedFormat`], see below) and that a
    /// [`BackendClass::Network`] backend has a [`crate::UserConfirmed`]
    /// ([`ErrorKind::ConsentRequired`]).
    ///
    /// # Formats and conversion
    ///
    /// A document in a format the printer accepts is sent as it is. Otherwise, when the app has
    /// registered a [`DocumentConverter`] ([`Self::set_converter`]), the registry picks the best
    /// format the converter makes and the printer takes, chooses the resolution (the caller's
    /// [`crate::JobOptions::resolution`], else the printer's listed one nearest the policy's target
    /// dpi), runs the converter and sends its result; [`Submitted::conversion`] says so. With no
    /// converter or no format in common the call fails with [`ErrorKind::UnsupportedFormat`],
    /// whose message lists the document's format and the accepted formats. See [`crate::convert`].
    ///
    /// For a backend whose accepted formats depend on the printer
    /// ([`Backend::formats_per_printer`]) and with a converter registered, deciding costs one
    /// [`Backend::printer_details`] call (for WS-Print one read-only request); pass details you
    /// already have to [`Self::submit_job_with_details`] to avoid it.
    pub fn submit_job(&self, request: SubmitRequest<'_>) -> Result<Submitted, PrintError> {
        self.submit_job_with_details(request, None)
    }

    /// [`Self::submit_job`] with the printer's details when the caller already has them (a UI that
    /// showed the printer's capabilities has just read them). The registry then does not ask the
    /// backend again to choose a format and resolution. The details must be those of the printer
    /// the request names (their `id` is not checked); stale details can only make the choice of
    /// format worse, never bypass the backend's own checks.
    pub fn submit_job_with_details(&self, request: SubmitRequest<'_>, known: Option<PrinterDetails>) -> Result<Submitted, PrintError> {
        let backends = self.snapshot();
        if backends.is_empty() {
            return Err(unsupported());
        }
        let mut request = request;
        let (backend, native): (&Arc<dyn Backend>, Option<String>) = match request.job.printer.clone() {
            Some(id) => {
                let (backend, native) = Self::route(&backends, &id)?;
                (backend, Some(native.to_string()))
            }
            None => match backends.as_slice() {
                [only] => (only, None),
                many => {
                    let with_default = many.iter().find(|b| b.list_printers().is_ok_and(|list| list.iter().any(|p| p.default)));
                    match with_default {
                        Some(backend) => (backend, None),
                        None => {
                            return Err(PrintError::new(
                                ErrorKind::NotFound,
                                format!("no default printer and no printer chosen (registered: {})", Self::names_text(many)),
                            ));
                        }
                    }
                }
            },
        };
        if request.cancel.is_cancelled() {
            return Err(PrintError::new(ErrorKind::Cancelled, "the print job was cancelled before it started"));
        }
        let converted = self.prepare_document(backend.as_ref(), native.as_deref(), &mut request, known)?;
        require_consent(backend.as_ref(), &request)?;
        request.job.printer = native;
        let expected = ExpectedTotals::for_request(&request.job, &request.options);
        let SubmitRequest { document, job, options, consent, cancel, progress } = request;
        // The converted bytes (if any) are what is sent.
        let document = match &converted {
            Some((owned, _)) => owned.as_document(),
            None => document,
        };
        // A backend that reports sheets but no total gets the caller's page count as the total.
        let has_callback = progress.is_some();
        let mut caller = progress;
        let mut with_total = |event: Progress| {
            if let Some(report) = caller.as_deref_mut() {
                report(event.or_expected(expected.sheets));
            }
        };
        let forwarded = SubmitRequest { document, job, options, consent, cancel, progress: if has_callback { Some(&mut with_total) } else { None } };
        let mut submitted = backend.submit(forwarded)?;
        let name = backend.name();
        submitted.backend = name;
        submitted.job = submitted.job.map(|job| JobId::new(name, job.native()));
        submitted.watch = backend.watch_support();
        submitted.poll_interval = backend.recommended_poll_interval();
        submitted.expected = expected;
        submitted.conversion = converted.map(|(_, conversion)| conversion);
        if let Some(status) = submitted.status.as_mut() {
            expected.apply(status);
        }
        Ok(submitted)
    }

    /// Decide whether the document goes as it is or through the converter, and convert it. Returns
    /// the converted document and what was done to it; `None` when the original is sent. May set
    /// the request's resolution (PWG raster output only).
    fn prepare_document(
        &self,
        backend: &dyn Backend,
        native: Option<&str>,
        request: &mut SubmitRequest<'_>,
        known: Option<PrinterDetails>,
    ) -> Result<Option<(OwnedDocument, Conversion)>, PrintError> {
        let format = request.document.format;
        let (converter, policy) = self.conversion_setup();
        let per_printer = backend.formats_per_printer();
        let mut details = known;
        // The printer's own list matters only to a backend that has one per printer, and only when
        // something could be done about a mismatch.
        if details.is_none()
            && per_printer
            && converter.is_some()
            && let Some(native) = native
        {
            // Asking a network printer is contact: it needs the person's yes like a job does.
            require_consent(backend, request)?;
            details = Some(backend.printer_details(native)?);
        }
        let accepted: Vec<DocumentFormat> = match details.as_ref().filter(|d| per_printer && !d.formats.is_empty()) {
            Some(d) => d.formats.iter().copied().filter(|f| backend.accepted_formats().contains(f)).collect(),
            None => backend.accepted_formats().to_vec(),
        };
        if accepted.contains(&format) {
            return Ok(None);
        }
        let name = backend.name();
        let accepted_text = accepted.iter().map(|f| f.mime()).collect::<Vec<_>>().join(", ");
        let Some(converter) = converter else {
            return Err(PrintError::new(
                ErrorKind::UnsupportedFormat,
                format!("the {name} backend does not accept {} documents (it accepts: {accepted_text})", format.mime()),
            ));
        };
        let offered = converter.targets(format);
        let Some(target) = pick_target(&offered, &policy.preference, &accepted) else {
            let offered_text = if offered.is_empty() {
                "nothing from this format".to_owned()
            } else {
                offered.iter().map(|f| f.mime()).collect::<Vec<_>>().join(", ")
            };
            return Err(PrintError::new(
                ErrorKind::UnsupportedFormat,
                format!(
                    "no way to print a {} document on the {name} backend: it accepts: {accepted_text}; the converter can make: {offered_text}",
                    format.mime()
                ),
            ));
        };
        require_consent(backend, request)?;
        // The resolutions and the colour report come from the details; a backend whose list is exact
        // was not asked yet, and not being able to say is only a default.
        if details.is_none()
            && let Some(native) = native
        {
            details = backend.printer_details(native).ok();
        }
        let resolution = request
            .options
            .resolution
            .unwrap_or_else(|| choose_resolution(details.as_ref().map_or(&[][..], |d| d.resolutions.as_slice()), policy.target_dpi));
        let colour = wants_colour(request.options.colour, request.job.grayscale, details.as_ref().and_then(|d| d.colour));
        let mut context = ConvertContext::new(resolution, colour);
        context.printer = request.job.printer.clone();
        context.media = request.options.media.clone();
        context.orientation = request.options.orientation;
        context.duplex = request.job.duplex;
        context.expected_pages = request.options.expected_pages;
        context.cancel = request.cancel.clone();
        let before_sending = |error: PrintError| {
            let error = if error.phase().is_none() { error.with_phase(Phase::Prepare) } else { error };
            // Nothing has reached a printer, whatever the converter says about its own failure.
            error.with_safety(RetrySafety::Safe)
        };
        let owned = converter.convert(request.document, target, &context).map_err(before_sending)?;
        if owned.format != target {
            return Err(before_sending(PrintError::new(
                ErrorKind::Failed,
                format!("the converter returned {} where {} was asked for", owned.format.mime(), target.mime()),
            )));
        }
        if request.cancel.is_cancelled() {
            return Err(PrintError::new(ErrorKind::Cancelled, "the print job was cancelled while the document was converted"));
        }
        if target == DocumentFormat::PwgRaster && request.options.resolution.is_none() {
            // The raster is made at this resolution; say so in the job as well.
            request.options.resolution = Some(resolution);
        }
        let conversion = Conversion { from: format, to: target, resolution, bytes: owned.bytes.len() };
        Ok(Some((owned, conversion)))
    }

    /// [`Self::submit_job`], reduced to the older [`Receipt`] (a message and the backend's job id).
    /// It no longer waits for the job: a receipt means "accepted".
    pub fn submit(&self, request: SubmitRequest<'_>) -> Result<Receipt, PrintError> {
        self.submit_job(request).map(|submitted| submitted.receipt())
    }

    /// The backend that owns a job.
    fn backend_of(backends: &[Arc<dyn Backend>], job: &JobId) -> Result<Arc<dyn Backend>, PrintError> {
        if backends.is_empty() {
            return Err(unsupported());
        }
        match backends.iter().find(|backend| backend.name() == job.backend()) {
            Some(backend) => Ok(Arc::clone(backend)),
            None => Err(PrintError::new(
                ErrorKind::UnknownBackend,
                format!("no print backend named {:?} for job {job} (registered: {})", job.backend(), Self::names_text(backends)),
            )),
        }
    }

    /// One reading of an accepted job's status: a single bounded request to the backend that owns
    /// the job. See [`crate::backends::Backend::job_status`]. A status carries no caller-supplied
    /// totals: a [`JobWatch`] applies [`Submitted::expected`] to what it reads.
    pub fn job_status(&self, job: &JobId) -> Result<JobStatus, PrintError> {
        let backends = self.snapshot();
        Self::backend_of(&backends, job)?.job_status(job)
    }

    /// Ask the owning backend to cancel an accepted job. See
    /// [`crate::backends::Backend::cancel_job`].
    pub fn cancel_job(&self, job: &JobId) -> Result<(), PrintError> {
        let backends = self.snapshot();
        Self::backend_of(&backends, job)?.cancel_job(job)
    }

    /// Whether (and how) the backend that owns this job id can report on it.
    pub fn watch_support(&self, job: &JobId) -> Result<WatchSupport, PrintError> {
        let backends = self.snapshot();
        Ok(Self::backend_of(&backends, job)?.watch_support())
    }

    /// How often to ask [`Self::job_status`] for this job; `None` when the backend cannot report.
    pub fn recommended_poll_interval(&self, job: &JobId) -> Result<Option<Duration>, PrintError> {
        let backends = self.snapshot();
        Ok(Self::backend_of(&backends, job)?.recommended_poll_interval())
    }

    /// A [`JobWatch`] for an accepted job, with the backend's support and recommended interval.
    /// Drive it with [`JobWatch::tick`] against this registry. `Err` when the id names no backend.
    pub fn job_watch(&self, job: &JobId, policy: WaitPolicy) -> Result<JobWatch, PrintError> {
        let backends = self.snapshot();
        let backend = Self::backend_of(&backends, job)?;
        Ok(JobWatch::new(job.clone(), policy).with_support(backend.watch_support()).with_recommended_interval(backend.recommended_poll_interval()))
    }
}

/// A network backend is not contacted, not even to read, without the person's confirmation.
fn require_consent(backend: &dyn Backend, request: &SubmitRequest<'_>) -> Result<(), PrintError> {
    if backend.class() == BackendClass::Network && request.consent.is_none() {
        return Err(PrintError::new(
            ErrorKind::ConsentRequired,
            format!("the {} backend prints over a network and needs the person's confirmation (UserConfirmed)", backend.name()),
        ));
    }
    Ok(())
}

fn unsupported() -> PrintError {
    PrintError::new(ErrorKind::Unsupported, "no print backend is registered")
}

static GLOBAL: OnceLock<Registry> = OnceLock::new();

/// The process-wide registry. On first use it holds the platform's built-in backend (`cups` on
/// macOS and Linux, `spooler` on Windows; none elsewhere). Register more at start-up.
pub fn registry() -> &'static Registry {
    GLOBAL.get_or_init(|| {
        let registry = Registry::new();
        crate::builtin::register_builtin(&registry);
        registry
    })
}

/// Register a backend with the process-wide registry. See [`Registry::register`].
pub fn register(backend: Box<dyn Backend>) -> Result<(), RegisterError> {
    registry().register(backend)
}

/// The printers of every backend of the process-wide registry. See [`Registry::list_printers`].
pub fn list_printers() -> Result<Listing<PrinterRef>, PrintError> {
    registry().list_printers()
}

/// One printer's capabilities. See [`Registry::printer_details`].
pub fn printer_details(id: &str) -> Result<PrinterDetails, PrintError> {
    registry().printer_details(id)
}

/// Print through the process-wide registry; returns once the printer accepted the document. See
/// [`Registry::submit`]. [`crate::submit`] is the unchanged single-backend entry point.
pub fn submit_to_backend(request: SubmitRequest<'_>) -> Result<Receipt, PrintError> {
    registry().submit(request)
}

/// [`submit_to_backend`] with the full [`Submitted`] (the job handle to follow). See
/// [`Registry::submit_job`].
pub fn submit_job(request: SubmitRequest<'_>) -> Result<Submitted, PrintError> {
    registry().submit_job(request)
}

/// Register the app document converter with the process-wide registry. See
/// [`Registry::set_converter`].
pub fn set_converter(converter: Arc<dyn DocumentConverter>) {
    registry().set_converter(converter);
}

/// One reading of a job's status through the process-wide registry. See [`Registry::job_status`].
pub fn job_status(job: &JobId) -> Result<JobStatus, PrintError> {
    registry().job_status(job)
}

/// Cancel a job through the process-wide registry. See [`Registry::cancel_job`].
pub fn cancel_job(job: &JobId) -> Result<(), PrintError> {
    registry().cancel_job(job)
}
