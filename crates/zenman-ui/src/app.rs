//! `ZenApp` — the shell (plan §5.7 `app.rs` row).
//!
//! The plan's struct names fields owned by later steps (`dialogs`, `menus`, `keymap`, `origins`);
//! they arrive with their steps (10) and this struct is the one place they get added. What step 7
//! really means — "the app opens with an empty dock + 2 panes; header, drawers and tab strip are
//! visible; jobs tick; the layout persists" — is all here.
//!
//! `Session` is held **by value**, not `Arc<Session>` as the plan writes: `Session` is already an
//! `Arc<Inner>` handle (it is `Clone`), and `Session::poll` takes `&mut self` — an `Arc` would force
//! `Arc::get_mut` gymnastics for no shared-ownership benefit (the UI is the only holder). Recorded
//! as a plan correction.

use std::time::Instant;

use egui::{CentralPanel, Panel, Ui};

use std::sync::Arc;

use zenman_core::error::CoreError;
use zenman_core::events::ChangeCause;
use zenman_core::jobs::JobId;
use zenman_core::ops::ConflictPolicy;
use zenman_core::ops::clipboard::{ClipOp, ClipScope, ClipboardStore};
use zenman_core::ops::delete::DeleteReq;
use zenman_core::ops::paste::{PasteMode, PasteReq};
use zenman_core::path::{AbsPath, FsPath};
use zenman_core::services::dirs::ListDir;
use zenman_core::session::Origin;
use zenman_core::session::{Receipt, Session, StartupNote};
use zenman_core::store::config::{LayoutMode, PaneSlot, ThemeMode};

use crate::dock::{self, Dock};
use crate::jobs_ui::JobsUi;
use crate::keymap::{Command, KeyMap};
use crate::panels::{
    self,
    file_pane::{PaneCommand, RowOrder},
};
use crate::persist::Persisted;
use crate::state::{AppModel, Tab, TabId, panes_for, quad_slots};
use crate::theme::{self, Tokens};
use crate::widgets::{disk_meter, toast};

/// The whole shell.
pub struct ZenApp {
    /// The engine handle (plan §4.1's `Session`).
    pub session: Session,
    /// UI-owned state (plan §4.3).
    pub model: AppModel,
    /// The pane grid.
    pub dock: Dock,
    /// Toast queue (plan: `notices::State`).
    pub notices: toast::State,
    /// Job correlation state.
    pub jobs: JobsUi,
    /// Settings/dock store (`craft-settings`; no `std::fs` anywhere).
    pub persist: Persisted,
    /// The theme choice (mirrored to `ShellSettings` on save).
    pub theme: ThemeMode,
    /// The design tokens for the current theme.
    pub tokens: Tokens,
    /// The boot report lines (GPU profile, startup notes) shown in the header tooltip/status area.
    pub boot_lines: Vec<String>,
    /// The context menu the panes asked for, waiting for step 10's `menus::Registry` to draw it.
    pub context_request: Option<(PaneSlot, Option<usize>, egui::Pos2)>,
    /// The keymap — **the** single source of truth for dispatch and for the menu hints 10b draws
    /// (plan D7).
    pub keys: KeyMap,
    /// The pane whose <kbd>Ctrl+Shift+N</kbd> is waiting for step 10c's name prompt
    /// (`Session::mkdir` needs a name, and inventing one is not an option).
    pub new_folder_request: Option<PaneSlot>,
    /// When the next debounced persist is due (plan §5.7 `persist_at`).
    pub persist_at: Option<Instant>,
    /// The last frame's start, for the step-8 `perf_frame` budget check.
    pub last_frame: Instant,
}

impl ZenApp {
    /// Build the shell over a live session, restoring the persisted dock (plan F17: validated
    /// before first paint, with a report — never silently).
    #[must_use]
    pub fn new(session: Session, boot_lines: Vec<String>) -> Self {
        let theme = session.config().theme;
        let persist = Persisted::memory();
        let layout = persist.settings.get().layout;
        Self {
            dock: dock::default_workspace(layout),
            model: AppModel::new(layout),
            session,
            notices: toast::State::default(),
            jobs: JobsUi::default(),
            persist,
            theme,
            tokens: Tokens::default(),
            boot_lines,
            context_request: None,
            keys: KeyMap::default(),
            new_folder_request: None,
            persist_at: None,
            last_frame: Instant::now(),
        }
    }

    /// Build with an already-open persisted store (the binary's path; tests use [`Self::new`]).
    #[must_use]
    pub fn with_persist(session: Session, boot_lines: Vec<String>, persist: Persisted) -> Self {
        let theme = session.config().theme;
        let layout = persist.settings.get().layout;
        let mut app = Self {
            dock: dock::default_workspace(layout),
            model: AppModel::new(layout),
            session,
            notices: toast::State::default(),
            jobs: JobsUi::default(),
            persist,
            theme,
            tokens: Tokens::default(),
            boot_lines,
            context_request: None,
            keys: KeyMap::default(),
            new_folder_request: None,
            persist_at: None,
            last_frame: Instant::now(),
        };
        // `restore` needs the store and the app apart (the store lives *in* the app), so lift it
        // out, restore, and put it back.
        let store = std::mem::replace(&mut app.persist, Persisted::memory());
        let report = store.restore(&mut app, &is_registered);
        app.persist = store;
        match report {
            dock::ApplyReport::Applied => {}
            dock::ApplyReport::NewerVersion { found } => app.notices.post(
                toast::Level::Warning,
                format!("saved layout is from a newer zenman (dock v{found}); using the default"),
            ),
            dock::ApplyReport::Invalid => app.notices.post(
                toast::Level::Warning,
                "saved layout was unreadable; using the default",
            ),
        }
        app
    }

    /// Queue the engine's startup notes as toasts (plan F1: "degrade and say so" — the notes come
    /// from `Session::open` and must reach the user, not the log).
    pub fn post_startup_notes(&mut self, notes: &[StartupNote]) {
        for note in notes {
            self.notices.post(toast::Level::Warning, note.message());
        }
    }

    /// Open a tab on `path` in `slot` and list it — the entry point step 8 exists for, and the
    /// reason the panes are no longer empty (plan §7 row 8).
    pub fn open_tab(&mut self, slot: PaneSlot, path: FsPath) -> TabId {
        let title = tab_title(&path);
        let id = self.model.next_tab_id();
        {
            let pane = self.model.pane_mut(slot);
            pane.tabs.push(crate::state::Tab::new(id, path, title));
            pane.active = pane.tabs.len() - 1;
            pane.touch_mru(id);
            id
        };
        self.list(slot, false, false);
        id
    }

    /// List the shown tab, replacing or appending its page.
    ///
    /// The read goes through `Session::page_dir`, which answers `Done` when the directory cache has
    /// it and `Running` when it must be read (plan §4.1); either way the frame that asked is the
    /// frame that paints, because nothing here waits.
    pub fn list(&mut self, slot: PaneSlot, append: bool, all: bool) {
        let Some(ix) = self
            .model
            .panes
            .get(&slot)
            .map(|pane| pane.active.min(pane.tabs.len().saturating_sub(1)))
        else {
            return;
        };
        self.list_at(slot, ix, append, all);
    }

