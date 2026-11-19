//! The archive service (plan §5.3's `services/archive.rs`, §5.5's four modules seen from the façade).
//!
//! One object per session, holding the two stateful pieces archive browsing has: the manifest cache and
//! the extract job table. The UI never touches `archive/*` directly — it calls these five operations, in
//! the shape the pane already uses for directories.
//!
//! ## Jobs, and what is not one
//!
//! * [`ArchiveService::list`] and [`ArchiveService::props`] are **synchronous**: they read the manifest,
//!   which is the container's central directory (a few kilobytes for a real archive) and is cached, so a
//!   job would cost more than the work. Go's `/api/archive/dir` was synchronous too.
//! * [`ArchiveService::read`] is synchronous *and capped*: it streams one member through
//!   [`Limits::nested_archive_max_bytes`], which is a bound on the answer's size, not on the wait.
//! * [`ArchiveService::extract`] is a **job** ([`JobKind::Extract`](crate::jobs::JobKind::Extract)) —
//!   unbounded work, progress, cancellation, and a destination the pane must reload.
//!
//! ## The index
//!
//! Archive *members* are never indexed, and this is the written decision plan §5.10 asks for: the SQLite
//! `path` column is a unique index over *filesystem* paths
//! ([`AbsPath`](crate::path::AbsPath)), and a member path is not one — it is `(host, internal)`, it has
//! no `parent` row, it disappears when the container is renamed, and 50 000 members of one zip would
//! swamp the 5 000-entry walk cap for rows that no `open` can reach. So the archive layer emits **no
//! [`IndexOp`](crate::index::intent::IndexOp)** for members; extraction emits `Created` for the *files it
//! wrote to the filesystem*, which are real. `contract_archive.rs` pins both halves.

use std::sync::Arc;

use crate::archive::extract::{ExtractJob, ExtractReq, ExtractService};
use crate::archive::vfs::{ArchiveStore, ManifestCache, clean_internal_path};
use crate::entry::FileEntry;
use crate::error::CoreError;
use crate::jobs::{JobId, JobRegistry};
use crate::limits::Limits;
use crate::path::{ArchiveFormat, FsPath, VfsKind};
use crate::session::Origin;

/// What the properties dialog shows for an archive or one of its subtrees (plan §7 row 13: Properties
/// opens on Alt+Enter for **archive entries** too — that is step 13b's job, and this is its payload).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveProps {
    /// The container format, from the host's name (Go parity, see [`crate::archive::detect`]).
    pub format: ArchiveFormat,
    /// The host file's size on disk.
    pub host_bytes: u64,
    /// Nodes in the shown subtree, files and directories.
    pub entries: usize,
    /// Non-directory nodes.
    pub files: usize,
    /// Directory nodes.
    pub dirs: usize,
    /// Sum of the members' uncompressed sizes.
    pub total_bytes: u64,
    /// Members whose payload is encrypted (plan §5.5's `ArchiveEncrypted` preview: the dialog can warn
    /// before the read fails).
    pub encrypted: usize,
    /// Members that are themselves archives — the depth-2 hint.
    pub nested: usize,
    /// Store depth of the shown subtree: 1 = the host, 2 = inside a nested archive.
    pub depth: usize,
}

/// The session's archive facility.
pub struct ArchiveService {
    store: Arc<ArchiveStore>,
    extracts: Arc<ExtractService>,
    limits: Arc<Limits>,
}

impl ArchiveService {
    /// Build the service over the session's job table and limits.
    #[must_use]
    pub fn new(jobs: Arc<JobRegistry>, limits: Arc<Limits>) -> Self {
        let cache = ManifestCache::new(limits.archive_cache_entries, limits.archive_cache_ttl);
        let store = Arc::new(ArchiveStore::new(cache, Arc::clone(&limits)));
        let extracts = Arc::new(ExtractService::new(
            Arc::clone(&jobs),
            Arc::clone(&store),
            &limits,
        ));
        Self {
            store,
            extracts,
            limits,
        }
    }

    /// The store behind the façade (the pane's `FsSource`-shaped view).
    #[must_use]
    pub fn store(&self) -> &Arc<ArchiveStore> {
        &self.store
    }

    /// The limits the caps come from.
    #[must_use]
    pub fn limits(&self) -> &Arc<Limits> {
        &self.limits
    }

    /// `archive_list` — the rows of one archive directory, in the same [`FileEntry`] shape a real
    /// directory listing produces.
    ///
    /// # Errors
    /// `UnsupportedArchive` for a filesystem path, `ArchiveModified` when the host changed since the
    /// manifest was built (plan F7), plus the open errors.
    pub fn list(&self, path: &FsPath) -> Result<Vec<FileEntry>, CoreError> {
        self.store.list(path)
    }

    /// `archive_read` — one member's bytes, capped.
    pub fn read(&self, path: &FsPath) -> Result<Vec<u8>, CoreError> {
        self.store.read(path)
    }

    /// `archive_stream` — one member into the caller's sink (the `stream_file` criterion: no whole-member
    /// and no whole-archive buffering).
    pub fn stream(&self, path: &FsPath, sink: &mut dyn std::io::Write) -> Result<u64, CoreError> {
        self.store.stream(path, sink)
    }

    /// `archive_props` — the properties payload for a container or one of its subtrees.
    pub fn props(&self, path: &FsPath) -> Result<ArchiveProps, CoreError> {
        let VfsKind::Archive { host, .. } = &path.store else {
            return Err(CoreError::UnsupportedArchive {
                archive: path.to_string(),
            });
        };
        let manifest = self.store.manifest_of(&path.store)?;
        let want = clean_internal_path(&path.relative);
        let prefix = if want.is_empty() {
            String::new()
        } else {
            format!("{want}/")
        };
        let host_bytes = std::fs::metadata(host.as_path()).map_or(0, |m| m.len());
        let mut props = ArchiveProps {
            format: crate::archive::detect(host.as_path()).unwrap_or(ArchiveFormat::Zip),
            host_bytes,
            entries: 0,
            files: 0,
            dirs: 0,
            total_bytes: 0,
            encrypted: 0,
            nested: 0,
            depth: path.store_depth(),
        };
        for (key, node) in manifest.iter() {
            if !want.is_empty() && key != &want && !key.starts_with(&prefix) {
                continue;
            }
            props.entries += 1;
            if node.is_dir {
                props.dirs += 1;
            } else {
                props.files += 1;
                props.total_bytes += u64::try_from(node.size.max(0)).unwrap_or(0);
            }
            if crate::archive::detect_name(key).is_some() {
                props.nested += 1;
            }
            if node.encrypted {
                props.encrypted += 1;
            }
        }
        Ok(props)
    }

    /// `archive_extract` — start an extraction job (progress/cancel through the job table, `DirChanged`
    /// from [`crate::session::Session::archive_extract`], which is the only caller that can invalidate).
    pub fn extract(
        &self,
        req: &ExtractReq,
        origin: Origin,
        on_done: crate::archive::extract::DoneHook,
    ) -> Result<JobId, CoreError> {
        self.extracts.start(req, origin, on_done)
    }

    /// The status of one extraction (the dialog's poll).
    #[must_use]
    pub fn extract_status(&self, id: JobId) -> Option<Arc<ExtractJob>> {
        self.extracts.status(id)
    }

    /// Forget finished extractions past the retention window; the session's tick calls it.
    pub fn gc(&self) {
        self.extracts.gc();
    }
}
