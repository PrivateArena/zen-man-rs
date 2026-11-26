//! The 30 px status bar (plan §5.8 `status_bar.rs` row).
//!
//! Go's said `N items selected` plus a hover tooltip of the first ten names in *insertion* order
//! (`file-list.js:569`, `Array.from(set).slice(0, 10)`); here the clamp is in **display order**,
//! which is the plan's one-line fix, and the plural is a real test rather than a hope.
//!
//! The bar also carries the shell's one write report — the `Saved` pill (Go's `_showSaveIndicator`),
//! which used to sit next to the workspace button in the tab row. A tab is the row's scarcest
//! pixel: the user reads the tab names twice a second and the write report once, so it belongs
//! on the line that already reports the state of the frame, not on the one that carries the tabs.
//!
//! And it carries Go's second indicator: the amber underlined `✂ N items in action` of the queued
//! cut/copy — the scissors for a move, the clipboard for a copy, printed next to the selection count
//! in Go's status line (the reference shots this port was checked against). The list it hovers open
//! is the item shape `api/ops.go:166-176` returns: glyph, name, size ([`items_popup`]).
//! It describes the *session* clipboard, not the pane, so it stays up when the queued rows are not
//! even on screen — which is the point after a Copy/Cut Inside.
//!
//! The widget is fed plain data ([`StatusModel`], [`StatusItem`], [`ClipStatus`]) and never asks the
//! engine a question: only [`crate::app::ZenApp`] reads `Session::clipboard`.

use egui::{Align, Color32, Layout, Pos2, Rect, Response, Sense, Stroke, Ui, Vec2, pos2, vec2};

use crate::icons;
use crate::jobs_ui::JobSlot;
use crate::panels::header;
use crate::theme::Tokens;
use crate::widgets::entry_row::UNKNOWN_SIZE;

/// How many selected names the tooltip lists before it says "and N more" (`file-list.js:578`).
pub const DISPLAY_CAP: usize = 10;

/// The write report's one word (Go's `#save-indicator` text): the pill's room is counted from it, so
/// the width and the drawing must name the same string.
pub const SAVED_LABEL: &str = "Saved";

/// Percent per unit fraction: the job line reports a whole-number percentage of an
/// `f32` progress fraction, and the geometry guard wants that number named.
const PERCENT_PER_FRACTION: f32 = 100.0;

/// The share of the bar the selection cell (and the action chip beside it) may use.
const LEFT_FRACTION: f32 = 0.45;

/// The share of the bar where the boot note starts, and where the job/pane cell starts.
const NOTE_FRACTION: f32 = 0.46;
const RIGHT_FRACTION: f32 = 0.74;

/// The dashed underline's dash and gap in points — the indicator is *underlined* like Go's, and
/// `Shape::dashed_line` is what this egui rev gives a dashed stroke
/// (`epaint/src/shapes/shape.rs:177-193`).
const INDICATOR_DASH: f32 = 3.0;
const INDICATOR_GAP: f32 = 2.0;

/// What the bar's left cell is saying.
#[derive(Clone, Debug, PartialEq)]
pub enum StatusInfo {
    /// `N items` — the listing's own count.
    Count(usize),
    /// The listing failed.
    Error(String),
    /// A job is moving bytes **and has said how many**: the two numbers come from a
    /// `JobProgress`, never from a placeholder. Step 9's paste/copy is what fills this.
    Copying {
        /// Bytes done.
        done: u64,
        /// Bytes total, `None` while unknown.
        total: Option<u64>,
    },
    /// A job is running and its progress is a *fraction* (a listing read, a size walk): the
    /// shell has a label and a 0..=1 fraction, not byte counts, and says exactly that rather than
    /// inventing `0 of 0`.
    Running {
        /// The job's own label, already worded.
        label: String,
        /// Last reported fraction; `None` while the job is a spinner.
        frac: Option<f32>,
    },
    /// The last thing that finished.
    Done(String),
}

