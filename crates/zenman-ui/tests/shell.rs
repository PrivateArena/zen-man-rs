//! The shell skeleton as `egui_kittest` sees it: the widget tree and the state machine, asserted
//! head-lessly (no rasteriser, no window — `Harness` renders nothing unless asked).
//!
//! This is the assertable half of "empty panes render, header + drawers + tab strip visible"
//! (plan §7 step 7); the pixel half is `tests/snapshots.rs`, and the human review of that PNG
//! against `docs/parity/` is step 15's (plan §7: references are never pixel-diffed).

mod common;

use std::path::Path;

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

use zenman_core::path::{AbsPath, FsPath};
use zenman_core::store::config::{LayoutMode, PaneSlot};
use zenman_ui::app::ZenApp;
use zenman_ui::state::{Tab, panes_for, quad_slots};

/// One harness, sized like the real window, with the SVG loaders installed the way `apps/zenman`
/// installs them (so icon painting exercises the real registry path).
fn harness() -> Harness<'static, ZenApp> {
    let session = common::session("kittest");
    let app = ZenApp::new(session, vec!["test boot line".into()]);
    shell_harness(app)
}

/// The one harness shape every test uses: window-sized, SVG loaders installed (the icon registry
/// needs them exactly as `apps/zenman` installs them), dark theme applied.
///
/// The accessibility check is **on**: every control the chrome draws — the workspace button, the
/// address buttons, each icon in the navigation ribbon — publishes a name, because an icon with no
/// name is unreachable by keyboard, test or screen reader alike.
fn shell_harness(app: ZenApp) -> Harness<'static, ZenApp> {
    Harness::builder()
        .with_accessibility_check(true)
        .with_size(egui::vec2(1440.0, 900.0))
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
        )
}

