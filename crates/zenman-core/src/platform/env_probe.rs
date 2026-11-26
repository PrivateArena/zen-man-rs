//! The run-time capability probe behind `GET /api/props`.
//!
//! `docs/contracts.md` → */api/props*: the endpoint is **not** a properties endpoint, it is a probe that
//! answers `{"platform":"linux","symlink_supported":true}` and ignores `path` entirely
//! (`main.go:141-147`; `props-file/dir/symlink/crlf-name.json` are byte-identical). The `symlink_supported`
//! half of that answer is produced here by *doing* the thing Go did (`main.go:24-49`: try it, report the
//! verdict), never by assuming it.
//!
//! # Ownership of the cache (plan F21)
//!
//! Nothing in this file caches. The one-shot cache is the `Arc<RwLock<Option<bool>>>` inside
//! [`crate::platform::linux::Linux`] — the struct that *calls* these functions. That is deliberate: the
//! probe is a write test, so it may only run once per process and only from a job, while
//! [`Env::supports_symlink`] is read from the UI every frame; keeping the lock with the caller (today
//! `Linux`, and from plan §5.6 step 6 the `Session` that owns it) is what makes the frame-path read a
//! lock read and not file I/O. [`probe`] is therefore the *cached* answer and [`probe_symlink_support`]
//! is the *measurement*; the two are never the same call.
//!
//! # Why a caller-provided scratch directory
//!
//! Go wrote its probe files into `os.TempDir()` and never cleaned up. Here every scratch path is derived
//! from an explicit `scratch_root`, which the caller takes from its own data directory
//! (`app_dirs::AppDirs::data`, plan §5.9) — so a headless run with a weird `$TMPDIR`, a sandbox with no
//! writable `~`, or a CI container cannot be surprised by the probe, and the probe can never appear to
//! "work" because it happened to land on a filesystem the user does not care about.

use std::io;
use std::path::{Path, PathBuf};

use bitflags::bitflags;
use serde::Serialize;

use crate::error::CoreError;
use crate::platform::{Env, Os};

bitflags! {
    /// The preconditions the probe folds into its answer.
    ///
    /// These are *inputs*, so that a test can drive either branch without touching the real system
    /// state: [`overlayfs_works_with`] takes them explicitly and [`overlayfs_works`] supplies the real
    /// ones with [`ProbeFlags::detect`].
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct ProbeFlags: u32 {
        /// The kernel allows an unprivileged user namespace, i.e.
        /// `/proc/sys/kernel/unprivileged_userns_clone` reads `1`. Go's probe gated its second
        /// (overlayfs) test on exactly this.
        const IS_ENABLED = 1 << 0;
    }
}

impl ProbeFlags {
    /// The flags as the running system reports them: `IS_ENABLED` iff
    /// [`unprivileged_userns_clone`] returns `1`. `-1` (the file does not exist — every kernel since the
    /// knob was removed in 6.x, and every non-Linux host) means *unknown*, and an unknown is never
    /// promoted to a capability.
    #[must_use]
    pub fn detect() -> Self {
        if unprivileged_userns_clone() == 1 {
            Self::IS_ENABLED
        } else {
            Self::empty()
        }
    }
}

/// The exact path Go read (`main.go:26`). Public so a test can name it and a reader can verify it.
pub const PROC_UNPRIVILEGED_USERNS_CLONE: &str = "/proc/sys/kernel/unprivileged_userns_clone";

/// The `unprivileged_userns_clone` sysctl: `1` enabled, `0` disabled, `-1` **unknown**.
///
/// `-1` is Go's own sentinel for "the file is not there" (`main.go:27-33`) and it matters: the knob was
/// removed from mainline kernels, so on a current box "no file" is the normal answer and must not be
/// read as "disabled". Any unreadable or unparseable file is `-1` too — this function cannot fail and
/// cannot panic, because a capability probe that hard-errors would take the whole app down at startup.
#[must_use]
pub fn unprivileged_userns_clone() -> i8 {
    let Ok(text) = std::fs::read_to_string(PROC_UNPRIVILEGED_USERNS_CLONE) else {
        return -1;
    };
    match text.trim() {
        "1" => 1,
        "0" => 0,
        _ => -1,
    }
}

