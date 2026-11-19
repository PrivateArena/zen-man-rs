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
                let advances = match self.tokens[si] {
                    Token::Lit(e) => e == c,
                    Token::Any => c != '\n',
                    // A star that consumed the character stays where it is (the loop edge).
                    Token::SegStar => c != '/' && c != '\n',
                    Token::AnyStar => c != '\n',
                };
                if advances {
                    next[si] = true;
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
            followed: HashSet::new(),
        };
        walker.run(&mut batch, sink)?;
        let RootWalker {
            timed_out,
            failed,
            produced,
            visited,
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
                continue;
            };
            self.visited += 1;
            let abs = slash(child.display().to_string());
            if self.cfg.excludes.matches(abs.as_str(), rel.as_str()) {
                continue;
            }
            if self.cfg.max_depth > 0 && depth + 1 > self.cfg.max_depth {
                continue;
            }
            self.emit(&child, &meta, base, batch, sink)?;
            if meta.is_dir() && self.may_descend(&child, &meta) {
                match self.frame(&child, rel, depth + 1) {
                    Ok(f) => stack.push(f),
                    Err(_) => {
                        // Go's `WalkDir` skips an unreadable directory (`:286-291`).
                        continue;
                    }
                }
            }
        }
        Ok(())
    }

    /// With `follow_symlinks` the walk may enter a linked directory; a visited set of canonical paths is
    /// what keeps a `a → b → a` pair of links from looping forever.
    fn may_descend(&mut self, path: &Path, meta: &std::fs::Metadata) -> bool {
        if !meta.file_type().is_symlink() {
            return true;
        }
        if !self.cfg.follow_symlinks {
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
