//! The context menu of one **search result row** — [`menus::search_menu`]'s five rows, raised by both
//! search dialogs so that a right-click means the same thing in each.
//!
//! # Why a dialog owns a menu at all
//!
//! [`crate::app::ZenApp`] already keeps one open menu, adopts requests for it a frame late, and draws
//! it (`crate::app::ZenApp::draw_menu`). That menu is *the pane's*: its rows are chosen from the
//! pane's listing ([`crate::app::ZenApp::menu_context`] reads the shown tab's selection, its clipboard,
//! whether the directory is an archive), its rows act on the pane's selection
//! ([`crate::app::ZenApp::dispatch`]), and it is drawn on the frame's way **before** the dialogs —
//! [`crate::menus`] is explicit that the last allocated overlapping rect owns the pointer, so a menu
//! drawn there would sit *under* the dialog card that asked for it.
//!
//! So the search dialogs draw their own, from their `drain`, which runs after the card: [`ResultMenu`]
//! holds what a row of the pane would have held — the row, the anchor, the layer id — and the dialog
//! resolves the row into a path when a row is clicked.
//!
//! # The frame it waits
//!
//! [`menus::show`] closes a menu when *any* pointer press lands outside it, read from the live input
//! state. A menu drawn in the same frame as the right-click that asked for it would therefore close on
//! the press that opened it, so `armed` makes it wait one frame — the same rule
//! `crate::app::ZenApp::open_requested_menu` exists for, arriving by the other door.

use egui::{Id, Pos2, Rect};

use crate::keymap::KeyMap;
use crate::menus::{self, MenuAction, MenuContext, MenuTarget, ResultVerb};
use crate::theme::Tokens;

/// One result row's menu, open. `Copy` because everything in it is a coordinate or an id; the *row* is
/// an index into the dialog's own list, which is the only thing the dialog can promise not to have
/// moved when the drain asks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResultMenu {
    /// The row it was opened over, in the dialog's list order.
    row: usize,
    /// Where the pointer was. Frozen, because an [`egui::Area`] remembers the position of a known id.
    at: Pos2,
    /// The layer. Fresh per opening: this fork's [`Id::unique`] answers the same id every time, so a
    /// second opening would be dragged back to the first one's anchor.
    id: Id,
    /// False for the frame the right-click landed in — see [the frame it waits](self).
    armed: bool,
    /// Last frame's rect, as painted. The card behind needs it to tell "a click outside me" from "a
    /// click on this menu", and this frame's rect does not exist until it has been painted.
    rect: Option<Rect>,
}

/// What drawing the menu produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuShot {
    /// Nothing decided: it stays up.
    Open,
    /// A row was clicked, and the dialog's own row is what it names.
    Picked(ResultVerb),
    /// A click outside it. The menu goes away; the dialog keeps its place — a dismissed menu is not an
    /// answer, and Go hid the menu the same way (`app.js:166-169`, a capture-phase document click that
    /// closes nothing but the menu — and, like the port's, still lets the click reach the row under it).
    Dismissed,
}

impl ResultMenu {
    /// Open over `row`, anchored where the pointer was. Not armed yet, so nothing is drawn this frame.
    #[must_use]
    pub fn new(row: usize, at: Pos2) -> Self {
        Self {
            row,
            at,
            id: Id::unique("zenman-result-menu"),
            armed: false,
            rect: None,
        }
    }

    /// The row this menu speaks about, in the dialog's list order.
    #[must_use]
    pub fn row(&self) -> usize {
        self.row
    }

    /// Where it was painted last frame, if it was painted at all.
    #[must_use]
    pub fn rect(&self) -> Option<Rect> {
        self.rect
    }

    /// The menu's rows, so a test can assert the five labels and their states without a frame.
    #[must_use]
    pub fn specs() -> Vec<Vec<menus::MenuSpec>> {
        menus::search_menu()
    }

    /// Draw it and report what happened. Called from a dialog's `drain`, which is the first moment in
    /// the frame that is both after the card and holding a `&ZenApp` (for the keymap the chord hints
    /// come from — [`menus::MenuStyle`]'s other half).
    pub fn show(&mut self, keys: &KeyMap, tokens: &Tokens, ctx: &egui::Context) -> MenuShot {
        if !self.armed {
            self.armed = true;
            return MenuShot::Open;
        }
        let mut clicked = Vec::new();
        // Not the pane's context: every row of `search_menu` is `always`, so nothing here can be read
        // wrongly as a fact about the listing behind the card — but the target says what it is anyway,
        // which is how `menu_for` would have answered too.
        let menu_ctx = MenuContext {
            target: MenuTarget::Result,
            ..Default::default()
        };
        let outcome = menus::show(
            ctx,
            self.id,
            self.at,
            &menu_ctx,
            &menus::search_menu(),
            menus::MenuStyle { keys, tokens },
            &mut clicked,
        );
        self.rect = Some(outcome.rect);
        // One row per click, and every row of this menu names a `ResultVerb`; anything else is a bug in
        // the builder, not a state to handle, so it is ignored rather than guessed at.
        if let Some(MenuAction::Result(verb)) = clicked.first() {
            return MenuShot::Picked(*verb);
        }
        if outcome.close {
            return MenuShot::Dismissed;
        }
        MenuShot::Open
    }
}

/// Where a row's menu belongs: under the pointer, which is where the right-click happened, falling
/// back to the row's own left edge — the fallback matters for the one pointer that has no position, a
/// synthetic click from a test harness or from keyboard-driven activation, and it is a legal place for
/// a menu.
#[must_use]
pub fn pointer_at(ui: &egui::Ui, response: &egui::Response) -> Pos2 {
    response
        .interact_pointer_pos()
        .or_else(|| ui.input(|i| i.pointer.hover_pos()))
        .unwrap_or_else(|| response.rect.left_top())
}

/// Go's document-level click handler (`quick-find.js:207-215`, `search-manager.js:311-319`): a press
/// that begins outside the dialog dismisses it.
///
/// The dim behind the card deliberately allocates **no input** (see [`super`]'s docs — the pane behind
/// keeps its wheel and its hover), so there is no "backdrop was clicked" response to ask. The test is
/// the press position against the two rects the dialog knows about: its card, and the row menu floating
/// outside it.
///
/// `opening` is the frame the card appeared in. The press that got the dialog there — a footer button of
/// another dialog, a pane row, the chord's own key — is still live in this frame's input, and a dialog
/// that answered it would be a dialog that closed before it was ever seen.
#[must_use]
pub fn dismiss_by_click(ui: &egui::Ui, card: Rect, menu: Option<Rect>, opening: bool) -> bool {
    if opening {
        return false;
    }
    ui.input(|i| {
        // The *primary* button, because that is the gesture Go listens for (`'click'`, not
        // `'contextmenu'`): a right-click elsewhere belongs to whatever is behind the dim, and must not
        // answer the dialog.
        i.pointer.button_pressed(egui::PointerButton::Primary)
            && i.pointer.interact_pos().is_none_or(|pos| {
                !card.contains(pos) && !menu.is_some_and(|menu| menu.contains(pos))
            })
    })
}
