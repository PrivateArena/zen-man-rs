//! Step 9b acceptance gate (plan §7 row 9, paste half; §5.10 names this file): the write path is the Go
//! program's, and the record it appends is the Go program's, byte for byte.
//!
//! Everything runs against real temporary trees — no mock filesystem, no Go server, no GPU. The three seams
//! a test needs are the ones the plan named for this step:
//!
//! * [`MoveFs`](zenman_core::ops::r#move::MoveFs) is the `EXDEV` seam (§5.3: "the cross-device path is
//!   covered by an injected-failure seam, mocked in tests"), so the copy+delete fallback is exercised on a
//!   single disk;
//! * [`ProgressSink`](zenman_core::ops::ProgressSink) is the progress channel, so a test can assert progress
//!   was reported *and* fire the cancel token part-way through a walk;
//! * the session's undo log is a real file in the scratch directory, which is what makes the byte-parity
//!   claim below checkable rather than aspirational.
//!
//! # The byte-parity claim (`paste_appends_a_go_parity_log_record`)
//!
//! `history/codec.rs` is frozen (house rule 8) and untouched by this step. The test reads the frames the
//! *paste* wrote and asserts two things: `codec::encode_vec(decode(frame)) == frame` for every frame (the
//! decode→encode identity `contract_history.rs` uses), and that the record's field roles match the
//! `paste-copy` record of the frozen `tests/golden/log.json` — action byte, reversibility, empty `name`,
//! non-empty `dest`, sources in the order the walk ran them.
//!
//! Unix-only: the trees need symlinks and `0000` modes.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zenman_core::error::CoreError;
use zenman_core::events::{EngineEvent, JobPayload};
use zenman_core::history::codec::{self, ActionType};
use zenman_core::jobs::{CancelToken, JobId};
use zenman_core::limits::Limits;
use zenman_core::ops::clipboard::{ClipOp, ClipScope, ClipboardStore};
use zenman_core::ops::copy::{copy_file, copy_item};
use zenman_core::ops::guard::{self, SourceReject};
use zenman_core::ops::r#move as move_ops;
use zenman_core::ops::r#move::{MoveFs, OsMoveFs};
use zenman_core::ops::paste::{
    self, PasteMode, PasteOutcome, PasteReport, PasteReq, SkipReason, TRACK_ENTRIES_CAP,
};
use zenman_core::ops::{ConflictPolicy, ProgressSink};
use zenman_core::path::{AbsPath, FsPath};
use zenman_core::platform::default_env;
use zenman_core::session::{Origin, Receipt, Session, SessionPaths};
use zenman_core::store::config::ZenConfig;

const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/golden");

fn golden_json(name: &str) -> serde_json::Value {
    let text = fs::read_to_string(Path::new(GOLDEN).join(name)).expect("golden is readable");
    serde_json::from_str(&text).expect("golden is valid JSON")
}

// -------------------------------------------------------------------------------------- scratch trees

// Scratch naming, recursive delete and stale-tree reaping: the shared test-support module.
#[path = "../src/scratch.rs"]
mod scratch;

/// A fresh, uniquely-named scratch tree (`zenman-contract-ops-<tag>-<pid>-<seq>`).
fn temp_dir(tag: &str) -> PathBuf {
    scratch::create(&format!("contract-ops-{tag}"))
}

/// Recursive delete for a *test scratch tree* — delegated to the shared module
/// (`scratch::remove_tree`), which is `remove_dir_all`-free (`clippy.toml` says product deletion
/// must go through `ops::delete` — which must never be what cleans a temp directory anyway).
fn remove_tree(path: &Path) -> std::io::Result<()> {
    scratch::remove_tree(path)
}

fn write(path: &Path, bytes: &[u8]) -> AbsPath {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent created");
    }
    fs::write(path, bytes).expect("file written");
    abs(path)
}

fn mkdir(path: &Path) -> AbsPath {
    fs::create_dir_all(path).expect("dir created");
    abs(path)
}

fn abs(path: &Path) -> AbsPath {
    AbsPath::try_from(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn read(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The invalidation key the engine uses for a local path (plan §4.1: `DirChanged` names an `FsPath`).
fn fskey(path: &AbsPath) -> String {
    FsPath::from_abs(path.clone()).to_string()
}

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::symlink_metadata(path)
        .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
        .permissions()
        .mode()
        & 0o7777
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .unwrap_or_else(|e| panic!("chmod {}: {e}", path.display()));
}

// ----------------------------------------------------------------------------------------- test seams

/// Counts progress reports; can fire the cancel token after the n-th one.
struct Recorder {
    calls: Mutex<Vec<(u64, u64, String)>>,
    cancel_after: Option<usize>,
    token: Option<Arc<CancelToken>>,
}

impl Recorder {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            cancel_after: None,
            token: None,
        }
    }

    fn cancelling(after: usize, token: Arc<CancelToken>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            cancel_after: Some(after),
            token: Some(token),
        }
    }

    fn seen(&self) -> usize {
        self.calls.lock().expect("recorder is not poisoned").len()
    }

    fn totals(&self) -> (u64, u64) {
        let guard = self.calls.lock().expect("recorder is not poisoned");
        guard
            .iter()
            .fold((0, 0), |(b, f), (nb, nf, _)| (b + nb, f + nf))
    }
}

impl ProgressSink for Recorder {
    fn advance(&self, bytes: u64, files: u64, current: &str) {
        let mut guard = self.calls.lock().expect("recorder is not poisoned");
        guard.push((bytes, files, current.to_string()));
        if let (Some(after), Some(token)) = (self.cancel_after, self.token.as_ref())
            && guard.len() >= after
        {
            token.cancel();
        }
    }
}

/// A [`MoveFs`] whose `rename` always answers `EXDEV`: the cross-device fallback, on one disk.
struct CrossDevice;

impl MoveFs for CrossDevice {
    fn rename(&self, _src: &AbsPath, _dst: &AbsPath) -> Result<(), CoreError> {
        Err(move_ops::exdev())
    }

    fn remove(&self, path: &AbsPath) -> Result<(), CoreError> {
        OsMoveFs.remove(path)
    }
}

fn cancel() -> Arc<CancelToken> {
    Arc::new(CancelToken::new())
}

// -------------------------------------------------------------------------------------- the engine seam

fn open_session(dir: &Path) -> Session {
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

/// Drive a paste job to its terminal event, collecting every event the engine emitted on the way —
/// including the coalesced `DirChanged`s, which is why this keeps draining for a while after the finish and
/// then flushes what the coalescing window still holds.
fn wait_paste(session: &mut Session, job: JobId) -> (PasteReport, Vec<EngineEvent>) {
    let mut events = Vec::new();
    let mut report: Option<PasteReport> = None;
    for round in 0..120 {
        events.extend(session.poll());
        if report.is_none() {
            report = events.iter().find_map(|ev| match ev {
                EngineEvent::JobFinished { id, payload, .. } if *id == job => match payload {
                    JobPayload::Paste(r) => Some(r.clone()),
                    other => panic!("paste job {job} finished with a non-paste payload: {other:?}"),
                },
                _ => None,
            });
        }
        if report.is_some() && round > 8 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    events.extend(session.dirs().flush_invalidations());
    match report {
        Some(r) => (r, events),
        None => panic!("the paste job {job} never produced a terminal event: {events:?}"),
    }
}

/// The `(directory, origin)` pairs a run invalidated.
fn dir_changed(events: &[EngineEvent]) -> Vec<(String, Option<Origin>)> {
    events
        .iter()
        .filter_map(|ev| match ev {
            EngineEvent::DirChanged { path, origin, .. } => Some((path.to_string(), *origin)),
            _ => None,
        })
        .collect()
}

fn paste_req(sources: Vec<AbsPath>, dest: AbsPath, mode: PasteMode) -> PasteReq {
    PasteReq {
        sources,
        dest,
        mode,
        policy: ConflictPolicy::SkipExisting,
    }
}

fn started(session: &mut Session, req: &PasteReq) -> (JobId, Origin) {
    let origin = session.next_origin();
    match session.paste(req, origin) {
        Ok(Receipt::Running { job, origin }) => (job, origin),
        Ok(Receipt::Done(r)) => panic!("a paste is never synchronous: {r:?}"),
        Err(e) => panic!("the paste was refused before it started: {e:?}"),
    }
}

// --------------------------------------------------------------------------------------------- the copy

/// Go's `copyDir`/`copyFile` semantics, with the two things Go got wrong fixed and asserted: the copy keeps
/// the source mode *and* mtime, and a symlink inside the tree comes out as a symlink.
#[test]
fn copy_tree_preserves_bytes_mode_mtime_and_links() {
    let root = temp_dir("copy-tree");
    let src = mkdir(&root.join("src"));
    let body = vec![7u8; 3000];
    write(&root.join("src/alpha.txt"), &body);
    set_mode(&root.join("src/alpha.txt"), 0o640);
    write(&root.join("src/sub/beta.txt"), b"beta");
    std::os::unix::fs::symlink(root.join("alpha.txt"), root.join("src/link.txt")).expect("symlink");
    let dest = mkdir(&root.join("dest"));
    let token = cancel();
    let rec = Recorder::new();

    let stats = copy_item(
        &src,
        &dest.join("src").expect("joined"),
        ConflictPolicy::SkipExisting,
        true,
        &token,
        &rec,
    )
    .expect("the copy succeeds");

    assert_eq!(
        stats.bytes, 3004,
        "both files' bytes, and nothing for the link"
    );
    assert_eq!(stats.files, 2);
    assert_eq!(stats.dirs, 2, "the tree root and `sub`");
    assert_eq!(stats.links, 1, "the symlink was recreated, not read");
    assert!(stats.skipped.is_empty());
    assert_eq!(
        read(&root.join("dest/src/alpha.txt")),
        body,
        "content is byte-equal"
    );
    assert_eq!(
        mode_of(&root.join("dest/src/alpha.txt")),
        0o640,
        "Go preserved mode"
    );
    let src_m = fs::metadata(root.join("src/alpha.txt")).expect("src stat");
    let dst_m = fs::metadata(root.join("dest/src/alpha.txt")).expect("dst stat");
    assert_eq!(
        src_m.modified().expect("mtime"),
        dst_m.modified().expect("mtime"),
        "plan §5.3 asks for mtime too, which Go's `copyFile` lost"
    );
    let link = fs::symlink_metadata(root.join("dest/src/link.txt")).expect("link stat");
    assert!(link.file_type().is_symlink(), "a link came out as a link");
    assert_eq!(stats.dest, Some(dest.join("src").expect("joined")));
    // Progress is asserted through the same sink a job hands the engine, so the numbers the jobs pane
    // shows are the numbers the walk produced.
    assert!(
        rec.seen() >= 2,
        "every file reported progress, saw {}",
        rec.seen()
    );
    assert_eq!(rec.totals(), (3004, 3), "two files plus the recreated link");
    let _ = remove_tree(&root);
}

/// The self-copy refusal: Go's `copyFile` opened the source, created the same path as the destination and
/// truncated it — the copy deleted the file it was copying. A non-regular source (a directory here, a FIFO
/// in the fixture tree) is refused rather than opened, because opening one can block the worker forever
/// (plan §6.1 F1).
#[test]
fn copy_refuses_a_file_onto_itself_and_a_non_regular_source() {
    let root = temp_dir("self-copy");
    let f = write(&root.join("f.txt"), b"keep me");
    let dir = mkdir(&root.join("some_dir"));
    let token = cancel();

    let err = copy_file(&f, &f, &token, &Recorder::new())
        .expect_err("a file cannot be copied onto itself");
    assert!(
        matches!(err, CoreError::DestInsideSource { .. }),
        "typed containment error, got {err:?}"
    );
    assert_eq!(
        read(&root.join("f.txt")),
        b"keep me",
        "and the bytes survived"
    );

    let err = copy_file(&dir, &f, &token, &Recorder::new()).expect_err("a directory is not a file");
    assert!(
        matches!(err, CoreError::Unsupported { .. }),
        "refused, not opened: {err:?}"
    );
    assert_eq!(read(&root.join("f.txt")), b"keep me");
    let _ = remove_tree(&root);
}

// ----------------------------------------------------------------------------------------- the paste

/// The whole §7-row-9 paste claim in one test: a copy paste is a job, it invalidates the destination with
/// the caller's `Origin`, and the undo-log record it appends is Go's — both in its field roles (compared
/// against the frozen `log.json` record) and in its bytes (`decode → encode` is the identity).
#[test]
fn paste_appends_a_go_parity_log_record() {
    let root = temp_dir("paste-log");
    let mut session = open_session(&root);
    let one = write(&root.join("tree/one.txt"), b"one");
    let two = write(&root.join("tree/two.txt"), b"two");
    let dest = mkdir(&root.join("dest"));
    let (job, origin) = started(
        &mut session,
        &paste_req(
            vec![two.clone(), one.clone()],
            dest.clone(),
            PasteMode::Copy,
        ),
    );
    let (report, events) = wait_paste(&mut session, job);

    assert_eq!(report.mode, PasteMode::Copy);
    assert_eq!(
        report.placed().len(),
        2,
        "both files landed: {:?}",
        report.outcomes
    );
    assert!(report.log_id.is_some(), "the record was appended");
    assert_eq!(report.bytes, 6);
    assert!(
        !report.reload_required,
        "2 sources is under the {TRACK_ENTRIES_CAP} cap"
    );
    assert_eq!(
        report
            .placed()
            .iter()
            .map(|(s, _)| s.to_string_lossy())
            .collect::<Vec<_>>(),
        vec![two.to_string_lossy(), one.to_string_lossy()],
        "the log's source order is the request order, as Go appended it"
    );

    // DirChanged, with the origin that asked (plan §4.1's correlation rule).
    let want = fskey(&dest);
    let changed = dir_changed(&events);
    assert!(
        changed
            .iter()
            .any(|(p, o)| *p == want && *o == Some(origin)),
        "the destination was invalidated with the caller's origin; got {changed:?}"
    );
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, EngineEvent::LogAppended { id } if Some(*id) == report.log_id)),
        "the history pane is told about the record: {events:?}"
    );

    // The record itself, against the frozen capture.
    let records = session.log_recent(None).expect("the log reads back");
    let rec = records
        .records
        .iter()
        .find(|r| r.action == ActionType::PASTE_COPY)
        .expect("a paste-copy record exists");
    let golden = golden_json("log.json");
    let go = golden["records"]
        .as_array()
        .expect("records")
        .iter()
        .find(|r| r["action"].as_i64() == Some(i64::from(ActionType::PASTE_COPY.0)))
        .expect("the capture contains a paste-copy record");
    assert_eq!(go["action_str"].as_str(), Some("paste-copy"));
    assert_eq!(rec.action, ActionType::PASTE_COPY);
    assert_eq!(
        rec.name, "",
        "`name` is unused by paste, exactly as in the capture"
    );
    assert_eq!(go["name"].as_str(), Some(""), "and the capture agrees");
    assert_eq!(
        rec.dest,
        dest.to_string_lossy(),
        "`dest` carries the destination directory"
    );
    assert_eq!(rec.sources.len(), 2);
    assert!(
        rec.action.is_reversible(),
        "paste-copy is reversible (`api/log.go:596-602`)"
    );

    // Byte identity of every frame the paste wrote.
    let bytes = fs::read(root.join("action.log")).expect("the log exists");
    let mut pos = 0usize;
    let mut frames = 0usize;
    while pos < bytes.len() {
        let d = codec::decode_at(&bytes[pos..], pos as u64).expect("every frame decodes");
        assert!(d.crc_ok, "frame at {pos} has a bad CRC");
        let re = codec::encode_vec(&d.record).expect("and re-encodes");
        assert_eq!(re.len(), d.consumed, "the frame length round-trips");
        assert_eq!(
            re,
            &bytes[pos..pos + d.consumed],
            "re-encode is the identity"
        );
        pos += d.consumed;
        frames += 1;
    }
    assert_eq!(frames, 1, "a copy paste appends exactly one record");
    let _ = remove_tree(&root);
}

