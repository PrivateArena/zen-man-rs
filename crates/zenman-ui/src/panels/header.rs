//! The workspace vocabulary of the pane chrome (plan §5.8 `panels/header.rs`, §9.2 step-8c review
//! gap 1, restyled for the browser-like shell).
//!
//! Go's header was `Workspace: <select>` · `New` · `Delete` · `Saved` · disk meter. That is now one
//! control: a button naming the session on screen, whose popup lists every stored workspace and
//! carries the two verbs of the selected one on its right edge (`Edit`, `Delete`) above the *New
//! workspace* row. Nothing here prints a chord any more — the navigation bar owns the buttons, and
//! [`crate::keymap::KeyMap::hint`] stays the one table for what a key is bound to (plan **D7**).
//!
//! # Who owns what
//!
//! [`WorkspaceBar`] owns the autosave state machine (`workspaces::Autosave`, step 14a) and the
//! `Saved` clock — the *clock*, not the pill: the tab row is the scarce real estate in the shell, so
//! what it reports is drawn by [`crate::panels::status_bar`] and this row reserves only the width of
//! the workspace name. The shell owns the engine. The bar never calls `Session` — it emits
//! [`HeaderCommand`]s, exactly like a pane emits [`crate::panels::file_pane::PaneCommand`], and the
//! `impl ZenApp` block at the bottom of this file is the only place that answers them.
//!
//! # Fitting at 800 px
//!
//! The plan's criterion is "every control reachable at 800 px", and one control is now narrow enough
//! to satisfy it by itself: the button ellipsises its label to the room it has, and the list, the
//! disk line and the two verbs live in a popup that is only laid out while it is open.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use egui::{
    Color32, CornerRadius, Frame, Label, Margin, Rect, RichText, ScrollArea, Stroke, Ui, pos2, vec2,
};

use zenman_core::error::CoreError;
use zenman_core::path::{FsPath, ResolveKind};
use zenman_core::services::workspaces::{Autosave, Pending, RESERVED, WorkspaceMeta};
use zenman_core::store::config::{
    GroupColor, LayoutMode, PaneSlot, TabGroupSession, ViewMode, Workspace,
};

use crate::app::ZenApp;
use crate::state::{GroupId, TabGroup};
use crate::theme::Tokens;
use crate::widgets::{disk_meter, toast};

/// What the workspace control asks the shell to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeaderCommand {
    /// Select a stored workspace: `Session::workspace_restore`.
    Restore(String),
    /// Ask for a name, then store the running session under it: see [`ZenApp::create_workspace_prompt`].
    Create,
    /// Store the running session under a name: `Session::workspace_save`.
    SaveNew(String),
    /// Ask for a new name for a stored workspace: see [`ZenApp::rename_workspace_prompt`].
    Rename(String),
    /// Move a stored workspace: `Session::workspace_rename`.
    RenameTo {
        /// The name it has now.
        from: String,
        /// The name the user typed.
        to: String,
    },
    /// Delete a stored workspace. Destructive, so the shell answers it with a `Confirm`.
    Delete(String),
    /// Show / hide the action-history slide-in (the navigation bar's `history` button).
    ToggleLog,
    /// Show / hide the places drawer.
    ToggleLeftDrawer,
    /// Show / hide the custom-actions drawer.
    ToggleRightDrawer,
    /// The `F3` chord's button: the two-pane grid, or the single pane.
    ToggleSplit,
    /// The `F4` chord's button: the four-pane grid.
    ToggleQuad,
    /// The zoom row's `+`: ten per cent bigger (`zoom.rs`). Window-wide, like the layout switches —
    /// the scale is the *window's*, not one pane's listing.
    ZoomIn,
    /// The zoom row's `−`: ten per cent smaller.
    ZoomOut,
    /// The zoom row's number: back to the native scale, the way <kbd>Ctrl</kbd>+<kbd>0</kbd> does.
    ZoomReset,
}

/// The pane chrome's workspace control: one button that names the session on screen, a popup that
/// lists every stored one, and the `Edit`/`Delete` verbs of the Edge workspace menu.
#[derive(Debug)]
pub struct WorkspaceBar {
    /// The autosave target — always the reserved name (Go's `autoSaveSession()` wrote `default`).
    pub target: String,
    /// The debounce/`Saved` clock, driven by the frame's `Instant` (14a: core cannot own a timer).
    pub autosave: Autosave,
    /// The stored workspaces, refreshed from the session whenever the panel is drawn.
    pub names: Vec<WorkspaceMeta>,
    /// What the button shows.
    pub selected: String,
    /// The dock tree carried into the next autosave (serialised only when the dock moved).
    pub dock: Option<serde_json::Value>,
}

impl Default for WorkspaceBar {
    fn default() -> Self {
        Self {
            target: RESERVED.to_string(),
            autosave: Autosave::default(),
            names: Vec::new(),
            selected: workspace_label(RESERVED),
            dock: None,
        }
    }
}

impl WorkspaceBar {
    /// Replace the dropdown's contents (from `Session::workspaces`).
    pub fn refresh(&mut self, names: Vec<WorkspaceMeta>) {
        self.names = names;
    }

    /// Note the state the shell wants persisted (the frame tick that also drives
    /// [`Persisted::autosave`](crate::persist::Persisted::autosave)).
    pub fn note(&mut self, session: &Workspace, now: Instant) {
        self.autosave.note(&self.target, session, now);
    }

    /// The outstanding write, once its debounce has elapsed.
    pub fn take_due(&mut self, now: Instant) -> Option<Pending> {
        self.autosave.take_due(now)
    }

    /// A write landed: start the `Saved` chip's clock.
    pub fn mark_saved(&mut self, now: Instant) {
        self.autosave.mark_saved(now);
    }

    /// Light the chip for a write that was not the autosave's (see [`Autosave::show_chip`]).
    pub fn show_chip(&mut self, now: Instant) {
        self.autosave.show_chip(now);
    }

    /// Forget the outstanding write (a restore took the pane set away from it).
    pub fn discard(&mut self) {
        self.autosave.discard();
    }

