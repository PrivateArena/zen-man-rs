//! The ribbon's parent button and the panel behind it: the shell's second level.
//!
//! The reference bar was one row of icons with nothing behind them (`nav_bar.rs`). This is the
//! thing behind one of them — a button that opens a panel of icon-over-label tiles the way
//! Firefox's application menu does, for the controls that are *modes* rather than actions: the
//! listing's view and the two layout switches today, and the room is here for the ones that follow
//! (zoom, theme).
//!
//! # Staying open
//!
//! A tile is a switch, so clicking one must not cost the panel: it closes on a click *outside* it,
//! on Escape, on a second click of the parent button, or on the header's `X` — and on nothing else.
//! That is [`PopupCloseBehavior::CloseOnClickOutside`] plus a close button of our own, and it is
//! the whole difference between this panel and the bar's own dropdown ([`egui::Popup::menu`],
//! which closes on the first click anywhere).
//!
//! # Adding a tile
//!
//! [`Entry`] is the panel's only vocabulary: a glyph, the word under it (which doubles as the
//! accessible name, so the word on screen is the word a test or a screen reader uses), a hover
//! line that may repeat it with its chord, and the command a click reports. The list itself lives
//! in [`crate::panels::nav_bar::menu_entries`] — one place, because the panel is the bar's.
//!
//! The one thing in here that is not a tile is the zoom row: the `− | 100% | +` strip Firefox puts at
//! the top of its application menu, because a zoom wants its level *printed*, and a tile of a number
//! is a control the user has to open twice to read.

use std::sync::Arc;

use egui::{
    Align, Align2, Color32, CornerRadius, FontId, Galley, Layout, Margin, Popup,
    PopupCloseBehavior, PopupKind, Rect, Response, Role, Sense, SetOpenCommand, Stroke, Ui,
    WidgetInfo, pos2, vec2,
};

use super::file_pane::PaneCommand;
use super::header::HeaderCommand;
use crate::theme::Tokens;
use crate::zoom::ZoomStatus;
use zenman_core::store::config::DEFAULT_ZOOM_PERCENT;

/// The panel's title, and the parent button's tooltip: the shortest name that says what the button
/// is for without naming today's two tiles and going stale on tomorrow's third.
const TITLE: &str = "Menu";

/// The parent button's accessible name — the ribbon's own style, two words at most (`nav_bar`).
const BUTTON_NAME: &str = "menu";

/// The header's close button: tooltip and accessible name alike, as everywhere in the ribbon.
const CLOSE: &str = "Close menu";

/// What the panel's popup is keyed by, under the pane's own [`egui::Id`] — see [`show`].
const POPUP_KEY: &str = "app-menu";

/// The zoom row's cells, left to right: the accessible name, then the hover line. The names are the
/// searchable words (`−` and `+` are not), the hover lines quote egui's own chords — the row is the
/// button half of what `egui::gui_zoom` already answers, and those chords are not in the shell's
/// keymap, so plan **D7**'s one-table rule has nothing to say about them.
const ZOOM_CELLS: [(&str, &str); 3] = [
    ("zoom out", "Zoom out (Ctrl+-)"),
    ("reset zoom", "Reset zoom (Ctrl+0)"),
    ("zoom in", "Zoom in (Ctrl++)"),
];

/// The row's minus: U+2212, the typographic sign Firefox prints, not the ASCII hyphen of a filename.
const ZOOM_MINUS: &str = "\u{2212}";

/// What clicking a tile reports, in the bar's own two command kinds.
#[derive(Clone, Debug, PartialEq)]
pub enum EntryCommand {
    /// A command for the pane the bar belongs to.
    Pane(PaneCommand),
    /// A command for the window.
    Global(HeaderCommand),
}

impl From<PaneCommand> for EntryCommand {
    fn from(command: PaneCommand) -> Self {
        Self::Pane(command)
    }
}

impl From<HeaderCommand> for EntryCommand {
    fn from(command: HeaderCommand) -> Self {
        Self::Global(command)
    }
}

/// One tile of the panel.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// The glyph, by registry name (`icons::paint`).
    pub icon: &'static str,
    /// The caption under the glyph, and the accessible name.
    pub label: String,
    /// The hover line: the same words plus the chord, the way the ribbon spells it (**D7**).
    pub tooltip: String,
    /// `true` when this tile *is* the state on screen — the accent frame's condition, and the
    /// ribbon's (`nav_bar::icon_button_state`).
    pub selected: bool,
    /// What a click reports.
    pub command: EntryCommand,
}

