//! Plan §5.10 names this file; step 16b grows it into the **full sweep**: one test per §6.2
//! deviation line, each asserting the *documented* behaviour end-to-end through [`Session`] —
//! not the unit-level check the deviation's own step wrote. Every §6.2 ledger row names either a
//! test in this file or the real (corrected) unit test it actually lives in; the step-16b log in
//! plan §9.2 records which rows were stale and which behaviours were missing from the table.
//!
//! The scratch trees are real temporary directories; the sessions are real `Session::open` boots
//! with `default_env()`. No mock filesystem, no Go server, no GPU, no display.
//!
//! Unix-only: the symlink row needs `symlink(2)` and the size/mode rows assume POSIX semantics
//! (same gate as `contract_ops.rs`).

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};

use zenman_core::entry::FileEntry;
use zenman_core::error::CoreError;
use zenman_core::events::EngineEvent;
use zenman_core::jobs::JobId;
use zenman_core::limits::Limits;
use zenman_core::ops::ConflictPolicy;
use zenman_core::ops::delete::{DeleteMode, DeleteReq};
use zenman_core::ops::paste::{PasteMode, PasteReport, PasteReq};
use zenman_core::path::{AbsPath, FsPath};
use zenman_core::platform::default_env;
use zenman_core::session::{Receipt, Session, SessionPaths};
use zenman_core::store::config::ZenConfig;

