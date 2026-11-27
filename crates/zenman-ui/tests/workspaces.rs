//! Step 14b: the workspace vocabulary (plan §7 row 14, §9.2 step-8c review gap 1) and the `Saved`
//! report behind it — which lives in the status bar, not in the tab row (`panels::status_bar`).
//!
//! Go's header was `Workspace: <select>` · `New` · `Delete` · `Saved` · disk meter. It is now one
//! button and its popup (`panels::header`), plus the navigation ribbon's window verbs. This file
//! asserts the names are on screen, that every *interactive* one is reachable at 800 px (plan §5.8's
//! criterion, restated for the chrome that replaced the header), and that the workspace verbs really
//! move state through `Session` — not just the toast.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui::Vec2;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

use zenman_core::path::{AbsPath, FsPath};
use zenman_core::services::workspaces::{Autosave, SAVED_INDICATOR_TTL};
use zenman_core::store::config::{LayoutMode, PaneSlot};
use zenman_ui::app::ZenApp;
use zenman_ui::dialogs::{Answer, Choice, DialogKind};
use zenman_ui::panels::header::HeaderCommand;
use zenman_ui::state::panes_for;

const SETTLE: usize = 6;

fn tree(tag: &str, files: &[&str]) -> PathBuf {
    let root = scratch(tag);
    common::remove_tree(&root);
    std::fs::create_dir_all(&root).expect("fixture directory");
    for name in files {
        std::fs::write(root.join(name), format!("{name}\n")).expect("fixture file");
    }
    root
}

fn scratch(tag: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("zenman-ui-fixtures")
        .join(format!("zenman-ui-fixture-{tag}"))
}

fn build(app: ZenApp, size: Vec2) -> Harness<'static, ZenApp> {
    let mut harness = Harness::builder()
        .with_size(size)
        .with_max_steps(64)
        .with_accessibility_check(true)
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
    for _ in 0..SETTLE {
        harness.run();
    }
    harness
}

fn pane_app(tag: &str, dir: &Path) -> ZenApp {
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

/// Every chrome control a user can click without opening a popup, by the name it answers to: the
/// workspace button of the tab row, the global group of the navigation ribbon, and the parent menu
/// that folds the modes away (`app_menu.rs`).
const INTERACTIVE: [&str; 5] = ["workspace", "history", "places", "custom actions", "menu"];

/// The tiles behind that parent button.
const MENU_TILES: [&str; 3] = ["Grid view", "2-pane", "4-pane"];

/// Click the first control of one name: every pane carries its own chrome, so a window-level control
/// exists once per pane in a grid, and `get_by_label`'s "exactly one" is the wrong demand.
fn click_first(harness: &mut Harness<'static, ZenApp>, label: &str) {
    let at = harness
        .query_all_by_label(label)
        .map(|node| node.rect().center())
        .next()
        .unwrap_or_else(|| panic!("{label:?} is not in the widget tree"));
    for pressed in [true, false] {
        harness.input_mut().events.push(egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        });
    }
    harness.run();
}

/// How many times the shell drew a chrome string (the a11y tree is what a click can reach).
fn drawn(harness: &Harness<'static, ZenApp>, label: &str) -> usize {
    harness.query_all_by_label(label).count()
}

/// `true` while the pane's parent menu is open, which is the only time its tiles are on screen.
fn menu_is_open(harness: &Harness<'static, ZenApp>) -> bool {
    harness.query_all_by_label("4-pane").count() > 0
}

#[test]
fn the_workspace_button_carries_the_reference_vocabulary() {
    let dir = tree("ws-vocab", &["a.txt"]);
    let mut app = pane_app("ws-vocab", &dir);
    // The popup's rows are the stored workspaces, and a fresh config has none: write the
    // autosave target once so the test sees the same list a running shell shows.
    app.save_workspace_as(zenman_core::services::workspaces::RESERVED);
    let mut harness = build(app, egui::vec2(1600.0, 800.0));
    // Go's `select` + `New` + `Delete` are one button; its vocabulary moved into the popup, and the
    // two verbs of the selected workspace to that popup's right edge.
    assert!(
        drawn(&harness, "workspace") > 0,
        "the workspace control is missing from the tab row"
    );
    click_first(&mut harness, "workspace");
    for want in ["New workspace", "Rename workspace", "Delete workspace"] {
        assert!(
            drawn(&harness, want) > 0,
            "the workspace popup is missing {want:?}: {:?}",
            harness.query_all_by_label("").count()
        );
    }
    // `Default` is present and flagged, not hidden (Go hid it and so made it unrestorable).
    let names = harness.state().workspaces.names.clone();
    assert!(
        names.iter().any(|meta| meta.reserved),
        "the reserved autosave workspace is missing from the popup: {names:?}"
    );
    assert!(
        drawn(&harness, "default (autosave)") > 0,
        "and the popup does not show it with its flag"
    );
}

#[test]
fn every_interactive_chrome_control_is_reachable_at_800_px() {
    // Plan §5.8's criterion, restated for the chrome that replaced the header: nothing a user can
    // click is dropped when the window is small. The ribbon's globals and the pane tools both stay
    // inline here — the address strip's floor is what folds them, and 800 px is above it.
    let dir = tree("ws-800", &["a.txt"]);
    let mut harness = build(pane_app("ws-800", &dir), egui::vec2(800.0, 600.0));
    for name in
        INTERACTIVE
            .iter()
            .chain(&["refresh", "recursive listing", "select all", "select none"])
    {
        let mut inside = 0;
        for node in harness.query_all_by_label(name) {
            // A control drawn past the window's edge is named but unreachable: egui still records
            // it, so presence alone would pass. This is the a11y-tree version of a click.
            if node.rect().max.x <= 800.0 && node.rect().min.x >= 0.0 {
                inside += 1;
            }
        }
        assert!(
            inside > 0,
            "{name:?} is not inside the 800 px window — the chrome dropped or pushed out a control"
        );
    }
    // The folded half, too: the panel is anchored to a button in the corner, so this is where a
    // narrow window would otherwise clip it.
    click_first(&mut harness, "menu");
    assert!(
        menu_is_open(&harness),
        "the parent menu did not open in an 800 px window"
    );
    for name in MENU_TILES {
        let inside = harness
            .query_all_by_label(name)
            .filter(|node| node.rect().max.x <= 800.0 && node.rect().min.x >= 0.0)
            .count();
        assert!(
            inside > 0,
            "{name:?} is not inside the 800 px window once the parent menu is open"
        );
    }
    // And they are the grid the panel was sized for: three tiles of the token size, in order, on
    // one line — a panel that wrapped or shrank its tiles would pass the reachability test alone.
    let rects: Vec<egui::Rect> = MENU_TILES
        .iter()
        .map(|name| {
            harness
                .query_all_by_label(name)
                .next()
                .unwrap_or_else(|| panic!("{name:?} has no tile of its own"))
                .rect()
        })
        .collect();
    let tokens = zenman_ui::theme::Tokens::dark();
    for rect in &rects {
        assert_eq!(
            (rect.width(), rect.height()),
            (tokens.menu_tile_width, tokens.menu_tile_height),
            "a tile was not the size the panel's width was derived from"
        );
    }
    assert!(
        rects[0].min.x < rects[1].min.x && rects[1].min.x < rects[2].min.x,
        "the tiles are not in the order the menu lists them: {rects:?}"
    );
    assert!(
        (rects[0].center().y - rects[2].center().y).abs() < 0.5,
        "the tiles wrapped onto more than one row: {rects:?}"
    );

    // Where the reload button ended up: a browser's user reaches for it beside the address, so it
    // rides after `..` and not after the pane tools.
    let rect_of = |label: &str| {
        harness
            .query_all_by_label(label)
            .next()
            .unwrap_or_else(|| panic!("{label:?} is not in the widget tree"))
            .rect()
    };
    let up = rect_of("Parent directory");
    let reload = rect_of("refresh");
    assert!(
        reload.min.x >= up.max.x && reload.min.x - up.max.x < 20.0,
        "reload is not the button right after the parent-directory one: {up:?} then {reload:?}"
    );
}

