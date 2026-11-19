//! The live (filesystem) search service — plan §5.3 `services/search_live.rs`, §7 step 11, first half.
//!
//! This is Go's `HandleSearch` (`api/search.go`) as an in-process, cancellable, throttled walk. The
//! indexed FTS path is `services/search_index.rs` (step 12); nothing here touches the database.
//!
//! # The wire semantics this reproduces (all `api/search.go`, cross-checked against `tests/golden/search-*.json`)
//!
//! | Behaviour | Go | Here |
//! |---|---|---|
//! | result cap | `limit` default **500** (`:67-74`); `limit <= 0` → **1 000 000** | [`SearchQuery::limit`], `0` means the same ceiling ([`UNBOUNDED_LIMIT`]) |
//! | cap semantics | checked on a *match*, before appending: `capped` only when a `(limit+1)`-th match exists (`:119-122`) | identical — a walk that finds exactly `limit` rows reports `truncated: false` |
//! | `total_matched` | `len(matches)`, **not** the number of names that matched (`:222-226`) | [`SearchStatus::total_matched`], same |
//! | order | `filepath.WalkDir` pre-order, per directory in `os.ReadDir` name order; **not** dirs-first (`:108-160`, §2.4 row *`/api/search` (live)*) | identical, and asserted stable (§9.2 step-11a) |
//! | `rel_path` | `filepath.Rel(root, path)`, and just the name in the non-recursive branch (`:129`, `:196`) | identical |
//! | hidden files | never filtered (§2.3) | [`HiddenPolicy::Include`] is the default; `Hide` is an opt-in the Go UI never had |
//! | extension filter | **does not exist** in the live path (it is index-only: `api/search_index.go`, `api/search_index_api.go`) | not added here either; step 12 owns it |
//! | scope filter (dirs/files) | **does not exist** in the live path | plan §5.3 names [`ScopeFilter`]; its default [`ScopeFilter::Both`] is Go's behaviour |
//! | stat failure | row keeps `size:0`, `mod_time:0`, `mode:""` (`:136-144`) | the port's degraded row (`mode:"unknown"`, zero-time sentinel) — §2.4's deliberate deviation, same as `/api/dir` |
//! | bad regex | 400 `Invalid regex: …` (`:83-86`) | [`CoreError::BadRegex`], raised **before** the job starts so the dialog can show it inline |
//! | cancellation | the request context, checked per entry (`:111-113`) | [`CancelToken::check`] per entry, so a superseded walk stops on the next name |
//!
//! # Why the walk is a job and the throttle is here
//!
//! A keystroke re-issues the query, and a walk of a large tree is exactly the "one `readdir` storm per
//! keystroke" plan §4.3 forbids on the render path. §7 step 11's criterion is **≤1 walk per 250 ms of
//! typing**, and the only way that is a *rule* rather than a habit is for the counter to live in core:
//! [`SearchThrottle`] owns the window, [`Session::search_live`](crate::session::Session::search_live)
//! consults it, and a caller that ignores the answer still cannot start a second walk. A throttled
//! request is *remembered*, not dropped: the newest deferred query starts itself from
//! [`Session::poll`](crate::session::Session::poll) once the window closes, so the last keystroke always
//! wins and no timer is the UI's responsibility.
//!
//! # Cancellation and staleness
//!
//! Starting a search supersedes the previous one: its job is cancelled (Go aborted the previous request
//! the same way, by closing its HTTP request). A superseded or cancelled job still emits its one
//! terminal `JobFinished` — the job table's contract — but it never emits [`EngineEvent::SearchReady`],
//! because the session's [`SearchLane`] only forwards the result of the job it currently considers live,
//! and that event carries the `origin` so the pane can drop an answer that is not its own (§4.3's
//! correlation rule).

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cursor::Cursor;
use crate::entry::{EntryBuilder, EntryStat, FileEntry};
use crate::error::CoreError;
use crate::jobs::CancelToken;
use crate::limits::Limits;
use crate::path::{AbsPath, FsPath};
use crate::services::dirs::FsSource;

pub use crate::services::name_match::{NameMatcher, NameMode, compile_name_match};

// --------------------------------------------------------------------------------------------- numbers

/// Go's default `limit` when the request has none (`api/search.go:66`, `limit := 500`).
pub const DEFAULT_LIMIT: usize = 500;

/// What Go turns `limit <= 0` into: "effectively unlimited safety ceiling" (`api/search.go:72`), i.e.
/// [`crate::limits::LIVE_SEARCH_CAP`] / [`Limits::live_search_cap`] as a `usize`.
pub const UNBOUNDED_LIMIT: usize = crate::limits::LIVE_SEARCH_CAP as usize;

/// Plan §7 step 11's typing cadence: at most one walk per this interval.
pub const THROTTLE_WINDOW: Duration = Duration::from_millis(250);

/// Port-side depth ceiling for a live walk. Go's `filepath.WalkDir` is unbounded (`api/search.go:109`);
/// the port bounds it like every other recursive walk (plan §5.3's `copy_tree` uses 128), so a
/// pathological or self-referential tree cannot pin a job slot forever.
pub const MAX_DEPTH: usize = 128;

/// The row the UI asks for: one match, with the entry exactly as `/api/dir` would render it plus the
/// search-root-relative path Go's `SearchEntry.rel_path` carries (`api/search.go:24`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SearchHit {
    /// `name`/`is_dir`/`size`/`mod_time`/`mode` — the same `Entry` a listing row uses, so the quick-find
    /// list and the file pane draw identically.
    pub entry: FileEntry,
    /// `rel_path`: path from the search root (`"sub/b.txt"`), or the bare name in a non-recursive search
    /// (`api/search.go:196`).
    pub rel_path: String,
}

