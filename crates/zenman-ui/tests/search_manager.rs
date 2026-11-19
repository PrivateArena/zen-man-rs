//! The step-12c Search Manager dialog and the index-config write path.
//!
//! What this file owns (plan §7 row 12's UI half + §5.8's `search_manager` row):
//!
//! * **`Ctrl+Shift+F` opens it** (the step-10a toast is gone), and the dialog's draft *is* the live
//!   config — the object `tests/golden/index-status.json`'s `config` describes;
//! * **a rebuild started from the dialog reaches a terminal state** and the status panel's numbers
//!   then agree with [`Session::index_status`](zenman_core::session::Session::index_status) — the
//!   `index-status.json` fields, not a copy that can drift;
//! * **`ext:` / `type:` queries return the rows Go returns** (the operators are core's, and there is
//!   **no `size:`** — an unrecognised operator stays ordinary text), with Go's `capped` semantics
//!   (`total_matched > limit + offset`) surfaced on the page control;
//! * **a stale answer is not displayed** (the `quick_find` rule, spelled a second time here);
//! * **a root added through the dialog is persisted** — read back by a *reopened* dialog and by
//!   `Session::index_config()`, which is `index-config-apply.json`'s `{"saved": true}` made testable;
//! * **an invalid config value shows inline and persists nothing**;
//! * **Escape cancels a running rebuild**.

mod common;

use std::path::{Path, PathBuf};

use egui::Key;
use egui_kittest::Harness;

use zenman_core::path::{AbsPath, FsPath};
use zenman_core::services::search_index::{IndexEntry, IndexSearchResults};
use zenman_core::session::{Session, SessionPaths};
use zenman_core::store::config::{LayoutMode, PaneSlot, ZenConfig};
use zenman_ui::app::ZenApp;
use zenman_ui::dialogs::Dialog;
use zenman_ui::dialogs::search_manager::{self, ManagerRequest, SearchManager};
use zenman_ui::keymap::{Chord, Command};
use zenman_ui::state::panes_for;

// --------------------------------------------------------------------- fixtures

fn scratch(tag: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("zenman-ui-fixtures")
        .join(format!("zenman-ui-fixture-{tag}"))
}

/// The small indexed tree — 7 files and 3 directories:
///
/// ```text
/// <scratch>/tree/
///   alpha.txt  beta.txt  gamma.txt  notes.md
///   docs/readme.md
///   src/main.rs  src/util/copy.txt
/// ```
fn tree(tag: &str) -> PathBuf {
    let root = scratch(tag).join("tree");
    common::remove_tree(&root);
    for name in [
        "alpha.txt",
        "beta.txt",
        "gamma.txt",
        "notes.md",
        "docs/readme.md",
        "src/main.rs",
        "src/util/copy.txt",
    ] {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().expect("every fixture file has a parent"))
            .expect("fixture directory");
        std::fs::write(&path, format!("{name} body\n")).expect("fixture file");
    }
    root
}

/// The tree [`big_tree`] writes, and how many rows a finished rebuild would index.
const BIG_DIRS: usize = 10;
const BIG_FILES: usize = 300;
const BIG_TOTAL: usize = BIG_DIRS * BIG_FILES;

/// A tree big enough that a rebuild is still walking when <kbd>Escape</kbd> lands.
fn big_tree(tag: &str) -> PathBuf {
    let root = scratch(tag).join("tree");
    common::remove_tree(&root);
    for d in 0..BIG_DIRS {
        let dir = root.join(format!("d{d}"));
        std::fs::create_dir_all(&dir).expect("fixture directory");
        for f in 0..BIG_FILES {
            std::fs::write(dir.join(format!("f{f:04}.txt")), "x\n").expect("fixture file");
        }
    }
    root
}

