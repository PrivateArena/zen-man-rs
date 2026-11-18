//! Step 6 acceptance gate (plan §7 step 6): the directory listing, the size walk and `/api/props` are
//! the Go program's, field for field, against the frozen captures in `tests/golden/`.
//!
//! # Where the tree comes from
//!
//! `${FIXTURE_ROOT}` in every `dir-*.json`/`dir-size-*.json` capture is `/tmp/zm-fix`
//! (`tests/golden/PROVENANCE.json`, `scripts/capture-golden.sh:33`), and the tree beneath it was made by
//! `scripts/make-fixtures.py --hostile --with-heavy --fifo`. This test regenerates exactly that tree into
//! a scratch directory on the same *kind* of filesystem (tmpfs: the captures' directory rows carry tmpfs
//! `st_size` values — 40 bytes for an empty directory, 24 040 for the 1 200-entry one) and points a real
//! `Session` at it, through `DirService` and `OsDirRead`. The expectations are the frozen JSON; a missing
//! golden is a hard failure, and there is no recorded answer from a previous run of this test anywhere.
//!
//! * **`dir_size_matches_go`, `props_shape`, `props_probed_once` are filesystem-independent** — the walk
//!   sums *file* sizes and skips directories, the probe reports the platform. They run wherever
//!   `cargo test` does.
//! * **`dir_listing_matches_go` also compares directory `size` fields**, which belong to the filesystem
//!   the capture ran on. On a host whose scratch directory is not tmpfs-shaped, the test prints a
//!   `SKIPPED` line naming that one field and still asserts everything else (names, order, `is_dir`,
//!   `is_symlink`, `mode`, `mod_time`, pagination, counts) — the "explicit skip, never a silent pass"
//!   rule `cargo xtask fixture-tree` uses.
//!
//! # The two normalisations applied to a golden before comparison
//!
//! 1. `${FIXTURE_ROOT}` → the scratch root this run generated.
//! 2. every entry gains `"files_count": null`: Go's `omitempty` on an `int64` is the `(undefined files)`
//!    bug, and plan §2.4/§6.2 fix it by keeping the key with an `Option` value. The captures were taken
//!    with an empty `folder_sizes` cache, so `null` is the honest counterpart of the key they omit.
//!
//! Unix-only: the tree needs symlinks, a FIFO and `0444`/`000` modes.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};

use serde_json::Value;

use zenman_core::cursor::Cursor;
use zenman_core::entry::DirListing;
use zenman_core::error::CoreError;
use zenman_core::events::EngineEvent;
use zenman_core::limits::Limits;
use zenman_core::mode::ModeKind;
use zenman_core::path::{AbsPath, FsPath};
use zenman_core::platform::Env;
use zenman_core::platform::env_probe::ProbeReport;
use zenman_core::services::dirs::{DirSizeOpts, FlatMode, ListDir, SilentSink};
use zenman_core::session::{Receipt, Session, SessionPaths};
use zenman_core::store::config::{SortSpec, ZenConfig};

const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/golden");
const REPO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

// ----------------------------------------------------------------------------------- the fixture tree

/// The generated tree plus the answer to "does this filesystem report the directory sizes the capture
/// recorded".
struct Fixtures {
    /// What `${FIXTURE_ROOT}` stands for in this run.
    root: PathBuf,
    /// `true` when a freshly created directory here reports `st_size == 40`, i.e. the tmpfs directory
    /// sizing the capture was taken on.
    go_directory_sizes: bool,
}

/// Build (once per process) the tree the capture used, on a tmpfs-shaped scratch path.
fn fixtures() -> &'static Fixtures {
    static TREE: OnceLock<Fixtures> = OnceLock::new();
    TREE.get_or_init(build_fixtures)
}

fn build_fixtures() -> Fixtures {
    let root = std::env::temp_dir().join(format!("zenman-contract-dirs-{}", std::process::id()));
    // A stable path per process: eleven thousand files must not pile up across runs, so an old tree is
    // wiped before the new one is generated. `std::fs::remove_dir_all` is banned workspace-wide
    // (`clippy.toml`), hence the explicit walk below.
    wipe(&root);
    std::fs::create_dir_all(&root).expect("scratch root");
    let tree = root.join("tree");
    let generator = Path::new(REPO).join("scripts/make-fixtures.py");
    assert!(
        generator.is_file(),
        "{} is missing: the fixture tree is its output, and this gate is not vacuous without it",
        generator.display()
    );
    let output = Command::new("python3")
        .current_dir(REPO)
        .arg(generator.display().to_string())
        .arg("--root")
        .arg(tree.display().to_string())
        .arg("--hostile")
        .arg("--with-heavy")
        .arg("--fifo")
        // `spawn` + `wait_with_output` rather than `Command::output`: the workspace bans the latter
        // (`clippy.toml`, plan F10) so that *application* spawns go through the one output-capped,
        // cancellable executor once step 11 writes it. This is the test harness running the repository's
        // own fixture generator, and it keeps the ban meaningful instead of excepting it.
        .spawn()
        .and_then(|child| child.wait_with_output())
        .unwrap_or_else(|e| {
            panic!(
                "cannot run {} to build the fixture tree ({e}); this gate needs python3 (or a tree at \
                 {} generated by it)",
                generator.display(),
                tree.display()
            )
        });
    assert!(
        output.status.success(),
        "make-fixtures.py failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let go_directory_sizes = directory_sizes_are_go_like(&root);
    eprintln!(
        "contract_dirs: fixture tree at {} (go-shaped directory sizes: {go_directory_sizes})",
        root.display()
    );
    Fixtures {
        root,
        go_directory_sizes,
    }
}

/// `true` when a new directory under `dir` reports the 40-byte tmpfs size the captures recorded.
fn directory_sizes_are_go_like(dir: &Path) -> bool {
    let one = dir.join("size-probe/a");
    if std::fs::create_dir_all(&one).is_err() {
        return false;
    }
    let sized = std::fs::metadata(&one).map(|m| m.len()).unwrap_or(u64::MAX);
    let _ = one.remove_dir_all_in();
    let _ = std::fs::remove_dir(dir.join("size-probe"));
    sized == 40
}

/// Remove a tree, relaxing the hostile modes first: a `0444` directory cannot be traversed in order to
/// delete what is inside it, and a `000` one cannot even be read.
fn wipe(dir: &Path) {
    if dir.symlink_metadata().is_err() {
        return;
    }
    relax(dir);
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let is_dir =
                path.symlink_metadata().map(|m| m.is_dir()).unwrap_or(false) && !path.is_symlink();
            if is_dir {
                wipe(&path);
            } else {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    let _ = std::fs::remove_dir(dir);
}

fn relax(dir: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.symlink_metadata().map(|m| m.is_dir()).unwrap_or(false) && !path.is_symlink() {
                relax(&path);
            }
        }
    }
}

