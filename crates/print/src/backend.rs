//! The [`Backend`] trait and the neutral types a backend speaks.
//!
//! A backend is one way of getting a document onto paper: the system spooler (CUPS, the Windows
//! spooler) or, later, a network protocol (WS-Print, IPP). The [`Registry`](crate::Registry) holds
//! several at once, namespaces their printer ids (`"<backend>:<native id>"`), aggregates their
//! printer lists and routes details and jobs by that prefix.
//!
//! # Writing a backend
//!
//! * **Object safe, blocking, `Send + Sync`.** The registry stores `Arc<dyn Backend>` and calls
//!   from whatever thread the app uses (a worker, by convention; printing is synchronous, as it is
//!   for [`crate::submit`]). A backend that speaks a network protocol blocks on its own transport
//!   inside [`Backend::submit`]; there is no async runtime in this crate.
//! * **Cancellation and progress travel in the request**: [`SubmitRequest::cancel`] is polled at
//!   the backend's own checkpoints (between sheets, between network round trips); progress is the
//!   same `(processed, sheets)` callback as [`crate::submit_with_progress`]. Neither is required:
//!   the built-in spooler backends check the token only before they start.
//! * **Ids**: a backend names printers with its own native ids and never adds its prefix; the
//!   registry does. A native id may contain colons; only the first colon separates the prefix.
//! * **Capabilities are lists and may be unknown.** [`PrinterDetails`] uses `Option`/empty lists
//!   for "not published". Options such as duplex, colour and scaling are hints a backend may
//!   ignore; [`PrinterDetails::honoured`] tells a UI which ones it applies.
//! * **Status is optional.** `None` is "cannot say", not "all clear". Spoolers report after the
//!   fact; a network protocol can report state up front.
//! * **Documents are bytes plus a declared format.** A backend declares what it accepts with
//!   [`Backend::accepted_formats`]; the registry refuses anything else before the backend is called
//!   ([`ErrorKind::UnsupportedFormat`]). A backend that converts (for example PDF to raster) does
//!   so inside `submit`.
//! * **Consent.** A backend whose [`Backend::class`] is [`BackendClass::Network`] only receives
//!   requests that carry a [`UserConfirmed`]; the registry enforces it so a backend cannot forget.
//! * **Errors are neutral**: choose an [`ErrorKind`] and attach the native code with
//!   [`PrintError::with_native`].
//! * **Registration**: `registry.register(Box::new(MyBackend::new()))`, once at start-up. The
//!   process-wide registry ([`crate::registry`]) already holds the platform's built-in backend.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{Error, Job, PrinterOption};

/// A native error code and the domain that owns it. `domain` names the code space:
/// `"win32"`, `"ipp"`, `"wsd-fault"`, `"cups"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeCode {
    pub domain: &'static str,
    pub code: i64,
    /// The code's symbolic name, when known.
    pub name: Option<&'static str>,
}

/// What went wrong, independent of the backend. The kind and [`ErrorKind::is_retryable`] are the
/// contract callers branch on; the message is for people.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// No backend is registered, or the platform has no print stack.
    Unsupported,
    /// The printer id names a backend that is not registered (or has no prefix).
    UnknownBackend,
    /// The backend does not know the printer, or there is no default printer.
    NotFound,
    /// The request is malformed or asks for something the printer cannot do.
    InvalidRequest,
    /// The printer or backend does not accept the document's format.
    UnsupportedFormat,
    /// A network-class backend was asked to print without a [`UserConfirmed`].
    ConsentRequired,
    /// The caller cancelled.
    Cancelled,
    /// The spooler, device or network is not reachable right now.
    Unavailable,
    /// The device is busy; try again later.
    Busy,
    /// The device did not answer in time.
    Timeout,
    /// The printer or driver refused the job.
    Rejected,
    /// Reading or writing the document failed.
    Io,
    /// Anything else; the message says more.
    Failed,
}

impl ErrorKind {
    /// Whether trying the same request again later can succeed.
    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::Unavailable | Self::Busy | Self::Timeout)
    }
}