impl SearchHit {
    /// The match's base name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.entry.name
    }

    /// `true` for a directory row (`api/search.go:20`, `is_dir`).
    #[must_use]
    pub fn is_dir(&self) -> bool {
        self.entry.is_dir
    }
}

/// How a walk ended. `truncated` is Go's `capped` (`api/search.go:75`, `:120`); the rest are the port's
/// walk budgets, each of which stops the walk instead of hanging it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SearchStatus {
    /// Rows kept. Go's `total_matched` is this same number (`api/search.go:225`), *not* the count of
    /// names that matched — a truncated search reports the cap, not the truth.
    pub total_matched: usize,
    /// Go's `capped`: a `(limit+1)`-th match existed (`api/search.go:119-122`).
    pub truncated: bool,
    /// Names examined (Go's `WalkDir` callback invocations, root excluded).
    pub nodes_visited: usize,
    /// Directories met (`d.IsDir()`, `api/search.go:126`), whether or not the walk entered them: a
    /// non-recursive scan still sees the directories in the scope.
    pub dirs_visited: usize,
    /// Directories that refused to read — Go skips them (`api/search.go:115-119`, "skip folders we can't
    /// access"); the count is ours.
    pub dirs_skipped: usize,
    /// The walk stopped descending because [`SearchBudget::max_depth`] was reached (port-side).
    pub depth_truncated: bool,
    /// The walk stopped because [`SearchBudget::max_nodes`] was spent (port-side; plan §5.2's
    /// `live_search_nodes`).
    pub node_budget_hit: bool,
    /// The walk stopped because [`SearchBudget::max_elapsed_ms`] elapsed (port-side).
    pub timed_out: bool,
    /// Wall-clock length of the walk, so a slow root is visible in the jobs pane.
    pub elapsed_ms: u64,
}

impl SearchStatus {
    /// Go's wire field name for [`Self::truncated`].
    #[must_use]
    pub const fn capped(&self) -> bool {
        self.truncated
    }

    /// `true` when any budget (rows, nodes, depth, time) cut the walk short.
    #[must_use]
    pub fn limited(&self) -> bool {
        self.truncated || self.node_budget_hit || self.depth_truncated || self.timed_out
    }
}

/// The plan's name for [`SearchStatus`] (`LiveSearchStats`); both spellings are exported so §5.3 and
/// the step-11 UI row name the same thing.
pub type LiveSearchStats = SearchStatus;

/// Dirs, files, or either (`api/search.go` has no such filter; plan §5.3's `ScopeFilter` does, and its
/// default is Go's behaviour).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeFilter {
    /// Both — what Go always searched.
    #[default]
    Both,
    /// Directories only.
    Dirs,
    /// Non-directories only (a symlink to a directory is a directory, per plan F5).
    Files,
}

/// Whether dot-files participate. Go's backend never filtered hidden files (§2.3), so the default is
/// [`Self::Include`]; `Hide` is the option the port exposes and the UI may default to per dialog.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HiddenPolicy {
    /// Go's behaviour: `.git`, `.bashrc` and friends are searchable rows.
    #[default]
    Include,
    /// Skip names starting with `.` (except the `.`/`..` spellings, which are not hidden markers here).
    Hide,
}

/// The walk's budgets other than the row cap. Every default comes from [`Limits`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SearchBudget {
    /// Names examined before the walk gives up ([`Limits::live_search_nodes`]).
    pub max_nodes: usize,
    /// Descent ceiling ([`MAX_DEPTH`]).
    pub max_depth: usize,
    /// Wall-clock ceiling in milliseconds; `None` = unbounded (cancellation is the only stop).
    pub max_elapsed_ms: Option<u64>,
}

impl Default for SearchBudget {
    fn default() -> Self {
        Self::from_limits(&Limits::default())
    }
}

impl SearchBudget {
    /// The budgets plan §5.2 measured for this engine.
    #[must_use]
    pub fn from_limits(limits: &Limits) -> Self {
        Self {
            max_nodes: limits.live_search_nodes,
            max_depth: MAX_DEPTH,
            max_elapsed_ms: None,
        }
    }
}

/// One search request (§5.3's `LiveSearchReq`; the plan's name is exported as [`SearchQuery`]'s alias).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct SearchQuery {
    /// The directory the walk starts in. Go's `path` parameter, resolved and required to be a directory
    /// (`api/search.go:43-60`) — an archive path has no business here.
    pub scope: AbsPath,
    /// The raw query, exactly as typed (the matcher does its own lowercasing/tokenising).
    pub query: String,
    /// Substring, word or regex (`api/search.go`'s `regex` flag plus its default branch).
    pub mode: NameMode,
    /// Dirs/files/both.
    pub scope_filter: ScopeFilter,
    /// Go's `recursive`: `false` scans the scope's own entries only (`api/search.go:162-201`).
    pub recursive: bool,
    /// Row cap. `0` is Go's `limit <= 0` and means [`UNBOUNDED_LIMIT`]; the UI default is
    /// [`DEFAULT_LIMIT`].
    pub limit: usize,
    /// Reserved: the live search has no paging of its own (Go re-issued the request per keystroke). The
    /// cursor is carried so step 11b can page a *cached* result set without inventing a second type.
    pub cursor: Cursor,
    /// Hidden-file policy.
    pub hidden: HiddenPolicy,
    /// The walk budgets.
    pub budget: SearchBudget,
}