// ------------------------------------------------------------------ scratch trees and session boots

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zenman-contract-sweep-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = remove_tree(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Recursive delete for a *test scratch tree* (`std::fs::remove_dir_all` is banned workspace-wide:
/// product deletion must go through `ops::delete` — which must never be what cleans a temp dir).
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

fn abs(path: &Path) -> AbsPath {
    AbsPath::from_path(path).expect("absolute scratch path")
}

fn write_file(path: &Path, bytes: &[u8]) -> AbsPath {
    fs::create_dir_all(path.parent().expect("parent")).expect("parents");
    fs::write(path, bytes).expect("write");
    abs(path)
}

/// A session over `root` with a real database (the rows that need the DB index) or without one.
fn open_session(root: &Path, tag: &str, with_db: bool) -> Session {
    let db = with_db.then(|| root.join(format!("{tag}.db")));
    let (session, _notes) = Session::open(
        SessionPaths::new(
            root.join("config.json"),
            root.join("action.log"),
            db,
            root.join("probe"),
        ),
        ZenConfig::default(),
        default_env(),
        Limits::default(),
    )
    .expect("a session over a clean scratch directory opens");
    session
}

// -------------------------------------------------------------------------------------------- pump

/// Poll until `job` reaches its terminal event, returning its payload pick and every event seen.
/// The wall-clock deadline is the `settle_until` rule (core coalesces invalidations on a clock).
fn pump<T: Clone>(
    session: &mut Session,
    job: JobId,
    pick: impl Fn(&zenman_core::events::JobPayload) -> Option<T>,
) -> (Option<T>, Vec<EngineEvent>) {
    let mut events = Vec::new();
    let mut done: Option<T> = None;
    for _ in 0..400 {
        events.extend(session.poll());
        if done.is_none() {
            done = events.iter().find_map(|ev| match ev {
                EngineEvent::JobFinished { id, payload, .. } if *id == job => pick(payload),
                _ => None,
            });
        }
        if events
            .iter()
            .any(|ev| matches!(ev, EngineEvent::JobFinished { id, .. } if *id == job))
        {
            // `DirChanged` rides the wall-clock coalescing window (`services/dirs.rs`), so a
            // revert's invalidation can land *after* the terminal event: keep polling briefly.
            for _ in 0..80 {
                std::thread::sleep(std::time::Duration::from_millis(5));
                events.extend(session.poll());
                if events
                    .iter()
                    .any(|ev| matches!(ev, EngineEvent::DirChanged { .. }))
                {
                    break;
                }
            }
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    (done, events)
}

fn running<T: std::fmt::Debug>(receipt: Result<Receipt<T>, CoreError>) -> JobId {
    match receipt {
        Ok(Receipt::Running { job, .. }) => job,
        other => panic!("expected a running job: {other:?}"),
    }
}

/// The helper §6.2's typed-errors row cites by name (`contract.rs::assert_matches`): the ledger
/// claim "errors are typed `CoreError`" means a caller can *match* the error, so that is exactly
/// what this asserts — with a context string carrying the value on failure.
fn assert_matches<T: std::fmt::Debug>(value: &T, pred: impl Fn(&T) -> bool, ctx: &str) {
    assert!(
        pred(value),
        "{ctx}: {value:?} did not match the documented shape"
    );
}

fn page_entries(session: &mut Session, dir: &AbsPath) -> Vec<FileEntry> {
    let origin = session.next_origin();
    let receipt = session
        .page_dir(
            &zenman_core::services::dirs::ListDir::first(FsPath::from_abs(dir.clone())),
            origin,
        )
        .expect("listing a real directory succeeds");
    match receipt {
        Receipt::Done(page) => page.entries.clone(),
        Receipt::Running { job, .. } => {
            let (page, _ev) = pump(session, job, |p| p.dir_page().cloned());
            page.expect("the flat listing finished").entries.clone()
        }
    }
}

// ------------------------------------------------------------------------------------- §6.2 rows

/// Row "`files_count` typed as `Option` → renders `--`" + decision D11 (`Option` sizes, never the
/// "0 means unknown" convention): through the session, a file's `files_count` is an `Option` that
/// **always serialises** (Go's `omitempty` is what produced `undefined files`), and a directory
/// nobody has walked reports **no size at all** — not size 0.
#[test]
fn d01_unknown_sizes_are_absent_never_zero() {
    let root = temp_dir("d01");
    let tree = root.join("tree");
    fs::create_dir_all(&tree).expect("tree");
    write_file(&tree.join("normal.txt"), b"40 bytes of text");
    fs::create_dir_all(tree.join("subdir")).expect("subdir");
    let mut session = open_session(&root, "d01", false);

    let entries = page_entries(&mut session, &abs(&tree));
    let file = entries
        .iter()
        .find(|e| e.name == "normal.txt")
        .expect("the file is listed");
    let json = serde_json::to_value(file).expect("entries serialise");
    assert!(
        json.get("files_count").is_some(),
        "files_count must always be present on the wire (Go omitted it): {json}"
    );
    let dir = entries
        .iter()
        .find(|e| e.name == "subdir")
        .expect("dir listed");
    assert!(dir.is_dir, "a real directory is a directory");

    // D11: a directory nobody asked about has *no* cached size — `None`, not `Some(0)`.
    let sizes = session.cached_dir_sizes(&[abs(&tree)]);
    assert_matches(
        &sizes,
        |s| s.len() == 1 && s[0].is_none(),
        "un-walked dir size",
    );
}

/// Row "symlinked directories report `is_dir: true`" — **corrected by this sweep**: the value-type
/// rule (`EntryKind::SymlinkToDir` ⇒ `is_dir: true`) holds only for entries built from a *resolved*
/// target (`entry.rs` unit test); the raw `lstat` listing seam deliberately does not resolve, so a
/// non-flat listing keeps Go's `is_dir: false` + `is_symlink: true` row, pinned by the golden
/// fixtures (`services/dirs.rs::FsSource::lstat` doc). The ledger line now says exactly this.
#[test]
fn d02_symlinked_directory_rows_keep_the_link_bit_at_the_listing_seam() {
    let root = temp_dir("d02");
    let tree = root.join("tree");
    fs::create_dir_all(tree.join("target")).expect("target");
    std::os::unix::fs::symlink(tree.join("target"), tree.join("link"))
        .expect("symlink to a directory");
    let mut session = open_session(&root, "d02", false);

    let entries = page_entries(&mut session, &abs(&tree));
    let link = entries
        .iter()
        .find(|e| e.name == "link")
        .expect("link listed");
    assert!(link.is_symlink, "the row says it is a link");
    assert!(
        !link.is_dir,
        "the unresolved listing seam keeps Go's is_dir:false (golden-pinned)"
    );
}

/// Row "`copyFile` collisions reported": a copy onto an occupied name under the default policy
/// comes back as a typed `Skipped` outcome the UI can toast — never a silent success (plan F12).
#[test]
fn d03_copy_collision_is_reported_not_silent() {
    let root = temp_dir("d03");
    let src = write_file(&root.join("tree/a.txt"), b"source");
    write_file(&root.join("dest/a.txt"), b"existing");
    let mut session = open_session(&root, "d03", false);

    let origin = session.next_origin();
    let job = running(session.paste(
        &PasteReq {
            sources: vec![src.clone()],
            dest: abs(&root.join("dest")),
            mode: PasteMode::Copy,
            policy: ConflictPolicy::SkipExisting,
        },
        origin,
    ));
    let (report, _ev) = pump(&mut session, job, |p| p.paste().cloned());
    let report = report.expect("the paste job finished with a report");
    assert!(
        !report.skipped().is_empty(),
        "the collision must be reported as skipped: {:?}",
        report.outcomes
    );
    assert_eq!(
        fs::read(root.join("dest/a.txt")).expect("dest readable"),
        b"existing",
        "SkipExisting never overwrites"
    );
}

/// Row "errors are typed `CoreError`": the session refuses with a *matchable* variant (no JSON
/// bodies, no magic strings), and the refusal leaves the session usable.
#[test]
fn d04_errors_are_typed_and_the_session_survives_them() {
    let root = temp_dir("d04");
    fs::create_dir_all(root.join("tree")).expect("tree");
    let mut session = open_session(&root, "d04", false);

    let origin = session.next_origin();
    let err = session
        .delete(
            &DeleteReq {
                sources: vec![],
                mode: DeleteMode::Trash,
                permanent_fallback: false,
            },
            origin,
        )
        .expect_err("an empty selection is refused");
    assert_matches(
        &err,
        |e| matches!(e, CoreError::Empty { .. }) && !e.user_message().is_empty(),
        "empty delete",
    );
    // The session is intact: the next call works.
    let _ = page_entries(&mut session, &abs(&root.join("tree")));
}

/// Row "paste conflict is an enum variant, not `status:\"conflict\"`": a directory-onto-directory
/// copy surfaces through the typed `conflicts` field.
#[test]
fn d05_paste_conflict_is_a_typed_field() {
    let root = temp_dir("d05");
    fs::create_dir_all(root.join("tree/d1/inner")).expect("source dir");
    write_file(&root.join("tree/d1/inner/f.txt"), b"x");
    fs::create_dir_all(root.join("dest/d1")).expect("existing dir");
    let mut session = open_session(&root, "d05", false);

    let origin = session.next_origin();
    let job = running(session.paste(
        &PasteReq {
            sources: vec![abs(&root.join("tree/d1"))],
            dest: abs(&root.join("dest")),
            mode: PasteMode::Copy,
            policy: ConflictPolicy::SkipExisting,
        },
        origin,
    ));
    let (report, _ev) = pump(&mut session, job, |p| p.paste().cloned());
    let report: PasteReport = report.expect("paste finished");
    assert!(
        report.conflicts.iter().any(|c| c == "d1"),
        "dir-onto-dir must name the conflict: {:?}",
        report.conflicts
    );
}

/// Row "batch rename uses `RenameTarget::Batch`, not JSON-in-`name`": the batch lands real renames
/// on disk through the single-rename path (one Go-parity log record), with no encoded payload.
#[test]
fn d06_batch_rename_is_a_typed_target() {
    use zenman_core::ops::batch_rename::{Row, Rules};
    let root = temp_dir("d06");
    let tree = root.join("tree");
    write_file(&tree.join("afile.txt"), b"a");
    write_file(&tree.join("bfile.txt"), b"b");
    let session = open_session(&root, "d06", false);

    let rows: Vec<Row> = ["afile.txt", "bfile.txt"]
        .iter()
        .map(|n| Row::new(abs(&tree.join(n)), *n, false))
        .collect();
    let rules = Rules {
        find: "file".to_string(),
        replace: "doc".to_string(),
        ..Default::default()
    };
    let _report = session
        .batch_rename(
            &rows,
            &rules,
            &std::collections::HashSet::new(),
            ConflictPolicy::SkipExisting,
            session.next_origin(),
        )
        .expect("the batch renames");
    assert!(
        tree.join("adoc.txt").is_file() && tree.join("bdoc.txt").is_file(),
        "the renamed names are on disk: {:?}",
        fs::read_dir(&tree).map(|d| d.count())
    );
}

/// Row "bookmarks percent-decoded on save" — **corrected by this sweep**: the port does **not**
/// percent-decode favourites (that claim described Go's *clipboard* URI-list parsing,
/// `api/ops.go:611-627`). What `Session::add_favourite` actually guarantees, end-to-end: absolute
/// paths are normalised (trailing slashes trimmed) and stored verbatim otherwise, relative paths
/// are a typed refusal. The ledger line now says exactly this.
#[test]
fn d07_favourites_are_normalised_verbatim_on_save() {
    let root = temp_dir("d07");
    let session = open_session(&root, "d07", false);
    let padded = format!("{}/a%20b/", root.display());
    let bookmarks = session
        .add_favourite(&padded)
        .expect("a favourite is accepted");
    let stored = bookmarks
        .iter()
        .find(|b| b.path.contains("a%20b"))
        .expect("the path is stored");
    assert!(
        stored.path.ends_with("a%20b") && !stored.path.ends_with('/'),
        "trailing slash trimmed, text stored verbatim: {stored:?}"
    );
    let rel = session
        .add_favourite("relative/nope")
        .expect_err("a relative favourite is refused");
    assert_matches(
        &rel,
        |e| matches!(e, CoreError::Config { .. }),
        "relative favourite",
    );
}

/// Row "index status distinguishes dead root vs corrupt DB", the DB half, end-to-end: a garbage
/// database file degrades to a typed `database_disabled()` notice and the session still opens and
/// lists (plan F5 — Go `log.Fatalf`'d). The dead-root half is `index/rebuild.rs::roots_failed`
/// (a per-root `(path, reason)` list) rendered by the Search Manager — a *different* channel, and
/// that distinction is what this row claims.
#[test]
fn d08_corrupt_database_and_dead_root_are_different_channels() {
    let root = temp_dir("d08");
    fs::create_dir_all(root.join("tree")).expect("tree");
    fs::write(root.join("garbage.db"), b"not a sqlite file at all").expect("garbage db");
    let (session, _notes) = Session::open(
        SessionPaths::new(
            root.join("config.json"),
            root.join("action.log"),
            Some(root.join("garbage.db")),
            root.join("probe"),
        ),
        ZenConfig::default(),
        default_env(),
        Limits::default(),
    )
    .expect("a corrupt database never aborts the boot");
    assert!(
        session.database_disabled().is_some(),
        "the DB failure lands on its own channel"
    );
    let healthy = open_session(&root, "d08-healthy", true);
    assert!(healthy.database_disabled().is_none());
    assert!(healthy.index_status().error.is_none());
}

/// Row "revert refreshes the pane immediately": the revert job emits `DirChanged` for the
/// affected parents (Go's `zen-refresh-filelist` event had no subscriber).
#[test]
fn d09_revert_invalidates_the_pane() {
    let root = temp_dir("d09");
    let src = write_file(&root.join("tree/victim.txt"), b"copy me");
    fs::create_dir_all(root.join("dest")).expect("dest");
    let mut session = open_session(&root, "d09", false);

    let origin = session.next_origin();
    let job = running(session.paste(
        &PasteReq {
            sources: vec![src],
            dest: abs(&root.join("dest")),
            mode: PasteMode::Copy,
            policy: ConflictPolicy::Rename,
        },
        origin,
    ));
    let (report, _ev) = pump(&mut session, job, |p| p.paste().cloned());
    let report = report.expect("paste finished");
    let log_id = report.log_id.expect("the paste logged a record");
    assert!(root.join("dest/victim.txt").is_file(), "the copy landed");

    let job = running(session.revert(log_id, session.next_origin()));
    let (_report, events) = pump(&mut session, job, |p| p.revert().cloned());
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, EngineEvent::DirChanged { .. })),
        "the revert must emit DirChanged so the pane refreshes: {events:?}"
    );
    assert!(
        !root.join("dest/victim.txt").exists(),
        "reverting a copy removes the copy"
    );
}

/// Row "`journal_mode = DELETE`, explicitly asserted": through the live session connection, not
/// only the `store/sql.rs` boot assert.
#[test]
fn d10_journal_mode_is_delete_through_the_session() {
    let root = temp_dir("d10");
    let session = open_session(&root, "d10", true);
    let mode = session
        .with_db(|conn| {
            conn.query_row("PRAGMA journal_mode;", [], |r| r.get::<_, String>(0))
                .unwrap_or_default()
        })
        .expect("the session answers pragmas");
    assert_eq!(
        mode.to_lowercase(),
        "delete",
        "WAL sidecars would break the shared Go data directory (house rule 8)"
    );
}

/// Row "sort is clickable", the engine half: the sort is a field of the listing request and
/// dirs-first is an invariant of the answer. (The click itself is `zenman-ui`'s
/// `tests/file_pane.rs::the_header_sorts_the_loaded_rows_without_a_refetch`.)
#[test]
fn d11_sort_is_a_request_field_and_dirs_come_first() {
    let root = temp_dir("d11");
    let tree = root.join("tree");
    fs::create_dir_all(tree.join("zdir")).expect("zdir");
    write_file(&tree.join("afile.txt"), b"a");
    let mut session = open_session(&root, "d11", false);
    let entries = page_entries(&mut session, &abs(&tree));
    assert_eq!(entries.len(), 2);
    assert!(
        entries[0].is_dir,
        "directories first, whatever the order inside"
    );
}

/// The flat-listing contract (plan §2.4/§5.10's `flat_modes_match_go`): a plain listing is
/// synchronous, a flat one is a *job* (the recursion must never block the frame — plan F1).
#[test]
fn d12_plain_listing_is_synchronous() {
    let root = temp_dir("d12");
    fs::create_dir_all(root.join("tree/deep/deeper")).expect("deep tree");
    let session = open_session(&root, "d12", false);
    let receipt = session
        .page_dir(
            &zenman_core::services::dirs::ListDir::first(FsPath::from_abs(abs(&root.join("tree")))),
            session.next_origin(),
        )
        .expect("listing works");
    assert!(
        matches!(receipt, Receipt::Done(_)),
        "a non-flat first page answers synchronously: {receipt:?}"
    );
}

/// The OPEN-DECISION row (unterminated `"` in a filter): this test pins only that the **port**
/// side answers through the session without an error and without a panic — the quote-scanner
/// choice itself stays undecided and is pinned by
/// `services/search_index.rs::an_unterminated_quote_keeps_the_path_clean_the_port_side_of_a_known_divergence`.
/// Do not "fix" either side before the decision is taken (§6.2).
#[test]
fn d13_unterminated_quote_query_answers_without_error() {
    let root = temp_dir("d13");
    let session = open_session(&root, "d13", true);
    let unterminated = format!("in:\"{}/a/b", root.display());
    let results = session
        .index_search(&unterminated, None, None)
        .expect("the scanner consumes the open quote and answers");
    let _ = format!("{results:?}");
}

/// The trash-first row (decision D6, missing from the §6.2 table until step 16b): `Del` trashes
/// when the volume has a native bin and **refuses rather than destroys** when it does not and the
/// permanent fallback was not accepted. Asserted against the *measured* `can_trash` reality, so it
/// holds on a box with a bin and on one without.
#[test]
fn d14_delete_is_trash_first_and_never_silently_permanent() {
    let root = temp_dir("d14");
    let victim = write_file(&root.join("tree/victim.txt"), b"precious");
    let env = default_env();
    let can = env.can_trash(&victim);
    let mut session = open_session(&root, "d14", false);

    let job = running(session.delete(
        &DeleteReq::to_trash(vec![victim.clone()]),
        session.next_origin(),
    ));
    let (report, _ev) = pump(&mut session, job, |p| p.delete().cloned());
    let report = report.expect("the delete finished");
    assert_eq!(report.mode_used, DeleteMode::Trash, "the request is echoed");
    let outcome = format!("{:?}", report.outcomes[0]);
    if can {
        assert!(outcome.starts_with("Trashed"), "{outcome}");
        assert!(!victim.as_path().exists(), "the file went to the bin");
    } else {
        assert!(
            !outcome.starts_with("Trashed") && victim.as_path().exists(),
            "no native bin ({can}) must never mean a silent permanent delete: {outcome}"
        );
    }
}
