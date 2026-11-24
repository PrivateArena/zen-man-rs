//! The modal dialog stack (plan §5.8 `dialogs/mod.rs`, §7 row 10).
//!
//! Two kinds, one at a time:
//!
//! * [`Prompt`] — one text field (new folder, permissions);
//! * [`Confirm`] — a question with named answers (permanent delete, paste conflict).
//!
//! The plan's `Stack(Vec<Dialog>)` is deliberately a single slot: Go's whole dialog surface was
//! `window.prompt` / `window.confirm` / `window.alert`, all three of which are *blocking and
//! singular*, and the §5.8 `quick_find` / `search_manager` / `batch_rename` / `properties` dialogs
//! are steps 11-14's. When they arrive the slot becomes the stack; until then `open` refuses a
//! second dialog rather than silently replacing the first, which is the "only one dialog at a time"
//! modal-safety rule.
//!
//! # How a result becomes a command (plan §4.1)
//!
//! A dialog never touches the engine. [`Dialogs::show`] returns [`Answer`]s, and the caller
//! (`ZenApp::ui`, after the draw, exactly like the pane commands) turns them into `Session` calls.
//! That is what makes the whole surface testable — and what keeps a dialog that is *about* pane A
//! from acting on whichever pane happens to be focused when the button is clicked: the subject
//! travels inside [`DialogKind`].

pub mod action_editor;
pub mod batch_rename;
pub mod confirm;
pub mod extract;
pub mod prompt;
pub mod properties;
pub mod quick_find;
pub mod search_manager;

pub use action_editor::{ActionEditor, ActionOutcome};
pub use batch_rename::{BatchOutcome, BatchRename};
pub use confirm::{Choice, Confirm, ConfirmButton};
pub use extract::ExtractProgress;
pub use prompt::{Prompt, Validation};
pub use properties::{PropertiesDialog, Subject as PropertiesSubject};
pub use quick_find::{QuickFind, QuickOutcome};
pub use search_manager::{IndexOutcome, SearchManager};

use egui::{Align2, Area, CornerRadius, Id, Order, Ui, Vec2};

use zenman_core::ops::paste::PasteMode;
use zenman_core::path::AbsPath;
use zenman_core::store::config::PaneSlot;

use crate::state::GroupId;
use crate::theme::Tokens;

/// What an answer is about. Everything the command needs travels with the dialog, not with the
/// focused pane (see the module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DialogKind {
    /// `Prompt` → `Session::mkdir`.
    NewFolder {
        /// The pane whose directory is the parent.
        slot: PaneSlot,
    },
    /// `Prompt` → `Session::chmod`.
    Chmod {
        /// The pane whose selection is the target.
        slot: PaneSlot,
        /// The paths the mode goes on — frozen when the dialog opened.
        paths: Vec<AbsPath>,
    },
    /// `Confirm` → `Session::delete` with [`DeleteReq::permanent`](zenman_core::ops::delete::DeleteReq::permanent).
    PermanentDelete {
        /// The pane whose selection is the target.
        slot: PaneSlot,
        /// The paths that would go — frozen when the dialog opened, and printed in it (plan D6).
        paths: Vec<AbsPath>,
    },
    /// The quick-find dialog (step 11b) — the subject is the *pane*, and the answer is a row.
    QuickFind {
        /// The pane whose directory is the scope and whose tab takes the navigation.
        slot: PaneSlot,
    },
    /// The Search Manager (step 12c) — the subject is the *index*; the pane only receives the row a
    /// row activation navigates to.
    SearchManager {
        /// The pane an activated row lands in.
        slot: PaneSlot,
    },
    /// `Confirm` (overwrite / skip / rename) → `Session::paste`/`paste_clipboard` with the answer's
    /// [`ConflictPolicy`](zenman_core::ops::ConflictPolicy).
    PasteConflict {
        /// The pane to report to.
        slot: PaneSlot,
        /// Where the bytes land.
        dest: AbsPath,
        /// `None` = follow the clipboard's own op (Go's `paste`), `Some` = an explicit mode.
        mode: Option<PasteMode>,
        /// A drag's sources; empty for a clipboard paste, which reads the live clipboard.
        sources: Vec<AbsPath>,
    },
    /// Batch rename (step 14b) — the subject is the pane whose selection the rows came from.
    BatchRename {
        /// The pane the rows came from, and the pane the reload lands in.
        slot: PaneSlot,
    },
    /// The custom-action editor (step 14b) — the subject is the pane a test run is aimed at.
    ActionEditor {
        /// The pane whose selection the applicability preview was computed against.
        slot: PaneSlot,
    },
    /// `Confirm` → `Session::workspace_delete` (step 14b; destructive, so it asks).
    DeleteWorkspace {
        /// The stored workspace the answer is about.
        name: String,
    },
    /// `Prompt` → `Session::workspace_save`: the name a *New workspace* is stored under.
    NewWorkspace,
    /// `Prompt` → `Session::workspace_rename`: the new name of a stored workspace.
    RenameWorkspace {
        /// The name it has now.
        name: String,
    },
    /// `Prompt` → the rename of one pane's tab group (`docs/tab-groups-design.md` §6). The subject is
    /// the pane *and* the group, because a group id is per pane and the answer has to land in the
    /// strip that asked — and because the group may have been closed while the prompt was up, which is
    /// a no-op rather than a panic.
    RenameTabGroup {
        /// The pane whose strip owns the group.
        slot: PaneSlot,
        /// The group.
        id: GroupId,
    },
    /// `Properties` (step 13b) — the subject is the path it printed, and the only answer is "close".
    Properties {
        /// The pane the dialog was opened from.
        slot: PaneSlot,
        /// What it is about, in display form (a filesystem path, or an archive member path).
        path: String,
    },
}