/// A session **with** the index database (a `common::session` has none, and every index call needs
/// one), configured to index `root` through the very write path this step adds.
fn index_session(tag: &str, root: &Path) -> Session {
    let dir = scratch(tag);
    std::fs::create_dir_all(&dir).expect("fixture directory");
    let paths = SessionPaths::new(
        dir.join("config.json"),
        dir.join("action.log"),
        Some(dir.join("zen-man.db")),
        dir.join("probe"),
    )
    .with_db_open(4, std::time::Duration::from_millis(25));
    let (session, _notes) = Session::open(
        paths,
        ZenConfig::default(),
        zenman_core::platform::default_env(),
        ZenConfig::default().limits(),
    )
    .expect("a session opens over a fresh directory");
    let mut cfg = session.index_config();
    cfg.roots = vec![root.display().to_string()];
    session
        .set_index_config(cfg, session.next_origin())
        .expect("the fixture root is a sound configuration");
    session
}

fn abs(path: &Path) -> AbsPath {
    AbsPath::try_from(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn build_harness(app: ZenApp) -> Harness<'static, ZenApp> {
    let mut harness = Harness::builder()
        .with_size(egui::vec2(1400.0, 900.0))
        // Rebuilds, the status poll and the coalesced `DirChanged` window stretch the frame loop.
        .with_max_steps(64)
        // A seeded `TextEdit` never surfaces an a11y name in this fork (plan §9.2 step 10 trap).
        .with_accessibility_check(false)
        .with_context_setup(|ctx| {
            egui_extras::install_image_loaders(ctx);
            zenman_ui::theme::fonts::install(ctx);
            zenman_ui::theme::apply(ctx, zenman_core::store::config::ThemeMode::Dark);
        })
        .build_ui_state(
            |ui, app| {
                app.logic(ui.ctx());
                app.ui(ui);
            },
            app,
        );
    for _ in 0..6 {
        harness.run();
    }
    harness
}

fn manager_harness(tag: &str, root: &Path) -> Harness<'static, ZenApp> {
    let session = index_session(tag, root);
    let listing = scratch(tag).join("listing");
    std::fs::create_dir_all(&listing).expect("listing directory");
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Single));
    app.open_tab(PaneSlot::Left, FsPath::from_abs(abs(&listing)));
    build_harness(app)
}

// --------------------------------------------------------------------- driving the dialog

fn open_manager(harness: &mut Harness<'static, ZenApp>) {
    let ctx = harness.ctx.clone();
    harness
        .state_mut()
        .dispatch(PaneSlot::Left, Command::SearchManager, &ctx);
    harness.run();
    harness.run();
}

fn manager(harness: &Harness<'static, ZenApp>) -> SearchManager {
    match harness.state().dialogs.active() {
        Some(Dialog::SearchManager(m)) => m.as_ref().clone(),
        other => panic!("expected the search manager, found {other:?}"),
    }
}

fn edit_manager(harness: &mut Harness<'static, ZenApp>, edit: impl FnOnce(&mut SearchManager)) {
    match harness.state_mut().dialogs.active_mut() {
        Some(Dialog::SearchManager(m)) => edit(std::ops::DerefMut::deref_mut(m)),

        other => panic!("expected the search manager, found {other:?}"),
    }
}

fn press_key(harness: &mut Harness<'static, ZenApp>, key: Key) {
    harness.input_mut().events.push(egui::Event::Key {
        modifiers: Default::default(),
        physical_key: None,
        key,
        pressed: true,
        repeat: false,
    });
    harness.run();
    harness.run();
}

/// Index the fixture tree *through the dialog* (the plan's Rebuild button, not a direct call).
fn rebuild_through_dialog(harness: &mut Harness<'static, ZenApp>) {
    edit_manager(harness, |m| m.requests.push(ManagerRequest::Rebuild));
    common::settle_until(harness, "the rebuild reaches a terminal state", |h| {
        !h.state().session.index_is_running()
            && manager(h).rebuild.is_none()
            && !manager(h).status.is_indexing
    });
}

