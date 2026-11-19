//! The index walker — plan §5.4 `index/walker.rs` row, a port of `api/search_index_walker.go`'s
//! `walkRoot` + `compileExcludes`.
//!
//! # What one row is
//!
//! [`IndexedRow`] is Go's `fileIndexEntry` (`api/search_index_walker.go:14-25`), field for field:
//! `name` is **lowercased**, `ext` is lowercased **without** the dot, `parent` is `filepath.Dir`, `mtime`
//! is **seconds**, `is_dir` is `d.IsDir()` from the *readdir* type (so a symlink to a directory is
//! `is_dir: false, is_symlink: true` — plan §2.3, and the reason `link-to-dir` is not navigable in the
//! index either). The plan §5.4 row puts this type in `store/sql.rs`; it lives here because it is the
//! walk's output type and `store/sql.rs` is only the opener (it has never owned a row type).
//!
//! # The exclude semantics, exactly
//!
//! Go's `compileExcludes` (`:462-492`) is a glob→regex rewrite, not a `filepath.Match`:
//! `**`→`.*`, `*`→`[^/]*`, `?`→`.`, and every byte of `.+()|{}^$[]` escaped; the result is then wrapped
//! as `(^|/)…($|/)`, so a pattern matches a **whole path component run**, never a substring of a name.
//! That wrapper is unconditional in practice: `^` and `$` are themselves escaped, so no pattern can
//! reach the `HasPrefix("^")`/`HasSuffix("$")` escape hatches. The compiled regex is then matched against
//! **both** the slash-normalised absolute path and the root-relative path with its leading separator
//! kept (`relPath = path[len(absRoot):]`, `:303-309`) — which is why `/proc` excludes an absolute root
//! and `**/.git` excludes a directory at any depth. A directory hit prunes the subtree; a file hit drops
//! the row (`:311-318`).
//!
//! This module implements the same language **without a regex engine** (there is none in the graph —
//! plan §9.2 step-11a), by simulating the token sequence of the translated pattern over the path with an
//! NFA whose start state is re-enabled at every `/` boundary and whose accept state only counts at a `/`
//! boundary or end of string. That is the language `(^|/)P($|/)` accepts, character for character,
//! including the two Go quirks worth keeping: `.` never matches `\n` (so `?` and `**` stop at a newline),
//! and an empty pattern matches every absolute path.
//!
//! Unlike Go, an unusable pattern is **reported**, not silently dropped (plan §5.4's "fixes Go silently
//! ignoring invalid patterns"): [`compile_excludes`] refuses an empty pattern, one containing a NUL, and
//! one longer than [`MAX_EXCLUDE_LEN`], and [`crate::session::Session::index_status`] surfaces it as
//! `error`.
//!
//! # Order, workers and the per-root timeout
//!
//! Go spawns one goroutine per root behind a `worker_count` semaphore and hands the rows to a single
//! writer goroutine over a channel (`:170-205`) — which makes the *row ids* nondeterministic, because
//! `files.id` is the insert order. Here the roots are walked **sequentially, in configured order**, and
//! each directory's children in `os.ReadDir`'s byte order, exactly like `filepath.WalkDir`: the same tree
//! therefore produces the same ids, the same tie-break order in `ORDER BY is_dir DESC, name ASC`, and a
//! golden-comparable index. [`WalkCfg::workers`] is kept for config parity (it is what Go's
//! `worker_count` means) and is documented as reserved; parallel walking is a step-12b+ decision, not a
//! silent no-op.
//!
//! [`WalkCfg::per_root_timeout`] is Go's `context.WithTimeout(ctx, 30*time.Second)` per root. Go's
//! context is only observed at `ctx.Err()` **once per visited entry** (`:293-295`), so a `readdir` blocked
//! in a dead mount hangs Go exactly as long as it would hang here. The port therefore uses a wall-clock
//! deadline checked at the same points, and records the fact in [`WalkStats::timed_out_roots`] — which
//! Go does not report at all (its timed-out root is indistinguishable from an empty one). A blocked
//! syscall is still not interruptible from user space; that is a limitation shared with the reference,
//! not a regression.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::error::CoreError;
use crate::jobs::CancelToken;
use crate::path::AbsPath;

/// Go's default per-root timeout (`api/search_index_walker.go:187`).
pub const DEFAULT_PER_ROOT_TIMEOUT: Duration = Duration::from_secs(30);
/// Go's default `worker_count` when the config says `<= 0` and the SSD heuristic is unavailable
/// (`api/search_index_walker.go:495-520` returns 2 on any error).
pub const DEFAULT_WORKERS: u16 = 2;
/// The batch the sink is handed, and the transaction size plan §5.4 asks for (Go's writer flushes at
/// 1 000, `api/search_index_walker.go:402`).
pub const BATCH_ROWS: usize = 1_000;
/// Longest exclude pattern the compiler accepts. Beyond this the NFA is still linear but the pattern has
/// stopped being a configuration value a human wrote.
pub const MAX_EXCLUDE_LEN: usize = 1_024;

/// One row of `files`, in Go's column order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexedRow {
    /// Absolute path — `files.path`, and the unique key.
    pub path: String,
    /// Lowercased base name — the column FTS indexes and `ORDER BY name` sorts on.
    pub name: String,
    /// Base name as it is on disk.
    pub name_orig: String,
    /// Lowercased extension **without** the dot; `""` when there is none.
    pub ext: String,
    /// `filepath.Dir(path)`.
    pub parent: String,
    /// `lstat` size (a symlink's own size, like Go's `d.Info()`).
    pub size: i64,
    /// `lstat` mtime, **seconds**.
    pub mtime: i64,
    /// `d.IsDir()` from the readdir type — false for a linked directory.
    pub is_dir: bool,
    /// `d.Type()&ModeSymlink != 0`.
    pub is_symlink: bool,
}

