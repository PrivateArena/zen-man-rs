//! The tab groups of `docs/tab-groups-design.md` §1, driven end to end through the real shell: a
//! live `Session`, a real directory, real `Event::PointerButton`s and real `Event::PointerMoved`s.
//!
//! What this file owns (the UI half of §10's steps 4 through 9):
//!
//! * the **chip** — painted, clickable, folding and unfolding the run it heads (§1.1), and the fold
//!   that never hides the tab the pane is *listing* (invariant 4);
//! * the **hover flyout** of a folded chip, and the fact that picking a row leaves the run folded (§1.6);
//! * the **chip's menu** — rename, collapse, select, the nine colours, ungroup, close (§1.4) — and the
//!   two destructive verbs, which differ in exactly one thing;
//! * **drag** onto a chip and out of a run (§1.5), and the rule that the *selection* is what
//!   `New Tab Group` groups (§1.3);
//! * and **the restart** (§1.7): a named, folded group that has to come back named and folded.
//!
//! The model half — the algebra and its four invariants — is `state.rs`'s own test module, and the
//! strip's pure layout is [`items`]; this file is the "does the shell paint and route it" half. So the
//! fixtures mint a [`GroupId`] and hand it to `Pane::create_group` rather than assigning `tab.group`,
//! which would name a group that does not exist (invariant 2) and trip `debug_assert_invariants`.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui::{Event, Modifiers, PointerButton, Pos2, Vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

use zenman_core::path::{AbsPath, FsPath};
use zenman_core::session::{Session, SessionPaths};
use zenman_core::store::config::{GroupColor, LayoutMode, PaneSlot, ZenConfig};
use zenman_ui::app::ZenApp;
use zenman_ui::dialogs::{Answer, Choice, DialogKind};
use zenman_ui::keymap::KeyMap;
use zenman_ui::menus::MenuTarget;
use zenman_ui::panels::file_pane::PaneCommand;
use zenman_ui::panels::tab_bar::{StripItem, items};
use zenman_ui::state::{
    DropTarget, GroupId, GroupTarget, GroupVerb, TabGroup, TabId, TabSelKind, panes_for,
};

/// Frames to let the shell settle after a non-job action.
const SETTLE: usize = 6;

/// The window this suite drives: wide enough that four tabs and a chip fit without the strip evicting
/// one into the overflow popup.
const SHELL_WIDTH: f32 = 1200.0;
/// …and tall enough that the flyout has somewhere to land below the strip.
const SHELL_HEIGHT: f32 = 800.0;

/// How many frames the hover test parks the pointer on the chip, and why that is enough: kittest
/// advances `Context::input.time` by the harness's `step_dt` (0.25 s) per frame, so the shell's
/// `Tokens::group_hover_open` delay of 0.28 s is two frames — plus the frame in which the strip
/// reports the chip, the one in which the shell arms the hover off that report, and a spare.
const FLYOUT_FRAMES: usize = 5;

// --------------------------------------------------------------------- harness plumbing

fn fixture_path(path: &Path) -> FsPath {
    FsPath::from_abs(AbsPath::try_from(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
}

/// The shell, painted, with the accessibility check **on** — the way `tests/menus.rs` runs it.
fn build_harness(app: ZenApp, size: Vec2) -> Harness<'static, ZenApp> {
    harness_with(app, size, true)
}

/// …and the same builder with the check **off**, for the test here that opens a prompt: the fork's
/// `TextEdit` never surfaces an accessible name (its `hint_text` placeholder does not reach the a11y
/// node once the field holds text), so every frame with an open prompt trips the harness's
/// unnamed-input assertion. `tests/dialogs.rs` turns it off for exactly that reason.
fn build_prompt_harness(app: ZenApp, size: Vec2) -> Harness<'static, ZenApp> {
    harness_with(app, size, false)
}

fn harness_with(app: ZenApp, size: Vec2, accessibility_check: bool) -> Harness<'static, ZenApp> {
    let mut harness = Harness::builder()
        .with_accessibility_check(accessibility_check)
        .with_size(size)
        // Same reason `tests/menus.rs` gives: the pane's commands run at the *end* of the frame, and
        // jobs plus the coalesced `DirChanged` window stretch the frame loop.
        .with_max_steps(64)
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

/// The four directories of `common::nested_tree`: four distinct tab titles, one of which (`nested`)
/// is the *parent* of two others — so a test can tell the strip's tabs from the trail's chips.
fn tab_dirs(tag: &str) -> Vec<PathBuf> {
    let nested = common::nested_tree(tag);
    vec![
        nested.clone(),
        nested.join("alpha"),
        nested.join("beta"),
        nested.join("beta").join("inner"),
    ]
}

/// A one-pane shell with four tabs. The last one is the shown one, which is what makes a fold of the
/// earlier tabs a fold of tabs the pane is *not* showing.
fn four_tab_app(tag: &str) -> (ZenApp, Vec<TabId>) {
    let session = common::session(tag);
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Single));
    for dir in tab_dirs(tag) {
        app.open_tab(PaneSlot::Left, fixture_path(&dir));
    }
    let ids: Vec<TabId> = app.model.panes[&PaneSlot::Left]
        .tabs
        .iter()
        .map(|tab| tab.id)
        .collect();
    (app, ids)
}