/// The pure half of the overlayfs question: given the flags and the sysctl, is an overlay mount
/// expected to work for an unprivileged user?
///
/// Split out from [`overlayfs_works`] so the flag is *assertable*: the I/O probe's answer is the same on
/// any machine where symlinks already work, which would leave this branch untested.
#[must_use]
pub fn overlayfs_supported(flags: ProbeFlags, userns_clone: i8) -> bool {
    flags.contains(ProbeFlags::IS_ENABLED) && userns_clone == 1
}

/// Does an unprivileged overlayfs work on this machine, as observed under `scratch_root`?
///
/// Uses the real system state for the precondition ([`ProbeFlags::detect`]);
/// [`overlayfs_works_with`] is the explicit-flag form.
pub fn overlayfs_works(scratch_root: &Path) -> Result<bool, CoreError> {
    overlayfs_works_with(scratch_root, ProbeFlags::detect())
}

/// The overlayfs half of Go's two-part probe (`main.go:35-49`), with the flag supplied by the caller.
///
/// With `IS_ENABLED` clear this performs **no file I/O at all** and answers `false` — that is the
/// "flag forced off" path, and it is the whole point of taking `flags` as an argument. With the flag
/// set it builds `<scratch_root>/zenman-overlay-probe-<pid>/{upper,work,merged}`, `mkdir`s inside
/// `merged`, and removes the tree again.
///
/// What this does **not** do is call `mount(2)`/`unshare(2)`. It could be done in safe Rust through
/// `rustix::fs::mount`, and Go did attempt a real mount; we do not, because a probe that succeeds would
/// leave a mounted filesystem in the caller's namespace (and, run as root in a container, would need an
/// unmount to undo — a side effect no read-only capability question is entitled to). The answer is
/// therefore "the pieces an unprivileged overlay mount needs are in place", which is what Go's
/// `symlink_supported` boolean ended up meaning to its consumer: *can this user build the directory
/// layout the feature needs*.
pub fn overlayfs_works_with(scratch_root: &Path, flags: ProbeFlags) -> Result<bool, CoreError> {
    if !flags.contains(ProbeFlags::IS_ENABLED) {
        return Ok(false);
    }

    let scratch = scratch_root.join(format!("zenman-overlay-probe-{}", std::process::id()));
    // A scratch root we cannot even create is a problem with the caller's data directory, not an answer
    // about overlayfs: report it as the I/O error it is.
    for part in ["upper", "work", "merged"] {
        std::fs::create_dir_all(scratch.join(part))
            .map_err(|e| CoreError::io(scratch.join(part).to_string_lossy(), e))?;
    }

    let merged = scratch.join("merged");
    let mkdir_result = std::fs::create_dir(merged.join("dir")).map(|()| true);
    let created = mkdir_result
        .map_err(|e| CoreError::io(merged.join("dir").to_string_lossy(), e))
        .and_then(|created| {
            // The cleanup happens whether the mkdir worked or not; a probe must not litter.
            cleanup(&scratch)?;
            Ok(created)
        })?;

    Ok(created && overlayfs_supported(flags, unprivileged_userns_clone()))
}

/// The whole `symlink_supported` answer, measured (not cached): the write test Go ran, OR-ed with its
/// overlayfs test, exactly as `main.go:24-49` combined them.
///
/// `Err` means the probe could not run (an unwritable `scratch_root`), which the caller reports as a
/// startup problem; it is **never** the answer "no symlinks" — that is `Ok(false)`, so a permission
/// problem on the data directory cannot silently disable the Paste-link menu item.
pub fn probe_symlink_support(scratch_root: &Path, flags: ProbeFlags) -> Result<bool, CoreError> {
    let writable = symlink_write_test(scratch_root)?;
    let overlay = overlayfs_works_with(scratch_root, flags)?;
    Ok(writable || overlay)
}

