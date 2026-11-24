# Tab groups (Firefox-style) — design

Status: **shipped**. Landed as six commits after `358aa3d`, each with its own tests; §10 is the order
they went in. Surveys: `PROJECT_OVERVIEW.md`, commit `358aa3d` (the tab menu whose greyed group block this
replaces). Reviewed twice against an external design review; two of its suggestions were rejected with
evidence in §9. The two places where the shipped code is more precise than this document say so inline:
`MenuContext::tab_count` (§6) counts instead of being a bool, and `DropTarget::End` (§8) is the one
off-item landing that means something.

## 1. Feature

A pane's tab strip groups tabs the way Firefox does:

1. a **label chip** at the left of the group's run: click = collapse/expand, right-click = group menu;
2. **nine colours** to paint that label with;
3. **single- or multi-select** tabs, right-click → `New Tab Group`;
4. chip menu: **rename**, **ungroup** (release its tabs), **close group** (wipe its tabs);
5. **drag** a tab (or the whole selection) into or out of a group;
6. a collapsed group still **lists its tabs on hover**, and picking one switches to it;
7. groups **survive a restart** and restore in their pane, in their workspace.

## 2. Identity: `GroupId`; colour is an attribute

```rust
// crates/zenman-ui/src/state.rs
pub struct GroupId(pub u64);                       // minted by AppModel::next_group_id, like TabId
pub struct TabGroup { pub id: GroupId, pub name: String, pub color: GroupColor, pub collapsed: bool }
pub struct Pane { tabs, active, mru, pub groups: Vec<TabGroup>,
                  pub tab_sel: BTreeSet<TabId>, pub tab_sel_anchor: Option<TabId>, .. }
pub struct Tab { pub group: Option<GroupId>, .. }   // was Option<GroupColor>
```

Colour-as-identity was rejected: Go configs put the *same* letter on non-contiguous tabs (`A,none,A`);
re-colouring would have to re-key membership and merge with whatever already owned that colour; and every
id derived from a colour (popup ids, `DialogKind::RenameTabGroup { color }`, a queued command) goes stale
the moment the colour changes. An unnamed group stores `name: String::new()` and renders
[`GroupColor::name`], so re-colouring an unnamed group re-labels it, which is what Firefox does.

Re-colour = `Pane::set_group_color(id, color)`: sets the record's colour, and if another group already
owns it **merges** — the smaller run moves next to the larger (order preserved inside each), the second
record dies, the survivor is forced **expanded** (folding tabs the user was looking at is a surprise, not
a merge).

Invariants, enforced by `Pane`'s algebra and checked by `Pane::debug_assert_invariants()`, called at the
end of `ZenApp::run_command` and `ZenApp::apply_workspace`:

1. one group's members are **contiguous** in `pane.tabs` (`Pane::compact`, after create/merge/restore);
2. `groups` has an entry **iff** at least one tab names it (`Pane::prune_groups`, after every membership
   or tab-list change: close, drag-out, ungroup, duplicate) and pruning runs **before** compacting —
   compacting with a dangling id is the panic site;
3. `tab_sel ⊆ ids of pane.tabs`, pruned at the same points;
4. **a collapsed group still shows its active tab**, as one tab beside the chip. Nothing has to fight the
   user's collapse and `Pane::activate` never edits `collapsed`. Every reorder, close and prune goes
   through a `Pane` method that keeps `mru`/`active`/`tab_sel` in step — no bulk `tabs.retain` in `app.rs`.

`Pane.collapsed_groups: Vec<GroupColor>` (declared, never read) is deleted; the algebra replaces it.

## 3. Palette: nine colours

`GroupColor` gains `F, G, H, I` (`orange, cyan, pink, teal`) beside Go's `A..E`. The letters stay the
on-disk spelling — `#[serde(rename_all = "UPPERCASE")]` **is** Go's `group` key, so they are not renamable
to colour words — and identity moved to `group_id`, so a colour is now only an accent. `color()` still
answers `Some("#ff4a4a")` for `A` alone (`docs/contracts.md` pins that hex; the rest is the port's
palette). Added: `GroupColor::ALL`, `name()` (`"Orange"`), `label()` (`"Orange group"`), `parse_letter()`
(either case), and `deserialize_group_color` — a lenient reader, because a strict one turns Go's
lowercase `"a"` into a `Deserialize` **error** and that error drops the whole workspace.
`Tokens::group_orange/cyan/pink/teal` (dark + light) complete it.