/// …painted, with the a11y check on.
fn four_tab_harness(tag: &str) -> (Harness<'static, ZenApp>, Vec<TabId>) {
    let (app, ids) = four_tab_app(tag);
    (build_harness(app, shell_size()), ids)
}

/// …painted for a test that is about to open a modal prompt.
fn four_tab_prompt_harness(tag: &str) -> (Harness<'static, ZenApp>, Vec<TabId>) {
    let (app, ids) = four_tab_app(tag);
    (build_prompt_harness(app, shell_size()), ids)
}

/// Wide enough that the strip never evicts a tab into the overflow popup, so every tab and every chip
/// this test points at is really on screen.
fn shell_size() -> Vec2 {
    egui::vec2(SHELL_WIDTH, SHELL_HEIGHT)
}

/// Group `members` into a named group of `color` — the way the model says to: mint the id, then let
/// the pane's algebra own the membership. `name: ""` is the *unnamed* state, whose chip says its
/// colour.
fn make_group(
    harness: &mut Harness<'static, ZenApp>,
    members: &[TabId],
    name: &str,
    color: GroupColor,
) -> GroupId {
    let gid = {
        let app = harness.state_mut();
        let gid = app.model.next_group_id();
        {
            let pane = app.model.pane_mut(PaneSlot::Left);
            pane.create_group(gid, members, color)
                .unwrap_or_else(|| panic!("{members:?} are not tabs of the fixture pane"));
            if !name.is_empty() {
                assert!(
                    pane.rename_group(gid, name),
                    "the group was pruned before it could be named"
                );
            }
        }
        gid
    };
    harness.run();
    gid
}

/// Fold or unfold a run through the chip's own command — the one `TabCommand::ToggleGroup` becomes.
fn toggle_collapse(harness: &mut Harness<'static, ZenApp>, gid: GroupId) {
    harness.state_mut().run_command(
        PaneSlot::Left,
        PaneCommand::TabGroup {
            target: GroupTarget::Group(gid),
            verb: GroupVerb::ToggleCollapse,
        },
    );
    harness.run();
}

/// The pane's group `gid`, cloned, so a test can name the field it checked.
fn group(harness: &Harness<'static, ZenApp>, slot: PaneSlot, gid: GroupId) -> TabGroup {
    let pane = &harness.state().model.panes[&slot];
    pane.group(gid)
        .cloned()
        .expect("the pane still has the group this test made")
}

// --------------------------------------------------------------------- the driving primitives

/// A press+release at one point, with the modifier keys held. The strip reads <kbd>Ctrl</kbd> off the
/// *frame's* modifier state, not off the click, so the state is set on its own frame before the press
/// and released after it — the same sequence `kittest`'s `click_modifiers` produces for a node.
fn click_at(
    harness: &mut Harness<'static, ZenApp>,
    at: Pos2,
    button: PointerButton,
    modifiers: Modifiers,
) {
    if !modifiers.is_none() {
        harness
            .input_mut()
            .events
            .push(Event::ModifiersChanged(modifiers));
        harness.run();
    }
    for pressed in [true, false] {
        harness.input_mut().events.push(Event::PointerButton {
            pos: at,
            button,
            pressed,
            modifiers,
        });
    }
    harness.run();
    harness.run();
    if !modifiers.is_none() {
        harness
            .input_mut()
            .events
            .push(Event::ModifiersChanged(Modifiers::default()));
        harness.run();
    }
}

fn is_open(harness: &Harness<'static, ZenApp>) -> bool {
    harness.state().menu.is_some()
}

/// The open menu as data: `(label, enabled, hint)` in menu order, straight out of the shell's own
/// [`ZenApp::menu_specs`] — the same call the renderer makes.
fn rows(harness: &Harness<'static, ZenApp>) -> Vec<(String, bool, Option<String>)> {
    let state = harness.state();
    let Some(open) = state.menu.clone() else {
        return Vec::new();
    };
    let ctx = state.menu_context(open.slot, open.target);
    let keys = KeyMap::default();
    state
        .menu_specs()
        .iter()
        .flatten()
        .map(|spec| {
            (
                spec.label.clone(),
                spec.enabled(&ctx),
                spec.hint(&keys).or_else(|| spec.icon.map(str::to_string)),
            )
        })
        .collect()
}

fn labels(harness: &Harness<'static, ZenApp>) -> Vec<String> {
    rows(harness)
        .into_iter()
        .map(|(label, _, _)| label)
        .collect()
}

/// A point on the `ix`th tab — a tab's title is a `.selectable(false)` [`egui::Label`], so its node is
/// queryable but not click-through, hence a *point*. The same word is a breadcrumb chip and often a
/// row name, and the strip is what tells them apart: it sits above the address strip.
fn tab_point(harness: &Harness<'static, ZenApp>, slot: PaneSlot, ix: usize) -> Pos2 {
    let title = harness.state().model.panes[&slot].tabs[ix].title.clone();
    harness
        .query_all_by_label(&title)
        .map(|node| node.rect())
        .min_by(|a, b| a.min.y.total_cmp(&b.min.y))
        .unwrap_or_else(|| panic!("{title:?} is drawn nowhere, so there is no tab to click"))
        .center()
}

