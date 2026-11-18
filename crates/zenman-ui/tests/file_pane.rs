//! The file pane as `egui_kittest` sees it (plan §7 step 8): the real `ZenApp`, a real `Session`, a
//! real directory on disk, the real `PaneDock` draw path — and the pane's own behaviour: rows,
//! sorting without a refetch, pagination, the grid, selection, the tab strip.
//!
//! Why this file exists next to `shell.rs`: step 7 proved the *shell* draws. Step 8's pane is the
//! first thing that draws engine data, and its Done-when clauses ("the sort header toggles asc/desc
//! and re-sorts locally without a refetch", "scroll-to-bottom triggers `LoadMore`") are only
//! observable through the whole draw path, not from a widget unit test.
//!
//! The listing is [`common::listing_tree`] — fixed names, fixed count, fixed mtimes, never the
//! developer's home directory.

mod common;

use egui_kittest::Harness;
use egui_kittest::kittest::{AccessKitNode, Queryable};

use zenman_core::path::{AbsPath, FsPath};
use zenman_core::store::config::{LayoutMode, PaneSlot, SortColumn, ViewMode};
use zenman_ui::app::ZenApp;
use zenman_ui::panels::file_pane::PaneCommand;
use zenman_ui::state::{FlatMode, Tab, panes_for};

/// The page size a fresh tab asks for ([`zenman_ui::state::DEFAULT_PAGE_SIZE`], Go's 200).
const PAGE: usize = zenman_ui::state::DEFAULT_PAGE_SIZE;

/// Frames to let the shell settle after an action.
const SETTLE: usize = 6;

/// Points one injected wheel event scrolls (`MouseWheelUnit::Point`; the scroller's own clamping
/// decides where the list ends up, not an animation).
const WHEEL_POINTS: f32 = 1200.0;

/// How many wheel events the scroll test is willing to send before calling the scroller broken.
const MAX_WHEEL_STEPS: usize = 40;

/// Where the injected wheel lands: inside the single pane's list, below the header.
const OVER_THE_LIST: egui::Pos2 = egui::Pos2::new(400.0, 400.0);

/// Send one real wheel event over the list and run a frame.
fn wheel(harness: &mut Harness<'static, ZenApp>, points: f32) {
    let input = harness.input_mut();
    // The wheel is routed by the pointer's position, so park it over the list first.
    input.events.push(egui::Event::PointerMoved(OVER_THE_LIST));
    input.events.push(egui::Event::MouseWheel {
        // This fork has no `Pixels` unit — `Point` *is* the logical-pixel one.
        unit: egui::MouseWheelUnit::Point,
        // A positive delta moves the *content* down; scrolling towards the bottom is negative.
        delta: egui::Vec2::new(0.0, -points),
        phase: egui::TouchPhase::Move,
        source: egui::MouseWheelSource::Wheel,
        modifiers: egui::Modifiers::default(),
    });
    harness.run();
}

/// A single-pane shell with a tab on the fixture listing, at a reference window size.
fn pane_harness(tag: &str) -> Harness<'static, ZenApp> {
    let session = common::session(tag);
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Single));
    let path = fixture_path(&common::listing_tree(tag));
    app.open_tab(PaneSlot::Left, path);
    let mut harness = Harness::builder()
        .with_size(egui::vec2(1200.0, 800.0))
        .with_context_setup(|ctx| {
            egui_extras::install_image_loaders(ctx);
            zenman_ui::theme::fonts::install(ctx);
            zenman_ui::theme::apply(ctx, zenman_core::store::config::ThemeMode::Dark);
        })
        .build_ui_state(
            |ui, app| {
                app.logic(ui.ctx());
                app.ui(ui);
            },
            app,
        );
    settle(&mut harness);
    harness
}

fn settle(harness: &mut Harness<'static, ZenApp>) {
    for _ in 0..SETTLE {
        harness.run();
    }
}

/// The pane's shown tab (the harness is a single pane, so there is exactly one candidate).
fn shown<'h>(harness: &'h Harness<'static, ZenApp>) -> &'h Tab {
    shown_in(harness.state())
}

fn shown_in(state: &ZenApp) -> &Tab {
    state
        .model
        .panes
        .get(&PaneSlot::Left)
        .and_then(|p| p.active_tab())
        .expect("the harness opened a tab")
}

fn names(tab: &Tab) -> Vec<String> {
    tab.entries
        .entries
        .iter()
        .map(|entry| entry.name.clone())
        .collect()
}