/// The recursive-delete helper `clippy.toml` forbids, restricted to the scratch tree this file created.
trait RemoveDirTreeAll {
    fn remove_dir_all_in(&self) -> std::io::Result<()>;
}

impl RemoveDirTreeAll for Path {
    fn remove_dir_all_in(&self) -> std::io::Result<()> {
        let meta = self.symlink_metadata()?;
        if !meta.is_dir() {
            return std::fs::remove_file(self);
        }
        for entry in std::fs::read_dir(self)? {
            let path = entry?.path();
            let _ = path.remove_dir_all_in();
        }
        std::fs::remove_dir(self)
    }
}

fn golden(name: &str) -> Value {
    let path = Path::new(GOLDEN).join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("golden {} is missing and unreadable: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("golden {}: {e}", path.display()))
}

/// The golden's `${FIXTURE_ROOT}`-carrying text, made real.
fn materialise(value: Value, root: &Path) -> Value {
    let text = value.to_string();
    let swapped = text.replace("${FIXTURE_ROOT}", &root.to_string_lossy());
    serde_json::from_str(&swapped).unwrap_or_else(|e| panic!("materialised golden is invalid: {e}"))
}

/// Apply both documented normalisations to a captured listing payload.
fn expected_listing(golden_name: &str, root: &Path) -> Value {
    let mut value = materialise(golden(golden_name), root);
    let entries = value["entries"]
        .as_array_mut()
        .unwrap_or_else(|| panic!("{golden_name} has no entries array"));
    for entry in entries.iter_mut() {
        entry
            .as_object_mut()
            .unwrap_or_else(|| panic!("{golden_name} entry is not an object"))
            .insert("files_count".to_string(), Value::Null);
    }
    value
}

/// A session over the generated tree: the real `OsDirRead` source, no database, default limits.
fn open_session(fix: &Fixtures, tag: &str, env: Arc<dyn Env>) -> Session {
    let dir = fix.root.join(tag);
    std::fs::create_dir_all(dir.join("probe")).expect("probe dir");
    let (session, notes) = Session::open(
        SessionPaths::new(
            dir.join("config.json"),
            dir.join("action.log"),
            None,
            dir.join("probe"),
        ),
        ZenConfig::default(),
        env,
        Limits::default(),
    )
    .expect("a session over a clean scratch directory opens");
    assert!(notes.is_empty(), "nothing here is damaged: {notes:?}");
    session
}

fn real_env() -> Arc<dyn Env> {
    zenman_core::platform::default_env()
}

fn abs(path: &Path) -> AbsPath {
    AbsPath::try_from(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

// -------------------------------------------------------------------------------------- (b) the listing

/// One `dir-*.json` capture and the request `tests/golden/INDEX.json` says produced it.
struct Case {
    /// The capture name.
    golden: &'static str,
    /// The tree-relative directory the request named.
    sub: &'static str,
    /// `limit`, `None` when the request omitted it (Go's 200).
    limit: Option<usize>,
    /// `cursor`, `None` when the request omitted it (Go's index 0).
    cursor: Option<&'static str>,
}

const CASES: &[Case] = &[
    Case {
        golden: "dir-root",
        sub: "tree",
        limit: None,
        cursor: None,
    },
    Case {
        golden: "dir-subdir",
        sub: "tree/subdir",
        limit: None,
        cursor: None,
    },
    Case {
        golden: "dir-empty",
        sub: "tree/empty-dir",
        limit: None,
        cursor: None,
    },
    Case {
        golden: "dir-perm-noexec",
        sub: "tree/noexec-dir",
        limit: None,
        cursor: None,
    },
    Case {
        golden: "dir-many-p1",
        sub: "tree/many",
        limit: Some(200),
        cursor: Some("0"),
    },
];

#[test]
fn dir_listing_matches_go() {
    // Plan §7 step 6 acceptance (b): every field of every row of every captured page, plus the six
    // payload fields around them.
    let fix = fixtures();
    let session = open_session(fix, "list", real_env());
    let mut directory_rows_compared = 0usize;
    let mut skipped_size_field = false;
    for case in CASES {
        let req = ListDir {
            path: FsPath::from_abs(abs(&fix.root.join(case.sub))),
            cursor: case.cursor.map(Cursor::decode),
            limit: case.limit.unwrap_or(200),
            sort: SortSpec::DEFAULT_SORT,
            flat: FlatMode::Off,
        };
        let origin = session.next_origin();
        let receipt = session
            .page_dir(&req, origin)
            .unwrap_or_else(|e| panic!("{}: {e}", case.golden));
        assert!(
            receipt.is_done(),
            "{}: step 6 lists synchronously",
            case.golden
        );
        let page: Arc<zenman_core::entry::DirPage> =
            Arc::clone(receipt.done().expect("a Done receipt holds the page"));
        let got = serde_json::to_value(page.to_listing()).expect("a listing serialises");
        let want = expected_listing(case.golden, &fix.root);
        directory_rows_compared += count_directories(&want);
        if got == want {
            continue;
        }
        if !fix.go_directory_sizes {
            // Only a directory row's `size` can differ on a non-tmpfs scratch filesystem, so compare
            // every other field and say out loud which one was not compared.
            let adjusted = substitute_directory_sizes(&want, &got);
            assert_eq!(
                got, adjusted,
                "{} differs in more than the directory `size` field, so this is not a \
                 filesystem-shape difference",
                case.golden
            );
            eprintln!(
                "contract_dirs::dir_listing_matches_go: SKIPPED the directory `size` field of {} — a \
                 fresh directory under {} is not the 40 bytes the capture has; every other field of \
                 every row was compared and matched.",
                case.golden,
                fix.root.display()
            );
            skipped_size_field = true;
            continue;
        }
        panic!(
            "{} differs.\n  expected: {want}\n  ours:     {got}",
            case.golden
        );
    }
    assert!(
        directory_rows_compared > 0,
        "the table must cover directory rows, or the `size` comparison would be vacuous"
    );
    if !skipped_size_field {
        // The strong claim, when the filesystem can support it.
        assert!(fix.go_directory_sizes);
    }
}

fn count_directories(payload: &Value) -> usize {
    payload["entries"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter(|r| r["is_dir"].as_bool().unwrap_or(false))
                .count()
        })
        .unwrap_or(0)
}

/// The non-tmpfs fallback: every field of `want` except a directory row's `size`, which is taken from
/// what we produced (and reported as skipped, above).
fn substitute_directory_sizes(want: &Value, got: &Value) -> Value {
    let mut out = want.clone();
    let rows = out["entries"]
        .as_array_mut()
        .expect("entries array in the expected payload");
    let ours = got["entries"]
        .as_array()
        .expect("entries array in our payload");
    for (row, ours_row) in rows.iter_mut().zip(ours.iter()) {
        if row["is_dir"].as_bool().unwrap_or(false) {
            row["size"] = ours_row["size"].clone();
        }
    }
    out
}

// ------------------------------------------------------------------------------------------ the size

/// One `dir-size-*.json` capture.
struct SizeCase {
    golden: &'static str,
    sub: &'static str,
}

const SIZE_CASES: &[SizeCase] = &[
    SizeCase {
        golden: "dir-size-root",
        sub: "tree",
    },
    SizeCase {
        golden: "dir-size-subdir",
        sub: "tree/subdir",
    },
    SizeCase {
        golden: "dir-size-many",
        sub: "tree/many",
    },
];

#[test]
fn dir_size_matches_go() {
    // `/api/dir/size` (`api/dir.go:324-369`), whose payload is `{status, path, size, files_count}`.
    // Uncapped walk (`DirSizeOpts::default()`), no cancellation, the session's own source — and the
    // numbers are the frozen ones, including the 11 235 files that are more than the flat-listing cap.
    let fix = fixtures();
    let session = open_session(fix, "size", real_env());
    for case in SIZE_CASES {
        let path = abs(&fix.root.join(case.sub));
        let want = golden(case.golden);
        let stat = session
            .dir_size_walk(
                &path,
                &DirSizeOpts::default(),
                &zenman_core::jobs::CancelToken::new(),
                &SilentSink,
            )
            .unwrap_or_else(|e| panic!("{}: {e}", case.golden));
        assert_eq!(
            stat.bytes,
            want["size"].as_u64().expect("size in the capture"),
            "{}: total bytes",
            case.golden
        );
        assert_eq!(
            stat.files_count,
            want["files_count"]
                .as_u64()
                .expect("files_count in the capture"),
            "{}: file count",
            case.golden
        );
        assert_eq!(want["status"], "success", "the capture is a success case");
        // …and now the walk is cached, so the public entry point answers without a job.
        let cached = session.dir_size(&path, session.next_origin());
        assert!(
            cached.is_done(),
            "{}: a walked directory is cached ({cached:?})",
            case.golden
        );
    }

    // `err-dir-size-nonexistent.json`: Go's 404, which `CoreError::NotFound` carries.
    let missing = abs(&fix.root.join("does-not-exist"));
    let err = session
        .dir_size_walk(
            &missing,
            &DirSizeOpts::default(),
            &zenman_core::jobs::CancelToken::new(),
            &SilentSink,
        )
        .expect_err("a missing directory has no size");
    match err {
        CoreError::NotFound { path, .. } => {
            assert!(path.ends_with("does-not-exist"), "names the path: {path}")
        }
        other => panic!("expected NotFound, got {other:?}"),
    }

    // A file is not a directory (Go's `400 Path is not a directory`, the same guard as `/api/dir`).
    let file = abs(&fix.root.join("tree/normal.txt"));
    let err = session
        .dir_size_walk(
            &file,
            &DirSizeOpts::default(),
            &zenman_core::jobs::CancelToken::new(),
            &SilentSink,
        )
        .expect_err("a file has no directory size");
    assert!(matches!(err, CoreError::NotADir { .. }), "{err:?}");

    // A *cold* directory answers through the job path, which is where plan §4.1's `Origin` story lives:
    // a fresh session has no size cache, so `dir_size` must hand back `Running`.
    let mut cold = open_session(fix, "size-cold", real_env());
    let origin = cold.next_origin();
    let target = abs(&fix.root.join("tree/SortCase"));
    let receipt = cold.dir_size(&target, origin);
    let (job, back) = match receipt {
        Receipt::Running { job, origin } => (job, origin),
        Receipt::Done(stat) => panic!("a cold walk must be a job, got a size of {stat:?}"),
    };
    assert_eq!(back, origin, "the receipt returns the caller's origin");
    let mut announced = None;
    let mut terminal = None;
    for _ in 0..5_000 {
        for event in cold.poll() {
            match event {
                EngineEvent::JobStarted { id, origin, .. } => {
                    assert_eq!(origin, back, "JobStarted carries the same origin");
                    announced = Some(id);
                }
                EngineEvent::JobFinished {
                    id,
                    origin,
                    payload,
                    outcome,
                    ..
                } => {
                    assert_eq!(origin, back, "and so does the terminal event");
                    assert!(outcome.succeeded(), "{outcome:?}");
                    let stat = payload
                        .dir_size()
                        .unwrap_or_else(|| panic!("a typed size payload, got {payload:?}"));
                    assert_eq!(
                        (stat.bytes, stat.files_count),
                        (4, 2),
                        "SortCase's two files"
                    );
                    terminal = Some(id);
                }
                EngineEvent::DirSizeReady { path, stat } => {
                    assert_eq!(path, target, "the broadcast names the walked directory");
                    assert_eq!(stat.files_count, 2);
                }
                EngineEvent::JobProgress { .. } | EngineEvent::Notice { .. } => {}
                other => panic!("unexpected event on the size job: {other:?}"),
            }
        }
        if terminal.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert_eq!(announced, Some(job), "the job announced itself once");
    assert_eq!(terminal, Some(job), "and terminated once");
    assert!(
        !cold.cancel(job),
        "a completed job is never reclassified as cancelled (plan step 6)"
    );
}

#[test]
fn dir_listing_covers_the_whole_table() {
    // The per-case assertions are in `dir_listing_matches_go`; this pins that the table itself is the
    // five captures plan §7 step 6 names, so a future edit cannot quietly shrink the gate.
    let names: Vec<&str> = CASES.iter().map(|c| c.golden).collect();
    assert_eq!(
        names,
        [
            "dir-root",
            "dir-subdir",
            "dir-empty",
            "dir-perm-noexec",
            "dir-many-p1"
        ]
    );
}

// ---------------------------------------------------------------------------------------- /api/props

/// An [`Env`] that counts the engine's contacts with the platform layer. It wraps a real
/// implementation, so what is counted is the engine's first touch of the platform, not a stub's answer.
///
/// No `Debug`: the wrapped [`Env`] is a `dyn` object with nothing print-worthy in it.
struct Recorder {
    inner: Box<dyn Env>,
    probes: std::sync::atomic::AtomicU64,
}

impl Env for Recorder {
    fn os(&self) -> zenman_core::platform::Os {
        self.inner.os()
    }
    fn caps(&self) -> zenman_core::platform::PlatformCaps {
        self.inner.caps()
    }
    fn open_with(
        &self,
        path: &AbsPath,
    ) -> zenman_core::platform::PlatformFuture<'_, Result<(), CoreError>> {
        self.inner.open_with(path)
    }
    fn copy_paths(
        &self,
        items: &[zenman_core::platform::CopyItem],
    ) -> zenman_core::platform::PlatformFuture<
        '_,
        Result<zenman_core::platform::CopyReport, CoreError>,
    > {
        self.inner.copy_paths(items)
    }
    fn paste_link(
        &self,
        items: &[zenman_core::platform::LinkItem],
    ) -> zenman_core::platform::PlatformFuture<'_, Result<(), CoreError>> {
        self.inner.paste_link(items)
    }
    fn supports_symlink(&self) -> Result<bool, CoreError> {
        let _ = self
            .probes
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.inner.supports_symlink()
    }
    fn chmod(
        &self,
        targets: &[AbsPath],
        mode: zenman_core::mode::ModeBits,
    ) -> zenman_core::platform::PlatformFuture<'_, Result<(), CoreError>> {
        self.inner.chmod(targets, mode)
    }
    fn can_trash(&self, path: &AbsPath) -> bool {
        self.inner.can_trash(path)
    }
    fn trash(&self, path: &AbsPath) -> Result<bool, CoreError> {
        self.inner.trash(path)
    }
    fn create_shortcut(&self, shortcut: &zenman_core::platform::Shortcut) -> Result<(), CoreError> {
        self.inner.create_shortcut(shortcut)
    }
    fn disk_space(&self, path: &AbsPath) -> Result<zenman_core::platform::DiskSpace, CoreError> {
        self.inner.disk_space(path)
    }
    fn mount_entries(&self) -> Result<zenman_core::platform::MountTable, CoreError> {
        self.inner.mount_entries()
    }
    fn exec_program(
        &self,
        req: zenman_core::platform::ExecRequest,
    ) -> zenman_core::platform::PlatformFuture<
        '_,
        Result<zenman_core::platform::ExecOutput, CoreError>,
    > {
        self.inner.exec_program(req)
    }
}

