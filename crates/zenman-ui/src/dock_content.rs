//! The `DockContent` adapter (plan §5.7 `dock_content.rs` row).
//!
//! Step 7's `body` is the **empty pane**: the 38 px tab strip the pane owns (the dock draws none —
//! stripless groups, §2.2), a toolbar row, and the empty listing placeholder. Step 8 replaces the
//! placeholder with `panels::file_pane::show` (the virtual list); nothing else about the plumbing
//! changes, which is the point of the adapter seam.

use craft_ui::docking::{DockArea, DockContent, DockOutput, DockStyle, Permissions};
use egui::{Color32, Rect, Stroke, Ui, Window};

use zenman_core::session::Session;
use zenman_core::store::config::PaneSlot;

use crate::app::ZenApp;
use crate::dock::{self, DockId};
use crate::panels::breadcrumb;
use crate::panels::file_pane::{self, PaneCommand, PaneModel};
use crate::panels::tab_bar::{self, TabBarModel, TabCommand};
use crate::state::Pane;
use crate::theme::Tokens;

/// What a drop on a pane body should produce (plan row: `UiCommand::PasteInto`). The command bus
/// itself is step 9's write path; step 7 only names the event so the adapter surface is final.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DropIntent {
    /// A drag released over this pane's body — step 9 turns it into a paste plan.
    PasteInto { pane: PaneSlot },
}

