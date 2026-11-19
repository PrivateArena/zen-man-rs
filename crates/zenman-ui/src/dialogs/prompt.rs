//! The single-line text modal (plan §5.8 `dialogs/prompt.rs`, §7 row 10).
//!
//! Go asked for every one of these strings with `window.prompt(message, default)` and then, for a
//! bad answer, with a second `alert()` — the dialog was gone by then, so the user had to start the
//! whole thing over. The port keeps Go's exact wording and its validation rule but keeps the dialog
//! open and shows the refusal **inline** under the field, which is this step's acceptance criterion
//! ("validates … before submitting", plan §5.8).
//!
//! <kbd>Enter</kbd> accepts, <kbd>Escape</kbd> cancels, and the field owns the keyboard from the
//! frame it opens (plan F15: that is also what stops the panes eating the same keystrokes).

use egui::{
    Align2, Area, Key, Order, RichText, TextEdit, Ui, Vec2,
    text::{CCursor, CCursorRange},
    widgets::text_edit::TextEditState,
};

use crate::dialogs::confirm::{Choice, modal_frame};
use crate::dialogs::{Answer, DialogKind, Dialogs};
use crate::theme::Tokens;

/// Which rule an answer is checked against. An enum rather than the plan's
/// `Option<fn(&str) -> Option<String>>` so a dialog stays `Eq`/`Clone` (a test can assert what is
/// open); the two rules below are the only two this step has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Validation {
    /// One legal name component ([`zenman_core::path::validate_dest_name`]) that is not taken.
    Name,
    /// Three or four octal digits ([`zenman_core::mode::parse_octal`], Go's `/^[0-7]{3,4}$/`).
    Octal,
    /// Anything non-empty.
    None,
}

/// A labelled text field with an accept and a Cancel button.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prompt {
    /// What the answer is *about*; comes back verbatim in [`Answer::Text`].
    pub kind: DialogKind,
    /// The heading (Go's `prompt()` had none; ours names the operation).
    pub title: String,
    /// The body — Go's `prompt()` message verbatim, multi-line where Go's was.
    pub label: String,
    /// The editable text, seeded with Go's default (`'New Folder'`, `'755'`, the old name).
    pub buffer: String,
    /// The accept button's label.
    pub ok_label: String,
    /// The rule an answer must satisfy.
    pub validate: Validation,
    /// Names that already exist at the destination — the inline "taken" case.
    pub taken: Vec<String>,
    /// The refusal currently on screen.
    pub error: Option<String>,
    /// Until the first frame, the field holds the keyboard.
    pub fresh: bool,
    /// How many leading characters get selected on open (the base name, extension excluded).
    pub select: Option<usize>,
}

impl Prompt {
    /// A prompt with Go's `Cancel` button and no inline error yet.
    #[must_use]
    pub fn new(
        kind: DialogKind,
        title: impl Into<String>,
        label: impl Into<String>,
        initial: impl Into<String>,
        ok_label: impl Into<String>,
        validate: Validation,
    ) -> Self {
        Self {
            kind,
            title: title.into(),
            label: label.into(),
            buffer: initial.into(),
            ok_label: ok_label.into(),
            validate,
            taken: Vec::new(),
            error: None,
            fresh: true,
            select: None,
        }
    }

    /// Why this answer is refused — Go's copy where Go had one (`alert`'s text, `:489`).
    #[must_use]
    pub fn rejection(&self) -> Option<String> {
        let text = self.buffer.trim();
        match self.validate {
            Validation::None => {
                if text.is_empty() {
                    Some("nothing to do: the field is empty".to_string())
                } else {
                    None
                }
            }
            Validation::Octal => zenman_core::mode::parse_octal(text).err().map(|_| {
                // `frontend/js/context-menu.js:489`, verbatim.
                "Invalid permission format. Please enter a 3 or 4-digit octal number \
                 (e.g., 755 or 644)."
                    .to_string()
            }),
            Validation::Name => {
                if let Err(e) = zenman_core::path::validate_dest_name(text) {
                    return Some(e.user_message());
                }
                if self.taken.iter().any(|name| name == text) {
                    return Some(format!("\"{text}\" already exists"));
                }
                None
            }
        }
    }