/// One row of the bar's hover popups: the name, the class the glyph shows, and the size the row
/// shows beside the name. Go's clipboard item (`api/ops.go:166-176`) carried exactly this triple, and
/// the selection popup reads the same triple off the listing rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusItem {
    /// The name, as the listing shows it (Go's `display`).
    pub name: String,
    /// `true` for a directory: the popup's glyph is the folder's, as in the listing.
    pub is_dir: bool,
    /// The byte count, `None` when nobody has read one — a directory's content size is the walk's
    /// answer and the popup has no walk to ask, which is the same fact [`size_cell`] paints as
    /// [`UNKNOWN_SIZE`].
    ///
    /// [`size_cell`]: crate::widgets::entry_row::size_cell
    pub size: Option<u64>,
}

/// The session clipboard, as the bar reports it. Plain data: the widget never asks the engine what is
/// queued — [`crate::app::ZenApp`] answers that once per frame and hands the answer in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipStatus {
    /// The verb, Go's `ClipOp::as_str` spelling (`"cut"` / `"copy"`). Never printed on the bar —
    /// the glyph says it — but it is the popup's accessible name, so a screen reader hears which one
    /// it is looking at.
    pub op: &'static str,
    /// The indicator's glyph, by registry name (`icons::KNOWN_ICONS`: `"cut"` / `"copy"`).
    pub icon: &'static str,
    /// How many items are queued — the number the chip prints and the list's title answers with,
    /// which is *not* `items.len()` once the queue is longer than [`DISPLAY_CAP`].
    pub total: usize,
    /// The head of the queue, in paste order (`Clipboard::items`), capped at [`DISPLAY_CAP`].
    pub items: Vec<StatusItem>,
}

/// The bar's input (plan row's `StatusModel`).
pub struct StatusModel<'a> {
    /// How many rows are selected: **all** of them, not the [`Self::items`] the bar was handed.
    /// The count is the promise and the list is its head, so the two arrive separately rather than
    /// the bar counting what it happens to have been given.
    pub selected: usize,
    /// The selected rows, **in display order** — the pane's visual order, not the set's — capped at
    /// [`DISPLAY_CAP`]: the count says what the whole list is, so a 450-row `Ctrl+A` costs ten name
    /// clones a frame rather than 450 of them to print ten lines.
    pub items: Vec<StatusItem>,
    /// The session clipboard (Go's second indicator): `None` when nothing is queued.
    pub clip: Option<ClipStatus>,
    /// The listing/job state.
    pub info: StatusInfo,
    /// The live jobs, for the right-hand cell.
    pub jobs: &'a [JobSlot],
    /// How many panes the grid shows.
    pub panes: usize,
    /// The shell's boot note (GPU profile, session report) — the one line the deleted header used
    /// to print. It shares the bar with the counts rather than costing a row of its own.
    pub note: Option<String>,
    /// The autosave's `Saved` report (see [`SAVED_LABEL`]): `true` for the 2 s after a write lands
    /// (`SAVED_INDICATOR_TTL`), answered by [`header::WorkspaceBar::chip_shows`].
    pub saved: bool,
}

