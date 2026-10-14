//! Network-printer destination: the address a person typed, what the printer said it supports, a
//! per-job confirmation and the state of one job. It sits beside the spooler-queue controls in the
//! parent module and shares nothing with them, so existing users are unaffected.
//!
//! The types here are deliberately light and backend-free: the host maps its real client's answers
//! into [`PrinterSummary`], [`Report`] and [`Failure`], supplies [`QueryPrinter`] and [`SendJob`]
//! workers, and words everything through [`Labels`]. No network work happens while drawing.
//!
//! Rules the model keeps:
//! - the options offered are the ones the printer lists, nothing else;
//! - the confirmation is never on by default and never remembered: it clears when the address
//!   changes, when the printer is queried again, when a job starts and when the dialog opens;
//! - a job can start only for an address that was queried, with the confirmation ticked.
//!
//! A job has two phases and the model shows both. The host's [`SendJob`] worker sends the document
//! (`JobView::Running`), reports **accepted** (`JobUpdate::Accepted`, `JobView::Accepted`: the printer
//! has the document; nothing says it has printed), may report what the printer says
//! (`JobUpdate::Status`), and finally answers with the result. A printer that cannot report ends with
//! [`Outcome::AcceptedUnknown`]: sent, outcome unknown. All of it arrives over `mpsc` receivers that
//! [`State::poll`] drains once per frame, so nothing blocks the UI thread.

use std::sync::mpsc::{Receiver, TryRecvError};

/// Duplex setting. [`Sides::token`] is the standard attribute value a printer lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sides {
    OneSided,
    LongEdge,
    ShortEdge,
}

impl Sides {
    pub const ALL: [Sides; 3] = [Sides::OneSided, Sides::LongEdge, Sides::ShortEdge];

    pub const fn token(self) -> &'static str {
        match self {
            Self::OneSided => "OneSided",
            Self::LongEdge => "TwoSidedLongEdge",
            Self::ShortEdge => "TwoSidedShortEdge",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Orientation {
    Portrait,
    Landscape,
    ReversePortrait,
    ReverseLandscape,
}

impl Orientation {
    pub const ALL: [Orientation; 4] = [Self::Portrait, Self::Landscape, Self::ReversePortrait, Self::ReverseLandscape];

    pub const fn token(self) -> &'static str {
        match self {
            Self::Portrait => "Portrait",
            Self::Landscape => "Landscape",
            Self::ReversePortrait => "ReversePortrait",
            Self::ReverseLandscape => "ReverseLandscape",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    Draft,
    Normal,
    High,
    Photo,
}

impl Quality {
    pub const ALL: [Quality; 4] = [Self::Draft, Self::Normal, Self::High, Self::Photo];

    pub const fn token(self) -> &'static str {
        match self {
            Self::Draft => "Draft",
            Self::Normal => "Normal",
            Self::High => "High",
            Self::Photo => "Photo",
        }
    }
}

/// What a printer lists it accepts, as tokens. An empty list means it did not say, and then
/// nothing of that kind is offered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Caps {
    pub formats: Vec<String>,
    pub sides: Vec<String>,
    pub orientations: Vec<String>,
    pub qualities: Vec<String>,
    pub media_sizes: Vec<String>,
    /// Allowed copies (min, max).
    pub copies: Option<(u32, u32)>,
}

/// What a query learned about a printer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrinterSummary {
    pub name: String,
    pub color: Option<bool>,
    pub state: String,
    pub queued_jobs: Option<u32>,
    pub reasons: Vec<String>,
    pub caps: Caps,
}

impl PrinterSummary {
    fn lists(&self, format: &str) -> bool {
        self.caps.formats.iter().any(|f| f.eq_ignore_ascii_case(format))
    }

    /// Does the printer take any of `formats` (media types the host can produce)? A printer that
    /// lists no formats is left to answer for itself.
    pub fn takes_any(&self, formats: &[&str]) -> bool {
        self.caps.formats.is_empty() || formats.iter().any(|f| self.lists(f))
    }
}

/// Which kind of failure happened; the app words the heading, [`Failure::detail`] is the client's
/// own text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// The address was refused by the host's network policy.
    Policy,
    /// The printer could not be reached.
    Transport,
    /// The printer's reply could not be understood.
    Protocol,
    /// The printer refused the request.
    Fault,
    /// The printer accepts none of the formats the document can be sent in.
    NoCommonFormat,
    /// The printer does not support a requested setting.
    Unsupported,
    /// The document could not be prepared.
    Encode,
    /// A setting is not valid.
    Invalid,
    /// The worker ended without an answer.
    Stopped,
    Other,
}

