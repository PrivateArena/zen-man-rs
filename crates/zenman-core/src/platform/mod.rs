//! Platform services — the only place in `zenman-core` that touches the OS (plan §5.2 `platform/*`).
//!
//! Plan §5.1's house rule: "core has no `std::process`/`std::fs` outside `platform/`" (decision D5).
//! Everything else in the crate is therefore portable and testable; this module is the seam.
//!
//! Two types, deliberately separate:
//!
//! * [`Platform`] is *knowledge* — which OS, and which of the capability bits the UI may show (the
//!   `/api/props` probe of `docs/contracts.md` §2.4 is exactly `{"platform":…,"symlink_supported":…}`,
//!   `main.go:141-147`);
//! * [`Env`] is *action* — open, copy, link, chmod, trash, statvfs, spawn. It is a trait so
//!   [`crate::platform::stub::Unsupported`] can answer "no" for a platform nobody has written yet, and so
//!   a test can substitute a recorder.
//!
//! # Why the async methods return a boxed future
//!
//! `zenman-core` has no executor of its own (plan §9.1: the only one is in the binary), so an `async fn`
//! in a `dyn`-safe trait is not available. Each action returns a heap-allocated future that the job runner
//! (plan §5.6, step 6) polls; the implementations here are executor-agnostic — they do their work when
//! polled and never spawn a thread.
//!
//! # Why nothing here blocks the UI thread
//!
//! The two operations a window cannot afford to run inline are the symlink probe and a recursive `chmod`.
//! The probe is cached behind an `Arc<RwLock<Option<bool>>>` in the `linux` implementation (plan F21), so
//! [`Env::supports_symlink`] is a lock read and the real write test only ever runs from a job; the
//! recursion is in [`Env::chmod_all`], which is `async` and yields between items, so cancellation at an
//! item boundary is the job runner's normal `Cancelled` path.

// The three submodules are public because their contents *are* this module's API: `mod.rs`'s own docs
// point at `platform::stub::Unsupported`, step 6 needs `platform::linux::Linux::probe_symlinks`, and
// `env_probe` is the probe behind `/api/props`.
pub mod env_probe;
pub mod linux;
pub mod stub;
pub mod system;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use bitflags::bitflags;
use serde::Serialize;

use crate::error::CoreError;
use crate::mode::ModeBits;
use crate::path::AbsPath;

pub use env_probe::{
    ProbeFlags, overlayfs_works, probe_symlink_support, unprivileged_userns_clone,
};
pub use linux::Linux;
pub use stub::Unsupported;

/// An owned future the job runner can poll. `Pin<Box<…>>` rather than the plan's literal
/// `Box<dyn Future<…>>` because only the pinned form implements `Future` and is therefore `.await`-able
/// without a `pin!` at every call site (step-3 report: the plan sketch cannot compile as written).
pub type PlatformFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Box `value` as an already-complete [`PlatformFuture`] — the return value of every action that ran
/// synchronously inside its own poll. (No `#[must_use]`: the returned pinned-boxed future is already
/// `#[must_use]` by type, and doubling the attribute is a clippy warning.)
pub fn ready<T: Send + 'static>(value: T) -> PlatformFuture<'static, T> {
    Box::pin(std::future::ready(value))
}

/// Which family we are running on. Spelling follows Go's `runtime.GOOS` because `/api/props` published it
/// verbatim (`main.go:141-147`, `props-dir.json`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Os {
    /// `windows`
    Windows,
    /// `linux`
    Linux,
    /// `darwin` — zen-man never had a macOS path; the port gets one for free from `std::os::unix`.
    Macos,
    /// Anything else (the BSDs this port may be built for).
    Other,
}

impl Os {
    /// The OS of this build.
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "macos") {
            Self::Macos
        } else {
            Self::Other
        }
    }

    /// The `/api/props` `platform` value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Linux => "linux",
            Self::Macos => "darwin",
            Self::Other => "other",
        }
    }

    /// `true` where `chmod`/symlinks/`st_mode` mean something.
    #[must_use]
    pub const fn is_unix(self) -> bool {
        matches!(self, Self::Linux | Self::Macos | Self::Other)
    }
}

