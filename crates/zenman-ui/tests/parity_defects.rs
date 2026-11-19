//! The 15c parity-defect pins: the geometry and wording contracts the snapshot review found broken,
//! asserted as *facts about the code* next to the frame-level assertions in `tests/snapshots.rs`.
//!
//! What a harness can see and what it cannot, stated honestly:
//!
//! * a kittest harness sees the **a11y tree** — allocated widgets (`ui.label`, buttons, the row nodes
//!   a custom row gives itself with `Response::widget_info`). It does **not** see `painter.text`, so
//!   the quick-find/search-manager *row cells* cannot be measured from the harness at all;
//! * therefore the "the two texts do not overlap" family of defects is pinned where it is decided —
//!   the pure column functions both the header and the painted rows now go through — and the frame
//!   itself stays a human gate (`cargo xtask snapshots` + `docs/parity/*`);
//! * opacity is likewise not observable as a node, so D1 is pinned as the *contract* the card is
//!   built from (`Tokens::modal_fill` is opaque, `Tokens::modal_backdrop` is not) plus the rendered
//!   baselines;
//! * `tests/snapshots.rs` owns the harness-side half of D2/D3/D4/D6 (labels present in the painted
//!   frame, rows realised in the list), because it already owns the settled app.

use egui::{Rect, vec2};

use zenman_core::ops::batch_rename::{Row, RowStatus, Rules};
use zenman_core::path::AbsPath;
use zenman_core::store::config::PaneSlot;
use zenman_ui::dialogs::batch_rename::{
    BatchRename, extension_dropped, preview_cells, row_status_text,
};
use zenman_ui::dialogs::search_manager::{SearchManager, result_cells};
use zenman_ui::panels::file_pane::{band, band_at};
use zenman_ui::theme::Tokens;
use zenman_ui::widgets::toast::{Level, State};

/// The stamp `tests/snapshots.rs` freezes `last_indexed` to — the *frozen* value, never a live clock.
const FROZEN_STAMP: i64 = 1_700_000_000;

// ------------------------------------------------------------------ D1: the card is opaque

#[test]
fn the_modal_card_fill_is_opaque_and_the_backdrop_is_not() {
    for tokens in [Tokens::dark(), Tokens::light()] {
        let fill = tokens.modal_fill();
        assert_eq!(
            fill.a(),
            255,
            "a dialog card that lets the shell through is 15c D1"
        );
        // And it is the *card* colour, not an invention: the token table still owns the hue.
        assert_eq!(
            (fill.r(), fill.g(), fill.b()),
            (tokens.bg_card.r(), tokens.bg_card.g(), tokens.bg_card.b()),
            "the fill is `bg_card`, alpha stripped"
        );
        let backdrop = tokens.modal_backdrop();
        assert!(
            backdrop.a() < 255 && backdrop.a() > 0,
            "the shell dim is translucent by design (Go dimmed the page behind its overlay)"
        );
    }
}

// ------------------------------------------------------------------ D2/D3: the table and the stamp

#[test]
fn the_result_columns_are_four_disjoint_cells_inside_the_row() {
    let row = Rect::from_min_size(egui::pos2(10.0, 0.0), vec2(900.0, 40.0));
    let cells = result_cells(row);
    assert_eq!(cells.len(), 4, "Name / Path / Size / Modified");
    let mut right = row.left();
    for cell in &cells {
        assert!(cell.width() > 0.0, "every column has width: {cells:?}");
        assert!(
            cell.left() >= right - f32::EPSILON,
            "columns are laid out in order and never overlap: {cells:?}"
        );
        right = cell.right();
    }
    assert!(
        right <= row.right() + 1.0,
        "the table fits its row: {cells:?} in {row:?}"
    );
}

#[test]
fn the_last_indexed_stamp_is_a_date_and_never_a_raw_unix_second() {
    assert_eq!(
        zenman_ui::dialogs::search_manager::last_indexed_text(FROZEN_STAMP),
        "2023 Nov 14, 22:13",
        "core formats the date (`metrics::format_mtime`); the UI owns no calendar"
    );
    assert_eq!(
        zenman_ui::dialogs::search_manager::last_indexed_text(0),
        "never",
        "Go's `0` is 'never indexed'"
    );
}

// ------------------------------------------------------------------ D4: the running frame

#[test]
fn an_active_rebuild_opens_the_settings_view_and_words_its_progress() {
    let mut manager = SearchManager::new(
        PaneSlot::Left,
        zenman_core::session::IndexConfig::default(),
        zenman_core::services::search_index::IndexStatus {
            is_indexing: false,
            total_files: 462,
            last_indexed: FROZEN_STAMP,
            is_dirty: false,
            config: zenman_core::services::search_index::IndexStatusConfig {
                roots: Vec::new(),
                excludes: Vec::new(),
                auto_index: false,
                worker_count: 4,
                follow_symlinks: false,
            },
            progress: zenman_core::services::search_index::IndexStatusProgress::default(),
            error: None,
        },
    );
    assert!(
        !manager.shows_settings(),
        "a terminal frame keeps the plain view"
    );
    manager.status.is_indexing = true;
    manager.status.progress.indexed = 200;
    assert!(
        manager.shows_settings(),
        "15c D4: Go's mid-rebuild frame *is* the settings view"
    );
    assert_eq!(
        manager.progress_text(),
        "indexing 200",
        "no stage, no invented stage"
    );
    manager.progress_stage = Some("/tmp/tree/docs".to_string());
    assert_eq!(
        manager.progress_text(),
        "indexing 200 — /tmp/tree/docs",
        "the stage is `JobProgress::current`, read from the job"
    );
}

