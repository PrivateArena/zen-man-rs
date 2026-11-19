//! Every numeric behaviour of the Go backend that a request could change (plan §5.2 `limits.rs` row).
//!
//! The numbers are the *measured* ones: `docs/contracts.md` is the authority, the Go file:line is
//! quoted with each constant, and where the plan's step-3 note and the Go source disagree the Go
//! source wins and the deviation is said out loud (see [`INDEX_MAX_DEPTH`] and
//! [`MAX_WALK_FILES`](crate::limits::MAX_WALK_FILES)' sibling [`INDEX_MAX_ENTRIES`]).
//!
//! The page-size **floor** ([`MAX_PAGE_SIZE`]) is ours: Go has no ceiling at all — `api/dir.go:106-109`
//! takes any `limit > 0`, so `limit=1000000000` asks for the whole directory in one page. Capping it is
//! a deliberate improvement (plan §6.2's list style: bounded work per request in a single process, where
//! an unbounded request blocks a job slot instead of a browser tab).

use std::time::Duration;

/// Flat (recursive) listing cap: Go's `limitWalk := 10000` (`api/dir.go:132`), proven by
/// `dir-flat-cap.json` (`total_count: 10000` out of a 10 005-file tree; `docs/contracts.md`
/// *Cursor and pagination*).
pub const MAX_WALK_FILES: usize = 10_000;

/// Largest page the port will serve. Ours, not Go's — Go ignores an oversized `limit` rather than
/// clamping it (`api/dir.go:106-109`).
pub const MAX_PAGE_SIZE: usize = 1_000;

/// Default page size: Go's `limit := 200` (`api/dir.go:106`), the value every `dir-*.json` golden used.
pub const DEFAULT_PAGE_SIZE: usize = 200;

/// Smallest page: a `limit` of 0 would make [`crate::cursor::page`] never advance, so the request is
/// clamped rather than hung. Go's guard is `l > 0` (`api/dir.go:107`), which has the same effect.
pub const MIN_PAGE_SIZE: usize = 1;

/// Cap on the byte offset the undo log is scanned/replayed (plan §5.2 step-3 note: `100_000`). Go has
/// no such cap — it walks the whole file from offset 0 (`api/log.go:161-199`), which is fine for a
/// request-driven server and not for an in-process startup scan.
pub const LOG_SCAN_CAP: usize = 100_000;

/// `track_entries` threshold: Go attaches the entry list to a job result only for ≤100 items and
/// otherwise sends `reload_required` (`api/ops.go`, `docs/contracts.md` *Deviation ledger* #12,
/// `op-paste-bulk.json` with 104).
pub const TRACK_ENTRIES_MAX: usize = 100;

/// Index walker depth cap. Plan §5.2's step-3 note records `maxDepth=20`; the **measured** Go default
/// is `MaxDepth: 0`, i.e. unlimited (`api/search_index.go:105`, and `api/search_index_walker.go:296`
/// only applies the limit when `> 0`). The port's default is the plan's 20 — the fixture tree is
/// exactly 20 deep (`scripts/make-fixtures.py`, `deep/d0/…/d19/leaf.txt`), so the boundary is testable.
pub const INDEX_MAX_DEPTH: usize = 20;

/// Index entry budget per rebuild. Plan §5.2's step-3 note records `maxEntries=5000`; no such constant
/// exists in the Go source (checked `api/search_index*.go`), so this is a port-side cap.
pub const INDEX_MAX_ENTRIES: usize = 5_000;

/// Live (filesystem) search result ceiling: Go's `limit <= 0` becomes `1000000`
/// (`api/search.go:72`, "effectively unlimited safety ceiling").
pub const LIVE_SEARCH_CAP: i64 = 1_000_000;

/// Live-search node budget (plan §5.2 `live_search_nodes`).
pub const LIVE_SEARCH_NODES: usize = 100_000;

/// Archive manifest LRU size: Go's `maxCacheEntries = 10` (`api/archive_vfs.go:43`).
pub const ARCHIVE_CACHE_ENTRIES: usize = 10;

/// Archive manifest TTL: Go's `cacheTTL = 15 * time.Minute` (`api/archive_vfs.go:44`).
pub const ARCHIVE_CACHE_TTL: Duration = Duration::from_secs(15 * 60);

