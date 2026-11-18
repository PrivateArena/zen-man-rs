//! The `DockContent` adapter (plan §5.7 `dock_content.rs` row).
//!
//! Step 7's `body` is the **empty pane**: the 38 px tab strip the pane owns (the dock draws none —
//! stripless groups, §2.2), a toolbar row, and the empty listing placeholder. Step 8 replaces the
//! placeholder with `panels::file_pane::show` (the virtual list); nothing else about the plumbing
//! changes, which is the point of the adapter seam.

use craft_ui::docking::{DockArea, DockContent, DockOutput, DockStyle, Permissions};
use egui::{Color32, Rect, Stroke, Ui, Window};

use zenman_core::store::config::PaneSlot;

use crate::app::ZenApp;
use crate::dock::{self, DockId};
use crate::theme::Tokens;

/// What a drop on a pane body should produce (plan row: `UiCommand::PasteInto`). The command bus
/// itself is step 9's write path; step 7 only names the event so the adapter surface is final.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DropIntent {
    /// A drag released over this pane's body — step 9 turns it into a paste plan.
    PasteInto { pane: PaneSlot },
}

/// One tab as the strip sees it: label, selected flag, group colour.
type TabStripEntry = (String, bool, Option<zenman_core::store::config::GroupColor>);

/// The mutable adapter `DockArea::show_content` calls into.
pub struct PaneDock<'a> {
    /// The shell state the bodies read (tab titles, empty pages).
    pub model: &'a mut crate::state::AppModel,
    /// The design tokens, so bodies never re-derive colours (the colour guard's other half).
    pub tokens: Tokens,
    /// Drops the body consumed this frame (drained by `show`).
    pub intents: Vec<DropIntent>,
}

impl<'a> DockContent<DockId> for PaneDock<'a> {
    fn body(&mut self, ui: &mut Ui, panel: &DockId) {
        self.pane(ui, dock::slot_of(panel));
    }

    fn logical_label(&self, panel: &DockId) -> String {
        let slot = dock::slot_of(panel);
        self.model
            .panes
            .get(&slot)
            .and_then(|pane| pane.active_tab())
            .map(|tab| tab.title.clone())
            .unwrap_or_else(|| format!("{slot:?} pane"))
    }

    fn permissions(&self, _panel: &DockId) -> Permissions {
        dock::pane_permissions()
    }

    fn limits(&self, _panel: &DockId) -> craft_ui::docking::PanelLimits {
        // Small but never zero: the empty pane still shows its strip, and the split tree must be
        // able to give every pane of the quad grid a real rect at the window minimum. Both numbers
        // are the token table's (plan §5.7: the geometry guard keeps them there).
        craft_ui::docking::PanelLimits {
            min: self.tokens.pane_min,
            max: egui::vec2(
                self.tokens.pane_unbounded_max,
                self.tokens.pane_unbounded_max,
            ),
        }
    }

    fn paint_icon(
        &mut self,
        _painter: &egui::Painter,
        _rect: Rect,
        panel: &DockId,
        _color: Color32,
    ) -> bool {
        // A stripless group shows no icon; returning false would fall back to the label initial
        // somewhere, and a pane has no dock-drawn icon at all.
        let _ = panel;
        true
    }

    fn resolve_drag(
        &mut self,
        _ctx: &egui::Context,
        _style: &DockStyle,
        request: &craft_ui::docking::DropRequest<'_, DockId>,
    ) -> Option<craft_ui::docking::DropFeedback> {
        // Step 7 has no host drags (no file rows yet), so a dock drag hitting the body is left to
        // the renderer, which will reject it against the panes' permissions. Step 8's file drag
        // claims it here and pushes `DropIntent::PasteInto`.
        let _ = request;
        None
    }
}