#[test]
fn the_popup_row_that_is_not_shown_restores_the_session_it_names() {
    // Clicking a workspace row is Go's `select` change: the stored session comes back, and the
    // control follows it. The reserved one and a named one differ in their tab count, which is what
    // makes the two rows distinguishable at all.
    let dir = tree("ws-switch", &["a.txt"]);
    let mut app = pane_app("ws-switch", &dir);
    app.save_workspace_as(zenman_core::services::workspaces::RESERVED);
    app.set_layout(panes_for(LayoutMode::Split));
    app.save_workspace_as("two-pane");
    assert_eq!(
        app.workspaces.selected, "two-pane",
        "the seed did not stick"
    );
    let mut harness = build(app, egui::vec2(1440.0, 900.0));
    click_first(&mut harness, "workspace");
    click_first(&mut harness, "default (autosave)");
    assert_eq!(
        harness.state().workspaces.selected,
        format!("{} (autosave)", zenman_core::services::workspaces::RESERVED),
        "the row click did not move the control"
    );
    assert_eq!(
        harness.state().model.panes.len(),
        1,
        "and did not restore the grid the row stands for"
    );
    // And the control now shows the row that was clicked, flagged exactly as the popup spelled it.
    assert!(
        harness
            .state()
            .workspaces
            .selected_name()
            .eq_ignore_ascii_case(zenman_core::services::workspaces::RESERVED),
        "the control moved to something else: {:?}",
        harness.state().workspaces.selected
    );
}

#[test]
fn the_workspace_verbs_move_the_stored_session_and_say_so() {
    // The popup's three verbs, driven through the shell's own answers: `New` and `Edit` ask for a
    // name in a `Prompt`, and the affirmative answer is what writes.
    let dir = tree("ws-verbs", &["a.txt"]);
    let mut app = pane_app("ws-verbs", &dir);
    let stored = |app: &ZenApp| -> Vec<String> {
        app.session
            .workspaces()
            .into_iter()
            .map(|meta| meta.name)
            .collect()
    };

    app.header_commands(vec![HeaderCommand::Create]);
    assert!(
        app.dialogs.active().is_some(),
        "New workspace asked no name"
    );
    app.answer(Answer::Text {
        kind: DialogKind::NewWorkspace,
        choice: Choice::Ok,
        text: "alpha".into(),
    });
    assert!(stored(&app).contains(&"alpha".into()), "New saved nothing");

    app.header_commands(vec![HeaderCommand::Rename("alpha".into())]);
    assert!(
        app.dialogs.active().is_some(),
        "Edit asked no name for its rename"
    );
    app.answer(Answer::Text {
        kind: DialogKind::RenameWorkspace {
            name: "alpha".into(),
        },
        choice: Choice::Ok,
        text: "beta".into(),
    });
    let names = stored(&app);
    assert!(
        names.contains(&"beta".to_string()) && !names.contains(&"alpha".to_string()),
        "Edit left {names:?}"
    );
    // A rename moves the entry, so the tabs come back with it — which is the whole reason this is
    // `Session::workspace_rename` and not Go's save-then-delete pair.
    app.set_layout(panes_for(LayoutMode::Split));
    app.restore_workspace("beta");
    assert_eq!(
        app.model.layout,
        LayoutMode::Single,
        "the renamed workspace did not carry its grid"
    );
    assert_eq!(
        app.model.panes[&PaneSlot::Left].tabs.len(),
        1,
        "and not its tab"
    );

    // Onto a taken name: refused, reported, and both workspaces still there.
    app.save_workspace_as("gamma");
    let before = stored(&app);
    app.rename_workspace("beta", "gamma");
    assert_eq!(
        stored(&app),
        before,
        "a rename onto an existing name changed the store"
    );
    assert!(
        app.notices
            .queued()
            .iter()
            .any(|n| n.text.contains("beta") && !n.text.contains("renamed")),
        "the refusal said nothing about beta"
    );
}

#[test]
fn new_select_and_delete_a_workspace_through_the_session() {
    let dir = tree("ws-crud", &["a.txt"]);
    let mut app = pane_app("ws-crud", &dir);
    app.save_workspace_as("alpha");
    let names: Vec<String> = app
        .session
        .workspaces()
        .into_iter()
        .map(|m| m.name)
        .collect();
    assert!(
        names.contains(&"alpha".to_string()),
        "New saved nothing: {names:?}"
    );
    assert_eq!(
        app.workspaces.selected, "alpha",
        "the selector did not move to the workspace it just stored"
    );

    // A second pane set, then back: `restore` replaces the tabs with the stored ones.
    app.set_layout(panes_for(LayoutMode::Split));
    app.restore_workspace("alpha");
    assert_eq!(
        app.model.layout,
        LayoutMode::Single,
        "restoring a single-pane workspace left a split grid behind"
    );
    assert!(
        app.notices
            .queued()
            .iter()
            .any(|n| n.text.contains("restored")),
        "a restore said nothing"
    );

    app.delete_workspace("alpha");
    let names: Vec<String> = app
        .session
        .workspaces()
        .into_iter()
        .map(|m| m.name)
        .collect();
    assert!(
        !names.contains(&"alpha".to_string()),
        "Delete left {names:?}"
    );
    // The destructive path asks first (plan D6): the confirm is what carries the answer.
    app.confirm_delete_workspace("beta");
    assert!(app.dialogs.active().is_some(), "Delete did not ask");
}