    /// The outstanding write, debounce ignored (see [`ZenApp::flush_session`]).
    pub fn take_pending(&mut self) -> Option<Pending> {
        self.autosave.take_pending()
    }

    /// Whether the `Saved` pill shows (2 s after each write, Go's `_showSaveIndicator`). The shell
    /// hands the answer to the status bar, which is where the pill is drawn.
    #[must_use]
    pub fn chip_shows(&self, now: Instant) -> bool {
        self.autosave.show_saved(now)
    }

    /// The popup's rows: every stored name, with the reserved one *flagged*, not hidden (Go
    /// filtered `default` out and thereby made a session saved as `Default` unrestorable), and the
    /// tab count that makes a row recognisable before it is clicked.
    #[must_use]
    pub fn labels(&self) -> Vec<(String, String, usize)> {
        self.names
            .iter()
            .map(|meta| {
                let label = if meta.reserved {
                    format!("{} (autosave)", meta.name)
                } else {
                    meta.name.clone()
                };
                (meta.name.clone(), label, meta.tabs)
            })
            .collect()
    }

    /// Draw the button and (while it is open) its popup; report what was asked.
    ///
    /// One control where Go had four (`select` + `New` + `Delete` + the autosave text): the button
    /// names the session on screen, the popup lists every stored one, and the two verbs of the
    /// selected workspace ride the popup's right edge, which is where a browser puts them. The
    /// autosave text is no longer here at all — see [`crate::panels::status_bar::StatusModel::saved`].
    pub fn show(
        &mut self,
        ui: &mut Ui,
        tokens: &Tokens,
        disk: Option<(u64, u64)>,
        room: f32,
    ) -> Vec<HeaderCommand> {
        let mut out = Vec::new();
        let rows = self.labels();
        // The rows carry the display label (the reserved one flagged, as on the button); the verbs
        // act on the *stored* name, which is what `Session` looks up.
        let current = self.selected.clone();
        let target = self.selected_name();
        let mut asked: Option<HeaderCommand> = None;
        let response = toggle(ui, tokens, &self.selected, room);
        egui::Popup::menu(&response)
            .width(tokens.workspace_select_width)
            .show(|ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Workspaces")
                            .strong()
                            .color(tokens.text_main)
                            .size(tokens.font_size),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if crate::panels::nav_bar::icon_button(
                            ui,
                            tokens,
                            "trash",
                            "Delete workspace",
                        )
                        .clicked()
                        {
                            asked.get_or_insert(HeaderCommand::Delete(target.clone()));
                        }
                        if crate::panels::nav_bar::icon_button(
                            ui,
                            tokens,
                            "edit",
                            "Rename workspace",
                        )
                        .clicked()
                        {
                            asked.get_or_insert(HeaderCommand::Rename(target));
                        }
                    });
                });
                ui.separator();
                ScrollArea::vertical()
                    .max_height(tokens.log_row_height * tokens.dropdown_visible_rows)
                    .show(ui, |ui| {
                        for (name, label, tabs) in &rows {
                            ui.horizontal(|ui| {
                                let shown = ui.selectable_label(*label == current, label.clone());
                                let clicked = shown.clicked();
                                // The count is what tells two workspaces apart before clicking
                                // either — Go's `select` had nothing but the name to go on.
                                ui.label(
                                    RichText::new(format!("{tabs} tab(s)"))
                                        .color(tokens.text_muted)
                                        .size(tokens.font_size - 2.0),
                                );
                                if clicked && *label != current {
                                    asked.get_or_insert(HeaderCommand::Restore(name.clone()));
                                }
                            });
                        }
                    });
                ui.separator();
                if ui
                    .button(RichText::new("New workspace").color(tokens.text_main))
                    .clicked()
                {
                    asked.get_or_insert(HeaderCommand::Create);
                }
                if let Some((free, total)) = disk {
                    ui.label(
                        RichText::new(format!(
                            "{} free of {}",
                            disk_meter::format_size(free),
                            disk_meter::format_size(total)
                        ))
                        .color(tokens.text_muted)
                        .size(tokens.font_size - 2.0),
                    );
                }
            });
        if let Some(command) = asked {
            out.push(command);
        }
        out
    }

    /// The stored name behind [`Self::selected`] — the same string the button prints, minus the
    /// reserved row's `(autosave)` flag.
    #[must_use]
    pub fn selected_name(&self) -> String {
        self.names
            .iter()
            .find(|meta| workspace_label(&meta.name) == self.selected)
            .map_or_else(|| self.selected.clone(), |meta| meta.name.clone())
    }
}

/// The width of the collapsed control itself: two icon boxes, the name, and a gap on either side of
/// the name — the same arithmetic [`toggle`] lays out with, which is what lets the name print in full
/// rather than ellipsising against a reservation that was counted differently.
fn toggle_width(ui: &Ui, tokens: &Tokens, label: &str) -> f32 {
    tokens.nav_button * 2.0 + tokens.nav_gap * 2.0 + label_width(ui, tokens, label)
}

/// The name's own width at the size the button paints it.
fn label_width(ui: &Ui, tokens: &Tokens, label: &str) -> f32 {
    ui.painter()
        .layout_no_wrap(label.to_owned(), tokens.font(), tokens.text_main)
        .size()
        .x
}

/// The room the whole control wants in the row: the button and the row's inset.
///
/// No pill is counted here since the `Saved` report moved to the status bar, which is what gives the
/// tab strip back its `Saved`-pill width.
#[must_use]
pub fn button_width(ui: &Ui, tokens: &Tokens, label: &str) -> f32 {
    toggle_width(ui, tokens, label) + tokens.chrome_pad_x
}

/// What [`chip`] needs, including the gap the row's own layout puts in front of it.
#[must_use]
pub(crate) fn chip_room(ui: &Ui, tokens: &Tokens, text: &str) -> f32 {
    chip_width(ui, tokens, text) + ui.spacing().item_spacing.x
}