/// What a closed dialog says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// A prompt's answer, with its text.
    Text {
        /// The subject.
        kind: DialogKind,
        /// Accept or cancel.
        choice: Choice,
        /// What was in the field.
        text: String,
    },
    /// A confirm's answer.
    Button {
        /// The subject.
        kind: DialogKind,
        /// Which button.
        choice: Choice,
    },
    /// A quick-find answer: a row to act on, or the dismissal (`Escape`).
    Quick {
        /// The subject ([`DialogKind::QuickFind`]).
        kind: DialogKind,
        /// What the user did.
        outcome: QuickOutcome,
    },
    /// A batch-rename answer.
    Batch {
        /// The subject ([`DialogKind::BatchRename`]).
        kind: DialogKind,
        /// What the user did.
        outcome: BatchOutcome,
    },
    /// A custom-action editor answer.
    Action {
        /// The subject ([`DialogKind::ActionEditor`]).
        kind: DialogKind,
        /// What the user did.
        outcome: ActionOutcome,
    },
    /// A Search Manager answer: a row to act on, or the dismissal. Everything else the manager asks
    /// for is a request it keeps for `search_manager::drain` — a config panel must not close on Save.
    Index {
        /// The subject ([`DialogKind::SearchManager`]).
        kind: DialogKind,
        /// What the user did.
        outcome: IndexOutcome,
    },
}

impl Answer {
    /// The subject.
    #[must_use]
    pub fn kind(&self) -> &DialogKind {
        match self {
            Self::Text { kind, .. } | Self::Button { kind, .. } | Self::Quick { kind, .. } => kind,
            Self::Index { kind, .. } | Self::Batch { kind, .. } | Self::Action { kind, .. } => kind,
        }
    }

    /// The answer.
    #[must_use]
    pub fn choice(&self) -> Choice {
        match self {
            Self::Text { choice, .. } | Self::Button { choice, .. } => *choice,
            Self::Quick {
                outcome: QuickOutcome::Activate { .. } | QuickOutcome::Closed { .. },
                ..
            } => Choice::Ok,
            Self::Index { .. } | Self::Batch { .. } | Self::Action { .. } => Choice::Ok,
        }
    }

    /// `true` for the affirmative answer (`Choice::Ok` or one of the three conflict answers).
    #[must_use]
    pub fn accepted(&self) -> bool {
        !matches!(self.choice(), Choice::Cancel)
    }
}

/// The one dialog that is open.
#[derive(Debug)]
pub enum Dialog {
    /// A text field.
    Prompt(Prompt),
    /// A question.
    Confirm(Confirm),
    /// The live search (step 11b).
    QuickFind(QuickFind),
    /// The Search Manager (step 12c). Boxed: it carries the whole config draft, and the plan's
    /// `Stack(Vec<Dialog>)` would otherwise pay 592 bytes per slot (`clippy::large_enum_variant`).
    SearchManager(Box<SearchManager>),
    /// Properties (step 13b) — local **and** archive subjects, one dialog, two engines.
    Properties(PropertiesDialog),
    /// Batch rename (step 14b). Boxed: it carries the whole preview table.
    BatchRename(Box<BatchRename>),
    /// The custom-action editor (step 14b).
    ActionEditor(Box<ActionEditor>),
}