#[test]
fn a_refused_workspace_name_is_reported_not_swallowed() {
    let dir = tree("ws-bad", &["a.txt"]);
    let mut app = pane_app("ws-bad", &dir);
    // `normalise_name` refuses a name with a separator; the bar shows the reason and saves nothing.
    let before = app.session.workspaces().len();
    app.save_workspace_as("has/a/slash");
    assert_eq!(
        app.session.workspaces().len(),
        before,
        "a name core refuses still reached the file"
    );
    assert!(
        zenman_core::services::workspaces::normalise_name("  ").is_err(),
        "core stopped refusing an empty name"
    );
}

#[test]
fn the_saved_chip_appears_and_then_expires() {
    let dir = tree("ws-chip", &["a.txt"]);
    let mut app = pane_app("ws-chip", &dir);
    // The clock is `workspaces::Autosave`'s, driven by an injected `Instant` (14a's design: core
    // owns the rule, the shell owns the timer).
    let mut clock = Autosave::new(Duration::from_millis(1));
    let now = Instant::now();
    clock.mark_saved(now);
    assert!(
        clock.show_saved(now),
        "a write that just landed shows no chip"
    );
    assert!(
        !clock.show_saved(now + SAVED_INDICATOR_TTL),
        "the chip outlived Go's 2 s"
    );
    // …and the same rule through the real shell, painted. The debounce is pushed out past the
    // test's TTL sleep: an autosave landing during the sleep would legitimately re-light the chip,
    // and the point here is the *expiry* of one that has already landed.
    app.workspaces.autosave = Autosave::new(Duration::from_secs(60));
    app.workspaces.mark_saved(Instant::now());
    let mut harness = build(app, egui::vec2(1600.0, 800.0));
    assert!(
        drawn(&harness, "Saved") > 0,
        "the chip was not drawn after a save"
    );
    std::thread::sleep(SAVED_INDICATOR_TTL + Duration::from_millis(100));
    for _ in 0..SETTLE {
        harness.run();
    }
    assert_eq!(
        drawn(&harness, "Saved"),
        0,
        "the chip is still up after the TTL"
    );
}

/// The tab row is the scarce real estate in the shell, so the write report moved to the bottom band:
/// one pill, drawn once, inside the status bar's 30 px, and clear of the workspace button it used to
/// sit next to.
#[test]
fn the_saved_pill_shares_the_status_bar_and_not_the_tab_row() {
    let dir = tree("ws-bar", &["a.txt"]);
    let mut app = pane_app("ws-bar", &dir);
    app.workspaces.autosave = Autosave::new(Duration::from_secs(60));
    app.workspaces.mark_saved(Instant::now());
    let size = egui::vec2(1600.0, 800.0);
    let harness = build(app, size);
    let pills: Vec<egui::Rect> = harness
        .query_all_by_label("Saved")
        .map(|node| node.rect())
        .collect();
    assert_eq!(
        pills.len(),
        1,
        "the write report is drawn once and in one place"
    );
    let tokens = zenman_ui::theme::Tokens::dark();
    let band = size.y - tokens.status_height;
    assert!(
        pills[0].min.y >= band - 1.0,
        "the pill sat at y {} — above the status band, which starts at {band}",
        pills[0].min.y
    );
    assert!(
        pills[0].max.x <= size.x,
        "the pill hangs off the bar's right edge ({:?})",
        pills[0]
    );
    let button = harness
        .query_all_by_label("workspace")
        .map(|node| node.rect())
        .next()
        .expect("the workspace button is still on the tab row");
    assert!(
        button.max.x <= pills[0].min.x,
        "the tab row is back to button-then-tabs: {:?} vs {:?}",
        button,
        pills[0]
    );
}

#[test]
fn the_autosave_writes_the_reserved_workspace_on_its_debounce() {
    let dir = tree("ws-auto", &["a.txt"]);
    let mut app = pane_app("ws-auto", &dir);
    app.workspaces.autosave = Autosave::new(Duration::from_millis(1));
    // Two ticks: the first notes the state, the second (after the debounce) must write it.
    app.tick_workspaces(Instant::now(), true);
    std::thread::sleep(Duration::from_millis(5));
    app.tick_workspaces(Instant::now(), false);
    let names: Vec<String> = app
        .session
        .workspaces()
        .into_iter()
        .map(|m| m.name)
        .collect();
    assert!(
        names.iter().any(|n| n.eq_ignore_ascii_case("default")),
        "the debounced autosave left {names:?}"
    );
    assert!(app.workspaces.chip_shows(Instant::now()));
}

// ------------------------------------------------------- the session a restart reopens
//
// The autosave half above proves the shell can write a workspace. These prove the reason it writes
// one: `restore_last_session` on a *second* shell over the same `config.json` — the same file, a new
// process, what the user calls "my tabs came back".

use std::collections::BTreeMap;

use zenman_core::session::{Session, SessionPaths};

use zenman_core::services::workspaces::RESERVED;
use zenman_core::store::config::{GroupColor, PaneSession, TabSession, ViewMode, Workspace};
use zenman_ui::panels::header::{Applied, SessionRestore};
use zenman_ui::state::quad_slots;

/// The session directory `common::session(tag)` is about to use.
fn session_tree(tag: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("zenman-ui-fixtures")
        .join(format!("zenman-ui-test-{tag}"))
}

/// A shell over an empty data directory: what the first launch on this folder sees.
fn fresh_shell(tag: &str) -> ZenApp {
    common::remove_tree(&session_tree(tag));
    ZenApp::new(common::session(tag), Vec::new())
}

/// A session opened the way the *binary* opens one: the settings file is read first (that is
/// [`config_boot`]’s job in `apps/zenman`) and handed to `Session::open`, which does not read the file
/// itself. `common::session` starts from the defaults, which is right for every other suite and wrong
/// for a restart — the whole point here is what came off the disk.
fn reopened_session(tag: &str) -> Session {
    let dir = session_tree(tag);
    let config_path = dir.join("config.json");
    let config = zenman_core::store::config::ZenConfig::load(&config_path).unwrap_or_else(|e| {
        panic!(
            "{} must hold the session the first shell wrote: {e}",
            config_path.display()
        )
    });
    let paths = SessionPaths::new(config_path, dir.join("action.log"), None, dir.join("probe"))
        .with_db_open(1, Duration::from_millis(1));
    let (session, _notes) = Session::open(
        paths,
        config,
        std::sync::Arc::new(common::NoDesktopEnv::new())
            as std::sync::Arc<dyn zenman_core::platform::Env>,
        zenman_core::store::config::ZenConfig::default().limits(),
    )
    .expect("a session opens over a written directory");
    session
}