/// A point on the chip whose label is `name`: the same painted-`Label` trick, and the same
/// top-most-match rule, because a group named after a directory shares its word with the trail.
fn chip_point(harness: &Harness<'static, ZenApp>, name: &str) -> Pos2 {
    harness
        .query_all_by_label(name)
        .map(|node| node.rect())
        .min_by(|a, b| a.min.y.total_cmp(&b.min.y))
        .unwrap_or_else(|| panic!("the chip {name:?} is not painted, so there is nothing to click"))
        .center()
}

/// A point on a row of the open menu, picked by position: the menu opens *at the pointer*, inside the
/// band its own subject lives in, so its row is always the lowest match of its label.
fn menu_row_point(harness: &Harness<'static, ZenApp>, label: &str) -> Pos2 {
    harness
        .query_all_by_label(label)
        .map(|node| node.rect())
        .max_by(|a, b| a.min.y.total_cmp(&b.min.y))
        .unwrap_or_else(|| panic!("{label:?} is not a row of the open menu"))
        .center()
}

/// What the strip would paint, in paint order: the pure half of `panels::tab_bar`, fed the pane's own
/// state. A fold is a statement about *this* list, not about pixels.
fn strip(harness: &Harness<'static, ZenApp>, slot: PaneSlot) -> Vec<StripItem> {
    let pane = &harness.state().model.panes[&slot];
    items(&pane.tabs, &pane.groups, pane.active)
}

/// The pane's tab ids, in strip order.
fn tab_ids(harness: &Harness<'static, ZenApp>, slot: PaneSlot) -> Vec<TabId> {
    harness.state().model.panes[&slot]
        .tabs
        .iter()
        .map(|tab| tab.id)
        .collect()
}

fn shown(harness: &Harness<'static, ZenApp>, slot: PaneSlot) -> Option<TabId> {
    harness.state().model.panes[&slot].active_id()
}

/// The titles of a group's tabs, in strip order — what the flyout lists, and what a test looks for.
fn member_titles(harness: &Harness<'static, ZenApp>, slot: PaneSlot, gid: GroupId) -> Vec<String> {
    let pane = &harness.state().model.panes[&slot];
    let members = pane.member_ids(gid);
    pane.tabs
        .iter()
        .filter(|tab| members.contains(&tab.id))
        .map(|tab| tab.title.clone())
        .collect()
}

// ------------------------------------------------------------------------------- the tests

/// §1.1, the chip's own click: it is on screen, it is the run's handle, and one click is the fold.
#[test]
fn the_chip_is_painted_and_its_click_folds_the_run() {
    let (mut harness, ids) = four_tab_harness("tgp-chip");
    let gid = make_group(&mut harness, &ids[0..2], "work", GroupColor::A);

    // The chip paints its name with a `Label`, so the word is in the a11y tree — which is also the
    // only way a test, or a screen reader, can find a hand-drawn strip at all.
    assert_eq!(
        harness.query_all_by_label("work").count(),
        1,
        "the group's chip is not painted exactly once"
    );

    let chip = chip_point(&harness, "work");
    click_at(
        &mut harness,
        chip,
        PointerButton::Primary,
        Modifiers::default(),
    );
    assert!(
        group(&harness, PaneSlot::Left, gid).collapsed,
        "a click on the chip did not fold the run"
    );
    let folded = strip(&harness, PaneSlot::Left);
    assert!(
        folded.contains(&StripItem::Group(gid)),
        "the fold swallowed the chip that is its only handle: {folded:?}"
    );
    for ix in [0, 1] {
        assert!(
            !folded.contains(&StripItem::Tab(ix)),
            "a folded run still lists member tab {ix}: {folded:?}"
        );
    }
    assert!(
        folded.contains(&StripItem::Tab(2)) && folded.contains(&StripItem::Tab(3)),
        "the fold hid tabs that are in no group: {folded:?}"
    );

    // …and the same click again is the way back.
    let chip = chip_point(&harness, "work");
    click_at(
        &mut harness,
        chip,
        PointerButton::Primary,
        Modifiers::default(),
    );
    assert!(
        !group(&harness, PaneSlot::Left, gid).collapsed,
        "a second click on the chip did not unfold the run"
    );
    let unfolded = strip(&harness, PaneSlot::Left);
    for ix in [0, 1] {
        assert!(
            unfolded.contains(&StripItem::Tab(ix)),
            "the unfolded run did not give back member tab {ix}: {unfolded:?}"
        );
    }
    harness.state().model.panes[&PaneSlot::Left].debug_assert_invariants();
}

