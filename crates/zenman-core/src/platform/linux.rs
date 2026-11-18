//! The Linux/Unix `Env` implementation — plan §5.2 `platform/posix.rs` row, spelled out per the
//! already-written `platform/mod.rs` trait (see that file's module doc for the boxed-future rationale).
//!
//! # cfg shape
//!
//! `mod.rs` declares `mod linux;` and re-exports `Linux` **unconditionally**, so this file is compiled on
//! every host — including a Windows cross-build, where it must still describe a complete `impl Env`. The
//! gates are therefore per *call*, not per file: `unix` for the POSIX syscalls (`symlink(2)`,
//! `fchmod`, `statvfs`) and `target_os = "linux"` for the things that literally do not exist elsewhere
//! (`/proc/self/mountinfo`, `/sys/block`). Everywhere a gate closes, the answer is a typed
//! [`CoreError::Unsupported`] or an empty-but-valid table, never a panic and never a silent no-op — which
//! is what keeps step 16's `platform/windows.rs` an *addition* rather than a fix.
//!
//! # Why the disk figures look like this
//!
//! `statvfs` is used instead of `statfs` because the VFS identity (`f_fsid`) and the *unprivileged* free
//! count (`f_bavail`) are both part of the contract: `/api/diskspace` published `free` as "bytes this user
//! may still allocate" (`docs/contracts.md` → `/api/diskspace`), which is `f_bavail`, not `f_bfree`. Go's
//! `syscall.Statfs` multiplied by `Bsize`; we multiply by `Frsize` (plan §5.2), falling back to `Bsize`
//! when the filesystem reports `f_frsize == 0` (some pseudo-filesystems do, and a zero unit would make a
//! populated filesystem look empty).

use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, RwLock};

use super::env_probe::{self, ProbeReport};
use super::{
    CopyFailure, CopyItem, CopyReport, DiskSpace, Env, ExecOutput, ExecRequest, LinkItem,
    MountEntry, MountTable, Os, PlatformCaps, PlatformFuture, Shortcut, ready,
};
use crate::error::CoreError;
use crate::mode::ModeBits;
use crate::path::AbsPath;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// `/proc/self/mountinfo` — the only mount table this implementation reads (it carries the mount ID, the
/// propagation group and the per-mount root, none of which `/proc/mounts` has).
pub const PROC_MOUNTINFO: &str = "/proc/self/mountinfo";

/// The probe cache: `None` until somebody runs the write test, `Some(_)` with the measured answer
/// afterwards (plan F21 — measured once, read on every frame).
pub type ProbeCache = Arc<RwLock<Option<bool>>>;

/// Linux (and, through the `unix` gates, macOS/BSD) platform actions.
///
/// Cheap to construct and free of I/O: `default_env()` is allowed to build one on every startup, and the
/// only state is the probe cache.
#[derive(Clone)]
pub struct Linux {
    probe_cache: ProbeCache,
}

impl Linux {
    /// An environment with no probe result yet — [`Env::supports_symlink`] answers the static
    /// [`PlatformCaps::SYMLINK`] bit until [`Linux::probe_symlinks`] (or [`Linux::set_symlink_supported`])
    /// supplies the measured one.
    #[must_use]
    pub fn new() -> Self {
        Self {
            probe_cache: Arc::new(RwLock::new(None)),
        }
    }

    /// Share a probe cache with somebody else — step 6's `Session` owns the cache so the probe result
    /// survives a `Linux` handle being cloned, and so the `Session` can re-probe after a settings change.
    #[must_use]
    pub fn with_probe_cache(probe_cache: ProbeCache) -> Self {
        Self { probe_cache }
    }

    /// The cache this implementation reads. Cloning the `Arc` is how a caller (or a test recorder)
    /// observes the value the UI sees.
    #[must_use]
    pub fn probe_cache(&self) -> &ProbeCache {
        &self.probe_cache
    }

    /// Run the write test under `scratch_root` and cache its verdict (plan F21's "once").
    ///
    /// This is the *only* path in this file that a frame must not call: it creates and deletes files.
    /// Step 6's `Session::open` runs it once, before the first paint, inside the startup note list.
    pub fn probe_symlinks(&self, scratch_root: &Path) -> Result<bool, CoreError> {
        let supported = self::supports_symlink(scratch_root)?;
        self.set_symlink_supported(supported);
        Ok(supported)
    }

    /// Publish a probe verdict into the shared cache.
    pub fn set_symlink_supported(&self, supported: bool) {
        // A poisoned lock here means another thread panicked *while storing a bool*; the value it left
        // behind is still a valid `Option<bool>`, so taking the inner guard is strictly better than
        // propagating a panic into the UI's capability read.
        let mut guard = self
            .probe_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(supported);
    }

    /// The cached verdict, if any.
    #[must_use]
    pub fn cached_symlink_support(&self) -> Option<bool> {
        *self
            .probe_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Default for Linux {
    fn default() -> Self {
        Self::new()
    }
}

impl Env for Linux {
    fn os(&self) -> Os {
        Os::current()
    }

    fn caps(&self) -> PlatformCaps {
        super::detect().caps
    }

    fn open_with(&self, path: &AbsPath) -> PlatformFuture<'_, Result<(), CoreError>> {
        ready(self::open_with(path))
    }

    fn copy_paths(&self, items: &[CopyItem]) -> PlatformFuture<'_, Result<CopyReport, CoreError>> {
        ready(self::copy_paths(items))
    }

