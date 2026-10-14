# `ScanJob`: one scan, stable across attempts

`Registry::acquire` is one blocking call that returns pages or an error. A person, though, presses Scan
once and expects one document: twenty sheets from the feeder, however many times the paper jams, the feeder
runs dry, or the link drops on the way. `ScanJob` (`craft_scan::job`) is the record of that single
acquisition. It is the scanning counterpart of the print side's `PrintJob`, kept as a separate, plain object:
printing is push-based (submit, then watch a remote job), scanning is pull-based (pages arrive while the call
runs and must survive its failure).

Everything is in memory and caller-driven. There is no thread, no timer, no clock (every call that changes
the job takes a `craft_job::Timestamp`), no async, and no I/O: **a `ScanJob` does not run `acquire`**. The app
does, on its own worker thread. The job plans the attempt, records what comes back, and says what may happen
next. It builds a `craft_job::JobView`, the same record a UI shows for a print job.

## 1. The loop

```text
let mut job = ScanJob::new(settings, ScanJobConfig::default().with_scanner(&details));

loop {
    let plan = job.begin_attempt(now())?;               // or begin_attempt_with(now(), Continuation::Append)
    let worker = spawn(move || craft_scan::acquire(&plan.settings));   // plan.settings is Send
    while !worker.is_finished() {
        job.record_progress(&plan.progress);            // pages so far, page in flight
        publish(job.snapshot());                        // cheap: no page bytes
        if user_pressed_stop { job.cancel(now()); }     // requests plan.cancel
    }
    job.finish_attempt(worker.join(), now())?;

    match job.retry_advice() {
        RetryAdvice::RetryNow => continue,                      // first attempt, glass fallback
        RetryAdvice::RetryAfter(wait) => { sleep(wait); continue }   // busy scanner, nothing kept
        _ => break,        // done, or a person decides (Interrupted / NeedsAttention), or CheckDeviceFirst
    }
}
// Interrupted / NeedsAttention: ask. Continue -> begin_attempt_with(.., Append). Start over -> Restart.
// Accept what is there -> finish(..). Stop -> cancel(..). Terminal: take_pages() and use them.
```

`begin_attempt_with` is only ever called on the person's say-so (or the app's, on their behalf). The job never
decides, on its own, to scan on after pages were kept.

## 2. States

The lifecycle is the job's own; it is separate from anything the scanner reports.

| State | Meaning | Leaves by |
|---|---|---|
| `Preparing` | created; no attempt has begun | `begin_attempt`, `cancel` |
| `Acquiring` | an attempt is running on the app's worker | `finish_attempt`, `device_cancelled`; `cancel` only requests the stop |
| `Interrupted` | the last attempt stopped for a **known cause** before the job was done. Pages are kept. `retry_advice()` says whether the app may go on by itself | `begin_attempt_with(Append \| Restart)`, `finish`, `cancel` |
| `NeedsAttention` | the last attempt ended and it is **not known** what the scanner did beyond what was reported. Nothing is resent | the same exits as `Interrupted` |
| `Terminal(Completed)` | every page that was wanted arrived (or the feeder emptied normally) | none |
| `Terminal(CompletedWithWarnings)` | fewer pages than wanted, accepted (`finish`) or delivered | none |
| `Terminal(Failed)` | stopped by a failure nothing in the job can cure | `begin_attempt` (a manual start over; history kept) |
| `Terminal(Cancelled)` | this side stopped it (`cancel`) | none |
| `Terminal(CancelledByDevice)` | the scanner ended it (`device_cancelled`) | none |

