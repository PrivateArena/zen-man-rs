//! The recent-tab switcher (<kbd>Ctrl+Tab</kbd>) and the recent-group switcher (<kbd>Alt+Z</kbd>)
//! of `docs/tab-switcher-design.md`, driven end to end through the real shell: a live `Session`,
//! a real directory, real [`Event::Key`]s with a real [`Event::ModifiersChanged`] strip, and the
//! panel's own arithmetic rects for the pointer (§9's "row rects come from the widget's own
//! `harness.state().switcher` geometry, not from the a11y tree").
//!
//! What this file owns (the §10 step-6 behavioural half):
//!
//! * the **open** — a chord press paints the panel and switches nothing (§9.1), and a *tap* is
//!   the open + commit pair (§9.2);
//! * the **walk** — deeper and wrapping (§9.3), backwards (§9.4), immune to the auto-repeat
//!   rate (§9.7);
//! * the **commit** — releasing *either* half of the chord lands the highlighted row, <kbd>Ctrl</kbd>
//!   alone (§9.5) with <kbd>Tab</kbd> still down, <kbd>Shift</kbd> never required (§9.4's tail), a
//!   clicked row at once (§9.8's second half);
//! * the **cancel** — <kbd>Escape</kbd> (§9.6) and a click on the wash (§9.9) leave the shown tab
//!   exactly where it was;
//! * the **ring** — a tab touched by hand joins the list in the user's own order (§9.10), and a
//!   pane with one tab answers nothing (§9.11);
//! * the **same panel over groups** — recency, and a commit that lands on a member (§9.12);
//! * and the **neighbours** — the fold flyout steps aside (§9.13), <kbd>Ctrl+PageUp/Down</kbd>
//!   keep their positional walk (§9.14), and a row that dies under the panel dies cleanly (§9.15).
//!
//! The input mechanic the whole file rides on: `Chord::event`'s shape — an [`Event::Key`] whose
//! *own* `modifiers` field the keymap reads, while [`egui::InputState::modifiers`] (what
//! [`Hold::held`](zenman_ui::panels::switcher::Hold) reads through `Switcher::released`) is fed
//! only by `Event::ModifiersChanged`. A press therefore arrives with the chord's modifiers on the
//! event *and* a matching `ModifiersChanged` frame, exactly as egui-winit delivers a real
//! <kbd>Ctrl</kbd> press; a test that wants to prove the **modifier's** release is what commits
//! sends the `ModifiersChanged` alone and leaves the key down.

mod common;

use std::path::Path;

