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
use zenman_ui::keymap::Command;
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
    pane_harness_at(tag, &common::listing_tree(tag), egui::vec2(1200.0, 800.0))
}

/// [`pane_harness`] that also **hands back** the recording launcher, for the tests that need to see
/// what was asked of the desktop. Every harness in this file already runs on `common::NoDesktopEnv`
/// (`common::session` is [`common::session_with_launcher`] with the recorder dropped), so the
/// difference here is observability, not safety: nothing ever spawns an editor mid-run.
fn pane_harness_logging(
    tag: &str,
) -> (
    Harness<'static, ZenApp>,
    std::sync::Arc<common::NoDesktopEnv>,
) {
    let (session, env) = common::session_with_launcher(tag);
    let dir = common::listing_tree(tag);
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Single));
    let path = fixture_path(&dir);
    app.open_tab(PaneSlot::Left, path);
    (build(app, egui::vec2(1200.0, 800.0)), env)
}

/// The one harness builder the two entry points above share: the shell's real draw path, driven
/// through the fork's `logic`/`ui` pair exactly as `apps/zenman` does.
fn build(app: ZenApp, size: egui::Vec2) -> Harness<'static, ZenApp> {
    let mut harness = Harness::builder()
        .with_accessibility_check(true)
        .with_size(size)
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

/// [`pane_harness`] with the tab's directory and the window size under test — the breadcrumb
/// tests need a deep start directory and a strip too narrow for it.
fn pane_harness_at(tag: &str, dir: &std::path::Path, size: egui::Vec2) -> Harness<'static, ZenApp> {
    let session = common::session(tag);
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Single));
    let path = fixture_path(dir);
    app.open_tab(PaneSlot::Left, path);
    build(app, size)
}

fn settle(harness: &mut Harness<'static, ZenApp>) {
    for _ in 0..SETTLE {
        harness.run();
    }
}

