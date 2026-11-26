//! Scratch-directory support for the workspace's tests: one naming rule, one recursive delete that
//! survives the hostile file modes some fixtures set, and one reaper for the trees a run left behind.
//!
//! Tests are why `/tmp` fills up. A scratch tree made at the top of a test body is removed by the
//! statement at the *bottom* of it, which a failing `assert!` skips, and which a `SIGKILL` (a Ctrl-C
//! during `cargo test`, a CI timeout) never runs at all. Nothing reaped the survivors, so they
//! accumulated: 14,038 directories and 23 GB on one developer's `tmpfs`-backed `/tmp`.
//!
//! Two rules keep that bounded:
//!
//! 1. every scratch name ends in `…-<pid>-<seq>`, so the owning process is readable from the name;
//! 2. [`create`] reaps the trees whose owner process is gone before making a new one.
//!
//! So a leak survives at most until the next test binary starts, instead of forever. The reaper is
//! deliberately conservative: a name whose owner is alive is never touched, and a name with no
//! readable owner is reaped only once it is older than [`ABANDONED_AFTER`].
//!
//! Kept free of `crate::` references so the other workspace members can include it with `#[path]`
//! (it is test support, never linked into a shipped binary). `#![allow(dead_code)]` because each
//! includer uses a different subset of this surface.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Once;
use std::sync::atomic::{AtomicU64, Ordering};

/// The prefixes of everything this workspace throws into the temp dir. `xtask-` is the pre-dating
/// spelling of the scratch names under `xtask/src`; it is listed so the reaper also takes those.
const SCRATCH_PREFIXES: [&str; 2] = ["zenman-", "xtask-"];

/// Age after which a scratch tree with no readable owner pid is treated as abandoned. A full
/// `cargo test --workspace` takes minutes, so six hours is far outside any live run.
const ABANDONED_AFTER: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// Sequence counter: unique within a process, so two tests of the same tag, or two tests racing on
/// one thread id, never share a tree.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Set once the reaper has run in this process.
static SWEEP: Once = Once::new();

/// A fresh, empty, uniquely-named scratch tree: `zenman-<name>-<pid>-<seq>`.
///
/// `name` carries the scope (`"walker-depth"`, `"contract-ops-copy"`); the returned path is stable
/// for the caller's own purposes but no two calls collide. Prefer [`Scratch`] when the tree's life
/// should end with the test body — this returns a plain path for the cases that hand the tree to a
/// `Session`/`AppPaths`/`OnceLock` fixture and delete it themselves.
pub fn create(name: &str) -> PathBuf {
    sweep_once();
    let seq = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("zenman-{name}-{}-{seq}", std::process::id()));
    // Cheap insurance against a name reused within one process (`remove_tree` on a fresh path is a
    // no-op); the cross-run case is the reaper's job.
    let _ = remove_tree(&path);
    std::fs::create_dir_all(&path)
        .unwrap_or_else(|e| panic!("cannot create the scratch tree {path:?}: {e}"));
    path
}

/// The same tree, deleted when it goes out of scope. The panic-safe form: `Drop` runs on the unwind
/// path, so a failing assertion still cleans up.
pub struct Scratch(PathBuf);

impl Scratch {
    /// Create `zenman-<name>-<pid>-<seq>` and own it.
    pub fn new(name: &str) -> Self {
        Self(create(name))
    }

    /// The directory path.
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Give up ownership *and* the cleanup: the tree survives this value. Only for a case that
    /// reports the path to the caller and reaps it itself.
    pub fn into_path_buf(self) -> PathBuf {
        let path = self.0.clone();
        std::mem::forget(self);
        path
    }
}

impl std::ops::Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::fmt::Debug for Scratch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = remove_tree(&self.0);
    }
}

