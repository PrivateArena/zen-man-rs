//! The action-log panel (plan §5.8 `panels/action_log.rs`, §7 row 14).
//!
//! A `SidePanel` over `Session::log_query` — newest first, paged by offset, with the two flags the
//! engine folds into one answer (`LogRow::can_revert`, 14a) driving a single enabled Revert button.
//!
//! # Refresh policy
//!
//! The plan asks for "a 5 s poll **plus** an event-driven refresh on `EngineEvent::LogAppended`".
//! Both are here: [`LogPanel::mark_dirty`] is called from the event drain (`jobs_ui`), and
//! [`POLL_INTERVAL`] is the fallback for actions made by *another* process. Neither is per-frame:
//! [`tick`] only re-queries when it is dirty or the poll is due, and it only assigns (and only asks
//! for a repaint) when the page actually changed — a losing-hover repaint loop is the failure this
//! crate already learned about in step 8, and a page that is re-assigned every frame is the same bug
//! with extra steps.
//!
//! # Reverting
//!
//! `Session::revert` is a job *after* a synchronous guard: the two refusals (already reverted, and
//! no filesystem change to undo — including Go's "delete is permanent on SSD") come back as typed
//! `Err`s from [`ZenApp::revert_record`], which surfaces the message in the panel's own status strip
//! as well as in a toast, so the reason survives the toast's 6 s.
//!
//! # Time
//!
//! The row prints Go's vocabulary (step 15b, defect D10): the `timeAgo` bucket
//! (`frontend/js/action-log.js:34-43`) as `LogRow::ago`, the per-type icon (`:17-24`) and the red
//! `permanent` marker (`:105-108`) under the panel's own "Action History" title (`index.html:628`).
//! The relative text is computed in core (`metrics::format_relative_time`) because `zenman-ui` has
//! no `chrono` edge — which was step 14b's reason for printing RFC 3339 instead; parity wins.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use egui::{
    Color32, CornerRadius, Frame, Layout, Margin, RichText, ScrollArea, Sense, Stroke, Ui, vec2,
};

use zenman_core::history::store::LogId;
use zenman_core::services::action_log::{DEFAULT_LIMIT, LogPage, LogQuery, LogRow};
use zenman_core::session::Receipt;

use crate::app::ZenApp;
use crate::theme::Tokens;
use crate::widgets::toast;

/// The fallback poll — Go's `setInterval(…, 5000)`.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// What a click inside the panel means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogCommand {
    /// Undo this record (`Session::revert`).
    Revert(LogId),
    /// Show/hide a row's full source list.
    Toggle(LogId),
    /// The next (older) page.
    Older,
    /// The previous (newer) page.
    Newer,
    /// The panel's `×` — Go's `H` chord closes it too.
    Close,
}

/// The panel's state.
#[derive(Debug)]
pub struct LogPanel {
    /// The current page, newest first.
    pub page: LogPage,
    /// Rows per page.
    pub limit: usize,
    /// Rows skipped from the newest end.
    pub offset: usize,
    /// Rows whose source list is expanded.
    pub expanded: BTreeSet<LogId>,
    /// The last thing the panel did, in its own words.
    pub status: Option<String>,
    /// The last refusal, kept until the next attempt (a toast expires; a refusal should not).
    pub error: Option<String>,
    /// `true` between a `LogAppended` and the refresh it asks for.
    dirty: bool,
    /// When the page was last read (the poll's clock).
    queried_at: Option<Instant>,
    /// Set once, so the very first open fills the page.
    pub ever_queried: bool,
}

impl Default for LogPanel {
    fn default() -> Self {
        Self {
            page: LogPage::empty(),
            limit: DEFAULT_LIMIT,
            offset: 0,
            expanded: BTreeSet::new(),
            status: None,
            error: None,
            dirty: true,
            queried_at: None,
            ever_queried: false,
        }
    }
}

impl LogPanel {
    /// A `LogAppended` landed: the next tick must re-read (plan §5.8: no 5 s wait after your own
    /// action).
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Is a re-read due (dirty, never queried, or the poll elapsed)?
    #[must_use]
    pub fn wants_query(&self, now: Instant) -> bool {
        self.dirty
            || self
                .queried_at
                .is_none_or(|at| now.duration_since(at) >= POLL_INTERVAL)
    }

    /// The query this panel is showing (and would show next).
    #[must_use]
    pub fn query(&self) -> LogQuery {
        LogQuery {
            limit: Some(self.limit),
            offset: self.offset,
            path: None,
        }
    }

