//! The step-10b menus, driven through the same harness the pane and keymap tests use: a real
//! `Session`, a real directory, real `Event::PointerButton`s.
//!
//! What this file owns (plan §7 row 10's menu half):
//!
//! * a right-click on a row opens the **row** menu, a right-click on the empty space below the rows
//!   opens the **background** one — with Go's items, in Go's order, and each row's hint spelled the
//!   way [`KeyMap`] spells its chord;
//! * the menu closes on a click elsewhere, on <kbd>Escape</kbd> and on the pane going away, and a
//!   greyed row fires nothing;
//! * a click routes through [`ZenApp::dispatch`] — asserted for `Copy` (the clipboard really holds the
//!   row afterwards) and for the **danger** row, which is the same D6 ladder
//!   `tests/keymap.rs::del_answers_the_d6_ladder_for_this_volume` walks from the keyboard.
//!
//! The pure cross-product over `MenuContext` is `src/menus/mod.rs`'s own test module; this file is
//! the "does the shell actually draw and route it" half.

mod common;

use std::path::{Path, PathBuf};

use egui::{Event, Modifiers, PointerButton, Pos2, Vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::{AccessKitNode, Queryable};

use zenman_core::path::{AbsPath, FsPath};
use zenman_core::store::config::{CustomAction, GroupColor, LayoutMode, PaneSlot, ViewMode};
use zenman_ui::app::ZenApp;
use zenman_ui::keymap::{Chord, KeyMap};
use zenman_ui::menus::MenuTarget;
use zenman_ui::panels::file_pane::PaneCommand;
use zenman_ui::state::{Tab, panes_for};

/// Frames to let the shell settle after a non-job action.
const SETTLE: usize = 6;

// --------------------------------------------------------------------- harness plumbing

/// A fixture directory under cargo's per-test scratch with only the named files (leftovers from a
/// previous run of the same target are removed — the trees persist, plan §9.2 step 9).
fn tree(tag: &str, files: &[&str]) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("zenman-ui-fixtures")
        .join(format!("zenman-ui-fixture-{tag}"));
    std::fs::create_dir_all(&root).expect("fixture directory");
    let wanted: std::collections::BTreeSet<String> =
        files.iter().map(|f| (*f).to_string()).collect();
    for entry in std::fs::read_dir(&root).expect("fixture directory is readable") {
        let entry = entry.expect("a readable directory entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        if !wanted.contains(&name) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    for name in files {
        std::fs::write(root.join(name), b"menu fixture\n").expect("fixture file");
    }
    root
}

fn fixture_path(path: &Path) -> FsPath {
    FsPath::from_abs(AbsPath::try_from(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
}

fn build_harness(app: ZenApp, size: Vec2) -> Harness<'static, ZenApp> {
    let mut harness = Harness::builder()
        .with_accessibility_check(true)
        .with_size(size)
        // The menu is opened by a command that ran at the *end* of the previous frame, and jobs plus
        // the coalesced `DirChanged` window stretch the frame loop: 10a found the builder's default too
        // tight, and 10b agrees.
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
    settle(&mut harness);
    harness
}

/// A single-pane shell listing `dir`.
fn pane_harness(tag: &str, dir: &Path) -> Harness<'static, ZenApp> {
    let session = common::session(tag);
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Single));
    app.open_tab(PaneSlot::Left, fixture_path(dir));
    build_harness(app, egui::vec2(1200.0, 800.0))
}

/// A two-pane shell, for the "a menu must not outlive its pane" assertion.
fn split_harness(tag: &str, left: &Path, right: &Path) -> Harness<'static, ZenApp> {
    let session = common::session(tag);
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Split));
    app.open_tab(PaneSlot::Left, fixture_path(left));
    app.open_tab(PaneSlot::Right, fixture_path(right));
    build_harness(app, egui::vec2(1600.0, 800.0))
}

fn settle(harness: &mut Harness<'static, ZenApp>) {
    for _ in 0..SETTLE {
        harness.run();
    }
}

fn shown<'h>(harness: &'h Harness<'static, ZenApp>, slot: PaneSlot) -> &'h Tab {
    harness
        .state()
        .model
        .panes
        .get(&slot)
        .and_then(|p| p.active_tab())
        .unwrap_or_else(|| panic!("the harness has a shown tab in {slot:?}"))
}

fn names(tab: &Tab) -> Vec<String> {
    tab.entries
        .entries
        .iter()
        .map(|entry| entry.name.clone())
        .collect()
}

fn notices(harness: &Harness<'static, ZenApp>) -> Vec<String> {
    harness
        .state()
        .notices
        .queued()
        .iter()
        .map(|notice| notice.text.clone())
        .collect()
}

fn any_notice(harness: &Harness<'static, ZenApp>, needle: &str) -> bool {
    notices(harness).iter().any(|text| text.contains(needle))
}

// --------------------------------------------------------------------- the driving primitives

/// Right-click the row labelled `name` — the same event a mouse produces, and the same one the pane
/// tests' `Queryable::click` sends, only with the other button.
fn right_click(harness: &mut Harness<'static, ZenApp>, name: &str) {
    harness.get_by_label(name).click_secondary();
    // The pane's commands run after the draw, so the menu exists on the *second* frame.
    harness.run();
    harness.run();
}

/// A raw pointer press+release at one point — for the space that is not a row.
fn click_at(harness: &mut Harness<'static, ZenApp>, at: Pos2, button: PointerButton) {
    for pressed in [true, false] {
        harness.input_mut().events.push(Event::PointerButton {
            pos: at,
            button,
            pressed,
            modifiers: Modifiers::default(),
        });
    }
    harness.run();
    harness.run();
}

