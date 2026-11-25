//! Step 15b: the side panels (plan §5.8 `panels/side_panel.rs`; defects D8/D9/D10 of §9.2
//! step-15a).
//!
//! The panels are a *view* of `Session::places`, so the tests are the three things the drawers now
//! promise: a favourite that survives the settings file, a place that navigates the real pane seam,
//! an unreachable place that says so, and a history panel that speaks Go's language.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

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
fn a_place_row_opens_a_tab_in_a_pane_that_has_none() {
    let tag = "side-navigate-empty";
    let dir = common::listing_tree(tag);
    let mut app = app(tag, &dir);
    // A second pane with no tab at all — the shape a fresh split used to arrive in.
    app.set_layout(panes_for(LayoutMode::Split));
    let ids: Vec<_> = app.model.panes[&PaneSlot::Right]
        .tabs
        .iter()
        .map(|tab| tab.id)
        .collect();
    app.model
        .panes
        .get_mut(&PaneSlot::Right)
        .expect("the split has a right pane")
        .close_tabs(&ids);
    assert!(app.model.panes[&PaneSlot::Right].tabs.is_empty());
    app.model.active_pane = PaneSlot::Right;

    let places = app.session.places();
    let disk = places
        .disks
        .first()
        .unwrap_or_else(|| panic!("there is always a root filesystem"));
    // Go's `navigateTo` bailed here (`navigation.js:55-58`) and the click was simply lost; the port
    // opens the tab the click asked for.
    app.run_side_panel_command(
        SidePanelCommand::navigate(&disk.mount_point).expect("a mount point is absolute"),
    );
    assert_eq!(
        shown_path(&app, PaneSlot::Right),
        Some(FsPath::from_abs(abs(Path::new(&disk.mount_point)))),
        "the drive row opened its directory in the empty pane"
    );
    assert_eq!(
        app.model.active_pane,
        PaneSlot::Right,
        "and stayed where the user had focused"
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
    // The cut is pixel-measured now, so it needs a real font: an immediate-mode harness (no
    // renderer, no window), with the three cases collected during the frame and asserted after it.
    let tokens = Tokens::dark();
    let long = "/run/media/jang/VERY-LONG-VOLUME-NAME-that-will-not-fit-a-250px-panel";
    let width = tokens.left_drawer_width - (tokens.drawer_pad_x + tokens.drawer_row_pad_x) * 2.0;
    let rows = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    let sink = rows.clone();
    let mut harness = Harness::builder()
        .with_size(egui::vec2(tokens.left_drawer_width, 200.0))
        .with_context_setup(|ctx| {
            zenman_ui::theme::fonts::install(ctx);
        })
        .build_ui(move |ui| {
            let mut sink = sink.borrow_mut();
            if !sink.is_empty() {
                return;
            }
            for (text, width) in [(long, width), (long, 1.0), ("Home", width)] {
                sink.push(side_panel::ellipsize_to_width(ui, text, width, &tokens));
            }
        });
    harness.run();
    let rows = rows.borrow();
    let (cut, tiny, whole) = (&rows[0], &rows[1], &rows[2]);
    assert!(cut.chars().count() < long.chars().count(), "{cut}");
    assert!(cut.ends_with('…'), "and it is marked as cut: {cut}");
    assert!(
        tiny.chars().count() <= 2,
        "a 1-point column still produces a row, not an overflow: {tiny}"
    );
    assert_eq!(whole, "Home", "a short label is untouched");
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
// --------------------------------------------------------------------------- the folder menu
//
// Go hangs one `contextmenu` handler on every `.sidebar-item` — places (`sidebar.js:32-37`), bookmarks
// (`:90-95`), mounts (`:129-134`) — and all three call `showFolderContextMenu`, whose item set is
// `Open in New Tab` ─ `Copy Path` `Copy Name` (`context-menu.js:146-160`). A right-click on a row is
// therefore about **the row's folder**, never about the pane's selection or its directory.

/// The whole shell, with the left drawer open.
fn drawer_harness(app: ZenApp) -> Harness<'static, ZenApp> {
    let mut harness = Harness::builder()
        .with_accessibility_check(true)
        .with_size(egui::vec2(1200.0, 800.0))
        .with_max_steps(64)
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
    harness.run();
    harness
}

/// The centre of a drawer row's a11y node, by label.
///
/// The label alone is not a key: a bookmark called `listing` shares its text with the tab title and the
/// breadcrumb chip of the same directory. The drawer's own band is what distinguishes it — everything
/// left of `Tokens::left_drawer_width` is the drawer, and a row of it is the only thing a click there can
/// reach.
fn row_at(harness: &mut Harness<'static, ZenApp>, label: &str) -> egui::Pos2 {
    let width = Tokens::dark().left_drawer_width;
    let rects: Vec<_> = harness
        .query_all_by_label(label)
        .map(|node| node.rect())
        .filter(|rect| rect.max.x <= width)
        .collect();
    assert_eq!(
        rects.len(),
        1,
        "exactly one row of the drawer (x <= {width}) is called {label:?}"
    );
    rects[0].center()
}

fn is_open(harness: &Harness<'static, ZenApp>) -> bool {
    harness.state().menu.is_some()
}

/// Choose an open menu's row: press and release in **separate** frames, because one frame only records
/// the press — and the press is also what closes a menu whose item is chosen, so a single-frame
/// `Queryable::click` never reaches the row.
fn click_row(harness: &mut Harness<'static, ZenApp>, label: &str) {
    let at = harness.get_by_label(label).rect().center();
    for pressed in [true, false] {
        harness.input_mut().events.push(egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        });
        harness.run();
    }
}