/// Concurrent extract jobs: Go's cap of 3 (`api/archive_extract.go`, plan §2.3).
pub const CONCURRENT_EXTRACTS: usize = 3;

/// Concurrent engine jobs (plan §5.2 `concurrent_jobs`).
pub const CONCURRENT_JOBS: usize = 8;

/// Nested-archive/uncompressed output ceiling: the port's addition that makes an archive bomb a
/// reported error instead of an OOM (plan F8, `Limits::nested_archive_max_bytes`).
pub const NESTED_ARCHIVE_MAX_BYTES: u64 = 1024 * 1024 * 1024;

/// Custom-action output cap (plan §5.2 `exec_output_cap`, 1 MiB; Go capped nothing — `plan F10`).
pub const EXEC_OUTPUT_CAP: usize = 1024 * 1024;

/// Done extract jobs are GC'd an hour after finishing: Go's `1*time.Hour`
/// (`api/archive_extract.go:49`).
pub const EXTRACT_DONE_TTL: Duration = Duration::from_secs(60 * 60);

/// Live job records survive this long after the job ends (plan §5.2 `live_job_ttl`, 30 min — Go's
/// eviction ticker for manifests is `30 * time.Minute`, `api/archive_extract.go:44`).
pub const LIVE_JOB_TTL: Duration = Duration::from_secs(30 * 60);

/// Progress events are throttled to one per this interval (plan F2, `ops/mod.rs::Progress::report`).
pub const PROGRESS_THROTTLE: Duration = Duration::from_millis(50);

/// Whether an undo-log append is `flush`+`sync_all`ed before it returns (plan §6.1 F4).
///
/// Go never synced (`api/log.go:249-252` is a bare `Write`), so a power loss after an append could
/// lose the last record — acceptable for a *history view*, not for the only undo record of a
/// `paste-move`. `history::store` therefore defaults to durable appends; the cost is one `fsync` per
/// user action, not per file.
pub const SYNC_EACH_WRITE: bool = true;

/// Watch-descriptor ceiling for one filesystem watcher (plan §5.5 `index/watcher.rs`, step 12b).
///
/// inotify costs one `inotify_add_watch` per directory — a recursive watch over N roots is N watches plus
/// one per subdirectory that appears — so the budget has to be a number the engine owns rather than an
/// arbitrary constant in the watcher. It sits below the usual `fs.inotify.max_user_watches` default
/// (8 192 on this box) because the same quota is shared with every other inotify user in the session
/// (and with the Go app, if it is still running); a root set over budget is refused at `start`, and a
/// directory that appears after the budget is spent is reported, not silently skipped.
pub const WATCH_MAX_WATCHES: usize = 1_024;

/// Per-directory TTLs, indexed by [`DirCacheTier`].
///
/// Go re-read every page of every directory and cached nothing, so these are ours; the three sizes
/// trade staleness against syscall load the way plan §4.3's `DirCacheEntry` needs (its 2 s entry TTL is
/// [`DirCacheTier::Small`]).
///
/// [`DirCacheTier`]: crate::limits::DirCacheTier
pub const DIR_CACHE_TTLS: [Duration; 3] = [
    DirCacheTier::Small.duration(),
    DirCacheTier::Mid.duration(),
    DirCacheTier::Big.duration(),
];

/// Which of the three directory-cache TTLs a listing gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirCacheTier {
    /// A small directory: re-validate quickly (2 s, plan §4.3).
    Small,
    /// A medium directory (30 s).
    Mid,
    /// A large or slow directory (120 s).
    Big,
}

impl DirCacheTier {
    /// The TTL for this tier.
    #[must_use]
    pub const fn duration(self) -> Duration {
        match self {
            Self::Small => Duration::from_secs(2),
            Self::Mid => Duration::from_secs(30),
            Self::Big => Duration::from_secs(120),
        }
    }

    /// Pick a tier from an entry count: ≤200 is small (one page), ≤5 000 is medium, beyond that the
    /// read cost dominates the staleness risk.
    #[must_use]
    pub fn for_entry_count(count: usize) -> Self {
        if count <= DEFAULT_PAGE_SIZE {
            Self::Small
        } else if count <= INDEX_MAX_ENTRIES {
            Self::Mid
        } else {
            Self::Big
        }
    }
}

