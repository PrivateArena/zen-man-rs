//! Shared fixture for the `zenman-ui` integration tests: a live `Session` over a private
//! temporary data directory, and the **deterministic** listing tree every file-pane test renders.
//!
//! The *tests* do IO — the crate under test never does (rule 7); the engine IO is the sanctioned
//! path (`Session::open` is core's own seam).
//!
//! The listing tree is the reason these tests can assert an *order*: fixed names, fixed count and
//! **fixed modification times**, so no test depends on when it ran or on the developer's home
//! directory. That is also what makes the snapshot baselines (`tests/snapshots.rs`) comparable
//! between machines.
//!
//! `allow(dead_code)`: this module is shared by every `zenman-ui` integration target, and each
//! target uses a different subset of it — the gate is `cargo xtask ci`'s zero-warning rule, and a
//! helper file cannot satisfy it per-target any other way.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use egui_kittest::Harness;
use zenman_core::session::{Session, SessionPaths};
use zenman_core::store::config::ZenConfig;
use zenman_ui::app::ZenApp;

// The shared scratch-tree delete (`remove_tree`), included textually from `zenman-core` — the same
// file every other test target uses. The fixture *names* here stay deterministic on purpose (see
// `fixture_base`), so only the delete is shared, not the naming.
#[path = "../../../zenman-core/src/scratch.rs"]
mod scratch;

/// Where every fixture of this test binary lives: cargo's own per-integration-test scratch
/// directory (`CARGO_TARGET_TMPDIR`, i.e. `<workspace>/target/tmp`) under `zenman-ui-fixtures`.
///
/// It is **deterministic**: no process id, so the *path painted into a snapshot baseline* — the
/// breadcrumb of `02-file-list.png` / `03-file-grid.png` — is the same string on every run of the
/// same checkout, which is what makes those PNGs byte-reproducible (plan §9.2 step-8c review 3).
/// Collisions are impossible the other way: the directory is per *cargo test invocation* of one
/// target, and every scene/test already passes its own unique tagged subdirectory, so two tests
/// never share a tree. Only if the scratch directory cannot be created at all (a read-only or
/// missing `target/`) does this fall back to the system temp directory — still pid-free.
fn fixture_base() -> PathBuf {
    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("zenman-ui-fixtures");
    if std::fs::create_dir_all(&scratch).is_ok() {
        return scratch;
    }
    let fallback = std::env::temp_dir().join("zenman-ui-fixtures");
    std::fs::create_dir_all(&fallback)
        .expect("neither cargo's scratch directory nor the temp directory is creatable");
    fallback
}

/// A session in `<scratch>/zenman-ui-fixtures/zenman-ui-test-<tag>`; two tags never collide.
#[must_use]
pub fn session(tag: &str) -> Session {
    session_with_launcher(tag).0
}

/// …and the launcher recorder it is wrapped in, for the tests that want to assert what the shell
/// asked the desktop to open.
pub fn session_with_launcher(tag: &str) -> (Session, std::sync::Arc<NoDesktopEnv>) {
    let dir = fixture_base().join(format!("zenman-ui-test-{tag}"));
    std::fs::create_dir_all(&dir).expect("the fixture directory is creatable");
    let paths = SessionPaths::new(
        dir.join("config.json"),
        dir.join("action.log"),
        None,
        dir.join("probe"),
    )
    .with_db_open(1, std::time::Duration::from_millis(1));
    let env = std::sync::Arc::new(NoDesktopEnv::new());
    let (session, _notes) = Session::open(
        paths,
        ZenConfig::default(),
        std::sync::Arc::clone(&env) as std::sync::Arc<dyn zenman_core::platform::Env>,
        ZenConfig::default().limits(),
    )
    .expect("a session opens over a fresh directory");
    (session, env)
}

