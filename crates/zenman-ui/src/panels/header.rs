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

use std::time::Instant;

use egui::{Color32, CornerRadius, Frame, Margin, RichText, ScrollArea, Stroke, Ui, vec2};

use zenman_core::services::workspaces::{
    Autosave, Pending, RESERVED, WorkspaceMeta, normalise_name,
};
use zenman_core::store::config::Workspace;

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

    /// Forget the outstanding write (a restore took the pane set away from it).
    pub fn discard(&mut self) {
        self.autosave.discard();
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
                    path: tab
                        .path
                        .host_abs()
                        .map_or_else(String::new, |host| host.to_string_lossy()),
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

    /// `Session::workspace_restore`, applied: layout, then one tab per stored tab, then a listing.
    pub fn restore_workspace(&mut self, name: &str) {
        match self.session.workspace_restore(name) {
            Ok(workspace) => {
                // A pending write of the *old* workspace must not land under the new name.
                self.workspaces.discard();
                self.workspaces.selected = workspace_label(name);
                self.apply_workspace(&workspace);
                self.notices
                    .post(toast::Level::Success, format!("workspace {name}: restored"));
            }
            Err(e) => self.notices.post(
                toast::Level::Error,
                format!("workspace {name}: {}", e.user_message()),
            ),
        }
    }

    /// Replace the grid with a stored workspace (Go's `restoreWorkspace`: tabs, then the active pane).
    pub fn apply_workspace(&mut self, workspace: &Workspace) {
        let layout = workspace.layout;
        self.model.layout = layout;
        crate::dock::set_layout(&mut self.model, &mut self.dock, panes_for(layout));
        for slot in self.model.grid_slots() {
            let Some(pane_session) = workspace.panes.get(&slot) else {
                continue;
            };
            let tabs: Vec<crate::state::Tab> = pane_session
                .tabs
                .iter()
                .filter_map(|tab| {
                    let abs = zenman_core::path::AbsPath::from_path(&tab.path).ok()?;
                    let path = zenman_core::path::FsPath::from_abs(abs.clone());
                    let id = self.model.next_tab_id();
                    Some(crate::state::Tab::new(
                        id,
                        path.clone(),
                        if tab.name.is_empty() {
                            tab_title(&path)
                        } else {
                            tab.name.clone()
                        },
                    ))
                })
                .collect();
            let count = tabs.len().max(1);
            if let Some(pane) = self.model.panes.get_mut(&slot) {
                pane.tabs = tabs;
                pane.active = pane_session.active.min(count.saturating_sub(1));
                pane.mru.clear();
            }
            for ix in 0..count {
                self.list_at(slot, ix, false, false);
            }
        }
        self.model.active_pane = workspace.active;
    }

    /// `Session::workspace_save` under a name the user typed (`New`).
    pub fn save_workspace_as(&mut self, name: &str) {
        let session = self.workspace_session();
        match self.session.workspace_save(name, &session) {
            Ok(()) => {
                let now = Instant::now();
                self.workspaces.mark_saved(now);
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

use crate::app::tab_title;
use crate::dialogs::{Choice, Confirm, ConfirmButton, Dialog, DialogKind};
use crate::state::panes_for;