/// Run a query through the dialog and wait for *its* answer (the counters, never a frame count).
fn query(harness: &mut Harness<'static, ZenApp>, text: &str, limit: usize) -> SearchManager {
    let before = manager(harness).painted;
    edit_manager(harness, |m| {
        m.query = text.to_string();
        m.sync_pills();
        m.limit = limit;
        m.page = 0;
    });
    common::settle_until(harness, text, |h| {
        let m = manager(h);
        m.query == text && m.limit == limit && m.painted > before
    });
    manager(harness)
}

fn names(rows: &[IndexEntry]) -> Vec<String> {
    let mut got: Vec<String> = rows.iter().map(|r| r.name.clone()).collect();
    got.sort();
    got
}

// --------------------------------------------------------------------- tests

/// The step-10a toast is gone: `Ctrl+Shift+F` opens the manager, on the live config.
#[test]
fn chord_opens_the_search_manager_on_the_live_config() {
    let root = tree("12c-open");
    let mut harness = manager_harness("12c-open", &root);
    harness
        .input_mut()
        .events
        .push(Chord::ctrl_shift(Key::F).event());
    harness.run();
    harness.run();
    let m = manager(&harness);
    assert_eq!(
        m.draft.roots,
        vec![root.display().to_string()],
        "the draft is the config the engine holds"
    );
    assert_eq!(
        m.limit,
        search_manager::DEFAULT_LIMIT,
        "Go's 200 rows per page"
    );
    assert!(!m.settings_open, "the config panel starts folded");
    assert_eq!(
        m.status.config.roots, m.draft.roots,
        "and the status block agrees"
    );
}

/// `index-status.json`'s fields, agreeing with the engine after a dialog-driven rebuild.
#[test]
fn rebuild_from_the_dialog_reaches_a_terminal_state_and_the_status_agrees() {
    let root = tree("12c-rebuild");
    let mut harness = manager_harness("12c-rebuild", &root);
    open_manager(&mut harness);
    assert!(
        !manager(&harness).status.is_indexing,
        "nothing is running before the button"
    );
    rebuild_through_dialog(&mut harness);
    let live = harness.state().session.index_status();
    let m = manager(&harness);
    assert!(
        !m.status.is_indexing,
        "the panel reached its terminal state"
    );
    assert_eq!(
        m.status.total_files, live.total_files,
        "total_files is the engine's number"
    );
    assert_eq!(m.status.last_indexed, live.last_indexed, "last_indexed too");
    assert_eq!(m.status.is_dirty, live.is_dirty, "and is_dirty");
    assert_eq!(
        m.status.progress.indexed, live.progress.indexed,
        "idle, progress.indexed equals the total"
    );
    assert_eq!(
        m.status.total_files, 11,
        "the seven fixture files, their three directories, and the indexed root itself"
    );
    assert_eq!(m.status.config.roots, live.config.roots);
    assert!(m.status.error.is_none(), "a good rebuild reports no error");
    assert_eq!(
        m.status_text(),
        format!("{} files indexed", live.total_files),
        "Go's status line, in the idle form"
    );
}