/// Draw the bar.
pub fn show(ui: &mut Ui, m: &StatusModel<'_>, tokens: &Tokens) {
    let rect = ui.available_rect_before_wrap();
    let bar = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), tokens.status_height));
    if ui.is_rect_visible(bar) {
        ui.painter().rect_filled(bar, 0.0, tokens.bg_sidebar);
        ui.painter()
            .hline(bar.x_range(), bar.min.y, tokens.hairline());
    }
    let count = m.selected;
    let left = if count == 0 {
        "0 items selected".to_string()
    } else {
        format!("{} item{} selected", count, plural(count))
    };
    let text = match &m.info {
        StatusInfo::Count(rows) => format!("{left} · {rows} item{} in the listing", plural(*rows)),
        StatusInfo::Error(text) => format!("{left} · {text}"),
        StatusInfo::Copying { done, total } => match total {
            Some(total) if *total > 0 => format!(
                "{left} · copying {} of {}",
                crate::widgets::disk_meter::format_size(*done),
                crate::widgets::disk_meter::format_size(*total)
            ),
            _ => format!("{left} · copying"),
        },
        StatusInfo::Running { label, frac } => match frac {
            Some(frac) => format!(
                "{left} · {label} {}%",
                (frac.clamp(0.0, 1.0) * PERCENT_PER_FRACTION).round()
            ),
            None => format!("{left} · {label}"),
        },
        StatusInfo::Done(text) => format!("{left} · {text}"),
    };
    // Three regions, counted as fractions of the bar: the selection and listing counts (with the
    // action chip at their end), the boot note, and the job/pane cell. The note is the long one, so it
    // gets its own third rather than whatever the two labels happened to leave.
    let left_width = bar.width() * LEFT_FRACTION;
    let note_min = bar.min.x + bar.width() * NOTE_FRACTION;
    let note_max = bar.min.x + bar.width() * RIGHT_FRACTION;
    // The chip's room is counted before anything is drawn, exactly like the `Saved` pill's: a cut that
    // lands must not shove the selection count sideways, and a chip that does not fit is not drawn at
    // all rather than one that overlaps its neighbour.
    let chip = m
        .clip
        .as_ref()
        .filter(|clip| !clip.items.is_empty())
        .map(|clip| (clip, action_label(clip.total)));
    let label_font = egui::FontId::proportional(tokens.font_size - 1.0);
    let chip_galley = chip.as_ref().map(|(_, label)| {
        ui.painter()
            .layout_no_wrap(label.clone(), label_font, tokens.warning)
    });
    let chip_room = chip_galley
        .as_ref()
        .map_or(0.0, |galley| action_chip_width(tokens, galley.size().x));
    let chip_shows = chip.is_some() && chip_room + tokens.status_note_min_width <= left_width;
    let label_room = if chip_shows {
        (left_width - chip_room).max(0.0)
    } else {
        left_width
    };
    let _label = ui.put(
        egui::Rect::from_min_size(
            egui::pos2(
                bar.min.x + tokens.cell_padding,
                bar.center().y - tokens.font_size,
            ),
            egui::vec2(label_room, tokens.font_size * 2.0),
        ),
        // The hover lives on [`hover_band`]'s full-height rect, allocated after this one.
        // The label itself is decoration.
        egui::Label::new(
            egui::RichText::new(&text)
                .color(tokens.text_muted)
                .size(tokens.font_size - 1.0),
        )
        .truncate()
        // Every label in this bar is chrome: reporting on the frame, not holding text to copy. A
        // selectable one would swallow the click and paint a highlight over the count instead.
        .selectable(false),
    );
    let label_end = note_min;
    // Both indicators hover open the same titled list (`items_popup`); the selection's is the one Go
    // had a plain tooltip for, and the plain text survives as [`selection_tooltip`].
    //
    // The hover is the *band*, not the words' rect: see [`hover_band`] for why the difference is the
    // whole usability of the thing.
    if count > 0 {
        let band = hover_band(
            ui,
            bar.min.x + tokens.cell_padding,
            label_room,
            bar,
            "selected",
        );
        items_popup(&band, "SELECTED", count, &m.items, tokens);
    }
    if let Some((clip, label_text)) = chip.filter(|_| chip_shows) {
        let text_size = chip_galley.map_or(Vec2::ZERO, |galley| galley.size());
        let at = pos2(bar.min.x + tokens.cell_padding + label_room, bar.center().y);
        action_chip(ui, at, chip_room, clip.icon, &label_text, text_size, tokens);
        let band = hover_band(ui, at.x, chip_room, bar, "in-action");
        items_popup(&band, "IN-ACTION", clip.total, &clip.items, tokens);
    }
    let live: Vec<&JobSlot> = m.jobs.iter().filter(|job| job.live).collect();
    let right = if live.is_empty() {
        format!("{} pane{}", m.panes, plural(m.panes))
    } else {
        format!(
            "{} · {} job{} running",
            live[0].label,
            live.len(),
            plural(live.len())
        )
    };
    // The boot note, in the gap between the two cells: it is a fact about the frame, and it is
    // long, so it gets the middle and the truncation, with the whole sentence on hover.
    if let Some(note) = &m.note {
        let gap = egui::Rect::from_min_max(
            egui::pos2(label_end + tokens.cell_padding, bar.min.y),
            egui::pos2(note_max, bar.max.y),
        );
        if gap.width() > tokens.status_note_min_width {
            let response = ui.put(
                gap,
                egui::Label::new(
                    egui::RichText::new(note)
                        .color(tokens.text_muted)
                        .size(tokens.font_size - 2.0),
                )
                .truncate()
                .selectable(false),
            );
            let _ = response.on_hover_text(note.clone());
        }
    }
    // The pill's slot is counted whether or not the pill is up: an autosave lands on every
    // navigation, and a report that appeared and vanished would shove the pane/job label sideways
    // twice a second — the same reason the tab row used to reserve the room for it.
    let gap = ui.spacing().item_spacing.x;
    let pill_width = header::chip_room(ui, tokens, SAVED_LABEL) - gap;
    let cell_right = bar.max.x - tokens.cell_padding;
    let label_width = (cell_right - pill_width - gap - note_max).max(0.0);
    ui.put(
        egui::Rect::from_min_size(
            egui::pos2(note_max, bar.center().y - tokens.font_size),
            egui::vec2(label_width, tokens.font_size * 2.0),
        ),
        egui::Label::new(
            egui::RichText::new(right)
                .color(tokens.text_muted)
                .size(tokens.font_size - 1.0),
        )
        .truncate()
        .selectable(false),
    );
    // The `Saved` pill, in the corner: the last thing in the bar, and the only thing in it that is
    // drawn as a frame rather than as text.
    if m.saved && cell_right - pill_width - gap > note_max {
        crate::widgets::in_rect(
            ui,
            egui::Rect::from_min_max(
                egui::pos2(cell_right - pill_width, bar.min.y),
                egui::pos2(cell_right, bar.max.y),
            ),
            Layout::left_to_right(Align::Center),
            |ui| {
                header::chip(ui, tokens, SAVED_LABEL, tokens.success);
            },
        );
    }
    ui.advance_cursor_after_rect(bar);
}