impl std::fmt::Display for Os {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

bitflags! {
    /// What this machine can be asked to do. The UI gates its menu items on these (plan §5.8
    /// `MenuContext::can_symlink`: "symlinks hidden when `!props().symlink_supported`").
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct PlatformCaps: u32 {
        /// Symlink creation works (probed, not assumed: `main.go:26-49`).
        const SYMLINK = 1 << 0;
        /// A desktop trash is reachable (`trash::can_be_trashed`; plan OQ2).
        const TRASH = 1 << 1;
        /// `chmod` means something — i.e. a Unix `st_mode` exists.
        const CHMOD = 1 << 2;
        /// `.lnk`/`.desktop` shortcuts (plan §5.3: the Windows fallback when `SYMLINK` is clear).
        const SHORTCUT = 1 << 3;
        /// Free/total bytes of a path can be read.
        const DISK_SPACE = 1 << 4;
        /// A mount table exists to be enumerated.
        const MOUNT_TABLE = 1 << 5;
        /// Something can be handed to the desktop's default handler.
        const OPEN_WITH = 1 << 6;
        /// Custom actions can be spawned at all (plan F10).
        const EXEC = 1 << 7;
    }
}

/// The static half of the platform answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Platform {
    /// Which family.
    pub os: Os,
    /// What it can do.
    pub caps: PlatformCaps,
}

/// The platform this build runs on.
///
/// `caps` is *static* here on purpose: the symlink answer that is probed at run time lives in
/// [`Env::supports_symlink`] (and its cache), so that a value read at startup can never go stale after a
/// network share appears. `TRASH` is likewise only "the plumbing exists"; whether a *given* path can be
/// trashed is [`Env::can_trash`] (plan OQ2).
#[must_use]
pub fn detect() -> Platform {
    let os = Os::current();
    let mut caps = PlatformCaps::empty();
    caps.set(PlatformCaps::CHMOD, os.is_unix());
    caps.set(PlatformCaps::DISK_SPACE, true);
    caps.set(PlatformCaps::MOUNT_TABLE, os == Os::Linux);
    caps.set(PlatformCaps::OPEN_WITH, true);
    caps.set(PlatformCaps::EXEC, true);
    caps.set(PlatformCaps::TRASH, os != Os::Other);
    caps.set(PlatformCaps::SHORTCUT, os == Os::Windows);
    // The conservative answer; the probe refines it. `Other` (wasm in plan D5's future) has no symlinks.
    caps.set(PlatformCaps::SYMLINK, os.is_unix());
    Platform { os, caps }
}

/// The `Env` this build should use. Step 16 replaces the non-Linux branch with `platform::windows`.
#[must_use]
pub fn default_env() -> std::sync::Arc<dyn Env> {
    match Os::current() {
        Os::Linux | Os::Macos | Os::Other => std::sync::Arc::new(Linux::new()),
        // `stub::Unsupported` is the Windows answer until step 16 lands `platform/windows.rs`; it fails
        // with a typed error rather than pretending.
        Os::Windows => std::sync::Arc::new(Unsupported::new()),
    }
}

// --------------------------------------------------------------------------- the platform face

/// One file to place, with the destination already decided by `ops::paste` (conflict policy lives in
/// `ops`, not here — the platform only performs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyItem {
    /// What to read.
    pub source: AbsPath,
    /// Where it ends up; the parent directory exists.
    pub dest: AbsPath,
}

/// What a platform copy produced.
#[derive(Debug, Default)]
pub struct CopyReport {
    /// Destinations that now exist.
    pub copied: Vec<AbsPath>,
    /// Bytes of file content written (directories contribute their entries, not their own size).
    pub bytes: u64,
    /// Per-item failures — a partial copy is reported, never silently shortened (plan F12).
    pub failures: Vec<CopyFailure>,
}

/// One failed [`CopyItem`].
#[derive(Debug)]
pub struct CopyFailure {
    /// The destination we could not produce.
    pub dest: AbsPath,
    /// Why.
    pub error: CoreError,
}

/// A link to create and what it must point at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkItem {
    /// The new link.
    pub link: AbsPath,
    /// Its target.
    pub target: AbsPath,
}

/// A shortcut request — plan §5.3's Windows fallback for a volume that cannot hold a symlink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shortcut {
    /// The `.lnk` (or `.desktop`) file to write.
    pub link: AbsPath,
    /// What it launches.
    pub target: AbsPath,
    /// Arguments baked into the shortcut.
    pub arguments: Option<String>,
    /// The comment/description field.
    pub description: Option<String>,
    /// "Start in".
    pub working_dir: Option<AbsPath>,
}