/// Go's operators, from the index: `ext:` picks the extension, `type:dir` the directories, and
/// `capped` is `total_matched > limit + offset`.
#[test]
fn index_queries_answer_from_the_rebuilt_index_and_capped_behaves() {
    let root = tree("12c-query");
    let mut harness = manager_harness("12c-query", &root);
    open_manager(&mut harness);
    rebuild_through_dialog(&mut harness);

    let ext = query(&mut harness, "ext:txt", 50);
    assert_eq!(ext.pills.len(), 1, "`ext:` is one chip");
    assert_eq!(ext.pills[0].label(), "ext:txt");
    assert_eq!(
        names(&ext.hits),
        vec!["alpha.txt", "beta.txt", "copy.txt", "gamma.txt"],
        "Go's `ext:` matches the extension column, wherever the file lives"
    );
    assert_eq!(ext.matched, 4, "and the count says so");
    assert!(!ext.capped, "four rows on a page of 50 is not capped");

    let dirs = query(&mut harness, "type:dir", 50);
    assert!(
        !dirs.hits.is_empty(),
        "`type:dir` finds the indexed directories"
    );
    assert_eq!(
        names(&dirs.hits),
        vec!["docs", "src", "tree", "util"],
        "all of them, in Go's `ORDER BY is_dir DESC, name ASC` (the walk indexes the root too)"
    );
    assert!(
        dirs.hits.iter().all(|r| r.is_dir),
        "and only directories: {:?}",
        names(&dirs.hits)
    );
    // Go's index rows carry `mode: ""` (plan §2.4) and an absolute `rel_path`.
    assert!(
        dirs.hits.iter().all(|r| r.mode.is_empty()),
        "the index stores no mode"
    );
    assert!(
        dirs.hits
            .iter()
            .all(|r| r.rel_path.starts_with('/') && r.rel_path.contains("tree")),
        "`rel_path` is the absolute path (Go's own comment on global search)"
    );

    // `size:` is not an operator in Go, so it is not a chip here — and it is not a match either.
    let size = query(&mut harness, "size:10", 50);
    assert!(
        size.pills.is_empty(),
        "there is no `size:` operator to chip"
    );
    assert_eq!(
        size.matched, 0,
        "and the word `size:10` matches nothing here"
    );

    // capped: one row per page over several matches, so `capped` is true and the page control lives.
    let paged = query(&mut harness, "txt", 1);
    assert_eq!(paged.hits.len(), 1, "one row per page");
    assert!(paged.capped, "Go's `capped` on a page that has a successor");
    assert!(
        paged.matched > 1,
        "and the total says so: {}",
        paged.matched
    );
    assert_eq!(
        paged.result_text(),
        format!("{}+ results (capped)", paged.matched)
    );
}

/// A stale answer never paints (the `quick_find` rule, plan §4.3).
#[test]
fn a_stale_index_answer_is_not_displayed() {
    let root = tree("12c-stale");
    let mut harness = manager_harness("12c-stale", &root);
    open_manager(&mut harness);
    rebuild_through_dialog(&mut harness);
    let session = harness.state().session.clone();
    let live = session
        .index_search("txt", Some(50), Some(0))
        .expect("the index answers a search");
    let rows = live.entries.len();
    assert!(rows > 0, "the fixture index has rows to deliver");
    let stale = session.next_origin();
    let fresh = session.next_origin();
    assert_ne!(stale, fresh, "two dispatches, two tokens");
    edit_manager(&mut harness, |m| {
        // The dialog's newest token is `fresh`; an answer tagged `stale` must be dropped.
        m.origin = Some(fresh);
        m.hits = Vec::new();
        assert!(!m.deliver(stale, &live), "a superseded token is refused");
        assert!(m.hits.is_empty(), "and its rows never reach the table");
        assert!(m.deliver(fresh, &live), "the current token is accepted");
        assert_eq!(m.hits.len(), rows);
    });
}

/// Adding a root through the dialog is Go's `POST /api/index/config`: it saves, marks the index
/// dirty, and a reopened dialog reads it back.
#[test]
fn adding_a_root_through_the_dialog_persists_it() {
    let root = tree("12c-config");
    let extra = scratch("12c-config").join("extra");
    std::fs::create_dir_all(&extra).expect("extra root");
    let mut harness = manager_harness("12c-config", &root);
    open_manager(&mut harness);
    let before = harness.state().session.index_config();
    edit_manager(&mut harness, |m| {
        m.settings_open = true;
        // the same edit the `add root` button makes, then `Apply`
        m.draft.roots.push(extra.display().to_string());
        m.requests.push(ManagerRequest::Apply);
    });
    common::settle_until(&mut harness, "the write lands", |h| {
        h.state().session.index_config().roots.len() == before.roots.len() + 1
            && manager(h).error.is_none()
    });
    let live = harness.state().session.index_config();
    assert!(live.roots.contains(&extra.display().to_string()));
    assert!(
        harness.state().session.index_status().is_dirty,
        "a new root set makes the index dirty (the golden `is_dirty` flag)"
    );
    // and it survives a reopen of the dialog — the value came from the settings file, not the frame
    press_key(&mut harness, Key::Escape);
    assert!(
        harness.state().dialogs.active().is_none(),
        "Apply does not close the editor, Escape does"
    );
    open_manager(&mut harness);
    assert_eq!(
        manager(&harness).draft,
        live,
        "the reopened dialog reads the saved config"
    );
    // the saved value is on disk, not just in the session
    let on_disk =
        ZenConfig::load(&harness.state().session.paths().config).expect("the config file");
    assert_eq!(on_disk.indexer, live, "the settings file carries it");
}

