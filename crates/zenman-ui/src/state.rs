//! UI state ownership (plan §4.3 + §5.7 `state.rs` row).
//!
//! Two deliberate departures from the plan text, both forced by what step 4 actually shipped:
//!
//! * `PaneSlot` / `LayoutMode` / `ViewMode` / `GroupColor` are **re-exported from
//!   `zenman_core::store::config`**, not redefined here. They are persisted values (the Go
//!   `config.json` migrates into them), so a second enum would be a second truth; the plan's
//!   `FlatMode` and `Quad` live nowhere in that table because Go never had them, and inventing a
//!   wire spelling here would break byte-compat. `ViewMode::Flat` is the plan's `FlatMode::Mixed`
//!   (core added it in step 4 with that reasoning); the quad grid is *shell* state, not config —
//!   see [`AppModel::layout`]'s field doc for how the two relate.
//! * `AppModel::tab_index` is a method, not a stored field: nothing in steps 7-8 mutates tabs
//!   during a frame, and a stale index is exactly the bug a stored mirror invites. Step 9+ can
//!   promote it to a cached field if a profile says it must.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use zenman_core::entry::DirPage;
use zenman_core::error::CoreError;
use zenman_core::path::{AbsPath, FsPath};
use zenman_core::session::Origin;
use zenman_core::store::config::SortSpec;

use crate::widgets::virtual_list::ScrollState;
use crate::widgets::virtual_list::SelKind;

pub use zenman_core::store::config::{GroupColor, LayoutMode, PaneSlot, ViewMode};

/// A tab's identity for the life of the session. `newtype over u64`, allocated by
/// [`AppModel::next_tab_id`]; the plan's `TabId` is the same idea (it is *not* an `Origin` — a tab
/// outlives every job it started).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TabId(pub u64);

/// The listing's flat (recursive) view mode.
///
/// **Plan correction.** §5.8 calls this `FlatMode` and §5.7's `state.rs` row lists it among the
/// `zenman_core::store::config` re-exports; the config enum is `ViewMode` and its `Flat` variant is
/// the *persisted* "is this tab a flat listing" answer (`docs/contracts.md` deviation ledger row 4:
/// Go stored `viewMode: "flat"`). The four-way answer Go kept in `tab.flatViewMode` was never in its
/// config at all, so it has no wire spelling and belongs to the shell. `ViewMode::Flat` therefore
/// stays the persisted value and this is the live one; [`FlatMode::from_view`] maps between them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FlatMode {
    /// An ordinary directory listing.
    #[default]
    Off,
    /// Every descendant, directories among them (Go's `mixed`).
    Mixed,
    /// Every descendant, files only (Go's `mixed_no_folders`).
    MixedNoFolders,
    /// Every descendant, files only, grouped under their directory (Go's `grouped`).
    Grouped,
}

impl FlatMode {
    /// The persisted spelling of this mode (`Flat` is "some flat mode", whichever the tab chose).
    #[must_use]
    pub const fn to_view(self) -> ViewMode {
        match self {
            Self::Off => ViewMode::List,
            Self::Mixed | Self::MixedNoFolders | Self::Grouped => ViewMode::Flat,
        }
    }

    /// The live mode a persisted value restores. The stored enum cannot tell the three flat modes
    /// apart, so a migrated session comes back in Go's own default, `mixed`.
    #[must_use]
    pub const fn from_view(view: ViewMode) -> Self {
        match view {
            ViewMode::Flat => Self::Mixed,
            ViewMode::List | ViewMode::Grid => Self::Off,
        }
    }

    /// `true` when the listing is recursive, i.e. when rows carry a `rel_path`.
    #[must_use]
    pub const fn is_flat(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// The navigation bar's recursive-listing rows: `(mode, label, tooltip)`, in Go's own words
    /// (`main.js`'s flat-view dropdown: *mixed*, *mixed_no_folders*, *grouped*) with the mode Go's
    /// plain listing had and its dropdown could not go back to.
    #[must_use]
    pub const fn rows() -> [(Self, &'static str, &'static str); 4] {
        [
            (
                Self::Off,
                "This folder",
                "List only the files and folders of this directory",
            ),
            (
                Self::Mixed,
                "Mixed",
                "List every entry under this directory, folders among them",
            ),
            (
                Self::MixedNoFolders,
                "No folders",
                "List every file under this directory, without the folder rows",
            ),
            (
                Self::Grouped,
                "Grouped",
                "The recursive listing, grouped by its own folder",
            ),
        ]
    }

    /// The mode's own word, for a tooltip that has to say what is current.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "this folder",
            Self::Mixed => "mixed",
            Self::MixedNoFolders => "no folders",
            Self::Grouped => "grouped",
        }
    }

    /// The **engine's** spelling of this mode — the value that goes into
    /// [`ListDir::flat`](zenman_core::services::dirs::ListDir::flat), and into
    /// [`DirCacheKey`](zenman_core::services::dirs::DirCacheKey) with it.
    ///
    /// `Grouped` maps to core's `Mixed`, not to its `Grouped`: Go asked for the two with *the same
    /// request* (`navigation.js:92-97`, where `mixed` and `grouped` both produce `flat=true`), the
    /// walk is identical, and the grouping is a shell-side render over `rel_path` — which is what
    /// [`crate::panels::file_pane::RowOrder`] does with it. Sharing the mode therefore shares the
    /// cache entry, which is the parity answer: two panes, one walk. (Core's own `Grouped` arm
    /// behaves the same way; the shell simply never needs to name it.)
    #[must_use]
    pub const fn to_core(self) -> zenman_core::services::dirs::FlatMode {
        match self {
            Self::Off => zenman_core::services::dirs::FlatMode::Off,
            Self::Mixed | Self::Grouped => zenman_core::services::dirs::FlatMode::Mixed,
            Self::MixedNoFolders => zenman_core::services::dirs::FlatMode::MixedNoFolders,
        }
    }
}

/// A tab's selection (plan §4.3 `sel` + `anchor`, §5.8's `PaneModel::sel`).
///
/// Keyed by the row's *listing-relative path* rather than by index, so a page swap or a reload
/// cannot move the selection (plan F21 — Go re-sorted and re-assigned indexes and lost it).
///
/// `origin` is the anchor a shift-range extends from, and it is stored as a **key, not an index**:
/// that is the "origin-preserving" half of plan §7 row 8. Go kept `anchorPath` (a path) for exactly
/// this reason; an index anchor would point at a different file after a reload.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    /// The selected row keys.
    keys: BTreeSet<String>,
    /// The row a range extends from, by key.
    origin: Option<String>,
}

impl Selection {
    /// An empty selection.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The selected keys, in sort order (a `BTreeSet`).
    #[must_use]
    pub fn keys(&self) -> &BTreeSet<String> {
        &self.keys
    }

    /// The range anchor, if one was ever set.
    #[must_use]
    pub fn origin(&self) -> Option<&str> {
        self.origin.as_deref()
    }

    /// `true` when `key` is selected.
    #[must_use]
    pub fn contains(&self, key: &str) -> bool {
        self.keys.contains(key)
    }

    /// The number of selected rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// `true` when nothing is selected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Forget the selection but keep the anchor (Go's `Esc` cleared the marks, not the anchor).
    pub fn clear(&mut self) {
        self.keys.clear();
    }

    /// Forget everything.
    pub fn reset(&mut self) {
        self.keys.clear();
        self.origin = None;
    }