impl IndexedRow {
    /// Go's `strings.ToLower(filepath.Ext(name))` minus the leading dot (`:339-342`).
    ///
    /// `filepath.Ext` scans the *final element* backwards and returns the suffix starting at the first
    /// dot it meets ("" when the element has no dot or ends with one — `Ext("a.") == "."`, and the
    /// caller's dot-strip turns that into `""`). `".hidden"` therefore yields `"hidden"`, which is what
    /// the Go index stores; a name like `back\slash.txt` yields `"txt"` because on Unix `\` is not a
    /// separator and never terminates the scan.
    #[must_use]
    pub fn extension_of(name: &str) -> String {
        for (i, c) in name.char_indices().rev() {
            if c == '/' {
                break;
            }
            if c == '.' {
                return name[i + 1..].to_lowercase();
            }
        }
        String::new()
    }

    /// The row for one path, from its metadata (the shape `index/updater.rs` re-derives after a
    /// mutation, so the two writers cannot drift).
    #[must_use]
    pub fn for_path(path: &AbsPath, meta: &std::fs::Metadata) -> Self {
        let shown = path.to_string_lossy();
        let name_orig = path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        Self {
            parent: path
                .parent()
                .map_or_else(|| "/".to_string(), |p| p.to_string_lossy()),
            ext: Self::extension_of(&name_orig),
            size: i64::try_from(meta.len()).unwrap_or(0),
            mtime: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0)),
            is_dir: meta.is_dir(),
            is_symlink: meta.file_type().is_symlink(),
            name: name_orig.to_lowercase(),
            path: shown,
            name_orig,
        }
    }
}

// ---------------------------------------------------------------------------------------------- excludes

/// How a compiled exclude matches. Mirrors the translated regex token by token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    /// A literal byte (Go escapes every regex metacharacter it is given).
    Lit(char),
    /// `?` → `.`: any character except a newline.
    Any,
    /// `*` → `[^/]*`: any run that does not cross a separator.
    SegStar,
    /// `**` → `.*`: any run, separators included, still not across a newline.
    AnyStar,
}

/// One compiled exclude pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledExclude {
    source: String,
    tokens: Vec<Token>,
}

impl CompiledExclude {
    /// The pattern text as it was written.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Go's `re.MatchString` on one slash-normalised path.
    #[must_use]
    pub fn matches(&self, path: &str) -> bool {
        self.search(path, None)
    }

    /// The NFA run. `steps`, when given, counts state examinations: a Thompson simulation touches each
    /// `(state, position)` pair at most once, so the bound is `tokens × characters` — the linear-time
    /// property RE2 (and therefore Go) has and a backtracker does not. `walker_tests` reads it.
    fn search(&self, path: &str, mut steps: Option<&mut u64>) -> bool {
        let chars: Vec<char> = path.chars().collect();
        let n = self.tokens.len();
        let mut active = vec![false; n + 1];
        for i in 0..=chars.len() {
            // `(^|/)`: a match may start at position 0 or just after a separator.
            if i == 0 || chars[i - 1] == '/' {
                active[0] = true;
                closure(&mut active, &self.tokens);
            }
            // `($|/)`: and may only end at the end of the string or just before a separator.
            if active[n] && (i == chars.len() || chars[i] == '/') {
                return true;
            }
            if i == chars.len() {
                break;
            }
            let c = chars[i];
            let mut next = vec![false; n + 1];
            for (si, on) in active.iter().enumerate().take(n) {
                if !on {
                    continue;
                }
                if let Some(counter) = steps.as_mut() {
                    **counter += 1;
                }
                let star = matches!(self.tokens[si], Token::SegStar | Token::AnyStar);
                let advances = match self.tokens[si] {
                    Token::Lit(e) => e == c,
                    Token::Any => c != '\n',
                    // A star that consumed the character stays where it is (the loop edge).
                    Token::SegStar => c != '/' && c != '\n',
                    Token::AnyStar => c != '\n',
                };
                if advances {
                    // A star keeps its own state and the closure below re-enables the "matched nothing"
                    // skip; every other token is *consumed*, so the machine moves to the next state.
                    next[if star { si } else { si + 1 }] = true;
                }
            }
            active = next;
            closure(&mut active, &self.tokens);
        }
        false
    }
}

/// Follow every star's epsilon edge (`X*` can also mean "matched nothing").
fn closure(active: &mut [bool], tokens: &[Token]) {
    for i in 0..tokens.len() {
        if active[i] && matches!(tokens[i], Token::SegStar | Token::AnyStar) {
            active[i + 1] = true;
        }
    }
}

/// Go's translation, character by character (`api/search_index_walker.go:466-484`).
fn translate(pattern: &str) -> Vec<Token> {
    let normalised = pattern.replace('\\', "/");
    let bytes: Vec<char> = normalised.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == '*' && bytes[i + 1] == '*' {
            tokens.push(Token::AnyStar);
            i += 2;
        } else if bytes[i] == '*' {
            tokens.push(Token::SegStar);
            i += 1;
        } else if bytes[i] == '?' {
            tokens.push(Token::Any);
            i += 1;
        } else {
            tokens.push(Token::Lit(bytes[i]));
            i += 1;
        }
    }
    tokens
}

/// A set of compiled exclude patterns, with Go's match-both-paths rule.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GlobMatcher {
    patterns: Vec<CompiledExclude>,
}

impl GlobMatcher {
    /// A matcher with no patterns — nothing is excluded.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            patterns: Vec::new(),
        }
    }

    /// The compiled patterns, in the order they were given (Go tests them in order and the first hit
    /// decides).
    #[must_use]
    pub fn patterns(&self) -> &[CompiledExclude] {
        &self.patterns
    }

    /// `true` when nothing is excluded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Go's exclude test: the absolute path *or* the root-relative path (`:311-318`).
    #[must_use]
    pub fn matches(&self, abs: &str, rel: &str) -> bool {
        self.patterns
            .iter()
            .any(|p| p.matches(abs) || (!rel.is_empty() && p.matches(rel)))
    }
}

/// Compile Go's exclude patterns.
///
/// # Errors
/// [`CoreError::BadRegex`] for an empty pattern, a NUL, or a pattern over [`MAX_EXCLUDE_LEN`]. Go
/// compiled in a loop that discarded a failure (`:489-491`); reporting is the plan §5.4 fix.
pub fn compile_excludes(patterns: &[String]) -> Result<GlobMatcher, CoreError> {
    let mut out = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        if pattern.is_empty() {
            return Err(CoreError::BadRegex {
                pattern: pattern.clone(),
                reason: "empty exclude pattern".to_string(),
            });
        }
        if pattern.chars().count() > MAX_EXCLUDE_LEN || pattern.contains('\0') {
            return Err(CoreError::BadRegex {
                pattern: pattern.clone(),
                reason: format!(
                    "exclude pattern is longer than {MAX_EXCLUDE_LEN} characters or contains a NUL"
                ),
            });
        }
        out.push(CompiledExclude {
            source: pattern.clone(),
            tokens: translate(pattern),
        });
    }
    Ok(GlobMatcher { patterns: out })
}