impl FailureKind {
    /// A tolerant mapping from a client's error-kind names; unknown names become `Other`.
    pub fn from_name(name: &str) -> FailureKind {
        match name {
            "policy" => Self::Policy,
            "transport" | "connection_closed" | "http" => Self::Transport,
            "xml" | "protocol" | "too_large" => Self::Protocol,
            "fault" => Self::Fault,
            "no_common_format" => Self::NoCommonFormat,
            "unsupported" => Self::Unsupported,
            "encode" => Self::Encode,
            "invalid" => Self::Invalid,
            "stopped" => Self::Stopped,
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub kind: FailureKind,
    pub detail: String,
    /// The request may have reached the printer (the connection dropped after the document went
    /// out): sending again could print twice. The UI says so; see [`Labels::outcome_unknown_warning`].
    pub outcome_unknown: bool,
}

impl Failure {
    pub fn new(kind: FailureKind, detail: impl Into<String>) -> Self {
        Failure { kind, detail: detail.into(), outcome_unknown: false }
    }

    /// Mark the failure as one that may have been delivered.
    #[must_use]
    pub fn with_outcome_unknown(mut self, outcome_unknown: bool) -> Self {
        self.outcome_unknown = outcome_unknown;
        self
    }
}

/// How a job ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    CompletedWithWarnings,
    CompletedWithErrors,
    Canceled,
    Aborted,
    Forgotten,
    StillRunning,
    /// The printer has the document and cannot say more: sent, outcome unknown. Not a failure and not
    /// a claim that it printed.
    AcceptedUnknown,
}

impl Outcome {
    /// A tolerant mapping from a client's outcome names; unknown names become `Aborted`.
    pub fn from_name(name: &str) -> Outcome {
        match name {
            "completed" => Outcome::Completed,
            "completed_with_warnings" => Outcome::CompletedWithWarnings,
            "completed_with_errors" => Outcome::CompletedWithErrors,
            "canceled" => Outcome::Canceled,
            "forgotten" => Outcome::Forgotten,
            "still_running" => Outcome::StillRunning,
            "accepted_unknown" | "no_status" => Outcome::AcceptedUnknown,
            _ => Outcome::Aborted,
        }
    }

    pub fn is_success(self) -> bool {
        matches!(self, Outcome::Completed | Outcome::CompletedWithWarnings)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub outcome: Outcome,
    pub printer: String,
    pub sheets: u64,
    /// The printer's reasons and the client's warnings, as received.
    pub notes: Vec<String>,
}

/// How far a job the printer accepted has got, as far as the printer says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stage {
    /// Accepted; nothing more is known (yet, or ever).
    #[default]
    Accepted,
    /// Queued behind other work.
    Waiting,
    Held,
    Printing,
    /// Paused and needing attention; the reasons are in [`Progress::notes`].
    Stopped,
}

impl Stage {
    /// A tolerant mapping from a client's job-state names (`pending`, `held`, `processing`,
    /// `stopped`); anything else, `unknown` included, is [`Stage::Accepted`].
    pub fn from_name(name: &str) -> Stage {
        match name {
            "pending" => Stage::Waiting,
            "held" => Stage::Held,
            "processing" => Stage::Printing,
            "stopped" => Stage::Stopped,
            _ => Stage::Accepted,
        }
    }
}

/// What the printer says about an accepted job.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub stage: Stage,
    pub sheets_done: Option<u32>,
    pub sheets_total: Option<u32>,
    /// The printer's reasons, as received.
    pub notes: Vec<String>,
}

/// What a [`SendJob`] worker reports before its result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobUpdate {
    /// The printer has accepted the document. Sending is over; nothing says it has printed.
    Accepted(Progress),
    /// A newer status of the accepted job.
    Status(Progress),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum JobView {
    #[default]
    Idle,
    /// The document is being sent; the printer has not accepted it yet.
    Running {
        canceling: bool,
    },
    /// The printer has the document (see [`Stage`]); the worker is still watching it.
    Accepted {
        canceling: bool,
        progress: Progress,
    },
    Finished(Report),
    Failed(Failure),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Query {
    #[default]
    Idle,
    Running,
    Done(Box<PrinterSummary>),
    Failed(Failure),
}

/// What the person asked for; the host turns it into its client's print options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Request {
    pub copies: Option<u32>,
    pub sides: Option<Sides>,
    pub orientation: Option<Orientation>,
    pub quality: Option<Quality>,
    pub media_size: Option<String>,
}

/// Why Print is not available yet (nothing is worded for a missing tick: the box is in plain view).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Block {
    Address,
    NotQueried,
    Format,
    NoPages,
    Unconfirmed,
    Busy,
}