    /// The anchor's index in the visual order, if it is still part of it.
    ///
    /// A linear scan: it runs on a shift-click, not per frame, and the order is a closure because
    /// the *pane* is what knows how a visual row maps to a row key (plan §5.8).
    #[must_use]
    pub fn origin_index<'a>(
        &self,
        len: usize,
        key_at: impl Fn(usize) -> Option<&'a str>,
    ) -> Option<usize> {
        let origin = self.origin.as_deref()?;
        (0..len).find(|ix| key_at(*ix) == Some(origin))
    }

    /// Apply one click (`Single`/`Add`/`Range`/`RangeAdd`, plan §5.8's `SelKind`).
    ///
    /// The semantics are Go's `handleItemClick` (`file-list.js:475-516`) with one fix: a range
    /// whose anchor has vanished becomes a plain selection instead of silently toggling, and the
    /// anchor always points at a real row.
    pub fn apply<'a>(
        &mut self,
        kind: SelKind,
        ix: usize,
        len: usize,
        key_at: impl Fn(usize) -> Option<&'a str>,
    ) {
        let Some(key) = key_at(ix).map(str::to_string) else {
            return;
        };
        match kind {
            SelKind::Single => {
                self.keys.clear();
                self.keys.insert(key.clone());
                self.origin = Some(key);
            }
            SelKind::Add | SelKind::Drop => {
                if !self.keys.remove(&key) {
                    self.keys.insert(key.clone());
                }
                self.origin = Some(key);
            }
            SelKind::Range | SelKind::RangeAdd => {
                let from = self.origin_index(len, &key_at);
                let Some(from) = from else {
                    self.keys.clear();
                    self.keys.insert(key.clone());
                    self.origin = Some(key);
                    return;
                };
                if kind == SelKind::Range {
                    self.keys.clear();
                }
                let (lo, hi) = if from <= ix { (from, ix) } else { (ix, from) };
                for index in lo..=hi {
                    if let Some(row) = key_at(index) {
                        self.keys.insert(row.to_string());
                    }
                }
                // Go leaves `anchorPath` where it was, and so does the port: repeated shift-clicks
                // extend from the same origin.
            }
        }
    }

    /// Select every row of the visual order (Go's `selectAll`, `file-list.js:679-691`).
    pub fn select_all<'a>(&mut self, len: usize, key_at: impl Fn(usize) -> Option<&'a str>) {
        for index in 0..len {
            if let Some(key) = key_at(index) {
                self.keys.insert(key.to_string());
            }
        }
        if self.origin.is_none() {
            self.origin = key_at(0).map(str::to_string);
        }
    }

    /// Select nothing, keeping the anchor.
    pub fn select_none(&mut self) {
        self.keys.clear();
    }

    /// Forget rows that are no longer in the listing (plan F21's second half, after a reload).
    /// Adopt the keys of a reloaded listing that still exist (plan F21's second half).
    pub fn retain<'a>(&mut self, len: usize, key_at: impl Fn(usize) -> Option<&'a str>) {
        let alive: BTreeSet<String> = (0..len)
            .filter_map(|ix| key_at(ix).map(str::to_string))
            .collect();
        self.keys.retain(|key| alive.contains(key));
        if self
            .origin
            .as_ref()
            .is_some_and(|origin| !alive.contains(origin))
        {
            self.origin = None;
        }
    }
}

/// One tab: a directory, a selection, a pending-job set (plan §4.3's field list).
#[derive(Clone, Debug)]
pub struct Tab {
    /// Identity.
    pub id: TabId,
    /// The listed directory, in store coordinates (plan §4.2: an archive member is a `FsPath` too).
    pub path: FsPath,
    /// Display title (the basename, or the store label; step 8 owns its exact wording).
    pub title: String,
    /// Back/forward stack, no depth cap (Go parity: `history.go` grew without bound).
    pub history: Vec<FsPath>,
    /// Cursor into `history`; `history.len()` means "not in history" until the first navigation.
    pub hist_ix: usize,
    /// The selection — keyed by row key, never by index, so a reload cannot lose it
    /// (plan F21's first half), and carrying the range anchor (its *origin*).
    pub sel: Selection,
    /// Per-directory selection memory (plan §4.3: the reason a tab that went away and came back
    /// still remembers what was picked).
    pub sel_by_path: BTreeMap<String, Selection>,
    /// List or grid.
    pub view: ViewMode,
    /// The flat (recursive) listing mode of this tab (plan §5.8's `flat`).
    pub flat: FlatMode,
    /// The column order the rows are shown in. A header click changes this *and re-sorts the loaded
    /// rows*, which is the plan's "without a refetch" rule.
    pub sort: SortSpec,
    /// The flat listing's collapsed groups, by directory key (Go's `collapsedFileGroups`).
    pub collapsed_groups: BTreeSet<String>,
    /// The scroller's state (plan §5.8's `scroll: &mut ScrollState`).
    pub scroll: ScrollState,
    /// The current page; shared, never cloned per frame (plan §4.3 rule 1).
    pub entries: Arc<DirPage>,
    /// Row count of the whole listing (the page's own `total_count`, mirrored for cheap reads).
    pub total: usize,
    /// Next-page cursor, `None` when everything is loaded.
    pub has_more: bool,
    /// The tab group this tab belongs to, if any. The **identity**, never the colour: what a group is
    /// called, what it is painted and whether it is folded all live in [`Pane::groups`], so re-colouring
    /// a group is one field write and not a re-key of every member (design §2).
    pub group: Option<GroupId>,
    /// A `load-more` page is in flight.
    pub loading_more: bool,
    /// The page size this tab asked for (the banner quotes it; the shell sets it from
    /// `Limits::dir_page_default`).
    pub page_size: usize,
    /// The listing's error, shown *in this tab only* (plan §5.6: Go showed the last error
    /// globally). `Arc` because `CoreError` is not `Clone` (plan §9.2 step-3 fact) and the event
    /// hands out `Arc<CoreError>` — the plan's `Option<CoreError>` cannot be filled from one.
    pub error: Option<Arc<CoreError>>,
    /// Jobs asked of this tab, by origin — the correlation record (plan §4.3's correlation rule).
    pub pending: BTreeSet<Origin>,
    /// The row a drag started on this frame, `None` when nothing is being dragged. Step 9's
    /// `DockContent::resolve_drag` reads it to build the payload; the pane records it from
    /// `RowEvent::DndGrab` (plan §5.8's `DndGrab{ix}`).
    pub drag_row: Option<usize>,
    /// The row <kbd>F2</kbd> put into rename, by **row key** (never an index: a reload must not
    /// move the editor onto another file — the same reason [`Tab::sel`] is keyed). Step 10a's
    /// `Command::Rename` sets it; the in-cell editor of step 10c is the consumer that must clear it
    /// (commit) or reset it to `None` (cancel).
    pub rename_target: Option<String>,
    /// The live in-cell rename editor for `rename_target` (step 10c). `None` until the row it names
    /// has been painted, because the editor is *the row's* — it has no rect before the row does.
    pub rename_editor: Option<RenameEditor>,
}

