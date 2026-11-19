//! Drag & drop between panes and onto directory rows, driven with real pointer events (plan §7 row 10,
//! §5.8 `drag_drop.rs`).
//!
//! The three things worth testing are the three things Go's HTML5 drag did: a drag *carries the
//! selection* and pastes it into the folder row it lands on (`file-list.js:298-345`), the clipboard's
//! op decides copy vs move (`effectAllowed = 'move'`, and `if (operation === 'cut')` removes the
//! sources), and only directory rows accept the drop (`dragover` checks `isDir`). The port adds
//! <kbd>Ctrl</kbd> = copy at release, which Go's browser never read, and runs the same collision
//! pre-flight the clipboard chords do.

mod common;

use std::path::{Path, PathBuf};

use egui::{Event, Modifiers, PointerButton, Pos2, Vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

use zenman_core::path::FsPath;
use zenman_core::store::config::{LayoutMode, PaneSlot};
use zenman_ui::app::ZenApp;
use zenman_ui::drag_drop::DragMode;
use zenman_ui::state::panes_for;

const SETTLE: usize = 6;

// --------------------------------------------------------------------- fixture + harness

fn scratch(tag: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("zenman-ui-fixtures")
        .join(format!("zenman-ui-fixture-{tag}"));
    common::remove_tree(&root);
    root
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap_or(path)).expect("fixture directory");
    std::fs::write(path, bytes).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
}

fn build_harness(app: ZenApp, size: Vec2) -> Harness<'static, ZenApp> {
    let mut harness = Harness::builder()
        .with_size(size)
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

fn settle(harness: &mut Harness<'static, ZenApp>) {
    for _ in 0..SETTLE {
        harness.run();
    }
}

// --------------------------------------------------------------------- the drag itself

/// Press on `from`, walk to `to`, release there — the event sequence a real drag produces, including
/// the movement past egui's drag threshold before the grab registers.
fn drag(harness: &mut Harness<'static, ZenApp>, from: Pos2, to: Pos2, modifiers: Modifiers) {
    // The modifier state is *held*, not per-event: egui's `InputState::modifiers` is what the drop
    // target reads, and it comes from `RawInput::modifiers` plus `ModifiersChanged` events, so a
    // release that merely carries `ctrl` on its own event would not be enough.
    harness
        .input_mut()
        .events
        .push(Event::ModifiersChanged(modifiers));
    push_button(harness, from, true, modifiers);
    move_to(harness, from);
    // Eight steps, written as fractions so no integer has to become a float.
    for t in [0.125_f32, 0.25, 0.375, 0.5, 0.625, 0.75, 0.875, 1.0] {
        move_to(harness, from.lerp(to, t));
    }
    push_button(harness, to, false, modifiers);
    settle(harness);
}

fn push_button(
    harness: &mut Harness<'static, ZenApp>,
    at: Pos2,
    pressed: bool,
    modifiers: Modifiers,
) {
    harness.input_mut().events.push(Event::PointerButton {
        pos: at,
        button: PointerButton::Primary,
        pressed,
        modifiers,
    });
    harness.run();
}

fn move_to(harness: &mut Harness<'static, ZenApp>, at: Pos2) {
    harness.input_mut().events.push(Event::PointerMoved(at));
    harness.run();
}

fn centre(harness: &Harness<'static, ZenApp>, label: &str) -> Pos2 {
    harness.get_by_label(label).rect().center()
}

/// A point in the empty band below `label`'s row — the pane body, not a row.
fn below(harness: &Harness<'static, ZenApp>, label: &str) -> Pos2 {
    let rect = harness.get_by_label(label).rect();
    Pos2::new(rect.center().x, rect.max.y + 90.0)
}

/// A single-pane listing of `root`, which holds `dirs` (each with `dir_files`) and `files`.
fn one_pane(
    tag: &str,
    dirs: &[(&str, &[&str])],
    files: &[(&str, &[u8])],
) -> (PathBuf, Harness<'static, ZenApp>) {
    let root = scratch(tag);
    std::fs::create_dir_all(&root).expect("fixture root");
    for (dir, inner) in dirs {
        // The directory exists whether or not it has contents — an empty one is the interesting drop
        // target, and `write` alone would not have created it.
        std::fs::create_dir_all(root.join(dir)).expect("fixture directory");
        for name in inner.iter() {
            write(&root.join(dir).join(name), b"inside\n");
        }
    }
    for (name, bytes) in files {
        write(&root.join(name), bytes);
    }
    let session = common::session(tag);
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Single));
    app.open_tab(PaneSlot::Left, FsPath::from_abs(absolute(&root)));
    let harness = build_harness(app, egui::vec2(1200.0, 800.0));
    (root, harness)
}