## 4. Persistence (feature 7)

Rides the existing session path: `header::workspace_session` → `Autosave` (which compares
`Workspace: PartialEq`, so a group change is a dirty change) → `config.json` → `header::apply_workspace`.
No new store, no schema bump.

```rust
// crates/zenman-core/src/store/config.rs   (done)
pub struct TabGroupSession { id: String, name: String, group: Option<GroupColor>, collapsed: bool }
pub struct PaneSession { tabs, active, #[serde(default, skip_serializing_if = "Vec::is_empty")] groups }
pub struct TabSession  { .., #[serde(default, skip_serializing_if = "Option::is_none")] group_id }
```

`skip_serializing_if` keeps a group-free config byte-identical to today's, so the Go goldens and
`--session` files do not move. A group id string is **`g{n}`, where `n` is the runtime `GroupId`**:
restore keeps a parsable id verbatim and bumps the counter past it, so saving a session that was just
restored writes the same bytes (a re-mint would make every restore dirty). Unparsable ids mint fresh ones.

Restore order — it matters, because `compact` reorders `pane.tabs`:

1. `String → GroupId` from the `groups` rows **first**, never by position; a clashing id keeps the first
   row and drops the rest;
2. build the tabs resolving `group_id`; a tab naming an **unknown** id falls through to the legacy letter
   path rather than losing membership (`group` still says which colour it was in);
3. synthesise legacy runs — one unnamed expanded group per contiguous run of a bare `group` letter —
   **after** step 1, so "colour already taken, take the first free one" cannot steal a colour from a real
   row. Past nine runs colours repeat: colour is an attribute now;
4. `compact`, then `prune`, then re-resolve `active` **from the tab id**, not the stored index;
5. apply `collapsed`; a row with no members is dropped;
6. clear `tab_drag`, the hover state and any pending group prompt/menu — their ids died with the workspace.

`docs/contracts.md` §"Session save and restore" records `groups`/`groupId` as port-only keys and that a Go
re-save drops them.

## 5. Strip layout (`panels/tab_bar.rs`)

```rust
pub enum StripItem { Tab(usize), Group(GroupId) }   // a chip; its members are hidden when collapsed
pub fn items(tabs: &[Tab], groups: &[TabGroup], active: usize) -> Vec<StripItem>
pub struct StripLayout { pub rects: Vec<(StripItem, Rect)>, pub overflow: Vec<(usize, /*tab ix*/ usize)> }
```

`items` is pure and unit-tested (`runs()` is deleted: no caller, no test — its doc even cites a test that
does not exist). It filters the tabs a collapse hides, except the active one (invariant 4).

**Chips are laid out by the strip; `fit` never sees them.** `craft_ui::tabs::fit` guarantees only the
`selected` item survives eviction, so a chip handed to it can end up in the overflow popup, and its one
shared `minimum_width` shrinks a chip under its own chevron. So: chip width = truncated label
(`tokens.group_label_max_width`) + chrome, chips clamped evenly against the strip floor by the strip
itself, then `fit` gets the **tabs only** with `available - chips - gaps`, and `selected` is the active
tab's index in the filtered list. Painting maps `TabFit` indices back through the tab list, so no command
ever carries an item index.

`StripLayout`'s rects are exposed as **output** on `TabBarModel`: the hover flyout (§7) and the drop
hit-test (§8) read them instead of each running its own pass. Overflow lists tabs, never chips.

Chip = rounded rect in `tokens.group_color(color)`, a chevron painted from token-sized points (not a `▸`
glyph — the bundled fonts do not guarantee one), and the name painted with `ui.put` + `Label`
(`selectable(false)`) so kittest finds it by label exactly as it finds a tab title today. Every size is a
token — `group_chip_height`, `group_chip_padding`, `group_chip_gap`, `group_chevron_width`,
`group_label_max_width`, `group_border_width`, `tab_hover_open`, `tab_hover_grace` — because
`cargo xtask ci`'s geometry guard fails any float ≥ 3.0 literal in `zenman-ui/src` and its colour guard
fails any `Color32::` outside `theme.rs`.

Multi-selected tabs paint with `tokens.bg_hover` and the group's border. Widget ids become
`id.with(("tab", tab.id))` / `("group", group.id)`: an index-keyed id changes on every reorder or
collapse, which drops egui's in-flight drag state.