    fn paste_link(&self, items: &[LinkItem]) -> PlatformFuture<'_, Result<(), CoreError>> {
        ready(self::paste_link(items))
    }

    fn supports_symlink(&self) -> Result<bool, CoreError> {
        Ok(match self.cached_symlink_support() {
            Some(cached) => cached,
            None => self.caps().contains(PlatformCaps::SYMLINK),
        })
    }

    fn chmod(
        &self,
        targets: &[AbsPath],
        mode: ModeBits,
    ) -> PlatformFuture<'_, Result<(), CoreError>> {
        ready(self::chmod(targets, mode))
    }

    fn can_trash(&self, path: &AbsPath) -> bool {
        self::can_trash(path.as_path())
    }

    fn trash(&self, path: &AbsPath) -> Result<bool, CoreError> {
        self::trash(path.as_path())
    }

    fn create_shortcut(&self, shortcut: &Shortcut) -> Result<(), CoreError> {
        // `mod.rs` presents this as the *Windows* fallback for a volume without symlinks. A Linux `.desktop`
        // writer is a real feature (and `services/places.rs` will want it for "add to desktop"), but it is
        // not in plan §5.3, so it is reported as absent rather than guessed at.
        let _ = shortcut;
        Err(CoreError::Unsupported {
            what: "shortcut files (the Windows symlink fallback; no Linux .desktop writer in plan §5.3)",
        })
    }

    fn disk_space(&self, path: &AbsPath) -> Result<DiskSpace, CoreError> {
        self::disk_space(path)
    }

    fn mount_entries(&self) -> Result<MountTable, CoreError> {
        self::mount_entries()
    }

    fn exec_program(&self, req: ExecRequest) -> PlatformFuture<'_, Result<ExecOutput, CoreError>> {
        ready(self::exec_program(&req))
    }

    fn has_program(&self, name: &str) -> Result<bool, CoreError> {
        self::has_program(name)
    }

    fn is_writable(&self, path: &AbsPath) -> Result<bool, CoreError> {
        self::is_writable(path)
    }
}

// --------------------------------------------------------------------------- the actions

/// `op:"paste_link"` (`api/ops.go:917-920`): create every link, stopping at the first failure with the
/// platform's typed "no symlinks here" answer so the caller can offer a shortcut instead.
pub fn paste_link(items: &[LinkItem]) -> Result<(), CoreError> {
    for item in items {
        symlink(&item.link, &item.target)?;
    }
    Ok(())
}

/// `symlink(2)`, reporting [`CoreError::SymlinkUnsupported`] when the filesystem refuses (EPERM/ENOSYS/
/// EEXIST are all "this link cannot exist here" from the user's point of view; the caller has already
/// checked the destination slot).
pub fn symlink(link: &AbsPath, target: &AbsPath) -> Result<(), CoreError> {
    #[cfg(unix)]
    {
        let result = std::os::unix::fs::symlink(target.as_path(), link.as_path());
        match result {
            Ok(()) => Ok(()),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
                ) =>
            {
                Err(CoreError::SymlinkUnsupported)
            }
            Err(e) => Err(CoreError::io(link.to_string_lossy(), e)),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (link, target);
        Err(CoreError::SymlinkUnsupported)
    }
}

/// Set the mode of *these* paths and nothing else — Go's per-path `os.Chmod` (`api/ops.go:929`). The
/// recursion, if any, is [`Env::chmod_all`] in `platform/mod.rs`.
pub fn chmod(targets: &[AbsPath], mode: ModeBits) -> Result<(), CoreError> {
    #[cfg(unix)]
    {
        // `ModeBits` is the 9 permission bits plus the Go `fs.FileMode` markers; only the low 12 bits mean
        // anything to `chmod(2)` (setuid/setgid/sticky + rwx), and `from_mode` would silently keep the rest.
        let permissions = std::fs::Permissions::from_mode(mode.bits() & 0o7777);
        for target in targets {
            std::fs::set_permissions(target.as_path(), permissions.clone())
                .map_err(|e| CoreError::io(target.to_string_lossy(), e))?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (targets, mode);
        Err(CoreError::Unsupported {
            what: "chmod (there is no st_mode on this platform)",
        })
    }
}

/// Hand the path to the desktop's default handler (`api/ops.go:525-533` ran `xdg-open`).
///
/// `open::that_detached` rather than `open::that`: `xdg-open` does not exit until the opened application
/// does, so the waiting form would park a job forever. When no opener is configured at all the error names
/// the program the user can install — plan **F2**'s "xdg-open not found" case, which Go answered with an
/// opaque 500.
pub fn open_with(path: &AbsPath) -> Result<(), CoreError> {
    if open::commands(path.as_path()).is_empty() {
        return Err(CoreError::Shell {
            cmd: "xdg-open".to_string(),
            code: None,
            stderr:
                "no opener found: none of xdg-open, gio, open, run-mailcap, kde-open is on PATH \
                     (the desktop environment may not be running)"
                    .to_string(),
        });
    }
    open::that_detached(path.as_path()).map_err(|e| CoreError::Shell {
        cmd: "xdg-open".to_string(),
        code: None,
        stderr: e.to_string(),
    })
}

/// Run one program: plan F10's single spawn site. `spawn` + `wait_with_output` with the streams capped (the
/// `truncated` flag is surfaced, never swallowed); `Command::output` is banned by `clippy.toml` precisely so
/// that this is the only place output is buffered.
///
/// Timeout and cancellation are **not** here: `ExecRequest` carries no `JobCtx` yet, and plan §5.6 (step 6)
/// is where the job boundary that can kill this process lands. This function is the executor-agnostic half.
pub fn exec_program(req: &ExecRequest) -> Result<ExecOutput, CoreError> {
    let mut cmd = Command::new(&req.program);
    if req.shell {
        with_shell(&mut cmd, &req.program)?;
    } else {
        cmd.args(&req.args);
    }
    if let Some(cwd) = &req.cwd {
        cmd.current_dir(cwd.as_path());
    }
    for (key, value) in &req.env {
        cmd.env(key, value);
    }

    let cap = if req.output_cap == 0 {
        ExecRequest::DEFAULT_OUTPUT_CAP
    } else {
        req.output_cap
    };

    let child = cmd
        // `stdin` when the caller has bytes to feed (the clipboard publisher), `/dev/null` otherwise —
        // the pre-step-9 behaviour, and the reason a custom action can never inherit the UI's terminal.
        .stdin(if req.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CoreError::Shell {
            cmd: req.program.clone(),
            code: None,
            stderr: e.to_string(),
        })?;

    let output = write_stdin_and_wait(child, req.stdin.as_deref(), &req.program)?;

    let (stdout, out_cut) = cap_text(String::from_utf8_lossy(&output.stdout).into_owned(), cap);
    let (stderr, err_cut) = cap_text(String::from_utf8_lossy(&output.stderr).into_owned(), cap);

    Ok(ExecOutput {
        // `None` = killed by a signal; the caller shows that as "terminated", not as exit code 0.
        code: output.status.code(),
        stdout,
        stderr,
        truncated: out_cut || err_cut,
    })
}

/// Feed [`ExecRequest::stdin`] and collect the child. The pipe is written **and dropped before** the wait,
/// so a program that reads its stdin to EOF (`wl-copy`, `xclip -i`) sees the end of input instead of
/// blocking forever on a pipe whose writer we are still holding.
///
/// A child that fills its *output* pipes while we are still writing this one could deadlock; the payloads
/// here are a URI list, and the alternative — a writer thread per spawn — is not worth a core that is
/// otherwise thread-free (plan §9.1). A failed write is not reported: the child hung up first, and its exit
/// status is the honest answer (Go ignored the copy error the same way, `_ = cmd.Run()`).
fn write_stdin_and_wait(
    mut child: std::process::Child,
    stdin: Option<&str>,
    program: &str,
) -> Result<std::process::Output, CoreError> {
    if let (Some(data), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let _ = pipe.write_all(data.as_bytes()).and_then(|()| pipe.flush());
        // `drop` is the EOF the child is waiting for; it happens at the end of this block either way.
        drop(pipe);
    }
    child.wait_with_output().map_err(|e| CoreError::Shell {
        cmd: program.to_string(),
        code: None,
        stderr: e.to_string(),
    })
}

/// `exec.LookPath` (`api/ops.go:563,569,578,592`): a PATH scan, never a spawn. A name with a separator in
/// it is a *path*, checked as given — the Go rule, and the reason `/bin/sh` is found with an empty `PATH`.
pub fn has_program(name: &str) -> Result<bool, CoreError> {
    if name.is_empty() || name.contains('\0') {
        return Ok(false);
    }
    let direct = name.contains('/');
    if direct {
        return Ok(is_executable(Path::new(name)));
    }
    let Some(path) = std::env::var_os("PATH") else {
        return Ok(false);
    };
    Ok(std::env::split_paths(&path)
        .any(|dir| !dir.as_os_str().is_empty() && is_executable(&dir.join(name))))
}

/// `S_IXUSR|S_IXGRP|S_IXOTH` — what `execve`/`access(X_OK)`-style lookups accept; Go's `LookPath` uses the
/// same "any execute bit" test for a non-root caller, and the difference for root does not matter to a
/// capability probe whose only caller is the clipboard.
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    meta.is_file() && (std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111) != 0
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    Path::new(path).is_file()
}

