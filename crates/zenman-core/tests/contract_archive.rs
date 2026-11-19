//! Plan §5.10's `contract_archive.rs`: the six named criteria of §5.5/§5.10, against the **captured
//! fixture archives** — the same `plain.zip` / `nested.zip` / `zipslip.zip` / `symlink.zip` that
//! `scripts/make-fixtures.py` writes and that the Go captures in `tests/golden/archive-*.json` were taken
//! from. Nothing in this file builds a container with the crate under test; the only synthesised
//! containers are the two the fixture generator cannot produce (an encrypted zip and a truncated one), and
//! both are byte-level edits of a captured file, not writer output.
//!
//! Two facts about those fixtures decide what "depth 2" means here (`scripts/make-fixtures.py:247-280`):
//!
//! * `nested.zip` holds `outer.txt`, `inner.zip` (**a real zip**) and `inner.7z` — and the inner 7z is a
//!   *stub*: the generator records that no `7z`/`7za`/`p7zip` exists on the capture host and that it must
//!   not download one, so the member is a 7z signature header and nothing else (`docs/contracts.md`,
//!   "Fixtures we cannot generate"). Depth 2 is therefore proven through `inner.zip`, and the 7z member is
//!   the *corrupt container* case (test 7).
//! * `zipslip.zip` holds `../escape.txt`, `/tmp/absolute-escape.txt` and `safe.txt` — so the zip-slip test
//!   proves refusal of both the traversal and the absolute entry, and that `safe.txt` never appears either.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use zenman_core::archive::extract::{ExtractReport, ExtractReq};
use zenman_core::archive::vfs::clean_internal_path;

use zenman_core::events::EngineEvent;
use zenman_core::index::intent::{IndexIntent, IndexOp, RecordingIntent};
use zenman_core::jobs::JobId;
use zenman_core::limits::Limits;
use zenman_core::ops::ConflictPolicy;
use zenman_core::path::{AbsPath, FsPath, VfsKind};
use zenman_core::platform::default_env;
use zenman_core::session::{Origin, Receipt, Session, SessionPaths};
use zenman_core::store::config::ZenConfig;

const REPO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

/// The archives directory, built once per process by the repository's own generator.
fn archives() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let root =
            std::env::temp_dir().join(format!("zenman-contract-archive-{}", std::process::id()));
        wipe(&root);
        fs::create_dir_all(&root).expect("scratch root");
        let out = root.join("archives");
        let generator = Path::new(REPO).join("scripts/make-fixtures.py");
        assert!(
            generator.is_file(),
            "{} is missing: the fixture archives are its output",
            generator.display()
        );
        // `spawn` + `wait_with_output`, not `Command::output` (`clippy.toml`, plan F10) — the same form
        // `contract_dirs.rs` uses for the same generator.
        let output = Command::new("python3")
            .current_dir(REPO)
            .arg(generator.display().to_string())
            .arg("--root")
            .arg(root.join("tree").display().to_string())
            .arg("--archives")
            .arg(out.display().to_string())
            .spawn()
            .and_then(|child| child.wait_with_output())
            .expect("run the fixture generator");
        assert!(
            output.status.success(),
            "make-fixtures.py failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(out.join("plain.zip").is_file(), "plain.zip was not built");
        out
    })
    .clone()
}