/// `192.0.2.50:5357/service` is read as `http://192.0.2.50:5357/service`; anything with another
/// scheme is left alone for the host's validator to accept or refuse.
pub fn normalize_url(text: &str) -> String {
    let t = text.trim();
    if t.is_empty() || t.contains("://") { t.to_owned() } else { format!("http://{t}") }
}

/// The pure model of the destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Draft {
    /// The address as typed.
    pub url: String,
    pub query: Query,
    /// The (normalised) address `query` was made for.
    queried: String,
    /// Bumped by every query so an answer that arrives late is dropped.
    generation: u64,
    pub copies: u32,
    pub sides: Option<Sides>,
    pub orientation: Option<Orientation>,
    pub quality: Option<Quality>,
    /// The person's yes to this one print. Never on by default, never kept.
    pub confirmed: bool,
    pub job: JobView,
}

impl Default for Draft {
    fn default() -> Self {
        Draft {
            url: String::new(),
            query: Query::Idle,
            queried: String::new(),
            generation: 0,
            copies: 1,
            sides: None,
            orientation: None,
            quality: None,
            confirmed: false,
            job: JobView::Idle,
        }
    }
}

impl Draft {
    /// A draft for the printer at `url` (nothing queried yet).
    pub fn for_address(url: &str) -> Self {
        Draft { url: url.to_owned(), ..Default::default() }
    }

    pub fn normalized_url(&self) -> String {
        normalize_url(&self.url)
    }

    /// A job is in flight: being sent, or accepted and still watched.
    pub fn running(&self) -> bool {
        matches!(self.job, JobView::Running { .. } | JobView::Accepted { .. })
    }

    pub fn querying(&self) -> bool {
        self.query == Query::Running
    }

    /// The printer's answer, when it is for the address now typed.
    pub fn printer(&self) -> Option<&PrinterSummary> {
        match &self.query {
            Query::Done(p) if self.queried == self.normalized_url() => Some(p),
            _ => None,
        }
    }

    /// Call after the address text changed: what was learned about another printer goes, and so
    /// does the confirmation.
    pub fn address_edited(&mut self) {
        self.confirmed = false;
        if self.queried != self.normalized_url() {
            if self.query != Query::Running {
                self.query = Query::Idle;
            }
            self.clear_choices();
        }
    }

    fn clear_choices(&mut self) {
        self.copies = 1;
        self.sides = None;
        self.orientation = None;
        self.quality = None;
    }

    /// The dialog opens: a confirmation from before is void, and so is a finished job's report.
    pub fn dialog_opened(&mut self) {
        self.confirmed = false;
        if !self.running() {
            self.job = JobView::Idle;
        }
    }

    pub fn offered_sides(&self) -> Vec<Sides> {
        self.printer().map(|p| Sides::ALL.into_iter().filter(|s| p.caps.sides.iter().any(|t| t == s.token())).collect()).unwrap_or_default()
    }

    pub fn offered_orientations(&self) -> Vec<Orientation> {
        self.printer()
            .map(|p| Orientation::ALL.into_iter().filter(|o| p.caps.orientations.iter().any(|t| t == o.token())).collect())
            .unwrap_or_default()
    }

    pub fn offered_qualities(&self) -> Vec<Quality> {
        self.printer().map(|p| Quality::ALL.into_iter().filter(|q| p.caps.qualities.iter().any(|t| t == q.token())).collect()).unwrap_or_default()
    }

    /// The most copies the printer takes (1 when it did not say).
    pub fn max_copies(&self) -> u32 {
        self.printer().and_then(|p| p.caps.copies).map_or(1, |(_, max)| max.clamp(1, 9999))
    }

    /// Start a query: returns the generation its answer must carry. Clears the confirmation.
    pub fn begin_query(&mut self) -> u64 {
        self.generation += 1;
        self.confirmed = false;
        self.query = Query::Running;
        self.generation
    }

    /// File a query's answer; one for an older query, or for an address no longer typed, is dropped.
    pub fn finish_query(&mut self, generation: u64, url: &str, result: Result<PrinterSummary, Failure>) {
        if generation != self.generation || url != self.normalized_url() {
            return;
        }
        match result {
            Ok(summary) => {
                self.queried = url.to_owned();
                self.query = Query::Done(Box::new(summary));
                self.keep_only_offered();
            }
            Err(f) => {
                self.queried.clear();
                self.query = Query::Failed(f);
                self.clear_choices();
            }
        }
    }

