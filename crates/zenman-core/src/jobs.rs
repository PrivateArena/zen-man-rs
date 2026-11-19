//! The job table — the in-process replacement for "the browser waits on a goroutine" (plan §5.6
//! `jobs.rs`, §4.1, §6.1 F1/F2/F3/F14).
//!
//! # Topology
//!
//! ```text
//! caller (UI thread)          worker thread
//!   Session::dir_size  ──attach──►  f(&CancelToken, &Progress)
//!        │                              │ JobEvent::Progress   (throttled at the source)
//!        │ Receipt::Running{job,origin} │ JobEvent::Finished | JobEvent::Failed
//!        ▼                              ▼
//!   Session::poll  ◄──── JobRegistry::poll_at  drains each job's mpsc::Receiver ──► Vec<EngineEvent>
//! ```
//!
//! One `std::thread` per job, one unbounded `mpsc::channel` per job, and **one** consumer: the UI
//! thread calls [`JobRegistry::poll`] once per frame (plan §4.1 "poll() once per frame, from
//! logic()"), which is the only place job events cross into the engine's event stream. Nothing here
//! blocks the caller: every receive is a `try_recv`, and a `JoinHandle` is only touched once
//! [`std::thread::JoinHandle::is_finished`] says the worker has already ended.
//!
//! # No `tokio`, and no executor except on the worker
//!
//! Plan §9.1: "the only executor is in the binary", and `tokio` is deliberately absent from the
//! graph. An `Env` action is a [`PlatformFuture`](crate::platform::PlatformFuture) that does its work
//! *when polled* (see the `platform/mod.rs` module docs), so a job that must await one drives it with
//! [`block_on`] **on its own worker thread**. [`block_on`] is a bounded poll driver, not a task
//! scheduler: it is correct for exactly the futures this crate produces (they complete in one poll)
//! and it *refuses* — rather than hangs or spins — when handed one that does not. Calling it on the
//! UI thread would be the frozen-window bug this module exists to prevent, so it is only ever called
//! from inside a [`spawn`]ed closure.
//!
//! # Cancellation is two flags, not one ([`CancelToken`])
//!
//! `requested` is set by whoever asks; `fired` is set by the worker that actually stopped. Without the
//! second flag, a `cancel` racing a job that was one instruction from finishing would report
//! "cancelled" for work that completed. [`JobRegistry::cancel`] additionally refuses to touch a job
//! that has already left the running set, so the caller cannot claim a cancellation that did not
//! happen either (plan §7 step 6's acceptance).
//!
//! # Exactly one terminal event
//!
//! [`spawn`] wraps the closure and sends precisely one terminal [`JobEvent`]; the table publishes it
//! once (guarded by [`JobRecord::terminal_at`]) and a *disconnected* receiver — a panicking worker —
//! is synthesised into a `Failed` terminal event, so no spinner can outlive its thread (plan F14).

use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::error::CoreError;
use crate::events::{EngineEvent, JobPayload};
use crate::limits::{LIVE_JOB_TTL, PROGRESS_THROTTLE};
use crate::path::AbsPath;
use crate::platform::PlatformFuture;
use crate::session::Origin;

/// Job identifiers, allocated by the table ([`JobRegistry::next_id`]) so one id is never two jobs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct JobId(pub u64);

impl JobId {
    /// The numeric id.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What a job does: the payload assertion, the thread name, the jobs-pane filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// A directory listing produced off the render path (cold cache, flat walk, archive manifest).
    List,
    /// The recursive `dir_size` walk (Go's synchronous `/api/dir/size`).
    DirSize,
    /// Paste (copy/cut/link) — step 9.
    Paste,
    /// Delete/trash — step 9.
    Delete,
    /// Undo-log revert — step 10.
    Revert,
    /// Live filesystem search — step 11.
    SearchLive,
    /// FTS index rebuild — step 12.
    IndexRebuild,
    /// Archive extraction — step 13.
    Extract,
    /// Custom action execution — step 11.
    ActionExec,
    /// Legacy config/session import — step 4's migration, re-offered at runtime.
    Import,
}

impl JobKind {
    /// The stable lowercase name (thread names, logs, pane filter).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::DirSize => "dir_size",
            Self::Paste => "paste",
            Self::Delete => "delete",
            Self::Revert => "revert",
            Self::SearchLive => "search_live",
            Self::IndexRebuild => "index_rebuild",
            Self::Extract => "extract",
            Self::ActionExec => "action_exec",
            Self::Import => "import",
        }
    }
}

impl std::fmt::Display for JobKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a job ended, kept separate from [`CoreError`] because `Partial`/`TimedOut` have no error object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Every item was processed.
    Done,
    /// Work stopped early with results worth keeping — a cancelled delete that already removed 3 of
    /// 10 paths is `Partial`, and its report is the truth about those 3 (plan F3).
    Partial,
    /// The closure returned `Err`; the typed error rides on the event.
    Failed,
    /// The user cancelled and the worker observed it ([`CancelToken::fired`]).
    Cancelled,
    /// The job outlived its [`JobSpec::budget`] and the watchdog ended it.
    TimedOut,
}

impl JobStatus {
    /// The [`JobInfo::state`] spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timeout",
        }
    }

    /// `true` when the job produced results the caller should apply.
    #[must_use]
    pub const fn is_success(self) -> bool {
        matches!(self, Self::Done | Self::Partial)
    }
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Byte/item counters of a running job — the payload of [`JobEvent::Progress`].
///
/// The `Option`s are the point: `(undefined files)` (plan §2.4, [`crate::metrics::FileSizes`]) came
/// from Go writing `0` where "unknown" was the truth. `total_bytes: None` means the walk has no size
/// answer yet and the UI renders `--`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct JobProgress {
    /// Bytes transferred or inspected so far.
    pub bytes_done: u64,
    /// Items (files, entries) finished so far.
    pub files_done: u64,
    /// Known total bytes, `None` while a walk cannot know.
    pub total_bytes: Option<u64>,
    /// Known total items.
    pub total_files: Option<u64>,
    /// The item currently being worked on — the only place a filename appears in a progress event.
    pub current: Option<String>,
}