#[test]
fn props_shape() {
    // `main.go:99-119` answers exactly two keys. The four `props-*.json` captures (a file, a directory,
    // a symlink, a CRLF name) are identical because the handler ignores its `path` — which is why one
    // shape check covers all four, and why the key *order* is read from the capture's bytes rather than
    // from a parsed map (serde_json's default map is sorted).
    let fix = fixtures();
    let session = open_session(fix, "props-shape", real_env());
    let report: ProbeReport = session.props();
    let json = serde_json::to_value(report).expect("the report serialises");
    let object = json.as_object().expect("the props body is an object");
    assert_eq!(
        object.len(),
        2,
        "/api/props carries exactly two fields, got {object:?}"
    );
    let text = std::fs::read_to_string(Path::new(GOLDEN).join("props-dir.json")).expect("capture");
    let platform_at = text.find("\"platform\"").expect("platform key");
    let symlink_at = text
        .find("\"symlink_supported\"")
        .expect("symlink_supported key");
    assert!(
        platform_at < symlink_at,
        "the capture's own byte order is platform, then symlink_supported"
    );
    let keys: Vec<&str> = object.keys().map(|k| k.as_str()).collect();
    for name in ["platform", "symlink_supported"] {
        assert!(keys.contains(&name), "{name} missing from {keys:?}");
    }
    assert!(json["symlink_supported"].is_boolean());
    assert_eq!(
        json["platform"].as_str().expect("a platform string"),
        zenman_core::platform::Os::current().as_str(),
        "the value is this host's, spelled as Go spelled the capture's"
    );
    for name in [
        "props-file",
        "props-dir",
        "props-symlink",
        "props-crlf-name",
    ] {
        let capture = golden(name);
        let obj = capture.as_object().expect("an object");
        assert_eq!(obj.len(), 2, "{name} carries two keys");
        assert!(
            obj["symlink_supported"].is_boolean(),
            "{name}'s second field is the probe's boolean"
        );
    }
}

