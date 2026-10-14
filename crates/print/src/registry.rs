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

use crate::backend::{Backend, BackendClass, ErrorKind, PrintError, PrinterDetails, PrinterRef, Progress, Receipt, SubmitRequest};
use crate::job::{ExpectedTotals, JobId, JobStatus, Submitted, WatchSupport};
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
}

impl Registry {
    /// An empty registry: every call answers [`ErrorKind::Unsupported`] until a backend is added.
    pub fn new() -> Self {
        Self::default()
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
    /// cancelled ([`ErrorKind::Cancelled`]), that the backend accepts the document's format
    /// ([`ErrorKind::UnsupportedFormat`]) and that a [`BackendClass::Network`] backend has a
    /// [`crate::UserConfirmed`] ([`ErrorKind::ConsentRequired`]).
    pub fn submit_job(&self, request: SubmitRequest<'_>) -> Result<Submitted, PrintError> {
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
        let format = request.document.format;
        if !backend.accepted_formats().contains(&format) {
            return Err(PrintError::new(
                ErrorKind::UnsupportedFormat,
                format!("the {} backend does not accept {} documents", backend.name(), format.mime()),
            ));
        }
        if backend.class() == BackendClass::Network && request.consent.is_none() {
            return Err(PrintError::new(
                ErrorKind::ConsentRequired,
                format!("the {} backend prints over a network and needs the person's confirmation (UserConfirmed)", backend.name()),
            ));
        }
        request.job.printer = native;
        let expected = ExpectedTotals::for_request(&request.job, &request.options);
        let SubmitRequest { document, job, options, consent, cancel, progress } = request;
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
        if let Some(status) = submitted.status.as_mut() {
            expected.apply(status);
        }
        Ok(submitted)
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

/// One reading of a job's status through the process-wide registry. See [`Registry::job_status`].
pub fn job_status(job: &JobId) -> Result<JobStatus, PrintError> {
    registry().job_status(job)
}

/// Cancel a job through the process-wide registry. See [`Registry::cancel_job`].
pub fn cancel_job(job: &JobId) -> Result<(), PrintError> {
    registry().cancel_job(job)
}