impl JobProgress {
    /// Zeroed counters with the given totals.
    #[must_use]
    pub const fn with_totals(total_bytes: Option<u64>, total_files: Option<u64>) -> Self {
        Self {
            bytes_done: 0,
            files_done: 0,
            total_bytes,
            total_files,
            current: None,
        }
    }

    /// A fraction in `0.0..=1.0` from the counters that are known; `None` when nothing is knowable
    /// (both totals unknown ⇒ a spinner, not a bar).
    #[must_use]
    pub fn fraction(&self) -> Option<f32> {
        if let Some(total) = self.total_bytes.filter(|t| *t > 0) {
            return Some(((self.bytes_done as f64 / total as f64).clamp(0.0, 1.0)) as f32);
        }
        if let Some(total) = self.total_files.filter(|t| *t > 0) {
            return Some(((self.files_done as f64 / total as f64).clamp(0.0, 1.0)) as f32);
        }
        None
    }
}

/// The terminal summary of a job, deliberately **without** progress fields: a `Finished` event that
/// also carried counters would let the UI read stale progress after completion, which is how Go's
/// extract-status handler could show 100 % of a job that had just failed. Counters live in
/// [`JobEvent::Progress`] and nowhere else — and [`spawn`] flushes the last counters *before* the
/// terminal event, so the final numbers are never lost.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct JobOutcome {
    /// How it ended.
    pub status: JobStatus,
    /// One-line human summary ("Pasted 12 items into /tmp/x"). The UI shows it; it never *parses* it
    /// (the typed answer is [`JobPayload`]).
    pub summary: String,
    /// Moved/renamed pairs, so a revert and the invalidation fan-out can name both sides.
    pub moved: Vec<(AbsPath, AbsPath)>,
    /// Non-fatal problems worth a toast (skipped collisions — plan F12).
    pub warnings: Vec<String>,
    /// Rendered failure text. The *typed* error rides on [`EngineEvent::JobFinished::error`].
    pub error: Option<String>,
    /// Bytes the job actually moved (its own accounting, not the walk's progress).
    pub bytes_copied: u64,
}

impl JobOutcome {
    /// A clean finish with a summary line.
    #[must_use]
    pub fn done(summary: impl Into<String>) -> Self {
        Self {
            status: JobStatus::Done,
            summary: summary.into(),
            moved: Vec::new(),
            warnings: Vec::new(),
            error: None,
            bytes_copied: 0,
        }
    }

    /// The outcome [`spawn`] builds when the worker stopped on a cancellation request.
    #[must_use]
    pub fn cancelled() -> Self {
        Self {
            status: JobStatus::Cancelled,
            summary: String::new(),
            moved: Vec::new(),
            warnings: Vec::new(),
            error: Some(CoreError::Cancelled.user_message()),
            bytes_copied: 0,
        }
    }

    /// The watchdog's outcome (plan §5.6: "a hung job becomes `JobFinished{Failed(timeout)}`").
    #[must_use]
    pub fn timed_out(budget: Duration) -> Self {
        Self {
            status: JobStatus::TimedOut,
            summary: String::new(),
            moved: Vec::new(),
            warnings: Vec::new(),
            error: Some(format!(
                "job exceeded its {} ms budget",
                u64::try_from(budget.as_millis()).unwrap_or(u64::MAX)
            )),
            bytes_copied: 0,
        }
    }

    /// A failure outcome with the caller's summary and the rendered error.
    #[must_use]
    pub fn failed(summary: impl Into<String>, error: &CoreError) -> Self {
        Self {
            status: JobStatus::Failed,
            summary: summary.into(),
            moved: Vec::new(),
            warnings: Vec::new(),
            error: Some(error.user_message()),
            bytes_copied: 0,
        }
    }

    /// `true` when the job reported success (including a partial one).
    #[must_use]
    pub const fn succeeded(&self) -> bool {
        self.status.is_success()
    }
}

/// What one worker thread sends to the table. The variants are disjoint *by construction*: progress
/// cannot ride on a terminal event, and a failure cannot be mistaken for a partial success.
#[derive(Clone, Debug)]
pub enum JobEvent {
    /// A throttled counter update.
    Progress(JobProgress),
    /// The job finished, with or without success. Exactly one of these per job, plus
    /// [`JobEvent::Failed`] for the error path — the table publishes whichever arrives, once.
    Finished {
        /// The terminal summary.
        outcome: JobOutcome,
        /// The typed result (plan §4.1 `JobPayload`).
        payload: JobPayload,
    },
    /// The job's closure returned an error; the typed error survives for the toast.
    Failed(Arc<CoreError>),
}

/// The two-flag cancellation token (plan §7 step 6: a *completed* job is not misclassified).
///
/// * [`CancelToken::cancel`] sets `requested` — "somebody wants this stopped".
/// * [`CancelToken::mark_fired`] sets `fired` — "the worker saw the request and stopped", so the
///   terminal event may honestly say `Cancelled` instead of `Failed`.
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    requested: Arc<std::sync::atomic::AtomicBool>,
    fired: Arc<std::sync::atomic::AtomicBool>,
}

impl CancelToken {
    /// A fresh, un-requested token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask for cancellation. `true` when this call was the one that asked.
    pub fn cancel(&self) -> bool {
        !self
            .requested
            .swap(true, std::sync::atomic::Ordering::AcqRel)
    }