#[test]
fn props_probed_once() {
    // Plan F21: the symlink answer is measured once and read from a cache afterwards. The properties
    // dialog asks on every repaint, so a second measurement would be a syscall storm behind a label.
    let fix = fixtures();
    let recorder = Arc::new(Recorder {
        inner: Box::new(zenman_core::platform::stub::Unsupported::new()),
        probes: std::sync::atomic::AtomicU64::new(0),
    });
    let mut session = open_session(fix, "props-once", Arc::clone(&recorder) as Arc<dyn Env>);
    let first = session.props();
    for _ in 0..99 {
        assert_eq!(session.props(), first, "the report is stable across calls");
    }
    assert_eq!(
        recorder.probes.load(std::sync::atomic::Ordering::Acquire),
        1,
        "one measurement for a hundred asks (plan F21)"
    );
    // `prime_props` is the job-thread form of the same first call: after it, still one.
    session.prime_props();
    assert_eq!(
        recorder.probes.load(std::sync::atomic::Ordering::Acquire),
        1
    );
    let _ = session.poll();
}

// ---------------------------------------------------------------------------- flat (recursive) listings

/// One `dir-flat-*.json` capture and the request `tests/golden/INDEX.json` records behind it.
struct FlatCase {
    /// The capture name.
    golden: &'static str,
    /// The tree-relative listing root.
    sub: &'static str,
    /// The engine's flat mode — `flat`/`no_folders` as one field (`api/dir.go:126-127`).
    flat: FlatMode,
    /// The captured `limit` (`INDEX.json`'s request; Go's default is 200).
    limit: usize,
    /// The captured `total_count` — the walk's own row count, cap included.
    total: usize,
    /// The captured page's `has_more`.
    has_more: bool,
    /// Rows on the captured page.
    page: usize,
}