    /// [`Self::list`] addressed to **one tab** rather than to the pane's shown one. Step 9d needs it:
    /// a `DirChanged` invalidates every tab that lists the changed directory, including the ones the
    /// user is not looking at (plan §4.3's correlation rule — a tab, not a pane, owns a listing).
    pub fn list_at(&mut self, slot: PaneSlot, ix: usize, append: bool, all: bool) {
        let Some(tab) = self.model.tab_at_mut((slot, ix)) else {
            return;
        };
        let origin = self.session.next_origin();
        let limit = if all {
            tab.total.max(tab.page_size)
        } else {
            tab.page_size
        };
        let cursor = if append {
            Some(tab.entries.cursor)
        } else {
            None
        };
        let req = ListDir {
            path: tab.path.clone(),
            cursor,
            limit,
            sort: tab.sort,
            // Step 9d: the tab's own flat mode, mapped to the engine's (`Grouped` walks `Mixed`).
            flat: tab.flat.to_core(),
        };
        tab.loading_more = append;
        let receipt = self.session.page_dir(&req, origin);
        match receipt {
            Ok(Receipt::Done(page)) => {
                let Some(tab) = self.model.tab_at_mut((slot, ix)) else {
                    return;
                };
                if append {
                    tab.append_page(page);
                } else {
                    tab.set_page_keeping_selection(page);
                }
                tab.loading_more = false;
            }
            Ok(Receipt::Running { job, origin }) => {
                if let Some(info) = self.session.job_info(job) {
                    let tab_id = self.model.tab_at((slot, ix)).map(|tab| tab.id);
                    self.jobs.track(info, Some(slot), tab_id);
                }
                if let Some(pane) = self.model.panes.get_mut(&slot)
                    && let Some(tab) = pane.tabs.get_mut(ix)
                {
                    pane.loading.insert(origin);
                    tab.pending.insert(origin);
                }
            }
            Err(e) => {
                if let Some(tab) = self.model.tab_at_mut((slot, ix)) {
                    tab.error = Some(Arc::new(e));
                    tab.loading_more = false;
                }
            }
        }
    }

    fn tab_mut_of(&mut self, slot: PaneSlot) -> Option<&mut crate::state::Tab> {
        self.model.panes.get_mut(&slot)?.active_tab_mut()
    }

    /// Carry out one pane command (plan §4.1's bus, from `dock_content`'s draw).
    pub fn run_command(&mut self, slot: PaneSlot, command: PaneCommand) {
        match command {
            PaneCommand::Navigate(path) => {
                self.remember_selection(slot);
                if let Some(tab) = self.tab_mut_of(slot) {
                    tab.navigate_to(path.clone(), tab_title(&path));
                }
                self.list(slot, false, false);
            }
            PaneCommand::Back | PaneCommand::Forward => {
                let path = {
                    let Some(tab) = self.tab_mut_of(slot) else {
                        return;
                    };
                    let path = if matches!(command, PaneCommand::Back) {
                        tab.go_back()
                    } else {
                        tab.go_forward()
                    };
                    tab.sel.reset();
                    path
                };
                if let Some(path) = path {
                    if let Some(tab) = self.tab_mut_of(slot) {
                        tab.path = path;
                    }
                    self.list(slot, false, false);
                }
            }
            PaneCommand::Up => {
                // `..` at the store root has nowhere to go: `FsPath::parent` is the honest guard
                // (it answers `None` at `/`), so no extra filter is needed here — and a filter
                // that is always true would be a comment pretending to be a check.
                let parent = self.tab_mut_of(slot).and_then(|tab| tab.path.parent());
                if let Some(parent) = parent {
                    self.remember_selection(slot);
                    if let Some(tab) = self.tab_mut_of(slot) {
                        tab.navigate_to(parent.clone(), tab_title(&parent));
                    }
                    self.list(slot, false, false);
                }
            }
            PaneCommand::BadAddress(text) => {
                self.notices
                    .post(crate::widgets::toast::Level::Warning, text);
            }
            PaneCommand::Open { ix } => {
                let entry = self
                    .tab_mut_of(slot)
                    .and_then(|tab| tab.entries.entries.get(ix).cloned());
                let Some(entry) = entry else { return };
                let child = {
                    let Some(tab) = self.tab_mut_of(slot) else {
                        return;
                    };
                    tab.path
                        .join_rel(&entry.rel_path)
                        .ok()
                        .or_else(|| tab.path.join(&entry.name).ok())
                };
                let Some(child) = child else { return };
                if entry.is_dir {
                    self.remember_selection(slot);
                    if let Some(tab) = self.tab_mut_of(slot) {
                        tab.navigate_to(child, entry.name.clone());
                    }
                    self.list(slot, false, false);
                } else {
                    // Plan §4.1's `Session::open` is step 9's write path; until then the shell says
                    // so instead of pretending the row was opened.
                    self.notices.post(
                        crate::widgets::toast::Level::Info,
                        format!("open: {}", entry.name),
                    );
                }
            }
            PaneCommand::Context { ix, at } => {
                // Recorded, not drawn: `menus::Registry` is step 10's, and a right-click that
                // selects its row is already half the behaviour.
                self.context_request = Some((slot, ix, at));
                if let Some(ix) = ix
                    && let Some(tab) = self.tab_mut_of(slot)
                {
                    let keys: Vec<String> = tab
                        .entries
                        .entries
                        .iter()
                        .map(|e| crate::state::Tab::row_key(e).to_string())
                        .collect();
                    tab.sel.apply(
                        crate::widgets::virtual_list::SelKind::Single,
                        ix,
                        keys.len(),
                        |i| keys.get(i).map(String::as_str),
                    );
                }
            }
            PaneCommand::Sort(column) => {
                if let Some(tab) = self.tab_mut_of(slot) {
                    // Go's own rule (`api/dir.go:121-124`): a fresh column starts *ascending*, and
                    // only a second click on the column already in play reverses it.
                    let descending = tab.sort.column == column && !tab.sort.descending;
                    tab.resort(zenman_core::store::config::SortSpec { column, descending });
                }
            }
            PaneCommand::ToggleGrid => {
                if let Some(tab) = self.tab_mut_of(slot) {
                    tab.view = match tab.view {
                        zenman_core::store::config::ViewMode::Grid => {
                            zenman_core::store::config::ViewMode::List
                        }
                        _ => zenman_core::store::config::ViewMode::Grid,
                    };
                }
            }
            PaneCommand::SetFlat(flat) => {
                // The mode is *listing* state, not just paint state: it changes which rows the engine
                // walks (`ListDir::flat`) and which cache entry answers (`DirCacheKey::flat`), so the
                // click re-lists instead of re-painting. Go's toolbar did the same — the dropdown
                // called `loadDirectory` again (`navigation.js:92-97`).
                if let Some(tab) = self.tab_mut_of(slot) {
                    tab.flat = flat;
                    // The rows the new mode still has keep their selection (keys are listing-relative
                    // paths, and a root row's rel path *is* its name); [`Tab::set_page_keeping_selection`]
                    // prunes the rest when the page lands.
                    tab.collapsed_groups.clear();
                }
                self.list(slot, false, false);
            }
            PaneCommand::LoadMore => self.list(slot, true, false),
            PaneCommand::LoadAll => self.list(slot, true, true),
            PaneCommand::Refresh => self.list(slot, false, false),
            PaneCommand::NewTab => {
                let path = self
                    .tab_mut_of(slot)
                    .map(|tab| tab.path.clone())
                    .unwrap_or(FsPath::ROOT);
                self.open_tab(slot, path);
            }
            PaneCommand::SelectTab(ix) => {
                if let Some(pane) = self.model.panes.get_mut(&slot)
                    && ix < pane.tabs.len()
                {
                    pane.active = ix;
                    let id = pane.tabs[ix].id;
                    pane.touch_mru(id);
                }
            }
            PaneCommand::CloseTab(ix) => {
                if let Some(pane) = self.model.panes.get_mut(&slot)
                    && ix < pane.tabs.len()
                {
                    let id = pane.tabs.remove(ix).id;
                    pane.mru.retain(|existing| *existing != id);
                    pane.active = pane.active.min(pane.tabs.len().saturating_sub(1));
                }
            }
            PaneCommand::SelectAll | PaneCommand::SelectNone => {
                if let Some(tab) = self.tab_mut_of(slot) {
                    let keys: Vec<String> = tab
                        .entries
                        .entries
                        .iter()
                        .map(|e| crate::state::Tab::row_key(e).to_string())
                        .collect();
                    if matches!(command, PaneCommand::SelectAll) {
                        tab.sel
                            .select_all(keys.len(), |i| keys.get(i).map(String::as_str));
                    } else {
                        tab.sel.select_none();
                    }
                }
            }
        }
    }