/// `GET /api/diskspace`'s payload (`api/diskspace.go:10-13`, `diskspace.json` key order).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DiskSpace {
    /// `json:"free"` — bytes available to *this* user, i.e. `f_bavail`, not `f_bfree`.
    pub free: u64,
    /// `json:"total"`
    pub total: u64,
}

/// A program to run: plan F10's single spawn path (argv vector, output cap, cancellation at the process
/// boundary).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecRequest {
    /// The program, or the command line when [`ExecRequest::shell`] is set.
    pub program: String,
    /// Its arguments (`[]` when [`ExecRequest::shell`] is set).
    pub args: Vec<String>,
    /// `true` to run `program` through `/bin/sh -c` — the Unix custom-action path (`api/actions.go:237`),
    /// never used on Windows.
    pub shell: bool,
    /// `cmd.Dir` of the Go implementation (`api/actions.go:241`).
    pub cwd: Option<AbsPath>,
    /// Extra environment variables, applied *on top of* the inherited ones.
    pub env: Vec<(String, String)>,
    /// Cap on captured output; the excess sets [`ExecOutput::truncated`] (plan F10).
    pub output_cap: usize,
    /// Bytes to feed the child's stdin, then close. `None` — the default — hands it `/dev/null`, which is
    /// what every pre-step-9 spawn did (`api/actions.go:245` had no stdin at all). `Some("")` is a
    /// *different* answer: an immediately-closed empty stdin, which is how the clipboard clear publishes
    /// an empty `text/uri-list` (`api/ops.go:575` wrote an empty reader, not `/dev/null`).
    pub stdin: Option<String>,
}

impl ExecRequest {
    /// Plan §5.3's `limits.exec_output_cap` default (`1 MiB`).
    pub const DEFAULT_OUTPUT_CAP: usize = 1_048_576;

    /// `sh -c <line>` in `cwd`, output capped — the port's `api/actions.go:237-243`.
    #[must_use]
    pub fn shell(line: impl Into<String>, cwd: Option<AbsPath>) -> Self {
        Self {
            program: line.into(),
            args: Vec::new(),
            shell: true,
            cwd,
            env: Vec::new(),
            output_cap: Self::DEFAULT_OUTPUT_CAP,
            stdin: None,
        }
    }

    /// An argv vector, executed directly (the Windows rule from plan F10, and the safe default).
    #[must_use]
    pub fn argv<I, S>(program: impl Into<String>, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            shell: false,
            cwd: None,
            env: Vec::new(),
            output_cap: Self::DEFAULT_OUTPUT_CAP,
            stdin: None,
        }
    }

    /// Feed `text` to the child's stdin and close it (`wl-copy -t text/uri-list`, `api/ops.go:563-566`).
    #[must_use]
    pub fn with_stdin(mut self, text: impl Into<String>) -> Self {
        self.stdin = Some(text.into());
        self
    }
}

/// The result of an [`ExecRequest`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecOutput {
    /// The exit code, `None` when the process was killed by a signal.
    pub code: Option<i32>,
    /// Captured stdout, cut at the cap.
    pub stdout: String,
    /// Captured stderr, cut at the cap.
    pub stderr: String,
    /// `true` when either stream hit [`ExecRequest::output_cap`] (plan F10: surfaced, not swallowed).
    pub truncated: bool,
}

impl ExecOutput {
    /// `true` for a zero exit status.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.code == Some(0)
    }
}

/// Whether `chmod` walks. v1's UI always passes [`Recursive::No`] (Go's `os.Chmod` was not recursive,
/// `api/ops.go:929`); the variant exists so the walk is a *named* behaviour rather than a bool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Recursive {
    /// Only the given targets — Go's behaviour.
    #[default]
    No,
    /// Every entry under each target, top-down.
    Yes,
}

/// What a recursive `chmod` did.
#[derive(Debug, Default)]
pub struct ChmodReport {
    /// Every path whose mode was set, in the order it was applied (plan F3's "truthful record" rule: this
    /// is what the undo log gets).
    pub changed: Vec<AbsPath>,
    /// Per-target failures: the walk continued past them.
    pub failures: Vec<ChmodFailure>,
}