impl Entry {
    /// One tile: the words are given as text and become owned, so the caller can format a chord into
    /// the tooltip and pass a `&str` literal for the caption.
    #[must_use]
    pub fn new(
        icon: &'static str,
        label: impl Into<String>,
        tooltip: impl Into<String>,
        selected: bool,
        command: impl Into<EntryCommand>,
    ) -> Self {
        Self {
            icon,
            label: label.into(),
            tooltip: tooltip.into(),
            selected,
            command: command.into(),
        }
    }
}

/// Draw the parent button in the ribbon, and the panel anchored under it while it is open.
///
/// `ui` is the ribbon's own right-to-left layout: the button takes one slot of it, the panel is a
/// separate layer. Clicked tiles land in the bar's two command vectors, like every other control
/// here (`nav_bar::show`).
///
/// The popup id is this pane's own [`Ui`] id plus [`POPUP_KEY`] rather than egui's automatic one,
/// which is what lets the *button* know it is open: egui mints a widget's id as it allocates it, so
/// an automatic id is not knowable before the button is drawn — and the frame that draws it is the
/// one that has to light it up.
pub fn show(
    ui: &mut Ui,
    tokens: &Tokens,
    entries: &[Entry],
    zoom: ZoomStatus,
    out: &mut Vec<PaneCommand>,
    global: &mut Vec<HeaderCommand>,
) {
    let popup_id = ui.scope_id().with(POPUP_KEY);
    let response = parent_button(ui, tokens, Popup::is_id_open(ui.ctx(), popup_id));
    let popup = Popup::new(popup_id, ui.ctx().clone(), &response, response.layer_id)
        .anchor_widget(response.id)
        .kind(PopupKind::Popup)
        .align(egui::RectAlign::BOTTOM_END)
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        .frame(frame(tokens))
        .width(panel_width(tokens))
        .accessible_name(TITLE)
        .open_memory(response.clicked().then_some(SetOpenCommand::Toggle));
    if let Some(body) = popup.show(|ui| draw_panel(ui, tokens, entries, zoom, out, global))
        && body.inner
    {
        Popup::close_id(ui.ctx(), popup_id);
    }
}

/// The ribbon's slot: the three lines, framed while the panel is open.
fn parent_button(ui: &mut Ui, tokens: &Tokens, open: bool) -> Response {
    crate::panels::nav_bar::icon_button_state(ui, tokens, "menu", TITLE, open, Some(BUTTON_NAME))
}

/// The panel's own box: the menu tokens rather than egui's popup default, so the width the button
/// asks for and the room the tiles get are the same arithmetic.
fn frame(tokens: &Tokens) -> egui::Frame {
    egui::Frame::new()
        .fill(tokens.bg_card)
        .stroke(Stroke::new(1.0, tokens.border))
        .corner_radius(CornerRadius::same(tokens.menu_corner_radius))
        .inner_margin(Margin::same(tokens.menu_tile_pad as i8))
        .shadow(egui::epaint::Shadow {
            offset: [0, tokens.menu_shadow_offset_y],
            blur: tokens.menu_shadow_blur,
            spread: 0,
            color: tokens.menu_shadow,
        })
}

/// The panel's width: one full row of tiles, the gaps between them, and the frame's two margins.
/// A short last row does not narrow it — a panel that changes width as it grows is a panel whose
/// tiles move under the cursor.
fn panel_width(tokens: &Tokens) -> f32 {
    let columns = f32::from(tokens.menu_columns).max(1.0);
    columns * tokens.menu_tile_width
        + (columns - 1.0) * tokens.menu_tile_gap
        + 2.0 * tokens.menu_tile_pad
}