/// The button's name, painted rather than allocated: an interaction rect laid over the button's own
/// *after* it takes the click (rule 10, plan §9.2 step 8 — the last-allocated overlapping rect wins),
/// which is why the address bar's chips are painted the same way (`breadcrumb::paint_chip_label`).
fn paint_clipped(ui: &Ui, rect: Rect, text: &str, tokens: &Tokens) {
    // The slot is exactly the name's width at the size it is painted, so the common case — a wide
    // pane, a short name — prints the whole thing and the ellipsis is only for a clamped button.
    let width = rect.width().max(0.0);
    if width <= 0.0 || text.is_empty() {
        return;
    }
    let mut job =
        egui::text::LayoutJob::simple(text.to_owned(), tokens.font(), tokens.text_main, width);
    job.wrap = egui::text::TextWrapping {
        max_width: width,
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    let galley = ui.painter().layout_job(job);
    ui.painter().galley(
        pos2(rect.min.x, rect.center().y - galley.size().y / 2.0),
        galley,
        tokens.text_main,
    );
}

/// How wide a pill is: its text and the frame's own two margins.
fn chip_width(ui: &Ui, tokens: &Tokens, text: &str) -> f32 {
    let font = egui::FontId::proportional(tokens.font_size - 1.0);
    ui.painter()
        .layout_no_wrap(text.to_owned(), font, tokens.text_main)
        .size()
        .x
        + tokens.chip_pad * 2.0
}

/// The collapsed workspace button: the stack glyph, the name of the session on screen, and the
/// chevron that says there is a list behind it.
///
/// It takes the width its name wants, or the `room` the row can spare — whichever is smaller, never
/// less than the two icon boxes — and the name ellipsises inside that ([`paint_clipped`]). The tabs
/// are the subject of the row; the button is a guest in it.
fn toggle(ui: &mut Ui, tokens: &Tokens, label: &str, room: f32) -> egui::Response {
    let want = toggle_width(ui, tokens, label);
    let width = want
        .min(room)
        .min(ui.available_width())
        .max(tokens.nav_button * 2.0);
    let height = tokens.nav_button;
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let radius = egui::CornerRadius::same(tokens.crumb_corner_radius);
        let fill = if response.is_pointer_button_down_on() {
            tokens.bg_hover
        } else if response.hovered() {
            tokens.bg_card
        } else {
            tokens.transparent
        };
        ui.painter().rect_filled(rect, radius, fill);
        if response.hovered() {
            ui.painter()
                .rect_stroke(rect, radius, tokens.hairline(), egui::StrokeKind::Inside);
        }
        let box_y = rect.center().y - tokens.nav_button / 2.0;
        let icon = Rect::from_min_size(
            pos2(rect.min.x, box_y),
            vec2(tokens.nav_button, tokens.nav_button),
        );
        crate::icons::paint(
            ui,
            icon,
            "workspace",
            tokens.row_icon_size,
            tokens.text_muted,
        );
        let chevron = Rect::from_min_size(
            pos2(rect.max.x - tokens.nav_button, box_y),
            vec2(tokens.nav_button, tokens.nav_button),
        );
        crate::icons::paint(
            ui,
            chevron,
            "chevron-down",
            tokens.row_icon_size,
            tokens.text_muted,
        );
        let text = Rect::from_min_max(
            pos2(icon.max.x + tokens.nav_gap, rect.min.y),
            pos2((chevron.min.x - tokens.nav_gap).max(icon.max.x), rect.max.y),
        );
        paint_clipped(ui, text, label, tokens);
    }
    let response = response.on_hover_text("Workspaces: switch, rename or delete a saved session");
    response.widget_info(|| {
        let mut info = egui::WidgetInfo::new(egui::Role::Button);
        info.label = Some("workspace".into());
        info
    });
    response
}

/// A small status pill: the only pill-shaped thing the chrome owns, drawn into a caller's cell —
/// today the status bar's, for the `Saved` report.
///
/// The text is a real `Label` (a painted string would be invisible to the accessibility tree, and
/// to every test), and the pill is its `Frame` — the frame itself allocates nothing interactive.
pub(crate) fn chip(ui: &mut Ui, tokens: &Tokens, text: &str, colour: Color32) {
    Frame::new()
        .stroke(Stroke::new(1.0, colour))
        .corner_radius(CornerRadius::same(tokens.corner_radius / 2))
        .inner_margin(Margin::same(tokens.chip_pad as i8))
        .show(ui, |ui| {
            ui.add(
                Label::new(
                    RichText::new(text)
                        .color(colour)
                        .size(tokens.font_size - 1.0),
                )
                // Chrome text: a selectable pill would highlight when clicked and take the click off
                // whatever the pill is reporting on (`tab_bar`'s title says the rest).
                .selectable(false),
            );
        });
}

// ------------------------------------------------------------------ the shell's answers

impl ZenApp {
    /// The session the autosave writes: every visible pane's tabs, in order, plus the dock tree.
    #[must_use]
    pub fn workspace_session(&self) -> Workspace {
        let mut panes = std::collections::BTreeMap::new();
        for (slot, pane) in &self.model.panes {
            let tabs = pane
                .tabs
                .iter()
                .map(|tab| zenman_core::store::config::TabSession {
                    id: format!("tab-{}", tab.id.0),
                    // The **store** string, not the host path: `Display`/`FsPath::resolve` round-trip
                    // an archive member (`/a/b.zip/inner`), while `host_abs()` answers its *container*,
                    // so a tab opened on a member directory would reload as a tab on the zip root.
                    path: tab.path.to_string(),
                    name: tab.title.clone(),
                    // Both halves of the membership: `group_id` is this app's group, `group` is Go's
                    // letter for the same thing, written from the group's colour so a Go re-read still
                    // sees the accent it wrote (design §4).
                    group: pane.tab_color(tab.group),
                    group_id: tab.group.map(|id| id.to_store()),
                    view: tab.view,
                })
                .collect::<Vec<_>>();
            let groups = pane
                .groups
                .iter()
                .map(|group| zenman_core::store::config::TabGroupSession {
                    id: group.id.to_store(),
                    name: group.name.clone(),
                    group: Some(group.color),
                    collapsed: group.collapsed,
                })
                .collect::<Vec<_>>();
            panes.insert(
                *slot,
                zenman_core::store::config::PaneSession {
                    tabs,
                    active: pane.active,
                    groups,
                },
            );
        }
        Workspace {
            panes,
            active: self.model.active_pane,
            layout: self.model.layout,
            dock: self.workspaces.dock.clone(),
        }
    }

