//! Step 15b: the side panels (plan §5.8 `panels/side_panel.rs`; defects D8/D9/D10 of §9.2
//! step-15a).
//!
//! The panels are a *view* of `Session::places`, so the tests are the three things the drawers now
//! promise: a favourite that survives the settings file, a place that navigates the real pane seam,
//! an unreachable place that says so, and a history panel that speaks Go's language.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use zenman_core::ops::delete::DeleteReq;
use zenman_core::ops::mkdir::MkdirReq;
use zenman_core::path::{AbsPath, FsPath};
use zenman_core::services::places::PlaceKind;
use zenman_core::store::config::{LayoutMode, PaneSlot, ZenConfig};

use zenman_ui::app::ZenApp;
use zenman_ui::panels::action_log;
use zenman_ui::panels::side_panel::{self, SidePanelCommand};
use zenman_ui::state::panes_for;
use zenman_ui::theme::Tokens;

fn app(tag: &str, dir: &Path) -> ZenApp {
    common::remove_tree(
        &std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("zenman-ui-fixtures")
            .join(format!("zenman-ui-test-{tag}")),
    );
    let session = common::session(tag);
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Single));
    app.open_tab(PaneSlot::Left, FsPath::from_abs(abs(dir)));
    app
}

fn abs(path: &Path) -> AbsPath {
    AbsPath::from_path(path.display().to_string().as_str())
        .unwrap_or_else(|e| panic!("{}: {}", path.display(), e.user_message()))
}

fn shown_path(app: &ZenApp, slot: PaneSlot) -> Option<FsPath> {
    app.model
        .panes
        .get(&slot)
        .and_then(|pane| pane.active_tab())
        .map(|tab| tab.path.clone())
}

/// Wait for a job (the permanent delete) to land: core coalesces on a wall clock (plan §9.2 step 9).
fn settle(mut probe: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        if probe() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the engine never answered within 4 s");
}

#[test]
fn a_favourite_round_trips_through_the_settings_file_and_a_reload() {
    let tag = "side-favourites";
    let dir = common::listing_tree(tag);
    let app = app(tag, &dir);
    let config = app.session.paths().config.clone();
    let before = app.session.favourites().len();
    let path = dir.display().to_string();

    let after = app
        .session
        .add_favourite(&path)
        .unwrap_or_else(|e| panic!("add: {}", e.user_message()));
    assert_eq!(after.len(), before + 1, "the list grew by one");
    assert!(
        after.iter().any(|b| b.path == path),
        "and holds the path we handed it: {after:?}"
    );

    // The write really is the settings file, not just the in-memory copy: `ZenConfig::load` is what
    // `Session::open` reads at boot, so a reload sees exactly this.
    let reloaded =
        ZenConfig::load(&config).unwrap_or_else(|e| panic!("reload: {}", e.user_message()));
    assert!(
        reloaded.bookmarks.iter().any(|b| b.path == path),
        "a reload sees the favourite: {:?}",
        reloaded.bookmarks
    );
    assert_eq!(
        reloaded.bookmarks,
        app.session.favourites(),
        "and agrees with the live session"
    );

    // Go deduplicates on add (`api/places.go:151-159`).
    let again = app
        .session
        .add_favourite(&format!("{path}/"))
        .unwrap_or_else(|e| panic!("add again: {}", e.user_message()));
    assert_eq!(
        again.len(),
        after.len(),
        "a trailing slash is not a second favourite"
    );

    let gone = app
        .session
        .remove_favourite(&path)
        .unwrap_or_else(|e| panic!("remove: {}", e.user_message()));
    assert_eq!(gone.len(), before, "the row is gone from the live list");
    let reloaded =
        ZenConfig::load(&config).unwrap_or_else(|e| panic!("reload: {}", e.user_message()));
    assert_eq!(reloaded.bookmarks, gone, "and from the file");
}

#[test]
fn a_relative_favourite_is_refused_rather_than_stored() {
    let tag = "side-relative";
    let dir = common::listing_tree(tag);
    let app = app(tag, &dir);
    let before = app.session.favourites().len();
    let answer = app.session.add_favourite("relative/way");
    assert!(answer.is_err(), "Go's path field is an absolute path");
    assert_eq!(
        app.session.favourites().len(),
        before,
        "and a refusal changed nothing"
    );
}