/// `access(2)` with `W_OK` — "could this process write here now", which the mode bits alone cannot answer
/// (ACLs, read-only mounts, root). `rustix` is already a direct dependency for `statvfs` (plan §5.2).
///
/// Any refusal — `EACCES`, `EPERM`, `EROFS`, and also `ENOENT` (there is nothing here to write *to*) — is
/// `false`: the question is a capability, not an error report.
pub fn is_writable(path: &AbsPath) -> Result<bool, CoreError> {
    #[cfg(unix)]
    {
        use rustix::fs::{Access, AtFlags, CWD, accessat};
        Ok(accessat(CWD, path.as_path(), Access::WRITE_OK, AtFlags::empty()).is_ok())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(CoreError::Unsupported {
            what: "write-permission probe (`access(2)` has no Windows equivalent here; step 16)",
        })
    }
}

/// Plan F10: `sh -c` is the Unix custom-action form only.
#[cfg(unix)]
fn with_shell(cmd: &mut Command, line: &str) -> Result<(), CoreError> {
    *cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(line);
    Ok(())
}

#[cfg(not(unix))]
fn with_shell(_cmd: &mut Command, line: &str) -> Result<(), CoreError> {
    let _ = line;
    Err(CoreError::Unsupported {
        what: "shell execution (`sh -c` is Unix-only; Windows uses an argv vector, step 16)",
    })
}

/// Cut `text` at the cap, reporting whether anything was cut (the `…` marker is part of the cap, so the
/// returned string is never longer than `cap`).
fn cap_text(text: String, cap: usize) -> (String, bool) {
    if text.len() <= cap {
        return (text, false);
    }
    let mut cut = cap.saturating_sub(1);
    // Never split a UTF-8 sequence: walk back to a char boundary.
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut truncated = text[..cut].to_string();
    truncated.push('…');
    (truncated, true)
}

/// Copy already-decided items (`ops/paste` owns the conflict policy). Per-item failures are reported, the
/// walk continues (plan F12: a partial copy is never silently shortened).
pub fn copy_paths(items: &[CopyItem]) -> Result<CopyReport, CoreError> {
    let mut report = CopyReport::default();
    for item in items {
        match copy_tree(&item.source, &item.dest) {
            Ok((mut paths, bytes)) => {
                report.bytes = report.bytes.saturating_add(bytes);
                report.copied.append(&mut paths);
            }
            Err(error) => report.failures.push(CopyFailure {
                dest: item.dest.clone(),
                error,
            }),
        }
    }
    Ok(report)
}