    /// The frame tick: note the wanted state, write it when the debounce elapses, and let the chip
    /// clock run down on its own (`SAVED_INDICATOR_TTL`, 14a's single source for both constants).
    pub fn tick_workspaces(&mut self, now: Instant, dock_moved: bool) {
        if self.workspaces.dock.is_none() || dock_moved {
            self.workspaces.dock = crate::dock::snapshot(&self.dock).ok();
        }
        let session = self.workspace_session();
        self.workspaces.note(&session, now);
        let Some(pending) = self.workspaces.take_due(now) else {
            return;
        };
        match self.session.workspace_save(&pending.name, &pending.session) {
            Ok(()) => {
                self.workspaces.mark_saved(now);
                self.workspaces.refresh(self.session.workspaces());
            }
            Err(e) => self.notices.post(
                toast::Level::Error,
                format!("workspace autosave: {}", e.user_message()),
            ),
        }
    }
    /// Answer what the workspace control and the navigation ribbon asked for.
    pub fn header_commands(&mut self, commands: Vec<HeaderCommand>) {
        for command in commands {
            match command {
                HeaderCommand::Restore(name) => self.restore_workspace(&name),
                HeaderCommand::Create => self.create_workspace_prompt(),
                HeaderCommand::SaveNew(name) => self.save_workspace_as(&name),
                HeaderCommand::Rename(name) => self.rename_workspace_prompt(&name),
                HeaderCommand::RenameTo { from, to } => self.rename_workspace(&from, &to),
                HeaderCommand::Delete(name) => self.confirm_delete_workspace(&name),
                HeaderCommand::ToggleLog => self.toggle_log_panel(),
                HeaderCommand::ToggleLeftDrawer => {
                    self.model.left_drawer.open = !self.model.left_drawer.open;
                }
                HeaderCommand::ToggleRightDrawer => {
                    self.model.right_drawer.open = !self.model.right_drawer.open;
                }
                HeaderCommand::ToggleSplit => self.toggle_split_layout(),
                HeaderCommand::ToggleQuad => self.toggle_quad_layout(),
                HeaderCommand::ZoomIn => self.zoom.zoom_in(),
                HeaderCommand::ZoomOut => self.zoom.zoom_out(),
                HeaderCommand::ZoomReset => self.zoom.reset(),
            }
        }
    }

    /// The zoom level `config.json` does not have yet, written now — **debounce ignored**, the same
    /// bargain [`Self::flush_session`] makes for the workspace: a quit inside the 1.5 s window used to
    /// lose the level, and the level is the one setting the next boot is judged by.
    ///
    /// No-ops when the file is already current, so the shutdown path can call it unconditionally.
    ///
    /// # Errors
    /// The config write's own failure, in words.
    pub fn flush_zoom(&mut self) -> Result<(), String> {
        let Some(percent) = self.zoom.pending() else {
            return Ok(());
        };
        self.session
            .set_zoom_percent(percent)
            .map_err(|e| format!("zoom {percent}%: {}", e.user_message()))
    }

    /// Write whatever the autosave is still holding, **ignoring the debounce** (window close, and the
    /// smoke run's self-close): the pending workspace, and the pending zoom level ([`Self::flush_zoom`]).
    ///
    /// Go had no such path: its only save was the 1.5 s `setTimeout` and nothing ran on exit, so a
    /// quit inside the window lost the tab opened, closed or navigated in the last second and a half.
    /// Tolerable for a divider position, not for the session the app now reloads at boot — which is
    /// why this is a separate call from [`tick_workspaces`](Self::tick_workspaces) rather than a
    /// shorter debounce. The write itself is atomic (`ZenConfig::save`, tmp+rename), so a flush that
    /// is interrupted leaves the *previous* session readable.
    ///
    /// # Errors
    /// The config write's own failure, in words: the only caller is a shutdown path, which prints.
    pub fn flush_session(&mut self, now: Instant) -> Result<(), String> {
        // Both halves get their attempt, and both are reported: a zoom write that fails must not cost
        // the user the session the next boot reloads (and the other way round), which is why this
        // collects rather than chains with `?`.
        let mut failures = Vec::new();
        if let Err(e) = self.flush_zoom() {
            failures.push(e);
        }
        let Some(pending) = self.workspaces.take_pending() else {
            return failures.pop().map_or(Ok(()), Err);
        };
        match self.session.workspace_save(&pending.name, &pending.session) {
            Ok(()) => {
                self.workspaces.mark_saved(now);
                self.workspaces.refresh(self.session.workspaces());
                failures.pop().map_or(Ok(()), Err)
            }
            Err(e) => {
                failures.push(format!("workspace {}: {}", pending.name, e.user_message()));
                Err(failures.join("; "))
            }
        }
    }

    /// Boot restore: the session the last run autosaved (Go's `restoreLastWorkspace`, `app.js:61` +
    /// `workspace.js:179-215`, which is why a restarted zen-man came back with its tabs).
    ///
    /// Only the autosave target is restored, because that is what "the tabs I had" means: waking up
    /// in some named workspace the user selected once and moved on from is a surprise window. The
    /// report says what came back — or what could not — and nothing here is silent.
    #[must_use]
    pub fn restore_last_session(&mut self) -> SessionRestore {
        match self.session.workspace_restore(RESERVED) {
            Err(CoreError::NotFound { .. }) => SessionRestore::Missing,
            Err(e) => SessionRestore::Failed(e.user_message().to_string()),
            Ok(workspace) => {
                let applied = self.apply_workspace(&workspace);
                self.workspaces.selected = workspace_label(RESERVED);
                self.note_lost_tabs(&applied);
                SessionRestore::Restored(applied)
            }
        }
    }

