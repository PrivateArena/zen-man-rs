//! The menus (plan §5.8 `menus/mod.rs` row) — step 10's **second** slice.
//!
//! Two halves, deliberately kept apart:
//!
//! * **The builders** are pure functions of a [`MenuContext`] to grouped [`MenuSpec`] data. No egui,
//!   no `Session`, no clock — which is what lets the acceptance criterion ("every `MenuContext`
//!   combination asserted", plan §7 row 10) be a *test over the cross-product* rather than a list of
//!   screenshots.
//! * [`show`] draws one such group list as a foreground [`Area`] and reports what was clicked. It owns
//!   no state: the open menu lives in [`crate::app::ZenApp`], and a click answers with a
//!   [`MenuAction`] that the shell routes through [`crate::app::ZenApp::dispatch`] — **the same
//!   function the chord runs** (plan **D7**: one table, two front ends, no duplicated action logic).
//!
//! # The Go source, and where each menu went
//!
//! | Go (`frontend/js/context-menu.js`, `tabs.js`) | Here |
//! |---|---|
//! | `showContextMenu` (`context-menu.js:42-131`) — a row, or the empty pane | [`file_menu`] (`MenuTarget::Row` / `Background`) |
//! | `showFolderContextMenu` (`:134-156`), the breadcrumb (`navigation.js:215`) and sidebar (`sidebar.js:36,93,131`) | [`folder_menu`], wired to the breadcrumb chips by [`crate::panels::breadcrumb`] |
//! | `showTabContextMenu` (`tabs.js:351-386`) | [`tab_menu`] |
//! | `showSearchResultContextMenu` (`:173-197`) | [`search_menu`], drawn by [`crate::dialogs::ResultMenu`]
//!   over a row of either search dialog's list |
//! | *no header/tray menu exists in Go at all* (`index.html:623` is the single `#context-menu` div) | none built |
//!
//! Deviations from the plan row, each with its reason:
//!
//! * **`MenuSpec::id: CommandId` → `action: MenuAction`.** The as-built keymap names commands
//!   [`Command`](crate::keymap::Command) (step 10a), and a *custom action* is not a `Command` — the
//!   plan's own "custom actions land before Delete" ordering needs a spec that names one, so the id
//!   is a sum of the two, not an enum of the keyboard's.
//! * **`label: &'static str` → `String`.** Same reason: a custom action's label is user data.
//! * **`chord: Option<Chord>` is gone.** Storing a chord would put the binding in two files, which is
//!   exactly what D7 forbids; [`MenuSpec::hint`] asks [`KeyMap::hint`] instead, so a re-bound key
//!   cannot leave a stale menu behind.
//! * **`MenuContext<'a>` has no lifetime** — nothing in it borrows. It carries one extra field,
//!   [`MenuTarget`](the row / background / folder distinction), because Go's menu shape depends on it
//!   and it is a sum, not one of the plan's seven booleans.
//! * **Disabled items stay visible**, greyed with [`Tokens::disabled_text`]. Go *removed* rows
//!   (`context-menu.js:57-66` only emits Paste Inside when a clipboard exists) and showed everything
//!   else unconditionally — including Delete over a read-only archive, which is the bug plan §5.8
//!   names. A greyed row keeps its place, teaches its chord and fires nothing.
//! * **The menu clamps, it does not flip.** Go's `positionElementSmartly` (`utils.js:21-44`) moves the
//!   whole box to the other side of the cursor near an edge and then clamps; egui's
//!   `Area::constrain(true)` clamps, which keeps the promise (the menu is never off-screen) with one
//!   fewer surprise.
//!
//! # What is not here yet (and who owns it)
//!
//! * **Change Permissions** (`context-menu.js:96`, `triggerChmod` at `:436-470`) is *absent*: it has
//!   no `Command` (10a's table has no chmod row, because chmod needs the octal prompt first) and the
//!   prompt is step **10c**. It lands as `Command::Chmod` + a `Prompt` dialog, in the Rename group.
//! * **Extract here / Properties** — steps **13** and **14**; they are the archive group's rows, and
//!   [`MenuContext::on_archive`] is already the predicate that gates them.
//! * **Custom actions execute in step 14** (`services/actions.rs`). [`file_menu`] takes the list and
//!   filters it by *role* only (Go's `getActionsForContext`, `custom-actions.js:337-366`); the glob
//!   `patterns` filter is that step's, and a click on a custom row is reported, never run.
//! * **Tab groups** ([`tab_menu`]'s greyed rows) are step **14**'s workspaces; `Duplicate Tab` is here
//!   ([`Command::DuplicateTab`]), the six group rows are not.
//! * **The `MenuTarget::Result` rows are drawn by their dialog, not by the shell.** A pane's menu is
//!   the shell's own (`ZenApp::draw_menu`), built from the shown tab's selection and dispatched
//!   against it. A search result is neither, and the card would cover a menu drawn where the panes'
//!   menus are drawn — so [`crate::dialogs::ResultMenu`] draws [`search_menu`] itself, from the
//!   dialog's `drain`. `menu_for`'s `MenuTarget::Result` arm is what it would answer through the
//!   shell's slot, which is why it exists and why nothing uses it.

use egui::text::LayoutJob;
use egui::{
    Area, Color32, CornerRadius, Frame, Id, LayerId, Margin, Order, Painter, Pos2, Rect, Response,
    Shadow, Stroke, Ui, Vec2, WidgetInfo,
};

use zenman_core::services::actions::{self, ActionContext};
use zenman_core::store::config::CustomAction;

use crate::icons;
use crate::keymap::{Chord, Command, KeyMap};
use crate::state::{GroupColor, GroupId, GroupVerb, TabId};
use crate::theme::Tokens;

/// What a menu item names: one of the shell's commands, or one of the user's actions (step 14).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuAction {
    /// A [`Command`] — clicked by the same [`crate::app::ZenApp::dispatch`] the chord uses.
    Command(Command),
    /// A custom action. Reported, never executed, until step 14's `services::actions`.
    Custom(CustomAction),
    /// A tab-group verb. Not a [`Command`]: these want no chord, and a payload variant of `Command`
    /// would break its `ALL` table and the keymap's. The verb names what to do; which group and which
    /// tabs it lands on is resolved from the menu's own target when the row is clicked
    /// (`docs/tab-groups-design.md` §6), so the spec stays the data this module insists on being.
    Group(GroupVerb),
    /// One row of a **search result's** menu ([`search_menu`], Go's `showSearchResultContextMenu`,
    /// `search-manager.js:376-390`). Not a [`Command`] either, for [`Self::Group`]'s reason: the
    /// keyboard has no binding for "open the folder that holds this row", and adding one to
    /// [`Command::ALL`] would put a chord on a verb Go never gave it. Which *row* is the owner's
    /// business — the path is not in the spec, so the spec stays data.
    Result(ResultVerb),
}

impl MenuAction {
    /// The [`Command`] half — what clicking the row *runs* — `None` for a custom action, a tab-group
    /// verb and a search-result row, whose verbs belong to the menu's owner, not to the keymap.
    #[must_use]
    pub fn command(&self) -> Option<Command> {
        match self {
            Self::Command(command) => Some(*command),
            Self::Custom(_) | Self::Group(..) | Self::Result(..) => None,
        }
    }

    /// The [`Command`] whose *binding* the row prints, which is not always the one it runs: a
    /// [`Self::Result`] row acts on a path the pane has not selected, so [`Self::command`] is `None` for
    /// it and would print no hint, while `context-menu.js:177` prints `open`'s chord beside the words.
    /// Two accessors, because *what to run* and *what to say* are genuinely different questions here.
    #[must_use]
    pub fn chord(&self) -> Option<Command> {
        match self {
            Self::Command(command) => Some(*command),
            Self::Result(verb) => verb.command(),
            Self::Custom(_) | Self::Group(..) => None,
        }
    }
}

/// What a search result's menu row does to the row it was opened over — Go's five `data-action`s
/// (`context-menu.js:175-193`), in its order. The two containing-folder rows are spelled `Reveal` /
/// `RevealInNewTab` because that is what the shell calls the action (`ZenApp::reveal_in_parent`, and
/// Go's own `reveal-search-result*` handlers beside it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultVerb {
    /// Go's `data-action="open"`: a folder navigates, a file opens — the row's own double-click.
    Open,
    /// `data-action="reveal"` — open the row's folder in the pane.
    Reveal,
    /// `data-action="revealNewTab"` — the same, in a tab of its own.
    RevealInNewTab,
    /// `data-action="copyPath"` — the row's own absolute path, as text.
    CopyPath,
    /// `data-action="copyName"` — just its name.
    CopyName,
}

impl ResultVerb {
    /// Go's exact wording (`context-menu.js:176-192`), which is the label the row prints.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Reveal => "Open Containing Folder",
            Self::RevealInNewTab => "Open Containing Folder in New Tab",
            Self::CopyPath => "Copy Path",
            Self::CopyName => "Copy Name",
        }
    }

    /// The rows that move the pane, and therefore close the dialog that asked ([`ResultVerb::Open`]
    /// and the two reveals); `false` for the two copy rows, which leave the dialog where it is.
    ///
    /// Go closes the search manager on every row (`closeSearchManager()` at `:683,691,701`) — the copy
    /// rows included, which is a bad day for anyone copying two paths, so the port keeps the card up for
    /// them and drops it only when the pane is about to change underneath it.
    #[must_use]
    pub const fn navigates(self) -> bool {
        matches!(self, Self::Open | Self::Reveal | Self::RevealInNewTab)
    }

    /// The keyboard's own verb for the same words, `None` when it has none. [`Self::label`]'s partner:
    /// `context-menu.js:177,188-192` prints `getShortcutDisplay('open'|'copy-path'|'copy-name')` beside
    /// these rows, and the port prints a hint only when the keymap really owns it — which is why the two
    /// folder rows have none, and why [`Command::Reveal`] (reveal in the *operating system's* file
    /// manager) is not the command behind `Open Containing Folder`.
    #[must_use]
    pub const fn command(self) -> Option<Command> {
        match self {
            Self::Open => Some(Command::Open),
            Self::CopyPath => Some(Command::CopyPath),
            Self::CopyName => Some(Command::CopyName),
            Self::Reveal | Self::RevealInNewTab => None,
        }
    }
}

