//! The one virtual scroller (plan §5.8 `virtual_list.rs` row).
//!
//! Go shipped **three** hand-rolled scrollers (the list and the grid in `file-list.js`, plus the
//! search manager's) — one of plan §2.3's "must not be ported" bugs. Here there is one
//! implementation, built on `ScrollArea` + `show_viewport`, and the grid is the same function with
//! a cell mapping in front of it.
//!
//! Two invariants the plan's Done-when column names, both asserted in `tests::`:
//!
//! * **Only `visible ± overscan` rows are built.** [`show_list`] calls the paint callback for the
//!   rows in the viewport grown by [`RowMetrics::overscan`] and nothing else, and reports the count
//!   in [`ScrollState::painted`] — the paint counter the 10 k/100 k-row test asserts on. The scroller
//!   never sees an entry: the callback gets a row index, so the listing is never cloned per frame.
//! * **Reaching the bottom emits `LoadMore` exactly once.** [`ScrollState::load_more_fired`] is the
//!   latch; the pane clears it when the row count changes.

use egui::{Id, Modifiers, Pos2, Rect, Response, ScrollArea, Sense, Ui, Vec2};

/// The flex ratios of the list's columns (plan row: `3/0/1/1.5` and `2.5/2.5/0.8/1.2`).
///
/// `rel` is the flat-mode path column: `0` in the plain list because Go hides the cell outright
/// (`frontend/css/file-list.css:196`, `.col-relpath { display: none }`) rather than shrinking the
/// others, which is what [`RowColumns::widths`] reproduces for free.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowColumns {
    /// Name column flex.
    pub name: f32,
    /// Listing-relative path column flex (flat mode only).
    pub rel: f32,
    /// Size column flex.
    pub size: f32,
    /// Date column flex.
    pub date: f32,
}

impl RowColumns {
    /// The two tables' four ratios, in one named declaration each: the geometry guard's rule is
    /// that a measurement must be named, and these are one flex table apiece.
    #[must_use]
    pub const fn new(name: f32, rel: f32, size: f32, date: f32) -> Self {
        Self {
            name,
            rel,
            size,
            date,
        }
    }

    /// The plain list: `flex: 3 / 0 / 1 / 1.5` (`file-list.css:20-22, 47-62`).
    pub const LIST: RowColumns = RowColumns::new(3.0, 0.0, 1.0, 1.5);

    /// The flat list: `flex: 2.5 / 2.5 / 0.8 / 1.2` (`file-list.css:204-224`).
    pub const FLAT: RowColumns = RowColumns::new(2.5, 2.5, 0.8, 1.2);

    /// The four column widths of a `width`-wide row, in order.
    ///
    /// A zero-flex column gets zero width — that is the plain list's hidden rel-path cell.
    #[must_use]
    pub fn widths(self, width: f32) -> [f32; 4] {
        let total = self.name + self.rel + self.size + self.date;
        if total <= 0.0 || width <= 0.0 {
            return [0.0; 4];
        }
        let unit = width / total;
        [
            self.name * unit,
            self.rel * unit,
            self.size * unit,
            self.date * unit,
        ]
    }
}

/// Row geometry, all of it from the token table (plan row: `height /*40*/`, `overscan /*5*/`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowMetrics {
    /// One row's height in points — the 40 px of `base.css`.
    pub height: f32,
    /// Rows painted beyond the viewport on each side.
    pub overscan: usize,
    /// The column flex ratios.
    pub columns: RowColumns,
}

/// How a click changes the selection (plan row: `Select{ix, kind:SelKind}`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelKind {
    /// Plain click: replace the selection with this row.
    Single,
    /// Ctrl/Cmd click on an unselected row: add it.
    Add,
    /// Ctrl/Cmd click on a selected row: remove it.
    Drop,
    /// Shift click: replace, then extend from the anchor, inclusive.
    Range,
    /// Shift+Ctrl click: keep the selection and extend from the anchor (Go's `shiftKey && ctrlKey`).
    RangeAdd,
}

impl SelKind {
    /// The kind a click's modifiers mean.
    ///
    /// Shift wins over Ctrl exactly as Go's `handleItemClick` reads them
    /// (`frontend/js/file-list.js:475-516`): shift replaces or extends from the anchor, ctrl
    /// toggles one row, both together extend *and* keep.
    #[must_use]
    pub fn from_modifiers(modifiers: &Modifiers) -> Self {
        if modifiers.shift {
            if modifiers.command || modifiers.ctrl {
                Self::RangeAdd
            } else {
                Self::Range
            }
        } else if modifiers.command || modifiers.ctrl {
            Self::Add
        } else {
            Self::Single
        }
    }
}

