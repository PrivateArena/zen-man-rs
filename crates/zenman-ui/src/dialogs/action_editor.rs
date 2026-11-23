//! The custom-action editor (plan §5.8 `dialogs/action_editor.rs`, §7 row 14).
//!
//! Create or edit one `CustomAction` — Go's seven fields, verbatim — with two live answers the web
//! editor never gave: *what the current selection this action would match* (`actions::for_context` /
//! `actions::matches`, the same predicates the context menu uses, so the dialog cannot disagree with
//! the menu), and a substituted preview of the command line (`actions::substitute`, 14a's
//! single-scan rewrite of Go's re-entrant `ReplaceAll`).
//!
//! # Test run, and why it is gated
//!
//! A test run goes through `Session::exec_action`, i.e. it **spawns**. Three gates, in this order:
//!
//! 1. the action must already be **saved** — `exec_action` looks the id up in the persisted config,
//!    so a draft cannot be run at all (and an unsaved draft has no id to answer for);
//! 2. the target must be **inside the pane's own directory** — the check is on the resolved paths,
//!    not on the template, because the template's substitutions are exactly what can escape;
//! 3. the output goes to a toast, capped by core (`limits.exec_output_cap`).
//!
//! The integration test for this dialog runs against a **recorder `Env`** that implements
//! `platform::Env` by delegating to `platform::stub::Unsupported` and *recording* the
//! `ExecRequest`: nothing is spawned, and the assertion is on the argv (14a's proof, reused).
//!
//! # Validation
//!
//! Go validated nothing until the request reached the server. Here Save is blocked by
//! [`ActionEditor::validate`]: an empty name or command, a `role` with no known word in it, and an
//! empty or whitespace-bearing glob entry in `patterns` (Go split on `,` and compiled each piece, so
//! `"a,,b"` produced an empty pattern that matched nothing and taught the user nothing).

use egui::{Align2, Area, Id, Key, Order, RichText, Ui, Vec2, vec2};

use zenman_core::path::AbsPath;
use zenman_core::services::actions::{ActionContext, TOKENS, for_context, matches, substitute};
use zenman_core::session::{ActionExec, Receipt};
use zenman_core::store::config::{CustomAction, PaneSlot};

use crate::app::ZenApp;
use crate::dialogs::confirm::modal_frame;
use crate::dialogs::{Answer, Dialog, DialogKind};
use crate::theme::Tokens;
use crate::widgets::toast;

/// What the dialog can ask the shell to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionOutcome {
    /// Persist the draft (`action_create` / `action_update`).
    Saved(CustomAction),
    /// Run it — the shell re-opens the dialog afterwards, because a test run must not cost the user
    /// their draft.
    TestRun(CustomAction),
    /// <kbd>Escape</kbd>.
    Closed,
}

/// The editor's state.
#[derive(Clone, Debug)]
pub struct ActionEditor {
    /// `Some` when editing an existing action (its id, Go's 16-hex string).
    pub editing: Option<String>,
    /// The draft.
    pub draft: CustomAction,
    /// The context the preview and the applicability count run against.
    pub context: ActionContext,
    /// The slot the dialog belongs to (the context's home, and where a test run is aimed).
    pub slot: PaneSlot,
    /// Validation failures, recomputed every frame (they are cheap, and stale errors are worse).
    pub errors: Vec<String>,
    /// Until the first frame, the name field holds the keyboard.
    pub fresh: bool,
}

impl ActionEditor {
    /// Edit an existing action, or draft a new one aimed at `slot`'s selection.
    #[must_use]
    pub fn new(slot: PaneSlot, draft: CustomAction, context: ActionContext) -> Self {
        let editing = if draft.id.is_empty() {
            None
        } else {
            Some(draft.id.clone())
        };
        Self {
            editing,
            draft,
            context,
            slot,
            errors: Vec::new(),
            fresh: true,
        }
    }

    /// The draft's own words, with Go's rules: name, command, role, and every pattern non-empty.
    pub fn validate(&mut self) {
        self.errors.clear();
        if self.draft.name.trim().is_empty() {
            self.errors.push("a menu name is required".to_string());
        }
        if self.draft.command.trim().is_empty() {
            self.errors.push("a command line is required".to_string());
        }
        let roles = zenman_core::services::actions::roles(&self.draft);
        if roles.is_empty() {
            self.errors
                .push("pick at least one of files / dirs / background".to_string());
        }
        if self.draft.patterns.contains(",,")
            || self
                .draft
                .patterns
                .split(',')
                .any(|p| !p.trim().is_empty() && p.trim() != p)
        {
            self.errors
                .push("each pattern must be a non-empty glob without spaces".to_string());
        }
    }

    /// Which of the current selection the draft matches (the plan's "applicability preview").
    #[must_use]
    pub fn matched(&self) -> usize {
        for_context(std::slice::from_ref(&self.draft), &self.context).len()
    }