/// Draw the panel's body and report whether its close button was pressed.
///
/// The tiles do *not* close it: the popup's close behaviour is `CloseOnClickOutside`, so only a
/// click past its edge, Escape, the parent button, or the returned `true` here does.
fn draw_panel(
    ui: &mut Ui,
    tokens: &Tokens,
    entries: &[Entry],
    zoom: ZoomStatus,
    out: &mut Vec<PaneCommand>,
    global: &mut Vec<HeaderCommand>,
) -> bool {
    let mut close = false;
    // The header: the panel's name at the left edge, its `X` at the right one. The name is painted
    // and the button laid out, because a row that has to reach both edges of a fixed-width panel
    // cannot be left to a shrink-to-fit layout.
    let head = Rect::from_min_size(
        ui.cursor().min,
        vec2(ui.available_width(), tokens.menu_item_height),
    );
    if ui.is_rect_visible(head) {
        ui.painter().text(
            head.left_center(),
            Align2::LEFT_CENTER,
            TITLE,
            FontId::proportional(tokens.font_size),
            tokens.text_main,
        );
    }
    crate::widgets::in_rect(ui, head, Layout::right_to_left(Align::Center), |ui| {
        if crate::panels::nav_bar::icon_button(ui, tokens, "close", CLOSE).clicked() {
            close = true;
        }
    });
    ui.add_space(tokens.menu_tile_gap);
    zoom_row(ui, tokens, zoom, global);
    ui.separator();
    ui.add_space(tokens.menu_tile_gap);

    let columns = usize::from(tokens.menu_columns).max(1);
    for (row_ix, row) in entries.chunks(columns).enumerate() {
        if row_ix > 0 {
            ui.add_space(tokens.menu_tile_gap);
        }
        let row_width = row.len() as f32 * tokens.menu_tile_width
            + row.len().saturating_sub(1) as f32 * tokens.menu_tile_gap;
        let indent = (ui.available_width() - row_width) * 0.5;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = tokens.menu_tile_gap;
            // A short last row is centred, not left-piled: the panel is a grid of affordances and
            // the tile a finger is aiming at must not depend on how many rows the bar needed.
            ui.add_space(indent.max(0.0));
            for entry in row {
                if tile(ui, tokens, entry).clicked() {
                    match entry.command.clone() {
                        EntryCommand::Pane(command) => out.push(command),
                        EntryCommand::Global(command) => global.push(command),
                    }
                }
            }
        });
    }
    close
}

/// The zoom strip: `− | 100% | +`, three equal cells across the panel, the level printed in the middle
/// one — Firefox's own row, and the reason it is not a tile: the number is the state.
///
/// Laid out and hit-tested by hand rather than by a layout, because the row has to reach both edges
/// of a fixed-width panel and divide it exactly in three — the same reason the header above it is
/// painted ([`draw_panel`]). A cell with nothing left to give is dimmed, inert and announced as
/// disabled: the floor and the ceiling are egui's, and the row says so instead of clicking into a
/// no-op.
fn zoom_row(ui: &mut Ui, tokens: &Tokens, zoom: ZoomStatus, global: &mut Vec<HeaderCommand>) {
    let row = Rect::from_min_size(
        ui.cursor().min,
        vec2(ui.available_width(), tokens.menu_item_height),
    );
    let cell_width = row.width() / ZOOM_CELLS.len() as f32;
    for (ix, (name, tooltip)) in ZOOM_CELLS.iter().enumerate() {
        let x = row.min.x + cell_width * ix as f32;
        let cell = Rect::from_min_size(pos2(x, row.min.y), vec2(cell_width, row.height()));
        let ZoomCell {
            glyph,
            command,
            live,
            value,
        } = zoom_cell(zoom, ix);
        let response = ui.allocate_rect(cell, Sense::click());
        if ui.is_rect_visible(cell) {
            if live && response.hovered() {
                ui.painter().rect_filled(
                    cell,
                    CornerRadius::same(tokens.menu_corner_radius),
                    tokens.bg_hover,
                );
            }
            let tint = if !live {
                tokens.disabled_text
            } else if response.hovered() {
                tokens.text_main
            } else {
                tokens.text_muted
            };
            ui.painter().text(
                cell.center(),
                Align2::CENTER_CENTER,
                &glyph,
                FontId::proportional(tokens.font_size),
                tint,
            );
        }
        let response = if live {
            response.on_hover_text(*tooltip)
        } else {
            response
        };
        let label = (*name).to_owned();
        response.widget_info(|| {
            let mut info = WidgetInfo::new(Role::Button);
            info.label = Some(label.clone());
            // The row's own `add_enabled` (`nav_bar::icon_button_state`'s accent frame is its visual
            // half): without this a screen reader is offered a button that does nothing.
            info.enabled = live;
            // And the number the row exists to print, in the tree as well as on the screen.
            info.value = value;
            info
        });
        if live && response.clicked() {
            global.push(command);
        }
    }
    // The two dividers, painted after every fill: a hovered cell's own rect reaches the line, and
    // `bg_hover` is opaque.
    for edge in 1..ZOOM_CELLS.len() {
        if ui.is_rect_visible(row) {
            ui.painter().vline(
                row.min.x + cell_width * edge as f32,
                egui::Rangef::new(row.min.y + tokens.nav_gap, row.max.y - tokens.nav_gap),
                tokens.hairline(),
            );
        }
    }
    ui.advance_cursor_after_rect(row);
}