/// A print failure: a neutral [`ErrorKind`], a message and optionally the backend's native code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrintError {
    kind: ErrorKind,
    message: String,
    native: Option<NativeCode>,
}

impl PrintError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), native: None }
    }

    /// Attach the backend's own error code.
    #[must_use]
    pub fn with_native(mut self, native: NativeCode) -> Self {
        self.native = Some(native);
        self
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn native(&self) -> Option<NativeCode> {
        self.native
    }

    pub fn is_retryable(&self) -> bool {
        self.kind.is_retryable()
    }
}

impl std::fmt::Display for PrintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PrintError {}

/// The legacy spooler error keeps its message; it carries no more detail than that, so the kind
/// is [`ErrorKind::Failed`].
impl From<Error> for PrintError {
    fn from(error: Error) -> Self {
        Self::new(ErrorKind::Failed, error.to_string())
    }
}

/// The format of the bytes in a [`Document`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DocumentFormat {
    /// `application/pdf`, what the apps generate.
    Pdf,
    /// `image/pwg-raster`.
    PwgRaster,
    /// `application/postscript`.
    PostScript,
    /// `application/octet-stream`: the printer's own page description language.
    OctetStream,
}

impl DocumentFormat {
    pub const fn mime(self) -> &'static str {
        match self {
            Self::Pdf => "application/pdf",
            Self::PwgRaster => "image/pwg-raster",
            Self::PostScript => "application/postscript",
            Self::OctetStream => "application/octet-stream",
        }
    }
}

/// Document bytes and their declared format. The crate does not sniff or convert.
#[derive(Clone, Copy, Debug)]
pub struct Document<'a> {
    pub format: DocumentFormat,
    pub bytes: &'a [u8],
}

impl<'a> Document<'a> {
    pub fn new(format: DocumentFormat, bytes: &'a [u8]) -> Self {
        Self { format, bytes }
    }

    pub fn pdf(bytes: &'a [u8]) -> Self {
        Self::new(DocumentFormat::Pdf, bytes)
    }
}

/// Whether a backend stays on this machine or talks to devices over a network.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BackendClass {
    /// The system spooler: the user already chose the queue.
    #[default]
    Local,
    /// Connects to devices itself; every job needs a [`UserConfirmed`].
    Network,
}

/// Proof that a person agreed to this print. It carries no policy of its own: the registry only
/// requires one for [`BackendClass::Network`] backends, and an app decides how a person gets to
/// say yes. It can be obtained only through [`Self::attest`] or [`Self::interactive`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserConfirmed(());

impl UserConfirmed {
    /// The guard for agent and automation callers: pass the `user_confirmed` argument as given.
    /// Absent or false is [`ErrorKind::ConsentRequired`], with a message that says what to do.
    pub fn attest(user_confirmed: Option<bool>) -> Result<Self, PrintError> {
        if user_confirmed == Some(true) {
            Ok(Self(()))
        } else {
            Err(PrintError::new(
                ErrorKind::ConsentRequired,
                "printing to a network printer needs user_confirmed: true, set only after the person has agreed to this print",
            ))
        }
    }

    /// For interactive callers where the person has just pressed Print. Do not use it for anything
    /// an agent or script can reach.
    pub fn interactive() -> Self {
        Self(())
    }
}

/// A cooperative cancellation flag shared between the caller and a running job. Clones share the
/// flag.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// A printer in a list. In a [`Registry`](crate::Registry) result `id` is namespaced
/// (`"<backend>:<native id>"`); a backend returns its native id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrinterRef {
    pub id: String,
    /// What a person sees.
    pub name: String,
    pub default: bool,
    /// Whether the printer takes two-sided jobs; `None` when unknown.
    pub two_sided: Option<bool>,
}

/// The job settings of [`Job`] a printer may honour. Everything else is a hint it ignores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum JobHint {
    Copies,
    Collate,
    Duplex,
    Colour,
    Scaling,
    /// `Job::options`, the driver's own keywords.
    DriverOptions,
}