/// The tunable form of this module's constants (plan §5.2 `limits.rs` row: a `Limits` struct with a
/// `Default` that matches every number found in the Go source).
///
/// The constants are the source of truth; every field of [`Limits::default`] is one of them, so a test
/// can assert the pair never drifts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Default directory page size (Go: `api/dir.go:106`).
    pub dir_page_default: usize,
    /// Hard page-size ceiling (port-side, see [`MAX_PAGE_SIZE`]).
    pub dir_page_max: usize,
    /// Flat walk cap (Go: `api/dir.go:132`).
    pub flat_walk_cap: usize,
    /// `track_entries` threshold (Go: `api/ops.go`).
    pub track_entries_max: usize,
    /// Live search ceiling (Go: `api/search.go:72`).
    pub live_search_cap: i64,
    /// Live search node budget.
    pub live_search_nodes: usize,
    /// Archive manifest LRU size (Go: `api/archive_vfs.go:43`).
    pub archive_cache_entries: usize,
    /// Archive manifest TTL (Go: `api/archive_vfs.go:44`).
    pub archive_cache_ttl: Duration,
    /// Concurrent extract jobs (Go: `api/archive_extract.go`).
    pub concurrent_extracts: usize,
    /// Concurrent engine jobs.
    pub concurrent_jobs: usize,
    /// Nested archive byte ceiling (port-side, plan F8).
    pub nested_archive_max_bytes: u64,
    /// Custom action output cap.
    pub exec_output_cap: usize,
    /// Done extract job retention (Go: `api/archive_extract.go:49`).
    pub extract_done_ttl: Duration,
    /// Live job retention.
    pub live_job_ttl: Duration,
    /// Undo log scan cap (port-side).
    pub log_scan_cap: usize,
    /// Index depth cap (see [`INDEX_MAX_DEPTH`] for the Go difference).
    pub index_max_depth: usize,
    /// Index entry budget (see [`INDEX_MAX_ENTRIES`] for the Go difference).
    pub index_max_entries: usize,
    /// Directory cache TTLs.
    pub dir_cache_ttls: [Duration; 3],
    /// `flush` + `sync_all` every undo-log append (see [`SYNC_EACH_WRITE`]).
    pub sync_each_write: bool,
    /// Watch descriptors one watcher may hold (see [`WATCH_MAX_WATCHES`]; step 12b's watcher).
    pub watch_max_watches: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            dir_page_default: DEFAULT_PAGE_SIZE,
            dir_page_max: MAX_PAGE_SIZE,
            flat_walk_cap: MAX_WALK_FILES,
            track_entries_max: TRACK_ENTRIES_MAX,
            live_search_cap: LIVE_SEARCH_CAP,
            live_search_nodes: LIVE_SEARCH_NODES,
            archive_cache_entries: ARCHIVE_CACHE_ENTRIES,
            archive_cache_ttl: ARCHIVE_CACHE_TTL,
            concurrent_extracts: CONCURRENT_EXTRACTS,
            concurrent_jobs: CONCURRENT_JOBS,
            nested_archive_max_bytes: NESTED_ARCHIVE_MAX_BYTES,
            exec_output_cap: EXEC_OUTPUT_CAP,
            extract_done_ttl: EXTRACT_DONE_TTL,
            live_job_ttl: LIVE_JOB_TTL,
            log_scan_cap: LOG_SCAN_CAP,
            index_max_depth: INDEX_MAX_DEPTH,
            index_max_entries: INDEX_MAX_ENTRIES,
            dir_cache_ttls: DIR_CACHE_TTLS,
            sync_each_write: SYNC_EACH_WRITE,
            watch_max_watches: WATCH_MAX_WATCHES,
        }
    }
}

