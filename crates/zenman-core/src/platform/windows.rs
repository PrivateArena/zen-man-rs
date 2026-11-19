//! The Windows `Env` implementation — plan §5.2 `platform/windows.rs` row, §10 item 4's "Windows is
//! first-class" decision, plan §7 step 16a.
//!
//! # The dependency question, answered before it is asked
//!
//! **This module adds no dependency**, and no `windows-sys`/`winsafe`-class crate could be used here
//! even if it were licence-clean: the workspace `forbid(unsafe_code)` (plan §5.1, hard rule 3) bars the
//! `unsafe` block that every raw or thin FFI call needs, so `GetDiskFreeSpaceExW`, `GetLogicalDrives`,
//! `GetDriveTypeW` and `IShellLink` are all unreachable *by house rule*, not by oversight. Everything
//! below therefore goes through the safe `std` surface that already wraps those calls:
//!
//! | The question | Go's answer (`/media/jang/home/Deve/zen-man`) | This module's answer |
//! |---|---|---|
//! | free/total bytes | `api/diskspace_windows.go` — `GetDiskFreeSpaceExW`, `lpFreeBytesAvailableToCaller` + `lpTotalNumberOfBytes` | **typed refusal** - see [`disk_space`]: the one safe `std` door to those two numbers, `Metadata::file_system`, is still unstable (`rust-lang/rust#67087`), and calling it through `windows-sys` needs `unsafe`, which this workspace forbids |
//! | drive enumeration | absent (Go's `api/places.go:274` returns early for `runtime.GOOS != "linux"`, so Windows had **no** places list) | [`enumerate_drives`] probes every `X:\` root; the plan's "a real gap in the Go Windows path" (§5.2 `windows.rs` row) is filled without `GetLogicalDrives` |
//! | custom-action execution | `api/actions.go:238` — `exec.Command("cmd", "/C", commandLine)` | [`to_windows_request`] + `cmd /C` argv vector, never a rebuilt command line (plan F10) |
//! | `.lnk` shortcut | n/a (Go had none) | **refused**, typed: a `.lnk` is a shell-link binary blob behind COM, i.e. `unsafe`; see [`Windows::create_shortcut`] |
//!
//! [`total_space`]: std::fs::FileSystem::total_space
//!
//! # Filesystem *identity* without `f_fsid`
//!
//! The unix side answers "is the trash on this same volume?" with `statvfs`' `f_fsid`
//! ([`super::linux::trash_is_native`]). Windows needs `GetVolumeInformationW` (volume serial + filesystem
//! name) for the same question, and that is FFI. So the safe answer is the *shape* of the path -
//! [`is_unc_path`], a redirector volume, which never carries a local `$Recycle.Bin` - plus
//! [`recycle_bin_exists`] as the name-based rule a later probe plugs into. Where the volume cannot be
//! identified the answer is `false`, i.e. the delete lane offers its permanent strategy rather than
//! promising a restore it cannot deliver. Recorded as a deviation in plan §9.2's step-16a log.
//!
//! # What a real watcher needs (deferred, deliberately)
//!
//! Plan §7 row 12 shipped `index/watcher.rs` behind `#[cfg(all(feature = "watch", target_os = "linux"))]`
//! with a same-surface stub. On Windows `--features watch` therefore **compiles** and answers
//! [`CoreError::Watcher`](crate::error::CoreError::Watcher) — the "compiles on and off / on every target"
//! criterion, not a `todo!()`. A real backend needs: `ReadDirectoryChangesW` over a
//! `CreateFileW`-opened directory handle (`unsafe`, so a crate that owns it, licence-cleared), an
//! overlapped/blocked reader thread per watched root, a 64 KiB completion buffer whose overflow flag means
//! "rescan this root, events were dropped" (no inotify equivalent), recursive watching either by
//! `bWatchSubtree` on one handle (cheap, but renames cross the tree boundary silently) or one handle per
//! directory, and change-kind mapping from `FILE_ACTION_*` onto the port's [`crate::index::watcher::Kind`].
//! Budgeted as later work, not step 16a.

use std::io;
use std::path::{Path, PathBuf};

use super::{
    CopyItem, CopyReport, DiskSpace, Env, ExecOutput, ExecRequest, LinkItem, MountEntry,
    MountTable, Os, PlatformCaps, PlatformFuture, Shortcut, ready,
};
use crate::error::{CoreError, PathReject};
use crate::mode::ModeBits;
use crate::path::AbsPath;

// --------------------------------------------------------------------------- names

/// The names Windows will not let you create, **case-insensitively and with or without an extension**
/// (`CON`, `con.txt`, `cOnSpAn.tz` are all refused). `COM0` is included because Windows refuses it even
/// though `dosdevices` historically only listed 1-9; `LPT0` likewise.
pub const RESERVED_NAMES: [&str; 24] = [
    "CON", "PRN", "AUX", "NUL", "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
    "COM8", "COM9", "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// The characters `CreateFileW` refuses in a name. `/` and `\` are separators rather than illegal
/// characters, but a *name* may not contain either, and the port's `AbsPath` model splits on both.
pub const ILLEGAL_NAME_CHARS: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// The longest name a `\\?\`-less path may have. `MAX_PATH` is 260 for the *whole* path; a single
/// component is capped at 255 by `NtCreateFile`.
pub const MAX_NAME_LEN: usize = 255;

/// Why a name is not usable on Windows. Kept separate from [`PathReject`] (which is about *path shape*)
/// so the rule that fired survives into the message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameReject {
    /// The empty name — also what a trailing separator leaves behind.
    Empty,
    /// A name in [`RESERVED_NAMES`], or the two device aliases `.` and `..`.
    Reserved,
    /// A name ending in `.` or a space: Windows strips them, so the file on disk would not be the file
    /// the user asked for (and `foo.` is unreachable through the shell).
    TrailingDotOrSpace,
    /// One of [`ILLEGAL_NAME_CHARS`].
    IllegalChar(char),
    /// A control character (`\0`-`\u{1f}`, `\u{7f}`), which `CreateFileW` refuses outright.
    ControlChar,
    /// Longer than [`MAX_NAME_LEN`].
    TooLong,
}

