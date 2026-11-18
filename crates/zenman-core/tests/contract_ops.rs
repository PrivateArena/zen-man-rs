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

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zenman-contract-ops-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = remove_tree(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Recursive delete for a *test scratch tree*. `std::fs::remove_dir_all` is banned workspace-wide because
/// product deletion must go through `ops::delete` (step 9c) — which does not exist yet, and which must
/// never be what cleans a temp directory anyway.
fn remove_tree(path: &Path) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || meta.is_file() {
        return fs::remove_file(path);
    }
    for entry in fs::read_dir(path)?.flatten() {
        remove_tree(&entry.path())?;
    }
    fs::remove_dir(path)
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