/// The mutable adapter `DockArea::show_content` calls into.
pub struct PaneDock<'a> {
    /// The shell state the bodies read (tab titles, pages, selections).
    pub model: &'a mut crate::state::AppModel,
    /// The engine, for the size cache the directory rows paint from.
    pub session: &'a Session,
    /// The design tokens, so bodies never re-derive colours (the colour guard's other half).
    pub tokens: Tokens,
    /// Drops the body consumed this frame (drained by `show`).
    pub intents: Vec<DropIntent>,
    /// The shell's job slots, so a pane can say "busy" (plan row's `busy: &[JobSlot]`).
    pub jobs: &'a crate::jobs_ui::JobsUi,
    /// Commands the panes produced this frame, in `(pane, command)` order.
    pub commands: Vec<(PaneSlot, PaneCommand)>,
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
    /// One pane: its tab strip, its address strip, its listing (plan §5.7 `dock_content.rs`: the
    /// dock draws no header of its own, so all three live here).
    fn pane(&mut self, ui: &mut Ui, slot: PaneSlot) {
        let tokens = self.tokens;
        let active = slot == self.model.active_pane;
        let frame = egui::Frame::new().fill(tokens.bg_main).stroke(if active {
            Stroke::new(1.0, tokens.active_pane_border)
        } else {
            Stroke::new(1.0, tokens.border)
        });
        frame.show(ui, |ui| {
            let has_tabs = self
                .model
                .panes
                .get(&slot)
                .is_some_and(|pane| !pane.tabs.is_empty());
            if has_tabs {
                self.tab_strip(ui, slot);
                self.address_strip(ui, slot);
                self.listing(ui, slot);
            } else {
                ui.vertical_centered(|ui| {
                    ui.add_space(tokens.empty_pane_space);
                    ui.label(
                        egui::RichText::new(placeholder_title(slot))
                            .color(tokens.text_main)
                            .strong(),
                    );
                    ui.label(
                        egui::RichText::new("no tab — press + to open a directory")
                            .color(tokens.text_muted),
                    );
                });
            }
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

    /// The listing half of the pane (plan §5.8 `file_pane.rs`).
    fn listing(&mut self, ui: &mut Ui, slot: PaneSlot) {
        let tokens = self.tokens;
        let session = self.session;
        let jobs: Vec<crate::jobs_ui::JobSlot> = self
            .jobs
            .foreground
            .iter()
            .filter(|job| job.live && job.pane == Some(slot))
            .cloned()
            .collect();
        let Some(pane) = self.model.panes.get_mut(&slot) else {
            return;
        };
        let loading = !pane.loading.is_empty();
        let active = pane.active.min(pane.tabs.len().saturating_sub(1));
        let sizes = |path: &zenman_core::path::AbsPath| {
            session
                .cached_dir_sizes(std::slice::from_ref(path))
                .into_iter()
                .next()
                .flatten()
                .map(|stat| stat.sizes())
        };
        let Pane { tabs, .. } = pane;
        let Some(tab) = tabs.get_mut(active) else {
            return;
        };
        let mut model = PaneModel {
            slot,
            tab,
            loading,
            jobs: &jobs,
            sizes: &sizes,
            visible_rows: 0,
        };
        let panel = DockId::FilePane(slot);
        let commands = file_pane::show(ui, &panel, &mut model, &tokens);
        self.commands
            .extend(commands.into_iter().map(|cmd| (slot, cmd)));
    }

    /// The address strip (plan §5.8 `breadcrumb.rs`).
    fn address_strip(&mut self, ui: &mut Ui, slot: PaneSlot) {
        let tokens = self.tokens;
        let Some(pane) = self.model.panes.get_mut(&slot) else {
            return;
        };
        let active = pane.active.min(pane.tabs.len().saturating_sub(1));
        let Pane {
            tabs,
            address_editing,
            address_buffer,
            ..
        } = pane;
        let Some(tab) = tabs.get(active) else {
            return;
        };
        let can_back = tab.can_go_back();
        let can_forward = tab.can_go_forward();
        let mut out = Vec::new();
        let mut model = breadcrumb::BreadcrumbModel {
            path: &tab.path,
            can_back,
            can_forward,
            editing: address_editing,
            buffer: address_buffer,
        };
        breadcrumb::show(ui, &mut model, &tokens, &mut out);
        self.commands.extend(out.into_iter().map(|cmd| (slot, cmd)));
    }

    /// The coloured 38 px strip the pane owns *while its group is stripless* (plan §5.7: the dock
    /// reserves no header height for a protected single-panel group, which is what lets this live
    /// here). Step 8's `panels::tab_bar` is the implementation; this is the call site.
    fn tab_strip(&mut self, ui: &mut Ui, slot: PaneSlot) {
        let tokens = self.tokens;
        let id = file_pane::pane_id(slot).with("strip");
        let (commands, overflow_open) = {
            let Some(pane) = self.model.panes.get(&slot) else {
                return;
            };
            let mut model = TabBarModel {
                tabs: &pane.tabs,
                active: pane.active,
                overflow_open: pane.tabs_overflow_open,
            };
            let mut out = Vec::new();
            tab_bar::show(ui, id, &mut model, &tokens, &mut out);
            (out, model.overflow_open)
        };
        if let Some(pane) = self.model.panes.get_mut(&slot) {
            pane.tabs_overflow_open = overflow_open;
        }
        for command in commands {
            match command {
                TabCommand::Select(ix) => self.commands.push((slot, PaneCommand::SelectTab(ix))),
                TabCommand::Close(ix) => self.commands.push((slot, PaneCommand::CloseTab(ix))),
                TabCommand::NewTab => self.commands.push((slot, PaneCommand::NewTab)),
                TabCommand::ToggleOverflow => {
                    if let Some(pane) = self.model.panes.get_mut(&slot) {
                        pane.tabs_overflow_open = !pane.tabs_overflow_open;
                    }
                }
            }
        }
    }
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

    let (out, mut dock_error, focus, intents, commands) = {
        let mut content = PaneDock {
            model: &mut app.model,
            session: &app.session,
            tokens,
            intents: Vec::new(),
            jobs: &app.jobs,
            commands: Vec::new(),
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
            std::mem::take(&mut content.commands),
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
    let _ = intents; // step 9's paste path reads these (plan §5.7 `DropIntent::PasteInto`)
    // The pane's commands are the shell's per-frame input (plan §4.1): they run *after* the draw,
    // so a navigation never mutates the state another pane is painting from.
    for (slot, command) in commands {
        app.run_command(slot, command);
    }
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
        let session = crate::app::tests::memory_session();
        let jobs = crate::jobs_ui::JobsUi::default();
        let content = PaneDock {
            model: &mut model,
            session: &session,
            tokens: Tokens::dark(),
            intents: Vec::new(),
            jobs: &jobs,
            commands: Vec::new(),
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
