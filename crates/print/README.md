# craft-print

Submit a print-ready PDF to the system spooler. Used by PdfCraft and DesignCraft;
there are no dependencies on either app or on a UI framework. Requires Rust 1.95.

```rust,no_run
let job = craft_print::Job { title: "Report".into(), ..Default::default() };
let receipt = craft_print::submit(&pdf_bytes, &job)?;
```

The caller generates the PDF, checks document permissions, and handles page selection,
imposition, marks and bleed. This crate discovers printers, exposes CUPS driver options,
opens Windows printer preferences and submits jobs with copies, collation, duplex and colour.
`command_preview` returns diagnostics without creating files or starting a process.

## Backends and the registry

`submit`, `printers` and the rest of the functions above are the unchanged single-spooler API.
Beside them the crate has a `backends::Backend` trait and a `Registry` that holds several
backends at once (the system spooler plus, later, network protocols such as WS-Print or IPP):

```rust,no_run
use craft_print::{Document, Job, SubmitRequest};

let listing = craft_print::list_printers()?;          // every backend; ids look like "cups:Office"
for failure in &listing.errors { eprintln!("{}: {}", failure.backend, failure.error); }
let job = Job { printer: Some("cups:Office".into()), ..Job::default() };
let receipt = craft_print::submit_to_backend(SubmitRequest::new(Document::pdf(&pdf_bytes), job))?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

* **Built-in backend.** `SystemSpooler` is an adapter over the existing functions (`printers`,
  `printer_options`, `submit_with_progress`), so behaviour is the same. It registers as `cups` on
  macOS and Linux and as `spooler` on Windows (the `winprint` code stays behind `cfg(windows)`);
  the process-wide registry (`craft_print::registry()`) holds it from the start. The older
  `Backend` enum and `backend()` still describe that built-in kind; the trait lives at
  `craft_print::backends::Backend`.
* **Ids** are `"<backend>:<native id>"`; only the first colon separates the prefix. An id
  without a registered prefix is `ErrorKind::UnknownBackend`. A job with no printer goes to the
  only backend, or to the first one that lists a default printer.
* **Several backends, partial failure.** `list_printers` asks every backend and returns what
  answered plus one `BackendError` per backend that failed; it is `Err(Unsupported)` only when
  none is registered. `Registry::register` returns `RegisterError` for a duplicate or invalid name,
  never panics. Make a standalone `Registry::new()` for isolation or tests.
* **Neutral errors.** `PrintError::kind()` (`ErrorKind`) and `is_retryable()` are the contract;
  `native()` is an optional `NativeCode { domain, code, name }` with domains such as `"win32"`,
  `"ipp"`, `"wsd-fault"` or `"cups"`. The legacy `Error::Spool` converts with its message kept.
* **Capabilities may be unknown.** `PrinterDetails` lists formats, driver options, paper and the
  `JobHint`s the backend honours (`honoured`); an empty list or `None` means "not published".
  Duplex, colour, scaling and driver options in a `Job` are hints a backend may ignore, and
  `honoured` tells a UI which ones it applies. `status` is `None` when the backend cannot say
  before a job (CUPS and the Windows spooler report after the fact).
* **Documents** are bytes plus a `DocumentFormat` (`Pdf`, `PwgRaster`, `PostScript`,
  `OctetStream`). A backend declares `accepted_formats()`; the registry refuses anything else
  with `ErrorKind::UnsupportedFormat` before calling it.
* **Consent.** `UserConfirmed` is a marker with no policy of its own (`attest(Option<bool>)`
  errors unless `Some(true)`; `interactive()` for a Print button the person just pressed). Local
  backends (`BackendClass::Local`, the default) ignore it; a `BackendClass::Network` backend only
  ever receives requests that carry one, which the registry enforces.

### Job lifecycle

Submitting and watching are separate. `Registry::submit_job` returns a `Submitted` as soon as the
printer has **accepted** the document (a bounded call; it never waits for the paper). What happens
next is asked for, one request at a time, with the job's id:

```rust,no_run
use craft_print::{Document, Job, JobWatch, SubmitRequest, WaitPolicy, WatchStep};