/// One cell of the zoom row.
struct ZoomCell {
    /// The text: a glyph, or the level itself for the cell that prints it.
    glyph: String,
    /// What a click means.
    command: HeaderCommand,
    /// Whether there is anything this cell could do at the current level.
    live: bool,
    /// The level, for the a11y tree of the cell that *prints* it — `None` for the two glyph cells,
    /// whose names are already their whole meaning.
    value: Option<f64>,
}

/// The `ix`th cell: what it says, what clicking it means, and whether it has anything left to give
/// (`ZoomStatus`'s two bounds, and the native scale for the middle one).
fn zoom_cell(zoom: ZoomStatus, ix: usize) -> ZoomCell {
    match ix {
        0 => ZoomCell {
            glyph: ZOOM_MINUS.to_owned(),
            command: HeaderCommand::ZoomOut,
            live: !zoom.at_min,
            value: None,
        },
        1 => ZoomCell {
            glyph: zoom.caption(),
            command: HeaderCommand::ZoomReset,
            live: zoom.percent != DEFAULT_ZOOM_PERCENT,
            value: Some(f64::from(zoom.percent)),
        },
        _ => ZoomCell {
            glyph: "+".to_owned(),
            command: HeaderCommand::ZoomIn,
            live: !zoom.at_max,
            value: None,
        },
    }
}

/// One tile: the glyph over its caption, the pair centred in the tile's own box.
fn tile(ui: &mut Ui, tokens: &Tokens, entry: &Entry) -> Response {
    let (rect, response) = ui.allocate_exact_size(
        vec2(tokens.menu_tile_width, tokens.menu_tile_height),
        Sense::click(),
    );
    if ui.is_rect_visible(rect) {
        let radius = CornerRadius::same(tokens.menu_corner_radius);
        if entry.selected {
            ui.painter().rect_filled(rect, radius, tokens.bg_hover);
            ui.painter().rect_stroke(
                rect,
                radius,
                Stroke::new(1.0, tokens.accent),
                egui::StrokeKind::Inside,
            );
        } else if response.hovered() {
            ui.painter().rect_filled(rect, radius, tokens.bg_hover);
        }
        let tint = if entry.selected {
            tokens.text_active
        } else if response.hovered() {
            tokens.text_main
        } else {
            tokens.text_muted
        };
        let icon = tokens.menu_tile_icon_size;
        let caption = caption(ui, tokens, &entry.label, rect.width() - 2.0, tint);
        let block = icon + tokens.menu_tile_gap + caption.size().y;
        let top = rect.center().y - block * 0.5;
        crate::icons::paint(
            ui,
            Rect::from_min_size(pos2(rect.center().x - icon * 0.5, top), vec2(icon, icon)),
            entry.icon,
            icon,
            tint,
        );
        ui.painter().galley(
            pos2(
                rect.center().x - caption.size().x * 0.5,
                top + icon + tokens.menu_tile_gap,
            ),
            caption,
            tint,
        );
    }
    let label = entry.label.clone();
    let response = response.on_hover_text(entry.tooltip.clone());
    response.widget_info(|| {
        let mut info = WidgetInfo::new(Role::Button);
        info.label = Some(label.clone());
        info
    });
    response
}

/// A tile's caption: one line, ellipsised into the tile's width the way the workspace button's
/// name is (`header::paint_clipped`) — the tooltip has the whole words.
fn caption(ui: &Ui, tokens: &Tokens, text: &str, max_width: f32, colour: Color32) -> Arc<Galley> {
    let mut job = egui::text::LayoutJob::simple(
        text.to_owned(),
        FontId::proportional(tokens.menu_label_size),
        colour,
        max_width,
    );
    job.wrap = egui::text::TextWrapping {
        max_width,
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    ui.painter().layout_job(job)
}