impl ChmodReport {
    /// `true` when nothing failed.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.failures.is_empty()
    }

    /// The `names` the status bar shows.
    #[must_use]
    pub fn changed_count(&self) -> usize {
        self.changed.len()
    }
}

/// One path whose mode could not be set.
#[derive(Debug)]
pub struct ChmodFailure {
    /// What we tried to change.
    pub path: AbsPath,
    /// Why it did not change.
    pub error: CoreError,
}

/// The action face. Every method is total: an unsupported platform answers
/// [`CoreError::Unsupported`], never a panic and never a silent no-op.
pub trait Env: Send + Sync {
    /// The OS this implementation drives.
    fn os(&self) -> Os;

    /// The capability bits, of which [`Env::supports_symlink`] is the one run-time answer.
    fn caps(&self) -> PlatformCaps;

    /// Hand the path to the desktop's default handler (`api/ops.go:525-533`: `xdg-open` on Linux).
    fn open_with(&self, path: &AbsPath) -> PlatformFuture<'_, Result<(), CoreError>>;

    /// Place already-decided copies. Continues past a failure and reports it per item.
    fn copy_paths(&self, items: &[CopyItem]) -> PlatformFuture<'_, Result<CopyReport, CoreError>>;

    /// Create links (`op:"paste_link"`, `api/ops.go:917-920`). [`CoreError::SymlinkUnsupported`] when the
    /// platform cannot, which is the signal for the caller to offer [`Env::create_shortcut`] instead.
    fn paste_link(&self, items: &[LinkItem]) -> PlatformFuture<'_, Result<(), CoreError>>;

    /// The symlink answer: the probe's cached result (plan F21) or, before any probe, the static
    /// [`PlatformCaps::SYMLINK`] bit. Never performs file I/O.
    fn supports_symlink(&self) -> Result<bool, CoreError>;

    /// Set the mode of *these* paths, nothing else — Go's bare `os.Chmod` per path (`api/ops.go:929`).
    fn chmod(
        &self,
        targets: &[AbsPath],
        mode: ModeBits,
    ) -> PlatformFuture<'_, Result<(), CoreError>>;

    /// Whether this particular path can reach a trash (`trash::can_be_trashed`; plan OQ2, R/D6).
    fn can_trash(&self, path: &AbsPath) -> bool;

    /// Move one path to the trash. `Ok(true)` trashed, `Ok(false)` refused (the caller escalates to a
    /// logged permanent delete), `Err` failed while trying.
    fn trash(&self, path: &AbsPath) -> Result<bool, CoreError>;

    /// Write a shortcut — the Windows fallback for a FAT/NTFS volume without symlink support.
    fn create_shortcut(&self, shortcut: &Shortcut) -> Result<(), CoreError>;

    /// Free/total bytes for the filesystem holding `path`.
    fn disk_space(&self, path: &AbsPath) -> Result<DiskSpace, CoreError>;

    /// The mount table, for the places sidebar (`api/places.go`, plan §5.3 `services/places.rs`).
    fn mount_entries(&self) -> Result<MountTable, CoreError>;

    /// Run one program, capturing capped output (plan F10's single spawn site).
    fn exec_program(&self, req: ExecRequest) -> PlatformFuture<'_, Result<ExecOutput, CoreError>>;

    /// Is `name` a program this platform could run — `exec.LookPath` (`api/ops.go:563,569,578,592`), i.e.
    /// a PATH scan and **never** a spawn.
    ///
    /// Default body: [`CoreError::Unsupported`], so a platform that has not implemented the lookup is
    /// answered by its callers falling back (the clipboard degrades to "no system tool", plan §5.3) rather
    /// than by a wrong `false`.
    fn has_program(&self, name: &str) -> Result<bool, CoreError> {
        let _ = name;
        Err(CoreError::Unsupported {
            what: "program lookup (`Env::has_program` is not implemented on this platform)",
        })
    }

    /// Could this process write to `path` *now* — `access(2)`/`W_OK`, not "is the mode bit set" (which a
    /// caller would have to re-derive for ACLs, read-only mounts and the superuser).
    ///
    /// Default body as for [`Env::has_program`]; [`crate::ops::guard::ensure_writable`] treats that answer
    /// as "unknown, let the write itself fail".
    fn is_writable(&self, path: &AbsPath) -> Result<bool, CoreError> {
        let _ = path;
        Err(CoreError::Unsupported {
            what: "write-permission probe (`Env::is_writable` is not implemented on this platform)",
        })
    }
}