    /// Apply a command, returning the ones the shell must answer to the engine.
    pub fn apply(&mut self, command: LogCommand) -> Option<LogId> {
        match command {
            LogCommand::Revert(id) => Some(id),
            LogCommand::Toggle(id) => {
                if !self.expanded.remove(&id) {
                    self.expanded.insert(id);
                }
                None
            }
            LogCommand::Older => {
                self.offset += self.limit;
                self.dirty = true;
                None
            }
            LogCommand::Newer => {
                self.offset = self.offset.saturating_sub(self.limit);
                self.dirty = true;
                None
            }
            LogCommand::Close => None,
        }
    }
}

/// Refresh the page if it is due (see the module docs for why this is not per-frame).
pub fn tick(app: &mut ZenApp) {
    let now = Instant::now();
    if !app.model.log_panel || !app.log.wants_query(now) {
        return;
    }
    let query = app.log.query();
    let Ok(page) = app.session.log_query(&query) else {
        return;
    };
    app.log.queried_at = Some(now);
    app.log.dirty = false;
    app.log.ever_queried = true;
    if page != app.log.page {
        // Only a real change repaints, and only a real change resets the refusal strip: paging while
        // an error is up keeps the error where the user can still read it.
        app.log.page = page;
    }
}

/// Draw the panel.
pub fn show(app: &mut ZenApp, ui: &mut Ui) {
    let tokens = app.tokens;
    let mut commands: Vec<LogCommand> = Vec::new();
    let page = app.log.page.clone();
    let expanded = &app.log.expanded;
    Frame::new()
        .fill(tokens.bg_sidebar)
        .inner_margin(Margin::same(tokens.dialog_padding as i8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(TITLE).color(tokens.text_main).strong());
                ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("×").clicked() {
                        commands.push(LogCommand::Close);
                    }
                });
            });
            ui.label(
                RichText::new(DESCRIPTION)
                    .color(tokens.text_muted)
                    .size(tokens.font_size - 1.0),
            );
            if let Some(error) = &app.log.error {
                let strip = error.clone();
                ui.label(
                    RichText::new(strip)
                        .color(tokens.danger)
                        .size(tokens.font_size - 1.0),
                );
            }
            if let Some(status) = &app.log.status {
                let strip = status.clone();
                ui.label(
                    RichText::new(strip)
                        .color(tokens.text_muted)
                        .size(tokens.font_size - 1.0),
                );
            }
            if page.rows.is_empty() {
                ui.add_space(tokens.drawer_section_gap);
                ui.label(
                    RichText::new("No actions yet — nothing has been renamed, moved or deleted.")
                        .color(tokens.text_muted),
                );
                ui.label(
                    RichText::new("the log records every change the shell makes, newest first")
                        .color(tokens.text_muted)
                        .size(tokens.font_size - 1.0),
                );
            }
            ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for row in &page.rows {
                        let open = expanded.contains(&row.id);
                        record_row(ui, row, &tokens, open, &mut commands);
                    }
                });
            ui.horizontal(|ui| {
                if ui.button("Older").clicked() && page.has_more {
                    commands.push(LogCommand::Older);
                }
                if ui.button("Newer").clicked() && app.log.offset > 0 {
                    commands.push(LogCommand::Newer);
                }
            });
        });
    for command in commands {
        if let Some(id) = app.log.apply(command.clone()) {
            app.revert_record(id);
        }
        if command == LogCommand::Close {
            app.model.log_panel = false;
        }
    }
}