/// A session whose scratch tree lives beside the archives.
fn session() -> Session {
    let dir = std::env::temp_dir().join(format!(
        "zenman-contract-archive-session-{}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("scratch dir");
    let (session, notes) = Session::open(
        SessionPaths::new(
            dir.join("config.json"),
            dir.join("action.log"),
            None,
            dir.join("probe"),
        ),
        ZenConfig::default(),
        default_env(),
        Limits::default(),
    )
    .expect("a session over a clean scratch directory opens");
    assert!(notes.is_empty(), "nothing here is damaged: {notes:?}");
    session
}

/// The `FsPath` of an archive's store root — the path shape 13b's pane will build when a row is entered
/// (`FsPath::enter`, not a hand-written `VfsKind`).
fn store_of(session_dir: &Path, name: &str) -> FsPath {
    let host = archive_path(name);
    let abs = AbsPath::from_path(host.clone()).expect("absolute host path");
    let fs = FsPath::from_abs(abs.clone());
    assert!(fs.is_archive_file(), "{name} claims an archive format");
    let entered = fs
        .as_store()
        .expect("activating an archive row opens a store");
    assert_eq!(entered.store_depth(), 1, "a host is one store deep");
    assert_eq!(
        entered.host_abs().as_ref(),
        Some(&abs),
        "the store's host is the file on disk"
    );
    let _ = session_dir;
    entered
}

fn archive_path(name: &str) -> PathBuf {
    archives().join(name)
}

/// A copy of a captured archive, so a test can damage it without touching the golden fixtures.
fn copy_archive(name: &str, tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zenman-contract-archive-copy-{}-{tag}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("copy dir");
    let to = dir.join(name);
    fs::copy(archive_path(name), &to).expect("copy the captured archive");
    to
}

/// Case 1 — `zip_browse` — the listing of `plain.zip`, field by field against the Go capture.
#[test]
fn zip_browse() {
    let s = session();
    let store = store_of(Path::new("/"), "plain.zip");
    let rows = s.archive_list(&store).expect("the host archive lists");
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["docs", "top.txt"],
        "Go's row order is name order"
    );

    // Field-by-field against `tests/golden/archive-list-plain.json`.
    let golden: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(Path::new(REPO).join("tests/golden/archive-list-plain.json"))
            .expect("golden"),
    )
    .expect("golden parses");
    let want = golden["entries"].as_array().expect("entries array");
    assert_eq!(want.len(), rows.len(), "same row count as Go");
    for (g, row) in want.iter().zip(&rows) {
        assert_eq!(g["name"].as_str().expect("name"), row.name);
        assert_eq!(g["is_dir"].as_bool().expect("is_dir"), row.is_dir);
        assert_eq!(g["size"].as_i64().expect("size"), row.size);
        assert_eq!(
            g["mode"].as_str().expect("mode"),
            row.mode,
            "the read-only cue"
        );
        assert_eq!(g["rel_path"].as_str().expect("rel_path"), row.rel_path);
        assert_eq!(
            g["mod_time"].as_i64().expect("mod_time"),
            row.mod_time.map(|t| t.timestamp()).unwrap_or_default(),
            "the fixture mtime is 2026-01-01T00:00:00Z"
        );
    }

    // A subdirectory of the archive lists the same way (`archive-list-plain-sub.json`).
    let docs = store.join("docs").expect("join inside the store");
    let sub = s.archive_list(&docs).expect("the subdirectory lists");
    let sub_names: Vec<&str> = sub.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(sub_names, vec!["nested", "readme.md"]);
    assert_eq!(
        sub[0].rel_path, "docs/nested",
        "rel_path is the internal path"
    );

    // Properties, which is what 13b's Alt+Enter dialog consumes.
    let props = s.archive_props(&store).expect("props");
    assert_eq!(props.files, 3);
    assert_eq!(props.dirs, 2, "`docs` and the synthesised `docs/nested`");
    assert_eq!(
        props.total_bytes,
        15 + 9 + 5,
        "top.txt + readme.md + deep.txt"
    );
    assert_eq!(props.depth, 1);
}

/// Case 2 — `nested_archive` — §7 row 13's criterion: `a.zip` containing another archive browses **to depth 2**,
/// and nothing browses deeper.
#[test]
fn nested_archive() {
    let s = session();
    let host = store_of(Path::new("/"), "nested.zip");
    let outer: Vec<String> = s
        .archive_list(&host)
        .expect("outer lists")
        .iter()
        .map(|r| r.name.clone())
        .collect();
    assert_eq!(outer, vec!["inner.7z", "inner.zip", "outer.txt"]);

    // Depth 2: `inner.zip` is entered as a store of its own, addressed by the *same* `FsPath` type.
    let inner = host
        .enter("inner.zip")
        .expect("a nested archive opens a second store");
    assert_eq!(inner.store_depth(), 2);
    assert!(matches!(
        &inner.store,
        VfsKind::Archive { nested: Some(n), .. } if n == "inner.zip"
    ));
    let rows = s.archive_list(&inner).expect("the nested archive lists");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "inner-file.txt");
    assert_eq!(rows[0].rel_path, "inner-file.txt");
    let body = s
        .archive_read(&inner.join("inner-file.txt").expect("member"))
        .expect("read the member");
    assert_eq!(
        body, b"inside inner.zip\n",
        "the nested member's bytes are the real ones"
    );

    // The depth cap: a third archive component is a *member*, not a store — which is what Go's
    // two-pass `parseVfsPath` expressed, and what `VfsKind::Archive` can hold.
    let deep = inner
        .join("deeper.zip")
        .expect("a third level is a plain member path");
    assert_eq!(deep.store_depth(), 2, "never three stores");
    assert_eq!(deep.relative, "deeper.zip");
    let err = s
        .archive_list(&deep)
        .expect_err("a member is not a directory");
    // Listing a non-directory member is a missing directory, i.e. a typed error and not a panic.
    assert_eq!(err.kind(), "not_found");
    assert_eq!(
        zenman_core::archive::MAX_NEST_DEPTH,
        2,
        "the documented cap and the structural cap agree"
    );
}

