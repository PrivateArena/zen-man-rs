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

use std::collections::HashSet;
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
use zenman_core::services::actions::ActionContext;
use zenman_core::services::dirs::ListDir;
use zenman_core::session::Origin;
use zenman_core::session::{Receipt, Session, StartupNote};
use zenman_core::store::config::{LayoutMode, PaneSlot, ThemeMode};

use crate::dialogs::{
    self, Answer, Choice, Confirm, ConfirmButton, Dialog, DialogKind, Prompt, Validation,
};
use crate::dock::{self, Dock};
use crate::jobs_ui::JobsUi;
use crate::keymap::{Command, KeyMap};
use crate::menus::{self, MenuAction, MenuContext, MenuSpec, MenuTarget};
use crate::panels::{
    self,
    file_pane::{PaneCommand, RowOrder},
    header::HeaderCommand,
};
use crate::persist::Persisted;
use crate::state::{
    AppModel, DropTarget, GroupId, GroupTarget, GroupVerb, Tab, TabId, TabSelKind, panes_for,
    quad_slots,
};
use crate::theme::{self, Tokens};
use crate::widgets::toast;

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
    /// Set only when [`Self::target`] is [`MenuTarget::Folder`]: the folder the menu acts on, which is
    /// Go's `data-target-path` (`context-menu.js:148`) — the breadcrumb chip's path, *never* the pane's
    /// selection. One field per live variant, so a folder menu cannot be drawn with row semantics.
    pub folder: Option<FsPath>,
    /// Where its top-left was asked to sit, in screen points.
    pub at: egui::Pos2,
    /// The menu layer's id. It is **stable** across openings — this fork's [`egui::Id::unique`] hashes
    /// its source with fixed seeds, so it answers the same id every time. That is why
    /// [`crate::menus::show`] pins the area with `fixed_pos`: an `Area` remembers the position of a
    /// known id, and one right-click elsewhere used to redraw at the first one's spot.
    pub id: egui::Id,
}

/// What a context menu was opened over — the half of a [`MenuRequest`] that says *which* thing the
/// rows act on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuSubject {
    /// The pane's listing: one row (`Some(ix)`, Go's `isItem`) or its empty space (`None`).
    Listing(Option<usize>),
    /// A folder that is not a row — a breadcrumb chip (Go's `showFolderContextMenu` + `data-path`).
    Folder(FsPath),
    /// One tab of the pane: Go's `showTabContextMenu` (`tabs.js:351`), named by its [`TabId`] — the
    /// strip index the click landed on is already stale by the time the menu is adopted one frame
    /// later, which is the reason [`MenuRequest`] waits a frame at all.
    Tab { id: TabId },
    /// One tab group: the menu of its chip, which acts on the whole run. `Copy`-friendly because a
    /// [`GroupId`](crate::state::GroupId) is one.
    Group { id: GroupId },
}

/// A tab drag in flight: the pane that owns it, and the tab it carries. The *target* is not stored —
/// it is recomputed from the strip's layout every frame, because that is the only geometry both the
/// pointer and the tabs agree on (design §8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TabDrag {
    /// The pane whose strip started the drag. A drop into another pane is out of scope.
    pub slot: PaneSlot,
    /// The tab under the pointer when the drag began.
    pub id: TabId,
}

/// The flyout that lists a collapsed group's tabs (Firefox's answer to "folded, but still usable").
///
/// `chip` and `popup` are both remembered because the popup *covers* the chip: the moment the pointer
/// moves into the list, `Response::hovered` on the chip is false, and a keep-alive built on it closes the
/// flyout before it can be read. So the test is the raw pointer position against the union of the two
/// rects, and `popup` is last frame's rect (this frame's does not exist until it has been painted).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TabGroupHover {
    /// The pane the chip belongs to.
    pub slot: PaneSlot,
    /// The group whose members are listed.
    pub id: GroupId,
    /// The chip's rect, as the strip reported it.
    pub chip: egui::Rect,
    /// When the pointer landed on the chip (`ctx.input(|i| i.time)`, seconds).
    pub since: f64,
    /// Last frame's popup rect, if it was drawn at all.
    pub popup: Option<egui::Rect>,
    /// When the pointer left both rects, `None` while it is inside either.
    pub left: Option<f64>,
}

/// A context menu a panel asked for, waiting one frame to be drawn (see
/// [`ZenApp::open_requested_menu`] for why the frame matters).
#[derive(Clone, Debug, PartialEq)]
pub struct MenuRequest {
    /// The pane the request came from.
    pub slot: PaneSlot,
    /// Rows, background, or one explicit folder path.
    pub subject: MenuSubject,
    /// Where to open it.
    pub at: egui::Pos2,
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
    /// The UI zoom (`zoom.rs`): the parent menu's `− | 100% | +` row, egui's scale factor, and
    /// `config.json`'s `zoomPercent`.
    pub zoom: crate::zoom::Zoom,
    /// The boot report lines (GPU profile, startup notes): the first one is the status bar's note.
    pub boot_lines: Vec<String>,
    /// The context menu a panel asked for last frame, waiting to be adopted by
    /// [`Self::open_requested_menu`].
    pub context_request: Option<MenuRequest>,
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
    /// The tab a strip drag is carrying, and which pane's strip is carrying it: the tab list is *not*
    /// reordered while the pointer is down (design §8), so this is the whole drag state — the target is
    /// recomputed from the layout every frame.
    pub tab_drag: Option<TabDrag>,
    /// The collapsed chip whose tab list is flown out (`docs/tab-groups-design.md` §7). One at a time,
    /// like `menu`, and for the same reason: it is a layer of its own with a keep-alive of its own.
    pub tab_group_hover: Option<TabGroupHover>,
    /// The chips the strips reported hovered this frame, consumed by [`Self::logic`].
    pub tab_group_reports: Vec<(PaneSlot, GroupId, egui::Rect)>,
    /// The workspace control of a pane's tab row: the popup, its verbs, the `✓ Saved` chip and the
    /// autosave clock (plan §5.8 `panels/header.rs`, step 14b).
    pub workspaces: crate::panels::header::WorkspaceBar,
    /// The action-log panel's state (plan §5.8 `panels/action_log.rs`, step 14b).
    pub log: crate::panels::action_log::LogPanel,
    /// When the next debounced persist is due (plan §5.7 `persist_at`).
    pub persist_at: Option<Instant>,
    /// The last frame's start, for the step-8 `perf_frame` budget check.
    pub last_frame: Instant,
    /// What the session's queue means for this frame: the rows a **cut** moves (the panes dim them)
    /// and what the status bar says about it. Refreshed at the top of [`Self::ui`]
    /// ([`Self::refresh_clip`]), so the four panes share one build and a cleared clipboard dims
    /// nothing the very next frame.
    clip: ClipView,
    /// A row a reveal has promised to put on screen, waiting for the listing that holds it
    /// ([`Self::reveal_in_parent`]).
    reveal: Option<Reveal>,
}

/// A reveal in flight: the navigation has been asked for, the selection cannot happen yet.
///
/// Go could do both in one function because its listing was already in memory (`navigateTo`, then
/// `selectedPaths.add` and `scrollIntoView` on a 150 ms timer, `quick-find.js:405-421`). Here the
/// parent directory may never have been listed, so the promise is kept in the shell and discharged
/// by [`ZenApp::apply_reveal`] on the frame the row exists — which is also the frame the scroller
/// is told to move.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Reveal {
    /// The pane being navigated.
    slot: PaneSlot,
    /// The item to select and scroll onto, once its directory is on screen.
    path: AbsPath,
    /// The page has already been widened once to reach it (Go never paged; the port does).
    widened: bool,
}

/// What one frame knows about the session's copy/cut queue, derived once for the four panes and the
/// status bar ([`ZenApp::refresh_clip`]).
///
/// The two halves answer the same question from two ends — *which rows did the user put in a queue*
/// and *what should the bar say about it* — so they are built together and cached together. The
/// queue itself is kept only to notice a change: comparing whole `Clipboard`s is exact where a
/// `(op, len)` fingerprint would not be, since a same-sized re-selection must still repaint.
#[derive(Clone, Default)]
struct ClipView {
    /// The queue the two projections came from.
    source: Option<zenman_core::ops::clipboard::Clipboard>,
    /// The paths a **cut** queues, which the panes dim; `None` for a copy or nothing queued.
    cut: Option<Arc<HashSet<AbsPath>>>,
    /// The status bar's `N items in action` indicator; `None` when nothing is queued.
    brief: Option<panels::status_bar::ClipStatus>,
}