/// The recursive-listing control is one dropdown in the navigation bar, so a mode is two clicks:
/// the button, then its row.
fn choose_flat(harness: &mut Harness<'static, ZenApp>, mode: &str) {
    harness.get_by_label("recursive listing").click();
    settle(harness);
    harness.get_by_label(mode).click();
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
fn clicking_mixed_lists_the_recursive_rows() {
    // Step 9d: the flat buttons fetch. The tab starts on the plain three-row listing; one click on
    // *Mixed* must produce the engine's recursive page, `rel_path` and all, through `page_dir`.
    let mut harness = nested_harness("flatmix");
    assert_eq!(
        rel_rows(shown(&harness)),
        vec![
            ("alpha".into(), true),
            ("beta".into(), true),
            ("top.txt".into(), false)
        ]
    );
    assert_eq!(shown(&harness).flat, FlatMode::Off);
    choose_flat(&mut harness, "Mixed");
    // The flat page is a real worker-thread job (`page_dir` turns a flat miss into `JobKind::List`)
    // and kittest's fake time never advances it — wait on the wall clock until every job is done and
    // the tab's own pending set has drained (see `common::settle_until`).
    common::settle_until(&mut harness, "the mixed walk finishes", |h| {
        h.state().jobs.live_count() == 0 && shown(h).pending.is_empty()
    });
    let tab = shown(&harness);
    assert_eq!(
        tab.flat,
        FlatMode::Mixed,
        "the toolbar's click reached the state"
    );
    assert_eq!(
        rel_rows(tab),
        common::nested_flat_rows(),
        "the recursive page, in the engine's own order: dirs first, then files, both by base name"
    );
    // The depth a `rel_path` carries is the grouping key and the rel-path column's text: `b2.txt`
    // is two levels down, and the pane shows it (`entry_row` paints `rel_path` in the flat columns).
    let deep = tab
        .entries
        .entries
        .iter()
        .find(|entry| entry.name == "b2.txt")
        .expect("the nested file is in the flat page");
    assert_eq!(deep.rel_path, "beta/inner/b2.txt");
    assert_eq!(
        deep.rel_path.matches('/').count() + 1,
        3,
        "three levels deep"
    );
    assert_eq!(
        deep.depth, 2,
        "Go's `depth` is the *separator* count of the rel path (`api/dir.go:153-160`), not the level count"
    );
    // The nested row is addressable, i.e. it was really painted.
    harness.get_by_label("b2.txt");
    // The flat column table is in use (the rel-path cell exists, and is not a zero-width one).
    assert!(
        zenman_ui::panels::file_pane::columns_of(FlatMode::Mixed).rel > 0.0,
        "a flat listing has a rel-path column"
    );
    // The fetch was a *job* (`page_dir` turns a flat miss into `JobKind::List`), and its origin was
    // bound to this pane and this tab — `JobsUi::track(info, pane, tab)`, plan §4.3's correlation rule.
    let id = tab.id;
    let tracked = harness
        .state()
        .jobs
        .foreground
        .iter()
        .any(|job| job.kind == zenman_core::jobs::JobKind::List && job.tab == Some(id));
    assert!(tracked, "the flat walk's origin is bound to the asking tab");
    assert!(
        tab.pending.is_empty(),
        "and resolved by the time the page landed"
    );
}

#[test]
fn no_folders_drops_the_folder_rows_and_still_shows_the_nested_files() {
    // `flat=true&no_folders=true` (`api/dir.go:141-143`): the *rows* go, the recursion stays.
    // Going Mixed → No folders also pins the re-fetch: the mode is part of the engine's cache key, so
    // the second click cannot be served the first mode's page.
    let mut harness = nested_harness("flatomit");
    choose_flat(&mut harness, "Mixed");
    common::settle_until(&mut harness, "the mixed walk finishes", |h| {
        h.state().jobs.live_count() == 0 && shown(h).pending.is_empty()
    });
    assert_eq!(
        shown(&harness).entries.entries.len(),
        8,
        "mixed keeps the folders"
    );
    choose_flat(&mut harness, "No folders");
    // The mode is part of the engine's cache key, so this is a *second* real walk, not a re-paint.
    common::settle_until(&mut harness, "the no-folders walk finishes", |h| {
        h.state().jobs.live_count() == 0 && shown(h).pending.is_empty()
    });
    let tab = shown(&harness);
    assert_eq!(tab.flat, FlatMode::MixedNoFolders);
    assert_eq!(
        rel_rows(tab),
        common::nested_flat_file_rows(),
        "a fresh page without the folder rows — not the mixed page re-painted"
    );
    assert!(
        !rel_rows(tab).iter().any(|(_, is_dir)| *is_dir),
        "no directory rows at all"
    );
    assert!(
        rel_rows(tab)
            .iter()
            .any(|(rel, _)| rel == "beta/inner/b2.txt"),
        "and the walk still went through `beta/inner`"
    );
    assert_eq!(
        harness.query_all_by_label("alpha").count(),
        0,
        "the suppressed folder is not painted anywhere"
    );
}

#[test]
fn grouped_walks_like_mixed_and_collapse_hides_a_group() {
    // Go asks for `grouped` and `mixed` with the *same* request (`navigation.js:92-97`); the grouping
    // is the pane's render over `rel_path`. So Mixed → Grouped must keep the same rows — and change
    // only the visual order.
    let mut harness = nested_harness("flatgrp");
    choose_flat(&mut harness, "Mixed");
    common::settle_until(&mut harness, "the mixed walk finishes", |h| {
        h.state().jobs.live_count() == 0 && shown(h).pending.is_empty()
    });
    let mixed = rel_rows(shown(&harness));
    choose_flat(&mut harness, "Grouped");
    // Grouped is the *same request* as Mixed (`navigation.js:92-97`), so no second walk: a frame
    // settle is enough here.
    settle(&mut harness);
    let tab = shown(&harness);
    assert_eq!(tab.flat, FlatMode::Grouped);
    assert_eq!(
        rel_rows(tab),
        mixed,
        "the same walk: grouping is a render, not a second fetch"
    );
    // The visual order now interleaves headers — the order *measured* from this fixture (the runs
    // are Go's: a header opens each contiguous run of one group, so a group whose rows are
    // interrupted by another's gets its header repeated). `""` is the listing root (Go's `.`, headed
    // `Root`); its count is the whole group — 3 rows (`alpha`, `beta`, `top.txt`) — even though its
    // two runs hold 2 and 1. `[beta:2]` appears twice because `group_of("beta/inner") == "beta"`: the
    // inner directory belongs to `beta`'s group, and the header reports the group, not the run.
    let order = visual_rows(tab);
    assert_eq!(
        order,
        vec![
            "[:3]",
            "alpha",
            "beta",
            "[beta:2]",
            "beta/inner",
            "[alpha:2]",
            "alpha/a1.txt",
            "alpha/a2.txt",
            "[beta:2]",
            "beta/b1.txt",
            "[beta/inner:1]",
            "beta/inner/b2.txt",
            "[:3]",
            "top.txt",
        ],
        "the grouped order, headers included (the count is the group's, the runs are Go's)"
    );
    // The header's count really is the whole group's: `beta`'s group is `beta/inner` + `beta/b1.txt`.
    let inner = tab
        .entries
        .entries
        .iter()
        .find(|entry| entry.rel_path == "beta/inner")
        .expect("the nested directory is in the flat page");
    assert_eq!(
        zenman_ui::panels::file_pane::group_of(inner),
        "beta",
        "the nested directory is keyed into its top-level group"
    );
    // Collapse: the group header is paint-only decoration with one interaction rect (rule 10), so it
    // has no a11y node of its own — the click is driven at the position the rows around it pin down.
    let header = group_header_pos(&harness, "a1.txt");
    click_at(&mut harness, header);
    let tab = shown(&harness);
    assert!(
        tab.collapsed_groups.contains("alpha"),
        "the header's click toggled its group: {:?}",
        tab.collapsed_groups
    );
    assert_eq!(
        visual_rows(tab),
        vec![
            "[:3]",
            "alpha",
            "beta",
            "[beta:2]",
            "beta/inner",
            "[alpha:2]",
            "[beta:2]",
            "beta/b1.txt",
            "[beta/inner:1]",
            "beta/inner/b2.txt",
            "[:3]",
            "top.txt",
        ],
        "the collapsed group's rows left the visual order, its header stayed with the whole count"
    );
    assert_eq!(
        harness.query_all_by_label("a1.txt").count(),
        0,
        "and they are not painted either"
    );
    // The header did not move (a collapse removes rows *below* it), so the same point expands again.
    click_at(&mut harness, header);
    assert!(
        shown(&harness).collapsed_groups.is_empty(),
        "and the second click expands"
    );
    harness.get_by_label("a1.txt");
}

#[test]
fn a_directory_change_reloads_the_tab_that_shows_it() {
    // Plan §4.1/F2 end to end in the UI: the op is the engine's real `mkdir` (step 9b/9c), the event
    // is its `DirChanged`, and the pane reloads — keeping the selection of a row that still exists.
    let tree = common::listing_tree("dirchange");
    // Sweep a leftover from an earlier run: the fixture tree lives in cargo's scratch directory and
    // persists, so a `made-dir` from a previous run would answer `AlreadyExists` and mis-count.
    let _ = std::fs::remove_dir(tree.join("made-dir"));
    let mut harness = pane_harness_at("dirchange", &tree, egui::vec2(1200.0, 800.0));
    harness.get_by_label("docs").click();
    settle(&mut harness);
    let before = shown(&harness).entries.entries.len();
    let scroll = shown(&harness).scroll.y;
    assert_eq!(shown(&harness).total, common::ROW_COUNT);
    let session = &harness.state().session;
    let dest = AbsPath::try_from(tree.as_path()).unwrap_or_else(|e| panic!("{tree:?}: {e}"));
    session
        .mkdir(
            &zenman_core::ops::mkdir::MkdirReq::new(dest, "made-dir"),
            session.next_origin(),
        )
        .unwrap_or_else(|e| panic!("mkdir: {}", e.user_message()));
    // One frame drains the event queue: both the invalidation and the reload it asked for — but the
    // invalidation itself is coalesced on a *wall-clock* window and the reload is a job, so frames
    // alone never get there (`common::settle_until`).
    harness.run();
    common::settle_until(
        &mut harness,
        "the reload lands (total grows by the mkdir)",
        |h| h.state().jobs.live_count() == 0 && shown(h).total == common::ROW_COUNT + 1,
    );
    let tab = shown(&harness);
    assert_eq!(tab.entries.entries.len(), before, "same page size");
    assert_eq!(
        tab.total,
        common::ROW_COUNT + 1,
        "and one more row in the listing"
    );
    assert!(
        names(tab).contains(&"made-dir".to_string()),
        "the new directory is in the reloaded page: {:?}",
        names(tab).into_iter().take(4).collect::<Vec<_>>()
    );
    assert!(
        tab.sel.contains("docs"),
        "the reload kept the selection of a row that still exists (plan F21)"
    );
    assert_eq!(tab.scroll.y, scroll, "and kept the scroll position");
    harness.get_by_label("made-dir");
    assert!(
        harness.state().model.stale.is_empty(),
        "the reload queue is drained, so a tab is not re-listed every frame"
    );
}

#[test]
fn several_changes_in_one_frame_reload_a_tab_once() {
    // The coalescing half, end to end: two ops, both events pending when the frame runs, and one
    // reload that shows both. `model.stale` is a set of tab ids, which is what makes it one.
    let tree = common::listing_tree("dirburst");
    // Same sweep as the single-change test — both directories persist across runs otherwise.
    let _ = std::fs::remove_dir(tree.join("first-dir"));
    let _ = std::fs::remove_dir(tree.join("second-dir"));
    let mut harness = pane_harness_at("dirburst", &tree, egui::vec2(1200.0, 800.0));
    let session = &harness.state().session;
    let dest = AbsPath::try_from(tree.as_path()).unwrap_or_else(|e| panic!("{tree:?}: {e}"));
    for name in ["first-dir", "second-dir"] {
        session
            .mkdir(
                &zenman_core::ops::mkdir::MkdirReq::new(dest.clone(), name),
                session.next_origin(),
            )
            .unwrap_or_else(|e| panic!("mkdir {name}: {}", e.user_message()));
    }
    // The real "not yet reloaded" check: `mkdir` is synchronous and only *queues* its
    // `DirChanged`, and nothing has polled since — so the tab still shows the pre-mutation listing.
    assert_eq!(
        shown(&harness).total,
        common::ROW_COUNT,
        "no reload has run yet: the events are still queued"
    );
    assert!(
        names(shown(&harness))
            .iter()
            .all(|n| n != "first-dir" && n != "second-dir"),
        "and neither new directory is in the page yet"
    );
    harness.run();
    // Both coalesced events and the one reload they trigger run on the wall clock / a worker thread.
    common::settle_until(
        &mut harness,
        "the single reload lands (total grows by two)",
        |h| h.state().jobs.live_count() == 0 && shown(h).total == common::ROW_COUNT + 2,
    );
    let tab = shown(&harness);
    assert_eq!(tab.total, common::ROW_COUNT + 2, "both directories landed");
    assert!(harness.state().model.stale.is_empty());
    for name in ["first-dir", "second-dir"] {
        assert!(
            names(tab).contains(&name.to_string()),
            "{name} is in the reloaded page"
        );
    }
}

/// A single-pane shell on [`common::nested_tree`] — the flat-mode fixture.
fn nested_harness(tag: &str) -> Harness<'static, ZenApp> {
    pane_harness_at(tag, &common::nested_tree(tag), egui::vec2(1200.0, 800.0))
}