    /// File the active tab's selection under its directory (plan §4.3's `sel_by_path`).
    fn remember_selection(&mut self, slot: PaneSlot) {
        let Some(tab) = self.tab_mut_of(slot) else {
            return;
        };
        let key = tab.path.to_string();
        let sel = tab.sel.clone();
        tab.sel_by_path.insert(key, sel);
    }

    /// Re-apply the theme after a change (settings load, `F5`-adjacent toggles from step 10).
    pub fn set_theme(&mut self, ctx: &egui::Context, choice: ThemeMode) {
        self.theme = choice;
        theme::apply(ctx, choice);
        self.tokens = theme::tokens(&ctx.global_style());
    }

    /// Change the grid (plan §5.7 `set_layout`: the only way the grid changes shape).
    pub fn set_layout(&mut self, slots: Vec<zenman_core::store::config::PaneSlot>) {
        let keep_mode = if slots.len() >= 2 {
            LayoutMode::Split
        } else {
            LayoutMode::Single
        };
        self.model.layout = keep_mode;
        dock::set_layout(&mut self.model, &mut self.dock, slots);
    }

    /// Per-frame non-drawing work (plan §5.7: poll jobs, drain events, request repaint while
    /// busy). The `logic` half of the fork's `eframe::App` contract, spelled without the `Frame`
    /// parameter so `egui_kittest` can drive it (the Harness has no logic/ui pair — plan §2.2
    /// correction, see the step-7 §9.2 row).
    pub fn logic(&mut self, ctx: &egui::Context) {
        self.handle_keys(ctx);
        JobsUi::tick(self, ctx);
        self.reload_stale();
        let now = Instant::now();
        self.last_frame = now;
        if dock::persist_pending(ctx, now) {
            let snap = self.snapshot();
            self.persist.autosave(&snap, now);
        }
    }

    /// Re-list the tabs the engine's `DirChanged` events invalidated (plan §4.1/F2: coalesced by
    /// core into one event per directory, coalesced *here* into one reload per tab).
    ///
    /// The queue is drained before the first reload runs, so an event that lands while a reload is in
    /// flight is carried to the next pass instead of doubling up. Nothing here resets scroll or the
    /// selection: the reload replaces the page, the selection is path-keyed (step 8), and only the
    /// rows that genuinely vanished leave it — which is the "quietly" half of plan §4.1's note, and
    /// why every [`ChangeCause`](zenman_core::events::ChangeCause) takes the same path.
    fn reload_stale(&mut self) {
        for id in self.model.take_stale() {
            // A tab closed between the event and this pass simply is not there to reload.
            let Some((slot, ix)) = self.model.tab_index(id) else {
                continue;
            };
            self.list_at(slot, ix, false, false);
        }
    }

    /// The values the persist layer mirrors (see [`crate::persist::ShellSnapshot`]).
    #[must_use]
    pub fn snapshot(&self) -> crate::persist::ShellSnapshot {
        crate::persist::ShellSnapshot {
            layout: self.model.layout,
            theme: self.theme,
            left_drawer: self.model.left_drawer.open,
            right_drawer: self.model.right_drawer.open,
            log_panel: self.model.log_panel,
            dock: dock::snapshot(&self.dock).ok().map(|v| v.to_string()),
        }
    }

    /// Per-frame drawing: header, drawers, dock, toasts.
    pub fn ui(&mut self, ui: &mut Ui) {
        let tokens = self.tokens;
        let header_height = tokens.header_height;
        let mut toggle = DrawerToggle::default();
        Panel::top("zenman-header")
            .exact_size(header_height)
            .show(ui, |ui| {
                self.header(ui, &mut toggle);
            });
        if self.model.log_panel {
            Panel::right("zenman-log")
                .exact_size(tokens.action_log_width)
                .resizable(false)
                .show(ui, |ui| {
                    self.action_log_placeholder(ui);
                });
        }
        if self.model.right_drawer.open {
            Panel::right("zenman-right-drawer")
                .exact_size(tokens.right_drawer_width)
                .resizable(false)
                .show(ui, |ui| {
                    self.right_drawer(ui);
                });
        }
        if self.model.left_drawer.open {
            Panel::left("zenman-left-drawer")
                .exact_size(tokens.left_drawer_width)
                .resizable(false)
                .show(ui, |ui| {
                    self.left_drawer(ui);
                });
        }
        Panel::bottom("zenman-status")
            .exact_size(tokens.status_height)
            .show(ui, |ui| {
                self.status_bar(ui);
            });
        CentralPanel::default().show(ui, |ui| {
            crate::dock_content::show(self, ui);
        });
        self.apply_toggles(&toggle);
        self.notices.show(ui.ctx(), &self.tokens);
    }

