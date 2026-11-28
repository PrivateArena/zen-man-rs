//! One directory row, in list and grid form (plan §5.8 `entry_row.rs` row).
//!
//! The row is a *paint*, not a widget: [`virtual_list`](super::virtual_list) owns the rect and the
//! interaction, this module owns the pixels inside it. That is what lets the list, the grid and
//! (step 11) the search results share one row drawing.
//!
//! # The `(undefined files)` bug, and what replaced it
//!
//! Go's size cell read
//!
//! ```javascript
//! if (entry.size > 0 || (entry.files_count !== undefined && entry.files_count > 0)) {
//!     sizeStr = formatSize(entry.size) + ` (${entry.files_count} file${…})`;
//! }
//! ```
//!
//! (`frontend/js/file-list.js:159-165`). `files_count` is `omitempty` on the wire
//! (`api/dir.go:250-257`), so an un-warmed directory row had `size > 0` — the directory's own
//! `st_size`, 40 bytes on tmpfs — and `files_count === undefined`, and the cell said **`40 B
//! (undefined files)`**. The port fixes it in two places: the DTO keeps `Option<u64>` and always
//! serialises it (see the module docs of `zenman_core::entry`), and [`size_cell`] renders `--` for
//! an unknown count and never the word `undefined`.
//!
//! A directory's byte total is never its `st_size` either: that is the inode/table size, which is
//! the number Go printed. It comes from the `dir_size` walk, cached in `DirService` and pushed as
//! `EngineEvent::DirSizeReady` (decision D11) — hence [`SizeLookup`].

use egui::{Align, Color32, Rect, Ui};

use zenman_core::entry::FileEntry;
use zenman_core::metrics::FileSizes;
use zenman_core::path::AbsPath;

use crate::icons;
use crate::theme::Tokens;
use crate::widgets::disk_meter::format_size;
use crate::widgets::virtual_list::{RowColumns, RowHit};

/// How a row paints. `Cut` is the `opacity: .5` of `.cut-state` (`frontend/css/file-list.css:41-43`)
/// — a class Go shipped and never applied; here it is wired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionState {
    /// Ordinary row.
    Normal,
    /// In the selection: `rgba(88,101,242,0.15)` fill, accent hairline (`file-list.css:38-42`).
    Selected,
    /// In the clipboard as a *move*: half opacity.
    Cut,
    /// Both selected and cut.
    SelectedCut,
}

impl SelectionState {
    /// The state a selection of `selected`/`cut` produces.
    #[must_use]
    pub const fn of(selected: bool, cut: bool) -> Self {
        match (selected, cut) {
            (true, true) => Self::SelectedCut,
            (true, false) => Self::Selected,
            (false, true) => Self::Cut,
            (false, false) => Self::Normal,
        }
    }

    /// The row's fill (`--bg-hover` under the pointer, the accent tint when selected).
    #[must_use]
    pub const fn fill(self, hovered: bool) -> RowFill {
        match self {
            Self::Normal => RowFill::Hover(hovered),
            Self::Selected | Self::SelectedCut => RowFill::Selected,
            Self::Cut => RowFill::Hover(hovered),
        }
    }

    /// The row's bottom border: the CSS swaps the hairline to the accent tint when selected.
    #[must_use]
    pub const fn border(self, tokens: &Tokens) -> Color32 {
        match self {
            Self::Selected | Self::SelectedCut => tokens.selected_row_border,
            Self::Normal | Self::Cut => tokens.border,
        }
    }

    /// The label colour: `--text-active` when selected, `--text-main` otherwise, dimmed by half
    /// when cut (the `opacity: .5` the dead CSS class describes).
    #[must_use]
    pub fn text(self, tokens: &Tokens) -> Color32 {
        let base = match self {
            Self::Selected | Self::SelectedCut => tokens.text_active,
            Self::Normal | Self::Cut => tokens.text_main,
        };
        match self {
            Self::Cut | Self::SelectedCut => tokens.dimmed(base),
            _ => base,
        }
    }

    /// The row's glyph colour — the whole row dims, not just its name. CSS's `opacity` covered the
    /// icon too (`file-list.css:41-43`), so the class's own tint ([`icon_tint`]) goes through the
    /// same [`Tokens::dimmed`] mix the label does.
    #[must_use]
    pub fn icon(self, entry: &FileEntry, tokens: &Tokens) -> Color32 {
        let base = icon_tint(entry, tokens);
        match self {
            Self::Cut | Self::SelectedCut => tokens.dimmed(base),
            _ => base,
        }
    }