/// The pane's rows as `(listing-relative path, is_dir)` — the flat page's real payload, since
/// `rel_path` is what the flat modes add (`api/dir.go:141-149`).
fn rel_rows(tab: &Tab) -> Vec<(String, bool)> {
    tab.entries
        .entries
        .iter()
        .map(|entry| {
            (
                if entry.rel_path.is_empty() {
                    entry.name.clone()
                } else {
                    entry.rel_path.clone()
                },
                entry.is_dir,
            )
        })
        .collect()
}

/// The pane's visual order: `"[key:count]"` for a group header, the row key for an entry. Headers are
/// paint-only, so this — not the widget tree — is where they are visible.
fn visual_rows(tab: &Tab) -> Vec<String> {
    let order = zenman_ui::panels::file_pane::RowOrder::build(tab);
    (0..order.len())
        .map(|ix| match order.get(ix) {
            Some(zenman_ui::panels::file_pane::VisualRowRef::Group { key, count }) => {
                format!("[{key}:{count}]")
            }
            _ => order.key_at(ix).unwrap_or("<unknown>").to_string(),
        })
        .collect()
}

/// Where a group header sits, derived from the rows pinned around it: the header of the group above
/// the row named `label`, one row pitch up. (The pitch is measured from two painted rows, never
/// hardcoded — it is the pane's geometry, not the test's.)
fn group_header_pos(harness: &Harness<'static, ZenApp>, label: &str) -> egui::Pos2 {
    let row = node_rect(harness, label);
    let below = node_rect(harness, next_row_label(label).unwrap_or(label));
    let pitch = (below.min.y - row.min.y).abs();
    let centre = row.min.y - pitch / 2.0;
    egui::pos2(row.center().x, centre)
}

