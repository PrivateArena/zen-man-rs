//! Directory listing + size services, and the cache that keeps a GUI from re-reading a directory per
//! keystroke (plan §5.3 `services/dirs.rs`, §4.3's cache rules, §6.1 F13).
//!
//! # What is and is not a seam
//!
//! [`crate::platform::Env`] answers *OS actions* (open, chmod, trash, statvfs, spawn) and has no
//! directory-read method, so this module owns the one place under `services/` that touches the
//! filesystem, behind [`FsSource`]. Every listing and size path in the engine therefore runs through a
//! swappable source — and that is how the cache assertion is made: [`CountingSource`] counts reads.
//!
//! # Cache model (plan §4.3)
//!
//! Go cached nothing and re-read the whole directory for every page of every request. In-process that
//! is one `readdir`+`stat` storm per repaint, so:
//!
//! * pages are keyed by [`DirCacheKey`] (store root + cursor + limit + sort) and expire by
//!   [`DirCacheTier`] (`Small` = plan §4.3's 2 s, then `Mid`, `Big`);
//! * [`DirService::invalidate`] takes an [`FsPath`], never an [`AbsPath`], so an archive member can be
//!   named (plan §5.9's row; §9.1's correction to §4.1), and drops every cached page that *is* `path`,
//!   is below it, or is above it (an entry's mtime and size appear in its parent's rows);
//! * the invalidation *event* is coalesced: repeated invalidations of one path inside one tick collapse
//!   into a single [`EngineEvent::DirChanged`] ([`DirService::take_invalidations`], plan §6.1 F2's
//!   storm row).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cursor::{self, Cursor};
use crate::entry::{DirEntryLike, DirPage, EntryKind, EntryLike, EntryStat, FileEntry, InfoEntry};
use crate::error::CoreError;
use crate::events::{ChangeCause, EngineEvent};
use crate::jobs::CancelToken;
use crate::limits::{DEFAULT_PAGE_SIZE, DirCacheTier, Limits, PROGRESS_THROTTLE};
use crate::metrics::GO_ZERO_TIME_UNIX;
use crate::path::{AbsPath, FsPath};
use crate::session::Origin;
use crate::store::config::{SortColumn, SortSpec};

// ------------------------------------------------------------------------------------------- the seam

/// The filesystem questions a directory service asks, and nothing else.
///
/// An `Err` from [`FsSource::lstat`] means "this row degrades" — Go's `entry.Info()` failure
/// (`api/dir.go:235-247`, `dir-perm-noexec.json`) — which the caller turns into a name-only row
/// rather than failing the whole listing.
pub trait FsSource: Send + Sync {
    /// Read one directory: name + kind from the readdir type, in no particular order (the listing
    /// sorts afterwards, exactly like Go).
    fn read_dir(&self, path: &AbsPath) -> Result<Vec<DirEntryLike>, CoreError>;

    /// `lstat` one path. This is Go's `fs.DirEntry.Info()`: **link metadata, not the target's**, which
    /// is why `link-to-dir` is `is_dir: false` on the wire (the follow policy plan §6.2 introduces is
    /// a step-9 option, off for parity).
    fn lstat(&self, path: &Path) -> Result<InfoEntry, CoreError>;
}

/// The `std::fs` implementation of [`FsSource`] — the only such call site under `services/`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OsDirRead;

impl FsSource for OsDirRead {
    fn read_dir(&self, path: &AbsPath) -> Result<Vec<DirEntryLike>, CoreError> {
        let dir = std::fs::read_dir(path.as_path()).map_err(|e| classify_read_error(path, e))?;
        let mut out = Vec::new();
        for entry in dir {
            let entry = match entry {
                Ok(e) => e,
                // One unreadable name is a skipped row, not a failed listing.
                Err(_) => continue,
            };
            // `DirEntry::file_type()` is `d_type` from the readdir and fails only on a platform with no
            // such information; an unknown type degrades to a file, the same answer Go's `Type()` gives
            // for an entry it could not classify.
            let kind = match entry.file_type() {
                Ok(file_type) => EntryKind::from_file_type(&file_type, false),
                Err(_) => EntryKind::File,
            };
            out.push(DirEntryLike::new(entry.file_name().to_string_lossy(), kind));
        }
        Ok(out)
    }

    fn lstat(&self, path: &Path) -> Result<InfoEntry, CoreError> {
        let meta = std::fs::symlink_metadata(path)
            .map_err(|e| CoreError::io(path.display().to_string(), e))?;
        let name = path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
            .into_owned();
        Ok(InfoEntry::from_metadata(name, &meta.file_type(), &meta))
    }
}

/// A `read_dir` failure worth distinguishing: ENOENT is a *gone* directory, EACCES is Go's 500 with
/// the permission text (`err-perm-denied-dir.json`), and anything else is the plain I/O error.
fn classify_read_error(path: &AbsPath, e: std::io::Error) -> CoreError {
    match e.kind() {
        std::io::ErrorKind::NotFound => CoreError::NotFound {
            path: path.to_string_lossy(),
            source_path: None,
        },
        std::io::ErrorKind::PermissionDenied => CoreError::PermissionDenied {
            path: path.to_string_lossy(),
        },
        _ => CoreError::io(path.to_string_lossy(), e),
    }
}

/// A [`FsSource`] that counts calls, so a test can assert the cache *hit* rather than the same answer
/// a re-read would have produced anyway.
pub struct CountingSource {
    /// The underlying source.
    pub inner: Arc<dyn FsSource>,
    reads: std::sync::atomic::AtomicU64,
    stats: std::sync::atomic::AtomicU64,
}