/// §5.3's spelling of [`SearchQuery`].
pub type LiveSearchReq = SearchQuery;

impl SearchQuery {
    /// A recursive substring search of `scope` with every default at Go's value.
    #[must_use]
    pub fn new(scope: AbsPath, query: impl Into<String>) -> Self {
        Self {
            scope,
            query: query.into(),
            mode: NameMode::default(),
            scope_filter: ScopeFilter::default(),
            recursive: true,
            limit: DEFAULT_LIMIT,
            cursor: Cursor::FIRST,
            hidden: HiddenPolicy::default(),
            budget: SearchBudget::default(),
        }
    }

    /// The effective row cap: `0` is Go's "unlimited" (`api/search.go:71-73`).
    #[must_use]
    pub fn result_cap(&self) -> usize {
        if self.limit == 0 {
            UNBOUNDED_LIMIT
        } else {
            self.limit
        }
    }

    /// Compile the name predicate. Kept separate from [`SearchService::run`] so the session can refuse a
    /// bad pattern before spending a job slot on it.
    ///
    /// # Errors
    /// [`CoreError::BadRegex`] for an unparseable regex.
    pub fn matcher(&self) -> Result<NameMatcher, CoreError> {
        compile_name_match(&self.query, self.mode)
    }
}

/// A finished search: the request that produced it, its rows, and how the walk ended. The whole triple
/// travels together in [`crate::events::EngineEvent::SearchReady`] so a pane can tell a stale answer from
/// a fresh one by comparing the query, not just the origin.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SearchResults {
    /// The request that produced these rows.
    pub query: SearchQuery,
    /// The rows, in walk order (Go's order; not dirs-first — plan §2.4).
    pub hits: Vec<SearchHit>,
    /// How the walk ended.
    pub status: SearchStatus,
}

impl SearchResults {
    /// Row count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.hits.len()
    }

    /// `true` when nothing matched — Go's `entries: null` (`search-live-recursive.json`).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }

    /// The rows.
    #[must_use]
    pub fn hits(&self) -> &[SearchHit] {
        &self.hits
    }
}

// ------------------------------------------------------------------------------------------ the service

/// The live search service: the filesystem seam plus the numbers the walk is bounded by.
#[derive(Clone)]
pub struct SearchService {
    source: Arc<dyn FsSource>,
    limits: Arc<Limits>,
}

impl std::fmt::Debug for SearchService {
    /// Hand-written: the source is a filesystem, which has nothing worth printing.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchService")
            .field("nodes", &self.limits.live_search_nodes)
            .finish_non_exhaustive()
    }
}

impl SearchService {
    /// Build the service over the same [`FsSource`] the directory service uses (rule 7: one place under
    /// `services/` touches the filesystem, and it is swappable so the walk is testable).
    #[must_use]
    pub fn new(source: Arc<dyn FsSource>, limits: Arc<Limits>) -> Self {
        Self { source, limits }
    }

    /// The limits the budgets default from.
    #[must_use]
    pub fn limits(&self) -> &Arc<Limits> {
        &self.limits
    }

    /// Run `req` and collect the rows (the form the job body uses).
    ///
    /// # Errors
    /// [`CoreError::NotFound`] (the scope is not there), [`CoreError::NotADir`],
    /// [`CoreError::BadRegex`], [`CoreError::Cancelled`], or the source's own error for the scope itself.
    pub fn search(
        &self,
        req: &SearchQuery,
        cancel: &CancelToken,
    ) -> Result<Arc<SearchResults>, CoreError> {
        let mut hits = Vec::new();
        let status = self.run(req, cancel, &mut |hit| hits.push(hit))?;
        Ok(Arc::new(SearchResults {
            query: req.clone(),
            hits,
            status,
        }))
    }

    /// The streaming walk: `sink` sees every row as it is found, which is what a cancellable job wants.
    ///
    /// # Errors
    /// As [`Self::search`]. A cancelled walk returns [`CoreError::Cancelled`] and the rows already handed
    /// to `sink` are simply dropped by the caller (Go discarded the partial response the same way).
    pub fn run(
        &self,
        req: &SearchQuery,
        cancel: &CancelToken,
        sink: &mut dyn FnMut(SearchHit),
    ) -> Result<SearchStatus, CoreError> {
        run(req, self.source.as_ref(), &self.limits, cancel, sink)
    }
}

/// §5.3's free-function form of the walk.
///
/// # Errors
/// As [`SearchService::run`].
pub fn run(
    req: &SearchQuery,
    source: &dyn FsSource,
    limits: &Limits,
    cancel: &CancelToken,
    sink: &mut dyn FnMut(SearchHit),
) -> Result<SearchStatus, CoreError> {
    cancel.check()?;
    let budget = SearchBudget {
        max_nodes: if req.budget.max_nodes == 0 {
            limits.live_search_nodes
        } else {
            req.budget.max_nodes
        },
        max_depth: if req.budget.max_depth == 0 {
            MAX_DEPTH
        } else {
            req.budget.max_depth
        },
        max_elapsed_ms: req.budget.max_elapsed_ms,
    };
    let mut walker = Walker {
        source,
        root: req.scope.clone(),
        root_fs: FsPath::from_abs(req.scope.clone()),
        req,
        matcher: req.matcher()?,
        cap: req.result_cap(),
        budget,
        started: Instant::now(),
        status: SearchStatus::default(),
        hits: Vec::new(),
    };
    walker.walk(cancel, sink)?;
    Ok(walker.finish())
}

