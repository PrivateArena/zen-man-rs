//! The header's workspace vocabulary (plan §5.8 `panels/header.rs`, §9.2 step-8c review gap 1).
//!
//! The reference header is
//! `Workspace: <select>` · `New` · `Delete` · `Saved` · disk meter · `History` · `2-Pane (F3)` ·
//! `4-Pane (F4)`. Step 8 drew the right-hand half; this file owns the left-hand half and the two
//! layout buttons' *chords*, which come from [`KeyMap::hint`] so a re-bound key cannot leave a stale
//! label behind (plan **D7** — one table, two front ends).
//!
//! # Who owns what
//!
//! [`WorkspaceBar`] owns the autosave state machine (`workspaces::Autosave`, step 14a) and the
//! `Saved` chip clock; the shell owns the engine. The bar never calls `Session` — it emits
//! [`HeaderCommand`]s, exactly like a pane emits [`crate::panels::file_pane::PaneCommand`], and the
//! `impl ZenApp` block at the bottom of this file is the only place that answers them.
//!
//! # Fitting at 800 px
//!
//! The plan's criterion is "every control reachable at 800 px". `fit_toolbar` does not exist in the
//! pinned `craft_ui`, so the fit is done here: the *optional, non-interactive* members (the boot
//! note, the disk meter text) are dropped first, and the interactive ones are never dropped. A test
//! asserts every interactive header control's rect lies inside the viewport at 800 and 1600 px.

use std::path::Path;
use std::time::Instant;

use egui::{Color32, CornerRadius, Frame, Margin, RichText, ScrollArea, Stroke, Ui, vec2};

use zenman_core::error::CoreError;
use zenman_core::path::{FsPath, ResolveKind};
use zenman_core::services::workspaces::{
    Autosave, Pending, RESERVED, WorkspaceMeta, normalise_name,
};
use zenman_core::store::config::{LayoutMode, PaneSlot, ViewMode, Workspace};

use crate::app::ZenApp;
use crate::theme::Tokens;
use crate::widgets::{disk_meter, toast};

/// What the workspace group asks the shell to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeaderCommand {
    /// Select a stored workspace: `Session::workspace_restore`.
    Restore(String),
    /// Store the running session under a new name: `Session::workspace_save`.
    SaveNew(String),
    /// Delete a stored workspace. Destructive, so the shell answers it with a `Confirm`.
    Delete(String),
}

/// The header's workspace group: the dropdown, `New`/`Delete`, the `Saved` chip, and the autosave
/// clock behind them.
#[derive(Debug)]
pub struct WorkspaceBar {
    /// The autosave target — always the reserved name (Go's `autoSaveSession()` wrote `default`).
    pub target: String,
    /// The debounce/`Saved` clock, driven by the frame's `Instant` (14a: core cannot own a timer).
    pub autosave: Autosave,
    /// The stored workspaces, refreshed from the session whenever the panel is drawn.
    pub names: Vec<WorkspaceMeta>,
    /// What the selector shows.
    pub selected: String,
    /// `Some` while the inline *New workspace* field is open.
    pub entry: Option<String>,
    /// The last validation failure (a name Go would have accepted and then made unselectable).
    pub error: Option<String>,
    /// The dock tree carried into the next autosave (serialised only when the dock moved).
    pub dock: Option<serde_json::Value>,
}