fn right_click(harness: &mut Harness<'static, ZenApp>, at: egui::Pos2) {
    for pressed in [true, false] {
        harness.input_mut().events.push(egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Secondary,
            pressed,
            modifiers: egui::Modifiers::default(),
        });
    }
    harness.run();
    harness.run();
}

/// The labels of the open menu, in order — the same read `tests/menus.rs` uses, so an item that is
/// only ever painted and never routed cannot pass for one.
fn labels(harness: &Harness<'static, ZenApp>) -> Vec<String> {
    harness
        .state()
        .menu_specs()
        .iter()
        .flatten()
        .map(|spec| spec.label.clone())
        .collect()
}

/// Every left-drawer row answers a right-click with the folder menu, for **its own** path — and a
/// right-click is not a navigation, so the pane behind it is untouched.
#[test]
fn right_clicking_a_drawer_row_opens_the_folder_menu_for_that_row() {
    let tag = "side-folder-menu";
    let dir = common::listing_tree(tag);
    let mut app = app(tag, &dir);
    let places = app.session.places();
    let home = places
        .user
        .iter()
        .find(|p| p.kind == PlaceKind::Home && p.exists)
        .expect("Home is a real place here");
    let disk = places.disks.first().expect("there is a root filesystem");
    app.session
        .add_favourite(&dir.display().to_string())
        .unwrap_or_else(|e| panic!("add: {}", e.user_message()));
    let favourites = app.session.places().favourites;
    let wanted: Vec<(&str, String, String)> = vec![
        ("place", home.label.clone(), home.path.clone()),
        (
            "bookmark",
            favourites[0].label.clone(),
            favourites[0].path.clone(),
        ),
        ("drive", disk.label.clone(), disk.mount_point.clone()),
    ];

    app.model.left_drawer.open = true;
    let listing = shown_path(&app, PaneSlot::Left).expect("the tab has a directory");
    let mut harness = drawer_harness(app);
    for (kind, label, path) in wanted {
        let at = row_at(&mut harness, &label);
        right_click(&mut harness, at);
        let open =
            harness.state().menu.clone().unwrap_or_else(|| {
                panic!("right-clicking the {kind} row {label:?} opened no menu")
            });
        assert_eq!(
            open.target,
            zenman_ui::menus::MenuTarget::Folder,
            "a {kind} row opens the folder menu"
        );
        assert_eq!(
            open.folder
                .and_then(|folder| folder.host_abs().map(|p| p.as_path().display().to_string())),
            Some(path.clone()),
            "and it is about {path:?}, the row's own path"
        );
        assert_eq!(
            shown_path(harness.state(), PaneSlot::Left),
            Some(listing.clone()),
            "a right-click never navigates ({label:?})"
        );
        // Escape is the shell's own way out of a menu (`app.js:276`).
        harness.input_mut().events.push(egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });
        harness.run();
        assert!(
            harness.state().menu.is_none(),
            "the menu of the row just tested is closed before the next one"
        );
    }
}

/// The menu a drawer row raises is the reference's, item for item and in its order, painted and not
/// merely routed. What the items *do* when chosen is the folder menu's own behaviour — asserted
/// end-to-end for the row's path by [`the_drawer_menu_opens_the_row_in_a_new_tab`] below, and its copy
/// half by `tests/menus.rs::the_folder_menu_copies_the_chip_and_not_the_selection`, which drives the
/// identical menu opened from a breadcrumb chip.
#[test]
fn the_drawer_menu_offers_the_folder_items_in_gos_order() {
    let tag = "side-folder-items";
    let dir = common::listing_tree(tag);
    let mut app = app(tag, &dir);
    let disk = app
        .session
        .places()
        .disks
        .first()
        .cloned()
        .expect("a root filesystem");
    app.model.left_drawer.open = true;
    let mut harness = drawer_harness(app);

    let at = row_at(&mut harness, &disk.label);
    right_click(&mut harness, at);
    assert_eq!(
        labels(&harness),
        vec![
            "Open in new tab".to_string(),
            "Copy path".to_string(),
            "Copy name".to_string(),
        ],
        "Go's `Open in New Tab` ─ `Copy Path` `Copy Name`, and nothing else"
    );
    for label in ["Open in new tab", "Copy path", "Copy name"] {
        assert_eq!(
            harness.query_all_by_label(label).count(),
            1,
            "{label} is drawn, not only routed"
        );
    }
}

/// `Open in new tab` on a row is Go's `open-path-in-new-tab` → `createTab(paneId, targetPath)`: a new
/// tab, listing **the row's** folder — the mouse all the way through, from the row's right-click to the
/// shell's folder dispatch.
#[test]
fn the_drawer_menu_opens_the_row_in_a_new_tab() {
    let tag = "side-folder-tab";
    let dir = common::listing_tree(tag);
    let mut app = app(tag, &dir);
    let disk = app
        .session
        .places()
        .disks
        .first()
        .cloned()
        .expect("a root filesystem");
    app.model.left_drawer.open = true;
    let mut harness = drawer_harness(app);
    let before = harness.state().model.panes[&PaneSlot::Left].tabs.len();

    let at = row_at(&mut harness, &disk.label);
    right_click(&mut harness, at);
    click_row(&mut harness, "Open in new tab");

    let pane = &harness.state().model.panes[&PaneSlot::Left];
    assert_eq!(pane.tabs.len(), before + 1, "one tab was added");
    assert_eq!(
        pane.active_tab().map(|tab| tab.path.to_string()),
        Some(disk.mount_point.clone()),
        "and it lists the folder of the row, not the pane's own directory"
    );
    assert!(!is_open(&harness), "a chosen item closes the menu");
}
