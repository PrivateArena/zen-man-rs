//! The pane's navigation bar: the address bar and the icon ribbon, in one row (plan §5.8
//! `breadcrumb.rs`, restyled for the browser-like shell).
//!
//! One strip, three groups, laid out from the pane's own width:
//!
//! ```text
//! [back][fwd][up]  Root / home / jang  [edit] | [view][refresh][walk][all][none] | [history][places][actions][2-pane][4-pane]
//! ```
//!
//! * the history and up buttons and the crumb trail are [`crate::panels::breadcrumb`]'s — this file
//!   does not re-lay them out, it only decides how much room it leaves them;
//! * the **pane tools** (`grid`, `refresh`, the recursive-listing dropdown, select all / none) used to
//!   be a separate 50 px row per pane with *text* buttons;
//! * the **global buttons** (action history, the two drawers, the two layout switches) used to be a
//!   window-wide header bar with text labels and printed chords. They are now the pane's, because
//!   the pane's chrome is the only chrome left. A state that is one per window reads the same from
//!   either end of it, which is what a browser's per-stack toolbar means.
//!
//! # Width
//!
//! The address bar is the subject of the row, so it is what the ribbon may never squeeze. The
//! ribbon's cost is arithmetic — a fixed square per button ([`Tokens::nav_button`]) — and known
//! before anything is measured, so the trail budgets against it (see [`ribbon_width`]). What the
//! ribbon may have is what the row has left after the address strip's floor
//! ([`Tokens::nav_trail_min_width`]): the globals fold into one overflow button first, the pane
//! tools after them.
//!
//! Every control here is an icon plus a tooltip plus an accessible label: an icon with no name would
//! be unreachable by keyboard, test or screen reader alike.

use egui::{Align, Layout, Rect, Response, RichText, Sense, Ui, vec2};

use crate::panels::breadcrumb::{self, BreadcrumbModel};
use crate::panels::file_pane::PaneCommand;
use crate::panels::header::HeaderCommand;
use crate::state::FlatMode;
use crate::theme::Tokens;

/// The current state of everything the bar shows.
pub struct NavBarModel<'a> {
    /// The address half (`Root / home / jang`), `None` while the pane has no tab to name a path for.
    /// A tab-less pane still gets the bar — its ribbon holds the window's verbs, and the `+` that
    /// makes the first tab lives in the row above.
    pub address: Option<BreadcrumbModel<'a>>,
    /// The recursive-listing mode the `tree` button shows.
    pub flat: FlatMode,
    /// The listing is in grid view.
    pub grid: bool,
    /// `true` when a 4-pane grid is on screen.
    pub quad: bool,
    /// `true` when a 2-pane grid is on screen.
    pub split: bool,
    /// The action-history slide-in is open.
    pub log_open: bool,
    /// The places drawer is open.
    pub left_drawer: bool,
    /// The custom-actions drawer is open.
    pub right_drawer: bool,
    /// The chord text the tooltips quote (`F3`, `Ctrl+H`) — [`crate::keymap::KeyMap::hint_text`],
    /// never a second table (plan **D7**).
    pub hints: NavBarHints,
}

/// The chords the ribbon's tooltips quote, read from the keymap once per frame by the caller.
pub struct NavBarHints {
    /// The re-read chord (`F5` in the default map).
    pub refresh: String,
    /// The action-history chord (`H` in the default map).
    pub history: String,
    /// The two-pane chord (`F3`).
    pub split: String,
    /// The four-pane chord (`F4`).
    pub quad: String,
}

/// One row of the ribbon: what to paint, and what clicking it means.
#[derive(Clone)]
enum Slot {
    /// A pane command (grid, refresh, selection).
    Pane(PaneCommand),
    /// A window command (drawers, history, layout).
    Global(HeaderCommand),
    /// Open the recursive-listing dropdown.
    Flat,
}

