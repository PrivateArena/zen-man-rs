//! The recursive copy engine — Go's `copyFile`/`copyDir`/`copyItem` (`api/ops.go:648-678,813-822`) with
//! the two silent behaviours replaced by reported ones (plan §5.3 `ops/copy.rs`, §6.1 F12).
//!
//! What is Go's and what is not, stated per line of the reference:
//!
//! | Go | Here |
//! |---|---|
//! | `copyFile` returns `nil` when the destination exists (`ops.go:649-651`) — a *silent* skip | the caller's [`ConflictPolicy`] decides, and a skip is a [`CopyStats::skipped`] entry |
//! | `copyFile` chmods the copy to the source mode, mtime is lost (`ops.go:671-674`) | mode **and** mtime are preserved (plan §5.3 asks for both; mtime is what makes a later incremental paste meaningful) |
//! | `copyDir` recurses with plain recursion (`ops.go:697-708`) | an explicit stack with [`MAX_COPY_DEPTH`], so a deep tree cannot overflow a worker thread's stack |
//! | `copyDir` creates the directory with the source mode and then writes into it (`ops.go:683`) | created with the source mode **plus** owner write/execute, and the exact mode applied *after* the subtree — a `0555` source directory used to make its own copy fail (`EACCES` on the child write) |
//! | `os.ReadDir` sorts, so Go's copy order is name order | sorted too, deliberately: the undo-log record's source list is only reproducible if the walk order is |
//! | `copyDir` treats every non-`IsDir()` entry as a file and reads it (`ops.go:700-704`) — a symlink to a directory is therefore opened and fails, and a FIFO blocks the request forever | symlinks are recreated as symlinks, and a FIFO/socket/device node is refused rather than opened (plan F1: nothing on a job thread may block on a pipe) |
//!
//! The tree walk reports progress through [`ProgressSink`] as it goes, and checks cancellation per item
//! and every [`CANCEL_WINDOW`] bytes inside one large file.

use std::io::{Read, Write};
use std::path::Path;

use crate::error::CoreError;
use crate::ops::{ConflictDecision, ConflictPolicy, ProgressSink};
use crate::path::AbsPath;

/// How deep [`copy_tree`] descends before refusing ([`CoreError::CapacityExceeded`]).
pub const MAX_COPY_DEPTH: usize = 128;

/// The streaming buffer, and therefore the granularity of a read/write.
const CHUNK: usize = 256 * 1024;

/// How many bytes are moved between two cancellation checks inside one file (plan §5.3: "cancel check
/// every 8 MiB").
const CANCEL_WINDOW: u64 = 8 * 1024 * 1024;

/// What one copied item cost, and where it actually landed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CopyStats {
    /// The destination the item was written to — [`ConflictPolicy::Rename`] can move it off the name the
    /// caller proposed, so this is the answer, not an echo.
    pub dest: Option<AbsPath>,
    /// Bytes of file content written.
    pub bytes: u64,
    /// Regular files written.
    pub files: u64,
    /// Directories created.
    pub dirs: u64,
    /// Links recreated.
    pub links: u64,
    /// Destinations that already existed and the policy left alone (plan F12 — never silent again).
    pub skipped: Vec<AbsPath>,
}

impl CopyStats {
    /// One more item's worth of work.
    fn absorb(&mut self, other: &Self) {
        self.bytes += other.bytes;
        self.files += other.files;
        self.dirs += other.dirs;
        self.links += other.links;
        self.skipped.extend(other.skipped.iter().cloned());
    }

    /// `true` when nothing was written (every candidate was skipped).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes == 0 && self.files == 0 && self.dirs == 0 && self.links == 0
    }
}

fn io_err(path: &AbsPath, e: std::io::Error) -> CoreError {
    CoreError::Io {
        path: path.to_string_lossy(),
        io: std::sync::Arc::new(e),
    }
}