impl Tab {
    /// A tab on `path` with an empty page (step 8 fills it via `page_dir` + `DirPageReady`).
    #[must_use]
    pub fn new(id: TabId, path: FsPath, title: impl Into<String>) -> Self {
        let title = title.into();
        let empty = Arc::new(empty_page(path.clone()));
        Self {
            id,
            path: path.clone(),
            title,
            // The trail *includes* the path the tab starts on, so `hist_ix` always addresses the
            // current entry (step 13b: that is what makes `go_forward` able to return anything).
            history: vec![path],
            hist_ix: 0,
            sel: Selection::new(),
            sel_by_path: BTreeMap::new(),
            view: ViewMode::default(),
            flat: FlatMode::default(),
            sort: SortSpec::DEFAULT_SORT,
            collapsed_groups: BTreeSet::new(),
            scroll: ScrollState::default(),
            entries: empty,
            total: 0,
            has_more: false,
            group: None,
            loading_more: false,
            page_size: DEFAULT_PAGE_SIZE,
            drag_row: None,
            rename_target: None,
            rename_editor: None,
            error: None,
            pending: BTreeSet::new(),
        }
    }

    /// A copy of this tab under a new id — Go's `duplicateTab` (`tabs.js:365-367`). The *view* travels
    /// (`view`, `flat`, `sort`, colour) because duplicating a red grid tab is how you make a second red
    /// grid tab; the listing, selection, scroll, history and group-collapse set do not, because the
    /// duplicate refetches and a copied page would be stale the moment either tab refreshed.
    #[must_use]
    pub fn duplicate(&self, id: TabId) -> Self {
        let mut tab = Self::new(id, self.path.clone(), self.title.clone());
        tab.view = self.view;
        tab.flat = self.flat;
        tab.sort = self.sort;
        tab.group = self.group;
        tab
    }
}

/// A page with no rows, the starting value of a tab before its first listing (step 8 swaps in the
/// real page; the type is `DirPage`, not `Option`, exactly as plan §4.3 requires).
#[must_use]
pub fn empty_page(path: FsPath) -> DirPage {
    DirPage {
        path,
        entries: Vec::new(),
        cursor: zenman_core::cursor::Cursor(0),
        has_more: false,
        total_count: 0,
        loaded_so_far: 0,
    }
}

/// One dock pane: its tabs, the active one, the MRU ring, in-flight refreshes (plan §4.3).
#[derive(Clone, Debug, Default)]
pub struct Pane {
    /// Tabs, left to right.
    pub tabs: Vec<Tab>,
    /// Index into `tabs` (clamped by every mutation; `Pane::active_tab` never panics on it).
    pub active: usize,
    /// Most-recently-used tab ring, cap 5 (`Alt+Z` in step 10).
    pub mru: std::collections::VecDeque<TabId>,
    /// This pane's tab groups, in creation order. An entry exists **iff** at least one tab names it
    /// ([`Pane::prune_groups`]) — see the invariants at the head of the group algebra.
    pub groups: Vec<TabGroup>,
    /// The tabs the strip has multi-selected (right-click acts on all of them, and so does
    /// <kbd>New Tab Group</kbd>). Ids, because a reorder must not move the selection onto another tab.
    pub tab_sel: BTreeSet<TabId>,
    /// The anchor a <kbd>Shift</kbd>-click ranges from.
    pub tab_sel_anchor: Option<TabId>,
    /// Refreshes in flight. **Not a bool**: two refreshes in one pane must not clear each other's
    /// spinner nor hang one (plan §4.3).
    pub loading: BTreeSet<Origin>,
    /// The address field is open (plan §5.8's breadcrumb row; the pane owns it, Go owned one per
    /// pane too).
    pub address_editing: bool,
    /// Its buffer.
    pub address_buffer: String,
    /// The tab strip's overflow list is open.
    pub tabs_overflow_open: bool,
}

impl Tab {
    /// The key a row is selected by: its listing-relative path, which is its name in an ordinary
    /// directory and its `a/b/c.txt` in a flat one (where names repeat).
    #[must_use]
    pub fn row_key(entry: &zenman_core::entry::FileEntry) -> &str {
        if entry.rel_path.is_empty() {
            &entry.name
        } else {
            &entry.rel_path
        }
    }

    /// Move the tab to `path`: the history grows (no depth cap, Go parity) and any forward entries
    /// the user had gone back past are truncated (plan §5.8's breadcrumb Done-when).
    ///
    /// Returns the previous path, which is what the caller stores in the per-directory selection
    /// memory before the view changes.
    pub fn navigate_to(&mut self, path: FsPath, title: impl Into<String>) -> Option<FsPath> {
        let previous = self.path.clone();
        // Anything the user had gone *forward* past is gone: a new navigation from the middle of
        // the trail truncates it (plan §5.8's breadcrumb Done-when). `hist_ix + 1` keeps the current
        // entry, which is the last thing the forward step may return to.
        self.history.truncate(self.hist_ix + 1);
        if previous != path {
            self.history.push(path.clone());
        }
        self.hist_ix = self.history.len().saturating_sub(1);
        self.title = title.into();
        self.path = path;
        self.sel.reset();
        self.scroll = ScrollState::default();
        self.collapsed_groups.clear();
        (previous != self.path).then_some(previous)
    }

    /// `true` when there is somewhere to go back to.
    #[must_use]
    pub fn can_go_back(&self) -> bool {
        self.hist_ix > 0
    }

    /// `true` when there is somewhere to go forward to.
    #[must_use]
    pub fn can_go_forward(&self) -> bool {
        self.hist_ix + 1 < self.history.len()
    }

    /// Step back one entry, returning the path to list.
    pub fn go_back(&mut self) -> Option<FsPath> {
        if !self.can_go_back() {
            return None;
        }
        self.hist_ix -= 1;
        let path = self.history.get(self.hist_ix).cloned()?;
        self.title = path.last_segment().unwrap_or("/").to_string();
        Some(path)
    }

    /// Step forward one entry, returning the path to list.
    pub fn go_forward(&mut self) -> Option<FsPath> {
        if !self.can_go_forward() {
            return None;
        }
        self.hist_ix += 1;
        let path = self.history.get(self.hist_ix).cloned()?;
        self.title = path.last_segment().unwrap_or("/").to_string();
        Some(path)
    }

    /// The absolute path a row points at, `None` when the tab is on an archive (plan §4.2: only a
    /// local path can have a `dir_size` answer).
    #[must_use]
    pub fn row_abs(&self, entry: &zenman_core::entry::FileEntry) -> Option<AbsPath> {
        // `rel_path` is only filled by the flat listings; in a plain directory the row *is* its
        // name — joining the name is what makes the row point at the entry, not at the directory.
        let rel = if entry.rel_path.is_empty() {
            entry.name.as_str()
        } else {
            entry.rel_path.as_str()
        };
        let path = self.path.join_rel(rel).ok()?;
        path.host_abs()
    }

    /// The selected rows' names, in listing order (the status bar's tooltip; Go's was insertion
    /// order, plan §5.8's fix).
    #[must_use]
    pub fn selected_names(&self) -> Vec<String> {
        self.entries
            .entries
            .iter()
            .filter(|entry| self.sel.contains(Self::row_key(entry)))
            .map(|entry| entry.name.clone())
            .collect()
    }

    /// Rows of the whole listing that are not loaded yet.
    #[must_use]
    pub fn rows_left(&self) -> usize {
        self.total.saturating_sub(self.entries.entries.len())
    }

    /// Install a freshly listed page (plan §4.3: the tab's `Arc`, never its contents, is replaced).
    pub fn set_page(&mut self, page: Arc<DirPage>) {
        self.total = page.total_count;
        self.has_more = page.has_more;
        self.entries = page;
        self.error = None;
        self.scroll.reset_load_more();
    }