`JobView::state` is that, reduced to `craft_job::JobState`: `Preparing` is `Pending`; `Acquiring` is `Processing`;
`Interrupted` is `NeedsAttention` unless the advice is an automatic retry, then `Pending`; `NeedsAttention` is
`NeedsAttention`; `Completed` is `Completed`; `CompletedWithWarnings` is `CompletedWithErrors` (pages are
missing); `Failed` is `Aborted` (craft-job's word for "stopped by a failure"); `Cancelled` and `CancelledByDevice`
are both `Cancelled`, and the reason `cancelled_by_device` tells them apart.

## 3. Pages so far

"Pages so far" is first-class. The snapshot lists every kept page (number, the attempt that delivered it, source,
format, width, height, dpi, bytes) and the total, plus the expected count when known (`ScanSettings::pages`, not
for hardware duplex, where sides are not sheets; `None` means "the whole feeder"). It never holds a page's
bytes: `snapshot()` clones a few small fields and is meant to be called every frame. The bytes live in the job
until `take_pages()`; `pages()` lets a preview read them without taking.

Pages reach the job two ways, and are never counted twice. While the worker runs, the app calls
`record_progress(&plan.progress)` (or `absorb_pages(vec)` if it drained the token itself). When the worker
returns, `finish_attempt` takes whatever the token still holds, then merges the result: the result's pages
replace this attempt's pages when it has at least as many (backends return all pages, `Incomplete` carries them),
otherwise the pages already in hand are kept. A backend that fails with a plain error after pushing pages
through the token therefore loses nothing, as long as the token was the way the pages travelled.

`take_pages()` is refused while an attempt runs. A pages-taken job can still `Append`: taken pages count as kept.

## 4. What survives which failure

| Failure | Pages kept | Next state | Advice |
|---|---|---|---|
| jam, cover open, multi-feed, attention, device fault (any time) | yes | `Interrupted` | `DoNotRetry`: a person acts first |
| feeder empty, pages in hand, **whole feeder wanted** or wanted count reached | yes | `Completed` | none (this is how a stack ends, per `after_transfer`) |
| feeder empty, pages in hand, **count short** (mid-job reload) | yes | `Interrupted` | `DoNotRetry`; the person reloads and chooses `Append` |
| feeder empty, **no pages**, request was `Auto`, glass exists | n/a | `Interrupted`, attempt `FellBackToGlass` | `RetryNow`; next plan has `Source::Glass` |
| feeder empty, no pages, feeder requested | n/a | `Interrupted` | `DoNotRetry` |
| busy, **no pages** | n/a | `Interrupted` while the budget lasts, then `Failed` | `RetryAfter`, then `DoNotRetry` |
| busy, pages kept | yes | `Interrupted` | `DoNotRetry` (paper position unknown) |
| link dropped (`Offline`) or an unclassified error, **no pages, nothing in flight** | n/a | as busy | as busy |
| link dropped or unclassified error **after pages, or mid-page** | yes | `NeedsAttention` | `CheckDeviceFirst` |
| validation failure, `Unsupported`, no source | n/a | `Failed` | `DoNotRetry` |
| `cancel()` | yes | `Terminal(Cancelled)` | `DoNotRetry` |
| `device_cancelled()` | yes | `Terminal(CancelledByDevice)` | `DoNotRetry` |

"Reading a failure" is three questions, answered from the existing `InterruptKind`, the page count and whether a
page was part-way across (`ProgressToken::in_page`):

* **transient** (`Busy`, `Offline`, and not unknown): waiting may cure it.
* **a person can cure it** (`CoverOpen`, `Jam`, `PaperProblem`, `FeederEmpty`, `MultiFeed`, `Attention`,
  `DeviceError`, `BusyOrFeederEmpty`): the job waits in `Interrupted`.
* **unknown outcome** (`Offline` or `Other` after pages, or mid-page): the transfer may have done more than we were
  told. `RetrySafety::OutcomeUnknown`; the job waits in `NeedsAttention`.

The automatic part is exactly `craft_job::advise`, fed `retryable = transient && nothing kept` and the safety above,
under `ScanJobConfig::retry` (default `RetryPolicy::default()`; `RetryPolicy::none()` turns it off). Its attempt count
is the failures in a row that delivered no page, so a large ADF scan interrupted ten times does not use up the
budget. Only `Safe` failures are ever automatic; `Idempotent` is never produced because a second `acquire` is never
harmless.

## 5. Continuation

When pages are kept, the next attempt must say what to do with them. There is no default, because where the paper
stopped is not knowable: the sheet that jammed may or may not have been scanned, and a person may have taken the
stack out and put it back.

* **`Append`**: keep the pages, ask for what is missing (`expected - kept`; the whole feeder when the total is
  unknown). The person has put the remaining sheets in. `NothingLeft` if the wanted count is already reached
  (use `finish`).
* **`Restart`**: discard the kept pages and the retry count, and scan the whole job again. Pages already
  `take_pages()`d are the app's.
* With nothing kept, the two are the same, and `begin_attempt` (no argument) is enough. With pages kept,
  `begin_attempt` returns `ContinuationRequired`.
* **`Resume`** (continue after the last good page without feeding again) does not exist. It would need a backend
  that can say it can; none can. `Continuation` is `#[non_exhaustive]` so it can be added.

## 6. `Auto`

The job leaves `Source::Auto` to the backend (it reads a fresh status at acquire time) and records what the
transfer actually used (`Acquired::source`, else `ProgressToken::source`). For the fallback rule it needs a guess
at what `Auto` resolved to: `Source::resolve(feeder_ready, has_feeder, has_glass)` over what the job knows
(`ScanJobConfig::with_scanner` or `with_capabilities`, refreshed by `observe_status`). Network scanner drivers
report no tray state, so `feeder_ready` is `None` there and the guess is the feeder, which is also what the drivers
do; an empty feeder is then learned from the failure. When `auto_fallback_to_glass` says so (Auto resolved to the
feeder, glass exists, **no page at all in hand**, the failure is `FeederEmpty` or `BusyOrFeederEmpty`), the attempt
is recorded as `FellBackToGlass`, the next plan asks for `Source::Glass` with one page, and the expected total
becomes one. A jam, a cover, a plain busy and any shortfall after pages stay errors, as the rule already says.

## 7. Cancel

`cancel(at)` is the client's stop. Between attempts the job ends `Cancelled` at once. While an attempt runs it
requests the attempt's `CancelToken` (the worker's transfer stops at the backend's next checkpoint) and the job
ends `Cancelled` when `finish_attempt` reports the return, whatever it was: a user who pressed Stop does not get a
"completed" job even if the last page raced in. Kept pages stay in the job.

`device_cancelled(at)` is the scanner's: the Cancel key on the panel, or the device dropping the job. It is a
different end because the app must not say "you cancelled it" and a retry is not implied. **No backend reports it
through `ScanError` today** (there is no `InterruptKind::Cancelled`), so the app calls it when it learns so from
its own protocol layer. That is a limit, not a design preference.

## 8. Idempotency, honestly

A printed document has a name the app can tag. A scan request does not: `ScanSettings` has no job-name field, WIA
and the inbox WSD scan driver have no job tag to set, SANE has no job list. eSCL has `ScanJobs` and WS-Scan has job
status, where a reconcile could look; **none of that is implemented here**.

So: the job carries a `Fingerprint` (title, creation time, a window) and `JobView::tag` exists, but
`ScanJobConfig::tag_support` is `TagSupport::None`, and so is `Backend::tag_support()`, a default method added so a
future eSCL or WS-Scan backend can say otherwise (`Registry::tag_support(id)` routes it). No tag is generated or sent.
When the outcome of an acquire is unknown the answer is `CheckDeviceFirst`: someone looks at the scanner.

Say it plainly: **a duplicate `acquire` is not free.** It moves the carriage and pulls paper through the
feeder. Sheets that were already scanned but whose pages were lost are ejected and cannot be scanned again by
repeating the call; they are only scanned again if a person puts them back. That is why an unknown outcome is never
retried by the job, and why `Append` is the person's decision.

## 9. `device_view`

`device_view(&Scanner, Option<HandlingStatus>, Option<Source>) -> craft_job::DeviceView` shows a scanner the same
way as a printer: capabilities, a `PaperPathView` (feeder loaded, capacity, glass, source in use) and one `Indicator`
per status bit.

* `feeder_ready` is an `Option<bool>` and becomes a `Tri` as given (`None` is `NotReported`).
* The other bits are plain `bool`s that mean "set only when reported". A status that is entirely clear and leaves
  the feeder unreported (`HandlingStatus::default()`) is what a driver that reports nothing looks like (the inbox
  WSD scan driver), so **every** indicator is `NotReported`, never `Off`. `None` (the backend cannot preflight) is
  the same. Once any bit says something, the others are read as given.
* Indicators for hardware the scanner lacks (no feeder, flatbed or duplexer) are left out unless the status asserts
  them. The lamp bit is an error flag: `LampError` is `On` or "no error reported".
* `From<Connection> for DeviceConnection`: `Usb` to `Usb`, `Wsd` to `Network`, `Other` to `Other`, `Unknown` to
  `Unknown`. (craft-job's README describes a different mapping that fits craft-print's wider set; this is what
  craft-scan's four variants are.)

## 10. Limits

* No persistence. A `ScanJob` lives in the process that made it; page bytes are held in memory (the app may
  `take_pages()` and spill to disk between attempts). Types are plain data; nothing implements `serde`.
* No reconcile: see section 8.
* `Resume`, device-cancel detection and a device-side job handle are not there (sections 5, 7, 8).
* The job id in `JobView` is `<backend>:local-<n>`, a process-unique number. There is no device job to name.
* Paper position is never inferred. Whether a jammed sheet was scanned is the person's call.
* An `Offline` failure before any page is treated as safe to repeat. If the link dropped after the scanner had
  already started the carriage, that is wrong in the same way a duplicate is; the job cannot tell, and the retry
  budget bounds the cost. An app that prefers to ask can use `RetryPolicy::none()`.
