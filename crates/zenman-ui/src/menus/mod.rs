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
//! | `showFolderContextMenu` (`:134-156`), the breadcrumb (`navigation.js:215`) and sidebar (`sidebar.js:36,93,131`) | [`folder_menu`] |
//! | `showTabContextMenu` (`tabs.js:351-386`) | [`tab_menu`] |
//! | `showSearchResultContextMenu` (`:159-177`) | [`search_menu`] |
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
//! * **Duplicate Tab / tab group colours** ([`tab_menu`]'s missing rows) are step **14**'s workspaces.
//! * **Open Containing Folder** ([`search_menu`]'s missing rows) is step **12**'s search manager.

use egui::text::LayoutJob;
use egui::{
    Area, Color32, CornerRadius, Frame, Id, Margin, Order, Pos2, Rect, Shadow, Stroke, Ui, Vec2,
    WidgetInfo,
};

use zenman_core::store::config::CustomAction;

use crate::icons;
use crate::keymap::{Chord, Command, KeyMap};
use crate::theme::Tokens;

/// What a menu item names: one of the shell's commands, or one of the user's actions (step 14).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuAction {
    /// A [`Command`] — clicked by the same [`crate::app::ZenApp::dispatch`] the chord uses.
    Command(Command),
    /// A custom action. Reported, never executed, until step 14's `services::actions`.
    Custom(CustomAction),
}

impl MenuAction {
    /// The [`Command`] half, `None` for a custom action.
    #[must_use]
    pub fn command(&self) -> Option<Command> {
        match self {
            Self::Command(command) => Some(*command),
            Self::Custom(_) => None,
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
    },
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
    /// A batch rename owns the pane (step 14's state; `false` until it exists — see
    /// [`crate::app::ZenApp::menu_context`]).
    pub in_batch: bool,
    /// What the click landed on.
    pub target: MenuTarget,
}

impl MenuContext {
    /// The seven plan booleans, as the cross-product test walks them.
    #[must_use]
    pub const fn bits(&self) -> [bool; 7] {
        [
            self.has_selection,
            self.all_dirs,
            self.has_clipboard,
            self.clip_is_cut,
            self.on_archive,
            self.can_symlink,
            self.in_batch,
        ]
    }

    /// A context from [`Self::bits`], leaving [`MenuContext::target`] alone.
    pub fn with_bits(&mut self, bits: [bool; 7]) {
        let [
            has_selection,
            all_dirs,
            has_clipboard,
            clip_is_cut,
            on_archive,
            can_symlink,
            in_batch,
        ] = bits;
        self.has_selection = has_selection;
        self.all_dirs = all_dirs;
        self.has_clipboard = has_clipboard;
        self.clip_is_cut = clip_is_cut;
        self.on_archive = on_archive;
        self.can_symlink = can_symlink;
        self.in_batch = in_batch;
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
        self.command()
            .and_then(|command| keys.bindings_for(command).first().copied())
    }