    /// Draw it; `Some(answer)` once the user has answered.
    pub fn show(&mut self, ui: &mut Ui, tokens: &Tokens) -> Option<Answer> {
        let width = tokens.dialog_width - tokens.dialog_padding * 2.0;
        // Read the two decisive keys *before* the field is allocated, and consume them: a focused
        // `TextEdit` would otherwise swallow <kbd>Enter</kbd> (and, in this fork, may swallow
        // <kbd>Escape</kbd>), and the dialog is the only thing that may answer them.
        let enter = consume_key(ui, Key::Enter);
        let escape = consume_key(ui, Key::Escape);
        let mut accept = false;
        let mut cancel = false;
        let title = self.title.clone();
        let label = self.label.clone();
        let ok_label = self.ok_label.clone();
        let error = self.error.clone();
        let mut fresh = self.fresh;
        Area::new(
            ui.scope_id()
                .with(format!("zenman-prompt-{}", title.clone())),
        )
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .order(Order::Foreground)
        .constrain(true)
        .show(ui.ctx(), |ui| {
            modal_frame(tokens, ui, width, |ui| {
                ui.spacing_mut().item_spacing = Vec2::new(0.0, tokens.dialog_gap);
                ui.label(RichText::new(title).color(tokens.text_main).strong());
                for line in label.lines() {
                    ui.label(RichText::new(line).color(tokens.text_muted));
                }
                let field = ui.add(
                    TextEdit::singleline(&mut self.buffer)
                        .desired_width(width - tokens.cell_padding * 2.0)
                        .font(tokens.font()),
                );
                if fresh {
                    field.request_focus();
                    // The selection lives in the widget's *stored* state, which exists only once
                    // it has been shown, so the highlight lands one frame later — before any
                    // keystroke can arrive.
                    if let Some(len) = self.select {
                        select_prefix(ui.ctx(), field.id, len);
                    }
                    fresh = false;
                }
                if let Some(error) = error {
                    ui.label(RichText::new(error).color(tokens.danger));
                }
                ui.horizontal(|ui| {
                    if ui
                        .button(RichText::new(ok_label.as_str()).color(tokens.text_main))
                        .clicked()
                    {
                        accept = true;
                    }
                    if ui
                        .button(RichText::new("Cancel").color(tokens.text_muted))
                        .clicked()
                    {
                        cancel = true;
                    }
                });
            });
        });
        self.fresh = fresh;
        if accept || enter {
            match self.rejection() {
                // The criterion: refuse *here*, with the reason, and stay open.
                Some(rejection) => {
                    self.error = Some(rejection);
                    return None;
                }
                None => {
                    self.error = None;
                    return Some(Answer::Text {
                        kind: self.kind.clone(),
                        choice: Choice::Ok,
                        text: self.buffer.clone(),
                    });
                }
            }
        }
        if cancel || escape {
            return Some(Answer::Text {
                kind: self.kind.clone(),
                choice: Choice::Cancel,
                text: self.buffer.clone(),
            });
        }
        None
    }
}

impl Dialogs {
    /// Queue a prompt (the caller's convenience; `open` still refuses a second dialog).
    pub fn open_prompt(&mut self, prompt: Prompt) -> bool {
        self.open(crate::dialogs::Dialog::Prompt(prompt))
    }
}

/// Take a key out of the event list and report whether it was there.
fn consume_key(ui: &mut Ui, key: Key) -> bool {
    ui.input_mut(|input| {
        let pressed = input.key_pressed(key);
        if pressed {
            input.consume_key(egui::Modifiers::NONE, key);
        }
        pressed
    })
}

/// Select `[0, len)` in a field that has just been shown.
fn select_prefix(ctx: &egui::Context, id: egui::Id, len: usize) {
    if let Some(mut state) = TextEditState::load(ctx, id) {
        state
            .cursor
            .set_char_range(Some(CCursorRange::two(CCursor::new(0), CCursor::new(len))));
        state.store(ctx, id);
    }
}
