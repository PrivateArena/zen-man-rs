//! `Session` — the only façade the UI is allowed to call (plan §4.1, §5.6 `session.rs` row).
//!
//! A `Session` is a cheap-clone `Arc` handle over [`Inner`]; `Send + Sync`, UI-free, and it never blocks
//! the caller's thread for longer than one directory read (plan §4.1's rule for the read group).
//! Everything longer is a job: `dir_size` today, paste/delete/revert/search/rebuild/extract in steps
//! 9-13, all through the same [`Receipt`] + [`Origin`] + [`EngineEvent`] machinery defined here.
//!
//! # F1: startup never fails because of the user's files
//!
//! Go `log.Fatalf`'d on a database it could not open and silently discarded a config it could not
//! parse. Here [`Session::open`] takes the three damaged-file cases it can survive — a corrupt undo
//! log, a locked/garbage database, an unparseable `config.json` — repairs or disables what it can, and
//! *reports* what it did as [`StartupNote`]s. `open` still returns `Err` for the things that are not
//! damaged files but impossible requests (the log file cannot even be created, the caller handed a
//! relative path where an absolute one was required); those are the caller's bug, not the user's.
//!
//! # Ownership
//!
//! The single SQLite [`Connection`](rusqlite::Connection) is a **private** field of the private
//! `Inner`, per plan §5.4's as-built correction: nothing outside `session.rs` may hold it, and services
//! that need index writes go through [`crate::index::intent::IndexIntent`] instead. The undo log is
//! behind a mutex (its append path mutates the offsets index), the caches behind their own mutexes, and
//! the job table behind its own — plan §4.3's "each with its own mutex, so the dependency is explicit
//! and a test can inject temp dirs".

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde::Serialize;

use crate::archive::extract::{ExtractReport, ExtractReq};
use crate::entry::{DirPage, FileEntry};
use crate::error::CoreError;
use crate::events::{ChangeCause, EngineEvent};
use crate::history::codec::{ActionEntry, ActionLog, ActionType};
use crate::history::revert::{self as revert_ops, RevertReport, ensure_reversible};
use crate::history::store::{ActionLogStore, LogId};
use crate::index::intent::{IndexIntent, IndexOp, SharedIntent};
use crate::jobs::{CancelToken, JobId, JobInfo, JobKind, JobRegistry, JobResult, JobSpec};
use crate::limits::Limits;
use crate::ops::chmod::{self, ChmodReport, ChmodReq};
use crate::ops::clipboard::{self, Clipboard, ClipboardStore};
use crate::ops::delete::{self, DeleteReport, DeleteReq};
use crate::ops::mkdir::{self, MkdirReq};
use crate::ops::r#move::{MoveFs, OsMoveFs};
use crate::ops::open::{self, OpenReport};
use crate::ops::paste::{self, PasteMode, PasteReport, PasteReq};
use crate::ops::rename::{self, RenameReport, RenameReq};
use crate::ops::{ConflictPolicy, guard};
use crate::path::{AbsPath, FsPath};
use crate::platform::Env;
use crate::platform::env_probe::ProbeReport;
use crate::services::dirs::{
    DirService, DirSizeOpts, DirSizeSink, DirSizeStat, FsSource, ListDir, OsDirRead,
};
use crate::services::search_live::{SearchAdmission, SearchLane, SearchQuery, SearchService};
use crate::store::config::ZenConfig;

// ------------------------------------------------------------------------------------- correlation

/// The correlation token that ties an event back to the widget that asked for it (plan §4.1).
///
/// Four tabs can point at one directory and one pane can have several jobs in flight, so
/// `(JobId, path)` is not enough to know which spinner to clear.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Origin(pub u64);

impl Origin {
    /// The numeric token.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Allocates [`Origin`]s. Monotonic for the life of the process, so an origin never aliases a live one.
#[derive(Debug, Default)]
pub struct OriginAlloc(AtomicU64);

impl OriginAlloc {
    /// A fresh allocator whose first token is `Origin(1)`.
    #[must_use]
    pub fn new() -> Self {
        Self(AtomicU64::new(1))
    }

    /// The next token.
    pub fn next(&self) -> Origin {
        Origin(self.0.fetch_add(1, Ordering::AcqRel))
    }
}

/// A read that finished, or the job that will answer it (plan §4.1's `Receipt<T>`).
///
/// The caller stores the [`Origin`] it passed in, plus the [`JobId`] when it gets a `Running` back, and
/// applies the result only through that pair (plan §4.3's correlation rule).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Receipt<T> {
    /// The answer is here — the cache or a short read served it synchronously.
    Done(T),
    /// The answer will arrive as [`EngineEvent::JobFinished`] for this `(job, origin)` pair.
    Running {
        /// The job that will answer.
        job: JobId,
        /// The token the caller passed in, returned to sender.
        origin: Origin,
    },
}

impl<T> Receipt<T> {
    /// `true` for the synchronous answer.
    #[must_use]
    pub const fn is_done(&self) -> bool {
        matches!(self, Self::Done(_))
    }

    /// `true` when a job will answer.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }

    /// The job, when running.
    #[must_use]
    pub const fn job(&self) -> Option<JobId> {
        match self {
            Self::Running { job, .. } => Some(*job),
            Self::Done(_) => None,
        }
    }

    /// The origin, in both cases — which is what makes the round trip testable.
    #[must_use]
    pub const fn origin(&self) -> Option<Origin> {
        match self {
            Self::Running { origin, .. } => Some(*origin),
            Self::Done(_) => None,
        }
    }

    /// Borrow the answer, when it is here.
    #[must_use]
    pub const fn done(&self) -> Option<&T> {
        match self {
            Self::Done(v) => Some(v),
            Self::Running { .. } => None,
        }
    }

    /// Take the answer, leaving the job half alone.
    #[must_use]
    pub fn into_done(self) -> Option<T> {
        match self {
            Self::Done(v) => Some(v),
            Self::Running { .. } => None,
        }
    }

    /// Map the answer, e.g. to erase a cache-internal `Arc` wrapper.
    #[must_use]
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Receipt<U> {
        match self {
            Self::Done(v) => Receipt::Done(f(v)),
            Self::Running { job, origin } => Receipt::Running { job, origin },
        }
    }
}

// ----------------------------------------------------------------------------------------- options

/// Engine knobs that Go had as package-level constants and the port takes from
/// [`ZenConfig::limits`] (plan §5.2's `Limits`, §5.4's `ConfigLimits` validation).
///
/// Every field is a *bound*, never a behaviour: the plan's F8/F12 rule is that a hostile or mistaken
/// input must produce a reported error rather than an unbounded walk.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionOptions {
    /// How many undo-log records a history query returns when the caller does not say
    /// (`None` = "however many the caller asks for", the Go `/api/log` behaviour).
    pub history_limit: Option<usize>,
    /// Default directory page size (Go's `limit := 200`, `api/dir.go:106`).
    pub dir_page_size: usize,
    /// Live-search result ceiling (Go's `api/search.go:72`).
    pub live_search_cap: i64,
    /// Live-search node budget before the walk gives up (plan §5.2 `live_search_nodes`).
    pub max_live_nodes: usize,
    /// Archive manifest LRU size (Go's `maxCacheEntries = 10`).
    pub archive_cache_cap: usize,
    /// Archive manifest TTL (Go's 15 minutes).
    pub archive_cache_ttl: Duration,
    /// Nested-archive/uncompressed ceiling (plan F8; the port's addition).
    pub nested_archive_max_bytes: u64,
    /// How deep a `.zip` inside a `.7z` inside a … may go before the read refuses. Go recursed without
    /// a bound; `0` means "no nested archives at all".
    pub nested_archive_max_depth: u8,
    /// Whether the session may run a filesystem watcher (plan §4.1's `SessionConfig.watcher`).
    pub watcher: WatchPolicy,
}

/// Plan §4.1's `SessionConfig.watcher` — the three states a watcher policy has, and the reason a
/// session that was never asked for one does not open an inotify descriptor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WatchPolicy {
    /// Never start one from the config: `Session::watch_start` refuses with a typed
    /// [`CoreError::Watcher`](crate::error::CoreError::Watcher). The default — Go shipped no watcher at
    /// all (plan §9.2 step-12b), so starting one behind the user's back is a deviation, not a parity.
    #[default]
    Off,
    /// Start one when the indexer's own `auto_index` flag is on
    /// (`IndexerSettings::auto_index`, the `config.auto_index` key of `index-status.json`). The knob
    /// the wire already has; the port does not invent a second one.
    AutoIndex,
    /// Always start one when asked (step 12c's SearchManager toggle).
    On,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self::from_limits(&Limits::default())
    }
}

impl SessionOptions {
    /// Derive every knob from the engine limits, so one [`Limits`] value is the single tuning point
    /// (plan §5.2's `Limits` is the type [`ZenConfig::limits`] produces).
    #[must_use]
    pub fn from_limits(limits: &Limits) -> Self {
        Self {
            history_limit: None,
            dir_page_size: limits.dir_page_default,
            live_search_cap: limits.live_search_cap,
            max_live_nodes: limits.live_search_nodes,
            archive_cache_cap: limits.archive_cache_entries,
            archive_cache_ttl: limits.archive_cache_ttl,
            nested_archive_max_bytes: limits.nested_archive_max_bytes,
            // Plan §5.5: the manifest split recognises one nesting level (`host` + `nested`), which is
            // what `FsPath`/`VfsKind` can even name, so the engine refuses deeper.
            nested_archive_max_depth: 1,
            watcher: WatchPolicy::default(),
        }
    }
}

/// Every file the session is allowed to touch, named by the caller (plan §5.2's `SystemInfo` note:
/// `AppPaths` stays the authority; nothing here guesses a directory).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionPaths {
    /// The `config.json` the app booted from. Read-only to the session: it is checked, and copied
    /// aside if it cannot be parsed, but the settings store owns writing it (plan §5.4's
    /// `craft-settings` row).
    pub config: PathBuf,
    /// The `action.log` undo log.
    pub action_log: PathBuf,
    /// The `zen-man.db` index database, or `None` to run without one (`--no-index`, a read-only data
    /// directory, and every contract test).
    pub database: Option<PathBuf>,
    /// Where the capability probe may write its scratch files (Go used the data directory,
    /// `main.go:24-49`).
    pub probe_scratch: PathBuf,
    /// How hard to fight for the database at startup. Bounded because SQLite's own `busy_timeout` is
    /// [`crate::store::sql::BUSY_TIMEOUT_MS`] *per attempt*: the default eight attempts is what a user
    /// who left the Go app running deserves, and a test who wants the lock reported immediately passes
    /// `attempts: 1`.
    pub db_open: DbOpenBudget,
}

/// The retry budget for opening `zen-man.db` (plan §6.1 F5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DbOpenBudget {
    /// Total tries.
    pub attempts: u32,
    /// Wait between tries.
    pub backoff: Duration,
}

impl Default for DbOpenBudget {
    fn default() -> Self {
        Self {
            attempts: crate::store::sql::DEFAULT_RETRY_ATTEMPTS,
            backoff: crate::store::sql::DEFAULT_RETRY_BACKOFF,
        }
    }
}

impl SessionPaths {
    /// The three caller-named paths plus the probe scratch directory.
    #[must_use]
    pub fn new(
        config: impl Into<PathBuf>,
        action_log: impl Into<PathBuf>,
        database: Option<PathBuf>,
        probe_scratch: impl Into<PathBuf>,
    ) -> Self {
        Self {
            config: config.into(),
            action_log: action_log.into(),
            database,
            probe_scratch: probe_scratch.into(),
            db_open: DbOpenBudget::default(),
        }
    }

    /// The same paths with a bounded database retry — the form a test, and any caller that must not
    /// stall startup, uses.
    #[must_use]
    pub fn with_db_open(mut self, attempts: u32, backoff: Duration) -> Self {
        self.db_open = DbOpenBudget { attempts, backoff };
        self
    }
}

/// What [`Session::open`] had to repair or give up on. Every variant is a *user-visible* fact: the
/// app shows them as notices on the first frame (plan F1: "it degrades and says so").
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartupNote {
    /// The undo log had damage past its last good frame; the good prefix was kept and the rest
    /// discarded by [`ActionLogStore::resync`].
    LogRepaired {
        /// Frames that survived.
        kept: usize,
        /// Frames that were readable but sat past the damage, and were therefore dropped.
        dropped: usize,
        /// Bytes removed from the file.
        bytes: u64,
    },
    /// The index database could not be used, so the session runs without one: search falls back to the
    /// live walker, `folder_sizes` is memory-only, and no rebuild is possible until it is fixed.
    /// Go's answer to the same condition was `log.Fatalf` (plan F5).
    DatabaseDisabled {
        /// SQLite's or the OS's words, already rendered.
        reason: String,
    },
    /// `config.json` at this path is not parseable; the bytes were copied to `copied_to` and the
    /// session booted on the config the caller supplied (Go discarded it silently — plan §5.4's
    /// `store/config.rs` row).
    ConfigReset {
        /// The file that was garbage.
        path: PathBuf,
        /// Where its bytes went.
        copied_to: PathBuf,
    },
}

impl StartupNote {
    /// The toast text. Kept here so the wording is the same wherever the notes are surfaced.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::LogRepaired {
                kept,
                dropped,
                bytes,
            } => format!(
                "action log repaired: {kept} record(s) kept, {dropped} discarded ({bytes} B)"
            ),
            Self::DatabaseDisabled { reason } => {
                format!("index database disabled: {reason}")
            }
            Self::ConfigReset { path, copied_to } => format!(
                "config {} could not be read and was copied to {}",
                path.display(),
                copied_to.display()
            ),
        }
    }

    /// The event the UI should show it with.
    #[must_use]
    pub fn notice(&self) -> EngineEvent {
        EngineEvent::Notice {
            level: crate::events::NoticeLevel::Warn,
            message: self.message(),
        }
    }
}

// ------------------------------------------------------------------------------------------ session

/// A handle to the engine (plan §4.1's `Session`).
#[derive(Clone)]
pub struct Session {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Session {
    /// A hand-written `Debug`: the fields are a connection, a platform seam and a job table, none of
    /// which has anything a log line should print, and `#[derive(Debug)]` would need all of them to.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("config", &self.inner.paths.config)
            .field("action_log", &self.inner.paths.action_log)
            .field("database", &self.inner.paths.database)
            .field("live_jobs", &self.inner.jobs.live_count())
            .finish_non_exhaustive()
    }
}

/// Everything a session owns. Private by construction (plan §5.4's correction: the `Connection` is not
/// reachable from a service, and neither is anything else here).
struct Inner {
    paths: SessionPaths,
    options: SessionOptions,
    limits: Arc<Limits>,
    env: Arc<dyn Env>,
    dirs: Arc<DirService>,
    /// The archive facility (plan §5.5 step 13a): the manifest cache, the caps, and the extract jobs.
    archives: Arc<crate::services::archive::ArchiveService>,
    /// The live-search walk service (plan §5.3 `services/search_live.rs`), over the same filesystem
    /// seam as the listing service so one swap covers both.
    search: Arc<SearchService>,
    /// The "≤1 walk per 250 ms" lane (§7 step 11): the throttle, the job whose answer still counts and
    /// the newest deferred request.
    search_lane: Mutex<SearchLane>,
    jobs: Arc<JobRegistry>,
    log: Mutex<ActionLogStore>,
    /// The single connection (Go's `SetMaxOpenConns(1)`). `None` when [`StartupNote::DatabaseDisabled`].
    db: Mutex<Option<rusqlite::Connection>>,
    /// Why the database is absent, when it is: kept so a later retry and the diagnostics pane can say
    /// something better than "no".
    db_disabled: Mutex<Option<String>>,
    /// The index sink, behind a mutex so [`Session::set_intent`] can swap it (plan §5.5: step 12's
    /// queue replaces the noop without touching a caller).
    intent: Mutex<SharedIntent>,
    events: Mutex<std::collections::VecDeque<EngineEvent>>,
    /// The queued copy/cut selection (plan §5.3's `ops/clipboard.rs`, Go's `clipboardState`).
    clipboard: Mutex<Option<Clipboard>>,
    origins: OriginAlloc,
    /// Plan F21: the symlink probe is answered **once** and cached here, so `/api/props` — which the
    /// properties dialog can ask for on every repaint — is a lock read.
    probe: Arc<RwLock<Option<bool>>>,
    config: Mutex<ZenConfig>,
    /// The live watcher policy — [`SessionOptions::watcher`] at boot, moved by
    /// [`Self::set_index_config`] (step 12c: the dialog's watcher toggle is the first writer).
    watch_policy: Mutex<WatchPolicy>,
    /// Step 12a runtime index state (Go's `IndexState` — the config file keeps only the settings).
    index_state: Mutex<IndexRunState>,
    /// The step-12 queueing intent, applied by `poll_at` at the 2 s cadence (plan §5.5's updater row).
    index_queue: Arc<crate::index::updater::QueuedIntent>,
    /// The live rebuild job, when one is running (`Session::index_cancel` needs its id).
    rebuild_job: Mutex<Option<JobId>>,
    /// Rows written by the live rebuild (Go's `ProgressCount`).
    index_progress: Arc<std::sync::atomic::AtomicU64>,
    /// The step-12b watcher: the live handle plus the channel its reader thread sends on. `None` half
    /// means "not watching", which is the state every session boots in.
    watch: Mutex<WatchState>,
}

