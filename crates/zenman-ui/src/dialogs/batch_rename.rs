//! Batch rename (plan §5.8 `dialogs/batch_rename.rs`, §7 row 14).
//!
//! The seven inputs Go's `batch-rename.js` had, the live preview table with its three statuses, and
//! Apply through `Session::batch_rename` (one `action.log` record, one `DirChanged` reload per
//! affected directory — core's guarantee, not this dialog's).
//!
//! # The preview is recomputed on *change*, never per frame
//!
//! 14a measured the pure preview at ~2.1 ms median for 10 000 rows in release, and the plan's claim is
//! "recomputed on any input change". That is what [`BatchRename::show`] does: it recomputes when one of
//! the inputs reports `changed()`, and never otherwise — so an idle dialog with a repaint running (a
//! job spinner, a blinking caret) costs nothing. [`BatchRename::computes`] counts the recomputes so a
//! test can assert exactly that.
//!
//! # Go's two kept bugs, visible here
//!
//! * the `case` checkbox is wired to the `i` **flag**, so *checked means ignore case* — Go's label
//!   says the opposite; this dialog labels the behaviour and keeps the semantics;
//! * with the extension box ticked, `previewName` is the *stem* alone, i.e. **the extension is
//!   dropped** (`batch-rename.js:281`). Kept, and named in the field's hover text.
//!
//! A bad pattern is not an error row: it is one panel-level message and every target left untouched
//! (`PreviewSet::regex_error`), which is what the browser showed.

use std::collections::HashSet;

use egui::{Align2, Area, Id, Key, Order, Rect, RichText, ScrollArea, Sense, Ui, Vec2, vec2};

use zenman_core::ops::ConflictPolicy;
use zenman_core::ops::batch_rename::{PreviewSet, Row, RowStatus, Rules};
use zenman_core::session::{BatchRenameReport, Session};
use zenman_core::store::config::PaneSlot;

use crate::app::ZenApp;
use crate::dialogs::confirm::modal_frame;
use crate::dialogs::{Answer, Dialog, DialogKind};
use crate::state::Tab;
use crate::theme::Tokens;
use crate::widgets::toast;

/// What the dialog can ask the shell to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatchOutcome {
    /// Run the batch. The rows and rules travel with the answer: the dialog never touches the
    /// engine (plan §4.1), and the subject is the *slot it opened from*, not the focused one.
    Apply {
        /// The rows, in selection order.
        rows: Vec<Row>,
        /// The rules as typed.
        rules: Rules,
        /// How a duplicate target is handled.
        policy: ConflictPolicy,
    },
    /// <kbd>Escape</kbd> — Go's `closeBatchRename`, which restores the pane titles it never changed.
    Closed,
}

/// The dialog's state: the rules, the rows they run over, and the preview they produced.
#[derive(Clone, Debug)]
pub struct BatchRename {
    /// The pane this batch belongs to.
    pub slot: PaneSlot,
    /// The selection, frozen when the dialog opened: the shell reloads the pane under a dialog, and
    /// a preview whose rows move between keystrokes is a preview of nothing.
    pub rows: Vec<Row>,
    /// The rules form.
    pub rules: Rules,
    /// The directory's other names (Go's `otherExistingNames`), for the collision pass.
    pub existing: HashSet<String>,
    /// The last preview.
    pub preview: PreviewSet,
    /// `Rename` numbers the duplicates (the port's second pass); `SkipExisting` declines them.
    pub policy: ConflictPolicy,
    /// Recomputes so far — the "not per frame" assertion.
    pub computes: usize,
    /// Until the first frame, the find field holds the keyboard.
    pub fresh: bool,
}

/// The preview table's columns, in Go's dry-run words (15c D12).
pub const COLUMNS: [&str; 3] = ["Original Name", "New Name", "Status"];

/// One cell row of the preview table — what [`BatchRename::table`] builds and [`preview_row`] paints,
/// so the text a test asserts on is the text the frame shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewCell {
    /// The name the row has.
    pub original: String,
    /// The name it would get.
    pub preview: String,
    /// The status column's words.
    pub status: String,
    /// Which colour the row gets.
    pub kind: RowStatus,
}

