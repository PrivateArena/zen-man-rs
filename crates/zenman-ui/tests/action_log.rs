//! Step 14b: the action-log panel (plan §7 row 14, §5.8 `panels/action_log.rs`).
//!
//! The panel is a page over `Session::log_query`, so the tests are pages, a revert that really
//! changes the filesystem, and the two refusals core answers with typed `Err`s.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

use zenman_core::ops::delete::DeleteReq;
use zenman_core::ops::mkdir::MkdirReq;
use zenman_core::path::{AbsPath, FsPath};
use zenman_core::store::config::{LayoutMode, PaneSlot};
use zenman_ui::app::ZenApp;
use zenman_ui::panels::action_log::{self, LogCommand, LogPanel};
use zenman_ui::state::panes_for;

fn scratch(tag: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("zenman-ui-fixtures")
        .join(format!("zenman-ui-fixture-{tag}"))
}

fn app(tag: &str, dir: &Path) -> ZenApp {
    common::remove_tree(
        &std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("zenman-ui-fixtures")
            .join(format!("zenman-ui-test-{tag}")),
    );
    let session = common::session(tag);
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Single));
    app.open_tab(
        PaneSlot::Left,
        FsPath::from_abs(AbsPath::from_path(dir).expect("an absolute fixture path")),
    );
    app
}

/// `mkdir` one row and return its log id (the newest record, read through the panel's own seam).
fn mkdir_and_id(app: &mut ZenApp, dir: &AbsPath, name: &str) -> i64 {
    let origin = app.session.next_origin();
    app.session
        .mkdir(&MkdirReq::new(dir.clone(), name), origin)
        .unwrap_or_else(|e| panic!("mkdir {name}: {}", e.user_message()));
    refresh(app, 0);
    app.log
        .page
        .rows
        .first()
        .unwrap_or_else(|| panic!("the mkdir of {name} logged nothing"))
        .id
}

/// Drive the panel's own refresh (event-dirty or poll-due), exactly as `logic` does.
fn refresh(app: &mut ZenApp, offset: usize) {
    app.model.log_panel = true;
    app.log.offset = offset;
    app.log.mark_dirty();
    action_log::tick(app);
}

/// Wait for a job (the revert) to land: core coalesces on a wall clock, so this sleeps (plan §9.2
/// step 9's `settle_until` reasoning, without the harness because no frame is needed).
fn settle(mut probe: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if probe() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the engine never answered within 2 s");
}

#[test]
fn the_log_pages_newest_first_and_reports_has_more() {
    let root = scratch("log-pages");
    common::remove_tree(&root);
    std::fs::create_dir_all(&root).expect("fixture directory");
    let dir = AbsPath::from_path(&root).expect("absolute");
    let mut app = app("log-pages", &root);
    for name in ["d1", "d2", "d3", "d4"] {
        mkdir_and_id(&mut app, &dir, name);
    }
    app.log.limit = 2;
    refresh(&mut app, 0);
    let labels: Vec<String> = app.log.page.rows.iter().map(|r| r.label.clone()).collect();
    assert_eq!(labels.len(), 2, "the page ignored its limit: {labels:?}");
    assert!(labels[0].contains("d4"), "newest-first broke: {labels:?}");
    assert!(
        labels[1].contains("d3"),
        "the page skipped a record: {labels:?}"
    );
    assert!(
        app.log.page.has_more,
        "older records exist and `has_more` said no"
    );
    assert!(
        app.log.page.total >= 4,
        "total under-reported: {}",
        app.log.page.total
    );

    // The panel's own paging command, then a refresh.
    app.log.apply(LogCommand::Older);
    action_log::tick(&mut app);
    let labels: Vec<String> = app.log.page.rows.iter().map(|r| r.label.clone()).collect();
    assert!(
        labels.iter().any(|l| l.contains("d2")),
        "the older page lost d2: {labels:?}"
    );
    assert!(
        labels.iter().any(|l| l.contains("d1")),
        "the older page lost d1: {labels:?}"
    );
    assert!(!app.log.page.has_more, "the last page still claims more");
    app.log.apply(LogCommand::Newer);
    assert_eq!(
        app.log.offset, 0,
        "`Newer` did not walk back to the newest page"
    );
}