/// A shell over the *same* `config.json`, built after the first one wrote it: the restart.
fn reopened_shell(tag: &str) -> ZenApp {
    ZenApp::new(reopened_session(tag), Vec::new())
}

/// The tab strip of one pane, as stored paths (the thing a user recognises as "my tabs").
fn strip(app: &ZenApp, slot: PaneSlot) -> Vec<String> {
    app.model.panes[&slot]
        .tabs
        .iter()
        .map(|tab| tab.path.to_string())
        .collect()
}

/// Exit the way the binary does: note the state on the frame tick, then flush what the debounce
/// still holds.
fn shutdown(app: &mut ZenApp) {
    app.tick_workspaces(Instant::now(), true);
    app.flush_session(Instant::now())
        .expect("the session file is writable");
}

#[test]
fn the_tabs_that_were_open_are_the_tabs_that_come_back() {
    let first = tree("ws-back-first", &["a.txt"]);
    let second = tree("ws-back-second", &["b.txt"]);
    let other = tree("ws-back-other", &["c.txt"]);

    let third = tree("ws-back-third", &["d.txt"]);
    let mut app = fresh_shell("ws-back");
    app.set_layout(panes_for(LayoutMode::Split));
    let left =
        |dir: &Path| FsPath::from_abs(AbsPath::from_path(dir).expect("an absolute fixture path"));
    app.open_tab(PaneSlot::Left, left(&first));
    app.open_tab(PaneSlot::Left, left(&second));
    app.open_tab(PaneSlot::Left, left(&third));
    app.open_tab(PaneSlot::Right, left(&other));
    app.model.active_pane = PaneSlot::Left;
    // The middle tab: neither the first nor the last, so a restore that recomputed the selection
    // instead of reading it back cannot pass by accident.
    app.model.pane_mut(PaneSlot::Left).active = 1;
    // A **named, folded, green** group of one tab, next to two plain ones: the group record is the
    // state a restart has to bring back (design §4's feature 7), and the two ungrouped tabs are what
    // proves the restore did not group everything that shares a colour. The grid view is the other
    // per-tab field the first restore path dropped on the floor.
    let group = app.model.next_group_id();
    {
        let pane = app.model.pane_mut(PaneSlot::Left);
        let member = pane.tabs[0].id;
        assert!(
            pane.create_group(group, &[member], GroupColor::C).is_some()
                && pane.rename_group(group, "in progress")
                && pane.set_collapsed(group, true),
            "the fixture group does not exist"
        );
        pane.tabs[0].view = ViewMode::Grid;
        pane.debug_assert_invariants();
    }
    shutdown(&mut app);

    let mut back = reopened_shell("ws-back");
    let report = back.restore_last_session();
    assert!(report.restored(), "{report:?}");
    assert_eq!(
        strip(&back, PaneSlot::Left),
        vec![
            first.to_string_lossy(),
            second.to_string_lossy(),
            third.to_string_lossy()
        ],
        "the left strip, in order: {}",
        report.boot_line()
    );
    assert_eq!(
        strip(&back, PaneSlot::Right),
        vec![other.to_string_lossy()],
        "the right pane's tab is part of the session too"
    );
    assert_eq!(
        back.model.layout,
        LayoutMode::Split,
        "the grid that was saved is the grid that came back"
    );
    let tab = &back.model.panes[&PaneSlot::Left].tabs[0];
    assert_eq!(tab.view, ViewMode::Grid, "the view mode was lost");
    // (a) the group itself, as a record: colour, name and fold all survived the file.
    let back_pane = &back.model.panes[&PaneSlot::Left];
    let restored = tab
        .group
        .expect("the grouped tab came back with no group at all");
    let restored_group = back_pane
        .group(restored)
        .expect("the tab names a group the pane does not have (invariant 2)");
    assert_eq!(
        restored_group.color,
        GroupColor::C,
        "the group came back in another colour"
    );
    assert_eq!(
        restored_group.name, "in progress",
        "the group's own name was lost — a renamed group that returns unnamed is a lost rename"
    );
    assert!(
        restored_group.collapsed,
        "the folded group came back unfolded, so the strip grew tabs the user had hidden"
    );
    assert_eq!(
        back_pane.tab_color(tab.group),
        Some(GroupColor::C),
        "the member's accent is not its group's colour"
    );
    // (b) the member tab points at that group and nothing else does. Tab ids are re-minted by the
    // restore (`TabId` is a runtime identity; only the *group* id is adopted), so membership is checked
    // against the tab that came back in the grouped tab's place.
    assert_eq!(
        tab.group,
        Some(restored),
        "the tab's own pointer does not name the group the pane holds"
    );
    assert_eq!(
        back_pane.member_ids(restored),
        vec![back_pane.tabs[0].id],
        "the group's membership is not the tab that was saved in it"
    );
    // (c) the two tabs that were never grouped are still ungrouped.
    assert_eq!(
        back_pane.tabs[1].group, None,
        "the tab beside the group was pulled into it by the restart"
    );
    assert_eq!(back_pane.tabs[2].group, None, "and so was the third one");
    // (d) and the restore left the pane in a legal state, not merely a plausible-looking one.
    back_pane.debug_assert_invariants();
    assert_eq!(
        back.model.panes[&PaneSlot::Left].active,
        1,
        "the selected tab was not the one that was saved"
    );
    assert!(
        !back.model.panes[&PaneSlot::Left].mru.is_empty(),
        "the MRU ring came back empty, so the recent-tabs list would have nothing to walk"
    );
    // The report says what it did, in the words the boot line prints.
    assert!(
        report.boot_line().contains("4 tab(s) in 2 pane(s)"),
        "{}",
        report.boot_line()
    );
}

// ------------------------------------------------------- a Go config that only has the letter
//
// Go never stored a group *record*: the `group` letter was the whole story, and it put the same letter
// on tabs that were not next to each other. Such a file has no `groups` rows and no `groupId` on any
// tab, so the restore has to invent the groups (design §4 step 3) and then write them back as the real
// thing — otherwise the same repair would run again on every start, and the two runs the file
// describes (`B, none, B`) would keep being whatever the restore felt like.