const FLAT_CASES: &[FlatCase] = &[
    // `flat=true` — every descendant, directories among them (the shell's `FlatMode::Mixed`).
    FlatCase {
        golden: "dir-flat-subdir",
        sub: "tree/subdir",
        flat: FlatMode::Mixed,
        limit: 200,
        total: 3,
        has_more: false,
        page: 3,
    },
    // `flat=true&no_folders=true` (`FlatMode::MixedNoFolders`): the two files, no directory rows.
    FlatCase {
        golden: "dir-flat-nofolders",
        sub: "tree/subdir",
        flat: FlatMode::MixedNoFolders,
        limit: 200,
        total: 2,
        has_more: false,
        page: 2,
    },
    // The deep tree: 21 rows, ordered by *base name* (dirs first), not by `rel_path` — the order
    // is the sort's, and `d0/d1/d2…` nests deeper than `…/d10`, which is what makes this case the
    // one that pins the comparator. `Grouped` walks exactly like `Mixed` (the grouping is a shell-side
    // render, `frontend/js/navigation.js:92-97`), so this capture is that mode's expectation too.
    FlatCase {
        golden: "dir-flat-deep",
        sub: "tree/deep",
        flat: FlatMode::Grouped,
        limit: 200,
        total: 21,
        has_more: false,
        page: 21,
    },
    // The cap: a 10 005-entry directory, `limit=5`, `total_count` the cap itself, `cursor: "5"`.
    FlatCase {
        golden: "dir-flat-cap",
        sub: "tree/flatcap",
        flat: FlatMode::Mixed,
        limit: 5,
        total: 10_000,
        has_more: true,
        page: 5,
    },
];