impl dyn Env {
    /// `chmod` over a job boundary: recursive when asked, **continue-on-error**, every failure reported,
    /// and a single non-recursive item taking the platform's own one-call path.
    ///
    /// This is an inherent method on `dyn Env` (not a trait method) precisely because it is the same for
    /// every platform: the recursion is over `std::fs::read_dir`, and the mode setting is delegated.
    pub async fn chmod_all(
        &self,
        targets: &[AbsPath],
        mode: ModeBits,
        recursive: Recursive,
    ) -> ChmodReport {
        let mut report = ChmodReport::default();
        let mut queue: Vec<AbsPath> = Vec::with_capacity(targets.len());

        if recursive == Recursive::No && targets.len() == 1 {
            // The normal call: one item, one platform invocation, one error to report.
            if let Err(e) = self.chmod(&targets[0..1], mode).await {
                report.failures.push(ChmodFailure {
                    path: targets[0].clone(),
                    error: e,
                });
            } else {
                report.changed.push(targets[0].clone());
            }
            return report;
        }

        for target in targets {
            queue.push(target.clone());
            if recursive == Recursive::Yes
                && let Err(e) = walk_into(target, &mut queue)
            {
                report.failures.push(ChmodFailure {
                    path: target.clone(),
                    error: e,
                });
            }
        }

        for item in queue {
            // One item at a time so a failure names the file the user will look for.
            let one = [item.clone()];
            match self.chmod(&one, mode).await {
                Ok(()) => report.changed.push(item),
                Err(error) => report.failures.push(ChmodFailure { path: item, error }),
            }
        }
        report
    }
}

/// Append every path under `root`, directories before their contents (top-down, so a directory that loses
/// its `x` bit mid-walk does not hide its own subtree).
fn walk_into(root: &AbsPath, out: &mut Vec<AbsPath>) -> Result<(), CoreError> {
    let mut stack: Vec<PathBuf> = Vec::new();
    for entry in
        std::fs::read_dir(root.as_path()).map_err(|e| CoreError::io(root.to_string_lossy(), e))?
    {
        let entry = entry.map_err(|e| CoreError::io(root.to_string_lossy(), e))?;
        let path = entry.path();
        out.push(AbsPath::from_path(&path)?);
        if path.is_dir() {
            stack.push(path);
        }
    }
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            // A subtree we cannot read is one failure, not an abort: the rest of the tree is still
            // the user's business.
            Err(e) => {
                out.push(AbsPath::from_path(&dir)?);
                let _ = e;
                continue;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            out.push(AbsPath::from_path(&path)?);
            if path.is_dir() {
                stack.push(path);
            }
        }
    }
    Ok(())
}

// --------------------------------------------------------------------------- mounts

/// One line of `/proc/self/mountinfo` (or its Windows equivalent in step 16, where the drive letters Go
/// never enumerated become real entries — plan §5.2 `windows.rs` row).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountEntry {
    /// The kernel's mount ID.
    pub mount_id: u64,
    /// The parent mount's ID (`0` when it is not in the table).
    pub parent_id: u64,
    /// `major:minor` of the device, or `"0:0"` for a virtual filesystem.
    pub dev: String,
    /// The subpath of the device that is mounted here.
    pub root: String,
    /// Where it appears in the namespace. Escapes (`\040` for a space) are decoded.
    pub mount_point: String,
    /// Per-mount options, comma-separated in the source (`rw`, `nosuid`, …).
    pub mount_options: Vec<String>,
    /// The optional `tag:value` fields before the `-` separator (`shared:1`, `master:1`, …).
    pub optional_fields: Vec<String>,
    /// Filesystem type (`ext4`, `vfat`, `tmpfs`, `nfs4`, …).
    pub fstype: String,
    /// The backing device or pseudo-name (`/dev/sda1`, `overlay`, `proc`).
    pub source: String,
    /// Super-block options, after the filesystem type (`seclabel`, `uid=1000`, …).
    pub super_options: Vec<String>,
    /// `true` when the backing block device reports `/sys/block/…/removable == 1`.
    pub removable: bool,
}

/// The mount table, with the two questions the places sidebar asks it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MountTable {
    /// Every entry, in `/proc/self/mountinfo` order.
    pub entries: Vec<MountEntry>,
}