/// Right-click the empty band below the last row of `slot`'s listing.
fn right_click_background(harness: &mut Harness<'static, ZenApp>) {
    let wanted = if harness.query_all_by_label("c.md").count() > 0 {
        "c.md"
    } else {
        "a.txt"
    };
    let last = harness.get_by_label(wanted).rect().max;
    click_at(
        harness,
        Pos2::new(last.x - 60.0, last.y + 60.0),
        PointerButton::Secondary,
    );
}

/// The open menu as data: `(label, enabled, hint)` in menu order, straight out of the shell's own
/// [`ZenApp::menu_specs`] — the same call the renderer makes.
fn rows(harness: &Harness<'static, ZenApp>) -> Vec<(String, bool, Option<String>)> {
    let state = harness.state();
    let Some(open) = state.menu.clone() else {
        return Vec::new();
    };
    let ctx = state.menu_context(open.slot, open.target);
    let keys = KeyMap::default();
    state
        .menu_specs()
        .iter()
        .flatten()
        .map(|spec| {
            (
                spec.label.clone(),
                spec.enabled(&ctx),
                spec.hint(&keys).or_else(|| spec.icon.map(str::to_string)),
            )
        })
        .collect()
}

fn labels(harness: &Harness<'static, ZenApp>) -> Vec<String> {
    rows(harness)
        .into_iter()
        .map(|(label, _, _)| label)
        .collect()
}

fn is_open(harness: &Harness<'static, ZenApp>) -> bool {
    harness.state().menu.is_some()
}

/// The menu rows the shell actually *drew*, read back out of the accessibility tree — a painted menu
/// with no nodes is a menu nobody can click.
fn drawn_nodes(harness: &Harness<'static, ZenApp>, label: &str) -> usize {
    harness.query_all_by_label(label).count()
}

// ------------------------------------------------------------------------------- the tests

#[test]
fn right_clicking_a_row_opens_go_row_menu_with_the_keymaps_hints() {
    let dir = tree("menus-row", &["a.txt", "b.txt", "c.md"]);
    let mut harness = pane_harness("menus-row", &dir);
    right_click(&mut harness, "a.txt");
    assert!(is_open(&harness), "the right-click opened nothing");

    let rows = rows(&harness);
    let enabled: Vec<&str> = rows
        .iter()
        .filter(|(_, on, _)| *on)
        .map(|(label, _, _)| label.as_str())
        .collect();
    // The right-click selected the row (Go's `handlePaneContextMenu:19-27`), so its own commands are
    // live; the clipboard ones are not, and the directory block is absent for a file row.
    assert_eq!(
        labels(&harness),
        [
            "Open",
            "Cut",
            "Copy",
            "Copy path",
            "Copy name",
            "Rename",
            "Change permissions",
            "Properties",
            "Extract here",
            "Move to trash",
            "Delete permanently",
        ],
        "the file row's menu, in `context-menu.js:71-107`'s order, plus 13b's archive group"
    );
    assert_eq!(
        enabled,
        [
            "Open",
            "Cut",
            "Copy",
            "Copy path",
            "Copy name",
            "Rename",
            "Change permissions",
            "Properties",
            "Move to trash",
            "Delete permanently"
        ],
        "a plain filesystem row enables everything but `Extract here`, which needs a store"
    );
    // The hint cell is the keymap's own spelling (plan D7), not a second table.
    let hint = |label: &str| {
        rows.iter()
            .find(|(candidate, _, _)| candidate == label)
            .and_then(|(_, _, hint)| hint.clone())
    };
    assert_eq!(hint("Copy").as_deref(), Some("Ctrl+C"));
    assert_eq!(hint("Move to trash").as_deref(), Some("Delete"));
    assert_eq!(hint("Delete permanently").as_deref(), Some("Shift+Delete"));
    assert_eq!(hint("Rename").as_deref(), Some("F2"));

    // …and the rows really are on screen, as nodes: the menu paints its text, so its a11y nodes are
    // the only thing a test (or a screen reader) can grab.
    for label in ["Copy", "Move to trash"] {
        assert_eq!(drawn_nodes(&harness, label), 1, "{label} was not drawn");
    }
}

#[test]
fn right_clicking_a_directory_row_adds_go_directory_block() {
    // `nested_tree` gives real directory rows (`alpha`, `beta`) — the `isDir` half of Go's branch.
    let listing = common::nested_tree("menus-dir");
    let mut harness = pane_harness("menus-dir", &listing);
    right_click(&mut harness, "alpha");
    assert_eq!(
        labels(&harness),
        [
            "Open in new tab",
            "Copy inside",
            "Cut inside",
            "Paste inside",
            "Open",
            "Cut",
            "Copy",
            "Copy path",
            "Copy name",
            "Rename",
            "Change permissions",
            "Properties",
            "Extract here",
            "Move to trash",
            "Delete permanently",
        ],
        "`context-menu.js:46-70`'s block sits above Open"
    );
    let rows = rows(&harness);
    let disabled: Vec<&str> = rows
        .iter()
        .filter(|(_, on, _)| !*on)
        .map(|(label, _, _)| label.as_str())
        .collect();
    assert_eq!(
        disabled,
        ["Paste inside", "Extract here"],
        "only the clipboard row and the archive row are greyed"
    );
}