/// Case 3 — `stream_file` — one member streams out, byte-identical to the captured stream body, and the cap
/// turns an over-long read into a typed error rather than an allocation.
#[test]
fn stream_file() {
    let s = session();
    let store = store_of(Path::new("/"), "plain.zip");
    let member = store.join("top.txt").expect("member path");
    let golden = fs::read(Path::new(REPO).join("tests/golden/archive-stream-plain.txt"))
        .expect("the captured stream body");
    let mut out: Vec<u8> = Vec::new();
    let n = s
        .archive_stream(&member, &mut out)
        .expect("streaming works");
    assert_eq!(out, golden, "the same bytes Go's /api/archive/stream sent");
    assert_eq!(n as usize, golden.len());

    // The capped read of the same member agrees, and a member over the cap is refused.
    assert_eq!(s.archive_read(&member).expect("read"), golden);
    let tight = Session::open(
        SessionPaths::new(
            std::env::temp_dir().join("zenman-tight-config.json"),
            std::env::temp_dir().join("zenman-tight-log"),
            None,
            std::env::temp_dir().join("zenman-tight-probe"),
        ),
        ZenConfig::default(),
        default_env(),
        Limits {
            nested_archive_max_bytes: 4,
            ..Limits::default()
        },
    )
    .expect("a capped session opens")
    .0;
    let err = tight
        .archive_read(&member)
        .expect_err("4 bytes cannot hold 15");
    assert_eq!(
        err.kind(),
        "capacity_exceeded",
        "plan F8: a cap is an error value"
    );
}

/// Case 4 — `extract_zip_slip_refused` — `zipslip.zip` extracts to **nothing**.
#[test]
fn extract_zip_slip_refused() {
    let s = session();
    let store = store_of(Path::new("/"), "zipslip.zip");
    // The listing shows the hostile members: a browse must not hide them (Go's capture agrees,
    // `tests/golden/archive-list-zipslip.json`).
    let names: Vec<String> = s
        .archive_list(&store)
        .expect("a slip archive still lists")
        .iter()
        .map(|r| r.name.clone())
        .collect();
    assert!(
        names.contains(&"..".to_string()),
        "the `..` node is visible: {names:?}"
    );

    let dest = scratch("zipslip-dest");
    wipe(&dest);
    fs::create_dir_all(&dest).expect("dest");
    let req = ExtractReq::all(
        AbsPath::from_path(archive_path("zipslip.zip")).expect("host"),
        None,
        AbsPath::from_path(dest.join("out")).expect("dest"),
    );
    let err = match s.archive_extract(&req, Origin(1)) {
        Ok(_) => panic!("the slip archive must not start a job"),
        Err(e) => e,
    };
    assert_eq!(err.kind(), "invalid_path", "{err}");
    assert!(
        !dest.join("out").exists(),
        "the plan pass writes nothing, so a refusal leaves no partial tree"
    );
    assert!(
        !Path::new(archives().parent().expect("root"))
            .join("escape.txt")
            .exists(),
        "nothing escaped the destination either"
    );
    assert!(
        !Path::new("/tmp/absolute-escape.txt").exists(),
        "the absolute member name never reached the filesystem"
    );
    // The safe member did not appear either: extraction is all-or-nothing on a hostile container.
    assert!(!dest.join("out").join("safe.txt").exists());
}