    /// The right-aligned hint [`show`] prints; `None` for a menu-only command and for a custom action.
    #[must_use]
    pub fn hint(&self, keys: &KeyMap) -> Option<String> {
        self.chord(keys).map(|chord| chord.display())
    }
}

/// Two specs are the same *row* when they look the same. `when` is deliberately not part of it:
/// comparing function pointers is deprecated (`unpredictable_function_pointer_comparisons`) and
/// meaningless here — the behaviour a predicate encodes is what [`MenuSpec::enabled`] and the
/// cross-product test check, in every combination, which is stronger than pointer equality.
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
fn not_yet_wired(_: &MenuContext) -> bool {
    false
}

// ------------------------------------------------------------------------------------------- builders

/// The context menu over a row, or over the empty pane — Go's `showContextMenu`
/// (`context-menu.js:42-131`), group by group.
///
/// The outer `Vec` is the separator structure. Groups are built in Go's order, with two deliberate
/// changes the plan names: the custom actions come **before** the danger group (Go appended them after
/// Delete, `:127-130`), and Delete is split into the D6 pair.
#[must_use]
pub fn file_menu(ctx: &MenuContext, actions: &[CustomAction]) -> Vec<Vec<MenuSpec>> {
    let custom = custom_specs(actions, ctx);
    let groups = match ctx.target {
        MenuTarget::Background => {
            let mut groups = vec![vec![
                item(Command::Paste, paste_here, icon_paste(ctx)),
                item(Command::PasteLink, paste_link_here, Some("link")),
            ]];
            groups.push(vec![item(Command::NewFolder, writable, Some("folder"))]);
            groups.push(custom);
            groups
        }
        MenuTarget::Row { on_dir } => {
            let rename = if ctx.in_batch {
                // Go's `triggerRename` sent a >1 selection to `batch-rename.js`
                // (`context-menu.js:404-410`); the dialog is step 14's, so the row is honest that it
                // cannot run yet rather than silently renaming one row.
                spec(
                    MenuAction::Command(Command::Rename),
                    "Rename batch".to_string(),
                    Some("edit"),
                    false,
                    not_yet_wired,
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
            groups.push(vec![
                item(Command::Cut, selection_writable, Some("cut")),
                item(Command::Copy, selection, Some("copy")),
                item(Command::CopyPath, selection, Some("document")),
                item(Command::CopyName, selection, Some("file")),
            ]);
            // Go's `Change Permissions` row sits directly under Rename (`context-menu.js:105-107`),
            // in the same block — 10b left it out, and 10c gives it the prompt it was missing.
            groups.push(vec![rename, item(Command::Chmod, selection_writable, None)]);
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
pub fn background_menu(ctx: &MenuContext, actions: &[CustomAction]) -> Vec<Vec<MenuSpec>> {
    let mut ctx = *ctx;
    ctx.target = MenuTarget::Background;
    file_menu(&ctx, actions)
}

/// The breadcrumb / sidebar folder menu (Go's `showFolderContextMenu`, `context-menu.js:134-156`).
///
/// Go's `Copy Path` / `Copy Name` here carry *no* chord span (`:143-149`) even though the commands
/// have chords; we print the real ones (D7). Its folder-scoped custom actions (`:151-155`, which
/// execute against the clicked path rather than the selection) are step 14's.
#[must_use]
pub fn folder_menu(actions: &[CustomAction]) -> Vec<Vec<MenuSpec>> {
    let ctx = MenuContext {
        target: MenuTarget::Row { on_dir: true },
        ..MenuContext::default()
    };
    let groups = vec![
        vec![item(Command::OpenInNewTab, always, Some("plus"))],
        vec![
            item(Command::CopyPath, always, Some("document")),
            item(Command::CopyName, always, Some("file")),
        ],
        custom_specs(actions, &ctx),
    ];
    shrink(groups)
}

/// The tab-strip menu (Go's `showTabContextMenu`, `tabs.js:351-386`).
///
/// Go's `Duplicate Tab` (`tabs.js:365-367`) and the five `… Group` rows plus `Remove Group`
/// (`tabs.js:353-359,378-381`) need `Tab::{group,color}` plumbing that is step 14's workspaces; they
/// are absent here rather than faked.
#[must_use]
pub fn tab_menu() -> Vec<Vec<MenuSpec>> {
    shrink(vec![
        vec![item(Command::NewTab, always, Some("plus"))],
        vec![item(Command::CloseTab, always, Some("close"))],
        vec![
            item(Command::CopyPath, always, Some("document")),
            item(Command::CopyName, always, Some("file")),
        ],
    ])
}

/// A search-result menu (Go's `showSearchResultContextMenu`, `context-menu.js:159-177`).
///
/// `Open Containing Folder` and its `… in New Tab` twin (`:164-169`) are the search manager's, i.e.
/// step 12's — they need a *path* target, not the pane's selection, so they are not here yet.
#[must_use]
pub fn search_menu() -> Vec<Vec<MenuSpec>> {
    shrink(vec![
        vec![item(Command::Open, always, Some("external-link"))],
        vec![
            item(Command::CopyPath, always, Some("document")),
            item(Command::CopyName, always, Some("file")),
        ],
    ])
}

/// The custom-action group. Go filters by `role` (`files`/`dirs`/`background`, `|`-separated) and
/// then by the `patterns` globs (`custom-actions.js:337-357`); **the glob half is step 14's**, and
/// moving this whole function into `services/actions::for_context` with it is the handover.
#[must_use]
pub fn custom_specs(actions: &[CustomAction], ctx: &MenuContext) -> Vec<MenuSpec> {
    actions
        .iter()
        .filter(|action| role_matches(action, ctx))
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

/// Go's role filter (`custom-actions.js:340-353`) minus the glob half: a background click takes
/// `background` actions, a dir row `dirs`, a file row `files`.
#[must_use]
pub fn role_matches(action: &CustomAction, ctx: &MenuContext) -> bool {
    let mut roles = action.role.split('|').map(str::trim);
    match ctx.target {
        MenuTarget::Background => roles.any(|role| role == "background"),
        MenuTarget::Row { on_dir } => roles.any(|role| {
            if on_dir {
                role == "dirs"
            } else {
                role == "files"
            }
        }),
    }
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
/// `id` must be fresh per opening: an [`Area`] remembers the position of a known id, and a second
/// right-click elsewhere would otherwise redraw at the first one.
/// What a menu is drawn with — the two things a row needs beyond its own spec, kept together so
/// [`show`] stays under the argument ceiling: the chord table the hints come from (D7) and the token
/// table the colours and the geometry come from.
pub struct MenuStyle<'a> {
    /// The chord table every hint is read out of.
    pub keys: &'a KeyMap,
    /// The design tokens: every colour and every point measurement.
    pub tokens: &'a Tokens,
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
    let area = Area::new(id)
        .order(Order::Foreground)
        .movable(false)
        .default_pos(at)
        .constrain(true)
        .show(ctx, |ui| {
            Frame::new()
                .fill(tokens.bg_sidebar)
                .stroke(Stroke::new(1.0, tokens.border))
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
                    let width = content_width(ui, groups, keys, menu_ctx, tokens);
                    ui.set_min_width(width);
                    for (gi, group) in groups.iter().enumerate() {
                        if group.is_empty() {
                            continue;
                        }
                        if gi > 0 {
                            separator(ui, tokens);
                        }
                        for spec in group {
                            row(ui, spec, menu_ctx, keys, tokens, out);
                        }
                    }
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

/// The frame's own width: Go's `min-width: 180px` (`context-menu.css:8`) grown to fit the widest
/// label + chord pair, and never past the screen.
fn content_width(
    ui: &Ui,
    groups: &[Vec<MenuSpec>],
    keys: &KeyMap,
    menu_ctx: &MenuContext,
    tokens: &Tokens,
) -> f32 {
    let painter = ui.painter();
    let mut widest = tokens.menu_min_width;
    for group in groups {
        for spec in group {
            let icon = if spec.icon.is_some() {
                tokens.row_icon_size + tokens.menu_icon_gap
            } else {
                0.0
            };
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
            widest = widest.max(icon + label + hint + 2.0 * tokens.menu_item_pad_x);
        }
    }
    // Never the whole screen: an over-long custom-action name wraps into the label budget instead
    // of pushing the frame off the pane it belongs to.
    widest.min(tokens.menu_max_width)
}

/// One item: the interact rect first (so the row owns its own click), the pixels after.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn row(
    ui: &mut Ui,
    spec: &MenuSpec,
    menu_ctx: &MenuContext,
    keys: &KeyMap,
    tokens: &Tokens,
    out: &mut Vec<MenuAction>,
) {
    let enabled = spec.enabled(menu_ctx);
    let size = Vec2::new(ui.available_width(), tokens.menu_item_height);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let hint = spec.hint(keys);
    if ui.is_rect_visible(rect) {
        let hovered = enabled && response.hovered();
        if hovered {
            ui.painter()
                .rect_filled(rect, CornerRadius::ZERO, tokens.bg_hover);
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
                Rect::from_min_size(Pos2::new(x, rect.center().y), Vec2::splat(side)),
                icon,
                side,
                colour,
            );
            x += side + tokens.menu_icon_gap;
        }
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
        let show_hint = enabled && hint_width + tokens.menu_hint_gap <= budget;
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
    // One node per row for the accessibility tree and for kittest (`Role::Button`, labelled with the
    // menu text) — the same seam `panels::file_pane` uses for its rows, with the chord in the
    // `hint_text` slot so a screen reader reads the shortcut too.
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

/// The rule between two groups (`context-menu.css:29-33`: 1 px of `--border-color`, `margin: 5px 0`).
fn separator(ui: &mut Ui, tokens: &Tokens) {
    let height = 1.0 + 2.0 * f32::from(tokens.menu_separator_gap);
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), height),
        egui::Sense::hover(),
    );
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

    /// The seven plan booleans in every combination, `bit 0` = `has_selection` … `bit 6` = `in_batch`.
    fn all_bits() -> impl Iterator<Item = [bool; 7]> {
        (0..128u16).map(|n| {
            [
                n & 1 != 0,
                n & 2 != 0,
                n & 4 != 0,
                n & 8 != 0,
                n & 16 != 0,
                n & 32 != 0,
                n & 64 != 0,
            ]
        })
    }

    fn at(bits: u16, target: MenuTarget) -> MenuContext {
        at_bits(into_bits(bits), target)
    }

    fn at_bits(bits: [bool; 7], target: MenuTarget) -> MenuContext {
        let mut ctx = MenuContext::default();
        ctx.with_bits(bits);
        ctx.target = target;
        ctx
    }

    /// Every target shape the file menu answers, so one walk covers `3 × 128` contexts.
    const TARGETS: [MenuTarget; 3] = [
        MenuTarget::Background,
        MenuTarget::Row { on_dir: true },
        MenuTarget::Row { on_dir: false },
    ];

    /// An **independent** answer for one command in one context: `None` = the row is not in this
    /// menu at all, `Some(enabled)` = it is. Written from Go's own guards (`context-menu.js`'s
    /// `isItem`/`isDir`/`hasClipboard` branches and the `selectedPaths.size > 0` guard inside every
    /// `trigger*`), never by calling a builder.
    fn expect(command: Command, ctx: &MenuContext) -> Option<bool> {
        let background = ctx.target == MenuTarget::Background;
        let on_dir = ctx.target == MenuTarget::Row { on_dir: true };
        let writable = !ctx.on_archive;
        match command {
            Command::Open => (!background).then_some(ctx.has_selection),
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
            // Go's >1-selection rename is the batch dialog (`context-menu.js:404-410`), step 14's.
            Command::Rename => {
                (!background).then_some(ctx.has_selection && writable && !ctx.in_batch)
            }
            Command::Copy | Command::CopyPath | Command::CopyName => {
                (!background).then_some(ctx.has_selection)
            }
            // Go's `Change Permissions` row (`context-menu.js:105-107`) sits with Rename and needs the
            // same thing: a selection whose mode to change.
            Command::Chmod => (!background).then_some(ctx.has_selection && writable),
            _ => None,
        }
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
                let groups = file_menu(&ctx, &[]);
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
                let groups = file_menu(&ctx, &[]);
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
                    ctx.target == MenuTarget::Row { on_dir: true }
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
                assert_eq!(
                    enabled(Command::Rename),
                    enabled(Command::Delete) && !ctx.in_batch
                );
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
                    ctx.target == MenuTarget::Row { on_dir: true }
                        && ctx.has_selection
                        && ctx.all_dirs
                        && !ctx.on_archive
                );
                // Nothing is *hidden* because it is disabled: a greyed row keeps its place.
                for command in [Command::Delete, Command::Paste, Command::NewFolder] {
                    if expect(command, &ctx).is_some() {
                        assert!(present(command), "{command:?} vanished from {ctx:?}");
                    }
                }
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

    /// One literal row per **distinct menu shape** — the human-readable half of the cross-product.
    /// The combination is named by its bits (`0x1` = `has_selection` … `0x40` = `in_batch`).
    #[test]
    fn the_menu_shapes_are_this_table() {
        let row = |bits: u16, on_dir: bool| {
            shape(
                &file_menu(&at(bits, MenuTarget::Row { on_dir }), &[]),
                &at(bits, MenuTarget::Row { on_dir }),
            )
        };
        let background = |bits: u16| {
            let ctx = at(bits, MenuTarget::Background);
            shape(&file_menu(&ctx, &[]), &ctx)
        };
        let table = [
            // nothing selected, empty clipboard, plain filesystem
            (
                "0x00 row(dir)",
                row(0x00, true),
                "Open in new tab-, Copy inside-, Cut inside-, Paste inside- | Open- | Cut-, Copy-, Copy path-, Copy name- | Rename-, Change permissions- | Move to trash-, Delete permanently-",
            ),
            (
                "0x00 row(file)",
                row(0x00, false),
                "Open- | Cut-, Copy-, Copy path-, Copy name- | Rename-, Change permissions- | Move to trash-, Delete permanently-",
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
                row(0x01, false),
                "Open+ | Cut+, Copy+, Copy path+, Copy name+ | Rename+, Change permissions+ | Move to trash+, Delete permanently+",
            ),
            // the same row when it is a directory: Go's top block appears above Open
            (
                "0x01+0x02 row(dir)",
                row(0x01 | 0x02, true),
                "Open in new tab+, Copy inside+, Cut inside+, Paste inside- | Open+ | Cut+, Copy+, Copy path+, Copy name+ | Rename+, Change permissions+ | Move to trash+, Delete permanently+",
            ),
            // a read-only store: every write is greyed, every read is not (Go offered Delete here)
            (
                "0x01+0x10 row(file)",
                row(0x01 | 0x10, false),
                "Open+ | Cut-, Copy+, Copy path+, Copy name+ | Rename-, Change permissions- | Move to trash-, Delete permanently-",
            ),
            (
                "0x04+0x10 background",
                background(0x04 | 0x10),
                "Paste-, Paste link- | New folder-",
            ),
            // the batch state: the rename row says what it is and cannot yet run
            (
                "0x01+0x40 row(file)",
                row(0x01 | 0x40, false),
                "Open+ | Cut+, Copy+, Copy path+, Copy name+ | Rename batch-, Change permissions+ | Move to trash+, Delete permanently+",
            ),
            // everything true at once
            // every flag at once: the archive half *wins* the writes, and the batch half renames
            // the rename row — which is why this shape is the interesting one.
            (
                "0x7f row(dir)",
                row(0x7f, true),
                "Open in new tab+, Copy inside-, Cut inside-, Paste inside- | Open+ | Cut-, Copy+, Copy path+, Copy name+ | Rename batch-, Change permissions- | Move to trash-, Delete permanently-",
            ),
            (
                "0x7f background",
                background(0x7f),
                "Paste-, Paste link- | New folder-",
            ),
            // everything except the read-only store: the fully live menu
            (
                "0x6f row(dir)",
                row(0x6f, true),
                "Open in new tab+, Copy inside+, Cut inside+, Paste inside+ | Open+ | Cut+, Copy+, Copy path+, Copy name+ | Rename batch-, Change permissions+ | Move to trash+, Delete permanently+",
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

    fn into_bits(n: u16) -> [bool; 7] {
        [
            n & 1 != 0,
            n & 2 != 0,
            n & 4 != 0,
            n & 8 != 0,
            n & 16 != 0,
            n & 32 != 0,
            n & 64 != 0,
        ]
    }

    #[test]
    fn the_danger_group_is_last_and_holds_the_delete_pair() {
        for bits in all_bits() {
            let ctx = at_bits(bits, MenuTarget::Row { on_dir: false });
            let groups = file_menu(&ctx, &[]);
            let last = groups.last().expect("the row menu always has a group");
            let commands: Vec<Command> = last.iter().filter_map(|spec| spec.command()).collect();
            assert_eq!(
                commands,
                vec![Command::Delete, Command::PermanentDelete],
                "{ctx:?}"
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
        let ctx = at(0x01, MenuTarget::Row { on_dir: false });
        let groups = file_menu(&ctx, &actions);
        let names: Vec<&str> = groups
            .iter()
            .flatten()
            .filter_map(|spec| match &spec.action {
                MenuAction::Custom(a) => Some(a.name.as_str()),
                MenuAction::Command(_) => None,
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

        let dir_ctx = at_(MenuTarget::Row { on_dir: true });
        let dir_names: Vec<String> = file_menu(&dir_ctx, &actions)
            .iter()
            .flatten()
            .filter_map(|spec| match &spec.action {
                MenuAction::Custom(a) => Some(a.name.clone()),
                MenuAction::Command(_) => None,
            })
            .collect();
        assert_eq!(dir_names, ["on-dirs", "everywhere"]);

        let bg_ctx = at(0x00, MenuTarget::Background);
        let bg_names: Vec<String> = file_menu(&bg_ctx, &actions)
            .iter()
            .flatten()
            .filter_map(|spec| match &spec.action {
                MenuAction::Custom(a) => Some(a.name.clone()),
                MenuAction::Command(_) => None,
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
        let ctx = at(0x7f, MenuTarget::Row { on_dir: true });
        for spec in file_menu(&ctx, &[]).iter().flatten() {
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
            file_menu(&bg, &[])
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
            labels(&folder_menu(&[])),
            [
                "Open in new tab Alt+T",
                "Copy path Alt+Shift+C",
                "Copy name Ctrl+Alt+C",
            ]
        );
        // `showTabContextMenu` (`tabs.js:351-386`) minus the step-14 rows.
        assert_eq!(
            labels(&tab_menu()),
            [
                "New tab Ctrl+T",
                "Close tab Ctrl+W",
                "Copy path Alt+Shift+C",
                "Copy name Ctrl+Alt+C",
            ]
        );
        // `showSearchResultContextMenu` (`context-menu.js:159-177`) minus the step-12 rows.
        assert_eq!(
            labels(&search_menu()),
            [
                "Open Enter",
                "Copy path Alt+Shift+C",
                "Copy name Ctrl+Alt+C",
            ]
        );
        // The folder and search menus have nothing to be greyed by: their target is the click.
        let ctx = MenuContext::default();
        assert!(
            folder_menu(&[])
                .iter()
                .flatten()
                .all(|spec| spec.enabled(&ctx))
        );
    }

    #[test]
    fn the_paste_glyph_follows_the_clipboard_op() {
        let mut ctx = at(0x04, MenuTarget::Background);
        assert_eq!(icon_paste(&ctx), Some("paste"));
        ctx.clip_is_cut = true;
        assert_eq!(icon_paste(&ctx), Some("cut"));
        let groups = file_menu(&ctx, &[]);
        let paste = &groups[0][0];
        assert_eq!(paste.command(), Some(Command::Paste));
        assert_eq!(
            paste.icon,
            Some("cut"),
            "the row shows what a paste will do"
        );
    }

    #[test]
    fn background_menu_is_the_file_menus_background_half() {
        for bits in all_bits() {
            let ctx = at_bits(bits, MenuTarget::Row { on_dir: true });
            assert_eq!(
                background_menu(&ctx, &[]),
                file_menu(&at_bits(bits, MenuTarget::Background), &[]),
                "the plan names both builders; they must not diverge"
            );
        }
    }
}