/// Recursive delete for a *test scratch tree*.
///
/// `std::fs::remove_dir_all` is banned workspace-wide (`clippy.toml`: product deletion goes through
/// `ops::delete`, trash-first and logged) and would fail anyway on the fixtures that chmod a
/// directory to `000`, so the modes are relaxed on the way down. Errors are returned rather than
/// swallowed so a caller can decide; the reaper ignores them, since a tree it cannot delete will be
/// retried by the next run.
pub fn remove_tree(path: &Path) -> std::io::Result<()> {
    relax(path);
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() {
        for entry in std::fs::read_dir(path)? {
            remove_tree(&entry?.path())?;
        }
        std::fs::remove_dir(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Reap the scratch trees this workspace leaked in a previous run, if their owner is gone.
///
/// Safe to call from a test binary's setup and from `cargo xtask scratch`.
pub fn sweep_once() {
    SWEEP.call_once(|| {
        sweep_stale();
    });
}

/// [`sweep_once`] without the once-only guard, for the gate that reports a count.
///
/// Returns the number of entries removed. Every removal is best-effort: another test binary running
/// in parallel may delete the same tree, and `ENOENT` there is success.
pub fn sweep_stale() -> usize {
    let base = std::env::temp_dir();
    let Ok(entries) = std::fs::read_dir(&base) else {
        return 0;
    };
    let mut reaped = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !SCRATCH_PREFIXES.iter().any(|p| name.starts_with(p)) {
            continue;
        }
        if !reapable(name, &path) {
            continue;
        }
        if remove_tree(&path).is_ok() {
            reaped += 1;
        }
    }
    reaped
}

/// Does this entry belong to a run that has ended?
///
/// A readable owner pid decides immediately, and a pid that is merely alive always spares the tree:
/// a misread pid can only cost a leak, never a directory a running test is using. Without a pid the
/// only signal is age.
///
/// Exported for `cargo xtask scratch`, which runs the reaper and then refuses to pass while anything
/// reapable is still on disk.
pub fn reapable(name: &str, path: &Path) -> bool {
    match owner_pid(name) {
        Some(pid) if pid == std::process::id() => false,
        Some(pid) => !alive(pid),
        // A tree only, never a loose file: an old `/tmp/zenman-something.txt` with no pid in its name
        // is more likely to be the user's than ours.
        None => is_tree(path) && abandoned(path),
    }
}

/// Is this a directory rather than a file or a symlink? (Symlinks are followed only to *ask*; the
/// delete itself never follows one.)
fn is_tree(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

/// The process id encoded in a scratch name: the first `-`-separated component that is a plain
/// number. Names are built as `<scope>-<case>-<pid>-<seq>`, and every case tag is written with a
/// letter (`d01`, `zoom2`, `profile0`), so the first bare number is the owner.
///
/// `None` off unix, where liveness cannot be cheaply queried: a pid in a name there must not veto the
/// age rule, or nothing on that target would ever be reaped.
fn owner_pid(name: &str) -> Option<u32> {
    if !cfg!(unix) {
        return None;
    }
    name.split('-').find_map(|part| part.parse::<u32>().ok())
}

/// Is a process id still running? `/proc/<pid>` is the only lookup that costs no child process
/// (`Command::output` is banned by `clippy.toml`, and spawning per entry would be absurd).
fn alive(pid: u32) -> bool {
    if !cfg!(unix) {
        // No `/proc` to consult, so nothing to prove: let the age rule decide.
        return false;
    }
    std::path::Path::new("/proc").join(pid.to_string()).exists()
}

/// Has this entry not been touched for [`ABANDONED_AFTER`]? An unreadable mtime counts as fresh, so
/// a surprise only ever delays a reap.
fn abandoned(path: &Path) -> bool {
    let Ok(then) = path.metadata().and_then(|m| m.modified()) else {
        return false;
    };
    // A clock in the future means the entry is newer than the epoch jump, not older.
    then.elapsed().is_ok_and(|age| age > ABANDONED_AFTER)
}

/// Every scratch entry in the temp dir, with its age: the reaper's leftovers.
///
/// Called straight after [`sweep_stale`], a non-empty answer means a run is still holding those
/// trees, or that a tree could not be deleted — the second case is what the hygiene gate fails on.
pub fn survivors() -> Vec<(PathBuf, std::time::Duration)> {
    let base = std::env::temp_dir();
    let Ok(entries) = std::fs::read_dir(&base) else {
        return Vec::new();
    };
    let mut left = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let named = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| SCRATCH_PREFIXES.iter().any(|p| name.starts_with(p)));
        if !named {
            continue;
        }
        let age = match path
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t.elapsed())
        {
            Ok(Ok(age)) => age,
            // No clock to age by: report it as brand new, which keeps the gate's "still reapable and
            // still here" test as the only thing it can fail on.
            _ => std::time::Duration::ZERO,
        };
        left.push((path, age));
    }
    left.sort();
    left
}