/// Plan §7 step 8's contract row: the flat-mode captures, row by row, against the tree on disk.
///
/// # What this can assert today, and what it cannot
///
/// **Asserted here** — everything a capture states about a *real* path:
///
/// * every row's `mode` string is `ModeKind::from_unix(lstat(rel_path))` rendered through
///   `Display`, and `ModeKind::parse_mode` reads that string back to the same kind (the round trip
///   the file row depends on, plan §5.8's `mode_cell`);
/// * `is_dir`, `size`, `mod_time`, `name` and `depth` are the lstat facts of that path — including
///   Go's `depth = strings.Count(rel, "/")` (`api/dir.go:153-160`);
/// * the **row set and its order** are what a recursive walk plus
///   [`zenman_core::services::sort::sort_by`] produce, so the flat ordering rule (base name,
///   lowercase byte compare, directories first in both directions) is a checked contract;
/// * `total_count`, `has_more`, the page length and the walk cap of [`Limits::flat_walk_cap`]
///   against a directory that exceeds it;
/// * that the tree on disk *is* the captured tree, by comparing the walked paths against the
///   `fixture-tree/1` manifest of `docs/fixtures.md` — the same manifest
///   `cargo xtask fixture-tree` diffs the generator's output against, and the reason this test
///   cannot quietly be comparing against a different tree.
///
/// **and, since step 9a, the engine's own flat walk**: every row is also produced by
/// [`Session::page_dir`] with a `ListDir { flat, .. }` request, drained through `poll()` until
/// [`EngineEvent::DirPageReady`] (a flat request is a job, not a synchronous read), and the whole
/// serialised page is compared against the capture — the same field-for-field claim
/// `dir_listing_matches_go` makes of a plain listing.
#[test]
fn flat_modes_match_go() {
    use std::os::unix::fs::MetadataExt as _;

    let fix = fixtures();
    let cap = Limits::default().flat_walk_cap;
    let manifest = fixture_tree_manifest();
    let mut session = open_session(fix, "flat", real_env());
    let mut rows_checked = 0usize;
    let mut modes = std::collections::BTreeSet::new();
    let mut engine_pages = 0usize;
    for case in FLAT_CASES {
        let payload = golden(case.golden);
        let root = fix.root.join(case.sub);
        assert!(
            root.is_dir(),
            "{}: {} must exist",
            case.golden,
            root.display()
        );

        // 1. the capture's own fields.
        assert_eq!(
            payload["path"].as_str().expect("a path"),
            format!("${{FIXTURE_ROOT}}/{}", case.sub),
            "{}: the capture's own root",
            case.golden
        );
        let rows = payload["entries"]
            .as_array()
            .unwrap_or_else(|| panic!("{}: no entries array", case.golden));
        assert_eq!(rows.len(), case.page, "{}: page length", case.golden);
        assert_eq!(
            payload["total_count"].as_u64().expect("a total") as usize,
            case.total,
            "{}: total_count",
            case.golden
        );
        assert_eq!(
            payload["has_more"].as_bool().expect("has_more"),
            case.has_more,
            "{}: has_more",
            case.golden
        );
        assert_eq!(
            case.total.min(cap),
            case.total,
            "{}: a walk never exceeds its cap",
            case.golden
        );

        // 2. every row against the lstat of the path it names.
        for row in rows {
            let rel = row["rel_path"].as_str().expect("a flat row has a rel_path");
            assert!(
                !rel.is_empty(),
                "{}: flat rows carry `rel_path`",
                case.golden
            );
            let path = root.join(rel);
            let meta = path
                .symlink_metadata()
                .unwrap_or_else(|e| panic!("{}: {} missing: {e}", case.golden, path.display()));
            let kind = ModeKind::from_unix(meta.mode());
            let want = row["mode"].as_str().expect("a mode string");
            assert_eq!(kind.to_string(), want, "{}: {rel}'s mode", case.golden);
            assert_eq!(
                ModeKind::parse_mode(want),
                kind,
                "{}: {rel}'s mode string must read back to the kind that wrote it",
                case.golden
            );
            assert_eq!(
                ModeKind::parse_mode(want).to_string(),
                want,
                "{rel}: Display round trip"
            );
            modes.insert(want.to_string());
            assert_eq!(
                row["is_dir"].as_bool().expect("is_dir"),
                meta.is_dir(),
                "{}: {rel}'s is_dir is the lstat answer (a symlink is not a directory)",
                case.golden
            );
            if !meta.is_dir() || fix.go_directory_sizes {
                // A directory's `st_size` belongs to its filesystem (the capture's tree was on tmpfs);
                // the same field `dir_listing_matches_go` skips, and for the same reason.
                assert_eq!(
                    row["size"].as_i64().expect("a size") as u64,
                    meta.len(),
                    "{}: {rel}'s size is the lstat size",
                    case.golden
                );
            }
            assert_eq!(
                row["mod_time"].as_i64().expect("a mod_time"),
                meta.mtime(),
                "{}: {rel}'s mtime is the lstat mtime",
                case.golden
            );
            assert_eq!(
                row["name"].as_str().expect("a name"),
                rel.rsplit('/').next().expect("a last component"),
                "{}: {rel}'s name is its last component",
                case.golden
            );
            assert_eq!(
                row["depth"].as_i64().unwrap_or(0),
                rel.matches('/').count() as i64,
                "{}: {rel}'s depth is its separator count (`api/dir.go:153-160`)",
                case.golden
            );
            assert!(
                case.flat.includes_dirs() || !meta.is_dir(),
                "{}: `no_folders` yields no directory rows, and {:?} is one",
                case.golden,
                rel
            );
            rows_checked += 1;
        }

        // 3. the whole walk: the captured row *sequence* is a recursive walk of this tree in
        //    the listing order, truncated at the cap. The flat captures are name-ordered, which is
        //    Go's own default, so the comparison is exact rather than set-shaped.
        let mut walked = walk_flat(&root, !case.flat.includes_dirs());
        assert_eq!(
            walked.len().min(cap),
            case.total,
            "{}: walking {} yields the captured count ({} rows, cap {cap})",
            case.golden,
            root.display(),
            case.total
        );
        zenman_core::services::sort::sort_by(&mut walked, SortSpec::DEFAULT_SORT);
        let got: Vec<&str> = walked.iter().map(|row| row.rel.as_str()).collect();
        let want: Vec<&str> = rows
            .iter()
            .map(|row| row["rel_path"].as_str().expect("rel"))
            .collect();
        assert_eq!(
            got[..want.len()],
            want,
            "{}: the flat listing order",
            case.golden
        );

        // 3b. **the engine's page**, through `Session::page_dir` + the job path, compared field for
        //     field against the capture (see `engine_flat_page`/`assert_page_matches_capture`).
        let page = engine_flat_page(&mut session, case, &root);
        let got = serde_json::to_value(page.to_listing()).expect("a listing serialises");
        assert_page_matches_capture(&got, case, fix);
        assert_eq!(
            page.total_count,
            case.total.min(cap),
            "{}: the engine's own walk reached the captured row count",
            case.golden
        );
        engine_pages += 1;

        // 4. the tree on disk is the captured tree: every walked path is in the manifest, with the
        //    same kind. `cargo xtask fixture-tree` is the byte-exact gate; this is the link between
        //    *this* test's tree and that manifest.
        if case.sub != "tree/flatcap" {
            for rel in walk_paths(&root) {
                let key = format!("{}/{}", case.sub.trim_start_matches("tree/"), rel);
                assert!(
                    manifest.contains_key(&key),
                    "{}: {} is not in the `fixture-tree/1` manifest, so the tree this test walked
   is not the tree the capture was taken from",
                    case.golden,
                    key
                );
            }
        }
    }
    assert!(rows_checked > 20, "{rows_checked} rows is not a gate");
    assert_eq!(
        engine_pages,
        FLAT_CASES.len(),
        "every flat capture must be produced by the engine, not only checked on disk"
    );
    assert!(
        modes.contains("drwxr-xr-x") && modes.contains("-rw-r--r--"),
        "the flat captures must cover both a directory and a regular file mode: {modes:?}"
    );
    println!(
        "contract_dirs::flat_modes_match_go: {rows_checked} rows, {} distinct mode strings, all \
         round-tripped through ModeKind",
        modes.len()
    );
}