/// Case 5 — `encrypted_archive_error` — an encrypted container is a typed error, never a panic and never an
/// empty listing.
///
/// The fixture generator cannot write an encrypted zip (python's `zipfile` has no encryption support), so
/// the file is a captured archive with the general-purpose *encrypted* bit set in every header — a
/// byte-level edit, not a synthesised container.
#[test]
fn encrypted_archive_error() {
    let path = copy_archive("plain.zip", "encrypted");
    let mut bytes = fs::read(&path).expect("read the copy");
    let mut flipped = 0usize;
    for i in 0..bytes.len().saturating_sub(3) {
        if bytes[i..].starts_with(b"PK\x03\x04") && i + 8 <= bytes.len() {
            bytes[i + 6] |= 0x01; // local header: general purpose flags
            flipped += 1;
        } else if bytes[i..].starts_with(b"PK\x01\x02") && i + 10 <= bytes.len() {
            bytes[i + 8] |= 0x01; // central directory header: same flags field
            flipped += 1;
        }
    }
    assert!(flipped >= 8, "every header got the bit ({flipped} records)");
    fs::write(&path, &bytes).expect("write the encrypted-flag archive");

    let s = session();
    let abs = AbsPath::from_path(&path).expect("abs");
    let store = FsPath::from_abs(abs.clone()).as_store().expect("store");
    let props = s.archive_props(&store).expect("the manifest still reads");
    assert_eq!(props.encrypted, 4, "all four members report the flag");
    let err = s
        .archive_read(&store.join("top.txt").expect("member"))
        .expect_err("reading an encrypted member must fail");
    assert_eq!(err.kind(), "archive_encrypted", "{err}");
    assert_eq!(err.status_code(), 422, "Go's own status for this case");
}

/// Case 6 — `modified_archive_conflict` — plan F7: the host changed under the cached manifest, so the engine
/// refuses to stream the wrong bytes (Go answered HTTP 409; the pane reloads and shows a notice).
#[test]
fn modified_archive_conflict() {
    let path = copy_archive("plain.zip", "modified");
    let s = session();
    let abs = AbsPath::from_path(&path).expect("abs");
    let store = FsPath::from_abs(abs.clone()).as_store().expect("store");
    let first = s.archive_list(&store).expect("first listing");
    assert_eq!(first.len(), 2);

    // A different mtime is the whole test (Go compared mtimes, not contents).
    fs::File::options()
        .write(true)
        .open(&path)
        .expect("open for touching")
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000))
        .expect("set a new mtime");
    let err = s
        .archive_list(&store)
        .expect_err("the cached manifest is now stale");
    assert_eq!(err.kind(), "archive_modified");
    assert_eq!(err.status_code(), 409, "Go's 409, typed");

    // ...and the recovery is exactly what the pane does on the error: re-list, which re-verifies.
    let after = s
        .archive_list(&store)
        .expect("the second listing rebuilds the manifest");
    assert_eq!(after.len(), 2, "the same archive, freshly parsed");
}

/// 7. A corrupt container (the captured `inner.7z` stub, and a truncated zip) is a typed error.
#[test]
fn corrupt_container_is_a_typed_error() {
    let s = session();
    // `nested.zip`'s `inner.7z` is a 7z *signature stub* (see the module docs): the header parses as 7z,
    // the archive it describes does not exist.
    let host = store_of(Path::new("/"), "nested.zip");
    let stub = host
        .enter("inner.7z")
        .expect("a 7z member opens a store attempt");
    let err = s
        .archive_list(&stub)
        .expect_err("an empty header stub is not an archive");
    assert!(
        matches!(
            err.kind(),
            "archive_corrupt" | "unsupported_archive" | "io_error"
        ),
        "the 7z stub reports {err} (kind {})",
        err.kind()
    );

    // A truncated zip: the central directory is gone, so even the listing fails — and it fails typed.
    let path = copy_archive("plain.zip", "truncated");
    let mut bytes = fs::read(&path).expect("read");
    bytes.truncate(bytes.len() / 2);
    fs::write(&path, &bytes).expect("write the truncated copy");
    let abs = AbsPath::from_path(&path).expect("abs");
    let store = FsPath::from_abs(abs).as_store().expect("store");
    let err = s
        .archive_list(&store)
        .expect_err("a zip without a central directory is corrupt");
    assert_eq!(err.kind(), "archive_corrupt", "{err}");
}