    /// Per-row applicability: Go tests the *whole* context at once, so a selection of one directory
    /// and one file with `role: files` still shows the action; the count of matching *rows* is the
    /// port's own, and it is what the dialog prints.
    #[must_use]
    pub fn matching_rows(&self) -> usize {
        let dir = self.context.target().cloned();
        let targets: Vec<AbsPath> = self
            .context
            .files()
            .iter()
            .filter_map(|p| AbsPath::from_path(p).ok())
            .collect();
        targets
            .iter()
            .filter(|path| {
                let one = ActionContext::rows(dir.clone(), vec![(*path).clone()], false);
                matches(&self.draft, &one)
            })
            .count()
    }

    /// Draw it.
    pub fn show(&mut self, ui: &mut Ui, tokens: &Tokens) -> Option<Answer> {
        self.validate();
        let escape = ui.input_mut(|input| {
            let pressed = input.key_pressed(Key::Escape);
            if pressed {
                input.consume_key(egui::Modifiers::NONE, Key::Escape);
            }
            pressed
        });
        let preview = substitute(&self.draft.command, &self.context);
        let matched = self.matched();
        let rows = self.matching_rows();
        let tokens_list = TOKENS.join("  ");
        let mut outcome: Option<ActionOutcome> = None;
        let width = tokens.dialog_width * 2.0;
        let mut fresh = self.fresh;
        Area::new(Id::unique("zenman-action-editor"))
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .order(Order::Foreground)
            .constrain(true)
            .show(ui.ctx(), |ui| {
                modal_frame(tokens, ui, width, |ui| {
                    ui.spacing_mut().item_spacing = vec2(0.0, tokens.form_gap);
                    ui.label(
                        RichText::new(if self.editing.is_some() {
                            "Edit action"
                        } else {
                            "New action"
                        })
                        .color(tokens.text_main)
                        .strong(),
                    );
                    for (label, field) in [
                        ("Name", &mut self.draft.name),
                        ("Icon", &mut self.draft.icon),
                        ("Role", &mut self.draft.role),
                        ("Patterns", &mut self.draft.patterns),
                    ] {
                        let mut text = field.clone();
                        ui.horizontal(|ui| {
                            ui.add_sized(
                            vec2(tokens.form_label_width, tokens.menu_item_height),
                            egui::Label::new(RichText::new(label).color(tokens.text_muted)),
                        );
                            let f = ui.add_sized(
                                vec2(tokens.form_field_width, tokens.menu_item_height),
                                egui::TextEdit::singleline(&mut text),
                            );
                            if f.changed() {
                                field.clone_from(&text);
                            }
                            if fresh {
                                f.request_focus();
                                fresh = false;
                            }
                        });
                    }
                    let mut command = self.draft.command.clone();
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            vec2(tokens.form_label_width, tokens.menu_item_height),
                            egui::Label::new(RichText::new("Command").color(tokens.text_muted)),
                        );
                        let f = ui.add_sized(
                            vec2(tokens.form_field_width, tokens.menu_item_height),
                            egui::TextEdit::singleline(&mut command).hint_text("echo {name}"),
                        );
                        if f.changed() {
                            self.draft.command.clone_from(&command);
                        }
                    });
                    ui.label(
                        RichText::new(format!("tokens: {tokens_list}"))
                            .color(tokens.text_muted)
                            .size(tokens.font_size - 1.0),
                    );
                    ui.label(
                        RichText::new(format!("preview: {preview}"))
                            .color(tokens.accent)
                            .size(tokens.font_size - 1.0),
                    );
                    ui.label(
                        RichText::new(format!(
                            "matches this selection: {} of {} row(s)",
                            if matched > 0 { rows } else { 0 },
                            self.context.files().len()
                        ))
                        .color(if matched > 0 { tokens.success } else { tokens.text_muted })
                        .size(tokens.font_size - 1.0),
                    );
                    ui.checkbox(&mut self.draft.show_output, "show the output");
                    for error in &self.errors {
                        let line = error.clone();
                        ui.label(RichText::new(line).color(tokens.danger).size(tokens.font_size - 1.0));
                    }
                    ui.horizontal(|ui| {
                        let ok = self.errors.is_empty();
                        if ui.add_enabled(ok, egui::Button::new("Save")).clicked() {
                            outcome = Some(ActionOutcome::Saved(self.draft.clone()));
                        }
                        let runnable = self.editing.is_some();
                        let run = ui.add_enabled(runnable, egui::Button::new("Test run"));
                        if run.clicked() {
                            outcome = Some(ActionOutcome::TestRun(self.draft.clone()));
                        }
                        if runnable {
                            run.on_hover_text("the saved action runs; save first to test a draft");
                        } else {
                            run.on_hover_text(
                                "a test run spawns: save the action first, and it runs on this selection only when every target is inside the pane's own directory",
                            );
                        }
                        if ui.button("Cancel").clicked() {
                            outcome = Some(ActionOutcome::Closed);
                        }
                    });
                });
            });
        self.fresh = fresh;
        outcome
            .or_else(|| escape.then_some(ActionOutcome::Closed))
            .map(|outcome| Answer::Action {
                kind: DialogKind::ActionEditor { slot: self.slot },
                outcome,
            })
    }
}