/// Invariant 4, from the chip: `Pane::activate` never edits `collapsed` and the strip never hides the
/// tab the pane is listing, so a fold cannot leave the user looking at a tab that is not on the strip.
#[test]
fn a_fold_never_hides_the_tab_the_pane_is_showing() {
    let (mut harness, ids) = four_tab_harness("tgp-fold-shown");
    let gid = make_group(&mut harness, &ids[1..3], "work", GroupColor::A);

    // Stand on a tab *inside* the run, the way a strip click does …
    harness.state_mut().run_command(
        PaneSlot::Left,
        PaneCommand::SelectTab {
            id: ids[2],
            kind: TabSelKind::Replace,
        },
    );
    harness.run();
    let before = shown(&harness, PaneSlot::Left);
    assert_eq!(
        before,
        Some(ids[2]),
        "fixture: the pane is showing a member"
    );

    // … and fold the run out from under it.
    toggle_collapse(&mut harness, gid);

    assert!(
        group(&harness, PaneSlot::Left, gid).collapsed,
        "fixture: the run is folded"
    );
    assert_eq!(
        shown(&harness, PaneSlot::Left),
        before,
        "folding a run moved the pane off the tab it was listing"
    );
    let folded = strip(&harness, PaneSlot::Left);
    assert!(
        folded.contains(&StripItem::Group(gid)) && folded.contains(&StripItem::Tab(2)),
        "the fold hid the tab the pane is showing, or lost the chip beside it: {folded:?}"
    );
    assert!(
        !folded.contains(&StripItem::Tab(1)),
        "the fold hid nothing at all, so it is not a fold: {folded:?}"
    );
    harness.state().model.panes[&PaneSlot::Left].debug_assert_invariants();
}

/// §1.6: a collapsed group still lists its tabs — on hover — and picking one works *inside* the fold.
#[test]
fn hovering_a_folded_chip_flies_out_its_tab_list() {
    let (mut harness, ids) = four_tab_harness("tgp-hover");
    let gid = make_group(&mut harness, &ids[1..3], "work", GroupColor::A);
    toggle_collapse(&mut harness, gid);
    let titles = member_titles(&harness, PaneSlot::Left, gid);
    assert_eq!(titles, ["alpha", "beta"], "fixture: the folded run's tabs");
    assert!(
        !strip(&harness, PaneSlot::Left).contains(&StripItem::Tab(1)),
        "fixture: the members are folded away, which is the whole point of the flyout"
    );
    // How many times each member's title is drawn *before* the flyout: a folded tab is off the strip,
    // but its word may be a breadcrumb chip, and only a difference proves the flyout painted it.
    let before: Vec<usize> = titles
        .iter()
        .map(|title| harness.query_all_by_label(title).count())
        .collect();

    // The strip *reports* the hovered chip as output — a pointer move is all a test can send, and a
    // second one keeps the hover alive the way the shell's keep-alive reads it.
    let at = chip_point(&harness, "work");
    for _ in 0..FLYOUT_FRAMES {
        harness.input_mut().events.push(Event::PointerMoved(at));
        harness.run();
        if harness
            .state()
            .tab_group_hover
            .is_some_and(|hover| hover.popup.is_some())
        {
            break;
        }
    }
    let flown = harness
        .state()
        .tab_group_hover
        .unwrap_or_else(|| panic!("the pointer sat on the chip and nothing was armed"));
    assert_eq!(flown.id, gid, "the hover is about another group");
    assert_eq!(
        flown.slot,
        PaneSlot::Left,
        "the hover belongs to the wrong pane"
    );
    assert!(
        flown
            .chip
            .expand(harness.state().tokens.group_hover_bridge)
            .contains(at),
        "the armed hover recorded a chip rect the pointer is not on: {:?}",
        flown.chip
    );
    assert!(
        flown.popup.is_some(),
        "the flyout never opened after the open delay, so the fold has no way in"
    );

    for (ix, title) in titles.iter().enumerate() {
        let drawn: Vec<_> = harness
            .query_all_by_label(title)
            .map(|node| node.rect())
            .collect();
        let was = before[ix];
        let now = drawn.len();
        assert!(
            now > was,
            "the flyout does not list the folded member {title:?}: {was} row(s) before the hover, {now} after"
        );
        // …and the new row is *below* the chip: the flyout hangs from its left-bottom corner, so a
        // row drawn in the strip's own band would be the tab, not the list.
        assert!(
            drawn.iter().any(|rect| rect.min.y > flown.chip.max.y),
            "{title:?} is drawn in the strip's band, not flown out under the chip: {drawn:?}"
        );
    }

    // Picking a row shows that tab and leaves the group folded — the flyout is how you work *inside*
    // a fold, and invariant 4 then keeps the chosen tab painted beside the chip.
    let row = harness
        .query_all_by_label(&titles[0])
        .map(|node| node.rect())
        .max_by(|a, b| a.min.y.total_cmp(&b.min.y))
        .expect("the flyout's row for the first member")
        .center();
    click_at(
        &mut harness,
        row,
        PointerButton::Primary,
        Modifiers::default(),
    );
    assert_eq!(
        shown(&harness, PaneSlot::Left),
        Some(ids[1]),
        "the flyout row did not show its tab"
    );
    assert!(
        group(&harness, PaneSlot::Left, gid).collapsed,
        "picking a row of the flyout unfolded the run"
    );
    assert!(
        strip(&harness, PaneSlot::Left).contains(&StripItem::Tab(1)),
        "the tab the flyout switched to is not on the strip: {:?}",
        strip(&harness, PaneSlot::Left)
    );
    harness.state().model.panes[&PaneSlot::Left].debug_assert_invariants();
}