/// The row after `label` in [`common::nested_flat_rows`] — the pitch's second measurement.
fn next_row_label(label: &str) -> Option<&'static str> {
    match label {
        "a1.txt" => Some("a2.txt"),
        "b1.txt" => Some("b2.txt"),
        _ => None,
    }
}

fn node_rect(harness: &Harness<'static, ZenApp>, label: &str) -> egui::Rect {
    harness
        .query_all_by_label(label)
        .next()
        .map(|node| node.rect())
        .unwrap_or_else(|| panic!("no painted row {label:?}"))
}

/// A real press-and-release at a position — the path a click on paint-only decoration takes, given
/// that such a row has no accessible node to click through.
fn click_at(harness: &mut Harness<'static, ZenApp>, pos: egui::Pos2) {
    harness.hover_at(pos);
    harness.run();
    harness.drag_at(pos);
    harness.run();
    harness.drop_at(pos);
    settle(harness);
}

/// Every accessible label in the tree, sorted — the pinning aid for "which chips survive the
/// collapse", and the payload of the failures when the chip set changes.
fn all_labels(harness: &Harness<'static, ZenApp>) -> Vec<String> {
    // The predicate is the only place the label is readable (`query_all_by` hands the closure an
    // `AccessKitNode` and yields action-ready nodes); answering `false` collects nothing.
    let labels = std::cell::RefCell::new(Vec::new());
    harness
        .query_all_by(|node: &AccessKitNode<'_>| {
            if let Some(label) = node.label() {
                labels.borrow_mut().push(label);
            }
            false
        })
        // The predicate is the collection; the iterator itself is the point, so drain it.
        .count();
    let mut labels = labels.into_inner();
    labels.sort();
    labels
}

