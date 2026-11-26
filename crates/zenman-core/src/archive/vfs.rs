//! The archive manifest and the read-only store behind a pane (plan §5.5's `archive/vfs.rs` row).
//!
//! The manifest is the in-memory directory tree of one archive: `BTreeMap` keyed by cleaned internal
//! path, with the synthesised parent directories Go synthesised (`api/archive_parser.go:110-160`). It is
//! what makes a member addressable by [`FsPath`](crate::path::FsPath) — the same shape a real directory
//! listing produces — which is why the pane needs no archive-specific branch to show one.
//!
//! ## The `./` bug, fixed
//!
//! Go's `cleanInternalPath` (`api/archive_parser.go:32-35`) was `ToSlash` + `strings.Trim(p, "/")`, so a
//! member stored as `./x/y.txt` kept its leading `./` and `synthesizeDirectories` then produced a directory
//! node literally named `.` (`api/archive_parser.go:110-140`). Plan §5.5 lists that as a bug-for-bug
//! exception: [`clean_internal_path`] drops `.` components, and
//! [`tests::a_dot_prefixed_member_does_not_synthesise_a_dot_directory`] pins it.
//!
//! ## What is deliberately *not* fixed here
//!
//! A zip-slip member (`../escape.txt`) keeps its node — Go listed it
//! (`tests/golden/archive-list-zipslip.json`), and a listing is harmless. The refusal belongs to the one
//! path that can write, [`crate::archive::extract`].

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, TimeZone, Utc};

use crate::archive::reader::{ArchiveReader, RawEntry, open, open_bytes};
use crate::entry::FileEntry;
use crate::error::CoreError;
use crate::limits::{ARCHIVE_CACHE_ENTRIES, ARCHIVE_CACHE_TTL, Limits};
use crate::path::{AbsPath, FsPath, VfsKind};

/// The mode every archive member reports: Go's hard-coded read-only cue
/// (`api/archive_vfs.go:28` `Mode: "-r--r--r--"`), which is also the honest one — the VFS is read-only
/// (`ops::guard::vfs_write_guard`).
pub const ARCHIVE_MEMBER_MODE: &str = "-r--r--r--";

/// Manifest entry cap. Beyond this a container is a bomb, not a directory (`limits.rs`'s sibling to
/// [`Limits::nested_archive_max_bytes`]).
pub const MAX_MANIFEST_ENTRIES: usize = 100_000;

/// Go's `cleanInternalPath` (`api/archive_parser.go:32`), with the `.`-component fix described above.
///
/// Note what it does *not* do: a `..` component survives, because a listing that hid it would disagree
/// with the golden capture of the same archive.
#[must_use]
pub fn clean_internal_path(p: &str) -> String {
    p.split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// One node of the manifest. Field-for-field Go's `VirtualNode` (`api/archive_vfs.go:13-20`), with
/// `mod_time` as Unix seconds rather than a `time.Time` so it crosses the wire unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualNode {
    /// The last path component.
    pub name: String,
    /// Directory node (real or synthesised).
    pub is_dir: bool,
    /// Uncompressed size; 0 for a directory.
    pub size: i64,
    /// Modification time, Unix seconds.
    pub mod_time: i64,
    /// The cleaned internal path — the node's key, and the `rel_path` of its row.
    pub internal_path: String,
    /// Child keys, sorted (Go appended them in map order; sorted is deterministic, which the pane needs).
    pub children: Vec<String>,
    /// The member's payload is encrypted — **not** part of Go's node (`api/archive_vfs.go:13-20`), added
    /// because the properties dialog has to warn before a read fails with
    /// [`CoreError::ArchiveEncrypted`] rather than after.
    pub encrypted: bool,
}

impl VirtualNode {
    /// The row the pane paints: Go's `ToFileEntry` (`api/archive_vfs.go:23-31`), including its
    /// `rel_path = internal_path` and the read-only mode string.
    #[must_use]
    pub fn to_entry(&self) -> FileEntry {
        FileEntry {
            name: self.name.clone(),
            is_dir: self.is_dir,
            is_symlink: false,
            size: self.size,
            mod_time: unix_to_datetime(self.mod_time),
            mode: ARCHIVE_MEMBER_MODE.to_string(),
            rel_path: self.internal_path.clone(),
            depth: 0,
            files_count: None,
        }
    }