#[test]
fn a_letter_only_go_session_comes_back_as_two_groups_and_leaves_as_one_of_rows() {
    let dirs = ["ws-legacy-a", "ws-legacy-b", "ws-legacy-c"].map(|tag| tree(tag, &["a.txt"]));
    let app = fresh_shell("ws-legacy");
    let tab = |id: &str, path: &Path, group: Option<GroupColor>| TabSession {
        id: id.into(),
        path: path.to_string_lossy().into_owned(),
        name: id.into(),
        group,
        group_id: None, // a Go file has no idea this key exists
        view: ViewMode::List,
    };
    let session = Workspace {
        panes: BTreeMap::from([(
            PaneSlot::Left,
            PaneSession {
                // Go's own spelling of two groups that share a colour — in Go a colour *was* a group,
                // which is exactly why identity had to move off it (design §2).
                tabs: vec![
                    tab("t1", &dirs[0], Some(GroupColor::B)),
                    tab("t2", &dirs[1], None),
                    tab("t3", &dirs[2], Some(GroupColor::B)),
                ],
                active: 0,
                groups: Vec::new(), // the rows this file cannot have
            },
        )]),
        active: PaneSlot::Left,
        layout: LayoutMode::Single,
        dock: None,
    };
    app.session
        .workspace_save(RESERVED, &session)
        .expect("stored as a migrated session would be");

    let mut back = reopened_shell("ws-legacy");
    let SessionRestore::Restored(applied) = back.restore_last_session() else {
        panic!("a stored session was not applied");
    };
    assert_eq!(
        applied.tabs, 3,
        "every tab of the letter-only strip has to come back: {applied:?}"
    );
    let pane = &back.model.panes[&PaneSlot::Left];
    // One group per **contiguous run**, not per letter and not one group of three.
    assert_eq!(
        pane.groups.len(),
        2,
        "two runs of one letter are two groups: {:?}",
        pane.groups
    );
    for group in &pane.groups {
        assert!(
            group.is_unnamed(),
            "a synthesised group has no name to invent, so it must stay unnamed: {group:?}"
        );
        assert_eq!(
            group.label(),
            GroupColor::B.name(),
            "an unnamed group labels with its colour's word, which is what its chip shows"
        );
        assert!(
            !group.collapsed,
            "a file that never mentioned a fold must not come back folded: {group:?}"
        );
        assert_eq!(
            group.color,
            GroupColor::B,
            "the run keeps the letter's colour it was written with"
        );
    }
    assert_eq!(
        pane.members(pane.groups[0].id),
        vec![0],
        "the first run is the first tab alone"
    );
    assert_eq!(
        pane.members(pane.groups[1].id),
        vec![2],
        "and the second run is the third tab alone — the middle one is not a member of either"
    );
    assert_eq!(
        pane.tabs[1].group, None,
        "the tab written with no letter came back grouped"
    );
    pane.debug_assert_invariants();

    // And the letter-only file leaves as a real one: rows, and a pointer on each member.
    let written = back.workspace_session();
    let written = &written.panes[&PaneSlot::Left];
    assert!(
        !written.groups.is_empty(),
        "the synthesised groups were not written back as rows, so the repair runs again next start"
    );
    assert_eq!(
        written.groups.len(),
        2,
        "the two runs were flattened into one row: {:?}",
        written.groups
    );
    assert_eq!(
        written
            .tabs
            .iter()
            .filter(|tab| tab.group_id.is_some())
            .count(),
        2,
        "the two tabs that came back in a group must each name a row, and the tab that came back \
         ungrouped must still name nothing: {:?}",
        written.tabs
    );
    assert_ne!(
        written.tabs[0].group_id, written.tabs[2].group_id,
        "the two runs point at the same row, so on disk they are one group again: {:?}",
        written.groups
    );
    for tab in &written.tabs {
        let named = tab.group_id.as_deref();
        assert!(
            named.is_none()
                || written
                    .groups
                    .iter()
                    .any(|row| Some(row.id.as_str()) == named),
            "a tab names a row the file does not hold: {:?}",
            written.tabs
        );
    }
    // Go's own key keeps the letter, so a Go re-read still sees the accents it wrote (design §4).
    assert_eq!(
        written.tabs.iter().map(|tab| tab.group).collect::<Vec<_>>(),
        vec![Some(GroupColor::B), None, Some(GroupColor::B)],
        "the letter half of the membership was dropped on the way out"
    );
}

#[test]
fn a_restored_group_keeps_the_ids_it_was_saved_with() {
    // `Autosave` compares the whole `Workspace` by value, so a restore that re-minted its group ids
    // would come back `!=` the bytes it had just read and dirty the config on every single start — and
    // a group whose id moved would strand the `groupId` of any tab whose row was written elsewhere.
    let dirs = ["ws-keep-a", "ws-keep-b", "ws-keep-c"].map(|tag| tree(tag, &["a.txt"]));
    // What the shell would write right now: the row ids and every tab's pointer.
    let written = |app: &ZenApp| -> (Vec<String>, Vec<Option<String>>) {
        let session = app.workspace_session();
        let pane = &session.panes[&PaneSlot::Left];
        (
            pane.groups.iter().map(|group| group.id.clone()).collect(),
            pane.tabs.iter().map(|tab| tab.group_id.clone()).collect(),
        )
    };
    // …and what it holds in memory: id, name, member count.
    let groups = |app: &ZenApp| -> Vec<(String, String, usize)> {
        let pane = &app.model.panes[&PaneSlot::Left];
        pane.groups
            .iter()
            .map(|group| {
                let members = pane
                    .tabs
                    .iter()
                    .filter(|tab| tab.group == Some(group.id))
                    .count();
                (group.id.to_store(), group.name.clone(), members)
            })
            .collect()
    };

    let mut app = fresh_shell("ws-keep");
    app.set_layout(panes_for(LayoutMode::Single));
    for dir in &dirs {
        app.open_tab(
            PaneSlot::Left,
            FsPath::from_abs(AbsPath::from_path(dir).expect("absolute")),
        );
    }
    // A group that is made and then released **before** the save, so the one row the file holds is not
    // the first id this shell ever minted: a restore that re-minted instead of adopting would write a
    // different string for the same group, and the comparisons below would catch it.
    let released = app.model.next_group_id();
    let group = app.model.next_group_id();
    {
        let pane = app.model.pane_mut(PaneSlot::Left);
        let ids = pane.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>();
        assert!(
            pane.create_group(released, &ids[..1], GroupColor::A)
                .is_some()
                && pane.create_group(group, &ids[1..], GroupColor::D).is_some()
                && pane.rename_group(group, "work"),
            "the fixture groups were not made"
        );
        pane.ungroup(released);
        assert_eq!(
            pane.groups.len(),
            1,
            "releasing the first group must leave exactly the row this test is about"
        );
        pane.debug_assert_invariants();
    }
    let minted = group.to_store();
    assert_ne!(
        minted,
        released.to_store(),
        "fixture: the row that survives is not the first id this shell minted, which is what lets the \
         comparisons below see a re-mint"
    );
    let first = written(&app);
    assert_eq!(
        first.0,
        vec![minted.clone()],
        "the group the shell made writes the id it was minted with"
    );
    shutdown(&mut app);

    let want = vec![(minted.clone(), "work".to_string(), 2)];
    let mut back = reopened_shell("ws-keep");
    assert!(
        back.restore_last_session().restored(),
        "fixture: the session must load"
    );
    let second = written(&back);
    assert_eq!(
        second, first,
        "the first restore re-minted the ids it had just read, so every start dirties the file: \
         {second:?} vs {first:?}"
    );
    assert_eq!(
        groups(&back),
        want,
        "the id survived but the group it names did not"
    );
    back.model.panes[&PaneSlot::Left].debug_assert_invariants();
    assert_eq!(
        back.model.panes[&PaneSlot::Left].tabs[1].group,
        Some(group),
        "the restore minted a fresh identity instead of keeping the one the file named"
    );
    // Adopting is also what keeps the allocator honest: a group made after a restore must not be the
    // one that was just read back.
    assert_ne!(
        back.model.next_group_id(),
        group,
        "the restore left the id allocator below the ids it loaded, so the next `New Tab Group` would \
         collide with a group already on the strip"
    );
    shutdown(&mut back);

    let mut again = reopened_shell("ws-keep");
    assert!(
        again.restore_last_session().restored(),
        "fixture: the second restart must load"
    );
    let third = written(&again);
    assert_eq!(
        third, first,
        "the second restore moved the ids: {third:?} vs {first:?}"
    );
    assert_eq!(
        groups(&again),
        want,
        "two restarts renamed the group out from under its own id"
    );
    again.model.panes[&PaneSlot::Left].debug_assert_invariants();
}