    /// Say what a restore could not bring back. Every path that applies a stored session — the boot
    /// autosave, `--session`, and the popup's rows — goes through this, so a lost tab is never a
    /// line only one of them prints.
    pub fn note_lost_tabs(&mut self, applied: &Applied) {
        if applied.lost() == 0 {
            return;
        }
        self.notices.post(
            toast::Level::Warning,
            format!(
                "session: {} tab(s) restored, {} lost: {} unreadable path(s), {} in an unopenable pane",
                applied.tabs,
                applied.lost(),
                applied.skipped,
                applied.dropped
            ),
        );
    }

    /// `Session::workspace_restore`, applied: layout, then one tab per stored tab, then a listing.
    pub fn restore_workspace(&mut self, name: &str) {
        match self.session.workspace_restore(name) {
            Ok(workspace) => {
                // A pending write of the *old* workspace must not land under the new name.
                self.workspaces.discard();
                self.workspaces.selected = workspace_label(name);
                let applied = self.apply_workspace(&workspace);
                self.notices
                    .post(toast::Level::Success, format!("workspace {name}: restored"));
                if applied.skipped > 0 {
                    self.notices.post(
                        toast::Level::Warning,
                        format!(
                            "workspace {name}: {} stored tab(s) had a path this build cannot read",
                            applied.skipped
                        ),
                    );
                }
            }
            Err(e) => self.notices.post(
                toast::Level::Error,
                format!("workspace {name}: {}", e.user_message()),
            ),
        }
    }

    /// Replace the grid with a stored workspace (Go's `restoreSession`: tabs, then the active pane).
    ///
    /// Three questions the stored file answers that a bare `layout` cannot, and which the first
    /// step-14b cut of this function did not ask:
    ///
    /// * **which panes** — a quad session names `LeftBottom`/`RightBottom`, and `LayoutMode` has no
    ///   quad spelling ([`crate::state::AppModel::quad`] derives it from the pane map), so the slot
    ///   set comes from the map's keys — and only from them: `store::legacy` records an *empty*
    ///   `right` pane for every Go `split:false` session, so widening on a named-but-empty pane would
    ///   turn every migrated single-pane session into a split one, permanently (see [`session_slots`]);
    /// * **how the panes were divided** — [`Workspace::dock`] carries the divider positions the user
    ///   dragged, and is reconciled against those slots rather than trusted;
    /// * **what each tab looked like** — [`TabSession`] has always stored `group` and `view`; a tab
    ///   that comes back in a different view mode is a different tab.
    ///
    /// A stored path that is not absolute is **skipped and counted** rather than opened: Go resolved
    /// a relative session entry against the *server's* cwd (`api/dir.go:39-44`), which this app has
    /// no honest equivalent for. A path that is absolute but gone is **kept** — the tab lists an error,
    /// which is what Go did and what the user expects to see ("where was that folder?"), and it is
    /// the reason [`TabSession::path`] is a raw string instead of a validated path type.
    #[must_use]
    pub fn apply_workspace(&mut self, workspace: &Workspace) -> Applied {
        let (slots, dropped) = session_slots(workspace);
        // The dock may be left alone only when it already has exactly this pane set — and that is two
        // questions, not one. The model's panes and the dock's own leaves can disagree at this point:
        // `Persisted::restore` installs the settings store's tree without reshaping the model, so a
        // four-leaf tree from a quad run can be sitting over the two panes the mode spells. Rebuilding
        // is then the repair; leaving it would paint panels with no pane behind them.
        //
        // When they do agree, rebuilding is the loss: at boot the tree in place is the one
        // `Persisted::restore` put there a moment ago from the shell settings, divider drags included,
        // and `set_layout` would stamp half-and-half ratios over it — which the next autosave writes
        // back, so the user could never get them again.
        let want: std::collections::BTreeSet<PaneSlot> = slots.iter().copied().collect();
        let grid: std::collections::BTreeSet<PaneSlot> = self.model.panes.keys().copied().collect();
        let shaped = grid == want && crate::dock::panels(&self.dock) == slots;
        // The mode follows the grid, which is [`crate::dock::set_layout`]'s own rule — taking it from
        // the file instead would leave `layout_mode_is_consistent` false on the branch below.
        self.model.layout = if slots.len() >= 2 {
            LayoutMode::Split
        } else {
            LayoutMode::Single
        };
        if !shaped {
            crate::dock::set_layout(&mut self.model, &mut self.dock, slots.clone());
        }
        let mut applied = Applied {
            panes: slots.len(),
            dropped,
            ..Default::default()
        };
        for slot in &slots {
            let Some(pane_session) = workspace.panes.get(slot) else {
                continue;
            };
            // Design §4 step 1: the **rows** first, keyed by their stored id string, so a tab can be
            // resolved against them and so a clash keeps the first row rather than the last.
            let (mut groups, ids) = stored_groups(&mut self.model, &pane_session.groups);
            let mut tabs: Vec<crate::state::Tab> = Vec::with_capacity(pane_session.tabs.len());
            // The letters of the tabs that came out memberless — design §4 step 2's fallback list, in
            // strip order and `None` wherever a `group_id` resolved.
            let mut orphan_letters: Vec<Option<GroupColor>> = Vec::new();
            for stored in &pane_session.tabs {
                let Some(path) = stored_path(&stored.path) else {
                    applied.skipped += 1;
                    orphan_letters.push(None);
                    continue;
                };
                let id = self.model.next_tab_id();
                let title = if stored.name.is_empty() {
                    tab_title(&path)
                } else {
                    stored.name.clone()
                };
                let mut tab = crate::state::Tab::new(id, path, title);
                // Step 2: `groupId` wins. A tab naming a row that is **gone** falls through to the
                // letter rather than losing its membership, because `group` still says what colour it
                // was in — that is the whole reason both keys are written.
                let member = stored
                    .group_id
                    .as_deref()
                    .and_then(|raw| ids.get(raw))
                    .copied();
                tab.group = member;
                orphan_letters.push(if member.is_none() { stored.group } else { None });
                // `view` is the *persisted* answer and `flat` the live one (plan §5.8's `state.rs`
                // row): a stored `Flat` cannot say which of the three flat modes it was, so it comes
                // back as Go's own default through `FlatMode::from_view`.
                tab.flat = crate::state::FlatMode::from_view(stored.view);
                tab.view = if tab.flat.is_flat() {
                    ViewMode::Flat
                } else {
                    stored.view
                };
                tabs.push(tab);
            }
            // Step 3: a session written before tab groups (or by Go, which only ever had the letter)
            // becomes one unnamed, expanded group per contiguous run of one letter. After the rows, so
            // that a repair can never steal a colour a real row holds.
            for (start, end, color) in legacy_runs(&orphan_letters) {
                let id = self.model.next_group_id();
                for tab in &mut tabs[start..end] {
                    tab.group = Some(id);
                }
                groups.push(TabGroup::new(id, color));
            }
            applied.tabs += tabs.len();
            let shown = pane_session.active.min(tabs.len().saturating_sub(1));
            let order: Vec<crate::state::TabId> = tabs.iter().map(|tab| tab.id).collect();
            let wanted = order.get(shown).copied();
            if let Some(pane) = self.model.panes.get_mut(slot) {
                pane.tabs = tabs;
                pane.groups = groups;
                pane.mru.clear();
                // The MRU ring (`Alt+Z`, step 10) is part of the pane's state: an empty one makes that
                // chord do nothing on a pane that plainly has tabs in it. Touched in strip order, the
                // shown tab last so it lands at the front.
                for id in &order {
                    pane.touch_mru(*id);
                }
                if let Some(id) = wanted {
                    pane.touch_mru(id);
                }
                // Step 4: contiguity, then the empty rows, then the shown tab **by id** — compacting
                // moves tabs, so the index the file stored no longer names it.
                pane.compact();
                pane.prune_groups();
                pane.active = wanted
                    .and_then(|id| pane.tabs.iter().position(|tab| tab.id == id))
                    .unwrap_or(0);
                pane.tab_sel = wanted.into_iter().collect();
                pane.tab_sel_anchor = wanted;
                pane.debug_assert_invariants();
            }
            let shown = self
                .model
                .panes
                .get(slot)
                .map_or(0, |pane| pane.tabs.len().min(pane.active));
            // Only the tab that is on screen is read here, and it is the only synchronous read this
            // path makes. The rest of the strip is queued for the next frame's `reload_stale`: Go did
            // the same (`createTab(…, isLazy)`, `workspace.js:32-35`), and `list_at` opens an archive
            // store inline — which, from inside the `run_native` creator, would put every container
            // of the session between the launch and the first frame.
            if order.get(shown).is_some() {
                self.list_at(*slot, shown, false, false);
            }
            for id in order
                .iter()
                .filter(|id| Some(**id) != order.get(shown).copied())
            {
                self.model.queue_reload(*id);
            }
        }
        applied.dock = self.apply_session_dock(workspace, &slots);
        self.model.active_pane = if slots.contains(&workspace.active) {
            workspace.active
        } else {
            slots.first().copied().unwrap_or(PaneSlot::Left)
        };
        // Go restores a workspace and then runs `updateLayout` (`workspace.js:75-79` → the
        // `setSplitView`/`setQuadView` it calls), which seeds any pane the session left with no tab.
        // A pane named by the file with an empty tab list is exactly that case.
        self.fill_empty_panes();
        applied
    }

