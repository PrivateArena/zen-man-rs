//! The `DockContent` adapter (plan §5.7 `dock_content.rs` row).
//!
//! A pane body is the chrome it owns plus the listing: the **tab row** (the workspace button and the
//! strip — the dock draws no header of its own for a stripless group, §2.2), the **navigation bar**
//! (address strip and tool ribbon), then `panels::file_pane::show`. There is no window-wide header:
//! every control the shell has is either in a pane's chrome or in a drawer, which is what the
//! browser-shaped shell asked for and what the tab row's workspace button makes possible.

use craft_ui::docking::{DockArea, DockContent, DockOutput, DockStyle, Permissions};
use egui::{Align, Color32, Layout, Rect, Stroke, Ui, Window, pos2, vec2};

use zenman_core::session::Session;
use zenman_core::store::config::PaneSlot;

use crate::app::ZenApp;
use crate::dock::{self, DockId};
use crate::keymap::{Command, KeyMap};
use crate::panels::breadcrumb;
use crate::panels::file_pane::{self, PaneCommand, PaneModel};
use crate::panels::header::{self, HeaderCommand, WorkspaceBar};
use crate::panels::nav_bar::{self, NavBarHints, NavBarModel};
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
    /// The file drag in flight, cloned in so a body can offer its rows as drop targets (step 10c).
    pub drag: Option<crate::drag_drop::DragPayload>,
    /// The workspace control of the tab row: the session on screen, the stored ones, the autosave
    /// clock. Borrowed separately from `model` so the pane can drive it (plan §5.8 `header.rs`).
    pub workspaces: &'a mut WorkspaceBar,
    /// The keymap, for the chord text the ribbon's tooltips quote (plan **D7**).
    pub keys: &'a KeyMap,
    /// Free/total bytes of the data volume, for the workspace popup.
    pub disk: Option<(u64, u64)>,
    /// What the chrome asked the shell to do, drained by [`show`].
    pub header: Vec<HeaderCommand>,
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
    /// One pane: its tab row, its navigation bar, its listing (plan §5.7 `dock_content.rs`: the
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
            // The chrome rows sit flush: a browser's tab row, toolbar and column header are one
            // continuous surface, and the 4 px between them here is `item_spacing`, not design.
            ui.spacing_mut().item_spacing.y = 0.0;
            // One egui id space per pane: without this, the identically-ordered widgets of two
            // panes (back/forward/up, row rects) share one `Id`, and the second pane trips
            // `Context::check_for_id_clash` — whose flame glyph this font stack panics on (rule 11).
            ui.push_id(slot as u8, |ui| {
                let has_tabs = self
                    .model
                    .panes
                    .get(&slot)
                    .is_some_and(|pane| !pane.tabs.is_empty());
                // The chrome is drawn whether or not the pane has tabs: it is where the workspace
                // button, the drawers and the layout switches live, and a tab-less pane is exactly
                // the case that needs its `+`.
                self.tab_row(ui, slot);
                self.nav_row(ui, slot);
                if has_tabs {
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
            drag: self.drag.clone(),
        };
        let panel = DockId::FilePane(slot);
        let commands = file_pane::show(ui, &panel, &mut model, &tokens);
        self.commands
            .extend(commands.into_iter().map(|cmd| (slot, cmd)));
    }

    /// The navigation bar (plan §5.8 `breadcrumb.rs`, `nav_bar.rs`): the address strip and the icon
    /// ribbon, in the one row the pane owns.
    fn nav_row(&mut self, ui: &mut Ui, slot: PaneSlot) {
        let tokens = self.tokens;
        let quad = self.model.quad();
        let split = self.model.layout == zenman_core::store::config::LayoutMode::Split && !quad;
        let log_open = self.model.log_panel;
        let left_drawer = self.model.left_drawer.open;
        let right_drawer = self.model.right_drawer.open;
        let hints = NavBarHints {
            refresh: self.keys.hint_text(Command::Refresh),
            history: self.keys.hint_text(Command::ActionLog),
            split: self.keys.hint_text(Command::SplitLayout),
            quad: self.keys.hint_text(Command::QuadLayout),
        };
        // A pane without a tab has no path, no history and no listing mode — but its navigation bar
        // is still where the window's verbs live, so the bar is drawn with an empty address half.
        let mut address = None;
        let mut flat = crate::state::FlatMode::Off;
        let mut grid = false;
        if let Some(pane) = self.model.panes.get_mut(&slot) {
            let active = pane.active.min(pane.tabs.len().saturating_sub(1));
            let Pane {
                tabs,
                address_editing,
                address_buffer,
                ..
            } = pane;
            if let Some(tab) = tabs.get(active) {
                flat = tab.flat;
                grid = tab.view == zenman_core::store::config::ViewMode::Grid;
                address = Some(breadcrumb::BreadcrumbModel {
                    path: &tab.path,
                    can_back: tab.can_go_back(),
                    can_forward: tab.can_go_forward(),
                    editing: address_editing,
                    buffer: address_buffer,
                });
            }
        }
        let mut out = Vec::new();
        let mut global = Vec::new();
        let mut model = NavBarModel {
            address,
            flat,
            grid,
            quad,
            split,
            log_open,
            left_drawer,
            right_drawer,
            hints,
        };
        nav_bar::show(ui, &mut model, &tokens, &mut out, &mut global);
        self.commands.extend(out.into_iter().map(|cmd| (slot, cmd)));
        self.header.extend(global);
    }

    /// The row the pane's tabs and its workspace button share (plan §5.7: the dock reserves no
    /// header height for a protected single-panel group, which is what lets this live here).
    fn tab_row(&mut self, ui: &mut Ui, slot: PaneSlot) {
        let tokens = self.tokens;
        let whole = Rect::from_min_size(
            ui.cursor().min,
            vec2(ui.available_width(), tokens.tab_bar_height),
        );
        // The strip paints its own background from where its tabs start; the row behind the
        // workspace button is the same colour, so the pair reads as one bar.
        if ui.is_rect_visible(whole) {
            ui.painter().rect_filled(whole, 0.0, tokens.bg_sidebar);
        }
        // The button wants the width of its name; the strip needs the width of a tab. The tabs win.
        let (room, _) = tab_row_split(&tokens, whole.width());
        let button = header::button_width(ui, &tokens, &self.workspaces.selected).min(room);
        let button_rect = Rect::from_min_max(
            pos2(whole.min.x + tokens.chrome_pad_x, whole.min.y),
            pos2(whole.min.x + tokens.chrome_pad_x + button, whole.max.y),
        );
        let mut asked = Vec::new();
        crate::widgets::in_rect(
            ui,
            button_rect,
            Layout::left_to_right(Align::Center),
            |ui| {
                asked = self.workspaces.show(ui, &tokens, self.disk, button);
            },
        );
        self.header.extend(asked);
        let id = file_pane::pane_id(slot).with("strip");
        let (commands, overflow_open) = match self.model.panes.get(&slot) {
            Some(pane) => {
                let mut model = TabBarModel {
                    tabs: &pane.tabs,
                    active: pane.active,
                    overflow_open: pane.tabs_overflow_open,
                };
                let mut out = Vec::new();
                let strip = Rect::from_min_max(
                    pos2(button_rect.max.x + tokens.nav_group_gap, whole.min.y),
                    whole.max,
                );
                tab_bar::show(ui, id, strip, &mut model, &tokens, &mut out);
                (out, model.overflow_open)
            }
            // A slot the model does not have draws no tabs — but the row is still its height, or the
            // listing below would be painted over the chrome that is there.
            None => (Vec::new(), false),
        };
        if let Some(pane) = self.model.panes.get_mut(&slot) {
            pane.tabs_overflow_open = overflow_open;
        }
        for command in commands {
            match command {
                TabCommand::Select(ix) => self.commands.push((slot, PaneCommand::SelectTab(ix))),
                TabCommand::Close(ix) => self.commands.push((slot, PaneCommand::CloseTab(ix))),
                TabCommand::Context { ix, at } => {
                    self.commands
                        .push((slot, PaneCommand::TabContext { ix, at }));
                }
                TabCommand::NewTab => self.commands.push((slot, PaneCommand::NewTab)),
                TabCommand::ToggleOverflow => {
                    if let Some(pane) = self.model.panes.get_mut(&slot) {
                        pane.tabs_overflow_open = !pane.tabs_overflow_open;
                    }
                }
            }
        }
        // The row is over: the button, the strip and the `+` were all placed inside `whole`.
        ui.advance_cursor_after_rect(whole);
    }
}