/// 8. A symlink member extracts as a regular file — never as a link, and never outside the destination.
#[test]
fn extract_symlink_member_is_a_plain_file() {
    let mut s = session();
    let dest = scratch("symlink-dest");
    wipe(&dest);
    fs::create_dir_all(&dest).expect("dest");
    let out = dest.join("out");
    let req = ExtractReq::all(
        AbsPath::from_path(archive_path("symlink.zip")).expect("host"),
        None,
        AbsPath::from_path(&out).expect("dest"),
    );
    let id = job_of(s.archive_extract(&req, Origin(7)).expect("extract starts"));
    let report = finish(&mut s, id, true);
    assert_eq!(
        report.written.len(),
        2,
        "both members extracted: {report:?}"
    );
    assert_eq!(report.symlinks, 0, "the count is filled by the reader pass");

    let link = out.join("link.txt");
    let real = out.join("real.txt");
    assert!(real.is_file(), "the ordinary member is there");
    assert!(link.is_file(), "the symlink member became a file");
    let md = fs::symlink_metadata(&link).expect("lstat");
    assert!(
        !md.file_type().is_symlink(),
        "extraction never creates a symlink: that is the escape route"
    );
    assert_eq!(
        fs::read(&link).expect("read link.txt"),
        b"real.txt",
        "Go parity: the member's body is the target text (`archive-list-symlink.json`)"
    );
    assert_eq!(
        fs::read_dir(&out)
            .expect("dest listing")
            .filter(std::result::Result::is_ok)
            .count(),
        2,
        "the destination holds exactly the two members, and nothing beside them"
    );
}

/// 9. An extraction that would clobber is answered by the policy, not by silence (plan F12).
#[test]
fn extract_collision_follows_the_policy() {
    let mut s = session();
    let dest = scratch("collision-dest");
    wipe(&dest);
    fs::create_dir_all(&dest).expect("dest");
    let out = dest.join("out");
    fs::create_dir_all(&out).expect("out");
    fs::write(out.join("top.txt"), b"already here\n").expect("seed the collision");
    fs::create_dir_all(out.join("docs")).expect("seed the directory collision");
    fs::write(out.join("docs").join("readme.md"), b"mine\n").expect("seed a nested collision");

    // The v1 default: skip, and say so.
    let req = ExtractReq {
        policy: ConflictPolicy::SkipExisting,
        ..ExtractReq::all(
            AbsPath::from_path(archive_path("plain.zip")).expect("host"),
            None,
            AbsPath::from_path(&out).expect("dest"),
        )
    };
    let id = job_of(s.archive_extract(&req, Origin(11)).expect("extract starts"));
    let report = finish(&mut s, id, true);
    let skipped_names: Vec<&str> = report.skipped.iter().map(|s| s.as_str()).collect();
    assert!(
        skipped_names.contains(&"top.txt"),
        "the colliding member is reported, not dropped: {skipped_names:?}"
    );
    assert_eq!(
        fs::read(out.join("top.txt")).expect("still there"),
        b"already here\n",
        "SkipExisting leaves the existing file alone"
    );
    assert!(
        out.join("docs").join("nested").join("deep.txt").is_file(),
        "the merge half of the same job still wrote what was free"
    );

    // The rename policy keeps both.
    let req = ExtractReq {
        policy: ConflictPolicy::Rename,
        ..req
    };
    let id = job_of(s.archive_extract(&req, Origin(12)).expect("extract starts"));
    let renamed_report = finish(&mut s, id, true);
    let renamed: Vec<&str> = renamed_report
        .renamed
        .iter()
        .map(|(member, _)| member.as_str())
        .collect();
    assert_eq!(
        renamed.len(),
        3,
        "every member now collides, because the first run left its own files: {renamed:?}"
    );
    assert!(renamed.contains(&"top.txt"), "{renamed:?}");
    let renamed: Vec<PathBuf> = fs::read_dir(&out)
        .expect("read dest")
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().contains(" (1)"))
        })
        .collect();
    assert!(
        !renamed.is_empty(),
        "the rename policy produced a `name (1)` copy: {:?}",
        fs::read_dir(&out).map(|d| d.count())
    );
    assert_eq!(
        fs::read(out.join("top.txt")).expect("original"),
        b"already here\n",
        "and it did not touch the original"
    );
}