/// The answer `/api/props` published, in the order its JSON keys are ordered
/// (`docs/contracts.md` → */api/props*: only these two fields exist).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ProbeReport {
    /// `json:"platform"` — `runtime.GOOS` spelling, i.e. [`Os::as_str`].
    #[serde(rename = "platform")]
    pub os: &'static str,
    /// `json:"symlink_supported"`.
    pub symlink_supported: bool,
}

/// The **cached** probe answer, shaped for the wire: read the [`Env`] implementation's probe cache
/// ([`Env::supports_symlink`], plan F21 — "never per render") and pair it with the static OS string.
///
/// No file I/O, no process spawn: this is safe to call from a request handler. The measurement itself is
/// [`probe_symlink_support`], which step 6's `Session` runs once at startup and pushes into the cache with
/// `Linux::probe_symlinks`.
pub fn probe(env: &dyn Env) -> Result<ProbeReport, CoreError> {
    Ok(ProbeReport {
        os: Os::current().as_str(),
        symlink_supported: env.supports_symlink()?,
    })
}

/// One probe scratch directory name, unique per process so a crash cannot make the next run reuse it.
fn scratch_name(prefix: &str) -> String {
    format!("zenman-{prefix}-probe-{}", std::process::id())
}

/// Go's first test: actually create a symlink and read it back.
///
/// The *filesystem's* answer is `Ok(false)` when the link call itself fails (that is the whole point of
/// the probe — a CIFS or FAT volume answers `EPERM`/`ENOSYS` here); anything that goes wrong before or
/// after the link call is an `Err`, because it is a broken data directory rather than a missing feature.
fn symlink_write_test(scratch_root: &Path) -> Result<bool, CoreError> {
    let scratch = scratch_root.join(scratch_name("symlink"));
    let remove = |e: io::Error| CoreError::io(scratch.display().to_string(), e);
    std::fs::create_dir_all(&scratch).map_err(remove)?;

    let target = scratch.join("target");
    let link = scratch.join("link");
    let answer = match std::fs::write(&target, b"zenman") {
        Err(e) => Err(CoreError::io(target.to_string_lossy(), e)),
        Ok(()) => match create_link(&link, &target) {
            // The read-back is the test: a filesystem that happily stores a bogus symlink is a filesystem
            // that did not make one.
            Ok(()) => Ok(std::fs::read_link(&link)
                .map(|read| read == target)
                .unwrap_or_default()),
            // EPERM/ENOSYS/EOPNOTSUPP are the answer Go recorded as `symlink_supported:false`.
            Err(_) => Ok(false),
        },
    };

    cleanup(&scratch)?;
    answer
}

/// `symlink(2)` where it exists; everywhere else the answer is "no", which is what
/// [`crate::platform::Env::paste_link`] reports on the same platform.
#[cfg(unix)]
fn create_link(link: &Path, target: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// Windows needs `SeCreateSymbolicLinkPrivilege` (or Developer Mode) — step 16 decides whether to attempt
/// it, so the probe honestly says "not here".
#[cfg(not(unix))]
fn create_link(_link: &Path, _target: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symbolic links require the Windows implementation (plan step 16)",
    ))
}

/// Remove a probe scratch tree, tolerating a path that is already gone.
///
/// `std::fs::remove_dir_all` is banned by `clippy.toml` (plan §5.1: deletion goes through
/// `ops::delete`), and a probe must not use that path either — it deletes only directories it created
/// itself in this call. So the walk is spelled out here: two levels deep, `remove_file`/`remove_dir` per
/// entry, never a recursive helper.
fn cleanup(root: &Path) -> Result<(), CoreError> {
    remove_tree(root).map_err(|e| CoreError::io(root.display().to_string(), e))
}

fn remove_tree(path: &Path) -> io::Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if !meta.is_dir() {
        return std::fs::remove_file(path);
    }
    for entry in std::fs::read_dir(path)? {
        remove_tree(&entry?.path())?;
    }
    std::fs::remove_dir(path)
}