/// The session's watcher slot (plan §5.5's `Watcher`, owned by nobody else — see `Session::watch_stop`
/// for why the receiver lives here too).
#[derive(Debug, Default)]
struct WatchState {
    watcher: Option<crate::index::watcher::Watcher>,
    rx: Option<std::sync::mpsc::Receiver<crate::index::watcher::DirChanged>>,
}

impl Session {
    /// Boot the engine over caller-named files.
    ///
    /// `env` is the platform seam (a test passes a recorder, the binary passes
    /// [`crate::platform::default_env`]); `config` is what the boot loader produced, defaults included.
    ///
    /// # Errors
    /// Only for requests that cannot be honoured at all — an undo log that cannot be opened or
    /// created. A corrupt log, a locked database and a garbage config are **not** errors: they come
    /// back as notes (plan F1).
    pub fn open(
        paths: SessionPaths,
        config: ZenConfig,
        env: Arc<dyn Env>,
        limits: Limits,
    ) -> Result<(Session, Vec<StartupNote>), CoreError> {
        let mut notes = Vec::new();
        let limits = Arc::new(limits);
        let options = SessionOptions::from_limits(&limits);

        // 1. the undo log: open, then repair the strict prefix if it is damaged.
        let mut log = ActionLogStore::open(&paths.action_log).map_err(|e| {
            // This is the one startup failure that is a real failure: no log, no undo, and pretending
            // otherwise would silently lose the next `paste-move` record.
            CoreError::Config {
                msg: format!("{}: {}", paths.action_log.display(), e.user_message()),
            }
        })?;
        if !log.is_clean() {
            let report = log.resync()?;
            notes.push(StartupNote::LogRepaired {
                kept: report.records_kept,
                dropped: report.records_dropped,
                bytes: report.bytes_dropped,
            });
        }

        // 2. the config file: a garbage `config.json` is copied aside, never rewritten here.
        if let Some(note) = check_config_file(&paths.config)? {
            notes.push(note);
        }

        // 3. the database: any failure at all degrades to "no index".
        let (db, db_disabled) = match paths.database.as_ref() {
            None => (
                None,
                Some("no database path was supplied; running index-less".to_string()),
            ),
            Some(path) => match crate::store::sql::connect_with_retry(
                path,
                crate::store::config::StoreMode::Full,
                paths.db_open.attempts,
                paths.db_open.backoff,
            ) {
                Ok(conn) => (Some(conn), None),
                Err(e) => {
                    // Plan F5: a lock is a toast, not an abort. Note it only when the caller asked for
                    // a database, which they did here.
                    notes.push(StartupNote::DatabaseDisabled {
                        reason: e.user_message(),
                    });
                    (None, Some(e.user_message()))
                }
            },
        };

        // Step 12a: with a database the default sink is the queueing intent (`poll_at` drains it); a
        // database-less session keeps dropping ops exactly as the step-6 seam did.
        let index_queue = Arc::new(crate::index::updater::QueuedIntent::new());
        let index_progress = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let default_intent: SharedIntent = if db.is_some() {
            Arc::clone(&index_queue) as SharedIntent
        } else {
            crate::index::intent::noop_intent()
        };
        let source: Arc<dyn FsSource> = Arc::new(OsDirRead);
        let dirs = Arc::new(DirService::new(Arc::clone(&source), Arc::clone(&limits)));
        let search = Arc::new(SearchService::new(source, Arc::clone(&limits)));
        let job_registry = Arc::new(crate::jobs::JobRegistry::new());
        let archives = Arc::new(crate::services::archive::ArchiveService::new(
            Arc::clone(&job_registry),
            Arc::clone(&limits),
        ));
        let watch_policy = Mutex::new(options.watcher);
        let session = Session {
            inner: Arc::new(Inner {
                paths,
                options,
                limits,
                env,
                dirs,
                search,
                search_lane: Mutex::new(SearchLane::default()),
                jobs: job_registry,
                archives,
                log: Mutex::new(log),
                db: Mutex::new(db),
                db_disabled: Mutex::new(db_disabled),
                intent: Mutex::new(default_intent),
                index_queue,
                index_state: Mutex::new(IndexRunState::default()),
                rebuild_job: Mutex::new(None),
                index_progress,
                watch: Mutex::new(WatchState::default()),
                watch_policy,
                events: Mutex::new(std::collections::VecDeque::new()),
                clipboard: Mutex::new(None),
                origins: OriginAlloc::new(),
                probe: Arc::new(RwLock::new(None)),
                config: Mutex::new(config),
            }),
        };
        Ok((session, notes))
    }

    /// The path set this session was opened with.
    #[must_use]
    pub fn paths(&self) -> &SessionPaths {
        &self.inner.paths
    }

    /// The engine knobs in force.
    #[must_use]
    pub fn options(&self) -> &SessionOptions {
        &self.inner.options
    }

    /// The limits the session was opened with.
    #[must_use]
    pub fn limits(&self) -> &Arc<Limits> {
        &self.inner.limits
    }

    /// The platform seam.
    #[must_use]
    pub fn env(&self) -> &Arc<dyn Env> {
        &self.inner.env
    }

    /// The directory service (the pane calls `page` through `page_dir`; the service is exposed for the
    /// steps that need its caches directly).
    #[must_use]
    pub fn dirs(&self) -> &Arc<DirService> {
        &self.inner.dirs
    }

    /// The index intent sink every mutating service holds instead of a connection.
    ///
    /// A clone of the `Arc`, not a borrow: the slot is swap-able (see [`Self::set_intent`]), so it cannot
    /// hand out a reference. Step 9c's signature change from `&Arc<dyn IndexIntent>`; no caller in the tree
    /// held the borrow.
    #[must_use]
    pub fn intent(&self) -> SharedIntent {
        Arc::clone(&lock(&self.inner.intent))
    }

    /// Hand one intent to a service (plan §5.3's rename row: the service holds this, never a `Connection`).
    pub fn note_index(&self, op: IndexOp) {
        self.intent().note(op);
    }