    /// Replace the page *and* keep the selection honest (plan F21's second half): every key the new
    /// listing still has survives the swap — a reload, or a switch to a flat mode whose root rows
    /// carry the same keys — and the rows that vanished drop out. Scroll is not touched.
    pub fn set_page_keeping_selection(&mut self, page: Arc<DirPage>) {
        self.set_page(page);
        self.prune_selection();
    }

    /// Forget the selected rows this listing does not have. Called on a page *replacement*, never on
    /// an append, and keyed by row path — so a re-sort cannot prune anything (plan F21).
    pub fn prune_selection(&mut self) {
        let keys: Vec<&str> = self.entries.entries.iter().map(Self::row_key).collect();
        self.sel.retain(keys.len(), |ix| keys.get(ix).copied());
    }

    /// Append a `load-more` page (Go's `tab.loadedEntries.concat(data.entries)`).
    ///
    /// The one place a listing is copied, and it is copied on a page boundary, never per frame.
    pub fn append_page(&mut self, page: Arc<DirPage>) {
        let mut merged = (*self.entries).clone();
        merged.entries.extend(page.entries.iter().cloned());
        merged.cursor = page.cursor;
        merged.has_more = page.has_more;
        merged.total_count = page.total_count;
        merged.loaded_so_far = merged.entries.len();
        self.total = merged.total_count;
        self.has_more = merged.has_more;
        self.entries = Arc::new(merged);
        self.scroll.reset_load_more();
    }

    /// Re-sort the loaded rows in place (the header click's "without a refetch", plan §5.8).
    pub fn resort(&mut self, sort: SortSpec) {
        self.sort = sort;
        let mut rows = (*self.entries).clone();
        zenman_core::services::sort::sort_entries(&mut rows.entries, sort);
        self.entries = Arc::new(rows);
    }
}

impl Pane {
    /// The shown tab, if the pane has one.
    #[must_use]
    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs
            .get(self.active.min(self.tabs.len().saturating_sub(1)))
    }

    /// The shown tab, mutably.
    pub fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        let ix = self.active.min(self.tabs.len().saturating_sub(1));
        self.tabs.get_mut(ix)
    }

    /// Push a tab to the MRU ring, keeping the cap.
    pub fn touch_mru(&mut self, id: TabId) {
        self.mru.retain(|existing| *existing != id);
        self.mru.push_front(id);
        while self.mru.len() > MRU_CAP {
            self.mru.pop_back();
        }
    }
}

// ------------------------------------------------------------------  tab groups (design §2)
//
// The algebra below is the only way a group changes. Everything the user can do — the strip's chip,
// the tab menu, a drag, a restart — ends up here, which is what keeps the four invariants true:
//
// 1. a group's members are **contiguous** in `Pane::tabs` (`Pane::compact`);
// 2. `Pane::groups` holds an entry **iff** at least one tab names it (`Pane::prune_groups`, and pruning
//    happens **before** compacting — compacting with a dangling id is the panic site);
// 3. `Pane::tab_sel` only holds ids of tabs this pane has;
// 4. a collapsed group still shows its **active** tab, so `Pane::active` is never a hidden index and
//    activation never has to edit `collapsed`.

/// One tab group of one pane.
///
/// A runtime identity, minted per shell by [`AppModel::next_group_id`] exactly like [`TabId`], and
/// persisted only as an opaque string ([`GroupId::to_store`]) because the strip, the menus, the hover
/// popup and a rename prompt all need to name a group without holding a borrow of the pane.
///
/// Colour is **not** identity: [`TabGroup`] carries both, and [`Pane::set_group_color`] can move a group
/// to another colour without any member having to change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GroupId(pub u64);

impl GroupId {
    /// The store spelling of this id — `g7`. Written into [`TabSession::group_id`]
    /// (`zenman_core::store::config`) and read back by [`GroupId::from_store`], which is what lets a
    /// restored session keep the ids it was saved with instead of dirtying itself (design §4).
    #[must_use]
    pub fn to_store(self) -> String {
        format!("g{}", self.0)
    }

    /// The id a stored string names, when this app wrote it. Anything else (`None`) is a hand-edited
    /// or foreign id and gets a fresh one.
    #[must_use]
    pub fn from_store(raw: &str) -> Option<Self> {
        let tail = raw.strip_prefix('g')?;
        if tail.is_empty() || !tail.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        tail.parse::<u64>().ok().map(Self)
    }
}

/// One tab group: a label, a colour and a fold (`docs/tab-groups-design.md` §2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabGroup {
    /// Identity, minted per app by [`AppModel::next_group_id`].
    pub id: GroupId,
    /// The chip's text; **empty means "name it after the colour"**, which is why the label is derived
    /// ([`TabGroup::label`]) instead of defaulted — an unnamed group that gets re-coloured has to follow.
    pub name: String,
    /// The palette entry the chip and its members' accent bars paint with.
    pub color: GroupColor,
    /// Folded to the chip alone.
    pub collapsed: bool,
}

impl TabGroup {
    /// A group of `id` in `color`, unnamed and expanded.
    #[must_use]
    pub fn new(id: GroupId, color: GroupColor) -> Self {
        Self {
            id,
            name: String::new(),
            color,
            collapsed: false,
        }
    }

    /// What the chip says: the user's name, or the colour's own word while it has none.
    #[must_use]
    pub fn label(&self) -> &str {
        if self.name.is_empty() {
            self.color.name()
        } else {
            &self.name
        }
    }

    /// `true` when this group is the unnamed default, whose label is its colour.
    #[must_use]
    pub fn is_unnamed(&self) -> bool {
        self.name.is_empty()
    }
}

/// How a strip click changes the multi-selection (`SelKind` of the tab strip).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabSelKind {
    /// Plain click: this tab alone.
    Replace,
    /// <kbd>Ctrl</kbd>/`Cmd`-click: in or out of the set.
    Toggle,
    /// <kbd>Shift</kbd>-click: the run from the anchor to here (hidden tabs included — the strip's order
    /// is the tab order, and a collapsed group the user cannot see is still the block they meant).
    Range,
}

impl Pane {
    /// The group `id`, if this pane still has it.
    #[must_use]
    pub fn group(&self, id: GroupId) -> Option<&TabGroup> {
        self.groups.iter().find(|group| group.id == id)
    }

    /// The colour a tab's accent bar paints with — `None` for an ungrouped tab, and for a tab whose
    /// group is gone (which [`Pane::debug_assert_invariants`] calls a bug, not a state).
    #[must_use]
    pub fn tab_color(&self, group: Option<GroupId>) -> Option<GroupColor> {
        group.and_then(|id| self.group(id)).map(|group| group.color)
    }