    /// Drop choices the (new) printer does not list, and bring copies within its limit.
    pub fn keep_only_offered(&mut self) {
        if self.sides.is_some_and(|s| !self.offered_sides().contains(&s)) {
            self.sides = None;
        }
        if self.orientation.is_some_and(|o| !self.offered_orientations().contains(&o)) {
            self.orientation = None;
        }
        if self.quality.is_some_and(|q| !self.offered_qualities().contains(&q)) {
            self.quality = None;
        }
        self.copies = self.copies.clamp(1, self.max_copies());
    }

    /// What stops a print now. `address_ok`: the address passes the host's policy. `sheets`: the
    /// app's own settings give something to print. `formats`: media types the host can produce.
    pub fn block(&self, address_ok: bool, sheets: bool, formats: &[&str]) -> Option<Block> {
        if self.running() || self.querying() {
            return Some(Block::Busy);
        }
        if !address_ok {
            return Some(Block::Address);
        }
        let Some(printer) = self.printer() else { return Some(Block::NotQueried) };
        if !printer.takes_any(formats) {
            return Some(Block::Format);
        }
        if !sheets {
            return Some(Block::NoPages);
        }
        if !self.confirmed {
            return Some(Block::Unconfirmed);
        }
        None
    }

    /// The settings to send, limited to what the printer lists. `media` is the media-size token
    /// the app wants; it is dropped unless the printer lists it.
    pub fn request(&self, media: Option<&str>) -> Request {
        let printer = self.printer();
        let media_size = media.filter(|m| printer.is_some_and(|p| p.caps.media_sizes.iter().any(|s| s == m))).map(str::to_owned);
        Request {
            copies: (self.copies > 1).then_some(self.copies.min(self.max_copies())),
            sides: self.sides.filter(|s| self.offered_sides().contains(s)),
            orientation: self.orientation.filter(|o| self.offered_orientations().contains(o)),
            quality: self.quality.filter(|q| self.offered_qualities().contains(q)),
            media_size,
        }
    }

    /// A job starts: the confirmation is spent.
    pub fn job_started(&mut self) {
        self.confirmed = false;
        self.job = JobView::Running { canceling: false };
    }

    /// Cancel was pressed; returns `true` when a running job should be told.
    pub fn job_canceling(&mut self) -> bool {
        match &mut self.job {
            JobView::Running { canceling } | JobView::Accepted { canceling, .. } if !*canceling => {
                *canceling = true;
                true
            }
            _ => false,
        }
    }

    /// The printer accepted the document. Ignored unless a job is being sent; a cancel already
    /// pressed is kept.
    pub fn job_accepted(&mut self, progress: Progress) {
        if let JobView::Running { canceling } = self.job {
            self.job = JobView::Accepted { canceling, progress };
        }
    }

    /// A newer status of the accepted job. A status that arrives before the acceptance is taken as
    /// the acceptance; one that arrives after the job ended is dropped.
    pub fn job_progress(&mut self, progress: Progress) {
        match &mut self.job {
            JobView::Accepted { progress: shown, .. } => *shown = progress,
            JobView::Running { canceling } => self.job = JobView::Accepted { canceling: *canceling, progress },
            _ => {}
        }
    }

    pub fn job_finished(&mut self, view: JobView) {
        self.job = view;
    }
}

pub type QueryReceiver = Receiver<Result<PrinterSummary, Failure>>;

/// Ask the printer at the (normalised) address what it supports.
pub type QueryPrinter = Box<dyn Fn(&str) -> QueryReceiver>;

/// A running job: its result, optionally what happens on the way, and a way to ask the host to stop
/// it. Build it with [`JobHandle::new`] (and [`JobHandle::with_updates`]).
///
/// The worker sends [`JobUpdate::Accepted`] when the printer has the document, [`JobUpdate::Status`]
/// as it learns more, and last the result on `result`; for a printer that cannot report it sends
/// `Accepted` and then `Ok(Report { outcome: Outcome::AcceptedUnknown, .. })`. A worker that sends no
/// updates behaves as before. A worker that stops after `Accepted` without a result is shown as
/// sent, outcome unknown, not as a failure.
#[non_exhaustive]
pub struct JobHandle {
    pub result: Receiver<Result<Report, Failure>>,
    pub updates: Option<Receiver<JobUpdate>>,
    pub cancel: Box<dyn Fn()>,
}

impl JobHandle {
    /// A handle with a result and a cancel, and no updates.
    pub fn new(result: Receiver<Result<Report, Failure>>, cancel: Box<dyn Fn()>) -> Self {
        JobHandle { result, updates: None, cancel }
    }

    /// Also deliver what happens between acceptance and the result.
    #[must_use]
    pub fn with_updates(mut self, updates: Receiver<JobUpdate>) -> Self {
        self.updates = Some(updates);
        self
    }
}