    /// The parent key (`""` for a top-level node).
    #[must_use]
    pub fn parent_key(&self) -> &str {
        match self.internal_path.rfind('/') {
            Some(i) => &self.internal_path[..i],
            None => "",
        }
    }
}

fn unix_to_datetime(secs: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(secs, 0).single()
}

/// The tree: nodes keyed by cleaned internal path. The store root is the implicit `""` key, which is why
/// there is no root node in the map — Go kept one (`api/archive_vfs.go:161-175`); a pane that lists `""`
/// asks [`Manifest::children_of`] instead, and gets the same answer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    nodes: BTreeMap<String, VirtualNode>,
}

impl Manifest {
    /// One node.
    #[must_use]
    pub fn get(&self, internal_path: &str) -> Option<&VirtualNode> {
        self.nodes.get(internal_path)
    }

    /// The rows of one directory (`""` = the archive root), sorted by name.
    #[must_use]
    pub fn children_of(&self, dir: &str) -> Vec<&VirtualNode> {
        let want = clean_internal_path(dir);
        let mut out: Vec<&VirtualNode> = self
            .nodes
            .values()
            .filter(|n| clean_internal_path(n.parent_key()) == want && n.internal_path != want)
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Every node, in path order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &VirtualNode)> {
        self.nodes.iter()
    }

    /// One member's subtree — the node itself, then everything under it, in path order.
    ///
    /// A range read rather than a scan of [`Self::iter`], because the caller that wants one folder out
    /// of a 200 000-node container must not pay for the container: a paste of N members costs N
    /// subtree walks, not N full ones. Keys are cleaned and `/`-separated, so `source` and each of its
    /// descendants sort between `source` and `source` + the highest scalar value; a sibling named
    /// `source-x` falls inside that range and the exact prefix test is what turns it away. An empty
    /// `source` (the store root, which has no node) walks nothing.
    pub fn subtree(&self, source: &str) -> impl Iterator<Item = (&String, &VirtualNode)> {
        let want = clean_internal_path(source);
        let prefix = format!("{want}/");
        self.nodes
            .range(want.clone()..=format!("{want}\u{10ffff}"))
            .filter(move |(key, _)| key.as_str() == want || key.starts_with(&prefix))
    }

    /// Node count (files and directories).
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// `true` when the container holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Sum of the file sizes — the extract job's `bytes_total`.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.nodes
            .values()
            .filter(|n| !n.is_dir)
            .map(|n| u64::try_from(n.size.max(0)).unwrap_or(0))
            .sum()
    }
}