impl CountingSource {
    /// Wrap `inner`.
    #[must_use]
    pub fn new(inner: Arc<dyn FsSource>) -> Self {
        Self {
            inner,
            reads: std::sync::atomic::AtomicU64::new(0),
            stats: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// `read_dir` calls so far.
    #[must_use]
    pub fn reads(&self) -> u64 {
        self.reads.load(std::sync::atomic::Ordering::Acquire)
    }

    /// `lstat` calls so far.
    #[must_use]
    pub fn stats(&self) -> u64 {
        self.stats.load(std::sync::atomic::Ordering::Acquire)
    }
}

impl FsSource for CountingSource {
    fn read_dir(&self, path: &AbsPath) -> Result<Vec<DirEntryLike>, CoreError> {
        let _ = self.reads.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.inner.read_dir(path)
    }

    fn lstat(&self, path: &Path) -> Result<InfoEntry, CoreError> {
        let _ = self.stats.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.inner.lstat(path)
    }
}

// ---------------------------------------------------------------------------------------- page types

/// What a caller asks for: the `GET /api/dir` parameters (plan §4.1's `ListDir`), minus
/// `flat`/`no_folders`, whose walk arrives with `ops/list.rs` in step 9.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListDir {
    /// The store + relative path to list.
    pub path: FsPath,
    /// `None` = the first page (Go's absent or unparsable `cursor`, `api/dir.go:111-115`).
    pub cursor: Option<Cursor>,
    /// Page size, clamped by [`Limits::page_size`] inside [`DirService::page`].
    pub limit: usize,
    /// Column and direction. Dirs-first is an invariant, not a field (plan §5.3's `sort_entries` row).
    pub sort: SortSpec,
}

impl ListDir {
    /// The first page, default sort — what a pane asks for on `cd`.
    #[must_use]
    pub fn first(path: FsPath) -> Self {
        Self {
            path,
            cursor: None,
            limit: DEFAULT_PAGE_SIZE,
            sort: SortSpec::DEFAULT_SORT,
        }
    }
}

/// One cached page plus the two times plan §4.3's `DirCacheEntry` carries.
#[derive(Clone, Debug)]
pub struct DirPageValue {
    /// The directory this entry answers, kept alongside the page so invalidation can ask the
    /// ancestor/descendant question without re-parsing a cache key.
    pub store_root: FsPath,
    /// The page. Shared, because a pane's `Tab::entries` is the same `Arc` (plan §4.3: "no clone per
    /// frame").
    pub page: Arc<DirPage>,
    /// When the directory was read.
    pub fetched_at: Instant,
    /// When the entry stops being answerable without a re-read.
    pub invalidate_at: Instant,
    /// How many times this entry has been served.
    pub hits: u64,
    /// Which TTL tier this page got, from its entry count.
    pub tier: DirCacheTier,
}

/// The cache key: the store root plus everything that changes the answer (plan §5.6 step-6 row).
///
/// It hashes to its own [`DirCacheKey::token`] text rather than deriving `Ord`/`Hash`: [`FsPath`] and
/// [`SortSpec`] are `Eq` but not ordered, and the cache wants one cheap comparable key, not four
/// partially-ordered ones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirCacheKey {
    /// The listed directory in store coordinates (`FsPath`, so an archive listing is keyable).
    pub store_root: FsPath,
    /// Requested cursor, `None` = first page.
    pub cursor: Option<Cursor>,
    /// Requested (clamped) page size.
    pub limit: usize,
    /// Sort column + direction.
    pub sort: SortSpec,
}

impl DirCacheKey {
    /// The one-to-one text form of the key: `DirToken | cursor | limit | sort`.
    ///
    /// `DirToken`'s grammar escapes `|` inside the path, so the join cannot be forged by a filename, and
    /// two different keys can never produce the same text.
    #[must_use]
    pub fn token(&self) -> String {
        let cursor = self.cursor.map_or(String::new(), |c| c.encode());
        format!(
            "{}|{}|{}|{:?}/{}",
            self.store_root.dir_token(),
            cursor,
            self.limit,
            self.sort.column,
            if self.sort.descending { "desc" } else { "asc" }
        )
    }
}

/// A bounded LRU cache: insertion-order map plus a recency tick per entry (plan §5.6's `BoundedCache`).
/// Both caches here are small, so the eviction scan is a `BTreeMap` walk, not an extra linked list.
#[derive(Debug)]
pub struct BoundedCache<K, V> {
    cap: usize,
    entries: BTreeMap<K, CacheEntry<V>>,
    clock: u64,
    hits: u64,
    misses: u64,
}

#[derive(Debug)]
struct CacheEntry<V> {
    tick: u64,
    value: V,
}

impl<K, V> BoundedCache<K, V>
where
    K: Ord + Clone,
{
    /// A cache holding at most `cap` entries (at least one).
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            entries: BTreeMap::new(),
            clock: 0,
            hits: 0,
            misses: 0,
        }
    }

    /// Look up and refresh recency, counting a hit/miss.
    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        let found = self.entries.contains_key(key);
        if found {
            self.hits += 1;
            self.clock += 1;
            let tick = self.clock;
            // `BTreeMap::get_mut` cannot touch a sibling field, and the key is already known present.
            if let Some(entry) = self.entries.get_mut(key) {
                entry.tick = tick;
                return Some(&mut entry.value);
            }
            None
        } else {
            self.misses += 1;
            None
        }
    }

    /// Read-only lookup (no recency, no counters).
    #[must_use]
    pub fn peek(&self, key: &K) -> Option<&V> {
        self.entries.get(key).map(|e| &e.value)
    }

    /// Insert, evicting least-recently-used entries past the cap.
    pub fn insert(&mut self, key: K, value: V) {
        self.clock += 1;
        let tick = self.clock;
        self.entries.insert(key, CacheEntry { tick, value });
        while self.entries.len() > self.cap {
            let lru = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.tick)
                .map(|(k, _)| k.clone());
            match lru {
                Some(k) => {
                    self.entries.remove(&k);
                }
                None => break,
            }
        }
    }

    /// Remove every key for which `pred` answers `true`. Returns how many went.
    pub fn retain_keys(&mut self, mut pred: impl FnMut(&K) -> bool) -> usize {
        let doomed: Vec<K> = self.entries.keys().filter(|k| pred(k)).cloned().collect();
        let n = doomed.len();
        for k in doomed {
            self.entries.remove(&k);
        }
        n
    }

    /// Remove every entry for which `pred` answers `true`, value included.
    ///
    /// The two-argument form is what `invalidate` needs: it decides on the *listed path*, which the
    /// cache key holds only as a token string.
    pub fn retain_where(&mut self, mut pred: impl FnMut(&K, &V) -> bool) -> usize {
        let doomed: Vec<K> = self
            .entries
            .iter()
            .filter(|(k, e)| pred(k, &e.value))
            .map(|(k, _)| k.clone())
            .collect();
        let n = doomed.len();
        for k in doomed {
            self.entries.remove(&k);
        }
        n
    }

    /// Read-only iteration.
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.entries.iter().map(|(k, e)| (k, &e.value))
    }

    /// `true` when nothing is cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entry count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Drop everything, returning the number removed.
    pub fn clear(&mut self) -> usize {
        let n = self.entries.len();
        self.entries.clear();
        n
    }

    /// Hit/miss counters.
    #[must_use]
    pub fn counts(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }
}

/// A cached size answer: the `/api/dir/size` payload minus its `status` (which the port expresses as
/// a `Result`), and the `folder_sizes` row of plan §5.4.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DirSizeStat {
    /// The directory that was walked (Go's `resolvedPath`).
    pub path: AbsPath,
    /// Sum of the sizes of the files beneath it, links included by their *link* size (Go's
    /// `api/dir.go:388-397` walks `d.Info()`, which is `lstat`).
    pub bytes: u64,
    /// How many non-directory entries the walk counted.
    pub files_count: u64,
}

impl DirSizeStat {
    /// The `(size, files_count)` pair Go's handler answered with.
    #[must_use]
    pub const fn to_wire_pair(&self) -> (u64, u64) {
        (self.bytes, self.files_count)
    }