#[test]
fn a_place_row_navigates_the_active_pane_through_the_pane_seam() {
    let tag = "side-navigate";
    let dir = common::listing_tree(tag);
    let mut app = app(tag, &dir);
    let places = app.session.places();
    let home = places
        .user
        .iter()
        .find(|p| p.kind == PlaceKind::Home && p.exists)
        .unwrap_or_else(|| panic!("Home is a real place here: {:?}", places.user));
    let command = SidePanelCommand::navigate(&home.path).expect("Home is an absolute path");
    app.run_side_panel_command(command);
    assert_eq!(
        shown_path(&app, PaneSlot::Left),
        Some(FsPath::from_abs(abs(Path::new(&home.path)))),
        "the drawer drove the tab through `PaneCommand::Navigate`"
    );
    // …and the mounted drives navigate the same way.
    let disk = places
        .disks
        .first()
        .unwrap_or_else(|| panic!("there is always a root filesystem"));
    app.run_side_panel_command(
        SidePanelCommand::navigate(&disk.mount_point).expect("a mount point is absolute"),
    );
    assert_eq!(
        shown_path(&app, PaneSlot::Left),
        Some(FsPath::from_abs(abs(Path::new(&disk.mount_point)))),
        "a disk row navigates too"
    );
}

#[test]
fn an_unreachable_place_is_a_typed_state_not_a_dead_row() {
    let tag = "side-missing";
    let dir = common::listing_tree(tag);
    let app = app(tag, &dir);
    let missing = dir.join("no-such-place");
    app.session
        .add_favourite(&missing.display().to_string())
        .unwrap_or_else(|e| panic!("add: {}", e.user_message()));
    let places = app.session.places();
    let row = places
        .favourites
        .iter()
        .find(|p| p.path == missing.display().to_string())
        .expect("the row is still listed");
    assert!(!row.exists, "it knows the place is not there");
    let note = side_panel::place_note(row).expect("and it says why");
    assert!(note.starts_with("unreachable"), "the wording: {note}");
    assert!(
        SidePanelCommand::navigate(&row.path).is_some(),
        "the row still answers — the pane shows core's own typed directory error"
    );
}

#[test]
fn a_drawer_row_is_ellipsised_to_the_panel_width_it_actually_has() {
    let tokens = Tokens::dark();
    let long = "/run/media/jang/VERY-LONG-VOLUME-NAME-that-will-not-fit-a-250px-panel";
    let width = tokens.left_drawer_width - tokens.row_pad_x * 2.0;
    let short = side_panel::ellipsize_to_width(long, width, &tokens);
    assert!(short.chars().count() < long.chars().count(), "{short}");
    assert!(short.ends_with('…'), "and it is marked as cut: {short}");
    assert!(
        side_panel::ellipsize_to_width(long, 1.0, &tokens)
            .chars()
            .count()
            <= 2,
        "a 1-point column still produces a row, not an overflow"
    );
    assert_eq!(
        side_panel::ellipsize_to_width("Home", width, &tokens),
        "Home",
        "a short label is untouched"
    );
}

#[test]
fn the_history_panel_uses_gos_vocabulary_and_a_real_permanent_record() {
    let tag = "side-history";
    let dir = common::listing_tree(tag);
    let mut app = app(tag, &dir);
    // The fixture tree persists between runs (`common`), so the row this test makes is cleared first.
    let _ = std::fs::remove_dir(dir.join("side-kept"));
    // A reversible record…
    app.session
        .mkdir(
            &MkdirReq::new(abs(&dir), "side-kept"),
            app.session.next_origin(),
        )
        .unwrap_or_else(|e| panic!("mkdir: {}", e.user_message()));
    // …and a genuine permanent one: `Session::delete` with the trash ladder skipped, which is the
    // only record core logs as not reversible.
    let victim = dir.join("side-victim.txt");
    let _ = std::fs::remove_file(&victim);
    std::fs::write(&victim, b"gone").expect("a file to delete");
    app.session
        .delete(
            &DeleteReq::permanent(vec![abs(&victim)]),
            app.session.next_origin(),
        )
        .unwrap_or_else(|e| panic!("delete: {}", e.user_message()));
    app.model.log_panel = true;
    settle(|| {
        app.log.mark_dirty();
        action_log::tick(&mut app);
        app.log.page.rows.iter().any(|r| !r.reversible)
    });
    let rows = &app.log.page.rows;
    let permanent = rows
        .iter()
        .find(|r| !r.reversible)
        .expect("the permanent delete is logged as irreversible");
    assert_eq!(
        action_log::trailing_marker(permanent),
        Some(action_log::PERMANENT),
        "and the row shows Go's `permanent` marker"
    );
    let undoable = rows
        .iter()
        .find(|r| r.can_revert)
        .expect("the mkdir can be undone");
    assert_eq!(
        action_log::trailing_marker(undoable),
        None,
        "that one shows Undo"
    );
    assert!(
        permanent.ago.ends_with(" ago"),
        "Go's relative time, not an RFC 3339 stamp: {:?}",
        permanent.ago
    );
    assert_eq!(action_log::TITLE, "Action History", "Go's title");
    assert!(
        action_log::DESCRIPTION.contains("Reversible actions"),
        "Go's description line"
    );
    assert_eq!(action_log::row_icon("mkdir"), "folder", "a per-type icon");
    assert_eq!(action_log::row_icon("delete"), "trash");
    assert_ne!(
        action_log::row_icon("mkdir"),
        action_log::row_icon("rename"),
        "and they differ by type"
    );
}