// ------------------------------------------------------------------------------------------------- walk

/// How to walk, in the shape plan §5.4 names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalkCfg {
    /// Roots, in order. Go's `IndexConfig.Roots`.
    pub roots: Vec<AbsPath>,
    /// The compiled excludes.
    pub excludes: GlobMatcher,
    /// Descent cap; `0` is unlimited (Go's `MaxDepth: 0`, applied only when `> 0`).
    pub max_depth: u32,
    /// Descend into symlinked directories. Go records the flag and **never acts on it** —
    /// `filepath.WalkDir` does not follow links and `walkRoot` has no follow branch
    /// (`api/search_index_walker.go:330-335`); here `true` really follows, with a visited-set guard so a
    /// link cycle cannot run forever.
    pub follow_symlinks: bool,
    /// Go's `worker_count`. Reserved — see the module docs on ordering.
    pub workers: u16,
    /// Wall-clock budget per root ([`DEFAULT_PER_ROOT_TIMEOUT`]).
    pub per_root_timeout: Duration,
}

impl Default for WalkCfg {
    /// Go's defaults: no roots, its six stock excludes, unlimited depth, no follow, 2 workers, 30 s.
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            excludes: GlobMatcher::empty(),
            max_depth: 0,
            follow_symlinks: false,
            workers: DEFAULT_WORKERS,
            per_root_timeout: DEFAULT_PER_ROOT_TIMEOUT,
        }
    }
}

impl WalkCfg {
    /// The config the Go app would have run with, from the port's own `IndexerSettings`.
    ///
    /// # Errors
    /// As [`compile_excludes`].
    pub fn from_settings(
        roots: &[String],
        excludes: &[String],
        max_depth: u32,
        follow_symlinks: bool,
        worker_count: u32,
    ) -> Result<Self, CoreError> {
        let parsed = roots
            .iter()
            .map(|r| AbsPath::from_path(r.clone()))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            roots: parsed,
            excludes: compile_excludes(excludes)?,
            max_depth,
            follow_symlinks,
            workers: u16::try_from(worker_count.max(1)).unwrap_or(DEFAULT_WORKERS),
            per_root_timeout: DEFAULT_PER_ROOT_TIMEOUT,
        })
    }
}

/// What a walk did. The counters are atomic because the plan names them that way and because a caller
/// may want to read them from a progress reporter while the walk runs.
#[derive(Debug, Default)]
pub struct WalkStats {
    /// Rows handed to the sink (Go's `ProgressCount` counts the same thing, per flush).
    pub indexed: AtomicU64,
    /// Entries dropped — a failed `lstat` (Go's `d.Info()` error, `:336-338`), an exclude hit, a
    /// depth cut.
    pub skipped: AtomicU64,
    /// Directory reads that failed and were skipped (Go's `WalkDir` error branch, `:286-291`).
    pub dirs_skipped: AtomicU64,
    /// Names examined, roots included.
    pub visited: AtomicU64,
    /// Roots that produced at least one row.
    pub roots_ok: usize,
    /// Per-root failure text, kept per root (plan F6: Go returned one generic `"rebuild failed"`).
    pub roots_failed: Vec<(AbsPath, String)>,
    /// Roots that ran out of [`WalkCfg::per_root_timeout`].
    pub timed_out_roots: Vec<AbsPath>,
    /// Wall-clock length of the walk.
    pub elapsed: Duration,
}

impl WalkStats {
    /// Rows produced.
    #[must_use]
    pub fn rows(&self) -> u64 {
        self.indexed.load(Ordering::Acquire)
    }

    /// Cloneable snapshot of the counters (what `JobProgress` and `IndexStatus` read).
    #[must_use]
    pub fn snapshot(&self) -> WalkCounters {
        WalkCounters {
            indexed: self.rows(),
            skipped: self.skipped.load(Ordering::Acquire),
            dirs_skipped: self.dirs_skipped.load(Ordering::Acquire),
            visited: self.visited.load(Ordering::Acquire),
        }
    }
}

/// The readable form of [`WalkStats`]'s counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WalkCounters {
    /// Rows produced.
    pub indexed: u64,
    /// Entries dropped.
    pub skipped: u64,
    /// Directory reads that failed.
    pub dirs_skipped: u64,
    /// Names examined.
    pub visited: u64,
}

/// One directory frame of the explicit stack (the same shape `services/search_live.rs` uses, so the two
/// walks agree on order).
struct Frame {
    path: PathBuf,
    /// Root-relative path with the leading separator Go keeps, `""` for the root itself.
    rel: String,
    depth: u32,
    names: Vec<std::ffi::OsString>,
    idx: usize,
}