    /// The typed pair the UI renders, where `files` is never an honest `0`
    /// ([`crate::metrics::FileSizes`], plan §2.4's `(undefined files)` row).
    #[must_use]
    pub const fn sizes(&self) -> crate::metrics::FileSizes {
        crate::metrics::FileSizes {
            bytes: self.bytes,
            files: Some(self.files_count),
        }
    }
}

// --------------------------------------------------------------------------------------- the walk

/// How far a size walk may go.
///
/// **The default is uncapped, and that is deliberate.** [`crate::limits::MAX_WALK_FILES`] is Go's
/// *flat-listing* cap (`api/dir.go:132`, proven by `dir-flat-cap.json`), and the measured
/// `dir-size-root.json` walk counted 11 235 files — above it. Reusing the listing cap here would
/// change the number the golden pins, so the size walk's own bound is a caller choice; pass
/// `Some(limits.flat_walk_cap)` to have one and get
/// [`CoreError::CapacityExceeded`] rather than a quietly wrong total.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirSizeOpts {
    /// Maximum entries the walk may visit before it refuses (see the type docs).
    pub walk_cap: Option<usize>,
    /// Report progress every this many items (the walk's throttle, on top of [`Progress`]'s).
    pub progress_every: u64,
}

impl Default for DirSizeOpts {
    fn default() -> Self {
        Self {
            walk_cap: None,
            progress_every: 256,
        }
    }
}

/// Sink the size walk reports through, so `services/` does not have to name the job type it is called
/// with: `Session` passes [`crate::jobs::Progress`] (which implements this), a test passes a recorder.
pub trait DirSizeSink: Send + Sync {
    /// `items` entries visited, `bytes` accumulated so far, `current` the last name seen (`""` at the
    /// end of a walk).
    fn report(&self, items: u64, bytes: u64, current: &str);
}

impl DirSizeSink for crate::jobs::Progress {
    fn report(&self, items: u64, bytes: u64, current: &str) {
        let name = (!current.is_empty()).then(|| current.to_string());
        // `set`, not `add`: the walk's numbers are cumulative already, and adding cumulative totals
        // every 256 items would quadratically overstate the progress bar.
        crate::jobs::Progress::set(self, bytes, items, name);
    }
}

/// A sink that drops everything (a cached-path read that has no listener).
#[derive(Clone, Copy, Debug, Default)]
pub struct SilentSink;

impl DirSizeSink for SilentSink {
    fn report(&self, _items: u64, _bytes: u64, _current: &str) {}
}

/// A sink that keeps only the final counters — what `dir_size_matches_go`'s cancellation test reads.
#[derive(Debug, Default)]
pub struct RecordingSink {
    /// Every report, in order.
    pub reports: std::sync::Mutex<Vec<(u64, u64, String)>>,
}

impl DirSizeSink for RecordingSink {
    fn report(&self, items: u64, bytes: u64, current: &str) {
        let mut guard = self
            .reports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.push((items, bytes, current.to_string()));
    }
}

// --------------------------------------------------------------------------------------- the service

/// The directory service: listing, the two caches, and the invalidation queue.
///
/// No `Debug`: the source is `dyn FsSource` (a filesystem, or a test's in-memory tree) and neither can
/// print anything meaningful.
pub struct DirService {
    source: Arc<dyn FsSource>,
    limits: Arc<Limits>,
    inner: std::sync::Mutex<DirInner>,
}

struct DirInner {
    pages: BoundedCache<String, DirPageValue>,
    sizes: BoundedCache<DirSizeStatKey, DirSizeStat>,
    /// Paths invalidated but not yet announced, keyed by token so repeats collapse (plan F2).
    pending: BTreeMap<String, PendingChange>,
    /// The coalescing window.
    coalesce: Duration,
}

#[derive(Clone, Debug)]
struct PendingChange {
    path: FsPath,
    first_seen: Instant,
    count: u32,
    cause: ChangeCause,
    origin: Option<Origin>,
}

/// The size cache is keyed by the canonical path token (plan §4.2: `AbsPath` is the only key type for
/// `folder_sizes`), so `/a/./b` and `/a/b` cannot be two rows.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct DirSizeStatKey(String);

impl DirSizeStatKey {
    fn of(path: &AbsPath) -> Self {
        Self(FsPath::from_abs(path.clone()).dir_token().to_string())
    }
}

impl DirService {
    /// A service over `source` with `limits`, using the default page-cache capacity.
    #[must_use]
    pub fn new(source: Arc<dyn FsSource>, limits: Arc<Limits>) -> Self {
        Self::with_capacity(source, limits, 64)
    }

    /// As [`Self::new`], with an explicit page-cache capacity and the plan's coalescing window.
    #[must_use]
    pub fn with_capacity(
        source: Arc<dyn FsSource>,
        limits: Arc<Limits>,
        page_cache_cap: usize,
    ) -> Self {
        Self {
            source,
            limits,
            inner: std::sync::Mutex::new(DirInner {
                pages: BoundedCache::new(page_cache_cap),
                sizes: BoundedCache::new(4_096),
                pending: BTreeMap::new(),
                coalesce: PROGRESS_THROTTLE,
            }),
        }
    }

    /// The configured limits.
    #[must_use]
    pub fn limits(&self) -> &Arc<Limits> {
        &self.limits
    }

    /// The filesystem source, for callers (the size walk, step 9's ops) that need it.
    #[must_use]
    pub fn source(&self) -> &Arc<dyn FsSource> {
        &self.source
    }

    /// One page of a listing, from the cache when it is fresh.
    ///
    /// Synchronous by contract: one `readdir` of one directory is plan §4.1's definition of a read that
    /// may run on the caller's thread ("they never block longer than one directory"). A cold archive
    /// manifest and a flat walk are jobs — step 13 and step 9 route those through `Session::page_dir`,
    /// which asks [`Self::cached_page`] first and spawns only when the cache cannot answer.
    ///
    /// # Errors
    /// [`CoreError::NotFound`] when the directory is gone, [`CoreError::PermissionDenied`] when it
    /// cannot be read, [`CoreError::NotADir`] when it is a file, [`CoreError::VfsReadOnly`] when the
    /// path names an archive store (that is the archive service's listing, not this one).
    pub fn page(&self, req: &ListDir) -> Result<Arc<DirPage>, CoreError> {
        let key = self.key_for(req);
        let now = Instant::now();
        if let Some(hit) = self.fresh_page(&key, now) {
            return Ok(hit);
        }
        let page = Arc::new(self.read_page(&req.path, req.cursor, key.limit, req.sort)?);
        let tier = DirCacheTier::for_entry_count(page.total_count);
        let ttl = self.limits.dir_cache_ttls[tier_index(tier)];
        let mut inner = lock(&self.inner);
        inner.pages.insert(
            key.token(),
            DirPageValue {
                store_root: req.path.clone(),
                page: Arc::clone(&page),
                fetched_at: now,
                invalidate_at: now + ttl,
                hits: 0,
                tier,
            },
        );
        drop(inner);
        Ok(page)
    }