/// The walk itself: `filepath.WalkDir`'s pre-order over `os.ReadDir`'s name-sorted siblings, as an
/// explicit stack (the same shape `DirService::walk_flat` uses, so the two walks agree on order).
struct Walker<'a> {
    source: &'a dyn FsSource,
    root: AbsPath,
    root_fs: FsPath,
    req: &'a SearchQuery,
    matcher: NameMatcher,
    cap: usize,
    budget: SearchBudget,
    started: Instant,
    status: SearchStatus,
    hits: Vec<SearchHit>,
}

/// One directory being iterated.
struct Frame {
    path: AbsPath,
    rel: String,
    depth: usize,
    names: Vec<crate::entry::DirEntryLike>,
    idx: usize,
}

impl Frame {
    fn new(
        path: AbsPath,
        rel: String,
        depth: usize,
        mut names: Vec<crate::entry::DirEntryLike>,
    ) -> Self {
        // `os.ReadDir` sorts by name (`api/dir.go`, `api/search.go:163`), and `WalkDir` walks in that
        // order: byte order on the raw name, case-sensitively, *not* the listing's dirs-first comparator.
        names.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        Self {
            path,
            rel,
            depth,
            names,
            idx: 0,
        }
    }
}

impl Walker<'_> {
    fn walk(
        &mut self,
        cancel: &CancelToken,
        sink: &mut dyn FnMut(SearchHit),
    ) -> Result<(), CoreError> {
        cancel.check()?;
        // Go's `os.Stat` (`api/search.go:47`): it follows a symlink, so a linked directory is a valid
        // scope, and a missing one is the 404 (`err-search-nonexistent.json`).
        let meta = self
            .source
            .lstat(self.root.as_path())
            .map_err(|_| CoreError::NotFound {
                path: self.root.to_string_lossy(),
                source_path: None,
            })?;
        if !meta.kind.is_navigable_dir() {
            return Err(CoreError::NotADir {
                path: self.root.to_string_lossy(),
            });
        }
        let root_names = self.source.read_dir(&self.root)?;
        let mut stack = vec![Frame::new(self.root.clone(), String::new(), 0, root_names)];
        while let Some(frame) = stack.last_mut() {
            if frame.idx >= frame.names.len() {
                stack.pop();
                continue;
            }
            let entry = frame.names[frame.idx].clone();
            frame.idx += 1;
            let (parent, parent_rel, depth) = (frame.path.clone(), frame.rel.clone(), frame.depth);
            // Go checks the request context once per visited name (`api/search.go:111-113`, `:174-177`).
            cancel.check()?;
            self.status.nodes_visited += 1;
            if self.status.nodes_visited > self.budget.max_nodes {
                self.status.node_budget_hit = true;
                return Ok(());
            }
            if self.time_spent() {
                return Ok(());
            }
            let Ok(child) = parent.join(&entry.name) else {
                continue;
            };
            let rel = if parent_rel.is_empty() {
                entry.name.clone()
            } else {
                format!("{parent_rel}/{}", entry.name)
            };
            // `d.IsDir()` from the readdir type: Go neither reports a linked directory as a directory nor
            // descends into it (`api/search.go:126`, `filepath.WalkDir` does not follow links).
            let is_dir = entry.kind.is_dir();
            if is_dir {
                self.status.dirs_visited += 1;
            }
            if self.passes(&entry.name, is_dir) && !self.record(&child, &rel, entry, depth, sink)? {
                return Ok(());
            }
            if self.req.recursive && is_dir {
                if depth + 1 > self.budget.max_depth {
                    self.status.depth_truncated = true;
                    continue;
                }
                match self.source.read_dir(&child) {
                    Ok(names) => stack.push(Frame::new(child, rel, depth + 1, names)),
                    // Go's callback returns `SkipDir` on a walk error: an unreadable directory is skipped,
                    // never a failed search (`api/search.go:115-119`).
                    Err(_) => self.status.dirs_skipped += 1,
                }
            }
        }
        Ok(())
    }

    fn time_spent(&mut self) -> bool {
        let Some(ms) = self.budget.max_elapsed_ms else {
            return false;
        };
        if self.started.elapsed() < Duration::from_millis(ms) {
            return false;
        }
        self.status.timed_out = true;
        true
    }

    /// The hidden/scope filters. Name matching is the matcher's job; the root itself is never a row
    /// (`api/search.go:123-125`).
    fn passes(&self, name: &str, is_dir: bool) -> bool {
        match self.req.scope_filter {
            ScopeFilter::Dirs if !is_dir => return false,
            ScopeFilter::Files if is_dir => return false,
            _ => {}
        }
        !(self.req.hidden == HiddenPolicy::Hide && is_hidden(name))
    }

    /// Match, cap-check and emit. `false` means "the row cap was hit; stop the whole walk" — Go's
    /// `filepath.SkipAll` (`api/search.go:119-122`).
    fn record(
        &mut self,
        child: &AbsPath,
        rel: &str,
        dirent: crate::entry::DirEntryLike,
        depth: usize,
        sink: &mut dyn FnMut(SearchHit),
    ) -> Result<bool, CoreError> {
        if !self.matcher.matches(&dirent.name) {
            return Ok(true);
        }
        if self.hits.len() >= self.cap {
            self.status.truncated = true;
            return Ok(false);
        }
        // Go's `d.Info()` (`api/search.go:136-144`): `lstat`, so a link row shows the link's own size and
        // mode. A failure degrades the row instead of dropping it.
        let info = self.source.lstat(child.as_path()).ok();
        let stat = EntryStat::in_listing(
            FsPath::from_abs(child.clone()),
            self.root_fs.clone(),
            dirent,
            info,
        );
        let entry = EntryBuilder::from_stat(&stat)
            .depth(i32::try_from(depth).unwrap_or(i32::MAX))
            .files_count(None)
            .build();
        let hit = SearchHit {
            entry,
            rel_path: rel.to_string(),
        };
        self.hits.push(hit.clone());
        sink(hit);
        Ok(true)
    }

    fn finish(&mut self) -> SearchStatus {
        self.status.total_matched = self.hits.len();
        self.status.elapsed_ms =
            u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.status.clone()
    }
}