impl ZenApp {
    /// Build the shell over a live session, restoring the persisted dock (plan F17: validated
    /// before first paint, with a report — never silently).
    #[must_use]
    pub fn new(session: Session, boot_lines: Vec<String>) -> Self {
        let theme = session.config().theme;
        let zoom = crate::zoom::Zoom::from_config(session.config().zoom_percent);
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
            zoom,
            boot_lines,
            context_request: None,
            menu: None,
            keys: KeyMap::default(),
            new_folder_request: None,
            dialogs: dialogs::Dialogs::default(),
            extracts: Vec::new(),
            archives_touched: false,
            drag: None,
            tab_drag: None,
            tab_group_hover: None,
            tab_group_reports: Vec::new(),
            workspaces: crate::panels::header::WorkspaceBar::default(),
            log: crate::panels::action_log::LogPanel::default(),
            persist_at: None,
            last_frame: Instant::now(),
            clip: ClipView::default(),
            reveal: None,
        }
    }

    /// Build with an already-open persisted store (the binary's path; tests use [`Self::new`]).
    #[must_use]
    pub fn with_persist(session: Session, boot_lines: Vec<String>, persist: Persisted) -> Self {
        let theme = session.config().theme;
        let zoom = crate::zoom::Zoom::from_config(session.config().zoom_percent);
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
            zoom,
            boot_lines,
            context_request: None,
            menu: None,
            keys: KeyMap::default(),
            new_folder_request: None,
            dialogs: dialogs::Dialogs::default(),
            extracts: Vec::new(),
            archives_touched: false,
            drag: None,
            tab_drag: None,
            tab_group_hover: None,
            tab_group_reports: Vec::new(),
            workspaces: crate::panels::header::WorkspaceBar::default(),
            log: crate::panels::action_log::LogPanel::default(),
            persist_at: None,
            last_frame: Instant::now(),
            clip: ClipView::default(),
            reveal: None,
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
                // A pane with no tab has nothing to navigate: Go's `navigateTo` returned silently
                // (`navigation.js:55-58`), which is what made a fresh pane feel unresponsive. The
                // click is unambiguous about intent, so the port opens the tab instead.
                if self.tab_mut_of(slot).is_none() {
                    self.open_tab(slot, path);
                    return;
                }
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
                    // A file row (or a member past the store cap): hand it to the desktop. This is
                    // the same behaviour the menu's `Open` row runs — `Command::Open` and this arm
                    // are two front ends on one action (plan **D7**) — and until now this arm only
                    // *said* the row was opened. The path is the row's own `row_abs`, because a
                    // double-click owes nothing to what the first click of the pair selected.
                    let Some(abs) = self.shown_tab(slot).and_then(|tab| tab.row_abs(&entry)) else {
                        self.notices.post(
                            crate::widgets::toast::Level::Warning,
                            format!("open {}: this tab lists no local path", entry.name),
                        );
                        return;
                    };
                    self.run_open(slot, vec![abs], Some(child));
                }
            }
            PaneCommand::FolderContext { path, at } => {
                // Go's breadcrumb `contextmenu` handler passed the chip's `data-path` to
                // `showFolderContextMenu` and stopped propagation (`navigation.js:211-215`). The
                // selection stays as it was: a chip is not a row.
                self.context_request = Some(MenuRequest {
                    slot,
                    subject: MenuSubject::Folder(path),
                    at,
                });
            }
            PaneCommand::TabContext { id, at } => {
                // The selection is folded **before** the request is recorded, because the menu is adopted
                // next frame and its rows are enabled off the selection: without the fold, a
                // `New Tab Group` greyed (or lit) for a stale set of tabs would fire on that set
                // (`Pane::right_click_selection`). What the right-click does *not* do is switch tabs:
                // Go's `showTabContextMenu` (`tabs.js:351-386`) never called `switchTab`.
                if let Some(pane) = self.model.panes.get_mut(&slot) {
                    pane.right_click_selection(id);
                }
                self.context_request = Some(MenuRequest {
                    slot,
                    subject: MenuSubject::Tab { id },
                    at,
                });
            }
            PaneCommand::Context { ix, at } => {
                // Recorded, not drawn: the menu is adopted next frame (`open_requested_menu`), and a
                // right-click that selects its row is already half the behaviour (Go's
                // `handlePaneContextMenu:19-27`) — but only *half*: Go selected the row **unless it was
                // already selected**, so right-clicking into a multi-selection keeps it and the menu acts
                // on the whole set. `Selection::fold_right_click` is that rule.
                self.context_request = Some(MenuRequest {
                    slot,
                    subject: MenuSubject::Listing(ix),
                    at,
                });
                if let Some(ix) = ix
                    && let Some(tab) = self.tab_mut_of(slot)
                {
                    tab.sel
                        .fold_right_click(ix, |i| tab.entries.entries.get(i).map(Tab::row_key));
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
            PaneCommand::SelectTab { id, kind } => {
                // Resolved now, not when the command was queued: a stale command is dropped, never
                // clamped onto another tab. The click kind goes first, so the run a <kbd>Shift</kbd>
                // click takes is the run of the anchor it left behind, and `activate` then follows the
                // click — the pane shows what was clicked whatever the selection ended up as.
                if let Some(pane) = self.model.panes.get_mut(&slot) {
                    // Order matters: the range's anchor is whatever the *previous* click left, so the
                    // selection is taken before `select_tab` moves it, and the pane then shows the tab
                    // the click landed on (`show` does not touch the set it just built).
                    if kind == TabSelKind::Replace {
                        pane.activate(id);
                    } else {
                        pane.select_tab(id, kind);
                        pane.show(id);
                    }
                }
            }
            PaneCommand::BeginTabDrag(id) => {
                self.tab_drag = Some(TabDrag { slot, id });
            }
            PaneCommand::EndTabDrag(target) => self.end_tab_drag(slot, target),
            PaneCommand::CloseTab(id) => self.close_tabs(slot, &[id]),
            PaneCommand::CloseTabs(ids) => self.close_tabs(slot, &ids),
            PaneCommand::TabGroup { target, verb } => self.run_group_command(slot, target, verb),
            PaneCommand::GroupContext { id, at } => {
                // Recorded like any other menu request, and likewise without selecting anything: the
                // chip is not a tab, and a right-click that folded the run would be a surprise.
                self.context_request = Some(MenuRequest {
                    slot,
                    subject: MenuSubject::Group { id },
                    at,
                });
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
        // Whatever the command did, the tab strip's invariants (`docs/tab-groups-design.md` §2) are the
        // pane's business, and this is the one point every command passes through: a verb that folds a
        // run it just emptied, or closes a tab out from under the selection, fails here in the suite that
        // exercised it instead of painting something impossible later.
        if let Some(pane) = self.model.panes.get(&slot) {
            pane.debug_assert_invariants();
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
                self.selection_fs(slot)
            } else {
                tab.row_fs(entry).map(|path| vec![path]).unwrap_or_default()
            }
        };
        if paths.is_empty() {
            return;
        }
        // A drag out of an archive is a read: the store is read-only, so a *move* would ask core
        // for `PasteMode::Cut` of members, which `Session::paste` refuses with `VfsReadOnly`.
        // Grab a copy instead — pasting members into a host directory extracts them.
        let mode = if self.in_archive(slot) {
            crate::drag_drop::DragMode::Copy
        } else {
            crate::drag_drop::grab_mode(self.session.clipboard().as_ref())
        };
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
        // An archive pane is not a writable destination: its rows' `row_abs` answers the
        // *container*, so without this gate a drop on a member row would paste next to the zip.
        // Same refusal (and same words) as the keyboard ladder's [`Self::READ_ONLY_REFUSED`].
        if self.in_archive(slot) {
            self.drag = None;
            self.notices.post(
                toast::Level::Warning,
                "drop: an archive listing is read-only — extract it first",
            );
            return;
        }
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
        // The background of an archive listing is not a directory: `shown_dir_abs` would answer the
        // *container* itself, so the drop would target `/a/b.zip`. Refused, like the keyboard
        // `Paste` in the same pane.
        if self.in_archive(slot) {
            self.drag = None;
            self.notices.post(
                toast::Level::Warning,
                "drop: an archive listing is read-only — extract it first",
            );
            return;
        }
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
            .and_then(|path| path.parent())
            .is_some_and(|parent| parent == FsPath::from_abs(dest.clone()));
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
        // Go's `updateLayout` seeds every new pane with a tab; without it a 2/4-pane grid opens
        // panels the user cannot drive at all.
        self.fill_empty_panes();
    }

    /// Give every pane of the current grid that has no tab a tab on the directory the user was
    /// looking at — Go's `updateLayout` (`split-view.js:47-57`, `68-74`), which is why a fresh split
    /// or quad pane in the original was never blank.
    ///
    /// Without it the new pane has no address bar to type in, nothing to list, and every
    /// tab-dependent command (sidebar navigation, `Up`, the ribbon) bails on its `active_tab()`, so
    /// the pane reads as dead. The source is the directory in view (any pane's shown tab, since the
    /// active one may itself be empty); the active pane is not moved, so focus stays where the user
    /// left it, and panes that already have tabs are left alone — a toggle back and forth preserves
    /// them.
    ///
    /// A grid where **nothing** has a tab is left alone too: that is the start-of-run case, and the
    /// caller (apps/zenman) answers it with the home directory rather than Go's `/` fallback — the
    /// same reason the port refuses to open on the server's cwd. Returns the number of panes seeded.
    pub fn fill_empty_panes(&mut self) -> usize {
        let source = match self
            .model
            .panes
            .iter()
            .find(|(_, pane)| !pane.tabs.is_empty())
            .and_then(|(_, pane)| pane.active_tab())
        {
            // Any tabbed pane, not necessarily the active one: the source is only "the directory the
            // user was looking at", and an active pane that is itself empty has nothing to offer.
            Some(tab) => tab.path.clone(),
            None => return 0,
        };
        let empty: Vec<PaneSlot> = self
            .model
            .panes
            .iter()
            .filter(|(_, pane)| pane.tabs.is_empty())
            .map(|(slot, _)| *slot)
            .collect();
        for slot in &empty {
            self.open_tab(*slot, source.clone());
        }
        empty.len()
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
        // The reveal's second half: a row can only be selected and scrolled onto once its directory
        // is on screen, which is a frame later when the pane has never listed it.
        self.apply_reveal();
        let now = Instant::now();
        self.last_frame = now;
        // The zoom's frame tick: hand egui the level the row or the chord asked for, and write it to
        // `config.json` once its own debounce has elapsed (`zoom.rs`).
        if let Some(percent) = self.zoom.tick(ctx, now)
            && let Err(e) = self.session.set_zoom_percent(percent)
        {
            self.notices.post(
                toast::Level::Error,
                format!("could not save the zoom: {}", e.user_message()),
            );
        }
        let dock_moved = dock::persist_pending(ctx, now);
        if dock_moved {
            let snap = self.snapshot();
            self.persist.autosave(&snap, now);
        }
        // Step 14b: the two 14a-backed clocks, on the same frame tick as `Persisted::autosave` —
        // the log panel's event-driven/poll refresh, and the workspace autosave + `✓ Saved` chip.
        crate::panels::action_log::tick(self);
        self.tick_workspaces(now, dock_moved);
        self.tick_group_hover(ctx);
    }

    /// The hover flyout's frame tick (`docs/tab-groups-design.md` §7): decide whether a collapsed chip
    /// is being *looked at*, and keep the list alive while the pointer travels from the chip into it.
    ///
    /// Two things make this a state machine rather than one `hovered()` test. The strips report their
    /// chips during the draw, so what is read here is last frame's report — the same hand-over
    /// [`Self::context_request`] uses for the same reason. And once the popup exists it *covers* the
    /// chip, so `Response::hovered` on the chip goes false exactly when the user reaches for a row: the
    /// keep-alive therefore tests the raw pointer against the union of the two rects.
    fn tick_group_hover(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        let pointer = ctx.input(|i| i.pointer.hover_pos());
        let reported = self.tab_group_reports.drain(..).next();
        if let Some((slot, id, chip)) = reported {
            match &mut self.tab_group_hover {
                // Same chip, newer rect: the strip may have re-laid itself (another tab folded away).
                Some(hover) if hover.slot == slot && hover.id == id => hover.chip = chip,
                other => {
                    *other = Some(TabGroupHover {
                        slot,
                        id,
                        chip,
                        since: now,
                        popup: None,
                        left: None,
                    });
                }
            }
        }
        // A menu, a modal or a tab drag is a different kind of attention: the flyout steps aside.
        if self.menu.is_some() || self.dialogs.is_open() || self.tab_drag.is_some() {
            self.tab_group_hover = None;
            return;
        }
        let Some(hover) = self.tab_group_hover.as_mut() else {
            return;
        };
        // The group may have been closed, renamed or re-coloured since it was reported: an id that no
        // longer names a group is not a flyout.
        if !self
            .model
            .panes
            .get(&hover.slot)
            .is_some_and(|pane| pane.group(hover.id).is_some_and(|group| group.collapsed))
        {
            self.tab_group_hover = None;
            return;
        }
        let inside = pointer.is_some_and(|at| {
            hover
                .chip
                .union(hover.popup.unwrap_or(hover.chip))
                .expand(self.tokens.group_hover_bridge)
                .contains(at)
        });
        if inside {
            hover.left = None;
        } else if hover.left.is_none() {
            hover.left = Some(now);
        }
        // A pointer that left the window is not "on its way to the list" — it is gone.
        if pointer.is_none() {
            hover.left = Some(now - self.tokens.group_hover_grace);
        }
        if hover
            .left
            .is_some_and(|left| now - left >= self.tokens.group_hover_grace)
        {
            self.tab_group_hover = None;
            return;
        }
        // Nothing repaints an idle shell, so the two timers have to ask for the frames they need: the
        // open delay if the hover has not reached it, the grace if the pointer is on its way out.
        let wait = if inside && now - hover.since < self.tokens.group_hover_open {
            Some(self.tokens.group_hover_open - (now - hover.since))
        } else if !inside {
            hover
                .left
                .map(|left| (self.tokens.group_hover_grace - (now - left)).max(0.0))
        } else {
            None
        };
        if let Some(secs) = wait {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(secs.max(0.01)));
        }
    }

    /// The list a collapsed group flies out (`docs/tab-groups-design.md` §7): one row per member tab,
    /// drawn over the chip in the shell's own menu frame.
    ///
    /// Picking a row shows that tab and **leaves the group folded** — that is the point of the flyout,
    /// and it is why the row is a plain `SelectTab` rather than an unfold plus a select.
    fn show_group_hover(&mut self, ctx: &egui::Context) {
        let Some(hover) = self.tab_group_hover else {
            return;
        };
        if ctx.input(|i| i.time) - hover.since < self.tokens.group_hover_open {
            return;
        }
        let Some(pane) = self.model.panes.get(&hover.slot) else {
            return;
        };
        let shown = pane.active_id();
        let rows: Vec<(TabId, String)> = pane
            .member_ids(hover.id)
            .iter()
            .filter_map(|id| pane.tabs.iter().find(|tab| tab.id == *id))
            .map(|tab| (tab.id, tab.title.clone()))
            .collect();
        if rows.is_empty() {
            return;
        }
        let tokens = self.tokens;
        let mut picked: Option<TabId> = None;
        let area = egui::Area::new(egui::Id::unique(("zenman-group-hover", hover.id)))
            .order(egui::Order::Foreground)
            .movable(false)
            .fixed_pos(hover.chip.left_bottom())
            .constrain(true)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(tokens.bg_sidebar)
                    .stroke(egui::Stroke::new(1.0, tokens.border))
                    .corner_radius(egui::CornerRadius::same(tokens.menu_corner_radius))
                    .inner_margin(egui::Margin::symmetric(0, tokens.menu_frame_pad_y as i8))
                    .shadow(egui::Shadow {
                        offset: [0, tokens.menu_shadow_offset_y],
                        blur: tokens.menu_shadow_blur,
                        spread: 0,
                        color: tokens.menu_shadow,
                    })
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
                        ui.set_min_width(hover.chip.width().max(tokens.tab_min_width));
                        egui::ScrollArea::vertical()
                            .max_height(tokens.group_hover_max_height)
                            .show(ui, |ui| {
                                for (id, title) in &rows {
                                    let row =
                                        ui.selectable_label(Some(*id) == shown, title.as_str());
                                    if row.clicked() {
                                        picked = Some(*id);
                                    }
                                }
                            });
                    });
            });
        if let Some(hover) = self.tab_group_hover.as_mut() {
            hover.popup = Some(area.response.rect);
        }
        if let Some(id) = picked {
            self.run_command(
                hover.slot,
                crate::panels::file_pane::PaneCommand::SelectTab {
                    id,
                    kind: crate::state::TabSelKind::Replace,
                },
            );
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

    /// Per-frame drawing: the panes and their chrome, the drawers, the dock, the toasts.
    pub fn ui(&mut self, ui: &mut Ui) {
        // The frame's two views of the queue, built **once**: the four panes and the status bar read
        // one projection each rather than asking the engine every row and every frame (`ClipView`).
        self.refresh_clip();
        let tokens = self.tokens;
        self.open_requested_menu();
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
        // The dock fills the frame: `CentralPanel::default()`'s own 8 px margin is dead space
        // around a grid that already strokes its own panes.
        CentralPanel::default()
            .frame(egui::Frame::new().fill(tokens.bg_main))
            .show(ui, |ui| {
                crate::dock_content::show(self, ui);
            });
        self.notices.show(ui.ctx(), &self.tokens);
        // **Last**, on the foreground layer: in this egui fork the last-allocated overlapping rect
        // wins hover and click (plan §9.2 step 8), so the menu is drawn after every panel, the dock
        // and the toast stack, and nothing can steal its rows.
        // The extraction panels, on the foreground layer but **modeless** (see `dialogs/extract`'s
        // module doc): a modal would block the pane the extraction repopulates.
        crate::dialogs::extract::show(self, ui, &tokens);
        self.draw_menu(ui.ctx());
        // Above the menu: the flyout is what the pointer is reaching for, and a hover that opened a
        // menu would have had it stepped aside for anyway (`tick_group_hover`).
        self.show_group_hover(ui.ctx());
        // **Last**, above the menu: a modal is the only thing on screen that may answer a key, and
        // `handle_keys` has already stood down for it. Its answers run here, after the draw, exactly
        // like the pane commands — a dialog never touches the engine itself.
        let answers = self.dialogs.show(ui, &self.tokens);
        for answer in answers {
            self.answer(answer);
        }
        // The manager's non-closing requests (Rebuild / Apply / the watch toggle) run here, after the
        // draw, exactly like the answers — a dialog still never calls the engine itself.
        crate::dialogs::search_manager::drain(self, ui.ctx());
        // …and the quick-find footer's (`Copy Found`, `Cut Found`, `Copy Path`, `Copy Name`), which
        // need `ctx` for the *text* clipboard and a dialog that must stay open.
        crate::dialogs::quick_find::drain(self, ui.ctx());
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

    /// The rows the **cut** clipboard queues, as the shared set the panes dim from (see
    /// [`Self::cut_clip`] and [`PaneModel::cut`](panels::file_pane::PaneModel::cut)). `None` when
    /// nothing is queued, or when the queue is a *copy* — a copy dims nothing.
    #[must_use]
    pub fn cut_clip(&self) -> Option<Arc<HashSet<AbsPath>>> {
        self.clip.cut.clone()
    }

    /// The session clipboard as the status bar's second indicator (Go's amber `✂ N items in action`),
    /// or `None` when nothing is queued. The queue is the *session's*, so it is reported whatever the
    /// active tab shows: after a Cut Inside the rows are not even in the listing, and the indicator is
    /// the only trace left.
    #[must_use]
    pub fn clip_status(&self) -> Option<panels::status_bar::ClipStatus> {
        self.clip.brief.clone()
    }

    /// This frame's two views of the engine's queue: the set of paths a **cut** moves, and the
    /// status bar's indicator. Go keyed its `.cut-state` class by path (`file-list.js:437-449`) and so
    /// does the set; the queue is the session's, so it names rows the active pane may not even show.
    ///
    /// Rebuilt only when the queue *changed*, compared field by field against the queue last seen (not
    /// guessed from its length, which a same-sized re-selection would fool), because a frame asks this
    /// question four times over: an unchanged frame then reads the engine through
    /// [`Session::clipboard_with`](zenman_core::session::Session::clipboard_with) and allocates
    /// nothing, where a 500-row queue would otherwise cost 500 path and 500 name clones, per frame.
    fn refresh_clip(&mut self) {
        let unchanged = self
            .session
            .clipboard_with(|clip| self.clip.source.as_ref() == Some(clip))
            .unwrap_or(self.clip.source.is_none());
        if unchanged {
            return;
        }
        // Rebuilt from a borrow: whatever the queue's size, a frame needs a set of paths and ten rows.
        let next = self.session.clipboard_with(|clip| {
            if clip.is_empty() {
                // Remembered, and projected as nothing: an empty queue still *exists*, and forgetting
                // it would ask this question again on every following frame.
                return ClipView {
                    source: Some(clip.clone()),
                    cut: None,
                    brief: None,
                };
            }
            let brief = panels::status_bar::ClipStatus {
                op: clip.op.as_str(),
                // The glyph says which of the two it is: Go swapped the scissors for the clipboard
                // (`file-list.js:437-449`) and so here.
                icon: match clip.op {
                    ClipOp::Copy => "copy",
                    ClipOp::Cut => "cut",
                },
                total: clip.items.len(),
                items: clip
                    .items
                    .iter()
                    .take(panels::status_bar::DISPLAY_CAP)
                    .map(|item| panels::status_bar::StatusItem {
                        name: item.display.clone(),
                        is_dir: item.is_dir,
                        // The same rule the listing row's size cell keeps (`entry_row::size_cell`,
                        // [`RowOrder::selected_items`]): a folder's `st_size` is its inode's record, not
                        // its content, so the queue's list shows the placeholder rather than a number
                        // the row two inches above it refuses to print.
                        size: if item.is_dir {
                            None
                        } else {
                            Some(item.size).flatten()
                        },
                    })
                    .collect(),
            };
            // A copy is not a move: its rows keep their full colour because the source survives the
            // paste, so only a cut builds the set. A cut queue is host-only by rule (core refuses
            // `Cut` of a member with `VfsReadOnly`), so mapping to host paths loses nothing; a
            // hypothetical member is skipped rather than dimming its container's rows.
            let cut = (clip.op == ClipOp::Cut).then(|| {
                Arc::new(
                    clip.items
                        .iter()
                        .filter_map(|i| i.path.host_abs())
                        .collect(),
                )
            });
            ClipView {
                source: Some(clip.clone()),
                cut,
                brief: Some(brief),
            }
        });
        self.clip = next.unwrap_or_default();
    }

    /// The 30 px status bar (plan §5.8 `status_bar.rs`), fed by the active pane's tab.
    fn status_bar(&mut self, ui: &mut Ui) {
        let tokens = self.tokens;
        let slot = self.model.active_pane;
        let items = self
            .model
            .panes
            .get(&slot)
            .and_then(|p| p.active_tab())
            .map(|tab| {
                let order = panels::file_pane::RowOrder::build(tab);
                // Only the head of the list, and only up to the popup's own cap: the bar prints a
                // count from the selection and ten names from the rows, never one name per selected
                // row (`Ctrl+A` on a 450-row listing would otherwise clone 450 names a frame).
                order.selected_items(&tab.sel, panels::status_bar::DISPLAY_CAP)
            })
            .unwrap_or_default();
        let (selected, rows, error) = self
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
            // The count comes from the selection, not from `items`: the bar's list is a head, and the
            // number it prints and the number its popup titles are the whole of it.
            selected: selected.len(),
            items,
            clip: self.clip_status(),
            info,
            jobs: &self.jobs.foreground,
            panes: self.model.panes.len(),
            note: self.boot_lines.first().cloned(),
            // The tab row reports the workspace *name*; the write that follows a navigation is told
            // here, on the frame clock the autosave itself runs on.
            saved: self.workspaces.chip_shows(self.last_frame),
        };
        panels::status_bar::show(ui, &model, &tokens);
    }

    /// The right drawer: Go's Custom Actions manager (`index.html:425-487`), step 15b.
    fn right_drawer(&mut self, ui: &mut Ui) {
        crate::panels::side_panel::show_right(self, ui);
    }

    /// The left drawer: places, favourites, mounted drives (step 15b; defects D8/D9).
    fn left_drawer(&mut self, ui: &mut Ui) {
        crate::panels::side_panel::show_left(self, ui);
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

    /// Go's `setSplitView(!state.isSplit)`: the two-pane grid, or the single pane when it is already
    /// showing. The `F3` chord and the ribbon's `2-pane` button are the same two lines of code.
    pub(crate) fn toggle_split_layout(&mut self) {
        let to = if self.model.panes.len() > 1 {
            panes_for(LayoutMode::Single)
        } else {
            panes_for(LayoutMode::Split)
        };
        self.set_layout(to);
    }

    /// Go's `setQuadView(!state.isQuad)` (`keyboard.js:195`), same rule as [`Self::toggle_split_layout`].
    pub(crate) fn toggle_quad_layout(&mut self) {
        let to = if self.model.quad() {
            panes_for(LayoutMode::Single)
        } else {
            quad_slots()
        };
        self.set_layout(to);
    }

    /// Open the active pane's address field, holding the path it stands for (the same rule the
    /// strip's own click follows, `panels::breadcrumb::open`) — a bare `editing = true` is what left
    /// the strip empty.
    pub fn open_address_bar(&mut self, slot: PaneSlot) {
        let path = self
            .model
            .panes
            .get(&slot)
            .and_then(|pane| pane.active_tab())
            .map(|tab| tab.path.to_string());
        let Some(pane) = self.model.panes.get_mut(&slot) else {
            return;
        };
        if let Some(path) = path {
            pane.address_buffer = path;
        }
        pane.address_editing = true;
    }

    // ---------------------------------------------------------------------------- menus (step 10b)

    /// Adopt a recorded [`PaneCommand::Context`](crate::panels::file_pane::PaneCommand::Context) as
    /// the open menu. Runs at the top of the draw, because the request arrives at the *end* of the
    /// previous frame (the panes' commands run after the dock, [`crate::dock_content::show`]) — which
    /// is also what keeps the opening right-click from being the click-away that closes it.
    fn open_requested_menu(&mut self) {
        let Some(MenuRequest { slot, subject, at }) = self.context_request.take() else {
            return;
        };
        let (target, folder) = match subject {
            MenuSubject::Listing(ix) => match ix {
                Some(ix) => {
                    let on_dir = self
                        .model
                        .panes
                        .get(&slot)
                        .and_then(|pane| pane.active_tab())
                        .and_then(|tab| tab.entries.entries.get(ix))
                        .is_some_and(|entry| entry.is_dir);
                    (MenuTarget::Row { on_dir }, None)
                }
                None => (MenuTarget::Background, None),
            },
            MenuSubject::Folder(path) => (MenuTarget::Folder, Some(path)),
            // No field is added to `OpenMenu`: `target` already carries the id (and is `Copy`), so a
            // fourth one would be a second spelling of the same identity.
            // The two facts the tab menu's group rows need come along for the ride, resolved **now** —
            // from the tab the click landed on and the selection the right-click left behind, never from
            // the pane's shown tab, which this menu says nothing about.
            MenuSubject::Tab { id } => {
                let pane = self.model.panes.get(&slot);
                let group = pane.and_then(|pane| pane.group_of(id));
                // The count the rows speak about: the selection the right-click just folded, which is
                // one when the click landed outside it (`Pane::right_click_selection`).
                let count = pane
                    .filter(|pane| pane.tab_sel.contains(&id))
                    .map_or(1, |pane| pane.tab_sel.len().max(1));
                (MenuTarget::Tab { id, group, count }, None)
            }
            MenuSubject::Group { id } => {
                let collapsed = self
                    .model
                    .panes
                    .get(&slot)
                    .and_then(|pane| pane.group(id))
                    .is_some_and(|group| group.collapsed);
                (MenuTarget::TabGroup { id, collapsed }, None)
            }
        };
        self.menu = Some(OpenMenu {
            slot,
            target,
            folder,
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
        menus::menu_for(&ctx, &self.menu_target(open), &actions)
    }

    /// What the open menu's rows act on, as the [`ActionContext`] the custom-action filter reads:
    /// the pane's selection for a listing menu, the clicked folder for a [`MenuTarget::Folder`] one
    /// (Go kept the two apart with `data-target-path`, `context-menu.js:148-156`).
    fn menu_target(&self, open: &OpenMenu) -> ActionContext {
        match &open.folder {
            Some(folder) => self.folder_action_context(open.slot, folder),
            None => self.action_context(open.slot),
        }
    }

    /// Go's `getActionsForContext(true, true, folderPath)` (`context-menu.js:161`): the folder is the
    /// target and the pane's directory is `{dir}`.
    ///
    /// A crumb inside an archive has no [`AbsPath`] of its own — `host_abs` answers the *container* —
    /// so it gets the context of a background click rather than one whose target is the `.zip`.
    #[must_use]
    pub fn folder_action_context(&self, slot: PaneSlot, folder: &FsPath) -> ActionContext {
        let dir = self.shown_dir_abs(slot);
        match &folder.store {
            zenman_core::path::VfsKind::Fs => match folder.host_abs() {
                Some(abs) => ActionContext::rows(dir, vec![abs], true),
                None => ActionContext::background(dir),
            },
            zenman_core::path::VfsKind::Archive { .. } => ActionContext::background(dir),
        }
    }

    /// The context the builders ask about, derived from live state field by field — the plan row's
    /// "a `MenuContext` field that cannot be populated yet is a documented gap, not a hardcoded
    /// `true`".
    ///
    /// The two tab facts come from the **target**, not from the pane's shown tab: a tab menu says nothing
    /// about the tab that happens to be on screen, and a chip menu has no tab at all.
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
            // …and the tab menu's two, straight off the target (see the method's doc).
            in_group: match target {
                MenuTarget::Tab { group, .. } => group.is_some(),
                MenuTarget::TabGroup { .. } => true,
                _ => false,
            },
            tab_count: match target {
                MenuTarget::Tab { count, .. } => count,
                _ => 0,
            },
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
        // …nor its **tab**: a closed or dragged-away tab must not leave a menu whose verbs name
        // nothing, so a tab menu is dropped the moment the strip no longer has that id. By id, not by
        // index: closing a tab shifts every index after it, and the menu would survive onto the tab
        // that slid into its place.
        if self.menu.as_ref().is_some_and(|open| {
            matches!(open.target, MenuTarget::Tab { id, .. } if self
                .model
                .panes
                .get(&open.slot)
                .is_none_or(|pane| !pane.tabs.iter().any(|tab| tab.id == id)))
        }) {
            self.menu = None;
        }
        // …and a **chip** menu must not outlive its group, which the same argument covers: closing the
        // last tab of a run buries the group, and its menu would be naming nothing.
        if self.menu.as_ref().is_some_and(|open| {
            matches!(open.target, MenuTarget::TabGroup { id, .. } if self
                .model
                .panes
                .get(&open.slot)
                .is_none_or(|pane| pane.group(id).is_none()))
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
        let tab_id = match open.target {
            MenuTarget::Tab { id, .. } => Some(id),
            _ => None,
        };
        for action in clicked {
            self.menu = None;
            match (action, open.folder.clone(), tab_id) {
                // A tab-group row: the target this menu was opened over says what it means — a chip
                // names its group, a tab names whichever group holds it — and the verb is resolved when
                // the row is clicked, not when the rows were built (§6).
                (MenuAction::Group(verb), ..) => {
                    let Some(target) = Self::group_target(open.target) else {
                        continue;
                    };
                    self.run_command(
                        open.slot,
                        PaneCommand::TabGroup {
                            target,
                            verb: verb.clone(),
                        },
                    );
                }
                (MenuAction::Command(command), Some(folder), _) => {
                    self.dispatch_folder(open.slot, command, &folder, ctx);
                }
                // The tab menu's rows name their own tab (Go's `data-tab-id`, `tabs.js:361`), so the
                // verb runs against the tab under the cursor rather than the shown one.
                (MenuAction::Command(command), None, Some(id)) => {
                    self.dispatch_tab(open.slot, command, id, ctx);
                }
                (MenuAction::Command(command), None, None) => {
                    self.dispatch(open.slot, command, ctx)
                }
                // A search result's menu is not drawn here: the dialog that owns the row draws it and
                // answers it (`dialogs::result_menu`), because this menu is the *pane's* — its rows speak
                // about the pane's selection, and a result row is not one. Getting here would mean a
                // `MenuTarget::Result` had been adopted into the shell's slot, which nothing does.
                (MenuAction::Result(_), ..) => {}
                // Go's `custom-action-for-path` (`context-menu.js:710`): a folder menu's rows carry
                // their own target, filtered *and* executed against the clicked path rather than
                // against the pane's selection.
                (MenuAction::Custom(action), folder, _) => {
                    let context = match &folder {
                        Some(folder) => self.folder_action_context(open.slot, folder),
                        None => self.action_context(open.slot),
                    };
                    self.run_action_in(open.slot, &action, context);
                }
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
        // The modifier strip is read out of the events themselves, so the real frame shape is what
        // gets tested: a chord press, then the clipboard event egui-winit pushes in its place.
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
            // `SingleLayout` needs no chord of its own, and why the ribbon's two buttons run these
            // very methods rather than a second copy of the rule.
            Command::SplitLayout => self.toggle_split_layout(),
            Command::QuadLayout => self.toggle_quad_layout(),
            Command::Refresh => self.refresh(slot),
            Command::ToggleView => self.run_command(slot, PaneCommand::ToggleGrid),
            Command::NewTab => self.run_command(slot, PaneCommand::NewTab),
            Command::DuplicateTab => self.duplicate_shown_tab(slot),
            Command::CloseTab => self.close_shown_tab(slot),
            Command::NextTab => self.cycle_tab(slot, 1),
            Command::PrevTab => self.cycle_tab(slot, -1),
            Command::CycleTabsMru => self.cycle_mru_tab(slot),
            Command::GoUp => self.run_command(slot, PaneCommand::Up),
            Command::SelectAll => self.run_command(slot, PaneCommand::SelectAll),
            Command::SelectNone => self.escape(slot),
            Command::InvertSelection => self.invert_selection(slot),
            Command::FocusSearch => self.open_address_bar(slot),
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
                // The row's own spelling, not the container's: the menu offers Copy path/Copy name
                // inside an archive (menus read "reading is never refused"), and a member's text is
                // `host.zip/member`, which is what a shell or a paste target can actually use.
                let lines: Vec<String> = self
                    .selection_fs(slot)
                    .iter()
                    .map(|path| folder_text(path, named))
                    .collect();
                self.copy_lines(&lines, ctx);
            }
            Command::Paste => match self.shown_dir_abs(slot) {
                Some(dest) => self.run_paste(slot, dest, None),
                None => self.no_directory(command),
            },
            Command::OpenInNewTab => match self.single_selected_dir(slot) {
                Some(dir) => {
                    // Go's `open-in-new-tab` (`keyboard.js:230-234`) demands one selected directory
                    // — `getSelectedDir()` — and opens the *pane's* new tab there. The path is the
                    // row's own, so inside an archive the new tab lands on the member directory
                    // rather than back on the container.
                    self.open_tab(slot, dir);
                }
                None => self.notices.post(
                    toast::Level::Warning,
                    "open in new tab: no directory selected",
                ),
            },
            Command::PasteInside => match self.single_selected_dir(slot) {
                // The destination of a paste is always a host directory (an archive listing is
                // refused by `READ_ONLY_REFUSED` before this dispatch), so the member case cannot
                // arrive here — a store-root path simply has no host answer.
                Some(dir) => match dir.host_abs() {
                    Some(dest) => self.run_paste(slot, dest, None),
                    None => self.no_directory(command),
                },
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
            Command::AddToFavorites => self.add_current_to_favourites(slot),
            Command::OpenContextMenu => {
                let at = ctx.input(|i| i.pointer.hover_pos());
                if let Some(at) = at {
                    self.context_request = Some(MenuRequest {
                        slot,
                        subject: MenuSubject::Listing(None),
                        at,
                    });
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

    /// The shown directory as an absolute path — a paste destination for **host** callers only.
    /// In an archive listing this answers the *container* (`FsPath::host_abs`), so every write
    /// ladder that takes this path is gated by [`Self::in_archive`] first; the drag/drop route is
    /// gated the same way.
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

    /// The selection's absolute **host** paths, in listing order (plan §5.8's display-order fix).
    /// In an archive listing [`Tab::row_abs`] answers the *container*, so this is only honest for
    /// the genuinely-host callers left on it — open / rename / delete / chmod / copy-path text —
    /// all of which the dispatch ladder refuses inside an archive. Clipboard queueing and dragging
    /// use [`Self::selection_fs`], which keeps members.
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

    /// The selection's paths in the pane's own store, in listing order — the clipboard/drag
    /// currency: host rows give host paths, archive rows give the **member** path, never the
    /// container.
    pub(crate) fn selection_fs(&self, slot: PaneSlot) -> Vec<FsPath> {
        let Some(tab) = self.shown_tab(slot) else {
            return Vec::new();
        };
        tab.entries
            .entries
            .iter()
            .filter(|entry| tab.sel.contains(Tab::row_key(entry)))
            .filter_map(|entry| tab.row_fs(entry))
            .collect()
    }

    /// The index of the only selected row — `None` when none or several are selected. The one
    /// question the selection answers for single-row activation and for Go's `-inside` chords.
    fn single_selected_ix(&self, slot: PaneSlot) -> Option<usize> {
        let tab = self.shown_tab(slot)?;
        if tab.sel.len() != 1 {
            return None;
        }
        tab.entries
            .entries
            .iter()
            .position(|entry| tab.sel.contains(Tab::row_key(entry)))
    }

    /// The one selected row when it is a directory — Go's `getSelectedDir()`
    /// (`keyboard.js:139-150`), which demands *exactly one* selection for the `-inside` chords. The
    /// row's own path, so a member directory answers the member and not its container.
    fn single_selected_dir(&self, slot: PaneSlot) -> Option<FsPath> {
        let ix = self.single_selected_ix(slot)?;
        let tab = self.shown_tab(slot)?;
        let entry = tab.entries.entries.get(ix)?;
        if !entry.is_dir {
            return None;
        }
        tab.row_fs(entry)
    }

    pub(crate) fn nothing_selected(&mut self, what: &str) {
        self.notices
            .post(toast::Level::Warning, format!("{what}: nothing selected"));
    }

    /// Put `lines` on the clipboard as one block — Go's `navigator.clipboard.writeText`
    /// (`context-menu.js:582-591`); egui's way out is the `CopyText` output command, which the
    /// shell's backend answers. An empty block is the loud no-op, never silence.
    pub(crate) fn copy_lines(&mut self, lines: &[String], ctx: &egui::Context) {
        let text = lines.join("\n");
        if text.is_empty() {
            self.nothing_selected("copy");
        } else {
            ctx.copy_text(format!("{text}\n"));
        }
    }

    /// Run one **folder menu** row against the folder it was opened over — the third front end on
    /// [`Self::dispatch`]'s table (plan **D7**). The chord has no path to name, so the menu supplies
    /// one; Go spelled the same difference as `data-target-path` (`context-menu.js:636-656`).
    ///
    /// Every other command is a row [`crate::menus::folder_menu`] does not draw, and is refused here
    /// rather than run against the pane's selection by accident.
    fn dispatch_folder(
        &mut self,
        slot: PaneSlot,
        command: Command,
        folder: &FsPath,
        ctx: &egui::Context,
    ) {
        match command {
            // Go's `open-path-in-new-tab` → `createTab(paneId, targetPath)` (`:636-638`): a *new*
            // tab, not the navigation a left-click on the same chip does.
            Command::OpenInNewTab => {
                self.open_tab(slot, folder.clone());
            }
            Command::CopyPath | Command::CopyName => {
                let named = command == Command::CopyName;
                self.copy_lines(&[folder_text(folder, named)], ctx);
            }
            other => self.notices.post(
                toast::Level::Warning,
                format!("{}: not a folder command", other.label()),
            ),
        }
    }

    /// Run one **tab menu** row against the tab it was opened over — the fourth front end on
    /// [`Self::dispatch`]'s table (plan **D7**). [`Command::CloseTab`] and its neighbours normally
    /// answer for the pane's *shown* tab; here they name the tab under the cursor, which is the whole
    /// point of the menu. Every other command is a row [`crate::menus::tab_menu`] does not draw, and is
    /// refused here rather than run against the shown tab by accident.
    fn dispatch_tab(&mut self, slot: PaneSlot, command: Command, id: TabId, ctx: &egui::Context) {
        let Some(tab) = self
            .model
            .panes
            .get(&slot)
            .and_then(|pane| pane.tabs.iter().find(|tab| tab.id == id))
        else {
            self.notices.post(
                toast::Level::Warning,
                format!("{}: that tab is gone", command.label()),
            );
            return;
        };
        // Read what the verbs need first: the match below needs `&mut self`, and a menu can
        // have been drawn against a tab that closed before the click landed.
        let path = tab.path.clone();
        match command {
            Command::CloseTab => self.run_command(slot, PaneCommand::CloseTab(id)),
            Command::DuplicateTab => self.duplicate_tab(slot, id),
            Command::CopyPath | Command::CopyName => {
                let named = command == Command::CopyName;
                self.copy_lines(&[folder_text(&path, named)], ctx);
            }
            other => self.notices.post(
                toast::Level::Warning,
                format!("{}: not a tab command", other.label()),
            ),
        }
    }

    /// Go's `duplicateTab` (`tabs.js:365-367`): the copy opens **beside** its original — a browser's
    /// rule, and the reason the tab colour runs [`crate::panels::tab_bar::runs`] stays legible.
    ///
    /// The original's position is resolved from `id` here, at apply time: the insert below needs the
    /// pane mutably, and the menu can have been drawn against a tab that closed before the click landed
    /// — which is a notice, never a copy of some other tab.
    fn duplicate_tab(&mut self, slot: PaneSlot, id: TabId) {
        let new_id = self.model.next_tab_id();
        let read = self.model.panes.get(&slot).and_then(|pane| {
            pane.tabs
                .iter()
                .position(|tab| tab.id == id)
                .map(|ix| (pane.tabs[ix].duplicate(new_id), ix))
        });
        let Some((copy, ix)) = read else {
            self.notices.post(
                toast::Level::Warning,
                format!("{}: that tab is gone", Command::DuplicateTab.label()),
            );
            return;
        };
        let Some(pane) = self.model.panes.get_mut(&slot) else {
            return;
        };
        // `ix + 1`, clamped: a pane whose last tab was duplicated has its copy last, and a pane that
        // lost tabs in between cannot be indexed past its own length.
        let inserted_at = (ix + 1).min(pane.tabs.len());
        pane.tabs.insert(inserted_at, copy);
        pane.active = inserted_at;
        pane.touch_mru(new_id);
        // The duplicate lists for itself — the same hand-over [`Self::open_tab`] makes for an
        // appended tab, and the reason the new tab is not an empty page forever.
        self.list_at(slot, inserted_at, false, false);
    }

    /// [`Self::dispatch`]'s route, for a caller that clicked on nothing: the verb answers for the
    /// pane's **shown** tab — the same id [`Self::close_shown_tab`] takes, under the same empty-pane
    /// guard (a pane with no tab toasts, it does not underflow).
    fn duplicate_shown_tab(&mut self, slot: PaneSlot) {
        let Some(pane) = self.model.panes.get(&slot) else {
            return;
        };
        if pane.tabs.is_empty() {
            self.nothing_selected("duplicate tab");
            return;
        }
        let ix = pane.active.min(pane.tabs.len() - 1);
        let id = pane.tabs[ix].id;
        self.duplicate_tab(slot, id);
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
        let named = named_summary(&paths);
        let confirm = Confirm::new(
            DialogKind::PermanentDelete { slot, paths },
            // The heading is not the button's wording: two nodes with one label is a menu the mouse
            // cannot aim at (`get_by_label` refuses the ambiguity — and so does a screen reader).
            "Permanent delete",
            format!("Are you sure you want to permanently delete these {count} item(s)?"),
            vec![
                named,
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

    /// Quick find's `Delete Found` question. Go asked it once, with one answer, in a browser
    /// `confirm` that could only say OK or Cancel (`quick-find.js:799`) — and its OK was
    /// `os.RemoveAll`. The port keeps the count and the names Go showed and adds the answer Go could
    /// not carry: the trash. Safe button first, so <kbd>Enter</kbd> and <kbd>Escape</kbd> both leave
    /// the bytes alone (`Confirm`'s rule, and the reason the card exists).
    fn open_found_delete_confirm(&mut self, slot: PaneSlot, paths: Vec<AbsPath>) {
        if paths.is_empty() {
            self.nothing_selected("delete");
            return;
        }
        let count = paths.len();
        let named = named_summary(&paths);
        let confirm = Confirm::new(
            DialogKind::DeleteFound { slot, paths },
            "Delete found",
            format!("Are you sure you want to delete these {count} matched item(s)?"),
            vec![
                named,
                "The list is every match of the current query, not the rows on screen.".to_string(),
            ],
            vec![
                ConfirmButton::new("Cancel", Choice::Cancel),
                ConfirmButton::new("Move to trash", Choice::Ok),
                ConfirmButton::danger("Delete permanently", Choice::Permanent),
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
        sources: Vec<FsPath>,
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

    /// Reveal one row in the folder that holds it — Go's `executeItemAction(…, 'reveal')`, which
    /// `quick-find.js:321-325` hangs on the result row's right-click and `context-menu.js:690-698` on
    /// the search menu's `Open Containing Folder`: navigate to the parent, make the row the selection,
    /// scroll onto it.
    fn reveal_in_parent(&mut self, slot: PaneSlot, path: AbsPath) {
        let Some(parent) = self.arm_reveal(slot, &path) else {
            return;
        };
        self.navigate_shown(slot, parent);
    }

    /// The same, in a tab of its own — Go's `reveal-search-result-new-tab` (`:700-707`), which calls
    /// `createTab(…, parentDir)` and then `switchTab`. [`Self::open_tab`] does both and lists the new
    /// tab, so all that is left is the promise, which names the *tab that is now shown*.
    fn reveal_in_new_tab(&mut self, slot: PaneSlot, path: AbsPath) {
        let Some(parent) = self.arm_reveal(slot, &path) else {
            return;
        };
        self.open_tab(slot, parent);
    }

    /// Promise the reveal, and answer the question both verbs ask first: which folder has to be
    /// listed. `None`, with a toast, for a path with no parent — the filesystem root, which is a row
    /// the index can hand back.
    #[must_use]
    fn arm_reveal(&mut self, slot: PaneSlot, path: &AbsPath) -> Option<FsPath> {
        let Some(parent) = path.parent() else {
            self.notices.post(
                toast::Level::Warning,
                format!("reveal: {} has no parent folder", path.to_string_lossy()),
            );
            return None;
        };
        self.reveal = Some(Reveal {
            slot,
            path: path.clone(),
            widened: false,
        });
        Some(FsPath::from_abs(parent))
    }

    /// The second half of a reveal, once per frame: when the pane's listing carries the promised row,
    /// select it and ask the scroller to bring it on screen. A row that has no listing to live in — a
    /// pane that failed, moved on, or never had it — drops the promise quietly, which is what Go's
    /// `setTimeout(…, 150)` did when its query for the row came back empty.
    fn apply_reveal(&mut self) {
        let Some(reveal) = self.reveal.clone() else {
            return;
        };
        let Some(name) = reveal
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            self.reveal = None;
            return;
        };
        // Read the pane, then act on it: widening the page needs `&mut self`, which no borrow of the
        // tab may span.
        enum Step {
            /// The listing this row belongs to has not arrived.
            Wait,
            /// Nothing to reveal any more.
            Gone,
            /// The row is past the page: read the whole directory, once.
            Widen,
            /// The row, as its listing index.
            Here(usize),
        }
        let step = match self
            .model
            .panes
            .get(&reveal.slot)
            .and_then(|pane| pane.active_tab())
        {
            None => Step::Gone,
            Some(tab) if tab.error.is_some() => Step::Gone,
            // The tab has been steered somewhere else (the user, or a `DirChanged` reload of another
            // directory): this row is not what that pane is looking at.
            Some(tab) if !lists_parent(tab, &reveal.path) => Step::Gone,
            // The rows on screen are still the *previous* directory's.
            Some(tab) if tab.entries.path != tab.path => Step::Wait,
            Some(tab) => match row_named(tab, &name) {
                Some(ix) => Step::Here(ix),
                // A paged listing is the one case the shell can fix: the pane's own
                // `Load all items` verb is the call, and it is made once.
                None if tab.has_more && !reveal.widened => Step::Widen,
                None => Step::Gone,
            },
        };
        match step {
            Step::Wait => {}
            Step::Gone => self.reveal = None,
            Step::Widen => {
                if let Some(ix) = self
                    .model
                    .panes
                    .get(&reveal.slot)
                    .map(|pane| pane.active.min(pane.tabs.len().saturating_sub(1)))
                {
                    self.list_at(reveal.slot, ix, false, true);
                }
                if let Some(pending) = self.reveal.as_mut() {
                    pending.widened = true;
                }
            }
            Step::Here(ix) => {
                self.reveal = None;
                let Some(tab) = self.tab_mut_of(reveal.slot) else {
                    return;
                };
                let Some(entry) = tab.entries.entries.get(ix) else {
                    // The page turned over between the read and the write. Nothing is owed: the row
                    // was there a moment ago and a reload will say where it is now.
                    return;
                };
                // A folded group has no row on screen to reveal, so the reveal opens it (Go's flat
                // listing had nothing to open).
                let group = crate::panels::file_pane::group_of(entry);
                tab.collapsed_groups.remove(&group);
                let key = crate::state::Tab::row_key(entry).to_string();
                tab.sel.select_key(&key);
                // The scroller wants the *visual* row: the entry's index plus every group header in
                // front of it. `RowOrder` owns its data, so it does not keep the tab borrowed.
                let order = RowOrder::build(tab);
                if let Some(row) = (0..order.len()).find(|row| order.entry_ix(*row) == Some(ix)) {
                    tab.scroll.jump_to_row = Some(row);
                }
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
                crate::dialogs::IndexOutcome::Reveal { slot, path } => {
                    self.reveal_in_parent(slot, path);
                }
                crate::dialogs::IndexOutcome::RevealInNewTab { slot, path } => {
                    self.reveal_in_new_tab(slot, path);
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
                crate::dialogs::QuickOutcome::DeleteFound { slot, paths } => {
                    // Go deleted on one browser `confirm` whose only yes was the permanent one
                    // (`quick-find.js:799`); the port hands the question to the same card the context
                    // menu's two destructive rows use, which is why this dialog had to close first —
                    // the stack holds one modal, and the *bytes* wait for the answer.
                    self.open_found_delete_confirm(slot, paths);
                }
                crate::dialogs::QuickOutcome::Reveal { slot, path } => {
                    self.reveal_in_parent(slot, path);
                }
                crate::dialogs::QuickOutcome::RevealInNewTab { slot, path } => {
                    self.reveal_in_new_tab(slot, path);
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
            DialogKind::BatchRename { .. } | DialogKind::ActionEditor { .. } => {}
            // The workspace popup's two prompts. A cancelled one is a no-op; an empty name never
            // reaches here (`Validation::None`), and an illegal one is core's to refuse.
            DialogKind::NewWorkspace => {
                if choice == Choice::Ok && !text.is_empty() {
                    self.header_commands(vec![HeaderCommand::SaveNew(text)]);
                }
            }
            DialogKind::RenameTabGroup { slot, id } => {
                // The group may have been closed while the prompt was up; the verb then names nothing,
                // and `run_group_command` says so by doing nothing. An empty answer is not refused here:
                // that is the group's *unnamed* state, whose label is its colour.
                if choice == Choice::Ok {
                    self.run_command(
                        slot,
                        PaneCommand::TabGroup {
                            target: GroupTarget::Group(id),
                            verb: GroupVerb::Name { id, name: text },
                        },
                    );
                }
            }
            DialogKind::RenameWorkspace { name } => {
                if choice == Choice::Ok && !text.is_empty() {
                    self.header_commands(vec![HeaderCommand::RenameTo {
                        from: name,
                        to: text,
                    }]);
                }
            }
            // A *cancelled* workspace delete is the only answer that reaches this match; the
            // affirmative one ran above (`Answer::Button` on `DeleteWorkspace`).
            DialogKind::DeleteWorkspace { .. } => {}
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
            DialogKind::DeleteFound { slot, paths } => {
                // Two destructive answers, both named: Go's `confirm` had one, and it was the worse
                // one. Anything else (Cancel, `Escape`) leaves every byte where it is.
                let req = match choice {
                    Choice::Ok => DeleteReq::to_trash(paths),
                    Choice::Permanent => DeleteReq::permanent(paths),
                    _ => {
                        self.notices.post(
                            toast::Level::Info,
                            format!(
                                "delete found: cancelled: {} item(s) left alone",
                                paths.len()
                            ),
                        );
                        return;
                    }
                };
                let origin = self.session.next_origin();
                match self.session.delete(&req, origin) {
                    Ok(Receipt::Running { job, origin }) => self.note_running(slot, job, origin),
                    Ok(Receipt::Done(report)) => self.notices.post(
                        toast::Level::Success,
                        format!("delete found: {}", report.summary()),
                    ),
                    Err(e) => self.toast_error("delete found", e),
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
    pub fn paste_conflicts(&self, dest: &AbsPath, sources: &[FsPath]) -> Vec<String> {
        let Some(existing) = self.listing_names_of(dest) else {
            return Vec::new();
        };
        let mut out: Vec<String> = Vec::new();
        for path in sources {
            // The name a paste answers at `dest`: the basename for a host path, the member's own
            // name for an archive path — which is exactly where `Session::paste` re-roots it.
            let name = path.last_segment().unwrap_or_default().to_string();
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
        explicit: &[FsPath],
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
                pane.address_buffer.clear();
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

    /// <kbd>Ctrl+W</kbd> — close the shown tab, with the pane's bookkeeping identical to the
    /// strip's own close button: both are one [`PaneCommand::CloseTab`], i.e.
    /// [`crate::state::Pane::close_tabs`].
    /// The one way tabs are closed: [`Pane::close_tabs`] keeps `active`, the MRU ring, the strip's
    /// multi-selection and the group table in step, which a bare `tabs.remove` does not — and then the
    /// shown tab is listed, which is the half a caller must not forget.
    fn close_tabs(&mut self, slot: PaneSlot, ids: &[TabId]) {
        let shown = self
            .model
            .panes
            .get(&slot)
            .and_then(|pane| pane.active_tab())
            .map_or(TabId::default(), |tab| tab.id);
        let Some(pane) = self.model.panes.get_mut(&slot) else {
            return;
        };
        pane.close_tabs(ids);
        self.tab_group_hover = None;
        if self.tab_drag.is_some_and(|drag| ids.contains(&drag.id)) {
            self.tab_drag = None;
        }
        let Some((ix, id)) = pane
            .active_tab()
            .map(|tab| (pane.active, tab.id))
            .filter(|(_, id)| *id != shown)
        else {
            return;
        };
        // The tab that took the focus was possibly never listed (a restore queues all but the shown
        // one), so it is listed now — the same hand-over `activate` gets from a click.
        self.list_at(slot, ix, false, false);
        let _ = id;
    }

    /// The end of a strip drag: resolve what was being carried, move it, and let go of the state
    /// (`docs/tab-groups-design.md` §8).
    ///
    /// Dragging a tab that is part of a multi-selection drags the selection — the browser rule, and the
    /// one that makes "shift-click four tabs, drag them onto the chip" the whole of a group. A release
    /// outside every rect cancels: the order is untouched, because a guess at a position is worse than
    /// the drag not having happened.
    fn end_tab_drag(&mut self, slot: PaneSlot, target: Option<DropTarget>) {
        let Some(drag) = self.tab_drag.take() else {
            return;
        };
        if drag.slot != slot {
            return;
        }
        let Some(target) = target else {
            return;
        };
        let carried = self
            .model
            .panes
            .get(&slot)
            .map(|pane| {
                if pane.tab_sel.contains(&drag.id) {
                    pane.selected_tabs()
                } else {
                    vec![drag.id]
                }
            })
            .unwrap_or_else(|| vec![drag.id]);
        let Some(pane) = self.model.panes.get_mut(&slot) else {
            return;
        };
        pane.move_tabs(&carried, target);
        pane.debug_assert_invariants();
    }

    /// A tab-group verb, applied to one pane (`docs/tab-groups-design.md` §6).
    ///
    /// Every chip click, every tab-menu row and every chip-menu row lands here, so the algebra in
    /// [`crate::state::Pane`] is the only place a group changes and the invariants are checked in one
    /// place too. The subject is resolved **first**: a merge mints an id, which is the model's counter
    /// and cannot be touched while the pane is borrowed.
    fn run_group_command(&mut self, slot: PaneSlot, target: GroupTarget, verb: GroupVerb) {
        let (group, tabs) = match self.model.panes.get(&slot) {
            None => return,
            Some(pane) => match target {
                // The chip's menu: the group is named, and "the tabs it acts on" are its run.
                GroupTarget::Group(id) => (Some(id), pane.member_ids(id)),
                // A tab's menu: the group is whichever holds it, and the tabs are the multi-selection
                // when the click landed inside it — which the right-click just made true (§6).
                GroupTarget::Tab(id) => {
                    let group = pane.group_of(id);
                    let tabs = if pane.tab_sel.contains(&id) {
                        pane.selected_tabs()
                    } else {
                        vec![id]
                    };
                    (group, tabs)
                }
            },
        };
        // Two verbs cannot run inside the borrow of the pane the others need: closing tabs is the shell's
        // own path (the MRU ring and the successor are its business), and renaming opens a dialog. They
        // are recorded here and run after, which is why this is one exhaustive match and not a `return`
        // sprinkled through it.
        enum Deferred {
            Nothing,
            Close(Vec<TabId>),
            Prompt(GroupId),
        }
        let mut deferred = Deferred::Nothing;
        {
            let fresh = self.model.next_group_id();
            let Some(pane) = self.model.panes.get_mut(&slot) else {
                return;
            };
            match verb {
                GroupVerb::ToggleCollapse => {
                    if let Some(id) = group {
                        pane.toggle_collapsed(id);
                    }
                }
                GroupVerb::New => {
                    let color = pane.free_color();
                    pane.create_group(fresh, &tabs, color);
                }
                GroupVerb::NewColor(color) => {
                    // The colour already has a group here? Join it. That is what the nine menu rows mean
                    // — "put these tabs in the red group" — and there is at most one of those per pane,
                    // so the question has one answer.
                    match pane
                        .groups
                        .iter()
                        .find(|held| held.color == color)
                        .map(|held| held.id)
                    {
                        Some(have) => {
                            pane.join_group(have, &tabs);
                        }
                        None => {
                            pane.create_group(fresh, &tabs, color);
                        }
                    }
                }
                GroupVerb::Leave => {
                    pane.leave_group(&tabs);
                }
                GroupVerb::Ungroup => {
                    if let Some(id) = group {
                        pane.ungroup(id);
                    }
                }
                GroupVerb::Name { id, name } => {
                    pane.rename_group(id, &name);
                }
                GroupVerb::Color(color) => {
                    if let Some(id) = group {
                        pane.set_group_color(id, color);
                    }
                }
                GroupVerb::Select => {
                    if let Some(id) = group {
                        pane.tab_sel = pane.member_ids(id).into_iter().collect();
                        pane.tab_sel_anchor = pane.tab_sel.iter().next().copied();
                    }
                }
                GroupVerb::Close => deferred = Deferred::Close(tabs),
                GroupVerb::Rename => {
                    if let Some(id) = group {
                        deferred = Deferred::Prompt(id);
                    }
                }
            }
            pane.debug_assert_invariants();
        }
        match deferred {
            Deferred::Nothing => {}
            Deferred::Close(ids) => self.close_tabs(slot, &ids),
            Deferred::Prompt(id) => self.rename_group_prompt(slot, id),
        }
        // A tab that went into a group, or out of one, may have made a chip appear or vanish: the flyout
        // of a group that no longer exists must not stay on screen.
        self.tab_group_hover = None;
    }

    /// Which thing a menu's target says a group verb is about (`docs/tab-groups-design.md` §6). The
    /// listing's targets have no answer, which is why this is a total function returning `Option` rather
    /// than a `match` the compiler could check for me: a background click cannot group anything.
    #[must_use]
    fn group_target(target: MenuTarget) -> Option<GroupTarget> {
        match target {
            MenuTarget::TabGroup { id, .. } => Some(GroupTarget::Group(id)),
            MenuTarget::Tab { id, .. } => Some(GroupTarget::Tab(id)),
            _ => None,
        }
    }

    /// The prompt behind the chip menu's `Rename Group` row — the same path the workspace rename uses
    /// ([`crate::panels::header::ZenApp_rename_workspace_prompt`]), including the empty answer, which is
    /// how a group goes back to being named after its colour.
    fn rename_group_prompt(&mut self, slot: PaneSlot, id: GroupId) {
        let Some(group) = self
            .model
            .panes
            .get(&slot)
            .and_then(|pane| pane.group(id))
            .cloned()
        else {
            return;
        };
        let mut prompt = Prompt::new(
            DialogKind::RenameTabGroup { slot, id },
            "Rename tab group",
            format!("New name for the {:?} group:", group.color),
            group.name.clone(),
            "Rename",
            Validation::None,
        );
        // The name is what is there already: select it, so typing replaces it (`rename_workspace_prompt`).
        prompt.select = Some(group.name.chars().count());
        self.open_dialog(Dialog::Prompt(prompt));
    }

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
        let id = pane.tabs[ix].id;
        self.run_command(slot, PaneCommand::CloseTab(id));
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
        let id = pane.tabs[next].id;
        // The step is by position — a ring walk has to — but the *showing* goes through `activate`, so
        // the id is what the pane is asked for: the MRU ring and the selection follow the shown tab the
        // same way a strip click makes them.
        pane.activate(id);
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
        // Keep the cursor on screen. `ScrollState::y` is an *output* — the scroller re-derives it from
        // egui's own scroll state every frame — so the old `tab.scroll.y = target` write could not
        // move the view, and a keyboard move down off the page selected a row nobody could see. The
        // request field is the input the scroller answers.
        let from = tab.scroll.y;
        if (target as f32) >= from + page as f32 || (target as f32) < from {
            tab.scroll.jump_to_row = Some(target);
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

    /// <kbd>Enter</kbd> — Go's `triggerOpen`. One row is the pane's own activation rule (see
    /// [`PaneCommand::Open`]); a multi-selection is the batch, which core routes: a directory is
    /// *reported* in `navigated` and the pane goes there, a file goes to the desktop, and each
    /// failure is toasted on its own (Go's single `lastErr`, made per-path).
    fn open_selection(&mut self, slot: PaneSlot) {
        // One row goes through the pane's own activation rule (`PaneCommand::Open`), which owns the
        // store ladder — so Enter, the menu row and a double-click cannot disagree (plan **D7**: one
        // behaviour, several front ends). A multi-selection has no single row to enter, so it stays
        // the batch of desktop launches Go's `handleOpen` ran.
        if let Some(ix) = self.single_selected_ix(slot) {
            self.run_command(slot, PaneCommand::Open { ix });
            return;
        }
        let paths = self.selection_abs(slot);
        if paths.is_empty() {
            self.nothing_selected("open");
            return;
        }
        self.run_open(slot, paths, None);
    }

    /// Hand `paths` to the desktop and act on the engine's own routing: a directory is **navigation**
    /// (`ops::open`'s `OpenRoute::Navigate`, reported rather than waiting for a launcher that never
    /// runs) and a file is a launch, each failure named per path.
    ///
    /// Only ever reached with host paths: a single selected row goes through [`PaneCommand::Open`],
    /// which knows stores, and a multi-selection inside an archive is refused below. Opening is a
    /// host-only verb — the queueing verbs use [`Tab::row_fs`]; [`Tab::row_abs`], which the batch
    /// was built with, still answers the **container** for a member path, which is exactly why the
    /// refusal has to stay here. Opening a member is extraction, which is [`Command::Extract`].
    ///
    /// `navigate_to` is the multi-selection batch's fallback: with several rows there is no single
    /// joined path to move to, so the reported host directory is used.
    fn run_open(&mut self, slot: PaneSlot, paths: Vec<AbsPath>, navigate_to: Option<FsPath>) {
        if self.in_archive(slot) {
            self.notices.post(
                toast::Level::Warning,
                "open: an archive member is not a local file — extract it first",
            );
            return;
        }
        match self.session.open_paths(&paths) {
            Ok(report) => {
                if let Some(dir) = report.navigated.first() {
                    let target = navigate_to.unwrap_or_else(|| FsPath::from_abs(dir.clone()));
                    self.navigate_shown(slot, target);
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
        // The queue speaks `FsPath`: an archive row queues the *member*, not the container.
        let sources = self.selection_fs(slot);
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

    /// `Copy Found` / `Cut Found` (`quick-find.js:747-789`): the queue is the *whole match set* of the
    /// live search, not the pane's selection — hence its own front end, with the paths handed in by
    /// the dialog. Go closed its overlay and wrote to the pane's status line; the dialog stays up so
    /// one opening can chain verbs, so the confirmation is a toast.
    pub(crate) fn found_clipboard(&mut self, paths: &[AbsPath], op: ClipOp) {
        let what = match op {
            ClipOp::Copy => "copy found",
            ClipOp::Cut => "cut found",
        };
        if paths.is_empty() {
            self.notices
                .post(toast::Level::Warning, format!("{what}: nothing to queue"));
            return;
        }
        let req = ClipboardStore {
            op,
            // The found set is host paths; `from_abs` is the lossless injection into `FsPath`.
            sources: paths.iter().cloned().map(FsPath::from_abs).collect(),
            scope: ClipScope::Selected,
        };
        match self.session.clipboard_store(&req) {
            Ok(clip) => self.notices.post(
                toast::Level::Success,
                format!(
                    "{} {} found item(s) to the clipboard",
                    match op {
                        ClipOp::Copy => "copied",
                        ClipOp::Cut => "cut",
                    },
                    clip.items.len()
                ),
            ),
            Err(e) => self.toast_error(what, e),
        };
    }

    /// `Copy Path` / `Copy Name` over the match set (`quick-find.js:703-745`): the *text* clipboard,
    /// which is what makes the result usable outside the app — Go wrote
    /// `paths.join('\n') + '\n'` (`context-menu.js:582-591`), and so does [`Self::copy_lines`].
    pub(crate) fn found_copy_text(&mut self, paths: &[AbsPath], named: bool, ctx: &egui::Context) {
        if paths.is_empty() {
            self.nothing_selected(if named { "copy name" } else { "copy path" });
            return;
        }
        let lines: Vec<String> = paths
            .iter()
            .map(|path| path_text(path.as_path(), named))
            .collect();
        self.copy_lines(&lines, ctx);
        self.notices.post(
            toast::Level::Success,
            format!(
                "copied {}s of {} found item(s) to the system clipboard",
                if named { "name" } else { "path" },
                paths.len()
            ),
        );
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

/// `true` when `tab` is the pane that holds `path` — the directory the reveal asked for, and the one
/// its rows are keyed against.
fn lists_parent(tab: &crate::state::Tab, path: &AbsPath) -> bool {
    path.parent()
        .is_some_and(|parent| tab.path == FsPath::from_abs(parent))
}

/// The listing row a revealed name answers to, or `None` when the page does not carry it.
///
/// The name is the whole test in a plain listing, where a row is its own entry; in a flat one the
/// same name can appear at two depths, and then the *first* is what Go's `selectedPaths.add(itemPath)`
/// matched too (its `rel_path` was the full path from the search root, not from the listing).
fn row_named(tab: &crate::state::Tab, name: &str) -> Option<usize> {
    tab.entries
        .entries
        .iter()
        .position(|entry| crate::state::Tab::row_key(entry) == name)
}

/// The first three names of a doomed set, plus Go's `and N more` tail — the line both destructive
/// confirms print, because a bare count does not tell the user *which* items they are answering for.
fn named_summary(paths: &[AbsPath]) -> String {
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
    named.join(", ")
}

/// One line of `Copy path` / `Copy name` for a listing row.
fn path_text(path: &std::path::Path, named: bool) -> String {
    if named {
        path.file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
    } else {
        path.to_string_lossy().into_owned()
    }
}

/// …for a folder chip: [`FsPath`]'s own [`std::fmt::Display`] rule, so a crumb inside an archive copies
/// its `host/member/relative` spelling and not the container's.
fn folder_text(folder: &FsPath, named: bool) -> String {
    if named {
        folder.last_segment().unwrap_or_default().to_owned()
    } else {
        folder.to_string()
    }
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use zenman_core::platform;
    use zenman_core::session::SessionPaths;
    use zenman_core::store::config::ZenConfig;

    /// A session whose files live in memory-equivalent temp paths (tests only — the *crate* still
    /// never touches `std::fs`; `Session::open` is core's IO seam). The tree comes from the shared
    /// scratch module: `zenman-ui-test-<pid>-<seq>`, fresh per call, and reaped by the sweep once
    /// this process is gone (the tree must outlive the call — a `Session` holds its paths — so this
    /// hands back a plain `Session`, not a guard).
    pub(crate) fn memory_session() -> Session {
        let dir = crate::scratch::create("ui-test");
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
    fn a_new_pane_of_the_grid_gets_a_tab_on_the_directory_in_view() {
        let session = memory_session();
        let mut app = ZenApp::new(session, Vec::new());
        app.set_layout(panes_for(LayoutMode::Single));
        // Nothing has a tab yet, which is the start-of-run case: seeding it with `/` would take the
        // start away from the caller (apps/zenman opens the user's home).
        assert_eq!(app.fill_empty_panes(), 0, "an all-empty grid seeds nothing");
        app.open_tab(PaneSlot::Left, FsPath::ROOT);

        app.set_layout(panes_for(LayoutMode::Split));
        assert_eq!(
            shown(&app, PaneSlot::Right),
            Some(FsPath::ROOT),
            "the new pane of a split opens on the directory the user was looking at (Go's \
             `updateLayout`, split-view.js:68-74)"
        );
        assert_eq!(
            app.model.active_pane,
            PaneSlot::Left,
            "and seeding does not steal the focus"
        );

        app.set_layout(quad_slots());
        for slot in [PaneSlot::LeftBottom, PaneSlot::RightBottom] {
            assert_eq!(
                shown(&app, slot),
                Some(FsPath::ROOT),
                "the quad seeds both bottom panes ({slot:?})"
            );
        }
        // Panes that already have tabs keep them, and one tab is all a pane gets.
        app.set_layout(panes_for(LayoutMode::Single));
        app.set_layout(quad_slots());
        for slot in quad_slots() {
            let pane = &app.model.panes[&slot];
            assert_eq!(pane.tabs.len(), 1, "{slot:?} is not re-seeded: {pane:?}");
        }

        /// The shown tab's path, or `None` for a pane with nothing to show.
        fn shown(app: &ZenApp, slot: PaneSlot) -> Option<FsPath> {
            app.model
                .panes
                .get(&slot)
                .and_then(|pane| pane.active_tab())
                .map(|tab| tab.path.clone())
        }
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