impl Default for WorkspaceBar {
    fn default() -> Self {
        Self {
            target: RESERVED.to_string(),
            autosave: Autosave::default(),
            names: Vec::new(),
            selected: RESERVED.to_string(),
            entry: None,
            error: None,
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

    /// Whether the `Saved` chip shows (2 s after each write, Go's `_showSaveIndicator`).
    #[must_use]
    pub fn chip_shows(&self, now: Instant) -> bool {
        self.autosave.show_saved(now)
    }

    /// The dropdown's rows: every stored name, with the reserved one *flagged*, not hidden (Go
    /// filtered `default` out and thereby made a session saved as `Default` unrestorable).
    #[must_use]
    pub fn labels(&self) -> Vec<(String, String)> {
        self.names
            .iter()
            .map(|meta| {
                let label = if meta.reserved {
                    format!("{} (autosave)", meta.name)
                } else {
                    meta.name.clone()
                };
                (meta.name.clone(), label)
            })
            .collect()
    }

    /// Draw the group and report what it asked for.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        tokens: &Tokens,
        disk: Option<(u64, u64)>,
        now: Instant,
    ) -> Vec<HeaderCommand> {
        let mut out = Vec::new();
        let labels = self.labels();
        let mut current = self.selected.clone();
        let mut entry = self.entry.clone();
        let room = ui.available_width();
        let mut asked = None;
        let mut cancel = false;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing = vec2(tokens.header_gap, 0.0);
            ui.label(
                RichText::new("Workspace:")
                    .color(tokens.text_muted)
                    .size(tokens.font_size),
            );
            // `craft_ui::ComboBox`, not egui's: the framework's own guard says egui's popup cannot
            // grow while it is open, and the workspace list is exactly the kind that grows.
            let _combo = craft_ui::ComboBox::from_id_salt("zenman-workspace-select")
                .width(tokens.workspace_select_width)
                .selected_text(RichText::new(current.clone()).color(tokens.text_main))
                .show_ui(ui, |ui| {
                    ScrollArea::vertical()
                        .max_height(tokens.log_row_height * tokens.dropdown_visible_rows)
                        .show(ui, |ui| {
                            for (name, label) in &labels {
                                ui.selectable_value(&mut current, name.clone(), label);
                            }
                        });
                });
            if ui
                .button(RichText::new("New").color(tokens.text_main))
                .clicked()
            {
                entry = Some(String::new());
            }
            if ui
                .button(RichText::new("Delete").color(tokens.danger))
                .clicked()
            {
                asked = Some(HeaderCommand::Delete(current.clone()));
            }
            if self.chip_shows(now) {
                chip(ui, tokens, "Saved", tokens.success);
            }
            if let Some((free, total)) = disk
                && room > tokens.workspace_select_width * tokens.header_disk_fit_ratio
            {
                ui.label(
                    RichText::new(format!(
                        "{} free of {}",
                        disk_meter::format_size(free),
                        disk_meter::format_size(total)
                    ))
                    .color(tokens.text_muted)
                    .size(tokens.font_size - 1.0),
                );
            }
            if let Some(text) = &mut entry {
                let field = ui.add_sized(
                    vec2(tokens.form_field_width, tokens.menu_item_height),
                    egui::TextEdit::singleline(text).hint_text("new workspace name"),
                );
                let commit = ui.button("Save").clicked()
                    || field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
                if commit {
                    asked = normalise_name(text)
                        .map(|name| Some(HeaderCommand::SaveNew(name)))
                        .unwrap_or_else(|e| {
                            self.error = Some(e.user_message());
                            None
                        });
                    if asked.is_some() {
                        entry = None;
                    }
                }
            }
        });
        if cancel {
            entry = None;
        }
        if let Some(text) = entry {
            self.entry = Some(text);
        } else {
            self.entry = None;
        }
        if current != self.selected {
            self.selected.clone_from(&current);
            out.push(HeaderCommand::Restore(current));
        }
        if let Some(command) = asked {
            out.push(command);
        }
        out
    }
}

/// A small status chip: `Saved` in the success colour, the only pill-shaped thing the header owns.
///
/// The text is a real `Label` (a painted string would be invisible to the accessibility tree, and
/// to every test), and the pill is its `Frame` — the frame itself allocates nothing interactive.
fn chip(ui: &mut Ui, tokens: &Tokens, text: &str, colour: Color32) {
    Frame::new()
        .stroke(Stroke::new(1.0, colour))
        .corner_radius(CornerRadius::same(tokens.corner_radius / 2))
        .inner_margin(Margin::same(tokens.chip_pad as i8))
        .show(ui, |ui| {
            ui.label(
                RichText::new(text)
                    .color(colour)
                    .size(tokens.font_size - 1.0),
            );
        });
}

