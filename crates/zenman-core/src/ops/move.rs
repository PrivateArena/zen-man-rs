//! Move = rename when the filesystem can do it, copy+delete when it cannot, merge when a directory meets
//! a directory (plan §5.3 `ops/move.rs`, Go `api/ops.go:710-811`).
//!
//! The three-way shape is Go's, and so is the reason for the [`MoveFs`] seam: `os.Rename` answers
//! `EXDEV` on a cross-device move, and the only way to test the fallback honestly is to inject that
//! answer (plan §5.3: "the `EXDEV` path is covered by an injected-failure seam, mocked in tests").
//!
//! Deviations from the reference, each named where the code does it:
//!
//! * `moveItem` errors when a *file* lands on an existing file (`api/ops.go:716-720`); here that is
//!   [`ConflictPolicy`] speaking, so copy and move answer a collision the same way (ledger #10 keeps the
//!   error as the `SkipExisting` outcome — reported, not silent).
//! * `mergeDirViaRename` fans the merge out over 64 goroutines (`api/ops.go:773-808`). Here it is a
//!   sequential walk: the whole operation already runs on a job thread (plan F1), and a deterministic
//!   order is what makes the undo-log record reproducible.
//! * the cross-device directory fallback leaves a non-empty source directory behind and reports success
//!   (`api/ops.go:745-750`); here the leftovers come back as [`CoreError::Conflict`] so the user learns
//!   the move did not finish.
//! * a symlink source is moved **as a link** (`symlink_metadata`), where Go's `os.Stat` followed it and
//!   then renamed the link anyway — the two answers could not both be right.

use std::sync::Arc;

use crate::error::CoreError;
use crate::jobs::CancelToken;
use crate::ops::copy::copy_file;
use crate::ops::{ConflictDecision, ConflictPolicy, ProgressSink};
use crate::path::AbsPath;

/// The filesystem half of a move, so a test can make `rename` answer `EXDEV` on a single-disk machine.
///
/// Deliberately two methods only: the copy side of the fallback is [`crate::ops::copy`] (the same engine
/// a paste-copy uses, with the same policy and progress reporting), not a second implementation here.
pub trait MoveFs: Send + Sync {
    /// `os.Rename`. The fast path, and the one that fails with [`exdev`] across devices.
    ///
    /// # Errors
    /// [`CoreError::Io`] with the OS's own errno, so [`is_cross_device`] can recognise it.
    fn rename(&self, src: &AbsPath, dst: &AbsPath) -> Result<(), CoreError>;

    /// Remove one path — a file, a link, or a directory that is now empty.
    ///
    /// # Errors
    /// [`CoreError::Io`], including the non-empty-directory answer when a merge left leftovers.
    fn remove(&self, path: &AbsPath) -> Result<(), CoreError>;
}

/// The [`MoveFs`] that talks to the operating system.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsMoveFs;

impl MoveFs for OsMoveFs {
    fn rename(&self, src: &AbsPath, dst: &AbsPath) -> Result<(), CoreError> {
        std::fs::rename(src.as_path(), dst.as_path()).map_err(|e| CoreError::Io {
            path: dst.to_string_lossy(),
            io: Arc::new(e),
        })
    }

    fn remove(&self, path: &AbsPath) -> Result<(), CoreError> {
        let meta = std::fs::symlink_metadata(path.as_path()).map_err(|e| CoreError::Io {
            path: path.to_string_lossy(),
            io: Arc::new(e),
        })?;
        let result = if meta.is_dir() {
            std::fs::remove_dir(path.as_path())
        } else {
            std::fs::remove_file(path.as_path())
        };
        result.map_err(|e| CoreError::Io {
            path: path.to_string_lossy(),
            io: Arc::new(e),
        })
    }
}

/// The `EXDEV` errno as this platform spells it, wrapped the way [`MoveFs::rename`] wraps failures.
///
/// A test seam *and* the production answer: the mock returns exactly this so the fallback it exercises is
/// the one the real code takes.
#[must_use]
pub fn exdev() -> CoreError {
    CoreError::Io {
        path: String::new(),
        io: Arc::new(std::io::Error::from_raw_os_error(exdev_raw())),
    }
}

#[cfg(unix)]
fn exdev_raw() -> i32 {
    // `EXDEV` on the glibc/musl/BSD line Linux, macOS and the other unices we target all sit on: the one
    // errno whose whole meaning is "these two paths are not on one filesystem, copy it instead"
    // (`rename(2)`/`renameat2`). Compared through [`std::io::Error::from_raw_os_error`] so a platform that
    // re-maps it still answers its own number.
    std::io::Error::from_raw_os_error(18)
        .raw_os_error()
        .unwrap_or(18)
}

/// Windows spells the same condition differently; step 16's `platform/windows` work owns that answer, and
/// until then a Windows rename failure is reported rather than silently turned into a copy.
#[cfg(not(unix))]
fn exdev_raw() -> i32 {
    std::io::Error::from_raw_os_error(18)
        .raw_os_error()
        .unwrap_or(0)
}