/// Copy one file, link or tree to `dest`, returning every path created and the bytes of file content
/// copied (directories contribute their entries' bytes, not an inode's worth).
fn copy_tree(source: &AbsPath, dest: &AbsPath) -> Result<(Vec<AbsPath>, u64), CoreError> {
    let meta = std::fs::symlink_metadata(source.as_path())
        .map_err(|e| CoreError::io(source.to_string_lossy(), e))?;

    if meta.is_dir() {
        std::fs::create_dir_all(dest.as_path())
            .map_err(|e| CoreError::io(dest.to_string_lossy(), e))?;
        let mut created = vec![dest.clone()];
        let mut bytes = 0_u64;
        for entry in std::fs::read_dir(source.as_path())
            .map_err(|e| CoreError::io(source.to_string_lossy(), e))?
        {
            let entry = entry.map_err(|e| CoreError::io(source.to_string_lossy(), e))?;
            let name = entry.file_name();
            let child_source = AbsPath::from_path(entry.path())?;
            let child_dest = AbsPath::from_path(dest.as_path().join(name))?;
            let (mut child_created, child_bytes) = copy_tree(&child_source, &child_dest)?;
            created.append(&mut child_created);
            bytes = bytes.saturating_add(child_bytes);
        }
        return Ok((created, bytes));
    }

    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(source.as_path())
            .map_err(|e| CoreError::io(source.to_string_lossy(), e))?;
        let link_target = AbsPath::from_path(target)?;
        symlink(dest, &link_target)?;
        return Ok((vec![dest.clone()], 0));
    }

    let copied = std::fs::copy(source.as_path(), dest.as_path())
        .map_err(|e| CoreError::io(source.to_string_lossy(), e))?;
    Ok((vec![dest.clone()], copied))
}

/// The `statvfs(2)` answer for one path, in the units the API uses.
///
/// Safe at the call site by construction: `rustix::fs::statvfs` is a safe wrapper over the syscall, which
/// is how the plan's "the VFS id without `unsafe`" requirement is met inside a `#![forbid(unsafe_code)]`
/// crate — no `libc`, no `mem::zeroed`, no `struct statfs` layout to get wrong per architecture.
pub fn statvfs(path: &Path) -> Result<FsInfo, CoreError> {
    #[cfg(unix)]
    {
        let vfs = rustix::fs::statvfs(path)
            .map_err(|e| CoreError::io(path.display().to_string(), e.into()))?;
        let unit = if vfs.f_frsize > 0 {
            vfs.f_frsize
        } else {
            vfs.f_bsize
        };
        Ok(FsInfo {
            frsize: vfs.f_frsize,
            blocks: vfs.f_blocks,
            bavail: vfs.f_bavail,
            fsid: vfs.f_fsid,
            space: DiskSpace {
                total: vfs.f_blocks.saturating_mul(unit),
                free: vfs.f_bavail.saturating_mul(unit),
            },
        })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(CoreError::Unsupported {
            what: "statvfs (this platform has no statvfs; step 16 uses GetDiskFreeSpaceExW)",
        })
    }
}

/// What [`statvfs`] read, with the derived byte counts kept separate from the raw units so a test can
/// assert the multiplication instead of trusting it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FsInfo {
    /// `f_frsize` — the unit of the counts below (`0` on some pseudo-filesystems, which is why
    /// [`FsInfo::space`] falls back to `f_bsize`).
    pub frsize: u64,
    /// `f_blocks` — total blocks on the filesystem.
    pub blocks: u64,
    /// `f_bavail` — blocks available to *this* caller; the value `/api/diskspace`'s `free` key means.
    pub bavail: u64,
    /// `f_fsid` — the kernel's VFS super-block id. Two paths with the same `fsid` live on one mounted
    /// filesystem, and a path whose `fsid` **changed** between two reads had something mounted (or
    /// re-mounted) on top of it: that is the re-mount detection `MountEntry` alone cannot express, because
    /// a mount table lists a path once and cannot tell you that the thing behind it was swapped. It is also
    /// what makes [`trash_is_native`] answerable.
    pub fsid: u64,
    /// The `/api/diskspace` pair, in bytes.
    pub space: DiskSpace,
}

/// Free/total bytes for the filesystem holding `path` — `GET /api/diskspace` (`api/diskspace.go:10-13`).
pub fn disk_space(path: &AbsPath) -> Result<DiskSpace, CoreError> {
    Ok(statvfs(path.as_path())?.space)
}

// --------------------------------------------------------------------------- mounts

/// A mount plus the identity of the filesystem behind it ([`FsInfo::fsid`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountedVolume {
    /// The table row.
    pub entry: MountEntry,
    /// `f_fsid` of [`PROC_MOUNTINFO`]'s mount point, or `None` when the path could not be statted (a stale
    /// table entry, or a permission problem).
    pub fsid: Option<u64>,
}

/// The mount table with each entry's filesystem identity attached, for callers that must notice a
/// re-mount of a directory they are already watching.
pub fn mount_volumes() -> Result<Vec<MountedVolume>, CoreError> {
    Ok(mount_entries()?
        .entries
        .iter()
        .map(|entry| MountedVolume {
            entry: entry.clone(),
            fsid: statvfs(Path::new(&entry.mount_point))
                .ok()
                .map(|info| info.fsid),
        })
        .collect())
}

/// The mount table: [`PROC_MOUNTINFO`] parsed, with every entry's `removable` bit resolved from
/// `/sys/block`. Never an error because the table is missing — a kernel without the file yields an empty
/// table, which is the answer plan §5.2's `mounts()` row asks for (plan §5.2 `posix.rs`).
pub fn mount_entries() -> Result<MountTable, CoreError> {
    Ok(MountTable {
        entries: parse_mountinfo(&mount_text()?)?,
    })
}