/// A cut paste moves the bytes, logs `paste-move`, invalidates *both* sides, and clears the clipboard —
/// Go's `handlePaste` `operation == "cut"` branch (`api/ops.go:321,339-344`).
#[test]
fn paste_cut_moves_sources_and_clears_the_clipboard() {
    let root = temp_dir("paste-cut");
    let mut session = open_session(&root);
    let src_dir = mkdir(&root.join("from"));
    let a = write(&root.join("from/a.txt"), b"aaaa");
    let b = write(&root.join("from/b.txt"), b"bb");
    let dest = mkdir(&root.join("to"));
    session
        .clipboard_store(&ClipboardStore {
            op: ClipOp::Cut,
            sources: vec![a.clone(), b.clone()],
            scope: ClipScope::Selected,
        })
        .expect("the selection is queued");
    assert!(session.clipboard().is_some(), "and it is visible to the UI");

    let origin = session.next_origin();
    let job = match session.paste_clipboard(dest.clone(), ConflictPolicy::SkipExisting, origin) {
        Ok(Receipt::Running { job, .. }) => job,
        other => panic!("the queued cut pastes as a job: {other:?}"),
    };
    let (report, events) = wait_paste(&mut session, job);

    assert_eq!(report.placed().len(), 2);
    assert!(report.failed().is_empty(), "{:?}", report.outcomes);
    assert_eq!(read(&root.join("to/a.txt")), b"aaaa");
    assert_eq!(read(&root.join("to/b.txt")), b"bb");
    assert!(
        !exists(&root.join("from/a.txt")),
        "a cut leaves nothing behind"
    );
    assert!(!exists(&root.join("from/b.txt")));
    assert!(
        session.clipboard().is_none(),
        "a clean cut clears the clipboard (Go parity)"
    );
    assert!(
        session
            .log_recent(None)
            .expect("log")
            .records
            .iter()
            .any(|r| r.action == ActionType::PASTE_MOVE && r.dest == dest.to_string_lossy()),
        "the record is a paste-move"
    );
    let changed = dir_changed(&events);
    let from = fskey(&src_dir);
    let to = fskey(&dest);
    assert!(
        changed.iter().any(|(p, _)| *p == to),
        "destination invalidated: {changed:?}"
    );
    assert!(
        changed.iter().any(|(p, _)| *p == from),
        "the source directory is invalidated too, or the pane shows the file twice: {changed:?}"
    );
    let _ = remove_tree(&root);
}

/// The three collision answers, on one taken name — plan F12's "a rejection is typed, never a silent
/// skip", and the reason the policy lives in `ops` rather than in each caller.
#[test]
fn each_collision_policy_answers_a_taken_name() {
    let root = temp_dir("policy");
    let mut session = open_session(&root);
    for (n, policy) in [
        ("skip", ConflictPolicy::SkipExisting),
        ("over", ConflictPolicy::Overwrite),
        ("rename", ConflictPolicy::Rename),
    ] {
        let source = write(&root.join(format!("src-{n}/f.txt")), b"fresh");
        let dest = mkdir(&root.join(format!("dest-{n}")));
        let taken = root.join(format!("dest-{n}/f.txt"));
        write(&taken, b"ORIGINAL");
        let req = PasteReq {
            sources: vec![source.clone()],
            dest,
            mode: PasteMode::Copy,
            policy,
        };
        let (job, _o) = started(&mut session, &req);
        let (report, _ev) = wait_paste(&mut session, job);
        assert_eq!(report.outcomes.len(), 1, "{n}: {:?}", report.outcomes);
        match policy {
            ConflictPolicy::SkipExisting => {
                assert_eq!(
                    report.outcomes[0],
                    PasteOutcome::Skipped {
                        source: source.clone(),
                        target: abs(&taken),
                        reason: SkipReason::Existing,
                    },
                    "the skip is a value, not an absence"
                );
                assert_eq!(read(&taken), b"ORIGINAL", "and the original survived");
            }
            ConflictPolicy::Overwrite => {
                assert_eq!(read(&taken), b"fresh", "the clobber landed");
                assert!(matches!(report.outcomes[0], PasteOutcome::Placed { .. }));
            }
            ConflictPolicy::Rename => {
                assert_eq!(read(&taken), b"ORIGINAL", "nothing was clobbered");
                let renamed = root.join(format!("dest-{n}/f (1).txt"));
                assert_eq!(read(&renamed), b"fresh", "and both names now exist");
                assert_eq!(
                    report.outcomes[0],
                    PasteOutcome::Placed {
                        source: source.clone(),
                        target: abs(&renamed),
                    },
                    "the report names the path that was actually used"
                );
            }
        }
    }
    let _ = remove_tree(&root);
}

/// The containment guard, end to end: a wholly refused selection is an error on the call, and a partly
/// refused one pastes the rest and reports what it refused (Go `continue`d, `api/ops.go:309`).
#[test]
fn the_guard_refuses_a_paste_into_itself_and_reports_a_partial() {
    let root = temp_dir("guard");
    let mut session = open_session(&root);
    let project = mkdir(&root.join("project"));
    let build = mkdir(&root.join("project/build"));
    let outside = write(&root.join("loose.txt"), b"y");

    // `project` → `project/build`: every source refused, so the call itself fails and no job is born.
    let origin = session.next_origin();
    let err = session
        .paste(
            &paste_req(vec![project.clone()], build.clone(), PasteMode::Copy),
            origin,
        )
        .expect_err("pasting a directory into itself is refused");
    assert!(
        matches!(&err, CoreError::DestInsideSource { src, dest }
            if *src == project.to_string_lossy()
                && *dest == build.join("project").expect("joined").to_string_lossy()),
        "typed, naming the source and the target that would have landed inside it: {err:?}"
    );
    assert!(session.jobs().is_empty(), "a refused paste spawns no job");
    // …and the `src == dst` half of Go's rule (`api/ops.go:309`): a file pasted into its own directory
    // would be written onto itself.
    let inner = write(&root.join("project/inner.txt"), b"i");
    let g = guard::guard_sources(std::slice::from_ref(&inner), &project).expect("guard runs");
    assert_eq!(g.rejected, vec![(inner.clone(), SourceReject::SamePath)]);
    assert!(g.allowed.is_empty());

    // A mixed selection into `project/build`: `project` is refused, `loose.txt` still pastes.
    let req = PasteReq {
        sources: vec![project.clone(), outside],
        dest: build.clone(),
        mode: PasteMode::Copy,
        policy: ConflictPolicy::SkipExisting,
    };
    let (job, _o) = started(&mut session, &req);
    let (report, _ev) = wait_paste(&mut session, job);
    assert_eq!(
        report.rejected,
        vec![(project.clone(), SourceReject::DestInsideSource)],
        "the refused source is named, with its reason"
    );
    assert_eq!(
        report.placed().len(),
        1,
        "the rest of the selection still pasted"
    );
    assert!(exists(&root.join("project/build/loose.txt")));
    let _ = remove_tree(&root);
}