#[test]
fn right_clicking_the_empty_space_opens_the_background_menu() {
    let dir = tree("menus-bg", &["a.txt", "b.txt", "c.md"]);
    let mut harness = pane_harness("menus-bg", &dir);
    right_click_background(&mut harness);
    assert!(
        is_open(&harness),
        "no background menu; notices {:?}",
        notices(&harness)
    );
    assert_eq!(
        labels(&harness),
        ["Paste", "Paste link", "New folder"],
        "Go's empty-space menu (`context-menu.js:108-124`)"
    );
    let rows = rows(&harness);
    assert_eq!(
        rows.iter()
            .filter(|(_, on, _)| *on)
            .map(|(label, _, _)| label.as_str())
            .collect::<Vec<_>>(),
        ["New folder"],
        "an empty clipboard greys both paste rows, it does not hide them"
    );
    assert_eq!(drawn_nodes(&harness, "Paste"), 1, "the menu is drawn");
}

#[test]
fn a_click_elsewhere_closes_the_menu() {
    let dir = tree("menus-away", &["a.txt", "b.txt", "c.md"]);
    let mut harness = pane_harness("menus-away", &dir);
    right_click(&mut harness, "a.txt");
    assert!(is_open(&harness));
    // The header's empty middle: no pane, no menu, nothing to fire.
    click_at(&mut harness, Pos2::new(400.0, 10.0), PointerButton::Primary);
    assert!(
        !is_open(&harness),
        "the menu survived a click outside its frame"
    );
    assert_eq!(
        drawn_nodes(&harness, "Copy"),
        0,
        "a closed menu must not keep its nodes in the tree"
    );
}

#[test]
fn escape_closes_the_menu_and_keeps_the_selection() {
    let dir = tree("menus-esc", &["a.txt", "b.txt", "c.md"]);
    let mut harness = pane_harness("menus-esc", &dir);
    right_click(&mut harness, "a.txt");
    assert!(is_open(&harness));
    assert_eq!(shown(&harness, PaneSlot::Left).sel.len(), 1);
    harness
        .input_mut()
        .events
        .push(Chord::key(egui::Key::Escape).event());
    harness.run();
    assert!(!is_open(&harness), "Escape closed nothing");
    assert_eq!(
        shown(&harness, PaneSlot::Left).sel.len(),
        1,
        "Escape is spent on the menu — Go's own order (`app.js:276`, `action-log.js:231`)"
    );
    // …and a second Escape now does the file list's own thing.
    harness
        .input_mut()
        .events
        .push(Chord::key(egui::Key::Escape).event());
    harness.run();
    assert!(shown(&harness, PaneSlot::Left).sel.is_empty());
}

#[test]
fn a_greyed_item_fires_nothing() {
    let dir = tree("menus-grey", &["a.txt", "b.txt", "c.md"]);
    let mut harness = pane_harness("menus-grey", &dir);
    right_click_background(&mut harness);
    assert!(is_open(&harness));
    assert!(!harness.state().session.clipboard().is_some());
    // `Paste` is drawn and greyed: clicking it must not paste, must not toast, must not close.
    harness.get_by_label("Paste").click();
    harness.run();
    assert!(
        !any_notice(&harness, "the clipboard is empty"),
        "a disabled row reached `dispatch`: {:?}",
        notices(&harness)
    );
    assert!(
        is_open(&harness),
        "a disabled click still dismissed the menu"
    );
    assert!(!harness.state().session.clipboard().is_some());
}

#[test]
fn copy_from_the_menu_leaves_the_clipboard_set_after_it_closes() {
    let dir = tree("menus-copy", &["a.txt", "b.txt", "c.md"]);
    let mut harness = pane_harness("menus-copy", &dir);
    right_click(&mut harness, "a.txt");
    harness.get_by_label("Copy").click();
    harness.run();
    assert!(!is_open(&harness), "a chosen item always closes the menu");
    let clip = harness
        .state()
        .session
        .clipboard()
        .expect("the menu's Copy queued the row");
    assert_eq!(clip.items.len(), 1, "exactly the right-clicked row");
    assert!(
        clip.paths()
            .first()
            .is_some_and(|path| path.as_path().to_string_lossy().ends_with("a.txt")),
        "the clipboard holds {:?}",
        clip.paths()
    );
    // And the *same* dispatch the chord runs: the menu now says Paste, because the state says so.
    right_click_background(&mut harness);
    let rows = rows(&harness);
    assert!(
        rows.iter().any(|(label, on, _)| label == "Paste" && *on),
        "the rebuilt menu did not notice the clipboard: {rows:?}"
    );
}

#[test]
fn the_danger_item_routes_through_the_same_dispatch_as_delete() {
    // Plan **D6**, from the mouse: the menu's red row is the very command `Delete` runs, so it answers
    // for the volume it is on — the trash branch when the fsid has one, a said-out-loud refusal when
    // it does not. Branch-and-print, exactly like `tests/keymap.rs`'s keyboard version.
    let dir = tree("menus-danger", &["one.txt", "two.txt"]);
    let one = dir.join("one.txt");
    let can_trash =
        zenman_core::platform::default_env().can_trash(&AbsPath::try_from(one.as_path()).unwrap());
    println!(
        "menu D6 branch: can_trash({}) = {can_trash} — {}",
        one.display(),
        if can_trash {
            "TRASHED branch: this volume has a trash location"
        } else {
            "REFUSED branch: no trash root, permanent_fallback == false"
        }
    );
    let mut harness = pane_harness("menus-danger", &dir);
    right_click(&mut harness, "one.txt");
    assert!(
        harness.query_all_by_label("Move to trash").count().eq(&1),
        "the danger row is labelled by `Command::Delete`'s own text"
    );
    harness.get_by_label("Move to trash").click();
    common::settle_until(&mut harness, "the delete job's summary toast", |h| {
        any_notice(h, "trashed") || any_notice(h, "failed")
    });
    assert!(!is_open(&harness), "the chosen item left the menu open");
    if can_trash {
        assert!(
            !one.exists(),
            "the trash branch moved the file off the disk"
        );
        common::settle_until(&mut harness, "the row to disappear", |h| {
            !names(shown(h, PaneSlot::Left)).contains(&"one.txt".to_string())
        });
    } else {
        assert!(one.exists(), "the refused branch must not touch the bytes");
        assert!(
            any_notice(&harness, "0 trashed, 0 deleted, 1 failed"),
            "the refusal is said out loud: {:?}",
            notices(&harness)
        );
    }
}