/// The state a printer reports before a job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PrinterState {
    Idle,
    Processing,
    Stopped,
    Offline,
}

/// A printer's reported state. Absent from [`PrinterDetails`] when the backend cannot say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrinterStatus {
    pub state: PrinterState,
    /// Human-readable reasons the device gives (`"paper empty"`), as reported.
    pub reasons: Vec<String>,
}

/// What a printer can do. Every list may be empty or `None`, which means "not published", not
/// "none supported".
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PrinterDetails {
    pub id: String,
    pub name: String,
    pub default: bool,
    /// The formats this printer takes; empty when the backend publishes only its own list.
    pub formats: Vec<DocumentFormat>,
    /// Driver job options (CUPS: the PPD's). Empty when none or unknown.
    pub options: Vec<PrinterOption>,
    pub two_sided: Option<bool>,
    /// Whether colour is available; `None` when unknown.
    pub colour: Option<bool>,
    /// Paper names the printer offers; empty when not published.
    pub paper: Vec<String>,
    pub max_copies: Option<u32>,
    /// Which [`Job`] settings this backend applies. `None`: not published, treat as unknown.
    pub honoured: Option<Vec<JobHint>>,
    pub status: Option<PrinterStatus>,
}

/// What a backend returns for an accepted job. Receipts are messages, not portable job ids.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    /// The backend's registry name.
    pub backend: &'static str,
    pub message: String,
    /// The backend's job id, when it has one.
    pub job_id: Option<String>,
}

/// Progress callback: `(processed, sheets)`, as for [`crate::submit_with_progress`].
pub type Progress<'a> = &'a mut dyn FnMut(usize, usize);

/// One print request. [`Job::printer`] is the printer: a namespaced id when it goes to a
/// [`Registry`](crate::Registry), the native id when a backend receives it; `None` is the default.
pub struct SubmitRequest<'a> {
    pub document: Document<'a>,
    pub job: Job,
    pub consent: Option<UserConfirmed>,
    pub cancel: CancelToken,
    pub progress: Option<Progress<'a>>,
}

impl<'a> SubmitRequest<'a> {
    pub fn new(document: Document<'a>, job: Job) -> Self {
        Self { document, job, consent: None, cancel: CancelToken::new(), progress: None }
    }

    #[must_use]
    pub fn with_consent(mut self, consent: UserConfirmed) -> Self {
        self.consent = Some(consent);
        self
    }

    #[must_use]
    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    #[must_use]
    pub fn with_progress(mut self, progress: Progress<'a>) -> Self {
        self.progress = Some(progress);
        self
    }
}

/// One way of printing. Ids passed in and returned are the backend's **native** ids; the
/// [`Registry`](crate::Registry) adds and strips the `"<name>:"` prefix. See the module docs.
pub trait Backend: Send + Sync {
    /// The registry name and id prefix: short, lowercase ASCII letters, digits, `-` and `_`
    /// (`"cups"`, `"spooler"`, `"wsd"`, `"ipp"`).
    fn name(&self) -> &'static str;

    /// Whether jobs need a [`UserConfirmed`]. Defaults to [`BackendClass::Local`].
    fn class(&self) -> BackendClass {
        BackendClass::Local
    }

    /// The formats [`Backend::submit`] takes. Defaults to PDF.
    fn accepted_formats(&self) -> &[DocumentFormat] {
        &[DocumentFormat::Pdf]
    }

    /// The printers the backend lists. Opens none of them.
    fn list_printers(&self) -> Result<Vec<PrinterRef>, PrintError>;

    /// What one printer (a native id) can do. [`ErrorKind::NotFound`] for an unknown printer.
    fn printer_details(&self, native_id: &str) -> Result<PrinterDetails, PrintError>;

    /// Print. `request.job.printer` is a native id or `None` for the backend's default. The
    /// registry has already checked the format, the consent and that the token is not cancelled.
    fn submit(&self, request: SubmitRequest<'_>) -> Result<Receipt, PrintError>;
}