/// `true` when a rename failure means "different filesystem", i.e. "copy it instead".
///
/// Windows spells this differently (plan step 16's `platform/windows` work); until then a Windows rename
/// failure is reported rather than silently turned into a copy.
#[must_use]
pub fn is_cross_device(error: &CoreError) -> bool {
    matches!(error, CoreError::Io { io, .. }
        if io.raw_os_error() == Some(exdev_raw()))
}

/// What one moved item cost, and where it landed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MoveStats {
    /// The destination actually used (see [`CopyStats::dest`]).
    pub dest: Option<AbsPath>,
    /// Bytes copied by the cross-device fallback; a same-device rename moves no bytes, so this is `0`.
    pub bytes: u64,
    /// Entries the fallback had to write (0 for a rename).
    pub files: u64,
    /// Directories the fallback had to create (0 for a rename).
    pub dirs: u64,
    /// Destinations a [`ConflictPolicy::SkipExisting`] merge left alone — and the reason the source may
    /// still exist.
    pub skipped: Vec<AbsPath>,
    /// `true` when the whole item went over the rename path.
    pub renamed: bool,
}

impl MoveStats {
    /// `true` when nothing was written (the item was skipped).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes == 0 && self.files == 0 && self.dirs == 0 && !self.renamed
    }
}

/// Move `src` to `target` (the guard's `dest.join(base(src))`), applying `policy` to a taken name.
///
/// The order is Go's: try the rename, then the directory merge, then the cross-device copy+delete. The
/// last one rolls back — a failed fallback deletes the partial destination and leaves the source alone
/// (plan §5.3: "cross-device failure leaves the source intact").
///
/// # Errors
/// [`CoreError::Cancelled`], [`CoreError::Io`], [`CoreError::DestInsideSource`], or — for a merge that
/// could not empty its source — [`CoreError::Conflict`].
pub fn move_item(
    src: &AbsPath,
    target: &AbsPath,
    policy: ConflictPolicy,
    fs: &dyn MoveFs,
    cancel: &CancelToken,
    progress: &dyn ProgressSink,
) -> Result<MoveStats, CoreError> {
    cancel.check()?;
    if src == target {
        return Err(CoreError::DestInsideSource {
            src: src.to_string_lossy(),
            dest: target.to_string_lossy(),
        });
    }
    let meta = std::fs::symlink_metadata(src.as_path()).map_err(|e| CoreError::Io {
        path: src.to_string_lossy(),
        io: Arc::new(e),
    })?;
    let src_is_dir = meta.is_dir();
    let decision = policy.decide(src_is_dir, target)?;
    let merge = decision == ConflictDecision::Merge;
    let target = match decision {
        ConflictDecision::Skip => {
            return Ok(MoveStats {
                dest: Some(target.clone()),
                skipped: vec![target.clone()],
                ..Default::default()
            });
        }
        ConflictDecision::Rename(new) => new,
        ConflictDecision::Merge | ConflictDecision::Proceed => target.clone(),
    };
    // A directory onto an existing directory is Go's merge (`api/ops.go:730-735`), decided before the
    // rename is even tried: `rename(2)` answers `ENOTEMPTY` and the fallback would then have to re-derive
    // the same answer from a different errno.
    if merge {
        return merge_dir_into(src, &target, policy, fs, cancel, progress);
    }
    if !src_is_dir && policy == ConflictPolicy::Overwrite {
        // `rename(2)` replaces a regular destination anyway; removing first is what makes the *directory*
        // destination case (`ENOTEMPTY`) fail loudly instead of falling through to a merge.
        if std::fs::symlink_metadata(target.as_path()).is_ok() && !is_dir(&target) {
            fs.remove(&target)?;
        }
    }
    match fs.rename(src, &target) {
        Ok(()) => {
            progress.advance(0, 1, &name_of(&target));
            Ok(MoveStats {
                dest: Some(target),
                renamed: true,
                ..Default::default()
            })
        }
        Err(rename_err) => {
            // Go's step 2 (`api/ops.go:730-735`): a directory rename that failed because a directory is
            // sitting there is a merge, whatever the errno said (`ENOTEMPTY` on the same device, `EXDEV`
            // across).
            if src_is_dir && is_dir(&target) {
                return merge_dir_into(src, &target, policy, fs, cancel, progress);
            }
            if !is_cross_device(&rename_err) {
                return Err(rename_err);
            }
            cross_device_move(src, &target, src_is_dir, policy, fs, cancel, progress)
        }
    }
}