/// Draw the bar and report both command kinds.
pub fn show(
    ui: &mut Ui,
    m: &mut NavBarModel<'_>,
    tokens: &Tokens,
    out: &mut Vec<PaneCommand>,
    global: &mut Vec<HeaderCommand>,
) {
    let whole = Rect::from_min_size(
        ui.cursor().min,
        vec2(ui.available_width(), tokens.toolbar_height),
    );
    // One bar, one background: the row reads as a single control, not as an address strip with a
    // toolbar parked on the pane's own background beside it.
    if ui.is_rect_visible(whole) {
        ui.painter().rect_filled(whole, 0.0, tokens.bg_card);
        ui.painter()
            .hline(whole.x_range(), whole.max.y - 1.0, tokens.hairline());
    }
    // The pane's own tools mean nothing without a tab to apply them to.
    let pane_tools: Vec<Slot> = if m.address.is_some() {
        vec![
            Slot::Pane(PaneCommand::ToggleGrid),
            Slot::Pane(PaneCommand::Refresh),
            Slot::Flat,
            Slot::Pane(PaneCommand::SelectAll),
            Slot::Pane(PaneCommand::SelectNone),
        ]
    } else {
        Vec::new()
    };
    let globals = [
        Slot::Global(HeaderCommand::ToggleLog),
        Slot::Global(HeaderCommand::ToggleLeftDrawer),
        Slot::Global(HeaderCommand::ToggleRightDrawer),
        Slot::Global(HeaderCommand::ToggleSplit),
        Slot::Global(HeaderCommand::ToggleQuad),
    ];
    // The address bar is the subject of this row: the ribbon gets what the row has left after its
    // floor ([`Tokens::nav_trail_min_width`]), and the widest shape that fits is the one drawn.
    let budget = (whole.width() - tokens.nav_trail_min_width).max(0.0);
    let mut folded: Vec<Slot> = Vec::new();
    let (shown_tools, shown_globals) =
        if ribbon_width(tokens, pane_tools.len() + globals.len()) <= budget {
            (pane_tools.clone(), globals.to_vec())
        } else if ribbon_width(tokens, pane_tools.len() + 1) <= budget {
            folded.extend(globals.iter().cloned());
            (pane_tools.clone(), Vec::new())
        } else {
            folded.extend(pane_tools.iter().cloned());
            folded.extend(globals.iter().cloned());
            (Vec::new(), Vec::new())
        };
    let count = shown_tools.len() + shown_globals.len() + usize::from(!folded.is_empty());
    let right = ribbon_width(tokens, count);
    // The ribbon sits flush with the pane's right edge; the address strip takes all the rest.
    let ribbon_rect = Rect::from_min_max(
        egui::pos2((whole.max.x - right).max(whole.min.x), whole.min.y),
        whole.max,
    );
    let trail_rect = Rect::from_min_max(whole.min, egui::pos2(ribbon_rect.min.x, whole.max.y));
    let inset = ((tokens.toolbar_height - tokens.nav_button) * 0.5).max(0.0);
    let ribbon_rect = Rect::from_min_max(
        egui::pos2(ribbon_rect.min.x, ribbon_rect.min.y + inset),
        egui::pos2(ribbon_rect.max.x, ribbon_rect.max.y - inset),
    );
    crate::widgets::in_rect(ui, trail_rect, Layout::left_to_right(Align::Center), |ui| {
        match &mut m.address {
            Some(address) => breadcrumb::show(ui, address, tokens, out),
            None => {
                // The bar of a pane with nothing listed: the room is kept, the affordances are not.
                ui.label(
                    RichText::new("no directory")
                        .color(tokens.text_muted)
                        .size(tokens.font_size),
                );
            }
        }
    });
    crate::widgets::in_rect(
        ui,
        ribbon_rect,
        Layout::right_to_left(Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing = vec2(tokens.nav_gap, 0.0);
            if !folded.is_empty() {
                draw_overflow(ui, tokens, &folded, m, out, global);
            }
            for slot in shown_globals.iter().rev() {
                paint_slot(ui, tokens, slot.clone(), m, out, global);
            }
            group_divider(
                ui,
                tokens,
                !shown_globals.is_empty() && !shown_tools.is_empty(),
            );
            for slot in shown_tools.iter().rev() {
                paint_slot(ui, tokens, slot.clone(), m, out, global);
            }
        },
    );
    ui.advance_cursor_after_rect(whole);
}

/// The width a ribbon of `count` icon buttons costs, including its overflow button's share.
#[must_use]
pub fn ribbon_width(tokens: &Tokens, count: usize) -> f32 {
    count as f32 * (tokens.nav_button + tokens.nav_gap) + tokens.nav_group_gap
}

/// One chrome icon button: a token-sized square, a painted glyph, a tooltip, and — the part that
/// keeps an icon-only control reachable — an accessible label of its own.
pub fn icon_button(ui: &mut Ui, tokens: &Tokens, icon: &str, tooltip: &str) -> Response {
    icon_button_state(ui, tokens, icon, tooltip, false, None)
}