/// The table's three columns as cells of a row rect (15c D12: the columns, not a `old → new` string).
#[must_use]
pub fn preview_cells(rect: Rect) -> [Rect; 3] {
    let shares = [0.36, 0.36, 0.28];
    let mut out = [Rect::NOTHING; 3];
    let mut x = rect.left();
    for (slot, share) in out.iter_mut().zip(shares) {
        let width = rect.width() * share;
        *slot = Rect::from_min_size(egui::pos2(x, rect.min.y), vec2(width, rect.height()));
        x += width;
    }
    out
}

/// Did the rename drop the extension? Go's `Rename Extension Too` box previews the **stem** alone
/// (`batch-rename.js:281`), which is why `a.txt` legitimately renames to `a` — 15c D12 checked, and it
/// is the kept quirk, not a truncated paint.
#[must_use]
pub fn extension_dropped(original: &str, preview: &str) -> bool {
    original.rfind('.').is_some() && preview.rfind('.').is_none()
}

/// The status column's words: Go's dry-run labels, plus the reason when the reason is the extension
/// box (15c D12 — the frame has to *say* what happened).
#[must_use]
pub fn row_status_text(kind: RowStatus, dropped_extension: bool) -> &'static str {
    match (kind, dropped_extension) {
        (RowStatus::Success, true) => "RENAMED (extension dropped)",
        (RowStatus::Success, false) => "RENAMED",
        (RowStatus::Unchanged, _) => "UNCHANGED",
        (RowStatus::Conflict, _) => "CONFLICT",
    }
}

impl BatchRename {
    /// Build the dialog and run the first preview (the table is never empty on open).
    #[must_use]
    pub fn new(slot: PaneSlot, rows: Vec<Row>, rules: Rules, existing: HashSet<String>) -> Self {
        let preview = Session::batch_rename_preview(&rows, &rules, &existing);
        Self {
            slot,
            rows,
            rules,
            existing,
            preview,
            policy: ConflictPolicy::SkipExisting,
            computes: 1,
            fresh: true,
        }
    }

    /// The preview as the table paints it (15c D12). Built once per draw, from the last preview — the
    /// `computes` counter still says how often the *engine* ran.
    #[must_use]
    pub fn table(&self) -> Vec<PreviewCell> {
        self.preview
            .rows
            .iter()
            .map(|row| PreviewCell {
                original: row.original.clone(),
                preview: row.preview.clone(),
                kind: row.status,
                status: {
                    let words = row_status_text(
                        row.status,
                        self.rules.include_extension
                            && row.status == RowStatus::Success
                            && extension_dropped(&row.original, &row.preview),
                    );
                    if row.status == RowStatus::Conflict && !row.reason.is_empty() {
                        format!("{words} ({})", row.reason)
                    } else {
                        words.to_string()
                    }
                },
            })
            .collect()
    }

    /// Re-run the pure preview (the 2 ms / 10 k figure) and count it.
    pub fn recompute(&mut self) {
        self.preview = Session::batch_rename_preview(&self.rows, &self.rules, &self.existing);
        self.computes += 1;
    }

    /// Apply is blocked by a bad pattern and by any unresolved conflict (plan §5.8's criterion).
    #[must_use]
    pub fn can_apply(&self) -> bool {
        self.preview.regex_error.is_none()
            && self.preview.changed() > 0
            && (!self.preview.has_conflicts() || self.policy == ConflictPolicy::Rename)
    }