/// Send the app's document to the (normalised) address with the person's settings.
pub type SendJob = Box<dyn Fn(&str, Request) -> JobHandle>;

/// Validate a normalised address against the host's network policy. `Err` is shown inline.
pub type Validate = dyn Fn(&str) -> Result<(), Failure>;

/// App-localised wording. Strings documented with `{state}`, `{reasons}`, `{printer}` or `{n}`
/// are templates; identity never depends on the text.
pub struct Labels<'a> {
    pub address: &'a str,
    /// Example text shown in an empty address field.
    pub address_hint: &'a str,
    /// A note under the field saying which addresses are accepted; empty to omit.
    pub address_note: &'a str,
    pub query_printer: &'a str,
    pub querying: &'a str,
    pub query_first: &'a str,
    /// Template with `{state}`.
    pub printer_status: &'a str,
    /// Template with `{reasons}`.
    pub reasons: &'a str,
    pub no_common_format: &'a str,
    pub copies: &'a str,
    pub sides: &'a str,
    pub sides_off: &'a str,
    pub sides_long_edge: &'a str,
    pub sides_short_edge: &'a str,
    pub orientation: &'a str,
    pub portrait: &'a str,
    pub landscape: &'a str,
    pub reverse_portrait: &'a str,
    pub reverse_landscape: &'a str,
    pub quality: &'a str,
    pub draft: &'a str,
    pub normal: &'a str,
    pub high: &'a str,
    pub photo: &'a str,
    /// The printer's own default.
    pub auto: &'a str,
    /// The wording of the per-job confirmation belongs to the app.
    pub confirm: &'a str,
    /// Template with `{printer}`.
    pub sending: &'a str,
    pub canceling: &'a str,
    pub cancel: &'a str,
    /// Template with `{printer}`.
    pub sent: &'a str,
    /// Template with `{printer}`: the printer has the document and nothing more is known yet.
    pub accepted: &'a str,
    pub stage_waiting: &'a str,
    pub stage_held: &'a str,
    pub stage_printing: &'a str,
    pub stage_stopped: &'a str,
    /// Template with `{n}`.
    pub sheets: &'a str,
    /// Template with `{done}` and `{total}`.
    pub sheets_progress: &'a str,
    pub outcome_warnings: &'a str,
    pub outcome_errors: &'a str,
    pub outcome_canceled: &'a str,
    pub outcome_aborted: &'a str,
    pub outcome_forgotten: &'a str,
    pub outcome_still_running: &'a str,
    /// Sent, and the printer cannot say what became of it.
    pub outcome_accepted_unknown: &'a str,
    /// Shown under a failure whose request may have reached the printer: check the printer before
    /// sending again.
    pub outcome_unknown_warning: &'a str,
    pub failure_policy: &'a str,
    pub failure_transport: &'a str,
    pub failure_protocol: &'a str,
    pub failure_fault: &'a str,
    pub failure_no_common_format: &'a str,
    pub failure_unsupported: &'a str,
    pub failure_encode: &'a str,
    pub failure_invalid: &'a str,
    pub failure_stopped: &'a str,
    pub failure_other: &'a str,
}

impl Labels<'_> {
    pub fn failure_heading(&self, kind: FailureKind) -> &str {
        match kind {
            FailureKind::Policy => self.failure_policy,
            FailureKind::Transport => self.failure_transport,
            FailureKind::Protocol => self.failure_protocol,
            FailureKind::Fault => self.failure_fault,
            FailureKind::NoCommonFormat => self.failure_no_common_format,
            FailureKind::Unsupported => self.failure_unsupported,
            FailureKind::Encode => self.failure_encode,
            FailureKind::Invalid => self.failure_invalid,
            FailureKind::Stopped => self.failure_stopped,
            FailureKind::Other => self.failure_other,
        }
    }

    /// The sentence for an ending; `None` for a plain success (the "sent" notice says it).
    pub fn outcome_text(&self, outcome: Outcome) -> Option<&str> {
        match outcome {
            Outcome::Completed => None,
            Outcome::CompletedWithWarnings => Some(self.outcome_warnings),
            Outcome::CompletedWithErrors => Some(self.outcome_errors),
            Outcome::Canceled => Some(self.outcome_canceled),
            Outcome::Aborted => Some(self.outcome_aborted),
            Outcome::Forgotten => Some(self.outcome_forgotten),
            Outcome::StillRunning => Some(self.outcome_still_running),
            Outcome::AcceptedUnknown => Some(self.outcome_accepted_unknown),
        }
    }

    /// The sentence for a stage of an accepted job; `{printer}` is filled for [`Stage::Accepted`].
    pub fn stage_text(&self, stage: Stage, printer: &str) -> String {
        match stage {
            Stage::Accepted => fill(self.accepted, "printer", printer),
            Stage::Waiting => self.stage_waiting.to_owned(),
            Stage::Held => self.stage_held.to_owned(),
            Stage::Printing => self.stage_printing.to_owned(),
            Stage::Stopped => self.stage_stopped.to_owned(),
        }
    }

    fn sides_label(&self, sides: Sides) -> &str {
        match sides {
            Sides::OneSided => self.sides_off,
            Sides::LongEdge => self.sides_long_edge,
            Sides::ShortEdge => self.sides_short_edge,
        }
    }

    fn orientation_label(&self, orientation: Orientation) -> &str {
        match orientation {
            Orientation::Portrait => self.portrait,
            Orientation::Landscape => self.landscape,
            Orientation::ReversePortrait => self.reverse_portrait,
            Orientation::ReverseLandscape => self.reverse_landscape,
        }
    }

    fn quality_label(&self, quality: Quality) -> &str {
        match quality {
            Quality::Draft => self.draft,
            Quality::Normal => self.normal,
            Quality::High => self.high,
            Quality::Photo => self.photo,
        }
    }
}