/// Go's `mergeDirViaRename` (`api/ops.go:773`): rename each child of `src_dir` into the existing
/// `dst_dir`, then remove the now-empty source shell.
///
/// Nested collisions go through the same [`ConflictPolicy`], so a merge of a merge cannot invent a second
/// answer to "the name is taken".
///
/// # Errors
/// As [`move_item`]; a leftover (skipped) child keeps the source directory and yields
/// [`CoreError::Conflict`] naming what is still there.
pub fn merge_dir_into(
    src_dir: &AbsPath,
    dst_dir: &AbsPath,
    policy: ConflictPolicy,
    fs: &dyn MoveFs,
    cancel: &CancelToken,
    progress: &dyn ProgressSink,
) -> Result<MoveStats, CoreError> {
    cancel.check()?;
    let mut stats = MoveStats {
        dest: Some(dst_dir.clone()),
        ..Default::default()
    };
    let entries = read_sorted(src_dir)?;
    for entry in entries {
        cancel.check()?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let sub_src = src_dir.join(&name)?;
        let sub_dst = dst_dir.join(&name)?;
        let moved = move_item(&sub_src, &sub_dst, policy, fs, cancel, progress)?;
        stats.bytes += moved.bytes;
        stats.files += moved.files;
        stats.dirs += moved.dirs;
        stats.skipped.extend(moved.skipped.iter().cloned());
    }
    if !stats.skipped.is_empty() {
        let leftovers: Vec<String> = stats.skipped.iter().map(name_of).collect();
        return Err(CoreError::Conflict {
            conflicts: leftovers,
            dest: dst_dir.to_string_lossy(),
        });
    }
    fs.remove(src_dir)?;
    stats.dirs += 1;
    Ok(stats)
}

// -------------------------------------------------------------------------------------- internals

fn name_of(path: &AbsPath) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn is_dir(path: &AbsPath) -> bool {
    std::fs::symlink_metadata(path.as_path()).is_ok_and(|m| m.is_dir())
}

fn read_sorted(dir: &AbsPath) -> Result<Vec<std::fs::DirEntry>, CoreError> {
    let mut out = Vec::new();
    let err_path = dir.to_string_lossy();
    let read = std::fs::read_dir(dir.as_path()).map_err(|e| CoreError::Io {
        path: err_path.clone(),
        io: Arc::new(e),
    })?;
    for entry in read {
        let entry = entry.map_err(|e| CoreError::Io {
            path: err_path.clone(),
            io: Arc::new(e),
        })?;
        out.push(entry);
    }
    out.sort_by_key(std::fs::DirEntry::file_name);
    Ok(out)
}

/// The `EXDEV` path: copy with the normal engine, then delete the source.
fn cross_device_move(
    src: &AbsPath,
    target: &AbsPath,
    src_is_dir: bool,
    policy: ConflictPolicy,
    fs: &dyn MoveFs,
    cancel: &CancelToken,
    progress: &dyn ProgressSink,
) -> Result<MoveStats, CoreError> {
    if src_is_dir {
        return cross_device_dir(src, target, policy, fs, cancel, progress);
    }
    let copied = copy_file(src, target, cancel, progress);
    match copied {
        Ok(bytes) => match fs.remove(src) {
            Ok(()) => Ok(MoveStats {
                dest: Some(target.clone()),
                bytes,
                files: 1,
                ..Default::default()
            }),
            Err(e) => {
                // The copy landed and the source did not go; say so rather than pretend the move worked.
                let _ = fs.remove(target);
                Err(e)
            }
        },
        Err(e) => {
            // Roll the partial destination back: "cross-device failure leaves the source intact" means
            // the *destination* must not keep a half-written file either (plan §5.3).
            let _ = fs.remove(target);
            Err(e)
        }
    }
}

fn cross_device_dir(
    src: &AbsPath,
    target: &AbsPath,
    policy: ConflictPolicy,
    fs: &dyn MoveFs,
    cancel: &CancelToken,
    progress: &dyn ProgressSink,
) -> Result<MoveStats, CoreError> {
    std::fs::create_dir_all(target.as_path()).map_err(|e| CoreError::Io {
        path: target.to_string_lossy(),
        io: Arc::new(e),
    })?;
    let mut stats = MoveStats {
        dest: Some(target.clone()),
        ..Default::default()
    };
    for entry in read_sorted(src)? {
        cancel.check()?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let sub_src = src.join(&name)?;
        let sub_dst = target.join(&name)?;
        let moved = move_item(&sub_src, &sub_dst, policy, fs, cancel, progress)?;
        stats.bytes += moved.bytes;
        stats.files += moved.files;
        stats.dirs += moved.dirs;
        stats.skipped.extend(moved.skipped.iter().cloned());
    }
    match fs.remove(src) {
        Ok(()) => {
            stats.dirs += 1;
            Ok(stats)
        }
        Err(e) => {
            if stats.skipped.is_empty() {
                return Err(e);
            }
            let leftovers: Vec<String> = stats.skipped.iter().map(name_of).collect();
            Err(CoreError::Conflict {
                conflicts: leftovers,
                dest: target.to_string_lossy(),
            })
        }
    }
}