    /// A cached page for `req`, `None` when it must be re-read.
    #[must_use]
    pub fn cached_page(&self, req: &ListDir) -> Option<Arc<DirPage>> {
        let key = self.key_for(req);
        self.fresh_page(&key, Instant::now())
    }

    fn key_for(&self, req: &ListDir) -> DirCacheKey {
        DirCacheKey {
            store_root: req.path.clone(),
            cursor: req.cursor,
            limit: self.limits.page_size(Some(req.limit)),
            sort: req.sort,
        }
    }

    fn fresh_page(&self, key: &DirCacheKey, now: Instant) -> Option<Arc<DirPage>> {
        let token = key.token();
        let mut inner = lock(&self.inner);
        let stale = inner
            .pages
            .peek(&token)
            .is_some_and(|v| v.invalidate_at < now);
        if stale {
            // An expired entry must not be served, and must not shadow the re-read: drop it.
            inner.pages.retain_keys(|k| *k == token);
            return None;
        }
        inner.pages.get_mut(&token).map(|v| {
            v.hits += 1;
            Arc::clone(&v.page)
        })
    }

    /// Read and page one directory, ignoring the cache.
    ///
    /// The Go algorithm, entry for entry (`api/dir.go:96-281`): read names, `stat` everything when the
    /// sort needs it, sort dirs-first, paginate, then `stat` the page window for the rows.
    ///
    /// # Errors
    /// As [`Self::page`].
    pub fn read_page(
        &self,
        path: &FsPath,
        cursor: Option<Cursor>,
        limit: usize,
        sort: SortSpec,
    ) -> Result<DirPage, CoreError> {
        let host = require_local(path)?;
        let meta = self
            .source
            .lstat(host.as_path())
            .map_err(|_| CoreError::NotFound {
                path: path.to_string(),
                source_path: Some(host.to_string_lossy()),
            })?;
        if !meta.kind.is_navigable_dir() {
            // Go: `if !info.IsDir() { 400 "Path is not a directory" }` (`api/dir.go:98-101`), where
            // `info` is `os.Stat` — so a *symlinked* directory passes, as it must.
            return Err(CoreError::NotADir {
                path: path.to_string(),
            });
        }
        let raw = self.source.read_dir(&host)?;
        let needs_stats = matches!(sort.column, SortColumn::Size | SortColumn::Date);
        let mut rows: Vec<Row> = raw
            .into_iter()
            .map(|dirent| Row {
                info: needs_stats
                    .then(|| self.stat_entry(&host, &dirent).ok())
                    .flatten(),
                dirent,
            })
            .collect();
        sort_rows(&mut rows, sort);
        let total_count = rows.len();
        let start = cursor.unwrap_or(Cursor::FIRST);
        let (page_rows, next, has_more) = cursor::page(rows.as_slice(), start, limit);
        let entries: Vec<FileEntry> = page_rows
            .iter()
            .map(|row| self.build_entry(path, &host, row))
            .collect();
        let loaded_so_far = usize::try_from(
            start
                .0
                .saturating_add(u64::try_from(entries.len()).unwrap_or(u64::MAX)),
        )
        .unwrap_or(total_count)
        .min(total_count);
        Ok(DirPage {
            path: path.clone(),
            entries,
            cursor: next,
            has_more,
            total_count,
            loaded_so_far,
        })
    }

    /// The `entry.Info()` half, which may fail: the caller then builds the degraded row.
    fn stat_entry(&self, host: &AbsPath, dirent: &DirEntryLike) -> Result<InfoEntry, CoreError> {
        let path = host.join(dirent.file_name())?;
        self.source.lstat(path.as_path())
    }

    fn build_entry(&self, listing: &FsPath, host: &AbsPath, row: &Row) -> FileEntry {
        // Go re-stats the page window unconditionally for the mode (`api/dir.go:236-247`), which is
        // also what makes the degraded row possible; `FsSource::lstat` keeps it off the link's target.
        let info = self.stat_entry(host, &row.dirent).ok();
        let entry_path = listing
            .join(row.dirent.file_name())
            .unwrap_or_else(|_| listing.clone());
        let stat = EntryStat::in_listing(entry_path, listing.clone(), row.dirent.clone(), info);
        let files_count = row
            .dirent
            .kind()
            .is_dir()
            .then(|| host.join(row.dirent.file_name()).ok())
            .flatten()
            .and_then(|dir| {
                self.cached_sizes(std::slice::from_ref(&dir))
                    .into_iter()
                    .next()
                    .flatten()
            })
            .map(|s| s.files_count);
        crate::entry::EntryBuilder::from_stat(&stat)
            .files_count(files_count)
            .build()
    }

    // --------------------------------------------------------------------------------------- sizes

    /// The cached size answers for `dirs`, in order, `None` where nothing is cached.
    ///
    /// This is what `GetCachedDirSize` (`api/dir.go:250-257`) was against the `folder_sizes` table;
    /// the SQLite half of the pair is plan §5.4's `Db::dir_size_get`/`dir_size_put` row and arrives with
    /// step 12. Until then this in-memory cache is the only answer, which is exactly the state
    /// `dir-*.json` was captured in — no row, so those goldens carry no `files_count` at all.
    #[must_use]
    pub fn cached_sizes(&self, dirs: &[AbsPath]) -> Vec<Option<DirSizeStat>> {
        let inner = lock(&self.inner);
        dirs.iter()
            .map(|d| {
                let key = DirSizeStatKey::of(d);
                inner
                    .sizes
                    .iter()
                    .find(|(k, _)| **k == key)
                    .map(|(_, v)| v.clone())
            })
            .collect()
    }

    /// Record a size answer (the walk's own cache write, and what step 12 replays from the database).
    pub fn put_size(&self, stat: DirSizeStat) {
        let mut inner = lock(&self.inner);
        inner.sizes.insert(DirSizeStatKey::of(&stat.path), stat);
    }