/// Two panes, left listing `left`, right listing `right`.
fn two_panes(tag: &str, left: &Path, right: &Path) -> Harness<'static, ZenApp> {
    let session = common::session(tag);
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Split));
    app.open_tab(PaneSlot::Left, FsPath::from_abs(absolute(left)));
    app.open_tab(PaneSlot::Right, FsPath::from_abs(absolute(right)));
    build_harness(app, egui::vec2(1600.0, 800.0))
}

fn absolute(path: &Path) -> zenman_core::path::AbsPath {
    zenman_core::path::AbsPath::try_from(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

// ------------------------------------------------------------------------------- the tests

/// No clipboard at all: a Go drag is a **move** (`effectAllowed = 'move'`), so the source disappears.
#[test]
fn dragging_a_row_onto_a_folder_row_moves_it() {
    let (root, mut harness) = one_pane("dnd-move", &[("alpha", &[])], &[("top.txt", b"move me\n")]);
    let from = centre(&harness, "top.txt");
    let to = centre(&harness, "alpha");
    drag(&mut harness, from, to, Modifiers::default());
    common::settle_until(&mut harness, "the move lands", |_| {
        root.join("alpha").join("top.txt").is_file() && !root.join("top.txt").exists()
    });
    assert_eq!(
        std::fs::read(root.join("alpha").join("top.txt")).expect("moved file"),
        b"move me\n",
        "the bytes came along"
    );
    assert!(
        harness.state().drag.is_none(),
        "the drag ended with the release"
    );
}

/// <kbd>Ctrl</kbd> at the release is the port's addition (Go read no modifier): the source stays.
#[test]
fn ctrl_drag_copies_and_leaves_the_source_where_it_was() {
    let (root, mut harness) = one_pane("dnd-copy", &[("beta", &[])], &[("keep.txt", b"copy me\n")]);
    let from = centre(&harness, "keep.txt");
    let to = centre(&harness, "beta");
    let ctrl = Modifiers {
        ctrl: true,
        command: true,
        ..Modifiers::NONE
    };
    drag(&mut harness, from, to, ctrl);
    common::settle_until(&mut harness, "the copy lands", |_| {
        root.join("beta").join("keep.txt").is_file()
    });
    assert!(
        root.join("keep.txt").is_file(),
        "Ctrl turned the move into a copy: the source is untouched"
    );
}

/// A cut clipboard makes the drag a move even though a drag has no clipboard of its own — Go's
/// `if (operation === 'cut')` branch (`file-list.js:331-345`).
#[test]
fn a_copy_clipboard_makes_the_drop_a_copy_and_a_cut_clipboard_a_move() {
    let (root, mut harness) = one_pane(
        "dnd-clip",
        &[("gamma", &[])],
        &[("clip.txt", b"clipboard\n")],
    );
    let source = zenman_core::path::AbsPath::try_from(root.join("clip.txt").as_path())
        .unwrap_or_else(|e| panic!("{e}"));
    harness
        .state()
        .session
        .clipboard_store(&zenman_core::ops::clipboard::ClipboardStore {
            op: zenman_core::ops::clipboard::ClipOp::Copy,
            sources: vec![source.clone()],
            scope: zenman_core::ops::clipboard::ClipScope::Selected,
        })
        .expect("the clipboard takes the copy");
    // The mode a grab means with a *copy* clipboard is Copy; a cut clipboard's is Move (Go's op).
    let copy_clip = harness.state().session.clipboard();
    assert_eq!(
        zenman_ui::drag_drop::grab_mode(copy_clip.as_ref()),
        DragMode::Copy,
        "a copy clipboard drags as a copy"
    );
    harness
        .state()
        .session
        .clipboard_store(&zenman_core::ops::clipboard::ClipboardStore {
            op: zenman_core::ops::clipboard::ClipOp::Cut,
            sources: vec![source],
            scope: zenman_core::ops::clipboard::ClipScope::Selected,
        })
        .expect("the clipboard takes the cut");
    let cut_clip = harness.state().session.clipboard();
    assert_eq!(
        zenman_ui::drag_drop::grab_mode(cut_clip.as_ref()),
        DragMode::Move,
        "a cut clipboard drags as a move"
    );
    let from = centre(&harness, "clip.txt");
    let to = centre(&harness, "gamma");
    drag(&mut harness, from, to, Modifiers::default());
    common::settle_until(&mut harness, "the cut-drag moves the file", |_| {
        root.join("gamma").join("clip.txt").is_file() && !root.join("clip.txt").exists()
    });
}

/// The cross-pane case: the drop lands on the *other* pane's background, i.e. `DropIntent::PasteInto`.
#[test]
fn dragging_into_another_pane_moves_the_file_into_its_directory() {
    let left = scratch("dnd-pane-left");
    let right = scratch("dnd-pane-right");
    std::fs::create_dir_all(&left).expect("left");
    std::fs::create_dir_all(&right).expect("right");
    write(&left.join("travel.txt"), b"cross pane\n");
    write(&right.join("resident.txt"), b"already here\n");
    let mut harness = two_panes("dnd-panes", &left, &right);
    let from = centre(&harness, "travel.txt");
    let to = below(&harness, "resident.txt");
    drag(&mut harness, from, to, Modifiers::default());
    common::settle_until(&mut harness, "the cross-pane move lands", |_| {
        right.join("travel.txt").is_file() && !left.join("travel.txt").exists()
    });
    assert!(
        right.join("resident.txt").is_file(),
        "the destination's own files are untouched"
    );
}

/// A drop whose name is already taken asks the same three-way question as a paste — before writing.
#[test]
fn a_drop_onto_a_taken_name_asks_before_it_touches_anything() {
    let src = scratch("dnd-clash-src");
    let dst = scratch("dnd-clash-dst");
    std::fs::create_dir_all(&src).expect("src");
    std::fs::create_dir_all(&dst).expect("dst");
    write(&src.join("dup.txt"), b"incoming\n");
    write(&dst.join("dup.txt"), b"existing\n");
    let mut harness = two_panes("dnd-clash", &src, &dst);
    let left_row = harness
        .query_all_by_label("dup.txt")
        .next()
        .map_or(Pos2::ZERO, |node| node.rect().center());
    drag(
        &mut harness,
        left_row,
        Pos2::new(1200.0, 600.0),
        Modifiers::default(),
    );
    assert!(
        harness.state().dialogs.is_open(),
        "the collision is asked about, not resolved silently"
    );
    assert_eq!(
        std::fs::read(dst.join("dup.txt")).expect("readable"),
        b"existing\n",
        "asking wrote nothing"
    );
    harness.get_by_label("Skip").click();
    settle(&mut harness);
    assert_eq!(
        std::fs::read(dst.join("dup.txt")).expect("readable"),
        b"existing\n",
        "and the answer that keeps the bytes keeps them"
    );
}

/// A drag that lands on nothing is dropped, not queued: the next click is not a secret paste.
#[test]
fn a_drag_released_on_nothing_pastes_nothing() {
    let (root, mut harness) = one_pane(
        "dnd-nowhere",
        &[("delta", &[])],
        &[("stray.txt", b"nowhere\n")],
    );
    let from = centre(&harness, "stray.txt");
    // Release in the far corner: below the rows, but the pane's own background takes it and the
    // directory is the source's own — a no-op that must not touch the disk.
    let to = Pos2::new(1150.0, 780.0);
    drag(&mut harness, from, to, Modifiers::default());
    settle(&mut harness);
    assert!(
        harness.state().drag.is_none(),
        "the payload is gone with the release"
    );
    assert!(
        root.join("stray.txt").is_file(),
        "a no-op drop leaves the file where it was"
    );
    assert_eq!(
        std::fs::read_dir(root.join("delta"))
            .expect("readable")
            .count(),
        0,
        "and puts nothing in the folder"
    );
}