#[test]
fn a_menu_does_not_outlive_its_pane() {
    let left = tree("menus-left", &["a.txt"]);
    let right = tree("menus-right", &["b.txt"]);
    let mut harness = split_harness("menus-pane", &left, &right);
    // The *right* pane: collapsing to one pane must remove the slot the menu belongs to, not the
    // other one.
    right_click(&mut harness, "b.txt");
    assert_eq!(
        harness.state().menu.as_ref().map(|open| open.slot),
        Some(PaneSlot::Right),
        "the menu was opened in the wrong pane"
    );
    harness
        .state_mut()
        .set_layout(panes_for(LayoutMode::Single));
    harness.run();
    assert!(
        !is_open(&harness),
        "a menu survived the pane it was opened in"
    );
}

#[test]
fn the_open_menu_context_is_read_from_the_engine_not_invented() {
    // Every field of `MenuContext` the shell can answer for, answered here: the clipboard pair from
    // `Session::clipboard`, the store kind from the tab's `FsPath`, the symlink capability from the
    // `Session`'s `Env`. `in_batch` is the documented gap (step 14).
    let listing = common::nested_tree("menus-ctx");
    let mut harness = pane_harness("menus-ctx", &listing);
    let ctx = harness.state().menu_context(
        PaneSlot::Left,
        zenman_ui::menus::MenuTarget::Row { on_dir: false },
    );
    assert!(!ctx.has_selection, "nothing has been clicked yet");
    assert!(!ctx.on_archive, "a plain directory is writable");
    assert!(!ctx.in_batch, "step 14's batch state does not exist yet");
    assert_eq!(
        ctx.can_symlink,
        zenman_core::platform::default_env()
            .supports_symlink()
            .unwrap_or(false),
        "the menu asks the platform, not a constant"
    );
    right_click(&mut harness, "alpha");
    let open = harness.state().menu.clone().expect("the dir row's menu");
    let ctx = harness.state().menu_context(open.slot, open.target);
    assert!(ctx.has_selection, "the right-click selected the row");
    assert!(ctx.all_dirs, "the only selected row is a directory");
    assert!(!ctx.has_clipboard);
    assert_eq!(
        harness
            .state()
            .menu_context(PaneSlot::Left, zenman_ui::menus::MenuTarget::Background)
            .target,
        zenman_ui::menus::MenuTarget::Background
    );
}

// --------------------------------------------------------------------- step 14b

/// A session with one real custom action, `*.txt`-scoped (`services::actions`, not the menu's).
fn with_action(
    harness: &mut Harness<'static, ZenApp>,
    id: &str,
    name: &str,
    role: &str,
    patterns: &str,
) {
    harness
        .state_mut()
        .session
        .action_create(CustomAction {
            id: id.to_string(),
            name: name.to_string(),
            icon: String::new(),
            command: "echo {name}".to_string(),
            role: role.to_string(),
            patterns: patterns.to_string(),
            show_output: false,
        })
        .expect("the action persists");
    harness.run();
}

#[test]
fn custom_actions_come_from_the_real_service_including_the_glob_half() {
    // 10b's handover: `menus::custom_specs` used to filter by role alone. The glob half is what
    // this test can tell apart — a `files`-role action scoped to `*.txt` must *not* appear on the
    // `c.md` row, which the placeholder always showed.
    let dir = tree("menus-14b", &["a.txt", "c.md"]);
    let mut harness = pane_harness("menus-14b", &dir);
    with_action(&mut harness, "aa11", "Word it", "files", "*.txt");
    with_action(
        &mut harness,
        "bb22",
        "Always",
        "files",
        String::new().as_str(),
    );

    right_click(&mut harness, "a.txt");
    let names: Vec<String> = rows(&harness).into_iter().map(|(l, _, _)| l).collect();
    assert!(names.iter().any(|l| l == "Word it"), "{names:?}");
    close_menu(&mut harness);

    right_click(&mut harness, "c.md");
    let names: Vec<String> = rows(&harness).into_iter().map(|(l, _, _)| l).collect();
    assert!(
        !names.iter().any(|l| l == "Word it"),
        "the pattern filter is back to role-only (10b's placeholder): {names:?}"
    );
    assert!(names.iter().any(|l| l == "Always"), "{names:?}");
}

#[test]
fn a_batch_selection_labels_the_rename_row_and_the_menu_says_so() {
    let dir = tree("menus-batch14", &["a.txt", "b.txt", "c.md"]);
    let mut harness = pane_harness("menus-batch14", &dir);
    let ctx = harness
        .state()
        .menu_context(PaneSlot::Left, MenuTarget::Row { on_dir: false });
    assert!(
        !ctx.in_batch,
        "nothing is selected, so no batch owns the pane"
    );
    // The menu has to be open for `menu_specs` to answer (it is the same call the renderer makes),
    // so open it first and *then* widen the selection: the rows are rebuilt from live state every
    // frame, which is what lets a batch light up while its own menu is on screen.
    right_click(&mut harness, "a.txt");
    select_two(&mut harness);
    harness.run();
    let ctx = harness
        .state()
        .menu_context(PaneSlot::Left, MenuTarget::Row { on_dir: false });
    assert!(
        ctx.in_batch,
        "`in_batch` still has no source (step 14b gave it one)"
    );
    // And the builder's batch arm is now a live row, not `not_yet_wired`.
    let specs = harness.state().menu_specs();
    let batch = specs
        .iter()
        .flatten()
        .find(|spec| spec.label == "Rename batch")
        .expect("the >1-selection menu has no `Rename batch` row");
    assert!(
        batch.enabled(&ctx),
        "the batch row is greyed although a batch is selected"
    );
}