    /// Walk one directory tree and cache the answer.
    ///
    /// Cancellation is checked at **item boundaries**: before each directory read and between every two
    /// entries, so a cancelled walk stops on a whole-item edge, never mid-item (plan §7 step 6). On
    /// `Err(`[`CoreError::Cancelled`]`)` the cache is left untouched, and the job wrapper still emits
    /// its one terminal event.
    ///
    /// # Errors
    /// [`CoreError::NotFound`], [`CoreError::NotADir`], [`CoreError::Cancelled`],
    /// [`CoreError::CapacityExceeded`], or the source's own error.
    pub fn dir_size(
        &self,
        path: &AbsPath,
        opts: &DirSizeOpts,
        cancel: &CancelToken,
        progress: &dyn DirSizeSink,
    ) -> Result<DirSizeStat, CoreError> {
        let meta = self
            .source
            .lstat(path.as_path())
            .map_err(|_| CoreError::NotFound {
                path: path.to_string_lossy(),
                source_path: None,
            })?;
        if !meta.kind.is_navigable_dir() {
            return Err(CoreError::NotADir {
                path: path.to_string_lossy(),
            });
        }
        let mut stack = vec![path.clone()];
        let mut bytes: u64 = 0;
        let mut files: u64 = 0;
        let mut visited: u64 = 0;
        let mut last = String::new();
        while let Some(dir) = stack.pop() {
            cancel.check()?;
            let entries = match self.source.read_dir(&dir) {
                Ok(entries) => entries,
                // Go skips a subtree it cannot read (`api/dir.go:375-380`: the walk callback returns
                // nil on error), so an unreadable directory contributes nothing but is not an error.
                Err(_) => continue,
            };
            for entry in entries {
                cancel.check()?;
                visited += 1;
                if let Some(cap) = opts.walk_cap
                    && usize::try_from(visited).unwrap_or(usize::MAX) > cap
                {
                    return Err(CoreError::CapacityExceeded {
                        what: "dir_size walk",
                        limit: cap,
                    });
                }
                last = entry.file_name().to_string();
                let Ok(child) = dir.join(entry.file_name()) else {
                    continue;
                };
                if entry.kind().is_dir() {
                    stack.push(child);
                    continue;
                }
                // A file that will not stat is skipped entirely — no bytes, no count — as
                // `api/dir.go:388-392` does, rather than becoming a phantom zero.
                let Ok(info) = self.source.lstat(child.as_path()) else {
                    continue;
                };
                bytes = bytes.saturating_add(u64::try_from(info.size()).unwrap_or(u64::MAX));
                files += 1;
                if files.is_multiple_of(opts.progress_every.max(1)) {
                    progress.report(visited, bytes, &last);
                }
            }
        }
        progress.report(visited, bytes, &last);
        let stat = DirSizeStat {
            path: path.clone(),
            bytes,
            files_count: files,
        };
        self.put_size(stat.clone());
        Ok(stat)
    }

    // -------------------------------------------------------------------------------- invalidation

    /// Say that `path`'s contents (or something under or above it) changed.
    ///
    /// `&FsPath`, so an archive member is nameable (plan §5.9). Cached pages go *immediately* — a
    /// coalescing window buys back staleness, which is the one thing a cache must never sell — and one
    /// [`EngineEvent::DirChanged`] is queued.
    pub fn invalidate(&self, path: &FsPath, cause: ChangeCause, origin: Option<Origin>) {
        let mut inner = lock(&self.inner);
        inner.pages.retain_where(|_, value| {
            let root = &value.store_root;
            *root == *path || root.starts_with(path) || path.starts_with(root)
        });
        let token = path.dir_token().to_string();
        let entry = inner.pending.entry(token).or_insert_with(|| PendingChange {
            path: path.clone(),
            first_seen: Instant::now(),
            count: 0,
            cause,
            origin,
        });
        entry.count += 1;
        // The most recent cause wins for the coalesced event: a watch-driven change that a local job
        // then repeated is a `LocalJob` change as far as the pane's spinner bookkeeping cares.
        if cause == ChangeCause::LocalJob {
            entry.cause = cause;
            entry.origin = origin;
        }
    }

    /// Release every invalidation whose coalescing window has elapsed: at most one
    /// [`EngineEvent::DirChanged`] per distinct path, however many invalidations that path received
    /// (plan F2 — a 2 000-file paste must not be 2 000 repaints).
    ///
    /// An invalidation younger than [`Self::coalesce_window`] is *held*, because the repeats that make a
    /// storm arrive inside one tick; releasing nothing for 50 ms and one event afterwards is the whole
    /// coalescing mechanism. [`Self::flush_invalidations`] is the end-of-job form that never holds.
    #[must_use]
    pub fn take_invalidations(&self) -> Vec<EngineEvent> {
        self.take_invalidations_at(Instant::now())
    }

    /// [`Self::take_invalidations`] with an injectable clock, so the window is testable without sleeping.
    #[must_use]
    pub fn take_invalidations_at(&self, now: Instant) -> Vec<EngineEvent> {
        let mut inner = lock(&self.inner);
        let window = inner.coalesce;
        let all = std::mem::take(&mut inner.pending);
        let mut held = BTreeMap::new();
        let mut out = Vec::new();
        for (token, change) in all {
            if now.duration_since(change.first_seen) >= window {
                out.push(EngineEvent::DirChanged {
                    path: change.path,
                    cause: change.cause,
                    origin: change.origin,
                });
            } else {
                held.insert(token, change);
            }
        }
        inner.pending = held;
        out
    }

    /// Release *everything*, ignoring the window: the shutdown and end-of-batch path.
    #[must_use]
    pub fn flush_invalidations(&self) -> Vec<EngineEvent> {
        let mut inner = lock(&self.inner);
        std::mem::take(&mut inner.pending)
            .values()
            .map(|p| EngineEvent::DirChanged {
                path: p.path.clone(),
                cause: p.cause,
                origin: p.origin,
            })
            .collect()
    }

    /// Anything waiting to be announced? `Session::wants_repaint` keeps the frame loop alive on this
    /// (plan F14: a held event is still an event on its way).
    #[must_use]
    pub fn has_pending_invalidations(&self) -> bool {
        !lock(&self.inner).pending.is_empty()
    }

    /// How many *distinct* paths are waiting to be announced, and how many invalidations those
    /// represent: the coalescing ratio, for a debug overlay.
    #[must_use]
    pub fn pending_invalidations(&self) -> (usize, u32) {
        let inner = lock(&self.inner);
        let raw = inner.pending.values().map(|p| p.count).sum();
        (inner.pending.len(), raw)
    }

    /// The coalescing window in force.
    #[must_use]
    pub fn coalesce_window(&self) -> Duration {
        lock(&self.inner).coalesce
    }

    /// Force every cached page to be stale: the `F5`/Refresh bypass, and the way the expiry test avoids
    /// sleeping for a TTL.
    pub fn expire_pages(&self) {
        let mut inner = lock(&self.inner);
        let keys: Vec<String> = inner.pages.iter().map(|(k, _)| k.clone()).collect();
        let past = Instant::now() - Duration::from_secs(1);
        for key in keys {
            if let Some(v) = inner.pages.get_mut(&key) {
                v.invalidate_at = past;
            }
        }
    }

    /// Discard every cached page (an `F5`/Refresh bypass, or a store-mode change).
    pub fn clear_page_cache(&self) -> usize {
        lock(&self.inner).pages.clear()
    }

    /// Cache counters, for the diagnostics pane.
    #[must_use]
    pub fn cache_stats(&self) -> DirCacheStats {
        let inner = lock(&self.inner);
        let (hits, misses) = inner.pages.counts();
        DirCacheStats {
            pages: inner.pages.len(),
            sizes: inner.sizes.len(),
            hits,
            misses,
        }
    }
}

/// The directory-cache counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirCacheStats {
    /// Cached pages.
    pub pages: usize,
    /// Cached size rows.
    pub sizes: usize,
    /// Cache hits served.
    pub hits: u64,
    /// Reads the cache could not serve.
    pub misses: u64,
}