    /// The muted cell colour (the size, the date, a flat row's rel path): `--text-muted`, dimmed on a
    /// cut row exactly as [`Self::text`] dims the name. The list row's three right-hand cells used to
    /// read `tokens.text_muted` straight, which left a cut row's size and date at full strength.
    #[must_use]
    pub fn muted(self, tokens: &Tokens) -> Color32 {
        match self {
            Self::Cut | Self::SelectedCut => tokens.dimmed(tokens.text_muted),
            _ => tokens.text_muted,
        }
    }

    /// `true` when the row is part of the selection.
    #[must_use]
    pub const fn is_selected(self) -> bool {
        matches!(self, Self::Selected | Self::SelectedCut)
    }
}

/// Which fill a row asks for, kept as an enum so the *colour* literals stay in `theme.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowFill {
    /// Nothing, unless the pointer is over the row.
    Hover(bool),
    /// The accent selection tint.
    Selected,
}

/// The size answers the rows paint from (plan row: `SizeLookup … backed by
/// DirService::cached_sizes`; rows repaint on `EngineEvent::DirSizeReady`).
///
/// A lookup rather than a map so the pane answers from the engine's cache per row and the widget
/// crate never learns that cache's shape.
#[derive(Clone, Copy)]
pub struct SizeLookup<'a>(pub &'a dyn Fn(&AbsPath) -> Option<FileSizes>);

impl SizeLookup<'_> {
    /// The size of one path, `None` when the walk has not run — the `--` case.
    #[must_use]
    pub fn get(&self, path: &AbsPath) -> Option<FileSizes> {
        (self.0)(path)
    }

    /// A lookup that knows nothing: every directory row renders `--`.
    pub const UNKNOWN: SizeLookup<'static> = SizeLookup(&|_| None);
}

/// The placeholder a size cell shows when nobody has read a size: the `--` of [`size_cell`]'s unknown
/// cases, and the same two characters the status bar's popup prints for a directory it has not walked
/// (plan §2.4: an unreadable size is not a size of zero, and it is not nothing to say either).
pub const UNKNOWN_SIZE: &str = "--";

/// The size cell's text — see the module docs for the bug this is the fix of.
///
/// * a file: its `st_size`, formatted;
/// * a directory whose walk finished: `"<bytes> (<n> files)"`, Go's wording with a count that is
///   always a number;
/// * a directory whose walk finished but whose count is unknown: `"<bytes> (-- files)"`;
/// * a directory nobody has walked: `--` — never `0 files`, never `undefined`.
#[must_use]
pub fn size_cell(entry: &FileEntry, sizes: Option<FileSizes>) -> String {
    if !entry.is_dir {
        let bytes = u64::try_from(entry.size.max(0)).unwrap_or(u64::MAX);
        return format_size(bytes);
    }
    let warmed = sizes.map(|s| s.bytes);
    let count = sizes
        .and_then(|s| s.files)
        .or(entry.files_count)
        .map(|n| n.to_string());
    match (warmed, count) {
        (Some(bytes), Some(files)) => {
            format!("{} ({} file{})", format_size(bytes), files, plural(&files))
        }
        (Some(bytes), None) => format!("{} (-- files)", format_size(bytes)),
        (None, Some(files)) => format!("-- ({} file{})", files, plural(&files)),
        (None, None) => UNKNOWN_SIZE.to_string(),
    }
}

/// The English plural `s` after a numeric count.
fn plural(count: &str) -> &'static str {
    if count == "1" { "" } else { "s" }
}

/// The date cell: Go's `formatDate` (`frontend/js/utils.js:9-18`) — short month, day, year,
/// `HH:MM` — and `--` for the mtime Go's zero `time.Time` produced (plan F20).
#[must_use]
pub fn date_cell(entry: &FileEntry) -> String {
    let Some(mtime) = entry.mtime_secs() else {
        return "--".to_string();
    };
    if zenman_core::metrics::is_go_zero_time(mtime) {
        return "--".to_string();
    }
    format_mtime(mtime)
}

/// `YYYY MMM D, HH:MM` (plan §5.8's `format_mtime`), in UTC — the one place the port formats a
/// date, which is why the list and the search manager can never disagree (Go had one function too,
/// and no other).
#[must_use]
pub fn format_mtime(secs: i64) -> String {
    zenman_core::metrics::format_mtime(Some(secs))
}