let registry = craft_print::registry();
let job = Job { printer: Some("wsd:http://192.0.2.1:5357/p".into()), ..Job::default() };
let request = SubmitRequest::new(Document::pdf(&pdf_bytes), job).with_consent(craft_print::UserConfirmed::interactive());
let submitted = registry.submit_job(request)?;            // accepted; not printed yet
let Some(mut watch) = JobWatch::for_submitted(&submitted, WaitPolicy::default()) else { return Ok(()) };
// In a UI frame or a worker loop; each tick makes at most one request, and only when one is due:
match watch.tick(registry) {
    WatchStep::Pending { next_in } => { /* come back in next_in */ }
    WatchStep::Update(status) => { /* status.state, status.sheet_fraction() */ }
    WatchStep::Retrying { error, next_in, .. } => { /* a poll failed transiently */ }
    WatchStep::Done(outcome) => { /* Terminal, Forgotten, NoStatus, TimedOut or Failed */ }
    _ => {}
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

* `Submitted` carries a typed `JobId` (`backend()` and `native()`; text form `"wsd:<printer>#7"` via
  `Display`/`FromStr`, `JobId::new(backend, native)` to build one, a typed `ParseJobIdError` for bad
  text, never a panic), `accepted_at`, warnings, a message, and what the backend can do afterwards
  (`watch: WatchSupport`, `poll_interval`). The job is `None` for a spooler that gives no handle.
* `WatchSupport::None` means the backend cannot report (a fire-and-forget spooler): the status is
  `JobState::Unknown`, "accepted, outcome unknown", which is not terminal and not a failure. Do not
  poll it. `Poll` means ask every `poll_interval`; `Events` is reserved (nothing listens yet).
* `JobStatus` has a `JobState` (Pending, Held, Processing, Stopped, Completed, CompletedWithWarnings,
  CompletedWithErrors, Cancelled, Aborted, Unknown), reasons, optional sheet and page counters and
  `terminal()`. A job the device has forgotten is `ErrorKind::NotFound`; a watch reports it as
  `WatchOutcome::Forgotten` (most likely finished).
* `Registry::job_status(&JobId)` and `cancel_job(&JobId)` are single bounded requests routed by the
  id's backend. Keep the `JobId` (or its string and `.parse()` it later); do not build ids by hand.
* `JobWatch` is a state machine **you** advance: no thread, no timer. It has an interval that backs
  off while nothing changes, a `max_wait`, a `CancelToken` (one cancel is sent, then it keeps watching
  for the end state), failure retries under a `RetryPolicy`, and an injectable `Clock` (`ManualClock`
  in tests). `poll_status` forces a read now. `wait_for_job(registry, &job, WaitPolicy)` is the
  blocking convenience for a CLI, built on the same ticks; `max_wait` is finite (two minutes by
  default) and the job is left alone when it runs out.
* **Retry.** `PrintError::is_retryable()` says the condition is transient; `retry_safety()` says
  whether sending again is safe: `Safe` (nothing was accepted), `Idempotent` (a query or cancel) or
  `OutcomeUnknown` (the request may have arrived: reconcile before resubmitting). `phase()` and
  `retry_after()` complete it, and `RetryPolicy` (`next_delay`, `delay`, the `retry` helper) is a
  bounded exponential backoff with jitter that never repeats an `OutcomeUnknown` error. The neutral
  defaults per `ErrorKind` are tabulated in [docs/job-lifecycle.md](docs/job-lifecycle.md), which also
  has the survey of how CUPS/IPP, WS-Print and the Windows spooler separate accepting from printing.
* `Registry::submit` and `submit_to_backend` still return a `Receipt`; it now means "accepted".

### Per-job options and totals

* `SubmitRequest::with_options(JobOptions)` carries per-job **hints** beside the spooler's own `Job`:
  `media` (a PWG token such as `iso_a4_210x297mm`), `orientation`, `quality`, `resolution`
  (`Resolution::new(x_dpi, y_dpi)`), an explicit `colour` (`ColourMode::Auto`, `Colour`, `Mono`; it
  overrides `Job::grayscale` unless `Auto`) and `expected_pages`. Build with `JobOptions::new()` and
  the `with_*` methods; the struct is `#[non_exhaustive]`. A backend applies what it can;
  `PrinterDetails::honoured` lists the `JobHint`s it applies (`Media`, `Orientation`, `Quality`,
  `Resolution` and `Colour` joined `Copies`, ...). The built-in spooler honours the colour choice only.
* `Registry::submit_job` accepts them, so an app needs no backend-specific entry point. Settings that
  are backend-wide configuration (a network backend's ticket shape, strict mode, transfer) stay on
  the backend: an app that wants different ones per invocation builds a backend for that invocation
  (or its own `Registry`) with them.
* **Totals are optional.** `Progress { done, total: Option<u32> }` is what the progress callback gets
  (`fraction()` is `None` while the total is unknown), and `JobStatus`'s counters were already
  optional. `expected_pages` is the page count the app knows: the registry turns it into an
  estimated sheet total (halved for two-sided jobs, times the copies; `Submitted::expected`) and
  uses it as the total of progress events and, through `JobWatch::for_submitted` (or
  `JobWatch::with_expected`), of status readings, **only where the backend reports none**; a
  backend-reported total always wins. `JobStatus::sheet_fraction()` is then determinate.

### Writing a backend (the extension points for a network backend)

* The trait is object safe and **blocking but bounded**: `Backend: Send + Sync`, stored as
  `Arc<dyn Backend>`. There is no async runtime here; a network backend blocks on its own transport
  for at most its per-request timeouts, never for the length of a print (see "Job lifecycle").
  Cancellation is a cloneable `CancelToken` in the `SubmitRequest` that the backend polls between
  round trips (the registry also checks it before calling); progress is a callback of
  `Progress { done, total: Option<u32> }` events (see "Per-job options and totals").
* Implement `name`, `list_printers`, `printer_details` and `submit` (which returns `Submitted` once the
  printer has accepted the document). Override `watch_support`, `job_status` and `cancel_job` if the
  protocol can tell what happened to a job afterwards; the defaults say "accepted, outcome unknown".
  Override `class` (network backends return `BackendClass::Network`) and `accepted_formats` (for
  example raster or the printer's own language) as needed. Use native ids; the registry adds the
  prefix, to printers and to jobs. A job is `JobId::new(self.name(), native)`.
* Register once at start-up: `craft_print::register(Box::new(MyBackend::new()))?`, or on a
  `Registry` you own. A backend that needs extra dependencies can live behind a cargo feature
  that registers it; the base crate is complete without it.

`tests/registry.rs` drives the public API with test-only fake backends (CUPS-like with options
and no status, IPP-like with state up front, WSD-like with formats and a consent requirement, and
one that always fails): aggregation, routing, partial failure, duplicate registration, unknown
prefix, consent and format refusals.

## WS-Print backend (cargo feature `wsd`)

`craft_print::wsd::WsdBackend` prints to network printers that speak the WS-Print service
(Web Services for Devices). It is a `BackendClass::Network` backend registered as `wsd`; its
printers are `wsd:<url>`.

```rust,no_run
use craft_print::wsd::WsdBackend;
use craft_print::{Document, Job, SubmitRequest, UserConfirmed};

let wsd = WsdBackend::new();
wsd.add_named("http://10.0.0.20:5357/print", "Front office")?; // the app chose this printer
craft_print::register(Box::new(wsd.clone()))?;               // clones share the printer list

let details = craft_print::printer_details("wsd:http://10.0.0.20:5357/print")?; // one round trip
let job = Job { printer: Some("wsd:http://10.0.0.20:5357/print".into()), ..Job::default() };
let request = SubmitRequest::new(Document::pdf(&pdf_bytes), job).with_consent(UserConfirmed::interactive());
let submitted = craft_print::submit_job(request)?;   // returns when the printer has accepted the document
let job = submitted.job.expect("WS-Print gives every accepted job a handle"); // "wsd:<url>#<job id>"
let status = craft_print::job_status(&job.id)?;       // one GetJobElements; or follow it with a JobWatch
# Ok::<(), Box<dyn std::error::Error>>(())
```

| Feature | Default | Adds | Notes |
| --- | --- | --- | --- |
| `wsd` | on | `craft-wsd` (git, see below) and `roxmltree`; through `craft-wsd`: `thiserror` and `ureq` (plain HTTP, no TLS, no proxy, no redirects) | Not built for `wasm32`. `--no-default-features` is the CUPS/Windows crate with none of them. |

* **Safety stance.** Nothing in this feature discovers or connects until the app adds a printer.
  The backend lists only printers the app added (`add`, `add_named`, `with_printers`); it does no
  network discovery and sends no multicast (finding printers is a separate decision). Making a
  backend, adding a printer and listing open no connection; a connection is made by
  `printer_details` (one `GetPrinterElements`) and by a job. Every job needs consent: the
  registry refuses a request without a `UserConfirmed` (`ErrorKind::ConsentRequired`), and the
  backend checks again if it is called directly. A request can only reach a printer that was added.
  URLs are `http://` with a numeric loopback, link-local or private address; host names, public
  addresses, redirects and proxies are refused (`craft-wsd`'s policy). Replies, lists and XML are
  size-capped and every reply is checked (action, relation to the request, namespaces).
* **Printer details** map from `GetPrinterElements`: accepted formats (PDF, PWG raster,
  PostScript, octet-stream, in the printer's order; other formats are not named), two-sided,
  colour, paper names, maximum copies, status (idle, processing, stopped, with the reasons the
  printer gives) and the hints it applies (copies, duplex, colour as grey). Unlisted
  dimensions are `None` or empty, not "unsupported".
* **Jobs.** A document is bytes in a `DocumentFormat` the printer lists (case-insensitively; the
  printer's spelling is sent), otherwise `ErrorKind::UnsupportedFormat` before a job exists. The
  job is created and the document sent as an MTOM part (or inline base64, `with_transfer`);
  **`submit` returns when the printer has accepted the document** (the reply to `SendDocument`),
  after at most capabilities, create, send (and a cleanup cancel when the printer refused the
  document), each bounded by the request timeout. It never waits for the print. The accepted job
  is `Submitted::job`, `wsd:<printer url>#<job id>`. Copies, two-sided and grey come from the
  `Job`; collation, scaling and driver options have no WS-Print setting and are reported as
  warnings in `Submitted::warnings`. Progress is `(done, 3)` protocol phases (capabilities read,
  job created, document accepted), not sheets. The cancel token is polled between round trips;
  cancelling before the document is sent cancels the created job and returns
  `ErrorKind::Cancelled`. A failed send whose outcome is unknown (the connection closed or timed
  out after the document was written) leaves the created job alone: cancelling could abort a job
  that is printing.
* **Following a job.** `watch_support()` is `Poll` (events through WS-Eventing would need an inbound
  listener and a lease; that is a documented future option and nothing listens) and
  `recommended_poll_interval()` is two seconds (`with_poll_interval`). `job_status` is one
  `GetJobElements`: the nine WS-Print job states map to `Pending`, `Held`, `Processing` (also
  `Started` and `Terminating`), `Stopped`, `Completed`, `CompletedWithWarnings`,
  `CompletedWithErrors` (from the state reasons), `Cancelled` and `Aborted` (`Other` is `Unknown`),
  with the state reasons and `sheets_done` (`MediaSheetsCompleted`; WS-Print does not give a total).
  A job the printer does not list is `ErrorKind::NotFound` whose message says printers forget finished
  jobs, so it most likely finished; a watch reports it as `Forgotten`. `cancel_job` is `CancelJob`,
  treating "too late" as success (`wsd::job::cancel_job_tolerant`). A job handle must name an added
  printer; anything else is `ErrorKind::InvalidRequest`, never `NotFound`.
* **Reconcile.** After an error with `retry_safety() == OutcomeUnknown` (for example the connection
  closed with no reply to `SendDocument`), `WsdBackend::reconcile(printer, title)` reads
  `GetActiveJobs` and `GetJobHistory` (reads only) and returns the jobs with that title from this
  backend's user: `Found`, `Ambiguous` (newest first) or `NotFound`. Job summaries have no creation
  time, so the window is what the printer still lists. A job created without a document shows as
  pending and can be cancelled through the registry.
* **Ticket shapes.** Some printers' firmware does not answer tickets containing `JobProcessing` or
  `DocumentProcessing`. `with_ticket_shape(TicketShape::Description | JobNameOnly | Empty)` sends a
  smaller ticket (`Full`, the default, writes everything set). A smaller ticket has no room for
  copies, two-sided and the like: such a job is refused before anything is sent
  (`ErrorKind::InvalidRequest`) unless `with_drop_ticket_settings(true)` drops them with a
  warning each. `with_strict(true)` refuses settings the printer lists as unsupported and sends
  every setting as `MustHonor`; otherwise unsupported settings are lowered or left out, with a warning.
* **Errors** are neutral `PrintError`s. Faults carry `NativeCode { domain: "wsd-fault", code,
  name }` (codes 1 to 16 in the order of the service description, 100 for
  `ActionNotSupported`, 0 for a vendor code) and temporary refusals (not accepting jobs,
  temporary error, document active) are `ErrorKind::Busy` with a suggested `retry_after`. Beyond
  the kind, every error says whether the condition is transient (`is_retryable`), whether sending
  again is safe (`retry_safety`) and where it failed (`phase`). The rules, in short: a failure that
  proves the printer did not act is `Safe` (policy, connect failure, 408, 425, 429, 503, any SOAP
  fault); a failure after the request may have arrived is `OutcomeUnknown` for a create or a send
  (connection closed or reset, timeout waiting for the reply, 500, 502, 504, an unreadable reply)
  and `Idempotent` for a query or a cancel. A refused redirect, a refused address, 501, 505 and
  other 4xx are not retryable. The full table, per error kind and operation, is in
  [docs/job-lifecycle.md](docs/job-lifecycle.md) and pinned by the table tests in `wsd::error`.
  `craft-wsd` reports transport failures as a typed kind and stage (connect failures, timeouts,
  redirects, a closed connection), so nothing is read from text; a `Retry-After` the printer sends
  becomes the error's `retry_after`.
* **Lower layers** are public for apps that need them: `wsd::client::Client` (one method per
  operation: `GetPrinterElements`, `CreatePrintJob`, `SendDocument`, `GetJobElements`,
  `GetActiveJobs`, `GetJobHistory`, `CancelJob`), `wsd::job::submit_document` (the accepted
  half of a print) and `wsd::job::print_document` (accept, then a blocking watch) with a
  `DocumentEncoder` (`PassThrough`, `Chain`, `RasterEncoder` over a caller-supplied page renderer
  that writes PWG raster) and `cancel_job_tolerant`. `wsd::adapter` is the only file that names
  `craft-wsd`.
* **Dependency.** `craft-wsd` is in `storytold/craft-libs` pull request 113 and is pinned to that
  pull request's ref (`rev = "refs/pull/113/head"`) until it merges; then pin an immutable
  revision.
* **Tests.** `tests/wsd_registry.rs` (through the registry: listing, policy, consent, format
  refusal, ticket shapes, accepted-not-printed submits, watching on an injected clock, cancellation,
  progress, outcomes, faults, retries, reconcile after a lost reply, and hostile replies from
  loopback servers: huge, malformed, wrong namespace, HTML 5xx, redirects, silence, a closed port,
  connection closed), `tests/wsd_client.rs` (the client layer against an in-memory fake printer) and the unit
  tests of each module. `tests/wsd_live.rs` runs only when `FAKEPRINT_URL` names a loopback
  WS-Print service of a test server; it is skipped otherwise.

- **macOS/Linux:** CUPS `lpstat`, `lpoptions` and `lp`. Jobs stream through stdin. Exact-size
  sheets use `fit-to-page=false`; shrink-to-fit uses IPP `print-scaling=auto-fit`.
- **Windows:** in this process, with no PowerShell, script or other program started, so
  printing works where App Control for Business or AppLocker enforce script rules. Printers
  come from `EnumPrinters` (local queues and `\\server\queue` connections) and the default from
  `GetDefaultPrinter`. A job is drawn as vectors by the in-box `Windows.Data.Pdf` with Direct2D
  into the XPS print API (the `winprint` crate), and copies, collation, duplex, colour, paper,
  orientation and "no scaling" travel in a PrintTicket that the driver validates first; a
  driver that would change the paper, orientation or scaling is refused before anything is
  sent. Paper is the driver's own option for the sheet's size (to 0.25 mm), preferring the one
  named like the standard size ("ARCH D" over a custom 24 × 36 in entry). This backend comes
  from storytold/pdfcraft#599 and #1132.
  A job has one ticket, so one paper and orientation. Sheets of the other orientation are
  turned back with `/Rotate`, so portrait and landscape sheets still print as one job. Exact
  size refuses sheets of different sizes and paper the driver lacks. Shrink-to-fit uses the
  driver's matching paper, else its default paper, and puts every sheet on it: a sheet that
  doesn't fit inside the paper's widest hardware margin is shrunk, a smaller one keeps its
  size, and both are centred. Both are done by appending an incremental update to the PDF
  (new versions of the pages that need it, the original bytes untouched); the crate reads
  only cross-reference sections, object streams and the page tree for this, leniently and
  with every size bounded. Encrypted PDFs can be turned but not reframed. Annotations on a
  reframed page are dropped; print-ready PDFs have them flattened. Nondefault or invalid
  `/UserUnit` is refused because the native renderer does not reliably honor physical units.
  Parsing caps cumulative cross-reference/object-stream records at 500,000, decoded streams
  at 16 MiB each and 32 MiB per job, and parsed nodes/tokens at 250,000/16 MiB per job and
  32,000/1 MiB per object. Files exceeding these limits return a clear error.
  Long-edge duplex is `TwoSidedLongEdge` and short-edge `TwoSidedShortEdge`; `duplex: None`
  leaves duplex out of the ticket. Driver-specific defaults remain in Windows printer
  preferences (`rundll32 printui.dll`, the one process this crate starts, and only when asked).
- **Browser/other targets:** discovery is empty and submission reports an unsupported
  platform; callers can offer PDF export instead.

Windows jobs briefly use a file in a private temporary folder (random name, this user's only),
named after the job's title because the spooler shows it as the document name; the folder is
removed once the spooler has the job, on success and failure. Failed and canceled terminal
package statuses are errors, including failures received after every sheet was processed.
A missing completion notification times out after two minutes after the native print control
is closed and requests cancellation. Calls into a driver and interactive output-file dialogs
are not covered by this notification timeout. Windows answers when the job is in
the queue ("spooled 3 sheet(s)"): a paused queue or an offline printer keeps it there. CUPS jobs
never use a PDF temporary file. Printing is synchronous; UI callers should submit on a worker.
`submit_with_progress` reports `(processed, sheets)` before the first Windows sheet and after
each sheet is successfully added to the native print job. These are source PDF sheets, excluding
copies and duplex; the events describe sheet processing, and do not confirm physical output.
The callback runs on the thread calling submission, can capture non-`Send` state, and should
return promptly. It is never retained. Keep waiting for the final `Result` even at full progress:
the job can still fail. The API does not provide cancellation. CUPS reports no progress.
UI callers must forward events to their own progress state; adopting the API alone does not
change an application's UI.
Receipts are backend messages, not portable job IDs. `Job::duplex = None` preserves the queue's
duplex default; `Some(Duplex::Off)` forces simplex. `Job::scaling` defaults to `Scaling::Exact`;
callers exporting arbitrary document sizes can choose `Scaling::ShrinkToFit`. Exact mode reports
a driver paper-size mismatch. On Windows `command_preview` describes the in-process job
(`xps-print`, then the printer and options as `key=value`) instead of a command line. The crate
does not provide PostScript output, separations, trapping or colour-managed device rendering.

Windows shrink-to-fit refuses pages with annotations when their geometry needs fitting.
Callers must flatten annotation appearances into page content first. Exact-size printing and
rotation retain the annotations. This avoids losing printed comments, forms or other marks.
The Windows renderer uses viewer visibility. Native submission therefore accepts only plain
visible printable annotation flags (`F=4`); annotations without Print, with NoView, or with
other visibility/rotation flags require caller flattening. This prevents printing comments
that were marked nonprinting or omitting print-only marks. A visible Square appearance with
`F=4` was rendered in the Windows qualification fixture; other appearance types still need
their own fidelity checks.

Native Windows PDF submission checks the driver's validated copy count and explicit
simplex/duplex and grayscale requests before sending. Collation must be retained when both
the page count and copy count exceed one. A driver that drops or changes these options is
refused; select supported options or flatten grayscale into the print-ready page content.
Without an explicit duplex request, the queue default applies. Normal color output retains
the driver's color policy. Single-page or single-copy jobs have no collating order to check.

`cargo test -p craft-print` tests CUPS transport (including pipe-sized jobs and refusal),
printer parsing, PPD options and argument mapping, and on every platform the Windows pieces that
need no printer: the PrintTicket, the checks on the driver's validated ticket and capabilities,
paper matching, job file names, and the PDF sheet reader and its update (classic and compressed
cross-references, turning, reframing, refusal, and thousands of truncated and corrupted files
that must fail without a panic). On Windows it also submits a mixed-orientation exact-size job
and a shrink-to-fit job to a temporary paused queue (inbox "Generic / Text Only" driver on
`LPT1:`) and checks each arrives as one job with all its sheets. The queue portion explicitly
skips if the driver or administrative rights are absent. Windows CI installs the inbox test
driver and sets `CRAFT_PRINT_REQUIRE_QUEUE=1`, making queue preparation/submission mandatory on
its runner. Two ignored tests check a real printer by hand: `an_installed_driver_validates_the_ticket`
(`CRAFT_PRINT_TEST_PRINTER`; sends no job) and `an_installed_printer_prints_a_file`
(`CRAFT_PRINT_TEST_PRINTER`, `CRAFT_PRINT_TEST_FILE`, optional `CRAFT_PRINT_TEST_SCALING=shrink`
and `CRAFT_PRINT_TEST_DUPLEX=long`). Physical printer fidelity remains a hardware check.

The cross-platform GitHub Actions workflow is defined in `.github/workflows/print.yml`.
It runs formatting, Clippy and this crate's own tests on Windows, Linux and macOS when print
code changes, and supports manual runs. The Windows job executes the spooler regression above;
physical printer fidelity still requires hardware verification.

Windows discovery reports driver duplex support in `Printer::two_sided` (a two-sided option in
the driver's PrintCapabilities); CUPS leaves it unknown. Explicit simplex/duplex choices that the driver
drops are refused before submission.

Windows dependencies: the maintained [`winprint` fork](https://github.com/storytold/winprint.rs)
(BSD-3-Clause, pinned to an immutable Git revision, its PDFium-downloading `pdfium` feature off) and `winsafe` (MIT) hold every Win32 and WinRT call, so the crate keeps `unsafe` forbidden;
`roxmltree` reads PrintTickets and `miniz_oxide` inflates PDF streams. `winprint` 0.2.1 lists
reqwest (with rustls) and sigstore crates as non-optional build-dependencies, used only by its
`pdfium` feature: they compile into its build script on Windows but not into the library. Apps
running `cargo deny` need a licence exception for `webpki-root-certs` (CDLA-Permissive-2.0, CA
list data reqwest brings) and an advisory ignore for RUSTSEC-2024-0370 (`proc-macro-error`
unmaintained, a compile-time proc macro of winprint's `fmt-derive`).

`craft_print::rgb` exposes the safe `craft-winprint-gdi` API for native driver settings,
printable geometry, RGB pages and caller-composed vector strokes. Applications retain ICC
conversion, document layout, labels and print-mark composition. The shared transport validates
single-copy DEVMODE settings before emitting manual copies and checks GDI errors. Color policy
is explicit: application-managed/unmanaged RGB disables DC and old-style halftone correction
with `ICM_DONE_OUTSIDEDC`, reapplied after `StartPage`; driver-managed RGB uses `ICM_OFF`.
Vendor driver automatic color correction remains a driver setting and requires qualification.
Other platforms return `rgb::PrintError::Unsupported`. The Windows PDF backend and its existing
completion dependency pin are independent of this additional MIT OR Apache-2.0 API.