/// The `Env` every UI harness runs on: the host platform for every question, **except** the desktop
/// launcher, which is recorded instead of run.
///
/// The rule is that a test may not launch the desktop. `Env::open_with` hands a path to `xdg-open`,
/// which on a developer's machine opens an editor window — and when the path is a *directory* it asks
/// the session manager for the default **file manager**, which on a machine where zen-man-rs is
/// installed means this project's test suite spawning this project's own binary (and the desktop
/// reporting the result in a modal dialog). None of it is observable from a headless `egui_kittest`
/// frame, so recording the hand-over is both the hermetic answer and the stronger assertion: the test
/// can name the path the launcher was asked about.
///
/// Every other method forwards to [`zenman_core::platform::default_env`], because the disk meter, the
/// mount table, the trash predicate and the writable probe are exactly what the shell renders and must
/// see for real.
///
/// `exec_program` is recorded for the same reason `open_with` is: it is the *other* spawn the shell can
/// reach from a menu row — design §4's `Execute` — and a test that clicks it would otherwise really
/// start the fixture script (and, sandboxed, really try to make namespaces). The core suite's `OpEnv`
/// set the pattern (`zenman-core/tests/contract_ops.rs:1060`): record the whole [`ExecRequest`], answer
/// a successful [`ExecOutput`], spawn nothing.
pub struct NoDesktopEnv {
    inner: std::sync::Arc<dyn zenman_core::platform::Env>,
    opened: std::sync::Mutex<Vec<String>>,
    /// Every [`ExecRequest`] handed to `exec_program`, in order — argv, environment, cwd and `detached`
    /// and all, because for `Execute` the request *is* the behaviour (design §4: one argv vector, no
    /// shell string).
    execs: std::sync::Mutex<Vec<zenman_core::platform::ExecRequest>>,
}

impl NoDesktopEnv {
    /// Wrap the host's environment.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: zenman_core::platform::default_env(),
            opened: std::sync::Mutex::new(Vec::new()),
            execs: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// The paths handed to the launcher, in order.
    #[must_use]
    pub fn opened(&self) -> Vec<String> {
        self.opened.lock().expect("not poisoned").clone()
    }

    /// The requests handed to `exec_program`, in order — the launch tests' argv assertions, read the
    /// same way [`Self::opened`] reads the launcher's.
    #[must_use]
    pub fn execs(&self) -> Vec<zenman_core::platform::ExecRequest> {
        self.execs.lock().expect("not poisoned").clone()
    }
}

impl Default for NoDesktopEnv {
    fn default() -> Self {
        Self::new()
    }
}

impl zenman_core::platform::Env for NoDesktopEnv {
    fn os(&self) -> zenman_core::platform::Os {
        self.inner.os()
    }

    fn caps(&self) -> zenman_core::platform::PlatformCaps {
        self.inner.caps()
    }

    fn open_with(
        &self,
        path: &zenman_core::path::AbsPath,
    ) -> zenman_core::platform::PlatformFuture<'_, Result<(), zenman_core::error::CoreError>> {
        self.opened
            .lock()
            .expect("not poisoned")
            .push(path.to_string_lossy());
        // `Ok`: that is what the host answered before (`xdg-open` spawns and returns), so the shell's
        // success lane is the one under test, and the refusal lane keeps its own tests in
        // `zenman-core`'s `contract_ops`.
        zenman_core::platform::ready(Ok(()))
    }

    fn copy_paths(
        &self,
        items: &[zenman_core::platform::CopyItem],
    ) -> zenman_core::platform::PlatformFuture<
        '_,
        Result<zenman_core::platform::CopyReport, zenman_core::error::CoreError>,
    > {
        self.inner.copy_paths(items)
    }

    fn paste_link(
        &self,
        items: &[zenman_core::platform::LinkItem],
    ) -> zenman_core::platform::PlatformFuture<'_, Result<(), zenman_core::error::CoreError>> {
        self.inner.paste_link(items)
    }

    fn supports_symlink(&self) -> Result<bool, zenman_core::error::CoreError> {
        self.inner.supports_symlink()
    }

    /// Design §5.4's sandbox capability, answered by the **real PATH scan** and deliberately *not*
    /// forwarded to `self.inner.supports_sandbox()`: that probe is the measured one, and forwarding would
    /// spawn `unshare` from a test process — exactly what this fake exists to prevent (see the type's
    /// docs on `exec_program`). The kernel half of the answer is measured once, for real, in
    /// `zenman-core`'s `platform::linux` tests; here only the row's *presence* is under test.
    fn supports_sandbox(&self) -> Result<bool, zenman_core::error::CoreError> {
        Ok(self
            .inner
            .has_program(zenman_core::platform::PROGRAM_UNSHARE)
            .unwrap_or(false))
    }

    fn chmod(
        &self,
        targets: &[zenman_core::path::AbsPath],
        mode: zenman_core::mode::ModeBits,
    ) -> zenman_core::platform::PlatformFuture<'_, Result<(), zenman_core::error::CoreError>> {
        self.inner.chmod(targets, mode)
    }

    fn can_trash(&self, path: &zenman_core::path::AbsPath) -> bool {
        self.inner.can_trash(path)
    }

    fn trash(
        &self,
        path: &zenman_core::path::AbsPath,
    ) -> Result<bool, zenman_core::error::CoreError> {
        self.inner.trash(path)
    }

    fn create_shortcut(
        &self,
        shortcut: &zenman_core::platform::Shortcut,
    ) -> Result<(), zenman_core::error::CoreError> {
        self.inner.create_shortcut(shortcut)
    }

    fn disk_space(
        &self,
        path: &zenman_core::path::AbsPath,
    ) -> Result<zenman_core::platform::DiskSpace, zenman_core::error::CoreError> {
        self.inner.disk_space(path)
    }

    fn mount_entries(
        &self,
    ) -> Result<zenman_core::platform::MountTable, zenman_core::error::CoreError> {
        self.inner.mount_entries()
    }

    fn exec_program(
        &self,
        req: zenman_core::platform::ExecRequest,
    ) -> zenman_core::platform::PlatformFuture<
        '_,
        Result<zenman_core::platform::ExecOutput, zenman_core::error::CoreError>,
    > {
        // Recorded, never forwarded (module docs): the exit status the launcher *would* have had is not
        // what a UI test asserts, and the one that is (`Execute` ran, `Execute (sandbox)` passed the
        // wrapper's variables) is fully decided by the request the shell built.
        self.execs.lock().expect("not poisoned").push(req.clone());
        zenman_core::platform::ready(Ok(zenman_core::platform::ExecOutput {
            code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
        }))
    }

    fn has_program(&self, name: &str) -> Result<bool, zenman_core::error::CoreError> {
        self.inner.has_program(name)
    }

    fn is_writable(
        &self,
        path: &zenman_core::path::AbsPath,
    ) -> Result<bool, zenman_core::error::CoreError> {
        self.inner.is_writable(path)
    }
}