impl Limits {
    /// Clamp a requested page size into `[MIN_PAGE_SIZE, MAX_PAGE_SIZE]`.
    #[must_use]
    pub fn page_size(&self, requested: Option<usize>) -> usize {
        requested
            .unwrap_or(self.dir_page_default)
            .clamp(1, self.dir_page_max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_constants() {
        let l = Limits::default();
        assert_eq!(l.dir_page_default, DEFAULT_PAGE_SIZE);
        assert_eq!(l.flat_walk_cap, MAX_WALK_FILES);
        assert_eq!(l.track_entries_max, TRACK_ENTRIES_MAX);
        assert_eq!(l.archive_cache_entries, ARCHIVE_CACHE_ENTRIES);
        assert_eq!(l.archive_cache_ttl, ARCHIVE_CACHE_TTL);
        assert_eq!(l.concurrent_extracts, CONCURRENT_EXTRACTS);
        assert_eq!(l.concurrent_jobs, CONCURRENT_JOBS);
        assert_eq!(l.nested_archive_max_bytes, NESTED_ARCHIVE_MAX_BYTES);
        assert_eq!(l.exec_output_cap, EXEC_OUTPUT_CAP);
        assert_eq!(l.extract_done_ttl, EXTRACT_DONE_TTL);
        assert_eq!(l.live_job_ttl, LIVE_JOB_TTL);
        assert_eq!(l.dir_cache_ttls, DIR_CACHE_TTLS);
        assert_eq!(l.sync_each_write, SYNC_EACH_WRITE);
        assert_eq!(l.watch_max_watches, WATCH_MAX_WATCHES);
    }

    #[test]
    fn numbers_match_the_measured_go_source() {
        // The five numbers plan §5.2's step-3 note attributes to Go, each with its measured anchor.
        assert_eq!(DEFAULT_PAGE_SIZE, 200); // api/dir.go:106
        assert_eq!(MAX_WALK_FILES, 10_000); // api/dir.go:132
        assert_eq!(TRACK_ENTRIES_MAX, 100); // api/ops.go track_entries
        assert_eq!(LIVE_SEARCH_CAP, 1_000_000); // api/search.go:72
        assert_eq!(ARCHIVE_CACHE_ENTRIES, 10); // api/archive_vfs.go:43
        assert_eq!(ARCHIVE_CACHE_TTL, Duration::from_secs(900)); // api/archive_vfs.go:44
        assert_eq!(EXTRACT_DONE_TTL, Duration::from_secs(3600)); // api/archive_extract.go:49
        // Ours, and said so: the page-size ceiling and the log scan cap.
        assert_eq!(MAX_PAGE_SIZE, 1_000);
        assert_eq!(LOG_SCAN_CAP, 100_000);
    }

    #[test]
    fn contracts_document_values_agree_with_limits() {
        // Plan §5.2 `limits.rs` *Done when*: the numbers quoted in docs/contracts.md match
        // `Limits::default()`. Reading the file (rather than a copy of it) is the point: a doc change
        // must break this test.
        let doc = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/contracts.md"),
        )
        .unwrap();
        let l = Limits::default();
        for needle in [
            "default **200**",     // /api/dir limit
            "`total_count:10000`", // flat cap
            "10 000",              // prose form of the same cap
        ] {
            assert!(
                doc.contains(needle),
                "docs/contracts.md no longer mentions {needle:?}"
            );
        }
        assert_eq!(l.dir_page_default, 200);
        assert_eq!(l.flat_walk_cap, 10_000);
    }

    #[test]
    fn page_size_clamps_both_ends() {
        let l = Limits::default();
        assert_eq!(l.page_size(None), 200);
        assert_eq!(l.page_size(Some(0)), MIN_PAGE_SIZE);
        assert_eq!(l.page_size(Some(50)), 50);
        assert_eq!(l.page_size(Some(usize::MAX)), MAX_PAGE_SIZE);
    }

    #[test]
    fn dir_cache_ttls_are_ordered_and_indexable() {
        assert_eq!(DIR_CACHE_TTLS.len(), 3);
        assert_eq!(
            DIR_CACHE_TTLS,
            [
                DirCacheTier::Small.duration(),
                DirCacheTier::Mid.duration(),
                DirCacheTier::Big.duration()
            ]
        );
        assert!(DirCacheTier::Small.duration() < DirCacheTier::Mid.duration());
        assert!(DirCacheTier::Mid.duration() < DirCacheTier::Big.duration());
        assert_eq!(DirCacheTier::for_entry_count(200), DirCacheTier::Small);
        assert_eq!(DirCacheTier::for_entry_count(201), DirCacheTier::Mid);
        assert_eq!(DirCacheTier::for_entry_count(6_000), DirCacheTier::Big);
    }
}