/// §1.4: the chip's menu, and the rename that goes through the shell's one prompt path.
#[test]
fn the_chips_menu_renames_the_group() {
    let (mut harness, ids) = four_tab_prompt_harness("tgp-rename");
    // Unnamed on purpose: an unnamed group's chip says its colour word, which is what the test has to
    // find by label — and what a rename has to stop saying.
    let gid = make_group(&mut harness, &ids[1..3], "", GroupColor::A);

    let at = chip_point(&harness, GroupColor::A.name());
    harness
        .state_mut()
        .run_command(PaneSlot::Left, PaneCommand::GroupContext { id: gid, at });
    harness.run();
    harness.run();
    assert!(is_open(&harness), "the chip's right-click opened nothing");
    assert_eq!(
        harness.state().menu.as_ref().map(|open| open.target),
        Some(MenuTarget::TabGroup {
            id: gid,
            collapsed: false
        }),
        "the chip's menu must name the group it was opened over"
    );
    assert_eq!(
        labels(&harness),
        [
            "Rename Group",
            "Collapse Group",
            "Select Tabs in Group",
            "Red Group",
            "Blue Group",
            "Green Group",
            "Yellow Group",
            "Purple Group",
            "Orange Group",
            "Cyan Group",
            "Pink Group",
            "Teal Group",
            "Ungroup",
            "Close Tabs in Group",
        ],
        "`group_menu`'s rows, in order, with the palette's nine colours between the verbs"
    );
    assert!(
        rows(&harness).iter().all(|(_, on, _)| *on),
        "a chip menu row is greyed although the chip named a live group: {:?}",
        rows(&harness)
    );

    // The row's own command — and its answer, the way `tests/workspaces.rs` answers a rename prompt.
    harness.state_mut().run_command(
        PaneSlot::Left,
        PaneCommand::TabGroup {
            target: GroupTarget::Group(gid),
            verb: GroupVerb::Rename,
        },
    );
    assert!(
        harness.state().dialogs.is_open(),
        "the rename row asked for no name"
    );
    harness.state_mut().answer(Answer::Text {
        kind: DialogKind::RenameTabGroup {
            slot: PaneSlot::Left,
            id: gid,
        },
        choice: Choice::Ok,
        text: "chores".into(),
    });
    harness.run();

    let named = group(&harness, PaneSlot::Left, gid);
    assert_eq!(
        named.name, "chores",
        "the prompt's answer never reached the model"
    );
    assert_eq!(
        named.label(),
        "chores",
        "the chip still labels itself with the colour it was born in"
    );
    assert_eq!(
        harness.query_all_by_label("chores").count(),
        1,
        "the renamed chip is not what the strip paints"
    );
    harness.state().model.panes[&PaneSlot::Left].debug_assert_invariants();
}

/// The two destructive verbs of §1.4, which differ in exactly one thing: whether the tabs come along.
#[test]
fn ungroup_releases_the_tabs_and_close_wipes_them() {
    // (a) `Ungroup`: the run goes, the tabs stay open.
    let (mut harness, ids) = four_tab_harness("tgp-ungroup");
    let gid = make_group(&mut harness, &ids[1..3], "work", GroupColor::A);
    harness.state_mut().run_command(
        PaneSlot::Left,
        PaneCommand::TabGroup {
            target: GroupTarget::Group(gid),
            verb: GroupVerb::Ungroup,
        },
    );
    let pane = &harness.state().model.panes[&PaneSlot::Left];
    assert_eq!(
        pane.tabs.len(),
        4,
        "ungroup closed tabs, and it is the verb that only releases them"
    );
    assert!(
        pane.groups.is_empty(),
        "ungroup left the group record behind: {:?}",
        pane.groups
    );
    assert_eq!(
        tab_ids(&harness, PaneSlot::Left),
        ids,
        "ungroup reordered the strip it released"
    );
    pane.debug_assert_invariants();

    // (b) `Close`: the run's tabs go, and the strip's other tabs are none of its business.
    let (mut harness, ids) = four_tab_harness("tgp-close");
    let gid = make_group(&mut harness, &ids[1..3], "work", GroupColor::A);
    harness.state_mut().run_command(
        PaneSlot::Left,
        PaneCommand::TabGroup {
            target: GroupTarget::Group(gid),
            verb: GroupVerb::Close,
        },
    );
    harness.run();
    let left = tab_ids(&harness, PaneSlot::Left);
    assert_eq!(
        left,
        vec![ids[0], ids[3]],
        "closing the group took the wrong tabs, or the right ones plus a neighbour"
    );
    let pane = &harness.state().model.panes[&PaneSlot::Left];
    assert!(
        pane.groups.is_empty(),
        "the group outlived its last tab (invariant 2): {:?}",
        pane.groups
    );
    assert!(
        shown(&harness, PaneSlot::Left).is_some(),
        "a pane that still has two tabs is not showing one of them"
    );
    pane.debug_assert_invariants();

    // (c) The same verb with nothing left over: the pane comes to the shell's ordinary empty state,
    // which is `tests/shell.rs`'s no-tab placeholder — not a panic, and not a stale shown tab.
    let (mut harness, ids) = four_tab_harness("tgp-close-all");
    let gid = make_group(&mut harness, &ids, "work", GroupColor::A);
    harness.state_mut().run_command(
        PaneSlot::Left,
        PaneCommand::TabGroup {
            target: GroupTarget::Group(gid),
            verb: GroupVerb::Close,
        },
    );
    harness.run();
    let pane = &harness.state().model.panes[&PaneSlot::Left];
    assert!(
        pane.tabs.is_empty(),
        "the group's last tabs survived their group"
    );
    assert!(
        pane.groups.is_empty(),
        "an emptied pane still holds a group: {:?}",
        pane.groups
    );
    assert!(pane.active_tab().is_none(), "the empty pane shows a tab");
    assert_eq!(
        harness.query_all_by_label_contains("no tab").count(),
        1,
        "the emptied pane did not fall back to the shell's no-tab placeholder"
    );
    pane.debug_assert_invariants();
}