    /// `true` once anybody has asked. Lock-free: safe on an item boundary of a hot walk.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.requested.load(std::sync::atomic::Ordering::Acquire)
    }

    /// The worker's "I stopped because you asked" signal.
    pub fn mark_fired(&self) {
        self.fired.store(true, std::sync::atomic::Ordering::Release);
    }

    /// `true` when a requested cancellation actually took effect.
    #[must_use]
    pub fn fired(&self) -> bool {
        self.fired.load(std::sync::atomic::Ordering::Acquire)
    }

    /// The item-boundary check every long walk uses: `Err(`[`CoreError::Cancelled`]`)` once asked for,
    /// so a `?`-driven walk stops at the next item rather than mid-item (plan F3).
    pub fn check(&self) -> Result<(), CoreError> {
        if self.is_cancelled() {
            Err(CoreError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// The worker's reporting handle, throttled to [`PROGRESS_THROTTLE`] (plan F2 — the throttle is the
/// first of two guards; the table's last-value-wins coalescing is the second).
#[derive(Debug)]
pub struct Progress {
    tx: Sender<JobEvent>,
    throttle: Duration,
    state: Mutex<ProgressState>,
}

#[derive(Debug, Default)]
struct ProgressState {
    latest: JobProgress,
    last_emit: Option<Instant>,
    closed: bool,
}

impl Progress {
    fn new(tx: Sender<JobEvent>, throttle: Duration) -> Self {
        Self {
            tx,
            throttle,
            state: Mutex::new(ProgressState::default()),
        }
    }

    /// Report `p`, at most once per throttle window. The latest value is always remembered, so a
    /// suppressed report cannot lose the final count — [`Self::flush`] sends it.
    pub fn report(&self, p: JobProgress) {
        self.emit(p, false);
    }

    /// Add to the running counters and report.
    pub fn add(&self, bytes: u64, files: u64, current: Option<String>) {
        let next = {
            let mut guard = lock(&self.state);
            guard.latest.bytes_done = guard.latest.bytes_done.saturating_add(bytes);
            guard.latest.files_done = guard.latest.files_done.saturating_add(files);
            guard.latest.current = current;
            guard.latest.clone()
        };
        self.emit(next, false);
    }

    /// Set the counters to absolute values (a walk that counts cumulatively, like `dir_size`'s: the
    /// totals it reports *are* the progress, not deltas to add).
    pub fn set(&self, bytes_done: u64, files_done: u64, current: Option<String>) {
        let next = {
            let mut guard = lock(&self.state);
            guard.latest.bytes_done = bytes_done;
            guard.latest.files_done = files_done;
            guard.latest.current = current;
            guard.latest.clone()
        };
        self.emit(next, false);
    }

    /// Set the totals, bypassing the throttle (the walk's "now I know" moment).
    pub fn set_totals(&self, total_bytes: Option<u64>, total_files: Option<u64>) {
        let next = {
            let mut guard = lock(&self.state);
            guard.latest.total_bytes = total_bytes;
            guard.latest.total_files = total_files;
            guard.latest.clone()
        };
        self.emit(next, true);
    }

    /// The last counters this job reported.
    #[must_use]
    pub fn snapshot(&self) -> JobProgress {
        lock(&self.state).latest.clone()
    }

    /// Send the remembered counters unconditionally, once, before the terminal event. Further reports
    /// are dropped: the table must never see progress after a terminal event.
    pub fn flush(&self) {
        let snap = self.snapshot();
        self.emit(snap, true);
        lock(&self.state).closed = true;
    }

    fn emit(&self, p: JobProgress, force: bool) {
        let send = {
            let mut guard = lock(&self.state);
            if guard.closed {
                return;
            }
            guard.latest = p.clone();
            let now = Instant::now();
            let due = guard
                .last_emit
                .is_none_or(|at| force || now.duration_since(at) >= self.throttle);
            if !due {
                None
            } else {
                guard.last_emit = Some(now);
                Some(p)
            }
        };
        // A closed receiver means the table forgot this job (`forget`/shutdown): stop reporting.
        if let Some(p) = send {
            let _ = self.tx.send(JobEvent::Progress(p));
        }
    }
}

/// Everything [`spawn`] needs before the job runs.
#[derive(Clone, Debug)]
pub struct JobSpec {
    /// The id, allocated by the table.
    pub id: JobId,
    /// What kind of work this is.
    pub kind: JobKind,
    /// The label the jobs pane shows ("Paste 3 items").
    pub label: String,
    /// The correlation token of the widget that asked (plan §4.1).
    pub origin: Origin,
    /// Watchdog limit; `None` = no limit (a user-cancelled walk, not a hung one).
    pub budget: Option<Duration>,
    /// Progress coalescing interval; `None` = [`PROGRESS_THROTTLE`].
    pub throttle: Option<Duration>,
    /// The cancellation half the table keeps, so `JobRegistry::cancel` reaches the *worker's* token and
    /// not a stranger: `spawn` clones this into the worker, and both halves share the same two flags.
    pub token: CancelToken,
}

impl JobSpec {
    /// A plain job: no budget, default throttle, fresh cancellation token.
    #[must_use]
    pub fn new(id: JobId, kind: JobKind, label: impl Into<String>, origin: Origin) -> Self {
        Self {
            id,
            kind,
            label: label.into(),
            origin,
            budget: None,
            throttle: None,
            token: CancelToken::new(),
        }
    }
}

/// The worker's answer: a typed payload plus the summary that goes with it.
#[derive(Clone, Debug)]
pub struct JobResult<T> {
    /// The typed result the UI applies.
    pub payload: T,
    /// The summary; build with [`JobOutcome::done`] and fill `moved`/`warnings` as needed.
    pub outcome: JobOutcome,
}

impl<T> JobResult<T> {
    /// A successful result with a summary line.
    #[must_use]
    pub fn done(payload: T, summary: impl Into<String>) -> Self {
        Self {
            payload,
            outcome: JobOutcome::done(summary),
        }
    }
}

/// A spawned job's thread handle.
#[derive(Debug)]
pub struct JobHandle {
    /// The job's id.
    pub id: JobId,
    /// What it was.
    pub kind: JobKind,
    /// The widget that asked.
    pub origin: Origin,
    /// The worker thread. Left detached if the table forgets the job first.
    pub join: JoinHandle<()>,
}

/// Run `f` on its own thread and hand back its handle plus its event stream.
///
/// The wrapper guarantees the table's invariants: the final counters are flushed before the terminal
/// event, and the terminal event exists in every path — success, cancellation, error, **panic**. A
/// closure that panics is caught at this boundary (plan §6.1: a job thread must never be able to
/// take the process or a silently-`Failed`-forever job down with it): the panic becomes a typed
/// `CoreError` terminal event naming the job, so the poller sees a normal failure. The default panic
/// hook still runs first, which is what `crash_guard` (plan §5.9) hooks to write a report.
#[must_use = "a spawned job's receiver must be polled, or its completion is lost"]
pub fn spawn<F, T>(spec: JobSpec, f: F) -> (JobHandle, Receiver<JobEvent>)
where
    F: FnOnce(&CancelToken, &Progress) -> Result<JobResult<T>, CoreError> + Send + 'static,
    T: Into<JobPayload> + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    let token = spec.token.clone();
    let progress = Progress::new(tx.clone(), spec.throttle.unwrap_or(PROGRESS_THROTTLE));
    // A second sender held back for the one path where the worker never starts.
    let spawn_failed_tx = tx.clone();
    let name = format!("zenman-{}-{}", spec.kind, spec.id);
    let join = std::thread::Builder::new()
        .name(name)
        .spawn(move || {
            // The panic boundary (step 16b): `catch_unwind` here, not in the caller — every job
            // closure is `FnOnce`, so a panic that escaped would both kill the thread and leave the
            // table guessing from a disconnected receiver. The payload is reduced to its message;
            // an exotic payload that is not a string is still named.
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&token, &progress)))
                    .unwrap_or_else(|payload| {
                        let msg = payload.downcast_ref::<&str>().map_or_else(
                            || {
                                payload
                                    .downcast_ref::<String>()
                                    .cloned()
                                    .unwrap_or_else(|| "<non-string panic payload>".to_string())
                            },
                            |s| (*s).to_string(),
                        );
                        // `System` renders its payload whole (the `Io` variant would hide the panic
                        // message behind "I/O on <name>"), and the user deserves to see what panicked.
                        Err(CoreError::System(std::io::Error::other(format!(
                            "job {} panicked: {msg}",
                            spec.id
                        ))))
                    });
            progress.flush();
            let terminal = match result {
                Ok(res) => JobEvent::Finished {
                    outcome: res.outcome,
                    payload: res.payload.into(),
                },
                // A cancellation request wins over whatever error the walk surfaced on the way out:
                // the user asked, the walk stopped, that is the truth the UI should show.
                Err(e) if token.is_cancelled() => {
                    token.mark_fired();
                    let _ = e;
                    JobEvent::Finished {
                        outcome: JobOutcome::cancelled(),
                        payload: JobPayload::None,
                    }
                }
                Err(e) => JobEvent::Failed(Arc::new(e)),
            };
            let _ = tx.send(terminal);
        })
        .unwrap_or_else(|e| {
            // `Builder::spawn` fails only on OS thread exhaustion. Report it as a job that is already
            // dead: the table's first poll turns the terminal event (or the closed channel, if this
            // send also failed) into the same `Failed` outcome any other lost worker gets.
            let _ = spawn_failed_tx.send(JobEvent::Failed(Arc::new(CoreError::io(
                format!("job {}", spec.id),
                e,
            ))));
            std::thread::spawn(|| {})
        });
    (
        JobHandle {
            id: spec.id,
            kind: spec.kind,
            origin: spec.origin,
            join,
        },
        rx,
    )
}