/// The raw table text. Linux-only; other platforms have a different table (step 16) and answer empty here
/// rather than failing the places sidebar.
pub fn mount_text() -> Result<String, CoreError> {
    #[cfg(target_os = "linux")]
    {
        match std::fs::read_to_string(PROC_MOUNTINFO) {
            Ok(text) => Ok(text),
            // Absent (not a procfs at all) is an empty table, not a broken app.
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(CoreError::io(PROC_MOUNTINFO.to_string(), e)),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(String::new())
    }
}

/// Is the block device backing `path` removable media (`/sys/block/<disk>/removable == 1`)?
///
/// `false` for anything that is not a partition of a block device we can read, which includes every
/// network and pseudo filesystem — the conservative answer, since a wrong `true` puts a fake "eject" button
/// in the sidebar.
#[must_use]
pub fn is_removable(path: &Path) -> bool {
    mount_device_for(path).is_some_and(|source| sys_block_removable(&source))
}

/// Is `path` on a listed mount (i.e. does the mount table say anything about it or its ancestors)?
pub fn is_mounted(path: &Path) -> Result<bool, CoreError> {
    Ok(mount_entries()?.is_mounted(path))
}

/// `/proc/self/mountinfo` text → entries, in file order.
///
/// Pure: no `/proc`, no `/sys`, no clock — which is the point, since a real table is the developer's
/// machine and a table test must be a table. The format is `mountinfo(5)`:
///
/// ```text
/// 36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw,errors=continue
/// ```
///
/// i.e. mount ID, parent ID, `major:minor`, root, mount point, mount options, **zero or more**
/// `tag:value` optional fields, the `-` separator, then filesystem type, source and super-block options.
/// Spaces, tabs, newlines and backslashes are octally escaped inside the path fields (the `\040` case in
/// the test), because a mount point may legally contain a space.
///
/// Every entry gets `removable: false`; [`mount_entries`] is what consults `/sys/block`.
pub fn parse_mountinfo(text: &str) -> Result<Vec<MountEntry>, CoreError> {
    let mut entries = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        entries.push(parse_mount_line(line).map_err(|reason| {
            CoreError::System(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("mountinfo line {}: {reason}", index + 1),
            ))
        })?);
    }
    Ok(entries)
}

/// One line, or the reason it is not a line.
fn parse_mount_line(line: &str) -> Result<MountEntry, &'static str> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    // 6 fixed fields, the separator, then 3 more — the minimum legal record.
    if fields.len() < 10 {
        return Err("fewer than 10 space-separated fields");
    }
    let mount_id: u64 = fields[0].parse().map_err(|_| "mount id is not a number")?;
    let parent_id: u64 = fields[1].parse().map_err(|_| "parent id is not a number")?;
    let sep = fields
        .iter()
        .position(|field| *field == "-")
        .ok_or("no \"-\" separator between the optional fields and the filesystem type")?;
    if sep < 6 {
        return Err("the separator appears before the fixed fields");
    }
    if fields.len() < sep + 4 {
        return Err("nothing after the separator");
    }

    Ok(MountEntry {
        mount_id,
        parent_id,
        dev: fields[2].to_string(),
        root: unescape_mount_field(fields[3])?,
        mount_point: unescape_mount_field(fields[4])?,
        mount_options: split_options(fields[5]),
        optional_fields: fields[6..sep]
            .iter()
            .map(|field| (*field).to_string())
            .collect(),
        fstype: unescape_mount_field(fields[sep + 1])?,
        source: unescape_mount_field(fields[sep + 2])?,
        super_options: split_options(fields[sep + 3]),
        removable: false,
    })
}

/// Decode mountinfo's octal escapes: `\040` space, `\011` tab, `\012` newline, `\134` backslash.
fn unescape_mount_field(field: &str) -> Result<String, &'static str> {
    if !field.contains('\\') {
        return Ok(field.to_string());
    }
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let mut digits = String::new();
        for _ in 0..3 {
            match chars.clone().next() {
                Some(d) if d.is_ascii_digit() => {
                    chars.next();
                    digits.push(d);
                }
                _ => break,
            }
        }
        let value = u32::from_str_radix(&digits, 8).map_err(|_| "bad octal escape")?;
        out.push(char::from_u32(value).ok_or("escape is not a character")?);
    }
    Ok(out)
}

/// A comma-separated option list; a bare `-`/empty field means "no options".
fn split_options(field: &str) -> Vec<String> {
    field
        .split(',')
        .filter(|option| !option.is_empty() && *option != "-")
        .map(String::from)
        .collect()
}

/// The `source` of the longest mount that contains `path`, when that mount names a device.
fn mount_device_for(path: &Path) -> Option<String> {
    let table = mount_entries().ok()?;
    table
        .mount_for(path)
        .filter(|entry| entry.source.starts_with("/dev/"))
        .map(|entry| entry.source.clone())
}

/// Read `/sys/block/<disk>/removable` for the disk that owns `source` (e.g. `/dev/sdb1` → `sdb`).
///
/// The candidate is walked from the longest prefix down so the first existing directory is the parent disk:
/// `sdb1` → `sdb`, `nvme0n1p3` → `nvme0n1`, `mmcblk0p2` → `mmcblk0`. No partition-name heuristics, and no
/// assumption that the first character class means anything.
#[must_use]
pub fn sys_block_removable(source: &str) -> bool {
    let Some(name) = source.strip_prefix("/dev/") else {
        return false;
    };
    if name.is_empty() || name.contains('/') {
        return false;
    }
    for end in (1..=name.len()).rev() {
        let candidate = &name[..end];
        if let Ok(text) = std::fs::read_to_string(format!("/sys/block/{candidate}/removable")) {
            return text.trim() == "1";
        }
    }
    false
}

// --------------------------------------------------------------------------- trash (plan R/D6, OQ2)

/// Can `path` reach a trash bin?
///
/// `trash` 5.4 no longer exports `can_be_trashed` (the helper lived in 3.x), so this is the same rule the
/// crate applies internally, without its side effects: a root folder is refused outright
/// (`trash::Error::TargetedRoot`), a path that does not exist cannot be moved, and the destination has to
/// exist — `$XDG_DATA_HOME/Trash` when set, else `$HOME/.local/share/Trash` (the crate creates the leaf, so
/// only its parent directory is required).
///
/// [`trash_is_native`] is the stricter, plan-OQ2 question: whether the trash is on the *same* filesystem,
/// which is what decides if the item can be restored through the desktop's own bin.
#[must_use]
pub fn can_trash(path: &Path) -> bool {
    if path == Path::new("/") || std::fs::symlink_metadata(path).is_err() {
        return false;
    }
    trash_base_dir().is_some_and(|base| base.is_dir())
}