fn fill(template: &str, key: &str, value: &str) -> String {
    template.replace(&format!("{{{key}}}"), value)
}

/// What a frame of [`State::show`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Response {
    /// Why Print is unavailable now (`None` means it may start), for the app's Print button.
    pub block: Option<Block>,
    /// An option, the address or the confirmation changed.
    pub changed: bool,
    /// Query printer was clicked and a query was started.
    pub query_started: bool,
    /// Cancel was clicked and the host was told.
    pub cancel_requested: bool,
}

/// The destination's state: the model plus the workers it is waiting for.
#[derive(Default)]
pub struct State {
    pub draft: Draft,
    query_rx: Option<(u64, String, QueryReceiver)>,
    job: Option<JobHandle>,
}

impl State {
    pub fn new(draft: Draft) -> Self {
        State { draft, ..Default::default() }
    }

    /// Start a query. False when the address is not acceptable, a query or job is active; no
    /// worker is started then.
    pub fn start_query(&mut self, query: &QueryPrinter, address_ok: bool) -> bool {
        if !address_ok || self.draft.querying() || self.draft.running() {
            return false;
        }
        let url = self.draft.normalized_url();
        let generation = self.draft.begin_query();
        self.query_rx = Some((generation, url.clone(), query(&url)));
        true
    }

    /// Start a job when nothing blocks it ([`Draft::block`]); the confirmation is spent. `media`
    /// is the media-size token the app wants (used only when the printer lists it).
    pub fn start_job(&mut self, send: &SendJob, address_ok: bool, sheets: bool, formats: &[&str], media: Option<&str>) -> bool {
        if self.draft.block(address_ok, sheets, formats).is_some() {
            return false;
        }
        let request = self.draft.request(media);
        let handle = send(&self.draft.normalized_url(), request);
        self.draft.job_started();
        self.job = Some(handle);
        true
    }

    /// Ask the host to stop the running job, once. True when it was told.
    pub fn cancel_job(&mut self) -> bool {
        if !self.draft.job_canceling() {
            return false;
        }
        if let Some(job) = &self.job {
            (job.cancel)();
        }
        true
    }

    /// The dialog opens (see [`Draft::dialog_opened`]).
    pub fn dialog_opened(&mut self) {
        self.draft.dialog_opened();
    }