/// [`icon_button`] with the two things a *toggle* needs: the accent frame that says "this is the
/// mode on screen" (Go's `.btn-global.active`, `workspace.css:36-58`), and the name the a11y tree
/// gets, when the tooltip is a sentence rather than a label.
fn icon_button_state(
    ui: &mut Ui,
    tokens: &Tokens,
    icon: &str,
    tooltip: &str,
    selected: bool,
    name: Option<&str>,
) -> Response {
    let side = tokens.nav_button;
    let (rect, response) = ui.allocate_exact_size(vec2(side, side), Sense::click());
    if ui.is_rect_visible(rect) {
        let radius = egui::CornerRadius::same(tokens.crumb_corner_radius);
        if selected {
            ui.painter().rect_filled(rect, radius, tokens.bg_hover);
            ui.painter().rect_stroke(
                rect,
                radius,
                egui::Stroke::new(1.0, tokens.accent),
                egui::StrokeKind::Inside,
            );
        } else if response.hovered() {
            ui.painter().rect_filled(rect, radius, tokens.bg_hover);
        }
        let tint = if selected {
            tokens.text_active
        } else if response.hovered() {
            tokens.text_main
        } else {
            tokens.text_muted
        };
        crate::icons::paint(ui, rect, icon, tokens.row_icon_size, tint);
    }
    let response = response.on_hover_text(tooltip);
    let label = name.unwrap_or(tooltip).to_owned();
    response.widget_info(|| {
        let mut info = egui::WidgetInfo::new(egui::Role::Button);
        info.label = Some(label.clone());
        info
    });
    response
}

/// The hairline between two ribbon groups; paints nothing when there is only one group.
fn group_divider(ui: &mut Ui, tokens: &Tokens, draw: bool) {
    if !draw {
        return;
    }
    let side = tokens.nav_button;
    let (rect, _) = ui.allocate_exact_size(
        vec2(
            (tokens.nav_group_gap - tokens.nav_gap * 2.0).max(tokens.nav_gap),
            side,
        ),
        Sense::hover(),
    );
    if ui.is_rect_visible(rect) {
        ui.painter().vline(
            rect.center().x,
            egui::Rangef::new(rect.min.y + tokens.nav_gap, rect.max.y - tokens.nav_gap),
            tokens.hairline(),
        );
    }
}

/// Paint one ribbon control, and answer the click (or the dropdown) it gets.
fn paint_slot(
    ui: &mut Ui,
    tokens: &Tokens,
    slot: Slot,
    m: &NavBarModel<'_>,
    out: &mut Vec<PaneCommand>,
    global: &mut Vec<HeaderCommand>,
) {
    let icon = slot.icon(m.grid);
    let response = icon_button_state(
        ui,
        tokens,
        icon,
        &slot.tooltip(m),
        slot.selected(m),
        Some(slot.name()),
    );
    match slot {
        Slot::Flat => {
            egui::Popup::menu(&response).show(|ui| flat_rows(ui, m.flat, out));
        }
        other => {
            if response.clicked() {
                match other {
                    Slot::Pane(command) => out.push(command),
                    Slot::Global(command) => global.push(command),
                    _ => {}
                }
            }
        }
    }
}

/// The three recursive-listing modes, as menu rows in Go's own words (`main.js`'s flat-view
/// dropdown: *mixed*, *mixed_no_folders*, *grouped*). The chosen one is drawn as current.
fn flat_rows(ui: &mut Ui, flat: FlatMode, out: &mut Vec<PaneCommand>) {
    for (mode, label, tip) in FlatMode::rows() {
        if ui
            .selectable_label(flat == mode, label)
            .on_hover_text(tip)
            .clicked()
        {
            out.push(PaneCommand::SetFlat(mode));
        }
    }
}

/// The overflow popup: whichever groups the pane has no room for, as named rows. The groups keep
/// their order — the pane's tools, then the window's verbs — with the line between them, so the
/// menu reads as the bar it folded out of.
fn draw_overflow(
    ui: &mut Ui,
    tokens: &Tokens,
    folded: &[Slot],
    m: &NavBarModel<'_>,
    out: &mut Vec<PaneCommand>,
    global: &mut Vec<HeaderCommand>,
) {
    let response = icon_button(ui, tokens, "menu", "More controls");
    egui::Popup::menu(&response).show(|ui| {
        let (tools, globals): (Vec<Slot>, Vec<Slot>) = folded
            .iter()
            .cloned()
            .partition(|slot| matches!(slot, Slot::Pane(_) | Slot::Flat));
        overflow_rows(ui, tokens, &tools, m, out, global);
        if !tools.is_empty() && !globals.is_empty() {
            ui.separator();
        }
        overflow_rows(ui, tokens, &globals, m, out, global);
    });
}

/// The rows of one folded group. The recursive-listing control is not one command but three, so its
/// row becomes a labelled section rather than a button.
fn overflow_rows(
    ui: &mut Ui,
    tokens: &Tokens,
    slots: &[Slot],
    m: &NavBarModel<'_>,
    out: &mut Vec<PaneCommand>,
    global: &mut Vec<HeaderCommand>,
) {
    for slot in slots {
        if let Slot::Flat = slot {
            ui.label(
                RichText::new("Recursive listing")
                    .color(tokens.text_muted)
                    .size(tokens.font_size - 2.0),
            );
            flat_rows(ui, m.flat, out);
            continue;
        }
        if ui
            .button(RichText::new(slot.text(m)).color(tokens.text_main))
            .clicked()
        {
            match slot.clone() {
                Slot::Pane(command) => out.push(command),
                Slot::Global(command) => global.push(command),
                Slot::Flat => {}
            }
        }
    }
}

