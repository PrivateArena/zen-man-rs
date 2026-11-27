# Recent tab switcher (`Ctrl+Tab`) and tab group switcher (`Alt+Z`) — design

**Status: shipped.** Features 1–9 work; §10's seven steps are done and each carries its own tests (the
25 behavioural cases in `crates/zenman-ui/tests/tab_switcher.rs` — §9's list plus one case per
correction in §11 — the widget's geometry, hold and identity rules in `panels/switcher.rs`' unit tests,
the rings in `state.rs`'s, the table in `keymap.rs`'s, and the panel's *appearance* as the committed
baselines `18-recent-tabs.png` / `19-recent-groups.png`, which are re-rendered byte-identically by
`cargo xtask ci`'s snapshot gate).

Two rules in this document were wrong in their first drafting and are recorded here because each one is
a trap for whoever edits the hold logic next: see §11.

Go's reference is the recent-tabs overlay: `frontend/js/keyboard.js:23-141` (`ensureOverlayCreated`,
`renderRecentTabsOverlay`, `updateOverlaySelection`, `selectAndClose`, `cancelTabCycling`,
`triggerTabCycling`) with `state.js:73-92` (`getRecentTabs`) and the CSS in
`frontend/css/tabs.css:224-342`. That overlay was triggered by `Shift+Tab` / `Alt+Z` and <kbd>Ctrl+Tab</kbd>
was a plain positional cycle (`cycleTabs`, `keyboard.js:203-207`). This design keeps Go's overlay
mechanics — snapshot the recent list, highlight one row, commit when the modifier comes up, let the
pointer move the highlight — and moves them onto <kbd>Ctrl+Tab</kbd>, which is where users look for them.

The widget is written once and driven twice: the same panel lists tabs (<kbd>Ctrl+Tab</kbd>) and tab
groups (<kbd>Alt+Z</kbd>).

## 1. Feature

1. <kbd>Ctrl+Tab</kbd> opens a floating panel listing the pane's tabs in **recently-used order**, the
   tab already on screen first, the one you would go back to highlighted. Nothing is switched yet.
2. Another <kbd>Ctrl+Tab</kbd> moves the highlight one place **deeper into the past**; it wraps around
   the end of the list back to the shown tab. <kbd>Ctrl+Shift+Tab</kbd> walks it the other way.
   <kbd>↑</kbd>/<kbd>↓</kbd> move the highlight without wrapping (mouse-hand convenience).
3. Letting go of the **modifier** switches to the highlighted row. The **key** lifting is not a commit —
   it is the half of the next tap: the gesture is <kbd>Ctrl</kbd> held while <kbd>Tab</kbd> is tapped
   repeatedly, so every step begins with the key coming up on its own. Go's `keyupHandler` watches
   `e.key === currentModifierKey` for exactly this reason (`keyboard.js:126-129`). A single tap of the
   whole chord is therefore "back to the previous tab".
4. Moving the **mouse** over a row moves the highlight to it, in the frame it moves; **clicking** a row
   commits it at once; clicking **outside** the panel cancels. A pointer that has *not* moved never
   outvotes the keyboard: the panel opens centred under wherever the mouse was left, so "what is under
   the pointer" would otherwise hold row 3 forever and feature 2 would do nothing.
5. <kbd>Escape</kbd> cancels and leaves the shown tab where it was. Losing window focus cancels too.
6. Touching a tab at all — click, keyboard, a new tab, a flyout pick — puts it at the head of the
   recent list, which is what `Pane::touch_mru` already does; the panel is only a view of that ring.
7. The ring holds every tab of the pane (cap `MRU_CAP`), not Go's five
   (`state.js:91` slices to 5), so a pane with ten recent tabs offers ten rows.
8. <kbd>Alt+Z</kbd> opens the **same panel** listing the pane's **tab groups** in recently-used order
   (a group is recent when a tab inside it was shown); committing activates that group's most recent
   member tab. <kbd>Alt+Shift+Z</kbd> walks the list backwards.
9. A pane with one row to offer answers nothing at all: no panel, no switch (Go's
   `if (recentTabs.length <= 1) return;`, `keyboard.js:113`).