/// §1.5, the drag's two ends: a chip is a *join*, and empty strip is a *leave* — and either way the
/// run a group owns is one contiguous block (invariant 1).
#[test]
fn a_drag_onto_a_chip_joins_the_group_and_a_drag_out_releases_it() {
    let (mut harness, ids) = four_tab_harness("tgp-drag");
    let gid = make_group(&mut harness, &ids[1..3], "work", GroupColor::A);

    harness
        .state_mut()
        .run_command(PaneSlot::Left, PaneCommand::BeginTabDrag(ids[3]));
    assert!(
        harness.state().tab_drag.is_some(),
        "the strip's DragStart left no drag in flight"
    );
    harness.state_mut().run_command(
        PaneSlot::Left,
        PaneCommand::EndTabDrag(Some(DropTarget::OntoChip(gid))),
    );
    assert!(
        harness.state().tab_drag.is_none(),
        "the release left the drag in flight, so the next drop would move this tab too"
    );
    {
        let pane = &harness.state().model.panes[&PaneSlot::Left];
        assert_eq!(
            pane.group_of(ids[3]),
            Some(gid),
            "a drop on the chip did not join that group"
        );
        assert_eq!(
            pane.member_ids(gid),
            vec![ids[1], ids[2], ids[3]],
            "the joined tab is not at the end of the run it joined"
        );
        let run = pane.members(gid);
        assert!(
            run.windows(2).all(|pair| pair[1] == pair[0] + 1),
            "the group's members are not one contiguous run: {run:?}"
        );
        pane.debug_assert_invariants();
    }

    // …and the same tab, released past the end of every run, is out of it again.
    harness
        .state_mut()
        .run_command(PaneSlot::Left, PaneCommand::BeginTabDrag(ids[1]));
    assert!(
        harness.state().tab_drag.is_some(),
        "fixture: the second drag did not start"
    );
    harness.state_mut().run_command(
        PaneSlot::Left,
        PaneCommand::EndTabDrag(Some(DropTarget::End)),
    );
    {
        let pane = &harness.state().model.panes[&PaneSlot::Left];
        assert_eq!(
            pane.group_of(ids[1]),
            None,
            "a release outside every run left the tab in a group"
        );
        assert_eq!(
            pane.member_ids(gid),
            vec![ids[2], ids[3]],
            "the run kept the tab that left it"
        );
        assert_eq!(pane.tabs.len(), 4, "leaving a group is not closing a tab");
        let run = pane.members(gid);
        assert!(
            run.windows(2).all(|pair| pair[1] == pair[0] + 1),
            "the release broke the run it left behind: {run:?}"
        );
        pane.debug_assert_invariants();
    }
    settle(&mut harness);
    assert_eq!(
        harness.query_all_by_label("work").count(),
        1,
        "the chip is not painted after the drags that changed its run"
    );
}

/// …and the other half of the same rule: a right-click **outside** the selection is about one tab, so
/// the row that groups the selection is greyed and the colour rows do not quietly re-group three tabs
/// (`docs/tab-groups-design.md` §6's ordering rule — why the fold happens when the request is recorded
/// and not when the rows are built).
#[test]
fn a_right_click_outside_the_selection_acts_on_one_tab_only() {
    let (mut harness, ids) = four_tab_harness("tabs-groups-stale");
    let plain = Modifiers::default();
    // Two tabs selected …
    let first = tab_point(&harness, PaneSlot::Left, 0);
    click_at(&mut harness, first, PointerButton::Primary, plain);
    let second = tab_point(&harness, PaneSlot::Left, 1);
    click_at(
        &mut harness,
        second,
        PointerButton::Primary,
        Modifiers {
            ctrl: true,
            ..Modifiers::default()
        },
    );
    assert_eq!(
        harness.state().model.panes[&PaneSlot::Left].selected_tabs(),
        vec![ids[0], ids[1]],
        "fixture: two tabs are selected"
    );

    // …then a right-click on a third tab, which is not one of them.
    let third = tab_point(&harness, PaneSlot::Left, 2);
    click_at(&mut harness, third, PointerButton::Secondary, plain);
    assert!(is_open(&harness), "fixture: the menu is open");
    let open = harness.state().menu.clone().expect("fixture: a menu");
    let ctx = harness.state().menu_context(PaneSlot::Left, open.target);
    assert!(
        !ctx.multi_tab,
        "the menu read a two-tab selection off a right-click on a tab outside it: {ctx:?}"
    );
    let menu = rows(&harness);
    assert!(
        menu.iter()
            .any(|(label, on, _)| label == "New Tab Group" && !*on),
        "`New Tab Group` is lit over a single right-clicked tab: {menu:?}"
    );

    // The colour rows are about that one tab, and that is all they change.
    let row = menu_row_point(&harness, "Red Group");
    click_at(&mut harness, row, PointerButton::Primary, plain);
    settle(&mut harness);
    let pane = &harness.state().model.panes[&PaneSlot::Left];
    let grouped: Vec<TabId> = pane
        .tabs
        .iter()
        .filter(|tab| tab.group.is_some())
        .map(|tab| tab.id)
        .collect();
    assert_eq!(
        grouped,
        vec![ids[2]],
        "the stale selection was grouped instead of the tab that was actually clicked"
    );
    pane.debug_assert_invariants();
}