impl Slot {
    /// The glyph. `grid` and `list` describe the *other* mode, which is what the button offers.
    fn icon(&self, grid: bool) -> &'static str {
        match self {
            Slot::Pane(PaneCommand::ToggleGrid) => {
                if grid {
                    "list"
                } else {
                    "grid"
                }
            }
            Slot::Pane(PaneCommand::Refresh) => "refresh",
            Slot::Pane(PaneCommand::SelectAll) => "select-all",
            Slot::Pane(PaneCommand::SelectNone) => "select-none",
            Slot::Flat => "tree",
            Slot::Global(HeaderCommand::ToggleLog) => "history",
            Slot::Global(HeaderCommand::ToggleLeftDrawer) => "folder-open",
            Slot::Global(HeaderCommand::ToggleRightDrawer) => "terminal",
            Slot::Global(HeaderCommand::ToggleSplit) => "split",
            Slot::Global(HeaderCommand::ToggleQuad) => "quad",
            // A control this file does not name: the generic affordance, still reachable by name.
            Slot::Global(_) | Slot::Pane(_) => "menu",
        }
    }

    /// The accessible name: two words, no sentence, so an icon-only control stays reachable.
    fn name(&self) -> &'static str {
        match self {
            Slot::Pane(PaneCommand::ToggleGrid) => "view",
            Slot::Pane(PaneCommand::Refresh) => "refresh",
            Slot::Pane(PaneCommand::SelectAll) => "select all",
            Slot::Pane(PaneCommand::SelectNone) => "select none",
            Slot::Flat => "recursive listing",
            Slot::Global(HeaderCommand::ToggleLog) => "history",
            Slot::Global(HeaderCommand::ToggleLeftDrawer) => "places",
            Slot::Global(HeaderCommand::ToggleRightDrawer) => "custom actions",
            Slot::Global(HeaderCommand::ToggleSplit) => "2-pane",
            Slot::Global(HeaderCommand::ToggleQuad) => "4-pane",
            Slot::Global(_) | Slot::Pane(_) => "more",
        }
    }

    /// The row text of the overflow popup — the same words as the tooltip, chord included.
    fn text(&self, m: &NavBarModel<'_>) -> String {
        match self {
            Slot::Pane(PaneCommand::ToggleGrid) => {
                if m.grid {
                    "List view".to_string()
                } else {
                    "Grid view".to_string()
                }
            }
            Slot::Pane(PaneCommand::Refresh) => format!("Refresh ({})", m.hints.refresh),
            Slot::Pane(PaneCommand::SelectAll) => "Select all".to_string(),
            Slot::Pane(PaneCommand::SelectNone) => "Select none".to_string(),
            Slot::Flat => format!("Recursive listing: {}", m.flat.label()),
            Slot::Global(HeaderCommand::ToggleLog) => {
                format!("Action history ({})", m.hints.history)
            }
            Slot::Global(HeaderCommand::ToggleLeftDrawer) => "Places".to_string(),
            Slot::Global(HeaderCommand::ToggleRightDrawer) => "Custom actions".to_string(),
            Slot::Global(HeaderCommand::ToggleSplit) => format!("2-pane ({})", m.hints.split),
            Slot::Global(HeaderCommand::ToggleQuad) => format!("4-pane ({})", m.hints.quad),
            Slot::Global(_) | Slot::Pane(_) => "Action".to_string(),
        }
    }

    /// The tooltip is the row text, chord and all: the keymap's own spelling of `F3` is what says
    /// which key does the same thing (plan **D7** — one table, and the icon-only bar is its second
    /// front end), and one string for both surfaces is one less chance to disagree.
    fn tooltip(&self, m: &NavBarModel<'_>) -> String {
        self.text(m)
    }

    /// `true` when this button *is* the state on screen — the accent frame's condition.
    fn selected(&self, m: &NavBarModel<'_>) -> bool {
        match self {
            Slot::Global(HeaderCommand::ToggleLog) => m.log_open,
            Slot::Global(HeaderCommand::ToggleLeftDrawer) => m.left_drawer,
            Slot::Global(HeaderCommand::ToggleRightDrawer) => m.right_drawer,
            Slot::Global(HeaderCommand::ToggleSplit) => m.split,
            Slot::Global(HeaderCommand::ToggleQuad) => m.quad,
            _ => false,
        }
    }
}