fn close_menu(harness: &mut Harness<'static, ZenApp>) {
    harness.state_mut().menu = None;
    harness.run();
}

fn select_two(harness: &mut Harness<'static, ZenApp>) {
    let entries = harness
        .state()
        .model
        .panes
        .get(&PaneSlot::Left)
        .and_then(|p| p.active_tab())
        .map(|t| t.entries.clone())
        .expect("the listing loaded");
    assert!(entries.entries.len() > 1, "the fixture listed nothing");
    let tab = harness
        .state_mut()
        .model
        .panes
        .get_mut(&PaneSlot::Left)
        .and_then(|p| p.active_tab_mut())
        .expect("the pane has a tab");
    tab.sel.select_all(entries.entries.len(), |ix| {
        entries.entries.get(ix).map(Tab::row_key)
    });
}

/// Press and release at `at` in **one** frame, then stop: the frame's own platform output is what the
/// next assertion reads, and `Harness::run` would settle past it and flush the copy command.
fn one_frame_click(harness: &mut Harness<'static, ZenApp>, at: Pos2) {
    for pressed in [true, false] {
        harness.input_mut().events.push(Event::PointerButton {
            pos: at,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::default(),
        });
    }
    harness.step();
}

/// What the last frame put on the clipboard — egui's `CopyText` output command, the port's answer to
/// Go's `navigator.clipboard.writeText` (`context-menu.js:582-591`).
fn copied(harness: &Harness<'static, ZenApp>) -> Option<String> {
    harness
        .output()
        .platform_output
        .commands
        .iter()
        .find_map(|command| match command {
            egui::OutputCommand::CopyText(text) => Some(text.clone()),
            _ => None,
        })
}

// ------------------------------------------------------------------- the menu's screen position

/// A point inside the `row`th listing row, `x` points in from the column header's left edge.
fn row_point(harness: &Harness<'static, ZenApp>, row: usize, x: f32) -> Pos2 {
    let header = harness
        .query_all_by(|node: &AccessKitNode<'_>| {
            node.role() == egui::Role::Button && node.label().is_some_and(|l| l.starts_with("Name"))
        })
        .next()
        .expect("the column header's Name button")
        .rect();
    let tokens = harness.state().tokens;
    Pos2::new(
        header.min.x + x,
        header.max.y + tokens.row_height * (row as f32 + 0.5),
    )
}

/// The box the open menu actually painted: the union of the a11y nodes of the rows it drew. A
/// remembered-position `Area` moves this; a clamped one does not, so it is the only honest way to
/// assert *where* the menu landed.
fn drawn_menu(harness: &Harness<'static, ZenApp>) -> egui::Rect {
    let mut box_ = egui::Rect::NOTHING;
    for label in ["Open", "Copy path", "Delete permanently"] {
        for node in harness.query_all_by_label(label) {
            box_ = box_.union(node.rect());
        }
    }
    box_
}

/// The menu has to open where the pointer was — every time. `Area` remembers the position of a known
/// id, and this egui fork's `Id::unique` hashes its source with fixed seeds rather than counting, so
/// the same layer id comes back for every opening: with `Area::default_pos` the second, third and
/// hundredth menu all drew at the first one's spot (the parity defect, plan §9.2 step-10b's seam).
#[test]
fn the_context_menu_opens_under_the_pointer_wherever_the_pointer_was() {
    let dir = tree("menus-where", &["a.txt", "b.txt", "c.md"]);
    let mut harness = pane_harness("menus-where", &dir);
    let mut offsets = vec![];
    let mut tops = vec![];
    // One row each, at three different heights: the menu must follow the pointer, and the rows are
    // where a *row* menu can come from. Located through the column header rather than by a literal
    // y, because the chrome above the listing is the part that moves when the shell is restyled.
    let points = [
        row_point(&harness, 0, 150.0),
        row_point(&harness, 1, 320.0),
        row_point(&harness, 2, 90.0),
    ];
    for at in points {
        click_at(&mut harness, at, PointerButton::Secondary);
        let open = harness
            .state()
            .menu
            .clone()
            .expect("the right-click opened nothing");
        assert_eq!(open.at, at, "the request kept the wrong anchor");
        let drawn = drawn_menu(&harness);
        assert!(
            !drawn.is_negative(),
            "the menu painted nothing for a click at {at:?}"
        );
        // The rows' own boxes sit one border inside the frame, so the *offset* is the invariant and
        // the absolute point is not: same click shape, same offset, every time.
        offsets.push(drawn.min - at.to_vec2());
        tops.push(drawn.min);
        harness.key_press(egui::Key::Escape);
        harness.run();
    }
    assert!(
        offsets.windows(2).all(|pair| pair[0] == pair[1]),
        "the same click drew the menu at differing offsets: {offsets:?}"
    );
    for pair in tops.windows(2) {
        assert_ne!(
            pair[0], pair[1],
            "three clicks, one menu position: {tops:?}"
        );
    }
}

// ---------------------------------------------------------------------- the breadcrumb folder menu