    /// The group of the tab `id`, if the pane has it.
    #[must_use]
    pub fn group_of(&self, id: TabId) -> Option<GroupId> {
        self.tabs
            .iter()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.group)
    }

    /// The indices of a group's tabs, in strip order.
    #[must_use]
    pub fn members(&self, id: GroupId) -> Vec<usize> {
        self.tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| tab.group == Some(id))
            .map(|(ix, _)| ix)
            .collect()
    }

    /// The ids of a group's tabs, in strip order.
    #[must_use]
    pub fn member_ids(&self, id: GroupId) -> Vec<TabId> {
        self.members(id)
            .iter()
            .filter_map(|&ix| self.tabs.get(ix))
            .map(|tab| tab.id)
            .collect()
    }

    /// The group of the shown tab, which is the question a chip answers for one click.
    #[must_use]
    pub fn active_group(&self) -> Option<GroupId> {
        self.active_tab().and_then(|tab| tab.group)
    }

    /// Every tab of the pane, in strip order — the multi-selection resolved.
    #[must_use]
    pub fn selected_tabs(&self) -> Vec<TabId> {
        self.tabs
            .iter()
            .filter(|tab| self.tab_sel.contains(&tab.id))
            .map(|tab| tab.id)
            .collect()
    }

    /// The first colour no group of this pane holds (`GroupColor::ALL` order), or — when all nine are
    /// taken, which needs nine groups and is legal — the first colour overall.
    #[must_use]
    pub fn free_color(&self) -> GroupColor {
        *GroupColor::ALL
            .iter()
            .find(|color| !self.groups.iter().any(|group| group.color == **color))
            .unwrap_or(&GroupColor::A)
    }

    /// Make `tabs` one group of `color`, unnamed, and return its id. Nothing comes back for a selection
    /// of fewer than one live tab, which is the menu's "enabled" rule written down once.
    pub fn create_group(
        &mut self,
        id: GroupId,
        tabs: &[TabId],
        color: GroupColor,
    ) -> Option<GroupId> {
        let hit: Vec<TabId> = self.resolve(tabs);
        if hit.is_empty() {
            return None;
        }
        for tab in &mut self.tabs {
            if hit.contains(&tab.id) {
                tab.group = Some(id);
            }
        }
        self.groups.push(TabGroup::new(id, color));
        self.prune_groups();
        self.compact();
        Some(id)
    }

    /// Add `tabs` to an existing group (a drop on a chip, `Add to group` in the tab menu).
    pub fn join_group(&mut self, id: GroupId, tabs: &[TabId]) -> bool {
        if self.group(id).is_none() {
            return false;
        }
        let hit = self.resolve(tabs);
        if hit.is_empty() {
            return false;
        }
        for tab in &mut self.tabs {
            if hit.contains(&tab.id) {
                tab.group = Some(id);
            }
        }
        self.prune_groups();
        self.compact();
        true
    }

    /// Take `tabs` out of whatever group they are in, and drop the group when the last one leaves.
    pub fn leave_group(&mut self, tabs: &[TabId]) -> usize {
        let hit = self.resolve(tabs);
        let mut left = 0;
        for tab in &mut self.tabs {
            if hit.contains(&tab.id) && tab.group.take().is_some() {
                left += 1;
            }
        }
        self.prune_groups();
        self.compact();
        left
    }

    /// Release every member and forget the group — the tabs stay open (design §1: *ungroup*).
    pub fn ungroup(&mut self, id: GroupId) -> usize {
        let members = self.member_ids(id);
        let left = self.leave_group(&members);
        self.groups.retain(|group| group.id != id);
        left
    }

    /// Rename a group. An empty name is the *unnamed* state, not a name of zero width.
    pub fn rename_group(&mut self, id: GroupId, name: &str) -> bool {
        let Some(group) = self.groups.iter_mut().find(|group| group.id == id) else {
            return false;
        };
        group.name = name.trim().to_string();
        true
    }

    /// Fold or unfold a group. Never called on the shown tab's group by the *keyboard* paths, which is
    /// invariant 4's other half: the strip keeps that tab visible either way, so this cannot hide it.
    pub fn set_collapsed(&mut self, id: GroupId, collapsed: bool) -> bool {
        let Some(group) = self.groups.iter_mut().find(|group| group.id == id) else {
            return false;
        };
        group.collapsed = collapsed;
        true
    }

    /// Toggle it ([`TabCommand::ToggleGroup`](crate::panels::tab_bar::TabCommand::ToggleGroup)).
    pub fn toggle_collapsed(&mut self, id: GroupId) -> bool {
        let Some(want) = self.group(id).map(|group| !group.collapsed) else {
            return false;
        };
        self.set_collapsed(id, want)
    }

    /// Re-colour a group, and if `color` is already held, **merge** the two: the members of `id` join
    /// the group that owns the colour, which comes out expanded — folding tabs the user was looking at is
    /// a surprise, not a merge (design §2). Returns the group that survived.
    pub fn set_group_color(&mut self, id: GroupId, color: GroupColor) -> Option<GroupId> {
        let target = self
            .groups
            .iter()
            .find(|group| group.color == color && group.id != id)
            .map(|group| group.id);
        let Some(survivor) = target else {
            let group = self.groups.iter_mut().find(|group| group.id == id)?;
            group.color = color;
            return Some(id);
        };
        for tab in &mut self.tabs {
            if tab.group == Some(id) {
                tab.group = Some(survivor);
            }
        }
        self.groups.retain(|group| group.id != id);
        self.set_collapsed(survivor, false);
        self.prune_groups();
        self.compact();
        Some(survivor)
    }

    /// Append a group row. Only the restore path needs this: a runtime group is born in
    /// [`Pane::create_group`].
    pub fn add_group(&mut self, group: TabGroup) {
        if !self.groups.iter().any(|have| have.id == group.id) {
            self.groups.push(group);
        }
    }

    /// The single path that removes tabs: it keeps `active`, the MRU ring, the multi-selection and the
    /// group table in step, which is why nothing outside this `impl` may `tabs.remove` (invariant 2/3).
    /// Returns the ids it actually closed, in strip order.
    pub fn close_tabs(&mut self, ids: &[TabId]) -> Vec<TabId> {
        let hit = self.resolve(ids);
        if hit.is_empty() {
            return hit;
        }
        self.tabs.retain(|tab| !hit.contains(&tab.id));
        for id in &hit {
            self.mru.retain(|existing| existing != id);
            self.tab_sel.remove(id);
        }
        if self
            .tab_sel_anchor
            .is_some_and(|anchor| hit.contains(&anchor))
        {
            self.tab_sel_anchor = self.tab_sel.iter().next().copied();
        }
        // Clamp only. Which tab takes the focus after a close is the MRU ring's question, and the caller
        // answers it (`ZenApp::close_shown_tab`); this just never leaves `active` off the end.
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
        self.prune_groups();
        hit
    }

    /// The shown tab's id, or `None` in an empty pane.
    #[must_use]
    pub fn active_id(&self) -> Option<TabId> {
        self.active_tab().map(|tab| tab.id)
    }

    /// Show `id` (the id-based half of `SelectTab`): clamps, records the range anchor and the MRU ring.
    /// Never touches a group's `collapsed` — invariant 4 keeps the shown tab visible instead.
    pub fn activate(&mut self, id: TabId) -> bool {
        let Some(ix) = self.tabs.iter().position(|tab| tab.id == id) else {
            return false;
        };
        self.active = ix;
        self.tab_sel = BTreeSet::from([id]);
        self.tab_sel_anchor = Some(id);
        self.touch_mru(id);
        true
    }

    /// The strip's click handling: one tab, toggled, or the run from the anchor (see [`TabSelKind`]).
    /// The shown tab always follows the click — a strip click selects *and* shows, as in a browser; the
    /// rest of the set rides along for the verbs that need it.
    pub fn select_tab(&mut self, id: TabId, kind: TabSelKind) -> bool {
        let Some(ix) = self.tabs.iter().position(|tab| tab.id == id) else {
            return false;
        };
        match kind {
            TabSelKind::Replace => {
                self.tab_sel = BTreeSet::from([id]);
                self.tab_sel_anchor = Some(id);
            }
            TabSelKind::Toggle => {
                if !self.tab_sel.insert(id) {
                    self.tab_sel.remove(&id);
                }
                self.tab_sel_anchor = Some(id);
            }
            TabSelKind::Range => {
                let anchor = self.tab_sel_anchor.unwrap_or(id);
                let from = self
                    .tabs
                    .iter()
                    .position(|tab| tab.id == anchor)
                    .unwrap_or(ix);
                let (lo, hi) = (from.min(ix), from.max(ix));
                self.tab_sel = self
                    .tabs
                    .iter()
                    .skip(lo)
                    .take(hi - lo + 1)
                    .map(|tab| tab.id)
                    .collect();
            }
        }
        self.tab_sel
            .retain(|live| self.tabs.iter().any(|tab| tab.id == *live));
        true
    }

    /// Drop the rows no tab names any more (invariant 2). Idempotent, and called by every mutation that
    /// can empty a group.
    pub fn prune_groups(&mut self) {
        self.groups
            .retain(|group| self.tabs.iter().any(|tab| tab.group == Some(group.id)));
    }

    /// Restore contiguity (invariant 1): walking the strip, the first member of a group brings all of
    /// them with it. Tab order inside the group is kept, which is what makes it safe to run after a
    /// merge, a create or a restore of a Go session that never kept its letters contiguous.
    ///
    /// Runs on identity, not colour — two groups may share a colour once nine of them exist, and the
    /// compacting must not merge them.
    pub fn compact(&mut self) {
        if !self.tabs.iter().any(|tab| tab.group.is_some()) {
            return;
        }
        let shown = self.active_id();
        let len = self.tabs.len();
        let mut taken: Vec<bool> = vec![false; len];
        let mut order: Vec<usize> = Vec::with_capacity(len);
        for ix in 0..len {
            if taken[ix] {
                continue;
            }
            taken[ix] = true;
            order.push(ix);
            let Some(group) = self.tabs[ix].group else {
                continue;
            };
            for other in 0..len {
                if !taken[other] && self.tabs[other].group == Some(group) {
                    taken[other] = true;
                    order.push(other);
                }
            }
        }
        if order.iter().copied().eq(0..len) {
            return;
        }
        let mut slots: Vec<Option<Tab>> = self.tabs.drain(..).map(Some).collect();
        self.tabs = order
            .iter()
            .filter_map(|ix| slots.get_mut(*ix).and_then(Option::take))
            .collect();
        if let Some(id) = shown {
            self.active = self
                .tabs
                .iter()
                .position(|tab| tab.id == id)
                .unwrap_or(self.active.min(self.tabs.len().saturating_sub(1)));
        }
    }

    /// The invariants of this module, as a runtime check (design §2). Called at the end of
    /// `ZenApp::run_command` and of the workspace restore, so a UI path that breaks one is caught in
    /// the suite that exercised it rather than in the field.
    pub fn debug_assert_invariants(&self) {
        for group in &self.groups {
            let members = self.members(group.id);
            debug_assert!(
                !members.is_empty(),
                "group {:?} ({}) has no members (invariant 2)",
                group.id,
                group.label()
            );
            let contiguous = members.windows(2).all(|pair| pair[1] == pair[0] + 1);
            debug_assert!(
                contiguous,
                "group {:?} ({}) is not one run: {members:?} (invariant 1)",
                group.id,
                group.label()
            );
        }
        for tab in &self.tabs {
            if let Some(id) = tab.group {
                debug_assert!(
                    self.group(id).is_some(),
                    "tab {:?} names the dead group {id:?} (invariant 2)",
                    tab.id
                );
            }
        }
        for id in &self.tab_sel {
            debug_assert!(
                self.tabs.iter().any(|tab| tab.id == *id),
                "tab_sel holds the dead tab {id:?} (invariant 3)"
            );
        }
        debug_assert!(
            self.active < self.tabs.len() || self.tabs.is_empty(),
            "active {} points off a strip of {} (invariant 4)",
            self.active,
            self.tabs.len()
        );
    }

    /// Keep only the ids of tabs this pane actually has, in strip order — the resolution every
    /// id-carrying command runs first, so a tab closed between the click and the apply is ignored
    /// instead of shifting a later index (design §6).
    #[must_use]
    fn resolve(&self, ids: &[TabId]) -> Vec<TabId> {
        self.tabs
            .iter()
            .filter(|tab| ids.contains(&tab.id))
            .map(|tab| tab.id)
            .collect()
    }
}

