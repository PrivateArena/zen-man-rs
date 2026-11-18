//! The shell skeleton as `egui_kittest` sees it: the widget tree and the state machine, asserted
//! head-lessly (no rasteriser, no window — `Harness` renders nothing unless asked).
//!
//! This is the assertable half of "empty panes render, header + drawers + tab strip visible"
//! (plan §7 step 7); the pixel half is `tests/snapshots.rs`, and the human review of that PNG
//! against `docs/parity/` is step 15's (plan §7: references are never pixel-diffed).

mod common;

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

use zenman_core::path::FsPath;
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
fn shell_harness(app: ZenApp) -> Harness<'static, ZenApp> {
    Harness::builder()
        .with_size(egui::vec2(1440.0, 900.0))
        .with_context_setup(|ctx| {
            egui_extras::install_image_loaders(ctx);
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

#[test]
fn the_header_status_bar_and_pane_placeholder_are_in_the_tree() {
    let mut harness = harness();
    harness.run();
    harness.get_by_label("Zen-Man");
    harness.get_by_label("test boot line");
    harness.get_by_label_contains("pane(s)");
    harness.get_by_label_contains("empty pane");
}

#[test]
fn header_controls_reach_the_widget_tree_and_the_layout_button_reaches_the_state() {
    let mut harness = harness();
    harness.run();
    for name in [
        "single pane",
        "split panes",
        "quad panes",
        "places",
        "disks",
        "action log",
    ] {
        harness.get_by_label(name);
    }
    // Clicking the quad button must change the *state*, not just the picture.
    harness.get_by_label("quad panes").click();
    harness.run();
    let app = harness.state();
    assert!(app.model.quad(), "the grid must follow the button");
    assert!(
        app.dock.layout.validate().is_ok(),
        "and the dock layout must stay valid"
    );
    assert_eq!(app.model.panes.len(), 4);
    // Back to single — the plan's F3 regression path, driven through the real button.
    harness.get_by_label("single pane").click();
    harness.run();
    assert!(harness.state().model.layout_mode_is_consistent());
    assert_eq!(harness.state().model.panes.len(), 1);
}

#[test]
fn drawers_toggle_from_their_header_buttons() {
    let mut harness = harness();
    harness.run();
    assert!(!harness.state().model.left_drawer.open);
    harness.get_by_label("places").click();
    harness.run();
    assert!(harness.state().model.left_drawer.open);
    harness.get_by_label("Places");
    harness.get_by_label("disks").click();
    harness.run();
    assert!(harness.state().model.right_drawer.open);
    harness.get_by_label("Disks");
    harness.get_by_label("action log").click();
    harness.run();
    assert!(harness.state().model.log_panel);
    harness.get_by_label("Action log");
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