fn fixture_path(path: &std::path::Path) -> FsPath {
    FsPath::from_abs(AbsPath::try_from(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
}

#[test]
fn a_real_directory_lists_into_the_pane() {
    let harness = pane_harness("list");
    let tab = shown(&harness);
    assert_eq!(tab.entries.entries.len(), PAGE, "one page of the listing");
    assert!(tab.has_more, "the fixture is bigger than one page");
    assert_eq!(tab.total, common::ROW_COUNT, "and the total says so");
    // The engine's order, not the harness's: dirs first, hidden files never filtered, the
    // lowercase byte compare of plan §2.3.
    assert_eq!(
        names(tab).into_iter().take(6).collect::<Vec<_>>(),
        common::listing_names()
            .into_iter()
            .take(6)
            .collect::<Vec<_>>(),
        "the order the Go backend produces"
    );
    // The rows reach the widget tree: the pane paints them, and kittest sees the labels. Both of
    // these are inside the loaded page — `twenty-eight-chars-of-name.txt` is not (engine order puts
    // it past `row-0193`), so the addressability assertion names a row that is actually painted.
    harness.get_by_label("docs");
    harness.get_by_label("row-0000.txt");
    assert_eq!(
        harness
            .query_all_by_label("twenty-eight-chars-of-name.txt")
            .count(),
        0,
        "a row beyond the loaded page is not painted, and not addressable"
    );
    // The un-warmed directory's size cell: Go printed `40 B (undefined files)`; the port paints
    // `--` and never the word. The cells are *painted* text — a row allocates one interaction rect
    // and one a11y node, its name (plan §9.2 step 8) — so the cell text is read from the pane's own
    // cell builder, and the word is checked where it could actually appear: the widget tree.
    let docs = tab
        .entries
        .entries
        .iter()
        .find(|entry| entry.name == "docs")
        .expect("the first page holds `docs`");
    assert_eq!(
        zenman_ui::widgets::entry_row::size_cell(docs, None),
        "--",
        "an un-warmed directory's size cell is `--`"
    );
    assert_eq!(
        harness.query_all_by_label_contains("undefined").count(),
        0,
        "the string `undefined` must never be painted"
    );
    // The pane's own chrome is there too: the status bar counts the loaded rows in the listing's
    // own words. (`get_by_label_contains("item")` is not a query this test can make: the status bar
    // *and* the pagination banner both mention items, and `get_by_*` demands exactly one node.)
    assert!(
        harness
            .query_all_by_label_contains("200 items in the listing")
            .count()
            > 0,
        "the status bar counts the loaded rows"
    );
    assert_eq!(
        harness
            .query_all_by_label_contains("Loading limited")
            .count(),
        1,
        "and the pagination banner is up because the listing is not fully loaded"
    );
}

#[test]
fn the_header_sorts_the_loaded_rows_without_a_refetch() {
    // Plan §5.8's Done-when, and §6.2's "Sort is clickable" (Go shipped `data-sort` with zero
    // handlers). A click must reorder the rows the pane already holds.
    let mut harness = pane_harness("sort");
    let before = names(shown(&harness));
    harness.get_by_label("Modified").click();
    settle(&mut harness);
    let tab = shown(&harness);
    assert_eq!(
        tab.sort.column,
        SortColumn::Date,
        "the header reached the state"
    );
    assert!(
        !tab.sort.descending,
        "a fresh column starts ascending, as Go's `order=asc` default does"
    );
    assert_eq!(
        tab.entries.entries.len(),
        PAGE,
        "the loaded rows were re-ordered, not refetched"
    );
    // The expectation is the date order of **the loaded page** (`before`), not of the whole
    // listing: a header click re-sorts what the tab holds, and the fixture's mtimes are assigned
    // over all 457 rows, so a page is a prefix of the *listing*, not of the date order.
    assert_eq!(
        names(tab),
        common::loaded_by_date_ascending(&before),
        "the local re-sort agrees with the order the engine would produce"
    );
    assert_ne!(names(tab), before, "and it is a different order");

    // The active column now carries its arrow — *ascending* means ▲, and the click on it reverses.
    harness.get_by_label("Modified ▲").click();
    settle(&mut harness);
    let tab = shown(&harness);
    assert!(tab.sort.descending, "the second click reversed it");
    assert_eq!(
        names(tab),
        common::loaded_by_date_descending(&before),
        "descending, with directories still first in both directions (plan §2.3)"
    );

    // A third column, then back to the name column the pane started on.
    harness.get_by_label("Name").click();
    settle(&mut harness);
    let tab = shown(&harness);
    assert_eq!(tab.sort.column, SortColumn::Name);
    assert!(
        !tab.sort.descending,
        "a fresh column starts ascending again"
    );
    assert_eq!(names(tab), before, "and that is the order we began with");
}

#[test]
fn pagination_appends_one_page_at_a_time_and_stops_when_full() {
    let mut harness = pane_harness("page");
    assert_eq!(shown(&harness).entries.entries.len(), PAGE);
    harness.get_by_label_contains("Loading limited");
    harness
        .get_by_label(format!("Load next {PAGE} items").as_str())
        .click();
    settle(&mut harness);
    let loaded = PAGE * 2;
    assert_eq!(
        shown(&harness).entries.entries.len(),
        loaded,
        "one more page, appended to the rows already shown"
    );
    assert!(
        shown(&harness).has_more,
        "the fixture is bigger than two pages"
    );
    assert_eq!(
        names(shown(&harness)).first().map(String::as_str),
        Some("docs"),
        "appending does not disturb what was already there"
    );
    harness
        .get_by_label("Load all items (may cause lag)")
        .click();
    settle(&mut harness);
    let tab = shown(&harness);
    assert_eq!(tab.entries.entries.len(), common::ROW_COUNT);
    assert!(!tab.has_more, "everything is loaded");
    assert_eq!(
        harness
            .query_all_by_label_contains("Loading limited")
            .count(),
        0,
        "and the banner is gone with the pagination"
    );
}

#[test]
fn scrolling_to_the_bottom_asks_for_one_more_page_once() {
    // The `LoadMore` latch (plan §5.8's Done-when: "scroll-to-bottom triggers `LoadMore` exactly
    // once"). The pane's scroller is addressed by its own id; scrolling to the bottom of a
    // 457-row listing must fetch, and must not fetch twice per arrival.
    let mut harness = pane_harness("scroll");
    let rows = shown(&harness).entries.entries.len();
    // kittest's `scroll_down()` is a no-op in this fork (plan §9.2 step 8: the node helper does not
    // reach the scroller), so drive a real wheel event over the list — the path a touchpad takes —
    // until the scroller clamps at its own bottom. The assertion is about the *latch*, not the
    // distance travelled: one crossing must fetch exactly one page.
    let mut steps = 0;
    let mut reached = false;
    while !reached && steps < MAX_WHEEL_STEPS {
        wheel(&mut harness, WHEEL_POINTS);
        steps += 1;
        reached = shown(&harness).entries.entries.len() > rows;
    }
    assert!(
        reached,
        "the wheel never reached the bottom of the list ({steps} steps)"
    );
    let after = shown(&harness).entries.entries.len();
    assert_eq!(after, rows + PAGE, "reaching the bottom fetched one page");
    // Still at the bottom, still no further fetch — the latch only disarms when a page arrives,
    // and the pane stays clamped there, so these frames are the `once per crossing` half.
    settle(&mut harness);
    assert_eq!(
        shown(&harness).entries.entries.len(),
        after,
        "and it fetched once, not once per frame at the bottom"
    );
}

#[test]
fn the_grid_view_paints_the_same_rows() {
    // The view is pane state; the toolbar's toggle is an icon button (no accessible label), so the
    // command is driven exactly as the toolbar drives it — through `ZenApp::run_command`.
    let mut harness = pane_harness("grid");
    harness
        .state_mut()
        .run_command(PaneSlot::Left, PaneCommand::ToggleGrid);
    settle(&mut harness);
    let tab = shown(&harness);
    assert_eq!(tab.view, ViewMode::Grid, "the command reached the state");
    assert_eq!(tab.entries.entries.len(), PAGE, "and nothing was refetched");
    // The grid paints the same rows as tiles: the name labels are still in the tree, and the
    // listing is virtualised in tile rows, not tiles.
    harness.get_by_label("a.txt");
    assert!(
        tab.scroll.painted > 0 && tab.scroll.painted < PAGE,
        "the grid painted {} tiles of {PAGE} loaded rows — it must virtualise",
        tab.scroll.painted
    );
    // Back to the list, and the scroller's state is a different one (plan: list and grid must not
    // share scroll state).
    harness
        .state_mut()
        .run_command(PaneSlot::Left, PaneCommand::ToggleGrid);
    settle(&mut harness);
    assert_eq!(shown(&harness).view, ViewMode::List);
}

#[test]
fn a_click_selects_and_the_anchor_extends_like_go() {
    // `file-list.js:475-516`: a plain click replaces, ctrl adds, shift extends from the anchor.
    let mut harness = pane_harness("sel");
    harness.get_by_label("docs").click();
    settle(&mut harness);
    let tab = shown(&harness);
    assert_eq!(tab.sel.len(), 1, "a plain click selects one row");
    assert!(tab.sel.contains("docs"));
    assert_eq!(
        tab.sel.origin(),
        Some("docs"),
        "the anchor is kept *by key*, which is the origin-preserving half of plan §7 row 8"
    );
    // `reports` is the row after `docs`; a shift-click there ranges the two.
    harness
        .get_by_label("reports")
        .click_modifiers(egui::Modifiers {
            shift: true,
            ..Default::default()
        });
    settle(&mut harness);
    let tab = shown(&harness);
    assert_eq!(tab.sel.len(), 2, "shift extends the range inclusively");
    assert!(tab.sel.contains("docs") && tab.sel.contains("reports"));
    assert_eq!(
        tab.sel.origin(),
        Some("docs"),
        "and the anchor does not move"
    );
    // Ctrl adds a third without clearing the range.
    harness
        .get_by_label("a.txt")
        .click_modifiers(egui::Modifiers {
            ctrl: true,
            ..Default::default()
        });
    settle(&mut harness);
    assert_eq!(shown(&harness).sel.len(), 3, "ctrl adds to the selection");
    // The status bar counted them, in the pane's own words.
    harness.get_by_label_contains("3 items selected");
}

#[test]
fn the_selection_survives_a_re_sort_and_a_page_append() {
    // Plan F21: Go re-sorted, re-assigned indexes and lost the selection. Here the key is the row's
    // listing-relative path, so neither a re-sort nor an appended page can lose it.
    let mut harness = pane_harness("selsort");
    harness.get_by_label("a.txt").click();
    settle(&mut harness);
    assert!(shown(&harness).sel.contains("a.txt"));
    harness.get_by_label("Modified").click();
    settle(&mut harness);
    assert!(
        shown(&harness).sel.contains("a.txt"),
        "a re-sort kept the selection (plan F21)"
    );
    harness
        .get_by_label(format!("Load next {PAGE} items").as_str())
        .click();
    settle(&mut harness);
    let tab = shown(&harness);
    assert!(
        tab.sel.contains("a.txt") && tab.entries.entries.len() == PAGE * 2,
        "and so did an appended page"
    );
}

#[test]
fn the_flat_listing_buttons_are_disabled_and_say_why() {
    // Step 8's one honest deferral (plan §9.2 step-8 row): `ListDir` has no `flat` field until
    // step 9, so a recursive listing cannot be fetched. The buttons are *visible* and *disabled*,
    // not silently inert — and the mode itself is real view state, which the second half of this
    // test proves on the rows the pane already has.
    let mut harness = pane_harness("flat");
    let flat_labels = ["Mixed", "No folders", "Grouped"];
    let is_flat_button = |node: &AccessKitNode<'_>| {
        node.label()
            .is_some_and(|label| flat_labels.contains(&label.as_str()))
    };
    let painted = harness.query_all_by(is_flat_button).count();
    let disabled = harness
        .query_all_by(|node| is_flat_button(node) && node.is_disabled())
        .count();
    assert_eq!(painted, 3, "all three flat buttons are in the toolbar");
    assert_eq!(disabled, 3, "and all three are marked unavailable");
    assert_eq!(shown(&harness).flat, FlatMode::Off, "nothing was fetched");

    // The *grouped* view over the rows that are loaded is live: one group, the listing root.
    let tab = harness.state_mut();
    tab.run_command(PaneSlot::Left, PaneCommand::SetFlat(FlatMode::Grouped));
    settle(&mut harness);
    let tab = shown(&harness);
    assert_eq!(tab.flat, FlatMode::Grouped);
    assert!(
        tab.scroll.painted > 0,
        "the grouped order still paints rows: {:?}",
        tab.scroll
    );
}

#[test]
fn a_second_tab_lists_independently_and_the_strip_names_it() {
    let mut harness = pane_harness("tabs");
    harness
        .state_mut()
        .run_command(PaneSlot::Left, PaneCommand::NewTab);
    settle(&mut harness);
    let pane = &harness.state().model.panes[&PaneSlot::Left];
    assert_eq!(pane.tabs.len(), 2, "`+` opened a second tab");
    assert_ne!(pane.tabs[0].id, pane.tabs[1].id, "and it is its own tab");
    assert_eq!(
        pane.tabs[1].entries.entries.len(),
        PAGE,
        "the new tab listed for itself"
    );
    assert_eq!(pane.active, 1, "and became the shown one");
    // The strip paints the tab's title; the fixture directory is called `listing`. The breadcrumb
    // paints the same name for the same directory, so the strip's node is asserted as a *count*
    // over the label, not as the single node `get_by_label` demands.
    assert_eq!(
        harness.query_all_by_label("listing").count(),
        2,
        "the tab strip and the breadcrumb each name the tab's directory"
    );
    // …and the independence half survives the second name: both tabs hold their own page.
    let pane = &harness.state().model.panes[&PaneSlot::Left];
    assert!(
        pane.tabs
            .iter()
            .all(|tab| tab.entries.entries.len() == PAGE),
        "two tabs, two listings, one strip"
    );
}