impl PaneDock<'_> {
    /// One empty pane: strip, toolbar, placeholder (plan §5.7 Done-when: "four panes render with no
    /// dock-drawn tab strip and no reserved header height").
    fn pane(&mut self, ui: &mut Ui, slot: PaneSlot) {
        let tokens = self.tokens;
        let active = slot == self.model.active_pane;
        let frame = egui::Frame::new().fill(tokens.bg_main).stroke(if active {
            Stroke::new(1.0, tokens.active_pane_border)
        } else {
            Stroke::new(1.0, tokens.border)
        });
        frame.show(ui, |ui| {
            let pane_has_tabs = self
                .model
                .panes
                .get(&slot)
                .is_some_and(|pane| !pane.tabs.is_empty());
            if pane_has_tabs {
                self.tab_strip(ui, slot);
            }
            ui.vertical_centered(|ui| {
                ui.add_space(tokens.empty_pane_space);
                let title = self
                    .model
                    .panes
                    .get(&slot)
                    .and_then(|pane| pane.active_tab())
                    .map(|tab| tab.title.clone())
                    .unwrap_or_else(|| placeholder_title(slot));
                ui.label(egui::RichText::new(title).color(tokens.text_main).strong());
                ui.label(
                    egui::RichText::new("empty pane — step 8 fills it with the listing")
                        .color(tokens.text_muted),
                );
            });
            // The pane takes focus on click (DockOutput::focused also reports it; both agree).
            if ui.input(|i| i.pointer.any_pressed())
                && ui
                    .input(|i| i.pointer.interact_pos())
                    .is_some_and(|p| ui.max_rect().contains(p))
            {
                self.model.active_pane = slot;
            }
        });
    }

    /// The coloured 38 px strip the pane owns *while its group is stripless* (plan §5.7: the dock
    /// reserves no header height for a protected single-panel group, which is what lets this live
    /// here). Step 8's `panels/tab_bar.rs` inherits this job and its `craft_ui::tabs::fit` policy.
    fn tab_strip(&mut self, ui: &mut Ui, slot: PaneSlot) {
        let tokens = self.tokens;
        let (active_ix, titles): (usize, Vec<TabStripEntry>) = {
            let pane = match self.model.panes.get(&slot) {
                Some(pane) => pane,
                None => return,
            };
            let titles = pane
                .tabs
                .iter()
                .enumerate()
                .map(|(ix, tab)| (tab.title.clone(), ix == pane.active, tab.group))
                .collect();
            (pane.active, titles)
        };
        let _ = active_ix;
        let (rect, response) = ui.allocate_at_least(
            egui::vec2(ui.available_width(), tokens.tab_bar_height),
            egui::Sense::hover(),
        );
        if !ui.is_rect_visible(rect) {
            return;
        }
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, tokens.bg_card);
        let mut x = rect.min.x + tokens.tab_gap;
        let budget = tokens.tab_max_width;
        for (ix, (title, selected, group)) in titles.iter().enumerate() {
            let width = (title.len() as f32 * tokens.tab_glyph_advance + tokens.tab_fixed_chrome)
                .min(budget);
            let tab = Rect::from_min_size(
                egui::pos2(x, rect.min.y + tokens.tab_vertical_inset),
                egui::vec2(width, rect.height() - 2.0 * tokens.tab_vertical_inset),
            );
            let fill = if *selected {
                tokens.bg_hover
            } else {
                tokens.transparent
            };
            painter.rect_filled(tab, tokens.tab_corner_radius, fill);
            if let Some(color) = group {
                painter.rect_filled(
                    Rect::from_min_size(tab.min, egui::vec2(tokens.accent_bar_width, tab.height())),
                    2.0,
                    tokens.group_color(*color),
                );
            }
            let label = truncate(title, budget - tokens.tab_label_reserve, &tokens);
            painter.text(
                Rect::from_min_size(
                    tab.min + egui::vec2(tokens.tab_label_indent, 0.0),
                    tab.size(),
                )
                .center(),
                egui::Align2::LEFT_CENTER,
                &label,
                tokens.font(),
                if *selected {
                    tokens.text_active
                } else {
                    tokens.text_muted
                },
            );
            x += width + tokens.tab_gap;
            let _ = ix;
        }
        let _ = response;
    }
}