/// Archive **members** never reach the file index; extraction's real files do. This is the written
/// decision in `services/archive.rs` (the SQLite `path` index is a unique index over filesystem paths, and
/// a member is not one).
#[test]
fn archive_members_are_never_indexed() {
    let mut s = session();
    let recorder = Arc::new(RecordingIntent::new());
    s.set_intent(Arc::clone(&recorder) as Arc<dyn IndexIntent>);

    let host = store_of(Path::new("/"), "nested.zip");
    let _ = s.archive_list(&host).expect("list");
    let inner = host.enter("inner.zip").expect("nested store");
    let _ = s.archive_list(&inner).expect("nested list");
    let _ = s
        .archive_read(&inner.join("inner-file.txt").expect("member"))
        .expect("read");
    let _ = s.archive_props(&host).expect("props");
    let _ = s
        .archive_list(&store_of(Path::new("/"), "plain.zip"))
        .expect("list again");
    assert!(
        recorder.is_empty(),
        "browsing an archive wrote {} index ops: {:?}",
        recorder.len(),
        recorder.recorded()
    );

    // Extraction, by contrast, must report the files it really wrote.
    let dest = scratch("index-dest");
    wipe(&dest);
    fs::create_dir_all(&dest).expect("dest");
    let out = dest.join("out");
    let req = ExtractReq::all(
        AbsPath::from_path(archive_path("plain.zip")).expect("host"),
        None,
        AbsPath::from_path(&out).expect("dest"),
    );
    let id = job_of(s.archive_extract(&req, Origin(21)).expect("extract starts"));
    let report = finish(&mut s, id, true);
    assert_eq!(report.written.len(), 3, "three members, three real files");
    let ops = recorder.recorded();
    assert!(
        !ops.is_empty(),
        "an extraction writes real files, and those are indexable"
    );
    for op in &ops {
        let IndexOp::Created { path } = op else {
            panic!("an extract may only create: {op:?}");
        };
        assert!(
            AbsPath::from_path(&out)
                .expect("abs out")
                .is_ancestor_of(path),
            "every noted path is under the destination: {path}"
        );
    }
    assert!(
        ops.iter()
            .any(|op| clean_internal_path(&op.subject().to_string()).contains("top.txt")),
        "the extracted member is named: {ops:?}"
    );
}

// --------------------------------------------------------------------------- helpers

fn scratch(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "zenman-contract-archive-{}-{tag}",
        std::process::id()
    ))
}

fn job_of(receipt: Receipt<ExtractReport>) -> JobId {
    match receipt {
        Receipt::Running { job, .. } => job,
        Receipt::Done(_) => panic!("extraction is always a job (plan F1)"),
    }
}

/// Drain events until this job's terminal event arrives, then hand back its report.
///
/// The engine coalesces invalidations on a **wall-clock** window (`services/dirs.rs`), so one `poll` is
/// not enough — the loop keeps draining until `DirChanged` for the destination has come out too, which is
/// what the pane reload depends on.
fn finish(s: &mut Session, id: JobId, expect_dir_changed: bool) -> ExtractReport {
    let mut report: Option<ExtractReport> = None;
    let mut saw_dir_changed = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        for ev in s.poll() {
            match ev {
                EngineEvent::DirChanged { .. } => saw_dir_changed = true,
                EngineEvent::JobFinished {
                    id: done, error, ..
                } if done == id => {
                    if let Some(e) = error {
                        panic!("the extract job {done} failed: {e}");
                    }
                    report = s
                        .archive_extract_status(id)
                        .and_then(|job| job.report())
                        .or_else(|| Some(ExtractReport::default()));
                }
                _ => {}
            }
        }
        if report.is_some() && (saw_dir_changed || !expect_dir_changed) {
            break;
        }
        s.archive_gc();
        std::thread::sleep(Duration::from_millis(25));
    }
    let report = report.expect("the extract job reported a terminal event within 30 s");
    if expect_dir_changed {
        assert!(
            saw_dir_changed,
            "an extraction must invalidate its destination (plan F13)"
        );
    }
    report
}

/// `std::fs::remove_dir_all` is banned workspace-wide
fn wipe(dir: &Path) {
    if dir.symlink_metadata().is_err() {
        return;
    }
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let is_dir =
                path.symlink_metadata().map(|m| m.is_dir()).unwrap_or(false) && !path.is_symlink();
            if is_dir {
                wipe(&path);
            } else {
                let _ = fs::remove_file(&path);
            }
        }
    }
    let _ = fs::remove_dir(dir);
}