// --------------------------------------------------------------------------------------------- table

/// The table: live jobs, their receivers, and the coalesced engine events they produce.
#[derive(Debug)]
pub struct JobRegistry {
    next_id: std::sync::atomic::AtomicU64,
    inner: Mutex<RegistryInner>,
}

#[derive(Debug)]
struct RegistryInner {
    jobs: BTreeMap<u64, JobRecord>,
    /// Events produced outside a poll (currently [`EngineEvent::JobStarted`]).
    outbox: Vec<EngineEvent>,
    /// How long a terminal record is retained.
    terminal_ttl: Duration,
}

impl Default for JobRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// The identity half of a record, i.e. everything the table needs after the thread is gone.
#[derive(Clone, Debug)]
struct JobMeta {
    id: JobId,
    kind: JobKind,
    origin: Origin,
}

/// One row of the table: the plan's `JobTable` entry.
#[derive(Debug)]
pub struct JobRecord {
    meta: JobMeta,
    /// The worker thread until it is joined, then `None` (so "reaped" and "never had a thread" are the
    /// same state and `join_if_done` stays idempotent).
    join: Option<std::thread::JoinHandle<()>>,
    rx: Receiver<JobEvent>,
    /// The cancellation half the table holds (the worker holds the other clone).
    token: CancelToken,
    label: String,
    started: Instant,
    budget: Option<Duration>,
    running: bool,
    pending: Option<JobProgress>,
    last_emit: Option<Instant>,
    last_progress: Option<JobProgress>,
    /// The channel event that ended this job, `Some` only between arrival and publication.
    terminal: Option<JobEvent>,
    /// When the terminal event was published; `None` while the job is live.
    terminal_at: Option<Instant>,
    acknowledged: bool,
    outcome: Option<JobOutcome>,
    error: Option<Arc<CoreError>>,
}

impl JobRegistry {
    /// A table with the plan's terminal-record retention ([`LIVE_JOB_TTL`]).
    #[must_use]
    pub fn new() -> Self {
        Self::with_terminal_ttl(LIVE_JOB_TTL)
    }

    /// A table that forgets finished jobs after `terminal_ttl` (tests use a tiny one).
    pub fn with_terminal_ttl(terminal_ttl: Duration) -> Self {
        Self {
            next_id: std::sync::atomic::AtomicU64::new(1),
            inner: Mutex::new(RegistryInner {
                jobs: BTreeMap::new(),
                outbox: Vec::new(),
                terminal_ttl,
            }),
        }
    }