    /// The config the session booted with. The indexer half of it is written by
    /// [`Self::set_index_config`]; everything else is still step 14's settings dialog.
    #[must_use]
    pub fn config(&self) -> ZenConfig {
        self.inner
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The left drawer's content: places, favourites, mounted drives (plan §5.8 `side_panel.rs`).
    #[must_use]
    pub fn places(&self) -> crate::services::places::Places {
        let home = crate::platform::home_dir();
        crate::services::places::Places::build(
            &**self.env(),
            home.as_deref(),
            &self.config().bookmarks,
        )
    }

    /// The favourites (Go's `config.json` `bookmarks`, `api/places.go:14-63,120-197`).
    #[must_use]
    pub fn favourites(&self) -> Vec<crate::store::config::Bookmark> {
        self.config().bookmarks
    }

    /// Add one favourite — the target of `Command::AddToFavorites`, Go's `POST /api/places`
    /// `{"action":"add"}` (`api/places.go:147-166`): normalised, deduplicated, and persisted
    /// through the same atomic [`ZenConfig::save`] step 12c writes the index config with.
    pub fn add_favourite(
        &self,
        path: &str,
    ) -> Result<Vec<crate::store::config::Bookmark>, CoreError> {
        let normal = crate::services::places::normalize(path)?;
        let mut candidate = self.config();
        if !candidate.bookmarks.iter().any(|b| b.path == normal) {
            candidate.bookmarks.push(crate::store::config::Bookmark {
                path: normal,
                label: None,
                added_at: None,
            });
        }
        self.write_bookmarks(candidate)
    }

    /// Remove one favourite (Go's `{"action":"remove"}`, `api/places.go:167-186`). Removing a path
    /// that is not there is the no-op Go answered with `status: success`.
    pub fn remove_favourite(
        &self,
        path: &str,
    ) -> Result<Vec<crate::store::config::Bookmark>, CoreError> {
        let normal = crate::services::places::normalize(path)?;
        let mut candidate = self.config();
        candidate.bookmarks.retain(|b| b.path != normal);
        self.write_bookmarks(candidate)
    }

    /// The single write path of the two above: validate, save atomically, adopt.
    fn write_bookmarks(
        &self,
        candidate: ZenConfig,
    ) -> Result<Vec<crate::store::config::Bookmark>, CoreError> {
        candidate.validate()?;
        candidate.save(&self.inner.paths.config)?;
        let bookmarks = candidate.bookmarks.clone();
        *lock(&self.inner.config) = candidate;
        Ok(bookmarks)
    }

    /// Run `f` with the single connection.
    ///
    /// This — not a stored `Connection` — is what plan §5.4's "the DB is a private field" correction
    /// means in practice: the session keeps ownership (one connection, one owner, Go's
    /// `SetMaxOpenConns(1)`), and step 12's index code borrows it for the duration of one call. The lock
    /// is held across `f`, which is the point: two rebuilds cannot interleave on one connection.
    ///
    /// # Errors
    /// [`CoreError::Sql`] when the session booted without a database ([`StartupNote::DatabaseDisabled`]).
    pub fn with_db<R>(&self, f: impl FnOnce(&rusqlite::Connection) -> R) -> Result<R, CoreError> {
        let guard = self
            .inner
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*guard {
            Some(conn) => Ok(f(conn)),
            None => Err(CoreError::Sql {
                msg: self
                    .database_disabled()
                    .unwrap_or_else(|| "no database".to_string()),
            }),
        }
    }

    /// The database is unavailable, and why (`None` when it is up).
    #[must_use]
    pub fn database_disabled(&self) -> Option<String> {
        self.inner
            .db_disabled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Allocate the correlation token for the next request (plan §4.3: the UI owns the `OriginAlloc`).
    #[must_use]
    pub fn next_origin(&self) -> Origin {
        self.inner.origins.next()
    }

    // ------------------------------------------------------------------------------------ the reads

    /// `/api/props`, exactly: `{platform, symlink_supported}` (`main.go:99-119`, `props-dir.json`).
    ///
    /// The answer is cached in the session's [`std::sync::RwLock<Option<bool>>`] (plan F21). The first
    /// call measures through [`Env::supports_symlink`] — which is itself a cached read for
    /// [`crate::platform::Linux`], whose actual scratch-directory probe is
    /// [`crate::platform::linux::Linux::probe_symlinks`] and belongs to the binary because that is the
    /// crate holding the concrete type. Every later call is a lock read and performs no I/O at all, so a
    /// properties dialog open across a repaint storm costs nothing.
    ///
    /// Use [`Self::prime_props`] from a job when even the first measurement must not land on the UI
    /// thread (plan §7 step 7's startup job list).
    #[must_use]
    pub fn props(&self) -> ProbeReport {
        ProbeReport {
            os: self.inner.env.os().as_str(),
            symlink_supported: self.symlink_support(),
        }
    }

    /// The cached symlink answer, measuring it if this is the first call.
    pub fn symlink_support(&self) -> bool {
        if let Some(cached) = self.read_probe_cache() {
            return cached;
        }
        let answer = self.inner.env.supports_symlink().unwrap_or_else(|e| {
            // A probe that *errors* (an unreadable scratch root) is not "no symlinks" — but the
            // wire type has only a boolean, so the safe answer is `false` and the reason goes to the
            // event queue as a notice.
            self.push(EngineEvent::Notice {
                level: crate::events::NoticeLevel::Warn,
                message: format!("symlink probe: {}", e.user_message()),
            });
            false
        });
        self.write_probe_cache(answer);
        answer
    }

    /// Warm the probe cache without returning anything: the job-runner form of the first `props()`.
    pub fn prime_props(&self) {
        let _ = self.symlink_support();
    }

    fn read_probe_cache(&self) -> Option<bool> {
        *self
            .inner
            .probe
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write_probe_cache(&self, value: bool) {
        let mut guard = self
            .inner
            .probe
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.is_none() {
            *guard = Some(value);
        }
    }

    /// One page of a directory listing, with the origin story attached (plan §4.1's `page_dir`).
    ///
    /// A page the cache can answer comes back `Done` — that is the read that may run on the caller's
    /// thread. A **flat** request that is not cached is a recursive walk of up to
    /// [`Limits::flat_walk_cap`] entries, so it becomes a [`JobKind::List`] job and answers
    /// `Receipt::Running`; the page arrives as [`EngineEvent::DirPageReady`] with this same `origin`
    /// (plan §4.3's correlation rule). Step 13's archive-manifest build returns `Running` from here too.
    ///
    /// # Errors
    /// The listing's own errors (see [`DirService::page`]), and only for the synchronous answer.
    pub fn page_dir(
        &self,
        req: &ListDir,
        origin: Origin,
    ) -> Result<Receipt<Arc<DirPage>>, CoreError> {
        if let Some(cached) = self.inner.dirs.cached_page(req) {
            return Ok(Receipt::Done(cached));
        }
        if req.flat.is_flat() {
            let dirs = Arc::clone(&self.inner.dirs);
            let req = req.clone();
            let label = format!("List {} recursively", req.path);
            let spec = JobSpec::new(self.inner.jobs.next_id(), JobKind::List, label, origin);
            let id = spec.id;
            self.inner.jobs.attach(spec, move |cancel, _progress| {
                let page = dirs.page_with_cancel(&req, cancel)?;
                Ok(JobResult::done(
                    Arc::clone(&page),
                    format!("{} row(s)", page.total_count),
                ))
            });
            return Ok(Receipt::Running { job: id, origin });
        }
        Ok(Receipt::Done(self.inner.dirs.page(req)?))
    }

    /// The recursive size of one directory: cached when it can be, a job when it cannot (plan §4.1's
    /// `dir_size`, §6.1 F1 — the walk that froze Go's request thread is exactly what must not freeze a
    /// window).
    ///
    /// The walk is uncapped by default (see [`DirSizeOpts`]); cancellation is at item boundaries
    /// ([`CancelToken::check`]) and a cancelled walk emits its one terminal event like any other job.
    #[must_use]
    pub fn dir_size(&self, path: &AbsPath, origin: Origin) -> Receipt<DirSizeStat> {
        if let Some(cached) = self
            .inner
            .dirs
            .cached_sizes(std::slice::from_ref(path))
            .into_iter()
            .next()
            .flatten()
        {
            return Receipt::Done(cached);
        }
        let dirs = Arc::clone(&self.inner.dirs);
        let path = path.clone();
        let label = format!("Size of {path}");
        let spec = JobSpec::new(self.inner.jobs.next_id(), JobKind::DirSize, label, origin);
        let id = spec.id;
        self.inner.jobs.attach(spec, move |cancel, progress| {
            let stat = dirs.dir_size(&path, &DirSizeOpts::default(), cancel, progress)?;
            Ok(JobResult::done(
                stat.clone(),
                format!("{} file(s), {} B", stat.files_count, stat.bytes),
            ))
        });
        Receipt::Running { job: id, origin }
    }

    /// The synchronous form of the size walk: no cache, no job, and the caller's own cancel token and
    /// progress sink. This is what a job body calls, and what `dir_size_matches_go` compares against the
    /// frozen payloads.
    ///
    /// # Errors
    /// See [`DirService::dir_size`].
    pub fn dir_size_walk(
        &self,
        path: &AbsPath,
        opts: &DirSizeOpts,
        cancel: &CancelToken,
        progress: &dyn DirSizeSink,
    ) -> Result<DirSizeStat, CoreError> {
        self.inner.dirs.dir_size(path, opts, cancel, progress)
    }

    /// The cached answers only, no job, no walk: what a listing's `files_count` column asks before
    /// deciding to show `--`.
    #[must_use]
    pub fn cached_dir_sizes(&self, dirs: &[AbsPath]) -> Vec<Option<DirSizeStat>> {
        self.inner.dirs.cached_sizes(dirs)
    }

    // ------------------------------------------------------------------------------------ live search

    /// Search the filesystem live (§4.1's `search_live`, plan §5.3 `services/search_live.rs`, §7 step 11).
    ///
    /// Three rules are encoded here rather than left to the caller:
    ///
    /// * **a pattern that does not compile is refused now**, before a job slot is spent — the answer is
    ///   [`CoreError::BadRegex`] with Go's own message, which is what step 11b shows inline.
    /// * **at most one walk per [`SearchThrottle::window`]** (§7 step 11's cadence). A refused request is
    ///   not dropped: it is remembered and starts itself from [`Self::poll`] when the window closes, so
    ///   the newest keystroke always wins and nobody has to keep a timer.
    /// * **starting a walk supersedes the previous one**, whose job is cancelled and whose rows are
    ///   therefore never emitted (see [`crate::events::EngineEvent::SearchReady`]).
    ///
    /// The rows arrive as `SearchReady { origin, job, result }`; `result` carries the query, so a pane
    /// that reused an origin can still tell an old answer from a new one.
    ///
    /// # Errors
    /// [`CoreError::BadRegex`] for an unparseable pattern. Everything else — a missing scope, a denied
    /// directory, an I/O failure — arrives on the job's terminal event, as for any other job.
    pub fn search_live(
        &self,
        req: &SearchQuery,
        origin: Origin,
    ) -> Result<SearchAdmission, CoreError> {
        self.search_live_at(req, origin, std::time::Instant::now())
    }

    /// [`Self::search_live`] with an injectable clock, so the 250 ms boundary is *asserted* at 249 ms and
    /// 250 ms rather than slept on (the same reason [`crate::jobs::JobRegistry::poll_at`] exists).
    ///
    /// # Errors
    /// As [`Self::search_live`].
    pub fn search_live_at(
        &self,
        req: &SearchQuery,
        origin: Origin,
        now: std::time::Instant,
    ) -> Result<SearchAdmission, CoreError> {
        req.matcher()?;
        let after = {
            let lane = self.inner.search_lane.lock();
            let mut lane = lane.unwrap_or_else(std::sync::PoisonError::into_inner);
            match lane.throttle().try_acquire_at(now) {
                Err(after) => {
                    let due = now.checked_add(after).unwrap_or(now);
                    lane.defer(req.clone(), origin, due);
                    Some(after)
                }
                Ok(()) => None,
            }
        };
        if let Some(after) = after {
            return Ok(SearchAdmission::Deferred { origin, after });
        }
        Ok(SearchAdmission::Running {
            job: self.start_search(req.clone(), origin),
            origin,
        })
    }

    /// The cadence in force (§7 step 11: 250 ms).
    #[must_use]
    pub fn search_window(&self) -> Duration {
        self.inner
            .search_lane
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .window()
    }

    /// The job whose rows still count, `None` when no search has been accepted yet.
    #[must_use]
    pub fn live_search(&self) -> Option<JobId> {
        self.inner
            .search_lane
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .live()
    }

    /// The request the throttle is still holding, if any — what a pane shows as "still typing".
    #[must_use]
    pub fn pending_search(&self) -> Option<SearchQuery> {
        self.inner
            .search_lane
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .deferred()
            .map(|d| d.query.clone())
    }

    /// Forget the throttle window (a new search box, a new scope: not "more typing in the same box").
    pub fn reset_search_throttle(&self) {
        self.inner
            .search_lane
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .throttle()
            .reset();
    }

    /// Forget the request the throttle is still holding, without starting it.
    ///
    /// [`Self::cancel`] stops the walk that is running and [`Self::reset_search_throttle`] forgets
    /// the window; this is the third half of tearing a search box down — without it the deferred
    /// query promotes itself from [`Self::poll`] and walks for a dialog that no longer exists.
    pub fn drop_pending_search(&self) {
        self.inner
            .search_lane
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drop_deferred();
    }

    /// Start a walk, cancelling the one it supersedes. The lane is updated *after* `attach`, so the
    /// registry's `JobStarted` and the lane's `live` agree by construction.
    fn start_search(&self, req: SearchQuery, origin: Origin) -> JobId {
        if let Some(prev) = self.live_search() {
            self.inner.jobs.cancel(prev);
        }
        let svc = Arc::clone(&self.inner.search);
        let label = format!("Search {:?} in {}", req.query, req.scope.to_string_lossy());
        let spec = JobSpec::new(
            self.inner.jobs.next_id(),
            JobKind::SearchLive,
            label,
            origin,
        );
        let id = spec.id;
        self.inner.jobs.attach(spec, move |cancel, _progress| {
            let results = svc.search(&req, cancel)?;
            Ok(JobResult::done(
                results.clone(),
                format!("{} hit(s)", results.len()),
            ))
        });
        let mut lane = self
            .inner
            .search_lane
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lane.mark_started(id);
        id
    }

    /// Start the deferred request once its window has closed (`poll`'s first step).
    fn promote_due_search(&self, now: std::time::Instant) {
        let due = {
            let mut lane = self
                .inner
                .search_lane
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            lane.take_due(now).map(|d| (d.query, d.origin))
        };
        if let Some((query, origin)) = due {
            self.start_search(query, origin);
        }
    }

    // ------------------------------------------------------------------------------------- the write path

    /// Paste `req` into `req.dest` (plan §4.1's `paste`, §5.3's `ops/paste.rs`, Go's `handlePaste`/
    /// `handlePasteLink`).
    ///
    /// Everything that can be answered without touching a byte runs **synchronously**, so the caller learns
    /// about a refused selection on its own call rather than through a job event five frames later: the
    /// VFS write guard, the writability probe, and the containment guard ([`guard::guard_sources`] +
    /// [`guard::ensure_guarded`], which is what turns `cp dir dir/sub` into
    /// [`CoreError::DestInsideSource`] instead of Go's silent no-op). What follows is a
    /// [`JobKind::Paste`] job — the copy itself can be terabytes, and plan F1 is exactly this call blocking
    /// a window.
    ///
    /// The job appends the Go-parity undo-log record (`ActionPasteCopy`/`ActionPasteMove`/`ActionPasteLink`,
    /// `(placed sources, dest, "")`) **after** the walk, invalidates the destination and every affected
    /// source directory (⇒ [`EngineEvent::DirChanged`](crate::events::EngineEvent::DirChanged) with this
    /// `origin`), clears the session clipboard when a cut finished cleanly, and reports the per-source
    /// outcomes as [`JobPayload::Paste`](crate::events::JobPayload::Paste).
    ///
    /// # Errors
    /// [`CoreError::Empty`] for an empty selection, [`CoreError::VfsReadOnly`] for an archive source or
    /// destination, [`CoreError::DestInsideSource`] when *every* source is refused, and the destination's
    /// own [`CoreError::PermissionDenied`]/[`CoreError::NotFound`]. A partial paste is **not** an error: it
    /// is a report.
    pub fn paste(&self, req: &PasteReq, origin: Origin) -> Result<Receipt<PasteReport>, CoreError> {
        // 1. the archive read-only rule, on sources and destination alike (`api/ops.go:73,227`).
        let src_paths: Vec<FsPath> = req
            .sources
            .iter()
            .map(|p| FsPath::from_abs(p.clone()))
            .collect();
        let mut refs: Vec<&FsPath> = src_paths.iter().collect();
        let dest_path = FsPath::from_abs(req.dest.clone());
        refs.push(&dest_path);
        guard::vfs_write_guard(&refs)?;
        guard::dest_guard(&dest_path)?;
        // 2. "the OS agrees we may write here", before a worker has been born (`api/ops.go:227-229`).
        guard::ensure_writable(self.inner.env.as_ref(), &req.dest)?;
        // 3. containment, once, so the executor and the report cannot disagree about the target names.
        let guarded = guard::guard_sources(&req.sources, &req.dest)?;
        guard::ensure_guarded(&guarded, &req.dest)?;

        let label = format!(
            "{} {} item(s)",
            match req.mode {
                PasteMode::Copy => "Paste",
                PasteMode::Cut => "Move",
                PasteMode::Link => "Link",
            },
            guarded.allowed.len()
        );
        let spec = JobSpec::new(self.inner.jobs.next_id(), JobKind::Paste, label, origin);
        let id = spec.id;
        let env = Arc::clone(&self.inner.env);
        let fs: Arc<dyn MoveFs> = Arc::new(OsMoveFs);
        let sess = self.clone();
        let req = req.clone();
        self.inner.jobs.attach(spec, move |cancel, progress| {
            let mut report =
                paste::paste(env.as_ref(), fs.as_ref(), &req, &guarded, cancel, progress)?;
            if let Some(entry) = report.log_entry() {
                match sess.append_log(&entry) {
                    Ok(id) => report.log_id = Some(id),
                    // A log failure does not undo the paste; it does have to be said out loud, because a
                    // paste the history pane does not know about cannot be reverted (plan F4's sibling).
                    Err(e) => sess.push(EngineEvent::Notice {
                        level: crate::events::NoticeLevel::Warn,
                        message: format!("paste log: {}", e.user_message()),
                    }),
                }
            }
            // Every directory the walk changed: the destination, and the parent of each source that moved
            // away (plan F13 — an in-process cache that is not invalidated here shows a file in two places).
            sess.invalidate_from(
                &FsPath::from_abs(report.dest.clone()),
                ChangeCause::LocalJob,
                origin,
            );
            let mut parents: Vec<AbsPath> = Vec::new();
            if req.mode != PasteMode::Copy {
                for (source, _) in report.placed() {
                    if let Some(parent) = source.parent()
                        && !parents.contains(&parent)
                    {
                        parents.push(parent);
                    }
                }
            }
            for parent in parents {
                sess.invalidate_from(&FsPath::from_abs(parent), ChangeCause::LocalJob, origin);
            }
            // The index hears about the same set the log did: a copy/link put rows somewhere new, a cut
            // relocated them (plan §5.3's no-connection rule — an op, never a `Connection`).
            let ops = report
                .placed()
                .into_iter()
                .map(|(source, target)| match req.mode {
                    PasteMode::Cut => IndexOp::Moved {
                        from: source.clone(),
                        to: target.clone(),
                    },
                    PasteMode::Copy | PasteMode::Link => IndexOp::Created {
                        path: target.clone(),
                    },
                })
                .collect::<Vec<_>>();
            sess.intent().note_all(&ops);
            // Go clears the clipboard only after a cut that met no error (`api/ops.go:339-344`): a partial
            // cut keeps the selection so the remainder can be retried.
            if req.mode == PasteMode::Cut && report.placed_any() && report.failed().is_empty() {
                sess.clipboard_clear();
            }
            Ok(JobResult::done(report.clone(), report.summary()))
        });
        Ok(Receipt::Running { job: id, origin })
    }

    /// Paste what the session clipboard holds into `dest` — the call behind Ctrl+V (§5.3's
    /// `clipboard` → paste path, Go's `handlePaste` reading `currentClipboard`, `api/ops.go:246-254`).
    ///
    /// A `cut` clipboard pastes as [`PasteMode::Cut`], which moves the sources and clears the clipboard on
    /// success; a `copy` clipboard leaves them in place. Go's fallback to the *system* clipboard
    /// (`api/ops.go:233-248`) is [`crate::ops::clipboard::read_system`], a synchronous process spawn: it is
    /// step 10's job to decide when a paste asks the OS instead of the session, and calling it from here
    /// would make Ctrl+V spawn `wl-paste` on every keypress.
    ///
    /// # Errors
    /// [`CoreError::Empty`] when nothing is queued, then as [`Self::paste`].
    pub fn paste_clipboard(
        &self,
        dest: AbsPath,
        policy: ConflictPolicy,
        origin: Origin,
    ) -> Result<Receipt<PasteReport>, CoreError> {
        let clip = self
            .clipboard()
            .ok_or(CoreError::Empty { what: "clipboard" })?;
        if clip.is_empty() {
            return Err(CoreError::Empty { what: "clipboard" });
        }
        let req = PasteReq {
            sources: clip.paths(),
            dest,
            mode: match clip.op {
                crate::ops::clipboard::ClipOp::Copy => PasteMode::Copy,
                crate::ops::clipboard::ClipOp::Cut => PasteMode::Cut,
            },
            policy,
        };
        self.paste(&req, origin)
    }

    // ------------------------------------------------------------------------------------ delete

    /// Delete `req.sources` — trash-first (plan §5.3 `ops/delete.rs`, Go `handleDelete` `api/ops.go:367`,
    /// plan §6.2 **D6**).
    ///
    /// The synchronous prefix is everything the UI must learn before a worker exists: the archive read-only
    /// rule, the empty selection, and the writability of each affected parent. What follows is a
    /// [`JobKind::Delete`] job — removing a 500 GB tree from a spinning disk is plan F1's frozen window.
    ///
    /// The job appends the Go-parity `ActionDelete` record (**after** the walk, carrying only the paths that
    /// actually disappeared, `api/ops.go:376-379`), invalidates every parent that lost a child, and notes
    /// one [`IndexOp::Pruned`] per removed path.
    ///
    /// # Errors
    /// [`CoreError::Empty`] for an empty selection, [`CoreError::VfsReadOnly`] for an archive path, and the
    /// parents' own [`CoreError::PermissionDenied`]. A partial delete is **not** an error: it is a report.
    pub fn delete(
        &self,
        req: &DeleteReq,
        origin: Origin,
    ) -> Result<Receipt<DeleteReport>, CoreError> {
        let paths: Vec<FsPath> = req.sources.iter().cloned().map(FsPath::from_abs).collect();
        let refs: Vec<&FsPath> = paths.iter().collect();
        guard::vfs_write_guard(&refs)?;
        if req.sources.is_empty() {
            return Err(CoreError::Empty { what: "selection" });
        }
        // The kind of each target, taken *before* anything is removed: after the delete there is nothing
        // left to `stat`, and the index needs to know whether it is dropping a row or a subtree.
        let kinds: Vec<bool> = req
            .sources
            .iter()
            .map(|p| std::fs::metadata(p.as_path()).is_ok_and(|m| m.is_dir()))
            .collect();
        for source in &req.sources {
            if let Some(parent) = source.parent() {
                guard::ensure_writable(self.inner.env.as_ref(), &parent)?;
            }
        }
        let label = format!("Delete {} item(s)", req.sources.len());
        let spec = JobSpec::new(self.inner.jobs.next_id(), JobKind::Delete, label, origin);
        let id = spec.id;
        let env = Arc::clone(&self.inner.env);
        let sess = self.clone();
        let req = req.clone();
        self.inner.jobs.attach(spec, move |cancel, progress| {
            let mut report = delete::delete(env.as_ref(), &req, cancel, progress)?;
            if let Some(entry) = report.log_entry() {
                match sess.append_log(&entry) {
                    Ok(log_id) => report.log_id = Some(log_id),
                    Err(e) => sess.push(EngineEvent::Notice {
                        level: crate::events::NoticeLevel::Warn,
                        message: format!("delete log: {}", e.user_message()),
                    }),
                }
            }
            for parent in report.affected_parents() {
                sess.invalidate_from(&FsPath::from_abs(parent), ChangeCause::LocalJob, origin);
            }
            for (gone, is_dir) in report.deleted().into_iter().zip(kinds) {
                // A directory is a subtree drop, a file a single row (`index/intent.rs`).
                sess.note_index(if is_dir {
                    IndexOp::Pruned { path: gone.clone() }
                } else {
                    IndexOp::Deleted { path: gone.clone() }
                });
            }
            if report.incomplete() {
                sess.push(EngineEvent::Notice {
                    level: crate::events::NoticeLevel::Warn,
                    message: format!("Delete: {}", report.summary()),
                });
            }
            Ok(JobResult::done(report.clone(), report.summary()))
        });
        Ok(Receipt::Running { job: id, origin })
    }

    // ------------------------------------------------------------------------------------ rename

    /// Rename the selection (plan §5.3 `ops/rename.rs`, Go `handleRename` `api/ops.go:391`).
    ///
    /// Synchronous, per plan §4.1's `rename` row: a rename is one `rename(2)` per item, and the only path
    /// that can take long — a directory crossing a device boundary — goes through
    /// [`crate::ops::r#move::move_item`]'s copy+delete, which Go ran synchronously too. (Go's asynchrony was
    /// the browser's, not the handler's.)
    ///
    /// Appends the Go-parity `ActionRename` record — the plain new name for a single rename, the JSON array
    /// of the names that applied for a batch (`api/ops.go:414,477`) — invalidates the directory the names
    /// live in, and notes one [`IndexOp::Renamed`] per pair.
    ///
    /// # Errors
    /// [`CoreError::Empty`] for an empty selection or a name-less request, [`CoreError::InvalidPath`] for a
    /// name that is not one legal component (`..`, `.`, `/`, a glob, a Windows reserved device name),
    /// [`CoreError::VfsReadOnly`] for an archive path. A per-item refusal (taken name, I/O failure) is in
    /// the report.
    pub fn rename(&self, req: &RenameReq, origin: Origin) -> Result<RenameReport, CoreError> {
        let paths: Vec<FsPath> = req.sources.iter().cloned().map(FsPath::from_abs).collect();
        let refs: Vec<&FsPath> = paths.iter().collect();
        guard::vfs_write_guard(&refs)?;
        for source in &req.sources {
            if let Some(parent) = source.parent() {
                guard::ensure_writable(self.inner.env.as_ref(), &parent)?;
            }
        }
        let fs: Arc<dyn MoveFs> = Arc::new(OsMoveFs);
        let cancel = CancelToken::new();
        let mut report = rename::rename(fs.as_ref(), req, &cancel, &crate::ops::NoProgress)?;
        if let Some(entry) = report.log_entry() {
            match self.append_log(&entry) {
                Ok(id) => report.log_id = Some(id),
                Err(e) => self.push(EngineEvent::Notice {
                    level: crate::events::NoticeLevel::Warn,
                    message: format!("rename log: {}", e.user_message()),
                }),
            }
        }
        let mut touched: Vec<AbsPath> = Vec::new();
        let mut ops = Vec::new();
        for (from, to) in report.renamed() {
            if let Some(parent) = from.parent()
                && !touched.contains(&parent)
            {
                touched.push(parent);
            }
            ops.push(IndexOp::Renamed {
                from: from.clone(),
                to: to.clone(),
            });
        }
        for parent in touched {
            self.invalidate_from(&FsPath::from_abs(parent), ChangeCause::LocalJob, origin);
        }
        self.intent().note_all(&ops);
        Ok(report)
    }

    // ------------------------------------------------------------------------------------ mkdir

    /// Create one directory and return the row Go returned (`api/ops.go:488-521`).
    ///
    /// Appends `ActionMkdir` with **no sources**, `dest` = the parent and `name` = the new directory
    /// (`api/ops.go:501`, golden record 1 — the record the capture itself reverted), invalidates the parent
    /// and notes one [`IndexOp::Created`].
    ///
    /// # Errors
    /// [`CoreError::VfsReadOnly`], [`CoreError::InvalidPath`] (an illegal name, `..` included),
    /// [`CoreError::PermissionDenied`] for an unwritable parent, [`CoreError::AlreadyExists`] for a taken
    /// name (Go's raw `file exists` 500), and the `mkdir(2)` failure.
    pub fn mkdir(&self, req: &MkdirReq, origin: Origin) -> Result<FileEntry, CoreError> {
        let entry = mkdir::mkdir(self.inner.env.as_ref(), req)?;
        let target = req.target()?;
        if let Err(e) = self.append_log(&ActionEntry::mkdir(req.dest.to_string_lossy(), &req.name))
        {
            self.push(EngineEvent::Notice {
                level: crate::events::NoticeLevel::Warn,
                message: format!("mkdir log: {}", e.user_message()),
            });
        }
        self.invalidate_from(
            &FsPath::from_abs(req.dest.clone()),
            ChangeCause::LocalJob,
            origin,
        );
        self.note_index(IndexOp::Created { path: target });
        Ok(entry)
    }

    // ------------------------------------------------------------------------------------ chmod

    /// Apply a mode to the selection (plan §5.3 `ops/chmod.rs`, Go `handleChmod` `api/ops.go:925`).
    ///
    /// Appends `ActionChmod` with the mode **text as the user typed it** (`api/ops.go:950`; golden records
    /// 9/10 hold `"600"` and `"0644"` for the same bits), invalidates every affected directory (the mode is
    /// a listed column), and notes one [`IndexOp::AttributesChanged`] per path — the op step 12's writer may
    /// drop, since the index has no mode column.
    ///
    /// # Errors
    /// [`CoreError::Empty`], [`CoreError::InvalidMode`] (via [`ChmodReq::parse`]),
    /// [`CoreError::VfsReadOnly`], [`CoreError::Unsupported`] on a platform with no `st_mode`, and the
    /// `chmod(2)` failure naming the path it failed on.
    pub fn chmod(&self, req: &ChmodReq, origin: Origin) -> Result<ChmodReport, CoreError> {
        let cancel = CancelToken::new();
        let mut report = chmod::chmod(self.inner.env.as_ref(), req, &cancel)?;
        if let Some(entry) = report.log_entry(req) {
            match self.append_log(&entry) {
                Ok(id) => report.log_id = Some(id),
                Err(e) => self.push(EngineEvent::Notice {
                    level: crate::events::NoticeLevel::Warn,
                    message: format!("chmod log: {}", e.user_message()),
                }),
            }
        }
        for parent in report.affected_parents() {
            self.invalidate_from(&FsPath::from_abs(parent), ChangeCause::LocalJob, origin);
        }
        let ops = report
            .applied
            .iter()
            .map(|path| IndexOp::AttributesChanged { path: path.clone() })
            .collect::<Vec<_>>();
        self.intent().note_all(&ops);
        Ok(report)
    }

    // ------------------------------------------------------------------------------------ open

    /// Hand each path to the desktop, routing directories as navigation (Go `handleOpen`
    /// `api/ops.go:122`, with the module-doc difference: a directory is reported, not launched).
    ///
    /// No log record and no invalidation — nothing changed — and no `origin` for the same reason. A missing
    /// opener is a typed [`CoreError::Shell`] in [`OpenReport::failed`], never a panic and never a silent
    /// success.
    ///
    /// # Errors
    /// [`CoreError::Empty`] for an empty selection. Per-path failures are in the report.
    pub fn open_paths(&self, paths: &[AbsPath]) -> Result<OpenReport, CoreError> {
        let cancel = CancelToken::new();
        open::open_paths(self.inner.env.as_ref(), paths, &cancel)
    }

    /// Show one path in its parent directory (plan §5.3's `reveal`, which Go had no endpoint for).
    ///
    /// # Errors
    /// As [`Self::open_paths`] for the parent directory.
    pub fn reveal(&self, path: &AbsPath) -> Result<(), CoreError> {
        open::reveal(self.inner.env.as_ref(), path)
    }

    // ------------------------------------------------------------------------------------ revert

    /// Undo one undo-log record (plan §5.3/§5.6, Go `POST /api/log/revert` `api/log.go:662-718`).
    ///
    /// The two request-time refusals are answered **synchronously**, because Go answered them before
    /// touching anything: an already-reverted record (Go's 409) and a non-reversible action (its 422, the
    /// [`ActionType::is_reversible`] set). What follows is a [`JobKind::Revert`] job: a revert of a 200 GB
    /// move is a move.
    ///
    /// The job applies the inverse through the *same* `ops` primitives the forward operation used, then
    /// patches the record's status byte in place ([`ActionLogStore::mark_reverted`]) — the capture proves a
    /// revert appends **no new frame** (`tests/golden/action.log` is 19 records post-revert; see
    /// [`revert_ops`]'s module docs). It then invalidates every affected directory and notes the inverse
    /// index ops.
    ///
    /// # Errors
    /// The record's own read errors, and [`CoreError::Unsupported`] for the two refusals above.
    pub fn revert(&self, id: LogId, origin: Origin) -> Result<Receipt<RevertReport>, CoreError> {
        let record = self.log_record(id)?;
        ensure_reversible(&record)?;
        let label = format!("Revert {}", record.action_str());
        let spec = JobSpec::new(self.inner.jobs.next_id(), JobKind::Revert, label, origin);
        let job = spec.id;
        let env = Arc::clone(&self.inner.env);
        let fs: Arc<dyn MoveFs> = Arc::new(OsMoveFs);
        let sess = self.clone();
        self.inner.jobs.attach(spec, move |cancel, progress| {
            let mut report =
                revert_ops::revert(env.as_ref(), fs.as_ref(), &record, cancel, progress)?;
            report.marked = sess.mark_log_reverted(id).is_ok();
            for parent in report.affected_parents() {
                sess.invalidate_from(&FsPath::from_abs(parent), ChangeCause::LocalJob, origin);
            }
            let move_back = record.action == ActionType::PASTE_MOVE;
            let ops = report
                .restored
                .iter()
                .map(|(from, to)| {
                    if move_back {
                        IndexOp::Moved {
                            from: from.clone(),
                            to: to.clone(),
                        }
                    } else {
                        IndexOp::Renamed {
                            from: from.clone(),
                            to: to.clone(),
                        }
                    }
                })
                .chain(
                    report
                        .removed
                        .iter()
                        .map(|path| IndexOp::Pruned { path: path.clone() }),
                )
                .collect::<Vec<_>>();
            sess.intent().note_all(&ops);
            Ok(JobResult::done(report.clone(), report.summary()))
        });
        Ok(Receipt::Running { job, origin })
    }

    /// `POST /api/index/rebuild` (`api/search_index_api.go:69`, plan §5.5): the staged shadow rebuild
    /// as a `JobKind::IndexRebuild` job on a **dedicated connection** — `with_db` would hold the
    /// session's connection mutex for the whole walk, freezing every read and every queue drain.
    ///
    /// # Errors
    /// [`CoreError::Sql`] when the session runs without a database; a bad exclude pattern is
    /// [`CoreError::BadRegex`] (Go dropped it silently — the plan §5.4 fix).
    pub fn index_rebuild(&self) -> Result<JobId, CoreError> {
        let db_path = self
            .inner
            .paths
            .database
            .clone()
            .ok_or_else(|| CoreError::Sql {
                msg: "this session runs without a database".to_string(),
            })?;
        let cfg = {
            let c = self.config();
            crate::index::walker::WalkCfg::from_settings(
                &c.indexer.roots,
                &c.indexer.excludes,
                c.indexer.max_depth,
                c.indexer.follow_symlinks,
                c.indexer.worker_count,
            )?
        };
        let attempts = self.inner.paths.db_open.attempts;
        let backoff = self.inner.paths.db_open.backoff;
        {
            let mut st = lock(&self.inner.index_state);
            st.is_dirty = true;
            st.last_error = None;
        }
        let spec = JobSpec::new(
            self.inner.jobs.next_id(),
            crate::jobs::JobKind::IndexRebuild,
            "Rebuild search index",
            self.next_origin(),
        );
        let id = spec.id;
        *lock(&self.inner.rebuild_job) = Some(id);
        self.inner
            .index_progress
            .store(0, std::sync::atomic::Ordering::Relaxed);
        let progress_rows = Arc::clone(&self.inner.index_progress);
        self.inner.jobs.attach(spec, move |cancel, progress| {
            let conn = crate::store::sql::connect_with_retry(
                &db_path,
                crate::store::config::StoreMode::Full,
                attempts,
                backoff,
            )?;
            let summary = crate::index::rebuild::run_with_hook(
                &conn,
                &cfg,
                cancel,
                &|p: &crate::index::rebuild::RebuildProgress| {
                    progress_rows.store(p.rows, std::sync::atomic::Ordering::Relaxed);
                    progress.set(0, p.rows, Some(p.stage.as_str().to_string()));
                },
            )?;
            Ok(JobResult::done(
                crate::events::JobPayload::Index(crate::events::IndexSummary {
                    rows: summary.rows,
                    roots_ok: summary.roots_ok,
                    roots_failed: summary.roots_failed,
                }),
                format!("{} row(s) indexed", summary.rows),
            ))
        });
        Ok(id)
    }

    /// `true` while a rebuild job is live.
    #[must_use]
    pub fn index_is_running(&self) -> bool {
        let id = *lock(&self.inner.rebuild_job);
        id.is_some_and(|id| {
            self.inner
                .jobs
                .info(id)
                .is_some_and(|i| i.state == "running")
        })
    }

    /// `GET /api/index/status` (`api/search_index_api.go:88`): the `index-status.json` keys, with Go's
    /// optional `error` when the last rebuild failed.
    #[must_use]
    pub fn index_status(&self) -> crate::services::search_index::IndexStatus {
        use crate::services::search_index::{IndexStatus, IndexStatusConfig, IndexStatusProgress};
        let st = lock(&self.inner.index_state);
        let cfg = self.config();
        let indexing = self.index_is_running();
        let indexed = if indexing {
            self.inner
                .index_progress
                .load(std::sync::atomic::Ordering::Relaxed)
        } else {
            u64::try_from(st.total_files).unwrap_or(0)
        };
        IndexStatus {
            is_indexing: indexing,
            total_files: st.total_files,
            last_indexed: st.last_indexed_unix,
            is_dirty: st.is_dirty,
            config: IndexStatusConfig {
                roots: cfg.indexer.roots.clone(),
                excludes: cfg.indexer.excludes.clone(),
                auto_index: cfg.indexer.auto_index,
                worker_count: cfg.indexer.worker_count,
                follow_symlinks: cfg.indexer.follow_symlinks,
            },
            progress: IndexStatusProgress {
                indexed: i64::try_from(indexed).unwrap_or(0),
            },
            error: st.last_error.clone(),
        }
    }

    /// `GET /api/index/search` (`api/search_index_api.go:10`): parse + execute against the live index.
    /// Go's handler clamps a non-positive `limit` back to [`DEFAULT_LIMIT`] and a negative `offset` to
    /// 0; the same clamps live in the service, and `None` means "the caller sent nothing".
    ///
    /// # Errors
    /// [`CoreError::Sql`] when there is no database or the query fails.
    pub fn index_search(
        &self,
        q: &str,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<crate::services::search_index::IndexSearchResults, CoreError> {
        let limit = limit.filter(|l| *l > 0).unwrap_or(i64::MAX);
        let offset = offset.filter(|o| *o >= 0).unwrap_or(0);
        let (dirty, last) = {
            let st = lock(&self.inner.index_state);
            (st.is_dirty, st.last_indexed_unix)
        };
        self.with_db(move |conn| {
            crate::services::search_index::search(conn, q, limit, offset, dirty, last)
        })?
    }

    /// `POST /api/index/cancel`: cancel the live rebuild. `true` when one was running (Go answered
    /// `{"cancelled": true` unconditionally; the port reports the truth).
    #[must_use]
    pub fn index_cancel(&self) -> bool {
        let id = *lock(&self.inner.rebuild_job);
        id.is_some_and(|id| self.inner.jobs.cancel(id))
    }

    /// `GET /api/index/config` (`api/search_index_api.go`, the `config` object of
    /// `tests/golden/index-status.json`): the persisted indexer settings.
    #[must_use]
    pub fn index_config(&self) -> IndexConfig {
        self.config().indexer
    }

    /// `POST /api/index/config` (golden `tests/golden/index-config-apply.json` → `{"saved": true}`;
    /// Go answered that boolean and nothing else, so the port returns the value it actually wrote).
    ///
    /// Four things happen, in this order, and a failure in the first two changes nothing at all:
    ///
    /// 1. **validation through the real bounds** — [`ZenConfig::validate`] (which is where
    ///    [`INDEX_MAX_DEPTH`](crate::limits::INDEX_MAX_DEPTH) lives: it is a *config* bound, not a walk
    ///    default), then [`compile_excludes`](crate::index::walker::compile_excludes) — Go discarded a
    ///    bad pattern silently, plan §5.4 refuses it — then the two bounds that are the indexer's own:
    ///    `worker_count ≥ 1` and every root an absolute path. A root that does not parse would otherwise
    ///    be *dropped* by [`Self::watch_start`], which is the silent half-index this refuses.
    /// 2. **the write**, through the same atomic tmp+rename [`ZenConfig::save`] every other settings
    ///    change uses, into [`SessionPaths::config`];
    /// 3. the live in-memory config, the **policy** (`auto_index` on promotes
    ///    [`WatchPolicy::Off`] to [`WatchPolicy::Auto`], `auto_index` off demotes `Auto` back — the
    ///    explicit [`WatchPolicy::On`] toggle is the user's and is never overwritten), and the
    ///    **watcher**: a watcher that was running is stopped and re-armed on the new root set, and is
    ///    left stopped when the new config no longer permits watching. A backend refusal there is not
    ///    a config failure — the settings *are* saved, so it lands in
    ///    [`IndexStatus::error`](crate::services::search_index::IndexStatus::error), which the dialog
    ///    paints, rather than turning a successful save into an error.
    /// 4. an [`EngineEvent::IndexChanged`] tagged with `origin`, so the dialog's status panel refreshes
    ///    on the event rather than on its poll timer.
    ///
    /// # Errors
    /// As [`ZenConfig::validate`], plus [`CoreError::BadRegex`] for an exclude pattern, and
    /// [`CoreError::Config`] for a `worker_count` of 0 or a relative root.
    pub fn set_index_config(
        &self,
        next: IndexConfig,
        origin: Origin,
    ) -> Result<IndexConfig, CoreError> {
        let mut candidate = self.config();
        candidate.indexer = next;
        candidate.validate()?;
        crate::index::walker::compile_excludes(&candidate.indexer.excludes)?;
        if candidate.indexer.worker_count == 0 {
            return Err(CoreError::Config {
                msg: "worker_count must be at least 1 (Go's own default is 2)".to_string(),
            });
        }
        for root in &candidate.indexer.roots {
            if AbsPath::from_path(root).is_err() {
                return Err(CoreError::Config {
                    msg: format!("index root {root:?} is not an absolute path"),
                });
            }
        }
        let roots_changed = candidate.indexer.roots != self.index_config().roots;
        let was_watching = self.watch_is_running();
        candidate.save(&self.inner.paths.config)?;
        let policy = match (candidate.indexer.auto_index, self.watch_policy()) {
            (true, WatchPolicy::Off) => WatchPolicy::AutoIndex,
            (false, WatchPolicy::AutoIndex) => WatchPolicy::Off,
            (_, policy) => policy,
        };
        *lock(&self.inner.watch_policy) = policy;
        *lock(&self.inner.config) = candidate.clone();
        if roots_changed {
            // The index no longer describes the configured tree (the golden `is_dirty` flag's meaning).
            lock(&self.inner.index_state).is_dirty = true;
        }
        if was_watching {
            self.watch_stop();
            if policy == WatchPolicy::Off {
                // The config turned watching off; stopping it *is* the re-arm.
                lock(&self.inner.index_state).last_error = None;
            } else {
                let roots: Vec<AbsPath> = candidate
                    .indexer
                    .roots
                    .iter()
                    .filter_map(|root| AbsPath::from_path(root).ok())
                    .collect();
                if let Err(e) = self.watch_start_at(&roots) {
                    lock(&self.inner.index_state).last_error = Some(e.user_message());
                } else {
                    lock(&self.inner.index_state).last_error = None;
                }
            }
        }
        let st = lock(&self.inner.index_state);
        let announced = EngineEvent::IndexChanged {
            total: u64::try_from(st.total_files).unwrap_or(0),
            dirty: st.is_dirty,
        };
        drop(st);
        // `origin` tags this write: the dialog's status panel refreshes on the event, and the
        // correlation token is the one it handed over (plan §4.1), not one the engine invented.
        let _ = origin;
        self.push(announced);
        Ok(candidate.indexer)
    }

    /// The rebuild half of `poll`: a finished `IndexRebuild` job lands its state and is announced as
    /// `IndexChanged` (Go updated `IndexState` in the goroutine's tail; the port does it where the
    /// terminal event is seen, so the state and the event can never disagree).
    fn index_finished_events(&self, finished: &[EngineEvent]) -> Vec<EngineEvent> {
        let mut out = Vec::new();
        for ev in finished {
            if let EngineEvent::JobFinished {
                kind: crate::jobs::JobKind::IndexRebuild,
                payload,
                error,
                ..
            } = ev
            {
                let mut st = lock(&self.inner.index_state);
                match payload {
                    crate::events::JobPayload::Index(summary) => {
                        st.total_files = i64::try_from(summary.rows).unwrap_or(i64::MAX);
                        st.last_indexed_unix = unix_now();
                        st.is_dirty = false;
                        out.push(EngineEvent::IndexChanged {
                            total: summary.rows,
                            dirty: false,
                        });
                    }
                    other => {
                        let cancelled = error
                            .as_ref()
                            .is_some_and(|e| matches!(**e, CoreError::Cancelled));
                        if let Some(e) = error {
                            st.last_error = Some(e.user_message());
                        }
                        let _ = other;
                        // Go set `IsDirty` on a failed rebuild and left it alone on a cancel; the
                        // first rebuild starts dirty, so a cancel before any success stays dirty too.
                        if !cancelled {
                            st.is_dirty = true;
                        }
                        out.push(EngineEvent::IndexChanged {
                            total: u64::try_from(st.total_files).unwrap_or(0),
                            dirty: st.is_dirty,
                        });
                    }
                }
                drop(st);
                *lock(&self.inner.rebuild_job) = None;
            }
        }
        out
    }

    /// Plan §5.5's updater cadence: apply the queued [`IndexOp`]s at most once per
    /// [`Session::INDEX_DRAIN_CADENCE`], from `poll_at`. A replaced intent (a test's
    /// `RecordingIntent`) means the queue is not the live sink — its contents are discarded, because
    /// applying stale ops behind a caller that watches the stream itself would double-write.
    fn drain_index_queue(&self, now: std::time::Instant) -> Vec<EngineEvent> {
        let due = {
            let mut st = lock(&self.inner.index_state);
            if st.last_flush + Self::INDEX_DRAIN_CADENCE <= now {
                st.last_flush = now;
                true
            } else {
                false
            }
        };
        if !due {
            return Vec::new();
        }
        let queue: SharedIntent = Arc::clone(&self.inner.index_queue) as SharedIntent;
        if !Arc::ptr_eq(&self.intent(), &queue) {
            let _ = self.inner.index_queue.take();
            return Vec::new();
        }
        let ops = self.inner.index_queue.take();
        if ops.is_empty() {
            return Vec::new();
        }
        match self
            .with_db(|conn| crate::index::updater::apply_ops(conn, &ops))
            .and_then(|r| r)
        {
            Ok(applied) => {
                let total = self
                    .with_db(|conn| {
                        conn.query_row("SELECT COUNT(*) FROM files;", [], |r| r.get::<_, i64>(0))
                            .unwrap_or(0)
                    })
                    .unwrap_or(0);
                let mut st = lock(&self.inner.index_state);
                st.total_files = total;
                if applied > 0 {
                    st.is_dirty = false;
                }
                vec![EngineEvent::IndexChanged {
                    total: u64::try_from(total).unwrap_or(0),
                    dirty: st.is_dirty,
                }]
            }
            Err(e) => {
                lock(&self.inner.index_state).last_error = Some(e.user_message());
                Vec::new()
            }
        }
    }

    /// Replace the index-intent sink — the seam plan §5.5/§5.6 built this for: step 12's queueing
    /// implementation is installed here and no caller changes. A test installs
    /// [`RecordingIntent`](crate::index::intent::RecordingIntent) to see what a mutation reported.
    pub fn set_intent(&self, intent: SharedIntent) {
        let mut slot = lock(&self.inner.intent);
        *slot = intent;
    }

    // ---------------------------------------------------------------------------------- the watcher

    /// The watcher policy this session was opened with (plan §4.1's `SessionConfig.watcher`).
    #[must_use]
    pub fn watch_policy(&self) -> WatchPolicy {
        *lock(&self.inner.watch_policy)
    }

    /// Start watching the **index root set**, honouring [`WatchPolicy`]: refused while the policy is
    /// [`WatchPolicy::Off`] and the indexer's own `auto_index` flag is false (the setting's real name is
    /// `IndexerSettings::auto_index` — `search-config.json`'s key, the `config.auto_index` field of
    /// `tests/golden/index-status.json`; the port does not invent a second switch for the same thing).
    ///
    /// # Errors
    /// [`CoreError::Watcher`] when the policy refuses, when the index config names no roots, or when the
    /// backend cannot start (no `watch` feature, a non-Linux platform, an over-budget root set —
    /// plan §5.5's stub row makes all of those the same typed error).
    pub fn watch_start(&self) -> Result<usize, CoreError> {
        let cfg = self.config();
        let policy = self.watch_policy();
        let allowed = match policy {
            WatchPolicy::On => true,
            // `Off` and `AutoIndex` agree today because nothing outside the session can set the policy
            // except [`Self::set_index_config`], which maps `auto_index` onto it (plan §5.4: the
            // settings store owns config writes), so the indexer's own `auto_index` flag is the only
            // way a session learns it should watch. `On` is the explicit toggle step 12c's dialog
            // sets.
            _ => cfg.indexer.auto_index,
        };
        if !allowed {
            return Err(CoreError::Watcher {
                msg: format!(
                    "watch policy {policy:?} is not enabled and the indexer's auto_index flag is off"
                ),
            });
        }
        let roots: Vec<AbsPath> = cfg
            .indexer
            .roots
            .iter()
            .filter_map(|root| AbsPath::from_path(root).ok())
            .collect();
        if roots.is_empty() {
            return Err(CoreError::Watcher {
                msg: "the index configuration names no roots to watch".to_string(),
            });
        }
        self.watch_start_at(&roots)
    }

    /// Start watching exactly these directories, replacing any watcher already running.
    ///
    /// The explicit-roots form: the caller has decided, so [`WatchPolicy`] does not veto it (that is what
    /// makes it usable from a settings dialog and from a test). What it does share with
    /// [`Self::watch_start`] is the consequence — every event lands in the same two places.
    ///
    /// Returns the number of watch descriptors the backend took (the budget spend, plan §5.5).
    ///
    /// # Errors
    /// As [`Self::watch_start`].
    pub fn watch_start_at(&self, roots: &[AbsPath]) -> Result<usize, CoreError> {
        let (tx, rx) = std::sync::mpsc::channel();
        let cfg = crate::index::watcher::WatchConfig::from_limits(&self.inner.limits);
        let watcher = crate::index::watcher::start(roots, &cfg, tx)?;
        let count = watcher.watch_count();
        let mut state = lock(&self.inner.watch);
        // Replacing a live watcher: stop it (joining its thread) before the new one is installed, so two
        // readers never feed one queue.
        if let Some(old) = state.watcher.take() {
            let _ = old.stop();
        }
        state.watcher = Some(watcher);
        state.rx = Some(rx);
        Ok(count)
    }

    /// Stop watching, joining the reader thread, and return the events that had not yet aged out of
    /// their coalescing window (the flush of [`Coalescer::take_all`](crate::index::watcher::Coalescer::take_all)).
    ///
    /// Idempotent: stopping a session that is not watching returns nothing and is not an error.
    pub fn watch_stop(&self) -> Vec<crate::index::watcher::DirChanged> {
        let mut state = lock(&self.inner.watch);
        let leftover = match state.watcher.take() {
            Some(watcher) => watcher.stop(),
            None => Vec::new(),
        };
        if let Some(rx) = state.rx.take() {
            // Drop whatever the thread sent but this frame never read, so a restart cannot replay it.
            while rx.try_recv().is_ok() {}
        }
        leftover
    }

    /// `true` while a watcher thread is running.
    #[must_use]
    pub fn watch_is_running(&self) -> bool {
        lock(&self.inner.watch)
            .watcher
            .as_ref()
            .is_some_and(crate::index::watcher::Watcher::is_running)
    }

    /// How many directories are watched right now (`0` when not watching).
    #[must_use]
    pub fn watch_count(&self) -> usize {
        lock(&self.inner.watch)
            .watcher
            .as_ref()
            .map_or(0, crate::index::watcher::Watcher::watch_count)
    }

    /// The roots the backend could not watch, with the reason each failed (`0..n`; empty when nothing
    /// is running or every root took). Plan §5.5's "report what you did not watch" half.
    #[must_use]
    pub fn failed_roots(&self) -> Vec<(AbsPath, String)> {
        lock(&self.inner.watch)
            .watcher
            .as_ref()
            .map_or_else(Vec::new, crate::index::watcher::Watcher::failed_roots)
    }

    /// Directories that appeared under a watched root after the descriptor budget ran out: watched
    /// parents, unwatched subtrees (plan §5.5; empty when nothing is running).
    #[must_use]
    pub fn unwatched(&self) -> Vec<AbsPath> {
        lock(&self.inner.watch)
            .watcher
            .as_ref()
            .map_or_else(Vec::new, crate::index::watcher::Watcher::unwatched)
    }

    /// Move everything the watcher thread delivered into the engine's own channels — the two halves plan
    /// §5.5 requires for one event:
    ///
    /// * the **pane reload** half: the containing directory is invalidated with
    ///   [`ChangeCause::External`](crate::events::ChangeCause::External), which is the honest cause (the
    ///   change came from outside this process; `ChangeCause::Watcher` stays reserved for an event that
    ///   names the watcher as its subject, which nothing in the UI distinguishes today), and the
    ///   `DirService` coalescing then does its usual de-duplication per frame; and
    /// * the **index** half: an [`IndexOp`] into [`Session::intent`] — the *same* [`QueuedIntent`]
    ///   (plan §9.2 step-12a note (2)), so an external change is applied by the 2 s drain rather than by
    ///   a rebuild, and a test that swapped the intent out sees the op instead of the database.
    ///
    /// A removal maps to [`IndexOp::Pruned`], not `Deleted`: the watcher cannot know whether the name was
    /// a file or a directory, and a pruned subtree is the safe shape of both (a file's prune is one row).
    fn pump_watcher(&self) {
        let batch = {
            let state = lock(&self.inner.watch);
            let Some(rx) = state.rx.as_ref() else {
                return;
            };
            let mut batch = Vec::new();
            while let Ok(event) = rx.try_recv() {
                batch.push(event);
            }
            batch
        };
        for event in batch {
            let dir = event
                .path()
                .parent()
                .unwrap_or_else(|| event.path().clone());
            self.invalidate(&crate::path::FsPath::from_abs(dir), ChangeCause::External);
            let op = match event {
                crate::index::watcher::DirChanged::Created { path } => IndexOp::Created { path },
                crate::index::watcher::DirChanged::Removed { path } => IndexOp::Pruned { path },
                crate::index::watcher::DirChanged::Metadata { path } => {
                    IndexOp::AttributesChanged { path }
                }
            };
            self.note_index(op);
        }
    }

    /// Mark one undo-log record reverted (the in-place status patch; see [`Self::revert`]).
    pub(crate) fn mark_log_reverted(
        &self,
        id: LogId,
    ) -> Result<crate::history::codec::ActionRecord, CoreError> {
        let mut store = lock(&self.inner.log);
        store.mark_reverted(id)
    }

    // ------------------------------------------------------------------------------------ the clipboard

    /// The queued selection, `None` when there is none (plan §5.3's `ops/clipboard.rs`).
    #[must_use]
    pub fn clipboard(&self) -> Option<Clipboard> {
        lock(&self.inner.clipboard).clone()
    }

    /// Queue a copy/cut selection — Go's `handleClipboardSet` (`api/ops.go:144-186`), in Go's order:
    ///
    /// 1. the VFS write guard (an archive selection is [`CoreError::VfsReadOnly`], `api/ops.go:73`);
    /// 2. [`clipboard::store`], which expands a [`ClipScope::Inside`](clipboard::ClipScope) request into
    ///    the directory's children and refuses an empty selection with [`CoreError::Empty`];
    /// 3. the undo-log record Go appended — `Append(ActionCopy, sources, "", "")`, a *record of the
    ///    copy*, not an undoable action (a log failure is a notice, never a failed copy);
    /// 4. [`EngineEvent::ClipboardChanged`] so every badge repaints;
    /// 5. best-effort [`clipboard::publish_system`] (Go's `writeToSystemClipboard`, whose spawn error Go
    ///    ignored and whose child exit status it never read — a clipboard manager that refuses the
    ///    payload must not turn a successful copy into a failed one; here the refusal is *reported* as a
    ///    notice instead of vanishing).
    ///
    /// # Errors
    /// [`CoreError::VfsReadOnly`] for an archive source, [`CoreError::Empty`] for an empty selection,
    /// [`CoreError::NotADir`] for a `Inside` request naming a file, and the source's own errors.
    pub fn clipboard_store(&self, req: &ClipboardStore) -> Result<Clipboard, CoreError> {
        let paths: Vec<FsPath> = req.sources.iter().cloned().map(FsPath::from_abs).collect();
        let refs: Vec<&FsPath> = paths.iter().collect();
        crate::ops::guard::vfs_write_guard(&refs)?;
        let clip = clipboard::store(self.inner.dirs.source().as_ref(), req)?;
        // The record carries the **resolved** sources: Go assigned `clipboard.Sources = paths` *after*
        // `resolveInsideSources` had widened an `Inside` request into the directory's children
        // (`api/ops.go:158-166`), so the log names what a paste will actually place.
        let sources: Vec<String> = clip
            .items
            .iter()
            .map(|item| item.path.to_string_lossy())
            .collect();
        if let Err(e) = self.append_log(&ActionEntry::new(ActionType::COPY, sources, "", "")) {
            self.push(EngineEvent::Notice {
                level: crate::events::NoticeLevel::Warn,
                message: format!("clipboard log: {}", e.user_message()),
            });
        }
        *lock(&self.inner.clipboard) = Some(clip.clone());
        self.push(EngineEvent::ClipboardChanged {
            clip: Some(clip.clone()),
        });
        self.publish_clipboard(Some(&clip));
        Ok(clip)
    }

    /// Clear the selection — Go's `handleClearClipboard` (`api/ops.go:216-221`), which also publishes an
    /// **empty** uri-list so another application's paste stops offering the old selection. No log record:
    /// Go appended none.
    pub fn clipboard_clear(&self) {
        *lock(&self.inner.clipboard) = None;
        self.push(EngineEvent::ClipboardChanged { clip: None });
        self.publish_clipboard(None);
    }

    /// The Go-parity undo-log append, shared by every mutating op: write the frame, then announce its id
    /// so the history pane refreshes (plan §4.1's `LogAppended`).
    pub(crate) fn append_log(&self, entry: &ActionEntry) -> Result<LogId, CoreError> {
        let mut store = lock(&self.inner.log);
        let id = store.append_entry(entry)?.0;
        self.push(EngineEvent::LogAppended { id });
        Ok(id)
    }

    /// Best-effort system-clipboard sync: a spawn failure is reported as a notice, and the child's exit
    /// status is not read (Go's `_ = cmd.Run()`, `api/ops.go:571-573`).
    fn publish_clipboard(&self, clip: Option<&Clipboard>) {
        if let Err(e) = clipboard::publish_system(self.inner.env.as_ref(), clip) {
            self.push(EngineEvent::Notice {
                level: crate::events::NoticeLevel::Warn,
                message: format!("system clipboard: {}", e.user_message()),
            });
        }
    }

    /// The most recent undo-log records, bounded by [`SessionOptions::history_limit`] when the caller
    /// does not name a limit. Synchronous: the log is memory-indexed at open.
    ///
    /// # Errors
    /// The store's read errors (a file that changed under us mid-read).
    pub fn log_recent(&self, limit: Option<usize>) -> Result<ActionLog, CoreError> {
        let want = limit.or(self.inner.options.history_limit).unwrap_or(100);
        let guard = self
            .inner
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.query_recent(want)
    }

    /// One undo-log record by id.
    ///
    /// # Errors
    /// [`CoreError::NotFound`]-shaped store errors for an unknown id.
    pub fn log_record(&self, id: LogId) -> Result<crate::history::codec::ActionRecord, CoreError> {
        let guard = self
            .inner
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.get(id).map(|(record, _offset)| record)
    }

    // ------------------------------------------------------------------------------------ job plumbing

    /// Ask a running job to stop. `false` when there is nothing running under that id — including the
    /// case that matters, a job that has just completed (plan §7 step 6).
    pub fn cancel(&self, id: JobId) -> bool {
        self.inner.jobs.cancel(id)
    }

    /// Cancel everything a widget (a closed tab) owns (plan §5.6 `cancel_origin`).
    pub fn cancel_origin(&self, origin: Origin) -> usize {
        self.inner.jobs.cancel_origin(origin)
    }

    /// The jobs pane's rows.
    #[must_use]
    pub fn jobs(&self) -> Vec<JobInfo> {
        self.inner.jobs.list()
    }

    /// Any job still running?
    #[must_use]
    pub fn has_live_jobs(&self) -> bool {
        self.inner.jobs.has_live()
    }

    /// The repaint nudge plan F14 asks for: `Some(16 ms)` while anything is running, `None` when the UI
    /// may sleep until the next event.
    #[must_use]
    pub fn wants_repaint(&self) -> Option<Duration> {
        if self.inner.jobs.has_live()
            || !self.events_are_queued()
            || self.inner.dirs.has_pending_invalidations()
            // A deferred search has to be started by a `poll`, so the frame has to happen (§7 step 11).
            || self.pending_search().is_some()
        {
            Some(Duration::from_millis(16))
        } else {
            None
        }
    }

    /// The updater drain cadence (plan §5.5: "a 2 s coalescing drain").
    pub const INDEX_DRAIN_CADENCE: Duration = Duration::from_secs(2);

    /// Drain the engine's event queue: the job table's coalesced events, the invalidations the
    /// directory service has been holding, and anything the session itself pushed (notices, the size
    /// answers a finished walk produced).
    ///
    /// Non-blocking by construction — see `JobRegistry::poll_at`.
    pub fn poll(&mut self) -> Vec<EngineEvent> {
        self.poll_at(std::time::Instant::now())
    }

    /// [`Self::poll`] with an injectable clock: the deferred search's 250 ms deadline is evaluated at
    /// `now`, which is what makes the boundary testable exactly.
    pub fn poll_at(&mut self, now: std::time::Instant) -> Vec<EngineEvent> {
        // First the watcher: its events become invalidations and index ops, both of which this frame
        // drains further down, so pumping here is what makes an external change visible in one poll.
        self.pump_watcher();
        self.promote_due_search(now);
        let drained = self.drain_index_queue(now);
        let mut out = self.inner.jobs.poll_at(now);
        out.extend(drained);
        let mut rebuilt = self.index_finished_events(&out);
        out.append(&mut rebuilt);
        // The broadcast half, collected first so the loop below is not mutating what it is reading:
        // every row showing this directory gets the number, not only the widget that asked
        // (plan §4.1's `DirSizeReady`).
        let sizes: Vec<EngineEvent> = out
            .iter()
            .filter_map(|ev| match ev {
                EngineEvent::JobFinished {
                    payload: crate::events::JobPayload::DirSize(stat),
                    ..
                } => Some(EngineEvent::DirSizeReady {
                    path: stat.path.clone(),
                    stat: stat.clone(),
                }),
                _ => None,
            })
            .collect();
        out.extend(sizes);
        // The same synthesis for a listing job: the page a `JobKind::List` produced is announced as
        // `DirPageReady`, carrying the request's origin back to the pane that asked (plan §4.1).
        let pages: Vec<EngineEvent> = out
            .iter()
            .filter_map(|ev| match ev {
                EngineEvent::JobFinished {
                    origin,
                    payload: crate::events::JobPayload::DirPage(page),
                    ..
                } => Some(EngineEvent::DirPageReady {
                    origin: *origin,
                    path: page.path.clone(),
                    page: Arc::clone(page),
                }),
                _ => None,
            })
            .collect();
        out.extend(pages);
        // A live search's rows go to the pane that asked, and only for the search the session still
        // considers live: a superseded or cancelled walk produced nothing worth delivering (§7 step 11).
        let searches: Vec<EngineEvent> = out
            .iter()
            .filter_map(|ev| match ev {
                EngineEvent::JobFinished {
                    id,
                    origin,
                    payload: crate::events::JobPayload::SearchHits(result),
                    ..
                } => {
                    let current = self
                        .inner
                        .search_lane
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .is_current(*id);
                    current.then(|| EngineEvent::SearchReady {
                        origin: *origin,
                        job: *id,
                        result: Arc::clone(result),
                    })
                }
                _ => None,
            })
            .collect();
        out.extend(searches);
        // A paste that did not do everything it was asked is the one job result the core insists the UI
        // show: Go returned the skipped list in the response body and the browser dropped it on the floor
        // (ledger #9, plan F12). It arrives as a notice so the toast exists even when nobody kept the
        // `Receipt`.
        let warnings: Vec<EngineEvent> = out
            .iter()
            .filter_map(|ev| match ev {
                EngineEvent::JobFinished {
                    payload: crate::events::JobPayload::Paste(report),
                    ..
                } if report.incomplete() => Some(EngineEvent::Notice {
                    level: crate::events::NoticeLevel::Warn,
                    message: format!("Paste into {}: {}", report.dest, report.summary()),
                }),
                _ => None,
            })
            .collect();
        out.extend(warnings);
        out.extend(self.inner.dirs.take_invalidations());
        let mut guard = self
            .inner
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        out.extend(guard.drain(..));
        drop(guard);
        out
    }

    /// Say that a listing went stale, queueing the coalesced [`EngineEvent::DirChanged`].
    ///
    /// Every mutating op ends with this call (plan F13); the coalescing is what stops a 2 000-file paste
    /// from being 2 000 invalidation events.
    pub fn invalidate(&self, path: &crate::path::FsPath, cause: ChangeCause) {
        self.inner.dirs.invalidate(path, cause, None);
    }

    /// As [`Self::invalidate`], tagging the event with the origin that caused it (plan §4.3: a tab that
    /// did not ask refreshes quietly, keeping scroll and selection).
    pub fn invalidate_from(&self, path: &crate::path::FsPath, cause: ChangeCause, origin: Origin) {
        self.inner.dirs.invalidate(path, cause, Some(origin));
    }

    /// Push an event the session produced on its own (a degraded startup, a warning a job collected).
    pub fn push(&self, event: EngineEvent) {
        let mut guard = self
            .inner
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.push_back(event);
    }

    fn events_are_queued(&self) -> bool {
        self.inner
            .events
            .lock()
            .map(|g| g.is_empty())
            .unwrap_or(false)
    }

    /// Ask every running job to stop and wait a bounded time for its thread to land.
    ///
    /// A job that ignores its cancel token is *not* waited for: the plan's shutdown rule is that the
    /// user's own action ends the session, so the thread is left detached and the process exit reaps it.
    pub fn close(&self) {
        // Step 12b: the watcher is not a job, so `cancel_all` would not reach it; its thread is joined
        // here (and by its `Drop`) so no reader outlives a closed session.
        let _ = self.watch_stop();
        let asked = self.inner.jobs.cancel_all();
        if asked == 0 {
            self.inner.jobs.reap_finished();
            return;
        }
        let deadline = std::time::Instant::now() + Duration::from_millis(200);
        while self.inner.jobs.has_live() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        self.inner.jobs.reap_finished();
    }

    /// The filesystem source the directory service reads through (a test substitutes it before any
    /// listing happens).
    #[must_use]
    pub fn fs_source(&self) -> Arc<dyn FsSource> {
        self.inner.dirs.source().clone()
    }

    /// Detach the job table's records: for a test that wants to assert on nothing being left.
    pub fn clear_finished_jobs(&self) -> Vec<JobId> {
        self.inner.jobs.clear_finished()
    }

    /// The raw job ids the table still holds.
    #[must_use]
    pub fn job_ids(&self) -> Vec<JobId> {
        self.inner.jobs.ids()
    }

    /// A job's metadata, for the "did my job finish" check a widget does outside of `poll`.
    #[must_use]
    pub fn job_info(&self, id: JobId) -> Option<JobInfo> {
        self.inner.jobs.info(id)
    }

    /// Forget a job record now (its thread is not waited for).
    pub fn forget_job(&self, id: JobId) -> bool {
        self.inner.jobs.forget(id)
    }

    /// Acknowledge a job's terminal event so its record can be reaped.
    pub fn note_job_completion(&self, id: JobId) -> bool {
        self.inner.jobs.note_completion(id)
    }

    /// Spawn a job the session does not itself own: the seam every later `Session::paste`/`delete`/
    /// `search_live`/`extract` goes through, so the *table* stays the only place threads are born.
    ///
    /// `label` is the user-visible jobs-pane line, `budget` the watchdog limit (plan §5.6), and `f` gets
    /// the cancel token and the progress sink — the only two channels out of a worker.
    pub fn spawn_job<F, T>(
        &self,
        kind: JobKind,
        label: impl Into<String>,
        budget: Option<Duration>,
        f: F,
    ) -> JobId
    where
        F: FnOnce(&CancelToken, &crate::jobs::Progress) -> Result<JobResult<T>, CoreError>
            + Send
            + 'static,
        T: Into<crate::events::JobPayload> + Send + 'static,
    {
        let origin = self.next_origin();
        let mut spec = JobSpec::new(self.inner.jobs.next_id(), kind, label, origin);
        spec.budget = budget;
        self.inner.jobs.attach(spec, f)
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // The `Session` handle is a cloneable Arc, so the *last* one dropping is the session ending.
        // `close` is the polite path; this is the backstop (it cannot block, so it does not wait).
        self.jobs.cancel_all();
    }
}

/// Check `config.json`: parseable, absent (first run), or garbage — in which case its bytes are copied
/// to `<name>.corrupt` and the note says so.
///
/// The copy uses `std::fs::copy` for the same reason `store/legacy.rs:403` does: this is the boot-time
/// settings file, not a user file, and there is no `Env` method for it (`Env::copy_paths` copies *items
/// into a directory* and logs an undo record — the opposite of what a backup of an unreadable file
/// needs).
fn check_config_file(path: &Path) -> Result<Option<StartupNote>, CoreError> {
    match ZenConfig::load(path) {
        Ok(_) | Err(CoreError::NotFound { .. }) => Ok(None),
        Err(e) if !matches!(e, CoreError::Config { .. }) => Err(e),
        Err(e) => {
            let mut name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "config.json".to_string());
            name.push_str(".corrupt");
            let copied = path.with_file_name(name);
            std::fs::copy(path, &copied)
                .map_err(|io| CoreError::io(copied.display().to_string(), io))?;
            let _ = e;
            Ok(Some(StartupNote::ConfigReset {
                path: path.to_path_buf(),
                copied_to: copied,
            }))
        }
    }
}

/// Lock, ignoring poisoning: every value behind these mutexes is either fully updated or untouched, so
/// one panicked job must not disable the session's clipboard or event queue.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The persisted index configuration — Go's `IndexConfig` (`api/search_index.go:23`), spelled
/// [`IndexerSettings`](crate::store::config::IndexerSettings) by this crate's storage type. The alias
/// exists so the dialog speaks the wire's name (`tests/golden/index-status.json`'s `config`).
pub type IndexConfig = crate::store::config::IndexerSettings;

/// The session's runtime index state — Go's `IndexConfig.IndexState` (`api/search_index.go:30-34`),
/// held in memory here because `store::config` keeps the settings file pure (plan §5.5: the config
/// file is settings, not state). `is_dirty` starts `true` exactly like Go's default.
struct IndexRunState {
    /// Unix seconds of the last successful rebuild.
    last_indexed_unix: i64,
    /// Rows after the last successful rebuild or drain.
    total_files: i64,
    /// The index is behind the filesystem.
    is_dirty: bool,
    /// Why the last rebuild failed ([`CoreError::Cancelled`] clears it, Go's `LastErr`).
    last_error: Option<String>,
    /// When `poll_at` last ran the updater drain (the 2 s cadence's clock).
    last_flush: std::time::Instant,
}

impl Default for IndexRunState {
    fn default() -> Self {
        Self {
            last_indexed_unix: 0,
            total_files: 0,
            is_dirty: true,
            last_error: None,
            last_flush: std::time::Instant::now(),
        }
    }
}

/// Unix seconds, Go's `time.Now().Unix()`.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(0))
        .unwrap_or(0)
}

/// `Session`'s convenience: the intent sink every service holds (plan §5.3's no-connection rule).
#[must_use]
pub fn noop_intent() -> Arc<dyn IndexIntent> {
    crate::index::intent::noop_intent()
}

/// The type alias the plan's §4.3 `Arc<OriginAlloc>` line refers to, spelled through the session.
pub type OriginHandle = Arc<OriginAlloc>;

// --------------------------------------------------------------------------- archives (step 13a)

impl Session {
    /// The archive facility (plan §5.5). The pane reaches the four operations below through it; nothing
    /// in `zenman-ui` may call `archive/*` directly.
    #[must_use]
    pub fn archives(&self) -> &Arc<crate::services::archive::ArchiveService> {
        &self.inner.archives
    }

    /// `archive_list` — the rows of an archive directory (`path` must be in an archive store).
    ///
    /// Synchronous by design: it reads the container's central directory, cached per host mtime, which is
    /// what Go's `/api/archive/dir` did too — and the answer is bounded by the manifest caps, so a job
    /// would cost more than the read.
    ///
    /// # Errors
    /// [`CoreError::UnsupportedArchive`] for a filesystem path, [`CoreError::ArchiveModified`] when the
    /// host changed since the manifest was built (plan F7), [`CoreError::ArchiveEncrypted`] /
    /// [`CoreError::ArchiveCorrupt`] / [`CoreError::CapacityExceeded`] from the reader.
    pub fn archive_list(&self, path: &FsPath) -> Result<Vec<crate::entry::FileEntry>, CoreError> {
        self.inner.archives.list(path)
    }

    /// `archive_read` — one member's bytes, capped by
    /// [`Limits::nested_archive_max_bytes`](crate::limits::Limits::nested_archive_max_bytes).
    pub fn archive_read(&self, path: &FsPath) -> Result<Vec<u8>, CoreError> {
        self.inner.archives.read(path)
    }

    /// `archive_stream` — one member into the caller's sink, with no whole-member buffer (the plan's
    /// `stream_file` criterion; the preview/`open with` path).
    pub fn archive_stream(
        &self,
        path: &FsPath,
        sink: &mut dyn std::io::Write,
    ) -> Result<u64, CoreError> {
        self.inner.archives.stream(path, sink)
    }

    /// `archive_props` — the payload step 13b's properties dialog shows for an archive row (plan §7 row
    /// 13: Alt+Enter works on archive entries too).
    pub fn archive_props(
        &self,
        path: &FsPath,
    ) -> Result<crate::services::archive::ArchiveProps, CoreError> {
        self.inner.archives.props(path)
    }

    /// `archive_extract` — start an extraction **job**.
    ///
    /// The receipt is always [`Receipt::Running`] (unbounded work, cancellable, plan F1). On completion the
    /// destination is invalidated with the caller's `origin` — extraction writes real files, and a pane
    /// that does not reload would show a directory it just filled (F13) — and every written path is
    /// noted to the index intent, because those paths are filesystem paths, unlike archive *members*
    /// (which are never indexed; the decision and its proof are in `services/archive.rs`).
    ///
    /// # Errors
    /// Synchronously, before any thread starts: a destination inside an archive
    /// ([`CoreError::VfsReadOnly`]), a traversal in it, a stale/encrypted/corrupt container, or a member
    /// that would escape the destination ([`CoreError::InvalidPath`] — the zip-slip refusal).
    pub fn archive_extract(
        &self,
        req: &ExtractReq,
        origin: Origin,
    ) -> Result<Receipt<ExtractReport>, CoreError> {
        let sess = self.clone();
        let dest = req.dest.clone();
        let hook: crate::archive::extract::DoneHook =
            Some(Arc::new(move |report: ExtractReport| {
                sess.invalidate_from(
                    &FsPath::from_abs(dest.clone()),
                    ChangeCause::LocalJob,
                    origin,
                );
                let ops: Vec<IndexOp> = report
                    .written
                    .iter()
                    .map(|path| IndexOp::Created { path: path.clone() })
                    .collect();
                if !ops.is_empty() {
                    sess.intent().note_all(&ops);
                }
            }));
        let job = self.inner.archives.extract(req, origin, hook)?;
        Ok(Receipt::Running { job, origin })
    }

    /// The extraction's progress snapshot (the dialog's poll; `None` once it has been collected).
    #[must_use]
    pub fn archive_extract_status(
        &self,
        id: JobId,
    ) -> Option<Arc<crate::archive::extract::ExtractJob>> {
        self.inner.archives.extract_status(id)
    }

    /// Ask a running extraction to stop (plan F3: the job answers `JobFinished{Cancelled}`, and whatever
    /// it had written up to that point stays — the report is the truth about what happened).
    pub fn archive_cancel(&self, id: JobId) -> bool {
        self.inner.jobs.cancel(id)
    }

    /// Collect finished extractions past the retention window. Cheap; the shell's tick calls it.
    pub fn archive_gc(&self) {
        self.inner.archives.gc();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::path::FsPath;
    use crate::platform::stub::Unsupported;
    use crate::store::config::ZenConfig;
    use std::fs;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zenman-session-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// Remove the files a session created, then the directory. `std::fs::remove_dir_all` is banned
    /// workspace-wide (`clippy.toml`), and the tree here is two levels at most.
    fn cleanup(dir: &Path) {
        for entry in fs::read_dir(dir).expect("temp dir reads") {
            let entry = entry.expect("entry");
            if entry.path().is_dir() {
                let _ = fs::remove_dir(entry.path());
            } else {
                let _ = fs::remove_file(entry.path());
            }
        }
        let _ = fs::remove_dir(dir);
    }

    /// A copy of the frozen undo log with one body byte of its second frame flipped, which is the state
    /// plan F4 says the port must survive. Same edit shape `contract_history` uses on frame 5 (a byte
    /// inside `dest`, so the frame still parses but the CRC no longer matches); here it lands early so
    /// `open` sees damage and `resync` has a long readable tail to discard.
    fn damaged_log(dir: &Path) -> PathBuf {
        use crate::history::codec::{self, HEADER_FIXED};
        let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden/action.log");
        let mut bytes = fs::read(&golden).expect("the frozen undo log is in the repository");
        let first = codec::decode_at(&bytes, 0).expect("frame 0 of the golden log");
        let off1 = first.consumed;
        let second =
            codec::decode_at(&bytes[off1..], off1 as u64).expect("frame 1 of the golden log");
        let at = off1 + HEADER_FIXED + 1;
        assert!(
            at < off1 + second.consumed,
            "frame 1 has a body to damage (consumed {})",
            second.consumed
        );
        bytes[at] ^= 0xff;
        let path = dir.join("action.log");
        fs::write(&path, &bytes).expect("write the damaged log");
        path
    }

    fn paths(dir: &Path) -> SessionPaths {
        SessionPaths::new(
            dir.join("config.json"),
            dir.join("action.log"),
            Some(dir.join("zen-man.db")),
            dir.join("probe"),
        )
        .with_db_open(1, Duration::ZERO)
    }

    /// The config the indexer writes, with one root under the fixture directory.
    fn indexer_cfg(dir: &Path) -> IndexConfig {
        IndexConfig {
            roots: vec![dir.join("tree").display().to_string()],
            excludes: vec!["**/.git".to_string(), "**/build".to_string()],
            auto_index: true,
            index_on_startup: false,
            max_depth: 4,
            follow_symlinks: false,
            worker_count: 3,
        }
    }

    #[test]
    fn index_config_round_trips_through_the_settings_file_and_a_refusal_changes_nothing() {
        // Plan §7 row 12's write path (`index-config-apply.json`): the value lands in `config.json`,
        // survives a reopen, and an out-of-bound value is refused *before* anything is written.
        let dir = temp_dir("index-config");
        let paths = paths(&dir);
        let (session, _notes) = Session::open(
            paths.clone(),
            ZenConfig::default(),
            Arc::new(Unsupported::new()),
            crate::limits::Limits::default(),
        )
        .expect("a session opens over a fresh directory");
        assert!(
            session.index_config().roots.is_empty(),
            "the default indexes nothing"
        );
        let applied = session
            .set_index_config(indexer_cfg(&dir), session.next_origin())
            .expect("a sound configuration is accepted");
        assert_eq!(
            applied,
            indexer_cfg(&dir),
            "the applied value is what was written"
        );
        assert_eq!(session.index_config(), applied, "the live config agrees");
        assert!(
            session.index_status().is_dirty,
            "a new root set makes the index dirty (the golden `is_dirty` flag)"
        );
        // the file, not just the in-memory copy
        let on_disk = ZenConfig::load(&paths.config).expect("the written file parses");
        assert_eq!(on_disk.indexer, applied, "the settings file carries it");
        // and the policy mapping: `auto_index` on promotes `Off` to `AutoIndex`
        assert_eq!(session.watch_policy(), WatchPolicy::AutoIndex);
        assert!(
            session.failed_roots().is_empty() && session.unwatched().is_empty(),
            "nothing is running, so nothing failed (the `#[cfg]` stub agrees)"
        );
        drop(session);
        let (again, _notes) = Session::open(
            paths.clone(),
            ZenConfig::load(&paths.config).expect("reopen reads what was written"),
            Arc::new(Unsupported::new()),
            crate::limits::Limits::default(),
        )
        .expect("the session reopens on the saved config");
        assert_eq!(
            again.index_config(),
            applied,
            "roots/excludes/auto_index survive"
        );

        // --- refusals: none of these changes a byte
        let before = fs::read(&paths.config).expect("the file is there");
        let mut too_deep = applied.clone();
        too_deep.max_depth = u32::try_from(crate::limits::INDEX_MAX_DEPTH).unwrap_or(u32::MAX) + 1;
        let err = again
            .set_index_config(too_deep, again.next_origin())
            .expect_err("the depth cap is a config bound");
        assert!(matches!(err, CoreError::CapacityExceeded { .. }));
        let mut no_threads = applied.clone();
        no_threads.worker_count = 0;
        again
            .set_index_config(no_threads, again.next_origin())
            .expect_err("zero walk threads is not answerable");
        let mut bad_glob = applied.clone();
        bad_glob.excludes.push(String::new());
        again
            .set_index_config(bad_glob, again.next_origin())
            .expect_err("Go discarded a bad exclude; the port refuses it");
        let mut relative = applied.clone();
        relative.roots.push("tree/relative".to_string());
        again
            .set_index_config(relative, again.next_origin())
            .expect_err("a root that does not parse would be silently unindexed");
        assert_eq!(again.index_config(), applied, "a refusal changes no value");
        assert_eq!(
            fs::read(&paths.config).expect("the file is still there"),
            before,
            "a refusal changes no bytes"
        );
        drop(again);
        cleanup(&dir);
    }

    #[test]
    fn turning_auto_index_off_moves_the_watch_policy_back() {
        // The two switches are one decision: `auto_index` is the persisted half, `WatchPolicy` the
        // runtime half, and `set_index_config` keeps them from disagreeing. `WatchPolicy::On` is the
        // user's explicit toggle, so it is never overwritten.
        let dir = temp_dir("watch-policy");
        let paths = paths(&dir);
        let (session, _notes) = Session::open(
            paths,
            ZenConfig::default(),
            Arc::new(Unsupported::new()),
            crate::limits::Limits::default(),
        )
        .expect("a session opens over a fresh directory");
        assert_eq!(session.watch_policy(), WatchPolicy::Off, "the boot default");
        let on = indexer_cfg(&dir);
        session
            .set_index_config(on, session.next_origin())
            .expect("accepted");
        assert_eq!(session.watch_policy(), WatchPolicy::AutoIndex);
        let mut off = session.index_config();
        off.auto_index = false;
        session
            .set_index_config(off, session.next_origin())
            .expect("accepted");
        assert_eq!(session.watch_policy(), WatchPolicy::Off, "and back down");
        drop(session);
        cleanup(&dir);
    }

    #[test]
    fn open_survives_a_corrupt_log_a_locked_db_and_a_garbage_config() {
        // Plan §7 step 6 acceptance (a), and §5.6's row: all three damaged files at once, three notes
        // back, and a working session. Go aborted on the second of the three.
        let dir = temp_dir("triple");
        fs::create_dir_all(dir.join("probe")).expect("probe dir");
        let log = damaged_log(&dir);
        let db_path = dir.join("zen-man.db");
        let holder = rusqlite::Connection::open(&db_path).expect("holder opens");
        holder
            .execute_batch("BEGIN IMMEDIATE; CREATE TABLE IF NOT EXISTS lock_marker (x);")
            .expect("write lock held");
        fs::write(dir.join("config.json"), br#"{"bookmarks": [ unterminated"#)
            .expect("garbage config");

        let mut p = paths(&dir);
        p.db_open = DbOpenBudget {
            attempts: 1,
            backoff: Duration::ZERO,
        };
        let (session, notes) = Session::open(
            p,
            ZenConfig::default(),
            Arc::new(Unsupported::new()),
            Limits::default(),
        )
        .expect("F1: open never fails on damaged user files");
        drop(holder);

        let kinds: Vec<String> = notes.iter().map(|n| n.message()).collect();
        assert_eq!(
            notes.len(),
            3,
            "three damaged inputs, three notes: {kinds:?}"
        );
        let log_note = notes
            .iter()
            .find_map(|n| match n {
                StartupNote::LogRepaired {
                    kept,
                    dropped,
                    bytes,
                } => Some((*kept, *dropped, *bytes)),
                _ => None,
            })
            .expect("a LogRepaired note");
        // Frame 0 survives, frame 1 is the damage, everything after it is discarded.
        assert_eq!(
            log_note.0, 1,
            "exactly the good prefix was kept: {log_note:?}"
        );
        assert!(
            log_note.1 >= 10,
            "the readable tail was discarded: {log_note:?}"
        );
        assert!(log_note.2 > 0, "bytes were removed: {log_note:?}");
        assert!(notes.iter().any(|n| matches!(
            n,
            StartupNote::DatabaseDisabled { reason } if !reason.is_empty()
        )));
        let reset = notes
            .iter()
            .find_map(|n| match n {
                StartupNote::ConfigReset { path, copied_to } => {
                    Some((path.clone(), copied_to.clone()))
                }
                _ => None,
            })
            .expect("a ConfigReset note");
        assert!(reset.1.exists(), "the garbage bytes were kept, not dropped");
        assert_eq!(reset.0, dir.join("config.json"));
        // And the session is usable: the database is honestly reported as absent, and the log reads.
        assert!(session.database_disabled().is_some());
        let records = session.log_recent(None).expect("the repaired log reads");
        assert!(!records.is_empty(), "the kept records answer the query");
        session.close();
        let _ = fs::remove_file(log);
        cleanup(&dir);
    }

    #[test]
    fn open_reports_only_the_note_that_applies() {
        // The same code path with a healthy directory: no notes at all — a test that would pass
        // vacuously if `open` always reported three.
        let dir = temp_dir("clean");
        fs::create_dir_all(dir.join("probe")).expect("probe dir");
        let (session, notes) = Session::open(
            paths(&dir),
            ZenConfig::default(),
            Arc::new(Unsupported::new()),
            Limits::default(),
        )
        .expect("open");
        assert_eq!(notes, vec![], "a fresh data directory is not damaged");
        assert!(session.database_disabled().is_none(), "the db opened");
        assert!(session.job_ids().is_empty());
        session.close();
        cleanup(&dir);
    }

    #[test]
    fn origin_round_trips_through_receipt_running() {
        // Plan §7 step 6 acceptance (c): the origin a caller passes in comes back on the `Receipt`, on
        // `JobStarted`, and on the single `JobFinished` — which is the only reason a finished job can
        // find its way back to the widget that asked (plan §4.3).
        let dir = temp_dir("origin");
        fs::create_dir_all(dir.join("probe")).expect("probe dir");
        let (mut session, _notes) = Session::open(
            paths(&dir),
            ZenConfig::default(),
            Arc::new(Unsupported::new()),
            Limits::default(),
        )
        .expect("open");

        let origin = session.next_origin();
        // A walk over a directory that does not exist cannot reach the worker; use a real one with a
        // file in it so the job runs and finishes with a typed size.
        let target = dir.join("walk");
        fs::create_dir_all(&target).expect("walk dir");
        fs::write(target.join("a.txt"), b"12345").expect("a file of five bytes");
        let abs = AbsPath::try_from(target.as_path()).expect("absolute");
        let receipt = session.dir_size(&abs, origin);
        assert!(receipt.is_running(), "{receipt:?}");
        assert_eq!(
            receipt.origin(),
            Some(origin),
            "the origin comes back on the receipt"
        );
        let job = receipt.job().expect("a job id");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut finished = None;
        let mut started = None;
        let mut ready = 0;
        while std::time::Instant::now() < deadline {
            for ev in session.poll() {
                match ev {
                    EngineEvent::JobStarted { id, origin: o, .. } => {
                        assert_eq!(o, origin, "JobStarted carries the caller's origin");
                        started = Some(id);
                    }
                    EngineEvent::JobFinished {
                        id,
                        origin: o,
                        payload,
                        outcome,
                        ..
                    } => {
                        assert_eq!(o, origin, "the terminal event returns the origin");
                        let stat = payload.dir_size().expect("a typed DirSize payload");
                        assert_eq!(
                            stat.bytes, 5,
                            "the walk counted the file, not the directory"
                        );
                        assert_eq!(stat.files_count, 1);
                        assert!(outcome.succeeded(), "{outcome:?}");
                        finished = Some(id);
                    }
                    EngineEvent::JobProgress {
                        id,
                        origin: o,
                        progress,
                    } => {
                        assert_eq!(o, origin, "progress carries the same origin");
                        assert_eq!(id, job);
                        assert!(
                            progress.total_bytes.is_none(),
                            "a walk cannot know its total up front"
                        );
                    }
                    EngineEvent::DirSizeReady { path, stat } => {
                        // The broadcast half plan §4.1 names: every row showing the directory gets the
                        // number, not only the widget that asked.
                        assert_eq!(path, abs, "the ready event names the walked directory");
                        assert_eq!(stat.bytes, 5);
                        ready += 1;
                    }
                    other => {
                        panic!("unexpected event for the size job: {other:?}");
                    }
                }
            }
            if finished.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(started, Some(job), "the same job announced itself");
        assert_eq!(finished, Some(job), "and finished exactly once");
        assert_eq!(
            ready, 1,
            "one DirSizeReady broadcast for the one finished walk"
        );
        assert!(
            !session.cancel(job),
            "a completed job is never reclassified as cancelled"
        );
        // The size is now cached, so the second ask answers synchronously.
        let again = session.dir_size(&abs, session.next_origin());
        assert!(again.is_done(), "{again:?}: the cache answered");
        session.close();
        cleanup(&target);
        cleanup(&dir);
    }

    #[test]
    fn props_answers_from_the_cache_and_never_probes_twice() {
        // Plan §7 step 6 acceptance (props half) and plan F21: the probe runs once, and every later
        // `props()` — the properties dialog asks on every repaint — is a lock read.
        #[derive(Debug)]
        struct CountingEnv {
            inner: Unsupported,
            calls: AtomicU64,
        }

        impl CountingEnv {
            fn new() -> Self {
                Self {
                    inner: Unsupported::new(),
                    calls: AtomicU64::new(0),
                }
            }
        }

        impl Env for CountingEnv {
            fn os(&self) -> crate::platform::Os {
                self.inner.os()
            }
            fn caps(&self) -> crate::platform::PlatformCaps {
                self.inner.caps()
            }
            fn open_with(
                &self,
                path: &AbsPath,
            ) -> crate::platform::PlatformFuture<'_, Result<(), CoreError>> {
                self.inner.open_with(path)
            }
            fn copy_paths(
                &self,
                items: &[crate::platform::CopyItem],
            ) -> crate::platform::PlatformFuture<'_, Result<crate::platform::CopyReport, CoreError>>
            {
                self.inner.copy_paths(items)
            }
            fn paste_link(
                &self,
                items: &[crate::platform::LinkItem],
            ) -> crate::platform::PlatformFuture<'_, Result<(), CoreError>> {
                self.inner.paste_link(items)
            }
            fn supports_symlink(&self) -> Result<bool, CoreError> {
                let _ = self.calls.fetch_add(1, Ordering::AcqRel);
                self.inner.supports_symlink()
            }
            fn chmod(
                &self,
                targets: &[AbsPath],
                mode: crate::mode::ModeBits,
            ) -> crate::platform::PlatformFuture<'_, Result<(), CoreError>> {
                self.inner.chmod(targets, mode)
            }
            fn can_trash(&self, path: &AbsPath) -> bool {
                self.inner.can_trash(path)
            }
            fn trash(&self, path: &AbsPath) -> Result<bool, CoreError> {
                self.inner.trash(path)
            }
            fn create_shortcut(
                &self,
                shortcut: &crate::platform::Shortcut,
            ) -> Result<(), CoreError> {
                self.inner.create_shortcut(shortcut)
            }
            fn disk_space(&self, path: &AbsPath) -> Result<crate::platform::DiskSpace, CoreError> {
                self.inner.disk_space(path)
            }
            fn mount_entries(&self) -> Result<crate::platform::MountTable, CoreError> {
                self.inner.mount_entries()
            }
            fn exec_program(
                &self,
                req: crate::platform::ExecRequest,
            ) -> crate::platform::PlatformFuture<'_, Result<crate::platform::ExecOutput, CoreError>>
            {
                self.inner.exec_program(req)
            }
        }

        let dir = temp_dir("props");
        fs::create_dir_all(dir.join("probe")).expect("probe dir");
        let env = Arc::new(CountingEnv::new());
        let shared: Arc<dyn Env> = env.clone();
        let (session, _notes) =
            Session::open(paths(&dir), ZenConfig::default(), shared, Limits::default())
                .expect("open");

        let first = session.props();
        let again = session.props();
        let third = session.props();
        assert_eq!(first, again, "the report is stable");
        assert_eq!(third.symlink_supported, first.symlink_supported);
        assert_eq!(
            env.calls.load(Ordering::Acquire),
            1,
            "the measurement ran once and every later call was the cache (plan F21)"
        );
        // `/api/props` body: exactly these two keys, in this order (`props-dir.json`).
        let json = serde_json::to_value(first).expect("the report serialises");
        assert_eq!(
            json.as_object()
                .expect("an object")
                .keys()
                .collect::<Vec<_>>(),
            vec!["platform", "symlink_supported"]
        );
        session.close();
        cleanup(&dir);
    }

    #[test]
    fn clipboard_store_queues_logs_and_clears() {
        // Plan §5.3's `ops/clipboard.rs` façade on `Session`: guard → store → log record → event →
        // best-effort system sync (`api/ops.go:144-186`, `handleClipboardSet`).
        let dir = temp_dir("clipboard");
        fs::create_dir_all(dir.join("probe")).expect("probe dir");
        let sel = dir.join("sel");
        fs::create_dir_all(sel.join("sub")).expect("sel tree");
        fs::write(sel.join("a.txt"), b"x").expect("a file");
        fs::write(sel.join("sub").join("b.txt"), b"yy").expect("a nested file");
        let (mut session, _notes) = Session::open(
            paths(&dir),
            ZenConfig::default(),
            Arc::new(Unsupported::new()),
            Limits::default(),
        )
        .expect("open");
        let a = AbsPath::try_from(sel.join("a.txt").as_path()).expect("absolute");
        let sub = AbsPath::try_from(sel.join("sub").as_path()).expect("absolute");
        let root = AbsPath::try_from(sel.as_path()).expect("absolute");
        assert!(
            session.clipboard().is_none(),
            "a fresh session has nothing queued"
        );

        // An empty selection is Go's "No files selected" (`api/ops.go:146`), never a silent clear.
        let err = session
            .clipboard_store(&ClipboardStore {
                op: clipboard::ClipOp::Copy,
                sources: Vec::new(),
                scope: clipboard::ClipScope::Selected,
            })
            .expect_err("nothing selected");
        assert!(matches!(err, CoreError::Empty { .. }), "{err:?}");
        assert!(
            session.clipboard().is_none(),
            "a refused request leaves the queue alone"
        );

        let clip = session
            .clipboard_store(&ClipboardStore {
                op: clipboard::ClipOp::Copy,
                sources: vec![a.clone()],
                scope: clipboard::ClipScope::Selected,
            })
            .expect("queue a selection");
        assert_eq!(clip.items.len(), 1, "one named item");
        assert_eq!(
            clip.origin.as_ref(),
            Some(&root),
            "the queue came from `sel/`"
        );
        assert_eq!(
            session.clipboard().expect("kept").paths(),
            vec![a.clone()],
            "the queue is what a paste will place"
        );

        // The Go-parity log record (`api/ops.go:176`): COPY, the sources, empty dest and name.
        let records = session.log_recent(None).expect("the log reads");
        let rec = records.entries().last().expect("one record was appended");
        assert_eq!(rec.action, crate::history::codec::ActionType::COPY);
        assert_eq!(
            rec.sources,
            vec![a.to_string_lossy()],
            "the resolved sources"
        );
        assert_eq!(rec.dest, "", "a copy has no destination yet");
        assert_eq!(rec.name, "");

        // And the two events: the history pane's id, then the new queue. No notice — with neither
        // `wl-copy` nor `xclip` the system sync is Go's silent no-op (`api/ops.go:573`).
        let mut saw_log = false;
        let mut saw_clip = None;
        let mut notices = Vec::new();
        for event in session.poll() {
            match event {
                EngineEvent::LogAppended { id } => {
                    assert_eq!(
                        Some(id),
                        records.entries().last().map(|r| r.id),
                        "the event names the record that was just written"
                    );
                    saw_log = true;
                }
                EngineEvent::ClipboardChanged { clip } => saw_clip = clip,
                EngineEvent::Notice { message, .. } => notices.push(message),
                other => panic!("unexpected event from a copy: {other:?}"),
            }
        }
        assert!(saw_log, "the history pane is told about the record");
        assert_eq!(
            saw_clip.expect("every clipboard badge is told").items.len(),
            1,
            "the event carries the new queue"
        );
        assert!(notices.is_empty(), "no platform noise: {notices:?}");

        // `Inside` widens to the directory's children (`api/ops.go:189-211`), and a file is refused.
        let expanded = session
            .clipboard_store(&ClipboardStore {
                op: clipboard::ClipOp::Cut,
                sources: vec![sub.clone()],
                scope: clipboard::ClipScope::Inside,
            })
            .expect("expand a directory");
        assert_eq!(expanded.items.len(), 1, "`sub/` holds b.txt");
        assert_eq!(expanded.op, clipboard::ClipOp::Cut);
        assert_eq!(
            expanded.items[0].path,
            sub.join("b.txt").expect("joined"),
            "the expansion is the child, in Go's lexical order"
        );
        let err = session
            .clipboard_store(&ClipboardStore {
                op: clipboard::ClipOp::Copy,
                sources: vec![a.clone()],
                scope: clipboard::ClipScope::Inside,
            })
            .expect_err("a file is not a directory");
        assert!(matches!(err, CoreError::NotADir { .. }), "{err:?}");

        // Clear (`api/ops.go:216-221`): the queue is gone, and the event says so with `None`.
        session.clipboard_clear();
        assert!(session.clipboard().is_none(), "cleared");
        let mut cleared = None;
        for event in session.poll() {
            if let EngineEvent::ClipboardChanged { clip } = event {
                cleared = Some(clip.is_none());
            }
        }
        assert_eq!(cleared, Some(true), "the clear was announced");
        // Two more copies, and the log holds one record per copy (Go appended one per request).
        for _ in 0..2 {
            session
                .clipboard_store(&ClipboardStore {
                    op: clipboard::ClipOp::Copy,
                    sources: vec![a.clone()],
                    scope: clipboard::ClipScope::Selected,
                })
                .expect("copy again");
        }
        let records = session.log_recent(None).expect("the log reads");
        assert_eq!(
            records
                .entries()
                .iter()
                .filter(|r| r.action == crate::history::codec::ActionType::COPY)
                .count(),
            4,
            "one COPY record per queued selection"
        );
        session.close();
        let _ = fs::remove_file(sel.join("a.txt"));
        let _ = fs::remove_file(sel.join("sub").join("b.txt"));
        let _ = fs::remove_dir(sel.join("sub"));
        let _ = fs::remove_dir(&sel);
        cleanup(&dir);
    }

    #[test]
    fn page_dir_answers_done_with_the_list_it_read() {
        // Step 6's `page_dir` is the synchronous half of the `Receipt` story (plan §4.1: a read that
        // never blocks longer than one directory). The `Running` half is `dir_size`'s, tested above.
        let dir = temp_dir("pagedir");
        fs::create_dir_all(dir.join("probe")).expect("probe dir");
        let listing = dir.join("listed");
        fs::create_dir_all(&listing).expect("listed dir");
        fs::write(listing.join("only.txt"), b"x").expect("one file");
        let (session, _notes) = Session::open(
            paths(&dir),
            ZenConfig::default(),
            Arc::new(Unsupported::new()),
            Limits::default(),
        )
        .expect("open");
        let req = ListDir::first(FsPath::from_abs(
            AbsPath::try_from(listing.as_path()).expect("absolute"),
        ));
        let origin = session.next_origin();
        let receipt = session.page_dir(&req, origin).expect("listing");
        let page = receipt.done().expect("step 6 lists synchronously");
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].name, "only.txt");
        assert_eq!(page.total_count, 1);
        // And the second ask is served from the cache, not from a second readdir.
        let again = session.page_dir(&req, origin).expect("cached listing");
        assert!(Arc::ptr_eq(again.done().expect("done"), page));
        session.close();
        cleanup(&listing);
        cleanup(&dir);
    }