impl Dialog {
    /// Draw it; `Some(answer)` once answered.
    fn show(&mut self, ui: &mut Ui, tokens: &Tokens) -> Option<Answer> {
        match self {
            Self::Prompt(prompt) => prompt.show(ui, tokens),
            Self::Confirm(confirm) => confirm.show(ui, tokens),
            Self::QuickFind(find) => find.show(ui, tokens),
            Self::SearchManager(manager) => manager.show(ui, tokens),
            Self::Properties(dialog) => dialog.show(ui, tokens),
            Self::BatchRename(dialog) => dialog.show(ui, tokens),
            Self::ActionEditor(editor) => editor.show(ui, tokens),
        }
    }
}

/// The shell's modal state: at most one dialog, drawn last on the foreground layer.
#[derive(Default)]
pub struct Dialogs {
    active: Option<Dialog>,
}

impl Dialogs {
    /// The only open slot. `false` when a dialog is already up — the caller's toast says so, and
    /// nothing is lost, because Go's `window.prompt` refused the second call just as loudly.
    pub fn open(&mut self, dialog: Dialog) -> bool {
        if self.active.is_some() {
            return false;
        }
        self.active = Some(dialog);
        true
    }

    /// Is a dialog up? While one is, [`crate::app::ZenApp::handle_keys`] stands down: the panes
    /// must not consume the keys the dialog is answering (plan F15).
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.active.is_some()
    }

    /// Close whatever is up (`Escape` from the shell, a pane going away).
    pub fn close(&mut self) {
        self.active = None;
    }

    /// Lift the open dialog out (the frame's owner is `ZenApp`, and a dialog that needs the session
    /// must be handed both without aliasing — see [`properties::tick`](properties::tick)).
    pub fn take(&mut self) -> Option<Dialog> {
        self.active.take()
    }

    /// Put a lifted dialog back (a no-op if something else opened in between).
    pub fn put(&mut self, dialog: Dialog) {
        if self.active.is_none() {
            self.active = Some(dialog);
        }
    }

    /// What is open, for the tests and for a redraw of the *same* dialog.
    #[must_use]
    pub fn active(&self) -> Option<&Dialog> {
        self.active.as_ref()
    }

    /// `mutable access for the shell's own bookkeeping (the in-cell rename editor is separate).
    #[must_use]
    pub fn active_mut(&mut self) -> Option<&mut Dialog> {
        self.active.as_mut()
    }

    /// Draw the dialog and take its answers (it closes itself when it has one).
    pub fn show(&mut self, ui: &mut Ui, tokens: &Tokens) -> Vec<Answer> {
        let mut out = Vec::new();
        if self.active.is_some() {
            // 15c D1: dim the shell *before* the card is drawn. The card is opaque on its own layer
            // (`modal_frame` + `Order::Foreground`); the dim is what the reference does with the rest
            // of the window, and what makes "nothing of the listing shows through" a property of the
            // frame rather than of the colour's alpha.
            paint_backdrop(ui, tokens);
        }
        if let Some(dialog) = &mut self.active
            && let Some(answer) = dialog.show(ui, tokens)
        {
            out.push(answer);
            self.active = None;
        }
        out
    }
}

/// The dim over the shell, on its own layer: `Order::Middleware` sits above the panels
/// (`Order::Background`) and below the dialog card (`Order::Foreground`). It allocates **no input
/// sense** and is `interactable(false)`, so it cannot claim a hover or a click any widget owns
/// (rule 10: a losing claim repaints forever).
fn paint_backdrop(ui: &Ui, tokens: &Tokens) {
    let viewport = ui.input(|i| i.viewport_rect());
    Area::new(Id::unique("zenman-modal-backdrop"))
        // `Order::Middle` is this egui's layer between the panels (`Background`) and the dialog card
        // (`Foreground`) — there is no `Middleware` variant here.
        .order(Order::Middle)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .constrain(false)
        .interactable(false)
        .show(ui.ctx(), |ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            ui.allocate_space(viewport.size());
            let rect = ui.min_rect();
            ui.painter()
                .rect_filled(rect, CornerRadius::ZERO, tokens.modal_backdrop());
        });
}