Explicitly **not** in this design: live preview of the highlighted tab behind the panel (the highlight
is the promise, the release is the switch, and previewing would churn `mru` while cycling); persistent
recent order across restarts (§8); a cross-pane switcher (the panel always lists the pane that owns the
keys, like Go's `getRecentTabs(state.activePane)`).

## 2. Chords (`keymap.rs`)

`Chord::matches` is exact on the modifier set (`keymap.rs:450-465`), so <kbd>Ctrl+Tab</kbd> and
<kbd>Ctrl+Shift+Tab</kbd> are already two distinct answers and no chord below collides.

| Command | chords | was |
|---|---|---|
| `SwitchTab` | `Ctrl+Tab`, `Shift+Tab` | new |
| `SwitchTabBack` | `Ctrl+Shift+Tab` | new |
| `SwitchGroup` | `Alt+Z` | new |
| `SwitchGroupBack` | `Alt+Shift+Z` | new |
| `NextTab` | `Ctrl+PageDown` | `Ctrl+Tab` |
| `PrevTab` | `Ctrl+PageUp` | `Ctrl+Shift+Tab` |
| ~~`CycleTabsMru`~~ | — | **removed**: `SwitchTab` supersedes the one-step MRU walk |

Notes:

* `NextTab`/`PrevTab` keep their positional ring walk (`cycle_tab`, `app.rs:3396`); they only lose the
  chord that a switcher wants. <kbd>Ctrl+PageUp/Down</kbd> is the usual second spelling and was unbound
  (`PageUp`/`PageDown` bare belong to `MoveUp`/`MoveDown`, `keymap.rs:772`).
* `Shift+Tab` keeps a `SwitchTab` binding so Go's trigger survives (`keyboard.js:169-183`).
* Every new variant goes into `Command::ALL` (`keymap.rs:187`) and into `TABLE` (`keymap.rs:725`):
  `tests/keymap.rs:208` fails on a missing row and `:242` on a colliding chord.
* `Command::label()` (`keymap.rs:242`): "Recent tabs", "Recent tabs (backwards)", "Recent tab groups",
  "Recent tab groups (backwards)". The panel's own title comes from `SwitchKind::title()`, not from the
  command, because two chords feed one panel.
* A row in the parent menu is *not* added: a menu item cannot hold a key down (§1's whole mechanic).

`KeyMap::lookup_events` becomes `lookup_events_flagged`, because a switcher has to know whether the
press that answered was the keyboard or the keyboard's auto-repeat:

```rust
/// Every command this frame's events ask for, and whether the press that answered was an
/// auto-repeat (`Event::Key { repeat: true }`).
pub fn lookup_events_flagged(&self, events: &[Event]) -> Vec<(Command, bool)>
```

One function, not two: the flag is the whole reason a caller inspects the events, so a second
"commands only" entry point would be an API nobody calls (the crate's own tests, which want the plain
commands, have a local helper that drops the flag).

## 3. Recency model (`state.rs`)

```rust
/// [`Pane::mru`] capacity. The switcher lists the ring, so the cap is "every tab a pane will
/// realistically have", not a memory bound (`MRU_CAP` was 5 while the panel had no rows).
pub const MRU_CAP: usize = 32;
```

```rust
pub struct Pane {
    // …
    /// Most-recently-used **tab** ring, head = the tab on screen.
    pub mru: VecDeque<TabId>,
    /// Most-recently-used **group** ring, head = the group the shown tab belongs to (`Alt+Z`).
    /// A tab with no group does not touch it.
    pub group_mru: VecDeque<GroupId>,
}
```

`touch_mru` is unchanged in shape (retain + push_front + trim to `MRU_CAP`). Add the twin, and the two
readers the panel uses:

```rust
/// A tab becoming the shown one also makes its group the recent one.
pub fn touch_group_mru(&mut self, id: GroupId);

/// The tabs in recently-used order: the ring, then every tab that never earned a touch (a restored
/// session, a tab whose ring entry was capped away), in strip order. Go's `getRecentTabs`
/// (`state.js:73-92`) answers the same two-part question.
pub fn recent_tabs(&self) -> Vec<TabId>;

/// The groups in recently-used order, then the rest in creation order.
pub fn recent_groups(&self) -> Vec<GroupId>;
```

Upkeep — the same discipline as the group algebra, so the rings never name a dead id:

* `Pane::show` touches the tab **and** the shown tab's group — but showing is not the only way the
  screen moves. Three mutations change who is on screen, or which group they belong to, without a
  `show`: `close_tabs` clamps onto the dead tab's neighbour, and `create_group` / `join_group` /
  `leave_group` reassign membership under it. Each ends with the private `sync_recency`, which re-touches
  both rings for whoever is on screen now. That is what makes row 0 mean "the tab on screen" (§1.1) and
  makes the heads below worth asserting (§11.4).
* `Pane::close_tabs` prunes `mru` (`state.rs:1092`) and `prune_groups` prunes `group_mru`.
* `debug_assert_invariants` (its ring half split out as `debug_assert_rings`) checks: no id twice in
  either ring, every member exists, and **both heads are the shown tab and its group**.
* `move_tabs` needs nothing: the rings are keyed by `TabId`, so a reorder leaves them alone.

`Pane::recent_tabs` is also what `cycle_mru_tab` should read once `SwitchTab` replaces it — the helper
stays, and `cycle_mru_tab` is deleted with its command.

## 4. The switcher widget (`panels/switcher.rs`)

A new panel module. It owns **presentation and the highlight**, never the engine: it reports what the
pointer did, and `ZenApp` decides what that means. This is the project's command-enum rule (no
callbacks out of a widget) applied to a state machine.

```rust
/// What the panel is a list of. The rows are built the same way; only the words and the icon differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchKind {
    Tabs,
    Groups,
}

impl SwitchKind {
    /// The panel's heading, in Go's own words (`keyboard.js:33` "Recent Tabs"); the paint uppercases
    /// it and appends `" (N)"` with the row count.
    pub fn title(self) -> &'static str;
    /// `Tab` or `Z` — the key whose release is half of the commit test.
    pub fn key(self) -> Key;
    /// The hold for the chord that actually opened the panel, read off the press's modifiers:
    /// `Hold::TAB` for `Ctrl+Tab`, `Hold::TAB_SHIFT` for `Shift+Tab`, `Hold::Z_ALT` for `Alt+Z`.
    /// Never requires `Shift` for the *backwards* walk (§2).
    pub fn hold(self, modifiers: &egui::Modifiers) -> Hold;
}

/// One row's answer: what to switch to if this row is committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchTarget {
    Tab(TabId),
    Group(GroupId),
}

/// A row, ready to paint. Built by `ZenApp` from `Pane`, so the widget never reads the model.
#[derive(Clone, Debug)]
pub struct SwitchRow {
    pub target: SwitchTarget,
    /// `icons::paint` name — `Some("folder")` for a tab (`assets/icons/folder.svg`, Go's folder emoji);
    /// `None` for a group, whose mark is its colour dot.
    pub icon: Option<&'static str>,
    pub title: String,
    /// Second, muted line: the tab's path, or a group's `N tabs`.
    pub detail: Option<String>,
    /// The group colour dot, from `Tokens::group_color` (`tabs.css:330-342`). On the right for a tab
    /// that belongs to a group; on the left, as the whole mark, for a group row.
    pub dot: Option<Color32>,
}

/// The modifier-plus-key that keeps the panel alive. Releasing it commits, which is Go's
/// `keyupHandler` → `selectAndClose` (`keyboard.js:126-131`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hold {
    pub key: Key,
    /// Which modifiers must stay down. `shift` is only ever set by `Shift+Tab`: `Ctrl+Shift+Tab` steps
    /// the same panel backwards, so requiring `Shift` there would commit the instant it was pressed.
    /// `ctrl` is satisfied by `command`/`mac_cmd` too — `Chord::matches`' macOS lie.
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}
impl Hold {
    pub const TAB: Self;         // Ctrl + Tab
    pub const TAB_SHIFT: Self;   // Shift + Tab, Go's own trigger
    pub const Z_ALT: Self;       // Alt + Z
    pub const fn bare(key: Key) -> Self;   // no modifier state to read: the key alone holds
}
impl SwitchKind {
    /// The hold for the chord that actually opened the panel, off the press's modifiers:
    /// [`Hold::TAB`], [`Hold::TAB_SHIFT`], [`Hold::Z_ALT`], or [`Hold::bare`] when the press carried
    /// no modifiers at all (a synthetic stream — demanding `Ctrl` there would close the panel in the
    /// frame it opened).
    pub fn hold(self, modifiers: &egui::Modifiers) -> Hold;
}

impl Hold {
    /// Held while every required modifier is down. The key is **not** part of the test when a modifier
    /// is: `Ctrl` held and `Tab` tapped must be able to walk, and every tap lifts the key.
    /// [`Hold::bare`] (no modifier in it) is the one case whose key is the signal.
    pub fn held(&self, input: &InputState) -> bool;
}

/// The panel that is open, or `None` in `ZenApp` when nothing is.
#[derive(Clone, Debug)]
pub struct Switcher {
    pub kind: SwitchKind,
    pub slot: PaneSlot,
    pub rows: Vec<SwitchRow>,
    pub selected: usize,
    /// First row of the visible window (§4.1), kept under `selected` by every move.
    first: usize,
    hold: Hold,
    /// Where the panel was laid out last frame, and where it will be laid out again: the rects are
    /// arithmetic, not paint output (§4.1).
    frame: Rect,
    rects: Vec<Rect>,
}

/// What the pointer did to the panel this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchPointer {
    /// A row was clicked — commit it.
    Picked(usize),
    /// The panel was dismissed by a click outside it.
    Dismissed,
}

/// Which way the highlight travels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Forward,
    Back,
}
```

API (all pure, all cheap enough for every frame):

```rust
impl Switcher {
    /// `None` when there is nothing to switch to (fewer than two rows) — feature 9.
    pub fn open(kind: SwitchKind, slot: PaneSlot, rows: Vec<SwitchRow>, hold: Hold) -> Option<Self>;
    /// Move the highlight, wrapping: `Step::Forward` past the end lands on `0`, i.e. back on the tab
    /// that is on screen. Go's `(selectedIndex + 1) % length` (`keyboard.js:136`). Slides the window
    /// (§4.1) so the highlight is always on screen.
    pub fn step(&mut self, step: Step);
    /// Arrow keys: the same travel, clamped instead of wrapped.
    pub fn nudge(&mut self, delta: i32);
    /// The rows the panel can show at once for this screen and token set.
    pub fn visible(&self, screen: Rect, tokens: &Tokens) -> usize;
    /// The row the pointer is over, if it is over one; also moves the highlight to it.
    pub fn hover(&mut self, at: Pos2) -> Option<usize>;
    /// The highlighted row's target.
    pub fn target(&self) -> Option<SwitchTarget>;
    /// The modifier came up (`Hold::held` is false).
    pub fn released(&self, input: &InputState) -> bool;
    /// Drop rows that no longer exist (`ZenApp` hands back the survivors' index map).
    pub fn retain<F: FnMut(&SwitchRow) -> bool>(&mut self, keep: F);
    /// Paint. Returns the pointer's answer, and remembers the rects for the next frame.
    pub fn show(&mut self, ctx: &Context, tokens: &Tokens) -> Option<SwitchPointer>;
}
```

### 4.1 Layout is arithmetic

The panel is centred in `ctx.screen_rect()` (Go: `align-items/justify-content: center`,
`tabs.css:233-235`) with a fixed `tokens.switcher_width`. Its height is
`pad + title + visible_rows * row_height + pad`, where `visible_rows` is
`rows.len().min((tokens.switcher_max_height - title - 2 * pad) / row_height)`. So every rect — panel,
each row — is computable **before** anything is painted, which is what makes the hover answer the same
frame the pointer arrives instead of one frame later. `show` therefore:

0. takes `screen` from `ctx.viewport_rect()` — the pinned fork has no `Context::screen_rect()`;
1. computes the layout (`fn layout(rows: usize, first: usize, screen: Rect, tokens: &Tokens) ->
   SwitchLayout`, a function of its own so a test can check the geometry without a harness, exactly like
   `tab_bar::layout`);
2. if `ctx.input(|i| i.pointer.hover_pos())` **changed** since the last painted frame, hit-tests it
   against the row rects and writes `selected` (`over`/`armed` on the struct: the first frame records a
   position rather than obeying it, because a mouse that was already resting there when the chord was
   pressed has not asked for anything);
3. paints the backdrop, the frame, the heading, the visible rows;
4. returns `SwitchPointer` from the click it saw.

```rust
/// Where the panel and its rows land. `rects[i]` is row `first + i`, and only the visible window of
/// the list has a rect.
pub struct SwitchLayout {
    pub frame: Rect,
    pub heading: Rect,
    pub first: usize,
    pub rects: Vec<Rect>,
}
```

**A window, not a `ScrollArea`.** A long list shows the `visible_rows` rows around the highlight: `first`
is kept in step by `step`/`nudge`/`hover` so `selected` is always inside `first..first + visible`, the
same way the file list's scroller follows its cursor. A scroll area would break the arithmetic of §4.1
(a row's rect would depend on a scroll offset egui only knows after painting) and would answer the
pointer one frame late. The heading carries the total — `Recent Tabs (12)` — so a five-row window over
twelve rows never reads as a twelve-row pane.

### 4.2 Painting

* Backdrop: the whole screen rect filled `tokens.switcher_backdrop` (Go's `rgba(0,0,0,0.4)`,
  `tabs.css:230`) with a `Sense::click()` response. That response is what "click outside cancels" reads,
  and it also stops the file rows underneath from lighting up under the panel.
* Panel: `bg_sidebar` fill, `border` stroke, `switcher_corner_radius`, `menu_shadow`/`menu_shadow_blur`
  like the flyout (`app.rs:1137-1200`).
* Heading: `SwitchKind::title()`, uppercase, muted, `switcher_title_size` — a real `egui::Label` put in
  the rect, never painted text, so the accessibility tree and the kittest suites can see it (the
  pattern at `status_bar.rs:334-400`).
* Row: icon at the left (`icons::paint`), then a `Label` for `title` (`switcher_name_size`,
  `text_active`) and a `Label` for `detail` under it (`switcher_path_size`, `text_muted`, ellipsised),
  then the optional colour dot at the right edge.
* Selected row: `bg_hover` fill and a 1 px `accent` border (Go's `.selected`, `tabs.css:286-290`).
  Hover *is* selection (§1.4), so one rule covers both.

## 5. Wiring (`app.rs`)

One field:

```rust
/// The switcher panel that is open (`docs/tab-switcher-design.md` §4). One at a time, like `menu`.
pub switcher: Option<Switcher>,
```

* `handle_keys` runs the four switcher commands through
  `switch_chord(slot, kind, back, modifiers)` and swallows **everything else** while a panel is open, so
  a stray <kbd>D</kbd> held over an open panel cannot delete a file. The question is asked **per
  command**, not once per frame: a batch carrying the chord *and* a second keypress opens the panel and
  must not then hand that second key to the file list (§11.6). Repeats are dropped from the switcher
  commands with `lookup_events_flagged`, so holding <kbd>Ctrl+Tab</kbd> cannot run the highlight away at
  the auto-repeat rate — and egui *rewrites* the flag for a key that never came up (§11.3), so a step
  needs a real tap. <kbd>↑</kbd>/<kbd>↓</kbd> nudge, <kbd>Escape</kbd> cancels.
* `logic` calls `tick_switcher(ctx)` **immediately after** `handle_keys`, before anything else. It runs
  in that spot because a press and a release that land in one frame must still produce a switch
  (feature 3's quick tap), and it does, in order: let the panel go when a modal, a focused text field or
  a typing dock float has taken the keyboard away (a panel nobody can step would otherwise commit what
  it holds the moment its modifier comes up, possibly behind a dialog); cancel when the window lost
  focus; prune the rows that died; commit when the modifier came up. No repaint timer — every event that
  can move the panel is an input event, and an input event already buys a frame.
* `switch_chord` keys on the **kind *and* the pane**: `slot` is the pane that owns the keys, so a panel
  left open over the left one is stale the moment a click moves the keys, and it is rebuilt rather than
  stepped. A chord whose kind has nothing to list (feature 9) says nothing *and leaves the panel already
  on screen alone* — feature 9 is about opening, not about closing the user's panel.
* `dock_content`'s "a click focuses this pane" test is handed the panel's rect: the panel is centred in
  the window, so it floats over *several* panes, and a press on one of its rows must not move the
  keyboard to the pane underneath it (§11.5).
* `ui` calls `show_switcher(ui.ctx())` after `show_group_hover` and before the modals: the
  last-allocated overlapping rect wins the pointer in this fork.
* `escape` gets the switcher at the **head** of its ladder, above the address bar: while the panel is
  up, <kbd>Escape</kbd> belongs to the panel and nothing else.
* `tick_group_hover` adds `|| self.switcher.is_some()` to its "a different kind of attention" test, and
  opening the panel clears `tab_group_hover` in the same breath.
* Committing a tab: `run_command(slot, PaneCommand::SelectTab { id, kind: TabSelKind::Replace })`, the
  flyout row's own path, so the selection, the strip and `mru` all follow the way a click makes them.
* Committing a group: the member tab furthest forward in `Pane::recent_tabs` (`Pane::member_ids` plus
  the ring), activated the same way. A collapsed group **stays collapsed**: the strip keeps the shown tab
  of a folded group visible (`tab_bar.rs:130`), so nothing the switch lands can be hidden by it.
* `dispatch` keeps a non-keyboard path for the four commands (no menu row exists; `dispatch` is the seam
  the tests and any future UI share) and opens/steps exactly as `handle_keys` would — a dispatch is
  always a fresh intent, so the repeat question does not arise.

## 6. Rows, built by the shell

```rust
/// The tab rows, in `Pane::recent_tabs` order. The panel lists what the ring holds, capped by
/// `MRU_CAP`, and the ring holds the whole pane.
fn switcher_tab_rows(&self, slot: PaneSlot) -> Vec<SwitchRow>;
/// The group rows, in `Pane::recent_groups` order, each with its member count.
fn switcher_group_rows(&self, slot: PaneSlot) -> Vec<SwitchRow>;
```

* Tab row: `icon` is `Some("folder")` — every tab of a file manager is a directory view,
  Go's `📁`, `keyboard.js:52`); `title` = `Tab::title`; `detail` = the shown path as text; `dot` = the
  colour of the group the tab belongs to.
* Group row: `title` = the group's name (or its colour's word when unnamed, matching the chip);
  `detail` = `"N tabs"`; `dot` = the group colour.
* The **shown** tab is row 0 (`recent_tabs` already starts there). The initial highlight is row **1** —
  the tab you would go back to, which is where Go starts too (`selectedIndex = 1`, `keyboard.js:124`).

## 7. Tokens (`theme.rs`)

Sizes from Go's own CSS (`tabs.css:246-336`), colours from the existing palette:

```rust
pub switcher_width: f32,          // 420.0  (`tabs.css:251`)
pub switcher_pad: f32,            // 16.0   (`tabs.css:250`)
pub switcher_gap: f32,            // 8.0    (`tabs.css:255`)
pub switcher_row_pad_x: f32,      // 14.0   (`tabs.css:278`)
pub switcher_row_pad_y: f32,      // 10.0   (`tabs.css:278`)
pub switcher_row_height: f32,     // 46.0   (two lines plus the padding)
pub switcher_icon_size: f32,      // 24.0   (`tabs.css:301`)
pub switcher_dot_size: f32,       // 8.0    (`tabs.css:331`)
pub switcher_title_size: f32,     // 11.0   (`tabs.css:265`)
pub switcher_name_size: f32,      // 12.0   (`tabs.css:314`)
pub switcher_path_size: f32,      // 10.0   (`tabs.css:323`)
pub switcher_corner_radius: u8,   // 12     (`tabs.css:249`)
pub switcher_max_height: f32,     // 420.0
pub switcher_backdrop: Color32,   // rgba(0,0,0,102) (`tabs.css:230`)
```

Both `Tokens::dark()` and `Tokens::light()` get the set; the light backdrop is the same grey-black at a
lower alpha so the list stays readable.

## 8. Persistence

Out of scope, and honest about it: `Pane::mru` is not persisted today, and `apply_workspace` rebuilds it
by touching the strip in order and the shown tab last, so after a restart <kbd>Ctrl+Tab</kbd> answers
"the **rightmost** tab of the pane": the ring's second place is whatever was touched just before the
shown one, and on a restore that is the strip's last entry. That is the same answer the
strip-order fallback in `recent_tabs` gives, so nothing new breaks. Persisting the real order means a
`#[serde(default, skip_serializing_if = "Vec::is_empty")] mru: Vec<String>` on `PaneSession`
(`store/config.rs:408`) keyed by the existing `TabSession.id` — the trick `groups` used to stay
byte-compatible with Go's files — and a change to `apply_workspace`'s touch loop. Deferred until the
panel exists; recorded here so the next person does not rediscover it.

## 9. Tests

`crates/zenman-ui/tests/tab_switcher.rs`, one behaviour per `#[test]`, prose names, driven by the
`Harness` recipe of `tests/tab_groups.rs:64-100` (`app.logic()` + `app.ui()` per frame). Input helpers:
`kittest`'s `key_down_modifiers`/`key_up_modifiers` for the hold, and `click_at`/`PointerMoved` from
`tests/tab_groups.rs:203-231,469-479` for the pointer. Row rects come from the widget's own
`harness.state().switcher` geometry, not from the a11y tree.

1. `ctrl_tab_opens_the_panel_without_touching_the_shown_tab` — rows exist, `pane.active` unchanged.
2. `a_tap_of_ctrl_tab_lands_on_the_previous_tab` — press, release, `activate` happened.
3. `another_press_walks_deeper_and_wraps` — 3 tabs: highlight goes 1 → 2 → 0 → 1.
4. `ctrl_shift_tab_walks_backwards`.
5. `releasing_ctrl_commits_the_highlighted_row`.
6. `escape_cancels_and_leaves_the_shown_tab_alone`.
7. `holding_ctrl_tab_does_not_run_the_list_away` — several frames, one `repeat: true` event each: the
   highlight moves once.
8. `hovering_a_row_moves_the_highlight` and `clicking_a_row_commits_it`.
9. `clicking_outside_cancels`.
10. `a_tab_touched_by_hand_joins_the_list` — the user's example: three tabs, click the third, the panel
    order is the third, the first, the second.
11. `one_tab_answers_nothing` — a pane with a single tab opens no panel and switches nothing.
12. `alt_z_lists_the_groups_and_switches_to_a_member` — two named groups; the highlight order follows
    recency, i.e. the group whose tab was last shown comes first.
13. `the_panel_and_the_fold_flyout_do_not_fight` — a hovered chip loses its flyout to the panel.
14. `next_tab_and_prev_tab_still_walk_the_strip` — the rebound <kbd>Ctrl+PageUp/Down</kbd>.
15. `the_panel_dies_with_its_tab` — close the highlighted tab through the model between frames; the next
    frame does not panic and does not select a dead id.
16. `a_tap_of_tab_survives_its_own_release` — **one event per frame**: press, key up (Ctrl still down →
    the panel must stand), key down (→ one step deeper), modifier up (→ commit). The one-frame
    up-and-down of the other tests cannot catch a hold that watches the key, which is the bug this
    test exists to keep dead.
17. `a_still_pointer_loses_to_the_keyboard` — hover a row, let two frames pass with the mouse unmoved,
    step: the highlight moves anyway. Move the pointer *inside* the highlighted row: nothing is stolen.
    Move it into another row: it takes. Feature 4's boundary against feature 2.

Every §11 correction has a case of its own among the 23, because each one was found by *reading* code
that a suite of green tests had already approved: `row_zero_is_the_tab_on_screen_after_a_close`,
`grouping_the_shown_tab_leads_the_group_list`, `the_highlight_follows_its_row_and_keeps_its_place`,
`a_chord_after_the_keys_move_lists_the_new_pane`,
`the_frame_that_opens_the_panel_answers_no_other_key` (`Ctrl+A` in the opening frame must not reach the
listing), and the two hold/pointer cases 16 and 17.

Plus `state.rs` unit tests for `MRU_CAP`, `recent_tabs`, `recent_groups`, `touch_group_mru` and the
invariant asserts, and `keymap.rs` unit tests for the new table rows (`every_command_round_trips_…`,
`no_two_commands_answer_one_chord` cover most of it; the `CycleTabsMru` chord assertion at
`keymap.rs:1055` is deleted with the variant).

## 10. Steps

The ring and the chords come first because both are load-bearing for the panel; the widget is written
before the shell drives it so the geometry can be unit-tested alone; the behavioural suite is last
because it is the only thing that proves the release-detection works against a real event stream.

| # | Step | Verify |
|---|---|---|
| 1 | `state.rs`: `MRU_CAP` 32, `group_mru`, `touch_group_mru`, `recent_tabs`, `recent_groups`, upkeep in `show`/`close_tabs`/`prune_groups`, invariants | `cargo test -p zenman-ui --lib` (new unit tests + `mru_caps_at_five` retitled) |
| 2 | `keymap.rs`: 4 new commands, `NextTab`/`PrevTab` re-chorded, `CycleTabsMru` gone, `lookup_events_flagged`, module doc | `cargo test -p zenman-ui --lib keymap` |
| 3 | `theme.rs`: the `switcher_*` tokens, both palettes, and the new sizes quoted in `dark_tokens_quote_the_css_table` | `cargo test -p zenman-ui --lib theme` |
| 4 | `panels/switcher.rs`: kinds, rows, `Hold`, `layout`, `step`/`nudge`/`hover`/`retain`, `show` | `cargo check -p zenman-ui` + `layout` unit tests |
| 5 | `app.rs`: field, `switcher_*` row builders, `switch_chord`, `tick_switcher`, `show_switcher`, `handle_keys` swallow/repeat guard, `escape`, `logic`/`ui` hooks, mutual exclusion, group commit rule | `cargo clippy -p zenman-ui --all-targets` |
| 6 | `tests/tab_switcher.rs` (+ `lib` unit tests from steps 1–4 already in place) | `cargo test -p zenman-ui --test tab_switcher` |
| 7 | Docs: this file's Status → shipped, `PROJECT_OVERVIEW.md` read-triggers + file tree, `keymap.rs` module doc | `cargo xtask ci` |

## 11. Corrections found while building

1. **The hold watches the modifier, never the key.** This document first said the panel commits on
   <kbd>Ctrl</kbd> *or* <kbd>Tab</kbd>, "whichever comes up first" — plausible, and fatal: the gesture is
   <kbd>Ctrl</kbd> held while <kbd>Tab</kbd> is **tapped**, so the key comes up between every step, and the
   first tap's release would have closed the panel and landed on row 1 forever. Go's `keyupHandler` fires
   only on `e.key === currentModifierKey` (`keyboard.js:126-129`), which is the whole reason
   `Hold::held` ignores `key_down` when a modifier is in the chord (`Hold::bare` is the one case whose key
   is the signal, and it exists for a synthetic input stream that reports no modifiers at all).
   `a_tap_of_tab_survives_its_own_release` is the test that fails if this regresses.
2. **A still pointer does not select.** The panel is centred in the window, i.e. under wherever the mouse
   was left. Reading "what is under the pointer" every frame means the resting mouse outvotes every
   keystroke — the walk appears broken. `show` therefore moves the highlight only when the pointer's
   *position changed* since the frame it last painted, which is exactly what a `mouseover` event is
   (`keyboard.js:63-66`); the first frame after opening records the position instead of obeying it.
   `a_still_pointer_loses_to_the_keyboard` is the test.
3. **egui rewrites the `repeat` flag** (`input_state/mod.rs:403-411`: a `pressed: true` event for a key
   already in `keys_down` *becomes* a repeat). So `keymap.rs`'s `Chord::event()` only reads as a fresh
   press when the key came up first — which is what a real tap is, but a test that presses a held key twice
   is testing the auto-repeat guard, not the walk. That is why §9.7 and §9.3 are separate cases.
4. **`Context::screen_rect()` does not exist in the pinned fork** — `viewport_rect()`, plus
   `Event::WindowFocused(false)` where upstream egui says `LostFocus`. Both are cited where they are used.
5. **What was rejected.** *Live preview* of the highlighted tab behind the panel: it would touch `mru`
   while walking, which is the list the walk is reading. *Scrolling the row list*: a `ScrollArea` makes the
   rects depend on a scroll offset only egui knows after painting, i.e. §4.1's same-frame answer dies, so
   the window slides instead and the heading carries the total. *Persisting the rings* (§8): the restore
   path already rebuilds both, and the answer it gives is the one `recent_tabs` would give anyway.
6. **Recency follows the screen, not only `show`.** The rings were first written by `Pane::show` alone,
   which is nearly the whole story and none of the interesting ones: closing the tab on screen clamps
   `active` onto its neighbour without a `show`, so `mru.front()` was no longer the tab the user was
   looking at and row 0 lied about the pane (the highlight then started on the tab already showing, and
   the tap that followed switched nothing). Grouping the tab on screen has the same shape for the group
   ring. `Pane::sync_recency` is the upkeep, called by `close_tabs`, `create_group`, `join_group`,
   `leave_group` and `show`, and `debug_assert_rings` is the proof — it is the reason
   `row_zero_is_the_tab_on_screen_after_a_close` and `grouping_the_shown_tab_leads_the_group_list` exist.
7. **The highlight is a tab, not an index.** `retain` first clamped `selected` to the shortened list, so
   rows dying *above* the highlight silently moved it to another tab, and the release committed a row the
   user never chose. It now re-finds its own target, and keeps its *place* only when its row is the one
   that died (the next thing back in the past is where the finger was heading).
   `the_highlight_follows_its_row_and_keeps_its_place` is the case the first suite passed over because it
   closed the highlighted row, where both rules agree by accident.
8. **The panel's monopoly starts the frame it opens.** `handle_keys` decided "a panel is up" once, before
   the loop, so a frame carrying <kbd>Ctrl+Tab</kbd> *and* another keypress opened the panel and then
   handed that second key to the file list — one batch, one deleted file. The check moved inside the loop.
9. **A floating panel owns the pointer *and* the focus.** `dock_content`'s focus rule is a raw geometry
   test — press inside a pane's rect, that pane owns the keys — and the centred panel overlaps several
   panes, so clicking a row used to move the keys out from under the walk and the *next* chord stepped a
   different pane's list. Two rules fix the pair: the pane is told the panel's rect (`PaneDock::overlay`,
   arithmetic and therefore known before painting), and `switch_chord` refuses to step a panel that
   belongs to another pane.
10. **`Ui::put` centres, and a `Frame` sizes to what its children used.** Two claims in §4.1 and §4.2
    were true of the arithmetic and false of the pixels: `Label::halign` cannot left-align a label whose
    box `Ui::put` (`Layout::centered_and_justified`) already shrank to the text, so the rows painted
    centred — visible in the committed baseline, invisible in every test; and the panel's own box came
    out ~9 px shorter than `layout`'s frame, leaving a strip that the wash's dismissal test refused to
    release. `label_left` supplies the layout Go's flex column implies, and the frame's inner rect is
    claimed with `allocate_rect` so the painted panel and the hit-tested panel are one rect.
11. **Two rows, one edge.** `Rect::contains` is inclusive, and the rows butt-join, so a point on a shared
    edge lies in two rects — and the hover (`position`) and the click (the last allocation wins in this
    fork) disagreed about which one, so the highlight could say row 1 while the release committed row 2.
    Both now resolve to the row *below*: `hover` scans with `rposition` and the click loop assigns rather
    than `get_or_insert`s. `a_shared_row_edge_belongs_to_the_row_below` sits on the seam.