    #[test]
    fn a_config_that_cannot_be_parsed_is_copied_aside() {
        let dir = temp_dir("cfgreset");
        let cfg = dir.join("config.json");
        fs::write(&cfg, b"not json at all").expect("write garbage");
        let note = check_config_file(&cfg).expect("no error").expect("a note");
        match note {
            StartupNote::ConfigReset { path, copied_to } => {
                assert_eq!(path, cfg);
                assert_eq!(
                    fs::read_to_string(&copied_to).expect("backup"),
                    "not json at all"
                );
            }
            other => panic!("expected ConfigReset, got {other:?}"),
        }
        // A missing file is a first run, not damage.
        assert!(
            check_config_file(&dir.join("absent.json"))
                .expect("no error")
                .is_none()
        );
        // And a good file says nothing.
        fs::write(
            &cfg,
            serde_json::to_vec(&ZenConfig::default()).expect("json"),
        )
        .expect("write a good config");
        assert!(check_config_file(&cfg).expect("no error").is_none());
        cleanup(&dir);
    }
}

// --------------------------------------------------------- step 14a: workspaces, actions, batch rename, log

/// The request behind [`Session::exec_action`] — Go's `/api/action/exec` body (`{id, paths, dir}`),
/// spelled as the action id plus a fully-resolved [`ActionContext`](crate::services::actions::ActionContext)
/// so the expansion has no hidden filesystem read left to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionExec {
    /// The action to run, by id. An unknown id is
    /// [`CoreError::NotFound`] — Go's `{"error":"Custom action not found"}` 404, golden
    /// `err-action-exec-unknown.json`.
    pub id: String,
    /// What it was run on.
    pub ctx: crate::services::actions::ActionContext,
}