// --------------------------------------------------------------------------- the deterministic tree

/// Rows of generated filler: enough that one page (`Limits::dir_page_default`, Go's 200) does not
/// hold the listing, so the pagination banner and the `LoadMore` path are both reachable.
pub const BULK_ROWS: usize = 450;

/// Every row of [`listing_tree`]'s listing directory: 2 directories + 5 named files + [`BULK_ROWS`].
pub const ROW_COUNT: usize = BULK_ROWS + 7;

/// The first mtime the fixture assigns (2023-11-14 22:13:20 UTC), one *day* apart per row.
const MTIME_BASE: u64 = 1_700_000_000;

/// One day, the fixture's mtime step.
const DAY: u64 = 86_400;

/// The listing tree the file-pane and snapshot tests render.
///
/// ```text
/// <scratch>/zenman-ui-fixtures/zenman-ui-fixture-<tag>/listing/
///   docs/                    (dir, three files inside)
///   reports/                 (dir, empty)
///   .hidden                  (a dot file: the backend never filters hidden rows)
///   a.txt  b.txt  c.md
///   twenty-eight-chars-of-name.txt   (the ellipsis case)
///   row-0000.txt … row-0449.txt      (the bulk, and the page boundary)
/// ```
///
/// Every *file* gets a distinct mtime, assigned in **reverse alphabetical order**: that makes
/// "sort by date ascending" mean "reverse alphabetical", which is an assertion a test can state
/// exactly. The two directories get their own distinct pair, oldest in listing order, so a date
/// sort of the directory class is deterministic too.
///
/// Rebuilt on every call — idempotent, and cheap enough (hundreds of small files) that a test can
/// not compare against a stale tree.
pub fn listing_tree(tag: &str) -> PathBuf {
    let root = fixture_base().join(format!("zenman-ui-fixture-{tag}"));
    let listing = root.join("listing");
    std::fs::create_dir_all(listing.join("docs")).expect("fixture directory");
    std::fs::create_dir_all(listing.join("reports")).expect("fixture directory");
    write(&listing.join("docs/readme.md"), b"# readme\n");
    write(&listing.join("docs/notes.txt"), b"notes\n");
    write(&listing.join("docs/big.bin"), &vec![b'x'; 4096]);
    write(&listing.join("reports/empty"), b"\n");
    for name in [
        ".hidden",
        "a.txt",
        "b.txt",
        "c.md",
        "twenty-eight-chars-of-name.txt",
    ] {
        write(&listing.join(name), b"fixture\n");
    }
    for index in 0..BULK_ROWS {
        write(&listing.join(format!("row-{index:04}.txt")), b"bulk row\n");
    }
    // The mtimes, in reverse alphabetical order of the *displayed* rows: the last row of the
    // listing is the oldest. The index comes from the position (`last - index`), never from
    // `.rev().enumerate()` — an `.enumerate()` on a reversed iterator counts in the *displayed*
    // order, and two rows that end up with the same mtime leave their relative order to the
    // comparator, which is how a directory listing can reshuffle between frames.
    let mut rows = listing_names();
    rows.retain(|name| name != "docs" && name != "reports");
    let last = rows.len() - 1;
    for (index, name) in rows.iter().enumerate() {
        set_mtime(
            &listing.join(name),
            MTIME_BASE + (last - index) as u64 * DAY,
        );
    }
    // The two directories get their own distinct pair, oldest in *listing* order (`docs` first),
    // so a date-ascending listing reads `docs, reports` and a descending one reverses the class.
    for (index, name) in ["docs", "reports"].iter().enumerate() {
        set_mtime(&listing.join(name), MTIME_BASE + index as u64 * DAY);
    }
    listing
}