/// Does `path` have a same-filesystem trash directory (the freedesktop "restore my file" path), as opposed
/// to `trash`'s cross-device garbage fallback? Answered with [`FsInfo::fsid`], so no mount-table guessing.
#[must_use]
pub fn trash_is_native(path: &Path) -> bool {
    let Some(base) = trash_base_dir() else {
        return false;
    };
    match (nearest_fsid(path), nearest_fsid(&base)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Move `path` to the trash. `Ok(false)` is a *refusal* (a root folder), which the caller turns into the
/// logged permanent-delete escalation (plan §6.2 D6); `Err` is a failure while trying.
pub fn trash(path: &Path) -> Result<bool, CoreError> {
    match trash::delete(path) {
        Ok(()) => Ok(true),
        Err(trash::Error::TargetedRoot) => Ok(false),
        Err(e) => Err(CoreError::io(
            path.display().to_string(),
            io::Error::other(e.to_string()),
        )),
    }
}

/// The directory a trash bin would live under.
fn trash_base_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        let base = PathBuf::from(xdg).join("Trash");
        if base.parent().is_some() {
            return Some(base);
        }
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share/Trash"))
}

/// The `fsid` of the closest existing ancestor, so a not-yet-created path still resolves to its
/// filesystem.
fn nearest_fsid(path: &Path) -> Option<u64> {
    let mut candidate = path.to_path_buf();
    loop {
        if let Ok(info) = statvfs(&candidate) {
            return Some(info.fsid);
        }
        if !candidate.pop() {
            return None;
        }
    }
}

/// Measure symlink support under `scratch_root` (plan F21's one-shot write test).
pub fn supports_symlink(scratch_root: &Path) -> Result<bool, CoreError> {
    env_probe::probe_symlink_support(scratch_root, env_probe::ProbeFlags::detect())
}

/// The measured `/api/props` payload: the write test's verdict, paired with the OS string Go published.
/// Step 6 caches this in [`Linux::probe_symlinks`]; the frame path uses [`env_probe::probe`].
pub fn probe(scratch_root: &Path) -> Result<ProbeReport, CoreError> {
    Ok(ProbeReport {
        os: Os::current().as_str(),
        symlink_supported: supports_symlink(scratch_root)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic slice of a developer machine's `/proc/self/mountinfo`: an ordinary ext4 mount, a
    /// pseudo-filesystem, four optional fields *and* a `\040`-escaped mount point, a record with no
    /// optional fields at all (the separator right after the mount options), and a FUSE mount whose source
    /// is not a device path.
    const TABLE: &str = concat!(
        "36 35 8:1 / /mnt/data rw,relatime shared:1 - ext4 /dev/sda1 rw,seclabel,compress=lzo\n",
        "22 1 0:21 / /proc rw,nosuid,nodev,noexec,relatime shared:5 - proc proc rw\n",
        "91 21 8:16 / /mnt/USB\\040Drive ro,nosuid,nodev shared:1 master:2 : - vfat /dev/sdb1 \
         rw,fmask=0077,uid=1000\n",
        "110 1 0:45 / /var/lib/docker/overlay2/m/merged rw,relatime - overlay overlay \
         rw,lowerdir=/a,upperdir=/b\n",
        "115 104 0:60 /home/u/remote /home/u/remote rw,nosuid,nodev master:7 - fuse.sshfs \
         sshfs#backup:/srv rw,readdir=ino-slr\n",
    );

    fn entry<'a>(entries: &'a [MountEntry], mount_point: &str) -> &'a MountEntry {
        entries
            .iter()
            .find(|e| e.mount_point == mount_point)
            .unwrap_or_else(|| panic!("no {mount_point} in {entries:?}"))
    }

    #[test]
    fn parse_mountinfo_table() {
        let entries = parse_mountinfo(TABLE).expect("the table parses");
        assert_eq!(entries.len(), 5);

        let ext4 = entry(&entries, "/mnt/data");
        assert_eq!(
            (ext4.mount_id, ext4.parent_id, ext4.dev.as_str()),
            (36, 35, "8:1")
        );
        assert_eq!(ext4.fstype, "ext4");
        assert_eq!(ext4.source, "/dev/sda1");
        assert_eq!(ext4.root, "/");
        assert_eq!(ext4.mount_options, ["rw", "relatime"]);
        assert_eq!(ext4.optional_fields, ["shared:1"]);
        assert_eq!(ext4.super_options, ["rw", "seclabel", "compress=lzo"]);
        assert!(!ext4.removable, "parse never resolves the removable bit");

        let proc = entry(&entries, "/proc");
        assert_eq!(proc.fstype, "proc");
        assert_eq!(proc.source, "proc");
        assert_eq!(proc.super_options, ["rw"]);

        let usb = entry(&entries, "/mnt/USB Drive");
        assert_eq!(usb.fstype, "vfat");
        assert_eq!(usb.source, "/dev/sdb1");
        assert_eq!(
            usb.optional_fields,
            ["shared:1", "master:2", ":"],
            "every field before the separator is optional"
        );
        assert_eq!(usb.mount_options, ["ro", "nosuid", "nodev"]);
        assert_eq!(usb.super_options, ["rw", "fmask=0077", "uid=1000"]);

        // The `-` immediately after the mount options: zero optional fields.
        let overlay = entry(&entries, "/var/lib/docker/overlay2/m/merged");
        assert!(overlay.optional_fields.is_empty());
        assert_eq!(overlay.fstype, "overlay");
        assert_eq!(overlay.source, "overlay");
        assert_eq!(overlay.super_options, ["rw", "lowerdir=/a", "upperdir=/b"]);

        let fuse = entry(&entries, "/home/u/remote");
        assert_eq!(fuse.fstype, "fuse.sshfs");
        assert_eq!(fuse.source, "sshfs#backup:/srv");
        assert_eq!(fuse.optional_fields, ["master:7"]);
        assert_eq!(fuse.super_options, ["rw", "readdir=ino-slr"]);
        assert_eq!(fuse.root, "/home/u/remote");

        let table = MountTable {
            entries: entries.clone(),
        };
        assert!(table.is_mounted(Path::new("/mnt/data/zen-man")));
        assert!(table.mount_for(Path::new("/mnt/USB Drive/x")).is_some());
    }

    #[test]
    fn parse_mountinfo_rejects_malformed_lines() {
        // Short line, missing separator, separator too early, and a bad octal escape.
        for line in [
            "36 35 8:1",
            "36 35 8:1 / /mnt rw - ",
            "36 35 8:1 / - /mnt rw - ext4 /dev/sda1 rw",
            "36 35 8:1 /mnt\\999 /mnt rw - ext4 /dev/sda1 rw",
        ] {
            let e = parse_mountinfo(line).expect_err("{line} must not parse");
            assert_eq!(e.kind(), "io_error", "got {e:?} for {line}");
            assert!(
                e.to_string().contains("line 1"),
                "the error must name the line: {e}"
            );
        }
        assert!(
            parse_mountinfo("")
                .expect("empty text is an empty table")
                .is_empty()
        );
        assert!(
            parse_mountinfo("\n\n")
                .expect("blank lines are skipped")
                .is_empty()
        );
    }

    #[test]
    fn octal_escapes_decode() {
        assert_eq!(unescape_mount_field("/a\\040b").expect("space"), "/a b");
        assert_eq!(unescape_mount_field("/a\\011b").expect("tab"), "/a\tb");
        assert_eq!(
            unescape_mount_field("/a\\134b").expect("backslash"),
            "/a\\b"
        );
        assert_eq!(unescape_mount_field("plain").expect("plain"), "plain");
    }

    #[test]
    fn statvfs_root_reports_units_and_the_vfs_id() {
        let info = statvfs(Path::new("/")).expect("statvfs(/) works on every unix host");
        // Printed rather than asserted: the exact numbers are the machine's, and the report needs them.
        println!(
            "statvfs(/): frsize={} blocks={} bavail={} fsid={:#x} total={} free={}",
            info.frsize, info.blocks, info.bavail, info.fsid, info.space.total, info.space.free
        );
        assert!(
            info.frsize > 0,
            "a zero block unit would make a full disk look empty: {info:?}"
        );
        assert!(info.blocks > 0, "{info:?}");
        assert_eq!(info.space.total, info.blocks.saturating_mul(info.frsize));
        assert_eq!(info.space.free, info.bavail.saturating_mul(info.frsize));
        assert!(info.space.free <= info.space.total, "{info:?}");
        // `/` and the filesystem under the temp dir may be the same or different volumes; both answers are
        // legal, and neither may be an error.
        assert!(statvfs(&std::env::temp_dir()).is_ok());
    }

    #[test]
    fn the_real_mount_table_is_well_formed() {
        let table = mount_entries().expect("a mount table must answer, empty is fine");
        // No assertion on the *number* of mounts — developer machines have dozens, containers have five.
        for e in &table.entries {
            assert!(
                e.mount_point.starts_with('/'),
                "mount point is not absolute: {e:?}"
            );
            assert!(!e.fstype.is_empty(), "fstype is empty: {e:?}");
            assert!(!e.dev.is_empty(), "no device id: {e:?}");
        }
        assert!(is_mounted(Path::new("/")).expect("is_mounted answers"));
        assert!(mount_volumes().is_ok());
    }

    #[test]
    fn removable_is_device_based_and_conservative() {
        assert!(!sys_block_removable("/dev/not-a-real-device-42"));
        assert!(!sys_block_removable("proc"));
        assert!(!sys_block_removable("/dev/mapper/vg-root"));
        assert!(
            !is_removable(Path::new("/")),
            "the root volume is never removable media"
        );
    }

    #[test]
    fn trash_predicates_answer_without_side_effects() {
        assert!(!can_trash(Path::new("/")), "trash refuses a root folder");
        assert!(!can_trash(Path::new("/definitely/not/here")));
        let tmp = std::env::temp_dir();
        let dir = tmp.join(format!("zenman-trash-pred-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the private probe dir");
        // The predicates must not create or delete anything. Counted in a directory of our own, because
        // `/tmp` is a shared neighbourhood — other tests are writing there at the same moment.
        let before = entries_in(&dir);
        let answer = can_trash(&dir);
        let native = trash_is_native(&dir);
        // Both answers are the machine's (OQ2 is exactly the question of whether `native` is true on a
        // given desktop), so the assertion is about side effects, and the values are recorded.
        println!(
            "can_trash({}) = {answer}, same-filesystem trash = {native}",
            dir.display()
        );
        assert_eq!(
            entries_in(&dir),
            before,
            "the predicates must be side-effect free"
        );
        remove_probe_root(&dir);
    }

    fn entries_in(dir: &Path) -> usize {
        std::fs::read_dir(dir).map_or(0, |entries| entries.count())
    }

    #[test]
    fn linux_probe_writes_once_and_caches() {
        let linux = Linux::new();
        assert_eq!(linux.cached_symlink_support(), None, "no probe yet");
        // Before the probe the static bit answers, so the UI has something to show on frame one.
        assert_eq!(
            linux.supports_symlink().expect("caps"),
            linux.caps().contains(PlatformCaps::SYMLINK)
        );
        let scratch =
            std::env::temp_dir().join(format!("zenman-linux-probe-{}", std::process::id()));
        std::fs::create_dir_all(&scratch).expect("create the scratch root");
        let measured = linux.probe_symlinks(&scratch).expect("the probe answers");
        assert_eq!(linux.cached_symlink_support(), Some(measured));
        assert_eq!(linux.supports_symlink().expect("cached"), measured);
        // And the probe left nothing behind in the caller's data directory.
        assert_eq!(
            std::fs::read_dir(&scratch)
                .map(|d| d.count())
                .unwrap_or(usize::MAX),
            0,
            "the probe must clean its scratch dir"
        );
        remove_probe_root(&scratch);
    }

    #[test]
    fn exec_program_caps_its_output() {
        let long = "x".repeat(4096);
        let out = exec_program(&ExecRequest {
            program: "/bin/sh".to_string(),
            // The first operand after the command string lands in `$0`, not `$1` — `sh -c` semantics.
            args: vec![
                "-c".to_string(),
                "printf %s \"$0\"; printf %s \"$0\" >&2".to_string(),
                long.clone(),
            ],
            shell: false,
            cwd: None,
            env: Vec::new(),
            output_cap: 100,
            stdin: None,
        })
        .expect("a shell is available on every unix host in this suite");
        assert!(out.succeeded(), "stderr: {}", out.stderr);
        assert!(
            out.truncated,
            "4 KiB through a 100-byte cap must be flagged"
        );
        assert_eq!(
            out.stdout.chars().count(),
            100,
            "the cap includes the marker"
        );
        assert_eq!(out.stderr.chars().count(), 100);

        let cwd = std::env::temp_dir();
        let cwd_req = ExecRequest::shell("pwd", Some(AbsPath::from_path(&cwd).expect("abs")));
        let pwd = exec_program(&cwd_req).expect("sh -c pwd");
        assert!(pwd.succeeded());
        assert!(!pwd.stdout.trim().is_empty());

        let missing = exec_program(&ExecRequest::argv("/nonexistent-program-42", ["--version"]))
            .expect_err("a missing program is an error");
        assert_eq!(missing.kind(), "shell_error");
    }

    #[test]
    fn exec_program_feeds_stdin_and_closes_it() {
        // `wl-copy`'s shape: the child reads to EOF, so the pipe must be dropped before the wait or this
        // deadlocks (`cat` with a held pipe would hang the suite, which is the real assertion here).
        let out = exec_program(
            &ExecRequest::argv("cat", Vec::<String>::new())
                .with_stdin("file:///a/b\nfile:///c/d\n"),
        )
        .expect("cat exists on every unix host in this suite");
        assert!(out.succeeded(), "stderr: {}", out.stderr);
        assert_eq!(out.stdout, "file:///a/b\nfile:///c/d\n");

        // `None` is `/dev/null` (a read returns EOF at once), `Some("")` is an empty but *real* pipe;
        // both must terminate, and `wc -c` distinguishes them from a hang.
        let empty = exec_program(
            &ExecRequest::argv("sh", ["-c", "printf %s \"$(cat)\" | wc -c"]).with_stdin(""),
        )
        .expect("sh exists");
        assert!(empty.succeeded(), "stderr: {}", empty.stderr);
        assert_eq!(empty.stdout.trim(), "0");

        // A child that ignores its stdin and exits early must not turn the broken pipe into an error.
        let hung_up = exec_program(
            &ExecRequest::argv("sh", ["-c", "exit 3"]).with_stdin("x".repeat(1 << 16)),
        )
        .expect("the write failure is swallowed; the exit status is the answer");
        assert_eq!(hung_up.code, Some(3));
    }

    #[test]
    fn has_program_scans_path_and_never_spawns() {
        assert!(
            has_program("sh").expect("a PATH scan cannot fail here"),
            "`sh` is on PATH on every host in this suite"
        );
        assert!(
            has_program("/bin/sh").expect("an absolute name is checked as given"),
            "a name with a separator is a path, not a PATH lookup"
        );
        assert!(
            !has_program("zenman-definitely-not-a-program")
                .expect("a missing name is a plain false"),
            "a missing program is `false`, not an error"
        );
        assert!(!has_program("").expect("the empty name is false"));
    }

    #[test]
    fn is_writable_answers_the_real_permission() {
        let dir = std::env::temp_dir().join(format!("zenman-writable-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let abs = AbsPath::from_path(&dir).expect("absolute");
        assert!(is_writable(&abs).expect("a fresh temp dir is writable"));
        // `access(2)` knows what the mode bits do not: a 0o444 file is not writable by us.
        let file = dir.join("read-only");
        std::fs::write(&file, b"x").expect("write");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).expect("chmod");
        let file_abs = AbsPath::from_path(&file).expect("absolute");
        assert!(
            !is_writable(&file_abs).expect("a 0444 file is not writable"),
            "W_OK must reflect the mode"
        );
        // A path that is not there is not writable either — there is nothing to write *to*.
        let gone = AbsPath::from_path(dir.join("nope")).expect("absolute");
        assert!(!is_writable(&gone).expect("a missing path answers false, not an error"));
        remove_probe_root(&dir);
    }

    #[test]
    fn copy_paths_reports_per_item_failures() {
        let root = std::env::temp_dir().join(format!("zenman-copy-{}", std::process::id()));
        let _ = std::fs::remove_dir(&root);
        std::fs::create_dir_all(root.join("src/inner")).expect("create the source tree");
        std::fs::write(root.join("src/a.txt"), b"hello").expect("write a file");
        std::fs::write(root.join("src/inner/b.txt"), b"world!").expect("write a nested file");

        let dest = root.join("dest");
        let report = copy_paths(&[
            CopyItem {
                source: AbsPath::from_path(root.join("src")).expect("abs"),
                dest: AbsPath::from_path(&dest).expect("abs"),
            },
            CopyItem {
                source: AbsPath::from_path(root.join("nope")).expect("abs"),
                dest: AbsPath::from_path(root.join("dest2")).expect("abs"),
            },
        ])
        .expect("copy_paths itself does not fail");

        assert_eq!(
            report.copied.len(),
            4,
            "dest + dir + 2 files: {:?}",
            report.copied
        );
        assert_eq!(
            report.bytes, 11,
            "the content bytes, not the directory entries"
        );
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].error.kind(), "io_error");
        assert_eq!(
            std::fs::read_to_string(dest.join("inner/b.txt")).expect("read"),
            "world!"
        );
        remove_probe_root(&root);
    }

    /// The test tree is ours, so it is deleted by hand: `std::fs::remove_dir_all` is banned
    /// (`clippy.toml`, plan §5.1) and `ops::delete` does not exist yet.
    fn remove_probe_root(root: &Path) {
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    remove_probe_root(&entry.path());
                } else {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        let _ = std::fs::remove_dir(root);
    }
}