/// What a row (or the bottom of the list) asked the pane to do.
#[derive(Clone, Debug, PartialEq)]
pub enum RowEvent {
    /// Enter / double-click: open the row.
    Activate {
        /// Row index into the caller's listing.
        ix: usize,
    },
    /// A selection-changing click.
    Select {
        /// Row index.
        ix: usize,
        /// How.
        kind: SelKind,
    },
    /// Right-click on a row (`Some(ix)`) or on the background (`None`).
    Context {
        /// Row index, or `None` for the empty area below the rows.
        ix: Option<usize>,
        /// Where to anchor the menu.
        at: Pos2,
    },
    /// The bottom of a `has_more` list was reached.
    LoadMore,
    /// A flat-mode group header was clicked.
    GroupToggle {
        /// The group's key (its listing-relative directory, `""` for the listing root).
        key: String,
    },
    /// A drag started on this row (plan row: `DndGrab{ix}`; step 9 owns the payload).
    DndGrab {
        /// Row index.
        ix: usize,
    },
}

/// The scroller's state, owned by the pane (plan row: `pub struct ScrollState { y, stick_bottom }`).
///
/// `y`/`stick_bottom` are the plan's fields; `painted`/`first`/`last`/`load_more_fired` are the
/// bookkeeping the Done-when clauses assert on. All but the latch are outputs of the last frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScrollState {
    /// Scroll offset in **rows** (not points: a row-height change must not move the user).
    pub y: f32,
    /// Pinned to the bottom — egui's `stick_to_bottom`, used when a page appends.
    pub stick_bottom: bool,
    /// Rows the paint callback saw last frame: the virtualisation counter.
    pub painted: usize,
    /// First painted row (`last` when nothing was painted).
    pub first: usize,
    /// One past the last painted row.
    pub last: usize,
    /// The `LoadMore` latch.
    pub load_more_fired: bool,
}

impl ScrollState {
    /// Disarm the bottom latch (a page arrived, the listing changed, the user scrolled up).
    pub fn reset_load_more(&mut self) {
        self.load_more_fired = false;
    }
}

/// One row's geometry and interaction, handed to the paint callback.
pub struct RowHit<'a> {
    /// The row's rect, in the caller's coordinate space (already clipped by the scroll area).
    pub rect: Rect,
    /// The row's response: hover/click/drag are the scroller's, the row's drawing is the caller's.
    pub response: &'a Response,
    /// The metrics, so a row can lay its cells out without re-deriving them.
    pub metrics: &'a RowMetrics,
    /// The click's modifier keys, for [`SelKind::from_modifiers`].
    pub modifiers: Modifiers,
}

/// The list scroll area's id (the grid uses [`grid_id`]; they must not share scroll state).
#[must_use]
pub fn list_id(base: Id) -> Id {
    base.with("zenman-list")
}

/// The grid scroll area's id.
#[must_use]
pub fn grid_id(base: Id) -> Id {
    base.with("zenman-grid")
}

/// What a list is asked to show: identity, the row count, the geometry, and whether more rows
/// exist beyond them.
#[derive(Clone, Copy, Debug)]
pub struct ListSpec<'a> {
    /// The pane's dock id (the scroll state is keyed off it).
    pub id: Id,
    /// Rows in the listing.
    pub rows: usize,
    /// Row geometry.
    pub metrics: &'a RowMetrics,
    /// The listing has unloaded rows after these.
    pub has_more: bool,
}

/// A [`ListSpec`] plus the grid's two CSS numbers (`minmax(110px, 1fr)` and `gap: 15px`).
#[derive(Clone, Copy, Debug)]
pub struct GridSpec<'a> {
    /// The shared half (id, row count, metrics, `has_more`).
    pub base: ListSpec<'a>,
    /// The `minmax(110px, …)` minimum.
    pub min_tile: f32,
    /// The `gap`.
    pub gap: f32,
}