// ------------------------------------------------------------------ the shell's answers

impl ZenApp {
    /// The action context a pane's selection means (also what the *menu* filters with, so the two
    /// surfaces cannot disagree about which actions apply).
    #[must_use]
    pub fn action_context(&self, slot: PaneSlot) -> ActionContext {
        let dir = self.shown_dir_abs(slot);
        let Some(tab) = self.model.panes.get(&slot).and_then(|p| p.active_tab()) else {
            return ActionContext::background(dir);
        };
        let mut targets = Vec::new();
        let mut all_dirs = true;
        let mut any = false;
        for entry in &tab.entries.entries {
            if tab.sel.contains(crate::state::Tab::row_key(entry)) {
                any = true;
                all_dirs &= entry.is_dir;
                if let Some(path) = tab.row_abs(entry) {
                    targets.push(path);
                }
            }
        }
        if !any {
            ActionContext::background(dir)
        } else {
            ActionContext::rows(dir, targets, all_dirs)
        }
    }

    /// Open the editor for a new action, or for `action`.
    pub fn open_action_editor(&mut self, slot: PaneSlot, action: Option<CustomAction>) {
        let context = self.action_context(slot);
        let draft = action.unwrap_or_default();
        let editor = ActionEditor::new(slot, draft, context);
        if !self.open_dialog(Dialog::ActionEditor(Box::new(editor))) {
            self.notices.post(
                toast::Level::Warning,
                "action editor: a dialog is already open",
            );
        }
    }

    /// `Session::action_create` / `action_update`, chosen by whether the id is already stored.
    pub fn save_action(&mut self, draft: CustomAction) {
        let known = self
            .session
            .custom_actions()
            .iter()
            .any(|a| a.id == draft.id && !draft.id.is_empty());
        let result = if known {
            self.session.action_update(&draft)
        } else {
            let mut fresh = draft.clone();
            if fresh.id.is_empty() {
                fresh.id = zenman_core::services::actions::generate_id();
            }
            self.session.action_create(fresh)
        };
        match result {
            Ok(list) => {
                self.notices.post(
                    toast::Level::Success,
                    format!("action saved ({} total)", list.len()),
                );
            }
            Err(e) => self
                .notices
                .post(toast::Level::Error, format!("action: {}", e.user_message())),
        }
    }

    /// Run a *saved* action on `slot`'s selection, with the two safety gates (module docs).
    pub fn run_action(&mut self, slot: PaneSlot, draft: &CustomAction) {
        let context = self.action_context(slot);
        self.run_action_in(slot, draft, context);
    }

    /// …on a caller-supplied context, which is what lets the **folder** menu run an action against the
    /// path it was opened over — Go's `custom-action-for-path` (`context-menu.js:710-716`) — under the
    /// same two gates.
    pub fn run_action_in(&mut self, slot: PaneSlot, draft: &CustomAction, context: ActionContext) {
        let Some(dir) = context
            .target_dir()
            .and_then(|d| AbsPath::from_path(&d).ok())
        else {
            self.notices
                .post(toast::Level::Warning, "action: no directory to run in");
            return;
        };
        let escaped = context.files().iter().any(|path| {
            AbsPath::from_path(path).is_ok_and(|p| !p.as_path().starts_with(dir.as_path()))
        });
        if escaped {
            self.notices.post(
                toast::Level::Warning,
                "action refused: a target is outside the pane's directory",
            );
            return;
        }
        self.exec_action(slot, &draft.id, context);
    }

    /// `Session::exec_action` and its two receipts (detached/done vs a watching job).
    pub fn exec_action(&mut self, slot: PaneSlot, id: &str, context: ActionContext) {
        let origin = self.session.next_origin();
        let req = ActionExec {
            id: id.to_string(),
            ctx: context,
        };
        match self.session.exec_action(&req, origin) {
            Ok(Receipt::Done(outcome)) => {
                let text = if outcome.output.trim().is_empty() {
                    "done".to_string()
                } else {
                    outcome.trimmed().to_string()
                };
                self.notices.post(
                    if outcome.ok() {
                        toast::Level::Success
                    } else {
                        toast::Level::Error
                    },
                    format!("action: {text}"),
                );
            }
            Ok(Receipt::Running { .. }) => {
                let _ = slot;
                self.notices.post(
                    toast::Level::Info,
                    "action started: the job panel has the output",
                );
            }
            Err(e) => self
                .notices
                .post(toast::Level::Error, format!("action: {}", e.user_message())),
        }
    }
}