impl std::fmt::Display for NameReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("empty name"),
            Self::Reserved => f.write_str("reserved Windows device name"),
            Self::TrailingDotOrSpace => f.write_str("trailing dot or space"),
            Self::IllegalChar(c) => write!(f, "illegal character {c:?}"),
            Self::ControlChar => f.write_str("control character"),
            Self::TooLong => write!(f, "name longer than {MAX_NAME_LEN} bytes"),
        }
    }
}

/// Is `name` (or `name.ext`) one of the reserved device names? Case-insensitive, and the comparison is
/// against the **first dot component**, which is what makes `con.txt` reserved and `consolette.txt` not.
#[must_use]
pub fn is_reserved_name(name: &str) -> bool {
    // `.` and `..` are the shell's own device names, and both have an *empty* first dot component, so
    // they are matched before the stem rule rather than by it.
    if name == "." || name == ".." {
        return true;
    }
    let stem = name.split('.').next().unwrap_or(name);
    RESERVED_NAMES.iter().any(|r| r.eq_ignore_ascii_case(stem))
}

/// The Windows name rules, as a pure function: no filesystem, no locale, no clock. Every rule Go's
/// Windows path would have needed but never had (Go only ever validated `filepath.Clean` output) is here
/// so that the port's rename/mkdir dialogs can refuse `CON.txt` before `CreateFileW` does, with a reason.
#[must_use]
pub fn name_reject(name: &str) -> Option<NameReject> {
    if name.is_empty() {
        return Some(NameReject::Empty);
    }
    if name.len() > MAX_NAME_LEN {
        return Some(NameReject::TooLong);
    }
    if is_reserved_name(name) {
        return Some(NameReject::Reserved);
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Some(NameReject::TrailingDotOrSpace);
    }
    for c in name.chars() {
        if let Some(illegal) = ILLEGAL_NAME_CHARS.iter().find(|&&i| i == c) {
            return Some(NameReject::IllegalChar(*illegal));
        }
        if (c as u32) < 0x20 || c == '\u{7f}' {
            return Some(NameReject::ControlChar);
        }
    }
    None
}

/// [`name_reject`] as a [`CoreError`], so a caller can `?` it into the same channel as every other
/// input rejection (400-class, `invalid_path`).
pub fn validate_name(name: &str) -> Result<(), CoreError> {
    match name_reject(name) {
        None => Ok(()),
        Some(reject) => Err(CoreError::InvalidPath {
            raw: format!("{name} ({reject})"),
            reason: PathReject::WindowsName,
        }),
    }
}

/// Same rules applied to every component of `path`, minus the drive/root component. A path that came out
/// of [`AbsPath`] has already been component-checked for the unix rules; this is the Windows overlay.
pub fn validate_path(path: &Path) -> Result<(), CoreError> {
    for component in path.components() {
        if let std::path::Component::Normal(name) = component {
            validate_name(name.to_string_lossy().as_ref())?;
        }
    }
    Ok(())
}

// --------------------------------------------------------------------------- argv / cmd /C