/// The chip's words: `N item(s) in action`, the label the reference shots show beside the scissors.
#[must_use]
fn action_label(count: usize) -> String {
    format!("{count} item{} in action", plural(count))
}

/// How much room the chip needs: its glyph, a gap on each side of the words, and the words. The
/// same arithmetic the `Saved` pill's room is counted with (`header::chip_room`), for the same
/// reason: a chip measured while it is drawn moves the label it shares the bar with.
fn action_chip_width(tokens: &Tokens, text: f32) -> f32 {
    tokens.row_icon_size + tokens.chip_pad * 2.0 + text
}

/// Draw the in-action indicator: the glyph, the amber words, and the dashed underline the reference
/// shots show.
///
/// The words are a real [`egui::Label`] (so the accessibility tree and the kittest suites can find
/// them); everything else in the band is painted. The band itself was already counted by
/// [`action_chip_width`], so the chip cannot shove the selection count sideways. The label's response
/// is dropped: the popup hangs on [`hover_band`], which is the rect the pointer has to agree with.
fn action_chip(
    ui: &mut Ui,
    at: Pos2,
    room: f32,
    icon: &str,
    label: &str,
    text: Vec2,
    tokens: &Tokens,
) {
    let glyph = tokens.row_icon_size;
    let icon_at = Rect::from_min_size(pos2(at.x, at.y - glyph / 2.0), vec2(glyph, glyph));
    if ui.is_rect_visible(icon_at) {
        icons::paint(ui, icon_at, icon, glyph, tokens.warning);
    }
    let words = Rect::from_min_size(
        pos2(at.x + glyph + tokens.chip_pad, at.y - tokens.font_size),
        vec2(
            (room - glyph - tokens.chip_pad).max(0.0),
            tokens.font_size * 2.0,
        ),
    );
    let _response = ui.put(
        words,
        egui::Label::new(
            egui::RichText::new(label)
                .color(tokens.warning)
                .size(tokens.font_size - 1.0),
        )
        .truncate()
        .selectable(false),
    );
    // The underline sits under the *words*, not under the band: it marks the label as the thing to
    // hover, and a line across the empty tail would read as a frame.
    let y = at.y + text.y / 2.0 + 1.0;
    let x1 = (words.min.x + text.x).min(words.max.x);
    if ui.is_rect_visible(words) && x1 > words.min.x {
        for shape in egui::Shape::dashed_line(
            &[pos2(words.min.x, y), pos2(x1, y)],
            Stroke::new(1.0, tokens.warning),
            INDICATOR_DASH,
            INDICATOR_GAP,
        ) {
            ui.painter().add(shape);
        }
    }
}