    /// Install the workspace's own dock tree, reconciled against the panes the session registers
    /// (plan **F17**: a saved layout is validated before it reaches the first paint, and a refusal
    /// is reported). `false` when the session carried no tree or the tree was refused.
    ///
    /// The predicate is the session's own slot set, not "is this a file pane": with the wider answer
    /// `reconcile` demoted nothing, and a four-leaf tree from a stale file would paint three panels
    /// that have no pane behind them (`dock_content` correctly declines to draw those) while the model
    /// believed the session's one or two panes.
    fn apply_session_dock(&mut self, workspace: &Workspace, slots: &[PaneSlot]) -> bool {
        let Some(value) = workspace.dock.as_ref() else {
            return false;
        };
        let registered = |panel: &crate::dock::DockId| slots.contains(&crate::dock::slot_of(panel));
        let default = crate::dock::Dock::new(crate::dock::layout_for_slots(slots));
        match crate::dock::apply(&mut self.dock, value, &registered, &default) {
            crate::dock::ApplyReport::Applied => true,
            crate::dock::ApplyReport::NewerVersion { found } => {
                self.notices.post(
                    toast::Level::Warning,
                    format!(
                        "saved layout is from a newer zenman (dock v{found}); using the default"
                    ),
                );
                false
            }
            crate::dock::ApplyReport::Invalid => {
                self.notices.post(
                    toast::Level::Warning,
                    "saved layout was unreadable; using the default",
                );
                false
            }
        }
    }

    /// `Session::workspace_save` under a name the user typed (`New`).
    pub fn save_workspace_as(&mut self, name: &str) {
        let session = self.workspace_session();
        match self.session.workspace_save(name, &session) {
            Ok(()) => {
                let now = Instant::now();
                // The chip, not the autosave's memory: this wrote under a name the autosave does not
                // own, so it must not confirm the outstanding `default` write (that one lands or fails
                // on its own report).
                self.workspaces.show_chip(now);
                self.workspaces.selected = workspace_label(name);
                self.workspaces.refresh(self.session.workspaces());
                self.notices
                    .post(toast::Level::Success, format!("workspace {name}: saved"));
            }
            Err(e) => self.notices.post(
                toast::Level::Error,
                format!("workspace {name}: {}", e.user_message()),
            ),
        }
    }