#[test]
fn a_four_pane_session_comes_back_as_four_panes() {
    // `LayoutMode` cannot spell the quad grid (plan §5.7's `state.rs` correction), so the pane set has
    // to be read back out of the stored map — and the divider positions out of the stored dock tree.
    let dirs: Vec<PathBuf> = ["q1", "q2", "q3", "q4"]
        .iter()
        .map(|tag| tree(tag, &["x.txt"]))
        .collect();
    let mut app = fresh_shell("ws-quad");
    app.set_layout(quad_slots());
    for (slot, dir) in quad_slots().iter().zip(&dirs) {
        app.open_tab(
            *slot,
            FsPath::from_abs(AbsPath::from_path(dir).expect("absolute")),
        );
    }
    app.model.active_pane = PaneSlot::RightBottom;
    shutdown(&mut app);

    let mut back = reopened_shell("ws-quad");
    let report = back.restore_last_session();
    assert!(back.model.quad(), "the quad grid collapsed to two panes");
    assert_eq!(strip(&back, PaneSlot::LeftBottom).len(), 1);
    assert_eq!(strip(&back, PaneSlot::RightBottom).len(), 1);
    assert_eq!(
        back.model.active_pane,
        PaneSlot::RightBottom,
        "the focused pane was not the saved one"
    );
    assert!(
        matches!(&report, SessionRestore::Restored(applied) if applied.dock && applied.tabs == 4),
        "the session's own dock tree was not installed: {report:?}"
    );
}

#[test]
fn an_archive_tab_round_trips_where_a_host_path_would_not() {
    // `/x/y.zip` is a *store*, not a host directory: `host_abs()` answers `None` for it, so writing
    // the host path alone silently deleted the tab at the next restart.
    let mut app = fresh_shell("ws-vfs");
    let host = tree("ws-vfs-zip", &["inside.txt"]).join("packed.zip");
    let zip = FsPath::resolve(host.to_str().expect("utf-8"), Path::new("/"), None)
        .expect("an archive path parses");
    assert!(
        zip.store.is_archive(),
        "the fixture is meant to name an archive store"
    );
    app.open_tab(PaneSlot::Left, zip.clone());
    shutdown(&mut app);

    let mut back = reopened_shell("ws-vfs");
    let report = back.restore_last_session();
    assert!(report.restored(), "{report:?}");
    let tab = &back.model.panes[&PaneSlot::Left].tabs[0];
    assert_eq!(tab.path, zip, "the archive tab came back as something else");
}

#[test]
fn a_clean_exit_writes_what_the_debounce_was_still_holding() {
    let dir = tree("ws-flush", &["a.txt"]);
    let mut app = fresh_shell("ws-flush");
    // A debounce no run will wait out: the only way the write happens is the exit flush.
    app.workspaces.autosave = Autosave::new(Duration::from_secs(600));
    app.set_layout(panes_for(LayoutMode::Single));
    app.open_tab(
        PaneSlot::Left,
        FsPath::from_abs(AbsPath::from_path(&dir).expect("absolute")),
    );
    app.tick_workspaces(Instant::now(), true);
    assert!(
        app.session.workspaces().is_empty(),
        "a write landed before its debounce, so the test proves nothing"
    );
    app.flush_session(Instant::now()).expect("the flush writes");
    assert!(
        app.session
            .workspaces()
            .iter()
            .any(|meta| meta.name.eq_ignore_ascii_case(RESERVED)),
        "the exit flush saved nothing"
    );
    assert!(
        reopened_shell("ws-flush").restore_last_session().restored(),
        "the flushed session does not reload"
    );
}

#[test]
fn a_stored_path_that_is_not_absolute_is_skipped_and_said() {
    // Go wrote these: it kept the *internal* archive path (`api/archive_handlers.go:110-115`) and
    // resolved a relative one against its own cwd (`api/dir.go:39-44`). Reopening those here would
    // point a tab at whatever directory the process happened to start in.
    let app = fresh_shell("ws-relative");
    let session = Workspace {
        panes: BTreeMap::from([(
            PaneSlot::Left,
            PaneSession {
                tabs: vec![
                    TabSession {
                        id: "t1".into(),
                        path: "notes".into(),
                        name: "relative".into(),
                        group: None,
                        view: ViewMode::List,
                        group_id: None,
                    },
                    TabSession {
                        id: "t2".into(),
                        path: String::new(),
                        name: "empty".into(),
                        group: None,
                        view: ViewMode::List,
                        group_id: None,
                    },
                ],
                active: 0,
                groups: Vec::new(),
            },
        )]),
        active: PaneSlot::Left,
        layout: LayoutMode::Single,
        dock: None,
    };
    app.session
        .workspace_save(RESERVED, &session)
        .expect("stored as a session would be");

    let mut back = reopened_shell("ws-relative");
    let report = back.restore_last_session();
    assert!(!report.restored(), "nothing here is a path: {report:?}");
    let SessionRestore::Restored(applied) = &report else {
        panic!("a stored session was applied, not refused: {report:?}");
    };
    assert_eq!(applied.skipped, 2, "{applied:?}");
    assert_eq!(applied.tabs, 0);
    assert_eq!(applied.dropped, 0, "none of these was a pane problem");
    assert!(
        report
            .boot_line()
            .contains("nothing to restore (1 pane(s), 2 tab(s) lost)"),
        "{}",
        report.boot_line()
    );
    assert!(
        back.notices
            .queued()
            .iter()
            .any(|n| n.text.contains("unreadable path")),
        "a dropped tab was not reported to the user"
    );
}