#[test]
fn reverting_through_the_panel_changes_the_filesystem_and_marks_the_record() {
    let root = scratch("log-revert");
    common::remove_tree(&root);
    std::fs::create_dir_all(&root).expect("fixture directory");
    let dir = AbsPath::from_path(&root).expect("absolute");
    let mut app = app("log-revert", &root);
    let id = mkdir_and_id(&mut app, &dir, "made-to-undo");
    assert!(
        root.join("made-to-undo").is_dir(),
        "the mkdir itself failed"
    );
    refresh(&mut app, 0);
    let row = &app.log.page.rows[0];
    assert_eq!(row.id, id);
    assert!(
        row.can_revert,
        "a mkdir is reversible and the panel said otherwise"
    );

    app.revert_record(id);
    settle(|| !root.join("made-to-undo").exists());
    settle(|| {
        refresh(&mut app, 0);
        app.log.page.rows.iter().any(|r| r.id == id && r.reverted)
    });
    let row = app
        .log
        .page
        .rows
        .iter()
        .find(|r| r.id == id)
        .expect("the record vanished from its own page");
    assert!(row.reverted, "the panel still shows the record as live");
    assert!(!row.can_revert, "a reverted record still offers its button");
    assert_eq!(row.status, "reverted");
    assert!(
        app.log.error.is_none(),
        "a clean revert left an error strip: {:?}",
        app.log.error
    );
}

#[test]
fn a_refused_revert_keeps_its_reason_on_screen() {
    let root = scratch("log-refuse");
    common::remove_tree(&root);
    std::fs::create_dir_all(&root).expect("fixture directory");
    let file = root.join("gone.txt");
    std::fs::write(&file, "x\n").expect("fixture file");
    let dir = AbsPath::from_path(&root).expect("absolute");
    let mut app = app("log-refuse", &root);
    let origin = app.session.next_origin();
    app.session
        .delete(
            &DeleteReq::permanent(vec![dir.join("gone.txt").expect("joined")]),
            origin,
        )
        .expect("the permanent delete ran");
    // `delete` is a job (plan §4.1), so its record lands when the job finishes.
    settle(|| {
        refresh(&mut app, 0);
        !app.log.page.rows.is_empty()
    });
    let id = app.log.page.rows[0].id;
    assert!(
        !app.log.page.rows[0].can_revert,
        "Go's permanent delete is not undoable"
    );
    // The refusal still has to be *answerable* — the button is disabled, but a stale click, a
    // scripted call or a record that changed under the panel all land here.
    app.revert_record(id);
    let error = app.log.error.clone().unwrap_or_default();
    assert!(
        error.contains("permanent"),
        "the delete refusal was not surfaced: {error:?}"
    );
    assert!(
        app.notices
            .queued()
            .iter()
            .any(|n| n.text.contains("permanent")),
        "the refusal reached the panel but no toast"
    );
}

#[test]
fn reverting_twice_names_the_record_as_already_reverted() {
    let root = scratch("log-twice");
    common::remove_tree(&root);
    std::fs::create_dir_all(&root).expect("fixture directory");
    let dir = AbsPath::from_path(&root).expect("absolute");
    let mut app = app("log-twice", &root);
    let id = mkdir_and_id(&mut app, &dir, "undo-twice");
    app.revert_record(id);
    settle(|| !root.join("undo-twice").exists());
    settle(|| {
        app.log.error = None;
        app.revert_record(id);
        app.log
            .error
            .as_deref()
            .is_some_and(|e| e.contains("already"))
    });
    assert!(
        app.log
            .error
            .clone()
            .unwrap_or_default()
            .contains("already")
    );
}

#[test]
fn an_empty_log_says_so_in_words() {
    let root = scratch("log-empty");
    common::remove_tree(&root);
    std::fs::create_dir_all(&root).expect("fixture directory");
    let mut app = app("log-empty", &root);
    app.model.log_panel = true;
    let mut harness = Harness::builder()
        .with_size(egui::vec2(1400.0, 800.0))
        .with_max_steps(64)
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
    assert_eq!(
        harness.state().log.page.total,
        0,
        "the fixture logged something"
    );
    assert!(
        harness
            .query_all_by_label("No actions yet — nothing has been renamed, moved or deleted.")
            .count()
            > 0,
        "the drawer's empty state was not drawn"
    );
    let _ = LogPanel::default();
}