    /// The popup's *New workspace* row: a name first, then the write (the reference asked for the
    /// name with `window.prompt`, so a modal is the parity answer, not an invention).
    pub fn create_workspace_prompt(&mut self) {
        self.open_dialog(Dialog::Prompt(Prompt::new(
            DialogKind::NewWorkspace,
            "New workspace",
            "Name the workspace to save the current session as:",
            "",
            "Save",
            Validation::None,
        )));
    }

    /// The popup's `Edit` verb: the same dialog, seeded with the name it has now (Go had no rename
    /// at all; its Save-As was a save plus a delete, which loses the stored session if the second
    /// half is interrupted — [`Session::workspace_rename`] moves the entry instead).
    pub fn rename_workspace_prompt(&mut self, name: &str) {
        let mut prompt = Prompt::new(
            DialogKind::RenameWorkspace {
                name: name.to_string(),
            },
            "Rename workspace",
            format!("New name for the workspace {name:?}:"),
            name,
            "Rename",
            Validation::None,
        );
        prompt.select = Some(name.chars().count());
        self.open_dialog(Dialog::Prompt(prompt));
    }

    /// `Session::workspace_rename`, with the reporting the delete path already has: the chip, a
    /// refreshed list, and the selected name following the move.
    pub fn rename_workspace(&mut self, from: &str, to: &str) {
        if from == to {
            return;
        }
        match self.session.workspace_rename(from, to) {
            Ok(()) => {
                let now = Instant::now();
                self.workspaces.show_chip(now);
                if self.workspaces.selected == workspace_label(from) {
                    self.workspaces.selected = workspace_label(to);
                }
                self.workspaces.refresh(self.session.workspaces());
                self.notices.post(
                    toast::Level::Success,
                    format!("workspace {from}: renamed to {to}"),
                );
            }
            Err(e) => self.notices.post(
                toast::Level::Error,
                format!("workspace {from}: {}", e.user_message()),
            ),
        }
    }

    /// The destructive half: a `Confirm` whose affirmative answer deletes (plan **D6** — the
    /// reference made `Delete` a primary action, and a primary action on a stored session still has
    /// to ask).
    pub fn confirm_delete_workspace(&mut self, name: &str) {
        let tabs = self
            .workspaces
            .names
            .iter()
            .find(|meta| meta.name.eq_ignore_ascii_case(name))
            .map_or(0, |meta| meta.tabs);
        let detail = vec![format!("{tabs} tab(s) in the stored session")];
        let confirm = Confirm::new(
            DialogKind::DeleteWorkspace {
                name: name.to_string(),
            },
            "Delete workspace",
            format!("Delete the saved workspace {name:?}?"),
            detail,
            vec![
                ConfirmButton::new("Keep", Choice::Cancel),
                ConfirmButton::danger("Delete", Choice::Ok),
            ],
        );
        self.open_dialog(Dialog::Confirm(confirm));
    }

    /// `Session::workspace_delete` — the affirmative arm of the confirm above.
    pub fn delete_workspace(&mut self, name: &str) {
        match self.session.workspace_delete(name) {
            Ok(removed) => {
                self.workspaces.discard();
                self.workspaces.refresh(self.session.workspaces());
                self.notices.post(
                    if removed {
                        toast::Level::Success
                    } else {
                        toast::Level::Warning
                    },
                    format!(
                        "workspace {name}: {}",
                        if removed {
                            "deleted"
                        } else {
                            "no such workspace"
                        }
                    ),
                );
            }
            Err(e) => self.notices.post(
                toast::Level::Error,
                format!("workspace {name}: {}", e.user_message()),
            ),
        }
    }
}

/// The selector's text for a name, keeping the reserved row's flag.
fn workspace_label(name: &str) -> String {
    if zenman_core::services::workspaces::is_reserved(name) {
        format!("{name} (autosave)")
    } else {
        name.to_string()
    }
}

/// The pane set a stored session names, in grid order, **and** the number of stored tabs that set
/// cannot hold.
///
/// The persisted `LayoutMode` spells one or two panes; the quad grid is evidenced by its two bottom
/// panes ([`crate::state::AppModel::quad`] derives the same fact from the live pane map). Those three
/// sets are also the only grids [`crate::dock::layout_for_slots`] can build a tree for — a file naming
/// a single bottom pane would otherwise have been reported as a three-pane restore while the dock
/// quietly drew one leaf, which is the Go-F3 "silently dropped pane state" this function's doc forbids.
/// Such a file therefore restores as the grid its `layout` names, and the tabs with nowhere to go are
/// counted for the report.
fn session_slots(workspace: &Workspace) -> (Vec<PaneSlot>, usize) {
    let named: Vec<PaneSlot> = quad_slots()
        .into_iter()
        .filter(|slot| workspace.panes.contains_key(slot))
        .collect();
    // The quad grid is the one shape `LayoutMode` cannot spell, so four named panes *are* the
    // evidence for it. Three is not: it is a file this build has no tree for, and the mode's own
    // answer is the honest grid — with the leftover tabs counted in the report.
    if named.len() == 4 {
        return (quad_slots(), 0);
    }
    let slots = panes_for(workspace.layout);
    let dropped = workspace
        .panes
        .iter()
        .filter(|(slot, _)| !slots.contains(slot))
        .map(|(_, pane)| pane.tabs.len())
        .sum();
    (slots, dropped)
}