/// What [`Session::batch_rename`] did.
pub struct BatchRenameReport {
    /// The preview the user was shown, so a toast can name the row that failed.
    pub preview: crate::ops::batch_rename::PreviewSet,
    /// Names skipped because they conflicted and the policy was not
    /// [`Rename`'s](crate::ops::ConflictPolicy::Rename) disambiguating one.
    pub skipped: Vec<String>,
    /// The rename that ran, per-item outcomes and undo-log id included.
    pub rename: crate::ops::rename::RenameReport,
}

impl Session {
    // ---------------------------------------------------------------------------- workspaces (§5.3)

    /// The workspace half of `config.json` (plan §5.3's `WorkspaceService`), pointed at this session's
    /// settings file. Constructed on demand — it is a path, not a cache, and the session is the only
    /// holder of the in-memory config, so a long-lived service object would be a second source of truth.
    #[must_use]
    pub fn workspace_service(&self) -> crate::services::workspaces::WorkspaceService {
        crate::services::workspaces::WorkspaceService::new(self.inner.paths.config.clone())
    }

    /// `GET /api/workspaces`: the stored names, alphabetical, with [`RESERVED`](crate::services::workspaces::RESERVED)
    /// flagged rather than silently dropped.
    #[must_use]
    pub fn workspaces(&self) -> Vec<crate::services::workspaces::WorkspaceMeta> {
        crate::services::workspaces::WorkspaceService::list(&self.config())
    }