/// One record: the accent strip (paint-only), the label, the time, the sources, and the button.
///
/// The row allocates exactly one interaction rect per control (rule 10): the strip is a `Sense::hover`
/// allocation that is painted, never clicked, and the only clickables are egui's own buttons.
pub fn record_row(
    ui: &mut Ui,
    row: &LogRow,
    tokens: &Tokens,
    expanded: bool,
    out: &mut Vec<LogCommand>,
) {
    let colour = row_accent(row, tokens);
    let height = if expanded {
        tokens.log_row_height + tokens.log_inner_gap
    } else {
        tokens.log_row_height
    };
    Frame::new()
        .fill(tokens.bg_card)
        .stroke(Stroke::new(1.0, tokens.border))
        .corner_radius(CornerRadius::same(tokens.corner_radius))
        .inner_margin(Margin::same(tokens.row_pad_y as i8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (strip, _) =
                    ui.allocate_exact_size(vec2(tokens.log_accent_width, height), Sense::hover());
                if ui.is_rect_visible(strip) {
                    ui.painter().rect_filled(
                        strip,
                        CornerRadius::same(tokens.corner_radius),
                        colour,
                    );
                }
                // Go's per-type icon (`action-log.js:17-24`), paint-only beside the strip (rule 10).
                let (icon, _) = ui.allocate_exact_size(
                    vec2(tokens.row_icon_size + tokens.menu_icon_gap, height),
                    Sense::hover(),
                );
                crate::icons::paint(
                    ui,
                    icon,
                    row_icon(row.action),
                    tokens.row_icon_size,
                    tokens.text_muted,
                );
                ui.spacing_mut().item_spacing = vec2(tokens.log_inner_gap, 2.0);
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(shorten(&row.label, 200))
                            .color(tokens.text_main)
                            .strong(),
                    );
                    ui.label(
                        RichText::new(format!("{} · {}", row.ago, row.status))
                            .color(tokens.text_muted)
                            .size(tokens.font_size - 1.0),
                    );
                    let shown = if expanded {
                        row.sources.join(", ")
                    } else {
                        shorten(&row.sources.join(", "), 120)
                    };
                    ui.label(
                        RichText::new(if row.sources.is_empty() {
                            row.dest.clone()
                        } else {
                            shown
                        })
                        .color(tokens.text_muted)
                        .size(tokens.font_size - 1.0),
                    );
                    if !row.sources.is_empty()
                        && ui.button(if expanded { "less" } else { "more" }).clicked()
                    {
                        out.push(LogCommand::Toggle(row.id));
                    }
                });
                ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(marker) = trailing_marker(row) {
                        ui.label(
                            RichText::new(marker)
                                .color(tokens.danger)
                                .italics()
                                .size(tokens.font_size - 1.0),
                        );
                    }
                    let button = ui.add_enabled(
                        row.can_revert,
                        egui::Button::new(RichText::new("↩ Undo").color(if row.can_revert {
                            tokens.accent
                        } else {
                            tokens.disabled_text
                        })),
                    );
                    if button.clicked() {
                        out.push(LogCommand::Revert(row.id));
                    }
                });
            });
        });
    ui.add_space(tokens.drawer_section_gap);
}

/// The strip's colour, from the record's own state: red for an undo that cannot happen, green for
/// one that has, accent for the rest.
fn row_accent(row: &LogRow, tokens: &Tokens) -> Color32 {
    if row.reverted {
        tokens.success
    } else if row.reversible {
        tokens.accent
    } else {
        tokens.warning
    }
}

/// Trim a string for display, with the same ellipsis the file rows use.
fn shorten(text: &str, budget: usize) -> String {
    if text.chars().count() <= budget {
        return text.to_string();
    }
    let cut: String = text.chars().take(budget.saturating_sub(1)).collect();
    format!("{cut}…")
}

// ------------------------------------------------------------------ the shell's answers

impl ZenApp {
    /// Undo one record. Both of core's synchronous refusals land here as typed `Err`s and are
    /// surfaced *in the panel* as well as in a toast.
    pub fn revert_record(&mut self, id: LogId) {
        let origin = self.session.next_origin();
        match self.session.revert(id, origin) {
            Ok(Receipt::Running { .. }) => {
                self.log.status = Some(format!("reverting record {id}…"));
                self.log.error = None;
                self.log.mark_dirty();
            }
            Ok(Receipt::Done(_)) => {
                self.log.status = Some(format!("record {id} reverted"));
                self.log.error = None;
                self.log.mark_dirty();
            }
            Err(e) => {
                let message = e.user_message();
                self.log.error = Some(format!("revert {id}: {message}"));
                self.notices
                    .post(toast::Level::Error, format!("revert {id}: {message}"));
            }
        }
    }

    /// Toggle the drawer from the header's `History` button (Go's `#history-btn`).
    pub fn toggle_log_panel(&mut self) {
        self.model.log_panel = !self.model.log_panel;
        if self.model.log_panel {
            self.log.mark_dirty();
        }
    }
}

// ------------------------------------------------------------------- Go's vocabulary (15b)

/// Go's panel title (`index.html:628`).
pub const TITLE: &str = "Action History";

/// Go's description line under the title (`index.html:631`).
pub const DESCRIPTION: &str = "Recent file operations. Reversible actions can be undone instantly.";

/// Go's marker for a record that cannot be undone (`action-log.js:108`).
pub const PERMANENT: &str = "permanent";

/// The per-type icon — Go's `ACTION_META` (`action-log.js:17-24`) mapped onto this port's icon set,
/// because the reference's emoji are glyphs our installed font stack does not carry (rule 11).
#[must_use]
pub fn row_icon(action: &str) -> &'static str {
    match action {
        "copy" => "copy",
        "paste-copy" => "paste",
        "paste-move" => "cut",
        "delete" => "trash",
        "rename" => "edit",
        "mkdir" => "folder",
        "chmod" => "settings",
        "paste-link" => "link",
        _ => "info",
    }
}

/// What the row shows where `↩ Undo` would go: Go's `permanent` for a done record with no undo, and
/// nothing for one that has already been reverted.
#[must_use]
pub fn trailing_marker(row: &LogRow) -> Option<&'static str> {
    if row.can_revert || row.reverted {
        None
    } else {
        Some(PERMANENT)
    }
}