/// Go has no hidden-file rule; the port's `Hide` mode uses the one rule every file manager shares.
fn is_hidden(name: &str) -> bool {
    name.starts_with('.') && name != "." && name != ".."
}

// ------------------------------------------------------------------------------------------ the throttle

/// The "≤1 walk per 250 ms" rule, as data (`SearchThrottle`), not as a UI habit.
///
/// The clock is a parameter ([`Self::try_acquire_at`]) so the boundary is asserted exactly — 249 ms is
/// refused, 250 ms is accepted — instead of slept on.
#[derive(Debug)]
pub struct SearchThrottle {
    window: Duration,
    last: std::sync::Mutex<Option<Instant>>,
}

impl Default for SearchThrottle {
    fn default() -> Self {
        Self::new(THROTTLE_WINDOW)
    }
}

impl SearchThrottle {
    /// A throttle with a custom window (0 disables throttling: every request is admitted).
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            last: std::sync::Mutex::new(None),
        }
    }

    /// The window this throttle enforces.
    #[must_use]
    pub const fn window(&self) -> Duration {
        self.window
    }

    /// The instant the last admitted walk started.
    #[must_use]
    pub fn last_start(&self) -> Option<Instant> {
        *lock(&self.last)
    }

    /// Forget the window (a test seam, and what a mode change should do: a new scope is not "more typing
    /// in the same box").
    pub fn reset(&self) {
        *lock(&self.last) = None;
    }

    /// `Ok(())` when a walk may start now (and the window restarts); `Err(after)` says how long remains.
    ///
    /// The comparison is `>=`, so a request exactly one window after the last admitted one is admitted —
    /// one walk per window, never two inside one.
    pub fn try_acquire_at(&self, now: Instant) -> Result<(), Duration> {
        let mut guard = lock(&self.last);
        if let Some(prev) = *guard {
            let elapsed = now.saturating_duration_since(prev);
            if elapsed < self.window {
                return Err(self.window - elapsed);
            }
        }
        *guard = Some(now);
        Ok(())
    }

    /// [`Self::try_acquire_at`] with the wall clock.
    pub fn acquire(&self) -> Result<(), Duration> {
        self.try_acquire_at(Instant::now())
    }
}

/// What the session did with a search request (§4.1's `Receipt` answer, narrowed to the two outcomes a
/// search has).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchAdmission {
    /// A walk started; its rows arrive as [`crate::events::EngineEvent::SearchReady`] for this origin.
    Running {
        /// The job.
        job: crate::jobs::JobId,
        /// The caller's correlation token, returned to sender.
        origin: crate::session::Origin,
    },
    /// Suppressed by [`SearchThrottle`]. The request is remembered and will start on its own when the
    /// window closes (a later query replaces it), so the caller only has to keep showing the spinner.
    Deferred {
        /// The caller's correlation token.
        origin: crate::session::Origin,
        /// How long until the window closes — hand it to `Context::request_repaint_after`.
        after: Duration,
    },
}

impl SearchAdmission {
    /// The origin this answer belongs to.
    #[must_use]
    pub const fn origin(&self) -> crate::session::Origin {
        match self {
            Self::Running { origin, .. } | Self::Deferred { origin, .. } => *origin,
        }
    }

    /// The job, when a walk started.
    #[must_use]
    pub const fn job(&self) -> Option<crate::jobs::JobId> {
        match self {
            Self::Running { job, .. } => Some(*job),
            Self::Deferred { .. } => None,
        }
    }

    /// `true` when a walk is running.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }
}

/// The search lane: the throttle, the job whose answer still counts, and the newest deferred request.
/// Owned by the session (`Session::search_live`), public so its state is assertable.
#[derive(Debug)]
pub struct SearchLane {
    throttle: SearchThrottle,
    live: Option<crate::jobs::JobId>,
    deferred: Option<DeferredSearch>,
}

/// A request the throttle refused, kept so the newest keystroke still gets its answer.
#[derive(Clone, Debug)]
pub struct DeferredSearch {
    /// The request that is waiting.
    pub query: SearchQuery,
    /// Whose request it is.
    pub origin: crate::session::Origin,
    /// When the window closes and it may start.
    pub due: Instant,
}

impl Default for SearchLane {
    fn default() -> Self {
        Self::new(THROTTLE_WINDOW)
    }
}

