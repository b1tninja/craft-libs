# Tracked print jobs: one object per print, stable across retries

`docs/job-lifecycle.md` describes what a *backend* offers once a printer has accepted a document:
`submit` (bounded), `job_status` (one request), `cancel_job`, and an error vocabulary that says
whether sending again is safe. That is enough to follow **one remote job**. A person, though, does
not print "a remote job": they press Print once and expect one sheet of paper, however many
connections, retries and printer-side restarts it takes. This note describes the object that holds
that single user-visible print, `PrintJob`, and the common base that lets it work the same way over
a spooler (CUPS, the Windows spooler), IPP and WS-Print.

Everything here is in memory and driven by the caller. There is no thread, no timer, no async
runtime and no persistence in this version.

## 1. The object

`PrintJob` is the lifecycle record of one print. It is created from the request the app would have
sent to `Registry::submit_job` (a `Job`, `JobOptions`, the consent) plus a `DocumentSource`, and it
is advanced by the caller:

```text
let mut job = PrintJob::new(job, BytesSource::new(DocumentFormat::Pdf, bytes)).with_consent(ok);
loop {
    let step = job.advance(&registry);          // at most ONE bounded request per call
    publish(job.snapshot());                    // a cheap, immutable view for the UI
    match step.next_in { Some(wait) => sleep_or_return_to_the_frame_loop(wait), None => break }
}
```

* `advance(&Registry)` makes **at most one** request to a backend (a submit, a status read, a
  cancel, a reconcile) and only when one is due; otherwise it says how long until one is
  (`Advance::next_in`). `None` means "nothing will happen on its own": the job ended, is waiting for
  a person (`NeedsAttention`) or is accepted on a backend that cannot report.
* Waiting is measured on an injectable `Clock` (the same trait `JobWatch` uses, `ManualClock` in
  tests). The wall clock used to stamp the reconcile window is injectable too.
* `snapshot()` clones a few small fields and two reference-counted lists; it never touches the
  network and never blocks, so a UI can call it every frame.
* The object has its own lifecycle, **separate from the printer's**. The printer's `JobState`
  describes one remote job; the lifecycle describes the print as a whole and can outlive several
  remote jobs.

### Lifecycle states