    /// The 48 px header (plan §2.2: an `egui::TopBottomPanel`, *not* a dock panel — `Side` has no
    /// `Top`).
    fn header(&mut self, ui: &mut Ui, toggle: &mut DrawerToggle) {
        let tokens = self.tokens;
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Zen-Man")
                    .color(tokens.text_active)
                    .strong(),
            );
            if let Some(line) = self.boot_lines.first() {
                ui.label(
                    egui::RichText::new(line)
                        .color(tokens.text_muted)
                        .size(tokens.font_size - 2.0),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if icon_button(ui, &tokens, "info", "action log", self.model.log_panel).clicked() {
                    toggle.log = true;
                }
                if icon_button(ui, &tokens, "disk", "disks", self.model.right_drawer.open).clicked()
                {
                    toggle.right = true;
                }
                if icon_button(
                    ui,
                    &tokens,
                    "folder-open",
                    "places",
                    self.model.left_drawer.open,
                )
                .clicked()
                {
                    toggle.left = true;
                }
                ui.separator();
                let mode = self.model.layout;
                let quad = self.model.quad();
                if layout_button(
                    ui,
                    &tokens,
                    "list",
                    "single pane",
                    mode == LayoutMode::Single && !quad,
                )
                .clicked()
                {
                    toggle.single = true;
                }
                if layout_button(
                    ui,
                    &tokens,
                    "split",
                    "split panes",
                    mode == LayoutMode::Split && !quad,
                )
                .clicked()
                {
                    toggle.split = true;
                }
                if layout_button(ui, &tokens, "quad", "quad panes", quad).clicked() {
                    toggle.quad = true;
                }
            });
        });
    }

    /// The 30 px status bar (plan §5.8 `status_bar.rs`), fed by the active pane's tab.
    fn status_bar(&mut self, ui: &mut Ui) {
        let tokens = self.tokens;
        let slot = self.model.active_pane;
        let names = self
            .model
            .panes
            .get(&slot)
            .and_then(|p| p.active_tab())
            .map(|tab| {
                let order = panels::file_pane::RowOrder::build(tab);
                order.selected_names(&tab.sel)
            })
            .unwrap_or_default();
        let (selection, rows, error) = self
            .model
            .panes
            .get(&slot)
            .and_then(|p| p.active_tab())
            .map(|tab| {
                (
                    tab.sel.clone(),
                    tab.entries.entries.len(),
                    tab.error.as_ref().map(|e| e.user_message()),
                )
            })
            .unwrap_or_default();
        // The live job, if any. Its own words and its own fraction — never a placeholder `0 of 0`,
        // which is what this line used to print (plan §9.2 step-8 row, rough edge 5).
        let live = self.jobs.foreground.iter().find(|job| job.live);
        let info = match error {
            Some(text) => panels::status_bar::StatusInfo::Error(text),
            None => match live {
                Some(job) if !job.spinner => panels::status_bar::StatusInfo::Running {
                    label: job.label.clone(),
                    frac: Some(job.frac),
                },
                Some(job) => panels::status_bar::StatusInfo::Running {
                    label: job.label.clone(),
                    frac: None,
                },
                None => panels::status_bar::StatusInfo::Count(rows),
            },
        };
        let model = panels::status_bar::StatusModel {
            selection: &selection,
            names,
            info,
            jobs: &self.jobs.foreground,
            panes: self.model.panes.len(),
        };
        panels::status_bar::show(ui, &model, &tokens);
    }

    /// The right drawer: disks (plan §5.7: `widgets/disk_meter` is the point of it).
    fn right_drawer(&mut self, ui: &mut Ui) {
        let tokens = self.tokens;
        ui.label(
            egui::RichText::new("Disks")
                .color(tokens.text_main)
                .strong(),
        );
        let path = AbsPath::from_path("/");
        match path.map(|p| self.session.env().disk_space(&p)) {
            Ok(Ok(space)) => {
                let used = space.total.saturating_sub(space.free);
                ui.add_space(tokens.drawer_section_gap);
                let _ = disk_meter::show(ui, &tokens, "/", used, space.total);
            }
            Ok(Err(e)) => {
                ui.label(
                    egui::RichText::new(format!("disk space unavailable: {}", e.user_message()))
                        .color(tokens.text_muted),
                );
            }
            Err(_) => {
                ui.label(egui::RichText::new("no filesystem root").color(tokens.text_muted));
            }
        }
    }

    /// The left drawer: the persisted bookmarks (config is real since step 4; the tree/places
    /// model is step 14's).
    fn left_drawer(&mut self, ui: &mut Ui) {
        let tokens = self.tokens;
        ui.label(
            egui::RichText::new("Places")
                .color(tokens.text_main)
                .strong(),
        );
        let bookmarks = self.session.config().bookmarks;
        if bookmarks.is_empty() {
            ui.label(
                egui::RichText::new("no bookmarks yet — add them in step 14")
                    .color(tokens.text_muted),
            );
            return;
        }
        for bookmark in bookmarks {
            let label = bookmark.label.unwrap_or_else(|| bookmark.path.clone());
            crate::icons::paint(
                ui,
                ui.available_rect_before_wrap(),
                "folder",
                tokens.drawer_icon_size,
                tokens.text_muted,
            );
            ui.label(label);
        }
    }

    /// The action-log slide-in (step 14 owns the content; the panel and its toggle are step 7's so
    /// "drawers visible" is testable now).
    fn action_log_placeholder(&mut self, ui: &mut Ui) {
        let tokens = self.tokens;
        ui.label(
            egui::RichText::new("Action log")
                .color(tokens.text_main)
                .strong(),
        );
        ui.label(egui::RichText::new("records appear here from step 9").color(tokens.text_muted));
    }

    /// The status line: what a user glances at — jobs, panes, engine step.
    #[must_use]
    pub fn status_line(&self) -> String {
        let live = self.jobs.live_count();
        let jobs = if live == 0 {
            "idle".to_string()
        } else {
            format!("{live} job(s)")
        };
        format!(
            "zenman step {} · {} pane(s) · {jobs}",
            SHELL_STEP,
            self.model.panes.len()
        )
    }

    /// Apply pending header toggles (kept out of `header` so the closure borrows stay apart).
    fn apply_toggles(&mut self, toggle: &DrawerToggle) {
        if toggle.single {
            self.set_layout(panes_for(LayoutMode::Single));
        }
        if toggle.split {
            self.set_layout(panes_for(LayoutMode::Split));
        }
        if toggle.quad {
            self.set_layout(quad_slots());
        }
        if toggle.left {
            self.model.left_drawer.open = !self.model.left_drawer.open;
        }
        if toggle.right {
            self.model.right_drawer.open = !self.model.right_drawer.open;
        }
        if toggle.log {
            self.model.log_panel = !self.model.log_panel;
        }
    }

    // ---------------------------------------------------------------------------- keys (step 10a)

    /// The per-frame keyboard pass (plan §5.8 `keymap.rs`, decision **D7**).
    ///
    /// Runs from [`Self::logic`], i.e. before the draw, so a command mutates the state the frame is
    /// about to paint — the same order the pane commands run in, and for the same reason.
    ///
    /// Plan **F15**: the whole pass is skipped while a text field owns the keyboard (the address bar,
    /// and from 10c the in-cell rename), which is what makes Go's bare <kbd>H</kbd>
    /// (`action-log-init.js:16`) safe to bind. A dock float that owns typing is skipped too — its
    /// keys belong to it ([`crate::dock_content::forward_window_keys`], plan F16).
    pub fn handle_keys(&mut self, ctx: &egui::Context) {
        if ctx.text_edit_focused() || craft_ui::docking::typing_window(ctx).is_some() {
            return;
        }
        let commands = ctx.input(|input| self.keys.lookup_events(&input.events));
        let slot = self.model.active_pane;
        for command in commands {
            self.dispatch(slot, command, ctx);
        }
    }

    /// Run one keymap command against one pane — the seam plan §5.8's menu row shares with the
    /// keyboard, because a menu item and its chord must do the *same* thing (D7: one table, two
    /// front ends). 10b's `on_command` callback is meant to call this.
    ///
    /// Every command that needs a tab, a selection or a clipboard and does not find one is a
    /// **no-op with a toast** — never a panic, never silence.
    pub fn dispatch(&mut self, slot: PaneSlot, command: Command, ctx: &egui::Context) {
        match command {
            Command::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Command::ActionLog => self.model.log_panel = !self.model.log_panel,
            Command::ToggleLeftDrawer => {
                self.model.left_drawer.open = !self.model.left_drawer.open;
            }
            Command::ToggleRightDrawer => {
                self.model.right_drawer.open = !self.model.right_drawer.open;
            }
            Command::SingleLayout => self.set_layout(panes_for(LayoutMode::Single)),
            // Go's F3/F4 *toggle* (`setSplitView(!state.isSplit)`, `setQuadView(!state.isQuad)`,
            // `keyboard.js:194-195`), so the single pane is the chords' off-state — which is why
            // `SingleLayout` needs no chord of its own.
            Command::SplitLayout => {
                let to = if self.model.panes.len() > 1 {
                    panes_for(LayoutMode::Single)
                } else {
                    panes_for(LayoutMode::Split)
                };
                self.set_layout(to);
            }
            Command::QuadLayout => {
                let to = if self.model.quad() {
                    panes_for(LayoutMode::Single)
                } else {
                    quad_slots()
                };
                self.set_layout(to);
            }
            Command::Refresh => self.refresh(slot),
            Command::ToggleView => self.run_command(slot, PaneCommand::ToggleGrid),
            Command::NewTab => self.run_command(slot, PaneCommand::NewTab),
            Command::CloseTab => self.close_shown_tab(slot),
            Command::NextTab => self.cycle_tab(slot, 1),
            Command::PrevTab => self.cycle_tab(slot, -1),
            Command::CycleTabsMru => self.cycle_mru_tab(slot),
            Command::GoUp => self.run_command(slot, PaneCommand::Up),
            Command::SelectAll => self.run_command(slot, PaneCommand::SelectAll),
            Command::SelectNone => self.escape(slot),
            Command::InvertSelection => self.invert_selection(slot),
            Command::FocusSearch => {
                if let Some(pane) = self.model.panes.get_mut(&slot) {
                    pane.address_editing = true;
                }
            }
            Command::MoveUp
            | Command::MoveDown
            | Command::MoveTop
            | Command::MoveBottom
            | Command::PageUp
            | Command::PageDown => self.move_cursor(slot, command),
            Command::Copy => self.clipboard_store(slot, ClipOp::Copy, ClipScope::Selected),
            Command::Cut => self.clipboard_store(slot, ClipOp::Cut, ClipScope::Selected),
            Command::CopyInside => self.clipboard_store(slot, ClipOp::Copy, ClipScope::Inside),
            Command::CutInside => self.clipboard_store(slot, ClipOp::Cut, ClipScope::Inside),
            Command::CopyPath | Command::CopyName => {
                let named = command == Command::CopyName;
                let text = self
                    .selection_abs(slot)
                    .iter()
                    .map(|path| {
                        let path = path.as_path();
                        if named {
                            path.file_name()
                                .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
                        } else {
                            path.to_string_lossy().into_owned()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if text.is_empty() {
                    self.nothing_selected("copy");
                } else {
                    // Go's `navigator.clipboard.writeText` (`context-menu.js:582-591`); egui's way
                    // out is the `CopyText` output command, which the shell's backend answers.
                    ctx.copy_text(format!("{text}\n"));
                }
            }
            Command::Paste => match self.shown_dir_abs(slot) {
                Some(dest) => self.run_paste(slot, dest, None),
                None => self.no_directory(command),
            },
            Command::OpenInNewTab => match self.single_selected_dir(slot) {
                Some(dir) => {
                    // Go's `open-in-new-tab` (`keyboard.js:230-234`) demands one selected directory
                    // — `getSelectedDir()` — and opens the *pane's* new tab there.
                    self.open_tab(slot, FsPath::from_abs(dir));
                }
                None => self.notices.post(
                    toast::Level::Warning,
                    "open in new tab: no directory selected",
                ),
            },
            Command::PasteInside => match self.single_selected_dir(slot) {
                Some(dest) => self.run_paste(slot, dest, None),
                None => self.notices.post(
                    toast::Level::Warning,
                    "paste inside: select exactly one directory",
                ),
            },
            Command::PasteLink => match self.shown_dir_abs(slot) {
                Some(dest) => self.run_paste(slot, dest, Some(PasteMode::Link)),
                None => self.no_directory(command),
            },
            Command::Delete => self.run_delete(slot),
            Command::PermanentDelete => {
                // Plan **D6**: a permanent delete needs the user's answer, and the dialog that asks
                // it is step 10c's. Until that exists the only honest channel is this toast, and
                // nothing is destroyed. Recorded as a gap in plan §9.2's step-10a row.
                let count = self.selection_abs(slot).len();
                self.notices.post(
                    toast::Level::Warning,
                    if count == 0 {
                        "delete permanently: nothing selected".to_string()
                    } else {
                        format!(
                            "delete permanently: {count} item(s) need the confirmation dialog \
                             (step 10c) — nothing was deleted"
                        )
                    },
                );
            }
            Command::Rename => self.start_rename(slot),
            Command::Open => self.open_selection(slot),
            Command::NewFolder => {
                // `Session::mkdir` needs a name; Go asked for it with `window.prompt`, and the real
                // prompt is step 10c's, so the request is recorded and said out loud.
                self.new_folder_request = Some(slot);
                self.notices.post(
                    toast::Level::Info,
                    "new folder: the name prompt arrives with step 10c",
                );
            }
            Command::QuickFind => self.notices.post(
                toast::Level::Info,
                "quick find arrives with step 11 (`dialogs/quick_find.rs`)",
            ),
            Command::SearchManager => self.notices.post(
                toast::Level::Info,
                "the search manager arrives with step 12 (`dialogs/search_manager.rs`)",
            ),
            Command::Properties => self.notices.post(
                toast::Level::Info,
                "the properties dialog arrives with step 13 (`dialogs/properties.rs`)",
            ),
            Command::AddToFavorites => self.notices.post(
                toast::Level::Info,
                "favorites are written by step 14's places service",
            ),
            Command::OpenContextMenu => {
                let at = ctx.input(|i| i.pointer.hover_pos());
                if let Some(at) = at {
                    self.context_request = Some((slot, None, at));
                }
            }
            Command::CancelJob => {
                let live = self
                    .jobs
                    .foreground
                    .iter()
                    .rev()
                    .find(|job| job.live)
                    .cloned();
                match live {
                    Some(job) => {
                        let word = if self.session.cancel(job.id) {
                            "cancelling"
                        } else {
                            "already finished:"
                        };
                        self.notices
                            .post(toast::Level::Info, format!("{word} {}", job.label));
                    }
                    None => self.notices.post(toast::Level::Info, "no job to cancel"),
                }
            }
        }
    }

    // -------------------------------------------------------------------------- dispatch helpers

    /// The shown tab of `slot`, if the pane has one.
    fn shown_tab(&self, slot: PaneSlot) -> Option<&Tab> {
        self.model.panes.get(&slot)?.active_tab()
    }

    /// The id of `slot`'s shown tab (job correlation, plan §4.3).
    fn shown_tab_id(&self, slot: PaneSlot) -> Option<TabId> {
        self.shown_tab(slot).map(|tab| tab.id)
    }

    /// The shown directory as an absolute path — a paste destination. `None` on an archive path
    /// (plan §4.2: only a local path can be written to).
    fn shown_dir_abs(&self, slot: PaneSlot) -> Option<AbsPath> {
        self.shown_tab(slot)?.path.host_abs()
    }

    /// The selection's absolute paths, in listing order (plan §5.8's display-order fix). Archive
    /// rows have no host path, so they are simply not part of a selection the engine can act on.
    fn selection_abs(&self, slot: PaneSlot) -> Vec<AbsPath> {
        let Some(tab) = self.shown_tab(slot) else {
            return Vec::new();
        };
        tab.entries
            .entries
            .iter()
            .filter(|entry| tab.sel.contains(Tab::row_key(entry)))
            .filter_map(|entry| tab.row_abs(entry))
            .collect()
    }

    /// The one selected row when it is a directory — Go's `getSelectedDir()`
    /// (`keyboard.js:139-150`), which demands *exactly one* selection for the `-inside` chords.
    fn single_selected_dir(&self, slot: PaneSlot) -> Option<AbsPath> {
        let tab = self.shown_tab(slot)?;
        if tab.sel.len() != 1 {
            return None;
        }
        let entry = tab
            .entries
            .entries
            .iter()
            .find(|entry| tab.sel.contains(Tab::row_key(entry)))?;
        if !entry.is_dir {
            return None;
        }
        tab.row_abs(entry)
    }

    fn nothing_selected(&mut self, what: &str) {
        self.notices
            .post(toast::Level::Warning, format!("{what}: nothing selected"));
    }

    fn no_directory(&mut self, command: Command) {
        self.notices.post(
            toast::Level::Warning,
            format!("{}: this tab lists no local directory", command.label()),
        );
    }

    fn toast_error(&mut self, what: &str, e: CoreError) {
        self.notices
            .post(toast::Level::Error, format!("{what}: {}", e.user_message()));
    }

    /// File a job the write path just started — the same bookkeeping `list_at` does for a listing
    /// (plan §4.3's correlation rule: the origin, not the job id, is what ties it back to a tab).
    fn note_running(&mut self, slot: PaneSlot, job: JobId, origin: Origin) {
        if let Some(info) = self.session.job_info(job) {
            let tab_id = self.shown_tab_id(slot);
            self.jobs.track(info, Some(slot), tab_id);
        }
        if let Some(pane) = self.model.panes.get_mut(&slot) {
            pane.loading.insert(origin);
            if let Some(tab) = pane.active_tab_mut() {
                tab.pending.insert(origin);
            }
        }
    }

    /// <kbd>F5</kbd>: bypass the directory cache on purpose (plan F13 — an explicit refresh is a
    /// different question from a re-paint).
    fn refresh(&mut self, slot: PaneSlot) {
        if let Some(path) = self.shown_tab(slot).map(|tab| tab.path.clone()) {
            self.session.invalidate(&path, ChangeCause::External);
        }
        self.list(slot, false, false);
    }

    /// <kbd>Escape</kbd>, in Go's own order: close the thing that is open, and only clear the
    /// selection when nothing is open (`app.js:276` address bar, `action-log.js:231` panel,
    /// `keyboard.js:158,166` overlays).
    fn escape(&mut self, slot: PaneSlot) {
        if self
            .model
            .panes
            .get_mut(&slot)
            .is_some_and(|pane| pane.address_editing)
        {
            if let Some(pane) = self.model.panes.get_mut(&slot) {
                pane.address_editing = false;
            }
            return;
        }
        if self.context_request.take().is_some() {
            return;
        }
        if self.model.log_panel {
            self.model.log_panel = false;
            return;
        }
        if let Some(tab) = self
            .model
            .panes
            .get_mut(&slot)
            .and_then(|p| p.active_tab_mut())
        {
            if tab.rename_target.take().is_some() {
                return;
            }
            // Go cleared the marks and kept the anchor — [`Selection::clear`], not `reset`.
            tab.sel.clear();
        }
    }

    fn invert_selection(&mut self, slot: PaneSlot) {
        let Some(tab) = self
            .model
            .panes
            .get_mut(&slot)
            .and_then(|p| p.active_tab_mut())
        else {
            return;
        };
        let keys: Vec<String> = tab
            .entries
            .entries
            .iter()
            .map(|entry| Tab::row_key(entry).to_string())
            .collect();
        for (ix, key) in keys.iter().enumerate() {
            tab.sel.apply(
                crate::widgets::virtual_list::SelKind::Add,
                ix,
                keys.len(),
                |_| Some(key.as_str()),
            );
        }
    }

    /// <kbd>Ctrl+W</kbd> — close the shown tab, with the pane's index bookkeeping identical to the
    /// strip's own close button.
    fn close_shown_tab(&mut self, slot: PaneSlot) {
        let Some(pane) = self.model.panes.get(&slot) else {
            return;
        };
        if pane.tabs.is_empty() {
            self.notices
                .post(toast::Level::Warning, "close tab: no tab in this pane");
            return;
        };
        let ix = pane.active.min(pane.tabs.len() - 1);
        self.run_command(slot, PaneCommand::CloseTab(ix));
    }

    /// <kbd>Ctrl+Tab</kbd> / <kbd>Ctrl+Shift+Tab</kbd> — Go's `cycleTabs(dir)` (`keyboard.js:270-277`),
    /// wrapping, and a no-op when the pane holds one tab (Go's early return at `:272`).
    fn cycle_tab(&mut self, slot: PaneSlot, dir: i32) {
        let Some(pane) = self.model.panes.get_mut(&slot) else {
            return;
        };
        let len = pane.tabs.len();
        if len <= 1 {
            return;
        }
        let shown = pane.active.min(len - 1);
        let next = (shown as i32 + dir).rem_euclid(len as i32) as usize;
        pane.active = next;
        let id = pane.tabs[next].id;
        pane.touch_mru(id);
    }

    /// <kbd>Shift+Tab</kbd> / <kbd>Alt+Z</kbd> — the MRU walk. Go opens a held-modifier overlay
    /// (`keyboard.js:110-141`); the overlay is not this slice's, so the chord steps to the *second*
    /// most recent tab — which is where Go's overlay starts too (`selectedIndex = 1`, `:118`).
    fn cycle_mru_tab(&mut self, slot: PaneSlot) {
        let Some(pane) = self.model.panes.get_mut(&slot) else {
            return;
        };
        let Some(id) = pane
            .mru
            .get(1)
            .copied()
            .or_else(|| pane.mru.front().copied())
        else {
            return;
        };
        if let Some(ix) = pane.tabs.iter().position(|tab| tab.id == id) {
            pane.active = ix;
            pane.touch_mru(id);
        }
    }

    /// The arrow / Home / End / Page keys (plan §6.2's "Arrow/Home/End/PageUp/PageDown/Esc added"
    /// row — Go's file list had none of them).
    ///
    /// The cursor is a single-row selection moved over the pane's **visual** order
    /// ([`RowOrder`]), so group headers are skipped and a collapsed group cannot be walked into.
    fn move_cursor(&mut self, slot: PaneSlot, command: Command) {
        let Some(tab) = self
            .model
            .panes
            .get_mut(&slot)
            .and_then(|p| p.active_tab_mut())
        else {
            return;
        };
        let order = RowOrder::build(tab);
        let len = order.len();
        if len == 0 {
            return;
        }
        let rows: Vec<String> = (0..len)
            .map(|ix| order.key_at(ix).unwrap_or_default().to_string())
            .collect();
        let current = tab.sel.origin().and_then(|origin| {
            (0..len).find(|ix| rows.get(*ix).map(String::as_str) == Some(origin))
        });
        let page = (tab.scroll.last.saturating_sub(tab.scroll.first)).max(1) as i64;
        let delta = match command {
            Command::MoveUp => -1,
            Command::MoveDown => 1,
            Command::PageUp => -page,
            Command::PageDown => page,
            _ => 0,
        };
        let raw = match command {
            Command::MoveTop => 0,
            Command::MoveBottom => len as i64 - 1,
            _ => current.map_or(0, |ix| ix as i64 + delta),
        };
        let target = raw.clamp(0, len as i64 - 1) as usize;
        let Some(target) = nearest_selectable(target, len, |ix| {
            rows.get(ix).is_some_and(|key| !key.is_empty())
        }) else {
            return;
        };
        tab.sel.apply(
            crate::widgets::virtual_list::SelKind::Single,
            target,
            len,
            |ix| rows.get(ix).map(String::as_str),
        );
        // Keep the cursor on screen: `ScrollState::y` counts *rows*, and the painted span is the
        // page (`ScrollState::first..last`, written by the scroller every frame).
        let from = tab.scroll.y;
        if (target as f32) >= from + page as f32 || (target as f32) < from {
            tab.scroll.y = target as f32;
        }
    }

    /// <kbd>F2</kbd> — arm the in-cell rename on the single selected row. The editor is step 10c;
    /// this is the state it reads ([`Tab::rename_target`]), and the refusal is said out loud so the
    /// chord is never silently swallowed.
    fn start_rename(&mut self, slot: PaneSlot) {
        let key = self
            .shown_tab(slot)
            .filter(|tab| tab.sel.len() == 1)
            .and_then(|tab| tab.sel.keys().iter().next().cloned());
        let Some(key) = key else {
            self.notices
                .post(toast::Level::Warning, "rename: select exactly one row");
            return;
        };
        if let Some(tab) = self
            .model
            .panes
            .get_mut(&slot)
            .and_then(|p| p.active_tab_mut())
        {
            tab.rename_target = Some(key);
        }
    }

    /// <kbd>Enter</kbd> — Go's `triggerOpen`, with core's routing: a directory is *reported* in
    /// `navigated` and the pane goes there, a file is handed to the desktop, and each failure is
    /// toasted on its own (Go's single `lastErr`, made per-path).
    fn open_selection(&mut self, slot: PaneSlot) {
        let paths = self.selection_abs(slot);
        if paths.is_empty() {
            self.nothing_selected("open");
            return;
        }
        match self.session.open_paths(&paths) {
            Ok(report) => {
                if let Some(dir) = report.navigated.first() {
                    self.navigate_shown(slot, FsPath::from_abs(dir.clone()));
                }
                for (path, e) in &report.failed {
                    self.notices.post(
                        toast::Level::Error,
                        format!("open {}: {}", path.as_path().display(), e.user_message()),
                    );
                }
            }
            Err(e) => self.toast_error("open", e),
        }
    }

    /// The clipboard chords: `Copy`/`Cut` queue the selection, `CopyInside`/`CutInside` queue a
    /// directory's children (`ClipScope::Inside`, Go's `copy_inside`/`cut_inside`).
    fn clipboard_store(&mut self, slot: PaneSlot, op: ClipOp, scope: ClipScope) {
        let sources = self.selection_abs(slot);
        if sources.is_empty() {
            self.nothing_selected(match op {
                ClipOp::Copy => "copy",
                ClipOp::Cut => "cut",
            });
            return;
        }
        let req = ClipboardStore { op, sources, scope };
        if let Err(e) = self.session.clipboard_store(&req) {
            self.toast_error("clipboard", e);
        }
    }

    /// <kbd>Ctrl+V</kbd> / <kbd>Alt+V</kbd> / <kbd>Ctrl+Alt+V</kbd>.
    ///
    /// `mode == None` takes the clipboard's own op (a cut clipboard moves, a copy clipboard copies —
    /// that is `Session::paste_clipboard`); `Some(Link)` is Go's `paste-link`.
    /// `ConflictPolicy::DEFAULT` is `SkipExisting` — Go's silent skip — and the conflict prompt that
    /// replaces it is step 10c's (recorded as a gap).
    fn run_paste(&mut self, slot: PaneSlot, dest: AbsPath, mode: Option<PasteMode>) {
        let origin = self.session.next_origin();
        let receipt = match mode {
            None => self
                .session
                .paste_clipboard(dest, ConflictPolicy::DEFAULT, origin),
            Some(mode) => {
                let Some(clip) = self.session.clipboard() else {
                    self.notices
                        .post(toast::Level::Warning, "paste: the clipboard is empty");
                    return;
                };
                let req = PasteReq {
                    sources: clip.paths(),
                    dest,
                    mode,
                    policy: ConflictPolicy::DEFAULT,
                };
                self.session.paste(&req, origin)
            }
        };
        match receipt {
            Ok(Receipt::Running { job, origin }) => self.note_running(slot, job, origin),
            Ok(Receipt::Done(report)) => self.notices.post(
                toast::Level::Success,
                format!("paste: {}", report.summary()),
            ),
            Err(e) => self.toast_error("paste", e),
        }
    }

    /// <kbd>Delete</kbd> — **the trash path** (plan **D6**): `DeleteReq::to_trash`, which never
    /// escalates to destroying bytes without the user's answer. The tab reloads itself through the
    /// `DirChanged` the job emits.
    fn run_delete(&mut self, slot: PaneSlot) {
        let sources = self.selection_abs(slot);
        if sources.is_empty() {
            self.nothing_selected("delete");
            return;
        }
        let origin = self.session.next_origin();
        match self.session.delete(&DeleteReq::to_trash(sources), origin) {
            Ok(Receipt::Running { job, origin }) => self.note_running(slot, job, origin),
            Ok(Receipt::Done(report)) => self.notices.post(
                toast::Level::Success,
                format!("delete: {}", report.summary()),
            ),
            Err(e) => self.toast_error("delete", e),
        }
    }

    /// Navigate the shown tab of `slot` to `path` and list it (the Enter-on-a-directory path).
    fn navigate_shown(&mut self, slot: PaneSlot, path: FsPath) {
        self.remember_selection(slot);
        if let Some(tab) = self
            .model
            .panes
            .get_mut(&slot)
            .and_then(|p| p.active_tab_mut())
        {
            tab.navigate_to(path.clone(), tab_title(&path));
        }
        self.list(slot, false, false);
    }
}

/// The nearest row at or after `target` that is an entry row rather than a flat listing's group
/// header (falling back to the closest one before it), so an arrow key can never land on a header.
fn nearest_selectable(
    target: usize,
    len: usize,
    selectable: impl Fn(usize) -> bool,
) -> Option<usize> {
    (target..len).find(|ix| selectable(*ix)).or_else(|| {
        (0..=target.min(len.saturating_sub(1)))
            .rev()
            .find(|ix| selectable(*ix))
    })
}

/// The tab's title: the last path segment, `/` for the store root (Go used the same rule in
/// `tabs.js`, where the tab's text was the directory name).
#[must_use]
pub fn tab_title(path: &FsPath) -> String {
    match path.last_segment() {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => "/".to_string(),
    }
}

/// Which header buttons were pressed this frame.
#[derive(Clone, Copy, Default)]
struct DrawerToggle {
    single: bool,
    split: bool,
    quad: bool,
    left: bool,
    right: bool,
    log: bool,
}

/// The shell's build-step number, echoed in the status line (`apps/zenman` prints the crate
/// constants separately).
pub const SHELL_STEP: u32 = 7;

/// Is this dock panel part of the current grid? (the `registered` predicate of plan F17).
fn is_registered(panel: &dock::DockId) -> bool {
    // Every slot the grid can have is "registered" — the grid shape lives in the `Layout` itself,
    // and `reconcile` only asks this for panels the *file* named, all of which we accept.
    matches!(panel, dock::DockId::FilePane(_))
}

/// A small icon button with the token-derived visuals of `craft_ui::buttons::IconButtonStyle`.
///
/// Plan §2.2 names `IconButton::show` for this; its `paint_icon` callback receives a `&Ui`,
/// which cannot host the `egui::Image` widget the SVG registry needs (image loading is a `&mut Ui`
/// operation) — a fork-API correction recorded in §9.2. The frame painting therefore uses the
/// same `ButtonVisuals` types through the `Ui` it owns, so the visual language is unchanged and
/// the icon path is the registry's.
fn icon_button(
    ui: &mut Ui,
    tokens: &Tokens,
    icon: &str,
    tooltip: &str,
    selected: bool,
) -> egui::Response {
    let size = egui::vec2(
        tokens.header_height - tokens.header_button_inset,
        tokens.header_height - tokens.header_button_inset,
    );
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let visuals = if selected {
        craft_ui::buttons::ButtonVisuals {
            fill: tokens.bg_hover,
            stroke: egui::Stroke::new(1.0, tokens.accent),
            icon: tokens.text_active,
        }
    } else if response.hovered() {
        craft_ui::buttons::ButtonVisuals {
            fill: tokens.bg_hover,
            stroke: egui::Stroke::NONE,
            icon: tokens.text_main,
        }
    } else {
        craft_ui::buttons::ButtonVisuals {
            fill: tokens.transparent,
            stroke: egui::Stroke::NONE,
            icon: tokens.text_muted,
        }
    };
    if ui.is_rect_visible(rect) {
        let radius = egui::CornerRadius::same(tokens.corner_radius);
        if visuals.fill != tokens.transparent {
            ui.painter().rect_filled(rect, radius, visuals.fill);
        }
        if !visuals.stroke.is_empty() {
            ui.painter()
                .rect_stroke(rect, radius, visuals.stroke, egui::StrokeKind::Inside);
        }
        crate::icons::paint(ui, rect, icon, rect.height(), visuals.icon);
    }
    response.on_hover_text(tooltip)
}

/// A layout-mode button: [`icon_button`] with selection wired to the current grid.
fn layout_button(
    ui: &mut Ui,
    tokens: &Tokens,
    icon: &str,
    tooltip: &str,
    selected: bool,
) -> egui::Response {
    icon_button(ui, tokens, icon, tooltip, selected)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use zenman_core::platform;
    use zenman_core::session::SessionPaths;
    use zenman_core::store::config::ZenConfig;

    /// A session whose files live in memory-equivalent temp paths (tests only — the *crate* still
    /// never touches `std::fs`; `Session::open` is core's IO seam).
    pub(crate) fn memory_session() -> Session {
        let dir = std::env::temp_dir().join(format!("zenman-ui-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let paths = SessionPaths::new(
            dir.join("config.json"),
            dir.join("action.log"),
            None,
            dir.join("probe"),
        )
        .with_db_open(1, std::time::Duration::from_millis(1));
        let (session, _notes) = Session::open(
            paths,
            ZenConfig::default(),
            platform::default_env(),
            ZenConfig::default().limits(),
        )
        .expect("session opens over a fresh temp dir");
        session
    }

    #[test]
    fn the_app_opens_with_the_persisted_grid_and_no_panes_until_the_grid_is_set() {
        let session = memory_session();
        let app = ZenApp::new(session, vec!["test boot".into()]);
        // The default mode is Single (the persisted default) — one pane, consistent grid.
        assert_eq!(app.model.panes.len(), 1);
        assert!(app.model.layout_mode_is_consistent());
        let mut app = app;
        app.set_layout(panes_for(LayoutMode::Split));
        assert_eq!(
            app.model.panes.len(),
            2,
            "the plan row: the app can open with 2 panes"
        );
        assert!(app.model.layout_mode_is_consistent());
        app.set_layout(quad_slots());
        assert!(app.model.quad());
        assert!(app.dock.layout.validate().is_ok());
    }

    #[test]
    fn status_line_names_panes_and_idle() {
        let session = memory_session();
        let app = ZenApp::new(session, Vec::new());
        let line = app.status_line();
        assert!(line.contains("idle"), "{line}");
        assert!(line.contains("pane"), "{line}");
    }

    #[test]
    fn startup_notes_become_toasts() {
        let session = memory_session();
        let mut app = ZenApp::new(session, Vec::new());
        app.post_startup_notes(&[StartupNote::DatabaseDisabled {
            reason: "locked".into(),
        }]);
        assert_eq!(app.notices.queued().len(), 1);
    }
}