/// Walk every root and hand rows to `sink` in batches of [`BATCH_ROWS`].
///
/// The cancellation token is checked per entry (Go checks its context per entry, `:293`) and the root
/// deadline at the same points.
///
/// # Errors
/// [`CoreError::Cancelled`] when the token fires. A root that cannot be read is recorded in
/// [`WalkStats::roots_failed`] and does **not** fail the walk — Go indexed the remaining roots
/// (`:180-205`), and the golden's 1 260 rows include a tree whose `noread-dir` cannot be read.
pub fn walk(
    cfg: &WalkCfg,
    cancel: &CancelToken,
    sink: &mut dyn FnMut(&[IndexedRow]),
) -> Result<WalkStats, CoreError> {
    let started = Instant::now();
    let mut stats = WalkStats::default();
    let mut batch: Vec<IndexedRow> = Vec::with_capacity(BATCH_ROWS);
    for root in &cfg.roots {
        cancel.check()?;
        let deadline = started + cfg.per_root_timeout;
        let mut walker = RootWalker {
            cfg,
            root: root.clone(),
            abs_shown: slash(root.as_path().display().to_string()),
            cancel,
            deadline,
            timed_out: false,
            failed: None,
            visited: 0,
            produced: 0,
            skipped: 0,
            dirs_skipped: 0,
            followed: HashSet::new(),
        };
        walker.run(&mut batch, sink)?;
        let RootWalker {
            timed_out,
            failed,
            produced,
            visited,
            skipped,
            dirs_skipped,
            ..
        } = walker;
        if let Some(why) = failed {
            stats.roots_failed.push((root.clone(), why));
        } else if produced > 0 {
            stats.roots_ok += 1;
        } else {
            stats.roots_failed.push((
                root.clone(),
                "produced no rows (unreadable, empty or excluded)".to_string(),
            ));
        }
        if timed_out {
            stats.timed_out_roots.push(root.clone());
        }
        let _ = stats.visited.fetch_add(visited, Ordering::AcqRel);
        let _ = stats.indexed.fetch_add(produced, Ordering::AcqRel);
        let _ = stats.skipped.fetch_add(skipped, Ordering::AcqRel);
        let _ = stats.dirs_skipped.fetch_add(dirs_skipped, Ordering::AcqRel);
    }
    if !batch.is_empty() {
        sink(&batch);
    }
    stats.elapsed = started.elapsed();
    Ok(stats)
}

struct RootWalker<'a> {
    cfg: &'a WalkCfg,
    #[allow(dead_code)]
    root: AbsPath,
    abs_shown: String,
    cancel: &'a CancelToken,
    deadline: Instant,
    timed_out: bool,
    failed: Option<String>,
    visited: u64,
    produced: u64,
    /// Entries dropped: a failed `lstat`, an exclude hit, a depth cut (Go's three `return nil`s).
    skipped: u64,
    /// Directory reads that failed (Go's `WalkDir` error branch).
    dirs_skipped: u64,
    followed: HashSet<PathBuf>,
}

impl RootWalker<'_> {
    fn run(
        &mut self,
        batch: &mut Vec<IndexedRow>,
        sink: &mut dyn FnMut(&[IndexedRow]),
    ) -> Result<(), CoreError> {
        let meta = match std::fs::symlink_metadata(self.root.as_path()) {
            Ok(m) => m,
            Err(e) => {
                self.failed = Some(e.to_string());
                return Ok(());
            }
        };
        // Go indexes the root row itself — the `type:dir` golden's last entry is `…/tree`.
        if self.cfg.follow_symlinks
            && let Ok(real) = std::fs::canonicalize(self.root.as_path())
        {
            // A link that points back at the root (or at any ancestor of it) is a cycle.
            let _ = self.followed.insert(real);
        }
        if self.cfg.excludes.matches(self.abs_shown.as_str(), "") {
            self.failed = Some("the root matches an exclude pattern".to_string());
            return Ok(());
        }
        self.emit(
            &self.root.clone().into_path_buf(),
            &meta,
            String::new(),
            batch,
            sink,
        )?;
        if !meta.is_dir() {
            return Ok(());
        }
        let mut stack = match self.frame(&self.root.clone().into_path_buf(), String::new(), 0) {
            Ok(f) => vec![f],
            Err(why) => {
                self.failed = Some(why);
                return Ok(());
            }
        };
        while let Some(frame) = stack.last_mut() {
            if frame.idx >= frame.names.len() {
                stack.pop();
                continue;
            }
            let name = frame.names[frame.idx].clone();
            frame.idx += 1;
            let (parent, parent_rel, depth) = (frame.path.clone(), frame.rel.clone(), frame.depth);
            // Go checks the context once per visited name (`:293-295`); so does the deadline.
            self.cancel.check()?;
            if Instant::now() >= self.deadline {
                self.timed_out = true;
                return Ok(());
            }
            let child = parent.join(&name);
            let base = name.to_string_lossy().into_owned();
            let rel = format!("{parent_rel}/{base}");
            let Ok(meta) = std::fs::symlink_metadata(&child) else {
                // Go's `d.Info()` failure: the row is dropped, the walk continues (`:336-338`). This is
                // exactly why the fixture tree's `noexec-dir/inner.txt` is absent from the golden index.
                self.skipped += 1;
                continue;
            };
            self.visited += 1;
            let abs = slash(child.display().to_string());
            if self.cfg.excludes.matches(abs.as_str(), rel.as_str()) {
                self.skipped += 1;
                continue;
            }
            if self.cfg.max_depth > 0 && depth + 1 > self.cfg.max_depth {
                self.skipped += 1;
                continue;
            }
            self.emit(&child, &meta, base, batch, sink)?;
            if self.may_descend(&child, &meta) {
                match self.frame(&child, rel, depth + 1) {
                    Ok(f) => stack.push(f),
                    Err(_) => {
                        // Go's `WalkDir` skips an unreadable directory (`:286-291`).
                        self.dirs_skipped += 1;
                        continue;
                    }
                }
            }
        }
        Ok(())
    }

    /// With `follow_symlinks` the walk may enter a linked directory; a visited set of canonical paths is
    /// what keeps a `a → b → a` pair of links from looping forever.
    ///
    /// The `is_dir` decision for a link cannot come from `lstat` (which reports "symlink"), so the follow
    /// test has to run *before* any `is_dir` gate — otherwise the knob would be inert, which is exactly
    /// the Go bug this port says it fixes.
    fn may_descend(&mut self, path: &Path, meta: &std::fs::Metadata) -> bool {
        if !meta.file_type().is_symlink() {
            return meta.is_dir();
        }
        if !self.cfg.follow_symlinks {
            return false;
        }
        // Only descend into a link that actually carries a directory.
        if !std::fs::metadata(path).is_ok_and(|t| t.is_dir()) {
            return false;
        }
        match std::fs::canonicalize(path) {
            Ok(real) => self.followed.insert(real),
            Err(_) => false,
        }
    }

    fn frame(&self, path: &Path, rel: String, depth: u32) -> Result<Frame, String> {
        let mut names: Vec<std::ffi::OsString> = Vec::new();
        let read = std::fs::read_dir(path);
        match read {
            Ok(entries) => {
                for e in entries.flatten() {
                    names.push(e.file_name());
                }
            }
            Err(e) => return Err(e.to_string()),
        }
        // `os.ReadDir` sorts by name; `WalkDir` walks in that order. Byte order on the raw name.
        names.sort_by(|a, b| a.as_encoded_bytes().cmp(b.as_encoded_bytes()));
        Ok(Frame {
            path: path.to_path_buf(),
            rel,
            depth,
            names,
            idx: 0,
        })
    }

    fn emit(
        &mut self,
        path: &Path,
        meta: &std::fs::Metadata,
        base: String,
        batch: &mut Vec<IndexedRow>,
        sink: &mut dyn FnMut(&[IndexedRow]),
    ) -> Result<(), CoreError> {
        let base = if base.is_empty() {
            path.file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
        } else {
            base
        };
        let parent = path
            .parent()
            .map_or_else(|| "/".to_string(), |p| p.display().to_string());
        // The *stored* path is the raw one — Go writes `path` untouched and only slash-normalises the
        // copy it matches excludes against, so `back\slash.txt` keeps its backslash in the row (see the
        // `search-index-paged` golden's third entry).
        let row = IndexedRow {
            path: path.display().to_string(),
            name: base.to_lowercase(),
            ext: IndexedRow::extension_of(&base),
            name_orig: base,
            parent,
            size: i64::try_from(meta.len()).unwrap_or(0),
            mtime: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0)),
            is_dir: meta.is_dir(),
            is_symlink: meta.file_type().is_symlink(),
        };
        self.produced += 1;
        batch.push(row);
        if batch.len() >= BATCH_ROWS {
            sink(batch);
            batch.clear();
        }
        Ok(())
    }
}

