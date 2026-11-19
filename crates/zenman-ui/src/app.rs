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
use zenman_core::mode::ModeBits;
use zenman_core::ops::ConflictPolicy;
use zenman_core::ops::chmod::ChmodReq;
use zenman_core::ops::clipboard::{ClipOp, ClipScope, ClipboardStore};
use zenman_core::ops::delete::{DeleteMode, DeleteReq};
use zenman_core::ops::mkdir::MkdirReq;
use zenman_core::ops::paste::{PasteMode, PasteReq};
use zenman_core::ops::rename::RenameReq;
use zenman_core::path::{AbsPath, FsPath};
use zenman_core::services::dirs::ListDir;
use zenman_core::session::Origin;
use zenman_core::session::{Receipt, Session, StartupNote};
use zenman_core::store::config::{LayoutMode, PaneSlot, ThemeMode};

use crate::dialogs::{
    self, Answer, Choice, Confirm, ConfirmButton, DialogKind, Prompt, Validation,
};
use crate::dock::{self, Dock};
use crate::jobs_ui::JobsUi;
use crate::keymap::{Command, KeyMap};
use crate::menus::{self, MenuAction, MenuContext, MenuSpec, MenuTarget};
use crate::panels::{
    self,
    file_pane::{PaneCommand, RowOrder},
};
use crate::persist::Persisted;
use crate::state::{AppModel, Tab, TabId, panes_for, quad_slots};
use crate::theme::{self, Tokens};
use crate::widgets::{disk_meter, toast};