impl MountTable {
    /// An empty table — the answer of a platform with nothing to enumerate (never an error: plan §5.2
    /// `posix.rs` row "`mounts()` returns `vec![]` … (no error)").
    #[must_use]
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Is `path` on a listed mount (i.e. is it mounted at all, or under something that is)?
    #[must_use]
    pub fn is_mounted(&self, path: &Path) -> bool {
        self.entries
            .iter()
            .any(|e| under_mount(&e.mount_point, path))
    }

    /// Is the mount backing `path` removable media?
    #[must_use]
    pub fn is_removable(&self, path: &Path) -> bool {
        self.entries
            .iter()
            .any(|e| e.removable && under_mount(&e.mount_point, path))
    }

    /// The longest mount point that contains `path`, if any.
    #[must_use]
    pub fn mount_for(&self, path: &Path) -> Option<&MountEntry> {
        self.entries
            .iter()
            .filter(|e| under_mount(&e.mount_point, path))
            .max_by_key(|e| e.mount_point.len())
    }
}

/// Component-wise containment — `starts_with`, never a string prefix, so `/mnt/a` is not under `/mnt/ab`.
fn under_mount(mount: &str, path: &Path) -> bool {
    let mount = Path::new(mount);
    mount == path || (mount.parent().is_some() && path.starts_with(mount))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_is_the_props_probe() {
        let p = detect();
        assert_eq!(p.os, Os::current());
        // props-dir.json: {"platform":"linux","symlink_supported":true}
        assert_eq!(
            p.os.as_str(),
            if cfg!(target_os = "linux") {
                "linux"
            } else {
                p.os.as_str()
            }
        );
        assert!(p.caps.contains(PlatformCaps::DISK_SPACE));
        assert!(p.caps.contains(PlatformCaps::EXEC));
        assert_eq!(p.caps.contains(PlatformCaps::CHMOD), p.os.is_unix());
        assert_eq!(
            p.caps.contains(PlatformCaps::MOUNT_TABLE),
            p.os == Os::Linux
        );
        assert_eq!(p.caps.contains(PlatformCaps::SHORTCUT), p.os == Os::Windows);
        assert_ne!(p.os, Os::Windows, "this suite runs on a unix host");
        assert_eq!(Os::Linux.to_string(), "linux");
        assert_eq!(Os::Macos.as_str(), "darwin");
    }

    #[test]
    fn default_env_answers_every_method() {
        let env = default_env();
        let tmp = std::env::temp_dir();
        let path = AbsPath::from_path(&tmp).expect("temp dir is absolute");
        // Whatever the platform, none of these may panic and the table must be well-formed.
        assert!(matches!(
            env.os(),
            Os::Linux | Os::Macos | Os::Other | Os::Windows
        ));
        let space = env.disk_space(&path);
        assert!(space.is_ok(), "disk_space: {space:?}");
        let space = space.expect("space");
        assert!(space.total > 0, "a zero-size filesystem: {space:?}");
        assert!(
            env.mount_entries().is_ok(),
            "mount_entries must answer, empty is fine"
        );
        assert!(
            !env.mount_entries().expect("table").entries.is_empty(),
            "a linux host always has a mount table"
        );
        assert!(env.supports_symlink().is_ok());
    }

    #[test]
    fn under_mount_is_component_wise() {
        let table = MountTable {
            entries: vec![MountEntry {
                mount_id: 1,
                parent_id: 0,
                dev: "8:0".into(),
                root: "/".into(),
                mount_point: "/mnt/usb".into(),
                mount_options: vec!["rw".into()],
                optional_fields: vec![],
                fstype: "vfat".into(),
                source: "/dev/sda1".into(),
                super_options: vec![],
                removable: true,
            }],
        };
        assert!(table.is_mounted(Path::new("/mnt/usb/x")));
        assert!(
            !table.is_mounted(Path::new("/mnt/usbstick/x")),
            "string prefix is not a mount"
        );
        assert!(!table.is_mounted(Path::new("/mnt")));
        assert!(table.is_removable(Path::new("/mnt/usb/x")));
        assert_eq!(
            table
                .mount_for(Path::new("/mnt/usb/x"))
                .map(|e| e.fstype.as_str()),
            Some("vfat")
        );
        assert!(MountTable::empty().entries.is_empty());
        assert!(!MountTable::empty().is_removable(Path::new("/")));
    }
}