    /// Allocate the next id: monotonic for the life of the process.
    #[must_use]
    pub fn next_id(&self) -> JobId {
        JobId(
            self.next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// Spawn `f` under `spec` and register it; [`EngineEvent::JobStarted`] comes out of the next poll.
    pub fn attach<F, T>(&self, spec: JobSpec, f: F) -> JobId
    where
        F: FnOnce(&CancelToken, &Progress) -> Result<JobResult<T>, CoreError> + Send + 'static,
        T: Into<JobPayload> + Send + 'static,
    {
        let id = spec.id;
        let label = spec.label.clone();
        let event = EngineEvent::JobStarted {
            id,
            kind: spec.kind,
            label: spec.label.clone(),
            origin: spec.origin,
        };
        let started = Instant::now();
        let budget = spec.budget;
        let token = spec.token.clone();
        let (handle, rx) = spawn(spec, f);
        let JobHandle {
            id,
            kind,
            origin,
            join,
        } = handle;
        let meta = JobMeta { id, kind, origin };
        let mut inner = lock(&self.inner);
        inner.outbox.push(event);
        inner.jobs.insert(
            id.as_u64(),
            JobRecord {
                meta,
                join: Some(join),
                rx,
                token,
                label,
                started,
                budget,
                running: true,
                pending: None,
                last_emit: None,
                last_progress: None,
                terminal: None,
                terminal_at: None,
                acknowledged: false,
                outcome: None,
                error: None,
            },
        );
        id
    }

    /// Drain every job's channel and return the coalesced engine events.
    #[must_use]
    pub fn poll(&self) -> Vec<EngineEvent> {
        self.poll_at(Instant::now())
    }

    /// [`Self::poll`] with an injectable clock, so the throttle, the watchdog and the retention window
    /// are testable without sleeping.
    #[must_use]
    pub fn poll_at(&self, now: Instant) -> Vec<EngineEvent> {
        let ttl = {
            let inner = lock(&self.inner);
            inner.terminal_ttl
        };
        let mut inner = lock(&self.inner);
        let mut events = std::mem::take(&mut inner.outbox);
        // Reap first, and only what an *earlier* poll retired: a record whose terminal event was
        // published in this very poll must survive it, or a caller that reacts to the event by
        // acknowledging the job would find nothing left to acknowledge.
        let mut reaped: Vec<u64> = Vec::new();
        let ids: Vec<u64> = inner
            .jobs
            .iter()
            .filter(|(_, r)| {
                !r.running
                    && r.terminal_at.is_some()
                    && (r.acknowledged || now.duration_since(r.terminal_at.unwrap_or(now)) >= ttl)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in &ids {
            if let Some(record) = inner.jobs.get_mut(id) {
                record.join_if_done();
            }
            reaped.push(*id);
        }
        inner.jobs.retain(|_, _| true);
        for id in &reaped {
            inner.jobs.remove(id);
        }
        let live_ids: Vec<u64> = inner.jobs.keys().copied().collect();
        for id in live_ids {
            let Some(record) = inner.jobs.get_mut(&id) else {
                continue;
            };
            // 1. the watchdog (plan §5.6: a hung job becomes a terminal event, not a hung tab);
            if record.running
                && let Some(budget) = record.budget
                && now.duration_since(record.started) > budget
            {
                record.token.cancel();
                record.running = false;
                record.terminal = Some(JobEvent::Finished {
                    outcome: JobOutcome::timed_out(budget),
                    payload: JobPayload::None,
                });
            }
            // 2. whatever the worker has said since the last poll;
            while record.running {
                match record.rx.try_recv() {
                    Ok(JobEvent::Progress(p)) => record.pending = Some(p),
                    Ok(ev @ (JobEvent::Finished { .. } | JobEvent::Failed(_))) => {
                        record.running = false;
                        record.terminal = Some(ev);
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        record.running = false;
                        record.terminal = Some(JobEvent::Failed(Arc::new(CoreError::Sql {
                            msg: format!("job {id} ended without reporting"),
                        })));
                    }
                }
            }
            // 3. one coalesced progress event at most;
            if let Some(p) = record.pending.take() {
                let due = record
                    .last_emit
                    .is_none_or(|at| now.duration_since(at) >= PROGRESS_THROTTLE);
                if due {
                    record.last_emit = Some(now);
                    record.last_progress = Some(p.clone());
                    events.push(EngineEvent::JobProgress {
                        id: JobId(id),
                        origin: record.meta.origin,
                        progress: p,
                    });
                } else {
                    record.pending = Some(p);
                }
            }
            // 4. and one terminal event, published once, ever.
            if record.terminal_at.is_none()
                && let Some(ev) = record.terminal.take()
            {
                record.terminal_at = Some(now);
                events.push(record.publish(JobId(id), &ev));
            }
            // 5. release the thread as soon as it has ended; the record itself waits for retention.
            if !record.running {
                record.join_if_done();
            }
        }
        let _ = ttl;
        events
    }

    /// Ask one *running* job to stop. `false` when there is no running job with that id — a job that
    /// already reported is never rewritten as cancelled (plan §7 step 6).
    pub fn cancel(&self, id: JobId) -> bool {
        let mut inner = lock(&self.inner);
        inner
            .jobs
            .get_mut(&id.as_u64())
            .is_some_and(|r| r.cancel_now())
    }

    /// Cancel every running job that belongs to `origin`: a closed tab takes down its own jobs and no
    /// one else's (plan §5.6 `cancel_origin`). Returns how many were asked.
    pub fn cancel_origin(&self, origin: Origin) -> usize {
        let mut inner = lock(&self.inner);
        let mut asked = 0;
        for record in inner.jobs.values_mut() {
            if record.meta.origin == origin && record.cancel_now() {
                asked += 1;
            }
        }
        asked
    }

    /// Acknowledge a published terminal event so the record can be reaped (plan §7 step 6's
    /// `note_completion`). `true` when the job is known and already terminal.
    pub fn note_completion(&self, id: JobId) -> bool {
        let mut inner = lock(&self.inner);
        inner.jobs.get_mut(&id.as_u64()).is_some_and(|r| {
            let terminal = !r.running;
            // Only a terminal job is acknowledged: a running one is still going to publish its own
            // event, and a stray call must not mark it reappable.
            if terminal {
                r.acknowledged = true;
            }
            terminal
        })
    }

    /// Drop a record now, without waiting for its worker. `true` when a record was removed.
    pub fn forget(&self, id: JobId) -> bool {
        lock(&self.inner).jobs.remove(&id.as_u64()).is_some()
    }

    /// A snapshot of every record, live and retained.
    #[must_use]
    pub fn list(&self) -> Vec<JobInfo> {
        let inner = lock(&self.inner);
        let now = Instant::now();
        inner.jobs.values().map(|r| r.info(now)).collect()
    }

    /// `true` while any job is still running — `Session::wants_repaint`'s basis (plan F14).
    #[must_use]
    pub fn has_live(&self) -> bool {
        lock(&self.inner).jobs.values().any(|r| r.running)
    }

    /// How many jobs are running (the bounded-concurrency check, plan §5.6 `concurrent_jobs`).
    #[must_use]
    pub fn live_count(&self) -> usize {
        lock(&self.inner)
            .jobs
            .values()
            .filter(|r| r.running)
            .count()
    }

    /// Ask every running job to stop. Returns how many were asked.
    pub fn cancel_all(&self) -> usize {
        let mut inner = lock(&self.inner);
        let mut asked = 0;
        for record in inner.jobs.values_mut() {
            if record.cancel_now() {
                asked += 1;
            }
        }
        asked
    }

    /// Join every job whose thread has already ended. Never waits, so a hung job cannot hang shutdown.
    pub fn reap_finished(&self) {
        let mut inner = lock(&self.inner);
        for record in inner.jobs.values_mut() {
            record.join_if_done();
        }
    }

    /// One record by id, for the "did my job finish" check a widget does outside of `poll`.
    #[must_use]
    pub fn info(&self, id: JobId) -> Option<JobInfo> {
        let inner = lock(&self.inner);
        inner.jobs.get(&id.as_u64()).map(|r| r.info(Instant::now()))
    }

    /// Remove every retained-but-terminal record, returning their ids.
    pub fn clear_finished(&self) -> Vec<JobId> {
        let mut inner = lock(&self.inner);
        let ids: Vec<u64> = inner
            .jobs
            .iter()
            .filter(|(_, r)| !r.running)
            .map(|(id, _)| *id)
            .collect();
        for id in &ids {
            if let Some(mut record) = inner.jobs.remove(id) {
                record.join_if_done();
            }
        }
        ids.into_iter().map(JobId).collect()
    }

    /// The ids of every record.
    #[must_use]
    pub fn ids(&self) -> Vec<JobId> {
        lock(&self.inner).jobs.keys().copied().map(JobId).collect()
    }
}

impl Drop for JobRegistry {
    fn drop(&mut self) {
        // Backstop for a dropped session: ask everything to stop and join what has already ended.
        // `Session::close` is the polite version (cancel + a bounded wait for the threads to land).
        {
            let mut inner = lock(&self.inner);
            for record in inner.jobs.values_mut() {
                record.cancel_now();
            }
        }
        self.reap_finished();
    }
}

impl JobRecord {
    fn cancel_now(&mut self) -> bool {
        if !self.running {
            return false;
        }
        self.token.cancel()
    }

    /// Join the worker if it has ended. `true` once it has been joined (idempotent).
    fn join_if_done(&mut self) -> bool {
        let Some(join) = self.join.as_ref() else {
            return true;
        };
        if !join.is_finished() {
            return false;
        }
        let Some(join) = self.join.take() else {
            return true;
        };
        // A worker that panicked reports `Err`; the terminal event for that case is the synthesised
        // `Failed` one, so the join result is nothing the table needs to keep beyond the fact that the
        // thread is gone.
        let _ = join.join();
        true
    }

    fn publish(&mut self, id: JobId, ev: &JobEvent) -> EngineEvent {
        let (outcome, payload, error) = match ev {
            JobEvent::Progress(_) => (JobOutcome::done(String::new()), JobPayload::None, None),
            JobEvent::Finished { outcome, payload } => {
                // A cancelled or timed-out job has a typed error too, even though it came through the
                // `Finished` channel: the UI's toast needs `CoreError::Cancelled`, not just a status.
                let err = match outcome.status {
                    JobStatus::Cancelled => Some(Arc::new(CoreError::Cancelled)),
                    JobStatus::TimedOut => Some(Arc::new(CoreError::Sql {
                        msg: outcome.error.clone().unwrap_or_default(),
                    })),
                    _ => None,
                };
                (outcome.clone(), payload.clone(), err)
            }
            JobEvent::Failed(e) => (
                JobOutcome::failed(String::new(), e),
                JobPayload::None,
                Some(Arc::clone(e)),
            ),
        };
        self.outcome = Some(outcome.clone());
        self.error = error.clone();
        EngineEvent::JobFinished {
            id,
            kind: self.meta.kind,
            origin: self.meta.origin,
            outcome,
            payload,
            error,
        }
    }

    fn info(&self, now: Instant) -> JobInfo {
        let state = if self.running {
            "running"
        } else {
            self.outcome
                .as_ref()
                .map_or("finished", |o| o.status.as_str())
        };
        JobInfo {
            id: self.meta.id,
            kind: self.meta.kind,
            label: self.label.clone(),
            origin: self.meta.origin,
            state,
            progress: self
                .last_progress
                .as_ref()
                .and_then(JobProgress::fraction)
                .unwrap_or(0.0),
            message: self
                .last_progress
                .as_ref()
                .and_then(|p| p.current.clone())
                .unwrap_or_default(),
            elapsed_ms: now.duration_since(self.started).as_secs_f64() * 1_000.0,
            error: self.error.as_ref().map(|e| e.user_message()),
        }
    }
}

/// The plan's `JobInfo` row for the jobs pane.
#[derive(Clone, Debug, PartialEq)]
pub struct JobInfo {
    /// The job.
    pub id: JobId,
    /// Its kind.
    pub kind: JobKind,
    /// Human label.
    pub label: String,
    /// The widget that asked.
    pub origin: Origin,
    /// `running` / `done` / `partial` / `failed` / `cancelled` / `timeout`.
    pub state: &'static str,
    /// Last reported fraction; `0.0` when the job has no known total.
    pub progress: f32,
    /// The current item's name.
    pub message: String,
    /// Wall time so far.
    pub elapsed_ms: f64,
    /// Rendered failure text.
    pub error: Option<String>,
}

impl JobInfo {
    /// `true` while the job is still running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.state == "running"
    }
}

/// `futures`-free driver for a [`PlatformFuture`], **worker threads only**.
///
/// Every `Env` implementation in this crate does its work inside one `poll` ("they do their work when
/// polled and never spawn a thread", `platform/mod.rs`), so a one-shot poll is all a job needs. If a
/// future ever returns `Pending`, this yields up to [`BLOCK_ON_POLL_BUDGET`] times and then refuses
/// with [`CoreError::Unsupported`] rather than spinning — the honest answer to "this needs a real
/// executor", which this crate does not have (plan §9.1: no `tokio`, the only executor is in the
/// binary).
pub fn block_on<T>(mut fut: PlatformFuture<'_, T>) -> Result<T, CoreError> {
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    for _ in 0..BLOCK_ON_POLL_BUDGET {
        if let std::task::Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return Ok(v);
        }
        std::thread::yield_now();
    }
    Err(CoreError::Unsupported {
        what: "a platform future needing more than one poll requires an executor; \
               jobs::block_on is a worker-thread driver, not a scheduler (plan §9.1)",
    })
}

/// Polls [`block_on`] spends before refusing. Far past any real implementation's single poll.
const BLOCK_ON_POLL_BUDGET: usize = 4_096;

/// Lock, ignoring poisoning: everything behind these mutexes is either fully updated or not at all, so
/// one job's panic must not take the table — and with it the session — down.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// Step 16b (`crash_guard` polish, plan §5.9): a panicking job closure becomes a typed
    /// `Failed` terminal event — headless, through the public `spawn` seam, with a real panic.
    #[test]
    fn a_panicking_job_becomes_a_typed_failure_not_a_lost_thread() {
        let spec = JobSpec::new(
            JobId(999),
            JobKind::List,
            "panic probe".to_string(),
            Origin(1),
        );
        let (_handle, rx) = spawn(
            spec,
            |_cancel, _progress| -> Result<JobResult<()>, CoreError> {
                panic!("deliberate job panic from a test");
            },
        );
        // The flush of the final counters may precede the terminal event; drain to the terminal one.
        let event = loop {
            let ev = rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("the worker sends a terminal event even when it panics");
            if matches!(ev, JobEvent::Failed(_)) {
                break ev;
            }
        };
        match event {
            JobEvent::Failed(err) => {
                let msg = err.user_message();
                assert!(msg.contains("panicked"), "{msg}");
                assert!(msg.contains("deliberate job panic"), "{msg}");
            }
            other => panic!("a panic must surface as Failed, got {other:?}"),
        }
    }

    fn spec(reg: &JobRegistry, kind: JobKind) -> JobSpec {
        JobSpec::new(reg.next_id(), kind, "test", Origin(7))
    }

    /// Poll until `pred` holds, bounded so a regression fails instead of hanging.
    fn wait_for(reg: &JobRegistry, pred: impl Fn(&[EngineEvent]) -> bool) -> Vec<EngineEvent> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut all = Vec::new();
        while Instant::now() < deadline {
            all.extend(reg.poll());
            if pred(&all) {
                return all;
            }
            std::thread::yield_now();
        }
        panic!("job events never arrived; got {all:?}");
    }