/// The mode cell — the `fs.FileMode.String()` text, typed back through
/// [`ModeKind`](zenman_core::mode::ModeKind) so an unreadable row says `unknown` rather than the
/// empty string the index endpoint emitted (`docs/contracts.md` §2.4).
#[must_use]
pub fn mode_cell(entry: &FileEntry) -> String {
    entry.mode_kind().to_string()
}

/// The four column rects of a list row, given the row rect and the flex ratios.
#[must_use]
pub fn column_rects(rect: Rect, columns: &RowColumns, tokens: &Tokens) -> [Rect; 4] {
    let pad = tokens.row_pad_x;
    let inner = Rect::from_min_size(
        rect.min + egui::vec2(pad, 0.0),
        egui::vec2((rect.width() - pad * 2.0).max(0.0), rect.height()),
    );
    let widths = columns.widths(inner.width());
    let mut x = inner.min.x;
    let mut out = [Rect::NOTHING; 4];
    for (slot, width) in out.iter_mut().zip(widths) {
        *slot = Rect::from_min_size(
            egui::pos2(x, inner.min.y),
            egui::vec2(width.max(0.0), inner.height()),
        );
        x += width;
    }
    out
}

/// The name cell's two boxes: the row's icon square and the text box the name is painted in.
///
/// The split is the design's: the icon and the label share one flex cell with the gap between them
/// (`file-list.css:20-22`), so the label starts one icon-width and one gap from the cell's left
/// edge. [`paint_list_row`] and [`rename_rects`] both come through here — that is the whole point:
/// the in-cell editor has to put its caret on the pixel this function's glyph started on.
fn name_cells(name: Rect, tokens: &Tokens) -> (Rect, Rect) {
    let side = tokens.row_icon_size;
    let icon = Rect::from_min_size(
        egui::pos2(name.min.x, name.center().y - side / 2.0),
        egui::vec2(side, side),
    );
    let text_x = icon.max.x + tokens.row_icon_gap;
    let text = Rect::from_min_size(
        egui::pos2(text_x, name.min.y),
        egui::vec2((name.max.x - text_x).max(0.0), name.height()),
    );
    (icon, text)
}

/// The breathing room the rename frame leaves between its stroke and the glyph it wraps, in px.
///
/// The frame hugs the text, so the gap must be small enough that the box still reads as *this name,
/// being edited* and large enough that the stroke never touches a letter. It is the frame's own
/// inset, not one of the design's measurements, so there is no token for it.
const RENAME_FRAME_INSET: f32 = 2.0;

/// Where the in-cell rename editor goes in one list row ([`rename_rects`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenameCells {
    /// The rect the accent stroke is painted on. It starts to the right of the icon, so the icon of
    /// the row being renamed stays visible the whole time.
    pub frame: Rect,
    /// The rect to hand the `TextEdit` — with [`egui::Frame::NONE`] (no inset, no background over
    /// the row's selection fill) and [`egui::Align2::LEFT_CENTER`] — whose first glyph then lands on
    /// the pixel the static name's first glyph occupied.
    pub edit: Rect,
}

/// The geometry of the in-cell rename editor over one list row.
///
/// The editor *replaces* the name text, so it has to be the name text: [`RenameCells::edit`] is
/// exactly the rect [`paint_list_row`] paints the name into, and [`RenameCells::frame`] is that
/// box's line height — the row's icon square is the design's measure of one line
/// (`file-list.css:110-112`) — plus [`RENAME_FRAME_INSET`], with the icon left outside.
///
/// The vertical claim is not arithmetic on font metrics but a shared construction: egui centres a
/// `LEFT_CENTER` text edit's galley in the rect it is given (the atom block in the frame's inner
/// rect, the galley in the block — centring twice is centring once), and [`paint_text`] centres the
/// static galley in the same rect. Same font, same rect, same baseline.
pub fn rename_rects(row: Rect, columns: &RowColumns, tokens: &Tokens) -> RenameCells {
    let (_, text) = name_cells(column_rects(row, columns, tokens)[0], tokens);
    let height = tokens.row_icon_size + 2.0 * RENAME_FRAME_INSET;
    let frame = Rect::from_min_max(
        egui::pos2(
            text.min.x - RENAME_FRAME_INSET,
            text.center().y - height / 2.0,
        ),
        egui::pos2(text.max.x, text.center().y + height / 2.0),
    );
    RenameCells { frame, edit: text }
}