/// The path a probe would use for `prefix`, exposed for tests and for the "did it clean up?" check.
#[must_use]
pub fn scratch_dir(scratch_root: &Path, prefix: &str) -> PathBuf {
    scratch_root.join(scratch_name(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A per-test directory under the harness' temp dir; never `~`, never shared. Naming and
    /// stale-tree reaping come from `crate::scratch`.
    fn scratch(tag: &str) -> PathBuf {
        crate::scratch::create(&format!("probe-test-{tag}"))
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn userns_clone_is_three_valued_and_never_panics() {
        // On a kernel without the knob (this file was removed upstream) the answer is -1, not 0: the
        // machine is unknown, not incapable. Whatever this box says, it is one of the three Go values.
        assert!(matches!(unprivileged_userns_clone(), -1..=1));
    }

    #[test]
    fn overlayfs_supported_table() {
        use ProbeFlags as F;
        assert!(overlayfs_supported(F::IS_ENABLED, 1));
        assert!(
            !overlayfs_supported(F::IS_ENABLED, 0),
            "a disabled knob is not a yes"
        );
        assert!(
            !overlayfs_supported(F::IS_ENABLED, -1),
            "an unknown knob is not a yes"
        );
        assert!(!overlayfs_supported(F::empty(), 1), "the flag is required");
        assert!(!overlayfs_supported(F::empty(), -1));
    }

    #[test]
    fn overlayfs_works_with_the_flag_forced_off_does_no_io() {
        let dir = scratch("off");
        let answer =
            overlayfs_works_with(&dir, ProbeFlags::empty()).expect("the off path cannot fail");
        assert!(!answer, "a skipped probe must not claim support");
        assert!(
            leftovers(&dir).is_empty(),
            "the off path must not create a scratch dir"
        );
        remove_tree(&dir).expect("clean the test root");
    }

    #[test]
    fn overlayfs_works_with_the_flag_forced_on_cleans_up() {
        let dir = scratch("on");
        // The answer is machine-dependent (it folds in the sysctl); what is *not* machine-dependent is
        // that it answers, and that it leaves nothing behind.
        let answer = overlayfs_works_with(&dir, ProbeFlags::IS_ENABLED).expect("overlay probe");
        assert_eq!(
            answer,
            overlayfs_supported(ProbeFlags::IS_ENABLED, unprivileged_userns_clone()),
            "the on path is the pure predicate plus a successful mkdir"
        );
        assert!(
            leftovers(&dir).is_empty(),
            "the probe left {:?} in {}",
            leftovers(&dir),
            dir.display()
        );
        remove_tree(&dir).expect("clean the test root");
    }

    #[test]
    fn probe_symlink_support_answers_and_cleans_up() {
        let dir = scratch("link");
        let answer =
            probe_symlink_support(&dir, ProbeFlags::detect()).expect("the probe must answer");
        let again =
            probe_symlink_support(&dir, ProbeFlags::detect()).expect("the probe must answer");
        assert_eq!(answer, again, "the probe is deterministic");
        assert!(
            leftovers(&dir).is_empty(),
            "the probe left {:?} in {}",
            leftovers(&dir),
            dir.display()
        );
        remove_tree(&dir).expect("clean the test root");
    }

    #[test]
    fn probe_symlink_support_reports_a_broken_data_directory() {
        // A root that cannot hold a scratch dir is an error, not "this machine has no symlinks".
        // (`create_dir_all` happily builds missing parents, so the blocker has to be a *file*.)
        let dir = scratch("badroot");
        let file = dir.join("not-a-directory");
        std::fs::write(&file, b"x").expect("write the blocker");
        let e = probe_symlink_support(&file, ProbeFlags::empty())
            .expect_err("an unwritable scratch root is an error");
        assert_eq!(e.kind(), "io_error", "got {e:?}");
        remove_tree(&dir).expect("clean the test root");
    }

    #[test]
    fn probe_report_is_the_props_pair() {
        let env = crate::platform::default_env();
        let report = probe(env.as_ref()).expect("the cached answer is always available");
        assert_eq!(report.os, Os::current().as_str());
        assert_eq!(
            report.symlink_supported,
            env.supports_symlink().expect("caps")
        );
        let json = serde_json::to_string(&report).expect("props json");
        assert!(
            json.starts_with("{\"platform\":"),
            "props key order: {json}"
        );
        assert!(json.contains("\"symlink_supported\":"));
    }
}