/// How a tab row of `width` is split between the workspace control and the tab strip: the strip
/// keeps [`Tokens::tab_strip_min_width`] — one tab, its overflow button and the `+` — and the control
/// gets the rest, or its name, whichever is smaller. Past that the control ellipsises; the strip is
/// what the pane is for.
///
/// Returns `(room, strip)` — `room` is the control's band, `strip` what the tabs are left with.
#[must_use]
pub(crate) fn tab_row_split(tokens: &Tokens, width: f32) -> (f32, f32) {
    let chrome = tokens.chrome_pad_x * 2.0 + tokens.nav_group_gap;
    let room = (width - chrome - tokens.tab_strip_min_width).max(tokens.nav_button * 2.0);
    (room, (width - chrome - room).max(0.0))
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

    let (out, mut dock_error, focus, intents, commands, header) = {
        // One read of the config per frame, not one per pane: the popup's rows and the disk line are
        // the same data in both, and `Session::workspaces` clones the whole config to answer.
        app.workspaces.refresh(app.session.workspaces());
        let disk = zenman_core::path::AbsPath::from_path("/")
            .ok()
            .and_then(|root| app.session.env().disk_space(&root).ok())
            .map(|space| (space.free, space.total));
        let mut content = PaneDock {
            model: &mut app.model,
            session: &app.session,
            tokens,
            intents: Vec::new(),
            jobs: &app.jobs,
            commands: Vec::new(),
            drag: app.drag.clone(),
            workspaces: &mut app.workspaces,
            keys: &app.keys,
            disk,
            header: Vec::new(),
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
            std::mem::take(&mut content.header),
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
    // `DropIntent::PasteInto` is live: a drop the *dock* routed at a pane body (as opposed to the
    // claims the pane's own rows and background make, which arrive as `PaneCommand`s) lands in that
    // pane's directory.
    for intent in intents {
        match intent {
            DropIntent::PasteInto { pane } => app.drop_from_dock(pane),
        }
    }
    // The pane's commands are the shell's per-frame input (plan §4.1): they run *after* the draw,
    // so a navigation never mutates the state another pane is painting from.
    for (slot, command) in commands {
        app.run_command(slot, command);
    }
    // The chrome's commands are the same thing at window scope — the workspace verbs and the four
    // toggles of the ribbon. They run last, after the panes, because a layout switch reshapes the
    // grid the pane commands just listed into.
    app.header_commands(header);
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
    fn a_narrow_row_gives_its_tabs_the_floor_and_its_button_the_rest() {
        let tokens = Tokens::dark();
        // The smallest pane the dock accepts, and everything up from it: the strip never falls below
        // its floor, which is the point at which tabs stop being clickable.
        for width in [tokens.pane_min.x, 240.0, 400.0, 900.0, 1440.0] {
            let (room, strip) = tab_row_split(&tokens, width);
            assert!(
                strip >= tokens.tab_strip_min_width - tokens.nav_gap,
                "a row of {width} left the strip {strip} — below its floor"
            );
            assert!(
                room >= tokens.nav_button * 2.0,
                "a row of {width} left the workspace control {room} — not even its two icon boxes"
            );
            assert!(
                room + strip <= width,
                "a row of {width} allocated {room} + {strip}"
            );
        }
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
            drag: None,
            workspaces: &mut crate::panels::header::WorkspaceBar::default(),
            keys: &crate::keymap::KeyMap::default(),
            disk: None,
            header: Vec::new(),
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