/// Build the manifest of an opened container: list, clean, synthesise the missing parents, wire the
/// child lists.
///
/// # Errors
/// [`CoreError::CapacityExceeded`] when the member count or the uncompressed total exceeds the plan's
/// caps (F8), or whatever `list` reported.
pub fn build_manifest<R: ArchiveReader + ?Sized>(
    r: &mut R,
    limits: &Limits,
) -> Result<Manifest, CoreError> {
    let entries: Vec<RawEntry> = r.list()?;
    if entries.len() > MAX_MANIFEST_ENTRIES {
        return Err(CoreError::CapacityExceeded {
            what: "archive entries",
            limit: MAX_MANIFEST_ENTRIES,
        });
    }
    let cap = limits.nested_archive_max_bytes;
    let total: u64 = entries.iter().map(|e| e.size).sum();
    if total > cap {
        return Err(CoreError::CapacityExceeded {
            what: "archive bytes",
            limit: usize::try_from(cap).unwrap_or(usize::MAX),
        });
    }

    let mut nodes: BTreeMap<String, VirtualNode> = BTreeMap::new();
    for e in entries {
        let path = clean_internal_path(&e.path);
        if path.is_empty() {
            continue;
        }
        nodes.insert(
            path.clone(),
            VirtualNode {
                name: path.rsplit('/').next().unwrap_or(&path).to_string(),
                is_dir: e.is_dir,
                size: i64::try_from(e.size).unwrap_or(i64::MAX),
                mod_time: e.mtime,
                internal_path: path,
                children: Vec::new(),
                encrypted: e.encrypted,
            },
        );
    }

    // Synthesised parents (Go's `synthesizeDirectories`, one pass per level rather than its
    // repeat-until-stable loop): a member `a/b/c.txt` implies the nodes `a` and `a/b`.
    let leaf_paths: Vec<String> = nodes.keys().cloned().collect();
    for path in leaf_paths {
        let mut parts: Vec<&str> = path.split('/').collect();
        while parts.len() > 1 {
            parts.truncate(parts.len() - 1);
            let parent = parts.join("/");
            if nodes.contains_key(&parent) {
                break;
            }
            let mtime = nodes.get(path.as_str()).map_or(0, |n| n.mod_time).max(0);
            nodes.insert(
                parent.clone(),
                VirtualNode {
                    name: parts[parts.len() - 1].to_string(),
                    is_dir: true,
                    size: 0,
                    mod_time: mtime,
                    internal_path: parent,
                    children: Vec::new(),
                    encrypted: false,
                },
            );
        }
    }

    // Child lists, in path order (deterministic; Go appended while iterating a map).
    let keys: Vec<String> = nodes.keys().cloned().collect();
    for key in keys {
        let parent = nodes
            .get(&key)
            .map(|n| clean_internal_path(n.parent_key()))
            .unwrap_or_default();
        if parent == key {
            continue;
        }
        if let Some(p) = nodes.get_mut(&parent)
            && !p.children.contains(&key)
        {
            p.children.push(key);
        }
    }

    Ok(Manifest { nodes })
}

/// The cache key: `clean(host)` + `"::" + trimmed(nested)` (plan §5.5). The separator is the reason two
/// archives with the same basename in different directories cannot collide, and the reason a nested
/// store has its own entry (`api/archive_vfs.go:163-176` `BuildCacheKey`).
#[must_use]
pub fn build_cache_key(host: &AbsPath, nested: Option<&str>) -> String {
    match nested {
        None | Some("") => host.to_string(),
        Some(n) => format!("{}::{}", host.to_string_lossy(), clean_internal_path(n)),
    }
}

struct Slot {
    manifest: Arc<Manifest>,
    host_mtime: SystemTime,
    last_access: Instant,
    seq: u64,
}

/// The LRU manifest cache (plan §5.5: capacity [`ARCHIVE_CACHE_ENTRIES`], TTL [`ARCHIVE_CACHE_TTL`]).
///
/// Go ran a 5-minute eviction goroutine (`api/archive_vfs.go:52-77`); a library that spawns a thread per
/// process would be a surprise, so [`Self::evict_stale`] is a plain call the session's poll tick makes.
#[derive(Clone)]
pub struct ManifestCache {
    cap: usize,
    ttl: Duration,
    clock: Arc<std::sync::atomic::AtomicU64>,
    inner: Arc<Mutex<BTreeMap<String, Slot>>>,
}

impl Default for ManifestCache {
    fn default() -> Self {
        Self::new(ARCHIVE_CACHE_ENTRIES, ARCHIVE_CACHE_TTL)
    }
}