/// §1.3: the verb groups *the selection* — not the tab under the cursor, and not the whole strip.
#[test]
fn the_selection_is_what_new_tab_group_groups() {
    let (mut harness, ids) = four_tab_harness("tgp-selection");
    let ctrl = Modifiers {
        ctrl: true,
        ..Default::default()
    };
    // <kbd>Ctrl</kbd>-click two tabs: the strip's Toggle rule, off the events a mouse produces.
    let first = tab_point(&harness, PaneSlot::Left, 1);
    let second = tab_point(&harness, PaneSlot::Left, 2);
    click_at(&mut harness, first, PointerButton::Primary, ctrl);
    click_at(&mut harness, second, PointerButton::Primary, ctrl);
    assert_eq!(
        harness.state().model.panes[&PaneSlot::Left].selected_tabs(),
        vec![ids[1], ids[2]],
        "two ctrl-clicks did not build a two-tab selection"
    );

    // Right-click one of them: the menu has to speak about the whole selection, not the one tab.
    let second = tab_point(&harness, PaneSlot::Left, 2);
    click_at(
        &mut harness,
        second,
        PointerButton::Secondary,
        Modifiers::default(),
    );
    assert!(is_open(&harness), "the tab right-click opened nothing");
    let open = harness.state().menu.clone().expect("fixture: a menu");
    let ctx = harness.state().menu_context(PaneSlot::Left, open.target);
    assert!(
        ctx.multi_tab,
        "the tab menu read a one-tab selection off a two-tab one: {ctx:?}"
    );
    let menu = rows(&harness);
    assert!(
        menu.iter()
            .any(|(label, on, _)| label == "New Tab Group" && *on),
        "`New Tab Group` is greyed over a two-tab selection: {menu:?}"
    );

    let row = menu_row_point(&harness, "New Tab Group");
    click_at(
        &mut harness,
        row,
        PointerButton::Primary,
        Modifiers::default(),
    );
    harness.run();

    let pane = &harness.state().model.panes[&PaneSlot::Left];
    let grouped: Vec<TabId> = pane
        .tabs
        .iter()
        .filter(|tab| tab.group.is_some())
        .map(|tab| tab.id)
        .collect();
    assert_eq!(
        grouped,
        vec![ids[1], ids[2]],
        "the group is not exactly the selection the menu was opened over"
    );
    assert_eq!(
        pane.groups.len(),
        1,
        "one selection is one group, not one per tab: {:?}",
        pane.groups
    );
    let gid = pane.group_of(ids[1]).expect("the selection got a group");
    assert_eq!(
        pane.group_of(ids[2]),
        Some(gid),
        "the two selected tabs are in different groups"
    );
    assert_eq!(
        pane.tab_color(Some(gid)),
        Some(GroupColor::A),
        "`New Tab Group` takes the first colour the pane does not already hold"
    );
    assert!(
        pane.group(gid).expect("the new group").is_unnamed(),
        "a group started from the menu is unnamed, so its chip says its colour"
    );
    pane.debug_assert_invariants();
}

// ------------------------------------------------------------- the restart (§1.7, the headline)
//
// The session trip needs no harness: `shutdown` is the binary's own exit path and
// `restore_last_session` is a second shell over the same `config.json` — which is what "my tabs came
// back" means. These are `tests/workspaces.rs`'s restart idioms, applied to a folded group.

/// The session directory `common::session(tag)` is about to use.
fn session_tree(tag: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("zenman-ui-fixtures")
        .join(format!("zenman-ui-test-{tag}"))
}

/// A shell over an empty data directory: what the first launch on this folder sees.
fn fresh_shell(tag: &str) -> ZenApp {
    common::remove_tree(&session_tree(tag));
    ZenApp::new(common::session(tag), Vec::new())
}

/// A session opened the way the *binary* opens one: the settings file is read first and handed to
/// `Session::open`, which does not read it itself — `common::session` starts from the defaults, which
/// is right for every other suite and wrong for a restart, where the whole point is what came off
/// the disk.
fn reopened_session(tag: &str) -> Session {
    let dir = session_tree(tag);
    let config_path = dir.join("config.json");
    let config = ZenConfig::load(&config_path).unwrap_or_else(|e| {
        panic!(
            "{} must hold the session the first shell wrote: {e}",
            config_path.display()
        )
    });
    let paths = SessionPaths::new(config_path, dir.join("action.log"), None, dir.join("probe"))
        .with_db_open(1, Duration::from_millis(1));
    let (session, _notes) = Session::open(
        paths,
        config,
        std::sync::Arc::new(common::NoDesktopEnv::new())
            as std::sync::Arc<dyn zenman_core::platform::Env>,
        ZenConfig::default().limits(),
    )
    .expect("a session opens over a written directory");
    session
}