    /// Draw it.
    pub fn show(&mut self, ui: &mut Ui, tokens: &Tokens) -> Option<Answer> {
        let escape = ui.input_mut(|input| {
            let pressed = input.key_pressed(Key::Escape);
            if pressed {
                input.consume_key(egui::Modifiers::NONE, Key::Escape);
            }
            pressed
        });
        let mut outcome: Option<BatchOutcome> = None;
        let mut changed = false;
        let width = tokens.dialog_width * 2.0;
        let mut fresh = self.fresh;
        Area::new(Id::unique("zenman-batch-rename"))
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .order(Order::Foreground)
            .constrain(true)
            .show(ui.ctx(), |ui| {
                modal_frame(tokens, ui, width, |ui| {
                    ui.spacing_mut().item_spacing = vec2(0.0, tokens.form_gap);
                    ui.label(
                        RichText::new(format!("Batch rename — {} item(s)", self.rows.len()))
                            .color(tokens.text_main)
                            .strong(),
                    );
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Find").color(tokens.text_muted));
                        let mut find = self.rules.find.clone();
                        let field = ui.add_sized(
                            vec2(tokens.form_field_width, tokens.menu_item_height),
                            egui::TextEdit::singleline(&mut find).hint_text("text or pattern"),
                        );
                        if field.changed() {
                            self.rules.find = find;
                            changed = true;
                        }
                        if fresh {
                            field.request_focus();
                            fresh = false;
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Replace").color(tokens.text_muted));
                        let mut replace = self.rules.replace.clone();
                        let field = ui.add_sized(
                            vec2(tokens.form_field_width, tokens.menu_item_height),
                            egui::TextEdit::singleline(&mut replace)
                                .hint_text("[n] and [n:W] count, $1 groups in regex"),
                        );
                        if field.changed() {
                            self.rules.replace = replace;
                            changed = true;
                        }
                    });
                    ui.horizontal(|ui| {
                        changed |= ui.checkbox(&mut self.rules.regex, "Regex").changed();
                        changed |= ui
                            .checkbox(&mut self.rules.ignore_case, "Ignore case")
                            .on_hover_text("Go's `case` box: checked means *ignore* case")
                            .changed();
                        changed |= ui
                            .checkbox(&mut self.rules.include_extension, "Whole name")
                            .on_hover_text(
                                "Go's `ext` box — ticking it drops the extension, as Go did",
                            )
                            .changed();
                    });
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Counter").color(tokens.text_muted));
                        changed |= ui
                            .add(egui::DragValue::new(&mut self.rules.counter_start))
                            .changed();
                        changed |= ui
                            .add(egui::DragValue::new(&mut self.rules.counter_step))
                            .changed();
                        let (start, step) = self.rules.counter();
                        ui.label(
                            RichText::new(format!("[n] = {start}, then + {step}"))
                                .color(tokens.text_muted)
                                .size(tokens.font_size - 1.0),
                        );
                    });
                    if let Some(error) = &self.preview.regex_error {
                        let line = error.clone();
                        ui.label(
                            RichText::new(format!("pattern error: {line} — nothing was changed"))
                                .color(tokens.danger),
                        );
                    }
                    let conflicts = self.preview.conflicts().count();
                    if let Some(conflict) = self.preview.first_conflict() {
                        let reason = if conflict.reason.is_empty() {
                            conflict.preview.clone()
                        } else {
                            conflict.reason.clone()
                        };
                        let original = conflict.original.clone();
                        ui.label(
                            RichText::new(format!(
                                "{original}: {reason} — {conflicts} conflict(s)"
                            ))
                            .color(tokens.warning),
                        );
                        let before = self.policy;
                        let mut number = self.policy == ConflictPolicy::Rename;
                        ui.checkbox(
                            &mut number,
                            "number the duplicates instead of skipping them",
                        );
                        if number != (before == ConflictPolicy::Rename) {
                            self.policy = if number {
                                ConflictPolicy::Rename
                            } else {
                                ConflictPolicy::SkipExisting
                            };
                            changed = true;
                        }
                    }
                    ui.label(
                        RichText::new(format!(
                            "{} item(s) will be renamed",
                            self.preview.changed()
                        ))
                        .color(tokens.text_main),
                    );
                    // The preview is a **table** (15c D12): Go's dry-run view gave every row its
                    // original name, its new name and a status, and the old `old → new` line read as a
                    // truncation exactly where Go's kept extension quirk had done something real.
                    let table = self.table();
                    preview_header(ui, tokens);
                    ScrollArea::vertical()
                        .max_height(tokens.log_row_height * tokens.dropdown_visible_rows)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for cell in &table {
                                preview_row(ui, cell, tokens);
                            }
                        });
                    let ok = self.can_apply();
                    ui.horizontal(|ui| {
                        if ui.add_enabled(ok, egui::Button::new("Apply")).clicked() {
                            outcome = Some(BatchOutcome::Apply {
                                rows: self.rows.clone(),
                                rules: self.rules.clone(),
                                policy: self.policy,
                            });
                        }
                        if ui.button("Cancel").clicked() {
                            outcome = Some(BatchOutcome::Closed);
                        }
                    });
                });
            });
        self.fresh = fresh;
        if changed {
            self.recompute();
        }
        outcome
            .or_else(|| escape.then_some(BatchOutcome::Closed))
            .map(|outcome| Answer::Batch {
                kind: DialogKind::BatchRename { slot: self.slot },
                outcome,
            })
    }
}

