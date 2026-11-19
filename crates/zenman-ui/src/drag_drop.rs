//! Drag & drop between panes and onto directories (plan §5.8 `drag_drop.rs`, §7 row 10).
//!
//! # The mechanism, and why it is not `egui::DragAndDrop`
//!
//! The plan's row sketches `DragPayload` "registered via `egui::DragAndDrop` so OS file drops land in
//! the pane and our drags can leave the window". This fork *does* have that surface
//! (`Response::dnd_set_drag_payload` / `dnd_hover_payload` / `dnd_release_payload`,
//! `egui/src/response.rs:551-599`), and it is the right vehicle for *OS* drops; it is not the right
//! one for these two reasons, and both were measured rather than assumed:
//!
//! * `dnd_release_payload` fires only on a response whose `contains_pointer()` is true **in the same
//!   frame** as the release. Our drop targets live inside the dock's pane rects, which are allocated
//!   by `craft_ui::docking::DockArea` — a target response that is re-created every frame inside a
//!   nested `Ui` has no stable claim on the pointer, and step 10b already measured what an unstable
//!   hover claim costs in this fork (the never-settling repaint). A state machine that the *pane*
//!   asks the shell about once per frame cannot disagree with itself.
//! * The payload is *derived* state — the selection at grab time, the clipboard's op, the modifier
//!   held at release — which has to survive a re-sort, a `DirChanged` reload and a page swap mid-drag.
//!   `DragAndDrop`'s `Any` box cannot be re-derived, so a stale one would be pasted.
//!
//! So the drag is an explicit state machine owned by [`ZenApp::drag`](crate::app::ZenApp::drag): the
//! row's `RowEvent::DndGrab` (already the pane's seam, `RowEvent` in `widgets/virtual_list`) fills a
//! [`DragPayload`], every directory row and every pane body may claim the release, and the payload is
//! cleared at the end of the frame the button came up in. OS file drops — the other half of the plan
//! row — are platform work and stay recorded as such (plan §7 row 16).
//!
//! # What Go does
//!
//! `frontend/js/file-list.js:298-345`: `dragstart` puts `JSON.stringify([...tab.selectedPaths])` on the
//! data transfer and sets `effectAllowed = 'move'`; `dragover` accepts **only directory rows**
//! (`if (isDir)`, adding the `.drag-over` class); `drop` calls `executeFileOp('paste', sources, path)` —
//! the same paste endpoint as <kbd>Ctrl+V</kbd>, whose `operation` (the clipboard's own cut/copy op)
//! decides whether the sources disappear (`if (operation === 'cut')`).
//!
//! Note what that means for modifiers: **Go reads none.** There is no `ctrlKey` in its `dragover` or
//! `drop` handler, and `effectAllowed` is a constant `'move'` — so a Go drag is literally "paste the
//! clipboard's paths into that folder", and a drag started on an *unselected* row first replaces the
//! selection with that row (`:300-304`). The port matches all of that, and adds the one thing the
//! desktop convention demands and Go's browser could not express: holding <kbd>Ctrl</kbd> at the
//! moment of release turns the drop into a copy. That is the only way to ask for a copy when the drag
//! itself carries the paths rather than pointing at a clipboard.

use egui::{Area, Color32, CornerRadius, Frame, Margin, Modifiers, Order, Pos2, RichText, Ui};

use zenman_core::ops::clipboard::{ClipOp, Clipboard};
use zenman_core::ops::paste::PasteMode;
use zenman_core::path::AbsPath;
use zenman_core::store::config::PaneSlot;

use crate::theme::Tokens;

/// Copy or move. Go's single `effectAllowed = 'move'` line means *the clipboard's op* decides; for a
/// drop the port keeps that default and lets <kbd>Ctrl</kbd> force a copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragMode {
    /// The sources are gone afterwards (Go's `operation === 'cut'` branch).
    Move,
    /// The sources stay.
    Copy,
}

impl DragMode {
    /// The paste mode this drag means.
    #[must_use]
    pub const fn paste_mode(self) -> PasteMode {
        match self {
            Self::Move => PasteMode::Cut,
            Self::Copy => PasteMode::Copy,
        }
    }

    /// The word the ghost and the toast use.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Move => "Move",
            Self::Copy => "Copy",
        }
    }
}

/// What is being dragged: the paths the grab selected (Go's `selectedPaths`, not merely the row).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DragPayload {
    /// The pane the drag started in.
    pub source: PaneSlot,
    /// Go's `dataTransfer` payload, in listing order.
    pub paths: Vec<AbsPath>,
    /// The mode decided at grab time from the clipboard; [`drop_mode`] refines it at release.
    pub mode: DragMode,
}

impl DragPayload {
    /// The ghost's text: `"Move 3 item(s)"`.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{} {} item(s)", self.mode.word(), self.paths.len())
    }
}

/// The mode a grab means: the clipboard's own op, exactly like Go's paste endpoint. A drag with no
/// clipboard moves (Go's `effectAllowed = 'move'`), which is what a same-pane reorder means.
#[must_use]
pub fn grab_mode(clip: Option<&Clipboard>) -> DragMode {
    match clip {
        Some(clipboard) if clipboard.op == ClipOp::Cut => DragMode::Move,
        Some(_) => DragMode::Copy,
        None => DragMode::Move,
    }
}

/// The mode a *release* means: <kbd>Ctrl</kbd> forces a copy, otherwise the grab's mode stands.
#[must_use]
pub fn drop_mode(grab: DragMode, modifiers: &Modifiers) -> DragMode {
    if modifiers.ctrl { DragMode::Copy } else { grab }
}

/// The floating label that follows the pointer. Paint-only and `interactive(false)`: it can never
/// win a click from the row beneath it (rule 10 — decoration paints, interaction allocates).
pub fn paint_ghost(ui: &Ui, tokens: &Tokens, payload: &DragPayload, at: Pos2) {
    let label = payload.label();
    Area::new(ui.scope_id().with("zenman-drag-ghost"))
        .fixed_pos(at + tokens.drag_ghost_offset)
        .order(Order::Foreground)
        .interactable(false)
        .show(ui.ctx(), |ui| {
            Frame::new()
                .fill(tokens.bg_card)
                .stroke(egui::Stroke::new(1.0, tokens.accent))
                .corner_radius(CornerRadius::same(tokens.corner_radius))
                .inner_margin(Margin::symmetric(
                    tokens.cell_padding as i8,
                    tokens.row_pad_y as i8,
                ))
                .show(ui, |ui| {
                    ui.label(RichText::new(label).color(tokens.text_main));
                });
        });
}

/// Highlight the row or pane body that would take the drop — Go's `.drag-over` class
/// (`css/file-list.css`'s `.file-item.drag-over`), painted rather than allocated, for the same reason
/// the ghost is.
pub fn paint_drop_target(ui: &Ui, tokens: &Tokens, rect: egui::Rect) {
    let colour: Color32 = tokens.accent;
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(tokens.corner_radius),
        egui::Stroke::new(1.0, colour),
        egui::StrokeKind::Inside,
    );
}