/// What the menu was opened over. Go branched on exactly this (`context-menu.js:44` `isItem`, `:46`
/// `isDir`); it is a sum, so it is not one of [`MenuContext`]'s seven booleans.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MenuTarget {
    /// Empty pane space (Go's `showContextMenu(…, null, false, false)`).
    #[default]
    Background,
    /// One or more rows; `on_dir` is Go's `isDir` of the row under the cursor.
    Row {
        /// The row is a directory (or the selection is all directories).
        on_dir: bool,
        /// The row under the cursor is **runnable** — `zenman_core::ops::launch::classify` (design
        /// `docs/launch-sandbox-design.md` §3) answers anything but `NotRunnable` for it. Like
        /// `on_dir` this is a fact about the *click*, not about the shell's capabilities, so it
        /// lives here and not in [`MenuContext::bits`]: the cross product the tests walk is over the
        /// listing's booleans, and a row is either a script or it is not.
        on_run: bool,
    },
    /// A folder that is **not** a row of this listing — a breadcrumb chip (Go's
    /// `showFolderContextMenu` from `navigation.js:215`). Which folder is not in the variant: a path
    /// is not `Copy`, and this enum is carried inside the `Copy` [`MenuContext`], so
    /// [`crate::app::OpenMenu::folder`] names it. Go spelled the same thing as a DOM attribute
    /// (`data-target-path`, `context-menu.js:148-156`).
    Folder,
    /// The tab strip's menu (Go's `showTabContextMenu`, `tabs.js:351`); `id` is the tab the right-click
    /// landed on. `group` is the group that tab is in and `multi` whether more than one tab is selected
    /// — the two facts its group rows need, carried **here** rather than read back off the pane because
    /// the rows must not act on the shown tab ([`MenuContext`] warns about exactly that). An id and not a strip index, because the strip re-orders itself while the menu is
    /// open — an index would end up naming a tab the menu was never opened over. Its rows act on the
    /// tab `id` names, which is why
    /// [`crate::app::ZenApp::menu_context`] — built from the pane's *shown* tab — must never be read by
    /// one of its predicates, since the right-clicked tab need not be the shown one.
    Tab {
        /// The tab the click landed on.
        id: TabId,
        /// The group that tab belongs to, `None` when it belongs to none.
        group: Option<GroupId>,
        /// How many tabs the menu is speaking about: the multi-selection when the right-click landed
        /// inside it, one when it did not (`Pane::right_click_selection` made it so).
        count: usize,
    },
    /// A tab group's own menu, from its chip: every row acts on the whole run (`docs/tab-groups-design.md`
    /// §5), so this target has no tab and no selection.
    TabGroup {
        /// The group the chip is head of.
        id: GroupId,
        /// Whether the run is folded, which is what decides the label of the collapse row.
        collapsed: bool,
    },
    /// One row of a search result list — the find dialog's or the search manager's. Which row is not
    /// here (a path is not `Copy`, and [`MenuContext`] carries this enum) and not in
    /// [`MenuAction::Result`] either: the dialog that owns the menu holds the row and resolves it,
    /// exactly as [`Self::Tab`] holds an id its owner resolves.
    Result,
}

/// Everything a builder may ask about. Every field is derived by
/// [`crate::app::ZenApp::menu_context`] from live state — nothing here is a constant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MenuContext {
    /// At least one row is selected (Go: `tab.selectedPaths.size > 0`).
    pub has_selection: bool,
    /// Every selected row is a directory — the guard on *contents*-level clipboard work (Go's
    /// `copy_inside`/`cut_inside` handlers).
    pub all_dirs: bool,
    /// The clipboard holds something (Go: `state.clipboard && state.clipboard.op`).
    pub has_clipboard: bool,
    /// The clipboard is a *cut* (Go: `state.clipboard.op === 'cut'`).
    pub clip_is_cut: bool,
    /// The listed path lives in an archive store, i.e. is read-only (plan §4.2's `VfsKind`).
    pub on_archive: bool,
    /// The platform can make symlinks (`platform::Env::supports_symlink`).
    pub can_symlink: bool,
    /// A sandboxed launch could work here — `Session::can_sandbox`, the *measured* answer of design
    /// §5.4's probe: util-linux `unshare` on PATH **and** the kernel having granted the very namespace
    /// recipe the row will run. Gates the *presence* of the row, which is the only honest place for it:
    /// `setsid --fork` hides a refused launch from the caller, so a greyed row would be the last thing
    /// the user ever saw.
    pub can_sandbox: bool,
    /// A batch rename owns the pane: more than one row is selected, which is exactly what sent Go's
    /// [`crate::app::ZenApp::menu_context`]).
    pub in_batch: bool,
    /// The tab the menu was opened over belongs to a group. Tab menus only — a row of the listing has
    /// nothing to do with it, and `menu_context` answers `false` for every non-tab target.
    pub in_group: bool,
    /// How many tabs a tab menu's rows would act on — the selection the right-click left behind, never
    /// a stale one, because the right-click folded it first.
    pub tab_count: usize,
    /// What the click landed on.
    pub target: MenuTarget,
}

impl MenuContext {
    /// The plan's seven listing booleans plus design §5.4's `can_sandbox`, as the cross-product test
    /// walks them.
    ///
    /// [`MenuContext::in_group`] and [`MenuContext::tab_count`] are **not** in it: they belong to the tab
    /// menu, which reads nothing else, and widening this array would multiply every listing menu's
    /// exhaustive walk by four to prove nothing about it. The tab menu's two facts get their own table in
    /// [`crate::app::ZenApp::menu_context`]'s tests instead.
    #[must_use]
    pub const fn bits(&self) -> [bool; 8] {
        [
            self.has_selection,
            self.all_dirs,
            self.has_clipboard,
            self.clip_is_cut,
            self.on_archive,
            self.can_symlink,
            self.in_batch,
            self.can_sandbox,
        ]
    }

    /// A context from [`Self::bits`], leaving [`MenuContext::target`] and the two tab facts alone.
    pub fn with_bits(&mut self, bits: [bool; 8]) {
        let [
            has_selection,
            all_dirs,
            has_clipboard,
            clip_is_cut,
            on_archive,
            can_symlink,
            in_batch,
            can_sandbox,
        ] = bits;
        self.has_selection = has_selection;
        self.all_dirs = all_dirs;
        self.has_clipboard = has_clipboard;
        self.clip_is_cut = clip_is_cut;
        self.on_archive = on_archive;
        self.can_symlink = can_symlink;
        self.in_batch = in_batch;
        self.can_sandbox = can_sandbox;
    }
}

/// One menu row, as data.
#[derive(Clone, Debug)]
pub struct MenuSpec {
    /// What clicking it means.
    pub action: MenuAction,
    /// The text. Go's own wording (`context-menu.js`'s `<span>`s), not the plan's, except where D6
    /// splits one Go action into two (see [`file_menu`]).
    pub label: String,
    /// An icon from [`crate::icons`]. **Go paints none** (`context-menu.js:47-107` is label + chord
    /// only); the plan's `MenuSpec::icon` is the port's own column, sized by `Tokens::menu_icon_size`.
    pub icon: Option<&'static str>,
    /// Danger styling — Go's single `style="color: var(--danger-color)"` (`context-menu.js:99`).
    pub danger: bool,
    /// The enabled predicate. `false` greys the row and swallows its click; it never hides it.
    pub when: fn(&MenuContext) -> bool,
    /// Draw this row **and the one after it** side by side in one line (design §2: the `Execute` pair
    /// costs the popup one row, not two). Only ever set on the left half — a `paired` spec that is the
    /// last one in its group is a bug the renderer cannot express, so [`show`] falls back to drawing it
    /// alone rather than dropping it.
    pub paired: bool,
}

impl MenuSpec {
    /// The plan's `MenuSpec::id` — the command half, `None` for a custom action.
    #[must_use]
    pub fn command(&self) -> Option<Command> {
        self.action.command()
    }

    /// `true` when this spec is clickable in `ctx`.
    #[must_use]
    pub fn enabled(&self, ctx: &MenuContext) -> bool {
        (self.when)(ctx)
    }

    /// The chord this command answers, straight out of the keymap (D7 — never a copy of it).
    #[must_use]
    pub fn chord(&self, keys: &KeyMap) -> Option<Chord> {
        self.action
            .chord()
            .and_then(|command| keys.bindings_for(command).first().copied())
    }

    /// The right-aligned hint [`show`] prints; `None` for a menu-only command and for a custom action.
    #[must_use]
    pub fn hint(&self, keys: &KeyMap) -> Option<String> {
        self.chord(keys).map(|chord| chord.display())
    }

    /// Share this row's line with the spec that follows it — the layout half of design §2's pair, set
    /// at the call site (`file_menu`) so the *builder* says which rows share a line and the renderer
    /// never has to guess from a label.
    #[must_use]
    pub fn paired(mut self) -> Self {
        self.paired = true;
        self
    }
}

/// Two specs are the same *row* when they look the same. `when` is deliberately not part of it:
/// comparing function pointers is deprecated (`unpredictable_function_pointer_comparisons`) and
/// meaningless here — the behaviour a predicate encodes is what [`MenuSpec::enabled`] and the
/// cross-product test check, in every combination, which is stronger than pointer equality.
///
/// `paired` is left out for the same kind of reason: it is *layout*, not text. Two rows that print the
/// same words are the same offer whether they share a line or not, and the tests that compare menus
/// shape-for-shape ask about the offer (`tests/menus.rs`'s `labels`), never about the line box.
impl PartialEq for MenuSpec {
    fn eq(&self, other: &Self) -> bool {
        self.action == other.action
            && self.label == other.label
            && self.icon == other.icon
            && self.danger == other.danger
    }
}

// ------------------------------------------------------------------------------------------ predicates

// The plan's `when: fn(&MenuContext) -> bool`, named so the cross-product test can say which rule a
// combination broke. Each is one sentence about Go.
//
// SAFETY-of-style note: these are `fn` items, not closures, so they are nameable and comparable.

/// Always available.
fn always(_: &MenuContext) -> bool {
    true
}

/// Needs something selected (Go's `selectedPaths.size > 0` guard inside every `trigger*`).
fn selection(ctx: &MenuContext) -> bool {
    ctx.has_selection
}

/// Needs a selection *and* a writable listing. Go offered Cut/Rename/Delete on an archive anyway and
/// let the handler fail — plan §5.8's named bug.
fn selection_writable(ctx: &MenuContext) -> bool {
    ctx.has_selection && !ctx.on_archive
}

/// Needs a selection of directories only (`copy-inside`/`cut-inside` queue a directory's *children*).
fn dirs_writable(ctx: &MenuContext) -> bool {
    ctx.has_selection && ctx.all_dirs && !ctx.on_archive
}

/// Needs a clipboard and a writable destination.
fn paste_here(ctx: &MenuContext) -> bool {
    ctx.has_clipboard && !ctx.on_archive
}

/// A paste that must also have a platform that can do it: Go showed Paste Link unconditionally on
/// systems where `os.Symlink` returns EPERM and then errored (plan §5.8's second named bug).
fn paste_link_here(ctx: &MenuContext) -> bool {
    paste_here(ctx) && ctx.can_symlink
}

/// Writes into the current directory, which an archive cannot take.
fn writable(ctx: &MenuContext) -> bool {
    !ctx.on_archive
}

/// Step 14's batch rename is not on the shelf yet: the row is visible and says so by doing nothing.
/// A row inside a store: `Extract here` is the write that *is* allowed on a read-only listing.
fn archive_selection(ctx: &MenuContext) -> bool {
    ctx.on_archive && selection(ctx)
}

// ------------------------------------------------------------------------------------------- builders