/// The table's header, allocated as labels so the a11y tree carries it (15c D12).
fn preview_header(ui: &mut Ui, tokens: &Tokens) {
    let (rect, _) = ui.allocate_exact_size(
        vec2(ui.available_width(), tokens.header_row_height),
        Sense::hover(),
    );
    for (label, cell) in COLUMNS.iter().zip(preview_cells(rect)) {
        crate::widgets::in_rect(
            ui,
            cell,
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.label(RichText::new(*label).color(tokens.text_muted));
            },
        );
    }
    ui.advance_cursor_after_rect(rect);
}

/// One table line: three disjoint cells, coloured by status (15c D12).
fn preview_row(ui: &mut Ui, cell: &PreviewCell, tokens: &Tokens) {
    let colour = match cell.kind {
        RowStatus::Success => tokens.success,
        RowStatus::Unchanged => tokens.text_muted,
        RowStatus::Conflict => tokens.danger,
    };
    let (rect, _) = ui.allocate_exact_size(
        vec2(ui.available_width(), tokens.log_row_height),
        Sense::hover(),
    );
    let texts = [&cell.original, &cell.preview, &cell.status];
    for (text, cell_rect) in texts.iter().zip(preview_cells(rect)) {
        crate::widgets::in_rect(
            ui,
            cell_rect,
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.label(
                    RichText::new(text.as_str())
                        .color(colour)
                        .size(tokens.font_size - 1.0),
                );
            },
        );
    }
    ui.advance_cursor_after_rect(rect);
}

/// The rows + `existing` pair, split out so a test can call it without a shell.
#[must_use]
pub fn batch_rows_of(tab: &Tab) -> (Vec<Row>, HashSet<String>) {
    let mut rows = Vec::new();
    let mut existing = HashSet::new();
    for entry in &tab.entries.entries {
        let name = Tab::row_key(entry).to_string();
        if tab.sel.contains(Tab::row_key(entry)) {
            if let Some(path) = tab.row_abs(entry) {
                rows.push(Row::new(path, name, entry.is_dir));
            }
        } else {
            existing.insert(name);
        }
    }
    (rows, existing)
}

/// The dialog's own preview seam, so a test can name it without reaching into `Session`.
#[must_use]
pub fn preview(rows: &[Row], rules: &Rules, existing: &HashSet<String>) -> PreviewSet {
    Session::batch_rename_preview(rows, rules, existing)
}

// ------------------------------------------------------------------ the shell's answers

impl ZenApp {
    /// The selection as batch rows, plus the directory's other names (Go's `otherExistingNames`:
    /// the listing minus the selected rows, which is what a duplicate target is checked against).
    #[must_use]
    pub fn batch_rows(&self, slot: PaneSlot) -> (Vec<Row>, HashSet<String>) {
        self.model
            .panes
            .get(&slot)
            .and_then(|pane| pane.active_tab())
            .map_or_else(|| (Vec::new(), HashSet::new()), batch_rows_of)
    }

    /// Open the dialog for `slot`'s selection (Go's `triggerBatchRename`, and the >1-selection arm of
    /// `triggerRename`).
    pub fn open_batch_rename(&mut self, slot: PaneSlot) {
        let (rows, existing) = self.batch_rows(slot);
        if rows.len() < 2 {
            self.notices.post(
                toast::Level::Warning,
                "batch rename: select at least two rows",
            );
            return;
        }
        let dialog = BatchRename::new(slot, rows, Rules::default(), existing);
        if !self.open_dialog(Dialog::BatchRename(Box::new(dialog))) {
            self.notices.post(
                toast::Level::Warning,
                "batch rename: a dialog is already open",
            );
        }
    }

    /// `Session::batch_rename`, with the report's two halves spelled out (renamed, and declined).
    pub fn apply_batch_rename(
        &mut self,
        slot: PaneSlot,
        rows: Vec<Row>,
        rules: Rules,
        policy: ConflictPolicy,
    ) {
        let origin = self.session.next_origin();
        let existing = self.batch_rows(slot).1;
        match self
            .session
            .batch_rename(&rows, &rules, &existing, policy, origin)
        {
            Ok(report) => self.report_batch(&report),
            Err(e) => self.notices.post(
                toast::Level::Error,
                format!("batch rename: {}", e.user_message()),
            ),
        }
    }

    fn report_batch(&mut self, report: &BatchRenameReport) {
        let renamed = report.preview.changed();
        let skipped = report.skipped.len();
        self.notices.post(
            if skipped == 0 {
                toast::Level::Success
            } else {
                toast::Level::Warning
            },
            format!("batch rename: {renamed} renamed, {skipped} skipped"),
        );
    }
}
