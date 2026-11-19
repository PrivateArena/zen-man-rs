//! The yes/no/(three-way) modal (plan §5.8 `dialogs/confirm.rs`, §7 row 10).
//!
//! Go had exactly one of these per question, and it was the browser's: `window.confirm` answers a
//! boolean, so a question could never carry more than OK/Cancel and its buttons said "OK"/"Cancel".
//! The port keeps Go's **wording** — every string here is handed in by the caller, copied from
//! `frontend/js/context-menu.js` — and gains what Go's API could not: a *named* primary button
//! ("Delete permanently"), red danger styling on it, detail lines, and a third answer, which is what
//! the paste-conflict question needs (overwrite / skip / rename).
//!
//! # Layering and the keyboard
//!
//! The card is an `Area` on `Order::Foreground`, allocated after every panel, the dock and the menu
//! (see [`crate::dialogs`]), so it wins the pointer by construction rather than by hoping. Its two
//! decisive keys are read out of the event queue and *consumed* before anything else can see them,
//! which is the same trick the in-cell rename uses: in this fork a widget that consumes a key wins
//! it, and a dialog that loses <kbd>Escape</kbd> is a dialog nobody can leave.

use egui::{Align2, Area, CornerRadius, Frame, Key, Margin, Order, RichText, Stroke, Ui, Vec2};

use crate::dialogs::{Answer, DialogKind};
use crate::theme::Tokens;

/// The answer a confirm button carries (plan §4.1: a dialog result is a command, not a side effect).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    /// The dialog's own "yes" (Go's `confirm(...) === true`).
    Ok,
    /// The dialog's "no", and what <kbd>Escape</kbd> always means.
    Cancel,
    /// Paste conflict: replace what is there (`ConflictPolicy::Overwrite`).
    Overwrite,
    /// Paste conflict: keep what is there (`ConflictPolicy::SkipExisting`).
    Skip,
    /// Paste conflict: keep both (`ConflictPolicy::Rename`).
    Rename,
}

/// One button of a confirm.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfirmButton {
    /// Its label — Go's copy where Go had one.
    pub label: String,
    /// What pressing it answers.
    pub choice: Choice,
    /// Red text: the same `tokens.danger` the menus' danger rows use (plan D6's visual half).
    pub danger: bool,
}

impl ConfirmButton {
    /// A plain button.
    #[must_use]
    pub fn new(label: impl Into<String>, choice: Choice) -> Self {
        Self {
            label: label.into(),
            choice,
            danger: false,
        }
    }

    /// A red button — the destructive answer.
    #[must_use]
    pub fn danger(label: impl Into<String>, choice: Choice) -> Self {
        Self {
            danger: true,
            ..Self::new(label, choice)
        }
    }
}

/// A question with two, three or four named answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Confirm {
    /// What the answer is *about*; comes back verbatim in [`Answer::Button`].
    pub kind: DialogKind,
    /// The heading.
    pub title: String,
    /// The question, in Go's copy.
    pub question: String,
    /// The detail lines (which paths, and what "permanently" costs — plan D6).
    pub detail: Vec<String>,
    /// The buttons, in paint order; the first holds the keyboard, so it is the *safe* one.
    pub buttons: Vec<ConfirmButton>,
    /// Until the first frame, the default button holds the keyboard.
    pub fresh: bool,
}

impl Confirm {
    /// Build a question.
    #[must_use]
    pub fn new(
        kind: DialogKind,
        title: impl Into<String>,
        question: impl Into<String>,
        detail: Vec<String>,
        buttons: Vec<ConfirmButton>,
    ) -> Self {
        Self {
            kind,
            title: title.into(),
            question: question.into(),
            detail,
            buttons,
            fresh: true,
        }
    }

    /// The <kbd>Escape</kbd> answer: this dialog's own `Cancel` button when it has one, otherwise its
    /// last button — never its first, which is the one the dialog exists to ask about.
    #[must_use]
    pub fn cancel_choice(&self) -> Choice {
        self.buttons
            .iter()
            .find(|button| button.choice == Choice::Cancel)
            .or_else(|| self.buttons.last())
            .map_or(Choice::Cancel, |button| button.choice)
    }

    /// Draw it; `Some(answer)` once it has one.
    pub fn show(&mut self, ui: &mut Ui, tokens: &Tokens) -> Option<Answer> {
        let enter = consume_key(ui, Key::Enter);
        let escape = consume_key(ui, Key::Escape);
        let buttons = self.buttons.clone();
        let detail = self.detail.clone();
        let title = self.title.clone();
        let question = self.question.clone();
        let width = tokens.dialog_width - tokens.dialog_padding * 2.0;
        let mut pressed: Option<Choice> = None;
        let mut fresh = self.fresh;
        Area::new(ui.scope_id().with(format!("zenman-confirm-{title}")))
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .order(Order::Foreground)
            .constrain(true)
            .show(ui.ctx(), |ui| {
                modal_frame(tokens, ui, width, |ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(0.0, tokens.dialog_gap);
                    ui.label(RichText::new(title).color(tokens.text_main).strong());
                    ui.label(RichText::new(question).color(tokens.text_main));
                    for line in &detail {
                        ui.label(
                            RichText::new(line)
                                .color(tokens.text_muted)
                                .size(tokens.font_size - 1.0),
                        );
                    }
                    ui.add_space(tokens.dialog_gap);
                    ui.horizontal(|ui| {
                        for (ix, button) in buttons.iter().enumerate() {
                            let colour = if button.danger {
                                tokens.danger
                            } else {
                                tokens.text_main
                            };
                            let response = ui.button(
                                RichText::new(button.label.as_str())
                                    .color(colour)
                                    .size(tokens.font_size),
                            );
                            if ix == 0 && fresh {
                                response.request_focus();
                                fresh = false;
                            }
                            // <kbd>Enter</kbd> answers the *focused* default, which is button 0 — the
                            // safe one, by the construction every caller of this file follows.
                            if response.clicked() || (enter && ix == 0) {
                                pressed = Some(button.choice);
                            }
                        }
                    });
                });
            });
        self.fresh = fresh;
        let answer = pressed.or_else(|| escape.then(|| self.cancel_choice()));
        answer.map(|choice| Answer::Button {
            kind: self.kind.clone(),
            choice,
        })
    }
}

/// Take a key out of the event queue and report whether it was there.
fn consume_key(ui: &mut Ui, key: Key) -> bool {
    ui.input_mut(|input| {
        let pressed = input.key_pressed(key);
        if pressed {
            input.consume_key(egui::Modifiers::NONE, key);
        }
        pressed
    })
}

/// The card every dialog is drawn inside (the plan's `modal_frame`; `dock_content::modal_frame` is a
/// different thing — the drawers' frame, with no width contract).
pub(crate) fn modal_frame(tokens: &Tokens, ui: &mut Ui, width: f32, content: impl FnOnce(&mut Ui)) {
    Frame::new()
        .fill(tokens.bg_card)
        .stroke(Stroke::new(1.0, tokens.border))
        .corner_radius(CornerRadius::same(tokens.corner_radius))
        .inner_margin(Margin::same(tokens.dialog_padding as i8))
        .show(ui, |ui| {
            ui.set_width(width);
            content(ui);
        });
}