// ------------------------------------------------------- what the restore refuses, and what it says
//
// The plan's rule for this path is a report, never a silent drop (F17/F21), and a divider position is
// the state a user notices most. These cases are the refusals: a dock the settings file owns, a pane
// set this build cannot open, a selection index that no longer exists, and a store it could not read.

use craft_settings::Location;
use zenman_ui::craft_ui::docking::Node;
use zenman_ui::craft_ui::layout::SplitSize;
use zenman_ui::dock::{self, Dock};

/// Move a divider, which is the only way a saved dock tree differs from the default half-and-half.
fn drag_split(dock: &mut Dock, ratio: f32) {
    match &mut dock.layout.root {
        Some(Node::Split { size, .. }) => *size = SplitSize::Ratio(ratio),
        _ => panic!("the default two-pane tree is a split"),
    }
}

/// The divider the dock is holding now, or `None` when it is not a two-pane split.
fn split_ratio(app: &ZenApp) -> Option<f32> {
    match &app.dock.layout.root {
        Some(Node::Split {
            size: SplitSize::Ratio(ratio),
            ..
        }) => Some(*ratio),
        _ => None,
    }
}

#[test]
fn a_divider_the_user_dragged_comes_back_with_the_session() {
    let dirs = ["ws-div-a", "ws-div-b"].map(|tag| tree(tag, &["a.txt"]));
    let mut app = fresh_shell("ws-div");
    app.set_layout(panes_for(LayoutMode::Split));
    for (slot, dir) in [PaneSlot::Left, PaneSlot::Right].iter().zip(&dirs) {
        app.open_tab(
            *slot,
            FsPath::from_abs(AbsPath::from_path(dir).expect("absolute")),
        );
    }
    drag_split(&mut app.dock, 0.32);
    shutdown(&mut app);

    let mut back = reopened_shell("ws-div");
    let report = back.restore_last_session();
    assert!(
        matches!(&report, SessionRestore::Restored(applied) if applied.dock),
        "the session's own dock tree was not installed: {report:?}"
    );
    assert_eq!(
        split_ratio(&back),
        Some(0.32),
        "the divider jumped back to the default half-and-half"
    );
}

#[test]
fn a_session_with_no_dock_tree_keeps_the_divider_the_settings_stored() {
    // A Go-migrated session has no dock tree at all (`docs/contracts.md`: the quad/bottom keys were
    // sent but never stored), while `config.toml` holds the divider this port saved. The session must
    // not stamp its default topology over it — at boot that would be a regression the user could
    // never undo, because the next autosave writes the flat tree back.
    let dir = tree("ws-nodock-dir", &["a.txt"]);
    let mut app = fresh_shell("ws-nodock");
    app.set_layout(panes_for(LayoutMode::Split));
    app.open_tab(
        PaneSlot::Left,
        FsPath::from_abs(AbsPath::from_path(&dir).expect("absolute")),
    );
    app.workspaces.dock = None; // what a migrated `config.json` looks like
    app.save_workspace_as(RESERVED);

    let settings = scratch("ws-nodock-settings");
    std::fs::create_dir_all(&settings).expect("settings dir");
    {
        let (mut store, _report) = zenman_ui::persist::open(Location::Dir(settings.clone()));
        let mut dock = dock::default_workspace(LayoutMode::Split);
        drag_split(&mut dock, 0.44);
        let text = dock::snapshot(&dock).expect("serialisable").to_string();
        store.settings.update(|s| {
            s.layout = LayoutMode::Split;
            s.dock = Some(text);
        });
        store.flush().expect("written");
    }
    let (store, _report) = zenman_ui::persist::open(Location::Dir(settings));
    let mut back = ZenApp::with_persist(reopened_session("ws-nodock"), Vec::new(), store);
    assert_eq!(
        split_ratio(&back),
        Some(0.44),
        "the settings tree was not read"
    );

    let report = back.restore_last_session();
    assert!(
        matches!(&report, SessionRestore::Restored(applied) if !applied.dock),
        "a session with no dock tree must not claim to have installed one: {report:?}"
    );
    assert_eq!(
        split_ratio(&back),
        Some(0.44),
        "the restore overwrote the divider the settings file had"
    );
}

#[test]
fn a_migrated_single_pane_session_still_opens_as_single() {
    // `store::legacy` records an **empty** `right` pane for every Go workspace, `split:false` included
    // (`legacy.rs:458-462`: it is what the absent `split` key means). Treating a named-but-empty pane
    // as a pane the user had open would therefore turn every migrated single-pane session into a split
    // one — and the autosave would make that permanent within two seconds.
    let dir = tree("ws-migrated", &["a.txt"]);
    let app = fresh_shell("ws-migrated");
    let session = Workspace {
        panes: BTreeMap::from([
            (
                PaneSlot::Left,
                PaneSession {
                    tabs: vec![TabSession {
                        id: "t1".into(),
                        path: dir.to_string_lossy().into_owned(),
                        name: "home".into(),
                        group: None,
                        view: ViewMode::List,
                        group_id: None,
                    }],
                    active: 0,
                    groups: Vec::new(),
                },
            ),
            (
                PaneSlot::Right,
                PaneSession {
                    tabs: Vec::new(),
                    active: 0,
                    groups: Vec::new(),
                },
            ),
        ]),
        active: PaneSlot::Left,
        layout: LayoutMode::Single,
        dock: None,
    };
    app.session
        .workspace_save(RESERVED, &session)
        .expect("stored as a migrated session would be");

    let mut back = reopened_shell("ws-migrated");
    let SessionRestore::Restored(applied) = back.restore_last_session() else {
        panic!("a stored session was not applied");
    };
    assert_eq!(applied.panes, 1, "an empty stored pane widened the grid");
    assert_eq!(applied.tabs, 1);
    assert_eq!(applied.dropped, 0, "an empty pane has nothing to lose");
    assert_eq!(back.model.layout, LayoutMode::Single);
    assert_eq!(back.model.panes.len(), 1);
    assert!(
        back.model.layout_mode_is_consistent(),
        "the grid and the mode it spells stopped agreeing"
    );
    // And the autosave does not quietly undo the repair.
    back.workspaces.autosave = Autosave::new(Duration::from_millis(1));
    back.tick_workspaces(Instant::now(), true);
    assert_eq!(
        back.workspace_session().layout,
        LayoutMode::Single,
        "the restored session was saved back as a different grid"
    );
}