#[test]
fn the_breadcrumb_trail_is_chips_labelled_like_the_reference() {
    // Plan §9.2 step-8c review 1: the address strip is the reference's chips — one per segment,
    // the volume root named `Root` (`navigation.js:190`), `/` only ever painted as a separator,
    // never a widget. The tab is opened one level *into* the fixture listing so the trail has an
    // ancestor, the current segment and the root all at once.
    let tree = common::listing_tree("crumbs");
    let harness = pane_harness_at("crumbs", &tree.join("docs"), egui::vec2(1200.0, 800.0));
    if std::env::var("NOCAPTURE").is_ok_and(|v| v == "1") {
        println!("crumb labels: {:?}", all_labels(&harness));
    }
    assert_eq!(
        harness.query_all_by_label("Root").count(),
        1,
        "exactly one store-root chip, named the way the reference names it"
    );
    assert_eq!(
        harness.query_all_by_label("/").count(),
        0,
        "`/` is never a chip"
    );
    assert_eq!(
        harness.query_all_by_label_contains("/").count(),
        0,
        "and no widget anywhere carries a `/` — the separators are paint-only"
    );
    assert_eq!(
        harness.query_all_by_label("listing").count(),
        1,
        "one chip for the ancestor segment"
    );
    // `docs` is the *current* chip, and the tab strip names the tab after the same segment: two
    // nodes, one per surface — the same accounting `a_second_tab_…` makes for `listing`.
    assert_eq!(
        harness.query_all_by_label("docs").count(),
        2,
        "the current chip plus the tab strip's title for the same directory"
    );
}

#[test]
fn clicking_an_ancestor_chip_navigates_that_pane() {
    // The chips are the reference's `data-path` segments: clicking one lists that path *in the
    // pane that drew it*, through the pane's own `Navigate` command — which is history, so the
    // trail deepened on the way in can be walked back out.
    let mut harness = pane_harness_at(
        "crumbnav",
        &common::listing_tree("crumbnav").join("docs"),
        egui::vec2(1200.0, 800.0),
    );
    assert_eq!(
        shown(&harness).path,
        fixture_path(&common::listing_tree("crumbnav").join("docs"))
    );
    assert!(
        !shown(&harness).can_go_back(),
        "a freshly opened tab has no history yet"
    );
    harness.get_by_label("listing").click();
    settle(&mut harness);
    let tab = shown(&harness);
    assert_eq!(
        tab.path,
        fixture_path(&common::listing_tree("crumbnav")),
        "the ancestor chip listed its own segment"
    );
    assert!(tab.can_go_back(), "and the deepened trail is walkable back");
}

// ---------------------------------------------------------------------- the address *field*