/// One row's content: the wire entry, its absolute path when it has one (the size key), and how
/// it should paint.
#[derive(Clone, Copy, Debug)]
pub struct RowSpec<'a> {
    /// The wire row.
    pub entry: &'a FileEntry,
    /// Its absolute path, `None` for an archive member (whose size nobody has walked).
    pub path: Option<&'a AbsPath>,
    /// Selection/cut state.
    pub sel: SelectionState,
    /// `true` while the in-cell rename editor owns the name cell. The icon and the other cells still
    /// paint; the *name* does not, because the editor draws it over this very rect
    /// ([`rename_rects`]) and a second copy behind it would read as the old name bleeding through
    /// every keystroke.
    pub renaming: bool,
}

/// Paint one list row into `hit.rect`.
///
/// `path` is the entry's absolute path when it has one (an archive member does not), and is the
/// [`SizeLookup`] key. The four cells are the CSS flex columns: icon + name, the flat-mode rel path
/// (zero width in the plain list, so it vanishes exactly as `display:none` did), size and date —
/// the last two right-aligned and muted (`file-list.css:57-62`).
pub fn paint_list_row(
    ui: &mut Ui,
    hit: &RowHit<'_>,
    row: RowSpec<'_>,
    columns: &RowColumns,
    sizes: &SizeLookup<'_>,
    tokens: &Tokens,
) {
    let RowSpec {
        entry,
        path,
        sel,
        renaming,
    } = row;
    let rect = hit.rect;
    paint_row_background(ui, rect, sel, hit.response.hovered(), tokens);
    let [name, rel, size, date] = column_rects(rect, columns, tokens);
    let (icon_rect, name_text) = name_cells(name, tokens);
    // The icon of a row that is being renamed keeps painting: the editor's frame is laid out to
    // start right of it, so there is nothing to hide (`rename_rects`).
    icons::paint(
        ui,
        icon_rect,
        icons::icon_for(entry),
        tokens.row_icon_size,
        sel.icon(entry, tokens),
    );
    if !renaming {
        cell(
            ui,
            name_text,
            &entry.name,
            sel.text(tokens),
            Align::LEFT,
            tokens,
        );
    }
    if rel.width() > 0.0 {
        cell(
            ui,
            rel,
            &entry.rel_path,
            sel.muted(tokens),
            Align::LEFT,
            tokens,
        );
    }
    cell(
        ui,
        pad_left(size, tokens.cell_padding),
        &size_cell(entry, path.and_then(|p| sizes.get(p))),
        sel.muted(tokens),
        Align::RIGHT,
        tokens,
    );
    cell(
        ui,
        pad_left(date, tokens.cell_padding),
        &date_cell(entry),
        sel.muted(tokens),
        Align::RIGHT,
        tokens,
    );
}

/// Paint one grid tile into `hit.rect` (`file-list.css:66-96`: a centred glyph over a
/// two-line-clamped label, `border-radius: 8px`, the same selection fill and border).
pub fn paint_grid_tile(
    ui: &mut Ui,
    hit: &RowHit<'_>,
    entry: &FileEntry,
    sel: SelectionState,
    tokens: &Tokens,
) {
    let rect = hit.rect;
    if ui.is_rect_visible(rect) {
        let radius = egui::CornerRadius::same(tokens.corner_radius);
        match sel.fill(hit.response.hovered()) {
            RowFill::Hover(true) => {
                ui.painter().rect_filled(rect, radius, tokens.bg_hover);
            }
            RowFill::Selected => {
                ui.painter()
                    .rect_filled(rect, radius, tokens.selected_row_bg);
                ui.painter().rect_stroke(
                    rect,
                    radius,
                    egui::Stroke::new(1.0, tokens.selected_row_border),
                    egui::StrokeKind::Inside,
                );
            }
            RowFill::Hover(false) => {}
        }
    }
    let glyph_side = (rect.width() * tokens.grid_glyph_fraction)
        .min(rect.height() * tokens.grid_glyph_height_fraction);
    let label_height = rect.height() * tokens.grid_label_height_fraction;
    let icon_rect = Rect::from_min_size(
        egui::pos2(
            rect.center().x - glyph_side / 2.0,
            rect.center().y - glyph_side / 2.0 - label_height / 2.0,
        ),
        egui::vec2(glyph_side, glyph_side),
    );
    icons::paint(
        ui,
        icon_rect,
        icons::icon_for(entry),
        glyph_side,
        sel.icon(entry, tokens),
    );
    let label_rect = egui::Rect::from_min_size(
        egui::pos2(
            rect.min.x + tokens.cell_padding,
            icon_rect.max.y + tokens.grid_label_gap,
        ),
        egui::vec2(
            (rect.width() - 2.0 * tokens.cell_padding).max(0.0),
            label_height,
        ),
    );
    paint_text(
        ui,
        label_rect,
        &entry.name,
        tokens.grid_font_size,
        sel.text(tokens),
        Align::Center,
        tokens,
    );
}