/// The hover target of one indicator: the indicator's x-span, the **whole height** of the bar.
///
/// The list opens directly above the bar with no gap, so a full-height band and the list share an
/// edge: a pointer walking from the words up into the list is inside one or the other the whole way,
/// which is what makes the list *readable*. Anchored to the words' own rect — 26 px of a 30 px bar —
/// it would close on the way, and the queue would only ever be legible from the corner of one's eye.
/// Go got the same continuity free: the CSS `:hover` was on the element wrapping both
/// (`file-list.js:571-592`).
fn hover_band(ui: &mut Ui, x: f32, width: f32, bar: Rect, salt: &str) -> Response {
    let rect = Rect::from_min_max(pos2(x, bar.min.y), pos2(x + width, bar.max.y));
    let id = ui.scope_id().with(("status-hover", salt));
    ui.interact(rect, id, Sense::hover())
}

/// The hover list behind **both** indicators: a small-caps title, then one row per item (the glyph,
/// the name, the size on the right), then the `and N more item(s)` tail.
///
/// One painter for the two of them because the row shape is the one Go's clipboard item had
/// (`api/ops.go:166-176`): name, class, size. `total` is the whole queue or selection and `items` its
/// head — at most [`DISPLAY_CAP`] rows — so a 400-row selection can neither open a popup taller than
/// the window nor lie about how many items it is talking about.
fn items_popup(anchor: &Response, kind: &str, total: usize, items: &[StatusItem], tokens: &Tokens) {
    if items.is_empty() {
        return;
    }
    let title = format!("{kind} ITEMS ({total})");
    // The plain text doubles as the popup's accessible name: the rows are painted, and a screen reader
    // still has to hear the list.
    let spoken = item_list_text(total, items);
    let popup = egui::Popup::from_response(anchor);
    // The list has to outlive the pointer's departure from the words: [`Popup::get_popup_rect`]
    // answers where the list *would* sit this frame (the size it measured last frame, this anchor),
    // so a pointer inside it still counts as reading the queue — and one that has left both the band
    // and the list closes it again. Anchored to the hover alone, the list vanished the moment the
    // pointer moved to look at it, which is the one thing a list of queued names has to survive.
    let over_list = popup.get_popup_rect().is_some_and(|rect| {
        anchor
            .ctx
            .input(|i| i.pointer.hover_pos().is_some_and(|at| rect.contains(at)))
    });
    popup
        .open(anchor.hovered() || over_list)
        .accessible_name(format!("{title}: {spoken}"))
        .show(|ui| {
            ui.label(
                egui::RichText::new(&title)
                    .color(tokens.text_muted)
                    .size(tokens.font_size - 2.0)
                    .strong(),
            );
            let rows = items.len().min(DISPLAY_CAP);
            let tail = tail_line(total, rows);
            let font = egui::FontId::proportional(tokens.font_size - 1.0);
            let width_of = |text: String, colour: Color32| {
                ui.painter()
                    .layout_no_wrap(text, font.clone(), colour)
                    .size()
                    .x
            };
            // Two columns, both measured before anything is placed: the sizes line up on their right
            // edge, and the popup's width is the table's rather than the widest name's.
            let mut name_w = 0.0f32;
            let mut size_w = 0.0f32;
            for item in &items[..rows] {
                name_w = name_w.max(width_of(item.name.clone(), tokens.text_main));
                size_w = size_w.max(width_of(size_text(item), tokens.text_muted));
            }
            name_w = name_w.max(width_of(tail.clone(), tokens.text_muted));
            let row_w = tokens.row_icon_size + tokens.chip_pad * 2.0 + name_w + size_w;
            for item in &items[..rows] {
                popup_row(ui, item, row_w, tokens);
            }
            if !tail.is_empty() {
                ui.label(
                    egui::RichText::new(tail)
                        .color(tokens.text_muted)
                        .size(tokens.font_size - 1.0),
                );
            }
        });
}

