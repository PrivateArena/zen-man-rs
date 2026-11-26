//! The queued clipboard, seen from the two places it shows: the file list, whose rows dim while they
//! are queued for a **move** (Go's `.cut-state`, `file-list.css:41-43`, a class Go shipped and never
//! applied), and the status bar's second indicator — Go's amber underlined `✂ N items in action`.
//!
//! Both are the *same* fact: the session clipboard. The pane cannot ask the engine per row (that is a
//! mutex per row per pane), so the shell projects the queue onto a set once per frame
//! ([`ZenApp::cut_clip`]) and every row is painted from it. These tests drive the real chord over a
//! real `Session` and assert on all three links of that chain:
//!
//! * the queue the frame carries (exactly the cut rows' absolute paths),
//! * the state each row would paint ([`row_state`], the same call `paint_row` makes),
//! * the label the bar prints (found through the accessibility tree, the way a screen reader finds it).
//!
//! A **copy** is the negative case throughout: Go queued it the same way and dimmed nothing, because
//! its source survives the paste.

mod common;

use std::path::{Path, PathBuf};

use egui::Event;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

use zenman_core::path::{AbsPath, FsPath};
use zenman_core::store::config::{LayoutMode, PaneSlot};
use zenman_ui::app::ZenApp;
use zenman_ui::keymap::{Chord, Command, KeyMap};
use zenman_ui::panels::file_pane::row_state;
use zenman_ui::state::{Tab, panes_for};
use zenman_ui::widgets::entry_row::SelectionState;

/// Frames to let the shell settle after an action (the budget `tests/keymap.rs` uses).
const SETTLE: usize = 6;

/// The words the bar prints for the queue — the label the tests look for.
const IN_ACTION: &str = "items in action";

// --------------------------------------------------------------------- harness plumbing

/// A fixture directory with only the named files: enough for a two-row selection, and a third row
/// that must *not* dim.
fn tree(tag: &str, files: &[&str]) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("zenman-ui-fixtures")
        .join(format!("zenman-ui-fixture-{tag}"));
    std::fs::create_dir_all(&root).expect("fixture directory");
    for name in files {
        std::fs::write(root.join(name), b"clipboard fixture\n").expect("fixture file");
    }
    root
}

fn fixture_path(path: &Path) -> FsPath {
    FsPath::from_abs(abs(path))
}

fn abs(path: &Path) -> AbsPath {
    AbsPath::try_from(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// A single-pane shell showing `dir`, drawn through the fork's `logic`/`ui` pair as `apps/zenman`
/// does — the pair the cut set is refreshed on.
fn pane_harness(tag: &str, dir: &Path) -> Harness<'static, ZenApp> {
    let session = common::session(tag);
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Single));
    app.open_tab(PaneSlot::Left, fixture_path(dir));
    let mut harness = Harness::builder()
        .with_accessibility_check(true)
        .with_size(egui::vec2(1200.0, 800.0))
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

/// Press the chord a command's own hint spells (the `tests/keymap.rs` route).
fn press_command(harness: &mut Harness<'static, ZenApp>, command: Command) {
    let map = KeyMap::default();
    let chord = map
        .hint(command)
        .unwrap_or_else(|| panic!("{command:?} has no chord to press"));
    let parsed = Chord::parse(&chord).unwrap_or_else(|| panic!("hint {chord:?} does not parse"));
    harness.input_mut().events.push(parsed.event());
    harness.run();
    harness.run();
}

/// The tab the pane shows.
fn shown<'h>(harness: &'h Harness<'static, ZenApp>) -> &'h Tab {
    harness
        .state()
        .model
        .panes
        .get(&PaneSlot::Left)
        .and_then(|p| p.active_tab())
        .expect("the harness has a shown tab")
}

/// Select two rows the way the mouse does it: a click, then a ctrl-click added to it.
fn select_two(harness: &mut Harness<'static, ZenApp>) {
    harness.get_by_label("a.txt").click();
    settle(harness);
    harness
        .get_by_label("b.txt")
        .click_modifiers(egui::Modifiers {
            ctrl: true,
            ..Default::default()
        });
    settle(harness);
    assert_eq!(shown(harness).sel.len(), 2, "two rows are selected");
}

/// How many labels in the frame contain `needle`.
fn labels(harness: &Harness<'static, ZenApp>, needle: &str) -> usize {
    harness.query_all_by_label_contains(needle).count()
}

/// Where the pointer is: how a test aims its next move without knowing any panel's geometry.
fn pointer(harness: &Harness<'static, ZenApp>) -> egui::Pos2 {
    harness
        .ctx
        .input(|i| i.pointer.hover_pos())
        .expect("the pointer is somewhere")
}