/// One pre-page row: the readdir half plus the optional stat half.
#[derive(Clone, Debug)]
struct Row {
    dirent: DirEntryLike,
    info: Option<InfoEntry>,
}

impl Row {
    fn is_dir(&self) -> bool {
        self.dirent.kind().is_dir()
    }
}

/// The listing must be a filesystem path to be read here; an archive path belongs to the archive
/// service, and this is plan §4.2's "one remaining `match` on `FsPath::Archive` in the dispatcher".
fn require_local(path: &FsPath) -> Result<AbsPath, CoreError> {
    if path.store.is_archive() {
        return Err(CoreError::VfsReadOnly {
            path: path.to_string(),
        });
    }
    path.host_abs().ok_or_else(|| CoreError::InvalidPath {
        raw: path.to_string(),
        reason: crate::error::PathReject::Relative,
    })
}

/// Go's `sortEntries` (`api/dir.go:286-320`): dirs first always, then the column.
///
/// Two documented differences from Go, both plan §6.2's kind of fix: the tiebreak on the raw name
/// makes the order deterministic (Go's `sort.Slice` is *unstable*, so equal sizes have no defined order
/// at all and a page boundary could show the same file twice), and reversing for `descending` reverses
/// **within** each class so the dirs-first invariant survives both directions
/// (`dir-sort-date-desc.json`).
fn sort_rows(rows: &mut Vec<Row>, sort: SortSpec) {
    rows.sort_by(|a, b| {
        a.is_dir()
            .cmp(&b.is_dir())
            .reverse()
            .then_with(|| match sort.column {
                SortColumn::Size => size_of(a).cmp(&size_of(b)),
                SortColumn::Date => mtime_of(a).cmp(&mtime_of(b)),
                SortColumn::Name | SortColumn::Mode | SortColumn::Ext => a
                    .dirent
                    .file_name()
                    .to_lowercase()
                    .cmp(&b.dirent.file_name().to_lowercase()),
            })
    });
    if sort.descending {
        // Reverse *within* each class, so dirs-first survives both directions exactly as Go's reversed
        // comparison does (`dir-sort-date-desc.json` starts with the directory too).
        let (mut dirs, mut files): (Vec<Row>, Vec<Row>) =
            rows.iter().cloned().partition(Row::is_dir);
        dirs.reverse();
        files.reverse();
        rows.clear();
        rows.extend(dirs);
        rows.extend(files);
    }
}

fn size_of(row: &Row) -> i64 {
    row.info.as_ref().map_or(0, EntryLike::size)
}

fn mtime_of(row: &Row) -> i64 {
    row.info
        .as_ref()
        .and_then(|i| i.mtime_secs())
        .unwrap_or(GO_ZERO_TIME_UNIX)
}

fn tier_index(tier: DirCacheTier) -> usize {
    match tier {
        DirCacheTier::Small => 0,
        DirCacheTier::Mid => 1,
        DirCacheTier::Big => 2,
    }
}