/// The group header of a grouped flat listing (`file-list.css:228-262`): the `▶`/`▼` toggle, the
/// folder glyph, the group path and its item count.
pub fn paint_group_header(
    ui: &mut Ui,
    hit: &RowHit<'_>,
    label: &str,
    count: usize,
    collapsed: bool,
    tokens: &Tokens,
) {
    let rect = hit.rect;
    if !ui.is_rect_visible(rect) {
        return;
    }
    let fill = if hit.response.hovered() {
        tokens.bg_hover
    } else {
        tokens.group_header_bg
    };
    ui.painter().rect_filled(rect, 0.0, fill);
    let centre = rect.center().y;
    let arrow = if collapsed { "▶" } else { "▼" };
    let mut x = rect.min.x + tokens.cell_padding;
    ui.painter().text(
        egui::pos2(x + tokens.group_arrow_width / 2.0, centre),
        egui::Align2::CENTER_CENTER,
        arrow,
        tokens.font(),
        tokens.accent,
    );
    x += tokens.group_arrow_width;
    let icon_rect = Rect::from_min_size(
        egui::pos2(x, centre - tokens.row_icon_size / 2.0),
        egui::vec2(tokens.row_icon_size, tokens.row_icon_size),
    );
    icons::paint(
        ui,
        icon_rect,
        "folder",
        tokens.row_icon_size,
        tokens.warning,
    );
    x += tokens.row_icon_size + tokens.row_icon_gap;
    ui.painter().text(
        egui::pos2(x, centre),
        egui::Align2::LEFT_CENTER,
        label,
        tokens.font(),
        tokens.text_main,
    );
    ui.painter().text(
        egui::pos2(rect.max.x - tokens.cell_padding, centre),
        egui::Align2::RIGHT_CENTER,
        format!("{count} item{}", if count == 1 { "" } else { "s" }),
        tokens.font(),
        tokens.text_muted,
    );
}

/// The row background: the hover/selection fill and the bottom hairline.
fn paint_row_background(
    ui: &mut Ui,
    rect: Rect,
    sel: SelectionState,
    hovered: bool,
    tokens: &Tokens,
) {
    if !ui.is_rect_visible(rect) {
        return;
    }
    match sel.fill(hovered) {
        RowFill::Hover(true) => {
            ui.painter().rect_filled(rect, 0.0, tokens.bg_hover);
        }
        RowFill::Selected => {
            ui.painter().rect_filled(rect, 0.0, tokens.selected_row_bg);
        }
        RowFill::Hover(false) => {}
    }
    ui.painter().hline(
        rect.x_range(),
        rect.max.y - 1.0,
        egui::Stroke::new(1.0, sel.border(tokens)),
    );
}

/// The glyph colour: `#f0b232` for a folder, `--text-muted` for a file (`file-list.css:110-112`,
/// `.icon-folder`/`.icon-file`), the accent for a link (Go used a third emoji, `🔗`).
#[must_use]
pub fn icon_tint(entry: &FileEntry, tokens: &Tokens) -> Color32 {
    if entry.is_symlink {
        tokens.accent
    } else if entry.is_dir {
        tokens.warning
    } else {
        tokens.text_muted
    }
}

/// A single-line, ellipsised cell of text — **paint-only** (plan §9.2 step 8: allocating a
/// `Label` rect over a row's `ui.interact` rect steals its hover/click in this egui fork, the
/// last-allocated overlapping rect wins). Elision goes through [`egui::TextWrapping`] with
/// `max_rows: 1` and `overflow_character: Some('…')`, which is what `Label::truncate` did internally.
fn cell(ui: &mut Ui, rect: Rect, text: &str, colour: Color32, align: Align, tokens: &Tokens) {
    paint_text(ui, rect, text, tokens.font_size, colour, align, tokens);
}