/// Go's `strings.ReplaceAll(path, "\\", "/")` (`api/search_index_walker.go:307-309`). NOT a no-op on
/// Unix: the fixture tree contains `back\slash.txt`, and excluding depends on the match seeing the path
/// exactly the way Go's `stdPath`/`stdRelPath` do (the pattern side is normalised too, in `translate`).
/// Only the *matching* copies go through this — the stored row keeps the raw path.
fn slash(s: String) -> String {
    s.replace('\\', "/")
}

impl AbsPath {
    /// The owned `PathBuf` form (the walk is the only consumer that needs it; every other path crossing
    /// in this crate stays borrowed).
    #[must_use]
    pub fn into_path_buf(self) -> PathBuf {
        self.as_path().to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    fn matcher(list: &[&str]) -> GlobMatcher {
        compile_excludes(&strings(list)).expect("the pattern compiles")
    }

    fn compiled(pattern: &str) -> CompiledExclude {
        matcher(&[pattern]).patterns()[0].clone()
    }

    fn abs(p: &Path) -> AbsPath {
        AbsPath::from_path(p.display().to_string()).expect("the scratch root is absolute")
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zenman-walker-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn touch(root: &Path, rel: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent");
        }
        std::fs::write(&path, b"x").expect("file");
    }

    /// The fixed tree the walk tests share: nested directories, a 21-level chain, a backslash name, a
    /// quoted directory name, a dotfile, and an uppercase name (the `name` column is lowercased).
    fn tree(tag: &str) -> PathBuf {
        let root = scratch(tag);
        for rel in [
            "a.txt",
            "B.md",
            ".hidden",
            "back\\slash.txt",
            "sub/c.txt",
            "sub/deep/d.txt",
            "build/out.o",
            "dir with 'quote'/x.txt",
        ] {
            touch(&root, rel);
        }
        // 21 separators below the root — the depth the `search-index-word` golden needs, and the reason
        // `limits.rs`' INDEX_MAX_DEPTH = 20 is a config bound and not a walk default (plan §9.2).
        let deep = (0..21)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("/");
        touch(&root, &format!("{deep}/leaf.txt"));
        root
    }

    fn walk_cfg(root: &Path, excludes: &[&str]) -> WalkCfg {
        WalkCfg {
            roots: vec![abs(root)],
            excludes: matcher(excludes),
            ..WalkCfg::default()
        }
    }

    fn collect(cfg: &WalkCfg) -> (Vec<IndexedRow>, WalkStats) {
        let mut rows = Vec::new();
        let cancel = CancelToken::new();
        let stats = walk(cfg, &cancel, &mut |batch| rows.extend_from_slice(batch)).expect("walk");
        (rows, stats)
    }

    fn paths(rows: &[IndexedRow]) -> Vec<String> {
        rows.iter().map(|r| r.path.clone()).collect()
    }

    fn has(rows: &[IndexedRow], rel: &str) -> bool {
        rows.iter().any(|r| r.path == rel || r.path.ends_with(rel))
    }

    /// Go's translation rules (`api/search_index_walker.go:466-484`) plus the unconditional
    /// `(^|/)…($|/)` wrap (`:444-449`) and `re.MatchString` over the slash-normalised path (`:307-318`).
    const GLOB_TABLE: &[(&str, &str, bool)] = &[
        // `**` → `.*`: crosses separators, never a newline.
        ("**/a", "/root/x/a", true),
        ("**/a", "/root/x/ab", false),
        ("**/a", "/root/a", true),
        ("**/a", "/rootx/a", true),
        ("**/.git", "/home/u/proj/.git", true),
        ("**/.git", "/home/u/proj/.github", false),
        ("a**z", "/root/abz", true),
        ("**", "/root/any/thing", true),
        ("a**c", "/root/a\nc", false),
        // `*` → `[^/]*`: never crosses a separator.
        ("*.log", "/root/a.log", true),
        ("*.log", "/root/a.txt", false),
        ("*.log", "/root/deep/a.log", true),
        // `(^|/)` anchors at *every* separator, so `*.log` reaches any depth — it is a component-run
        // rule, not a "first directory" rule.
        ("/root/*.log", "/root/deep/x.log", false),
        ("/root/*.log", "/root/x.log", true),
        ("x*y", "/root/xabny", true),
        ("*", "/root/a/b", true),
        // `?` → `.`: exactly one character, never `\n`, never a separator.
        ("a?c", "/root/abc", true),
        ("a?c", "/root/ac", false),
        ("a?c", "/root/a/c", true), // Go's `.` is a regex any-char: it eats a separator, only `\n` is out
        ("a?c", "/root/a\nc", false),
        // the metacharacters Go escapes (`.+()|{}^$[]`) stay literals in the pattern.
        ("a.b", "/root/axb", false),
        ("a.b", "/root/a.b", true),
        ("+x", "/root/xx", false),
        ("(1)", "/root/(1)", true),
        ("a|b", "/root/a|b", true),
        ("a{2}", "/root/a{2}", true),
        ("[ab]", "/root/[ab]", true),
        ("[ab]", "/root/a", false),
        ("a$b", "/root/a$b", true),
        ("a}b", "/root/a}b", true),
        // `^`/`$` are escaped too, so the `HasPrefix("^")`/`HasSuffix("$")` escape hatches are
        // unreachable and the wrap really is unconditional.
        ("^root", "/root/x", false),
        ("x$", "/root/x/y", false),
        ("^x$", "/root/x", false),
        ("^x$", "/root/^x$", true),
        // whole path-component runs, never a substring of a name.
        ("build", "/root/build", true),
        ("build", "/root/build/out.o", true),
        ("build", "/root/mybuild", false),
        ("build", "/root/buildx/y", false),
        ("/proc", "/proc/self", true),
        ("/proc", "/dev-tools/x", false),
        ("dev", "/root/dev-tools/x", false),
        ("dev", "/root/tools/dev", true),
    ];

    #[test]
    fn the_exclude_engine_is_gos_translation_table() {
        for (pattern, path, want) in GLOB_TABLE {
            let m = matcher(&[*pattern]);
            assert_eq!(
                m.matches(path, ""),
                *want,
                "pattern {pattern:?} against {path:?}: Go compiles it and matches, this engine disagrees"
            );
        }
    }

    #[test]
    fn a_pattern_is_tested_against_both_the_absolute_and_the_relative_form() {
        // Go: `re.MatchString(stdPath) || re.MatchString(stdRelPath)` (`:311-316`), `relPath` keeping its
        // leading separator (`:303-309`). Because the absolute form is slashed too, the relative half is
        // a suffix of it and every `(^|/)` anchor it offers exists in the absolute form as well — so the
        // rule is "either half", which is what `GlobMatcher::matches` implements.
        let m = matcher(&["/proc", "**/vendor"]);
        assert!(m.matches("/proc/x", ""), "the root itself is `/proc`");
        assert!(m.matches("/mnt/proc/x", "/proc/x"), "relative-only hit");
        assert!(m.matches("/data/tree/vendor", "/tree/vendor"), "any depth");
        assert!(!m.matches("/data/tree/a.txt", "/a.txt"));
        // An empty relative form (the root) is not a second chance to match.
        assert!(!m.matches("/data/tree", ""));
    }

    #[test]
    fn a_broken_pattern_is_a_typed_error_and_a_strange_one_still_compiles() {
        // Go compiled in a loop and discarded the failure (`:489-491`); the port reports it instead
        // (plan §5.4 "fixes Go silently ignoring invalid patterns"), and neither path may panic.
        let overlong = "a".repeat(MAX_EXCLUDE_LEN + 1);
        for bad in ["", "\0", "a\0b", overlong.as_str()] {
            let err = compile_excludes(&strings(&[bad])).expect_err("these are refused");
            match err {
                CoreError::BadRegex { pattern, reason } => {
                    assert_eq!(&pattern, bad, "the report names the offending pattern");
                    assert!(!reason.is_empty());
                }
                other => panic!("expected a bad_regex refusal, got {other:?}"),
            }
        }
        // A truncated or metacharacter-heavy glob is *valid* to Go's rewriter, so it must be valid here.
        for ok in [
            "**", "*", "?", "a**", "[", "(", "\\", "..", "...", "**/**", "a?b**c",
        ] {
            let m = matcher(&[ok]);
            assert_eq!(m.patterns().len(), 1, "{ok:?} compiled");
        }
        // The length bound is characters, not bytes: a multi-byte pattern at the cap still compiles.
        let cap = "é".repeat(MAX_EXCLUDE_LEN);
        assert!(compile_excludes(&strings(&[cap.as_str()])).is_ok());
        assert!(
            matches!(
                compile_excludes(&strings(&["x", ""])),
                Err(CoreError::BadRegex { .. })
            ),
            "the second pattern is the bad one and the whole set is refused"
        );
    }

    #[test]
    fn a_pathological_pattern_costs_states_not_backtracks() {
        // The RE2 property Go relies on is linear time. `a*a*a*a*a*b` over a run of `a`s is the classic
        // backtracker killer; a Thompson simulation examines each `(state, position)` pair once, so the
        // step count is bounded by `states × positions` — asserted here instead of by a wall clock, which
        // a loaded machine would fail for reasons that have nothing to do with the engine.
        let pat = compiled("a*a*a*a*a*b");
        let states = u64::try_from(pat.tokens.len() + 1).unwrap_or(0);
        let mut previous = 0u64;
        for len in [50usize, 200, 800] {
            let path = format!("/root/{}", "a".repeat(len));
            let mut steps = 0u64;
            let hit = pat.search(path.as_str(), Some(&mut steps));
            assert!(!hit, "there is no `b` in {path:?}");
            let budget = states * u64::try_from(path.chars().count() + 2).unwrap_or(0);
            assert!(
                steps <= budget,
                "{len} characters cost {steps} state examinations, over the {budget} the NFA may use"
            );
            if previous > 0 {
                // Quadratic in the input at worst (states × positions), never exponential: 4× the input
                // may cost at most 5× the steps.
                assert!(steps <= previous * 5, "{len}: {steps} after {previous}");
            }
            previous = steps;
        }
        // And the same engine answers the hit correctly on the same shape.
        assert!(pat.matches("/root/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab"));
    }

    #[test]
    fn backslashes_are_slashed_for_matching_and_kept_in_the_stored_row() {
        // Go normalises a *copy* for the exclude match (`stdPath`, `:307`) and writes the raw path
        // (`:358`'s INSERT of `entry.Path`), which is why `back\slash.txt` survives in the golden index.
        assert_eq!(slash(String::from("a\\b/c")), "a/b/c");
        let m = matcher(&["**/slash.txt", "**/back\\slash.txt"]);
        assert!(
            m.matches(slash(String::from("/root/back\\slash.txt")).as_str(), ""),
            "the pattern's own `\\` is normalised too (`translate`), so both spellings hit"
        );
        let raw = matcher(&["**/nomatch"]);
        assert!(!raw.matches("/root/back\\slash.txt", ""));

        let root = tree("backslash");
        let (rows, _) = collect(&walk_cfg(&root, &[]));
        let needle = root.join("back\\slash.txt");
        let row = rows
            .iter()
            .find(|r| r.path == needle.display().to_string())
            .expect("the backslash row is indexed");
        assert_eq!(
            row.name_orig, "back\\slash.txt",
            "the row keeps the raw name"
        );
        assert_eq!(
            row.ext, "txt",
            "`\\` is not a separator on Unix, so the scan reaches `.txt`"
        );

        // And the exclude really prunes it when the pattern says so, through the slashed copy.
        let (kept, stats) = collect(&walk_cfg(&root, &["**/slash.txt"]));
        assert!(
            !has(&kept, "back\\slash.txt"),
            "the exclude sees `back/slash.txt`"
        );
        assert!(stats.snapshot().skipped >= 1, "the drop is counted");
    }

    /// `strings.ToLower(filepath.Ext(name))` minus the leading dot (`:339-342`), i.e. Go's scan of the
    /// final element backwards to the first dot.
    const EXT_TABLE: &[(&str, &str)] = &[
        ("a.txt", "txt"),
        ("A.TXT", "txt"),
        ("a.tar.gz", "gz"),
        (".hidden", "hidden"),
        ("..hidden", "hidden"),
        ("a.", ""),
        ("a", ""),
        ("", ""),
        (".", ""),
        ("..", ""),
        ("...", ""),
        ("a.b/c", ""),
        ("back\\slash.txt", "txt"),
        ("Makefile", ""),
        ("archive.TAR.bz2", "bz2"),
    ];

    #[test]
    fn extension_of_is_filepaths_ext_minus_the_dot() {
        for (name, want) in EXT_TABLE {
            assert_eq!(IndexedRow::extension_of(name), *want, "{name:?}");
        }
    }

    #[test]
    fn the_same_tree_walks_to_the_same_rows_in_the_same_order() {
        // Row ids are insert order, and `ORDER BY is_dir DESC, name ASC, id ASC` (the golden's tie-break)
        // is only reproducible while the walk is (`contract_search`'s `search-index-paged` ranks 4-6).
        let root = tree("determinism");
        let (first, _) = collect(&walk_cfg(&root, &[]));
        let (second, _) = collect(&walk_cfg(&root, &[]));
        let a = paths(&first);
        let b = paths(&second);
        assert_eq!(a.len(), 35, "root + the tree's files and directories");
        assert_eq!(a, b, "two walks of one tree produced different orders");
        // Pre-order, siblings byte-ordered, a directory immediately followed by its own child — Go's
        // `filepath.WalkDir` over `os.ReadDir` (`api/search.go:108-160`, same shape as the live walk).
        let sub = root.join("sub").display().to_string();
        let i = a
            .iter()
            .position(|p| *p == sub)
            .expect("the sub directory is a row");
        assert_eq!(a[i + 1], root.join("sub/c.txt").display().to_string());
        assert!(
            a[0] == root.display().to_string(),
            "the root is the first row"
        );
    }

    #[test]
    fn the_root_row_carries_gos_columns() {
        let root = tree("columns");
        let (rows, stats) = collect(&walk_cfg(&root, &[]));
        let me = rows.first().expect("the root is indexed");
        assert_eq!(me.path, root.display().to_string());
        assert!(me.is_dir, "d.IsDir() from the readdir type");
        assert!(!me.is_symlink);
        assert_eq!(
            me.parent,
            root.parent()
                .map_or_else(String::new, |p| p.display().to_string())
        );
        let b = rows
            .iter()
            .find(|r| r.name_orig == "B.md")
            .expect("the uppercase file");
        assert_eq!(
            b.name, "b.md",
            "`NameLower` is the lowercase column FTS and ORDER BY use"
        );
        assert_eq!(b.ext, "md");
        assert_eq!(b.parent, root.display().to_string());
        assert_eq!(
            stats.rows(),
            u64::try_from(rows.len()).unwrap_or(0),
            "rows == indexed"
        );
    }

    #[test]
    fn depth_zero_is_unlimited_and_a_cap_cuts_the_subtree() {
        let root = tree("depth");
        let (rows, _) = collect(&walk_cfg(&root, &[]));
        assert!(
            has(&rows, "leaf.txt"),
            "max_depth 0 is Go's unlimited default (`MaxDepth: 0` is applied only when > 0)"
        );
        let (capped, stats) = collect(&WalkCfg {
            max_depth: 3,
            ..walk_cfg(&root, &[])
        });
        assert!(
            !has(&capped, "leaf.txt"),
            "level 22 cannot survive a cap of 3"
        );
        assert!(has(&capped, "a.txt"), "level 1 survives");
        assert!(has(&capped, "sub/deep/d.txt"), "level 3 survives");
        assert!(
            stats.snapshot().skipped > 0,
            "a depth cut is a dropped entry, and `skipped` says so"
        );
    }

    #[test]
    fn an_excluded_directory_prunes_and_an_excluded_file_only_drops() {
        // Go returns `filepath.SkipDir` for a directory hit and plain `nil` for a file hit (`:311-318`).
        let root = tree("exclude");
        let (rows, _) = collect(&walk_cfg(&root, &["**/sub"]));
        assert!(!has(&rows, "c.txt"), "the whole subtree is gone");
        assert!(!has(&rows, "d.txt"));
        assert!(
            rows.iter().all(|r| !r.path.ends_with("sub")),
            "the excluded directory has no row either"
        );
        assert!(has(&rows, "a.txt"), "unrelated rows are untouched");

        let (rows, _) = collect(&walk_cfg(&root, &["*.txt"]));
        assert!(!has(&rows, "a.txt"), "the file row is dropped");
        assert!(
            has(&rows, "sub"),
            "a file hit drops one row; it is a directory hit that prunes a subtree"
        );
        assert!(
            !has(&rows, "c.txt"),
            "`(^|/)` anchors at every separator, so `*.txt` reaches any depth (Go's wrapper, `:444-449`)"
        );
        assert!(
            has(&rows, "deep"),
            "…and the directories under `sub` are still walked"
        );
        let (rows, _) = collect(&walk_cfg(&root, &["*.md"]));
        assert!(
            !has(&rows, "B.md"),
            "the pattern is matched against the path, not the lowercase name"
        );
        assert!(has(&rows, "a.txt"));
    }

    #[test]
    fn a_root_that_matches_an_exclude_produces_nothing_and_says_why() {
        let root = tree("excluded-root");
        let sub = root.join("sub");
        let (rows, stats) = collect(&walk_cfg(&sub, &["**/sub"]));
        assert!(
            rows.is_empty(),
            "Go's walkRoot returns SkipDir on the root itself"
        );
        assert_eq!(stats.roots_failed.len(), 1);
        assert!(stats.roots_failed[0].1.contains("exclude"));
    }

    #[test]
    fn roots_are_walked_in_order_and_a_broken_root_fails_alone() {
        // Go spawns one goroutine per root and indexes the survivors (`:180-205`); the port walks them
        // sequentially, in configured order, which is also what makes the ids reproducible.
        let a = tree("root-a");
        let b = tree("root-b");
        let missing = std::env::temp_dir().join("zenman-walker-does-not-exist");
        let cfg = WalkCfg {
            roots: vec![abs(&b), abs(&missing), abs(&a)],
            ..WalkCfg::default()
        };
        let (rows, stats) = collect(&cfg);
        let p = paths(&rows);
        let first_a = p
            .iter()
            .position(|x| x.starts_with(&a.display().to_string()))
            .expect("root a is indexed");
        assert!(
            p.iter()
                .take(first_a)
                .all(|x| x.starts_with(&b.display().to_string())),
            "root b's rows come first, in the configured order"
        );
        assert_eq!(
            stats.roots_failed.len(),
            1,
            "the missing root is reported, not fatal"
        );
        assert_eq!(stats.roots_failed[0].0, abs(&missing));
        assert_eq!(stats.roots_ok, 2);
    }

    #[test]
    #[cfg(unix)]
    fn a_directory_that_cannot_be_read_is_skipped_and_counted() {
        use std::os::unix::fs::PermissionsExt;
        // The 1 260-row golden mechanism (`contract_search::excludes_glob_parity`): the walk reports
        // `Ok`, the directory keeps its own row, its children have none.
        let root = scratch("unreadable");
        touch(&root, "top.txt");
        let dark = root.join("noexec-dir");
        let _ = std::fs::set_permissions(&dark, std::fs::Permissions::from_mode(0o755));
        std::fs::create_dir_all(&dark).expect("dir");
        std::fs::write(dark.join("inner.txt"), b"x").expect("inner");
        std::fs::set_permissions(&dark, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let (rows, stats) = collect(&walk_cfg(&root, &[]));
        assert!(has(&rows, "noexec-dir"), "the directory itself is indexed");
        assert!(!has(&rows, "inner.txt"), "and its contents are not");
        let snap = stats.snapshot();
        assert_eq!(
            snap.dirs_skipped, 1,
            "Go's WalkDir error branch is reported"
        );
        assert!(!stats.roots_failed.is_empty() || snap.indexed > 0);
    }

    #[test]
    #[cfg(unix)]
    fn symlinks_are_indexed_and_not_followed_unless_told() {
        // Go *records* `followSymlinks` and never acts on it — `filepath.WalkDir` does not follow and
        // `walkRoot` has no follow branch (`:330-335`). The port's `false` is that behaviour; `true` is
        // the documented deviation (with a canonical-path visited set), asserted on both sides so nobody
        // "unifies" the two.
        let root = tree("symlink");
        let link = root.join("link-to-sub");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(root.join("sub"), &link).expect("symlink");
        let (rows, _) = collect(&walk_cfg(&root, &[]));
        let row = rows
            .iter()
            .find(|r| r.name_orig == "link-to-sub")
            .expect("a symlink gets its own row");
        assert!(row.is_symlink);
        assert!(
            !row.is_dir,
            "is_dir is the readdir type: a linked directory is not a directory"
        );
        assert!(
            rows.iter()
                .all(|r| !r.path.contains(&(link.display().to_string() + "/c.txt"))),
            "the default walk does not descend into a linked directory"
        );

        let (following, _) = collect(&WalkCfg {
            follow_symlinks: true,
            ..walk_cfg(&root, &[])
        });
        assert!(
            has(&following, "link-to-sub/c.txt"),
            "the port's `true` really follows — the recorded deviation in the field docs"
        );
    }

    #[test]
    fn the_sink_sees_batches_of_a_thousand() {
        // Go's writer flushes one transaction per 1 000 entries (`:402`); the port hands the sink
        // exactly that, which is what makes a rebuild a bounded number of transactions.
        let root = scratch("batches");
        for i in 0..1_002 {
            touch(&root, &format!("f{i:04}.txt"));
        }
        let mut sizes = Vec::new();
        let cancel = CancelToken::new();
        let stats = walk(
            &walk_cfg(&root, &[]),
            &cancel,
            &mut |batch: &[IndexedRow]| sizes.push(batch.len()),
        )
        .expect("walk");
        assert_eq!(
            sizes,
            vec![BATCH_ROWS, 3],
            "root + 1 002 files in two flushes"
        );
        assert_eq!(stats.rows(), 1_003);
    }

    #[test]
    fn a_cancelled_walk_stops_at_the_next_entry_and_reports_cancelled() {
        // Go checks `ctx.Err()` once per visited entry (`:293-295`); so does this walk. The rows are
        // buffered in batches of 1 000, so the token is fired from the flush the big tree produces —
        // and the walk must stop there rather than finish and report success.
        let root = scratch("cancelled");
        for i in 0..1_002 {
            touch(&root, &format!("f{i:04}.txt"));
        }
        let cfg = walk_cfg(&root, &[]);
        let cancel = CancelToken::new();
        let mut seen = 0;
        let result = walk(&cfg, &cancel, &mut |batch: &[IndexedRow]| {
            seen += batch.len();
            if seen >= BATCH_ROWS {
                let _ = cancel.cancel();
            }
        });
        assert!(matches!(result, Err(CoreError::Cancelled)), "{result:?}");
        assert_eq!(
            seen, BATCH_ROWS,
            "the flush happened, the rest of the tree did not walk"
        );
    }
}