| State | Meaning | Leaves by |
|---|---|---|
| `Preparing` | created; printer, tag and backend not yet resolved | first `advance` |
| `Submitting` | the next `advance` sends the document (a retry also passes through here) | accepted, error |
| `Accepted` | the printer took the document; no status is being followed (the backend cannot report, the watch ended, or it has not been polled yet) | poll, `refresh`, end |
| `Watching(JobState)` | following the remote job; carries the printer's last reported state | the printer's end state |
| `RetryScheduled { attempt, next_at }` | a safe, transient failure; the next attempt is due at `next_at` (on the job's clock) | `advance` once due, `retry`, `cancel` |
| `NeedsAttention` | the outcome of a send is unknown (the request may have arrived). Nothing is resent. | `reconcile`, then `accept` or `retry`, or `cancel` |
| `Terminal(Completed)` | printed | none |
| `Terminal(CompletedWithWarnings)` | printed, with a printer warning, or the printer forgot the job so it most likely printed | none |
| `Terminal(Failed)` | refused, retries used up, a non-transient error, or completed with errors | manual `retry` |
| `Terminal(Cancelled)` | we cancelled it, or someone cancelled it at the printer | manual `retry` |
| `Terminal(Aborted)` | the printer gave up on it | manual `retry` |

A `Terminal` state is final for *this attempt chain*; `retry()` is the explicit act of starting a new
attempt, which keeps the history. `NeedsAttention` is deliberately a state of its own and not a kind of
failure: the print may be on its way.

For a UI the state is also reduced to `SimpleState` (Preparing, Sending, Queued, Held, Printing,
PrinterNeedsAttention, Retrying, NeedsAttention, Sent, Done, DoneWithWarnings, Failed, Cancelled).

## 2. What it remembers

* **A stable local id** (`LocalJobId`, a process-unique number) that never changes across retries.
  Remote `JobId`s come and go (one per accepted attempt); the local id is what a UI row binds to.
* **Attempts** (`Attempt`, bounded, oldest dropped): number, start and end on the job's clock, the
  `Phase` it reached, and an outcome: `Accepted(Option<JobId>)` (a spooler may give no handle) or
  `Error(AttemptError { kind, safety, retry_after, native, message })`, plus the remote job's last
  state when it ended (so "attempt 1 was aborted by the printer" survives the retry).
* **The request as sent** (`RequestRecord`): the `Job`, the `JobOptions`, the printer id as resolved,
  the tag, the document's length, format and a content fingerprint (64-bit FNV-1a: it detects "the
  file changed between attempts", it is not a cryptographic hash), and the backend's own description
  of how it was sent (`Submitted::effective`; for WS-Print the ticket shape). **Never the document
  bytes.** The bytes come from the `DocumentSource` the app supplies, read again for each attempt; a
  source that fails is an attempt that failed in `Phase::Prepare` with nothing sent.
* **A reconcile fingerprint** (`Fingerprint`): title, tag, the user the backend sends as, and the
  earliest creation time to accept (the first attempt's start minus a clock-skew allowance).
* **A bounded status trail** (`TrailEntry`): lifecycle changes, printer readings and notes, each with a
  timestamp on the job's clock. A printer reading identical to the previous one (`JobStatus::same_as`)
  is not recorded again, so a job that sits in `Processing` for ten minutes is one line.
* **Progress**: sheets and pages done, with totals when the printer reports them or the app expects
  them (`JobOptions::expected_pages` still flows through `ExpectedTotals`, a printer-reported total
  still wins), and the submission's own phase progress. Totals are optional; `fraction()` is `None`
  without one.
* **Warnings** from the backend (settings dropped) and from the job itself (the printer forgot the
  job; the document changed between attempts; tracking stopped).

## 3. What a UI sees

`Snapshot` (immutable): the local id, title, printer, `LifecycleState` and `SimpleState`, `settled`
(nothing more will happen without a person), progress, the printer's reasons, warnings, the last
error as a typed `AttemptError` (never a string to parse), the tag and whether it was applied, the
attempt timeline and the status trail, `RetryAdvice`, and optional `native` debug information (the
backend's description of the request, the remote handle, the last native error code). It carries no
document bytes and no protocol text.

`RetryAdvice` is the one question a UI has about retrying:

| Advice | When |
|---|---|
| `RetryNow` | the failure is known not to have printed (safe error, or a reconcile that found nothing on a backend that lists finished jobs) |
| `RetryAfter(duration)` | a retry is scheduled, or the device asked for a wait |
| `CheckPrinterFirst` | the outcome is unknown, the printer gave up (look at the paper path), or the backend cannot be asked (a resubmit might duplicate) |
| `DoNotRetry` | printed; the job is in flight; cancelled on purpose; or the error is not transient |

Actions: `cancel(&Registry)`, `retry()`, `reconcile(&Registry)`, `accept(&Registry, &JobId)` (adopt a
job reconcile found), `refresh(&Registry)` (read the status now, resuming a stopped watch) and
`cancel_token()` (a `CancelToken` an app thread can set while `advance` is blocked in a submit; the
backend checks it at its own checkpoints).

## 4. Retry rules

1. **Automatic retry only for `RetrySafety::Safe` errors that are also `is_retryable()`**, under a
   `RetryPolicy` that is on by default: at most **3 attempts** in a row, exponential backoff with
   jitter, a device's `retry_after` honoured (capped by the policy). It is configurable per job
   (`with_retry(Some(policy))`) and can be disabled (`with_retry(None)`).
2. **`OutcomeUnknown` is never retried automatically.** The job goes to `NeedsAttention`. The
   caller (or person) runs `reconcile`, which asks the backend whether a job with this print's tag
   (or, without tag support, title, user and time window) already exists:
   * `Found(job)`: the earlier attempt did create it. `accept` adopts it and the job carries on as
     `Watching`; `retry` would duplicate it.
   * `NotFound`: advice is `RetryNow` when the backend lists finished jobs too (so a quick job that
     already printed would still show), otherwise `CheckPrinterFirst`.
   * `Ambiguous(jobs)`: several candidates (the same title was printed before, or two attempts both
     got through). The person looks at the states and `accept`s one or retries.
   * `Err(Unsupported)`: the backend cannot be asked; advice is `CheckPrinterFirst`.
3. **A printer-side `Aborted` or `Cancelled` is never retried automatically.** It is a decision (or a
   fault) at the device; `retry()` is manual, makes a new attempt, gets a new remote `JobId`, and the
   history is kept.
4. A non-transient error (`Rejected`, `UnsupportedFormat`, `InvalidRequest`, `ConsentRequired`, ...) is
   `Terminal(Failed)` at once. Retries that run out are `Terminal(Failed)` with the last error and
   advice that follows from it.
5. `cancel()` works in every state (section 6).

## 5. The idempotency tag

The ambiguity in section 4 comes from one place: a connection that dies after the request was written.
A network backend cannot tell whether the printer made the job. To make "did it?" answerable, each
`PrintJob` generates a **short random tag** (8 lowercase hex characters, for example `abc123de`) when it is
created. The tag is the same for every attempt of that print and is carried to the printer where the
protocol allows:

```text
SubmitRequest::with_tag(tag)        // the request carries it
Backend::tag_support() -> TagSupport { placement: TagPlacement, matchable: bool }
TagPlacement::JobName      // a suffix on the job name: "Quarterly report [ref abc123de]"
TagPlacement::DocumentName // the same suffix on the document name
TagPlacement::Native       // a job attribute of the protocol (a dedicated, searchable field)
TagPlacement::None         // the backend cannot carry it
```

* The backend applies the tag, because it knows its own limits (a name length cap): the title is cut
  so that the suffix survives (`JobTag::apply_to`), and it is **appended exactly once** however many
  attempts there are. The registry drops a tag a backend cannot carry, and `Submitted::tag` says what
  was carried.
* `matchable` says whether `list_jobs` returns the tag back, so that reconcile can match on it.
* **Defaults.** On for `BackendClass::Network` backends, off for `BackendClass::Local`
  (`TagPolicy::Auto`; `Always` and `Never` override it per job).
  * Network: connection loss is exactly where the ambiguity arises, and the user's job name is a
    free-text field that already travels. The suffix costs a few characters of a title that shows up
    on the printer's own display and queue.
  * Local spooler: the hand-over is a local call or a pipe, so a failed hand-over is a definite failure
    far more often; the title shows in the system print queue UI the user already watches, and the
    suffix would be visible noise there. A spooler also offers no way to search by it today. An app that
    wants it anyway chooses `TagPolicy::Always`.
* A tag is a *correlation id*, not a secret: it is visible to anyone who can list the queue.
* Without tag support, reconcile falls back to **title, user and a time window** and may answer
  `Ambiguous`. It never claims more than it knows.

## 6. Cancel in every state

| State | `cancel` does |
|---|---|
| `Preparing`, `Submitting` | nothing was sent: `Terminal(Cancelled)`; the token is set so a submit in flight on another thread stops at its next checkpoint |
| `RetryScheduled` | `Terminal(Cancelled)`, no request |
| `Accepted`, `Watching` | `Registry::cancel_job` on the remote job; the job keeps being followed until the printer reports how it ended (`Cancelled`, or forgotten, which counts as cancelled here). A backend that cannot cancel returns `Unsupported` and the state is unchanged |
| `NeedsAttention` | reconcile first; jobs that are provably ours (tag matched) are cancelled; if the match is only by title the candidates are *not* cancelled (they could be someone else's) and the error says so; a backend that cannot be asked yields `Cancelled` with a warning that a job may still exist |
| `Terminal` | no-op, not an error |

## 7. The common base for other job sources

CUPS, IPP and the Windows spooler have queues of jobs, and so does WS-Print. Applications want to
see "what is this printer doing" and, above all, "is *my* job in there". The core therefore defines a
**backend-neutral remote job** and optional trait methods with safe defaults, so a backend adds
capabilities one at a time and a caller can tell exactly which it has.

```text
RemoteJob   { id: JobId, title, user: Option<_>, created: Option<SystemTime>, state: JobState,
              reasons, tag: Option<JobTag>, ours: bool, sheets_done, pages_done }
JobFilter   { user, title, since, tag, states }      // user/title/since/states restrict; tag marks `ours`
Fingerprint { title, user, tag, since }              // what reconcile looks for
Reconciled  { NotFound, Found(RemoteJob), Ambiguous(Vec<RemoteJob>) }
JobQueryCaps{ list, created_time, user, tag, history }

Backend::tag_support(&self) -> TagSupport                                        // default: none
Backend::job_query_caps(&self) -> JobQueryCaps                                   // default: nothing
Backend::list_jobs(&self, printer_native, &JobFilter) -> Result<Vec<RemoteJob>>  // default: Unsupported
Backend::find_job(&self, printer_native, &Fingerprint) -> Result<Reconciled>     // default: on top of list_jobs
Registry::list_jobs / find_job / backend_info                                    // routed by the printer id's prefix
```

`list_jobs` lists **foreign jobs too** (`ours == false`): a UI that shows a printer's queue needs them,
and a reconcile must not mistake them for its own. `JobFilter::tag` never hides a job, it only decides
`ours`. `JobQueryCaps` is `WatchSupport`'s sibling: a caller reads it before relying on a field (a
backend with `created_time: false` cannot narrow by time; one with `history: false` cannot show
finished jobs, so "not found" does not prove "did not print").

### Mapping table

What each source offers, and how it maps onto the neutral types. "Today" is what the built-in
adapters do in this change.

**CUPS (`lp`, `lpstat`, or IPP to `cupsd`).**

| Aspect | |
|---|---|
| Tag | `lp -t "<title> [ref abc123de]"` (job-name suffix). `job-name` is also a plain IPP attribute, so the same text is visible to `lpstat -o` output and to IPP. |
| Job id | printed at once by `lp` ("request id is Queue-12"); also `job-id` in IPP replies. |
| Listing | `lpstat -W all -o` (text, ids and user, no titles) or IPP `Get-Jobs` with `which-jobs` and `requested-attributes`. |
| User | `job-originating-user-name`. |
| Created time | `time-at-creation` (seconds since the scheduler started) and `date-time-at-creation` when present. |
| States | `pending`, `pending-held`, `processing`, `processing-stopped`, `canceled`, `aborted`, `completed` map to `Pending`, `Held`, `Processing`, `Stopped`, `Cancelled`, `Aborted`, `Completed` (reasons such as `job-completed-with-warnings` refine `Completed`). |
| Cancel | `cancel <id>` / IPP `Cancel-Job`. |
| Forgetting | finished jobs are kept as history by default (`PreserveJobHistory`, `MaxJobs`), so `which-jobs=completed` still finds them for a while; `history: true`. |
| Today | `SystemSpooler` (name `cups`): `tag_support` none, `job_query_caps` none, `list_jobs` returns `Unsupported`. Reason: the adapter shells out to `lp`/`lpstat`, which do not show titles, and listing by IPP needs a client this crate does not have; neither can be exercised on the Windows development machines or in CI here, so it is not shipped untested. The neutral types are what a future adapter fills in. |

**Generic IPP (a network printer or `cupsd` over the wire).**

| Aspect | |
|---|---|
| Tag | `job-name` suffix (visible on the device display); a dedicated field is better where the server accepts one: a job attribute (`TagPlacement::Native`) that `Get-Jobs` returns. `job-uuid` is assigned by the printer and cannot carry ours, but it is the stable handle to use after the first reply. |
| Listing | `Get-Jobs` with `which-jobs` (`not-completed`, `completed`, `all`), `limit`, `my-jobs`, `requesting-user-name`. |
| Attributes | `job-id`, `job-uuid`, `job-name`, `job-originating-user-name`, `job-state`, `job-state-reasons`, `time-at-creation` or `date-time-at-creation`, `job-media-sheets-completed`, `job-impressions-completed`. |
| States | as for CUPS (RFC 8011 `job-state`). |
| Cancel | `Cancel-Job` (idempotent for ended jobs: "not possible" is treated as success). |
| Forgetting | device-dependent: many printers keep a short history, some none. `Create-Job-Subscription` (optional, RFC 3995) would push changes; not used. |
| Today | no IPP adapter in this crate. The `testing` module's IPP-like fake exercises the contract (list with creation time, tag in a native attribute). |

**Windows spooler.**

| Aspect | |
|---|---|
| Tag | `DOC_INFO_1.pDocName` (the document name), set when the job is started; the suffix goes there (`TagPlacement::DocumentName`). The print queue window shows it. |
| Job id | returned when the document is started; the current adapter does not keep it (`Submitted::job` is `None`). |
| Listing | `EnumJobs` / `GetJob` (`JOB_INFO_1`/`2`). |
| Fields | `pDocument` (title), `pUserName`, `Submitted` (`SYSTEMTIME`, local time), `Status` bit flags, `TotalPages`, `PagesPrinted`. |
| States | `JOB_STATUS_PAUSED` -> `Held`; `SPOOLING`/`PRINTING`/`PRINTED` -> `Processing`; `ERROR`/`OFFLINE`/`PAPEROUT`/`USER_INTERVENTION`/`BLOCKED_DEVQ` -> `Stopped` with a reason; `DELETING`/`DELETED` -> `Cancelled`; `COMPLETE` -> `Completed`; no flags and queued -> `Pending`. |
| Cancel | `SetJob(JOB_CONTROL_DELETE)`. |
| Forgetting | **finished jobs vanish** from the queue unless the printer keeps them (`JOB_STATUS_RETAIN`): `history: false`. "Not listed" after submission therefore most likely means printed. |
| Today | `SystemSpooler` (name `spooler`): no job handle, `tag_support` none, `list_jobs` `Unsupported`. Reason: the printing code is a separate, large Windows path (XPS, Direct2D) that does not keep the spooler job id; listing needs it re-plumbed and a real queue to test against. Not testable here, so not shipped. |

**WS-Print.**

| Aspect | |
|---|---|
| Tag | `JobName` in the print ticket, suffix form (`TagPlacement::JobName`, `matchable: true`). The job name is cut to the protocol's 255 characters with the suffix kept. |
| Job id | `CreatePrintJob` reply (`JobId`); the neutral id is `wsd:<printer url>#<id>`. |
| Listing | `GetActiveJobs` and `GetJobHistory` (`JobSummary`: id, state, name, user, counters); `GetJobElements` for one job when a summary lacks the name or user. |
| User | `JobOriginatingUserName` (the backend sends a fixed name, never the account). |
| Created time | **not available** (`created: None`, `created_time: false`). |
| States | `Pending`, `PendingHeld`, `Processing`/`Started`/`Terminating`, `ProcessingStopped`, `Completed` (refined by reasons), `Canceled`, `Aborted` map to the neutral states, as `job_status` already does. |
| Cancel | `CancelJob` ("too late" is success). |
| Forgetting | the history window is device-dependent and often short; `history: true` with that caveat, and `NotFound` therefore means "not listed", not "never printed". |
| Today | `WsdBackend` implements `list_jobs` (active + history, `ours` from the suffix), `tag_support`, `job_query_caps`. The older `WsdBackend::reconcile(printer, title)` is unchanged; `Registry::find_job` is the neutral form of it. |

## 8. What this change does not do

* No persistence: a `PrintJob` lives in the process that made it. The types are plain data and do
  not implement `serde` traits; adding that behind a feature later does not change them.
* No threads or timers: the caller advances the job.
* No IPP client, no CUPS or Windows-spooler listing (section 7: not testable here).
* No change to the older `Registry::submit` / `submit_job` / `JobWatch` behaviour; a `PrintJob` is
  built on them.