/// The paint-only text primitive behind [`cell`] and the grid tile label.
fn paint_text(
    ui: &mut Ui,
    rect: Rect,
    text: &str,
    font_size: f32,
    colour: Color32,
    align: Align,
    _tokens: &Tokens,
) {
    if rect.width() <= 0.0 || text.is_empty() {
        return;
    }
    let mut job = egui::text::LayoutJob::simple(
        text.to_owned(),
        egui::FontId::proportional(font_size),
        colour,
        rect.width(),
    );
    job.wrap = egui::text::TextWrapping {
        max_width: rect.width(),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    let galley = ui.painter().layout_job(job);
    let width = galley.size().x;
    let x = match align {
        Align::RIGHT => rect.max.x - width,
        Align::Center => rect.center().x - width / 2.0,
        _ => rect.min.x,
    };
    let y = rect.center().y - galley.size().y / 2.0;
    ui.painter().galley(egui::pos2(x, y), galley, colour);
}

/// Leave `pad` points at the right edge of a right-aligned cell.
fn pad_left(rect: Rect, pad: f32) -> Rect {
    Rect::from_min_size(
        rect.min,
        egui::vec2((rect.width() - pad).max(0.0), rect.height()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use zenman_core::metrics::GO_ZERO_TIME_UNIX;

    /// A wire row. `mod_time` is a `chrono` type, so it is built by core's own constructor rather
    /// than by naming the type here (the UI crate does not depend on chrono).
    fn entry(name: &str, is_dir: bool, size: i64, files_count: Option<u64>) -> FileEntry {
        FileEntry {
            name: name.into(),
            is_dir,
            is_symlink: false,
            size,
            mod_time: zenman_core::entry::unix_to_datetime(zenman_core::metrics::GO_ZERO_TIME_UNIX),
            mode: if is_dir { "drwxr-xr-x" } else { "-rw-r--r--" }.into(),
            rel_path: String::new(),
            depth: 0,
            files_count,
        }
    }

    /// The same row with a real mtime.
    fn with_mtime(mut e: FileEntry, secs: i64) -> FileEntry {
        e.mod_time = zenman_core::entry::unix_to_datetime(secs);
        e
    }

    #[test]
    fn an_unwarmed_directory_is_dashes_and_never_undefined() {
        // The Go bug (`file-list.js:159-165`): a 40-byte tmpfs directory with no `files_count`
        // rendered `40 B (undefined files)`.
        let dir = entry("many", true, 40, None);
        let cell = size_cell(&dir, None);
        assert_eq!(cell, "--", "{cell}");
        assert!(!cell.contains("undefined"), "{cell}");
        assert!(
            !cell.contains("0 file"),
            "{cell} must not read as a real count of zero"
        );
    }

    #[test]
    fn a_warmed_directory_shows_bytes_and_count() {
        let dir = entry("many", true, 40, Some(1200));
        assert_eq!(size_cell(&dir, None), "-- (1200 files)");
        let warmed = FileSizes {
            bytes: 4096,
            files: Some(3),
        };
        assert_eq!(size_cell(&dir, Some(warmed)), "4.0 KiB (3 files)");
        let one = FileSizes {
            bytes: 10,
            files: Some(1),
        };
        assert_eq!(size_cell(&dir, Some(one)), "10 B (1 file)");
    }

    #[test]
    fn a_size_without_a_count_is_dashes_for_the_count_not_zero() {
        // Decision D11: `FileSizes{bytes:4096, files:None}` is "bytes known, count unknown", and the
        // plan's Done-when is that it renders `--` and never `undefined`.
        let dir = entry("d", true, 0, None);
        let sizes = FileSizes {
            bytes: 4096,
            files: None,
        };
        let cell = size_cell(&dir, Some(sizes));
        assert_eq!(cell, "4.0 KiB (-- files)");
        assert!(!cell.contains("undefined"), "{cell}");
    }

    #[test]
    fn a_file_shows_its_own_size_even_when_zero() {
        // Size 0 is a *fact* for a file and must look different from the unknown of a directory.
        assert_eq!(size_cell(&entry("empty", false, 0, None), None), "0 B");
        assert_eq!(size_cell(&entry("a", false, 1536, None), None), "1.5 KiB");
        // The degraded row (`dir-perm-noexec.json`) has `size: 0` and no count.
        let degraded = entry("noexec.bin", false, 0, None);
        assert_eq!(size_cell(&degraded, None), "0 B");
    }

    #[test]
    fn the_date_cell_covers_go_zero_time_and_the_missing_mtime() {
        let known = with_mtime(entry("a", false, 1, None), 1_767_225_600);
        assert_eq!(date_cell(&known), "2026 Jan 1, 00:00");
        let zeroed = with_mtime(entry("b", false, 1, None), GO_ZERO_TIME_UNIX);
        assert_eq!(date_cell(&zeroed), "--", "plan F20's degraded row");
        assert_eq!(date_cell(&entry("c", false, 1, None)), "--");
        assert_eq!(format_mtime(1_767_225_600), "2026 Jan 1, 00:00");
    }

    #[test]
    fn the_mode_cell_round_trips_the_golden_inventory() {
        // `docs/contracts.md` §fileMode strings — every distinct value the captures contain.
        for (mode, expect) in [
            ("-rw-r--r--", "-rw-r--r--"),
            ("-rwxr-xr-x", "-rwxr-xr-x"),
            ("----------", "----------"),
            ("drwxr-xr-x", "drwxr-xr-x"),
            ("dr--r--r--", "dr--r--r--"),
            ("d---------", "d---------"),
            ("Lrwxrwxrwx", "Lrwxrwxrwx"),
            ("prw-r--r--", "prw-r--r--"),
            ("-r--r--r--", "-r--r--r--"),
            ("unknown", "unknown"),
        ] {
            let mut e = entry("x", mode.starts_with('d'), 0, None);
            e.mode = mode.into();
            assert_eq!(mode_cell(&e), expect, "{mode} did not survive ModeKind");
        }
    }

    #[test]
    fn selection_states_map_to_the_css_colours() {
        let tokens = Tokens::dark();
        assert_eq!(
            SelectionState::Selected.border(&tokens),
            tokens.selected_row_border
        );
        assert_eq!(SelectionState::Normal.border(&tokens), tokens.border);
        assert_eq!(SelectionState::Selected.text(&tokens), tokens.text_active);
        assert_eq!(SelectionState::Normal.text(&tokens), tokens.text_main);
        // The cut row's `opacity: .5` is a real colour now, and it is *between* the two.
        let cut = SelectionState::Cut.text(&tokens);
        assert_ne!(cut, tokens.text_main, "a cut row must look dimmer");
        assert!(!SelectionState::Cut.is_selected());
        assert!(SelectionState::SelectedCut.is_selected());
        assert_eq!(SelectionState::of(true, false), SelectionState::Selected);
        assert_eq!(SelectionState::of(true, true), SelectionState::SelectedCut);
        assert_eq!(SelectionState::of(false, false), SelectionState::Normal);
    }

    #[test]
    fn a_cut_row_dims_every_colour_the_row_paints() {
        // The CSS `opacity: .5` covered the glyph and the muted cells as much as the name
        // (`file-list.css:41-43`), so no cell may read a token straight while the row is queued.
        let tokens = Tokens::dark();
        let file = entry("a", false, 1, None);
        let dir = entry("docs", true, 0, None);
        assert_eq!(SelectionState::Normal.icon(&dir, &tokens), tokens.warning);
        assert_eq!(SelectionState::Normal.muted(&tokens), tokens.text_muted);
        assert_ne!(
            SelectionState::Cut.icon(&dir, &tokens),
            tokens.warning,
            "a cut row's folder glyph dims"
        );
        assert_ne!(
            SelectionState::SelectedCut.icon(&dir, &tokens),
            tokens.warning,
            "and so does a cut row that is selected as well"
        );
        assert_eq!(
            SelectionState::Cut.icon(&file, &tokens),
            tokens.dimmed(tokens.text_muted),
            "a file's glyph is the muted tint, dimmed"
        );
        assert_eq!(
            SelectionState::Cut.muted(&tokens),
            tokens.dimmed(tokens.text_muted)
        );
        assert_ne!(
            SelectionState::Selected.muted(&tokens),
            tokens.dimmed(tokens.text_muted),
            "a plain selection dims nothing"
        );
    }

    #[test]
    fn column_rects_tile_the_row_and_drop_a_zero_flex_column() {
        let tokens = Tokens::dark();
        let row = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(620.0, 40.0));
        let [name, rel, size, date] = column_rects(row, &RowColumns::LIST, &tokens);
        assert_eq!(rel.width(), 0.0, "no rel column in the plain list");
        assert_eq!(name.min.x, tokens.row_pad_x, "the CSS row padding");
        assert!(
            (date.max.x - (620.0 - tokens.row_pad_x)).abs() < 0.01,
            "{date:?}"
        );
        assert!(name.max.x <= rel.min.x.max(name.max.x));
        let _ = size;
        let flat = column_rects(row, &RowColumns::FLAT, &tokens);
        assert!(
            flat[1].width() > 0.0,
            "the flat listing shows the rel column"
        );
    }

    #[test]
    fn the_rename_frame_hugs_the_name_and_leaves_the_icon_alone() {
        let tokens = Tokens::dark();
        let row = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(620.0, tokens.row_height));
        // Both flex tables, because the editor opens in the flat listing too.
        for columns in [RowColumns::LIST, RowColumns::FLAT] {
            let [name, ..] = column_rects(row, &columns, &tokens);
            let icon_right = name.min.x + tokens.row_icon_size;
            let cells = rename_rects(row, &columns, &tokens);
            // The glyph box is the text box `name_cells` hands the static name, so the first letter
            // cannot jump when F2 opens the editor.
            assert_eq!(
                cells.edit.min.x,
                icon_right + tokens.row_icon_gap,
                "the editor's text starts one icon and one gap into the name cell"
            );
            assert_eq!(cells.edit.max.x, name.max.x, "and owns the rest of it");
            assert_eq!(cells.edit.min.y, name.min.y, "the full row height");
            assert_eq!(cells.edit.max.y, name.max.y);
            // The frame hugs the glyph box, and its stroke stops short of the first letter by the
            // inset — never further left than the icon's right edge, which is the bug this rect is.
            assert_eq!(cells.frame.min.x, cells.edit.min.x - RENAME_FRAME_INSET);
            assert!(
                cells.frame.min.x >= icon_right,
                "the frame must not cover the icon: {:?} vs the icon ending at {icon_right}",
                cells.frame
            );
            assert_eq!(
                cells.frame.max.x, name.max.x,
                "the frame ends at the name cell's right edge"
            );
            assert!(
                cells.frame.height() <= row.height(),
                "a frame taller than the row would paint over its neighbours: {:?}",
                cells.frame
            );
        }
    }

    /// The row's name painted the way [`paint_list_row`] paints it, and the same string in the
    /// editor laid out the way [`crate::panels::file_pane`] adds it, in one frame — and then the two
    /// text shapes must agree on every pixel, because a transition into rename mode that moves the
    /// glyphs by a point is a transition the user sees.
    ///
    /// This is the test that makes the geometry claim in [`rename_rects`] a fact rather than a
    /// reading of egui's layout code: the font's row height, the frame's inset and the atom layout's
    /// centring are all egui's, and here they are measured rather than assumed.
    #[test]
    fn entering_rename_mode_does_not_move_a_single_glyph() {
        let tokens = Tokens::dark();
        let name = "report.pdf";
        let row = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(620.0, tokens.row_height));
        let cells = rename_rects(row, &RowColumns::LIST, &tokens);
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, zenman_core::store::config::ThemeMode::Dark);
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let out = ctx.run_ui(input, |ui| {
            cell(ui, cells.edit, name, tokens.text_main, Align::LEFT, &tokens);
            let mut buf = name.to_owned();
            ui.put(
                cells.edit,
                egui::TextEdit::singleline(&mut buf)
                    .font(tokens.font())
                    .align(egui::Align2::LEFT_CENTER)
                    .frame(egui::Frame::NONE)
                    .desired_width(cells.edit.width()),
            );
        });
        let painted: Vec<(egui::Pos2, Rect)> = out
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::Shape::Text(text) if text.galley.text() == name => {
                    Some((text.pos, text.galley.rect))
                }
                _ => None,
            })
            .collect();
        out.drop_without_applying_deltas();
        assert_eq!(painted.len(), 2, "the row's name and the editor's text");
        // Same font, same rect, one line: the two galleys are one box, so agreeing on the origin
        // means agreeing on the baseline.
        assert_eq!(
            painted[0].1, painted[1].1,
            "the editor laid the same text out at a different height"
        );
        assert_eq!(
            painted[0].0, painted[1].0,
            "the glyphs jumped on entering rename mode"
        );
        // And where that is, spelled out: the name cell's text x, one icon and one gap in.
        assert_eq!(
            painted[0].0.x,
            tokens.row_pad_x + tokens.row_icon_size + tokens.row_icon_gap,
            "the first glyph must sit where the row painted it"
        );
    }
}