/// Lock, ignoring poisoning: every field behind it is either fully updated or untouched, so one bad
/// job must not disable the cache for the session.
fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// An in-memory source: the seam's whole point is that these tests never touch a disk. Interior
    /// mutable because the service holds the `Arc` while a test is still filling it in.
    #[derive(Default)]
    struct Fake {
        dirs: Mutex<BTreeMap<String, Vec<DirEntryLike>>>,
        stats: Mutex<BTreeMap<String, InfoEntry>>,
        reads: std::sync::atomic::AtomicU64,
    }

    impl Fake {
        fn new() -> Self {
            Self::default()
        }

        /// Add a directory and its contents (the entries are what `read_dir` will answer).
        fn add_dir(&self, path: &str, entries: Vec<DirEntryLike>) {
            self.dirs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(path.to_string(), entries);
        }

        /// Add a stat answer.
        fn add_stat(&self, path: &str, info: InfoEntry) {
            self.stats
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(path.to_string(), info);
        }

        /// A directory that stats as a directory, with `entries` inside it.
        fn add_tree(&self, path: &str, entries: Vec<DirEntryLike>) {
            self.add_stat(
                path,
                InfoEntry::new(path, EntryKind::Dir, 40, Some(1), Some(0o040755)),
            );
            self.add_dir(path, entries);
        }
    }

    impl FsSource for Fake {
        fn read_dir(&self, path: &AbsPath) -> Result<Vec<DirEntryLike>, CoreError> {
            let _ = self.reads.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            let guard = self
                .dirs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard
                .get(&path.to_string_lossy())
                .cloned()
                .ok_or_else(|| CoreError::NotFound {
                    path: path.to_string_lossy(),
                    source_path: None,
                })
        }

        fn lstat(&self, path: &Path) -> Result<InfoEntry, CoreError> {
            let guard = self
                .stats
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard
                .get(&path.display().to_string())
                .cloned()
                .ok_or_else(|| CoreError::NotFound {
                    path: path.display().to_string(),
                    source_path: None,
                })
        }
    }

    fn file(name: &str, size: i64, mtime: i64) -> (DirEntryLike, InfoEntry) {
        let dirent = DirEntryLike::new(name, EntryKind::File);
        let info = InfoEntry::new(name, EntryKind::File, size, Some(mtime), Some(0o100644));
        (dirent, info)
    }

    fn dir(name: &str) -> DirEntryLike {
        DirEntryLike::new(name, EntryKind::Dir)
    }

    fn svc() -> (DirService, Arc<Fake>) {
        let fake = Arc::new(Fake::new());
        let svc = DirService::new(fake.clone(), Arc::new(Limits::default()));
        (svc, fake)
    }

    fn abs(p: &str) -> AbsPath {
        AbsPath::from_path(p).expect("absolute")
    }

    #[test]
    fn listing_is_sorted_dirs_first_by_lowercased_name() {
        let (svc, fake) = svc();
        fake.add_dir(
            "/d",
            vec![file("b.txt", 10, 1).0, dir("Sub"), file("A.txt", 5, 2).0],
        );
        fake.add_stat(
            "/d",
            InfoEntry::new("d", EntryKind::Dir, 40, Some(1), Some(0o040755)),
        );
        fake.add_stat("/d/b.txt", file("b.txt", 10, 1).1);
        fake.add_stat("/d/A.txt", file("A.txt", 5, 2).1);
        fake.add_stat(
            "/d/Sub",
            InfoEntry::new("Sub", EntryKind::Dir, 60, Some(3), Some(0o040755)),
        );
        let page = svc
            .read_page(&FsPath::fs("/d").unwrap(), None, 10, SortSpec::DEFAULT_SORT)
            .expect("listing");
        let names: Vec<&str> = page.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            ["Sub", "A.txt", "b.txt"],
            "dirs first, then a lowercase compare"
        );
        assert_eq!(page.total_count, 3);
        assert_eq!(page.loaded_so_far, 3);
        assert!(!page.has_more);
        assert_eq!(page.cursor_wire(), Cursor::IS_LAST);
    }

    #[test]
    fn descending_keeps_dirs_first_and_pages_advance() {
        let (svc, fake) = svc();
        fake.add_dir(
            "/d",
            vec![
                file("a", 30, 1).0,
                file("b", 10, 2).0,
                file("c", 20, 3).0,
                dir("z"),
            ],
        );
        fake.add_stat(
            "/d",
            InfoEntry::new("d", EntryKind::Dir, 40, Some(1), Some(0o040755)),
        );
        for (entry, info) in [file("a", 30, 1), file("b", 10, 2), file("c", 20, 3)] {
            fake.add_stat(&format!("/d/{}", entry.name), info);
        }
        fake.add_stat(
            "/d/z",
            InfoEntry::new("z", EntryKind::Dir, 40, Some(0), Some(0o040755)),
        );
        let sort = SortSpec {
            column: SortColumn::Size,
            descending: true,
        };
        let page = svc
            .read_page(&FsPath::fs("/d").unwrap(), None, 2, sort)
            .expect("p1");
        assert_eq!(page.entries[0].name, "z", "the dir still leads");
        assert_eq!(page.entries[1].name, "a", "biggest file next");
        assert!(page.has_more);
        assert_eq!(page.cursor, Cursor(2));
        assert_eq!(page.cursor_wire(), "2");
        let p2 = svc
            .read_page(&FsPath::fs("/d").unwrap(), Some(Cursor(2)), 2, sort)
            .expect("p2");
        assert_eq!(p2.entries[0].name, "c");
        assert!(!p2.has_more);
        assert_eq!(p2.cursor_wire(), Cursor::IS_LAST);
    }

    #[test]
    fn a_row_that_cannot_be_statted_degrades_like_go() {
        let (svc, fake) = svc();
        // The readdir saw the name; the stat is refused — the `dir-perm-noexec.json` shape.
        fake.add_dir("/d", vec![dir("sub")]);
        fake.add_stat(
            "/d",
            InfoEntry::new("d", EntryKind::Dir, 40, Some(1), Some(0o040755)),
        );
        let page = svc
            .read_page(&FsPath::fs("/d").unwrap(), None, 10, SortSpec::DEFAULT_SORT)
            .expect("listing");
        let row = &page.entries[0];
        assert_eq!(row.mode, "unknown");
        assert_eq!(row.size, 0);
        assert!(
            row.is_dir,
            "the kind came from the readdir, not the failed stat"
        );
        // The typed form says "no mtime"; the *wire* form is Go's zero time (`dir-perm-noexec.json`'s
        // `mod_time: -62135596800`), which is what `contract_dirs` compares.
        assert!(row.mod_time.is_none());
        assert_eq!(
            crate::entry::datetime_to_unix(row.mod_time),
            GO_ZERO_TIME_UNIX
        );
    }

    #[test]
    fn the_second_page_hit_reads_the_directory_once() {
        let fake = Arc::new(Fake::new());
        fake.add_dir("/d", vec![file("a", 1, 1).0]);
        fake.add_stat(
            "/d",
            InfoEntry::new("d", EntryKind::Dir, 40, Some(1), Some(0o040755)),
        );
        fake.add_stat("/d/a", file("a", 1, 1).1);
        let counter = Arc::new(CountingSource::new(Arc::clone(&fake) as Arc<dyn FsSource>));
        let svc = DirService::new(
            Arc::clone(&counter) as Arc<dyn FsSource>,
            Arc::new(Limits::default()),
        );
        let req = ListDir::first(FsPath::fs("/d").unwrap());
        let first = svc.page(&req).expect("first read");
        let again = svc.page(&req).expect("cached read");
        assert!(Arc::ptr_eq(&first, &again), "the same Arc is served twice");
        assert_eq!(counter.reads(), 1, "one readdir for two page() calls");
        assert_eq!(svc.cache_stats().hits, 1);
        assert_eq!(svc.cache_stats().misses, 1);
        // An invalidation forces the re-read.
        svc.invalidate(&FsPath::fs("/d").unwrap(), ChangeCause::LocalJob, None);
        let _ = svc.page(&req).expect("third read");
        assert_eq!(counter.reads(), 2, "invalidation cleared the entry");
    }

    #[test]
    fn invalidation_drops_the_parent_and_the_subtree_too() {
        let (svc, fake) = svc();
        fake.add_tree("/d", vec![]);
        fake.add_tree("/d/sub", vec![]);
        let root = FsPath::fs("/d").unwrap();
        let sub = FsPath::fs("/d/sub").unwrap();
        svc.page(&ListDir::first(root.clone())).expect("root page");
        svc.page(&ListDir::first(sub.clone())).expect("sub page");
        assert_eq!(svc.cache_stats().pages, 2);
        // A change to a file inside `/d/sub` invalidates the sub listing and its parent's rows.
        svc.invalidate(&sub.join("f.txt").unwrap(), ChangeCause::Watcher, None);
        assert_eq!(
            svc.cache_stats().pages,
            0,
            "both the ancestor and the subtree entries went"
        );
    }

    #[test]
    fn repeated_invalidations_collapse_into_one_event() {
        let (svc, _fake) = svc();
        let p = FsPath::fs("/d").unwrap();
        for _ in 0..50 {
            svc.invalidate(&p, ChangeCause::LocalJob, Some(Origin(3)));
        }
        let (distinct, raw) = svc.pending_invalidations();
        assert_eq!((distinct, raw), (1, 50), "one path, fifty invalidations");
        // Still inside the window: the event is held, because more repeats may still be coming.
        let now = Instant::now();
        assert!(
            svc.take_invalidations_at(now).is_empty(),
            "held for the tick"
        );
        assert!(svc.has_pending_invalidations());
        let events = svc.take_invalidations_at(now + svc.coalesce_window());
        assert_eq!(
            events.len(),
            1,
            "one DirChanged for fifty invalidations (plan F2)"
        );
        assert!(matches!(
            events[0],
            EngineEvent::DirChanged {
                cause: ChangeCause::LocalJob,
                origin: Some(Origin(3)),
                ..
            }
        ));
        assert!(svc.take_invalidations().is_empty(), "the queue drained");
        // And the end-of-batch form releases a change that has not aged yet.
        svc.invalidate(&p, ChangeCause::External, None);
        assert_eq!(svc.flush_invalidations().len(), 1);
    }

    #[test]
    fn the_size_walk_matches_go_exactly_on_a_tiny_tree() {
        let (svc, fake) = svc();
        // root: `a` (10 B), `gone` (unstatable → skipped), `sub/` (file `c`, 5 B), `l` → a directory.
        fake.add_dir(
            "/r",
            vec![
                file("a", 10, 1).0,
                DirEntryLike::new("gone", EntryKind::File),
                dir("sub"),
                DirEntryLike::new("l", EntryKind::SymlinkDir),
            ],
        );
        fake.add_stat(
            "/r",
            InfoEntry::new("r", EntryKind::Dir, 60, Some(1), Some(0o040755)),
        );
        fake.add_tree("/r/sub", vec![file("c", 5, 2).0]);
        fake.add_stat("/r/a", file("a", 10, 1).1);
        fake.add_stat("/r/sub/c", file("c", 5, 2).1);
        fake.add_stat(
            "/r/l",
            InfoEntry::new("l", EntryKind::SymlinkDir, 3, Some(1), Some(0o120777)),
        );
        let stat = svc
            .dir_size(
                &abs("/r"),
                &DirSizeOpts::default(),
                &CancelToken::new(),
                &SilentSink,
            )
            .expect("walk");
        // 10 (a) + 3 (l, counted by its *link* size: Go's `d.Info()` is lstat) + 5 (c); `gone` skipped.
        assert_eq!(stat.bytes, 18);
        assert_eq!(stat.files_count, 3);
        assert_eq!(svc.cached_sizes(&[abs("/r")])[0].as_ref(), Some(&stat));
    }

    /// A source that asks for cancellation itself, at a fixed item: the deterministic version of "the
    /// user pressed cancel while the walk was mid-directory", with no sleeping and no racing threads.
    struct CancellingWalk {
        /// The token the walk under test carries.
        token: CancelToken,
        /// After how many `lstat` calls the cancellation is requested.
        after: u64,
        stats: std::sync::atomic::AtomicU64,
    }

    impl FsSource for CancellingWalk {
        fn read_dir(&self, path: &AbsPath) -> Result<Vec<DirEntryLike>, CoreError> {
            if path.to_string_lossy() != "/r" {
                return Err(CoreError::NotFound {
                    path: path.to_string_lossy(),
                    source_path: None,
                });
            }
            Ok((0..5_000)
                .map(|i| DirEntryLike::new(format!("f{i}"), EntryKind::File))
                .collect())
        }

        fn lstat(&self, path: &Path) -> Result<InfoEntry, CoreError> {
            let n = self.stats.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            if n >= self.after {
                // The request lands mid-walk, and the very next item boundary must stop it.
                self.token.cancel();
            }
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name == "r" {
                return Ok(InfoEntry::new(
                    "r",
                    EntryKind::Dir,
                    40,
                    Some(1),
                    Some(0o040755),
                ));
            }
            Ok(InfoEntry::new(
                name,
                EntryKind::File,
                1,
                Some(1),
                Some(0o100644),
            ))
        }
    }

    #[test]
    fn the_size_walk_cancels_at_an_item_boundary() {
        let token = CancelToken::new();
        let walk = Arc::new(CancellingWalk {
            token: token.clone(),
            after: 100,
            stats: std::sync::atomic::AtomicU64::new(0),
        });
        let svc = DirService::new(
            Arc::clone(&walk) as Arc<dyn FsSource>,
            Arc::new(Limits::default()),
        );
        let sink = RecordingSink::default();
        let opts = DirSizeOpts {
            walk_cap: None,
            progress_every: 32,
        };
        let err = svc
            .dir_size(&abs("/r"), &opts, &token, &sink)
            .expect_err("cancelled");
        assert!(matches!(err, CoreError::Cancelled), "{err:?}");
        // 5 000 entries were offered; the walk stopped at the first boundary after item 100.
        let stopped_at = walk.stats.load(std::sync::atomic::Ordering::Acquire);
        assert!(
            stopped_at < 200,
            "the walk stopped at {stopped_at}, not at 5 000 (plan F3: cancellation at an item boundary)"
        );
        assert!(
            !sink.reports.lock().map(|g| g.is_empty()).unwrap_or(true),
            "progress was reported before the cancellation"
        );
        assert!(
            svc.cached_sizes(&[abs("/r")])[0].is_none(),
            "a cancelled walk caches nothing"
        );
    }

    #[test]
    fn the_walk_cap_reports_instead_of_undercounting() {
        let (svc, fake) = svc();
        fake.add_tree(
            "/r",
            (0..10)
                .map(|i| DirEntryLike::new(format!("f{i}"), EntryKind::File))
                .collect(),
        );
        for i in 0..10 {
            fake.add_stat(
                &format!("/r/f{i}"),
                InfoEntry::new("f", EntryKind::File, 1, Some(1), Some(0o100644)),
            );
        }
        let opts = DirSizeOpts {
            walk_cap: Some(4),
            ..DirSizeOpts::default()
        };
        let err = svc
            .dir_size(&abs("/r"), &opts, &CancelToken::new(), &SilentSink)
            .expect_err("capped");
        assert!(
            matches!(err, CoreError::CapacityExceeded { limit: 4, .. }),
            "{err:?}"
        );
        // And the uncapped default sees all ten (plan step-6 note: the size walk is not the listing cap).
        let stat = svc
            .dir_size(
                &abs("/r"),
                &DirSizeOpts::default(),
                &CancelToken::new(),
                &SilentSink,
            )
            .expect("uncapped");
        assert_eq!((stat.bytes, stat.files_count), (10, 10));
    }

    #[test]
    fn archive_paths_are_refused_not_mis_read() {
        let (svc, _fake) = svc();
        let arc = FsPath::archive(abs("/a.zip"), None, "inner").unwrap();
        let err = svc
            .read_page(&arc, None, 10, SortSpec::DEFAULT_SORT)
            .expect_err("archive");
        assert!(matches!(err, CoreError::VfsReadOnly { .. }), "{err:?}");
    }

    #[test]
    fn bounded_cache_evicts_the_least_recently_used() {
        let mut c = BoundedCache::<u32, String>::new(2);
        c.insert(1, "a".into());
        c.insert(2, "b".into());
        let _ = c.get_mut(&1);
        c.insert(3, "c".into());
        assert_eq!(c.len(), 2);
        assert!(c.peek(&1).is_some(), "1 was used most recently");
        assert!(c.peek(&2).is_none(), "2 is the LRU casualty");
        assert_eq!(c.counts(), (1, 0));
    }

    #[test]
    fn a_stale_entry_is_dropped_not_served() {
        let (svc, fake) = svc();
        fake.add_tree("/d", vec![]);
        let req = ListDir::first(FsPath::fs("/d").unwrap());
        svc.page(&req).expect("first");
        assert!(svc.cached_page(&req).is_some(), "fresh entry is served");
        svc.expire_pages();
        assert!(
            svc.cached_page(&req).is_none(),
            "the expired entry is not served"
        );
    }

    #[test]
    fn size_cache_is_keyed_canonically() {
        let (svc, _fake) = svc();
        svc.put_size(DirSizeStat {
            path: abs("/a/b"),
            bytes: 1,
            files_count: 2,
        });
        // The same directory through a path that needed cleaning: one row, not two.
        let same = AbsPath::from_path("/a/./b").expect("cleans to /a/b");
        assert_eq!(
            svc.cached_sizes(&[same])[0].as_ref().map(|s| s.bytes),
            Some(1)
        );
    }
}