/// Dir onto dir: Go merged (`copyDir`'s `MkdirAll`, `mergeDirViaRename`) while its `status:"conflict"`
/// answer was an HTTP 200 with a magic string (ledger #8). Here the merge happens and the collision is a
/// typed field of the report.
#[test]
fn merge_dir_onto_dir_merges_and_names_the_conflict() {
    let root = temp_dir("merge");
    let src = mkdir(&root.join("src"));
    write(&root.join("src/only.txt"), b"only");
    write(&root.join("src/nested/deep.txt"), b"deep");
    let dest = mkdir(&root.join("dest"));
    write(&root.join("dest/src/kept.txt"), b"kept");
    let token = cancel();

    let stats = move_ops::move_item(
        &src,
        &dest.join("src").expect("joined"),
        ConflictPolicy::Overwrite,
        &OsMoveFs,
        &token,
        &Recorder::new(),
    )
    .expect("the merge succeeds");
    assert!(
        exists(&root.join("dest/src/only.txt")),
        "the new file arrived"
    );
    assert!(
        exists(&root.join("dest/src/nested/deep.txt")),
        "and so did the new subdirectory"
    );
    assert_eq!(
        read(&root.join("dest/src/kept.txt")),
        b"kept",
        "the pre-existing file is untouched"
    );
    assert!(
        !exists(&root.join("src")),
        "the merged source shell is gone"
    );
    assert!(
        stats.skipped.is_empty(),
        "nothing was left behind: {:?}",
        stats.skipped
    );

    // …and the same pairing through the paste path reports the conflict instead of hiding it.
    let mut session = open_session(&root);
    let pkg = mkdir(&root.join("pkg"));
    write(&root.join("pkg/x.txt"), b"x");
    let into = mkdir(&root.join("into"));
    mkdir(&root.join("into/pkg"));
    let (job, _o) = started(
        &mut session,
        &paste_req(vec![pkg], into.clone(), PasteMode::Copy),
    );
    let (report, _ev) = wait_paste(&mut session, job);
    assert_eq!(
        report.conflicts,
        vec!["pkg".to_string()],
        "the dir-onto-dir pairing is named"
    );
    assert!(
        exists(&root.join("into/pkg/x.txt")),
        "and it merged, per the policy"
    );
    let _ = remove_tree(&root);
}

/// A merge that *cannot* finish is an error naming the leftovers, not Go's silent success with the source
/// still on disk (`api/ops.go:745-750`).
#[test]
fn a_blocked_merge_reports_the_leftovers() {
    let root = temp_dir("merge-blocked");
    let src = mkdir(&root.join("src"));
    write(&root.join("src/same.txt"), b"new content");
    let dest = mkdir(&root.join("dest"));
    write(&root.join("dest/src/same.txt"), b"ORIGINAL");
    let token = cancel();

    let err = move_ops::move_item(
        &src,
        &dest.join("src").expect("joined"),
        ConflictPolicy::SkipExisting,
        &OsMoveFs,
        &token,
        &Recorder::new(),
    )
    .expect_err("a file collision inside a merge is not silently skipped by a *move*");
    assert!(
        matches!(&err, CoreError::Conflict { conflicts, .. } if conflicts == &vec!["same.txt".to_string()]),
        "the report names what is still in the source: {err:?}"
    );
    assert_eq!(
        read(&root.join("dest/src/same.txt")),
        b"ORIGINAL",
        "the existing file was not clobbered"
    );
    assert!(
        exists(&root.join("src/same.txt")),
        "and the source is intact"
    );
    let _ = remove_tree(&root);
}

/// The `EXDEV` fallback, on one disk: `rename` refuses, so the engine copies and deletes. Plan §5.3's
/// requirement — "cross-device failure leaves the source intact" — is asserted with an unreadable source.
#[test]
fn move_across_devices_falls_back_to_copy_and_rolls_back_on_failure() {
    let root = temp_dir("exdev");
    let token = cancel();
    let file = write(&root.join("from/thing.txt"), b"cross device bytes");
    let dest = mkdir(&root.join("to"));

    let stats = move_ops::move_item(
        &file,
        &dest.join("thing.txt").expect("joined"),
        ConflictPolicy::SkipExisting,
        &CrossDevice,
        &token,
        &Recorder::new(),
    )
    .expect("the fallback path completes");
    assert!(
        stats.bytes > 0,
        "the fallback moved bytes, not a directory entry: {}",
        stats.bytes
    );
    assert!(!stats.renamed, "the rename never succeeded");
    assert_eq!(read(&root.join("to/thing.txt")), b"cross device bytes");
    assert!(
        !exists(&root.join("from/thing.txt")),
        "and the source went away"
    );

    // A directory crosses the same way: children copied, source removed.
    let tree = mkdir(&root.join("tree"));
    write(&root.join("tree/a/one.txt"), b"11");
    write(&root.join("tree/two.txt"), b"222");
    let stats = move_ops::move_item(
        &tree,
        &dest.join("tree").expect("joined"),
        ConflictPolicy::SkipExisting,
        &CrossDevice,
        &token,
        &Recorder::new(),
    )
    .expect("a tree crosses too");
    assert!(stats.bytes >= 5, "both files were copied: {stats:?}");
    assert_eq!(read(&root.join("to/tree/a/one.txt")), b"11");
    assert!(!exists(&root.join("tree")), "the whole source tree is gone");

    // Rollback: an unreadable source means the copy fails part-way.
    let doomed = write(&root.join("doomed.txt"), b"secret");
    set_mode(&root.join("doomed.txt"), 0o000);
    if fs::read(root.join("doomed.txt")).is_ok() {
        // Running as root, or on a filesystem that ignores modes: there is no way to make the copy fail,
        // so say so rather than pass a test that asserted nothing.
        println!("SKIPPED rollback half: this process can read a 0000 file");
        set_mode(&root.join("doomed.txt"), 0o644);
        let _ = remove_tree(&root);
        return;
    }
    let err = move_ops::move_item(
        &doomed,
        &dest.join("doomed.txt").expect("joined"),
        ConflictPolicy::SkipExisting,
        &CrossDevice,
        &token,
        &Recorder::new(),
    )
    .expect_err("the copy cannot succeed");
    assert!(
        matches!(
            err,
            CoreError::Io { .. } | CoreError::PermissionDenied { .. }
        ),
        "got {err:?}"
    );
    assert!(
        !exists(&root.join("to/doomed.txt")),
        "the partial destination was rolled back, not left half-written"
    );
    assert!(
        exists(&root.join("doomed.txt")),
        "and the source is still there"
    );
    set_mode(&root.join("doomed.txt"), 0o644);
    let _ = remove_tree(&root);
}

/// Cancelling part-way through a paste is a documented state, not a corrupted one: a prefix of the
/// selection exists at the destination, the rest does not, nothing moved — and, the point of appending
/// after the walk (plan F3), **no undo-log record was written at all**, so a revert can never be offered
/// for work that did not run.
#[test]
fn cancel_midway_leaves_a_prefix_and_no_log_record() {
    let root = temp_dir("cancel");
    let mut session = open_session(&root);
    mkdir(&root.join("bulk"));
    for i in 0..8 {
        write(&root.join(format!("bulk/f{i}.txt")), &vec![b'x'; 4096]);
    }
    let dest = mkdir(&root.join("dest"));
    let sources: Vec<AbsPath> = (0..8)
        .map(|i| abs(&root.join(format!("bulk/f{i}.txt"))))
        .collect();

    // The synchronous engine, so the cancel token is this test's own.
    let token = cancel();
    let rec = Recorder::cancelling(2, Arc::clone(&token));
    let g = guard::guard_sources(&sources, &dest).expect("nothing here is refused");
    let req = paste_req(sources.clone(), dest.clone(), PasteMode::Copy);
    let env = default_env();
    let err = paste::paste(env.as_ref(), &OsMoveFs, &req, &g, token.as_ref(), &rec)
        .expect_err("a cancelled paste does not report success");
    assert!(
        matches!(err, CoreError::Cancelled),
        "typed as cancellation: {err:?}"
    );
    assert!(
        rec.seen() >= 2,
        "progress was reported before the stop: {}",
        rec.seen()
    );
    let placed: Vec<String> = fs::read_dir(dest.as_path())
        .expect("dest readable")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(placed.len() < 8, "the walk stopped: {placed:?}");
    assert!(
        !placed.is_empty(),
        "and it had already placed the prefix it finished"
    );
    for name in &placed {
        assert_eq!(
            read(&root.join("bulk").join(name)),
            vec![b'x'; 4096],
            "a placed file is complete, and the source is intact"
        );
    }
    assert!(
        fs::metadata(root.join("action.log"))
            .map(|m| m.len() == 0)
            .unwrap_or(true),
        "a cancelled paste appends no undo record"
    );

    // Through the session, the same cancellation is a `Cancelled` job outcome, not a report — and a cut
    // that did not finish keeps its clipboard (Go's `lastErr == nil` condition, `api/ops.go:339`).
    session
        .clipboard_store(&ClipboardStore {
            op: ClipOp::Cut,
            sources: sources.clone(),
            scope: ClipScope::Selected,
        })
        .expect("queued");
    let _origin = session.next_origin();
    let (job, _o) = started(
        &mut session,
        &paste_req(sources, dest.clone(), PasteMode::Cut),
    );
    session.cancel(job);
    let mut outcome = None;
    for _ in 0..200 {
        for ev in session.poll() {
            if let EngineEvent::JobFinished { id, outcome: o, .. } = ev
                && id == job
            {
                outcome = Some(format!("{o:?}"));
            }
        }
        if outcome.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        outcome
            .as_deref()
            .is_some_and(|s| s.contains("Cancelled") || s.contains("Failed")),
        "the job reports cancellation, not a partial success: {outcome:?}"
    );
    let _ = remove_tree(&root);
}

/// `handlePasteLink` (`api/ops.go:839-917`): symlinks at the destination, sources untouched, and a
/// `paste-link` record — one of Go's five reversible action types.
#[test]
fn paste_link_creates_links_and_never_touches_the_sources() {
    let root = temp_dir("link");
    let mut session = open_session(&root);
    let src = write(&root.join("target.txt"), b"source stays");
    let dest = mkdir(&root.join("dest"));
    let (job, _o) = started(
        &mut session,
        &paste_req(vec![src.clone()], dest.clone(), PasteMode::Link),
    );
    let (report, _ev) = wait_paste(&mut session, job);
    if !report.placed_any() {
        println!(
            "SKIPPED: this platform refuses symlinks: {:?}",
            report.outcomes
        );
        let _ = remove_tree(&root);
        return;
    }
    let link = root.join("dest/target.txt");
    let meta = fs::symlink_metadata(&link).expect("the link exists");
    assert!(
        meta.file_type().is_symlink(),
        "paste-link produced a link, not a copy"
    );
    assert_eq!(
        fs::read_link(&link).expect("link target"),
        root.join("target.txt")
    );
    assert_eq!(
        read(&root.join("target.txt")),
        b"source stays",
        "source untouched"
    );
    assert!(
        session
            .log_recent(None)
            .expect("log")
            .records
            .iter()
            .any(|r| r.action == ActionType::PASTE_LINK && r.action.is_reversible()),
        "the record is a reversible paste-link"
    );
    assert_eq!(report.mode, PasteMode::Link);
    let _ = remove_tree(&root);
}

/// An empty selection is answered on the call, not by a job event (`api/ops.go:190`), and a paste of
/// nothing never becomes a job.
#[test]
fn a_refused_paste_never_becomes_a_job() {
    let root = temp_dir("refused");
    let session = open_session(&root);
    let dest = mkdir(&root.join("dest"));
    let origin = session.next_origin();
    let err = session
        .paste(
            &paste_req(Vec::new(), dest.clone(), PasteMode::Copy),
            origin,
        )
        .expect_err("nothing to paste");
    assert!(
        matches!(err, CoreError::Empty { .. }),
        "Go's \"no source folder specified\": {err:?}"
    );
    let err = session
        .paste_clipboard(dest, ConflictPolicy::SkipExisting, origin)
        .expect_err("and the clipboard is empty too");
    assert!(
        matches!(err, CoreError::Empty { .. }),
        "clipboard empty: {err:?}"
    );
    assert!(session.jobs().is_empty(), "a refused paste spawns no job");
    let _ = remove_tree(&root);
}

// ════════════════════════════════════════════════════════════════════════════════════════════
// step 9c: the rest of the write path — delete / rename / mkdir / chmod / open, and the revert of
// every record type that admits one.
//
// Same rules as the paste tests above: real temporary trees, the real undo-log file, the real job
// table, and the frozen capture as the authority on what a record has to look like. The one new
// seam is [`OpEnv`], a controllable [`Env`] — because the plan's delete ladder (trash / refused /
// broken) and the open path (never spawn `xdg-open` from a test) are both platform answers, and
// `trash`-ing a test's file into the developer's real `~/.local/share/Trash` is not a test, it is
// vandalism.
// ════════════════════════════════════════════════════════════════════════════════════════════