/// The context menu over a row, or over the empty pane — Go's `showContextMenu`
/// (`context-menu.js:42-131`), group by group.
///
/// The outer `Vec` is the separator structure. Groups are built in Go's order, with two deliberate
/// changes the plan names: the custom actions come **before** the danger group (Go appended them after
/// Delete, `:127-130`), and Delete is split into the D6 pair.
#[must_use]
pub fn file_menu(
    ctx: &MenuContext,
    sel: &ActionContext,
    actions: &[CustomAction],
) -> Vec<Vec<MenuSpec>> {
    let custom = custom_specs(actions, sel);
    let groups = match ctx.target {
        // [`menu_for`] routes [`MenuTarget::Folder`] to [`folder_menu`], [`MenuTarget::Tab`] to
        // [`tab_menu`] and [`MenuTarget::TabGroup`] to [`group_menu`]; sharing the background shape here is
        // only what keeps this match total, not a second answer for a folder, a tab or a chip.
        MenuTarget::Background
        | MenuTarget::Folder
        | MenuTarget::Tab { .. }
        | MenuTarget::TabGroup { .. }
        // [`menu_for`] answers [`MenuTarget::Result`] with [`search_menu`] above; like the other three,
        // it never reaches here.
        | MenuTarget::Result => {
            let mut groups = vec![vec![
                item(Command::Paste, paste_here, icon_paste(ctx)),
                item(Command::PasteLink, paste_link_here, Some("link")),
            ]];
            groups.push(vec![item(Command::NewFolder, writable, Some("folder"))]);
            groups.push(custom);
            groups
        }
        MenuTarget::Row { on_dir, on_run } => {
            let rename = if ctx.in_batch {
                // Go's `triggerRename` sent a >1 selection to `batch-rename.js`
                // (`context-menu.js:404-410`); the dialog is step 14's, so the row is honest that it
                // cannot run yet rather than silently renaming one row.
                spec(
                    MenuAction::Command(Command::Rename),
                    "Rename batch".to_string(),
                    Some("edit"),
                    false,
                    selection_writable,
                )
            } else {
                item(Command::Rename, selection_writable, Some("edit"))
            };
            let mut groups: Vec<Vec<MenuSpec>> = Vec::new();
            if on_dir {
                // Go's directory block (`:46-70`), which only ever appeared for `data-dir="true"`.
                groups.push(vec![
                    item(Command::OpenInNewTab, selection, Some("plus")),
                    item(Command::CopyInside, dirs_writable, Some("copy")),
                    item(Command::CutInside, dirs_writable, Some("cut")),
                    item(Command::PasteInside, paste_here, icon_paste(ctx)),
                ]);
            }
            groups.push(vec![item(Command::Open, selection, Some("external-link"))]);
            // Design §2: the **run** verb beside the **view** verb, in one line. A directory is never
            // runnable (`classify` says so), which is why this group sits inside the same `on_run` gate
            // the directory block sits inside its `on_dir` one — and why the pair lives *below* `Open`
            // rather than in the clipboard ladder, whose rows a script has no use for.
            if on_run {
                let plain = item(Command::Execute, selection, Some("terminal"));
                // Absent, never greyed (§5.4): a refused recipe has no greyed way to succeed either.
                let sandbox = ctx
                    .can_sandbox
                    .then(|| item(Command::ExecuteSandbox, selection, Some("code")));
                // `paired` is only true when the pair really is two rows: a lone `Execute` on a
                // sandbox-less platform keeps the whole line rather than half of one.
                groups.push(match sandbox {
                    Some(sandbox) => vec![plain.paired(), sandbox],
                    None => vec![plain],
                });
            }
            groups.push(vec![
                item(Command::Cut, selection_writable, Some("cut")),
                item(Command::Copy, selection, Some("copy")),
                item(Command::CopyPath, selection, Some("document")),
                item(Command::CopyName, selection, Some("file")),
            ]);
            // Go's `Change Permissions` row sits directly under Rename (`context-menu.js:105-107`),
            // in the same block — 10b left it out, and 10c gives it the prompt it was missing.
            groups.push(vec![rename, item(Command::Chmod, selection_writable, None)]);
            // The archive group (13b's handover): `Extract here` is the write that *is* allowed on a
            // read-only listing, and Properties is the Alt+Enter row the menu never offered.
            groups.push(vec![
                item(Command::Properties, selection, Some("info")),
                item(Command::Extract, archive_selection, Some("archive")),
            ]);
            groups.push(custom);
            groups.push(vec![
                danger(Command::Delete, selection_writable),
                danger(Command::PermanentDelete, selection_writable),
            ]);
            groups
        }
    };
    shrink(groups)
}

/// The empty-space variant, named because the plan names it: [`file_menu`] with the target forced.
#[must_use]
pub fn background_menu(
    ctx: &MenuContext,
    sel: &ActionContext,
    actions: &[CustomAction],
) -> Vec<Vec<MenuSpec>> {
    let mut ctx = *ctx;
    ctx.target = MenuTarget::Background;
    file_menu(&ctx, sel, actions)
}

/// The menu a target asks for — the one place [`MenuTarget`] picks a builder, so the shell cannot
/// draw the listing menu over a breadcrumb chip and vice versa.
#[must_use]
pub fn menu_for(
    ctx: &MenuContext,
    sel: &ActionContext,
    actions: &[CustomAction],
) -> Vec<Vec<MenuSpec>> {
    match ctx.target {
        MenuTarget::Folder => folder_menu(actions, sel),
        MenuTarget::Tab { .. } => tab_menu(ctx.tab_count.max(1)),
        MenuTarget::TabGroup { collapsed, .. } => group_menu(collapsed),
        // Reached by nobody yet: a dialog's menu is built by [`crate::dialogs::ResultMenu`], which has
        // no pane to ask for a target. Here so that `MenuTarget` picks its builder in one place.
        MenuTarget::Result => search_menu(),
        MenuTarget::Background | MenuTarget::Row { .. } => file_menu(ctx, sel, actions),
    }
}

/// The breadcrumb / sidebar folder menu (Go's `showFolderContextMenu`, `context-menu.js:134-156`),
/// drawn over a [`MenuTarget::Folder`].
///
/// Go's `Copy Path` / `Copy Name` here carry *no* chord span (`:143-149`) even though the commands
/// have chords; we print the real ones (D7). Its folder-scoped custom actions (`:151-155`, which
/// execute against the clicked path rather than the selection) arrive as the `custom_specs` group —
/// filtered by Go's own `getActionsForContext(true, true, folderPath)`, and answered against
/// `folderPath` by [`crate::app::ZenApp`] rather than against the pane's selection.
#[must_use]
pub fn folder_menu(actions: &[CustomAction], sel: &ActionContext) -> Vec<Vec<MenuSpec>> {
    let groups = vec![
        vec![item(Command::OpenInNewTab, always, Some("plus"))],
        vec![
            item(Command::CopyPath, always, Some("document")),
            item(Command::CopyName, always, Some("file")),
        ],
        custom_specs(actions, sel),
    ];
    shrink(groups)
}

/// The tab-strip menu (Go's `showTabContextMenu`, `tabs.js:351-386`), group by group.
///
/// Go's own shape, and the row order it prints: `Close tab`, `Duplicate tab` (`tabs.js:365-367`), the two
/// copy rows, then the tab-group block (`tabs.js:353-359,378-381`) — which used to be six greyed rows
/// and is now the feature: one row to start a group from the selection (and from one right-clicked tab:
/// that is how a group begins), one to take it back out, and the palette's nine colours, Go's
/// `Red Group`… wording kept because that is what the reference prints.
#[must_use]
pub fn tab_menu(count: usize) -> Vec<Vec<MenuSpec>> {
    let mut colours: Vec<MenuSpec> = GroupColor::ALL
        .iter()
        .map(|color| {
            spec(
                MenuAction::Group(GroupVerb::NewColor(*color)),
                color.label(),
                None,
                false,
                always,
            )
        })
        .collect();
    colours.insert(
        0,
        spec(
            MenuAction::Group(GroupVerb::New),
            // A single tab is a group too (that is how you start one), but a row that grouped three
            // tabs and one that grouped one are different offers, so the row says which it is.
            if count > 1 {
                format!("New Tab Group with {count} Tabs")
            } else {
                "New Tab Group".to_string()
            },
            Some("plus"),
            false,
            always,
        ),
    );
    shrink(vec![
        vec![item(Command::CloseTab, always, Some("close"))],
        vec![item(Command::DuplicateTab, always, Some("plus"))],
        vec![
            item(Command::CopyPath, always, Some("document")),
            item(Command::CopyName, always, Some("file")),
        ],
        colours,
        vec![spec(
            MenuAction::Group(GroupVerb::Leave),
            "Remove Group".to_string(),
            None,
            false,
            in_group,
        )],
    ])
}

/// A tab group's own menu, opened over its chip: the three verbs of `docs/tab-groups-design.md` §1
/// (rename, ungroup, wipe) plus the fold, "select the run", and the same nine colours, which **move the
/// group** here rather than the tabs (design §6) — and merge it into whichever group already wears the
/// colour picked.
#[must_use]
pub fn group_menu(collapsed: bool) -> Vec<Vec<MenuSpec>> {
    let mut out = vec![
        vec![spec(
            MenuAction::Group(GroupVerb::Rename),
            "Rename Group".to_string(),
            None,
            false,
            always,
        )],
        vec![spec(
            MenuAction::Group(GroupVerb::ToggleCollapse),
            if collapsed {
                "Expand Group".to_string()
            } else {
                "Collapse Group".to_string()
            },
            None,
            false,
            always,
        )],
        vec![spec(
            MenuAction::Group(GroupVerb::Select),
            "Select Tabs in Group".to_string(),
            None,
            false,
            always,
        )],
    ];
    let mut colours: Vec<MenuSpec> = GroupColor::ALL
        .iter()
        .map(|color| {
            spec(
                MenuAction::Group(GroupVerb::Color(*color)),
                color.label(),
                None,
                false,
                always,
            )
        })
        .collect();
    colours.push(spec(
        MenuAction::Group(GroupVerb::Ungroup),
        "Ungroup".to_string(),
        None,
        false,
        always,
    ));
    out.push(colours);
    out.push(vec![spec(
        MenuAction::Group(GroupVerb::Close),
        "Close Tabs in Group".to_string(),
        Some("close"),
        true,
        always,
    )]);
    shrink(out)
}

/// The tab menu's facts, and nothing else (see [`MenuContext::bits`] for why they are apart from the
/// listing's seven).
fn in_group(ctx: &MenuContext) -> bool {
    ctx.in_group
}

/// A search-result menu (Go's `showSearchResultContextMenu`, `context-menu.js:173-197`), which is what
/// **both** search dialogs answer a right-click with: the search manager raises it straight
/// (`search-manager.js:378-389`) and the find dialog used to skip it — Go's `quick-find.js:321-325` went
/// to `executeItemAction(index, 'reveal')` — but a two-row answer to the manager's five makes the same
/// gesture mean two things, so the port gave the find dialog the manager's menu instead of the reverse.
///
/// No row is greyed: every one acts on the row's own path, which always exists and is never read-only
/// in the way a listing's Delete is. The `… in New Tab` twin keeps Go's adjacency (`:182-183`, no
/// separator between the three) and the copy pair keeps its (`:185`).
#[must_use]
pub fn search_menu() -> Vec<Vec<MenuSpec>> {
    shrink(vec![
        vec![
            result_item(ResultVerb::Open, Some("external-link")),
            result_item(ResultVerb::Reveal, Some("folder-open")),
            result_item(ResultVerb::RevealInNewTab, Some("plus")),
        ],
        vec![
            result_item(ResultVerb::CopyPath, Some("document")),
            result_item(ResultVerb::CopyName, Some("file")),
        ],
    ])
}

/// One of [`search_menu`]'s rows. `MenuTarget::Result`, so a row can never be read as a row of the
/// listing behind the dialog.
fn result_item(verb: ResultVerb, icon: Option<&'static str>) -> MenuSpec {
    spec(
        MenuAction::Result(verb),
        verb.label().to_string(),
        icon,
        false,
        always,
    )
}