/// The rows of one stored pane, as runtime groups plus the `String → GroupId` map its tabs resolve
/// against (design §4 steps 1 and 3).
///
/// Ids come back from the file through [`AppModel::adopt_group_id`] rather than being re-minted, because
/// a restore that renamed its own groups would no longer equal what it read, and `Autosave` compares the
/// session by value: every start would dirty the session file.
fn stored_groups(
    model: &mut crate::state::AppModel,
    rows: &[TabGroupSession],
) -> (Vec<TabGroup>, BTreeMap<String, GroupId>) {
    let mut ids: BTreeMap<String, GroupId> = BTreeMap::new();
    let mut groups: Vec<TabGroup> = Vec::with_capacity(rows.len());
    let mut taken: Vec<GroupColor> = Vec::new();
    let mut colourless: Vec<usize> = Vec::new();
    for row in rows {
        if ids.contains_key(row.id.as_str()) {
            // Two rows, one id: the first is the group, the second is a duplicate of it in somebody
            // else's file. Keeping both would give one id two labels.
            continue;
        }
        let id = model.adopt_group_id(&row.id);
        ids.insert(row.id.clone(), id);
        let Some(color) = row.group else {
            colourless.push(groups.len());
            groups.push(TabGroup::new(id, GroupColor::A));
            continue;
        };
        taken.push(color);
        let mut group = TabGroup::new(id, color);
        group.name = row.name.clone();
        group.collapsed = row.collapsed;
        groups.push(group);
    }
    for ix in colourless {
        // A row that named no colour (hand-edited, or a foreign writer) takes the first colour no other
        // row of this pane holds; past nine of them colours repeat, because a colour is only an accent.
        let color = GroupColor::ALL
            .iter()
            .find(|color| !taken.contains(color))
            .copied()
            .unwrap_or(GroupColor::A);
        taken.push(color);
        groups[ix].color = color;
    }
    (groups, ids)
}

/// The contiguous runs of a bare group letter — design §4 step 3, the shape Go's sessions are in: Go had
/// no group *object*, so every run of one letter becomes one unnamed, expanded group.
///
/// `None` entries are tabs that already resolved to a stored row, and they break a run: the letter says
/// "same group as the tab next to me" only where the file had no row to say otherwise.
fn legacy_runs(letters: &[Option<GroupColor>]) -> Vec<(usize, usize, GroupColor)> {
    let mut runs: Vec<(usize, usize, GroupColor)> = Vec::new();
    for (ix, color) in letters.iter().copied().enumerate() {
        let Some(color) = color else { continue };
        match runs.last_mut() {
            Some((_, end, have)) if *have == color && *end == ix => *end = ix + 1,
            _ => runs.push((ix, ix + 1, color)),
        }
    }
    runs
}

/// Read a stored tab path back: an absolute host path or an archive VFS path (both round-trip
/// through [`FsPath`]), `None` for anything else — empty, relative, or `~`-prefixed. The rejection is
/// deliberate: resolving those needs a "current directory", and the only one this process has is the
/// one the user launched it from, which is Go's bug rather than a behaviour to keep.
fn stored_path(raw: &str) -> Option<FsPath> {
    match zenman_core::path::resolve_kind(raw) {
        ResolveKind::AbsoluteUnix | ResolveKind::AbsoluteWindows => {
            FsPath::resolve(raw, Path::new("/"), None).ok()
        }
        ResolveKind::Empty | ResolveKind::Relative | ResolveKind::Tilde => None,
    }
}

/// What [`ZenApp::apply_workspace`] did — a value, so the caller can *say* it (plan **F17**: a
/// swallowed report is how a dropped pane state stayed invisible for a release).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// Tabs that came back.
    pub tabs: usize,
    /// Panes in the grid after the restore.
    pub panes: usize,
    /// Stored tabs whose path this build cannot read (see [`stored_path`]).
    pub skipped: usize,
    /// Stored tabs that lived in a pane set this build cannot open (see [`session_slots`]).
    pub dropped: usize,
    /// `true` when the session's own dock tree was installed rather than the default topology.
    pub dock: bool,
}

impl Applied {
    /// Stored tabs that did not come back, for either reason.
    #[must_use]
    pub fn lost(&self) -> usize {
        self.skipped + self.dropped
    }

    /// "restored 6 tab(s) in 2 pane(s), 1 tab(s) with an unreadable path" — the same sentence in the
    /// autosave report and in the `--session` one, which is why it lives here.
    #[must_use]
    pub fn report(&self) -> String {
        let mut line = format!("restored {} tab(s) in {} pane(s)", self.tabs, self.panes);
        if self.skipped > 0 {
            line.push_str(&format!(
                ", {} tab(s) with an unreadable path",
                self.skipped
            ));
        }
        if self.dropped > 0 {
            line.push_str(&format!(
                ", {} tab(s) in a pane this build cannot open",
                self.dropped
            ));
        }
        if !self.dock {
            line.push_str(", default layout");
        }
        line
    }
}

/// What [`ZenApp::restore_last_session`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionRestore {
    /// Nothing was ever saved: a first run (or a fresh data directory).
    Missing,
    /// A session was stored and applied; [`Applied`] says what came of it, which may be *no* tabs —
    /// a session saved from an empty grid, or one whose tabs all named paths this build cannot read.
    /// Ask [`SessionRestore::restored`] before deciding whether to open a start tab.
    Restored(Applied),
    /// The store could not be read at all (a settings file this build cannot parse, an unreadable
    /// directory). The boot path still starts — a lost session is a notice, not an exit code.
    Failed(String),
}

impl SessionRestore {
    /// `true` when the caller must not open its own start tab: the session brought tabs back.
    #[must_use]
    pub fn restored(&self) -> bool {
        matches!(self, Self::Restored(applied) if applied.tabs > 0)
    }

    /// The one-line report: stdout (`apps/zenman`'s boot note), the window's boot lines, and the
    /// smoke test that reads stdout back.
    /// The line for the window's boot report and the log.
    #[must_use]
    pub fn boot_line(&self) -> String {
        match self {
            Self::Missing => "session: none saved yet".to_string(),
            Self::Restored(applied) if applied.tabs == 0 => format!(
                "session: nothing to restore ({} pane(s), {} tab(s) lost)",
                applied.panes,
                applied.lost()
            ),
            Self::Restored(applied) => format!("session: {}", applied.report()),
            Self::Failed(why) => format!("session: could not be restored ({why})"),
        }
    }
}

use crate::app::tab_title;
use crate::dialogs::{Choice, Confirm, ConfirmButton, Dialog, DialogKind, Prompt, Validation};
use crate::state::{panes_for, quad_slots};