/// What the pane would paint for every loaded row **this frame**: the shell's own cut set fed
/// through the same call [`row_state`] the row painter makes. This is the link between the engine's
/// queue and the row's colour — a set that was built but never consulted would fail here.
fn painted(harness: &Harness<'static, ZenApp>) -> Vec<(String, SelectionState)> {
    let cut = harness.state().cut_clip();
    let tab = shown(harness);
    tab.entries
        .entries
        .iter()
        .map(|entry| {
            let state = row_state(
                tab.row_abs(entry).as_ref(),
                tab.sel.contains(Tab::row_key(entry)),
                cut.as_deref(),
            );
            (entry.name.clone(), state)
        })
        .collect()
}

/// The state one named row paints as.
fn painted_as(harness: &Harness<'static, ZenApp>, name: &str) -> SelectionState {
    painted(harness)
        .into_iter()
        .find(|(row, _)| row == name)
        .unwrap_or_else(|| panic!("no row {name:?} was painted"))
        .1
}

// ------------------------------------------------------------------------- the cut queue

#[test]
fn a_cut_arms_the_indicator_and_dims_exactly_the_queued_rows() {
    let dir = tree("clip-cut", &["a.txt", "b.txt", "c.txt"]);
    let mut harness = pane_harness("clip-cut", &dir);
    select_two(&mut harness);
    assert!(
        harness.state().cut_clip().is_none(),
        "a selection alone queues nothing"
    );
    assert_eq!(
        painted_as(&harness, "a.txt"),
        SelectionState::Selected,
        "and an un-queued row is only selected"
    );

    press_command(&mut harness, Command::Cut);
    settle(&mut harness);

    // The bar's second indicator, in Go's words.
    assert!(
        labels(&harness, &format!("2 {IN_ACTION}")) > 0,
        "the status bar did not report the queue"
    );

    // The frame carries exactly the two cut rows, by absolute path — the key Go's class used.
    let queue = harness.state().cut_clip().expect("a cut queue");
    assert_eq!(queue.len(), 2, "{queue:?}");
    for name in ["a.txt", "b.txt"] {
        assert!(
            queue.contains(&abs(&dir.join(name))),
            "{name} is queued and the frame must say so"
        );
    }
    assert!(
        !queue.contains(&abs(&dir.join("c.txt"))),
        "the row nobody touched is not queued"
    );

    // And the rows the pane paints follow: selected *and* queued, which is `SelectedCut`.
    assert_eq!(painted_as(&harness, "a.txt"), SelectionState::SelectedCut);
    assert_eq!(painted_as(&harness, "b.txt"), SelectionState::SelectedCut);
    assert_eq!(painted_as(&harness, "c.txt"), SelectionState::Normal);
}

#[test]
fn a_copy_shows_the_indicator_and_dims_nothing() {
    let dir = tree("clip-copy", &["a.txt", "b.txt", "c.txt"]);
    let mut harness = pane_harness("clip-copy", &dir);
    select_two(&mut harness);

    press_command(&mut harness, Command::Copy);
    settle(&mut harness);

    assert!(
        labels(&harness, &format!("2 {IN_ACTION}")) > 0,
        "a copy is in action too — Go's indicator changed glyph, not existence"
    );
    assert!(
        harness.state().cut_clip().is_none(),
        "a copy queue must never reach the rows: its source survives the paste"
    );
    assert_eq!(painted_as(&harness, "a.txt"), SelectionState::Selected);
    assert_eq!(painted_as(&harness, "b.txt"), SelectionState::Selected);
}

#[test]
fn a_cleared_clipboard_takes_the_indicator_and_the_dimming_with_it() {
    // A cut that pastes clears the queue (`session.rs`'s `clipboard_clear`, Go `api/ops.go:339-344`);
    // the clear itself is the engine's, so this is the shell's half of that promise.
    let dir = tree("clip-clear", &["a.txt", "b.txt", "c.txt"]);
    let mut harness = pane_harness("clip-clear", &dir);
    select_two(&mut harness);
    press_command(&mut harness, Command::Cut);
    settle(&mut harness);
    assert!(labels(&harness, IN_ACTION) > 0, "the queue is up");
    assert_eq!(painted_as(&harness, "a.txt"), SelectionState::SelectedCut);

    harness.state_mut().session.clipboard_clear();
    settle(&mut harness);

    assert_eq!(
        labels(&harness, IN_ACTION),
        0,
        "the indicator outlives the queue by no frame at all"
    );
    assert!(harness.state().cut_clip().is_none());
    // The selection is untouched by the clear, so the rows keep their selection colour — only the
    // dimming goes.
    assert_eq!(painted_as(&harness, "a.txt"), SelectionState::Selected);
    assert_eq!(painted_as(&harness, "c.txt"), SelectionState::Normal);
}