/// Click the pencil at the end of the address strip, which opens the field holding the path the
/// strip stands for. Two nodes carry the tooltip's words — the button, and the free space that
/// opens the same field — so the button is picked by role (the rule `a_trail_too_narrow_…` uses).
fn open_address_field(harness: &mut Harness<'static, ZenApp>) {
    {
        let pencil = harness
            .query_all_by(|node: &AccessKitNode<'_>| {
                node.label().as_deref() == Some("Edit the path")
                    && node.role() == egui::Role::Button
            })
            .next()
            .expect("the address-field button is in the tree");
        pencil.click();
    }
    settle(harness);
    assert!(
        editing(harness),
        "the strip is the field now, and it opened focused"
    );
}

/// One real paste of a whole string — what the clipboard chord delivers (`Event::Paste`,
/// `egui::widgets::text_edit::builder`), the way a user moves a path out of a terminal.
fn paste_text(harness: &mut Harness<'static, ZenApp>, text: &str) {
    harness
        .input_mut()
        .events
        .push(egui::Event::Paste(text.to_string()));
    harness.run();
}

/// Real keystrokes, one frame apart; the first one replaces the path the field opened selected.
fn type_text(harness: &mut Harness<'static, ZenApp>, text: &str) {
    for c in text.chars() {
        harness
            .input_mut()
            .events
            .push(egui::Event::Text(c.to_string()));
        harness.run();
    }
}

/// <kbd>Enter</kbd>, then the frames the committed command needs to reach the engine and come back.
fn press_enter(harness: &mut Harness<'static, ZenApp>) {
    harness.input_mut().events.push(egui::Event::Key {
        modifiers: Default::default(),
        physical_key: None,
        key: egui::Key::Enter,
        pressed: true,
        repeat: false,
    });
    harness.run();
    settle(harness);
}

/// The active pane's address-field flag and buffer, which the pane owns (`state::Pane`).
fn editing(harness: &Harness<'static, ZenApp>) -> bool {
    harness.state().model.panes[&PaneSlot::Left].address_editing
}

fn address_buffer(harness: &Harness<'static, ZenApp>) -> String {
    harness.state().model.panes[&PaneSlot::Left]
        .address_buffer
        .clone()
}

#[test]
fn pasting_an_absolute_path_into_the_address_field_lists_it() {
    // The reported defect: with the pane on `…/listing/docs`, clicking the strip, pasting the
    // absolute path of `…/listing/reports` and pressing <kbd>Enter</kbd> left the pane where it
    // was — while clicking the `listing` chip beside it worked fine. The two decisive keys were
    // read *after* `ui.add`: egui's single-line `TextEdit` answers <kbd>Enter</kbd> by surrendering
    // its own focus, so `has_focus()` was already false on the frame the key arrived, the commit
    // fell through to the click-away branch, and the pasted path was cleared unlisted.
    let tree = common::listing_tree("addr-abs");
    let mut harness = pane_harness_at("addr-abs", &tree.join("docs"), egui::vec2(1200.0, 800.0));
    open_address_field(&mut harness);
    let target = tree.join("reports");
    paste_text(&mut harness, &target.to_string_lossy());
    assert_eq!(
        address_buffer(&harness),
        target.to_string_lossy(),
        "the paste replaced the path the field opened selected, as it does in any bar"
    );
    press_enter(&mut harness);
    let tab = shown(&harness);
    assert_eq!(
        tab.path,
        fixture_path(&target),
        "Enter listed the pasted path"
    );
    assert!(
        names(tab).contains(&"empty".to_string()),
        "and it listed the directory that was named, not the one the pane was on: {:?}",
        names(tab)
    );
    assert!(!editing(&harness), "the strip is chips again");
    assert!(
        tab.can_go_back(),
        "the walk is history, so it is walkable back"
    );
}

#[test]
fn typing_a_relative_path_in_the_address_field_lists_it_under_the_pane() {
    // Relative input means "under here" (`resolve_address` hands the tab's own path to
    // `FsPath::resolve`), which is the other half of what the field is for.
    let tree = common::listing_tree("addr-rel");
    let mut harness = pane_harness("addr-rel");
    open_address_field(&mut harness);
    type_text(&mut harness, "docs");
    press_enter(&mut harness);
    assert_eq!(
        shown(&harness).path,
        fixture_path(&tree.join("docs")),
        "`docs` was resolved against the pane's directory"
    );
}