/// The page size a fresh tab asks the engine for, until the shell applies `Limits`
/// (`Limits::dir_page_default` is the same 200 Go used; the value is passed in by the caller).
pub const DEFAULT_PAGE_SIZE: usize = 200;

/// [`Pane::mru`] capacity, plan §4.3's "cap 5".
pub const MRU_CAP: usize = 5;

/// An off-canvas drawer (the Go app's two sidebars).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrawerState {
    /// Whether it is open.
    pub open: bool,
}

/// The shell's own view state (plan §4.3 `AppModel`). The clipboard, modal stack and notices queue
/// arrive with their steps (9 and 10); `notices` lives in `widgets::toast::State` on `ZenApp`
/// because the toast widget owns its expiry.
#[derive(Clone, Debug)]
pub struct AppModel {
    /// Every pane by slot. A slot absent from the map is not part of the current grid — the grid
    /// shape and the pane contents are one question, answered once (which is how the plan's
    /// `layout_mode_is_consistent` test can be a real check).
    pub panes: BTreeMap<PaneSlot, Pane>,
    /// The persisted layout (core's `LayoutMode`: `Single | Split`). The plan's `Quad` is *shell*
    /// state with no Go wire counterpart — see [`AppModel::quad`], which derives the four-pane
    /// question from the pane map instead of inventing a config value Go cannot read.
    pub layout: LayoutMode,
    /// The pane that owns the keyboard focus.
    pub active_pane: PaneSlot,
    /// Left drawer (bookmarks/places, step 14 fills the content).
    pub left_drawer: DrawerState,
    /// Right drawer (disks — `widgets::disk_meter` is its first occupant).
    pub right_drawer: DrawerState,
    /// Action-log slide-in (step 14's `panels/action_log.rs`).
    pub log_panel: bool,
    /// The tabs a [`DirChanged`](zenman_core::events::EngineEvent::DirChanged) invalidated and that
    /// have not been re-listed yet (see [`AppModel::mark_stale`]). A *set* — that is what coalesces
    /// ten events of one paste into one reload per tab.
    pub stale: BTreeSet<TabId>,
    /// Tab-id allocator (see [`AppModel::next_tab_id`]).
    next_tab: u64,
    /// Tab-group-id allocator (see [`AppModel::next_group_id`]).
    next_group: u64,
}

impl Default for AppModel {
    fn default() -> Self {
        Self {
            panes: BTreeMap::new(),
            layout: LayoutMode::default(),
            active_pane: PaneSlot::default(),
            left_drawer: DrawerState::default(),
            right_drawer: DrawerState::default(),
            log_panel: false,
            stale: BTreeSet::new(),
            next_tab: 1,
            next_group: 1,
        }
    }
}

impl AppModel {
    /// Fresh shell state with the layout's panes registered and empty.
    #[must_use]
    pub fn new(layout: LayoutMode) -> Self {
        let mut model = Self {
            layout,
            ..Self::default()
        };
        model.set_grid(panes_for(layout));
        model
    }