/// The context menu a pane asked for, and the state that keeps it open across frames.
///
/// The plan's `menus::Registry` seam, as one option: at most one menu is open at a time (Go had one
/// `#context-menu` div, `index.html:623`), and it records *where* it was opened, not what it shows —
/// the rows are rebuilt from live state every frame, which is what lets a greyed `Paste` light up the
/// moment a `Copy` is clicked elsewhere in the same menu.
#[derive(Clone, Debug)]
pub struct OpenMenu {
    /// The pane it belongs to; the menu closes when that pane is gone (plan §4.3's ownership rule).
    pub slot: PaneSlot,
    /// Row or background, and for a row whether it was a directory (Go's `isDir`).
    pub target: MenuTarget,
    /// Where its top-left was asked to sit, in screen points.
    pub at: egui::Pos2,
    /// A fresh `Id` per opening: an `Area` remembers the position of a known id, so reusing one would
    /// redraw the second right-click at the first one's spot.
    pub id: egui::Id,
}

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
    /// The menu that is open *now* — the drawn half of `context_request` (step 10b).
    pub menu: Option<OpenMenu>,
    /// The keymap — **the** single source of truth for dispatch and for the menu hints 10b draws
    /// (plan D7).
    pub keys: KeyMap,
    /// The pane whose <kbd>Ctrl+Shift+N</kbd> armed the name prompt. It stays set while the prompt
    /// is up, which is what lets a second <kbd>Ctrl+Shift+N</kbd> be an honest no-op instead of a
    /// second dialog.
    pub new_folder_request: Option<PaneSlot>,
    /// The modal stack (step 10c): one prompt or one confirm, and its answer is a command.
    pub dialogs: dialogs::Dialogs,
    /// The extractions this shell started (step 13b): one modeless progress panel each, polled from
    /// the shell's tick, kept after they finish so the summary can be read (and closed).
    pub extracts: Vec<crate::dialogs::extract::ExtractProgress>,
    /// Has this session opened an archive store at all? The teardown pair (13a's handover) is worth
    /// running once a frame only while the answer is yes — an archive-free shell frame does not
    /// touch either cache.
    pub archives_touched: bool,
    /// The file drag in flight (step 10c, `drag_drop`): `None` when the pointer is not dragging rows.
    pub drag: Option<crate::drag_drop::DragPayload>,
    /// The header's workspace group: the dropdown, `New`/`Delete`, the `✓ Saved` chip and the
    /// autosave clock (plan §5.8 `panels/header.rs`, step 14b).
    pub workspaces: crate::panels::header::WorkspaceBar,
    /// The action-log panel's state (plan §5.8 `panels/action_log.rs`, step 14b).
    pub log: crate::panels::action_log::LogPanel,
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
            menu: None,
            keys: KeyMap::default(),
            new_folder_request: None,
            dialogs: dialogs::Dialogs::default(),
            extracts: Vec::new(),
            archives_touched: false,
            drag: None,
            workspaces: crate::panels::header::WorkspaceBar::default(),
            log: crate::panels::action_log::LogPanel::default(),
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
            menu: None,
            keys: KeyMap::default(),
            new_folder_request: None,
            dialogs: dialogs::Dialogs::default(),
            extracts: Vec::new(),
            archives_touched: false,
            drag: None,
            workspaces: crate::panels::header::WorkspaceBar::default(),
            log: crate::panels::action_log::LogPanel::default(),
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
        // Step 13b: an archive listing does not go through the directory cache. The manifest is the
        // listing (`Session::archive_list`, cached per host mtime inside core), it is whole — no
        // cursor, no `has_more`, no page job — and its failures are typed, so they become an inline
        // pane state *and* a toast rather than an empty pane (§7 row 13).
        if matches!(tab.path.store, zenman_core::path::VfsKind::Archive { .. }) {
            self.archives_touched = true;
            let path = tab.path.clone();
            match self.session.archive_list(&path) {
                Ok(entries) => {
                    let total = entries.len();
                    let page = std::sync::Arc::new(zenman_core::entry::DirPage {
                        path,
                        entries,
                        cursor: zenman_core::cursor::Cursor(0),
                        has_more: false,
                        total_count: total,
                        loaded_so_far: total,
                    });
                    if let Some(tab) = self.model.tab_at_mut((slot, ix)) {
                        if append {
                            tab.append_page(page);
                        } else {
                            tab.set_page_keeping_selection(page);
                        }
                        tab.error = None;
                        tab.loading_more = false;
                    }
                }
                Err(e) => {
                    let text = e.user_message();
                    if let Some(tab) = self.model.tab_at_mut((slot, ix)) {
                        let empty = std::sync::Arc::new(crate::state::empty_page(tab.path.clone()));
                        tab.entries = empty;
                        tab.error = Some(std::sync::Arc::new(e));
                        tab.loading_more = false;
                    }
                    self.notices.post(toast::Level::Error, text);
                }
            }
            return;
        }
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
                // Step 13b: entering a row can change **store**. `FsPath::enter`/`as_store` own the
                // rule (an archive-named row opens its container), and the cap is structural:
                // `MAX_NEST_DEPTH` is 2, so at depth 2 an archive-named member is a member and the
                // row activates as a file — "a third level does not open" (§7 row 13).
                let depth = self
                    .tab_mut_of(slot)
                    .map(|tab| tab.path.store_depth())
                    .unwrap_or_default();
                let opens_store = entry.is_dir
                    || (zenman_core::path::is_archive_name(&entry.name)
                        && depth < zenman_core::archive::MAX_NEST_DEPTH);
                let child = {
                    let Some(tab) = self.tab_mut_of(slot) else {
                        return;
                    };
                    let joined = if entry.rel_path.is_empty() {
                        tab.path.enter(&entry.name).ok()
                    } else {
                        tab.path.join_rel(&entry.rel_path).ok()
                    };
                    let joined = joined.or_else(|| tab.path.enter(&entry.name).ok());
                    joined.map(|joined| {
                        // `as_store` is the store switch; anything it refuses (a plain file, a
                        // depth-2 member) stays the plain joined path.
                        if opens_store {
                            joined.as_store().unwrap_or(joined)
                        } else {
                            joined
                        }
                    })
                };
                let Some(child) = child else { return };
                if opens_store {
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
            PaneCommand::BeginDrag { ix } => self.begin_drag(slot, ix),
            PaneCommand::DropInto { ix, copy } => self.drop_into_row(slot, ix, copy),
            PaneCommand::DropOnBackground { copy } => self.drop_into_pane(slot, copy),
            PaneCommand::CommitRename { ix, name } => self.commit_rename(slot, ix, name),
            PaneCommand::CancelRename => {
                if let Some(tab) = self.tab_mut_of(slot) {
                    tab.rename_target = None;
                    tab.rename_editor = None;
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

    // ------------------------------------------------------------------------ drag & drop (10c)

    /// Go's `dragstart` (`file-list.js:299-306`): a row outside the selection *replaces* it, and the
    /// payload is the selection, not the row. The mode is the clipboard's own op, which is what makes a
    /// Go drag "paste the clipboard into that folder" rather than a copy of what was grabbed.
    fn begin_drag(&mut self, slot: PaneSlot, ix: usize) {
        let paths = {
            let Some(tab) = self.shown_tab(slot) else {
                return;
            };
            let Some(entry) = tab.entries.entries.get(ix) else {
                return;
            };
            let key = Tab::row_key(entry);
            if tab.sel.contains(key) {
                self.selection_abs(slot)
            } else {
                tab.row_abs(entry)
                    .map(|path| vec![path])
                    .unwrap_or_default()
            }
        };
        if paths.is_empty() {
            return;
        }
        let mode = crate::drag_drop::grab_mode(self.session.clipboard().as_ref());
        self.drag = Some(crate::drag_drop::DragPayload {
            source: slot,
            paths,
            mode,
        });
    }

    /// The payload the release means, taken out of the app before the frame's end clears it.
    fn take_drag(&mut self, copy: bool) -> Option<crate::drag_drop::DragPayload> {
        let mut payload = self.drag.take()?;
        if copy {
            payload.mode = crate::drag_drop::DragMode::Copy;
        }
        Some(payload)
    }

    /// A drop on a directory row: Go's `executeFileOp('paste', sources, path)` (`file-list.js:320-330`).
    fn drop_into_row(&mut self, slot: PaneSlot, ix: usize, copy: bool) {
        let Some(payload) = self.take_drag(copy) else {
            return;
        };
        let Some(entry) = self
            .tab_mut_of(slot)
            .and_then(|tab| tab.entries.entries.get(ix).cloned())
        else {
            return;
        };
        if !entry.is_dir {
            // Unreachable through the pane, which only offers directory rows — but `PaneCommand` is a
            // public seam, and a drop on a file must be refused out loud, not pasted next to it.
            self.notices.post(
                toast::Level::Warning,
                format!("drop: {} is not a folder", entry.name),
            );
            return;
        }
        let Some(dest) = self.shown_tab(slot).and_then(|tab| tab.row_abs(&entry)) else {
            return;
        };
        self.drop_paste(&payload, slot, dest, payload.mode);
    }

    /// A drop on a pane's own background — `DropIntent::PasteInto{pane}`, into the directory it shows.
    fn drop_into_pane(&mut self, slot: PaneSlot, copy: bool) {
        let Some(payload) = self.take_drag(copy) else {
            return;
        };
        let Some(dest) = self.shown_dir_abs(slot) else {
            self.no_directory(Command::Paste);
            return;
        };
        self.drop_paste(&payload, slot, dest, payload.mode);
    }

    /// A drop the *dock* routed at a pane body rather than the pane's own rows (`DockContent`'s half of
    /// the seam, which the file drag does not normally reach).
    pub fn drop_from_dock(&mut self, slot: PaneSlot) {
        self.drop_into_pane(slot, false);
    }

    /// The drop's paste, behind the same collision pre-flight the clipboard chords run — so a drop can
    /// never clobber silently either.
    pub fn drop_paste(
        &mut self,
        payload: &crate::drag_drop::DragPayload,
        slot: PaneSlot,
        dest: AbsPath,
        mode: crate::drag_drop::DragMode,
    ) {
        let conflicts = self.paste_conflicts(&dest, &payload.paths);
        let same_dir = payload
            .paths
            .first()
            .and_then(|path| path.as_path().parent())
            .is_some_and(|parent| parent == dest.as_path());
        if same_dir && conflicts.is_empty() {
            // Dropping rows back into their own directory: Go performed the no-op happily, core refuses
            // it (plan F11), and the refusal is worth hearing rather than watching nothing happen.
            self.notices.post(
                toast::Level::Info,
                format!("{}: already there", mode.word().to_lowercase()),
            );
            return;
        }
        if !conflicts.is_empty() {
            self.open_paste_conflict_confirm(
                slot,
                dest,
                Some(mode.paste_mode()),
                payload.paths.clone(),
                conflicts,
            );
            return;
        }
        self.run_paste_with(
            slot,
            dest,
            Some(mode.paste_mode()),
            &payload.paths,
            ConflictPolicy::SkipExisting,
        );
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
        // Step 13b: the two archive polls, once per frame each (`Dialog::show` never touches the
        // engine, so the size answer and the extract progress are collected here).
        crate::dialogs::properties::tick(self);
        crate::dialogs::extract::tick(self);
        // 13a's teardown handover: collect finished extractions past the retention window and drop
        // stale manifests. Bounded work (both caches are capped by `Limits`), and only once the
        // session has actually opened a store.
        if self.archives_touched {
            self.session.archive_gc();
            self.session.archives().store().cache().evict_stale();
        }
        // The quick-find dialog's keystroke → engine seam (plan §5.8's `tick`), *after* the event
        // drain so a `SearchReady` never races the request that supersedes it in the same frame.
        crate::dialogs::quick_find::tick(self, ctx);
        // The Search Manager's poll/debounce (plan §5.8's `tick`, step 12c), same place for the same
        // reason: after the event drain, so an answer never races the request that superseded it.
        crate::dialogs::search_manager::tick(self, ctx);
        self.reload_stale();
        let now = Instant::now();
        self.last_frame = now;
        let dock_moved = dock::persist_pending(ctx, now);
        if dock_moved {
            let snap = self.snapshot();
            self.persist.autosave(&snap, now);
        }
        // Step 14b: the two 14a-backed clocks, on the same frame tick as `Persisted::autosave` —
        // the log panel's event-driven/poll refresh, and the workspace autosave + `✓ Saved` chip.
        crate::panels::action_log::tick(self);
        self.tick_workspaces(now, dock_moved);
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
        self.open_requested_menu();
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
        // **Last**, on the foreground layer: in this egui fork the last-allocated overlapping rect
        // wins hover and click (plan §9.2 step 8), so the menu is drawn after every panel, the dock
        // and the toast stack, and nothing can steal its rows.
        // The extraction panels, on the foreground layer but **modeless** (see `dialogs/extract`'s
        // module doc): a modal would block the pane the extraction repopulates.
        crate::dialogs::extract::show(self, ui, &tokens);
        self.draw_menu(ui.ctx());
        // **Last**, above the menu: a modal is the only thing on screen that may answer a key, and
        // `handle_keys` has already stood down for it. Its answers run here, after the draw, exactly
        // like the pane commands — a dialog never touches the engine itself.
        let answers = self.dialogs.show(ui, &self.tokens);
        for answer in answers {
            self.answer(answer);
        }
        // The manager's non-closing requests (Rebuild / Apply / the watch toggle) run here, after the
        // draw, exactly like the answers — a dialog still never calls the engine itself.
        crate::dialogs::search_manager::drain(self);
        // The drag ghost, on the foreground layer, paint-only: it must never claim a click from the row
        // under the pointer (`drag_drop::paint_ghost`).
        if let Some(payload) = self.drag.clone()
            && let Some(at) = ui.ctx().pointer_hover_pos()
        {
            crate::drag_drop::paint_ghost(ui, &self.tokens, &payload, at);
        }
        // The button came up: whatever did not claim the release this frame never will. The targets ran
        // first (they `take` the payload), so this is the "dropped on nothing" case.
        if ui.ctx().input(|i| i.pointer.any_released()) {
            self.drag = None;
        }
    }

    /// The 48 px header (plan §2.2: an `egui::TopBottomPanel`, *not* a dock panel — `Side` has no
    /// `Top`).
    fn header(&mut self, ui: &mut Ui, toggle: &mut DrawerToggle) {
        use crate::panels::header;
        let tokens = self.tokens;
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Zen-Man")
                    .color(tokens.text_active)
                    .strong(),
            );
            // The workspace group (plan §5.8 `panels/header.rs`, step-8c review gap 1):
            // `Workspace: <select>` · `New` · `Delete` · `✓ Saved` · the disk meter. It emits
            // commands like a pane does; the shell answers them, and the answer may open a dialog,
            // so nothing here is called from inside the draw closure.
            let disk = AbsPath::from_path("/")
                .ok()
                .and_then(|root| self.session.env().disk_space(&root).ok())
                .map(|space| (space.free, space.total));
            self.workspaces.refresh(self.session.workspaces());
            let commands = self.workspaces.show(ui, &tokens, disk, Instant::now());
            self.header_commands(commands);
            if let Some(line) = self.boot_lines.first() {
                ui.label(
                    egui::RichText::new(line)
                        .color(tokens.text_muted)
                        .size(tokens.font_size - 2.0),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Right-to-left, so these come out in the reference's order: `History` ·
                // `2-Pane (F3)` · `4-Pane (F4)`. The chord text is `KeyMap::hint`'s own spelling
                // (plan D7), never a second table.
                let quad = self.model.quad();
                let split = self.model.layout == LayoutMode::Split && !quad;
                let single = self.model.layout == LayoutMode::Single && !quad;
                let quad_hint = self.keys.hint_text(Command::QuadLayout);
                let split_hint = self.keys.hint_text(Command::SplitLayout);
                if header::chord_button(ui, &tokens, format!("4-Pane ({quad_hint})"), quad) {
                    toggle.quad = true;
                }
                if header::chord_button(ui, &tokens, format!("2-Pane ({split_hint})"), split) {
                    toggle.split = true;
                }
                if header::chord_button(ui, &tokens, "History".to_string(), self.model.log_panel) {
                    toggle.log = true;
                }
                ui.separator();
                if layout_button(ui, &tokens, "list", "single pane", single).clicked() {
                    toggle.single = true;
                }
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
        crate::panels::action_log::show(self, ui);
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
            self.toggle_log_panel();
        }
    }

    // ---------------------------------------------------------------------------- menus (step 10b)

    /// Adopt a recorded [`PaneCommand::Context`](crate::panels::file_pane::PaneCommand::Context) as
    /// the open menu. Runs at the top of the draw, because the request arrives at the *end* of the
    /// previous frame (the panes' commands run after the dock, [`crate::dock_content::show`]) — which
    /// is also what keeps the opening right-click from being the click-away that closes it.
    fn open_requested_menu(&mut self) {
        let Some((slot, ix, at)) = self.context_request.take() else {
            return;
        };
        let target = match ix {
            Some(ix) => {
                let on_dir = self
                    .model
                    .panes
                    .get(&slot)
                    .and_then(|pane| pane.active_tab())
                    .and_then(|tab| tab.entries.entries.get(ix))
                    .is_some_and(|entry| entry.is_dir);
                MenuTarget::Row { on_dir }
            }
            None => MenuTarget::Background,
        };
        self.menu = Some(OpenMenu {
            slot,
            target,
            at,
            id: egui::Id::unique("zenman-menu"),
        });
    }

    /// What the open menu shows, as data — the same call [`Self::draw_menu`] makes, so a test can read
    /// the exact labels, chords and enabled states without scraping a frame.
    #[must_use]
    pub fn menu_specs(&self) -> Vec<Vec<MenuSpec>> {
        let Some(open) = &self.menu else {
            return Vec::new();
        };
        let ctx = self.menu_context(open.slot, open.target);
        let actions = self.session.config().custom_actions;
        let sel = self.action_context(open.slot);
        menus::file_menu(&ctx, &sel, &actions)
    }

    /// The context the builders ask about, derived from live state field by field — the plan row's
    /// "a `MenuContext` field that cannot be populated yet is a documented gap, not a hardcoded
    /// `true`".
    #[must_use]
    pub fn menu_context(&self, slot: PaneSlot, target: MenuTarget) -> MenuContext {
        let tab = self
            .model
            .panes
            .get(&slot)
            .and_then(|pane| pane.active_tab());
        let has_selection = tab.is_some_and(|tab| !tab.sel.is_empty());
        // Go's `copy_inside`/`cut_inside` queue a directory's *children*, so they are offered only
        // when every selected row can answer for a directory (`api/ops.go`'s `copyInside`).
        let all_dirs = tab.is_some_and(|tab| {
            let mut any = false;
            let mut all = true;
            for entry in &tab.entries.entries {
                if tab.sel.contains(crate::state::Tab::row_key(entry)) {
                    any = true;
                    all &= entry.is_dir;
                }
            }
            any && all
        });
        let clip = self.session.clipboard();
        let on_archive = tab.is_some_and(|tab| tab.path.is_archive_file());
        MenuContext {
            has_selection,
            all_dirs,
            has_clipboard: clip.as_ref().is_some_and(|clip| !clip.is_empty()),
            clip_is_cut: clip.as_ref().is_some_and(|clip| clip.op == ClipOp::Cut),
            // `is_archive_file` is core's own read-only-store predicate (plan §4.2); it counts a real
            // directory named `fake.zip`, which is Go's `dir-root.json` parity case, not a bug here.
            on_archive,
            // The probed capability (`platform::Env::supports_symlink`, plan F21: a lock read, so this
            // is called while a menu is open and never per frame). An unprobed environment is `false`:
            // no capability we cannot confirm is one we will not offer.
            can_symlink: self.session.env().supports_symlink().is_ok_and(|ok| ok),
            // 14b: the real source. Go's `triggerRename` sent a selection of more than one row to
            // `batch-rename.js`, so "mid-batch" *is* "more than one row is selected".
            in_batch: tab.is_some_and(|tab| tab.sel.len() > 1),
            target,
        }
    }

    /// Draw the open menu (if any) and route what was clicked through [`Self::dispatch`] — the same
    /// function the chord runs (plan **D7**), so a menu item cannot behave differently from its key.
    fn draw_menu(&mut self, ctx: &egui::Context) {
        // A menu must not outlive the pane it was opened in.
        if self.menu.as_ref().is_some_and(|open| {
            !self
                .model
                .panes
                .get(&open.slot)
                .is_some_and(|pane| pane.active_tab().is_some())
        }) {
            self.menu = None;
        }
        let Some(open) = self.menu.clone() else {
            return;
        };
        let groups = self.menu_specs();
        let menu_ctx = self.menu_context(open.slot, open.target);
        let mut clicked = Vec::new();
        let outcome = menus::show(
            ctx,
            open.id,
            open.at,
            &menu_ctx,
            &groups,
            menus::MenuStyle {
                keys: &self.keys,
                tokens: &self.tokens,
            },
            &mut clicked,
        );
        if outcome.close {
            self.menu = None;
        }
        for action in clicked {
            self.menu = None;
            match action {
                MenuAction::Command(command) => self.dispatch(open.slot, command, ctx),
                MenuAction::Custom(action) => self.run_action(open.slot, &action),
            }
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
        if ctx.text_edit_focused()
            || craft_ui::docking::typing_window(ctx).is_some()
            || self.dialogs.is_open()
        {
            // Plan **F15**, in all three of its shapes: a text field (the address bar, the in-cell
            // rename) owns the keys, a dock float owns its own, and a modal owns everything.
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
        // Step 13b: inside an archive the write ladder is refused **here**, with the reason. The menus
        // already grey these rows out (`MenuContext::on_archive`), but a chord, a menu row or a drag
        // that still arrives gets an answer rather than a silent no-op or an engine error several
        // frames later — plan §5.8's named Go bug was Delete offered over a read-only VFS.
        const READ_ONLY_REFUSED: &[Command] = &[
            Command::NewFolder,
            Command::Paste,
            Command::PasteInside,
            Command::PasteLink,
            Command::Cut,
            Command::CutInside,
            Command::Delete,
            Command::PermanentDelete,
            Command::Rename,
            Command::Chmod,
        ];
        if READ_ONLY_REFUSED.contains(&command) && self.in_archive(slot) {
            self.notices.post(
                toast::Level::Warning,
                format!(
                    "{}: an archive listing is read-only — extract it first",
                    self.keys.hint_text(command)
                ),
            );
            return;
        }
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
            Command::Chmod => self.open_chmod_prompt(slot),
            Command::PermanentDelete => self.open_permanent_delete_confirm(slot),
            // Go's `triggerRename`: one row renames in cell, more than one goes to batch rename
            // (`context-menu.js:404-410`) — the same command, two arms, one dispatch.
            Command::Rename if self.selection_abs(slot).len() > 1 => self.open_batch_rename(slot),
            Command::Rename => self.start_rename(slot),
            Command::Open => self.open_selection(slot),
            Command::NewFolder => self.open_new_folder_prompt(slot),
            Command::QuickFind => match self.shown_dir_abs(slot) {
                // Step 11b: the live search dialog, scoped to the pane's directory (Go's
                // `openQuickFind` reads `getActiveTab().currentPath`, `quick-find.js:26`).
                Some(dir) => crate::dialogs::quick_find::open(self, slot, dir),
                None => self.no_directory(command),
            },
            Command::SearchManager => crate::dialogs::search_manager::open(self, slot),
            Command::Properties => crate::dialogs::properties::open(self, slot),
            // Step 13b: "extract shows progress + cancel". The panel is the progress; the cancel is
            // in it. Go's default (`ConflictPolicy::DEFAULT`, the same rule the paste path uses).
            Command::Extract => self.extract_here(slot, zenman_core::ops::ConflictPolicy::DEFAULT),
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
    pub(crate) fn shown_dir_abs(&self, slot: PaneSlot) -> Option<AbsPath> {
        self.shown_tab(slot)?.path.host_abs()
    }

    /// Is the shown tab inside an archive store? (The read-only predicate for the whole shell —
    /// `on_archive` in the menus, the guard in [`Self::dispatch`], the extract command.)
    #[must_use]
    pub fn in_archive(&self, slot: PaneSlot) -> bool {
        self.shown_tab(slot)
            .is_some_and(|tab| matches!(tab.path.store, zenman_core::path::VfsKind::Archive { .. }))
    }

    /// Extract the shown archive (or its selection) into the directory that holds it.
    ///
    /// The conflict policy is the caller's, because `ops::ConflictPolicy` is the engine's own answer
    /// to a collision at the destination and the UI must not invent a second one.
    pub fn extract_here(&mut self, slot: PaneSlot, policy: zenman_core::ops::ConflictPolicy) {
        let Some(tab) = self.shown_tab(slot).cloned() else {
            self.notices
                .post(toast::Level::Warning, "nothing to extract");
            return;
        };
        if !self.in_archive(slot) {
            self.notices.post(
                toast::Level::Warning,
                "extract works on an archive listing — open one first",
            );
            return;
        }
        self.archives_touched = true;
        // The selection's internal paths; nothing selected means the whole container (Go's
        // `sources: []` rule, `api/archive_extract.go:96`).
        let sources: Vec<String> = tab
            .entries
            .entries
            .iter()
            .filter(|e| tab.sel.keys().contains(crate::state::Tab::row_key(e)))
            .map(|e| {
                if e.rel_path.is_empty() {
                    e.name.clone()
                } else {
                    e.rel_path.clone()
                }
            })
            .collect();
        crate::dialogs::extract::start(self, slot, &tab.path, sources, policy);
    }

    /// The selection's absolute paths, in listing order (plan §5.8's display-order fix). Archive
    /// rows have no host path, so they are simply not part of a selection the engine can act on.
    pub(crate) fn selection_abs(&self, slot: PaneSlot) -> Vec<AbsPath> {
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

    pub(crate) fn nothing_selected(&mut self, what: &str) {
        self.notices
            .post(toast::Level::Warning, format!("{what}: nothing selected"));
    }

    fn no_directory(&mut self, command: Command) {
        self.notices.post(
            toast::Level::Warning,
            format!("{}: this tab lists no local directory", command.label()),
        );
    }

    // --------------------------------------------------------------------------- dialogs (10c)

    /// Put a modal up. The context menu comes down first: two foreground layers that both answer the
    /// pointer is how step 10b's overlapping-rect repaint was born, and a dialog is modal in Go's own
    /// sense — `window.prompt` took the whole page down with it.
    pub(crate) fn open_dialog(&mut self, dialog: dialogs::Dialog) -> bool {
        if self.dialogs.is_open() {
            self.notices
                .post(toast::Level::Warning, "a dialog is already open");
            return false;
        }
        self.menu = None;
        self.context_request = None;
        self.dialogs.open(dialog)
    }

    /// The loaded listing's names — the fast "that name is taken" list the prompts show. The engine's
    /// guard stays the authority; this only saves a round trip on the common case.
    fn shown_names(&self, slot: PaneSlot) -> Vec<String> {
        self.shown_tab(slot)
            .map(|tab| {
                tab.entries
                    .entries
                    .iter()
                    .map(|entry| entry.name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Go's `prompt('Enter new folder name:', 'New Folder')` (`context-menu.js:548-549`).
    fn open_new_folder_prompt(&mut self, slot: PaneSlot) {
        self.new_folder_request = Some(slot);
        let mut prompt = Prompt::new(
            DialogKind::NewFolder { slot },
            "New folder",
            "Enter new folder name:",
            "New Folder",
            "Create",
            Validation::Name,
        );
        prompt.taken = self.shown_names(slot);
        prompt.select = Some("New Folder".len());
        self.open_dialog(dialogs::Dialog::Prompt(prompt));
    }

    /// Go's `triggerChmod` prompt (`context-menu.js:475-490`), message and default included. Go
    /// seeded it with the literal `'755'`; the port seeds it with the selected row's **own** mode, in
    /// the octal spelling [`ChmodReq`] logs, because a prompt that lies about the current state is the
    /// kind of thing that changes a permission by accident.
    fn open_chmod_prompt(&mut self, slot: PaneSlot) {
        let paths = self.selection_abs(slot);
        if paths.is_empty() {
            self.nothing_selected("change permissions");
            return;
        }
        let seed = self
            .shown_tab(slot)
            .and_then(|tab| {
                tab.entries
                    .entries
                    .iter()
                    .find(|entry| tab.sel.contains(Tab::row_key(entry)))
            })
            .map(|entry| octal_text_of(&entry.mode))
            .unwrap_or_else(|| "755".to_string());
        let mut prompt = Prompt::new(
            DialogKind::Chmod { slot, paths },
            "Change permissions",
            format!(
                "Changing permissions for {} item(s).\n\
                 Common Permissions:\n\
                 \u{20}755 - Read/Write/Exec for owner, Read/Exec for others (Standard for folders & scripts)\n\
                 \u{20}644 - Read/Write for owner, Read-only for others (Standard for files)\n\
                 \u{20}700 - Full access for owner only (Private folders & scripts)\n\
                 \u{20}600 - Read/Write for owner only (Private configuration & keys)\n\
                 \u{20}777 - Full public access (Insecure)\n\
                 \n\
                 Enter 3 or 4-digit octal permissions:",
                // The count is the *selection*, which is what Go printed (`selectedPaths.size`).
                self.selection_abs(slot).len()
            ),
            seed.clone(),
            "Apply",
            Validation::Octal,
        );
        prompt.select = Some(seed.len());
        self.open_dialog(dialogs::Dialog::Prompt(prompt));
    }

    /// Plan **D6**'s question, in Go's copy (`context-menu.js:507`) with the part a browser `confirm`
    /// could not carry: *which* paths, and what "permanently" costs.
    fn open_permanent_delete_confirm(&mut self, slot: PaneSlot) {
        let paths = self.selection_abs(slot);
        if paths.is_empty() {
            self.nothing_selected("delete permanently");
            return;
        }
        let count = paths.len();
        let mut named: Vec<String> = paths
            .iter()
            .take(3)
            .map(|path| {
                path.as_path().file_name().map_or_else(
                    || path.as_path().display().to_string(),
                    |name| name.to_string_lossy().into_owned(),
                )
            })
            .collect();
        if count > named.len() {
            named.push(format!("and {} more", count - named.len()));
        }
        let confirm = Confirm::new(
            DialogKind::PermanentDelete { slot, paths },
            // The heading is not the button's wording: two nodes with one label is a menu the mouse
            // cannot aim at (`get_by_label` refuses the ambiguity — and so does a screen reader).
            "Permanent delete",
            format!("Are you sure you want to permanently delete these {count} item(s)?"),
            vec![
                named.join(", "),
                "The data is not recoverable. Nothing goes to the trash.".to_string(),
            ],
            // Safe answer first: `fresh` focuses the first button, so <kbd>Enter</kbd> and
            // <kbd>Escape</kbd> both leave the bytes alone. Go's `confirm` defaulted to OK, i.e. to
            // destroying them — a §6.2-class deviation, recorded in the step-10c log.
            vec![
                ConfirmButton::new("Cancel", Choice::Cancel),
                ConfirmButton::danger("Delete permanently", Choice::Ok),
            ],
        );
        self.open_dialog(dialogs::Dialog::Confirm(confirm));
    }

    /// Go's paste conflict (`context-menu.js:306` / `:379`) was a two-way *merge* `confirm`; the port
    /// asks the three-way question core can actually answer, in Go's wording, and re-runs the paste
    /// with the chosen [`ConflictPolicy`] instead of the silent `DEFAULT` skip.
    fn open_paste_conflict_confirm(
        &mut self,
        slot: PaneSlot,
        dest: AbsPath,
        mode: Option<PasteMode>,
        sources: Vec<AbsPath>,
        conflicts: Vec<String>,
    ) {
        let confirm = Confirm::new(
            DialogKind::PasteConflict {
                slot,
                dest,
                mode,
                sources,
            },
            "Resolve conflict",
            format!(
                "Entry(s) \"{}\" already exist at the destination.",
                conflicts.join(", ")
            ),
            vec!["Skip keeps what is there. Rename keeps both. Overwrite replaces it.".to_string()],
            vec![
                ConfirmButton::new("Skip", Choice::Skip),
                ConfirmButton::new("Rename", Choice::Rename),
                ConfirmButton::danger("Overwrite", Choice::Overwrite),
                ConfirmButton::new("Cancel", Choice::Cancel),
            ],
        );
        self.open_dialog(dialogs::Dialog::Confirm(confirm));
    }

    /// Act on one activated row: a directory navigates its pane, a file takes the desktop open
    /// (Go's `executeItemAction`). Shared by quick-find and the Search Manager, so both dialogs answer
    /// through the same seam as the chords and the menus rather than reaching for the engine.
    fn activate_path(&mut self, slot: PaneSlot, path: AbsPath, is_dir: bool) {
        if is_dir {
            self.navigate_shown(slot, FsPath::from_abs(path));
        } else {
            // The D6 branch-and-print convention, same as `open_paths` for a
            // selection: files go to the desktop opener, every failure toasts on its
            // own, and a directory the report re-routes still navigates.
            let paths = vec![path];
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
    }

    /// Turn a dialog answer into the command it stands for (plan §4.1). Called after the draw, like
    /// the pane commands, so a dialog can never mutate state another pane is painting from.
    pub fn answer(&mut self, answer: Answer) {
        // Step 14b: batch rename and the action editor answer themselves; both carry their subject.
        if let Answer::Batch { kind, outcome } = answer {
            if let (
                DialogKind::BatchRename { slot },
                crate::dialogs::BatchOutcome::Apply {
                    rows,
                    rules,
                    policy,
                },
            ) = (kind, outcome)
            {
                self.apply_batch_rename(slot, rows, rules, policy);
            }
            return;
        }
        if let Answer::Action { kind, outcome } = answer {
            let slot = match kind {
                DialogKind::ActionEditor { slot } => slot,
                _ => self.model.active_pane,
            };
            match outcome {
                crate::dialogs::ActionOutcome::Saved(draft) => self.save_action(draft),
                crate::dialogs::ActionOutcome::TestRun(draft) => {
                    // The dialog closes on any answer; a test run must not cost the user the draft,
                    // so it comes straight back with the same one.
                    self.run_action(slot, &draft);
                    self.open_action_editor(slot, Some(draft));
                }
                crate::dialogs::ActionOutcome::Closed => {}
            }
            return;
        }
        if let Answer::Button {
            kind: DialogKind::DeleteWorkspace { name },
            choice,
        } = &answer
        {
            if matches!(choice, Choice::Ok) {
                self.delete_workspace(name);
            }
            return;
        }
        // The Search Manager's closing answers (a row, or the dismissal); its non-closing requests
        // are drained by `search_manager::drain`.
        if let Answer::Index { outcome, .. } = answer {
            match outcome {
                crate::dialogs::IndexOutcome::Activate { slot, path, is_dir } => {
                    self.activate_path(slot, path, is_dir);
                }
                crate::dialogs::IndexOutcome::Closed { rebuild } => {
                    // Escape is Go's `closeSearchManager`: the rebuild it started is cancelled, so a
                    // dismissed dialog never leaves a walk running behind the shell.
                    if rebuild.is_some() {
                        // `true` when a rebuild really was running; a finished one needs nothing more.
                        let _ = self.session.index_cancel();
                    }
                }
            }
            return;
        }
        // The quick-find answers act on the engine directly (they carry their own subject);
        // the prompt/confirm surface below is the button-answered half.
        if let Answer::Quick { outcome, .. } = answer {
            match outcome {
                crate::dialogs::QuickOutcome::Activate { slot, path, is_dir } => {
                    self.activate_path(slot, path, is_dir);
                }
                crate::dialogs::QuickOutcome::Closed { job, .. } => {
                    // Close/teardown honesty: cancel what is still walking (prefer the lane's own
                    // job — a promoted deferred request is not the one the dialog remembered),
                    // and forget the window, so the next open is not throttled by this session.
                    if let Some(job) = self.session.live_search().or(job) {
                        self.session.cancel(job);
                    }
                    self.session.drop_pending_search();
                    self.session.reset_search_throttle();
                }
            }
            return;
        }
        let kind = answer.kind().clone();
        let choice = answer.choice();
        let text = match &answer {
            Answer::Text { text, .. } => text.trim().to_string(),
            Answer::Button { .. }
            | Answer::Quick { .. }
            | Answer::Index { .. }
            | Answer::Batch { .. }
            | Answer::Action { .. } => String::new(),
        };
        match kind {
            // The quick-find answer ran above; this arm only keeps the match total.
            DialogKind::QuickFind { .. } => {}
            // Properties is informational: closing it is the only answer it has, and it changes
            // nothing (step 13b).
            DialogKind::Properties { .. } => {}
            // The Search Manager's answers ran above; its requests are drained by the dialog module.
            DialogKind::SearchManager { .. } => {}
            // Batch rename and the action editor ran above; a *cancelled* workspace delete is the
            // only thing that reaches this match from 14b, and it changes nothing.
            DialogKind::BatchRename { .. }
            | DialogKind::ActionEditor { .. }
            | DialogKind::DeleteWorkspace { .. } => {}
            DialogKind::NewFolder { slot } => {
                if self.new_folder_request == Some(slot) {
                    self.new_folder_request = None;
                }
                if choice != Choice::Ok {
                    return;
                }
                let Some(dest) = self.shown_dir_abs(slot) else {
                    self.no_directory(Command::NewFolder);
                    return;
                };
                let origin = self.session.next_origin();
                match self.session.mkdir(&MkdirReq::new(dest, &text), origin) {
                    Ok(entry) => self
                        .notices
                        .post(toast::Level::Success, format!("new folder: {}", entry.name)),
                    Err(e) => self.toast_error("new folder", e),
                }
            }
            DialogKind::Chmod { paths, .. } => {
                if choice != Choice::Ok {
                    return;
                }
                let origin = self.session.next_origin();
                match ChmodReq::parse(paths, &text) {
                    Ok(req) => match self.session.chmod(&req, origin) {
                        Ok(report) => self.notices.post(
                            toast::Level::Success,
                            format!("chmod {}: {} path(s)", report.text, report.applied.len()),
                        ),
                        Err(e) => self.toast_error("chmod", e),
                    },
                    Err(e) => self.toast_error("chmod", e),
                }
            }
            DialogKind::PermanentDelete { slot, paths } => {
                if choice != Choice::Ok {
                    // The safety property the test asserts: a cancelled confirm changes nothing.
                    self.notices.post(
                        toast::Level::Info,
                        format!(
                            "delete permanently: cancelled: {} item(s) left alone",
                            paths.len()
                        ),
                    );
                    return;
                }
                let origin = self.session.next_origin();
                let req = DeleteReq {
                    sources: paths,
                    mode: DeleteMode::Permanent,
                    // The confirm *is* the opt-in plan **D6** asks for: the user has just been told,
                    // in the dialog's own words, that nothing is recoverable.
                    permanent_fallback: true,
                };
                match self.session.delete(&req, origin) {
                    Ok(Receipt::Running { job, origin }) => self.note_running(slot, job, origin),
                    Ok(Receipt::Done(report)) => self.notices.post(
                        toast::Level::Success,
                        format!("delete: {}", report.summary()),
                    ),
                    Err(e) => self.toast_error("delete permanently", e),
                }
            }
            DialogKind::PasteConflict {
                slot,
                dest,
                mode,
                sources,
            } => {
                if choice == Choice::Cancel {
                    self.notices
                        .post(toast::Level::Info, "paste: cancelled, nothing was written");
                    return;
                }
                let policy = match choice {
                    Choice::Overwrite => ConflictPolicy::Overwrite,
                    Choice::Rename => ConflictPolicy::Rename,
                    _ => ConflictPolicy::SkipExisting,
                };
                self.run_paste_with(slot, dest, mode, &sources, policy);
            }
        }
    }

    // ------------------------------------------------------------------------------- paste (10c)

    /// Which of `sources` already has a name at `dest` — the pre-flight the plan's §7 row 10 asks for
    /// before anything is written.
    ///
    /// It is a *read*, not a new core API: [`Session::list_dir`] is the synchronous listing call the
    /// address bar already uses, and the destination's first page is what a paste lands next to. A
    /// proper `Session::paste_preflight(&PasteReq) -> Vec<Conflict>` would be better still (it could
    /// see past the page, and would not repeat `plan`'s name arithmetic); what it would need is
    /// recorded in the plan log. On a read failure this answers "no conflicts", which is honest: the
    /// engine's guard is the authority, and `SkipExisting` — the policy the no-conflict path passes —
    /// cannot destroy anything.
    #[must_use]
    pub fn paste_conflicts(&self, dest: &AbsPath, sources: &[AbsPath]) -> Vec<String> {
        let Some(existing) = self.listing_names_of(dest) else {
            return Vec::new();
        };
        let mut out: Vec<String> = Vec::new();
        for path in sources {
            let name = path
                .as_path()
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            if existing.contains(&name) && !out.contains(&name) {
                out.push(name);
            }
        }
        out
    }

    /// The names in `dest`, from whichever listing already exists: a pane showing it (the paste-into-the-
    /// current-directory case, and the cross-pane drop), or `DirService`'s cache answered synchronously by
    /// [`Session::page_dir`] (plan §4.1's "cached ⇒ usually `Done`"). `None` when neither has it — a cold
    /// destination, which the pre-flight honestly cannot answer.
    #[must_use]
    fn listing_names_of(&self, dest: &AbsPath) -> Option<Vec<String>> {
        let path = FsPath::from_abs(dest.clone());
        let names = |page: &zenman_core::entry::DirPage| {
            page.entries
                .iter()
                .map(|entry| entry.name.clone())
                .collect::<Vec<_>>()
        };
        for pane in self.model.panes.values() {
            if let Some(tab) = pane.active_tab()
                && tab.path == path
            {
                return Some(names(&tab.entries));
            }
        }
        let origin = self.session.next_origin();
        match self.session.page_dir(&ListDir::first(path), origin) {
            Ok(Receipt::Done(page)) => Some(names(&page)),
            _ => None,
        }
    }

    /// <kbd>Ctrl+V</kbd> / <kbd>Alt+V</kbd> / <kbd>Ctrl+Alt+V</kbd>, with the collision pre-flight in
    /// front: a paste that would land on an existing name asks first (Go asked too, with a two-button
    /// `confirm`; plan F12 forbids the silent skip).
    fn run_paste(&mut self, slot: PaneSlot, dest: AbsPath, mode: Option<PasteMode>) {
        let Some(clip) = self.session.clipboard() else {
            self.notices
                .post(toast::Level::Warning, "paste: the clipboard is empty");
            return;
        };
        let sources = clip.paths();
        let conflicts = self.paste_conflicts(&dest, &sources);
        if conflicts.is_empty() {
            self.run_paste_with(slot, dest, mode, &[], ConflictPolicy::SkipExisting);
            return;
        }
        self.open_paste_conflict_confirm(slot, dest, mode, Vec::new(), conflicts);
    }

    /// The paste itself. `explicit` empty means "the clipboard's own paths, with its own op" — that is
    /// `Session::paste_clipboard`; a non-empty list is a drag's payload, which names its mode.
    pub fn run_paste_with(
        &mut self,
        slot: PaneSlot,
        dest: AbsPath,
        mode: Option<PasteMode>,
        explicit: &[AbsPath],
        policy: ConflictPolicy,
    ) {
        let origin = self.session.next_origin();
        let receipt = if explicit.is_empty() && mode.is_none() {
            self.session.paste_clipboard(dest, policy, origin)
        } else {
            let sources = if explicit.is_empty() {
                match self.session.clipboard() {
                    Some(clip) => clip.paths(),
                    None => {
                        self.notices
                            .post(toast::Level::Warning, "paste: the clipboard is empty");
                        return;
                    }
                }
            } else {
                explicit.to_vec()
            };
            let mode = mode.unwrap_or(PasteMode::Copy);
            let req = PasteReq {
                sources,
                dest,
                mode,
                policy,
            };
            self.session.paste(&req, origin)
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

    /// The rename the in-cell editor committed (plan §5.8: "commits on Enter … and validates the name
    /// through `ops::rename::validate_name` before submitting" — the engine's own guard is that
    /// validation, and its answer comes back *into the cell* as an inline error).
    fn commit_rename(&mut self, slot: PaneSlot, ix: usize, name: String) {
        let Some(tab) = self.tab_mut_of(slot) else {
            return;
        };
        let Some(entry) = tab.entries.entries.get(ix).cloned() else {
            if let Some(tab) = self.tab_mut_of(slot) {
                tab.rename_target = None;
                tab.rename_editor = None;
            }
            return;
        };
        let Some(source) = tab.row_abs(&entry) else {
            return;
        };
        let old = entry.name.clone();
        if name == old {
            // Nothing to do: close the editor quietly, exactly like Go's `if (newName !== oldName)`.
            tab.rename_target = None;
            tab.rename_editor = None;
            return;
        }
        let origin = self.session.next_origin();
        let outcome = self
            .session
            .rename(&RenameReq::one(source.clone(), name.clone()), origin)
            .map_err(|e| e.user_message())
            .and_then(|report| {
                report
                    .outcomes
                    .first()
                    .map(|outcome| match outcome {
                        zenman_core::ops::rename::RenameOutcome::Renamed { .. } => Ok(()),
                        zenman_core::ops::rename::RenameOutcome::Failed { error, .. } => {
                            Err(error.user_message())
                        }
                    })
                    .unwrap_or_else(|| Err("nothing was renamed".to_string()))
            });
        let Some(tab) = self.tab_mut_of(slot) else {
            return;
        };
        match outcome {
            Ok(()) => {
                tab.rename_target = None;
                tab.rename_editor = None;
            }
            Err(message) => {
                // Refuse *in the cell*: the editor stays open, with the reason under the field.
                if let Some(editor) = &mut tab.rename_editor {
                    editor.error = Some(message);
                    editor.fresh = false;
                }
            }
        }
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
        if self.menu.take().is_some() {
            // The menu is the thing that is open, and Go spent Escape on exactly that
            // (`keyboard.js:158`, `action-log.js:231`) — it does not also clear the selection.
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

/// The octal spelling of a listing's mode text (`-rwxr-xr-x` → `755`), for the chmod prompt's seed.
///
/// The listing carries Go's `fs.FileMode.String()` form, not octal, so the decode goes through core's
/// own parser and the re-encode through [`octal_text`](zenman_core::ops::chmod::octal_text) — the same
/// pair `ChmodReq::new` writes to the undo log, so the number in the prompt is the number in the log.
fn octal_text_of(mode: &str) -> String {
    // Go's `fs.FileMode.String()` is `<type><9 permission glyphs>`; the permission run is the tail, and
    // the special-bit glyphs (`s`, `t`) sit inside it, so taking the run from the end is exact.
    let glyphs: Vec<char> = mode
        .chars()
        .rev()
        .take_while(|c| matches!(c, 'r' | 'w' | 'x' | '-' | 's' | 't' | 'S' | 'T'))
        .take(9)
        .collect();
    if glyphs.len() != 9 {
        // Nothing to decode: Go's own prompt default, which is what the field would have shown anyway.
        return "755".to_string();
    }
    let mut bits = 0u32;
    for (ix, glyph) in glyphs.iter().rev().enumerate() {
        if *glyph != '-' {
            bits |= 0o400 >> ix;
        }
    }
    zenman_core::ops::chmod::octal_text(ModeBits::from_bits_retain(bits))
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