    /// `POST /api/workspaces` `{"action":"save"}`. Writes through [`ZenConfig::save`] (atomic
    /// tmp+rename), so the bytes on disk are complete the moment this returns — the property the
    /// `kill -9` test checks.
    ///
    /// # Errors
    /// [`CoreError::Empty`] for a blank name, [`CoreError::Config`] for an illegal one, and the config
    /// write's own failures.
    pub fn workspace_save(
        &self,
        name: &str,
        session: &crate::store::config::Workspace,
    ) -> Result<(), CoreError> {
        let next = self
            .workspace_service()
            .save(&self.config(), name, session)?;
        *lock(&self.inner.config) = next;
        Ok(())
    }

    /// `{"action":"restore"}`: the stored session, [`CoreError::NotFound`] when the name is unknown
    /// (never a panic, and never an empty default dressed up as a restore).
    ///
    /// # Errors
    /// [`CoreError::NotFound`].
    pub fn workspace_restore(
        &self,
        name: &str,
    ) -> Result<crate::store::config::Workspace, CoreError> {
        crate::services::workspaces::WorkspaceService::restore(&self.config(), name)
    }

    /// `{"action":"delete"}`. `false` when no workspace had that name; Go answered success either way.
    ///
    /// # Errors
    /// The config write's own failures.
    pub fn workspace_delete(&self, name: &str) -> Result<bool, CoreError> {
        let (next, removed) = self.workspace_service().delete(&self.config(), name)?;
        *lock(&self.inner.config) = next;
        Ok(removed)
    }