/// Copy one **regular** file, preserving mode and mtime, and return the byte count.
///
/// `src == dst` is refused with [`CoreError::DestInsideSource`] rather than truncating the source into
/// zero bytes — the case Go's `copyFile` did not check at all, and the reason the guard alone is not
/// enough: the guard only sees the top-level selection, this only sees the bytes.
///
/// A symlink passed here is *followed* (Go's `os.Stat`/`os.Open`, `api/ops.go:653`); the tree walk
/// recreates links instead. A FIFO, socket or device node is refused: reading one can block forever, and
/// no job thread may block on a pipe (plan F1).
///
/// # Errors
/// [`CoreError::Io`] for any filesystem failure, [`CoreError::DestInsideSource`] for a self-copy,
/// [`CoreError::Unsupported`] for a non-regular source, [`CoreError::Cancelled`] when the token fires.
pub fn copy_file(
    src: &AbsPath,
    dst: &AbsPath,
    cancel: &crate::jobs::CancelToken,
    progress: &dyn ProgressSink,
) -> Result<u64, CoreError> {
    cancel.check()?;
    if src == dst {
        return Err(CoreError::DestInsideSource {
            src: src.to_string_lossy(),
            dest: dst.to_string_lossy(),
        });
    }
    let meta = std::fs::metadata(src.as_path()).map_err(|e| io_err(src, e))?;
    if !meta.is_file() {
        return Err(CoreError::Unsupported {
            what: "copying a non-regular file (symlinks are recreated, and a FIFO/socket/device node \
                   would block the worker: plan §6.1 F1)",
        });
    }
    let mut reader = std::fs::File::open(src.as_path()).map_err(|e| io_err(src, e))?;
    let mut writer = std::fs::File::create(dst.as_path()).map_err(|e| io_err(dst, e))?;
    let mut buf = vec![0u8; CHUNK];
    let mut total = 0u64;
    let mut since_check = 0u64;
    loop {
        let n = reader.read(&mut buf).map_err(|e| io_err(src, e))?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n]).map_err(|e| io_err(dst, e))?;
        total += n as u64;
        since_check += n as u64;
        if since_check >= CANCEL_WINDOW {
            cancel.check()?;
            since_check = 0;
        }
    }
    writer.flush().map_err(|e| io_err(dst, e))?;
    // mtime before the handle closes; a platform that cannot set it says so rather than failing the copy.
    if let Ok(mtime) = meta.modified() {
        let _ = writer.set_modified(mtime);
    }
    drop(writer);
    drop(reader);
    apply_mode(src.as_path(), dst, &meta)?;
    progress.advance(total, 1, &name_of(dst));
    Ok(total)
}