    /// The plan row's consistency rule: the pane map and `layout` agree, for all three modes.
    /// `Single` is reachable from the quad grid — Go's F3 silently dropped the bottom panes' state
    /// on the way back; here `set_grid` *is* the single mutation, so nothing can be dropped.
    #[must_use]
    pub fn layout_mode_is_consistent(&self) -> bool {
        let want: Vec<PaneSlot> = panes_for(self.layout);
        let mut have: Vec<PaneSlot> = self.panes.keys().copied().collect();
        have.sort();
        let mut want_sorted = want;
        want_sorted.sort();
        have == want_sorted
    }

    /// `true` when the grid shows all four panes (the plan's `LayoutMode::Quad`, derived not stored).
    #[must_use]
    pub fn quad(&self) -> bool {
        self.panes.len() == 4
    }

    /// Replace the pane grid, preserving the state of every pane that stays.
    pub fn set_grid(&mut self, slots: Vec<PaneSlot>) {
        let old = std::mem::take(&mut self.panes);
        for slot in &slots {
            if let Some(pane) = old.get(slot) {
                self.panes.insert(*slot, pane.clone());
            } else {
                self.panes.insert(*slot, Pane::default());
            }
        }
        if !slots.contains(&self.active_pane) {
            self.active_pane = slots.first().copied().unwrap_or(PaneSlot::Left);
        }
    }

    /// The plan row's accessor.
    pub fn pane_mut(&mut self, slot: PaneSlot) -> &mut Pane {
        self.panes.entry(slot).or_default()
    }

    /// The shown tab of the active pane.
    pub fn tab_mut(&mut self) -> Option<&mut Tab> {
        let slot = self.active_pane;
        self.panes.get_mut(&slot)?.active_tab_mut()
    }

    /// Every tab of `slot` the tab strip should show — with the tab-group collapse (step 8's
    /// filtering) this is still every tab; the strip is the filter's owner.
    #[must_use]
    pub fn visible_tabs(&self, slot: PaneSlot) -> Vec<&Tab> {
        self.panes
            .get(&slot)
            .map(|pane| pane.tabs.iter().collect())
            .unwrap_or_default()
    }

    /// Every tab currently displaying `path` — the invalidation and post-revert fan-out helper.
    #[must_use]
    pub fn tabs_showing(&self, path: &FsPath) -> Vec<(PaneSlot, usize)> {
        self.panes
            .iter()
            .flat_map(|(slot, pane)| {
                pane.tabs
                    .iter()
                    .enumerate()
                    .filter(|(_, tab)| &tab.path == path)
                    .map(move |(ix, _)| (*slot, ix))
            })
            .collect()
    }

    /// Borrow one addressed tab.
    #[must_use]
    pub fn tab_at(&self, k: (PaneSlot, usize)) -> Option<&Tab> {
        self.panes.get(&k.0)?.tabs.get(k.1)
    }

    /// Borrow one addressed tab mutably.
    pub fn tab_at_mut(&mut self, k: (PaneSlot, usize)) -> Option<&mut Tab> {
        self.panes.get_mut(&k.0)?.tabs.get_mut(k.1)
    }

    /// `(PaneSlot, index)` of a tab id, the method form of the plan's stored `tab_index`.
    #[must_use]
    pub fn tab_index(&self, id: TabId) -> Option<(PaneSlot, usize)> {
        self.panes.iter().find_map(|(slot, pane)| {
            pane.tabs
                .iter()
                .position(|tab| tab.id == id)
                .map(|ix| (*slot, ix))
        })
    }

    /// Mark every tab that lists `path` **or something inside it** for a reload (plan §4.3's "explicit
    /// invalidation on every `DirChanged` touching the path or an ancestor"), answering with how many
    /// tabs it caught.
    ///
    /// The answer is the coalescing rule's other half: the queue is a set of tab ids, so N events in
    /// one frame — a paste that touched a directory and its parent, a watcher burst — reload each
    /// tab exactly once, in [`AppModel::take_stale`]'s single pass.
    pub fn mark_stale(&mut self, path: &FsPath) -> usize {
        let hits: Vec<TabId> = self
            .panes
            .values()
            .flat_map(|pane| pane.tabs.iter())
            .filter(|tab| tab.path.starts_with(path))
            .map(|tab| tab.id)
            .collect();
        let count = hits.len();
        self.stale.extend(hits);
        count
    }

    /// Drain the reload queue (empty afterwards, so an event that arrives *during* the reload pass is
    /// queued for the next one instead of being lost or double-run).
    pub fn take_stale(&mut self) -> Vec<TabId> {
        std::mem::take(&mut self.stale).into_iter().collect()
    }

    /// Queue one tab for that same next-pass reload, without an event to ask for it.
    ///
    /// This is the deferred half of a session restore: the tab that is not on screen does not need to
    /// be read before the window exists, and reading it there would put an archive open, a dead mount,
    /// or a slow directory in front of the first frame.
    pub fn queue_reload(&mut self, id: TabId) {
        self.stale.insert(id);
    }

    /// Allocate the next tab id.
    pub fn next_tab_id(&mut self) -> TabId {
        let id = TabId(self.next_tab);
        self.next_tab += 1;
        id
    }

    /// A fresh tab-group identity. `restore` re-uses the ids a session was saved with, so this also has
    /// to be **bumped past** them (`ZenApp::apply_workspace`) or the next `New Tab Group` would mint an
    /// id that is already on the strip.
    #[must_use]
    pub fn next_group_id(&mut self) -> GroupId {
        let id = GroupId(self.next_group);
        self.next_group += 1;
        id
    }

    /// Adopt a group id read back from a session: same number, and the allocator moves past it.
    ///
    /// Restoring must not re-mint, because a restore that changed its own ids would come out `!=` the
    /// bytes it read, and [`Autosave`](zenman_core::services::workspaces::Autosave) compares the session
    /// by value — every start would dirty the session file.
    pub fn adopt_group_id(&mut self, raw: &str) -> GroupId {
        match GroupId::from_store(raw) {
            Some(id) => {
                self.next_group = self.next_group.max(id.0 + 1);
                id
            }
            None => self.next_group_id(),
        }
    }

    /// The pane slots the current grid shows.
    #[must_use]
    pub fn grid_slots(&self) -> Vec<PaneSlot> {
        self.panes.keys().copied().collect()
    }
}

/// The pane set a layout mode means (the single source for `dock::default_layout` too).
#[must_use]
pub fn panes_for(layout: LayoutMode) -> Vec<PaneSlot> {
    match layout {
        LayoutMode::Single => vec![PaneSlot::Left],
        LayoutMode::Split => vec![PaneSlot::Left, PaneSlot::Right],
    }
}

