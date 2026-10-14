//! The registry of print backends. See [`crate::backend`] for how to write one.
//!
//! Printer ids are namespaced `"<backend>:<native id>"`. Listing asks every backend and keeps what
//! answers: a backend that fails does not hide the others' printers, it appears in
//! [`Listing::errors`]. Details and jobs route by the id's prefix. Ids are exact: an id without a
//! registered prefix is [`ErrorKind::UnknownBackend`], never a guess.

use std::sync::{Arc, OnceLock, PoisonError, RwLock};

use crate::backend::{Backend, BackendClass, ErrorKind, PrintError, PrinterDetails, PrinterRef, Receipt, SubmitRequest};

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
        if backends.is_empty() {
            return Err(unsupported());
        }
        let Some((prefix, native)) = split_id(id) else {
            return Err(PrintError::new(
                ErrorKind::UnknownBackend,
                format!("printer id {id:?} has no backend prefix; ids look like <backend>:<id> (registered: {})", Self::names_text(backends)),
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

    /// Print. `request.job.printer` picks the backend:
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
    pub fn submit(&self, request: SubmitRequest<'_>) -> Result<Receipt, PrintError> {
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
        backend.submit(request)
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

/// Print through the process-wide registry. See [`Registry::submit`]. [`crate::submit`] is the
/// unchanged single-backend entry point.
pub fn submit_to_backend(request: SubmitRequest<'_>) -> Result<Receipt, PrintError> {
    registry().submit(request)
}