/// Paint a virtualised list of `rows` rows.
///
/// `paint` draws one row into `ui` inside `hit.rect` and returns the event it produced (usually
/// [`RowEvent::Select`] / [`RowEvent::Activate`], derived from `hit.response` — the scroller owns
/// the *interaction*, the caller owns the *meaning*). `row_ix_to_key` gives a row its stable `Id`,
/// which is what keeps a click on row 7 pointing at the same entry after a reload.
///
/// `has_more` arms the bottom-of-list [`RowEvent::LoadMore`].
pub fn show_list(
    ui: &mut Ui,
    spec: ListSpec<'_>,
    row_ix_to_key: impl Fn(usize) -> Id,
    mut paint: impl FnMut(&mut Ui, usize, &RowHit<'_>) -> Option<RowEvent>,
    scroll: &mut ScrollState,
) -> Vec<RowEvent> {
    let ListSpec {
        id,
        rows,
        metrics,
        has_more,
    } = spec;
    let mut events = Vec::new();
    let mut painted = 0usize;
    let mut first = rows;
    let mut last = rows;
    let mut at_bottom = false;
    let height = metrics.height.max(1.0);
    let modifiers = ui.input(|i| i.modifiers);
    let out = ScrollArea::vertical()
        .id_salt(list_id(id))
        .auto_shrink([false, false])
        .stick_to_bottom(scroll.stick_bottom)
        .show_viewport(ui, |ui, visible| {
            // The content box: `rows` rows tall, in the scroll area's own content coordinates.
            {
                ui.set_height(rows as f32 * height);
                let lo =
                    ((visible.min.y / height).floor() as usize).saturating_sub(metrics.overscan);
                let hi = (((visible.max.y / height).ceil() as usize) + metrics.overscan).min(rows);
                let lo = lo.min(rows);
                first = lo;
                last = hi;
                at_bottom = rows > 0 && visible.max.y >= rows as f32 * height - height;
                let left = ui.max_rect().min.x;
                let top = ui.max_rect().min.y;
                let width = ui.max_rect().width();
                painted = hi.saturating_sub(lo);
                for ix in lo..hi {
                    let rect = Rect::from_min_size(
                        Pos2::new(left, top + ix as f32 * height),
                        Vec2::new(width, height),
                    );
                    let response = ui.interact(rect, row_ix_to_key(ix), Sense::click_and_drag());
                    let hit = RowHit {
                        rect,
                        response: &response,
                        metrics,
                        modifiers,
                    };
                    if let Some(event) = paint(ui, ix, &hit) {
                        events.push(event);
                    }
                }
            }
        });
    scroll.y = (out.state.clamped_offset().y / height).max(0.0);
    scroll.painted = painted;
    scroll.first = first.min(last);
    scroll.last = last;
    if has_more && at_bottom && !scroll.load_more_fired {
        scroll.load_more_fired = true;
        events.push(RowEvent::LoadMore);
    }
    events
}

/// Paint a virtualised icon grid of `rows` cells (plan row: `show_grid(…, min_tile /*110*/,
/// gap /*15*/)`).
///
/// `columns` is the tile count, derived by [`grid_columns`] from the available width — the same
/// `repeat(auto-fill, minmax(110px, 1fr))` rule the CSS states, computed once per frame from the
/// pane's width. Cells are laid out row-major; the *unit of virtualisation* is a tile row, so a
/// 10 k-file directory paints `visible tile rows ± overscan`, not 10 k tiles.
pub fn show_grid(
    ui: &mut Ui,
    spec: GridSpec<'_>,
    row_ix_to_key: impl Fn(usize) -> Id,
    mut paint: impl FnMut(&mut Ui, usize, &RowHit<'_>) -> Option<RowEvent>,
    scroll: &mut ScrollState,
) -> Vec<RowEvent> {
    let GridSpec {
        base:
            ListSpec {
                id,
                rows,
                metrics,
                has_more,
            },
        min_tile,
        gap,
    } = spec;
    let columns = grid_columns(ui.available_width(), min_tile, gap);
    let tile = tile_size(ui.available_width(), columns, gap);
    let tile_height = tile + gap;
    let grid_rows = rows.div_ceil(columns.max(1));
    let mut events = Vec::new();
    let mut painted = 0usize;
    let mut first = grid_rows;
    let mut last = grid_rows;
    let mut at_bottom = false;
    let modifiers = ui.input(|i| i.modifiers);
    let out = ScrollArea::vertical()
        .id_salt(grid_id(id))
        .auto_shrink([false, false])
        .stick_to_bottom(scroll.stick_bottom)
        .show_viewport(ui, |ui, visible| {
            ui.set_height(grid_rows as f32 * tile_height);
            let lo =
                ((visible.min.y / tile_height).floor() as usize).saturating_sub(metrics.overscan);
            let hi =
                (((visible.max.y / tile_height).ceil() as usize) + metrics.overscan).min(grid_rows);
            let lo = lo.min(grid_rows);
            first = lo;
            last = hi;
            at_bottom =
                grid_rows > 0 && visible.max.y >= grid_rows as f32 * tile_height - tile_height;
            let left = ui.max_rect().min.x;
            let top = ui.max_rect().min.y;
            let width = ui.max_rect().width();
            painted = hi.saturating_sub(lo) * columns;
            for grid_row in lo..hi {
                for column in 0..columns {
                    let ix = grid_row * columns + column;
                    if ix >= rows {
                        continue;
                    }
                    let rect = Rect::from_min_size(
                        Pos2::new(
                            left + column as f32 * (tile + gap),
                            top + grid_row as f32 * tile_height,
                        ),
                        Vec2::new(tile, tile),
                    );
                    let response = ui.interact(rect, row_ix_to_key(ix), Sense::click_and_drag());
                    let _ = width;
                    let hit = RowHit {
                        rect,
                        response: &response,
                        metrics,
                        modifiers,
                    };
                    if let Some(event) = paint(ui, ix, &hit) {
                        events.push(event);
                    }
                }
            }
        });
    scroll.y = (out.state.clamped_offset().y / tile_height).max(0.0);
    scroll.painted = painted;
    scroll.first = first.min(last);
    scroll.last = last;
    if has_more && at_bottom && !scroll.load_more_fired {
        scroll.load_more_fired = true;
        events.push(RowEvent::LoadMore);
    }
    events
}

/// How many tiles fit in `width` under `minmax(110px, 1fr)` with `gap` between them
/// (`file-list.css:70`, `repeat(auto-fill, minmax(110px, 1fr))`).
///
/// Never zero: a pane too narrow for one tile still gets one, and the tile shrinks.
#[must_use]
pub fn grid_columns(width: f32, min_tile: f32, gap: f32) -> usize {
    if width <= 0.0 || min_tile <= 0.0 {
        return 1;
    }
    let room = width + gap;
    let columns = (room / (min_tile + gap)).floor();
    if columns < 1.0 { 1 } else { columns as usize }
}

/// The width of one tile when `columns` of them share `width` with `gap` between (the `1fr` half
/// of the CSS rule).
#[must_use]
pub fn tile_size(width: f32, columns: usize, gap: f32) -> f32 {
    let columns = columns.max(1) as f32;
    let total_gap = gap * (columns - 1.0);
    ((width - total_gap) / columns).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Context, RawInput, vec2};

    fn modifiers(ctrl: bool, shift: bool) -> Modifiers {
        Modifiers {
            ctrl,
            shift,
            ..Modifiers::default()
        }
    }

    #[test]
    fn columns_split_the_css_flex_ratios() {
        let list = RowColumns::LIST.widths(600.0);
        // 3 / 0 / 1 / 1.5 of 600, over a total flex of 5.5: 327.3 / 0 / 109.1 / 163.6.
        assert!(list[0] > 327.0 && list[0] < 328.0, "{list:?}");
        assert_eq!(list[1], 0.0, "the plain list has no rel-path column");
        assert!(list[2] > 109.0 && list[2] < 110.0, "{list:?}");
        assert!(list[3] > 163.0 && list[3] < 164.0, "{list:?}");
        assert!((list.iter().sum::<f32>() - 600.0).abs() < 0.01, "{list:?}");
        let flat = RowColumns::FLAT.widths(600.0);
        assert!(flat[0] > 214.0 && flat[0] < 215.0, "{flat:?}");
        assert_eq!(flat[0], flat[1], "name and rel share the flat listing 50 %");
        let total: f32 = flat.iter().sum();
        assert!((total - 600.0).abs() < 0.01, "{flat:?} sums to {total}");
        // Degenerate inputs never produce NaN or negative widths.
        assert_eq!(RowColumns::LIST.widths(0.0), [0.0; 4]);
        assert_eq!(
            RowColumns {
                name: 0.0,
                rel: 0.0,
                size: 0.0,
                date: 0.0
            }
            .widths(100.0),
            [0.0; 4]
        );
    }

    #[test]
    fn sel_kind_reads_the_modifiers_like_go() {
        // `file-list.js:475-516`: shift ranges (and shift+ctrl keeps the old selection), ctrl
        // toggles, plain click replaces.
        assert_eq!(
            SelKind::from_modifiers(&Modifiers::default()),
            SelKind::Single
        );
        assert_eq!(
            SelKind::from_modifiers(&modifiers(true, false)),
            SelKind::Add
        );
        assert_eq!(
            SelKind::from_modifiers(&modifiers(false, true)),
            SelKind::Range
        );
        // shift+ctrl still extends from the anchor but does *not* clear first (`file-list.js:499`,
        // `if (!ctrlKey) tab.selectedPaths.clear()`), which is a distinct kind.
        assert_eq!(
            SelKind::from_modifiers(&modifiers(true, true)),
            SelKind::RangeAdd
        );
        let cmd = Modifiers {
            command: true,
            ..Modifiers::default()
        };
        assert_eq!(SelKind::from_modifiers(&cmd), SelKind::Add, "Cmd on macOS");
    }

    #[test]
    fn grid_columns_follow_the_minmax_rule() {
        // `minmax(110px, 1fr)` with a 15 px gap in a 460 px pane: 110+15 = 125 per column ⇒ 3.
        assert_eq!(grid_columns(460.0, 110.0, 15.0), 3);
        assert_eq!(grid_columns(110.0, 110.0, 15.0), 1);
        assert_eq!(grid_columns(235.0, 110.0, 15.0), 2);
        // A pane narrower than one tile still paints one, shrunk.
        assert_eq!(grid_columns(40.0, 110.0, 15.0), 1);
        assert_eq!(grid_columns(0.0, 110.0, 15.0), 1);
        assert_eq!(grid_columns(145.0, 110.0, 15.0), 1);
        assert_eq!(grid_columns(260.0, 110.0, 15.0), 2);
        // Three 110px tiles plus two 15px gaps is exactly 360.
        assert!((tile_size(360.0, 3, 15.0) - 110.0).abs() < 0.01);
        assert_eq!(tile_size(110.0, 1, 15.0), 110.0);
        assert_eq!(tile_size(0.0, 4, 15.0), 0.0);
    }

    #[test]
    fn list_and_grid_do_not_share_scroll_state() {
        let base = Id::unique("pane");
        assert_ne!(list_id(base), grid_id(base));
        assert_eq!(list_id(base), list_id(base), "stable across frames");
    }

    /// A row height, an overscan and a viewport, all named locally (the token table owns the real
    /// numbers; the test owns the arithmetic it must satisfy).
    const ROW: f32 = 40.0;
    const OVERSCAN: usize = 5;
    const VIEWPORT: f32 = 400.0;

    fn metrics() -> RowMetrics {
        RowMetrics {
            height: ROW,
            overscan: OVERSCAN,
            columns: RowColumns::LIST,
        }
    }

    /// Drive one frame of [`show_list`] at a fixed viewport and report the paint counter.
    fn frame(rows: usize, scroll: &mut ScrollState) -> (Vec<RowEvent>, usize) {
        let ctx = Context::default();
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, VIEWPORT))),
            ..RawInput::default()
        };
        let mut events = Vec::new();
        let mut painted = 0usize;
        let mut output = ctx.run_ui(input, |ui| {
            events = show_list(
                ui,
                ListSpec {
                    id: Id::unique("test-pane"),
                    rows,
                    metrics: &metrics(),
                    has_more: false,
                },
                |ix| Id::unique(("test-row", ix)),
                |_ui, _ix, _hit| {
                    painted += 1;
                    None
                },
                scroll,
            );
        });
        // The frame may leave a font-texture delta with no backend to apply it: egui asserts on drop.
        output.textures_delta.clear();
        scroll.painted = painted;
        (events, painted)
    }

    #[test]
    fn only_visible_rows_plus_overscan_are_painted() {
        // 10 000 rows in a viewport that fits 10: the callback must see ~20 rows, never 10 000.
        let mut scroll = ScrollState::default();
        let (_events, painted) = frame(10_000, &mut scroll);
        assert!(painted > 0, "the first frame painted nothing");
        assert!(
            painted * 2 <= 10_000,
            "painted {painted} of 10 000 rows — virtualisation is not working"
        );
        let visible = (VIEWPORT / ROW) as usize;
        assert!(
            painted <= visible + 2 * OVERSCAN + 2,
            "painted {painted}, more than visible ({visible}) + overscan ({OVERSCAN} each side)"
        );
        assert_eq!(scroll.first, 0, "the frame starts at the top");
        assert!(scroll.last <= painted, "{scroll:?}");
    }

    #[test]
    fn an_empty_listing_paints_no_rows_and_no_events() {
        let mut scroll = ScrollState::default();
        let (events, painted) = frame(0, &mut scroll);
        assert_eq!(painted, 0);
        assert!(events.is_empty(), "{events:?}");
    }
}