## 6. Commands: ids, resolved at apply time

```rust
pub enum TabCommand { Select { id: TabId, kind: SelKind }, Close(TabId), Context { id: TabId, at: Pos2 },
    NewTab, ToggleOverflow, ToggleGroup(GroupId), GroupContext { id: GroupId, at: Pos2 },
    DragStart(TabId), DragDrop(DropTarget) }
pub enum SelKind { Replace, Toggle, Range }               // the virtual_list modifier idiom
pub enum DropTarget { Before(TabId), End, OntoChip(GroupId) }
```

`dock_content` forwards them as id-carrying `PaneCommand`s (`SelectTabId`, `CloseTabs(Vec<TabId>)`,
`TabContextId`, `TabGroup { verb }`, `MoveTabs { tabs, target }`); `ZenApp::run_command` resolves id →
index **at apply time**, so a queued `Close` cannot act on a shifted index. `MoveTabs` ignores ids that
died and moves in **tab-list order** (selection order would re-shuffle the block); a drop onto a tab that
went to the overflow popup appends to the visible run. The old positional `PaneCommand::SelectTab/CloseTab`
stay for the keymap paths, which already address the shown tab. `SelKind` comes from
`ui.input(|i| i.modifiers)`, read once inside `show`.

Group verbs are **not** `Command`s (no chord wanted, and payload variants would break `Command::ALL` and
the keymap table): `menus::MenuAction::Group(GroupCommand)` with
`GroupCommand::{New, Leave, Rename, CloseGroup, ToggleCollapse, Color(GroupColor), SelectGroup}`. Which
tabs and which group a verb applies to is resolved in `ZenApp::run_group_command(slot, target, verb)` from
the open menu's `MenuTarget` and `Pane::tab_sel` — menu specs stay data-only.

Right-click rule: `PaneCommand::TabContext` folds the selection through `Pane::right_click_selection`
(a tab already in `tab_sel` keeps the set, any other collapses it to itself) **before** recording the
`context_request`, so `menu_context` never reads a stale selection and a row enabled for three tabs cannot
fire on one.

`MenuTarget::Tab { ix }` gains `group: Option<GroupId>, count: usize`; `MenuContext` gains `in_group` and
`tab_count`, and they are **not** in `bits()` — that array is the listing's seven plan booleans, which the
exhaustive 128-row table walks, and multiplying it by four would prove nothing about a listing menu. The
tab menu's own two facts get their own cases. New
`MenuTarget::TabGroup { id }` → `group_menu()`: Rename / Collapse-Expand / Select all in group / colour
rows / Ungroup / Close tabs in group (danger). The tab menu's six `MenuAction::Placeholder` rows become
real: `New Tab Group` (≥ 2 selected), `Add to group ▸` the nine colours, `Remove from group`.
`MenuAction::Placeholder` and `fn never` die; `tests/menus.rs`'s placeholder test becomes an enablement
test.

Rename uses the existing prompt path: `DialogKind::RenameTabGroup { slot, id }` +
`ZenApp::rename_group_prompt(id)` → `answer()` resolves the id (a group closed while the prompt was open is
a no-op, not a panic) → `PaneCommand::TabGroup { verb: Rename(name) }`.

## 7. Hover flyout (feature 6)

Not `egui::Popup::menu` (see §9). Same shape as the shell's other deferred popups — `context_request` +
`menu` — plus one state struct:

```rust
pub struct TabGroupHover { slot: PaneSlot, id: GroupId, chip: Rect, since: f64, popup: Option<Rect>, left: Option<f64> }
```

- the strip **reports** the hovered chip's rect as output (`TabBarModel::hover_chip: Option<(GroupId, Rect)>`),
  it does not emit a command per frame;
- `ZenApp::logic` opens it after `tokens.tab_hover_open`, and closes it `tab_hover_grace` after the pointer
  left `chip ∪ popup` — `response.hovered()` is a lie once the popup exists (the popup layer occludes the
  chip), so keep-alive tests raw `pointer.hover_pos()` against the union rect;
- `ctx.request_repaint_after` on both timers, or nothing moves without input;
- suppressed while a drag is in flight, while a context menu or modal is up, and when the pointer leaves
  the window;
- clicking an entry = `Select { id, Replace }` and **no** expand: the flyout is how you work inside a
  collapsed group, and invariant 4 keeps that tab painted.