/// The centre of a breadcrumb chip, picked by the strip it lives in. A crumb's text is shared with tab
/// titles and with row names, so the row is what identifies it — and the row is where `Root` is, the
/// segment every path in these fixtures has in the trail.
fn crumb(harness: &mut Harness<'static, ZenApp>, label: &str) -> Pos2 {
    let row = ["Root", "…"]
        .iter()
        .find_map(|anchor| harness.query_all_by_label(anchor).map(|n| n.rect()).next())
        .expect("a collapsed trail keeps its root chip or its marker")
        .center()
        .y;
    harness
        .query_all_by_label(label)
        .map(|n| n.rect())
        .find(|r| (r.center().y - row).abs() < 1.0)
        .unwrap_or_else(|| panic!("{label:?} is not a chip on the trail's row ({row})"))
        .center()
}

/// `common::nested_tree` lists `<tag>/nested`; opening `nested/beta/inner` puts `beta` in the trail as
/// a chip that is neither the store root nor the current segment, and leaves `b2.txt` the only row.
/// Twelve segments is what cargo's scratch directory makes of it — deep enough that the trail cannot
/// fit, which is the case the collapse has to get right.
fn inner_harness(tag: &str) -> (Harness<'static, ZenApp>, PathBuf, PathBuf) {
    let nested = common::nested_tree(tag);
    let beta = nested.join("beta");
    let inner = beta.join("inner");
    let mut harness = pane_harness(tag, &inner);
    // A row is selected first: nothing a crumb menu does may touch it, and nothing may copy it.
    harness.get_by_label("b2.txt").click();
    harness.run();
    assert_eq!(shown(&harness, PaneSlot::Left).sel.len(), 1, "fixture");
    (harness, beta, inner)
}

/// Go's `showFolderContextMenu` (`context-menu.js:134-156`), reached from `navigation.js:211-215`:
/// right-clicking a breadcrumb segment opens *its own* three-row menu over *that path*.
#[test]
fn right_clicking_a_breadcrumb_chip_opens_the_folder_menu() {
    let (mut harness, beta, _) = inner_harness("menus-crumb");
    let at = crumb(&mut harness, "beta");
    click_at(&mut harness, at, PointerButton::Secondary);
    assert!(is_open(&harness), "the crumb right-click opened nothing");

    assert_eq!(
        labels(&harness),
        ["Open in new tab", "Copy path", "Copy name"],
        "`showFolderContextMenu`'s three rows, and nothing of the row menu's ladder"
    );
    let open = harness.state().menu.clone().unwrap();
    assert_eq!(
        open.target,
        MenuTarget::Folder,
        "a crumb is not a row of the listing"
    );
    let folder = open.folder.expect("a folder menu names its folder");
    assert_eq!(
        folder.host_abs().map(|p| p.to_string_lossy()),
        Some(beta.display().to_string()),
        "the menu must act on the chip's own path"
    );
    assert_eq!(
        shown(&harness, PaneSlot::Left).sel.len(),
        1,
        "a crumb is not a row: Go's `stopPropagation` (`navigation.js:213`) left the selection alone"
    );
}

/// The other half of the collapse rule: the trail folds its **front**, never its end. The current
/// segment is where the user is standing and the only handle its folder menu has, so a chip that was
/// squeezed to zero width left a deep path naming every directory but the one being listed — with
/// nothing under the pointer to right-click.
#[test]
fn the_current_crumb_survives_a_trail_too_deep_for_the_pane() {
    let (mut harness, _, inner) = inner_harness("menus-crumb-current");
    let at = crumb(&mut harness, "inner");
    click_at(&mut harness, at, PointerButton::Secondary);
    let open = harness
        .state()
        .menu
        .clone()
        .expect("the current crumb opened no menu");
    assert_eq!(open.target, MenuTarget::Folder, "a crumb is a folder");
    assert_eq!(
        open.folder
            .and_then(|f| f.host_abs())
            .map(|p| p.to_string_lossy()),
        Some(inner.display().to_string()),
        "and it is the directory the pane is listing"
    );
    assert_eq!(
        labels(&harness),
        ["Open in new tab", "Copy path", "Copy name"],
        "the folder menu, on the segment that is on screen"
    );
}

/// The rows of that menu answer to the chip, not to the pane's selection — the `data-target-path`
/// half of Go's dispatcher (`context-menu.js:636-656`).
#[test]
fn the_folder_menu_copies_the_chip_and_not_the_selection() {
    let (mut harness, beta, inner) = inner_harness("menus-crumb-copy");
    let at = crumb(&mut harness, "beta");
    click_at(&mut harness, at, PointerButton::Secondary);
    let row = harness.get_by_label("Copy path").rect().center();
    one_frame_click(&mut harness, row);
    assert_eq!(
        copied(&harness),
        Some(format!("{}\n", beta.display())),
        "`Copy path` copied the crumb"
    );
    assert!(
        !copied(&harness).is_some_and(|text| text.contains("b2.txt")),
        "the selection leaked into a folder menu"
    );

    // …and `Copy name` is the same row's last segment.
    let at = crumb(&mut harness, "beta");
    click_at(&mut harness, at, PointerButton::Secondary);
    let row = harness.get_by_label("Copy name").rect().center();
    one_frame_click(&mut harness, row);
    assert_eq!(
        copied(&harness),
        Some("beta\n".to_string()),
        "the listed path is still {inner:?}"
    );
}

/// `Open in new tab` on a crumb is Go's `open-path-in-new-tab` → `createTab(paneId, targetPath)`:
/// a **new** tab in the pane that owns the strip, listing the chip — where a left-click on the same
/// chip navigates the tab that is already there.
#[test]
fn the_folder_menu_opens_the_chip_in_a_new_tab() {
    let (mut harness, beta, inner) = inner_harness("menus-crumb-tab");
    let before = harness.state().model.panes[&PaneSlot::Left].tabs.len();
    let at = crumb(&mut harness, "beta");
    click_at(&mut harness, at, PointerButton::Secondary);
    harness.get_by_label("Open in new tab").click();
    harness.run();
    assert!(!is_open(&harness), "a chosen item always closes the menu");

    let pane = &harness.state().model.panes[&PaneSlot::Left];
    assert_eq!(pane.tabs.len(), before + 1, "no tab was added");
    let tab = pane.active_tab().expect("the new tab is shown");
    assert_eq!(
        tab.path.host_abs().map(|p| p.to_string_lossy()),
        Some(beta.display().to_string()),
        "the new tab lists the chip, not the tab that was open ({inner:?})"
    );
}