/// Ellipsize a tab label to `max` points (the CSS `text-overflow: ellipsis` the strip needs).
/// The measure is the token font's nominal advance (`0.55 em`) — a deliberate approximation until
/// step 8's `panels/tab_bar.rs` replaces this with `craft_ui::tabs::fit` + galleys.
fn truncate(text: &str, max: f32, tokens: &Tokens) -> String {
    let advance = tokens.font_size * 0.55;
    let wide = advance * text.chars().count() as f32;
    if wide <= max {
        return text.to_string();
    }
    let keep = (max / advance).floor() as usize;
    let mut out: String = text.chars().take(keep.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn placeholder_title(slot: PaneSlot) -> String {
    match slot {
        PaneSlot::Left => "left".into(),
        PaneSlot::Right => "right".into(),
        PaneSlot::LeftBottom => "left · bottom".into(),
        PaneSlot::RightBottom => "right · bottom".into(),
    }
}

/// The per-frame dock draw (plan §5.7 `show`): build the style from tokens, render, then apply
/// every action the output carries and report its error — never silently.
///
/// The plan's `app.dialogs.blocks_dock()` / `app.overlay_rects()` arrive with step 10's modal
/// stack; until then nothing blocks the dock.
pub fn show(app: &mut ZenApp, ui: &mut Ui) {
    let tokens = app.tokens;
    let mut style = DockStyle::from_ui(ui);
    style.tab_height = tokens.tab_bar_height;
    style.gap = tokens.dock_gap;
    style.background = tokens.bg_main;
    style.tab_background = tokens.bg_sidebar;
    style.active_background = tokens.bg_card;
    style.text = tokens.text_main;
    style.inactive_text = tokens.text_muted;
    style.border = Stroke::new(1.0, tokens.border);
    style.accent = tokens.accent;
    style.font = tokens.font();
    style.floating_background = tokens.bg_card;

    let (out, mut dock_error, focus, intents) = {
        let mut content = PaneDock {
            model: &mut app.model,
            tokens,
            intents: Vec::new(),
        };
        let output: DockOutput<DockId> = DockArea::new(dock::dock_area_id()).show_content(
            ui,
            &app.dock.layout,
            &style,
            &mut content,
        );
        let DockOutput {
            actions,
            error,
            focused,
            ..
        } = output;
        (
            actions,
            error.map(|e| e.to_string()),
            focused.as_ref().map(dock::slot_of),
            std::mem::take(&mut content.intents),
        )
    };
    if let Some(slot) = focus {
        app.model.active_pane = slot;
    }
    if let Err(e) = app.dock.apply_all(out, dock::perms_of) {
        dock_error.get_or_insert_with(|| e.to_string());
    }
    if let Some(text) = dock_error {
        app.notices.post(crate::widgets::toast::Level::Error, text);
    }
    let _ = intents; // step 9's command bus drains these
}

/// The floating-window pass the plan row names (`send_keys(ctx, typing_window(ctx)?, events)`):
/// with the `native-windows` feature off (our build), `forwarded_events` is always empty, so this
/// is the honest no-op the framework itself implements — kept as the one call site.
pub fn forward_window_keys(ctx: &egui::Context, output: &DockOutput<DockId>) {
    if let (Some(viewport), false) = (
        craft_ui::docking::typing_window(ctx),
        output.forwarded_events.is_empty(),
    ) {
        craft_ui::docking::send_keys(ctx, viewport, output.forwarded_events.clone());
    }
}

/// A `Window` helper the drawers reuse (the plan's "absent from craft_ui: modal helper" — this is
/// the smallest thing that qualifies; step 10 grows it into `widgets/modals.rs`).
pub fn modal_frame(ui: &mut Ui, tokens: &Tokens) -> egui::Frame {
    let _ = ui;
    egui::Frame::new()
        .fill(tokens.bg_card)
        .stroke(Stroke::new(1.0, tokens.border))
        .corner_radius(egui::CornerRadius::same(tokens.corner_radius))
}

/// `Window` is named here so step 10's grep finds the intended import site; silence the unused
/// lint honestly (the drawers use it from step 8 on).
const _: Option<Window<'_>> = None;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Tab, TabId};
    use zenman_core::path::FsPath;

    #[test]
    fn truncate_ellipses_only_when_needed() {
        let tokens = Tokens::dark();
        assert_eq!(truncate("abc", 200.0, &tokens), "abc");
        let long = truncate(
            "a-really-long-directory-name-that-cannot-fit-150px",
            60.0,
            &tokens,
        );
        assert!(long.ends_with('…'), "{long}");
        assert!(long.chars().count() < 52);
        assert_eq!(truncate("exactly", 60.0, &tokens), "exactly");
    }

    #[test]
    fn placeholder_titles_are_distinct_per_slot() {
        let titles: Vec<String> = [
            PaneSlot::Left,
            PaneSlot::Right,
            PaneSlot::LeftBottom,
            PaneSlot::RightBottom,
        ]
        .iter()
        .map(|slot| placeholder_title(*slot))
        .collect();
        let unique: std::collections::HashSet<&String> = titles.iter().collect();
        assert_eq!(unique.len(), 4);
    }

    #[test]
    fn dock_id_round_trips_through_the_slot_helper() {
        let panel = DockId::FilePane(PaneSlot::RightBottom);
        assert_eq!(dock::slot_of(&panel), PaneSlot::RightBottom);
    }

    #[test]
    fn tab_title_survives_into_the_logical_label() {
        let mut model = crate::state::AppModel::new(zenman_core::store::config::LayoutMode::Single);
        let id = model.next_tab_id();
        model
            .pane_mut(PaneSlot::Left)
            .tabs
            .push(Tab::new(id, FsPath::ROOT, "downloads"));
        let content = PaneDock {
            model: &mut model,
            tokens: Tokens::dark(),
            intents: Vec::new(),
        };
        assert_eq!(
            content.logical_label(&DockId::FilePane(PaneSlot::Left)),
            "downloads"
        );
        assert!(
            content
                .permissions(&DockId::FilePane(PaneSlot::Left))
                .is_protected()
        );
        let _ = TabId(1);
    }
}