/// A shell over the *same* `config.json`, built after the first one wrote it: the restart.
fn reopened_shell(tag: &str) -> ZenApp {
    ZenApp::new(reopened_session(tag), Vec::new())
}

/// Exit the way the binary does: note the state on the frame tick, then flush what the debounce still
/// holds.
fn shutdown(app: &mut ZenApp) {
    app.tick_workspaces(Instant::now(), true);
    app.flush_session(Instant::now())
        .expect("the session file is writable");
}

/// The tab strip of one pane, as stored paths: the thing a user recognises as "my tabs".
fn stored_strip(app: &ZenApp, slot: PaneSlot) -> Vec<String> {
    app.model.panes[&slot]
        .tabs
        .iter()
        .map(|tab| tab.path.to_string())
        .collect()
}

/// §1.7, the feature's headline: the fold is *state*, so it has to survive the file.
#[test]
fn a_folded_group_survives_a_restart() {
    let dirs = tab_dirs("tgp-restart");
    let mut app = fresh_shell("tgp-restart");
    app.set_layout(panes_for(LayoutMode::Single));
    for dir in &dirs {
        app.open_tab(PaneSlot::Left, fixture_path(dir));
    }
    let ids: Vec<TabId> = app.model.panes[&PaneSlot::Left]
        .tabs
        .iter()
        .map(|tab| tab.id)
        .collect();
    // Named, green and folded — every attribute the chip paints — and the pane is standing on a tab
    // *inside* the run, so a restore that lost the fold or the shown tab cannot pass by accident.
    let gid = {
        let gid = app.model.next_group_id();
        {
            let pane = app.model.pane_mut(PaneSlot::Left);
            assert!(
                pane.create_group(gid, &ids[1..3], GroupColor::C).is_some()
                    && pane.rename_group(gid, "in progress")
                    && pane.set_collapsed(gid, true),
                "the fixture group does not exist"
            );
            assert!(pane.activate(ids[2]), "the fixture tab does not exist");
            pane.debug_assert_invariants();
        }
        gid
    };
    let shown_path = app.model.panes[&PaneSlot::Left].tabs[2].path.clone();
    shutdown(&mut app);

    let mut back = reopened_shell("tgp-restart");
    let report = back.restore_last_session();
    assert!(report.restored(), "{report:?}");
    assert_eq!(
        stored_strip(&back, PaneSlot::Left),
        dirs.iter()
            .map(|dir| dir.display().to_string())
            .collect::<Vec<String>>(),
        "the strip that came back is not the strip that was saved: {}",
        report.boot_line()
    );

    let pane = &back.model.panes[&PaneSlot::Left];
    // (a) the record itself, off the `groups` rows and not off the tabs' letters.
    let restored = pane.tabs[1]
        .group
        .expect("a tab saved inside a group came back with none");
    assert_eq!(
        restored, gid,
        "a restored group was re-minted, so every start dirties the session file it just read"
    );
    let group = pane
        .group(restored)
        .expect("the tab names a group the pane does not have (invariant 2)")
        .clone();
    assert_eq!(
        group.name, "in progress",
        "a renamed group that comes back unnamed is a lost rename"
    );
    assert_eq!(group.color, GroupColor::C, "another colour came back");
    assert!(
        group.collapsed,
        "the folded group came back unfolded, so the strip grew tabs the user had hidden: {group:?}"
    );
    assert_eq!(
        group.label(),
        "in progress",
        "the chip of the restored group does not say its own name"
    );
    // (b) the membership — and only the membership.
    assert_eq!(
        pane.member_ids(restored),
        vec![pane.tabs[1].id, pane.tabs[2].id],
        "the group's members are not the two tabs that were saved in it"
    );
    assert_eq!(
        pane.tabs[0].group, None,
        "the restart grouped a tab that was never in one"
    );
    assert_eq!(
        pane.tabs[3].group, None,
        "and this one too — grouping everything that exists is not a restore"
    );
    assert_eq!(
        pane.groups.len(),
        1,
        "the restart invented a group: {:?}",
        pane.groups
    );
    // (c) the pane came up on the tab it was on, not on the run's first member.
    assert_eq!(
        pane.active_id(),
        Some(pane.tabs[2].id),
        "the restored pane is showing another tab"
    );
    assert_eq!(
        pane.active_tab().expect("the restored tab").path,
        shown_path,
        "the restored pane is showing another directory"
    );
    // (d) and the strip that paints it: the chip, plus the one member it may not hide.
    let shown_items = items(&pane.tabs, &pane.groups, pane.active);
    assert!(
        shown_items.contains(&StripItem::Group(restored))
            && shown_items.contains(&StripItem::Tab(2))
            && !shown_items.contains(&StripItem::Tab(1)),
        "the restored fold is not the fold that was saved: {shown_items:?}"
    );
    pane.debug_assert_invariants();
}