use zenman_core::error::CoreError as PubError;
use zenman_core::history::codec::LogStatus;
use zenman_core::history::revert::{self as revert_ops, RevertReport};
use zenman_core::index::intent::{IndexIntent, IndexOp, RecordingIntent};
use zenman_core::mode::{self, ModeBits};
use zenman_core::ops::chmod::{self, ChmodReq, Recursive};
use zenman_core::ops::delete::{DeleteMode, DeleteOutcome, DeleteReport, DeleteReq};
use zenman_core::ops::mkdir::DEFAULT_DIR_MODE;
use zenman_core::ops::mkdir::MkdirReq;
use zenman_core::ops::open::{self, OpenRoute};
use zenman_core::ops::rename::{self, RenameOutcome, RenameReq, RenameTarget};
use zenman_core::platform::{
    CopyItem, CopyReport, DiskSpace, Env, ExecOutput, ExecRequest, LinkItem, MountTable, Os,
    PlatformCaps, PlatformFuture, Shortcut, ready,
};

/// An [`Env`] whose three answers a test decides: is there a trash bin, does it accept this path, and
/// what did the launcher get asked to open.
///
/// The remaining methods are the minimum that keeps `Session::open` honest: `supports_symlink` yes (the
/// link tests need it), `copy_paths`/`exec_program` refused (nothing here should reach them, and a call
/// that does becomes a loud typed error rather than a spawn).
struct OpEnv {
    /// Where this fake bin puts what it "trashes" — a real directory in the test's own temp tree.
    bin: PathBuf,
    /// `false` models the measured `/media/…` case (plan §9.1): a volume with its own fsid has no
    /// native trash, so [`Env::can_trash`] answers no.
    has_trash: bool,
    /// `true` models `trash::Error::TargetedRoot` — the bin exists and refuses *this* path (`Ok(false)`).
    refuses: bool,
    /// `true` models a failure *while* trashing (`Err`), the rung that must never escalate to a delete.
    broken: bool,
    /// `true` makes the platform refuse `chmod` (the `Unsupported` lane, and the stub's answer).
    chmod_refused: bool,
    /// `true` makes the platform report "no opener configured" (`CoreError::Shell`, as
    /// `platform::linux::open_with` does when `open::commands()` is empty).
    no_opener: bool,
    opened: Mutex<Vec<String>>,
    trashed: Mutex<Vec<String>>,
}

impl OpEnv {
    fn new(bin: &Path) -> Self {
        Self {
            bin: bin.to_path_buf(),
            has_trash: true,
            refuses: false,
            broken: false,
            chmod_refused: false,
            no_opener: false,
            opened: Mutex::new(Vec::new()),
            trashed: Mutex::new(Vec::new()),
        }
    }

    fn opened(&self) -> Vec<String> {
        self.opened.lock().expect("not poisoned").clone()
    }

    fn trashed(&self) -> Vec<String> {
        self.trashed.lock().expect("not poisoned").clone()
    }
}

impl Env for OpEnv {
    fn os(&self) -> Os {
        Os::current()
    }

    fn caps(&self) -> PlatformCaps {
        PlatformCaps::empty()
    }

    fn open_with(&self, path: &AbsPath) -> PlatformFuture<'_, Result<(), PubError>> {
        // The recorder's whole point: the test asserts the argv-equivalent (the path handed over) and no
        // `xdg-open` is ever spawned.
        self.opened
            .lock()
            .expect("not poisoned")
            .push(path.to_string_lossy());
        if self.no_opener {
            return ready(Err(PubError::Shell {
                cmd: "xdg-open".to_string(),
                code: None,
                stderr: "no opener found (simulated: open::commands() is empty)".to_string(),
            }));
        }
        ready(Ok(()))
    }

    fn copy_paths(&self, _items: &[CopyItem]) -> PlatformFuture<'_, Result<CopyReport, PubError>> {
        ready(Err(PubError::Unsupported {
            what: "the test env does not copy (ops::copy does)",
        }))
    }

    fn paste_link(&self, items: &[LinkItem]) -> PlatformFuture<'_, Result<(), PubError>> {
        #[cfg(unix)]
        {
            for item in items {
                if let Err(e) =
                    std::os::unix::fs::symlink(item.target.as_path(), item.link.as_path())
                {
                    return ready(Err(PubError::Io {
                        path: item.link.to_string_lossy(),
                        io: Arc::new(e),
                    }));
                }
            }
            ready(Ok(()))
        }
        #[cfg(not(unix))]
        {
            let _ = items;
            ready(Err(PubError::SymlinkUnsupported))
        }
    }

    fn supports_symlink(&self) -> Result<bool, PubError> {
        Ok(true)
    }

    fn chmod(
        &self,
        targets: &[AbsPath],
        mode: ModeBits,
    ) -> PlatformFuture<'_, Result<(), PubError>> {
        if self.chmod_refused {
            return ready(Err(PubError::Unsupported {
                what: "chmod (simulated: this platform has no st_mode)",
            }));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for target in targets {
                if let Err(e) = fs::set_permissions(
                    target.as_path(),
                    fs::Permissions::from_mode(mode.bits_octal()),
                ) {
                    return ready(Err(PubError::Io {
                        path: target.to_string_lossy(),
                        io: Arc::new(e),
                    }));
                }
            }
            ready(Ok(()))
        }
        #[cfg(not(unix))]
        {
            let _ = (targets, mode);
            ready(Err(PubError::Unsupported {
                what: "chmod (no st_mode on this platform)",
            }))
        }
    }

    fn can_trash(&self, _path: &AbsPath) -> bool {
        self.has_trash
    }

    fn trash(&self, path: &AbsPath) -> Result<bool, PubError> {
        if self.broken {
            return Err(PubError::Io {
                path: path.to_string_lossy(),
                io: Arc::new(std::io::Error::other("simulated trash failure")),
            });
        }
        if self.refuses {
            return Ok(false);
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        fs::rename(path.as_path(), self.bin.join(&name))
            .unwrap_or_else(|e| panic!("the fake bin cannot take {}: {e}", path.to_string_lossy()));
        self.trashed.lock().expect("not poisoned").push(name);
        Ok(true)
    }

    fn create_shortcut(&self, _shortcut: &Shortcut) -> Result<(), PubError> {
        Err(PubError::Unsupported {
            what: "shortcuts (not in plan §5.3)",
        })
    }

    fn disk_space(&self, _path: &AbsPath) -> Result<DiskSpace, PubError> {
        Ok(DiskSpace::default())
    }

    fn mount_entries(&self) -> Result<MountTable, PubError> {
        Err(PubError::Unsupported {
            what: "mounts (not needed here)",
        })
    }

    fn exec_program(&self, req: ExecRequest) -> PlatformFuture<'_, Result<ExecOutput, PubError>> {
        let _ = req;
        ready(Err(PubError::Unsupported {
            what: "spawns (the test env never starts a process)",
        }))
    }
}

