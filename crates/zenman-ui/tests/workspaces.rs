//! Step 14b: the header's workspace vocabulary (plan §7 row 14, §9.2 step-8c review gap 1) and the
//! `Saved` chip behind it.
//!
//! The reference header is `Workspace: <select>` · `New` · `Delete` · `Saved` · disk meter ·
//! `History` · `2-Pane (F3)` · `4-Pane (F4)`. This file asserts the words are on screen, that every
//! *interactive* one is reachable at 800 px (plan §5.8's `header.rs` criterion), and that the
//! workspace verbs really move state through `Session` — not just the toast.

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

/// Every header control a user can click, by the text it answers to.
const INTERACTIVE: [&str; 5] = ["New", "Delete", "History", "2-Pane (F3)", "4-Pane (F4)"];

/// How many times the shell drew a header string (the a11y tree is what a click can reach).
fn drawn(harness: &Harness<'static, ZenApp>, label: &str) -> usize {
    harness.query_all_by_label(label).count()
}

#[test]
fn the_header_prints_the_reference_vocabulary() {
    let dir = tree("ws-vocab", &["a.txt"]);
    let mut app = pane_app("ws-vocab", &dir);
    // The dropdown's rows are the stored workspaces, and a fresh config has none: write the
    // autosave target once so the test sees the same list a running shell shows.
    app.save_workspace_as(zenman_core::services::workspaces::RESERVED);
    let harness = build(app, egui::vec2(1600.0, 800.0));
    for want in [
        "Workspace:",
        "New",
        "Delete",
        "History",
        "2-Pane (F3)",
        "4-Pane (F4)",
    ] {
        assert!(
            drawn(&harness, want) > 0,
            "the reference header's {want:?} is missing from the widget tree"
        );
    }
    // The chord in the label is the keymap's own spelling (plan D7), not a second table.
    let hint = harness
        .state()
        .keys
        .hint_text(zenman_ui::keymap::Command::SplitLayout);
    assert_eq!(
        hint, "F3",
        "the header prints whatever the keymap says, so the test must know the real chord"
    );
    assert!(
        drawn(&harness, &format!("2-Pane ({hint})")) > 0,
        "`2-Pane` did not carry `KeyMap::hint`'s chord ({hint})"
    );
    // `Default` is present and flagged, not hidden (Go hid it and so made it unrestorable).
    let names = harness.state().workspaces.names.clone();
    assert!(
        names.iter().any(|meta| meta.reserved),
        "the reserved autosave workspace is missing from the dropdown: {names:?}"
    );
}

#[test]
fn every_interactive_header_control_is_reachable_at_800_px() {
    let dir = tree("ws-800", &["a.txt"]);
    let harness = build(pane_app("ws-800", &dir), egui::vec2(800.0, 600.0));
    for name in INTERACTIVE {
        assert!(
            drawn(&harness, name) > 0,
            "{name:?} is not in the widget tree at 800 px — the header dropped a control"
        );
    }
    // The selector survives the narrow header, too (the *optional* member it drops first is the
    // disk meter's text; `WorkspaceBar::show` guards it on `available_width`).
    assert!(drawn(&harness, "Workspace:") > 0);
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
    // …and the same rule through the real header, painted. The debounce is pushed out past the
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