/// A header button whose label carries its chord (`2-Pane (F3)`, the reference's own wording), and
/// Go's `.btn-global`/`.btn-global.active` visuals: `bg-main` + `border-color` at rest, the accent
/// fill with the `text-active` label once it is the mode on screen (`workspace.css:36-58`).
///
/// The accent fill is what makes a *toggle* read as a toggle: with `2-Pane` and `4-Pane` now the
/// only layout controls, the highlighted one is the grid you are looking at, and clicking it again
/// is what Go's `setSplitView(!state.isSplit)` means.
pub fn chord_button(
    ui: &mut Ui,
    tokens: &Tokens,
    label: String,
    active: bool,
    tooltip: &str,
) -> bool {
    let (fill, stroke, text) = if active {
        (
            tokens.accent,
            Stroke::new(1.0, tokens.accent),
            tokens.text_active,
        )
    } else {
        (
            tokens.bg_main,
            Stroke::new(1.0, tokens.border),
            tokens.text_main,
        )
    };
    ui.add(
        egui::Button::new(
            RichText::new(label)
                .color(text)
                .size(tokens.font_size - 1.0),
        )
        .fill(fill)
        .stroke(stroke)
        .corner_radius(CornerRadius::same(tokens.crumb_corner_radius)),
    )
    .on_hover_text(tooltip)
    .clicked()
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
                    // an archive member (`/a/b.zip/inner`), while `host_abs()` answers `None` for one
                    // and would have written an empty path — a tab that reloads as nothing.
                    path: tab.path.to_string(),
                    name: tab.title.clone(),
                    group: tab.group,
                    view: tab.view,
                })
                .collect::<Vec<_>>();
            panes.insert(
                *slot,
                zenman_core::store::config::PaneSession {
                    tabs,
                    active: pane.active,
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
    /// Answer what the workspace group asked for.
    pub fn header_commands(&mut self, commands: Vec<HeaderCommand>) {
        for command in commands {
            match command {
                HeaderCommand::Restore(name) => self.restore_workspace(&name),
                HeaderCommand::SaveNew(name) => self.save_workspace_as(&name),
                HeaderCommand::Delete(name) => self.confirm_delete_workspace(&name),
            }
        }
    }

    /// Write whatever the autosave is still holding, **ignoring the debounce** (window close, and the
    /// smoke run's self-close).
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
        let Some(pending) = self.workspaces.take_pending() else {
            return Ok(());
        };
        match self.session.workspace_save(&pending.name, &pending.session) {
            Ok(()) => {
                self.workspaces.mark_saved(now);
                self.workspaces.refresh(self.session.workspaces());
                Ok(())
            }
            Err(e) => Err(format!("workspace {}: {}", pending.name, e.user_message())),
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
    /// autosave, `--session`, and the header's dropdown — goes through this, so a lost tab is never a
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
            let mut tabs: Vec<crate::state::Tab> = Vec::with_capacity(pane_session.tabs.len());
            for stored in &pane_session.tabs {
                let Some(path) = stored_path(&stored.path) else {
                    applied.skipped += 1;
                    continue;
                };
                let id = self.model.next_tab_id();
                let title = if stored.name.is_empty() {
                    tab_title(&path)
                } else {
                    stored.name.clone()
                };
                let mut tab = crate::state::Tab::new(id, path, title);
                tab.group = stored.group;
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
            applied.tabs += tabs.len();
            let shown = pane_session.active.min(tabs.len().saturating_sub(1));
            let order: Vec<crate::state::TabId> = tabs.iter().map(|tab| tab.id).collect();
            if let Some(pane) = self.model.panes.get_mut(slot) {
                pane.tabs = tabs;
                pane.active = shown;
                pane.mru.clear();
                // The MRU ring (`Alt+Z`, step 10) is part of the pane's state: an empty one makes that
                // chord do nothing on a pane that plainly has tabs in it. Touched in strip order, the
                // shown tab last so it lands at the front.
                for id in &order {
                    pane.touch_mru(*id);
                }
                if let Some(id) = order.get(shown) {
                    pane.touch_mru(*id);
                }
            }
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
                self.workspaces.entry = None;
                self.workspaces.error = None;
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
use crate::dialogs::{Choice, Confirm, ConfirmButton, Dialog, DialogKind};
use crate::state::{panes_for, quad_slots};