// ------------------------------------------------------------------ D5: no stale terminal toast

#[test]
fn a_terminal_index_notice_can_be_withdrawn_when_a_new_rebuild_starts() {
    let mut notices = State::default();
    notices.post(Level::Success, "462 row(s) indexed");
    notices.post(Level::Info, "3 items selected");
    assert_eq!(
        notices.dismiss_containing("row(s) indexed"),
        1,
        "the finished rebuild's toast goes"
    );
    let texts: Vec<&str> = notices.queued().iter().map(|n| n.text.as_str()).collect();
    assert_eq!(texts, vec!["3 items selected"], "unrelated notices stay");
    assert_eq!(
        notices.dismiss_containing("row(s) indexed"),
        0,
        "and the call is idempotent"
    );
}

// ------------------------------------------------------------------ D6: quick-find row columns

#[test]
fn a_quick_find_row_gives_name_path_and_size_cells_that_do_not_overlap() {
    let tokens = Tokens::dark();
    let row = Rect::from_min_size(egui::pos2(0.0, 0.0), vec2(1200.0, tokens.row_height));
    let [name, path, size] = zenman_ui::dialogs::quick_find::row_cells(row, &tokens);
    assert!(
        name.right() <= path.left() + f32::EPSILON,
        "D6: {name:?} vs {path:?}"
    );
    assert!(
        path.right() <= size.left() + f32::EPSILON,
        "D6: {path:?} vs {size:?}"
    );
    assert!(
        (size.right() - row.right()).abs() <= tokens.cell_padding + 0.5,
        "the size column hugs the row's right edge (inside its padding): {size:?}"
    );
    for cell in [name, path, size] {
        assert!(cell.width() > 0.0, "every column is drawn: {cell:?}");
    }
}

// ------------------------------------------------------------------ D7: the band discipline

#[test]
fn a_bands_two_halves_are_disjoint_and_the_right_one_hugs_the_pane_edge() {
    // The quad layout's narrow pane (the frame the review found clipped).
    let whole = Rect::from_min_size(egui::pos2(20.0, 100.0), vec2(720.0, 50.0));
    for (left, right) in [band(whole), band_at(whole, 0.6)] {
        assert_eq!(
            left.min, whole.min,
            "the left group starts at the pane's left edge"
        );
        assert_eq!(
            right.max, whole.max,
            "the right group ends at its **right** edge"
        );
        assert!(
            left.intersect(right).area() <= 0.0,
            "the halves share at most an edge, so they cannot eat each other's clicks (rule 10):
             {left:?} {right:?}"
        );
    }
    let (message, buttons) = band_at(whole, 0.6);
    assert!(
        buttons.width() > message.width() * 0.4,
        "the buttons get a real cell in a narrow pane: {buttons:?}"
    );
}

// ------------------------------------------------------------------ D12: the preview is a table

/// The exact rows and rules `tests/snapshots.rs` renders as `13-batch-rename.png`: find `t`, replace
/// `T`, **Rename Extension Too** ticked — i.e. the frame whose `a.txt→a` line looked truncated.
fn scene() -> BatchRename {
    let listing = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("parity-defects");
    let rows: Vec<Row> = ["a.txt", "b.txt", "c.md"]
        .iter()
        .map(|name| {
            Row::new(
                AbsPath::try_from(listing.join(name).as_path())
                    .unwrap_or_else(|e| panic!("{name}: {e}")),
                *name,
                false,
            )
        })
        .collect();
    let rules = Rules {
        find: "t".to_string(),
        replace: "T".to_string(),
        include_extension: true,
        ..Rules::default()
    };
    BatchRename::new(
        PaneSlot::Left,
        rows,
        rules,
        std::collections::HashSet::new(),
    )
}

#[test]
fn the_extension_drop_is_the_quirk_and_the_table_says_so() {
    assert!(
        extension_dropped("a.txt", "a"),
        "Go's `Rename Extension Too` previews the stem alone (`batch-rename.js:281`)"
    );
    assert!(
        !extension_dropped("a.txt", "a.txt"),
        "nothing dropped, nothing said"
    );
    assert_eq!(
        row_status_text(RowStatus::Success, true),
        "RENAMED (extension dropped)"
    );
    assert_eq!(row_status_text(RowStatus::Unchanged, false), "UNCHANGED");

    let table = scene().table();
    let cells: Vec<(&str, &str, &str)> = table
        .iter()
        .map(|cell| {
            (
                cell.original.as_str(),
                cell.preview.as_str(),
                cell.status.as_str(),
            )
        })
        .collect();
    assert_eq!(
        cells,
        vec![
            ("a.txt", "a", "RENAMED (extension dropped)"),
            ("b.txt", "b", "RENAMED (extension dropped)"),
            ("c.md", "c", "RENAMED (extension dropped)"),
        ],
        "15c D12: `a.txt -> a` is the kept Go quirk, not a truncated paint — so the Status column says so"
    );
}

#[test]
fn the_preview_table_has_three_disjoint_columns_per_row() {
    let row = Rect::from_min_size(egui::pos2(0.0, 0.0), vec2(800.0, 24.0));
    let cells = preview_cells(row);
    assert_eq!(
        zenman_ui::dialogs::batch_rename::COLUMNS.len(),
        cells.len(),
        "one header per column"
    );
    let mut right = row.left();
    for cell in &cells {
        assert!(cell.left() >= right - f32::EPSILON, "{cells:?}");
        assert!(cell.width() > 0.0, "{cells:?}");
        right = cell.right();
    }
    assert!(right <= row.right() + 1.0, "{cells:?}");
}