#[test]
fn enter_in_the_address_field_re_lists_the_path_it_opened_with() {
    // The other half of "nothing happens": the field opens *holding* the current path, so Enter
    // with nothing changed is a re-list of the same directory — and it must still commit, the way
    // entering an unchanged value into Go's bar did. The strip closes either way; it is never left
    // holding a path the pane is not showing.
    let tree = common::listing_tree("addr-same");
    let mut harness = pane_harness_at("addr-same", &tree.join("docs"), egui::vec2(1200.0, 800.0));
    let before = shown(&harness).path.clone();
    open_address_field(&mut harness);
    press_enter(&mut harness);
    assert_eq!(shown(&harness).path, before, "the same path, listed again");
    assert_eq!(
        shown(&harness).total,
        3,
        "and the listing is there, not an empty pane: {:?}",
        names(shown(&harness))
    );
    assert!(!editing(&harness), "the strip is chips again");
    assert!(
        address_buffer(&harness).is_empty(),
        "`close` forgets what the field was holding"
    );
}

#[test]
fn a_trail_too_narrow_for_the_pane_collapses_and_keeps_the_edit_button() {
    // The collapse (`plan()` in `breadcrumb.rs`): root, `…`, and the deepest segments that fit.
    // The fixture path has ten filesystem segments above `listing`, so a 480-px window cannot
    // hold the whole trail; the assertions below are pinned to the chip set the layout actually
    // produces at this width (rerun with NOCAPTURE=1 --nocapture to see the full tree).
    let harness = pane_harness_at(
        "crumbnarrow",
        &common::listing_tree("crumbnarrow").join("docs"),
        egui::vec2(480.0, 480.0),
    );
    if std::env::var("NOCAPTURE").is_ok_and(|v| v == "1") {
        println!("narrow labels: {:?}", all_labels(&harness));
    }
    assert_eq!(
        harness.query_all_by_label("…").count(),
        1,
        "the trail collapsed to exactly one marker"
    );
    // What the collapse hides: the middle of the trail. What it keeps: the root, the tail the
    // user is navigating, and the button the budget was counted against.
    // The fixture's ten filesystem segments above `listing` are the middle: all folded away.
    for hidden in [
        "media",
        "jang",
        "home",
        "Deve",
        "zen-man-rs",
        "target",
        "tmp",
        "zenman-ui-fixtures",
        "zenman-ui-fixture-crumbnarrow",
    ] {
        assert_eq!(
            harness.query_all_by_label(hidden).count(),
            0,
            "the middle segment {hidden:?} must be folded into the marker"
        );
    }
    assert_eq!(
        harness.query_all_by_label("listing").count(),
        1,
        "the tail survives: `listing` is still a chip"
    );
    assert_eq!(
        harness.query_all_by_label("docs").count(),
        2,
        "and so is the current segment (chip + tab title)"
    );
    // The point of counting the button's width before the chips: the address-field affordance is
    // always inside the window, however the trail collapsed. Two nodes carry the tooltip's words —
    // the button, and the free space that opens the same field — so the *button* is picked by role.
    let edit = harness
        .query_all_by(|node: &AccessKitNode<'_>| {
            node.label().as_deref() == Some("Edit the path") && node.role() == egui::Role::Button
        })
        .next()
        .expect("the address-field button is in the tree");
    let rect = edit.rect();
    assert!(
        rect.max.x <= 480.0 && rect.min.x >= 0.0,
        "the edit button's rect {rect:?} must stay within the 480-px window"
    );
    // And the trail itself did not overflow either: everything the strip named is on screen.
    for chip in ["…", "listing", "docs"] {
        for node in harness.query_all_by_label(chip) {
            assert!(
                node.rect().max.x <= 480.0,
                "the node {chip:?} at {:?} must fit the window",
                node.rect()
            );
        }
    }
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
    // over the label, not as the single node `get_by_label` demands. With the chips (step 8c
    // review 1) the count went 2 → 3: both tabs' titles *and* the current crumb name the
    // directory — the semantics of the label are unchanged, there is simply one more surface.
    assert_eq!(
        harness.query_all_by_label("listing").count(),
        3,
        "two tab titles and the current crumb name the tab's directory"
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

/// The centre of a tab's title is where a user aims, and it is the one place the tab *used* not to
/// answer: a selectable title label takes `Sense::click_and_drag()` and is allocated on top of the
/// tab's own rect, so it swallowed the click (and painted a selection highlight) — only the four
/// edges of the tab still worked.
#[test]
fn clicking_the_middle_of_a_tab_title_shows_that_tab() {
    let mut harness = pane_harness("tab-centre");
    // A second tab over a *different* directory, so the two titles are distinguishable and the shown
    // one is the new one: the click has to travel backwards, onto the first tab's title.
    let docs = common::listing_tree("tab-centre").join("docs");
    harness
        .state_mut()
        .open_tab(PaneSlot::Left, fixture_path(&docs));
    settle(&mut harness);
    assert_eq!(
        harness.state().model.panes[&PaneSlot::Left].active,
        1,
        "the new tab is the shown one to begin with"
    );
    // The top-most node the word names is the tab's title: the address strip paints a crumb for the
    // same directory a row below the strip, so nothing else shares the label up there.
    let title = harness
        .query_all_by_label("listing")
        .map(|node| node.rect())
        .min_by(|a, b| a.min.y.total_cmp(&b.min.y))
        .expect("the first tab's title is painted on the strip");
    click_at(&mut harness, title.center());
    settle(&mut harness);
    assert_eq!(
        harness.state().model.panes[&PaneSlot::Left].active,
        0,
        "a click on the title's centre showed the tab behind it"
    );
}

// ------------------------------------------------------------------------------- activation

/// The index a row sits at in the shown tab's listing.
fn row_of(tab: &Tab, name: &str) -> usize {
    tab.entries
        .entries
        .iter()
        .position(|entry| entry.name == name)
        .unwrap_or_else(|| panic!("no row {name:?} in {:?}", names(tab)))
}

/// Activate a row the way a double-click does. `RowEvent::Activate` *is* this command —
/// [`zenman_ui::panels::file_pane::row_event`] answers `double_clicked()` with
/// [`PaneCommand::Open`], and the shell's half is what is under test here.
fn activate(harness: &mut Harness<'static, ZenApp>, name: &str) {
    let ix = row_of(shown(harness), name);
    harness
        .state_mut()
        .run_command(PaneSlot::Left, PaneCommand::Open { ix });
    settle(harness);
}

#[test]
fn a_double_click_on_a_file_hands_it_to_the_desktop() {
    let dir = common::listing_tree("open-file");
    let (mut harness, env) = pane_harness_logging("open-file");
    let before = shown(&harness).path.clone();

    activate(&mut harness, "a.txt");

    assert_eq!(
        env.opened(),
        vec![dir.join("a.txt").to_string_lossy().to_string()],
        "the row is handed to the desktop — the same route the context menu's `Open` row takes, \
         and not the toast that only *said* it was opened"
    );
    assert_eq!(
        shown(&harness).path,
        before,
        "opening a file never moves the pane"
    );
}

#[test]
fn a_double_click_on_a_directory_navigates_and_never_launches() {
    let dir = common::listing_tree("open-dir");
    let (mut harness, env) = pane_harness_logging("open-dir");

    activate(&mut harness, "docs");

    assert!(
        env.opened().is_empty(),
        "a directory is navigation, not a launch (`ops::open`'s `OpenRoute::Navigate`)"
    );
    assert_eq!(
        shown(&harness).path,
        fixture_path(&dir.join("docs")),
        "and the pane shows it"
    );
}

#[test]
fn a_keyboard_move_below_the_view_moves_the_view_too() {
    // `ScrollState::y` is an *output*: the scroller re-derives it from egui's own scroll state every
    // frame. The cursor-keeping code used to write it and call that scrolling — which selected a row
    // nobody could see. The scroller answers a one-shot request instead, and this is the test that
    // the request is what the code now sends.
    let mut harness = pane_harness("move-jump");
    let ctx = harness.ctx.clone();
    assert_eq!(
        shown(&harness).scroll.y,
        0.0,
        "the listing starts at the top"
    );
    harness
        .state_mut()
        .dispatch(PaneSlot::Left, Command::MoveBottom, &ctx);
    settle(&mut harness);
    let tab = shown(&harness);
    assert_eq!(tab.sel.len(), 1, "the bottom row is the selection");
    assert!(
        tab.scroll.y > 100.0,
        "the view followed the cursor: {} rows down",
        tab.scroll.y
    );
    let last = tab.entries.entries.iter().position(|entry| {
        tab.sel
            .keys()
            .iter()
            .any(|key| key == &entry.name || key == &entry.rel_path)
    });
    let Some(last) = last else {
        panic!(
            "the selected row is not in the listing: {:?}",
            tab.sel.keys()
        );
    };
    assert!(
        (tab.scroll.first..tab.scroll.last).contains(&last),
        "and the row it selected is on screen (row {last}, painted {}..{})",
        tab.scroll.first,
        tab.scroll.last
    );
}