/// The rows of [`listing_tree`], in the listing order the engine produces: directories first, then
/// the lowercase byte compare of plan §2.3 (so `.hidden` before `a.txt`, `row-…` before `t`).
#[must_use]
pub fn listing_names() -> Vec<String> {
    let mut files: Vec<String> = vec![
        ".hidden".into(),
        "a.txt".into(),
        "b.txt".into(),
        "c.md".into(),
        "twenty-eight-chars-of-name.txt".into(),
    ];
    for index in 0..BULK_ROWS {
        files.push(format!("row-{index:04}.txt"));
    }
    files.sort_by(|a, b| {
        a.to_lowercase()
            .cmp(&b.to_lowercase())
            .then_with(|| a.cmp(b))
    });
    let mut names = vec!["docs".to_string(), "reports".to_string()];
    names.extend(files);
    names
}

/// The date-sort order of **the loaded rows** — what a local re-sort of a page must produce, as
/// opposed to what a refetch of the whole listing would (`listing_tree`'s mtimes are assigned over
/// all rows, so a page is a *prefix* of the listing, not a prefix of the date order).
///
/// `loaded` must be in listing order. Dirs first (their class keeps listing order when ascending,
/// reversed when descending — plan §2.3's dirs-first invariant), then the loaded files in reverse
/// listing order, because [`listing_tree`] makes mtime rise with the listing position.
#[must_use]
pub fn loaded_by_date(loaded: &[String], descending: bool) -> Vec<String> {
    let (mut dirs, files): (Vec<String>, Vec<String>) = loaded
        .iter()
        .cloned()
        .partition(|name| name == "docs" || name == "reports");
    if descending {
        dirs.reverse();
    }
    let mut ordered = dirs;
    // Ascending: oldest first, i.e. reverse listing order. Descending: the listing order itself
    // (newest mtime first *is* the listing order, because the fixture's mtimes rise with it).
    let mut files = files;
    if !descending {
        files.reverse();
    }
    ordered.extend(files);
    ordered
}

/// [`loaded_by_date`]'s ascending half, named for the assertion it writes.
#[must_use]
pub fn loaded_by_date_ascending(loaded: &[String]) -> Vec<String> {
    loaded_by_date(loaded, false)
}

/// [`loaded_by_date`]'s descending half (newest mtime first, dirs still first).
#[must_use]
pub fn loaded_by_date_descending(loaded: &[String]) -> Vec<String> {
    loaded_by_date(loaded, true)
}

/// A small two-level tree, for the flat (recursive) listing modes — [`listing_tree`] cannot show a
/// group of more than one row, and step 9d's grouped view needs exactly that.
///
/// ```text
/// <scratch>/zenman-ui-fixture-<tag>/nested/
///   alpha/          a1.txt  a2.txt
///   beta/           b1.txt
///     inner/        b2.txt
///   top.txt
/// ```
///
/// Eight rows recursively, three in the plain listing.
#[must_use]
pub fn nested_tree(tag: &str) -> PathBuf {
    let root = fixture_base().join(format!("zenman-ui-fixture-{tag}"));
    let nested = root.join("nested");
    std::fs::create_dir_all(nested.join("alpha")).expect("fixture directory");
    std::fs::create_dir_all(nested.join("beta").join("inner")).expect("fixture directory");
    write(&nested.join("alpha/a1.txt"), b"a1\n");
    write(&nested.join("alpha/a2.txt"), b"a2\n");
    write(&nested.join("beta/b1.txt"), b"b1\n");
    write(&nested.join("beta/inner/b2.txt"), b"b2\n");
    write(&nested.join("top.txt"), b"top\n");
    nested
}