/// As [`open_session`], over a caller-supplied platform.
fn open_session_env(dir: &Path, env: Arc<dyn Env>) -> Session {
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

/// Drive any job to its terminal event. `None` when it failed or was cancelled — which some of these
/// tests want, and which is why this is not `wait_paste` again.
fn pump<T: Clone>(
    session: &mut Session,
    job: JobId,
    pick: impl Fn(&JobPayload) -> Option<T>,
) -> (Option<T>, Vec<EngineEvent>) {
    let mut events = Vec::new();
    let mut done: Option<T> = None;
    for round in 0..160 {
        events.extend(session.poll());
        if done.is_none() {
            done = events.iter().find_map(|ev| match ev {
                EngineEvent::JobFinished { id, payload, .. } if *id == job => pick(payload),
                _ => None,
            });
        }
        let terminal = events
            .iter()
            .any(|ev| matches!(ev, EngineEvent::JobFinished { id, .. } if *id == job));
        if terminal && round > 8 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    events.extend(session.dirs().flush_invalidations());
    (done, events)
}

fn running<T: std::fmt::Debug>(receipt: Result<Receipt<T>, PubError>) -> JobId {
    match receipt {
        Ok(Receipt::Running { job, .. }) => job,
        other => panic!("expected a running job: {other:?}"),
    }
}

/// The `(action, sources, dest, name)` roles of one written record, against the capture's record of the
/// same action byte.
fn assert_record_roles(_session: &Session, root: &Path, id: i64, want: ActionType) {
    let bytes = fs::read(root.join("action.log")).expect("the log exists");
    let mut pos = 0usize;
    let mut found = None;
    while pos < bytes.len() {
        let d = codec::decode_at(&bytes[pos..], pos as u64).expect("every frame decodes");
        assert!(d.crc_ok, "frame at {pos} has a bad CRC");
        assert_eq!(
            codec::encode_vec(&d.record).expect("re-encodes"),
            &bytes[pos..pos + d.consumed],
            "the frame the op wrote is byte-stable (house rule 8)"
        );
        if d.record.id == id {
            found = Some(d.record.clone());
        }
        pos += d.consumed;
    }
    let record = found.unwrap_or_else(|| panic!("record {id} is in the log: {bytes:?}"));
    assert_eq!(record.action, want);
    let golden = golden_json("log.json");
    let go = golden["records"]
        .as_array()
        .expect("records")
        .iter()
        .find(|r| r["action"].as_i64() == Some(i64::from(want.0)))
        .unwrap_or_else(|| panic!("the capture contains a {} record", want));
    assert_eq!(
        record.action_str(),
        go["action_str"].as_str().expect("action_str"),
        "the action name is Go's"
    );
    assert_eq!(
        record.is_reversible(),
        go["reversible"].as_bool().expect("reversible"),
        "reversibility is the byte `api/log.go:600-602` derives, per action"
    );
}

// ── delete ─────────────────────────────────────────────────────────────────────────────────────

/// Ladder rungs 1 and 3, plus the record: a root **with** a bin trashes, and the log says `delete`
/// with the Go field roles and `reversible:false` (golden records 7/8 — Go logged deletes, and
/// `log_revert.go` refuses to undo them).
#[test]
fn delete_trashes_on_a_root_that_has_a_bin() {
    let root = temp_dir("del-trash");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("bin dir");
    let env = Arc::new(OpEnv::new(&bin));
    let mut session = open_session_env(&root, Arc::clone(&env) as Arc<dyn Env>);
    let file = write(&root.join("tree/victim.txt"), b"gone");
    let dir = mkdir(&root.join("tree/sub"));
    let intent = Arc::new(RecordingIntent::new());
    session.set_intent(Arc::clone(&intent) as Arc<dyn IndexIntent>);

    let origin = session.next_origin();
    let job = match session.delete(
        &DeleteReq::to_trash(vec![file.clone(), dir.clone()]),
        origin,
    ) {
        Ok(Receipt::Running { job, .. }) => job,
        other => panic!("a delete is never synchronous: {other:?}"),
    };
    let (report, events) = pump(&mut session, job, |p| p.delete().cloned());
    let report = report.expect("the delete job finished with a report");

    assert_eq!(report.mode_used, DeleteMode::Trash);
    assert!(
        matches!(&report.outcomes[0], DeleteOutcome::Trashed { path } if *path == file),
        "{:?}",
        report.outcomes
    );
    assert_eq!(report.trashed().len(), 2, "both went to the bin");
    assert!(report.removed().is_empty(), "nothing was destroyed");
    assert!(report.failed().is_empty());
    assert!(!exists(&root.join("tree/victim.txt")));
    assert!(!exists(&root.join("tree/sub")));
    assert_eq!(
        env.trashed(),
        vec!["victim.txt".to_string(), "sub".to_string()],
        "the bin holds them"
    );

    // The Go-parity record: one `delete` for the call, sources = what actually went, empty dest/name,
    // and *not* reversible (`api/ops.go:378`, `api/log.go:600-602`).
    let id = report.log_id.expect("a record was appended");
    assert_record_roles(&session, &root, id, ActionType::DELETE);
    let record = session.log_record(id).expect("the record reads back");
    assert_eq!(record.dest, "", "Go appended none (`api/ops.go:378`)");
    assert_eq!(record.name, "");
    assert_eq!(record.sources.len(), 2);
    assert!(
        !record.is_reversible(),
        "a delete is recorded, never undone"
    );
    assert!(!report.incomplete(), "everything asked for happened");

    // Invalidation and the index, per affected directory.
    let parent = abs(&root.join("tree"));
    let changed = dir_changed(&events);
    assert!(
        changed
            .iter()
            .any(|(p, o)| *p == fskey(&parent) && *o == Some(origin)),
        "the parent that lost two children is invalidated with the caller's origin: {changed:?}"
    );
    let ops = intent.recorded();
    assert!(
        ops.contains(&IndexOp::Deleted { path: file.clone() }),
        "a file is one row: {ops:?}"
    );
    assert!(
        ops.contains(&IndexOp::Pruned { path: dir.clone() }),
        "a directory is a subtree: {ops:?}"
    );
    let _ = remove_tree(&root);
}

/// The whole of plan §6.2 **D6** and §9.1's measured trash facts, rung by rung.
#[test]
fn the_delete_ladder_only_destroys_what_the_user_agreed_to() {
    // Rung A: no bin at all (the `/media/…` volume case), no opt-in → refused, and the file survives.
    let root = temp_dir("del-no-bin");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("bin dir");
    let env = Arc::new(OpEnv {
        has_trash: false,
        ..OpEnv::new(&bin)
    });
    let mut session = open_session_env(&root, Arc::clone(&env) as Arc<dyn Env>);
    let file = write(&root.join("keep.txt"), b"still here");
    let job = running(session.delete(
        &DeleteReq::to_trash(vec![file.clone()]),
        session.next_origin(),
    ));
    let (report, _ev) = pump(&mut session, job, |p| p.delete().cloned());
    let report = report.expect("the job ran");
    assert!(
        matches!(&report.outcomes[0], DeleteOutcome::Failed { error, .. }
            if error.kind() == "permission_denied"),
        "a trash request the volume cannot honour is a refusal: {:?}",
        report.outcomes
    );
    assert!(exists(&root.join("keep.txt")), "and nothing was destroyed");
    assert!(
        report.log_entry().is_none(),
        "no record for work that did not run (plan F3)"
    );

    // Rung A′: the same volume, the D6 opt-in set → permanent, and the outcome *says* it was a fallback.
    let mut session = open_session_env(&root, Arc::clone(&env) as Arc<dyn Env>);
    let job = running(session.delete(
        &DeleteReq {
            sources: vec![file.clone()],
            mode: DeleteMode::Trash,
            permanent_fallback: true,
        },
        session.next_origin(),
    ));
    let (report, _ev) = pump(&mut session, job, |p| p.delete().cloned());
    let report = report.expect("the job ran");
    assert!(
        matches!(
            &report.outcomes[0],
            DeleteOutcome::Removed { fallback: true, .. }
        ),
        "the escalation is a value in the report: {:?}",
        report.outcomes
    );
    assert!(!exists(&root.join("keep.txt")));

    // Rung B: a bin that refuses *this* path (`trash::Error::TargetedRoot` → `Ok(false)`) with the opt-in.
    let root2 = temp_dir("del-refused");
    let bin2 = root2.join("bin");
    fs::create_dir_all(&bin2).expect("bin dir");
    let env2 = Arc::new(OpEnv {
        refuses: true,
        ..OpEnv::new(&bin2)
    });
    let mut session2 = open_session_env(&root2, Arc::clone(&env2) as Arc<dyn Env>);
    let anchored = write(&root2.join("root-item.txt"), b"x");
    let job = running(session2.delete(
        &DeleteReq::to_trash(vec![anchored.clone()]),
        session2.next_origin(),
    ));
    let (report, _ev) = pump(&mut session2, job, |p| p.delete().cloned());
    let report = report.expect("the job ran");
    assert!(
        matches!(&report.outcomes[0], DeleteOutcome::Failed { error, .. }
            if error.kind() == "permission_denied"),
        "a refused path without the opt-in is not a silent delete: {:?}",
        report.outcomes
    );
    assert!(exists(&root2.join("root-item.txt")));

    // Rung C: the bin *errored* while trying → a failure, never escalated, opt-in or not.
    let env3 = Arc::new(OpEnv {
        broken: true,
        ..OpEnv::new(&bin2)
    });
    let mut session3 = open_session_env(&root2, Arc::clone(&env3) as Arc<dyn Env>);
    let job = running(session3.delete(
        &DeleteReq {
            sources: vec![anchored.clone()],
            mode: DeleteMode::Trash,
            permanent_fallback: true,
        },
        session3.next_origin(),
    ));
    let (report, _ev) = pump(&mut session3, job, |p| p.delete().cloned());
    let report = report.expect("the job ran");
    assert!(
        matches!(&report.outcomes[0], DeleteOutcome::Failed { .. }),
        "{:?}",
        report.outcomes
    );
    assert!(
        exists(&root2.join("root-item.txt")),
        "a trash error never becomes a delete"
    );
    let _ = remove_tree(&root);
    let _ = remove_tree(&root2);
}

/// Go's own behaviour, through the *real* platform: one `ActionDelete` per call naming every path that
/// went, a directory and all of it (`api/ops.go:367-387`).
#[test]
fn a_permanent_delete_removes_a_tree_and_records_the_paths_that_went() {
    let root = temp_dir("del-permanent");
    let mut session = open_session(&root);
    let tree = root.join("tree");
    write(&tree.join("a.txt"), b"a");
    write(&tree.join("deep/b.txt"), b"b");
    let dir = abs(&tree);
    let ghost = write(&root.join("doomed.txt"), b"x");
    fs::remove_file(root.join("doomed.txt")).expect("removed by hand first");

    let origin = session.next_origin();
    let job = match session.delete(
        &DeleteReq::permanent(vec![dir.clone(), ghost.clone()]),
        origin,
    ) {
        Ok(Receipt::Running { job, .. }) => job,
        other => panic!("a delete is never synchronous: {other:?}"),
    };
    let (report, events) = pump(&mut session, job, |p| p.delete().cloned());
    let report = report.expect("the delete job finished with a report");
    assert_eq!(report.mode_used, DeleteMode::Permanent);
    assert_eq!(
        report.removed().len(),
        1,
        "one existed: {:?}",
        report.removed()
    );
    assert_eq!(
        report.removed()[0],
        (&dir, false),
        "asked for, not escalated"
    );
    assert_eq!(report.failed().len(), 1, "the other was already gone");
    assert_eq!(report.failed()[0].1.kind(), "not_found");
    assert!(!exists(&tree), "the whole tree went");
    let id = report.log_id.expect("a record");
    let record = session.log_record(id).expect("it reads back");
    assert_eq!(
        record.sources,
        vec![dir.to_string_lossy()],
        "the record names what actually disappeared (Go's comment at api/ops.go:376)"
    );
    assert!(
        dir_changed(&events)
            .iter()
            .any(|(p, o)| *o == Some(origin) && *p == fskey(&abs(&root))),
        "the directory that lost the tree is invalidated with the caller's origin: {:?}",
        dir_changed(&events)
    );
    let _ = remove_tree(&root);
}

// ── rename ─────────────────────────────────────────────────────────────────────────────────────

/// The three policies, on a name that is taken — the same question [`ConflictPolicy`] answers for a
/// paste, asked of a rename (plan §5.3's "one answer to 'the name is taken'").
#[test]
fn rename_answers_a_taken_name_by_policy() {
    let root = temp_dir("rename-policy");
    let session = open_session(&root);
    let taken = write(&root.join("tree/new-name.txt"), b"occupant");
    let _ = taken;

    // SkipExisting: the occupant stays, the rename is refused *for that item*, and nothing is logged.
    let src = write(&root.join("tree/old-name.txt"), b"payload");
    let err = session
        .rename(
            &RenameReq::one(src.clone(), "new-name.txt"),
            session.next_origin(),
        )
        .expect("the request itself is well formed");
    assert!(
        matches!(&err.outcomes[0], RenameOutcome::Failed { error, .. }
            if error.kind() == "already_exists"),
        "{:?}",
        err.outcomes
    );
    assert!(
        exists(&root.join("tree/old-name.txt")),
        "the source did not move"
    );
    assert_eq!(
        read(&root.join("tree/new-name.txt")),
        b"occupant",
        "and the occupant was not touched"
    );
    assert!(err.log_id.is_none(), "nothing renamed, nothing recorded");
    assert!(err.renamed().is_empty());

    // Overwrite: the occupant is replaced, and the record names the *source*, Go-style.
    let mut req = RenameReq::one(src.clone(), "new-name.txt");
    req.policy = ConflictPolicy::Overwrite;
    let report = session
        .rename(&req, session.next_origin())
        .expect("overwrite renames");
    let pairs = report.renamed();
    assert_eq!(pairs.len(), 1, "{:?}", report.outcomes);
    assert_eq!(
        pairs[0].1.to_string_lossy(),
        root.join("tree/new-name.txt").to_string_lossy()
    );
    assert_eq!(read(&root.join("tree/new-name.txt")), b"payload");
    assert!(!exists(&root.join("tree/old-name.txt")));

    // Rename: both survive, under `name (1).txt`.
    let third = write(&root.join("tree/third.txt"), b"third");
    let mut req = RenameReq::one(third.clone(), "new-name.txt");
    req.policy = ConflictPolicy::Rename;
    let report = session
        .rename(&req, session.next_origin())
        .expect("the rename policy invents a name");
    let (_from, to) = report.renamed()[0];
    assert_eq!(
        to.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .as_deref(),
        Some("new-name (1).txt")
    );
    assert!(
        exists(&root.join("tree/new-name.txt")),
        "the previous winner stays"
    );

    // The single-rename record: `Append(ActionRename, [src], "", newName)` (`api/ops.go:414`), and the
    // capture's roles for action byte 5 (`log.json` record 2).
    let id = report.log_id.expect("a record was appended");
    assert_record_roles(&session, &root, id, ActionType::RENAME);
    let record = session.log_record(id).expect("it reads back");
    assert_eq!(
        record.sources,
        vec![third.to_string_lossy()],
        "sources are the *old* paths"
    );
    assert_eq!(record.dest, "", "Go logged no dest for a rename");
    assert_eq!(record.name, "new-name (1).txt");
    assert!(record.is_reversible(), "`rename` is one of the five");
    let _ = remove_tree(&root);
}

/// The name is a *name*, not a path: `..`, `.`, `/` and a glob are all refused before anything is
/// renamed, and `foo..bar` — the substring Go's `validateDest` refused (ledger #14) — is allowed.
#[test]
fn a_rename_name_is_one_component_and_nothing_else() {
    assert!(
        rename::validate_rename_name("foo..bar").is_ok(),
        "the ledger-#14 false positive is gone"
    );
    assert!(rename::validate_rename_name("a b.txt").is_ok());
    for bad in ["", ".", "..", "a/b", "x*", "?y"] {
        let err = rename::validate_rename_name(bad).expect_err("`{bad}` is not one component");
        assert_eq!(err.kind(), "invalid_path", "`{bad}`: {err}");
    }

    let root = temp_dir("rename-name");
    let session = open_session(&root);
    let file = write(&root.join("tree/keep.txt"), b"payload");
    let mut escape = RenameReq::one(file.clone(), "../../etc/passwd");
    escape.target = RenameTarget::One("../../etc/passwd".to_string());
    let report = session
        .rename(&escape, session.next_origin())
        .expect("the request is a rename, refused per item");
    assert_eq!(report.renamed().len(), 0, "{:?}", report.outcomes);
    assert_eq!(
        report.failed()[0].1.kind(),
        "invalid_path",
        "the escape attempt is a typed rejection"
    );
    assert!(
        exists(&root.join("tree/keep.txt")),
        "and the file is where it was"
    );
    assert_eq!(read(&root.join("tree/keep.txt")), b"payload");
    let _ = remove_tree(&root);
}

/// Go's batch quirk, kept on purpose (`api/ops.go:466-474`): renaming a parent rewrites the paths of the
/// sources that lived under it, or the batch renames children that are no longer where the request said
/// they were. Plan §5.3's done-when row, and the record keeps Go's JSON-array `name`
/// (`tests/golden/log.json` record 14).
#[test]
fn a_batch_rename_renames_a_parent_and_its_children_and_logs_the_name_array() {
    let root = temp_dir("rename-batch");
    let mut session = open_session(&root);
    let parent = mkdir(&root.join("tree/parent"));
    let child = write(&root.join("tree/parent/child.txt"), b"c");
    let sibling = write(&root.join("tree/plain.txt"), b"s");

    let report = session
        .rename(
            &RenameReq::batch(
                vec![parent.clone(), child.clone(), sibling.clone()],
                vec![
                    "PARENT".to_string(),
                    "kid.txt".to_string(),
                    "plain-renamed.txt".to_string(),
                ],
            ),
            session.next_origin(),
        )
        .expect("a batch rename is one call");
    assert_eq!(report.renamed().len(), 3, "{:?}", report.outcomes);
    assert!(
        exists(&root.join("tree/PARENT/kid.txt")),
        "the child followed its parent: {:?}",
        report.outcomes
    );
    assert!(exists(&root.join("tree/plain-renamed.txt")));
    assert!(!exists(&root.join("tree/parent")));

    let id = report.log_id.expect("one record for the batch");
    assert_record_roles(&session, &root, id, ActionType::RENAME);
    let record = session.log_record(id).expect("it reads back");
    let names: Vec<String> = serde_json::from_str(&record.name).expect("`name` is Go's JSON array");
    assert_eq!(
        names,
        vec!["PARENT", "kid.txt", "plain-renamed.txt"],
        "the array is the names that applied, in order — what `revertRename` parses back"
    );
    assert_eq!(
        record.sources,
        vec![
            parent.to_string_lossy(),
            // Go's `renamedSources` collects `src` *after* the rewrite loop has relaid the later entries
            // (`api/ops.go:466-474`), so the child is recorded under its new parent — which is what makes
            // `revertRename`'s reverse pass land it back inside `/tree/parent`.
            abs(&root.join("tree/PARENT/child.txt")).to_string_lossy(),
            sibling.to_string_lossy()
        ],
        "the sources are the paths as renamed, exactly as Go collected them"
    );

    // …which is exactly what the revert does, in reverse order (plan §7 row 9's "revert of each
    // reversible action type passes", for `rename`).
    let origin = session.next_origin();
    let job = running(session.revert(id, origin));
    let (done, _ev) = pump(&mut session, job, |p| p.revert().cloned());
    let done = done.expect("the batch revert finished");
    assert_eq!(done.restored.len(), 3, "{done:?}");
    assert!(
        exists(&root.join("tree/parent/child.txt")),
        "back where they started"
    );
    assert!(exists(&root.join("tree/plain.txt")));
    assert_eq!(
        session.log_record(id).expect("the record").status,
        LogStatus::REVERTED,
        "the frame is patched in place — a revert appends nothing (the capture's 19-record log)"
    );
    let _ = remove_tree(&root);
}

// ── mkdir ──────────────────────────────────────────────────────────────────────────────────────

/// Go's `handleMkdir` (`api/ops.go:488-521`): the row it returned, and the record it appended —
/// `Append(ActionMkdir, [], dest, name)`, which is the *shape of golden record 1*, the one the capture
/// itself reverted.
#[test]
fn mkdir_makes_the_dir_returns_the_row_and_collides_honestly() {
    let root = temp_dir("mkdir");
    let mut session = open_session(&root);
    let tree = abs(&root.join("tree"));
    fs::create_dir_all(root.join("tree")).expect("tree");
    let intent = Arc::new(RecordingIntent::new());
    session.set_intent(Arc::clone(&intent) as Arc<dyn IndexIntent>);

    let origin = session.next_origin();
    let entry = session
        .mkdir(
            &MkdirReq {
                dest: tree.clone(),
                name: "made-dir".to_string(),
                mode: DEFAULT_DIR_MODE,
            },
            origin,
        )
        .expect("the directory is made");
    assert_eq!(entry.name, "made-dir");
    assert!(entry.is_dir, "Go's `isDir`");
    let mode_text = entry.mode.clone();
    assert!(
        entry.mode.starts_with('d'),
        "Go's FileMode.String(): {mode_text}"
    );
    let made = root.join("tree/made-dir");
    #[cfg(unix)]
    assert_eq!(
        mode_of(&made) & 0o700,
        0o700,
        "0755 & ~umask — the umask applies here exactly as it did to Go's `os.Mkdir(_, 0755)`"
    );
    assert!(entry.size >= 0, "a size was reported: {}", entry.size);

    let parent = abs(&root.join("tree"));
    let _ = session.poll();
    let changed = dir_changed(&session.dirs().flush_invalidations());
    assert!(
        changed
            .iter()
            .any(|(p, o)| *p == fskey(&parent) && *o == Some(origin)),
        "the parent listing is stale now: {changed:?}"
    );
    assert!(
        intent
            .recorded()
            .contains(&IndexOp::Created { path: abs(&made) })
    );

    let golden = golden_json("log.json");
    let go = golden["records"]
        .as_array()
        .expect("records")
        .iter()
        .find(|r| r["action"].as_i64() == Some(i64::from(ActionType::MKDIR.0)))
        .expect("the capture has a mkdir record");
    let record = session
        .log_recent(None)
        .expect("the log")
        .records
        .into_iter()
        .next()
        .expect("one record");
    assert_eq!(record.action, ActionType::MKDIR);
    assert!(
        record.sources.is_empty(),
        "an mkdir logs no sources: {:?}",
        record.sources
    );
    assert_eq!(
        go["sources"].as_array().expect("sources").len(),
        0,
        "and the capture agrees (golden record 1: `[]`, not absent)"
    );
    assert_eq!(
        record.dest,
        tree.to_string_lossy(),
        "`dest` is the parent, `name` the new directory (`api/ops.go:501`)"
    );
    assert_eq!(record.name, "made-dir");
    assert_eq!(
        record.is_reversible(),
        go["reversible"].as_bool().expect("reversible"),
        "and mkdir is one of the five reversible types"
    );
    assert!(record.is_reversible());

    // The collision Go reported as a raw `file exists` 500 is a typed `AlreadyExists` here, and an
    // illegal name never reaches the filesystem at all.
    let again = session
        .mkdir(
            &MkdirReq::new(tree.clone(), "made-dir"),
            session.next_origin(),
        )
        .expect_err("a taken name is a typed collision");
    assert_eq!(again.kind(), "already_exists");
    let escape = session
        .mkdir(
            &MkdirReq::new(tree.clone(), "../escape"),
            session.next_origin(),
        )
        .expect_err("`..` is not a directory name");
    assert_eq!(escape.kind(), "invalid_path");
    assert!(
        !root.join("escape").exists(),
        "and nothing was created outside the tree"
    );
    let _ = remove_tree(&root);
}

// ── chmod ──────────────────────────────────────────────────────────────────────────────────────

/// The octal↔symbolic pair the Go UI shows (`fs.FileMode`'s permission run vs. the text its dialog
/// sends), the mode actually landing on the file, and the record carrying **the text as typed** — golden
/// records 9 and 10 of `log.json` are `0o600`/`0o644` spelled `"600"` and `"0644"`, which only a
/// verbatim-text record can reproduce byte for byte.
#[test]
fn chmod_round_trips_octal_to_symbolic_and_logs_the_text_as_typed() {
    // The parse/format pair first, with no filesystem in sight.
    let bits = mode::parse_octal("0644").expect("four digits");
    assert_eq!(bits.bits_octal(), 0o644);
    assert_eq!(
        bits.to_string(),
        "rw-r--r--",
        "the nine glyphs Go's `FileMode.String()` ends with"
    );
    assert_eq!(
        chmod::octal_text(bits),
        "0644",
        "and the canonical text back"
    );
    assert_eq!(
        mode::parse_octal(&chmod::octal_text(bits))
            .expect("stable")
            .bits_octal(),
        0o644,
        "octal → text → octal is the identity"
    );
    assert_eq!(
        mode::parse_octal("600")
            .expect("three digits, as the capture typed")
            .bits_octal(),
        0o600
    );
    for bad in ["", "8", "888", "0o755", "77777", "64x"] {
        assert_eq!(
            mode::parse_octal(bad)
                .expect_err("`{bad}` is not a mode")
                .kind(),
            "invalid_mode",
            "`{bad}`: Go's `Invalid permission mode` (`err-chmod-octaltext.json`)"
        );
    }

    let root = temp_dir("chmod");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("bin dir");
    let env = Arc::new(OpEnv::new(&bin));
    let mut session = open_session_env(&root, Arc::clone(&env) as Arc<dyn Env>);
    let file = write(&root.join("tree/perm.txt"), b"x");
    set_mode(&root.join("tree/perm.txt"), 0o644);
    let intent = Arc::new(RecordingIntent::new());
    session.set_intent(Arc::clone(&intent) as Arc<dyn IndexIntent>);

    let origin = session.next_origin();
    let mut non_recursive = ChmodReq::parse(vec![file.clone()], "600").expect("a mode");
    assert_eq!(
        non_recursive.recursive,
        Recursive::No,
        "Go's chmod was not recursive"
    );
    non_recursive.recursive = Recursive::default();
    assert_eq!(non_recursive.recursive, Recursive::No);
    let report = session
        .chmod(&non_recursive, origin)
        .expect("the platform applies the mode");
    assert_eq!(
        mode_of(&root.join("tree/perm.txt")),
        0o600,
        "it landed on the file"
    );
    assert_eq!(
        report.text, "600",
        "the text the record carries is what the user typed"
    );
    let id = report.log_id.expect("a record was appended");
    assert_record_roles(&session, &root, id, ActionType::CHMOD);
    let record = session.log_record(id).expect("it reads back");
    assert_eq!(record.name, "600", "verbatim, as golden record 9 has it");
    assert_eq!(record.dest, "", "Go logged no dest for a chmod");
    assert_eq!(record.sources, vec![file.to_string_lossy()]);
    assert!(
        !record.is_reversible(),
        "chmod stores no previous mode, so it cannot be undone"
    );

    // The same bits spelled the capture's other way, which is golden record 10.
    let second = session
        .chmod(
            &ChmodReq::parse(vec![file.clone()], "0644").expect("a mode"),
            session.next_origin(),
        )
        .expect("the platform applies the mode");
    assert_eq!(mode_of(&root.join("tree/perm.txt")), 0o644);
    assert_eq!(
        session
            .log_record(second.log_id.expect("a record"))
            .expect("it reads back")
            .name,
        "0644",
        "two records, two spellings, one pair of bits — the capture's own pair"
    );

    // The intent and the invalidation the listing needs (the mode is a listed column).
    let ops = intent.recorded();
    assert!(
        ops.contains(&IndexOp::AttributesChanged { path: file.clone() }),
        "the mode change is noted, as the op step 12 may drop: {ops:?}"
    );
    let _ = session.poll();
    let changed = dir_changed(&session.dirs().flush_invalidations());
    assert!(
        changed
            .iter()
            .any(|(p, _)| *p == fskey(&abs(&root.join("tree")))),
        "the directory whose row changed is invalidated: {changed:?}"
    );

    // A read-only *file* is not a read-only *directory*: `chmod(2)` needs ownership, not write
    // permission, so the Go UI's "clear the read-only box" flow must work on a `0000` file — and the
    // platform's `is_writable` answer must not talk us out of it. (Nothing here self-skips: unlike a
    // read-only *mount*, this case is producible for real, and it is the one the guard could get wrong.)
    set_mode(&root.join("tree/perm.txt"), 0o000);
    let reread = session
        .chmod(
            &ChmodReq::new(vec![file.clone()], ModeBits::from_bits_retain(0o644)),
            session.next_origin(),
        )
        .expect("a 0000 file owned by us is chmod-able");
    assert_eq!(
        mode_of(&root.join("tree/perm.txt")),
        0o644,
        "and it changed: {:?}",
        reread.applied
    );

    // The two failure lanes: the path is gone (`io_error`, naming it), and the platform has no
    // `st_mode` at all (`unsupported`, the stub's answer — plan §5.2's "typed refusal, never a silent
    // success"). Both are the operation's own `Err`, because there is nothing partial about them.
    let missing = abs(&root.join("tree/nope.txt"));
    let err = session
        .chmod(
            &ChmodReq::new(vec![missing], ModeBits::EXECUTABLE),
            session.next_origin(),
        )
        .expect_err("chmod of a path that is not there fails");
    assert_eq!(err.kind(), "io_error", "{err}");
    assert!(
        err.user_message().contains("nope.txt"),
        "and it names the path: {err}"
    );

    let refused = Arc::new(OpEnv {
        chmod_refused: true,
        ..OpEnv::new(&bin)
    });
    let session2 = open_session_env(&root, refused as Arc<dyn Env>);
    let err = session2
        .chmod(
            &ChmodReq::new(vec![file.clone()], ModeBits::EXECUTABLE),
            session2.next_origin(),
        )
        .expect_err("a platform without chmod(2) says so");
    assert_eq!(err.kind(), "unsupported");
    assert!(!exists(&root.join("tree/perm.txt.bak")));
    let _ = remove_tree(&root);
}

// ── open ───────────────────────────────────────────────────────────────────────────────────────

/// The launcher seam, recorded rather than run: files go to the desktop, **directories are reported as
/// navigation** (Go ran `xdg-open` on those too, `api/ops.go:122-143`), a missing opener is a typed
/// user-facing error, and nothing about an `open` touches the log or a listing.
#[test]
fn open_launches_files_routes_directories_and_reports_a_missing_opener() {
    let root = temp_dir("open");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("bin dir");
    let env = Arc::new(OpEnv::new(&bin));
    let mut session = open_session_env(&root, Arc::clone(&env) as Arc<dyn Env>);
    let file = write(&root.join("tree/doc.txt"), b"x");
    let dir = mkdir(&root.join("tree/sub"));
    let link = {
        let target = root.join("tree/doc.txt");
        let link = root.join("tree/link.txt");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        abs(&link)
    };

    let report = session
        .open_paths(&[file.clone(), dir.clone(), link.clone()])
        .expect("the batch is accepted");
    assert_eq!(
        report.launched,
        vec![file.clone(), link.clone()],
        "a link is a file"
    );
    assert_eq!(
        report.navigated,
        vec![dir.clone()],
        "and a directory is a route, not a launch"
    );
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(
        env.opened(),
        vec![file.to_string_lossy(), link.to_string_lossy()],
        "the directory was never handed to the launcher — that is the navigation the UI must act on"
    );
    assert!(matches!(open::route(&dir), OpenRoute::Navigate(p) if p == dir));
    assert!(matches!(open::route(&file), OpenRoute::Launch(p) if p == file));

    // Nothing changed, so nothing is logged and nothing is invalidated.
    assert!(
        session
            .log_recent(None)
            .expect("the log reads")
            .records
            .is_empty(),
        "open writes no record"
    );
    let _ = session.poll();
    assert!(dir_changed(&session.dirs().flush_invalidations()).is_empty());

    // `reveal` is the parent directory, through the same seam (plan §5.3: Go had no reveal at all).
    session.reveal(&file).expect("the parent opens");
    assert_eq!(
        env.opened().last().map(String::as_str),
        Some(abs(&root.join("tree")).to_string_lossy().as_str())
    );

    // The measured `open` 5.4 fact: no `open::Error`, so a host with no opener is detected up front and
    // answered with a typed, installable-something error rather than a panic (`platform::linux::open_with`
    // does exactly this when `open::commands()` is empty).
    let no_opener = Arc::new(OpEnv {
        no_opener: true,
        ..OpEnv::new(&bin)
    });
    let session2 = open_session_env(&root, no_opener as Arc<dyn Env>);
    let report = session2
        .open_paths(std::slice::from_ref(&file))
        .expect("a per-path failure is not the call's error");
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert_eq!(
        report.failed[0].1.kind(),
        "shell_error",
        "the launcher's own failure, typed"
    );
    assert!(
        !report.failed[0].1.user_message().is_empty(),
        "and it says something usable"
    );
    assert!(report.launched.is_empty());

    assert_eq!(
        session.open_paths(&[]).expect_err("nothing to open").kind(),
        "empty",
        "Go's `No files specified to open` (`api/ops.go:124`)"
    );
    let _ = remove_tree(&root);
}

// ── revert (`api/log_revert.go`) ───────────────────────────────────────────────────────────────

/// Reverting a **copy** removes the copies and leaves the sources — Go's `revertPasteCopy`, whose
/// "already gone is success" is kept, and whose record flagging is the capture's own single revert
/// (`action.log` record 1: patched in place, nothing appended).
#[test]
fn reverting_a_copy_removes_the_copies_and_leaves_the_sources() {
    let root = temp_dir("revert-copy");
    let mut session = open_session(&root);
    let src = write(&root.join("src/one.txt"), b"payload");
    let dest = mkdir(&root.join("dest"));
    let (job, _origin) = started(
        &mut session,
        &paste_req(vec![src.clone()], dest.clone(), PasteMode::Copy),
    );
    let (report, _ev) = wait_paste(&mut session, job);
    let id = report.log_id.expect("the paste logged itself");
    assert!(exists(&root.join("dest/one.txt")));

    let job = running(session.revert(id, session.next_origin()));
    let (done, events) = pump(&mut session, job, |p| p.revert().cloned());
    let done: RevertReport = done.expect("the revert finished");
    assert_eq!(done.action, ActionType::PASTE_COPY);
    assert_eq!(
        done.removed,
        vec![abs(&root.join("dest/one.txt"))],
        "{done:?}"
    );
    assert!(done.restored.is_empty(), "a copy has nothing to put back");
    assert!(!exists(&root.join("dest/one.txt")), "the copy is gone");
    assert_eq!(
        read(&root.join("src/one.txt")),
        b"payload",
        "the source never was touched"
    );
    assert_eq!(
        session.log_record(id).expect("the record").status,
        LogStatus::REVERTED
    );
    assert_eq!(
        session.log_recent(None).expect("the log").records.len(),
        1,
        "a revert appends no frame — the capture's log stays at its original length"
    );
    assert!(
        dir_changed(&events).iter().any(|(p, _)| *p == fskey(&dest)),
        "the directory the copy was removed from is invalidated: {:?}",
        dir_changed(&events)
    );
    // Idempotent second attempt: refused for the *status*, not for the missing file.
    let err = session
        .revert(id, session.next_origin())
        .expect_err("an already-reverted record is refused");
    assert_eq!(err.kind(), "unsupported");
    let _ = remove_tree(&root);
}

/// Reverting a **move** puts the file back where it came from, through the same `move_item` the paste
/// used — and unlike the copy revert it *refuses* when the placed file is missing (`log_revert.go:88`).
#[test]
fn reverting_a_move_puts_the_source_back_where_it_came_from() {
    let root = temp_dir("revert-move");
    let mut session = open_session(&root);
    let src = write(&root.join("src/victim.txt"), b"payload");
    let dest = mkdir(&root.join("dest"));
    let (job, _origin) = started(
        &mut session,
        &paste_req(vec![src.clone()], dest.clone(), PasteMode::Cut),
    );
    let (report, _ev) = wait_paste(&mut session, job);
    assert!(
        !exists(&root.join("src/victim.txt")),
        "the cut moved it away"
    );
    let id = report.log_id.expect("the move logged itself");

    let job = running(session.revert(id, session.next_origin()));
    let (done, _ev) = pump(&mut session, job, |p| p.revert().cloned());
    let done = done.expect("the revert finished");
    assert_eq!(done.restored.len(), 1, "{done:?}");
    assert_eq!(done.restored[0].1, src, "back to the recorded source path");
    assert_eq!(read(&root.join("src/victim.txt")), b"payload");
    assert!(!exists(&root.join("dest/victim.txt")));
    assert_eq!(
        session.log_record(id).expect("the record").status,
        LogStatus::REVERTED
    );

    // Go's non-idempotence for this type: with the placed file gone, the revert refuses rather than
    // marking an undone move that left the user with nothing.
    fs::remove_file(root.join("src/victim.txt")).expect("moved away by hand");
    let mut fresh = open_session_env(&root, zenman_core::platform::default_env());
    let id2 = {
        let other = write(&root.join("src/other.txt"), b"o");
        let (job, _o) = started(
            &mut fresh,
            &paste_req(vec![other], dest.clone(), PasteMode::Cut),
        );
        let (r, _e) = wait_paste(&mut fresh, job);
        r.log_id.expect("logged")
    };
    fs::remove_file(root.join("dest/other.txt")).expect("the placed copy is gone");
    let job = running(fresh.revert(id2, fresh.next_origin()));
    let (done, _ev) = pump(&mut fresh, job, |p| p.revert().cloned());
    assert!(done.is_none(), "a move revert does not pretend: {done:?}");
    assert_ne!(
        fresh.log_record(id2).expect("the record").status,
        LogStatus::REVERTED,
        "and the record is not flagged when nothing was undone"
    );
    let _ = remove_tree(&root);
}

/// Reverting a **link** unlinks the link, never the target — Go's deliberate `os.Remove` and not
/// `RemoveAll` (`api/log_revert.go:31-45`).
#[test]
fn reverting_a_link_unlinks_the_link_and_never_the_target() {
    let root = temp_dir("revert-link");
    let mut session = open_session(&root);
    let target = write(&root.join("src/target.txt"), b"payload");
    let dest = mkdir(&root.join("dest"));
    let (job, _origin) = started(
        &mut session,
        &paste_req(vec![target.clone()], dest.clone(), PasteMode::Link),
    );
    let (report, _ev) = wait_paste(&mut session, job);
    let link = root.join("dest/target.txt");
    if !report.placed_any() {
        // The host refuses symlinks (the same self-skip `paste_link_creates_links…` uses): nothing to
        // revert, and asserting a link that was never made would prove nothing.
        println!(
            "SKIPPED: this platform refused the link paste: {:?}",
            report.outcomes
        );
        let _ = remove_tree(&root);
        return;
    }
    assert!(
        fs::symlink_metadata(&link)
            .expect("the link")
            .file_type()
            .is_symlink()
    );
    let id = report.log_id.expect("the link logged itself");

    let job = running(session.revert(id, session.next_origin()));
    let (done, _ev) = pump(&mut session, job, |p| p.revert().cloned());
    let done = done.expect("the revert finished");
    assert_eq!(done.removed, vec![abs(&link)], "the link went");
    assert!(!exists(&link), "and it is gone");
    assert!(
        fs::symlink_metadata(&link).is_err(),
        "no dangling link left behind"
    );
    assert_eq!(
        read(&root.join("src/target.txt")),
        b"payload",
        "the target survived — `os.Remove`, not `RemoveAll`"
    );
    assert_eq!(
        session.log_record(id).expect("the record").status,
        LogStatus::REVERTED
    );
    let _ = remove_tree(&root);
}

/// Reverting an **`mkdir`** removes the directory Go made — but only while it is still empty, which is
/// Go's data-loss guard (`api/log_revert.go:146-162`) and the reason this type is reversible at all.
#[test]
fn reverting_an_empty_mkdir_removes_it_and_a_full_one_refuses() {
    let root = temp_dir("revert-mkdir");
    let mut session = open_session(&root);
    let tree = mkdir(&root.join("tree"));
    let empty = session
        .mkdir(
            &MkdirReq::new(tree.clone(), "empty-dir"),
            session.next_origin(),
        )
        .expect("made");
    let full = session
        .mkdir(
            &MkdirReq::new(tree.clone(), "full-dir"),
            session.next_origin(),
        )
        .expect("made");
    write(
        &root.join("tree/full-dir/inside.txt"),
        b"the user put this here",
    );
    let records = session.log_recent(None).expect("the log").records;
    let empty_id = records
        .iter()
        .find(|r| r.name == empty.name)
        .expect("the mkdir record")
        .id;
    let full_record = records
        .iter()
        .find(|r| r.name == full.name)
        .expect("the other mkdir record")
        .clone();

    let job = running(session.revert(empty_id, session.next_origin()));
    let (done, _ev) = pump(&mut session, job, |p| p.revert().cloned());
    let done = done.expect("the revert finished");
    assert_eq!(done.removed, vec![abs(&root.join("tree/empty-dir"))]);
    assert!(!root.join("tree/empty-dir").exists());
    assert!(
        root.join("tree/full-dir").exists(),
        "and the untouched one is untouched"
    );

    // The non-empty case, at the ops level so the refusal is the *revert's* answer and not a job's:
    // Go names the item count, we name the path (`CoreError::AlreadyExists`).
    let cancel = zenman_core::jobs::CancelToken::new();
    let err = revert_ops::revert(
        zenman_core::platform::default_env().as_ref(),
        &OsMoveFs,
        &full_record,
        &cancel,
        &zenman_core::ops::NoProgress,
    )
    .expect_err("a directory someone filled is not removed");
    assert_eq!(err.kind(), "already_exists", "{err}");
    assert!(
        root.join("tree/full-dir/inside.txt").exists(),
        "nothing inside was lost"
    );
    assert_eq!(
        session
            .log_record(full_record.id)
            .expect("the record")
            .status,
        LogStatus::DONE,
        "and the record is still un-reverted"
    );

    // Go's idempotence for this type: a directory someone else already removed is a success, not an
    // error (`log_revert.go:152-154`).
    let gone = session
        .mkdir(
            &MkdirReq::new(tree.clone(), "gone-dir"),
            session.next_origin(),
        )
        .expect("made");
    let gone_id = session
        .log_recent(None)
        .expect("the log")
        .records
        .iter()
        .find(|r| r.name == gone.name)
        .expect("its record")
        .id;
    fs::remove_dir(root.join("tree/gone-dir")).expect("removed by hand");
    let mut empty_record = session.log_record(gone_id).expect("the record");
    empty_record.status = LogStatus::DONE; // pretend it was never flagged, to exercise the walk
    revert_ops::revert(
        zenman_core::platform::default_env().as_ref(),
        &OsMoveFs,
        &empty_record,
        &cancel,
        &zenman_core::ops::NoProgress,
    )
    .expect("already gone is success");
    let _ = remove_tree(&root);
}

/// The coverage matrix: `isReversible` (`api/log.go:600-602`) is the whole of the admission rule, and
/// every rejection says so **before** a path is touched.
#[test]
fn exactly_the_five_action_types_go_admits_are_reversible() {
    let root = temp_dir("revert-matrix");
    let mut session = open_session(&root);
    let tree = mkdir(&root.join("tree"));
    // One record of each of the eight types, all real: the five by doing the actions, the three by
    // letting the ops that log them run.
    let file = write(&root.join("tree/one.txt"), b"x");
    session
        .clipboard_store(&ClipboardStore {
            op: ClipOp::Copy,
            sources: vec![file.clone()],
            scope: ClipScope::Selected,
        })
        .expect("a copy logs `copy` (type 1)");
    let dest = mkdir(&root.join("dest"));
    let (job, _o) = started(
        &mut session,
        &paste_req(vec![file.clone()], dest.clone(), PasteMode::Copy),
    );
    let (_copy_report, _e) = wait_paste(&mut session, job);
    // A *different* file: the copy above already put `one.txt` in `dest`, and a cut that places nothing
    // appends nothing (plan F3's rule, which is what makes a record truthful).
    let victim = write(&root.join("tree/victim.txt"), b"v");
    let (job, _o) = started(
        &mut session,
        &paste_req(vec![victim.clone()], dest.clone(), PasteMode::Cut),
    );
    let (move_report, _e) = wait_paste(&mut session, job);
    assert!(move_report.placed_any(), "{:?}", move_report.outcomes);
    session
        .rename(
            &RenameReq::one(abs(&root.join("dest/one.txt")), "renamed.txt"),
            session.next_origin(),
        )
        .expect("renamed");
    session
        .mkdir(&MkdirReq::new(tree.clone(), "made"), session.next_origin())
        .expect("made");
    session
        .chmod(
            &ChmodReq::parse(vec![abs(&root.join("dest/renamed.txt"))], "600").expect("a mode"),
            session.next_origin(),
        )
        .expect("chmodded");
    let (job, _o) = started(
        &mut session,
        &paste_req(vec![file.clone()], dest.clone(), PasteMode::Link),
    );
    let _ = wait_paste(&mut session, job);
    let del = running(session.delete(
        &DeleteReq::permanent(vec![file.clone()]),
        session.next_origin(),
    ));
    let (del_report, _e) = pump(&mut session, del, |p| p.delete().cloned());
    let del_report: DeleteReport = del_report.expect("deleted");

    let mut by_action: Vec<(ActionType, i64)> = session
        .log_recent(None)
        .expect("the log")
        .records
        .iter()
        .map(|r| (r.action, r.id))
        .collect();
    by_action.sort();
    let ids: Vec<ActionType> = by_action.iter().map(|(a, _)| *a).collect();
    for want in [
        ActionType::COPY,
        ActionType::PASTE_COPY,
        ActionType::PASTE_MOVE,
        ActionType::RENAME,
        ActionType::MKDIR,
        ActionType::CHMOD,
        ActionType::PASTE_LINK,
        ActionType::DELETE,
    ] {
        assert!(
            ids.contains(&want),
            "the run produced a `{want}` record: {ids:?}"
        );
    }
    for &(action, id) in &by_action {
        let record = session.log_record(id).expect("the record");
        assert_eq!(
            record.is_reversible(),
            matches!(
                action,
                ActionType::PASTE_COPY
                    | ActionType::PASTE_MOVE
                    | ActionType::RENAME
                    | ActionType::MKDIR
                    | ActionType::PASTE_LINK
            ),
            "`{action}` reversibility is Go's derived set (`api/log.go:600-602`)"
        );
        let verdict = revert_ops::ensure_reversible(&record);
        if record.is_reversible() {
            assert!(verdict.is_ok(), "`{action}` is admitted");
        } else {
            let err = verdict.expect_err("`{action}` is refused");
            assert_eq!(err.kind(), "unsupported");
            if action == ActionType::DELETE {
                assert!(
                    err.to_string().contains("permanent"),
                    "and the delete refusal says why: {err}"
                );
            }
        }
    }
    // The plan §7 row-9 criterion, named: a delete is logged and *cannot* be reverted.
    assert_eq!(
        session
            .revert(
                del_report.log_id.expect("a delete record"),
                session.next_origin()
            )
            .expect_err("a delete is not undoable")
            .kind(),
        "unsupported"
    );
    assert!(
        !exists(&root.join("tree/one.txt")),
        "and the file really is gone"
    );
    let _ = remove_tree(&root);
}

// ── the two cross-cutting rules every mutation owes the engine ─────────────────────────────────

/// Plan §5.3's "every mutation calls `IndexIntent::note`" and plan F13's "every mutation invalidates",
/// asserted side by side for all five write operations plus the paste — the two things step 12 and the
/// file pane depend on and cannot check for themselves.
#[test]
fn every_write_op_notes_the_index_and_invalidates_the_directories_it_changed() {
    let root = temp_dir("write-events");
    let mut session = open_session(&root);
    let intent = Arc::new(RecordingIntent::new());
    session.set_intent(Arc::clone(&intent) as Arc<dyn IndexIntent>);
    let tree = mkdir(&root.join("tree"));
    let src_dir = mkdir(&root.join("src"));
    let dest = mkdir(&root.join("dest"));

    let expect = |session: &mut Session,
                  extra: &[EngineEvent],
                  origin,
                  what: &str,
                  want_dir: AbsPath,
                  want_op: IndexOp| {
        let _ = session.poll();
        let mut changed = dir_changed(extra);
        changed.extend(dir_changed(&session.dirs().flush_invalidations()));
        assert!(
            changed
                .iter()
                .any(|(p, o)| *p == fskey(&want_dir) && *o == Some(origin)),
            "{what}: {} was not invalidated with its origin: {changed:?}",
            want_dir.to_string_lossy()
        );
        assert!(
            intent.recorded().contains(&want_op),
            "{what}: the index was not told ({want_op:?}); got {:?}",
            intent.recorded()
        );
    };

    // mkdir → the parent, `Created`.
    let origin = session.next_origin();
    session
        .mkdir(&MkdirReq::new(tree.clone(), "made"), origin)
        .expect("made");
    expect(
        &mut session,
        &[],
        origin,
        "mkdir",
        tree.clone(),
        IndexOp::Created {
            path: abs(&root.join("tree/made")),
        },
    );

    // rename → the directory the name lives in, `Renamed`.
    let made = abs(&root.join("tree/made"));
    let origin = session.next_origin();
    session
        .rename(&RenameReq::one(made.clone(), "renamed"), origin)
        .expect("renamed");
    expect(
        &mut session,
        &[],
        origin,
        "rename",
        tree.clone(),
        IndexOp::Renamed {
            from: made.clone(),
            to: abs(&root.join("tree/renamed")),
        },
    );

    // chmod → the directory whose row changed, `AttributesChanged`.
    let file = write(&root.join("src/one.txt"), b"x");
    let origin = session.next_origin();
    session
        .chmod(
            &ChmodReq::parse(vec![file.clone()], "640").expect("a mode"),
            origin,
        )
        .expect("chmodded");
    expect(
        &mut session,
        &[],
        origin,
        "chmod",
        src_dir.clone(),
        IndexOp::AttributesChanged { path: file.clone() },
    );

    // paste (copy) → the destination, `Created`; the source directory is *not* invalidated by a copy.
    let origin = session.next_origin();
    let job = running(session.paste(
        &paste_req(vec![file.clone()], dest.clone(), PasteMode::Copy),
        origin,
    ));
    let (_report, ev) = pump(&mut session, job, |p| p.paste().cloned());
    expect(
        &mut session,
        &ev,
        origin,
        "paste-copy",
        dest.clone(),
        IndexOp::Created {
            path: abs(&root.join("dest/one.txt")),
        },
    );

    // delete → the parent that lost the child, `Deleted` for a file.
    let copy = abs(&root.join("dest/one.txt"));
    let origin = session.next_origin();
    let job = running(session.delete(&DeleteReq::permanent(vec![copy.clone()]), origin));
    let (_report, ev) = pump(&mut session, job, |p| p.delete().cloned());
    expect(
        &mut session,
        &ev,
        origin,
        "delete",
        dest.clone(),
        IndexOp::Deleted { path: copy.clone() },
    );

    // open changes nothing, so it owes nothing: no invalidation, no index op, no record. The
    // *directory* is the honest argument here — a directory is routed, never handed to the launcher
    // (`open::route`), so the assertion costs no spawned desktop application — and the launched-file
    // lane of the same promise is asserted by
    // `open_launches_files_routes_directories_and_reports_a_missing_opener`, on a recording env.
    let before = intent.len();
    session
        .open_paths(std::slice::from_ref(&dest))
        .expect("a directory open is a route, and needs no launcher");
    let _ = session.poll();
    assert!(dir_changed(&session.dirs().flush_invalidations()).is_empty());
    assert_eq!(
        intent.len(),
        before,
        "an `open` notes nothing, because nothing changed"
    );
    // (The fifth op, `chmod`, and the paste are both covered above; `open` is the one that owes nothing.)
    let _ = remove_tree(&root);
}
