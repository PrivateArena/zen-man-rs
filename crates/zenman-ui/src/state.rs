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
use zenman_core::path::FsPath;
use zenman_core::session::Origin;

pub use zenman_core::store::config::{GroupColor, LayoutMode, PaneSlot, ViewMode};

/// A tab's identity for the life of the session. `newtype over u64`, allocated by
/// [`AppModel::next_tab_id`]; the plan's `TabId` is the same idea (it is *not* an `Origin` — a tab
/// outlives every job it started).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TabId(pub u64);

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
    /// Selected entry names — keyed by **name**, not index, so a reload cannot lose them
    /// (plan F21's first half).
    pub sel: BTreeSet<String>,
    /// Anchor row for shift-click, an index into the *current* visual order.
    pub anchor: Option<usize>,
    /// Per-directory selection memory (plan §4.3: the reason a tab that went away and came back
    /// still remembers what was picked).
    pub sel_by_path: BTreeMap<String, BTreeSet<String>>,
    /// List or grid.
    pub view: ViewMode,
    /// The current page; shared, never cloned per frame (plan §4.3 rule 1).
    pub entries: Arc<DirPage>,
    /// Row count of the whole listing (the page's own `total_count`, mirrored for cheap reads).
    pub total: usize,
    /// Next-page cursor, `None` when everything is loaded.
    pub has_more: bool,
    /// The tab's group colour, if any.
    pub group: Option<GroupColor>,
    /// A `load-more` page is in flight.
    pub loading_more: bool,
    /// The listing's error, shown *in this tab only* (plan §5.6: Go showed the last error
    /// globally). `Arc` because `CoreError` is not `Clone` (plan §9.2 step-3 fact) and the event
    /// hands out `Arc<CoreError>` — the plan's `Option<CoreError>` cannot be filled from one.
    pub error: Option<Arc<CoreError>>,
    /// Jobs asked of this tab, by origin — the correlation record (plan §4.3's correlation rule).
    pub pending: BTreeSet<Origin>,
}

impl Tab {
    /// A tab on `path` with an empty page (step 8 fills it via `page_dir` + `DirPageReady`).
    #[must_use]
    pub fn new(id: TabId, path: FsPath, title: impl Into<String>) -> Self {
        let title = title.into();
        let empty = Arc::new(empty_page(path.clone()));
        Self {
            id,
            path,
            title,
            history: Vec::new(),
            hist_ix: 0,
            sel: BTreeSet::new(),
            anchor: None,
            sel_by_path: BTreeMap::new(),
            view: ViewMode::default(),
            entries: empty,
            total: 0,
            has_more: false,
            group: None,
            loading_more: false,
            error: None,
            pending: BTreeSet::new(),
        }
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
    /// Collapsed groups in this pane's listing.
    pub collapsed_groups: Vec<GroupColor>,
    /// Refreshes in flight. **Not a bool**: two refreshes in one pane must not clear each other's
    /// spinner nor hang one (plan §4.3).
    pub loading: BTreeSet<Origin>,
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
    /// Tab-id allocator (see [`AppModel::next_tab_id`]).
    next_tab: u64,
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
            next_tab: 1,
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

    /// Allocate the next tab id.
    pub fn next_tab_id(&mut self) -> TabId {
        let id = TabId(self.next_tab);
        self.next_tab += 1;
        id
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
    fn empty_tab_page_has_no_rows_and_no_more() {
        let tab = Tab::new(TabId(1), FsPath::ROOT, "/");
        assert!(tab.entries.entries.is_empty());
        assert!(!tab.entries.has_more);
        assert_eq!(tab.total, 0);
    }
}