/// An invalid value is refused by the engine's own bounds, shown inline, and changes nothing.
#[test]
fn an_invalid_config_value_shows_inline_and_does_not_persist() {
    let root = tree("12c-invalid");
    let mut harness = manager_harness("12c-invalid", &root);
    open_manager(&mut harness);
    let before = harness.state().session.index_config();
    edit_manager(&mut harness, |m| {
        m.settings_open = true;
        m.worker_text = "0".to_string();
        m.requests.push(ManagerRequest::Apply);
    });
    common::settle_until(&mut harness, "the refusal paints", |h| {
        manager(h).error.is_some()
    });
    let m = manager(&harness);
    let err = m.error.unwrap_or_default();
    assert!(
        err.contains("worker_count"),
        "the message names the field: {err}"
    );
    assert_eq!(
        harness.state().session.index_config(),
        before,
        "a refusal persists nothing"
    );
    assert_eq!(m.draft, before, "and the editor keeps the last good draft");
    assert!(
        manager(&harness).settings_open,
        "the refusal leaves the editor open for the fix"
    );
}

/// A refusal that is not a *value* refusal — the watcher — is the typed message, painted.
#[test]
fn a_watch_refusal_is_reported_not_swallowed() {
    let root = tree("12c-watch");
    let mut harness = manager_harness("12c-watch", &root);
    open_manager(&mut harness);
    edit_manager(&mut harness, |m| {
        m.requests.push(ManagerRequest::WatchStart);
    });
    common::settle_until(&mut harness, "the watch answer", |h| {
        let m = manager(h);
        m.watch_error.is_some() || m.watch.running
    });
    let m = manager(&harness);
    let running = harness.state().session.watch_is_running();
    assert_eq!(m.watch.running, running, "the panel agrees with the engine");
    if !running {
        // No `watch` feature in this build (or no inotify backend): the refusal is the typed
        // `CoreError::Watcher`, and it is painted rather than swallowed.
        let err = m.watch_error.unwrap_or_default();
        assert!(!err.is_empty(), "the watcher's reason is painted: {err}");
    }
}

