//! `mkdir`: validate the name, make the directory, hand back the row (plan §5.3 `ops/mkdir.rs`, Go
//! `handleMkdir` `api/ops.go:488-521`).
//!
//! Go's handler was eight lines of substance: join, `os.Mkdir(target, 0755)`, log, `Stat`, return the
//! entry. The port adds exactly two things — the name goes through the same validator a destination does
//! (so `..`, `.`, `/`, a glob and, on Windows, `CON`/`NUL`/`name:stream` are refused before anything is
//! created; plan §5.3's `validate_name` row), and a taken name is a typed [`CoreError::AlreadyExists`]
//! rather than Go's 500 with the raw `file exists` prose (`api/ops.go:495-498`).
//!
//! The mode is Go's `0755` default, passed through `mkdir(2)` so the process umask applies exactly as it
//! did in Go — a `DirBuilder::mode` of `0755` under the usual `022` umask produces `0755`, and under a `077`
//! umask produces `0700`, which is what Go did too.

use std::sync::Arc;

use crate::entry::{DirEntryLike, EntryBuilder, EntryKind, EntryStat, FileEntry, InfoEntry};
use crate::error::CoreError;
use crate::mode::ModeBits;
use crate::ops::guard::{ensure_writable, vfs_write_guard};
use crate::path::{AbsPath, FsPath, validate_dest_name};
use crate::platform::Env;

/// Go's `os.Mkdir(target, 0755)` mode (`api/ops.go:494`).
pub const DEFAULT_DIR_MODE: ModeBits = ModeBits::from_bits_retain(0o755);

/// One `mkdir` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MkdirReq {
    /// The parent directory — the record's `dest` (`api/ops.go:501`).
    pub dest: AbsPath,
    /// The new directory's name — validated by [`crate::path::validate_dest_name`], and the record's
    /// `name`.
    pub name: String,
    /// The mode, umask permitting. Go had no parameter for this; plan §5.3's row does.
    pub mode: ModeBits,
}

impl MkdirReq {
    /// `mkdir <dest>/<name>` with Go's `0755`.
    #[must_use]
    pub fn new(dest: AbsPath, name: impl Into<String>) -> Self {
        Self {
            dest,
            name: name.into(),
            mode: DEFAULT_DIR_MODE,
        }
    }

    /// The path that will be created.
    ///
    /// # Errors
    /// [`CoreError::InvalidPath`] for a name that is not one legal component.
    pub fn target(&self) -> Result<AbsPath, CoreError> {
        validate_dest_name(&self.name)?;
        self.dest.join(&self.name)
    }
}

/// Create one directory and describe it the way a listing would (Go's `FileEntry` payload,
/// `api/ops.go:512-520`).
///
/// The synchronous prefix is the whole operation — there is nothing here that can take long enough to need
/// a job (plan §4.1 lists `mkdir` under "mutations: synchronous when provably short").
///
/// # Errors
/// [`CoreError::VfsReadOnly`] for an archive destination, [`CoreError::InvalidPath`] for an illegal name,
/// [`CoreError::PermissionDenied`] when the platform says the parent is not writable,
/// [`CoreError::AlreadyExists`] when the target is taken (Go's raw `file exists` 500), and the `mkdir(2)`
/// failure as [`CoreError::Io`].
pub fn mkdir(env: &dyn Env, req: &MkdirReq) -> Result<FileEntry, CoreError> {
    let dest_fs = FsPath::from_abs(req.dest.clone());
    vfs_write_guard(std::slice::from_ref(&&dest_fs))?;
    let target = req.target()?;
    if std::fs::symlink_metadata(target.as_path()).is_ok() {
        return Err(CoreError::AlreadyExists {
            path: target.to_string_lossy(),
        });
    }
    ensure_writable(env, &req.dest)?;
    make_dir(&target, req.mode)?;
    stat_entry(&target)
}

// ------------------------------------------------------------------------------------------- internals

#[cfg(unix)]
fn make_dir(target: &AbsPath, mode: ModeBits) -> Result<(), CoreError> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = std::fs::DirBuilder::new();
    let built = builder
        .mode(mode.bits_octal() & 0o7777)
        .create(target.as_path());
    built.map_err(|io| CoreError::Io {
        path: target.to_string_lossy(),
        io: Arc::new(io),
    })
}

#[cfg(not(unix))]
fn make_dir(target: &AbsPath, _mode: ModeBits) -> Result<(), CoreError> {
    // No mode to pass on this platform; `create_dir` is the whole request.
    std::fs::create_dir(target.as_path()).map_err(|io| CoreError::Io {
        path: target.to_string_lossy(),
        io: Arc::new(io),
    })
}

/// The row Go returned for the new directory: `os.Stat` + `FileEntry` (`api/ops.go:505-519`).
pub(crate) fn stat_entry(path: &AbsPath) -> Result<FileEntry, CoreError> {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let meta = std::fs::metadata(path.as_path()).map_err(|io| CoreError::Io {
        path: path.to_string_lossy(),
        io: Arc::new(io),
    })?;
    let kind = EntryKind::from_file_type(&meta.file_type(), meta.is_dir());
    let dirent = DirEntryLike::new(name.clone(), kind);
    let info = InfoEntry::from_metadata(name, &meta.file_type(), &meta);
    let stat = EntryStat::standalone(FsPath::from_abs(path.clone()), dirent, Some(info));
    Ok(EntryBuilder::from_stat(&stat).build())
}