Drawn as an `egui::Area` (`Order::Foreground`) + `Frame::popup`, keyed by `GroupId`.

## 8. Drag and drop (feature 5)

Hand-rolled, like `drag_drop.rs` (evidence in §9). `ZenApp::tab_drag: Option<TabDrag { slot, ids: Vec<TabId> }>`.
Tab rects take `Sense::click_and_drag()` and emit `DragStart(id)` on `drag_started()`; while `tab_drag` is
Some the strip hit-tests `pointer.interact_pos()` against **this frame's** `StripLayout` rects (never
`hovered()`, which egui suppresses during a drag), paints a bar at the gap and tints the chips that would
take the tab, then emits `DragDrop(target)` on release. The model is never reordered mid-drag. Dragging a
tab that is part of a multi-selection moves the whole selection. Gap rules: inside a run → insert there;
on a chip → append to that group; outside any group's territory → `Pane::leave_group`. A reorder that
breaks a run re-compacts it (invariant 1) instead of silently releasing membership — leaving a group is
only ever an explicit drop outside one.

`DropTarget::End` — the strip's own floor past the last item — is the one landing outside an item that
means something: it is how the end of the strip is reached, and it releases the tab from its group.
Anywhere else off-item cancels.

Cross-pane tab drag is out of scope.

## 9. Rejected review advice, with evidence

- *"use `dnd_set_drag_payload`"* — `craft-libs crates/ui/src/docking/render/body_drag.rs:247`
  (`starts_in_body`) claims the pointer for the dock whenever `egui::DragAndDrop::has_any_payload(ctx)` is
  true; the shell already hand-rolls file drags for the same reason (`drag_drop.rs` module doc).
- *"the strip already computes runs, treat it as verified"* — `tab_bar::runs()` has no caller and no test.
- *"use `egui::Popup::menu` for the flyout"* — click-toggled, closes on outside click, and its id churns
  with a colour re-key. The shell already owns its popup lifetimes (`OpenMenu` + `fixed_pos` area).
- *"cap at five colours / defer the palette"* — with `GroupId` the palette is orthogonal; the feature asks
  for the common colour names, and it is one enum plus four tokens.

## 10. Steps

Model and persistence cannot land apart (moving `Tab.group` to an id breaks the accent bar, the session
writer and the reader at once), and id-based commands are the riskiest refactor, so they go first, alone,
with no visible change.

Every step below is in; `cargo xtask ci` is green with all of them, and §1's seven behaviours are driven
end to end by `crates/zenman-ui/tests/tab_groups.rs` (chip paint, fold, fold-shows-the-active-tab, hover
flyout, rename prompt, ungroup/close, drag in and out, selection grouping, restart).

| # | Step | Verify |
|---|---|---|
| 1 | `GroupColor::F..I` + lenient serde + `ALL`/`name`/`label`, `TabGroupSession`/`groups`/`group_id`, theme's 9 colours | `cargo test -p zenman-core`, `-p zenman-ui --lib theme` |
| 2 | Model **and** session round-trip: `GroupId`, `TabGroup`, `Pane.groups`, algebra, invariants, `Tab.group` → id, `workspace_session`/`apply_workspace`, contracts.md | `cargo test -p zenman-ui` (all suites) |
| 3 | Positional → id `TabCommand`/`PaneCommand`, `id.with(("tab", id))`, `dock_content` translate — **no visual change** | `cargo test -p zenman-ui` all green |
| 4 | `items()` + `StripLayout` output, chip painting, collapse, `ToggleGroup`, `tests/tab_groups.rs` with its first test | `--test tab_groups` + suite |
| 5 | Multi-select (`tab_sel`, `SelKind`, painting) + right-click selection rule | `--test tab_groups` |
| 6 | Menus: real tab-menu group rows, `group_menu()`, `MenuAction::Group`, verbs, rename prompt | `--test menus`, `--test tab_groups` |
| 7 | Hover flyout: state machine, timers, repaint | `--test tab_groups` (hover via `send_move_to`) |
| 8 | Drag & drop: reorder, join, leave, selection move | `--test tab_groups` |
| 9 | `debug_assert_invariants` call sites, restart test, `PROJECT_OVERVIEW.md`/`contracts.md`, `cargo xtask ci` | `cargo xtask ci` |

Out of scope: cross-pane tab drag, dragging a whole group by its chip, group thumbnails, a new rasterised
snapshot scene (baseline churn until the feature is stable).