/// <kbd>Escape</kbd> closes the dialog and cancels the rebuild it started: a dismissed manager must
/// not leave a walk running behind the shell, and an interrupted rebuild keeps the old index
/// (plan §7 row 12), which here means an empty one.
#[test]
fn escape_cancels_a_running_rebuild() {
    let root = big_tree("12c-cancel");
    let mut harness = manager_harness("12c-cancel", &root);
    open_manager(&mut harness);
    edit_manager(&mut harness, |m| m.requests.push(ManagerRequest::Rebuild));
    // one frame to drain the request, then interrupt whatever is walking
    harness.run();
    assert!(
        manager(&harness).rebuild.is_some(),
        "the dialog kept the job it started"
    );
    press_key(&mut harness, Key::Escape);
    assert!(
        harness.state().dialogs.active().is_none(),
        "Escape closes the manager"
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    while harness.state().session.index_is_running() && std::time::Instant::now() < deadline {
        harness.run();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        !harness.state().session.index_is_running(),
        "the rebuild reached a terminal state"
    );
    let status = harness.state().session.index_status();
    assert!(
        usize::try_from(status.total_files).unwrap_or(usize::MAX) < BIG_TOTAL,
        "the walk was interrupted, so the shadow table never swapped: {} rows",
        status.total_files
    );
    assert!(
        status.error.is_none()
            || status
                .error
                .as_deref()
                .is_some_and(|e| e.contains("cancel")),
        "a cancel is reported as a cancellation, not a failure: {:?}",
        status.error
    );
    assert!(status.is_dirty, "and the index is still dirty");
}

/// The pill scanner: Go's three operators, quoted values, and the operators it does not have.
#[test]
fn pills_are_go_operators_and_nothing_else() {
    let pills = search_manager::scan_pills("report ext:txt in:\"my folder\" type:dir size:10");
    let labels: Vec<String> = pills.iter().map(|p| p.label()).collect();
    assert_eq!(labels, vec!["ext:txt", "in:my folder", "type:dir"]);
    assert!(
        search_manager::scan_pills("size:10").is_empty(),
        "`size:` is ordinary text in Go, so it is ordinary text here"
    );
    assert_eq!(
        search_manager::scan_pills("next ext:txt").len(),
        1,
        "the word `next` is not an operator"
    );
    assert!(
        search_manager::scan_pills("text:1").is_empty(),
        "`text:` is not `ext:` with a tail"
    );
    // removing a chip is an exact edit of the field
    let session_dir = tree("12c-pills");
    let session = index_session("12c-pills", &session_dir);
    let mut m = SearchManager::new(
        PaneSlot::Left,
        session.index_config(),
        session.index_status(),
    );
    m.query = "leaf ext:txt type:dir".to_string();
    m.sync_pills();
    assert_eq!(m.pills.len(), 2);
    m.remove_pill(0);
    assert_eq!(m.query, "leaf type:dir", "only the chip's own token went");
    m.remove_pill(0);
    assert_eq!(m.query, "leaf ", "and the word survived both");
}

/// `SearchManager::deliver` is the only painter of rows, and it needs the dialog's own token.
#[test]
fn an_answer_without_a_token_paints_nothing() {
    let root = tree("12c-token");
    let session = index_session("12c-token", &root);
    let results = IndexSearchResults {
        entries: Vec::new(),
        total_matched: 0,
        capped: false,
        index_dirty: false,
        last_indexed_unix: 0,
    };
    let mut m = SearchManager::new(
        PaneSlot::Left,
        session.index_config(),
        session.index_status(),
    );
    let origin = session.next_origin();
    assert!(
        !m.deliver(origin, &results),
        "a dialog that asked nothing accepts nothing"
    );
    m.origin = Some(origin);
    assert!(
        m.deliver(origin, &results),
        "the token it asked with is accepted"
    );
}

/// <kbd>Enter</kbd> on a row navigates the pane the dialog was opened from — the answer leaves
/// through `ZenApp::answer`, the same one-way seam the chords and the menus use.
#[test]
fn enter_on_a_row_navigates_through_the_answer_seam() {
    let root = tree("12c-activate");
    let mut harness = manager_harness("12c-activate", &root);
    open_manager(&mut harness);
    rebuild_through_dialog(&mut harness);
    let dirs = query(&mut harness, "type:dir", 50);
    assert!(!dirs.hits.is_empty(), "the fixture tree has directories");
    let target = dirs
        .hits
        .first()
        .expect("a non-empty table")
        .rel_path
        .clone();
    edit_manager(&mut harness, |m| m.cursor = Some(0));
    press_key(&mut harness, Key::Enter);
    assert!(
        harness.state().dialogs.active().is_none(),
        "the answer closes the manager"
    );
    let shown = harness
        .state()
        .model
        .panes
        .get(&PaneSlot::Left)
        .and_then(|p| p.active_tab())
        .expect("the pane has a shown tab");
    assert_eq!(
        shown.path,
        FsPath::from_abs(abs(Path::new(&target))),
        "a directory row navigated the pane to the index's absolute `rel_path`"
    );
    assert!(
        !shown.title.is_empty(),
        "and the tab took the new title ({})",
        shown.title
    );
}