impl ManifestCache {
    /// A cache with the plan's defaults replaced by the caller's numbers.
    #[must_use]
    pub fn new(cap: usize, ttl: Duration) -> Self {
        Self {
            cap: cap.max(1),
            ttl,
            clock: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            inner: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Cached manifests right now.
    #[must_use]
    pub fn len(&self) -> usize {
        self.locked().len()
    }

    /// `true` when nothing is cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drop everything (a test seam, and what `ArchiveModified` does to the stale entry itself).
    pub fn clear(&self) {
        self.locked().clear();
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Slot>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Drop entries idle for longer than the TTL (Go's `evictStale`, `api/archive_vfs.go:59-77`).
    pub fn evict_stale(&self) {
        let now = Instant::now();
        let ttl = self.ttl;
        self.locked()
            .retain(|_, s| now.duration_since(s.last_access) <= ttl);
    }

    /// The manifest of `host` (`nested` = the inner store), cached and re-verified.
    ///
    /// # Errors
    /// [`CoreError::ArchiveModified`] when the host's mtime moved since the cached manifest was built —
    /// plan F7, Go's HTTP 409 — plus whatever opening the container reports.
    pub fn open_and_verify(
        &self,
        host: &AbsPath,
        nested: Option<&str>,
        limits: &Limits,
    ) -> Result<Arc<Manifest>, CoreError> {
        let key = build_cache_key(host, nested);
        let mtime = host_mtime(host)?;
        {
            let mut guard = self.locked();
            if let Some(slot) = guard.get_mut(&key) {
                if slot.host_mtime != mtime {
                    // Stale: drop it and say so, rather than streaming the wrong bytes (plan F7).
                    guard.remove(&key);
                    return Err(CoreError::ArchiveModified {
                        archive: host.to_string(),
                    });
                }
                slot.last_access = Instant::now();
                slot.seq = self
                    .clock
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Ok(Arc::clone(&slot.manifest));
            }
        }
        let mut reader = open_store(host, nested, limits.nested_archive_max_bytes)?;
        let manifest = Arc::new(build_manifest(reader.as_mut(), limits)?);
        let mut guard = self.locked();
        guard.insert(
            key.clone(),
            Slot {
                manifest: Arc::clone(&manifest),
                host_mtime: mtime,
                last_access: Instant::now(),
                seq: self
                    .clock
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            },
        );
        if guard.len() > self.cap {
            // LRU by last access (Go tracked this with an ordered `manifestKeys` slice; a scan is the same
            // thing over a bounded map).
            let victim = guard
                .iter()
                .min_by_key(|(_, s)| s.seq)
                .map(|(k, _)| k.clone());
            if let Some(v) = victim {
                guard.remove(&v);
            }
        }
        Ok(manifest)
    }
}

fn host_mtime(host: &AbsPath) -> Result<SystemTime, CoreError> {
    std::fs::metadata(host.as_path())
        .and_then(|m| m.modified())
        .map_err(|e| CoreError::Io {
            path: host.to_string(),
            io: Arc::new(e),
        })
}

/// Open a store — the host file, or a member of it when `nested` names an inner archive.
///
/// The nested case reads exactly one member (bounded by `cap`) and opens it in memory: that is plan
/// §5.5's `read_entry` rationale, and the reason a nested store costs the inner archive's size, never the
/// outer one's.
///
/// # Errors
/// `UnsupportedArchive` for an extension the name does not claim, `Unsupported` for RAR (plan §10 item
/// 3), plus the open errors.
pub fn open_store(
    host: &AbsPath,
    nested: Option<&str>,
    cap: u64,
) -> Result<Box<dyn ArchiveReader>, CoreError> {
    let name = host.to_string();
    let fmt =
        crate::archive::detect(host.as_path()).ok_or_else(|| CoreError::UnsupportedArchive {
            archive: name.clone(),
        })?;
    match nested {
        None => {
            let size = std::fs::metadata(host.as_path())
                .map_err(|e| CoreError::Io {
                    path: name.clone(),
                    io: Arc::new(e),
                })?
                .len();
            let file = std::fs::File::open(host.as_path()).map_err(|e| CoreError::Io {
                path: name.clone(),
                io: Arc::new(e),
            })?;
            open(fmt, file, size, &name)
        }
        Some(inner) => {
            let inner = clean_internal_path(inner);
            let mut outer = open_store(host, None, cap)?;
            let bytes = outer.read_to_vec(&inner, cap)?;
            let inner_fmt = crate::archive::detect_name(&inner).ok_or_else(|| {
                CoreError::UnsupportedArchive {
                    archive: format!("{name}{inner}"),
                }
            })?;
            open_bytes(inner_fmt, bytes, &format!("{name}::{inner}"))
        }
    }
}

/// The read-only, archive-shaped sibling of [`crate::services::dirs::FsSource`]: everything a pane or
/// the properties dialog needs from a store, addressed by [`FsPath`] and nothing else.
///
/// The point of the type is that the *store* is the only archive-aware thing in the pipeline: a listing
/// comes back as plain [`FileEntry`] rows whose `rel_path` is the internal path, so sorting, caching and
/// row painting never ask whether they are looking at a zip.
pub struct ArchiveStore {
    cache: ManifestCache,
    limits: Arc<Limits>,
}

impl ArchiveStore {
    /// A store over the caller's cache and limits.
    #[must_use]
    pub fn new(cache: ManifestCache, limits: Arc<Limits>) -> Self {
        Self { cache, limits }
    }

    /// The manifest cache (the extract job and the tests both need it).
    #[must_use]
    pub fn cache(&self) -> &ManifestCache {
        &self.cache
    }

    /// The limits this store enforces (the extract service and the session façade both read them).
    #[must_use]
    pub fn limits(&self) -> &Arc<Limits> {
        &self.limits
    }

    /// The manifest of the store `path` lives in.
    ///
    /// # Errors
    /// `UnsupportedArchive` for an `Fs`-store path (a plain directory is not an archive store).
    pub fn manifest_of(&self, store: &VfsKind) -> Result<Arc<Manifest>, CoreError> {
        match store {
            VfsKind::Fs => Err(CoreError::UnsupportedArchive {
                archive: "the filesystem".to_string(),
            }),
            VfsKind::Archive { host, nested } => {
                self.cache
                    .open_and_verify(host, nested.as_deref(), &self.limits)
            }
        }
    }

    /// The rows of one archive directory.
    pub fn list(&self, path: &FsPath) -> Result<Vec<FileEntry>, CoreError> {
        let manifest = self.manifest_of(&path.store)?;
        let dir = clean_internal_path(&path.relative);
        if !dir.is_empty() {
            // Go answered this case with a typed 500 (`err-archive-baddir.json`): a member is not a
            // directory, and an absent name is not one either. An empty *page* would be a lie.
            let known =
                manifest
                    .get(&dir)
                    .filter(|n| n.is_dir)
                    .ok_or_else(|| CoreError::NotFound {
                        path: path.to_string(),
                        source_path: path.host_abs().map(|h| h.to_string()),
                    })?;
            let _ = known;
        }
        Ok(manifest
            .children_of(&path.relative)
            .into_iter()
            .map(VirtualNode::to_entry)
            .collect())
    }

    /// One member's bytes, capped by the session's archive limit.
    pub fn read(&self, path: &FsPath) -> Result<Vec<u8>, CoreError> {
        let VfsKind::Archive { host, nested } = &path.store else {
            return Err(CoreError::UnsupportedArchive {
                archive: path.to_string(),
            });
        };
        let inner = path.relative.clone();
        if inner.is_empty() {
            // The store root has no bytes: it is a directory, and asking to read it is a missing member.
            return Err(CoreError::NotFound {
                path: path.to_string(),
                source_path: Some(host.to_string()),
            });
        }
        let mut reader = open_store(
            host,
            nested.as_deref(),
            self.limits.nested_archive_max_bytes,
        )?;
        reader.read_to_vec(&inner, self.limits.nested_archive_max_bytes)
    }

    /// Stream one member into `sink` (the pane's preview/`open with` path — no whole-member buffer).
    pub fn stream(&self, path: &FsPath, sink: &mut dyn std::io::Write) -> Result<u64, CoreError> {
        let VfsKind::Archive { host, nested } = &path.store else {
            return Err(CoreError::UnsupportedArchive {
                archive: path.to_string(),
            });
        };
        let inner = path.relative.clone();
        let mut reader = open_store(
            host,
            nested.as_deref(),
            self.limits.nested_archive_max_bytes,
        )?;
        reader.write_stream(&inner, sink, self.limits.nested_archive_max_bytes)
    }

    /// One member's row (`props` for the properties dialog).
    pub fn stat(&self, path: &FsPath) -> Result<FileEntry, CoreError> {
        let manifest = self.manifest_of(&path.store)?;
        manifest
            .get(&clean_internal_path(&path.relative))
            .map(VirtualNode::to_entry)
            .ok_or_else(|| CoreError::NotFound {
                path: path.to_string(),
                source_path: path.host_abs().map(|h| h.to_string()),
            })
    }

    /// The store's host file, when `path` is in one.
    #[must_use]
    pub fn host_of(path: &FsPath) -> Option<&AbsPath> {
        path.store.host()
    }

    /// Whether a path is inside an archive at all (13b's branch predicate: `Alt+Enter` on such a row
    /// opens the archive properties, not the chmod page).
    #[must_use]
    pub fn is_archive_path(path: &FsPath) -> bool {
        path.store.is_archive()
    }
}

/// The clipboard's read-only view of this store: [`stat`] and [`list`] are exactly the two facts
/// [`store`](crate::ops::clipboard::store) needs to name, size and expand a member queue without
/// ever naming `Session` (the seam lives next to its user, the impl next to its subject).
///
/// [`stat`]: ArchiveStore::stat
/// [`list`]: ArchiveStore::list
impl crate::ops::clipboard::MemberFacts for ArchiveStore {
    fn stat_member(&self, path: &FsPath) -> Result<FileEntry, CoreError> {
        self.stat(path)
    }

    fn list_members(&self, path: &FsPath) -> Result<Vec<FileEntry>, CoreError> {
        self.list(path)
    }
}

/// The `FsPath` of one member of a store — the constructor the pane uses after a row activation.
#[must_use]
pub fn member_path(host: &AbsPath, nested: Option<&str>, inner: &str) -> FsPath {
    FsPath {
        store: VfsKind::Archive {
            host: host.clone(),
            nested: nested.map(str::to_string),
        },
        relative: clean_internal_path(inner),
    }
}

/// Convenience for the callers that think in `Path`s (the contract tests, the CLI).
pub fn list_path(host: &Path, limits: &Limits) -> Result<Manifest, CoreError> {
    let abs = AbsPath::from_path(host.to_path_buf())?;
    let mut reader = open_store(&abs, None, limits.nested_archive_max_bytes)?;
    build_manifest(reader.as_mut(), limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_internal_path_drops_the_dot_that_go_kept() {
        assert_eq!(clean_internal_path("./a/b.txt"), "a/b.txt");
        assert_eq!(clean_internal_path("a/b/"), "a/b");
        assert_eq!(clean_internal_path("/lead/a.txt"), "lead/a.txt");
        assert_eq!(clean_internal_path("a//b"), "a/b");
        // The zip-slip member keeps its `..`: a listing must not lie about what the container holds.
        assert_eq!(clean_internal_path("../escape.txt"), "../escape.txt");
    }

    /// The range read `Manifest::subtree` is for: a member's own subtree, and *only* it — the
    /// lexicographic range also contains the sibling prefixes, which the exact test must turn away.
    #[test]
    fn subtree_walks_the_member_and_its_children_only() {
        let mut r = FakeReader::new(vec![
            entry("inner/a.txt", false, 3),
            entry("inner/deep/b.txt", false, 4),
            entry("inner-backup/x.txt", false, 1),
            entry("innertext.txt", false, 1),
            entry("top.txt", false, 1),
        ]);
        let m = build_manifest(&mut r, &Limits::default()).expect("manifest");
        let keys: Vec<String> = m.subtree("inner").map(|(k, _)| k.clone()).collect();
        assert_eq!(
            keys,
            ["inner", "inner/a.txt", "inner/deep", "inner/deep/b.txt"],
            "the synthesised directory nodes are part of the subtree"
        );
        assert_eq!(
            m.subtree("top.txt")
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>(),
            ["top.txt"],
            "a file is its own subtree"
        );
        assert_eq!(
            m.subtree("").count(),
            0,
            "the store root has no node, so it walks nothing rather than everything"
        );
        assert_eq!(
            m.subtree("gone").count(),
            0,
            "an absent member walks nothing"
        );
    }

    #[test]
    fn a_dot_prefixed_member_does_not_synthesise_a_dot_directory() {
        let mut r = FakeReader::new(vec![
            entry("./x/y.txt", false, 3),
            entry("top.txt", false, 1),
        ]);
        let m = build_manifest(&mut r, &Limits::default()).expect("manifest");
        assert!(
            m.get("x/y.txt").is_some(),
            "the member is cleaned, not dot-prefixed"
        );
        assert!(
            m.get(".").is_none(),
            "the Go `.` directory node is not reproduced"
        );
        assert!(m.get("x").is_some_and(|n| n.is_dir));
        assert_eq!(m.get("x").map(|n| n.children.len()), Some(1));
    }

    #[test]
    fn cache_keys_separate_nested_stores_and_same_basenames() {
        let a = AbsPath::from_path("/tree/one.zip").expect("abs");
        let b = AbsPath::from_path("/other/one.zip").expect("abs");
        assert_ne!(build_cache_key(&a, None), build_cache_key(&b, None));
        assert_eq!(
            build_cache_key(&a, Some("in.zip")),
            "one.zip::in.zip".replace("one", "/tree/one")
        );
        assert_ne!(
            build_cache_key(&a, None),
            build_cache_key(&a, Some("in.zip"))
        );
    }

    #[test]
    fn children_of_the_root_are_the_top_level_rows() {
        let mut r = FakeReader::new(vec![
            entry("docs/a.txt", false, 1),
            entry("docs", true, 0),
            entry("root.txt", false, 2),
        ]);
        let m = build_manifest(&mut r, &Limits::default()).expect("manifest");
        let top: Vec<&str> = m.children_of("").iter().map(|n| n.name.as_str()).collect();
        assert_eq!(top, vec!["docs", "root.txt"]);
        assert_eq!(m.children_of("docs").len(), 1);
        assert_eq!(
            m.children_of("/docs/").len(),
            1,
            "the dir argument is cleaned too"
        );
    }

    #[test]
    fn byte_cap_is_a_typed_error_not_an_oom() {
        let mut r = FakeReader::new(vec![entry("big.bin", false, 4096)]);
        let limits = Limits {
            nested_archive_max_bytes: 1024,
            ..Limits::default()
        };
        let err = build_manifest(&mut r, &limits).unwrap_err();
        assert_eq!(err.kind(), "capacity_exceeded");
    }

    #[test]
    fn host_of_and_is_archive_path_agree_with_the_store_type() {
        let host = AbsPath::from_path("/t/a.zip").expect("abs");
        let p = member_path(&host, Some("in.zip"), "deep/f.txt");
        assert!(ArchiveStore::is_archive_path(&p));
        assert_eq!(ArchiveStore::host_of(&p), Some(&host));
        assert_eq!(p.relative, "deep/f.txt");
    }

    // A reader that needs no container: the manifest logic is what these tests are about.
    struct FakeReader {
        entries: Vec<RawEntry>,
    }

    impl FakeReader {
        fn new(entries: Vec<RawEntry>) -> Self {
            Self { entries }
        }
    }

    fn entry(path: &str, is_dir: bool, size: u64) -> RawEntry {
        RawEntry {
            path: path.to_string(),
            is_dir,
            size,
            mtime: 1_767_225_600,
            mode: None,
            encrypted: false,
        }
    }

    impl ArchiveReader for FakeReader {
        fn list(&mut self) -> Result<Vec<RawEntry>, CoreError> {
            Ok(self.entries.clone())
        }

        fn write_stream(
            &mut self,
            inner: &str,
            sink: &mut dyn std::io::Write,
            _cap: u64,
        ) -> Result<u64, CoreError> {
            let want = clean_internal_path(inner);
            if self.entries.iter().any(|e| e.path == want) {
                sink.write_all(b"fake").map_err(CoreError::System)?;
                Ok(4)
            } else {
                Err(CoreError::NotFound {
                    path: want,
                    source_path: None,
                })
            }
        }
    }
}