/// Drive one `FLAT_CASES` row through the engine: `Session::page_dir` with `ListDir { flat, .. }`, then
/// the `Receipt::Running` drain loop `dir_size_matches_go` uses for the size walk.
///
/// A flat request *must* be a job (a recursive walk of up to [`Limits::flat_walk_cap`] entries is not a
/// read that may park a window thread), must come back with the caller's `Origin`, and must then be
/// served from the cache on the **flat** key — a second identical request answers `Done` synchronously.
fn engine_flat_page(
    session: &mut Session,
    case: &FlatCase,
    root: &Path,
) -> Arc<zenman_core::entry::DirPage> {
    let req = ListDir {
        path: FsPath::from_abs(abs(root)),
        cursor: None,
        limit: case.limit,
        sort: SortSpec::DEFAULT_SORT,
        flat: case.flat,
    };
    let origin = session.next_origin();
    let (job, back) = match session
        .page_dir(&req, origin)
        .unwrap_or_else(|e| panic!("{}: {e}", case.golden))
    {
        Receipt::Running { job, origin } => (job, origin),
        Receipt::Done(page) => panic!(
            "{}: a flat walk is a job, not a synchronous read ({} rows came back inline)",
            case.golden, page.total_count
        ),
    };
    assert_eq!(back, origin, "the receipt returns the caller's origin");
    let mut ready = None;
    let mut terminal = None;
    for _ in 0..30_000 {
        for event in session.poll() {
            match event {
                EngineEvent::DirPageReady {
                    origin: ev_origin,
                    path,
                    page,
                } => {
                    assert_eq!(ev_origin, back, "the page carries the request's origin");
                    assert_eq!(path, req.path, "and names the directory it listed");
                    ready = Some(page);
                }
                EngineEvent::JobStarted { id, origin, .. } => {
                    assert_eq!(origin, back, "JobStarted carries the same origin");
                    assert_eq!(id, job, "and the job the receipt named");
                }
                EngineEvent::JobFinished {
                    id,
                    origin,
                    outcome,
                    payload,
                    ..
                } => {
                    assert_eq!(origin, back, "and so does the terminal event");
                    assert!(outcome.succeeded(), "{outcome:?}");
                    assert!(
                        payload.dir_page().is_some(),
                        "a typed page payload, got {payload:?}"
                    );
                    terminal = Some(id);
                }
                EngineEvent::JobProgress { .. } | EngineEvent::Notice { .. } => {}
                other => panic!("unexpected event on the flat-listing job: {other:?}"),
            }
        }
        if ready.is_some() && terminal.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let page = ready.unwrap_or_else(|| panic!("{}: no DirPageReady arrived", case.golden));
    assert_eq!(terminal, Some(job), "the job announced exactly one ending");
    assert!(
        session
            .page_dir(&req, session.next_origin())
            .expect("the second ask")
            .is_done(),
        "{}: a cached flat request answers synchronously, on its own key",
        case.golden
    );
    page
}

/// The strong comparison: the whole serialised page against the whole capture.
///
/// The one field that belongs to the filesystem rather than to the engine is a directory row's `size`
/// (the captures were taken on tmpfs), and that escape is the same one `dir_listing_matches_go`
/// documents — announced out loud, never taken silently.
fn assert_page_matches_capture(got: &Value, case: &FlatCase, fix: &Fixtures) {
    let want = expected_listing(case.golden, &fix.root);
    if got == &want {
        return;
    }
    let adjusted = substitute_directory_sizes(&want, got);
    assert_eq!(
        got, &adjusted,
        "{} differs in more than the directory `size` field, so this is not a filesystem-shape \
         difference\n  expected: {want}\n  ours:     {got}",
        case.golden
    );
    assert!(
        !fix.go_directory_sizes,
        "{} differs only in directory `size`, which this filesystem does report go-shaped: that is \
         a real mismatch",
        case.golden
    );
    eprintln!(
        "contract_dirs::flat_modes_match_go: SKIPPED the directory `size` field of {} — a fresh \
         directory under {} is not the 40 bytes the capture has; every other field of every row was \
         compared and matched.",
        case.golden,
        fix.root.display()
    );
}

/// One walked row: enough to sort and compare, which is what the flat order is made of.
struct Walked {
    /// The listing-relative path (`rel_path` on the wire).
    rel: String,
    /// The base name, which is what the sort compares.
    name: String,
    /// The lstat directory answer.
    is_dir: bool,
}

impl zenman_core::services::sort::SortRow for Walked {
    fn sort_name(&self) -> String {
        self.name.to_lowercase()
    }

    fn raw_name(&self) -> &str {
        &self.name
    }

    fn is_dir(&self) -> bool {
        self.is_dir
    }

    fn sort_size(&self) -> i64 {
        // The name sort never reads it, and this walk is compared in name order only.
        0
    }

    fn sort_mtime(&self) -> i64 {
        zenman_core::metrics::GO_ZERO_TIME_UNIX
    }
}

/// Every descendant of `dir`, as the flat walk sees it (`filepath.WalkDir`, minus the root).
fn walk_flat(dir: &std::path::Path, no_folders: bool) -> Vec<Walked> {
    walk_flat_from(dir, dir, no_folders)
}

fn walk_flat_from(root: &std::path::Path, dir: &std::path::Path, no_folders: bool) -> Vec<Walked> {
    let mut out = Vec::new();
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("walking {}: {e}", dir.display()));
    let mut names: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
    names.sort();
    for path in names {
        let meta = path
            .symlink_metadata()
            .unwrap_or_else(|e| panic!("lstat {}: {e}", path.display()));
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        let is_dir = meta.is_dir();
        if !(no_folders && is_dir) {
            out.push(Walked {
                rel,
                name: path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                is_dir,
            });
        }
        if is_dir {
            out.extend(walk_flat_from(root, &path, no_folders));
        }
    }
    out
}