// --------------------------------------------------------------------------- the tab-strip menu

/// Go's `showTabContextMenu` (`tabs.js:351-386`) needs a tab that is *not* the shown one, or "the menu
/// acts on the tab under the cursor" is the same assertion as "…on the shown tab".
fn two_tab_harness(tag: &str) -> (Harness<'static, ZenApp>, PathBuf, PathBuf) {
    let listing = common::listing_tree(tag);
    let docs = listing.join("docs");
    let session = common::session(tag);
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Single));
    app.open_tab(PaneSlot::Left, fixture_path(&listing));
    app.open_tab(PaneSlot::Left, fixture_path(&docs));
    let harness = build_harness(app, egui::vec2(1200.0, 800.0));
    (harness, listing, docs)
}

/// A point on the `ix`th tab. The tab's title is a `.selectable(false)` [`egui::Label`], so its node is
/// queryable but not click-through — hence a *point*, fed to [`click_at`]. The same word is a
/// breadcrumb chip and often a row name, and the strip is what tells them apart: the strip sits above
/// the address strip, so the top-most match is the tab.
fn tab_point(harness: &Harness<'static, ZenApp>, slot: PaneSlot, ix: usize) -> Pos2 {
    let title = harness.state().model.panes[&slot].tabs[ix].title.clone();
    harness
        .query_all_by_label(&title)
        .map(|node| node.rect())
        .min_by(|a, b| a.min.y.total_cmp(&b.min.y))
        .unwrap_or_else(|| panic!("{title:?} is drawn nowhere, so there is no tab to click"))
        .center()
}

/// A point on a row of the open menu, picked by position: the strip's own `×` carries the hover name
/// `Close tab` too (an icon-only button takes its tooltip text as its accessible name), so the label
/// alone is ambiguous. The menu opens *at the pointer*, i.e. inside the strip's band, so its row is
/// always the lowest of the matches.
fn menu_row_point(harness: &Harness<'static, ZenApp>, label: &str) -> Pos2 {
    harness
        .query_all_by_label(label)
        .map(|node| node.rect())
        .max_by(|a, b| a.min.y.total_cmp(&b.min.y))
        .unwrap_or_else(|| panic!("{label:?} is not a row of the open menu"))
        .center()
}

/// The pane's tabs, cloned: a test that asserts a verb acted on one tab has to compare the whole
/// strip, and `Tab` is `Clone`.
fn pane(harness: &Harness<'static, ZenApp>, slot: PaneSlot) -> Vec<Tab> {
    harness.state().model.panes[&slot].tabs.clone()
}

/// Go's `showTabContextMenu`, group by group, opened over a tab that is not the shown one — and the
/// right-click leaves the shown tab alone, which is the whole difference between this menu and the
/// listing's.
#[test]
fn right_clicking_a_tab_opens_the_tab_menu_and_switches_to_nothing() {
    let (mut harness, _, docs) = two_tab_harness("menus-tab");
    assert_eq!(
        shown(&harness, PaneSlot::Left).path,
        fixture_path(&docs),
        "fixture: the second tab is the shown one"
    );

    let at = tab_point(&harness, PaneSlot::Left, 0);
    click_at(&mut harness, at, PointerButton::Secondary);
    assert!(is_open(&harness), "the tab right-click opened nothing");
    let open = harness.state().menu.clone().unwrap();
    assert_eq!(
        open.target,
        MenuTarget::Tab { ix: 0 },
        "the menu must name the tab under the cursor"
    );
    assert_eq!(
        open.slot,
        PaneSlot::Left,
        "the menu belongs to the pane that owns the strip"
    );
    assert_eq!(
        labels(&harness),
        [
            "Close tab",
            "Duplicate tab",
            "Copy path",
            "Copy name",
            "Red Group",
            "Blue Group",
            "Green Group",
            "Yellow Group",
            "Purple Group",
            "Remove Group",
        ],
        "`showTabContextMenu`'s rows, in `tabs.js:353-381`'s order"
    );
    assert_eq!(
        harness.state().model.panes[&PaneSlot::Left].active,
        1,
        "a right-click does not switch tabs — Go's `showTabContextMenu` never called `switchTab`"
    );
}

/// The verbs answer to the right-clicked tab, which is the one the pane is *not* showing.
#[test]
fn the_tab_menu_closes_the_tab_it_was_opened_over() {
    let (mut harness, _, docs) = two_tab_harness("menus-tab-close");
    let at = tab_point(&harness, PaneSlot::Left, 0);
    click_at(&mut harness, at, PointerButton::Secondary);
    let row = menu_row_point(&harness, "Close tab");
    one_frame_click(&mut harness, row);
    settle(&mut harness);
    assert!(!is_open(&harness), "a chosen item always closes the menu");

    let tabs = pane(&harness, PaneSlot::Left);
    assert_eq!(tabs.len(), 1, "one tab was closed, not both");
    assert_eq!(
        tabs[0].path,
        fixture_path(&docs),
        "the *shown* tab was closed instead of the right-clicked one"
    );
}