#[test]
fn the_indicator_names_rows_the_pane_does_not_show() {
    // The queue is the session's, not the pane's: after a Cut Inside — and after any cut whose rows
    // have since scrolled away — the bar still says what is waiting to be moved.
    let dir = tree("clip-invisible", &["a.txt", "b.txt", "c.txt"]);
    let mut harness = pane_harness("clip-invisible", &dir);
    select_two(&mut harness);
    press_command(&mut harness, Command::Cut);
    settle(&mut harness);
    let queued = harness.state().cut_clip().expect("a cut queue").len();

    // Deselect the rows: the indicator is not the selection, and must not vanish with it.
    harness
        .get_by_label("a.txt")
        .click_modifiers(egui::Modifiers {
            ctrl: true,
            ..Default::default()
        });
    harness
        .get_by_label("b.txt")
        .click_modifiers(egui::Modifiers {
            ctrl: true,
            ..Default::default()
        });
    settle(&mut harness);

    assert_eq!(shown(&harness).sel.len(), 0, "the selection is empty now");
    assert_eq!(
        harness.state().cut_clip().expect("still queued").len(),
        queued,
        "the queue is not"
    );
    assert!(
        labels(&harness, &format!("2 {IN_ACTION}")) > 0,
        "and the bar still says so"
    );
    // A queued row that is not selected any more is `Cut`: dimmed, unhighlighted.
    assert_eq!(painted_as(&harness, "a.txt"), SelectionState::Cut);
}

#[test]
fn hovering_an_indicator_opens_its_titled_item_list() {
    // Go's two hover lists (`SELECTED ITEMS (n)` / `IN-ACTION ITEMS (n)`), through the real pointer.
    let dir = tree("clip-popups", &["a.txt", "b.txt", "c.txt"]);
    let mut harness = pane_harness("clip-popups", &dir);
    select_two(&mut harness);
    press_command(&mut harness, Command::Cut);
    settle(&mut harness);

    harness
        .get_by_label_contains(&format!("2 {IN_ACTION}"))
        .hover();
    settle(&mut harness);
    assert!(
        harness
            .query_all_by_label_contains("IN-ACTION ITEMS (2)")
            .count()
            > 0,
        "the queue's own list did not open"
    );
    // The other indicator's list stays shut: exactly one popup follows the pointer.
    assert_eq!(
        harness
            .query_all_by_label_contains("SELECTED ITEMS (2)")
            .count(),
        0,
        "only the indicator under the pointer opens its list"
    );

    // And the selection's indicator opens the same table under its own title, closing the other.
    harness.get_by_label_contains("items selected").hover();
    settle(&mut harness);
    assert!(
        harness
            .query_all_by_label_contains("SELECTED ITEMS (2)")
            .count()
            > 0,
        "the selection's list did not open"
    );
    assert_eq!(
        harness
            .query_all_by_label_contains("IN-ACTION ITEMS (2)")
            .count(),
        0,
        "the queue's list did not follow the pointer away"
    );
}

#[test]
fn a_list_stays_open_while_the_pointer_reads_it() {
    // The point of the hover list is to be *read*, and a reader moves the pointer into it. A popup
    // anchored to the indicator's own words closes on that move, which is the failure the full-height
    // hover band (`status_bar::hover_band`) and the previous frame's list rect are there to prevent.
    let dir = tree("clip-hover-in", &["a.txt", "b.txt", "c.txt"]);
    let mut harness = pane_harness("clip-hover-in", &dir);
    select_two(&mut harness);
    press_command(&mut harness, Command::Cut);
    settle(&mut harness);

    harness
        .get_by_label_contains(&format!("2 {IN_ACTION}"))
        .hover();
    settle(&mut harness);
    assert!(
        labels(&harness, "IN-ACTION ITEMS (2)") > 0,
        "the list did not open from the indicator's own hover"
    );

    // Up out of the bar and onto the list itself, the way a cursor travels. `.hover()` left the
    // pointer at the label's centre, which is the bar's centre; one whole status height up is then
    // 15 px ABOVE the bar — inside the list, and inside nothing the bar owns. Reading the list
    // therefore means leaving the rect that opened it.
    let bar = harness.state().tokens.status_height;
    let at = pointer(&harness);
    let into = egui::pos2(at.x, at.y - bar);
    for _ in 0..3 {
        harness.input_mut().events.push(Event::PointerMoved(into));
        harness.run();
    }
    assert!(
        labels(&harness, "IN-ACTION ITEMS (2)") > 0,
        "the list closed on the way in: the pointer went {at:?} → {into:?}, one status height up"
    );

    // Away from both, and it lets go.
    let away = egui::pos2(at.x, at.y - bar * 8.0);
    for _ in 0..3 {
        harness.input_mut().events.push(Event::PointerMoved(away));
        harness.run();
    }
    assert_eq!(
        labels(&harness, "IN-ACTION ITEMS (2)"),
        0,
        "the list outlived the pointer"
    );
}