/// The custom-action group: `services::actions::for_context`, the same predicate the action
/// editor's applicability preview uses. 10b's promised handover — the placeholder role-only filter
/// and its private `role_matches` are gone, so the menu and the engine cannot disagree.
#[must_use]
pub fn custom_specs(actions: &[CustomAction], sel: &ActionContext) -> Vec<MenuSpec> {
    actions::for_context(actions, sel)
        .into_iter()
        .map(|action| {
            spec(
                MenuAction::Custom(action.clone()),
                action.name.clone(),
                None,
                false,
                always,
            )
        })
        .collect()
}

/// A command row, danger-free.
fn item(command: Command, when: fn(&MenuContext) -> bool, icon: Option<&'static str>) -> MenuSpec {
    spec(
        MenuAction::Command(command),
        command.label().to_string(),
        icon,
        false,
        when,
    )
}

/// A red row — Go's single `style="color: var(--danger-color)"` (`context-menu.js:99`).
fn danger(command: Command, when: fn(&MenuContext) -> bool) -> MenuSpec {
    spec(
        MenuAction::Command(command),
        command.label().to_string(),
        Some("trash"),
        true,
        when,
    )
}

fn spec(
    action: MenuAction,
    label: String,
    icon: Option<&'static str>,
    danger: bool,
    when: fn(&MenuContext) -> bool,
) -> MenuSpec {
    MenuSpec {
        action,
        label,
        icon,
        danger,
        when,
        paired: false,
    }
}

/// The Paste group's glyph follows the clipboard's own op, which is the one thing
/// [`MenuContext::clip_is_cut`] is good for visually (Go spelled it in the *status bar* instead,
/// `file-list.js`'s `updateClipboardUI`).
#[must_use]
pub const fn icon_paste(ctx: &MenuContext) -> Option<&'static str> {
    if ctx.clip_is_cut {
        Some("cut")
    } else {
        Some("paste")
    }
}

/// Drop the groups an empty selection left behind, so a menu never starts or ends on a rule.
#[must_use]
fn shrink(groups: Vec<Vec<MenuSpec>>) -> Vec<Vec<MenuSpec>> {
    groups
        .into_iter()
        .filter(|group| !group.is_empty())
        .collect()
}

// ------------------------------------------------------------------------------------------ rendering

/// What [`show`] decided this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MenuOutcome {
    /// The frame's rectangle, in screen points — what [`crate::app::ZenApp`] hit-tests for
    /// click-away, and what a test asserts the menu really landed where it was asked.
    pub rect: Rect,
    /// A press outside [`Self::rect`]: the menu is dismissed and fires nothing.
    pub close: bool,
}

/// Draw `groups` with its top-left at `at`, on the **foreground** layer, and report the clicks.
///
/// Layering is the whole game here (plan §9.2 step 8's rule: in this egui fork the *last-allocated*
/// overlapping rect wins hover and click): the area is `Order::Foreground`, which is above the pane's
/// `ui.interact` rows, and the caller must draw it **after** every other panel and after the toasts —
/// [`crate::app::ZenApp::ui`] does exactly that. Inside, one row is one interact rect, allocated by
/// [`Ui::allocate_exact_size`], and every glyph and glyph of text in it is *painted* (`Painter`), so
/// nothing allocates over it.
///
/// The position is set with [`Area::fixed_pos`], **not** [`Area::default_pos`]: an [`Area`] keeps the
/// position of a known id in memory, and this fork's [`Id::unique`] is a stable hash of its source and
/// not a per-call counter, so `default_pos` is read on the *first* opening of the session only — every
/// later menu then drew at the first one's spot (the parity defect this function's `at` exists to
/// avoid). `fixed_pos` overrides the stored position every frame and keeps `constrain(true)`, so the
/// menu still cannot leave the screen.
///
/// What a menu is drawn with — the two things a row needs beyond its own spec, kept together so
/// [`show`] stays under the argument ceiling: the chord table the hints come from (D7) and the token
/// table the colours and the geometry come from.
pub struct MenuStyle<'a> {
    /// The chord table every hint is read out of.
    pub keys: &'a KeyMap,
    /// The design tokens: every colour and every point measurement.
    pub tokens: &'a Tokens,
}

/// The popup's border, in points: the frame's stroke, and part of how far a row's corner sits inside
/// the frame's rounded one ([`hover_corners`]).
const FRAME_BORDER: u8 = 1;

/// A popup's own box, measured off the row text **before** any row is drawn.
///
/// The measurement has to come first, because an `Area` sizes itself to its content: left to itself
/// the first (sizing) pass runs at egui's `spacing.default_area_size` — 600 pt — every row then paints
/// all of it, and the frame is frozen at that width for as long as it lives, [`Ui::set_min_width`]
/// notwithstanding. That is why the rows take their width from here rather than from
/// `available_width`, and why the area is *seeded* with it (`Area::default_size`): the popup hugs the
/// widest label + chord pair from its very first frame.
#[derive(Clone, Copy)]
struct Geometry {
    /// The content width: Go's 180 pt floor (`context-menu.css:8`), the widest row, or
    /// [`Tokens::menu_max_width`], whichever is smallest.
    width: f32,
    /// The icon column every row reserves — the whole menu's, not the row's own, so a label never
    /// shifts sideways depending on whether *its* item has a glyph.
    icon_column: f32,
    /// Rows + rules + frame padding: the height the sizing pass should assume.
    height: f32,
}

impl Geometry {
    /// The area's seed size: the content box plus the frame's border on both sides of it.
    fn seed(&self) -> Vec2 {
        Vec2::new(
            self.width + 2.0 * f32::from(FRAME_BORDER),
            self.height + 2.0 * f32::from(FRAME_BORDER),
        )
    }
}

#[must_use]
pub fn show(
    ctx: &egui::Context,
    id: Id,
    at: Pos2,
    menu_ctx: &MenuContext,
    groups: &[Vec<MenuSpec>],
    style: MenuStyle<'_>,
    out: &mut Vec<MenuAction>,
) -> MenuOutcome {
    let MenuStyle { keys, tokens } = style;
    // Measured with the popup layer's own painter: the rows are not laid out yet, so this is the only
    // moment the text can be sized. Same fonts, same layer, so the numbers cannot disagree with
    // whatever [`row`] later paints.
    let geometry = geometry(
        &ctx.layer_painter(LayerId::new(Order::Foreground, id)),
        groups,
        menu_ctx,
        keys,
        tokens,
    );
    let area = Area::new(id)
        .order(Order::Foreground)
        .movable(false)
        .default_size(geometry.seed())
        .fixed_pos(at)
        .constrain(true)
        .show(ctx, |ui| {
            Frame::new()
                .fill(tokens.bg_sidebar)
                .stroke(Stroke::new(f32::from(FRAME_BORDER), tokens.border))
                .corner_radius(CornerRadius::same(tokens.menu_corner_radius))
                .inner_margin(Margin::symmetric(0, tokens.menu_frame_pad_y as i8))
                .shadow(Shadow {
                    offset: [0, tokens.menu_shadow_offset_y],
                    blur: tokens.menu_shadow_blur,
                    spread: 0,
                    color: tokens.menu_shadow,
                })
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing = Vec2::ZERO;
                    ui.set_min_width(geometry.width);
                    draw_lines(ui, groups, menu_ctx, keys, tokens, geometry, out);
                });
        });
    let rect = area.response.rect;
    let close = ctx.input(|input| {
        input.pointer.any_pressed()
            && input
                .pointer
                .interact_pos()
                .is_none_or(|pos| !rect.contains(pos))
    });
    MenuOutcome { rect, close }
}

/// `true` when the spec at index `i` of `group` is the **left** half of a pair: it asked to be paired
/// and there is a spec after it to pair with. The one definition, used by both the measurement and the
/// draw, so the two can never disagree about what a line is — and the rule that downgrades a `paired`
/// last-row-in-a-group (a builder bug) to an ordinary row instead of losing it.
fn is_pair(group: &[MenuSpec], i: usize) -> bool {
    group[i].paired && i + 1 < group.len()
}

/// The lines a group list really paints: a pair is **one** line, not two. [`hover_corners`] is indexed
/// by this, which is what makes both halves light up together and round together.
fn line_count(groups: &[Vec<MenuSpec>]) -> usize {
    groups
        .iter()
        .map(|group| (0..group.len()).filter(|&i| !is_pair(group, i)).count())
        .sum()
}

/// One row's own natural width: the icon column, the label, the hint it will actually print, and the
/// horizontal padding on **both** of its edges.
fn natural_width(
    painter: &Painter,
    spec: &MenuSpec,
    menu_ctx: &MenuContext,
    keys: &KeyMap,
    tokens: &Tokens,
    icon_column: f32,
) -> f32 {
    let label = painter
        .layout_job(singleline(
            &spec.label,
            tokens.font_size,
            tokens.text_main,
            f32::INFINITY,
        ))
        .size()
        .x;
    let hint = spec
        .enabled(menu_ctx)
        .then(|| spec.hint(keys))
        .flatten()
        .map_or(0.0, |hint| {
            painter
                .layout_job(singleline(
                    &hint,
                    tokens.menu_hint_font_size,
                    tokens.text_muted,
                    f32::INFINITY,
                ))
                .size()
                .x
                + tokens.menu_hint_gap
        });
    icon_column + label + hint + 2.0 * tokens.menu_item_pad_x
}

/// Every row of the popup, measured — see [`Geometry`] for why this happens before the draw.
fn geometry(
    painter: &Painter,
    groups: &[Vec<MenuSpec>],
    menu_ctx: &MenuContext,
    keys: &KeyMap,
    tokens: &Tokens,
) -> Geometry {
    let icon_column = if groups.iter().flatten().any(|spec| spec.icon.is_some()) {
        tokens.row_icon_size + tokens.menu_icon_gap
    } else {
        0.0
    };
    let mut width = tokens.menu_min_width;
    let mut rows = 0_usize;
    let mut rules = 0_usize;
    for group in groups {
        if group.is_empty() {
            continue;
        }
        rules += usize::from(rows > 0);
        let mut i = 0;
        while i < group.len() {
            // A pair contributes **one** row to the height and asks for **two** half-rows of width —
            // each half with its own label + hint budget and its own padding on both of its edges, so
            // the line is twice the wider half rather than the sum of the two. (One pad per half would
            // leave the label `menu_item_pad_x` short of what [`paint_row`] then budgets for it, which
            // is how `Execute (sandbox)` would come to wrap onto a second line inside a 26 pt row.)
            rows += 1;
            let left = natural_width(painter, &group[i], menu_ctx, keys, tokens, icon_column);
            if is_pair(group, i) {
                let right =
                    natural_width(painter, &group[i + 1], menu_ctx, keys, tokens, icon_column);
                width = width.max(2.0 * left.max(right));
                i += 2;
            } else {
                width = width.max(left);
                i += 1;
            }
        }
    }
    Geometry {
        // Never the whole screen: an over-long custom-action name wraps into the label budget instead
        // of pushing the frame off the pane it belongs to.
        width: width.min(tokens.menu_max_width),
        icon_column,
        height: rows as f32 * tokens.menu_item_height
            + rules as f32 * (1.0 + 2.0 * f32::from(tokens.menu_separator_gap))
            + 2.0 * f32::from(tokens.menu_frame_pad_y),
    }
}