/// A press-and-release at one point of the window: what a click on a pane body is, with no widget
/// to grab hold of.
fn press_at(harness: &mut Harness<'static, ZenApp>, at: egui::Pos2) {
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

/// The fixture directory as the shell's path type.
fn fixture_path(path: &Path) -> FsPath {
    FsPath::from_abs(
        AbsPath::try_from(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
    )
}

/// Click the first control of one name. Every pane carries its own navigation ribbon, so a window
/// verb exists once per pane in a grid — `get_by_label`'s "exactly one" is the wrong demand here.
fn click_chrome(harness: &mut Harness<'static, ZenApp>, label: &str) {
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

#[test]
fn the_chrome_status_bar_and_pane_placeholder_are_in_the_tree() {
    let mut harness = harness();
    harness.run();
    // One button where Go had `Workspace:` + `New` + `Delete` (`panels/header.rs`).
    harness.get_by_label("workspace");
    // The boot line lost its header cell and gained the status bar's middle.
    harness.get_by_label("test boot line");
    // Step 7's placeholder status line is gone: the bar is the real one (`panels/status_bar.rs`),
    // so the assertions are its own words — the selection cell and the pane cell.
    harness.get_by_label_contains("selected");
    harness.get_by_label_contains("in the listing");
    // And step 7's "empty pane" placeholder is now the no-tab placeholder (step 8 fills a pane that
    // *has* a tab with the listing; this harness has none, which is the other half of the state).
    harness.get_by_label_contains("no tab");
}

#[test]
fn chrome_controls_reach_the_widget_tree_and_the_layout_button_reaches_the_state() {
    let mut harness = harness();
    harness.run();
    // The navigation bar's global group: every one of them an icon with an accessible name, the
    // chord living in the tooltip rather than the label (`nav_bar.rs`). There is no single-pane
    // button: Go's two layout buttons *are* the F3/F4 chords and toggle themselves off.
    for name in ["history", "places", "custom actions", "2-pane", "4-pane"] {
        harness.get_by_label(name);
    }
    // Plan **D7**: one chord table, two front ends. The button no longer prints its key, so the
    // tooltip is what has to agree with the keymap — and it is read out of `KeyMap::hint`.
    let hint = harness
        .state()
        .keys
        .hint_text(zenman_ui::keymap::Command::QuadLayout);
    assert_eq!(hint, "F4", "the test must know the real chord");
    harness.get_by_label("4-pane").hover();
    harness.run();
    assert!(
        harness
            .query_all_by_label_contains(&format!("4-pane ({hint})"))
            .count()
            > 0,
        "the ribbon's tooltip did not carry `KeyMap::hint`'s chord ({hint})"
    );
    // Clicking the quad button must change the *state*, not just the picture.
    click_chrome(&mut harness, "4-pane");
    let app = harness.state();
    assert!(app.model.quad(), "the grid must follow the button");
    assert!(
        app.dock.layout.validate().is_ok(),
        "and the dock layout must stay valid"
    );
    assert_eq!(app.model.panes.len(), 4);
    // The same button again is the off-state — plan F3's regression path, driven through the real
    // control that now owns it.
    click_chrome(&mut harness, "4-pane");
    assert!(
        !harness.state().model.quad() && harness.state().model.layout_mode_is_consistent(),
        "and 4-pane toggles itself back off"
    );
    assert_eq!(harness.state().model.panes.len(), 1);
    click_chrome(&mut harness, "2-pane");
    assert_eq!(harness.state().model.panes.len(), 2);
    click_chrome(&mut harness, "2-pane");
    assert_eq!(
        harness.state().model.panes.len(),
        1,
        "and 2-pane toggles too"
    );
}

#[test]
fn drawers_toggle_from_their_ribbon_buttons() {
    let mut harness = harness();
    harness.run();
    assert!(!harness.state().model.left_drawer.open);
    click_chrome(&mut harness, "places");
    assert!(harness.state().model.left_drawer.open);
    // Step 15b: the drawers hold their real content — Go's section titles (uppercase, per
    // `.sidebar-section-title`) and the history panel's Go title.
    harness.get_by_label("PLACES");
    click_chrome(&mut harness, "custom actions");
    assert!(harness.state().model.right_drawer.open);
    // Go's right drawer is headed by its own `.sidebar-header h2`, not by a section title.
    harness.get_by_label("Custom Actions");
    click_chrome(&mut harness, "history");
    assert!(harness.state().model.log_panel);
    harness.get_by_label("Action History");
    // The left drawer's header is Go's `Zen-Man` (`index.html:21`) — the product name belongs to the
    // drawer and the window title, not to the global header.
    harness.get_by_label("Zen-Man");
}

#[test]
fn the_layout_button_opens_a_pane_that_is_already_live() {
    // The reported defect: the 2-pane button drew a second panel with no address bar, so nothing
    // (sidebar, ribbon, keys) could drive it. Go's `updateLayout` seeds it; this is that, through the
    // real control and a real click.
    let tag = "shell-live-pane";
    let dir = common::listing_tree(tag);
    let session = common::session(tag);
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Single));
    app.open_tab(PaneSlot::Left, fixture_path(&dir));
    let mut harness = shell_harness(app);
    harness.run();

    click_chrome(&mut harness, "2-pane");
    let paths: Vec<Option<FsPath>> = quad_slots()
        .into_iter()
        .filter(|slot| harness.state().model.panes.contains_key(slot))
        .map(|slot| {
            harness
                .state()
                .model
                .panes
                .get(&slot)
                .and_then(|pane| pane.active_tab())
                .map(|tab| tab.path.clone())
        })
        .collect();
    assert_eq!(
        paths,
        vec![Some(fixture_path(&dir)); 2],
        "both panes of the split show a directory — neither is a dead panel"
    );

    // A press in the new pane's body focuses it (the whole pane is the click target, as in Go's
    // `setupPaneListeners`), which is what makes the drawer's rows land there.
    press_at(&mut harness, egui::pos2(1_100.0, 500.0));
    assert_eq!(
        harness.state().model.active_pane,
        PaneSlot::Right,
        "the new pane takes the focus on a click in its listing"
    );
}

#[test]
fn a_narrow_pane_folds_its_ribbon_before_it_starves_the_address_bar() {
    // The ribbon is capped by the address strip's floor, not by a button count: a pane too narrow
    // for the whole ribbon puts it behind one overflow button instead of eating the trail.
    let session = common::session("fold");
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(quad_slots());
    // With tabs, because the point of the fold is that the chrome gives room back to the panes: a
    // workspace control that grew with its name used to push the whole strip out of a narrow leaf.
    for (ix, slot) in quad_slots().into_iter().enumerate() {
        let id = app.model.next_tab_id();
        app.model
            .pane_mut(slot)
            .tabs
            .push(Tab::new(id, FsPath::ROOT, format!("leaf {ix}")));
    }
    let mut harness = Harness::builder()
        .with_accessibility_check(true)
        .with_size(egui::vec2(900.0, 480.0))
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
    // Four panes in a 900x480 window: every one of them below the fold.
    assert_eq!(
        harness.query_all_by_label("More controls").count(),
        4,
        "each pane's ribbon folded into its own overflow button"
    );
    assert_eq!(
        harness.query_all_by_label("4-pane").count(),
        0,
        "and not one of them kept a global button inline"
    );
    // The window's verbs are still one click away — that is the difference between folded and gone.
    click_chrome(&mut harness, "More controls");
    assert!(
        harness
            .query_all_by_label_contains("Action history")
            .count()
            > 0,
        "the overflow menu did not open its rows"
    );
    // And the tabs survived: one close button per pane, every one of them inside the window.
    for node in harness.query_all_by_label("Close tab") {
        assert!(
            node.rect().max.x <= 900.0 && node.rect().min.x >= 0.0,
            "a tab of the strip was pushed out of the window: {:?}",
            node.rect()
        );
    }
    assert_eq!(
        harness.query_all_by_label("Close tab").count(),
        4,
        "the workspace control ate a pane's whole tab strip"
    );
}

#[test]
fn the_grid_restructures_with_the_same_layout_entry_points_the_binary_will_use() {
    // State-level twin of the click test: every mode reachable, dock valid, panes consistent.
    let session = common::session("grid");
    let mut app = ZenApp::new(session, Vec::new());
    for slots in [
        panes_for(LayoutMode::Single),
        panes_for(LayoutMode::Split),
        quad_slots(),
    ] {
        app.set_layout(slots.clone());
        assert_eq!(app.model.panes.len(), slots.len());
        assert!(app.dock.layout.validate().is_ok());
        if slots.len() < 4 {
            assert!(app.model.layout_mode_is_consistent());
        }
    }
}

#[test]
fn a_tab_title_shows_in_the_pane() {
    let session = common::session("strip");
    let mut app = ZenApp::new(session, Vec::new());
    let id = app.model.next_tab_id();
    let tab = Tab::new(id, FsPath::ROOT, "downloads");
    app.model.pane_mut(PaneSlot::Left).tabs.push(tab);
    let mut harness = shell_harness(app);
    harness.run();
    // The pane header and the strip both name the tab; the placeholder is a real label.
    harness.get_by_label("downloads");
}