/// The four-pane grid (the plan's `Quad`; not in `LayoutMode` because the Go config cannot store
/// it — see [`AppModel::quad`]).
#[must_use]
pub fn quad_slots() -> Vec<PaneSlot> {
    vec![
        PaneSlot::Left,
        PaneSlot::Right,
        PaneSlot::LeftBottom,
        PaneSlot::RightBottom,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_is_consistent_and_quad_reduces_to_split_then_single() {
        let mut model = AppModel::new(LayoutMode::Split);
        assert!(model.layout_mode_is_consistent());
        model.set_grid(quad_slots());
        assert!(model.quad());
        // The plan's F3 regression: Single must be reachable from the four-pane grid with the
        // surviving panes' state intact.
        model
            .pane_mut(PaneSlot::Left)
            .tabs
            .push(Tab::new(TabId(9), FsPath::ROOT, "left"));
        model.set_grid(panes_for(LayoutMode::Single));
        model.layout = LayoutMode::Single;
        assert!(model.layout_mode_is_consistent());
        assert_eq!(
            model.panes[&PaneSlot::Left].tabs.len(),
            1,
            "pane state survived"
        );
        assert!(!model.quad());
    }

    #[test]
    fn active_pane_follows_the_grid() {
        let mut model = AppModel::new(LayoutMode::Split);
        model.active_pane = PaneSlot::Right;
        model.set_grid(panes_for(LayoutMode::Single));
        assert_eq!(model.active_pane, PaneSlot::Left);
    }

    #[test]
    fn tabs_showing_and_tab_at_agree() {
        let mut model = AppModel::new(LayoutMode::Split);
        let a = model.next_tab_id();
        let b = model.next_tab_id();
        model
            .pane_mut(PaneSlot::Left)
            .tabs
            .push(Tab::new(a, FsPath::ROOT, "/"));
        model
            .pane_mut(PaneSlot::Right)
            .tabs
            .push(Tab::new(b, FsPath::ROOT, "/"));
        let hits = model.tabs_showing(&FsPath::ROOT);
        assert_eq!(hits.len(), 2);
        for (slot, ix) in hits {
            assert_eq!(
                model.tab_at((slot, ix)).map(|t| t.path.clone()),
                Some(FsPath::ROOT)
            );
            let id = model.tab_at((slot, ix)).map(|t| t.id).unwrap_or(a);
            assert_eq!(model.tab_index(id), Some((slot, ix)));
        }
        assert_eq!(model.tab_index(b), model.tab_index(b));
    }

    #[test]
    fn mru_caps_at_five() {
        let mut pane = Pane::default();
        for i in 1..=8 {
            pane.touch_mru(TabId(i));
        }
        assert_eq!(pane.mru.len(), MRU_CAP);
        assert_eq!(pane.mru.front(), Some(&TabId(8)), "most recent first");
        assert!(pane.mru.iter().all(|id| id.0 >= 4));
    }

    #[test]
    fn a_change_to_a_directory_marks_itself_and_everything_below_it_once() {
        use zenman_core::path::AbsPath;

        let mut model = AppModel::new(LayoutMode::Split);
        let here = model.next_tab_id();
        let inside = model.next_tab_id();
        let elsewhere = model.next_tab_id();
        let root = AbsPath::try_from(std::path::Path::new("/watched"))
            .unwrap_or_else(|e| panic!("a fixture path parses: {e}"));
        let root = FsPath::from_abs(root);
        model
            .pane_mut(PaneSlot::Left)
            .tabs
            .push(Tab::new(here, root.clone(), "watched"));
        model.pane_mut(PaneSlot::Left).tabs.push(Tab::new(
            inside,
            root.join("sub").unwrap_or(root.clone()),
            "sub",
        ));
        let other = FsPath::from_abs(
            AbsPath::try_from(std::path::Path::new("/other")).unwrap_or_else(|e| panic!("{e}")),
        );
        model
            .pane_mut(PaneSlot::Right)
            .tabs
            .push(Tab::new(elsewhere, other, "other"));
        // The directory itself and the tab deep inside it are stale; the unrelated pane is not.
        assert_eq!(model.mark_stale(&root), 2, "the path and its descendants");
        // Twice more, however many events arrive: the queue is a set, one reload per tab.
        assert_eq!(model.mark_stale(&root), 2);
        assert_eq!(model.mark_stale(&root), 2);
        let queued = model.take_stale();
        assert_eq!(queued.len(), 2, "three events, one reload each");
        assert!(!queued.contains(&elsewhere), "and never the unrelated tab");
        assert!(model.take_stale().is_empty(), "the queue drained");
    }

    #[test]
    fn a_flat_mode_maps_to_the_engine_mode_that_walks_the_same_way() {
        use zenman_core::services::dirs::FlatMode as Core;

        assert_eq!(FlatMode::Off.to_core(), Core::Off);
        assert_eq!(FlatMode::Mixed.to_core(), Core::Mixed);
        assert_eq!(
            FlatMode::Grouped.to_core(),
            Core::Mixed,
            "grouped walks like mixed: the grouping is the pane's render over `rel_path`"
        );
        assert_eq!(FlatMode::MixedNoFolders.to_core(), Core::MixedNoFolders);
        // The one question the engine answers differently — rows, not recursion.
        assert!(!Core::MixedNoFolders.includes_dirs());
        assert!(Core::MixedNoFolders.is_flat());
    }

    #[test]
    fn a_reload_keeps_the_selection_exactly_as_far_as_the_rows_do() {
        // The unit behind [`Tab::prune_selection`], which a page swap calls: plan F21's second half.
        let keys = |ix: usize| match ix {
            0 => Some("kept"),
            1 => Some("gone"),
            _ => None,
        };
        let mut sel = Selection::new();
        sel.apply(SelKind::Single, 0, 2, keys);
        sel.apply(SelKind::RangeAdd, 1, 2, keys);
        assert_eq!(sel.len(), 2);
        // The reloaded listing holds `kept` and `new`; `gone` was deleted out from under the tab.
        sel.retain(2, |ix| match ix {
            0 => Some("kept"),
            _ => Some("new"),
        });
        assert!(
            sel.contains("kept") && !sel.contains("gone"),
            "the selection survives exactly as far as the rows do"
        );
        assert_eq!(sel.origin(), Some("kept"), "and so does the anchor");
    }

    #[test]
    fn empty_tab_page_has_no_rows_and_no_more() {
        let tab = Tab::new(TabId(1), FsPath::ROOT, "/");
        assert!(tab.entries.entries.is_empty());
        assert!(!tab.entries.has_more);
        assert_eq!(tab.total, 0);
    }
}

/// The in-cell rename editor (plan §5.8 `modals.rs`: "in-cell `TextEdit` for rename").
///
/// It lives on the [`Tab`], not on the app, for the same reason the selection does: the row it sits
/// in belongs to one tab of one pane, and a tab that reloads out from under it loses both.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenameEditor {
    /// The row's key (the listing-relative identity `Selection` uses).
    pub key: String,
    /// The name being typed.
    pub buf: String,
    /// The inline refusal — Go's second `alert()`, now in the place the user is already looking.
    pub error: Option<String>,
    /// The names already in the directory (the loaded listing's; the engine's guard is the
    /// authority, this is the fast path).
    pub taken: Vec<String>,
    /// Until the first frame the field holds the keyboard.
    pub fresh: bool,
    /// How many leading characters to select on open — the base name, extension excluded.
    pub select: usize,
}

impl RenameEditor {
    /// Open an editor on `name`: the buffer holds the whole name, the selection holds the base.
    ///
    /// The extension is excluded the way every desktop file manager does it — the split is at the
    /// *last* dot that is not leading, so `.hidden` and `archive.tar.gz` keep their prefixes intact.
    #[must_use]
    pub fn new(key: String, name: &str) -> Self {
        Self {
            key,
            buf: name.to_string(),
            error: None,
            taken: Vec::new(),
            fresh: true,
            select: base_name_len(name),
        }
    }
}

/// Where the base name ends (see [`RenameEditor::new`]).
#[must_use]
pub fn base_name_len(name: &str) -> usize {
    match name.rfind('.') {
        Some(dot) if dot > 0 => dot,
        _ => name.len(),
    }
}