    fn saw_finish(evs: &[EngineEvent]) -> bool {
        evs.iter()
            .any(|e| matches!(e, EngineEvent::JobFinished { .. }))
    }

    #[test]
    fn finished_job_reports_its_payload_once() {
        let reg = JobRegistry::new();
        let s = spec(&reg, JobKind::DirSize);
        reg.attach(s, |_c, p| {
            p.add(10, 1, Some("f".into()));
            Ok(JobResult::done((), "ok"))
        });
        let events = wait_for(&reg, saw_finish);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, EngineEvent::JobStarted { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| saw_finish(std::slice::from_ref(e)))
                .count(),
            1,
            "exactly one terminal event: {events:?}"
        );
        assert!(!reg.has_live());
    }

    #[test]
    fn cancel_after_completion_is_not_a_cancellation() {
        let reg = JobRegistry::new();
        let id = reg.attach(spec(&reg, JobKind::List), |_c, _p| {
            Ok(JobResult::done((), "fast"))
        });
        wait_for(&reg, saw_finish);
        assert!(
            !reg.cancel(id),
            "a completed job must not be classified as cancelled (plan step 6)"
        );
        assert!(!reg.list()[0].is_running());
        assert_eq!(reg.list()[0].state, "done");
    }

    #[test]
    fn cancel_takes_effect_and_emits_one_terminal_event() {
        let reg = JobRegistry::new();
        let id = reg.attach(spec(&reg, JobKind::DirSize), |cancel: &CancelToken, _p| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !cancel.is_cancelled() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            cancel.check()?;
            Ok(JobResult::done((), "never cancelled"))
        });
        // Let the worker reach its loop before asking.
        while !reg.has_live() {
            let _ = reg.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(reg.cancel(id), "a running job cancels");
        let events = wait_for(&reg, saw_finish);
        let fin = events
            .iter()
            .find_map(|e| match e {
                EngineEvent::JobFinished { outcome, error, .. } => {
                    Some((outcome.status, error.as_ref().map(|e| e.to_string())))
                }
                _ => None,
            })
            .expect("a terminal event");
        assert_eq!(fin.0, JobStatus::Cancelled);
        assert!(fin.1.is_some(), "the typed error rides on the event");
        // A second cancel of the now-terminal job changes nothing.
        assert!(!reg.cancel(id));
    }

    #[test]
    fn progress_is_coalesced_and_never_follows_the_terminal_event() {
        let reg = JobRegistry::new();
        reg.attach(spec(&reg, JobKind::Paste), |_c, p| {
            for i in 0..2_000 {
                p.add(1, 1, Some(format!("item {i}")));
            }
            Ok(JobResult::done((), "done"))
        });
        let events = wait_for(&reg, saw_finish);
        let last = events
            .iter()
            .rposition(|e| saw_finish(std::slice::from_ref(e)))
            .expect("finished");
        assert!(
            !events[last + 1..]
                .iter()
                .any(|e| matches!(e, EngineEvent::JobProgress { .. })),
            "no progress after the terminal event"
        );
        let best = events
            .iter()
            .filter_map(|e| match e {
                EngineEvent::JobProgress { progress, .. } => Some(progress.files_done),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        assert!(best > 0, "counters reached the table: {events:?}");
        assert!(
            events
                .iter()
                .filter(|e| matches!(e, EngineEvent::JobProgress { .. }))
                .count()
                < 2_000,
            "the throttle/coalescing kept the event count down"
        );
    }

    #[test]
    fn watchdog_turns_a_hung_job_into_a_terminal_timeout() {
        let reg = JobRegistry::new();
        let mut s = spec(&reg, JobKind::Extract);
        s.budget = Some(Duration::from_millis(5));
        reg.attach(s, |_c, _p| {
            std::thread::sleep(Duration::from_secs(30));
            Ok(JobResult::done((), "too late"))
        });
        let events = wait_for(&reg, saw_finish);
        let status = events
            .iter()
            .find_map(|e| match e {
                EngineEvent::JobFinished { outcome, .. } => Some(outcome.status),
                _ => None,
            })
            .expect("terminal");
        assert_eq!(status, JobStatus::TimedOut);
    }

    #[test]
    fn cancel_origin_takes_only_its_own_jobs() {
        let reg = JobRegistry::new();
        // Each worker announces itself through this counter instead of a barrier, so the test can wait
        // for all three to be *inside* their loop before cancelling one origin.
        let arrived = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for o in [1_u64, 1, 2] {
            let a = Arc::clone(&arrived);
            let mut s = spec(&reg, JobKind::Delete);
            s.origin = Origin(o);
            reg.attach(s, move |c, _p| {
                a.fetch_add(1, Ordering::Release);
                let deadline = Instant::now() + Duration::from_secs(5);
                while !c.is_cancelled() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(2));
                }
                c.check()?;
                Ok(JobResult::done((), "x"))
            });
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while arrived.load(Ordering::Acquire) < 3 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            arrived.load(Ordering::Acquire),
            3,
            "every worker reached its loop"
        );
        assert_eq!(reg.cancel_origin(Origin(1)), 2, "both origin-1 jobs only");
        let infos = reg.list();
        let live = infos
            .iter()
            .filter(|i| i.origin == Origin(2) && i.is_running())
            .count();
        assert_eq!(live, 1, "the other origin is untouched");
        let _ = reg.cancel_all();
    }

    #[test]
    fn a_lost_worker_still_terminates_its_job() {
        let reg = JobRegistry::new();
        reg.attach(spec(&reg, JobKind::Import), |_c, _p| {
            // The plan's never-crash rule protects the *process* (the binary installs `crash_guard`);
            // what the table must guarantee is that a dead worker cannot strand a spinner.
            intentionally_panicking();
            #[allow(unreachable_code)]
            Ok(JobResult::done((), "unreachable"))
        });
        let events = wait_for(&reg, saw_finish);
        let status = events
            .iter()
            .find_map(|e| match e {
                EngineEvent::JobFinished { outcome, .. } => Some(outcome.status),
                _ => None,
            })
            .expect("terminal");
        assert_eq!(status, JobStatus::Failed);
    }

    #[test]
    #[should_panic(expected = "intentional job panic")]
    fn intentionally_panicking_is_a_panic() {
        intentionally_panicking();
    }

    #[allow(clippy::panic)] // the subject of two tests above is precisely a panicking worker
    fn intentionally_panicking() {
        panic!("intentional job panic");
    }

    #[test]
    fn block_on_drives_a_ready_future_and_refuses_a_stuck_one() {
        let v = block_on(crate::platform::ready(7_u8)).expect("a ready future completes");
        assert_eq!(v, 7);
        struct Stuck;
        impl std::future::Future for Stuck {
            type Output = ();
            fn poll(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<()> {
                std::task::Poll::Pending
            }
        }
        let err = block_on(Box::pin(Stuck)).expect_err("refused");
        assert!(matches!(err, CoreError::Unsupported { .. }), "{err:?}");
    }

    #[test]
    fn ids_never_repeat_and_retention_is_bounded() {
        let reg = JobRegistry::with_terminal_ttl(Duration::ZERO);
        let a = reg.next_id();
        assert_ne!(a, reg.next_id());
        let id = reg.attach(JobSpec::new(a, JobKind::List, "l", Origin(1)), |_c, _p| {
            Ok(JobResult::done((), "x"))
        });
        wait_for(&reg, saw_finish);
        assert!(reg.note_completion(id));
        // TTL 0 plus the acknowledgement reaps it on the next poll.
        let _ = reg.poll();
        assert!(reg.ids().iter().all(|i| *i != id), "record reaped");
        assert!(!reg.note_completion(id), "the record is gone");
    }

    #[test]
    fn note_completion_on_a_live_job_reports_false() {
        let reg = JobRegistry::new();
        let id = reg.attach(spec(&reg, JobKind::Revert), |c, _p| {
            while !c.is_cancelled() {
                std::thread::sleep(Duration::from_millis(2));
            }
            c.check()?;
            Ok(JobResult::done((), "x"))
        });
        assert!(!reg.note_completion(id), "still running");
        let _ = reg.cancel(id);
        wait_for(&reg, saw_finish);
        assert!(reg.note_completion(id));
    }
}