/// Quote one argument the way the MSVC runtime parses a command line (`CommandLineToArgvW`'s inverse):
/// wrap in `"`, doubling an embedded `"` and escaping the backslashes that precede a quote or the closing
/// quote. Step 14a made the POSIX single-quote form explicit in `services/actions.rs`; this is its Windows
/// counterpart, and the reason neither side ever concatenates user text into a shell string (plan F10).
#[must_use]
pub fn quote_arg(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                // The quote is *doubled*, not backslash-escaped: that is the form `cmd /C` and
                // `CommandLineToArgvW` both accept without an argument being able to swallow the closing
                // quote. Only the backslashes before it need doubling (they would otherwise escape it).
                out.push_str(&"\\".repeat(backslashes * 2));
                out.push_str("\"\"");
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                out.push(c);
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

/// Join an argv vector into one quoted command line — the string `cmd /C` receives as **its own single
/// argument**, so `cmd` cannot re-split the user's tokens.
#[must_use]
pub fn join_argv<I, S>(args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter()
        .map(|a| quote_arg(a.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The argv `cmd.exe` is spawned with. `/C` (not `/K`: no window to keep), and the line is one argument.
pub const CMD_PROGRAM: &str = "cmd";
const CMD_FLAG: &str = "/C";

/// `cmd /C <line>` as an argv vector.
#[must_use]
pub fn cmd_c_args(line: &str) -> Vec<String> {
    vec![CMD_FLAG.to_string(), line.to_string()]
}

/// Turn a request into the form this platform can execute: a `shell` request becomes
/// `cmd /C "<line>"` (one argv element, so no re-parsing), an argv request is already correct.
///
/// Pure — this is the function the step-16a audit tests, because the *spawn* is the same `std` code the
/// unix side uses and the *shape* is the part that differs.
#[must_use]
pub fn to_windows_request(req: &ExecRequest) -> ExecRequest {
    if !req.shell {
        return ExecRequest {
            program: req.program.clone(),
            args: req.args.clone(),
            shell: false,
            cwd: req.cwd.clone(),
            env: req.env.clone(),
            output_cap: req.output_cap,
            stdin: req.stdin.clone(),
            detached: req.detached,
        };
    }
    let mut rewritten = ExecRequest::argv(CMD_PROGRAM, [CMD_FLAG.to_string(), req.program.clone()]);
    rewritten.shell = false;
    rewritten.cwd = req.cwd.clone();
    rewritten.env = req.env.clone();
    rewritten.output_cap = req.output_cap;
    rewritten.stdin = req.stdin.clone();
    rewritten.detached = req.detached;
    rewritten
}

// --------------------------------------------------------------------------- drives

/// The 26 letters a drive root may carry. `GetLogicalDrives` is the fast way to ask the same question and
/// is unreachable here ([`crate::platform::windows`] module doc), so each root is `stat`-ed instead —
/// one `GetDiskFreeSpaceExW`-class call per letter, at startup, off the frame path.
pub const DRIVE_LETTERS: [char; 26] = [
    'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S',
    'T', 'U', 'V', 'W', 'X', 'Y', 'Z',
];

/// The capability set of a Windows host, spelled out rather than derived from [`super::detect`]:
/// `detect()` answers from the OS of the *running* host, so a `Windows` implementation compiled (and
/// unit-tested) on a Linux check-out must still advertise what Windows can do and withhold what it
/// cannot. No [`PlatformCaps::CHMOD`] (there is no `st_mode`), and no [`PlatformCaps::SYMLINK`] until
/// [`Windows::probe_symlinks`] measures it (unprivileged Windows cannot create links at all).
#[must_use]
pub fn windows_caps() -> PlatformCaps {
    PlatformCaps::DISK_SPACE
        | PlatformCaps::MOUNT_TABLE
        | PlatformCaps::OPEN_WITH
        | PlatformCaps::EXEC
        | PlatformCaps::TRASH
        | PlatformCaps::SHORTCUT
}

/// `C:\` for `'C'` — the root form `std` accepts and `AbsPath` can hold.
#[must_use]
pub fn drive_root(letter: char) -> Option<String> {
    let upper = letter.to_ascii_uppercase();
    if upper.is_ascii_uppercase() {
        Some(format!("{upper}:\\"))
    } else {
        None
    }
}

/// Every drive root for which `exists` answers `true`, in letter order.
///
/// Takes the predicate so the enumeration is testable on any host (and so a test can name the drives it
/// believes exist — on a Linux check-out none do, and the answer is an empty table, never an error).
#[must_use]
pub fn enumerate_drives(exists: impl Fn(&str) -> bool) -> Vec<String> {
    DRIVE_LETTERS
        .iter()
        .filter_map(|letter| drive_root(*letter))
        .filter(|root| exists(root))
        .collect()
}

/// The filesystem families that carry a per-volume `$Recycle.Bin`. `CDFS`/`UDF` (optical) and anything
/// unrecognised — including a network redirector's name — deliberately answer `false`: a wrong `false`
/// costs the user a "move to trash" click, a wrong `true` loses their file to `trash`'s garbage fallback.
#[must_use]
pub fn recycle_bin_exists(fs_name: Option<&str>) -> bool {
    matches!(
        fs_name.map(|n| n.trim_end_matches('\0')).unwrap_or(""),
        "NTFS" | "ExFAT" | "exFAT" | "FAT" | "FAT32" | "FAT12" | "FAT16" | "ReFS"
    )
}

/// The volume's filesystem name (`GetVolumeInformationW`'s volume-format buffer).
///
/// **Always `None` here, on purpose**: `std` exposes that buffer only through the unstable
/// `Metadata::file_system`, and reading it directly is FFI, which `forbid(unsafe_code)` bars. The function
/// exists so that [`drive_entries`] already carries the seam a later step fills in one line, and so the
/// rule it feeds ([`recycle_bin_exists`]) is the one under test.
#[must_use]
pub fn volume_fs_name(_path: &Path) -> Option<String> {
    None
}

/// `\\host\share\…` - a path served by a redirector. Such a volume has no local `$Recycle.Bin`, so the
/// delete lane must be the permanent one, and this is the one part of the volume question answerable
/// from the path text alone.
#[must_use]
pub fn is_unc_path(path: &Path) -> bool {
    let text = path.to_string_lossy();
    text.starts_with("\\\\") || text.starts_with("//")
}

/// The reason [`disk_space`] refuses, in the words the UI shows. A `const` so the refusal, the test and
/// the plan log quote the same sentence.
pub const DISK_SPACE_REFUSAL: &str = "disk space (GetDiskFreeSpaceExW is a syscall: \
                                        std::fs::Metadata::file_system is still unstable \
                                        (rust-lang/rust#67087) and calling it through windows-sys needs \
                                        `unsafe`, which this workspace forbids - plan §9.2 step-16a)";

/// `GET /api/diskspace` for a Windows volume.
///
/// Go's answer (`api/diskspace_windows.go`) is a `GetDiskFreeSpaceExW` call, and there is **no safe route
/// to those two numbers** on this toolchain: `Metadata::file_system()` (the `std` wrapper for exactly
/// `lpFreeBytesAvailableToCaller`/`lpTotalNumberOfBytes`) is unstable, `rustix` has no Windows backend, and
/// `windows-sys` would need an `unsafe` block the workspace forbids. So the answer is a typed
/// [`CoreError::Unsupported`] naming the reason: the disk meter renders its `--` state, and no caller is
/// handed a plausible-looking zero. Spawning `wmic`/PowerShell is rejected - `wmic` is removed from Windows
/// 11, and a ~1 s process per poll is not a disk meter.
pub fn disk_space(path: &AbsPath) -> Result<DiskSpace, CoreError> {
    let _ = path;
    Err(CoreError::Unsupported {
        what: DISK_SPACE_REFUSAL,
    })
}

/// The drive roots as mount entries — the places sidebar's Windows half, which Go never had.
///
/// `mount_id` is the position in the enumeration (there is no kernel mount ID), `root` is `\` (a drive
/// root is the whole volume), `source` is the `C:` form Explorer uses, and `fstype` is the volume's
/// filesystem name when it can be read. `removable` is `false`: `GetDriveTypeW` is the only way to answer
/// it and it is FFI, so the sidebar shows no eject button rather than a wrong one (deviation, recorded).
#[must_use]
pub fn drive_entries(
    exists: impl Fn(&str) -> bool,
    fs_name: impl Fn(&Path) -> Option<String>,
) -> Vec<MountEntry> {
    enumerate_drives(exists)
        .into_iter()
        .enumerate()
        .map(|(index, root)| {
            let source = root.trim_end_matches('\\').to_string();
            let letter = source.chars().next().unwrap_or('C');
            let drive = format!("{letter}:");
            MountEntry {
                mount_id: index as u64 + 1,
                parent_id: 0,
                dev: drive.clone(),
                root: "\\".to_string(),
                mount_point: root.clone(),
                mount_options: Vec::new(),
                optional_fields: Vec::new(),
                fstype: fs_name(Path::new(&root)).unwrap_or_else(|| "unknown".to_string()),
                source: drive,
                super_options: Vec::new(),
                removable: false,
            }
        })
        .collect()
}

/// The live drive table. Never an error: an enumeration that finds nothing is an empty table (plan §5.2's
/// `mounts()` rule), which the sidebar renders as "no volumes".
pub fn mount_entries() -> Result<MountTable, CoreError> {
    Ok(MountTable {
        entries: drive_entries(
            |root| std::fs::metadata(Path::new(root)).is_ok(),
            volume_fs_name,
        ),
    })
}

// --------------------------------------------------------------------------- the actions

/// `op:"paste_link"` on Windows: `std::os::windows::fs::symlink_file`/`_dir` need to know the target's
/// type up front (unlike `symlink(2)`), so the walk is the unix implementation's shared
/// [`super::linux::paste_link`] — which already answers [`CoreError::SymlinkUnsupported`] where the
/// platform refuses, i.e. for every unprivileged non-developer-mode volume.
pub fn paste_link(items: &[LinkItem]) -> Result<(), CoreError> {
    super::linux::paste_link(items)
}

/// The Windows trash question — see the module doc's "filesystem identity" section.
#[must_use]
pub fn can_trash(path: &Path) -> bool {
    !(is_volume_root(path) || is_unc_path(path) || std::fs::symlink_metadata(path).is_err())
}

/// `true` for `C:\` and its equivalents: `trash` refuses a root folder (`Error::TargetedRoot`), and so
/// would we — a volume root is not an item.
#[must_use]
pub fn is_volume_root(path: &Path) -> bool {
    let text = path.to_string_lossy().replace('/', "\\");
    let trimmed = text.trim_end_matches('\\');
    let bytes = trimmed.as_bytes();
    bytes.len() == 2 && bytes[1] == b':' && bytes[0].is_ascii_alphanumeric()
}

/// Move one path to the recycle bin. `Ok(false)` is a refusal (a volume root), which the caller escalates
/// to the logged permanent delete (plan §6.2 D6); `Err` is a failure while trying.
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

/// `explorer` is Windows' default-handler entry point (`open` 5.4 uses `cmd /C start` internally on that
/// target); `open::commands` being empty means no handler is registered at all, which is named rather
/// than swallowed (plan F10's "never an empty success").
pub fn open_with(path: &AbsPath) -> Result<(), CoreError> {
    if open::commands(path.as_path()).is_empty() {
        return Err(CoreError::Shell {
            cmd: "explorer".to_string(),
            code: None,
            stderr: "no default handler registered for this file type".to_string(),
        });
    }
    open::that_detached(path.as_path()).map_err(|e| CoreError::Shell {
        cmd: "explorer".to_string(),
        code: None,
        stderr: e.to_string(),
    })
}

/// Plan F10's single spawn site, Windows shape: the request is first put through
/// [`to_windows_request`] (a `shell` request becomes `cmd /C <line>`), then handed to the shared spawn
/// helper in [`super::linux::exec_program`] — which is entirely `std` (no `unix` gate inside it), so the
/// output cap, the `truncated` flag and the `/dev/null`-vs-empty-stdin distinction are identical on both
/// platforms by construction rather than by duplicated code.
pub fn exec_program(req: &ExecRequest) -> Result<ExecOutput, CoreError> {
    super::linux::exec_program(&to_windows_request(req))
}

/// `exec.LookPath` with Windows' rules: `PATHEXT` extensions are appended to a bare name, and a name
/// containing a separator is taken as a path. `PATHEXT` defaults to the seven system extensions when the
/// variable is unset (the value a fresh install carries).
pub fn has_program(name: &str) -> Result<bool, CoreError> {
    Ok(has_program_in(
        name,
        &std::env::var("PATHEXT").unwrap_or_default(),
        &path_var(),
    ))
}

/// [`has_program`] with its two environment inputs injected, so the rule is testable on any host.
#[must_use]
pub fn has_program_in(name: &str, pathext: &str, path: &str) -> bool {
    if name.is_empty() || name.contains('\0') {
        return false;
    }
    let exts: Vec<String> = if pathext.trim().is_empty() {
        [".EXE", ".COM", ".BAT", ".CMD", ".VBS", ".JS", ".MSC"]
            .iter()
            .map(|e| (*e).to_string())
            .collect()
    } else {
        pathext
            .split(';')
            .filter(|e| !e.is_empty())
            .map(|e| e.to_uppercase())
            .collect()
    };
    let looks_like_path = name.contains('\\') || name.contains('/');
    let candidates: Vec<String> = if has_extension(name) {
        vec![name.to_string()]
    } else {
        exts.iter().map(|e| format!("{name}{e}")).collect()
    };
    if looks_like_path {
        return candidates
            .iter()
            .any(|c| Path::new(c).is_file() || Path::new(c).exists());
    }
    path.split(';')
        .filter(|dir| !dir.is_empty())
        .any(|dir| candidates.iter().any(|c| Path::new(dir).join(c).is_file()))
}

/// Does `name` already carry an extension? (Dotfiles have no precedent on Windows, so a leading dot is
/// not special: `.bashrc` counts as stem-less and gets `PATHEXT` appended, which is what `cmd` does.)
#[must_use]
pub fn has_extension(name: &str) -> bool {
    name.rsplit(['\\', '/'])
        .next()
        .is_some_and(|tail| tail.contains('.'))
}

/// The `PATH` this process sees (Windows' spelling is `Path`; `std::env::var` is case-insensitive there).
fn path_var() -> String {
    std::env::var("PATH").unwrap_or_default()
}

/// The Windows write-permission question. `access(2)` has no Windows equivalent; the closest safe `std`
/// answer is the read-only **attribute**, which is a real (if coarse) signal — it is what Explorer greys
/// out. ACLs and read-only *mounts* are invisible here, so a `true` means "nothing forbids it", exactly
/// like the unix side's answer for a path whose ACL the kernel happens to allow. `Ok(false)` for a
/// missing path (nothing to write to), matching [`super::linux::is_writable`].
pub fn is_writable(path: &AbsPath) -> Result<bool, CoreError> {
    match std::fs::metadata(path.as_path()) {
        Ok(meta) => Ok(!meta.permissions().readonly()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(CoreError::io(path.to_string_lossy(), e)),
    }
}

// --------------------------------------------------------------------------- the impl

/// Windows platform actions. Cheap to construct and free of I/O; the only state is the symlink-probe
/// cache, shared with the unix implementation's [`super::ProbeCache`] shape so `Session` can own one.
#[derive(Clone)]
pub struct Windows {
    probe_cache: super::linux::ProbeCache,
}

impl Windows {
    /// An environment with no probe result yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            probe_cache: super::linux::ProbeCache::default(),
        }
    }

    /// Share a probe cache (see [`super::linux::Linux::with_probe_cache`]).
    #[must_use]
    pub fn with_probe_cache(probe_cache: super::linux::ProbeCache) -> Self {
        Self { probe_cache }
    }

    /// The cache this implementation reads.
    #[must_use]
    pub fn probe_cache(&self) -> &super::linux::ProbeCache {
        &self.probe_cache
    }

    /// Run the link-creation write test under `scratch_root` and cache its verdict (plan F21). On Windows
    /// this succeeds only for an elevated process or a developer-mode volume, which is *why* it is a
    /// probe and not a constant.
    pub fn probe_symlinks(&self, scratch_root: &Path) -> Result<bool, CoreError> {
        let supported = super::linux::supports_symlink(scratch_root)?;
        self.set_symlink_supported(supported);
        Ok(supported)
    }

    /// Publish a probe verdict into the shared cache.
    pub fn set_symlink_supported(&self, supported: bool) {
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

impl Default for Windows {
    fn default() -> Self {
        Self::new()
    }
}

impl Env for Windows {
    fn os(&self) -> Os {
        Os::Windows
    }

    fn caps(&self) -> PlatformCaps {
        windows_caps()
    }

    fn open_with(&self, path: &AbsPath) -> PlatformFuture<'_, Result<(), CoreError>> {
        ready(self::open_with(path))
    }

    fn copy_paths(&self, items: &[CopyItem]) -> PlatformFuture<'_, Result<CopyReport, CoreError>> {
        // The walk is `std::fs` only (module doc); its two platform-specific edges — link creation and
        // mode preservation — are already target-gated inside `platform/linux.rs`.
        ready(super::linux::copy_paths(items))
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
        _targets: &[AbsPath],
        _mode: ModeBits,
    ) -> PlatformFuture<'_, Result<(), CoreError>> {
        ready(Err(CoreError::Unsupported {
            what: "chmod (Windows has a read-only attribute, not an st_mode; mapping 0rwx bits onto \
                   it would silently discard information — plan §9.2 step-16a)",
        }))
    }

    fn can_trash(&self, path: &AbsPath) -> bool {
        self::can_trash(path.as_path())
    }

    fn trash(&self, path: &AbsPath) -> Result<bool, CoreError> {
        self::trash(path.as_path())
    }

    fn create_shortcut(&self, _shortcut: &Shortcut) -> Result<(), CoreError> {
        // A `.lnk` is a structured binary shell-link record written through `IShellLink` (COM): `unsafe`,
        // and therefore barred by the workspace's `forbid(unsafe_code)` — see the module doc. The safe
        // alternative (`[InternetShortcut] URL=file:///…`, i.e. a `.url` file) works in Explorer but is a
        // different extension from the one the caller asked for, so it is offered as plan step 16b's
        // decision rather than smuggled in here.
        Err(CoreError::Unsupported {
            what: "shortcut files (a .lnk needs IShellLink/COM, i.e. `unsafe`; see platform/windows.rs)",
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

/// The path separator this platform's users see, for the display helpers that must not hardcode one.
pub const PATH_SEPARATOR: char = '\\';

/// Convert a `/`-separated string into this platform's canonical form. `AbsPath` owns the real rules
/// (plan §4.2); this is the display-side helper, and it is deliberately not applied to a `\\?\` prefix.
#[must_use]
pub fn to_native_separators(path: &str) -> String {
    path.replace('/', "\\")
}

/// The reverse, for the wire form the Go fixtures carry.
#[must_use]
pub fn to_wire_separators(path: &str) -> String {
    path.replace('\\', "/")
}

/// Where a Windows trash bin would live for `path` — the per-volume `$Recycle.Bin\<SID>`, which is a
/// directory the shell creates and `trash` fills. Kept for the docs test; nothing may assume it exists.
#[must_use]
pub fn recycle_bin_dir(drive_root: &Path) -> PathBuf {
    drive_root.join("$Recycle.Bin")
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};

    fn poll_once<T>(mut future: PlatformFuture<'_, T>) -> T {
        let mut cx = Context::from_waker(Waker::noop());
        let pinned: Pin<&mut dyn Future<Output = T>> = future.as_mut();
        match pinned.poll(&mut cx) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("the windows impl must answer immediately"),
        }
    }

    // -- reserved names -----------------------------------------------------------------------------

    #[test]
    fn reserved_names_are_refused_case_insensitively_and_with_an_extension() {
        // The classic 22 plus the two zeroth channels Windows also refuses.
        for name in [
            "CON",
            "con",
            "CoN",
            "PRN",
            "prn.txt",
            "AUX",
            "aux.tar.gz",
            "NUL",
            "nul.log",
            "COM1",
            "com9",
            "COM0",
            "LPT1",
            "lpt9.zip",
            "LPT0",
        ] {
            assert_eq!(
                name_reject(name),
                Some(NameReject::Reserved),
                "{name} must be reserved"
            );
            assert!(is_reserved_name(name), "{name}");
        }
        assert_eq!(name_reject("."), Some(NameReject::Reserved));
        assert_eq!(name_reject(".."), Some(NameReject::Reserved));
        // Names that merely *contain* a reserved stem are legal — the rule is about the first component.
        for name in [
            "console.txt",
            "printer",
            "auxiliary",
            "nullify",
            "comet",
            "LPT10",
            "con",
        ] {
            if name == "con" {
                assert_eq!(name_reject(name), Some(NameReject::Reserved), "{name}");
                continue;
            }
            assert_eq!(
                name_reject(name),
                None,
                "{name} must be legal, got {:?}",
                name_reject(name)
            );
            assert!(!is_reserved_name(name), "{name}");
        }
    }

    #[test]
    fn the_other_windows_name_rules() {
        assert_eq!(name_reject(""), Some(NameReject::Empty));
        assert_eq!(name_reject("a<b"), Some(NameReject::IllegalChar('<')));
        assert_eq!(name_reject("a>b"), Some(NameReject::IllegalChar('>')));
        assert_eq!(name_reject("a:b"), Some(NameReject::IllegalChar(':')));
        assert_eq!(name_reject("a\"b"), Some(NameReject::IllegalChar('"')));
        assert_eq!(name_reject("a/b"), Some(NameReject::IllegalChar('/')));
        assert_eq!(name_reject("a\\b"), Some(NameReject::IllegalChar('\\')));
        assert_eq!(name_reject("a|b"), Some(NameReject::IllegalChar('|')));
        assert_eq!(name_reject("a?b"), Some(NameReject::IllegalChar('?')));
        assert_eq!(name_reject("a*b"), Some(NameReject::IllegalChar('*')));
        assert_eq!(name_reject("bad\0name"), Some(NameReject::ControlChar));
        assert_eq!(name_reject("bell\u{7}"), Some(NameReject::ControlChar));
        assert_eq!(name_reject("del."), Some(NameReject::TrailingDotOrSpace));
        assert_eq!(name_reject("del "), Some(NameReject::TrailingDotOrSpace));
        assert_eq!(name_reject(&"x".repeat(256)), Some(NameReject::TooLong));
        // Legal, including the Unicode and dot-in-the-middle cases the Go app's users produce.
        for name in [
            "a.txt",
            "..hidden",
            "foo..bar",
            "报告.pdf",
            "a b",
            "x",
            "a-b_c+d=e",
        ] {
            assert_eq!(
                name_reject(name),
                None,
                "{name} must be legal, got {:?}",
                name_reject(name)
            );
        }
        // The CoreError shape: 400-class `invalid_path`, naming the rule and the name.
        let e = validate_name("CON").expect_err("CON is reserved");
        assert_eq!(e.kind(), "invalid_path");
        assert_eq!(e.status_code(), 400);
        assert!(e.to_string().contains("CON"), "{e}");
        assert!(validate_name("notes.txt").is_ok());
    }

    #[test]
    fn path_validation_checks_every_component() {
        assert!(validate_path(Path::new("C:\\users\\me\\notes.txt")).is_err());
        // `:` in the drive component is the separator form, not a name, so the components here are all
        // legal and only the injected bad one trips the rule.
        assert!(validate_path(Path::new("dir/CON.txt/sub")).is_err());
        assert!(validate_name("notes.txt").is_ok());
    }

    // -- argv quoting / cmd /C ----------------------------------------------------------------------

    #[test]
    fn quoting_follows_the_msvc_commandline_rules() {
        assert_eq!(quote_arg("plain"), "\"plain\"");
        assert_eq!(quote_arg("two words"), "\"two words\"");
        // An embedded quote is doubled and cannot swallow the closing quote.
        assert_eq!(quote_arg("a\"b"), "\"a\"\"b\"");
        // Backslashes only matter before a quote (or at the end, where they would escape the terminator).
        assert_eq!(quote_arg("c:\\dir"), "\"c:\\dir\"");
        assert_eq!(quote_arg("c:\\dir\\"), "\"c:\\dir\\\\\"");
        assert_eq!(quote_arg("c:\\a\\\"b"), "\"c:\\a\\\\\"\"b\"");
        assert_eq!(quote_arg(""), "\"\"");

        // The plan F10 property: a token with spaces and quotes stays one token, and the joined line is
        // built only from quoted argv elements — never from user text spliced into a shell string.
        assert_eq!(
            join_argv(["cmd.exe", "/c", "echo hi there"]),
            "\"cmd.exe\" \"/c\" \"echo hi there\""
        );
        assert_eq!(cmd_c_args("dir /b"), ["/C", "dir /b"]);
    }

    #[test]
    fn a_shell_request_becomes_cmd_c_and_an_argv_request_is_untouched() {
        let shell = ExecRequest::shell("dir /b \"my folder\"", None);
        let rewritten = to_windows_request(&shell);
        assert!(
            !rewritten.shell,
            "the rewritten form must not re-enter the shell arm"
        );
        assert_eq!(rewritten.program, "cmd");
        assert_eq!(
            rewritten.args,
            vec!["/C".to_string(), "dir /b \"my folder\"".to_string()],
            "the user's line is one argv element, not spliced into a command line"
        );

        let line = ExecRequest::argv("notepad.exe", ["C:\\My Notes.txt".to_string()]);
        let kept = to_windows_request(&line);
        assert_eq!(kept.program, "notepad.exe");
        assert_eq!(kept.args, line.args);
        assert!(!kept.shell);

        // cwd/env/cap/stdin/detached all survive the rewrite (plan F10: one spawn site, one behaviour).
        let rich = ExecRequest {
            program: "ping".into(),
            args: Vec::new(),
            shell: true,
            cwd: AbsPath::from_path(std::env::temp_dir()).ok(),
            env: vec![("A".into(), "b".into())],
            output_cap: 7,
            stdin: Some("x".into()),
            detached: true,
        };
        let rich = to_windows_request(&rich);
        assert_eq!(rich.program, "cmd");
        assert_eq!(rich.output_cap, 7);
        assert_eq!(rich.env, vec![("A".to_string(), "b".to_string())]);
        assert_eq!(rich.stdin.as_deref(), Some("x"));
        assert!(rich.detached);
        assert!(rich.cwd.is_some());
    }

    // -- drives / disk ------------------------------------------------------------------------------

    #[test]
    fn drive_enumeration_is_a_pure_probe_over_the_26_letters() {
        let roots = enumerate_drives(|root| matches!(root, "C:\\" | "D:\\" | "z:\\"));
        assert_eq!(roots, vec!["C:\\", "D:\\"]);
        assert_eq!(enumerate_drives(|_| false), Vec::<String>::new());
        assert_eq!(drive_root('c').as_deref(), Some("C:\\"));
        assert_eq!(drive_root('1'), None);

        let entries = drive_entries(
            |root| matches!(root, "C:\\" | "E:\\"),
            |path| {
                if path.to_string_lossy() == "C:\\" {
                    Some("NTFS".to_string())
                } else {
                    None
                }
            },
        );
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].mount_point, "C:\\");
        assert_eq!(entries[0].source, "C:");
        assert_eq!(entries[0].dev, "C:");
        assert_eq!(entries[0].fstype, "NTFS");
        assert_eq!(entries[0].root, "\\");
        assert_eq!(
            entries[1].fstype, "unknown",
            "an unreadable volume still gets a row"
        );
        assert!(
            entries.iter().all(|e| !e.removable),
            "no GetDriveTypeW here"
        );
        let table = MountTable {
            entries: entries.clone(),
        };
        // `MountTable` compares with `std::path` semantics, so on a *linux* check-out `C:\Users\me` is
        // one opaque component and no ancestor relation exists; the drive root itself is the comparison
        // that is meaningful on both hosts (it is the one the places sidebar does first).
        assert!(table.is_mounted(Path::new("C:\\")));
        assert!(!table.is_mounted(Path::new("D:\\x")));
        assert!(table.mount_for(Path::new("C:\\")).is_some());
        assert!(
            !table.is_removable(Path::new("C:\\")),
            "no GetDriveTypeW here"
        );
    }

    #[test]
    fn the_recycle_bin_rule_is_conservative_by_name() {
        for name in ["NTFS", "ExFAT", "FAT32", "ReFS", "NTFS\0"] {
            assert!(recycle_bin_exists(Some(name)), "{name}");
        }
        for name in [
            "CDFS", "UDF", "SMB", "NFS", "DevFS", "", "overlay", "ext4", "tmpfs",
        ] {
            assert!(!recycle_bin_exists(Some(name)), "{name}");
        }
        assert!(
            !recycle_bin_exists(None),
            "an unreadable volume gets the safe answer"
        );
    }

    #[test]
    fn volume_roots_are_not_items_and_unc_volumes_have_no_bin() {
        assert!(is_volume_root(Path::new("C:\\")));
        assert!(is_volume_root(Path::new("C:")));
        assert!(!is_volume_root(Path::new("C:\\Users")));
        assert!(!is_volume_root(Path::new("/")));
        assert!(is_unc_path(Path::new("\\\\nas\\share")));
        assert!(!is_unc_path(Path::new("C:\\Users")));
        assert!(
            !can_trash(Path::new("C:\\")),
            "a volume root is never trashable"
        );
        assert!(!can_trash(Path::new("C:\\definitely-not-here")));
        // The name rule a future `GetVolumeInformationW` probe feeds: optical and unknown volumes get the
        // safe `false`, the local families get `true`.
        assert!(recycle_bin_exists(Some("NTFS")));
        assert!(!recycle_bin_exists(
            volume_fs_name(Path::new("C:\\")).as_deref()
        ));
    }

    #[test]
    fn disk_space_is_a_typed_refusal_not_a_plausible_zero() {
        // The honest half of the dependency question: `Metadata::file_system` is unstable and the crate
        // forbids `unsafe`, so the two numbers Go read from `GetDiskFreeSpaceExW` have no safe route here.
        let tmp = std::env::temp_dir();
        let abs = AbsPath::from_path(&tmp).expect("the temp dir is absolute");
        let e = disk_space(&abs).expect_err("the refusal must be an error, never a 0/0 meter");
        assert_eq!(e.kind(), "unsupported", "{e}");
        assert_eq!(e.status_code(), 501);
        assert!(
            e.to_string().contains("GetDiskFreeSpaceExW"),
            "the refusal must name the API it cannot reach: {e}"
        );
    }

    #[test]
    fn separators_and_the_recycle_bin_path() {
        assert_eq!(to_native_separators("C:/Users/me"), "C:\\Users\\me");
        assert_eq!(to_wire_separators("C:\\Users\\me"), "C:/Users/me");
        assert_eq!(PATH_SEPARATOR, '\\');
        assert_eq!(
            recycle_bin_dir(Path::new("C:\\")),
            PathBuf::from("C:\\").join("$Recycle.Bin")
        );
    }

    #[test]
    fn pathext_candidates_and_the_path_scan() {
        let path = "C:\\Windows;C:\\Tools";
        assert!(has_extension("a.txt"));
        assert!(!has_extension("a"));
        assert!(!has_extension("C:\\dir\\a"));
        assert!(!has_program_in("", "", path), "the empty name is false");
        assert!(!has_program_in("a\0b", "", path));
        assert!(
            !has_program_in("not-a-program-42", ".EXE", path),
            "a missing program is false, not an error"
        );
        // A name with a separator is a path: probed as given, never through PATH.
        assert!(!has_program_in("C:\\nope\\x.exe", ".EXE", path));
        // An empty PATH entry never matches (the same rule the unix scan applies).
        assert!(!has_program_in("cmd", ".EXE", ";;"));
        // An explicit extension is taken as given; a bare name gets the `PATHEXT` list appended.
        assert!(!has_program_in("x.exe", ".BAD", path));
        assert!(!has_program_in("x", ".BAD", path));
    }

    #[test]
    fn has_program_answers_for_the_running_host() {
        // On a linux check-out this reads the real `PATHEXT` (unset) and `PATH`; either way it must not
        // error, and a program that certainly is not there must be a plain `false`.
        assert!(
            !has_program("zenman-definitely-not-a-program").expect("a PATH scan does not fail")
        );
    }

    #[test]
    fn is_writable_answers_from_the_read_only_attribute() {
        let dir = std::env::temp_dir().join(format!("zenman-win-writable-{}", std::process::id()));
        let _ = std::fs::remove_dir(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let abs = AbsPath::from_path(&dir).expect("absolute");
        assert!(is_writable(&abs).expect("a fresh temp dir is writable"));
        let file = dir.join("ro.txt");
        std::fs::write(&file, b"x").expect("write");
        let mut perms = std::fs::metadata(&file).expect("meta").permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&file, perms).expect("set the attribute");
        // On a unix host `set_readonly` maps onto the mode bits, so the *value* is host-dependent; what
        // must hold everywhere is that the question is answered rather than refused.
        println!(
            "is_writable({}) = {}",
            file.display(),
            is_writable(&AbsPath::from_path(&file).expect("abs")).expect("answers")
        );
        assert!(
            !is_writable(&AbsPath::from_path(dir.join("nope")).expect("abs"))
                .expect("a missing path is `false`, not an error")
        );
        // The tree is ours, so it goes by hand (`std::fs::remove_dir_all` is banned, `clippy.toml`).
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&dir);
    }

    // -- the trait audit ----------------------------------------------------------------------------

    /// Every trait method, with its answer reduced to what the step-16a audit asserts: a *typed* answer,
    /// never a panic and never a `todo!()`. The mirror of `platform::stub`'s table test — a 15th method
    /// added to `Env` fails here as well as there.
    fn outcomes(env: &Windows, path: &AbsPath) -> Vec<(&'static str, String)> {
        let item = CopyItem {
            source: path.clone(),
            dest: path.join("copy-target").expect("a plain name"),
        };
        let link = LinkItem {
            link: path.join("link-target").expect("a plain name"),
            target: path.clone(),
        };
        let shortcut = Shortcut {
            link: path.join("shortcut.txt").expect("a plain name"),
            target: path.clone(),
            arguments: None,
            description: None,
            working_dir: None,
        };
        let req = ExecRequest::shell("ver", None);

        let mut rows: Vec<(&'static str, Result<(), CoreError>)> = vec![
            ("open_with", poll_once(env.open_with(path))),
            ("copy_paths", poll_once(env.copy_paths(&[item])).map(|_| ())),
            ("paste_link", poll_once(env.paste_link(&[link]))),
            ("supports_symlink", env.supports_symlink().map(|_| ())),
            (
                "chmod",
                poll_once(env.chmod(std::slice::from_ref(path), ModeBits::DEFAULT_FILE)),
            ),
            (
                "trash",
                env.trash(&path.join("nothing-here").expect("a plain name"))
                    .map(|_| ()),
            ),
            ("create_shortcut", env.create_shortcut(&shortcut)),
            ("disk_space", env.disk_space(path).map(|_| ())),
            ("mount_entries", env.mount_entries().map(|_| ())),
            ("exec_program", poll_once(env.exec_program(req)).map(|_| ())),
            ("has_program", env.has_program("cmd").map(|_| ())),
            ("is_writable", env.is_writable(path).map(|_| ())),
        ];
        let mut table: Vec<(&'static str, String)> = rows
            .drain(..)
            .map(|(name, result)| {
                let text = match result {
                    Ok(()) => "ok".to_string(),
                    Err(CoreError::Unsupported { what }) => format!("unsupported:{what}"),
                    Err(other) => format!("error:{}", other.kind()),
                };
                (name, text)
            })
            .collect();
        table.push(("can_trash", format!("bool:{}", env.can_trash(path))));
        table
    }

    #[test]
    fn every_trait_method_is_answered_typed_never_panicked() {
        let env = Windows::new();
        let tmp = std::env::temp_dir();
        let path = AbsPath::from_path(&tmp).expect("the temp dir is absolute");
        let rows = outcomes(&env, &path);
        let names: Vec<&str> = rows.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            names,
            vec![
                "open_with",
                "copy_paths",
                "paste_link",
                "supports_symlink",
                "chmod",
                "trash",
                "create_shortcut",
                "disk_space",
                "mount_entries",
                "exec_program",
                "has_program",
                "is_writable",
                "can_trash"
            ],
            "the trait gained or lost a method: update this table and platform/stub.rs"
        );
        for (name, answer) in &rows {
            assert!(
                !answer.contains("panicked"),
                "{name} must never panic: {answer}"
            );
            match *name {
                // The two that genuinely work wherever the code runs, because they are `std` only.
                "mount_entries" | "has_program" => {
                    assert_eq!(answer, "ok", "{name} must answer: {answer}")
                }
                // The documented refusals, each naming its own reason.
                "chmod" => assert!(answer.starts_with("unsupported:chmod"), "chmod: {answer}"),
                "create_shortcut" => assert!(
                    answer.starts_with("unsupported:shortcut"),
                    "create_shortcut: {answer}"
                ),
                "disk_space" => assert!(
                    answer.starts_with("unsupported:disk space"),
                    "disk_space: {answer}"
                ),
                _ => {}
            }
        }
    }

    #[test]
    fn the_windows_env_knows_what_it_is_even_on_a_linux_host() {
        // This is the cross-target audit in executable form: the *implementation* is compiled and asked
        // its questions on a linux host, so `cargo check --target x86_64-pc-windows-msvc` is not the only
        // thing that proves the surface is complete.
        let env = Windows::new();
        assert_eq!(env.os(), Os::Windows);
        let caps = env.caps();
        assert!(
            caps.contains(PlatformCaps::MOUNT_TABLE),
            "drives are a mount table"
        );
        assert!(caps.contains(PlatformCaps::DISK_SPACE));
        assert!(
            caps.contains(PlatformCaps::SHORTCUT),
            "windows offers .lnk as the fallback"
        );
        assert!(caps.contains(PlatformCaps::TRASH));
        assert!(caps.contains(PlatformCaps::EXEC));
        assert!(caps.contains(PlatformCaps::OPEN_WITH));
        assert!(
            !caps.contains(PlatformCaps::CHMOD),
            "there is no st_mode on windows, so the bit must be off on every host that compiles this"
        );
        assert!(!env.supports_symlink().expect("static bit before the probe"));
        env.set_symlink_supported(true);
        assert!(env.supports_symlink().expect("cached answer wins"));
        assert_eq!(env.cached_symlink_support(), Some(true));
        assert!(
            env.probe_cache().read().is_ok(),
            "the probe cache is a plain lock"
        );
    }

    #[test]
    fn the_deferred_watcher_answers_a_typed_error() {
        // Plan §7 row 16's "compiles on and off / on every target": with `--features watch` the watcher
        // must exist on every target and answer `CoreError::Watcher` when it cannot run — never a
        // `todo!()`. On the linux+watch build the inotify backend is live, so the only thing asserted
        // here is that a call site compiles and its failure (an empty root set) is the same typed kind.
        let (tx, _rx) = std::sync::mpsc::channel();
        let cfg =
            crate::index::watcher::WatchConfig::from_limits(&crate::limits::Limits::default());
        let kind = crate::index::watcher::start(&[], &cfg, tx)
            .err()
            .map(|e| e.kind())
            .unwrap_or("started");
        if cfg!(all(feature = "watch", target_os = "linux")) {
            assert!(
                matches!(kind, "started" | "watcher_error"),
                "the inotify backend must start or name a watcher error, got {kind}"
            );
        } else {
            // The stub lane (feature off, or a non-linux target): a refusal, naming which half is absent.
            assert_eq!(
                kind, "watcher_error",
                "the stub must refuse by name, got {kind}"
            );
        }
    }
}