impl SearchLane {
    /// A lane with the given cadence.
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            throttle: SearchThrottle::new(window),
            live: None,
            deferred: None,
        }
    }

    /// The cadence in force.
    #[must_use]
    pub const fn window(&self) -> Duration {
        self.throttle.window()
    }

    /// The throttle (also reachable through [`Self::try_acquire_at`] for a test that wants the boundary).
    #[must_use]
    pub fn throttle(&self) -> &SearchThrottle {
        &self.throttle
    }

    /// The job whose result the session will forward, if any.
    #[must_use]
    pub const fn live(&self) -> Option<crate::jobs::JobId> {
        self.live
    }

    /// `true` when `job` is the search whose answer still counts — the staleness filter
    /// (`Session::poll`'s `SearchReady` synthesis).
    #[must_use]
    pub fn is_current(&self, job: crate::jobs::JobId) -> bool {
        self.live == Some(job)
    }

    /// The request waiting on the throttle, if any.
    #[must_use]
    pub fn deferred(&self) -> Option<&DeferredSearch> {
        self.deferred.as_ref()
    }

    /// Record an accepted start: this job's answer is the one that still counts, and any deferred
    /// request it superseded is forgotten ("the newest keystroke wins").
    pub fn mark_started(&mut self, job: crate::jobs::JobId) {
        self.live = Some(job);
        self.deferred = None;
    }

    /// Queue a refused request, replacing whatever was waiting.
    pub fn defer(&mut self, query: SearchQuery, origin: crate::session::Origin, due: Instant) {
        self.deferred = Some(DeferredSearch { query, origin, due });
    }

    /// Take the deferred request if its window has closed.
    pub fn take_due(&mut self, now: Instant) -> Option<DeferredSearch> {
        let due = self.deferred.as_ref().is_some_and(|d| d.due <= now);
        if due { self.deferred.take() } else { None }
    }
}

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use super::*;
    use crate::entry::{DirEntryLike, EntryKind, InfoEntry};
    use crate::jobs::JobId;
    use crate::session::Origin;

    /// An in-memory tree: [`FsSource`] is the sanctioned seam, so the walk is testable without a disk.
    #[derive(Default)]
    struct Mem {
        dirs: BTreeMap<String, Vec<DirEntryLike>>,
    }

    impl Mem {
        fn add(&mut self, dir: &str, name: &str, kind: EntryKind) {
            self.dirs
                .entry(dir.to_string())
                .or_default()
                .push(DirEntryLike::new(name, kind));
        }

        fn knows(&self, path: &str) -> bool {
            self.dirs.contains_key(path)
                || self.dirs.values().flatten().any(|e| {
                    self.dirs.keys().any(|d| {
                        format!("{d}/{}", e.name) == path
                            || (d == "/" && format!("/{}", e.name) == path)
                    })
                })
        }
    }

    impl FsSource for Mem {
        fn read_dir(&self, path: &AbsPath) -> Result<Vec<DirEntryLike>, CoreError> {
            self.dirs
                .get(path.to_string_lossy().as_str())
                .cloned()
                .ok_or_else(|| CoreError::NotFound {
                    path: path.to_string_lossy(),
                    source_path: None,
                })
        }

        fn lstat(&self, path: &std::path::Path) -> Result<InfoEntry, CoreError> {
            let full = path.display().to_string();
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| full.clone());
            if self.dirs.contains_key(full.as_str()) {
                return Ok(InfoEntry::new(
                    name,
                    EntryKind::Dir,
                    4096,
                    Some(1_700_000_000),
                    Some(0o040_755),
                ));
            }
            if self.knows(&full) {
                return Ok(InfoEntry::new(
                    name,
                    EntryKind::File,
                    3,
                    Some(1_700_000_000),
                    Some(0o100_644),
                ));
            }
            Err(CoreError::NotFound {
                path: full,
                source_path: None,
            })
        }
    }

    /// `/` → `.hidden.txt`, `alpha.txt`, `beta.txt`, `sub/{deep/delta.log, gamma.txt}`.
    fn sample() -> Mem {
        let mut m = Mem::default();
        m.add("/", "alpha.txt", EntryKind::File);
        m.add("/", "beta.txt", EntryKind::File);
        m.add("/", ".hidden.txt", EntryKind::File);
        m.add("/", "sub", EntryKind::Dir);
        m.add("/sub", "gamma.txt", EntryKind::File);
        m.add("/sub", "deep", EntryKind::Dir);
        m.add("/sub/deep", "delta.log", EntryKind::File);
        m
    }

    fn service(m: Mem) -> SearchService {
        SearchService::new(Arc::new(m), Arc::new(Limits::default()))
    }

    fn root() -> AbsPath {
        AbsPath::from_path("/").unwrap_or_else(|e| panic!("{e}"))
    }

    fn names(hits: &[SearchHit]) -> Vec<String> {
        hits.iter().map(|h| h.name().to_string()).collect()
    }

    fn expect_names(list: &[SearchHit], want: &[&str]) -> Vec<String> {
        let got = names(list);
        assert_eq!(
            got.iter().map(String::as_str).collect::<Vec<&str>>(),
            want,
            "walk order is Go's: `ReadDir` name order per directory, pre-order, never dirs-first"
        );
        got
    }

    /// Run a search and collect the streamed rows (which is also the proof the sink sees them).
    fn search(svc: &SearchService, q: &SearchQuery) -> SearchResults {
        let hits = Mutex::new(Vec::new());
        let status = svc
            .run(q, &CancelToken::new(), &mut |h| {
                lock(&hits).push(h);
            })
            .unwrap_or_else(|e| panic!("search {:?} failed: {e}", q.query));
        let hits = Mutex::into_inner(hits).unwrap_or_else(|e| e.into_inner());
        assert_eq!(hits.len(), status.total_matched);
        SearchResults {
            query: q.clone(),
            hits,
            status,
        }
    }

    #[test]
    fn walk_order_is_preorder_in_read_dir_order() {
        let svc = service(sample());
        let q = SearchQuery::new(root(), "");
        let r = search(&svc, &q);
        expect_names(
            &r.hits,
            &[
                ".hidden.txt",
                "alpha.txt",
                "beta.txt",
                "sub",
                // `deep` sorts before `gamma.txt`, and its own row comes first: that is `WalkDir`'s
                // pre-order, which is the whole point of this test.
                "deep",
                "delta.log",
                "gamma.txt",
            ],
        );
        assert_eq!(r.status.total_matched, 7);
        assert!(!r.status.truncated);
        assert!(!r.status.limited());
        assert_eq!(r.status.dirs_visited, 2);
    }

    #[test]
    fn rel_path_is_root_relative_and_flat_is_the_name() {
        let svc = service(sample());
        let mut q = SearchQuery::new(root(), "txt");
        let r = search(&svc, &q);
        expect_names(
            &r.hits,
            &[".hidden.txt", "alpha.txt", "beta.txt", "gamma.txt"],
        );
        let deep = r.hits.iter().find(|h| h.name() == "gamma.txt");
        assert_eq!(deep.map(|h| h.rel_path.as_str()), Some("sub/gamma.txt"));
        assert_eq!(
            r.hits
                .iter()
                .find(|h| h.name() == "alpha.txt")
                .map(|h| h.rel_path.as_str()),
            Some("alpha.txt")
        );
        // `api/search.go:162-201`: non-recursive is a flat `ReadDir` of the scope, `rel_path` = name.
        q.recursive = false;
        let flat = search(&svc, &q);
        expect_names(&flat.hits, &[".hidden.txt", "alpha.txt", "beta.txt"]);
        assert_eq!(flat.status.nodes_visited, 4);
        assert_eq!(flat.status.dirs_visited, 1, "`sub` is seen but not entered");
        assert_eq!(flat.status.dirs_skipped, 0);
    }

    #[test]
    fn hidden_and_scope_filters() {
        let svc = service(sample());
        let mut q = SearchQuery::new(root(), "");
        // Go's default: hidden rows are returned (`docs/contracts.md`: the backend never filters them).
        assert!(
            names(&search(&svc, &q).hits)
                .iter()
                .any(|n| n == ".hidden.txt")
        );
        q.hidden = HiddenPolicy::Hide;
        assert!(
            !names(&search(&svc, &q).hits)
                .iter()
                .any(|n| n == ".hidden.txt")
        );
        q.hidden = HiddenPolicy::Include;
        q.scope_filter = ScopeFilter::Dirs;
        expect_names(&search(&svc, &q).hits, &["sub", "deep"]);
        q.scope_filter = ScopeFilter::Files;
        let files = search(&svc, &q);
        assert!(files.hits.iter().all(|h| !h.is_dir()));
        assert_eq!(names(&files.hits).len(), 5);
    }

    #[test]
    fn regex_mode_is_case_insensitive_and_typed_on_error() {
        let svc = service(sample());
        let mut q = SearchQuery::new(root(), r"\.TXT$");
        q.mode = NameMode::Regex;
        // `(?i)` is hardcoded by Go (`api/search.go:82`), so an uppercase pattern still hits lowercase names.
        expect_names(
            &search(&svc, &q).hits,
            &[".hidden.txt", "alpha.txt", "beta.txt", "gamma.txt"],
        );
        q.query = "[".to_string();
        let e = svc
            .run(&q, &CancelToken::new(), &mut |_| {})
            .expect_err("an unparseable pattern is an error, not a walk");
        assert_eq!(e.kind(), "bad_regex");
        assert_eq!(
            e.to_string(),
            "Invalid regex: error parsing regexp: missing closing ]: `[`"
        );
    }

    #[test]
    fn every_budget_is_reported_by_its_own_flag() {
        let svc = service(sample());
        // The sample tree has four `.txt` matches, which is what the cap arithmetic below is about.
        let mut q = SearchQuery::new(root(), "txt");
        assert_eq!(search(&svc, &q).len(), 4);
        q.limit = 2;
        let r = search(&svc, &q);
        assert_eq!(r.len(), 2);
        assert!(r.status.truncated);
        assert!(r.status.capped());
        assert_eq!(r.status.total_matched, 2);
        q.limit = 4;
        assert!(
            !search(&svc, &q).status.truncated,
            "exactly `limit` matches is not Go's `capped` (`api/search.go:119-122`)"
        );
        // A zero limit is Go's `limit <= 0`: the 1 000 000 ceiling, not "no rows".
        q.limit = 0;
        assert_eq!(q.result_cap(), UNBOUNDED_LIMIT);
        assert_eq!(search(&svc, &q).len(), 4);
        // Node budget.
        q = SearchQuery::new(root(), "");
        q.budget.max_nodes = 2;
        let r = search(&svc, &q);
        assert!(r.status.node_budget_hit);
        assert!(r.len() < 7);
        // Depth budget: `deep/delta.log` is out of reach, `sub/gamma.txt` is not.
        q = SearchQuery::new(root(), "");
        q.budget.max_depth = 1;
        let r = search(&svc, &q);
        assert!(r.status.depth_truncated);
        assert!(names(&r.hits).iter().any(|n| n == "gamma.txt"));
        assert!(!names(&r.hits).iter().any(|n| n == "delta.log"));
        // Time budget: `Some(0)` trips at the first boundary check, so it is exact, not approximate.
        q = SearchQuery::new(root(), "");
        q.budget.max_elapsed_ms = Some(0);
        let r = search(&svc, &q);
        assert!(r.status.timed_out);
        assert!(r.len() < 7);
    }

    #[test]
    fn cancellation_stops_the_walk_before_anything_else() {
        let svc = service(sample());
        let token = CancelToken::new();
        assert!(token.cancel());
        let q = SearchQuery::new(root(), "");
        let e = svc
            .run(&q, &token, &mut |_| {})
            .expect_err("a cancelled walk reports cancellation");
        assert_eq!(e.kind(), "cancelled");
        // A cancelled superseded request never surfaces its regex error: the user has moved on.
        let mut bad = SearchQuery::new(root(), "(");
        bad.mode = NameMode::Regex;
        assert_eq!(
            svc.run(&bad, &token, &mut |_| {})
                .err()
                .map(|e| e.kind().to_string()),
            Some("cancelled".to_string())
        );
    }

    #[test]
    fn missing_scope_and_non_directory_are_typed() {
        let svc = service(sample());
        let mut q = SearchQuery::new(
            AbsPath::from_path("/gone").unwrap_or_else(|e| panic!("{e}")),
            "x",
        );
        let e = svc
            .run(&q, &CancelToken::new(), &mut |_| {})
            .expect_err("a missing scope is an error");
        assert_eq!(e.kind(), "not_found");
        q.scope = AbsPath::from_path("/alpha.txt").unwrap_or_else(|e| panic!("{e}"));
        let e = svc
            .run(&q, &CancelToken::new(), &mut |_| {})
            .expect_err("a file is not a search scope");
        assert_eq!(e.kind(), "not_a_directory");
    }

    #[test]
    fn throttle_boundary_is_exact_at_the_window() {
        let base = Instant::now();
        let t = SearchThrottle::new(THROTTLE_WINDOW);
        assert_eq!(THROTTLE_WINDOW, Duration::from_millis(250));
        assert!(
            t.try_acquire_at(base).is_ok(),
            "the first request always starts"
        );
        // 249 ms later: refused, with the remaining time known exactly.
        let at249 = base + Duration::from_millis(249);
        assert_eq!(
            t.try_acquire_at(at249),
            Err(Duration::from_millis(1)),
            "a second walk inside the window must be refused"
        );
        assert_eq!(
            t.last_start(),
            Some(base),
            "a refused request does not move the window"
        );
        // Exactly one window later: admitted, and the window restarts from there.
        let at250 = base + Duration::from_millis(250);
        assert!(t.try_acquire_at(at250).is_ok());
        assert_eq!(t.last_start(), Some(at250));
        assert!(t.try_acquire_at(at250 + Duration::from_millis(250)).is_ok());
    }

    #[test]
    fn a_hundred_keystrokes_admit_one_walk() {
        let base = Instant::now();
        let lane = SearchThrottle::default();
        let started = (0..100u64)
            .filter(|i| {
                lane.try_acquire_at(base + Duration::from_millis(*i))
                    .is_ok()
            })
            .count();
        assert_eq!(started, 1, "100 ms of typing is one walk");
        assert!(
            lane.try_acquire_at(base + Duration::from_millis(250))
                .is_ok()
        );
        assert!(
            lane.try_acquire_at(base + Duration::from_millis(500))
                .is_ok()
        );
    }

    #[test]
    fn lane_keeps_the_newest_deferred_request_until_it_is_due() {
        let base = Instant::now();
        let mut lane = SearchLane::new(THROTTLE_WINDOW);
        // An accepted start: the throttle is armed by the accept, and the lane records the job.
        assert!(lane.throttle().try_acquire_at(base).is_ok());
        lane.mark_started(JobId(1));
        assert!(lane.is_current(JobId(1)));
        for q in ["a", "ab", "abc"] {
            let query = SearchQuery::new(root(), q);
            match lane
                .throttle()
                .try_acquire_at(base + Duration::from_millis(10))
            {
                Err(after) => {
                    lane.defer(query, Origin(7), base + Duration::from_millis(10) + after)
                }
                Ok(()) => panic!("10 ms after an accepted start must be throttled"),
            }
        }
        assert_eq!(
            lane.deferred().map(|d| d.query.query.as_str()),
            Some("abc"),
            "the newest keystroke is the one that walks"
        );
        assert!(lane.take_due(base + Duration::from_millis(249)).is_none());
        let due = lane
            .take_due(base + Duration::from_millis(250))
            .expect("due at 250 ms");
        assert_eq!(due.query.query, "abc");
        assert!(lane.deferred().is_none());
        assert!(!lane.is_current(JobId(2)));
        assert_eq!(due.origin, Origin(7));
    }

    #[test]
    fn wire_types_serialize_as_snake_case() {
        let json =
            serde_json::to_string(&[ScopeFilter::Both, ScopeFilter::Dirs, ScopeFilter::Files])
                .unwrap();
        assert_eq!(json, r#"["both","dirs","files"]"#);
        assert_eq!(
            serde_json::to_string(&HiddenPolicy::Hide).unwrap(),
            r#""hide""#
        );
        let status = SearchStatus {
            total_matched: 3,
            truncated: true,
            ..SearchStatus::default()
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["total_matched"], 3);
        assert_eq!(json["truncated"], true);
    }
}