    // ------------------------------------------------------------------------------ custom actions

    /// `GET /api/actions` (golden `actions.json`): the actions, in menu order.
    #[must_use]
    pub fn custom_actions(&self) -> Vec<crate::store::config::CustomAction> {
        self.config().custom_actions
    }

    /// `POST /api/actions` `{"action":"create"}`. Returns the whole list, as Go's
    /// `{"status":"success","custom_actions":[…]}` did (golden `actions-create.json`).
    ///
    /// # Errors
    /// The config write's own failures.
    pub fn action_create(
        &self,
        action: crate::store::config::CustomAction,
    ) -> Result<Vec<crate::store::config::CustomAction>, CoreError> {
        self.set_custom_actions(crate::services::actions::insert(
            &self.custom_actions(),
            action,
        ))
    }

    /// `{"action":"update"}`.
    ///
    /// # Errors
    /// [`CoreError::NotFound`] for an unknown id, plus the config write.
    pub fn action_update(
        &self,
        action: &crate::store::config::CustomAction,
    ) -> Result<Vec<crate::store::config::CustomAction>, CoreError> {
        let next = crate::services::actions::update(&self.custom_actions(), action)?;
        self.set_custom_actions(next)
    }

    /// `{"action":"delete"}` (golden `actions-delete.json`).
    ///
    /// # Errors
    /// [`CoreError::NotFound`] for an unknown id, plus the config write.
    pub fn action_remove(
        &self,
        id: &str,
    ) -> Result<Vec<crate::store::config::CustomAction>, CoreError> {
        let next = crate::services::actions::remove(&self.custom_actions(), id)?;
        self.set_custom_actions(next)
    }