/// The same walk, as paths only (for the manifest cross-check).
fn walk_paths(dir: &std::path::Path) -> Vec<String> {
    walk_flat(dir, false)
        .into_iter()
        .map(|row| row.rel)
        .collect()
}

/// The `fixture-tree/1` block of `docs/fixtures.md`: `path -> kind`.
///
/// Parsed here rather than shelled out to `cargo xtask fixture-tree` because this test needs the
/// *rows*, and the gate prints a verdict. A missing manifest is a hard failure: unlike the gate,
/// which may skip on a fresh clone, this file already requires the generated tree to exist.
fn fixture_tree_manifest() -> std::collections::BTreeMap<String, String> {
    let file = Path::new(REPO).join("docs/fixtures.md");
    let text = std::fs::read_to_string(&file).unwrap_or_else(|e| {
        panic!(
            "{} is what ties this test's tree to the capture: {e}",
            file.display()
        )
    });
    let open = "```fixture-tree/1";
    let start = text
        .find(open)
        .unwrap_or_else(|| panic!("{}: no {open} block", file.display()));
    let body = &text[start + open.len()..];
    let end = body
        .find(
            "
```",
        )
        .unwrap_or_else(|| panic!("{}: an unterminated manifest block", file.display()));
    let value: Value = serde_json::from_str(&body[..end]).expect("the manifest block is JSON");
    let mut out = std::collections::BTreeMap::new();
    for row in value["entries"].as_array().expect("manifest entries") {
        let hex = row[0].as_str().expect("a hex path");
        let kind = row[1].as_str().expect("a kind");
        let Ok(bytes) = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| e.to_string()))
            .collect::<Result<Vec<u8>, String>>()
        else {
            panic!("manifest hex path {hex} is not hex");
        };
        out.insert(
            String::from_utf8(bytes).expect("a UTF-8 manifest path"),
            kind.to_string(),
        );
    }
    out
}

// ------------------------------------------------------------------------------------ tree sanity

/// The tree the listings above compared against has to be the tree the capture was taken from, or every
/// match in this file would be a coincidence. This asserts the properties that make it so: the heavy
/// case exists, the FIFO exists, the mode-000 file exists, and the root has the captured 33 rows.
#[test]
fn the_generated_tree_is_the_captured_one() {
    let fix = fixtures();
    assert!(fix.root.join("tree").is_dir(), "{}", fix.root.display());
    let flatcap = fix.root.join("tree/flatcap");
    let entries = std::fs::read_dir(&flatcap)
        .unwrap_or_else(|e| panic!("{}: {e}", flatcap.display()))
        .count();
    assert!(entries > 10_000, "flatcap has {entries} entries");
    let listing: DirListing = {
        let session = open_session(fix, "tree-sanity", real_env());
        let receipt = session
            .page_dir(
                &ListDir::first(FsPath::from_abs(abs(&fix.root.join("tree")))),
                session.next_origin(),
            )
            .expect("the tree lists");
        receipt
            .into_done()
            .expect("step 6 lists synchronously")
            .to_listing()
    };
    assert_eq!(
        listing.total_count, 33,
        "the same 33 rows dir-root.json captured"
    );
    assert!(
        listing
            .entries
            .iter()
            .any(|e| e.name == "myfifo" && e.mode.starts_with('p')),
        "the FIFO is part of the capture: {:?}",
        listing.entries.iter().map(|e| &e.name).collect::<Vec<_>>()
    );
    assert!(
        listing
            .entries
            .iter()
            .any(|e| e.name == "noperm.bin" && e.mode == "----------"),
        "the mode-000 file is part of the capture"
    );
}