/// Copy a directory tree into `dst`, merging with it when it already exists (Go's `copyDir` `MkdirAll`,
/// `api/ops.go:683`).
///
/// # Errors
/// As [`copy_file`], plus [`CoreError::CapacityExceeded`] past [`MAX_COPY_DEPTH`].
pub fn copy_tree(
    src: &AbsPath,
    dst: &AbsPath,
    policy: ConflictPolicy,
    symlinks: bool,
    cancel: &crate::jobs::CancelToken,
    progress: &dyn ProgressSink,
) -> Result<CopyStats, CoreError> {
    let mut stats = CopyStats::default();
    let mut stack = vec![(src.clone(), dst.clone(), 0usize)];
    // The directory modes Go applied *before* filling the tree; applied afterwards, once nothing else can
    // need write access to them.
    let mut deferred: Vec<(AbsPath, u32)> = Vec::new();
    while let Some((s, d, depth)) = stack.pop() {
        cancel.check()?;
        if depth > MAX_COPY_DEPTH {
            return Err(CoreError::CapacityExceeded {
                what: "copy tree depth",
                limit: MAX_COPY_DEPTH,
            });
        }
        let meta = std::fs::metadata(s.as_path()).map_err(|e| io_err(&s, e))?;
        create_dir_merged(&d, &meta, &mut deferred)?;
        stats.dirs += 1;
        // An unreadable directory is this item's failure and nothing else's: the error propagates to the
        // caller, which records it against this source and continues with the others (plan §6.1: the
        // failure is *attributed*, never swallowed as it was by Go's `return err` out of the whole paste).
        for entry in read_sorted(&s)? {
            cancel.check()?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let sub_src = s.join(&name)?;
            let sub_dst = d.join(&name)?;
            let kind = entry.file_type().map_err(|e| io_err(&sub_src, e))?;
            // The same policy, decided by the same function, at every depth: Go's `copyFile` skipped a
            // taken name only at the leaf it happened to notice, and `copyDir` overwrote nothing but also
            // reported nothing.
            let sub_dst = match policy.decide(kind.is_dir(), &sub_dst)? {
                ConflictDecision::Skip => {
                    stats.skipped.push(sub_dst);
                    continue;
                }
                ConflictDecision::Rename(new) => new,
                ConflictDecision::Proceed | ConflictDecision::Merge => sub_dst,
            };
            if kind.is_symlink() {
                if symlinks {
                    copy_link(&sub_src, &sub_dst, cancel, progress, &mut stats)?;
                } else if std::fs::metadata(sub_src.as_path()).is_ok_and(|m| m.is_dir()) {
                    stack.push((sub_src.clone(), sub_dst.clone(), depth + 1));
                } else {
                    copy_file_item(&sub_src, &sub_dst, cancel, progress, &mut stats)?;
                }
            } else if kind.is_dir() {
                stack.push((sub_src, sub_dst, depth + 1));
            } else if kind.is_file() {
                copy_file_item(&sub_src, &sub_dst, cancel, progress, &mut stats)?;
            }
        }
    }
    for (dir, mode) in &deferred {
        apply_mode_bits(dir, *mode);
    }
    stats.dest = Some(dst.clone());
    Ok(stats)
}

/// Copy one top-level selection entry: a file, a tree, or a link, honouring `policy`.
///
/// `target` is the [`crate::ops::guard::AllowedSource::target`] the guard already computed — this
/// function never re-derives it, it only consults the policy for whether it may be *used*.
///
/// # Errors
/// As [`copy_file`]. A [`CoreError::Cancelled`] aborts; every other error is this item's, and the caller
/// decides whether the operation as a whole failed.
pub fn copy_item(
    src: &AbsPath,
    target: &AbsPath,
    policy: ConflictPolicy,
    symlinks: bool,
    cancel: &crate::jobs::CancelToken,
    progress: &dyn ProgressSink,
) -> Result<CopyStats, CoreError> {
    cancel.check()?;
    if src == target {
        return Err(CoreError::DestInsideSource {
            src: src.to_string_lossy(),
            dest: target.to_string_lossy(),
        });
    }
    let meta = std::fs::metadata(src.as_path()).map_err(|e| io_err(src, e))?;
    let mut stats = CopyStats::default();
    let target = match policy.decide(meta.is_dir(), target)? {
        ConflictDecision::Skip => {
            stats.skipped.push(target.clone());
            stats.dest = Some(target.clone());
            return Ok(stats);
        }
        ConflictDecision::Rename(new) => new,
        ConflictDecision::Proceed | ConflictDecision::Merge => target.clone(),
    };
    if meta.is_dir() {
        let tree = copy_tree(src, &target, policy, symlinks, cancel, progress)?;
        stats.absorb(&tree);
    } else {
        if policy == ConflictPolicy::Overwrite && target_exists(&target) {
            std::fs::remove_file(target.as_path()).map_err(|e| io_err(&target, e))?;
        }
        let bytes = copy_file(src, &target, cancel, progress)?;
        stats.bytes += bytes;
        stats.files += 1;
    }
    stats.dest = Some(target);
    Ok(stats)
}

// -------------------------------------------------------------------------------------- internals

fn name_of(path: &AbsPath) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn target_exists(path: &AbsPath) -> bool {
    std::fs::symlink_metadata(path.as_path()).is_ok()
}