/// The rounded corners of one row's hover fill.
///
/// The frame is rounded (`context-menu.css:6-9`), the rows are not: a row that touches a corner may
/// only round away what `inset` (the border and the frame's vertical padding, so how far inside the
/// frame's outer edge its own edge is) has not already eaten of the arc. A square fill where the arc
/// still curves would paint a hard corner over the frame's rounding; a fill rounded *past* the arc
/// leaves a nick of bare frame colour at the popup's corner.
#[must_use]
fn hover_corners(index: usize, total: usize, radius: u8, inset: u8) -> CornerRadius {
    let r = radius.saturating_sub(inset);
    let (nw, ne) = if index == 0 { (r, r) } else { (0, 0) };
    let (sw, se) = if index + 1 == total { (r, r) } else { (0, 0) };
    CornerRadius { nw, ne, sw, se }
}

/// Which of a row's corners belong to the **left** half of a pair (design §2).
///
/// The row's rounding belongs to the two rects that now span its width: the pair is one line, so the
/// line's own corners are real, but they sit on the *outer* edges — the left rect owns the west pair
/// and the right rect the east pair. Painting the full set on both halves would round the seam in the
/// middle of the line, which is where the two hover fills meet.
#[must_use]
fn half_corners(full: CornerRadius, left: bool) -> CornerRadius {
    if left {
        CornerRadius {
            ne: 0,
            se: 0,
            ..full
        }
    } else {
        CornerRadius {
            nw: 0,
            sw: 0,
            ..full
        }
    }
}

/// Draw every row of the open popup, one **line** at a time: an ordinary row owns its line, and a
/// `paired` spec shares one with the spec that follows it (design §2's `Execute` pair).
///
/// Split out of [`show`] because this loop *is* the popup's vertical layout, and the pair's "one line,
/// not two" rule has to reach both the corner arithmetic — which line is the first, which is the last
/// ([`hover_corners`]) — and the drawing. Kept in one function, the two cannot disagree.
#[allow(clippy::too_many_arguments)]
fn draw_lines(
    ui: &mut Ui,
    groups: &[Vec<MenuSpec>],
    menu_ctx: &MenuContext,
    keys: &KeyMap,
    tokens: &Tokens,
    geometry: Geometry,
    out: &mut Vec<MenuAction>,
) {
    // A row's top or bottom edge is `FRAME_BORDER + menu_frame_pad_y` inside the frame's own edge,
    // which is as deep as the corner arc reaches: today's tokens leave a hovered end row exactly
    // `radius - inset = 0` to round.
    let inset = FRAME_BORDER + tokens.menu_frame_pad_y;
    let total = line_count(groups);
    let mut drawn = 0_usize;
    let mut first = true;
    for group in groups.iter().filter(|group| !group.is_empty()) {
        // The rule goes *between* two groups: an empty leading group must not buy the popup a rule
        // across its own top edge.
        if !first {
            separator(ui, tokens, geometry.width);
        }
        first = false;
        let mut ix = 0_usize;
        while ix < group.len() {
            let corners = hover_corners(drawn, total, tokens.menu_corner_radius, inset);
            if is_pair(group, ix) {
                row_pair(
                    ui,
                    &group[ix],
                    &group[ix + 1],
                    menu_ctx,
                    keys,
                    tokens,
                    geometry,
                    corners,
                    out,
                );
                ix += 2;
            } else {
                row(
                    ui, &group[ix], menu_ctx, keys, tokens, geometry, corners, out,
                );
                ix += 1;
            }
            // One *line*, which is what `total` counted: both halves of a pair map to the same index.
            drawn += 1;
        }
    }
}

/// The pixels of one row, inside the rect its caller allocated — the body [`row`] and each half of
/// [`row_pair`] share, so a paired row cannot drift from a whole one in colour, in the icon column or
/// in how much of its label it is allowed to print.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn paint_row(
    ui: &mut Ui,
    rect: Rect,
    spec: &MenuSpec,
    enabled: bool,
    hovered: bool,
    keys: &KeyMap,
    tokens: &Tokens,
    geometry: Geometry,
    corners: CornerRadius,
) {
    let hint = spec.hint(keys);
    if !ui.is_rect_visible(rect) {
        return;
    }
    let hovered = enabled && hovered;
    if hovered {
        ui.painter().rect_filled(rect, corners, tokens.bg_hover);
    }
    let colour = if !enabled {
        tokens.disabled_text
    } else if spec.danger {
        tokens.danger
    } else if hovered {
        tokens.text_active
    } else {
        tokens.text_main
    };
    let mut x = rect.min.x + tokens.menu_item_pad_x;
    if let Some(icon) = spec.icon {
        let side = tokens.row_icon_size;
        icons::paint(
            ui,
            // Centred *on* the row's mid-line: `Rect::from_min_size` at that y hangs the whole
            // glyph below it, which is what made the icon look like it had sunk out of the text.
            Rect::from_center_size(
                Pos2::new(x + side / 2.0, rect.center().y),
                Vec2::splat(side),
            ),
            icon,
            side,
            colour,
        );
    }
    // The column is the menu's, not the row's: a glyphless item's label lines up with its
    // neighbours' instead of starting half an icon early.
    x += geometry.icon_column;
    let budget = rect.max.x - x - tokens.menu_item_pad_x;
    let hint_width = hint.as_ref().map_or(0.0, |hint| {
        ui.painter()
            .layout_job(singleline(
                hint,
                tokens.menu_hint_font_size,
                tokens.text_muted,
                f32::INFINITY,
            ))
            .size()
            .x
    });
    // A row with no hint of its own must not be charged the gap: `hint_width` is 0.0 for a custom
    // action and for a tab-group verb alike, and the old test (`0 + gap <= budget`) was therefore
    // always true — it squeezed 24 pt out of a label that had nothing to sit against, which is what
    // wrapped `Select Tabs in Group` onto the row below it.
    let show_hint = hint.is_some() && enabled && hint_width + tokens.menu_hint_gap <= budget;
    let label_width = budget
        - if show_hint {
            hint_width + tokens.menu_hint_gap
        } else {
            0.0
        };
    let galley = ui.painter().layout_job(singleline(
        &spec.label,
        tokens.font_size,
        colour,
        label_width.max(0.0),
    ));
    ui.painter().galley(
        Pos2::new(x, rect.center().y - galley.size().y / 2.0),
        galley,
        colour,
    );
    if let Some(hint) = hint.as_deref().filter(|_| show_hint) {
        let galley = ui.painter().layout_job(singleline(
            hint,
            tokens.menu_hint_font_size,
            tokens.text_muted,
            f32::INFINITY,
        ));
        ui.painter().galley(
            Pos2::new(
                rect.max.x - tokens.menu_item_pad_x - galley.size().x,
                rect.center().y - galley.size().y / 2.0,
            ),
            galley,
            tokens.text_muted,
        );
    }
}

/// The accessibility node and the click, reported for a whole row and for a pair's half in the same
/// words: `Role::Button`, labelled with the menu text, the chord in the `hint_text` slot so a screen
/// reader reads the shortcut too — and a greyed row that fires nothing.
///
/// This is the seam kittest's `get_by_label("Execute")` walks, so both halves of a pair are *their own*
/// node with their own rect, not one node for the line.
fn report(
    response: &Response,
    spec: &MenuSpec,
    enabled: bool,
    keys: &KeyMap,
    out: &mut Vec<MenuAction>,
) {
    let hint = spec.hint(keys);
    let label = spec.label.clone();
    response.widget_info(|| {
        let mut info = WidgetInfo::new(egui::Role::Button);
        info.label = Some(label.clone());
        info.enabled = enabled;
        info.hint_text = hint.clone();
        info
    });
    if enabled && response.clicked() {
        out.push(spec.action.clone());
    }
}

/// One item: the interact rect first (so the row owns its own click), the pixels after.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn row(
    ui: &mut Ui,
    spec: &MenuSpec,
    menu_ctx: &MenuContext,
    keys: &KeyMap,
    tokens: &Tokens,
    geometry: Geometry,
    corners: CornerRadius,
    out: &mut Vec<MenuAction>,
) {
    let enabled = spec.enabled(menu_ctx);
    // The measured popup width, never `available_width` — see [`Geometry`].
    let size = Vec2::new(geometry.width, tokens.menu_item_height);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    paint_row(
        ui,
        rect,
        spec,
        enabled,
        response.hovered(),
        keys,
        tokens,
        geometry,
        corners,
    );
    report(&response, spec, enabled, keys, out);
}

/// The pair: `left` and `right` in one line of `menu_item_height`, each owning half the popup's width
/// (design §2 — the two run rows must not cost two lines).
///
/// [`Ui::allocate_exact_size`] advances on the **y** axis, so calling `row` twice would be two rows: the
/// line is allocated once, at the measured popup width, and the two rects are then *placed* inside it
/// with [`Ui::interact`], which registers each as its own widget — hover, click and the a11y node
/// ([`report`]) included, which is what `tests/menus.rs` clicks. This is the documented fallback to
/// [`Ui::put`], whose signature in this egui takes an [`egui::Widget`] rather than an [`egui::Sense`].
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn row_pair(
    ui: &mut Ui,
    left: &MenuSpec,
    right: &MenuSpec,
    menu_ctx: &MenuContext,
    keys: &KeyMap,
    tokens: &Tokens,
    geometry: Geometry,
    corners: CornerRadius,
    out: &mut Vec<MenuAction>,
) {
    let height = tokens.menu_item_height;
    // One line of the vertical layout, and the id both halves are salted from — the same pair of
    // steps `allocate_exact_size` performs, split so the two rects can sit side by side.
    let (id, line) = ui.allocate_space(Vec2::new(geometry.width, height));
    let half = geometry.width / 2.0;
    for (ix, spec) in [(0_usize, left), (1, right)] {
        let enabled = spec.enabled(menu_ctx);
        // The two halves tile the line *exactly*: the right one ends on the line's own edge rather
        // than on `min.x + 2 * (width / 2)`, which a float can land a hair short of, and a half a
        // point of bare frame between the two hover fills is exactly the kind of seam a user sees.
        let min = Pos2::new(line.min.x + ix as f32 * half, line.min.y);
        let max = if ix == 0 {
            Pos2::new(min.x + half, line.max.y)
        } else {
            line.max
        };
        let rect = Rect::from_min_max(min, max);
        let response = ui.interact(rect, id.with(ix), egui::Sense::click());
        paint_row(
            ui,
            rect,
            spec,
            enabled,
            response.hovered(),
            keys,
            tokens,
            geometry,
            half_corners(corners, ix == 0),
        );
        report(&response, spec, enabled, keys, out);
    }
}

/// The rule between two groups (`context-menu.css:29-33`: 1 px of `--border-color`, `margin: 5px 0`).
fn separator(ui: &mut Ui, tokens: &Tokens, width: f32) {
    let height = 1.0 + 2.0 * f32::from(tokens.menu_separator_gap);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), egui::Sense::hover());
    ui.painter()
        .hline(rect.x_range(), rect.center().y, tokens.hairline());
}