use egui::{Event, Key, Modifiers, PointerButton, Pos2, Vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

use zenman_core::path::{AbsPath, FsPath};
use zenman_core::store::config::{GroupColor, LayoutMode, PaneSlot};
use zenman_ui::app::ZenApp;
use zenman_ui::panels::file_pane::PaneCommand;
use zenman_ui::panels::switcher::{SwitchKind, SwitchTarget, Switcher};
use zenman_ui::state::{GroupId, GroupTarget, GroupVerb, TabId, panes_for};

/// Frames to let the shell settle after a non-job action — `tests/tab_groups.rs`'s number.
const SETTLE: usize = 6;

/// The window this suite drives: wide enough that four tabs fit the strip without eviction, and
/// 1200×800 like the design's own layout tests, so the centred panel has the geometry §4.1 says.
const SHELL_WIDTH: f32 = 1200.0;
/// …and tall enough that the fold flyout (§9.13) has somewhere to land below the strip.
const SHELL_HEIGHT: f32 = 800.0;

/// How many frames the hover test parks the pointer on the chip: kittest advances time by
/// `step_dt` (0.25 s) per frame, so the `group_hover_open` delay of 0.28 s is two frames — the
/// same arithmetic as `tests/tab_groups.rs`.
const FLYOUT_FRAMES: usize = 5;

// --------------------------------------------------------------------- harness plumbing

fn fixture_path(path: &Path) -> FsPath {
    FsPath::from_abs(AbsPath::try_from(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
}

/// The shell, painted, with the accessibility check **on**. The panel's backdrop and rows are
/// hand-allocated rects with `Sense::click`, not input widgets, and the fork's check only names
/// unnamed *inputs* — so nothing here has to turn it off.
fn build_harness(app: ZenApp, size: Vec2) -> Harness<'static, ZenApp> {
    let mut harness = Harness::builder()
        .with_accessibility_check(true)
        .with_size(size)
        // Same reason `tests/tab_groups.rs` gives: the pane's commands run at the *end* of the
        // frame, and jobs plus the coalesced `DirChanged` window stretch the frame loop.
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

/// The four directories of `common::nested_tree` (the `tests/tab_groups.rs` fixture): four
/// distinct tab titles — `nested`, `alpha`, `beta`, `inner`.
fn tab_dirs(tag: &str) -> Vec<std::path::PathBuf> {
    let nested = common::nested_tree(tag);
    vec![
        nested.clone(),
        nested.join("alpha"),
        nested.join("beta"),
        nested.join("beta").join("inner"),
    ]
}

/// A one-pane shell with the first `n` of `tab_dirs` opened as tabs, in that order — so the
/// *last* one is the shown one and the recency ring is the reverse of the strip.
fn tab_app(tag: &str, n: usize) -> (ZenApp, Vec<TabId>) {
    let session = common::session(tag);
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Single));
    for dir in tab_dirs(tag).into_iter().take(n) {
        app.open_tab(PaneSlot::Left, fixture_path(&dir));
    }
    let ids: Vec<TabId> = app.model.panes[&PaneSlot::Left]
        .tabs
        .iter()
        .map(|tab| tab.id)
        .collect();
    (app, ids)
}

/// …painted.
fn tab_harness(tag: &str, n: usize) -> (Harness<'static, ZenApp>, Vec<TabId>) {
    let (app, ids) = tab_app(tag, n);
    (build_harness(app, shell_size()), ids)
}

fn shell_size() -> Vec2 {
    egui::vec2(SHELL_WIDTH, SHELL_HEIGHT)
}

/// Group `members` into a named group of `color` — the way `tests/tab_groups.rs` does it: mint
/// the id, then let the pane's algebra own the membership.
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

/// Fold a run through the chip's own command, the way the strip's toggle does.
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

// --------------------------------------------------------------------- the driving primitives

/// A key event in the shape `Chord::event` produces: the modifiers ride on the **event**, which
/// is what `KeyMap::lookup_events_flagged` reads through `strips`; `InputState::modifiers` — what
/// `Hold::held` reads — is fed only by `Event::ModifiersChanged`, which is what [`press`] and
/// [`release`] pair with it.
fn key_event(key: Key, pressed: bool, repeat: bool, modifiers: Modifiers) -> Event {
    Event::Key {
        key,
        physical_key: None,
        pressed,
        repeat,
        modifiers,
    }
}

/// One frame carrying these events, then let the shell settle — the suites' batch idiom:
/// everything pushed lands in the *same* `run`, so press-and-release order inside a frame is the
/// test's, not the harness's.
fn frame(harness: &mut Harness<'static, ZenApp>, events: impl IntoIterator<Item = Event>) {
    harness.input_mut().events.extend(events);
    harness.run();
}

/// Press a chord: the modifier strip on its own, then the key down under it. One frame.
fn press(harness: &mut Harness<'static, ZenApp>, key: Key, modifiers: Modifiers) {
    frame(
        harness,
        [
            Event::ModifiersChanged(modifiers),
            key_event(key, true, false, modifiers),
        ],
    );
}

/// The keyboard sending the held chord *again*: `Event::Key { repeat: true }`, and no
/// `ModifiersChanged` — a repeat is not a new modifier state (§9.7's subject). egui's
/// `begin_pass` keeps its own `repeat` honest for a key that never came up, so a held
/// <kbd>Tab</kbd> re-pressed reads as a repeat however the event arrives.
fn press_repeat(harness: &mut Harness<'static, ZenApp>, key: Key, modifiers: Modifiers) {
    frame(harness, [key_event(key, true, true, modifiers)]);
}

/// Let the chord go: the key up, then the strip empty. One frame — the quick tap of §1.3 is
/// press, then this.
fn release(harness: &mut Harness<'static, ZenApp>, key: Key, modifiers: Modifiers) {
    frame(
        harness,
        [
            key_event(key, false, false, modifiers),
            Event::ModifiersChanged(Modifiers::default()),
        ],
    );
}

/// Only the modifier strip changes — the keys keep whatever state they had. This is the gesture
/// "let go of <kbd>Ctrl</kbd> while <kbd>Tab</kbd> is still down" (§9.5) and "let go of
/// <kbd>Ctrl</kbd> while <kbd>Shift</kbd> is still down" (§9.4's tail).
fn set_modifiers(harness: &mut Harness<'static, ZenApp>, modifiers: Modifiers) {
    frame(harness, [Event::ModifiersChanged(modifiers)]);
}

/// Step an open tab panel one place: <kbd>Ctrl+Tab</kbd> forward, <kbd>Ctrl+Shift+Tab</kbd> back
/// — one more *tap* of the chord while <kbd>Ctrl</kbd> stays down.
///
/// The tap is not a shortcut, it is the mechanism: the pinned fork's `InputState::begin_pass`
/// rewrites `Event::Key { pressed: true }` to `repeat: true` for a key that never came up
/// (`egui/src/input_state/mod.rs:403-411`), so a *re-`press`* of a still-held key is exactly the
/// auto-repeat §9.7 guards against. A fresh intent therefore lifts and re-presses the key, and
/// doing both inside one frame is what keeps the panel open — `Hold::held` is asked at the
/// frame's end, when <kbd>Tab</kbd> is down again, <kbd>Ctrl</kbd> never was not.
fn step(harness: &mut Harness<'static, ZenApp>, back: bool) {
    let modifiers = if back { ctrl_shift() } else { ctrl() };
    frame(
        harness,
        [
            Event::ModifiersChanged(modifiers),
            key_event(Key::Tab, false, false, modifiers),
            key_event(Key::Tab, true, false, modifiers),
        ],
    );
}

fn ctrl() -> Modifiers {
    Modifiers {
        ctrl: true,
        ..Modifiers::default()
    }
}

fn ctrl_shift() -> Modifiers {
    Modifiers {
        ctrl: true,
        shift: true,
        ..Modifiers::default()
    }
}

fn shift() -> Modifiers {
    Modifiers {
        shift: true,
        ..Modifiers::default()
    }
}

fn alt() -> Modifiers {
    Modifiers {
        alt: true,
        ..Modifiers::default()
    }
}

/// A press+release at one point, with the modifier keys held — `tests/tab_groups.rs`'s primitive,
/// so the panel's pointer rules are driven by the same events the strip's are.
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

// --------------------------------------------------------------------- the readers

/// The open panel, cloned — the widget's own state: rows, highlight, geometry.
fn switcher(harness: &Harness<'static, ZenApp>) -> Option<Switcher> {
    harness.state().switcher.clone()
}

fn panel_open(harness: &Harness<'static, ZenApp>) -> bool {
    switcher(harness).is_some()
}

/// The highlighted row, or `None` when no panel is up.
fn selected(harness: &Harness<'static, ZenApp>) -> Option<usize> {
    switcher(harness).map(|switcher| switcher.selected)
}

/// What each row would commit, in list order — the assertion every row-order test makes.
fn row_targets(harness: &Harness<'static, ZenApp>) -> Vec<SwitchTarget> {
    switcher(harness)
        .map(|switcher| switcher.rows.iter().map(|row| row.target).collect())
        .unwrap_or_default()
}

/// The rows' first lines — what §9.12 checks against the group names.
fn row_titles(harness: &Harness<'static, ZenApp>) -> Vec<String> {
    switcher(harness)
        .map(|switcher| switcher.rows.iter().map(|row| row.title.clone()).collect())
        .unwrap_or_default()
}

fn shown(harness: &Harness<'static, ZenApp>) -> Option<TabId> {
    harness.state().model.panes[&PaneSlot::Left].active_id()
}

/// The centre of row `ix` of the open panel — the widget's own rect, the same one `hover` and
/// the click hit-test answer to (§9: geometry, not the a11y tree).
fn row_point(harness: &Harness<'static, ZenApp>, ix: usize) -> Pos2 {
    switcher(harness)
        .and_then(|switcher| switcher.row_rect(ix))
        .unwrap_or_else(|| panic!("row {ix} has no rect, so the panel is not laid out"))
        .center()
}

/// Park the pointer on a row: `Switcher::show` hit-tests it in the same frame (§4.1's whole point).
fn hover_row(harness: &mut Harness<'static, ZenApp>, ix: usize) {
    let at = row_point(harness, ix);
    frame(harness, [Event::PointerMoved(at)]);
}

/// A point on the `ix`th tab of the strip — `tests/tab_groups.rs`'s painted-`Label` probe, and
/// the same top-most-match rule (the strip sits above the trail).
fn tab_point(harness: &Harness<'static, ZenApp>, ix: usize) -> Pos2 {
    let title = harness.state().model.panes[&PaneSlot::Left].tabs[ix]
        .title
        .clone();
    harness
        .query_all_by_label(&title)
        .map(|node| node.rect())
        .min_by(|a, b| a.min.y.total_cmp(&b.min.y))
        .unwrap_or_else(|| panic!("{title:?} is drawn nowhere, so there is no tab to click"))
        .center()
}

/// A point on the chip whose label is `name`.
fn chip_point(harness: &Harness<'static, ZenApp>, name: &str) -> Pos2 {
    harness
        .query_all_by_label(name)
        .map(|node| node.rect())
        .min_by(|a, b| a.min.y.total_cmp(&b.min.y))
        .unwrap_or_else(|| panic!("the chip {name:?} is not painted, so there is nothing to click"))
        .center()
}

// ------------------------------------------------------------------------------- the tests

/// §9.1: the press paints the panel and promises — it does not switch. The negative half is the
/// point: `pane.active_id()` is still the original tab.
#[test]
fn ctrl_tab_opens_the_panel_without_touching_the_shown_tab() {
    let (mut harness, ids) = tab_harness("tsw-open", 4);
    let before = shown(&harness);
    assert_eq!(
        before,
        Some(ids[3]),
        "fixture: the last-opened tab is shown"
    );

    press(&mut harness, Key::Tab, ctrl());

    let open = switcher(&harness).expect("Ctrl+Tab opened no panel");
    assert_eq!(
        open.kind,
        SwitchKind::Tabs,
        "Ctrl+Tab lists the pane's tabs"
    );
    assert_eq!(open.slot, PaneSlot::Left, "…of the pane that owns the keys");
    assert_eq!(
        open.rows.len(),
        4,
        "the panel lists the whole ring, not Go's five (feature 7)"
    );
    assert_eq!(
        open.selected, 1,
        "the highlight starts on the tab you would go back to, not the shown one"
    );
    assert_eq!(
        row_targets(&harness),
        vec![
            SwitchTarget::Tab(ids[3]),
            SwitchTarget::Tab(ids[2]),
            SwitchTarget::Tab(ids[1]),
            SwitchTarget::Tab(ids[0]),
        ],
        "row 0 is the shown tab, then the ring"
    );
    // …and the negative: a press is a promise, not a switch.
    assert_eq!(
        shown(&harness),
        before,
        "the panel opened and switched in the same breath"
    );
}

/// §9.2, feature 3: press-and-release in one motion is the open + commit pair — a tap of
/// <kbd>Ctrl+Tab</kbd> lands on the previous tab.
#[test]
fn a_tap_of_ctrl_tab_lands_on_the_previous_tab() {
    let (mut harness, ids) = tab_harness("tsw-tap", 4);
    press(&mut harness, Key::Tab, ctrl());
    assert!(panel_open(&harness), "fixture: the tap opened the panel");
    assert_eq!(
        shown(&harness),
        Some(ids[3]),
        "fixture: still on the shown tab"
    );

    release(&mut harness, Key::Tab, ctrl());

    assert!(
        !panel_open(&harness),
        "the release left the panel up — the promise was never kept"
    );
    assert_eq!(
        shown(&harness),
        Some(ids[2]),
        "a tap must land on the previous tab (row 1 of the ring)"
    );
}

/// §9.3: another press walks the highlight deeper into the past, and wraps past the end back to
/// the shown tab — `1 → 2 → 0 → 1` for three tabs, with nothing switched on the way.
#[test]
fn another_press_walks_deeper_and_wraps() {
    let (mut harness, ids) = tab_harness("tsw-walk", 3);
    press(&mut harness, Key::Tab, ctrl());
    let mut seen = vec![selected(&harness).expect("fixture: the panel is open")];
    for _ in 0..3 {
        step(&mut harness, false);
        seen.push(selected(&harness).expect("walking must not close the panel"));
    }
    assert_eq!(
        seen,
        vec![1, 2, 0, 1],
        "three rows walk 1 → 2 → 0 → 1 (§1.2)"
    );
    assert_eq!(
        shown(&harness),
        Some(ids[2]),
        "walking the highlight switches nothing"
    );
}

/// §9.4: <kbd>Ctrl+Shift+Tab</kbd> walks the list the other way — and its tail proves the hold
/// never requires <kbd>Shift</kbd>, so the reverse walk commits when <kbd>Ctrl</kbd> alone comes
/// up (§2: requiring `Shift` there would commit the instant the user reached for the reverse).
#[test]
fn ctrl_shift_tab_walks_backwards() {
    let (mut harness, ids) = tab_harness("tsw-back", 3);
    press(&mut harness, Key::Tab, ctrl());
    assert_eq!(selected(&harness), Some(1), "fixture: opened one deep");

    step(&mut harness, true);
    assert_eq!(selected(&harness), Some(0), "back is towards the shown row");
    step(&mut harness, true);
    assert_eq!(
        selected(&harness),
        Some(2),
        "and wraps the other way, past row 0 to the oldest row"
    );

    // Let go of Ctrl with Shift and Tab still down: `Hold::TAB` demands neither, so *this* is the
    // commit — a hold that required Shift would sit open and the shown tab would not move.
    set_modifiers(&mut harness, shift());
    assert!(
        !panel_open(&harness),
        "releasing Ctrl with Shift held left the panel open — the hold demands Shift"
    );
    assert_eq!(
        shown(&harness),
        Some(ids[0]),
        "row 2 of [third, second, first] is the first tab"
    );
}

/// §9.5, feature 3: releasing **Ctrl** — the modifier, with <kbd>Tab</kbd> never lifted — commits
/// the highlighted row.
#[test]
fn releasing_ctrl_commits_the_highlighted_row() {
    let (mut harness, ids) = tab_harness("tsw-ctrl-up", 4);
    press(&mut harness, Key::Tab, ctrl());
    assert_eq!(selected(&harness), Some(1), "fixture: row 1 highlighted");

    // Only the strip moves. `keys_down` still holds Tab — if the hold waited for the *key*,
    // nothing would commit here.
    set_modifiers(&mut harness, Modifiers::default());

    assert!(
        !panel_open(&harness),
        "Ctrl came up and the panel sat there — the release watches the key, not the modifier"
    );
    assert_eq!(
        shown(&harness),
        Some(ids[2]),
        "the highlighted row was not committed on the modifier's release"
    );
}

/// §9.6, feature 5: <kbd>Escape</kbd> cancels, and cancelling never switches.
#[test]
fn escape_cancels_and_leaves_the_shown_tab_alone() {
    let (mut harness, _ids) = tab_harness("tsw-esc", 4);
    let before = shown(&harness);
    press(&mut harness, Key::Tab, ctrl());
    assert_eq!(selected(&harness), Some(1), "fixture: one deep");

    // Escape as the keymap spells it: the chord `SelectNone` is exact on the modifier set, so the
    // bare Escape event carries no modifiers (`Chord::matches`, and the stream a user gets after
    // the *first* Ctrl-up would already have committed — the panel's hold and the chord table
    // simply agree that Escape is the panel's other exit).
    frame(
        &mut harness,
        [key_event(Key::Escape, true, false, Modifiers::default())],
    );

    assert!(!panel_open(&harness), "Escape did not dismiss the panel");
    assert_eq!(
        shown(&harness),
        before,
        "Escape committed the highlighted row; cancelling leaves the shown tab alone"
    );

    // And the chord that opened it is still "down" (Tab never came up): its release afterwards
    // must not resurrect a switch either.
    release(&mut harness, Key::Tab, ctrl());
    assert_eq!(shown(&harness), before, "the cancelled chord still fired");
}

/// §9.7: holding the chord steps the highlight **once**, not at the keyboard's auto-repeat rate —
/// `lookup_events_flagged`'s flag is the whole mechanism, so this pushes a fresh press at the end
/// to prove the guard is on the repeat and not on the press.
#[test]
fn holding_ctrl_tab_does_not_run_the_list_away() {
    let (mut harness, _ids) = tab_harness("tsw-repeat", 4);
    press(&mut harness, Key::Tab, ctrl());
    let at = selected(&harness).expect("fixture: the panel is open");
    assert_eq!(
        at, 1,
        "fixture: the opening press put the highlight on row 1"
    );

    for _ in 0..4 {
        press_repeat(&mut harness, Key::Tab, ctrl());
        assert_eq!(
            selected(&harness),
            Some(at),
            "a repeat press moved the highlight — the list ran away at the repeat rate"
        );
    }

    // A *fresh* tap of the same chord still steps: the guard swallows repeats, not intents.
    step(&mut harness, false);
    assert_eq!(
        selected(&harness),
        Some(2),
        "the repeat guard swallowed the next real press too"
    );
}

/// §9.8, feature 4: moving the mouse over a row moves the highlight **in the same frame** —
/// which only works because the rects are arithmetic (§4.1), so this drives the pointer against
/// the widget's own `row_rect`, one frame per move.
#[test]
fn hovering_a_row_moves_the_highlight() {
    let (mut harness, ids) = tab_harness("tsw-hover", 4);
    press(&mut harness, Key::Tab, ctrl());
    assert_eq!(
        selected(&harness),
        Some(1),
        "fixture: the opening highlight"
    );

    hover_row(&mut harness, 3);
    assert_eq!(
        selected(&harness),
        Some(3),
        "the pointer sat on row 3 and the highlight stayed put"
    );
    assert_eq!(
        shown(&harness),
        Some(ids[3]),
        "hover is the promise, not the switch (§1.4)"
    );

    hover_row(&mut harness, 0);
    assert_eq!(
        selected(&harness),
        Some(0),
        "and it follows back to the shown row"
    );
}

/// Feature 2 and feature 4 cannot both win unless the pointer has to *move*: the panel is centred, so it
/// opens under wherever the mouse happens to rest, and a panel that re-reads the pointer every frame
/// would let a still mouse outvote every keystroke. The walk survives a hand that is not moving, and
/// hands back the moment it moves — which is exactly Go's `mouseover` listener (`keyboard.js:63-66`).
#[test]
fn a_still_pointer_loses_to_the_keyboard() {
    let (mut harness, _) = tab_harness("tsw-still", 4);
    press(&mut harness, Key::Tab, ctrl());
    hover_row(&mut harness, 2);
    assert_eq!(selected(&harness), Some(2), "fixture: the hover landed");

    // Two frames of nothing at all — the pointer sits on row 2 the whole time — then a step.
    frame(&mut harness, []);
    frame(&mut harness, []);
    step(&mut harness, false);
    assert_eq!(
        selected(&harness),
        Some(3),
        "the panel re-read a pointer that had not moved, so the walk went nowhere"
    );
    step(&mut harness, false);
    assert_eq!(selected(&harness), Some(0), "and it did not wrap either");

    // The hand moves again, into a row the chord had already walked past: the pointer is a voter,
    // not a veto, and its vote counts when it is actually cast.
    hover_row(&mut harness, 1);
    assert_eq!(
        selected(&harness),
        Some(1),
        "a moved pointer did not take its row"
    );
    // And the walk answers again over the same still mouse, now resting on row 1.
    step(&mut harness, false);
    assert_eq!(
        selected(&harness),
        Some(2),
        "the mouse on row 1 outvoted the chord that was just pressed"
    );
}

/// §9.8's second half: clicking a row commits it at once — no release of the chord needed.
#[test]
fn clicking_a_row_commits_it() {
    let (mut harness, ids) = tab_harness("tsw-click", 4);
    // Tab and Ctrl stay down the whole time: the click alone has to land the row.
    press(&mut harness, Key::Tab, ctrl());
    assert_eq!(
        shown(&harness),
        Some(ids[3]),
        "fixture: nothing switched yet"
    );

    let row = row_point(&harness, 2);
    click_at(
        &mut harness,
        row,
        PointerButton::Primary,
        Modifiers::default(),
    );

    assert!(
        !panel_open(&harness),
        "the clicked row did not close the panel"
    );
    assert_eq!(
        shown(&harness),
        Some(ids[1]),
        "row 2 of [fourth, third, second, first] is the second tab"
    );
}

/// §9.9, feature 4: a click **outside** the panel cancels — no switch, panel gone.
#[test]
fn clicking_outside_cancels() {
    let (mut harness, ids) = tab_harness("tsw-outside", 4);
    press(&mut harness, Key::Tab, ctrl());
    let before = shown(&harness);
    assert_eq!(before, Some(ids[3]), "fixture: nothing switched yet");

    // A point on the wash and off the frame — the widget's own geometry says where the frame is.
    let frame_rect = switcher(&harness)
        .expect("fixture: the panel is open")
        .frame();
    let outside = Pos2::new(50.0, 50.0);
    assert!(
        !frame_rect.contains(outside),
        "fixture: the chosen point is inside the panel: {frame_rect:?}"
    );
    click_at(
        &mut harness,
        outside,
        PointerButton::Primary,
        Modifiers::default(),
    );

    assert!(
        !panel_open(&harness),
        "a click on the backdrop left the panel up"
    );
    assert_eq!(
        shown(&harness),
        before,
        "dismissing by click switched the pane"
    );
}

/// §9.10, feature 6: touching a tab puts it at the head of the ring, and the panel is only a
/// view of that ring — the user's own example, three tabs, click the third, the order is the
/// third, the first, the second.
#[test]
fn a_tab_touched_by_hand_joins_the_list() {
    let (mut harness, ids) = tab_harness("tsw-hand", 3);
    // Open order t1, t2, t3 leaves the ring [t3, t2, t1]. Click the **first** tab by hand …
    let first = tab_point(&harness, 0);
    click_at(
        &mut harness,
        first,
        PointerButton::Primary,
        Modifiers::default(),
    );
    assert_eq!(
        shown(&harness),
        Some(ids[0]),
        "fixture: the click showed the first tab"
    );
    // … then the **third**. The ring is now [t3, t1, t2] — the touched tabs joined the head.
    let third = tab_point(&harness, 2);
    click_at(
        &mut harness,
        third,
        PointerButton::Primary,
        Modifiers::default(),
    );
    assert_eq!(
        shown(&harness),
        Some(ids[2]),
        "fixture: the click showed the third tab"
    );

    press(&mut harness, Key::Tab, ctrl());
    assert_eq!(
        row_targets(&harness),
        vec![
            SwitchTarget::Tab(ids[2]),
            SwitchTarget::Tab(ids[0]),
            SwitchTarget::Tab(ids[1]),
        ],
        "the panel lists third, first, second — the ring the hand built"
    );
}

/// §9.11, feature 9: a pane with one tab to offer answers nothing at all — no panel, no switch.
#[test]
fn one_tab_answers_nothing() {
    let (mut harness, ids) = tab_harness("tsw-alone", 1);
    press(&mut harness, Key::Tab, ctrl());
    assert!(
        !panel_open(&harness),
        "a single-tab pane opened a switcher (Go's `recentTabs.length <= 1` return)"
    );
    assert_eq!(shown(&harness), Some(ids[0]), "and it switched even so");
    release(&mut harness, Key::Tab, ctrl());
    assert_eq!(
        shown(&harness),
        Some(ids[0]),
        "the release found something to commit"
    );
}

/// §9.12, feature 8: <kbd>Alt+Z</kbd> opens the same panel over the pane's **groups**, in
/// recently-used order — the group whose tab was last shown first, even though the other was
/// created first — and committing activates that group's most recent member tab.
#[test]
fn alt_z_lists_the_groups_and_switches_to_a_member() {
    let (mut harness, ids) = tab_harness("tsw-altz", 4);
    // Creation order *against* the expected answer: `two` exists first, `one` is the recent one.
    let two = make_group(&mut harness, &ids[2..4], "two", GroupColor::B);
    let one = make_group(&mut harness, &ids[0..2], "one", GroupColor::A);
    assert_ne!(one, two, "fixture: two groups");

    // Touch a member of `one` by hand: showing a tab makes its group the recent one (§3).
    let member = tab_point(&harness, 1);
    click_at(
        &mut harness,
        member,
        PointerButton::Primary,
        Modifiers::default(),
    );
    assert_eq!(
        shown(&harness),
        Some(ids[1]),
        "fixture: showing a member of `one`"
    );

    press(&mut harness, Key::Z, alt());
    let open = switcher(&harness).expect("Alt+Z opened no group panel");
    assert_eq!(open.kind, SwitchKind::Groups, "Alt+Z lists the groups");
    assert_eq!(
        row_titles(&harness),
        ["one", "two"],
        "the group whose tab was last shown leads the list, not the one created first"
    );
    assert_eq!(
        row_targets(&harness),
        vec![SwitchTarget::Group(one), SwitchTarget::Group(two)],
        "the rows commit the groups they name"
    );
    assert_eq!(
        open.selected, 1,
        "…highlighted on the group you would go back to"
    );

    release(&mut harness, Key::Z, alt());
    assert!(!panel_open(&harness), "the chord release kept the panel up");
    assert_eq!(
        shown(&harness),
        Some(ids[3]),
        "committing `two` must land on its member furthest forward in the tab ring — t4, not t3"
    );
    harness.state().model.panes[&PaneSlot::Left].debug_assert_invariants();
}

/// §9.13, §5's mutual exclusion: the panel and the fold flyout are one kind of attention — the
/// chord that opens the panel takes the flyout's life in the same breath.
#[test]
fn the_panel_and_the_fold_flyout_do_not_fight() {
    let (mut harness, ids) = tab_harness("tsw-flyout", 4);
    let gid = make_group(&mut harness, &ids[1..3], "work", GroupColor::A);
    toggle_collapse(&mut harness, gid);

    // Arm the flyout the way `tests/tab_groups.rs` does: park the pointer on the chip.
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
    assert!(
        harness
            .state()
            .tab_group_hover
            .is_some_and(|hover| hover.popup.is_some()),
        "fixture: the fold's flyout is open"
    );

    press(&mut harness, Key::Tab, ctrl());
    assert!(panel_open(&harness), "fixture: the switcher opened");
    assert!(
        harness.state().tab_group_hover.is_none(),
        "the flyout outlived the panel that supersedes it (§5)"
    );

    // And it stays gone while the panel is up — the hover tick must not re-arm around it.
    harness.input_mut().events.push(Event::PointerMoved(at));
    harness.run();
    harness.run();
    assert!(
        panel_open(&harness) && harness.state().tab_group_hover.is_none(),
        "the flyout reopened under the open panel"
    );
}

/// §9.14, §2's re-chording: <kbd>Ctrl+PageDown/Up</kbd> still walk the strip **positionally** —
/// the switcher took the <kbd>Ctrl+Tab</kbd> chords, not the ring walk.
#[test]
fn next_tab_and_prev_tab_still_walk_the_strip() {
    let (mut harness, ids) = tab_harness("tsw-pgdn", 4);
    assert_eq!(shown(&harness), Some(ids[3]), "fixture: on the last tab");

    // Ctrl+PageDown from the *last* strip position wraps to the first — `cycle_tab`, not the ring.
    press(&mut harness, Key::PageDown, ctrl());
    assert_eq!(
        shown(&harness),
        Some(ids[0]),
        "Ctrl+PageDown did not walk the strip positionally"
    );
    assert!(
        !panel_open(&harness),
        "the re-chorded walk opened the switcher panel instead"
    );
    release(&mut harness, Key::PageDown, ctrl());

    // …and Ctrl+PageUp walks it back.
    press(&mut harness, Key::PageUp, ctrl());
    assert_eq!(
        shown(&harness),
        Some(ids[3]),
        "Ctrl+PageUp did not walk back to the last tab"
    );
    assert!(!panel_open(&harness), "and this one opened the panel too");
    release(&mut harness, Key::PageUp, ctrl());
}

/// §9.15: a row that dies between frames is *pruned*, never promised — close the highlighted tab
/// through the model while the panel is open, and the next frame neither panics nor selects a
/// dead id, and the eventual commit lands on a living one.
#[test]
fn the_panel_dies_with_its_tab() {
    let (mut harness, ids) = tab_harness("tsw-dies", 3);
    press(&mut harness, Key::Tab, ctrl());
    let open = switcher(&harness).expect("fixture: the panel is open");
    assert_eq!(open.selected, 1, "fixture: row 1 is highlighted");
    assert_eq!(
        open.target(),
        Some(SwitchTarget::Tab(ids[1])),
        "fixture: the highlighted row is the tab about to be closed"
    );

    // The user's own close, through the model, between frames (the panel is not the only writer).
    harness
        .state_mut()
        .run_command(PaneSlot::Left, PaneCommand::CloseTab(ids[1]));
    harness.run();

    let pane = &harness.state().model.panes[&PaneSlot::Left];
    let left = switcher(&harness).expect("one row died of three: the panel loses it, it survives");
    assert_eq!(
        left.rows.len(),
        2,
        "the dead row is still in the list: the panel never pruned it"
    );
    assert!(
        left.selected < left.rows.len(),
        "the highlight names a row the list no longer has: {} of {}",
        left.selected,
        left.rows.len()
    );
    for row in &left.rows {
        let SwitchTarget::Tab(id) = row.target else {
            panic!("a tab panel that started with only tabs grew a group row: {row:?}")
        };
        assert!(
            pane.tabs.iter().any(|tab| tab.id == id),
            "the panel still promises the closed tab {id:?}"
        );
    }
    let want = left.target().expect("two live rows still have a target");

    release(&mut harness, Key::Tab, ctrl());
    let SwitchTarget::Tab(id) = want else {
        panic!("a tab panel commits something else: {want:?}")
    };
    assert_eq!(
        shown(&harness),
        Some(id),
        "the commit did not land on the row that took the dead one's place"
    );
    assert_ne!(id, ids[1], "and it must not name the closed tab");
    harness.state().model.panes[&PaneSlot::Left].debug_assert_invariants();
}

/// §1.2 and §1.3 together, one event per frame: the real gesture is <kbd>Ctrl</kbd> held while
/// <kbd>Tab</kbd> is *tapped*, so the key comes up in a frame of its own. A hold that watched the key
/// would commit on that frame — one step, then a closed panel — which is exactly what Go's
/// `keyupHandler` avoids by watching `currentModifierKey` alone (`keyboard.js:126-129`). The
/// one-frame tap of [`step`] cannot catch this, so the walk is repeated here at real timing.
#[test]
fn a_tap_of_tab_survives_its_own_release() {
    let (mut harness, ids) = tab_harness("tsw-tap-frames", 4);
    press(&mut harness, Key::Tab, ctrl());
    assert_eq!(selected(&harness), Some(1), "fixture: opened on row 1");

    // The key lifts, the modifier does not: nothing may happen at all.
    frame(&mut harness, [key_event(Key::Tab, false, false, ctrl())]);
    assert!(
        panel_open(&harness),
        "the panel committed on the *key's* release, so the walk stops after one step"
    );
    assert_eq!(shown(&harness), Some(ids[3]), "and it switched the tab");
    assert_eq!(selected(&harness), Some(1), "frame-per-event must be idle");

    // The second tap, a frame later: fresh, so it steps.
    frame(&mut harness, [key_event(Key::Tab, true, false, ctrl())]);
    assert_eq!(
        selected(&harness),
        Some(2),
        "the second tap did not walk one deeper into the past"
    );
    frame(&mut harness, [key_event(Key::Tab, false, false, ctrl())]);
    frame(&mut harness, [key_event(Key::Tab, true, false, ctrl())]);
    assert_eq!(
        selected(&harness),
        Some(3),
        "and the third tap did not walk"
    );

    // Only now does the hand let go of Ctrl.
    set_modifiers(&mut harness, Modifiers::default());
    assert!(
        !panel_open(&harness),
        "the modifier came up and the panel stayed open"
    );
    assert_eq!(
        shown(&harness),
        Some(ids[0]),
        "the tap walked to row 3, so row 3 is the tab that must be shown"
    );
}

// ------------------------------------------------------- the review round's regressions
//
// Five cases the first drafting got wrong. Each one is a behaviour the suite of §9 passed over: the
// ring's head after a close, the ring's head after a *group* mutation, the highlight's identity when
// rows die, whose pane a chord belongs to, and the frame in which the panel's monopoly starts.

/// §1.1 and §6: row `0` **is** the tab on screen. `Pane::close_tabs` clamps `active` onto the
/// neighbour of the tab that died, so unless the rings follow the clamp, the panel lists the dead tab's
/// position as "recent" and highlights the tab the user is already looking at.
#[test]
fn row_zero_is_the_tab_on_screen_after_a_close() {
    let (mut harness, ids) = tab_harness("tsw-row0", 3);
    // Hand-pick the first tab: the ring becomes [A, C, B] and the shown tab is not the strip's last.
    let first = tab_point(&harness, 0);
    click_at(
        &mut harness,
        first,
        PointerButton::Primary,
        Modifiers::default(),
    );
    assert_eq!(shown(&harness), Some(ids[0]), "fixture: on the first tab");

    harness
        .state_mut()
        .run_command(PaneSlot::Left, PaneCommand::CloseTab(ids[0]));
    let after = shown(&harness).expect("two tabs are left");
    assert_ne!(after, ids[0], "the shown tab was closed");

    press(&mut harness, Key::Tab, ctrl());
    assert_eq!(
        row_targets(&harness).first(),
        Some(&SwitchTarget::Tab(after)),
        "row 0 was not the tab on screen — it named {} while {} is showing",
        fmt_target(row_targets(&harness).first().copied()),
        fmt_target(Some(SwitchTarget::Tab(after))),
    );
    assert_eq!(selected(&harness), Some(1), "fixture: one place back");
    assert_ne!(
        row_targets(&harness).get(1),
        Some(&SwitchTarget::Tab(after)),
        "the walk began on the tab already on screen, so a tap would do nothing"
    );
}

/// §3: showing a tab makes its group recent, so *making* the shown tab a group must do the same —
/// otherwise the group under the user's nose is buried behind one they walked away from, and the ring
/// head invariant `debug_assert_invariants` checks would be a lie (`create_group` is not `show`).
#[test]
fn grouping_the_shown_tab_leads_the_group_list() {
    let (mut harness, ids) = tab_harness("tsw-glead", 4);
    // `two` first, over the last two — and the shown tab (`inner`) is one of them.
    make_group(&mut harness, &ids[2..4], "two", GroupColor::B);
    // Move onto the *first* tab, ungrouped, so the next mutation is the interesting one.
    let first = tab_point(&harness, 0);
    click_at(
        &mut harness,
        first,
        PointerButton::Primary,
        Modifiers::default(),
    );
    assert_eq!(shown(&harness), Some(ids[0]), "fixture: on the first tab");

    // Group the tab on screen. No click, no `show` — just membership.
    make_group(&mut harness, &ids[0..2], "one", GroupColor::A);

    press(&mut harness, Key::Z, alt());
    assert_eq!(
        row_titles(&harness),
        ["one", "two"],
        "the group the shown tab just joined did not take the head of the list"
    );
}

/// §4.1's identity rule: the promise is a **tab**, not an index. Rows dying above the highlight must
/// not move it, and when the highlighted tab is itself the one that dies, the highlight keeps its
/// *place* — the next thing back in the past, not an arbitrary neighbour.
#[test]
fn the_highlight_follows_its_row_and_keeps_its_place() {
    let (mut harness, ids) = tab_harness("tsw-follow", 4);
    press(&mut harness, Key::Tab, ctrl());
    step(&mut harness, false);
    step(&mut harness, false);
    assert_eq!(
        switcher(&harness).and_then(|s| s.target()),
        Some(SwitchTarget::Tab(ids[0])),
        "fixture: the highlight is at the last row, on the strip's first tab"
    );

    // The tab on screen dies — the row *above* the highlight goes with it.
    harness
        .state_mut()
        .run_command(PaneSlot::Left, PaneCommand::CloseTab(ids[3]));
    // A frame, because pruning the panel is `tick_switcher`'s job, not the closer's.
    harness.run();
    let open = switcher(&harness).expect("three rows are left: the panel survives");
    assert_eq!(
        open.target(),
        Some(SwitchTarget::Tab(ids[0])),
        "the highlight slid to another tab because its index moved"
    );
    let live: Vec<_> = open
        .rows
        .iter()
        .filter_map(|row| match row.target {
            SwitchTarget::Tab(id) => Some(id),
            SwitchTarget::Group(_) => None,
        })
        .collect();
    assert!(
        live.iter().all(|id| ids.contains(id)) && !live.contains(&ids[3]),
        "the panel still lists the closed tab: {live:?}"
    );

    release(&mut harness, Key::Tab, ctrl());
    assert_eq!(
        shown(&harness),
        Some(ids[0]),
        "the promise the panel kept was not the one it landed"
    );
}

/// §5: the panel lists the pane whose keys it has. A click in another pane moves the keys, and a panel
/// that keeps stepping the old pane's rows would commit into a pane the user walked away from.
#[test]
fn a_chord_after_the_keys_move_lists_the_new_pane() {
    let (mut harness, left, right) = split_harness("tsw-panes");
    press(&mut harness, Key::Tab, ctrl());
    assert_eq!(
        switcher(&harness).map(|s| s.slot),
        Some(PaneSlot::Left),
        "fixture: the left pane owns the keys"
    );
    assert_eq!(
        row_targets(&harness).len(),
        left,
        "and lists the left pane's tabs"
    );
    release(&mut harness, Key::Tab, ctrl());
    assert!(!panel_open(&harness), "fixture: released");

    // A click far into the right pane's body: the keys move.
    click_at(
        &mut harness,
        Pos2::new(SHELL_WIDTH - 40.0, SHELL_HEIGHT - 120.0),
        PointerButton::Primary,
        Modifiers::default(),
    );
    assert_eq!(
        harness.state().model.active_pane,
        PaneSlot::Right,
        "the click did not move the keys, so the rest of the test proves nothing"
    );

    press(&mut harness, Key::Tab, ctrl());
    let open = switcher(&harness).expect("the right pane has tabs to list");
    assert_eq!(
        open.slot,
        PaneSlot::Right,
        "the chord listed the wrong pane"
    );
    assert_eq!(
        row_targets(&harness).len(),
        right,
        "the panel kept the left pane's rows"
    );
    // Row 1 of the right pane, read before the release: the tab the highlight promised.
    let promised = {
        let state = harness.state();
        state.model.panes[&PaneSlot::Right].recent_tabs()[1]
    };
    release(&mut harness, Key::Tab, ctrl());
    assert_eq!(
        harness.state().model.panes[&PaneSlot::Right].active_id(),
        Some(promised),
        "the commit did not land on the promised row of the pane that owns the keys"
    );
}

/// §5's swallow starts on the frame the panel *opens*: a batch carrying the chord and a second keypress
/// used to hand that second key to the file list, because the panel's monopoly was decided once, for
/// the whole frame, before any of it ran.
#[test]
fn the_frame_that_opens_the_panel_answers_no_other_key() {
    let (mut harness, _) = tab_harness("tsw-swallow", 4);
    // Control: <kbd>Ctrl+A</kbd> with nothing up selects the listing.
    frame(
        &mut harness,
        [
            Event::ModifiersChanged(ctrl()),
            key_event(Key::A, true, false, ctrl()),
        ],
    );
    release(&mut harness, Key::A, ctrl());
    assert!(
        selected_rows(&harness) > 0,
        "the control failed: Ctrl+A selected nothing, so the real half of the test proves nothing"
    );
    harness
        .state_mut()
        .model
        .panes
        .get_mut(&PaneSlot::Left)
        .and_then(|pane| pane.active_tab_mut())
        .unwrap_or_else(|| panic!("the fixture pane lost its tab"))
        .sel
        .clear();
    harness.run();
    assert_eq!(
        selected_rows(&harness),
        0,
        "fixture: the selection was cleared"
    );

    // Chord and select-all, one frame, in that order.
    frame(
        &mut harness,
        [
            Event::ModifiersChanged(ctrl()),
            key_event(Key::Tab, true, false, ctrl()),
            key_event(Key::A, true, false, ctrl()),
        ],
    );
    assert!(panel_open(&harness), "the chord opened no panel");
    assert_eq!(
        selected_rows(&harness),
        0,
        "the file list answered <kbd>Ctrl+A</kbd> the same frame the panel opened"
    );
}

/// A short name for the failure messages: a target, in the shell's words.
fn fmt_target(target: Option<SwitchTarget>) -> String {
    match target {
        Some(SwitchTarget::Tab(id)) => format!("tab {id:?}"),
        Some(SwitchTarget::Group(id)) => format!("group {id:?}"),
        None => "nothing".to_owned(),
    }
}

/// How many listing rows the pane on screen has selected.
fn selected_rows(harness: &Harness<'static, ZenApp>) -> usize {
    harness.state().model.panes[&PaneSlot::Left]
        .active_tab()
        .map(|tab| tab.sel.len())
        .unwrap_or(0)
}

/// A split shell: `left` tabs in the left pane, `right` tabs in the right one, so a click can move the
/// keys from one to the other.
fn split_harness(tag: &str) -> (Harness<'static, ZenApp>, usize, usize) {
    let dirs = tab_dirs(tag);
    let session = common::session(tag);
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Split));
    for dir in dirs.iter().take(2) {
        app.open_tab(PaneSlot::Left, fixture_path(dir));
    }
    for dir in dirs.iter().skip(2) {
        app.open_tab(PaneSlot::Right, fixture_path(dir));
    }
    let left = app.model.panes[&PaneSlot::Left].tabs.len();
    let right = app.model.panes[&PaneSlot::Right].tabs.len();
    let mut harness = build_harness(app, shell_size());
    settle(&mut harness);
    (harness, left, right)
}