#[test]
fn a_stale_dock_tree_is_rebuilt_rather_than_painted_over_a_smaller_grid() {
    // The quad grid's mode can only be spelled `Split`, so the settings file can hold a four-leaf tree
    // while the session that follows names two panes. Leaving that tree in place paints two panels with
    // nothing behind them — and saves the mismatch on.
    let dir = tree("ws-stale", &["a.txt"]);
    let mut app = fresh_shell("ws-stale");
    app.set_layout(panes_for(LayoutMode::Split));
    for slot in panes_for(LayoutMode::Split) {
        app.open_tab(
            slot,
            FsPath::from_abs(AbsPath::from_path(&dir).expect("absolute")),
        );
    }
    app.workspaces.dock = None;
    app.save_workspace_as(RESERVED);

    let settings = scratch("ws-stale-settings");
    std::fs::create_dir_all(&settings).expect("settings dir");
    let quad = dock::snapshot(&dock::quad_workspace())
        .expect("serialisable")
        .to_string();
    {
        let (mut store, _report) = zenman_ui::persist::open(Location::Dir(settings.clone()));
        store.settings.update(|s| {
            s.layout = LayoutMode::Split;
            s.dock = Some(quad.clone());
        });
        store.flush().expect("written");
    }
    let (store, _report) = zenman_ui::persist::open(Location::Dir(settings));
    let mut back = ZenApp::with_persist(reopened_session("ws-stale"), Vec::new(), store);
    assert_eq!(
        dock::panels(&back.dock),
        quad_slots(),
        "the settings tree is the four-leaf one"
    );

    let SessionRestore::Restored(applied) = back.restore_last_session() else {
        panic!("a stored session was not applied");
    };
    assert_eq!(applied.panes, 2, "the session's grid is two panes");
    assert_eq!(
        dock::panels(&back.dock),
        panes_for(LayoutMode::Split),
        "a four-leaf tree was left over a two-pane grid"
    );
    assert!(
        back.model.layout_mode_is_consistent(),
        "the grid and its spelled mode disagreed"
    );
}

#[test]
fn a_three_pane_session_restores_the_grid_the_dock_can_hold_and_says_so() {
    // One, two and four panes are the trees `dock::layout_for_slots` can build. A file naming three is
    // not a grid this shell can paint, so it must be *reported* — not half-painted, which is the Go-F3
    // defect this port's whole pane model exists to avoid.
    let app = fresh_shell("ws-three");
    let dir = tree("ws-three-dir", &["a.txt"]);
    let tabs = |name: &str| {
        vec![TabSession {
            id: name.into(),
            path: dir.to_string_lossy().into_owned(),
            name: name.into(),
            group: None,
            view: ViewMode::List,
            group_id: None,
        }]
    };
    let session = Workspace {
        panes: BTreeMap::from([
            (
                PaneSlot::Left,
                PaneSession {
                    tabs: tabs("l"),
                    active: 0,
                    groups: Vec::new(),
                },
            ),
            (
                PaneSlot::Right,
                PaneSession {
                    tabs: tabs("r"),
                    active: 0,
                    groups: Vec::new(),
                },
            ),
            (
                PaneSlot::LeftBottom,
                PaneSession {
                    tabs: tabs("b"),
                    active: 0,
                    groups: Vec::new(),
                },
            ),
        ]),
        active: PaneSlot::Left,
        layout: LayoutMode::Split,
        dock: None,
    };
    app.session
        .workspace_save(RESERVED, &session)
        .expect("stored as a session would be");

    let mut back = reopened_shell("ws-three");
    let SessionRestore::Restored(applied) = back.restore_last_session() else {
        panic!("a stored session was not applied");
    };
    assert_eq!(applied.panes, 2, "the grid is not one the dock can build");
    assert_eq!(applied.tabs, 2, "the two panes that fit did not come back");
    assert_eq!(
        applied.dropped, 1,
        "the tab that had nowhere to go was not counted"
    );
    assert_eq!(back.model.panes.len(), 2);
    assert!(
        back.dock.layout.validate().is_ok(),
        "the installed dock tree does not match the two-pane grid"
    );
    assert!(
        back.notices
            .queued()
            .iter()
            .any(|n| n.text.contains("unopenable pane")),
        "a dropped tab was never mentioned to the user"
    );
}

#[test]
fn a_selection_that_no_longer_exists_is_clamped_not_wrapped() {
    let dir = tree("ws-clamp", &["a.txt"]);
    let app = fresh_shell("ws-clamp");
    let session = Workspace {
        panes: BTreeMap::from([(
            PaneSlot::Left,
            PaneSession {
                tabs: vec![
                    TabSession {
                        id: "t1".into(),
                        path: dir.to_string_lossy().into_owned(),
                        name: "one".into(),
                        group: None,
                        view: ViewMode::List,
                        group_id: None,
                    },
                    TabSession {
                        id: "t2".into(),
                        path: dir.to_string_lossy().into_owned(),
                        name: "two".into(),
                        group: None,
                        view: ViewMode::List,
                        group_id: None,
                    },
                ],
                active: 99,
                groups: Vec::new(),
            },
        )]),
        active: PaneSlot::Left,
        layout: LayoutMode::Single,
        dock: None,
    };
    app.session
        .workspace_save(RESERVED, &session)
        .expect("stored as a session would be");

    let mut back = reopened_shell("ws-clamp");
    let report = back.restore_last_session();
    assert!(report.restored(), "{report:?}");
    let pane = &back.model.panes[&PaneSlot::Left];
    assert_eq!(pane.tabs.len(), 2);
    assert_eq!(
        pane.active, 1,
        "an index past the end must clamp to the last tab, never panic or wrap"
    );
    assert!(
        pane.active_tab().is_some(),
        "the clamped tab is not readable"
    );
}

#[test]
fn a_store_that_cannot_be_read_is_a_report_not_an_empty_window() {
    // The two ways `restore_last_session` can refuse, and the words each one prints: the boot path
    // turns these into a stdout line, so the wording is part of the interface.
    let missing = SessionRestore::Missing;
    assert!(!missing.restored());
    assert_eq!(missing.boot_line(), "session: none saved yet");
    let failed = SessionRestore::Failed("Permission denied (os error 13)".to_string());
    assert!(!failed.restored());
    assert!(
        failed
            .boot_line()
            .contains("session: could not be restored (Permission denied"),
        "{}",
        failed.boot_line()
    );
    // Applied's own report is shared by the autosave and the `--session` line.
    let applied = Applied {
        tabs: 4,
        panes: 2,
        skipped: 1,
        dropped: 0,
        dock: true,
    };
    assert_eq!(
        applied.report(),
        "restored 4 tab(s) in 2 pane(s), 1 tab(s) with an unreadable path"
    );
    assert_eq!(applied.lost(), 1);
}