/// Give the owner back `rw`/`rx` on a tree so it can be walked and unlinked. Only touches what
/// actually needs it, so the cost on a 10k-file fixture tree is one `stat` per directory.
fn relax(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return;
        };
        if meta.is_dir() {
            if meta.permissions().mode() & 0o700 != 0o700 {
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
            }
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    relax(&entry.path());
                }
            }
        } else if !meta.is_symlink() && meta.permissions().mode() & 0o600 != 0o600 {
            // Not a symlink: `set_permissions` would follow the link and chmod a file that is not in
            // the scratch tree at all (a fixture's `link -> /etc/…`), and a link's own mode is 0777
            // anyway, so the guard above already spares it.
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_owner_pid_is_the_first_bare_number_in_the_name() {
        // Today's shape, plus the two older shapes the reaper still has to read (`-ThreadId(..)` and
        // a nanosecond stamp), where the pid is still the first number.
        assert_eq!(owner_pid("zenman-walker-depth-4211-3"), Some(4211));
        assert_eq!(
            owner_pid("zenman-legacy-apply-7781-ThreadId(12)"),
            Some(7781)
        );
        assert_eq!(owner_pid("zenman-session-zoom-9912-881234"), Some(9912));
        // A reused fixture directory: no owner to read, so only the age rule may take it.
        assert_eq!(owner_pid("zenman-ui-fixtures"), None);
    }

    #[test]
    fn a_live_owner_spares_the_tree_and_a_gone_one_does_not() {
        let ours = create("scratch-selftest");
        let name = ours
            .file_name()
            .expect("named")
            .to_string_lossy()
            .into_owned();
        assert!(
            !reapable(&name, &ours),
            "the running process must spare its own tree"
        );
        // Pid 0 is never a process, so `/proc/0` never exists: the reaper's "owner is gone" answer.
        let gone = std::env::temp_dir().join("zenman-scratch-selftest-0-1");
        std::fs::create_dir_all(&gone).expect("tree");
        assert!(
            reapable("zenman-scratch-selftest-0-1", &gone),
            "a tree whose owner has exited must be reaped"
        );
        remove_tree(&gone).expect("delete");
        remove_tree(&ours).expect("delete");
    }

    #[test]
    fn only_a_directory_is_a_tree() {
        // The age rule takes no-pid names, so it is restricted to directories: a stale
        // `/tmp/zenman-<something>.txt` with no pid is more likely the user's than ours.
        assert!(!is_tree(std::path::Path::new("/dev/null")));
        assert!(!is_tree(std::path::Path::new("/definitely/not/here")));
    }

    #[test]
    fn the_delete_relaxes_the_modes_a_fixture_sets() {
        let root = Scratch::new("scratch-selftest");
        let locked = root.join("noread");
        std::fs::create_dir_all(locked.join("inner")).expect("nested");
        std::fs::write(locked.join("inner/file.txt"), b"x").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
                .expect("lock");
        }
        // `remove_dir_all` would fail here, which is why the workspace bans it and this walks instead.
        remove_tree(&locked).expect("a 000 directory must still delete");
        assert!(!locked.exists());
    }

    #[test]
    fn the_sweep_reports_what_a_live_run_is_holding() {
        let ours = create("scratch-hold");
        assert!(
            survivors().iter().any(|(path, _)| path == &ours),
            "a tree this process made must be listed, not reaped"
        );
        remove_tree(&ours).expect("delete");
    }
}