/// One popup row, painted into a rect of its own: the glyph where the listing puts it, the name, the
/// size on the right ([`size_text`], which is the listing's own wording for "nobody read a size").
fn popup_row(ui: &mut Ui, item: &StatusItem, row_w: f32, tokens: &Tokens) {
    let glyph = tokens.row_icon_size;
    let (rect, _) = ui.allocate_exact_size(vec2(row_w, glyph), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    icons::paint(
        ui,
        Rect::from_min_size(
            pos2(rect.min.x, rect.center().y - glyph / 2.0),
            vec2(glyph, glyph),
        ),
        if item.is_dir { "folder" } else { "file" },
        glyph,
        // The listing's own rule: a folder is amber, a file is muted (`file-list.css:110-112`).
        if item.is_dir {
            tokens.warning
        } else {
            tokens.text_muted
        },
    );
    let font = egui::FontId::proportional(tokens.font_size - 1.0);
    ui.painter().text(
        pos2(rect.min.x + glyph + tokens.chip_pad, rect.center().y),
        egui::Align2::LEFT_CENTER,
        &item.name,
        font.clone(),
        tokens.text_main,
    );
    ui.painter().text(
        pos2(rect.max.x, rect.center().y),
        egui::Align2::RIGHT_CENTER,
        size_text(item),
        font,
        tokens.text_muted,
    );
}

/// The popup row's size cell: [`format_size`]'s words, or the same placeholder the listing's size cell
/// uses for a size nobody has read ([`UNKNOWN_SIZE`] — a directory's content size is the walk's
/// answer, and the popup has no walk to ask).
fn size_text(item: &StatusItem) -> String {
    item.size.map_or_else(
        || UNKNOWN_SIZE.to_string(),
        crate::widgets::disk_meter::format_size,
    )
}

/// The `and N more item(s)` line a capped list ends with, `""` when nothing was cut off.
fn tail_line(total: usize, shown: usize) -> String {
    let rest = total.saturating_sub(shown);
    if rest == 0 {
        return String::new();
    }
    format!("and {rest} more item{}", plural(rest))
}

/// The plain-text body of a popup: the first [`DISPLAY_CAP`] of `items` in order, then the remainder
/// of `total` (`file-list.js:571-592`, with the order fixed). Also the popup's accessible name, so the
/// words a screen reader hears and the words the pixels would print can never disagree.
#[must_use]
fn item_list_text(total: usize, items: &[StatusItem]) -> String {
    let shown = items.len().min(DISPLAY_CAP);
    let mut lines: Vec<String> = items.iter().take(shown).map(|i| i.name.clone()).collect();
    let tail = tail_line(total, shown);
    if !tail.is_empty() {
        lines.push(tail);
    }
    lines.join("\n")
}

/// The tooltip body: the first [`DISPLAY_CAP`] selected names in display order, then the remainder
/// (`file-list.js:571-592`, with the order fixed). The pixels show [`items_popup`]; this is the plain
/// text the popup's accessible name is built from.
#[must_use]
pub fn selection_tooltip(m: &StatusModel<'_>) -> String {
    item_list_text(m.selected, &m.items)
}

/// English plural `s`.
#[must_use]
pub fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A popup row.
    fn item(name: &str, is_dir: bool, size: Option<u64>) -> StatusItem {
        StatusItem {
            name: name.into(),
            is_dir,
            size,
        }
    }

    /// The bar's model, over whatever the test is about. `selected` is the count the bar was told,
    /// `items` the head it was handed — which is the whole point of keeping them apart.
    fn model(selected: usize, items: Vec<StatusItem>) -> StatusModel<'static> {
        StatusModel {
            selected,
            items,
            clip: None,
            info: StatusInfo::Count(0),
            jobs: &[],
            panes: 1,
            note: None,
            saved: false,
        }
    }

    #[test]
    fn the_action_chip_words_count_and_pluralise() {
        assert_eq!(action_label(1), "1 item in action");
        assert_eq!(action_label(2), "2 items in action");
        assert_eq!(action_label(0), "0 items in action");
    }

    #[test]
    fn the_popup_body_lists_the_names_then_the_remainder() {
        // What the shell hands the bar: ten names, and the count of the thirteen that are selected.
        let items = (0..DISPLAY_CAP)
            .map(|ix| item(&format!("row-{ix:02}"), false, Some(ix as u64)))
            .collect::<Vec<_>>();
        let text = selection_tooltip(&model(DISPLAY_CAP + 3, items));
        let lines = text.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), DISPLAY_CAP + 1, "{text}");
        assert_eq!(lines[0], "row-00");
        assert_eq!(lines[DISPLAY_CAP - 1], "row-09");
        assert_eq!(
            lines[DISPLAY_CAP], "and 3 more items",
            "the tail line the plain tooltip already had"
        );
        assert!(!text.contains("row-10"), "the head is a head: {text}");
    }

    #[test]
    fn a_short_list_gets_no_tail_line_and_an_empty_one_nothing_at_all() {
        let items = vec![item("a.txt", false, Some(1)), item("docs", true, None)];
        assert_eq!(selection_tooltip(&model(2, items)), "a.txt\ndocs");
        assert_eq!(selection_tooltip(&model(0, Vec::new())), "");
        // The title's number and the tail's come from the count, never from the head's length: a
        // 450-row selection is shown as ten names and 440 admitted ones.
        assert_eq!(tail_line(450, 2), "and 448 more items");
    }

    #[test]
    fn the_capped_tail_pluralises_with_the_remainder() {
        assert_eq!(tail_line(11, DISPLAY_CAP), "and 1 more item");
        assert_eq!(tail_line(12, DISPLAY_CAP), "and 2 more items");
        assert_eq!(tail_line(DISPLAY_CAP, DISPLAY_CAP), "");
    }

    #[test]
    fn an_unread_size_is_the_listings_placeholder_not_zero() {
        // Plan §2.4: a size nobody read is not a size of zero, and the popup must not invent its own
        // spelling of the fact either.
        assert_eq!(size_text(&item("a.txt", false, Some(2048))), "2.0 KiB");
        assert_eq!(size_text(&item("docs", true, None)), UNKNOWN_SIZE);
        assert_eq!(size_text(&item("docs", true, None)), "--");
    }
}