    /// Collect finished work. Requests occasional repaints while work is pending.
    pub fn poll(&mut self, ctx: &egui::Context) {
        if let Some((generation, url, receiver)) = &self.query_rx {
            let (generation, url) = (*generation, url.clone());
            let result = match receiver.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Disconnected) => Some(Err(Failure::new(FailureKind::Stopped, ""))),
                Err(TryRecvError::Empty) => None,
            };
            if let Some(result) = result {
                self.query_rx = None;
                self.draft.finish_query(generation, &url, result);
            }
        }
        if let Some(job) = &self.job {
            // What happened on the way first, in order, then the result; one frame drains them all.
            if let Some(updates) = &job.updates {
                while let Ok(update) = updates.try_recv() {
                    match update {
                        JobUpdate::Accepted(progress) => self.draft.job_accepted(progress),
                        JobUpdate::Status(progress) => self.draft.job_progress(progress),
                    }
                }
            }
            let view = match job.result.try_recv() {
                Ok(Ok(report)) => Some(JobView::Finished(report)),
                Ok(Err(failure)) => Some(JobView::Failed(failure)),
                // A worker that ended after the printer accepted the document did not fail: it sent.
                Err(TryRecvError::Disconnected) => Some(match &self.draft.job {
                    JobView::Accepted { .. } => JobView::Finished(Report {
                        outcome: Outcome::AcceptedUnknown,
                        printer: self.draft.printer().map(|p| p.name.clone()).unwrap_or_default(),
                        sheets: 0,
                        notes: Vec::new(),
                    }),
                    _ => JobView::Failed(Failure::new(FailureKind::Stopped, "")),
                }),
                Err(TryRecvError::Empty) => None,
            };
            if let Some(view) = view {
                self.job = None;
                self.draft.job_finished(view);
            }
        }
        if self.query_rx.is_some() || self.job.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    /// Draw the section: address, Query button, printer facts, listed options, the confirmation
    /// and the job's progress. The Print button stays the app's: enable it when
    /// [`Response::block`] is `None`, then call [`State::start_job`].
    pub fn show(&mut self, ui: &mut egui::Ui, id: egui::Id, host: &Host<'_>, labels: &Labels<'_>) -> Response {
        self.poll(ui.ctx());
        ui.push_id(id, |ui| self.draw(ui, host, labels)).inner
    }

    fn draw(&mut self, ui: &mut egui::Ui, host: &Host<'_>, labels: &Labels<'_>) -> Response {
        let mut response = Response::default();
        let busy = self.draft.running();
        let checked = (host.validate)(&self.draft.normalized_url());
        let address_ok = checked.is_ok();
        ui.horizontal_wrapped(|ui| {
            let label = ui.label(labels.address);
            let edit = ui
                .add_enabled(!busy, egui::TextEdit::singleline(&mut self.draft.url).hint_text(labels.address_hint).desired_width(240.0))
                .labelled_by(label.id);
            if edit.changed() {
                self.draft.address_edited();
                response.changed = true;
            }
            let can_query = address_ok && !busy && !self.draft.querying();
            if ui.add_enabled(can_query, egui::Button::new(labels.query_printer)).clicked() {
                response.query_started = self.start_query(host.query, address_ok);
            }
        });
        match (&checked, self.draft.url.trim().is_empty()) {
            (Err(failure), false) => failure_lines(ui, labels, failure),
            _ if !labels.address_note.is_empty() => {
                ui.label(egui::RichText::new(labels.address_note).small().weak());
            }
            _ => {}
        }
        if self.draft.querying() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(labels.querying);
            });
        }
        if let Query::Failed(failure) = &self.draft.query {
            failure_lines(ui, labels, failure);
        }
        response.changed |= self.printer_and_options(ui, labels, address_ok, host.formats);
        // The yes to this one print: off every time the dialog opens, the address changes or a job starts.
        let can_confirm = self.draft.printer().is_some() && !busy;
        if ui.add_enabled(can_confirm, egui::Checkbox::new(&mut self.draft.confirmed, labels.confirm)).changed() {
            response.changed = true;
        }
        response.block = self.draft.block(address_ok, host.sheets_ok, host.formats);
        response.cancel_requested = self.job_lines(ui, labels);
        response
    }

    fn printer_and_options(&mut self, ui: &mut egui::Ui, labels: &Labels<'_>, address_ok: bool, formats: &[&str]) -> bool {
        let Some(printer) = self.draft.printer().cloned() else {
            if self.draft.query == Query::Idle && address_ok {
                ui.label(egui::RichText::new(labels.query_first).small().weak());
            }
            return false;
        };
        let mut changed = false;
        ui.add_space(2.0);
        ui.label(egui::RichText::new(&printer.name).strong());
        ui.label(egui::RichText::new(fill(labels.printer_status, "state", &printer.state)).small().weak());
        if !printer.reasons.is_empty() {
            ui.label(egui::RichText::new(fill(labels.reasons, "reasons", &printer.reasons.join(", "))).small().weak());
        }
        if !printer.takes_any(formats) {
            ui.label(labels.no_common_format);
            return false;
        }
        let draft = &mut self.draft;
        let sides = draft.offered_sides();
        let orientations = draft.offered_orientations();
        let qualities = draft.offered_qualities();
        let max = draft.max_copies();
        ui.add_enabled_ui(!draft.running(), |ui| {
            egui::Grid::new("options").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                if max > 1 {
                    let label = ui.label(labels.copies);
                    changed |= ui.add(egui::DragValue::new(&mut draft.copies).range(1..=max)).labelled_by(label.id).changed();
                    ui.end_row();
                }
                if !sides.is_empty() {
                    let label = ui.label(labels.sides);
                    changed |= choice(ui, "sides", label.id, labels.auto, &mut draft.sides, &sides, |s| labels.sides_label(s));
                    ui.end_row();
                }
                if !orientations.is_empty() {
                    let label = ui.label(labels.orientation);
                    changed |=
                        choice(ui, "orientation", label.id, labels.auto, &mut draft.orientation, &orientations, |o| labels.orientation_label(o));
                    ui.end_row();
                }
                if !qualities.is_empty() {
                    let label = ui.label(labels.quality);
                    changed |= choice(ui, "quality", label.id, labels.auto, &mut draft.quality, &qualities, |q| labels.quality_label(q));
                    ui.end_row();
                }
            });
        });
        changed
    }

    /// Progress with Cancel while a job runs, then how it ended. True when Cancel told the host.
    fn job_lines(&mut self, ui: &mut egui::Ui, labels: &Labels<'_>) -> bool {
        match self.draft.job.clone() {
            JobView::Idle => false,
            JobView::Running { canceling } => {
                let mut cancel = false;
                ui.horizontal(|ui| {
                    ui.spinner();
                    let printer = self.draft.printer().map_or_else(|| self.draft.url.trim().to_owned(), |p| p.name.clone());
                    ui.label(if canceling { labels.canceling.to_owned() } else { fill(labels.sending, "printer", &printer) });
                    if ui.add_enabled(!canceling, egui::Button::new(labels.cancel)).clicked() {
                        cancel = self.cancel_job();
                    }
                });
                cancel
            }
            JobView::Accepted { canceling, progress } => {
                let mut cancel = false;
                let printer = self.draft.printer().map_or_else(|| self.draft.url.trim().to_owned(), |p| p.name.clone());
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(if canceling { labels.canceling.to_owned() } else { labels.stage_text(progress.stage, &printer) });
                    if ui.add_enabled(!canceling, egui::Button::new(labels.cancel)).clicked() {
                        cancel = self.cancel_job();
                    }
                });
                match (progress.sheets_done, progress.sheets_total) {
                    (Some(done), Some(total)) if total > 0 => {
                        let text = fill(&fill(labels.sheets_progress, "done", &done.to_string()), "total", &total.to_string());
                        ui.add(egui::ProgressBar::new(done.min(total) as f32 / total as f32).text(text));
                    }
                    (Some(done), _) if done > 0 => {
                        ui.label(egui::RichText::new(fill(labels.sheets, "n", &done.to_string())).small().weak());
                    }
                    _ => {}
                }
                for note in &progress.notes {
                    ui.label(egui::RichText::new(note).small().weak());
                }
                cancel
            }
            JobView::Finished(report) => {
                let text = labels.outcome_text(report.outcome).map_or_else(|| fill(labels.sent, "printer", &report.printer), str::to_owned);
                ui.label(egui::RichText::new(text).strong());
                if report.sheets > 0 {
                    ui.label(egui::RichText::new(fill(labels.sheets, "n", &report.sheets.to_string())).small().weak());
                }
                for note in &report.notes {
                    ui.label(egui::RichText::new(note).small().weak());
                }
                false
            }
            JobView::Failed(failure) => {
                failure_lines(ui, labels, &failure);
                false
            }
        }
    }
}