#[test]
fn a_queued_row_actually_paints_dimmer() {
    // The state model is not the feature; the pixels are. This measures the row's own name and the
    // row's own size/date cells before and after the cut, off the rendered frame — the `menus.rs`
    // precedent for a test about paint. CSS's `opacity: .5` covered the whole row
    // (`file-list.css:41-43`), so both halves are checked: a name-only dim would fail here.
    let dir = tree("clip-dim", &["a.txt", "b.txt", "c.txt"]);
    let mut harness = pane_harness("clip-dim", &dir);
    select_two(&mut harness);
    // No raster adapter, no pixels: skip out loud rather than turn a headless machine red.
    let Ok(before) = harness.render() else {
        println!("SKIP: no raster adapter, and this test measures painted pixels");
        return;
    };
    let name_before = ink_pixels(&before, &harness, "a.txt", NAME_COLUMN, BRIGHT_NAME);
    let cells_before = ink_pixels(&before, &harness, "a.txt", CELLS_COLUMN, BRIGHT_CELL);
    let control_before = ink_pixels(&before, &harness, "c.txt", NAME_COLUMN, BRIGHT_NAME);
    assert!(
        name_before > 20,
        "a selected row paints {name_before} near-white pixels: its name is not on screen"
    );
    assert!(
        cells_before > 10,
        "the row paints {cells_before} muted pixels: its size and date cells are not on screen"
    );

    press_command(&mut harness, Command::Cut);
    settle(&mut harness);
    let after = harness
        .render()
        .expect("the frame rendered before the cut and must not stop now");
    let name_after = ink_pixels(&after, &harness, "a.txt", NAME_COLUMN, BRIGHT_NAME);
    let cells_after = ink_pixels(&after, &harness, "a.txt", CELLS_COLUMN, BRIGHT_CELL);
    let control_after = ink_pixels(&after, &harness, "c.txt", NAME_COLUMN, BRIGHT_NAME);

    assert!(
        name_after * 4 < name_before,
        "the queued row still paints {name_after} of its {name_before} bright name pixels: \
         `.cut-state` is not reaching the name cell"
    );
    assert!(
        cells_after * 4 < cells_before,
        "the queued row still paints {cells_after} of its {cells_before} muted cell pixels: \
         the size and date columns ignore the queue, which the CSS opacity did not"
    );
    assert!(
        control_after > 20 && control_after >= control_before / 2,
        "the row that was never queued went pale too ({control_before} → {control_after})"
    );
}

/// The name column of a list row (`RowColumns::LIST` gives it `3 / 5.5` of the width, and the label
/// is left-aligned in it).
const NAME_COLUMN: (f32, f32) = (0.02, 0.45);

/// The size and date columns: the right-hand `2.5 / 5.5` of the row, both right-aligned.
const CELLS_COLUMN: (f32, f32) = (0.60, 0.99);

/// The luminance that separates `text_active`/`text_main` from [`Tokens::dimmed`] of either: full ink
/// starts at 227, the dimmed mix of it stops around 140.
const BRIGHT_NAME: u32 = 180;

/// The same split for the muted cells: `text_muted` is `(149, 155, 164)`, its dimmed mix is
/// `(85, 89, 92)`.
const BRIGHT_CELL: u32 = 120;

/// How many pixels of a row's band clear `threshold` on every channel, inside the x-span
/// `[span.0, span.1]` of the row's own rect. Both file glyphs and the size/date cells are
/// `text_muted`, so a span is what says which part of the row is being weighed.
fn ink_pixels(
    image: &image::RgbaImage,
    harness: &Harness<'static, ZenApp>,
    name: &str,
    span: (f32, f32),
    threshold: u32,
) -> usize {
    let scale = harness.ctx.pixels_per_point();
    let rect = harness
        .query_all_by_label(name)
        .next()
        .unwrap_or_else(|| panic!("no row {name:?} on screen"))
        .rect();
    let x0 = (rect.min.x * scale).floor().max(0.0) as u32;
    let y0 = (rect.min.y * scale).floor().max(0.0) as u32;
    let x1 = ((rect.max.x * scale).ceil() as u32).min(image.width() - 1);
    let y1 = ((rect.max.y * scale).ceil() as u32).min(image.height() - 1);
    let from = x0 + ((x1 - x0) as f32 * span.0) as u32;
    let to = x0 + ((x1 - x0) as f32 * span.1) as u32;
    (from..=to)
        .flat_map(|x| (y0..=y1).map(move |y| (x, y)))
        .filter(|(x, y)| {
            image
                .get_pixel(*x, *y)
                .0
                .iter()
                .take(3)
                .all(|c| u32::from(*c) >= threshold)
        })
        .count()
}