/// §11.9: the panel floats over several panes and the shell's focus rule is a raw geometry test, so a
/// press on one of its rows used to move the keys to whatever pane lay underneath — and the commit then
/// landed in a pane the user had walked away from while the walk itself kept stepping the first one's
/// list. The press inside the panel must change nothing but the highlight.
#[test]
fn a_press_on_a_row_does_not_move_the_keys() {
    let (mut harness, _, _) = split_harness("tsw-focus");
    press(&mut harness, Key::Tab, ctrl());
    assert_eq!(
        harness.state().model.active_pane,
        PaneSlot::Left,
        "fixture: the left pane owns the keys"
    );
    let right_before = harness.state().model.panes[&PaneSlot::Right].active_id();
    let at = row_point(&harness, 1);
    harness
        .state()
        .switcher
        .as_ref()
        .map(|switcher| {
            assert!(
                switcher.frame().contains(at),
                "fixture: the row is inside the panel"
            )
        })
        .expect("fixture: the panel is open");

    click_at(
        &mut harness,
        at,
        PointerButton::Primary,
        Modifiers::default(),
    );

    assert_eq!(
        harness.state().model.active_pane,
        PaneSlot::Left,
        "a click inside the floating panel moved the keys to the pane underneath it"
    );
    assert_eq!(
        harness.state().model.panes[&PaneSlot::Right].active_id(),
        right_before,
        "and the click switched a tab in the pane that lost the keys"
    );
}

/// §11.11: the rows butt-join, so a point on a shared edge lies inside two rects. The pointer resolves
/// it the same way whether it is *asking* (hover) or *answering* (click) — the row below, which is what
/// egui's own last-allocated-overlapping-rect rule gives the click.
#[test]
fn a_shared_row_edge_belongs_to_the_row_below() {
    let (mut harness, ids) = tab_harness("tsw-edge", 4);
    press(&mut harness, Key::Tab, ctrl());
    let edge = {
        let open = switcher(&harness).expect("fixture: the panel is open");
        let upper = open.row_rect(0).expect("row 0 is visible");
        let lower = open.row_rect(1).expect("row 1 is visible");
        assert_eq!(
            upper.max.y, lower.min.y,
            "fixture: the two rows share an edge"
        );
        Pos2::new(upper.center().x, upper.max.y)
    };
    frame(&mut harness, [Event::PointerMoved(edge)]);
    assert_eq!(
        selected(&harness),
        Some(1),
        "the hover resolved the shared edge to the row above, where the click will not"
    );
    release(&mut harness, Key::Tab, ctrl());
    assert_eq!(
        shown(&harness),
        Some(ids[2]),
        "the walk ended on the row the pointer never chose"
    );
}