/// What the host supplies to [`State::show`] besides wording.
pub struct Host<'a> {
    /// Checks the normalised address against the host's network policy; `Err` is shown inline.
    pub validate: &'a Validate,
    /// Asks a printer what it supports.
    pub query: &'a QueryPrinter,
    /// The app's own settings give something to print.
    pub sheets_ok: bool,
    /// Media types the app can produce; a printer listing none of them blocks Print.
    pub formats: &'a [&'a str],
}

fn failure_lines(ui: &mut egui::Ui, labels: &Labels<'_>, failure: &Failure) {
    ui.label(egui::RichText::new(labels.failure_heading(failure.kind)).strong());
    if !failure.detail.is_empty() {
        ui.label(egui::RichText::new(&failure.detail).small().weak());
    }
    if failure.outcome_unknown {
        ui.label(egui::RichText::new(labels.outcome_unknown_warning).small().strong());
    }
}

/// A choice among what the printer lists, with the printer's own default first. True on change.
fn choice<'a, T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    id: &str,
    label: egui::Id,
    auto: &'a str,
    value: &mut Option<T>,
    offered: &[T],
    name: impl Fn(T) -> &'a str,
) -> bool {
    let before = *value;
    let shown = value.map_or(auto, &name);
    egui::ComboBox::from_id_salt(id)
        .selected_text(shown)
        .width(180.0)
        .show_ui(ui, |ui| {
            ui.selectable_value(value, None, auto);
            for &o in offered {
                ui.selectable_value(value, Some(o), name(o));
            }
        })
        .response
        .labelled_by(label);
    *value != before
}

#[cfg(test)]
mod tests;