/// Go's `duplicateTab` (`tabs.js:365-367`) — of the tab under the cursor, beside it.
#[test]
fn the_tab_menu_duplicates_the_tab_it_was_opened_over() {
    let (mut harness, listing, _) = two_tab_harness("menus-tab-dup");
    // The right-clicked tab's own view and colour: the copy carries both, the listing carries none.
    let app = harness.state_mut();
    let tabs = &mut app
        .model
        .panes
        .get_mut(&PaneSlot::Left)
        .expect("fixture")
        .tabs;
    tabs[0].group = Some(GroupColor::A);
    tabs[0].view = ViewMode::Grid;
    let at = tab_point(&harness, PaneSlot::Left, 0);
    click_at(&mut harness, at, PointerButton::Secondary);
    let row = menu_row_point(&harness, "Duplicate tab");
    one_frame_click(&mut harness, row);
    settle(&mut harness);

    let tabs = pane(&harness, PaneSlot::Left);
    assert_eq!(tabs.len(), 3, "no tab was added");
    assert_eq!(tabs[0].path, tabs[1].path, "the copy sits beside it");
    assert_eq!(
        tabs[1].path,
        fixture_path(&listing),
        "of the right-clicked tab"
    );
    assert_ne!(tabs[1].id, tabs[0].id, "a tab id is not reused");
    assert_eq!(
        tabs[1].group,
        Some(GroupColor::A),
        "the group colour travels with the copy"
    );
    assert_eq!(
        tabs[1].view,
        ViewMode::Grid,
        "the view travels with the copy"
    );
    let pane = &harness.state().model.panes[&PaneSlot::Left];
    assert_eq!(pane.active, 1, "the copy is the shown tab");
}

/// `Copy path` / `Copy name` on the tab, not on the selection nor on the shown tab — the `data-tab-id`
/// half of Go's tab menu (`tabs.js:361`).
#[test]
fn the_tab_menu_copies_the_tab_it_was_opened_over() {
    let (mut harness, listing, docs) = two_tab_harness("menus-tab-copy");
    let at = tab_point(&harness, PaneSlot::Left, 0);
    click_at(&mut harness, at, PointerButton::Secondary);
    let row = menu_row_point(&harness, "Copy path");
    one_frame_click(&mut harness, row);
    assert_eq!(
        copied(&harness),
        Some(format!("{}\n", fixture_path(&listing))),
        "`Copy path` copied the shown tab ({docs:?}) instead of the right-clicked one"
    );

    // …and `Copy name` is that tab's own last segment.
    let at = tab_point(&harness, PaneSlot::Left, 0);
    click_at(&mut harness, at, PointerButton::Secondary);
    let row = menu_row_point(&harness, "Copy name");
    one_frame_click(&mut harness, row);
    assert_eq!(
        copied(&harness),
        Some("listing\n".to_string()),
        "`Copy name` is the tab's name, `copy_lines`'s newline and all"
    );
}

/// The tab-group block is the placeholder half of the menu: drawn so the shape matches Go's, greyed so
/// it fires nothing (`a_greyed_item_fires_nothing`'s rule, from the tab menu).
#[test]
fn the_tab_menus_group_rows_are_greyed_and_fire_nothing() {
    let (mut harness, _, _) = two_tab_harness("menus-tab-group");
    let at = tab_point(&harness, PaneSlot::Left, 0);
    click_at(&mut harness, at, PointerButton::Secondary);
    assert!(is_open(&harness), "fixture");

    let rows = rows(&harness);
    for label in [
        "Red Group",
        "Blue Group",
        "Green Group",
        "Yellow Group",
        "Purple Group",
        "Remove Group",
    ] {
        assert!(
            rows.iter()
                .any(|(row, enabled, _)| row == label && !*enabled),
            "{label} is not the greyed placeholder it is meant to be: {rows:?}"
        );
    }

    let before = pane(&harness, PaneSlot::Left);
    let row = menu_row_point(&harness, "Red Group");
    one_frame_click(&mut harness, row);
    harness.run();
    assert!(
        is_open(&harness),
        "a placeholder click dismissed the menu it did not act on"
    );
    let after = pane(&harness, PaneSlot::Left);
    assert_eq!(
        after.iter().map(|tab| tab.id).collect::<Vec<_>>(),
        before.iter().map(|tab| tab.id).collect::<Vec<_>>(),
        "a placeholder click touched the strip"
    );
    assert_eq!(
        harness.state().model.panes[&PaneSlot::Left].active,
        1,
        "and it switched tabs on the way"
    );
}

/// The mirror of `a_menu_does_not_outlive_its_pane` for the tab the menu was opened over: the strip
/// index the menu carries has to still name a tab. (It is the *last* tab here because closing an
/// earlier one leaves the index alive, pointing at whatever shifted into its place — a different
/// question, and not one a menu can answer for.)
#[test]
fn a_tab_menu_does_not_outlive_its_tab() {
    let (mut harness, listing, _) = two_tab_harness("menus-tab-stale");
    let at = tab_point(&harness, PaneSlot::Left, 1);
    click_at(&mut harness, at, PointerButton::Secondary);
    assert_eq!(
        harness.state().menu.as_ref().map(|open| open.target),
        Some(MenuTarget::Tab { ix: 1 }),
        "fixture: the menu was opened over the last tab"
    );

    harness
        .state_mut()
        .run_command(PaneSlot::Left, PaneCommand::CloseTab(1));
    harness.run();
    assert!(
        !is_open(&harness),
        "a menu survived the tab it was opened over"
    );
    let tabs = pane(&harness, PaneSlot::Left);
    assert_eq!(
        tabs.len(),
        1,
        "the other tab is none of this menu's business"
    );
    assert_eq!(tabs[0].path, fixture_path(&listing));
}