/// A one-line text job. `wrap_width` is the *label* budget: an over-long custom-action name wraps
/// rather than painting over the frame's border, which is the honest degradation.
fn singleline(text: &str, size: f32, colour: Color32, wrap_width: f32) -> LayoutJob {
    LayoutJob::simple(
        text.to_string(),
        egui::FontId::proportional(size),
        colour,
        wrap_width,
    )
}

#[cfg(test)]
mod tests {
    //! The plan §7 row 10 criterion lives here: **every** `MenuContext` combination is asserted, by
    //! walking the 2^7 cross-product of the boolean fields against an *independently written*
    //! expectation ([`expect`]) — a second formulation of Go's guards, not a restatement of the
    //! builders. On top of it, the named per-command invariants, and a literal snapshot of one row
    //! per distinct menu shape so a regression names the combination it broke.

    use super::*;
    use crate::keymap::KeyMap;
    use zenman_core::path::AbsPath;

    /// The eight listing booleans in every combination, `bit 0` = `has_selection` … `bit 7` =
    /// `can_sandbox`.
    fn all_bits() -> impl Iterator<Item = [bool; 8]> {
        (0..256u16).map(|n| {
            [
                n & 1 != 0,
                n & 2 != 0,
                n & 4 != 0,
                n & 8 != 0,
                n & 16 != 0,
                n & 32 != 0,
                n & 64 != 0,
                n & 128 != 0,
            ]
        })
    }

    fn at(bits: u16, target: MenuTarget) -> MenuContext {
        at_bits(into_bits(bits), target)
    }

    fn at_bits(bits: [bool; 8], target: MenuTarget) -> MenuContext {
        let mut ctx = MenuContext::default();
        ctx.with_bits(bits);
        ctx.target = target;
        ctx
    }

    /// Every target shape the file menu answers, so one walk covers `4 × 256` contexts. The two
    /// `on_run` rows are design §2's: a runnable file is the only shape the `Execute` pair appears on,
    /// and a directory never is ([`crate::app::ZenApp::open_requested_menu`] will not even ask
    /// `classify` for one).
    const TARGETS: [MenuTarget; 4] = [
        MenuTarget::Background,
        MenuTarget::Row {
            on_dir: true,
            on_run: false,
        },
        MenuTarget::Row {
            on_dir: false,
            on_run: false,
        },
        MenuTarget::Row {
            on_dir: false,
            on_run: true,
        },
    ];

    /// An **independent** answer for one command in one context: `None` = the row is not in this
    /// menu at all, `Some(enabled)` = it is. Written from Go's own guards (`context-menu.js`'s
    /// `isItem`/`isDir`/`hasClipboard` branches and the `selectedPaths.size > 0` guard inside every
    /// `trigger*`), never by calling a builder.
    fn expect(command: Command, ctx: &MenuContext) -> Option<bool> {
        let background = ctx.target == MenuTarget::Background;
        let on_dir = matches!(ctx.target, MenuTarget::Row { on_dir: true, .. });
        // Design §2: a fact about the clicked row, spelled here the same independent way `on_dir` is
        // — read off the *target*, never off a builder.
        let on_run = matches!(ctx.target, MenuTarget::Row { on_run: true, .. });
        let writable = !ctx.on_archive;
        match command {
            Command::Open => (!background).then_some(ctx.has_selection),
            // Design §2's pair, re-derived from the prose rather than from `file_menu`: `Execute` is
            // over a runnable row and nothing else, and both rows obey Go's `selectedPaths.size > 0`
            // guard because both act on the whole selection. The sandbox twin additionally needs the
            // measured sandbox probe (§5.4) — and is *absent*, never greyed, without it.
            Command::Execute => on_run.then_some(ctx.has_selection),
            Command::ExecuteSandbox => (on_run && ctx.can_sandbox).then_some(ctx.has_selection),
            Command::OpenInNewTab => (!background && on_dir).then_some(ctx.has_selection),
            Command::CopyInside | Command::CutInside => {
                (!background && on_dir).then_some(ctx.has_selection && ctx.all_dirs && writable)
            }
            Command::PasteInside => {
                (!background && on_dir).then_some(ctx.has_clipboard && writable)
            }
            Command::Paste => background.then_some(ctx.has_clipboard && writable),
            Command::PasteLink => {
                background.then_some(ctx.has_clipboard && writable && ctx.can_symlink)
            }
            Command::NewFolder => background.then_some(writable),
            Command::Cut | Command::Delete | Command::PermanentDelete => {
                (!background).then_some(ctx.has_selection && writable)
            }
            // Go's >1-selection rename is the batch dialog (`context-menu.js:404-410`), and 14b
            // gave it a real `in_batch`: the row is Rename either way, and it is live whenever a
            // selection is — the *label* is what changes, not the predicate.
            Command::Rename => (!background).then_some(ctx.has_selection && writable),
            // 13b's handover: Properties is the Alt+Enter row, and `Extract here` is the one write
            // a read-only store does allow.
            Command::Properties => (!background).then_some(ctx.has_selection),
            Command::Extract => (!background).then_some(ctx.has_selection && ctx.on_archive),
            Command::Copy | Command::CopyPath | Command::CopyName => {
                (!background).then_some(ctx.has_selection)
            }
            // Go's `Change Permissions` row (`context-menu.js:105-107`) sits with Rename and needs the
            // same thing: a selection whose mode to change.
            Command::Chmod => (!background).then_some(ctx.has_selection && writable),
            _ => None,
        }
    }

    /// A context whose selection is one ordinary file — what `for_context` needs to test the
    /// `patterns` half at all (a *background* context skips the pattern test, exactly as Go's does).
    /// A context whose selection is one directory row (Go's `isDir` half of the pattern test).
    fn dir_row() -> ActionContext {
        ActionContext::rows(
            None,
            vec![AbsPath::from_path("/tree/docs").expect("a constant absolute path")],
            true,
        )
    }

    fn file_row() -> ActionContext {
        ActionContext::rows(
            None,
            vec![AbsPath::from_path("/tree/a.txt").expect("a constant absolute path")],
            false,
        )
    }