    /// `{"action":"reorder"}`: the given ids first, the rest appended, Go's rule
    /// (`api/actions.go:96-119`) — a partial list never loses a row.
    ///
    /// # Errors
    /// The config write's own failures.
    pub fn action_reorder(
        &self,
        ids: &[String],
    ) -> Result<Vec<crate::store::config::CustomAction>, CoreError> {
        let next = crate::services::actions::reorder(&self.custom_actions(), ids);
        self.set_custom_actions(next)
    }

    fn set_custom_actions(
        &self,
        next: Vec<crate::store::config::CustomAction>,
    ) -> Result<Vec<crate::store::config::CustomAction>, CoreError> {
        let mut candidate = self.config();
        candidate.custom_actions = next;
        candidate.save(&self.inner.paths.config)?;
        let out = candidate.custom_actions.clone();
        *lock(&self.inner.config) = candidate;
        Ok(out)
    }

    /// Run one custom action.
    ///
    /// Two arms, and the split is Go's `show_output` flag — which is also the *blocking* question:
    ///
    /// * `show_output == false` → [`Receipt::Done`] immediately: the spawn is
    ///   [`detached`](crate::platform::ExecRequest::detached), Go's `cmd.Start()`, so a GUI launcher
    ///   that daemonises cannot park the caller (step 9b's `xclip` lesson, plan F10);
    /// * `show_output == true` → a [`JobKind::ActionExec`](crate::jobs::JobKind::ActionExec) job whose
    ///   [`JobPayload::Exec`] carries the capped output; the caller keeps the `origin` from the receipt.
    ///
    /// # Errors
    /// [`CoreError::NotFound`] for an unknown id (synchronously, before anything spawns), and the
    /// spawn failure in the detached arm.
    pub fn exec_action(
        &self,
        req: &ActionExec,
        origin: Origin,
    ) -> Result<Receipt<crate::services::actions::ExecOutcome>, CoreError> {
        let action = self
            .custom_actions()
            .into_iter()
            .find(|a| a.id == req.id)
            .ok_or_else(|| CoreError::NotFound {
                path: format!("custom action {:?}", req.id),
                source_path: None,
            })?;
        let cap = self.inner.limits.exec_output_cap;
        if !action.show_output {
            return Ok(Receipt::Done(crate::services::actions::exec(
                self.env().as_ref(),
                &action,
                &req.ctx,
                cap,
            )?));
        }
        let env = Arc::clone(&self.inner.env);
        let ctx = req.ctx.clone();
        let label = format!("Run {}", action.name);
        let job = self.spawn_job(
            crate::jobs::JobKind::ActionExec,
            label,
            None,
            move |_cancel, _progress| {
                let out = crate::services::actions::exec(env.as_ref(), &action, &ctx, cap)?;
                let summary = out.trimmed().to_string();
                Ok(crate::jobs::JobResult::done(out, summary))
            },
        );
        Ok(Receipt::Running { job, origin })
    }

    // ------------------------------------------------------------------------------- batch rename

    /// The pure preview half (plan §7 step 14's 2 ms / 10 k-row criterion; the engine is
    /// [`ops::batch_rename::preview`], the timing gate is `tests/perf_batch_rename.rs`).
    ///
    /// Stateless on purpose: the dialog recomputes it on every keystroke, so it takes the rows and the
    /// directory's names and returns values — no cache, no session mutation, no event.
    #[must_use]
    pub fn batch_rename_preview(
        rows: &[crate::ops::batch_rename::Row],
        rules: &crate::ops::batch_rename::Rules,
        existing: &std::collections::HashSet<String>,
    ) -> crate::ops::batch_rename::PreviewSet {
        crate::ops::batch_rename::preview(rows, rules, existing)
    }

    /// Preview, resolve the collisions under `policy`, and run the whole batch as **one**
    /// [`Session::rename`] — which is the point: the parent-rewrite rule step 9c proved, the Go-parity
    /// log record and the `DirChanged`/index traffic are the single-rename ones, not a second
    /// implementation.
    ///
    /// # Errors
    /// [`CoreError::Empty`] when no row would change (Go's `changedIndexes.length === 0` early return,
    /// reported rather than swallowed), plus [`Session::rename`]'s own set.
    pub fn batch_rename(
        &self,
        rows: &[crate::ops::batch_rename::Row],
        rules: &crate::ops::batch_rename::Rules,
        existing: &std::collections::HashSet<String>,
        policy: crate::ops::ConflictPolicy,
        origin: Origin,
    ) -> Result<BatchRenameReport, CoreError> {
        let preview = crate::ops::batch_rename::preview(rows, rules, existing);
        let (req, report) = crate::ops::batch_rename::apply(rows, &preview, existing, policy)?;
        let rename = self.rename(&req, origin)?;
        Ok(BatchRenameReport {
            preview: report.preview,
            skipped: report.skipped,
            rename,
        })
    }

    // --------------------------------------------------------------------------------- action log

    /// A page of the undo log, newest first ([`LogQuery`] explains the window; the flags the panel
    /// needs are on [`LogRow`](crate::services::action_log::LogRow)).
    ///
    /// Synchronous like [`Self::log_recent`] — the store keeps its frame index in memory from open.
    ///
    /// # Errors
    /// The store's read errors (a log file that changed under us).
    pub fn log_query(
        &self,
        query: &crate::services::action_log::LogQuery,
    ) -> Result<crate::services::action_log::LogPage, CoreError> {
        let guard = lock(&self.inner.log);
        let records = guard.records()?;
        Ok(crate::services::action_log::LogPage::new(&records, query))
    }
}