/// `read_dir` in name order (Go's `os.ReadDir` sorts; the copy order is the log's source order).
fn read_sorted(dir: &AbsPath) -> Result<Vec<std::fs::DirEntry>, CoreError> {
    let mut out = Vec::new();
    let read = std::fs::read_dir(dir.as_path()).map_err(|e| io_err(dir, e))?;
    for entry in read {
        let entry = entry.map_err(|e| io_err(dir, e))?;
        out.push(entry);
    }
    out.sort_by_key(std::fs::DirEntry::file_name);
    Ok(out)
}

fn copy_file_item(
    src: &AbsPath,
    dst: &AbsPath,
    cancel: &crate::jobs::CancelToken,
    progress: &dyn ProgressSink,
    stats: &mut CopyStats,
) -> Result<(), CoreError> {
    let bytes = copy_file(src, dst, cancel, progress)?;
    stats.bytes += bytes;
    stats.files += 1;
    Ok(())
}

fn copy_link(
    src: &AbsPath,
    dst: &AbsPath,
    cancel: &crate::jobs::CancelToken,
    progress: &dyn ProgressSink,
    stats: &mut CopyStats,
) -> Result<(), CoreError> {
    cancel.check()?;
    let link_target = std::fs::read_link(src.as_path()).map_err(|e| io_err(src, e))?;
    make_link(Path::new(&link_target), dst.as_path()).map_err(|e| io_err(dst, e))?;
    stats.links += 1;
    progress.advance(0, 1, &name_of(dst));
    Ok(())
}

/// `create_dir` for a tree node: succeed, or accept an existing directory (the merge case).
fn create_dir_merged(
    dir: &AbsPath,
    src_meta: &std::fs::Metadata,
    deferred: &mut Vec<(AbsPath, u32)>,
) -> Result<(), CoreError> {
    match std::fs::symlink_metadata(dir.as_path()) {
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => Err(CoreError::AlreadyExists {
            path: dir.to_string_lossy(),
        }),
        Err(_) => {
            std::fs::create_dir_all(dir.as_path()).map_err(|e| io_err(dir, e))?;
            deferred.push((dir.clone(), desired_mode(src_meta)));
            Ok(())
        }
    }
}

/// The mode Go handed to `MkdirAll`/`Chmod` — the permission bits of the source.
fn desired_mode(meta: &std::fs::Metadata) -> u32 {
    mode_bits(meta) & 0o7777
}

#[cfg(unix)]
fn mode_bits(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode()
}

#[cfg(not(unix))]
fn mode_bits(_meta: &std::fs::Metadata) -> u32 {
    0o644
}

#[cfg(unix)]
fn apply_mode(_src: &Path, dst: &AbsPath, meta: &std::fs::Metadata) -> Result<(), CoreError> {
    apply_mode_bits(dst, desired_mode(meta));
    Ok(())
}

#[cfg(not(unix))]
fn apply_mode(_src: &Path, _dst: &AbsPath, _meta: &std::fs::Metadata) -> Result<(), CoreError> {
    Ok(())
}

/// Best-effort: a filesystem that stores no mode (FAT, a network share without a mapping) must not turn
/// a successful copy into a failed one — Go's `copyFile` propagated the `Chmod` error, and a copy of a FAT
/// tree then looked like a partial failure.
#[cfg(unix)]
fn apply_mode_bits(path: &AbsPath, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path.as_path(), std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn apply_mode_bits(_path: &AbsPath, _mode: u32) {}

#[cfg(unix)]
fn make_link(target: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, dst)
}

#[cfg(windows)]
fn make_link(target: &Path, dst: &Path) -> std::io::Result<()> {
    match std::fs::metadata(target) {
        Ok(m) if m.is_dir() => std::os::windows::fs::symlink_dir(target, dst),
        _ => std::os::windows::fs::symlink_file(target, dst),
    }
}

#[cfg(not(any(unix, windows)))]
fn make_link(_target: &Path, _dst: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this platform creates no symlinks",
    ))
}