    /// The snapshot spelling: `Label+` for enabled, `Label-` for greyed, groups separated by ` | `.
    fn shape(groups: &[Vec<MenuSpec>], ctx: &MenuContext) -> String {
        groups
            .iter()
            .map(|group| {
                group
                    .iter()
                    .map(|spec| {
                        format!(
                            "{}{}",
                            spec.label,
                            if spec.enabled(ctx) { "+" } else { "-" }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .collect::<Vec<_>>()
            .join(" | ")
    }

    #[test]
    fn every_menu_context_combination_matches_the_independent_expectation() {
        let mut checked = 0usize;
        for bits in all_bits() {
            for target in TARGETS {
                let ctx = at_bits(bits, target);
                let groups = file_menu(&ctx, &ActionContext::background(None), &[]);
                assert!(
                    groups.iter().all(|group| !group.is_empty()),
                    "no group is ever empty (an empty group is a dangling rule): {ctx:?}"
                );
                let mut seen: Vec<Command> = Vec::new();
                for spec in groups.iter().flatten() {
                    let command = spec
                        .command()
                        .unwrap_or_else(|| panic!("no custom action was passed: {spec:?}"));
                    let text = format!("{command:?} in {ctx:?}");
                    match expect(command, &ctx) {
                        None => panic!("{command:?} must not appear in {ctx:?}"),
                        Some(enabled) => assert_eq!(spec.enabled(&ctx), enabled, "{text}"),
                    }
                    assert!(
                        !seen.contains(&command),
                        "{command:?} appears twice in {ctx:?}"
                    );
                    seen.push(command);
                    checked += 1;
                }
            }
        }
        // 3 targets × 128 combinations, each with its own row count: the guard against a walk that
        // silently compared nothing.
        assert!(checked > 1_000, "only {checked} specs were checked");
    }

    #[test]
    fn the_named_invariants_hold_in_every_combination() {
        for bits in all_bits() {
            for target in TARGETS {
                let ctx = at_bits(bits, target);
                let groups = file_menu(&ctx, &ActionContext::background(None), &[]);
                let enabled = |command: Command| {
                    groups
                        .iter()
                        .flatten()
                        .any(|spec| spec.command() == Some(command) && spec.enabled(&ctx))
                };
                let present = |command: Command| {
                    groups
                        .iter()
                        .flatten()
                        .any(|spec| spec.command() == Some(command))
                };
                // Paste is the clipboard, and nothing but — and it lives in the background menu,
                // while its `… Inside` twin lives in the directory block of the row menu.
                assert_eq!(
                    enabled(Command::Paste),
                    matches!(target, MenuTarget::Background)
                        && ctx.has_clipboard
                        && !ctx.on_archive
                );
                assert_eq!(
                    enabled(Command::PasteInside),
                    matches!(ctx.target, MenuTarget::Row { on_dir: true, .. })
                        && ctx.has_clipboard
                        && !ctx.on_archive
                );
                // The destructive rows need a selection, and an archive cannot be destroyed (Go
                // offered Delete over a read-only VFS and let the handler fail: plan §5.8).
                assert_eq!(
                    enabled(Command::Delete),
                    ctx.has_selection
                        && !ctx.on_archive
                        && !matches!(target, MenuTarget::Background)
                );
                assert_eq!(enabled(Command::PermanentDelete), enabled(Command::Delete));
                assert_eq!(enabled(Command::Cut), enabled(Command::Delete));
                // 14b: the batch row is the *same* command and the same predicate; only its label
                // changes, which is what `in_batch` is for now that it has a real source.
                assert_eq!(enabled(Command::Rename), enabled(Command::Delete));
                // Reading is never refused: Copy/Copy path/Copy name work inside an archive.
                if matches!(target, MenuTarget::Row { .. }) {
                    assert_eq!(enabled(Command::Copy), ctx.has_selection);
                    assert_eq!(enabled(Command::CopyPath), ctx.has_selection);
                    assert_eq!(enabled(Command::CopyName), ctx.has_selection);
                    assert_eq!(enabled(Command::Open), ctx.has_selection);
                }
                // A symlink the platform cannot make is never offered enabled.
                assert_eq!(
                    enabled(Command::PasteLink),
                    matches!(target, MenuTarget::Background)
                        && ctx.has_clipboard
                        && !ctx.on_archive
                        && ctx.can_symlink
                );
                // Contents-level clipboard work is a directory thing.
                assert_eq!(
                    enabled(Command::CopyInside),
                    matches!(ctx.target, MenuTarget::Row { on_dir: true, .. })
                        && ctx.has_selection
                        && ctx.all_dirs
                        && !ctx.on_archive
                );
                // A greyed row is still drawn: nothing vanishes because it is disabled.
                for command in [Command::Delete, Command::Paste, Command::NewFolder] {
                    if expect(command, &ctx).is_some() {
                        assert!(present(command), "{command:?} vanished from {ctx:?}");
                    }
                }
                run_pair(&groups, &ctx);
                // No command outside the file menu's vocabulary crept in.
                for spec in groups.iter().flatten() {
                    let command = spec.command().expect("built with no custom actions");
                    assert!(
                        expect(command, &ctx).is_some(),
                        "{command:?} is not part of the file menu: {ctx:?}"
                    );
                }
            }
        }
    }

    /// Design §2's run pair, as its own invariant so the walk above keeps its complexity budget:
    /// `Execute` belongs to a **runnable** row and to nothing else, `Execute (sandbox)` additionally to
    /// a platform whose measured probe says the `unshare` recipe works — and it is *absent* without one,
    /// never greyed, because a refused recipe has no greyed way to succeed (§5.4). Both are enabled by
    /// the selection, like every other row that acts on the selection.
    fn run_pair(groups: &[Vec<MenuSpec>], ctx: &MenuContext) {
        let present = |command: Command| {
            groups
                .iter()
                .flatten()
                .any(|spec| spec.command() == Some(command))
        };
        let enabled = |command: Command| {
            groups
                .iter()
                .flatten()
                .any(|spec| spec.command() == Some(command) && spec.enabled(ctx))
        };
        let on_run = matches!(ctx.target, MenuTarget::Row { on_run: true, .. });
        assert_eq!(
            present(Command::Execute),
            on_run,
            "the run row is not the row under the cursor: {ctx:?}"
        );
        assert_eq!(
            present(Command::ExecuteSandbox),
            on_run && ctx.can_sandbox,
            "the sandbox row is greyed instead of absent, or the reverse: {ctx:?}"
        );
        assert_eq!(
            enabled(Command::Execute),
            on_run && ctx.has_selection,
            "`Execute` does not obey Go's `selectedPaths.size > 0` guard: {ctx:?}"
        );
        assert_eq!(
            enabled(Command::ExecuteSandbox),
            on_run && ctx.can_sandbox && ctx.has_selection,
            "`Execute (sandbox)` does not obey the same guard: {ctx:?}"
        );
    }

    /// One literal row per **distinct menu shape** — the human-readable half of the cross-product.
    /// The combination is named by its bits (`0x1` = `has_selection` … `0x40` = `in_batch`,
    /// `0x80` = `can_sandbox`).
    #[test]
    fn the_menu_shapes_are_this_table() {
        let row = |bits: u16, on_dir: bool, on_run: bool| {
            let target = MenuTarget::Row { on_dir, on_run };
            shape(
                &file_menu(&at(bits, target), &ActionContext::background(None), &[]),
                &at(bits, target),
            )
        };
        let background = |bits: u16| {
            let ctx = at(bits, MenuTarget::Background);
            shape(
                &file_menu(&ctx, &ActionContext::background(None), &[]),
                &ctx,
            )
        };
        let table = [
            // nothing selected, empty clipboard, plain filesystem
            (
                "0x00 row(dir)",
                row(0x00, true, false),
                "Open in new tab-, Copy inside-, Cut inside-, Paste inside- | Open- | Cut-, Copy-, Copy path-, Copy name- | Rename-, Change permissions- | Properties-, Extract here- | Move to trash-, Delete permanently-",
            ),
            (
                "0x00 row(file)",
                row(0x00, false, false),
                "Open- | Cut-, Copy-, Copy path-, Copy name- | Rename-, Change permissions- | Properties-, Extract here- | Move to trash-, Delete permanently-",
            ),
            // a script with nothing selected, on a platform that cannot sandbox: one greyed run row and
            // no sandbox twin to grey
            (
                "0x00 row(script)",
                row(0x00, false, true),
                "Open- | Execute- | Cut-, Copy-, Copy path-, Copy name- | Rename-, Change permissions- | Properties-, Extract here- | Move to trash-, Delete permanently-",
            ),
            // …and the same row with `unshare` on PATH: both halves, both greyed
            (
                "0x80 row(script)",
                row(0x80, false, true),
                "Open- | Execute-, Execute (sandbox)- | Cut-, Copy-, Copy path-, Copy name- | Rename-, Change permissions- | Properties-, Extract here- | Move to trash-, Delete permanently-",
            ),
            (
                "0x00 background",
                background(0x00),
                "Paste-, Paste link- | New folder+",
            ),
            // a clipboard that can link, still nothing selected
            (
                "0x04+0x20 background",
                background(0x04 | 0x20),
                "Paste+, Paste link+ | New folder+",
            ),
            // one file selected: the whole write ladder is live
            (
                "0x01 row(file)",
                row(0x01, false, false),
                "Open+ | Cut+, Copy+, Copy path+, Copy name+ | Rename+, Change permissions+ | Properties+, Extract here- | Move to trash+, Delete permanently+",
            ),
            // the same file when it is a script and the platform can sandbox: design §2's pair, one
            // group between `Open` and the clipboard ladder
            (
                "0x01+0x80 row(script)",
                row(0x01 | 0x80, false, true),
                "Open+ | Execute+, Execute (sandbox)+ | Cut+, Copy+, Copy path+, Copy name+ | Rename+, Change permissions+ | Properties+, Extract here- | Move to trash+, Delete permanently+",
            ),
            // …and the same row where `unshare` is not on PATH: the sandbox row is *absent*, not greyed
            (
                "0x01 row(script), no unshare",
                row(0x01, false, true),
                "Open+ | Execute+ | Cut+, Copy+, Copy path+, Copy name+ | Rename+, Change permissions+ | Properties+, Extract here- | Move to trash+, Delete permanently+",
            ),
            // the same row when it is a directory: Go's top block appears above Open
            (
                "0x01+0x02 row(dir)",
                row(0x01 | 0x02, true, false),
                "Open in new tab+, Copy inside+, Cut inside+, Paste inside- | Open+ | Cut+, Copy+, Copy path+, Copy name+ | Rename+, Change permissions+ | Properties+, Extract here- | Move to trash+, Delete permanently+",
            ),
            // a read-only store: every write is greyed, every read is not (Go offered Delete here)
            (
                "0x01+0x10 row(file)",
                row(0x01 | 0x10, false, false),
                "Open+ | Cut-, Copy+, Copy path+, Copy name+ | Rename-, Change permissions- | Properties+, Extract here+ | Move to trash-, Delete permanently-",
            ),
            (
                "0x04+0x10 background",
                background(0x04 | 0x10),
                "Paste-, Paste link- | New folder-",
            ),
            // the batch state: the rename row says what it is and cannot yet run
            (
                "0x01+0x40 row(file)",
                row(0x01 | 0x40, false, false),
                "Open+ | Cut+, Copy+, Copy path+, Copy name+ | Rename batch+, Change permissions+ | Properties+, Extract here- | Move to trash+, Delete permanently+",
            ),
            // everything true at once
            // every flag at once: the archive half *wins* the writes, and the batch half renames
            // the rename row — which is why this shape is the interesting one.
            (
                "0x7f row(dir)",
                row(0x7f, true, false),
                "Open in new tab+, Copy inside-, Cut inside-, Paste inside- | Open+ | Cut-, Copy+, Copy path+, Copy name+ | Rename batch-, Change permissions- | Properties+, Extract here+ | Move to trash-, Delete permanently-",
            ),
            (
                "0x7f background",
                background(0x7f),
                "Paste-, Paste link- | New folder-",
            ),
            // everything except the read-only store: the fully live menu
            (
                "0x6f row(dir)",
                row(0x6f, true, false),
                "Open in new tab+, Copy inside+, Cut inside+, Paste inside+ | Open+ | Cut+, Copy+, Copy path+, Copy name+ | Rename batch+, Change permissions+ | Properties+, Extract here- | Move to trash+, Delete permanently+",
            ),
            // the fully live *script* row: every flag on, and the pair is the only two-in-a-line the
            // menu has
            (
                "0xef row(script)",
                row(0x6f | 0x80, false, true),
                "Open+ | Execute+, Execute (sandbox)+ | Cut+, Copy+, Copy path+, Copy name+ | Rename batch+, Change permissions+ | Properties+, Extract here- | Move to trash+, Delete permanently+",
            ),
            (
                "0x6f background",
                background(0x6f),
                "Paste+, Paste link+ | New folder+",
            ),
        ];
        for (name, actual, want) in table {
            assert_eq!(actual, want, "menu shape for {name}");
        }
    }

    fn into_bits(n: u16) -> [bool; 8] {
        [
            n & 1 != 0,
            n & 2 != 0,
            n & 4 != 0,
            n & 8 != 0,
            n & 16 != 0,
            n & 32 != 0,
            n & 64 != 0,
            n & 128 != 0,
        ]
    }

    #[test]
    fn the_danger_group_is_last_and_holds_the_delete_pair() {
        for bits in all_bits() {
            // …and for the **runnable** row too: design §2's `Execute` group goes between `Open` and
            // the clipboard ladder, never after Delete, so the pair must not become the last group.
            for on_run in [false, true] {
                let ctx = at_bits(
                    bits,
                    MenuTarget::Row {
                        on_dir: false,
                        on_run,
                    },
                );
                let groups = file_menu(&ctx, &ActionContext::background(None), &[]);
                let last = groups.last().expect("the row menu always has a group");
                let commands: Vec<Command> =
                    last.iter().filter_map(|spec| spec.command()).collect();
                assert_eq!(
                    commands,
                    vec![Command::Delete, Command::PermanentDelete],
                    "the run pair pushed the delete pair out of the last group: {ctx:?}"
                );
                assert!(
                    last.iter().all(|spec| spec.danger),
                    "the delete group is the danger group: {ctx:?}"
                );
                // …and it is the only one: Go coloured nothing else red (`context-menu.js:99`).
                for group in &groups[..groups.len() - 1] {
                    assert!(
                        group.iter().all(|spec| !spec.danger),
                        "a non-final group is red in {ctx:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn custom_actions_land_before_delete() {
        // Go appended them *after* Delete (`context-menu.js:127-130`); plan §5.8 calls that the
        // ordering bug. The role filter is Go's (`custom-actions.js:340-353`).
        let action = |name: &str, role: &str| CustomAction {
            id: name.to_string(),
            name: name.to_string(),
            role: role.to_string(),
            ..CustomAction::default()
        };
        let actions = [
            action("on-files", "files"),
            action("on-dirs", "dirs"),
            action("everywhere", "files|dirs|background"),
            action("bg-only", "background"),
        ];
        let ctx = at(
            0x01,
            MenuTarget::Row {
                on_dir: false,
                on_run: false,
            },
        );
        let groups = file_menu(&ctx, &file_row(), &actions);
        let names: Vec<&str> = groups
            .iter()
            .flatten()
            .filter_map(|spec| match &spec.action {
                MenuAction::Custom(a) => Some(a.name.as_str()),
                MenuAction::Command(_) | MenuAction::Group(..) | MenuAction::Result(..) => None,
            })
            .collect();
        assert_eq!(names, ["on-files", "everywhere"]);
        // the custom group sits before the danger group, and after Rename
        let order: Vec<String> = groups
            .iter()
            .flatten()
            .map(|spec| spec.label.clone())
            .collect();
        let pos_of = |label: &str| order.iter().position(|l| l == label);
        assert!(
            pos_of("Rename") < pos_of("on-files") && pos_of("on-files") < pos_of("Move to trash"),
            "{order:?}"
        );

        let dir_ctx = at_(MenuTarget::Row {
            on_dir: true,
            on_run: false,
        });
        let dir_names: Vec<String> = file_menu(&dir_ctx, &dir_row(), &actions)
            .iter()
            .flatten()
            .filter_map(|spec| match &spec.action {
                MenuAction::Custom(a) => Some(a.name.clone()),
                MenuAction::Command(_) | MenuAction::Group(..) | MenuAction::Result(..) => None,
            })
            .collect();
        assert_eq!(dir_names, ["on-dirs", "everywhere"]);

        let bg_ctx = at(0x00, MenuTarget::Background);
        let bg_names: Vec<String> = file_menu(&bg_ctx, &ActionContext::background(None), &actions)
            .iter()
            .flatten()
            .filter_map(|spec| match &spec.action {
                MenuAction::Custom(a) => Some(a.name.clone()),
                MenuAction::Command(_) | MenuAction::Group(..) | MenuAction::Result(..) => None,
            })
            .collect();
        assert_eq!(bg_names, ["everywhere", "bg-only"]);
    }

    fn at_(target: MenuTarget) -> MenuContext {
        let mut ctx = at(0x01, target);
        ctx.all_dirs = true;
        ctx
    }

    #[test]
    fn every_hint_is_the_keymaps_own_chord_and_menu_only_rows_print_none() {
        let keys = KeyMap::default();
        let ctx = at(
            0x7f,
            MenuTarget::Row {
                on_dir: true,
                on_run: false,
            },
        );
        for spec in file_menu(&ctx, &ActionContext::background(None), &[])
            .iter()
            .flatten()
        {
            let command = spec.command().expect("no custom actions passed");
            assert_eq!(
                spec.hint(&keys),
                keys.hint(command),
                "{} prints a hint the table does not have",
                spec.label
            );
            assert_eq!(
                spec.chord(&keys),
                keys.bindings_for(command).first().copied()
            );
        }
        // Go's menu prints no shortcut for Paste Link (`context-menu.js:113-116`); ours prints the
        // real chord, because D7 says the table is the source. So the only rows without a hint are
        // rows the table leaves unbound.
        let bg = at(0x04, MenuTarget::Background);
        assert!(
            file_menu(&bg, &ActionContext::background(None), &[])
                .iter()
                .flatten()
                .all(|spec| spec.hint(&keys).is_some()),
            "every background row has a chord"
        );
    }

    #[test]
    fn the_sibling_menus_are_the_rows_go_builds() {
        let keys = KeyMap::default();
        let labels = |groups: &[Vec<MenuSpec>]| -> Vec<String> {
            groups
                .iter()
                .flatten()
                .map(|spec| format!("{} {}", spec.label, spec.hint(&keys).unwrap_or_default()))
                .collect()
        };
        // `showFolderContextMenu` (`context-menu.js:134-156`).
        assert_eq!(
            labels(&folder_menu(&[], &ActionContext::background(None))),
            [
                "Open in new tab Alt+T",
                "Copy path Alt+Shift+C",
                "Copy name Ctrl+Alt+C",
            ]
        );
        // `showTabContextMenu` (`tabs.js:351-386`): the four live rows, then Go's tab-group block —
        // which is live now, and still prints no hint because a group verb has no chord (design §6), so
        // `format!` leaves the trailing space on those rows.
        assert_eq!(
            labels(&tab_menu(1)),
            [
                "Close tab Ctrl+W",
                "Duplicate tab ",
                "Copy path Alt+Shift+C",
                "Copy name Ctrl+Alt+C",
                "New Tab Group ",
                "Red Group ",
                "Blue Group ",
                "Green Group ",
                "Yellow Group ",
                "Purple Group ",
                "Orange Group ",
                "Cyan Group ",
                "Pink Group ",
                "Teal Group ",
                "Remove Group ",
            ]
        );
        // …and the chip's own menu, which is the port's (no Go counterpart at all): the three verbs the
        // feature is named for, the fold, "select the run", the palette, and the danger row last.
        assert_eq!(
            labels(&group_menu(false)),
            [
                "Rename Group ",
                "Collapse Group ",
                "Select Tabs in Group ",
                "Red Group ",
                "Blue Group ",
                "Green Group ",
                "Yellow Group ",
                "Purple Group ",
                "Orange Group ",
                "Cyan Group ",
                "Pink Group ",
                "Teal Group ",
                "Ungroup ",
                "Close Tabs in Group ",
            ]
        );
        assert!(
            group_menu(true)
                .iter()
                .flatten()
                .any(|spec| spec.label == "Expand Group"),
            "the fold's row says which way it goes"
        );
        // `showSearchResultContextMenu` (`context-menu.js:173-197`): Go's five rows, in its order, with
        // the chords its own `getShortcutDisplay` prints (`:177,188-192`) — and none on the two rows the
        // keyboard has no verb for.
        assert_eq!(
            labels(&search_menu()),
            [
                "Open Enter",
                "Open Containing Folder ",
                "Open Containing Folder in New Tab ",
                "Copy Path Alt+Shift+C",
                "Copy Name Ctrl+Alt+C",
            ]
        );
        // The folder and search menus have nothing to be greyed by: their target is the click.
        let ctx = MenuContext::default();
        assert!(
            folder_menu(&[], &ActionContext::background(None))
                .iter()
                .flatten()
                .all(|spec| spec.enabled(&ctx))
        );
    }

    /// The routing rule [`crate::app::ZenApp::menu_specs`] relies on: a [`MenuTarget::Folder`]
    /// answers with [`folder_menu`] and never with the listing ladder, and neither listing target ever
    /// answers with the folder's three rows. Get this wrong and a crumb right-click shows the
    /// *selection's* menu — which is what Go's `data-target-path` exists to prevent.
    #[test]
    fn only_a_folder_target_gets_the_folder_menu() {
        let labels = |groups: &[Vec<MenuSpec>]| -> Vec<String> {
            groups
                .iter()
                .flatten()
                .map(|spec| spec.label.clone())
                .collect()
        };
        let sel = ActionContext::background(None);
        let folder = at(0x00, MenuTarget::Folder);
        assert_eq!(
            labels(&menu_for(&folder, &sel, &[])),
            labels(&folder_menu(&[], &sel)),
            "a folder target must answer with `showFolderContextMenu`'s rows"
        );
        for listing in [
            at(
                0x01,
                MenuTarget::Row {
                    on_dir: false,
                    on_run: false,
                },
            ),
            at(0x00, MenuTarget::Background),
        ] {
            let got = labels(&menu_for(&listing, &sel, &[]));
            assert_eq!(
                got,
                labels(&file_menu(&listing, &sel, &[])),
                "a listing target must not reach `folder_menu`"
            );
            assert!(
                !got.contains(&"Open in new tab".to_string()),
                "a file row's menu grew the folder's row: {got:?}"
            );
        }
    }

    #[test]
    fn the_paste_glyph_follows_the_clipboard_op() {
        let mut ctx = at(0x04, MenuTarget::Background);
        assert_eq!(icon_paste(&ctx), Some("paste"));
        ctx.clip_is_cut = true;
        assert_eq!(icon_paste(&ctx), Some("cut"));
        let groups = file_menu(&ctx, &ActionContext::background(None), &[]);
        let paste = &groups[0][0];
        assert_eq!(paste.command(), Some(Command::Paste));
        assert_eq!(
            paste.icon,
            Some("cut"),
            "the row shows what a paste will do"
        );
    }

    /// **D11, reviewed against the Go source rather than the report.** The background menu *is*
    /// Paste / Paste Link / New Folder and nothing else: `context-menu.js:112-131` — the `else` arm of
    /// `showContextMenu` — emits exactly those three rows, and no `Select all` / `Invert selection` row
    /// exists anywhere in `frontend/` (`selectAll` is a `file-list.js` export reached by the
    /// `select-all` keyboard command, `keyboard.js:249`). The reference frame the defect cites
    /// (`docs/parity/10-context-menu-background.png`) does not show a menu at all — it shows the
    /// Search Manager's empty-query frame, so the scene name and the pixels disagree; 15d re-shoots
    /// it. Adding the block would be inventing a feature, so this test pins the shape Go has instead,
    /// and fails if anyone later "fixes" parity by adding one.
    #[test]
    fn background_menu_is_go_three_rows_and_carries_no_selection_block() {
        let ctx = at_bits([false; 8], MenuTarget::Background);
        let menu = background_menu(&ctx, &ActionContext::background(None), &[]);
        let rows: Vec<String> = menu
            .iter()
            .flat_map(|group| group.iter().map(|spec| spec.label.clone()))
            .collect();
        assert_eq!(
            rows,
            vec!["Paste", "Paste link", "New folder"],
            "Go's background menu, in Go's order"
        );
        let shape = shape(&menu, &ctx);
        assert!(
            !shape.to_lowercase().contains("select") && !shape.to_lowercase().contains("invert"),
            "the reference has no selection block to be missing: {shape}"
        );
    }

    #[test]
    fn the_hover_fill_rounds_only_what_the_frame_padding_leaves_of_the_corner() {
        // The frame is rounded, the rows are not (`hover_corners`): a row's edge already sits
        // `border + menu_frame_pad_y` inside the frame, so today's tokens (6 pt arc, 6 pt inset) leave
        // the end rows square — which is what is flush, not what a naive `radius - 1` would give.
        let (radius, inset) = (6, 6);
        for row in [
            hover_corners(0, 3, radius, inset),
            hover_corners(2, 3, radius, inset),
        ] {
            assert_eq!((row.nw, row.ne, row.sw, row.se), (0, 0, 0, 0));
        }
        // Shrink the padding and the end rows owe the arc an answer: top row rounds its top corners,
        // the last row its bottom ones, a lone row all four, the middle of three none.
        let shallow = 6 - 4;
        let first = hover_corners(0, 3, radius, 4);
        assert_eq!(
            (first.nw, first.ne, first.sw, first.se),
            (shallow, shallow, 0, 0)
        );
        let middle = hover_corners(1, 3, radius, 4);
        assert_eq!((middle.nw, middle.ne, middle.sw, middle.se), (0, 0, 0, 0));
        let last = hover_corners(2, 3, radius, 4);
        assert_eq!(
            (last.nw, last.ne, last.sw, last.se),
            (0, 0, shallow, shallow)
        );
        let alone = hover_corners(0, 1, radius, 4);
        assert_eq!(
            (alone.nw, alone.ne, alone.sw, alone.se),
            (shallow, shallow, shallow, shallow)
        );
        assert_eq!(
            hover_corners(0, 1, 1, 4).nw,
            0,
            "a frame rounder than the inset it sits in has nothing left to round"
        );
        // A pair's line keeps the same rounding, split: the left rect owns the west corners and the
        // right rect the east ones, so the seam in the middle of the line stays square (`half_corners`).
        let (left, right) = (half_corners(alone, true), half_corners(alone, false));
        assert_eq!(
            (left.nw, left.ne, left.sw, left.se),
            (shallow, 0, shallow, 0)
        );
        assert_eq!(
            (right.nw, right.ne, right.sw, right.se),
            (0, shallow, 0, shallow)
        );
        // A middle line has nothing to give either way.
        let middle = hover_corners(1, 3, radius, 4);
        assert_eq!(
            (
                half_corners(middle, true).nw,
                half_corners(middle, false).se
            ),
            (0, 0)
        );
    }

    #[test]
    fn background_menu_is_the_file_menus_background_half() {
        for bits in all_bits() {
            let ctx = at_bits(
                bits,
                MenuTarget::Row {
                    on_dir: true,
                    on_run: false,
                },
            );
            assert_eq!(
                background_menu(&ctx, &ActionContext::background(None), &[]),
                file_menu(
                    &at_bits(bits, MenuTarget::Background),
                    &ActionContext::background(None),
                    &[]
                ),
                "the plan names both builders; they must not diverge"
            );
        }
    }
}