/// The **flat** rows of [`nested_tree`] in the order the engine produces them: directories first
/// (`alpha`, `beta`, `inner` — sorted by *base* name, `api/dir.go:315`), then the files, also by base
/// name. Each row is `(rel_path, is_dir)`.
///
/// Note what that order is *not*: grouped. `top.txt` sorts after `b2.txt`, so the listing root's group
/// is interrupted and gets a second header — which is what Go's run-based grouping does too
/// (`file-list.js:71-97`), and why a header's count is the whole group's, not the run's.
#[must_use]
pub fn nested_flat_rows() -> Vec<(String, bool)> {
    [
        ("alpha", true),
        ("beta", true),
        ("beta/inner", true),
        ("alpha/a1.txt", false),
        ("alpha/a2.txt", false),
        ("beta/b1.txt", false),
        ("beta/inner/b2.txt", false),
        ("top.txt", false),
    ]
    .into_iter()
    .map(|(rel, is_dir)| (rel.to_string(), is_dir))
    .collect()
}

/// The same rows with the folder rows suppressed — [`nested_flat_rows`] minus its three `is_dir`
/// entries, because `no_folders` suppresses *rows*, never the recursion (`api/dir.go:141-143`).
#[must_use]
pub fn nested_flat_file_rows() -> Vec<(String, bool)> {
    nested_flat_rows()
        .into_iter()
        .filter(|(_, is_dir)| !is_dir)
        .collect()
}

// --------------------------------------------------------------------------- wall-clock settling

/// Run a frame, then sleep 5 ms of **real** time, and repeat until `pred` answers `true` — failing
/// with a clear message once a 2 s wall-clock deadline passes instead of hanging.
///
/// Why frame-count settling is not enough for the flat modes and the `DirChanged` reload: core
/// **coalesces** `DirChanged` on a wall-clock window (`DirService::inner.coalesce =
/// PROGRESS_THROTTLE`, `services/dirs.rs:583`, gated on `Instant::now()`), and a flat page is a
/// real worker-thread job. kittest's fake time advances neither the coalesce gate nor the worker,
/// so a `harness.run()`-only loop spins against a clock that never moves. Sleeping is the only
/// way past both — and the deadline turns a missing event into a named failure, not a hang.
///
/// Why [`Harness::run_ok`] and not [`Harness::run`]: `run` panics when a frame cannot settle inside
/// the harness's step budget (4 by default), and a **live job asks for the next repaint on purpose**
/// (`jobs_ui::keep_polling`) — so the budget says nothing about the thing being waited for, only
/// about how busy the machine is. Under load it blew roughly a run in four, on a suite whose logic
/// had not changed. The deadline is the assertion here; a step budget is not.
pub fn settle_until(
    harness: &mut Harness<'static, ZenApp>,
    what: &str,
    mut pred: impl FnMut(&Harness<'static, ZenApp>) -> bool,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        // Stepped, not settled: see the module note. `None` means "still repainting", which is what
        // the loop is for.
        let _settled = harness.run_ok();
        if pred(harness) {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!("settle_until({what:?}): timed out after 2 s of wall clock waiting for it");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
}

/// Pin a path's mtime (files *and* directories — `File::open` on a directory is read-only and
/// `futimens` accepts it, which is what makes a directory row's date cell deterministic too).
fn set_mtime(path: &Path, unix_secs: u64) {
    let when = SystemTime::UNIX_EPOCH + Duration::from_secs(unix_secs);
    let open = std::fs::File::options()
        .write(false)
        .read(true)
        .open(path)
        .or_else(|e| panic!("opening {} to set its mtime: {e}", path.display()));
    match open.and_then(|file| file.set_modified(when)) {
        Ok(()) => {}
        Err(e) => panic!("setting {} to {unix_secs}: {e}", path.display()),
    }
}

/// Remove a fixture tree, entry by entry — delegated to the shared scratch-module delete
/// (`std::fs::remove_dir_all` is banned workspace-wide, plan §5.1: product deletion goes through
/// `trash`, with a log) — and a fixture reset is deliberately *not* a product delete, so the
/// shared helper does the unlogged removal instead of pretending to be one.
pub fn remove_tree(path: &Path) {
    let _ = scratch::remove_tree(path);
}
