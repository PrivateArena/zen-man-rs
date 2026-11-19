//! Extract progress + cancel (plan §7 row 13, step 13b).
//!
//! The UX decision, stated: **modeless**. One panel per running/finished extraction, anchored
//! bottom-left, each with its own bar, its own <kbd>Cancel</kbd> and its own summary. Go had no
//! progress UI at all for this (it polled `/api/archive/extract/status` from nowhere — the handler
//! exists, the caller does not), and the plan's §5.8 dialog list has no row for it, so the choice is
//! the shell's; the criteria it has to meet are "shows progress + cancel", and both survive a
//! modeless panel better than a modal: a modal would block the very pane whose directory the
//! extraction repopulates.
//!
//! The job itself is core's (`Session::archive_extract` → [`Receipt::Running`]); this module owns
//! only the polling and the drawing, in the `dir_size`/`paste` shape:
//!
//! * the `Receipt::Running` origin is registered in [`JobsUi`](crate::jobs_ui::JobsUi), so the status
//!   strip and the terminal toast behave like every other job's;
//! * progress is read from [`ExtractJob::state`](zenman_core::archive::extract::ExtractJob::state),
//!   which is a lock-free snapshot — the panel never waits;
//! * cancel is [`Session::archive_cancel`](zenman_core::session::Session::archive_cancel);
//! * the terminal state prints the [`ExtractReport`](zenman_core::archive::extract::ExtractReport)
//!   summary, and the destination reload is core's own `DirChanged` (the done hook invalidates the
//!   destination with the caller's origin), so the panel does not reload anything itself.
//!
//! The destination is the directory holding the archive — "extract here", the only destination the
//! pane knows without a file picker (step 16's platform work). The conflict policy is
//! [`ConflictPolicy`]'s parameter, threaded from the caller so a test can prove it is honoured.

use egui::{Align2, Area, Order, ProgressBar, RichText, Ui, Vec2};

use zenman_core::archive::extract::{ExtractJob, ExtractReq};
use zenman_core::jobs::JobId;
use zenman_core::ops::ConflictPolicy;
use zenman_core::path::AbsPath;
use zenman_core::session::{Origin, Receipt};
use zenman_core::store::config::PaneSlot;

use crate::app::ZenApp;
use crate::theme::Tokens;
use crate::widgets::disk_meter::format_size;
use crate::widgets::toast;

/// One extraction the shell started, and what it last reported.
#[derive(Clone, Debug)]
pub struct ExtractProgress {
    /// The engine's job id (also the panel's `Area` identity, via [`ExtractProgress::id`]).
    pub id: JobId,
    /// The correlation origin the receipt answered with.
    pub origin: Origin,
    /// Where the bytes land (also the panel's title).
    pub dest: AbsPath,
    /// The asking pane (the job strip's correlation, plan §4.3).
    pub slot: PaneSlot,
    /// The asking tab, when a tab did.
    pub tab: Option<crate::state::TabId>,
    /// The last fraction, from the job's own byte counters.
    pub frac: f32,
    /// Bytes written / expected, for the label (never a placeholder `0 of 0`).
    pub bytes_done: u64,
    /// `None` while the total is unknown — the honest `--`, not `0 B` (plan D11).
    pub bytes_total: Option<u64>,
    /// Still running.
    pub live: bool,
    /// The finished job's summary line, or its error.
    pub summary: String,
}

impl ExtractProgress {
    /// The panel's stable area id (scoped to the job, so two extracts are two panels).
    #[must_use]
    pub fn area_id(&self, scope: egui::Id) -> egui::Id {
        scope.with(("zenman-extract", self.id.0))
    }
}

/// Start one extraction of `path` (an archive store path) into the directory holding the archive.
///
/// `sources` is the selection's internal paths; empty means the whole archive (Go's
/// `sources: []` rule, `api/archive_extract.go:96`).
pub fn start(
    app: &mut ZenApp,
    slot: PaneSlot,
    path: &zenman_core::path::FsPath,
    sources: Vec<String>,
    policy: ConflictPolicy,
) {
    let Some(host) = path.store.host().cloned() else {
        app.notices
            .post(toast::Level::Warning, "extract needs an archive listing");
        return;
    };
    let Some(dest) = host.parent() else {
        app.notices.post(
            toast::Level::Error,
            "cannot work out where to extract the archive",
        );
        return;
    };
    let nested = path.store.nested().map(str::to_string);
    let mut req = if sources.is_empty() {
        ExtractReq::all(host.clone(), nested.clone(), dest.clone())
    } else {
        ExtractReq {
            host: host.clone(),
            nested,
            sources,
            dest: dest.clone(),
            policy,
        }
    };
    req.policy = policy;
    let origin = app.session.next_origin();
    match app.session.archive_extract(&req, origin) {
        Ok(Receipt::Running { job, origin }) => {
            let tab = app
                .model
                .panes
                .get(&slot)
                .and_then(|p| p.active_tab())
                .map(|t| t.id);
            if let Some(info) = app.session.job_info(job) {
                app.jobs.track(info, Some(slot), tab);
            }
            app.extracts.push(ExtractProgress {
                id: job,
                origin,
                dest,
                slot,
                tab,
                frac: 0.0,
                bytes_done: 0,
                bytes_total: None,
                live: true,
                summary: String::new(),
            });
        }
        // A trivially-empty job answers here and now; the report still has to be shown.
        Ok(Receipt::Done(report)) => {
            let text = summary_of(&report, false);
            app.notices.post(toast::Level::Success, text);
        }
        Err(e) => app.notices.post(toast::Level::Error, e.user_message()),
    }
}

/// Poll every running extraction once per frame (the shell's `logic`, next to the job drain).
pub fn tick(app: &mut ZenApp) {
    for progress in &mut app.extracts {
        if !progress.live {
            continue;
        }
        let job = app.session.archive_extract_status(progress.id);
        match job {
            Some(job) => {
                let state = job.state();
                progress.frac = fraction(&job);
                progress.bytes_done = state.bytes_done;
                progress.bytes_total = (state.bytes_total > 0).then_some(state.bytes_total);
                if let Some(error) = state.error {
                    // Go's status handler reported `error` verbatim; the panel says which of the two
                    // terminal failures it is, because the difference decides what is on disk.
                    let head = if error.to_lowercase().contains("cancel") {
                        "Extraction cancelled"
                    } else {
                        "Extraction failed"
                    };
                    progress.summary = format!("{head}: {error}");
                }
                if state.done {
                    progress.live = false;
                    if let Some(report) = job.report() {
                        progress.summary = summary_of(&report, was_cancelled(&job));
                    } else if progress.summary.is_empty() {
                        progress.summary = "extraction finished".to_string();
                    }
                }
            }
            // Collected (`archive_gc` past the retention window, or a job the engine forgot): the
            // panel stops claiming to be live rather than spinning forever.
            None => {
                progress.live = false;
                if progress.summary.is_empty() {
                    progress.summary = "extraction finished".to_string();
                }
            }
        }
    }
}

/// Ask the engine to stop one extraction (it answers `JobFinished{Cancelled}`; what was written
/// up to that point stays, which is why the summary reports the truth and not a rollback).
pub fn cancel(app: &mut ZenApp, id: JobId) {
    if app.session.archive_cancel(id) {
        app.notices
            .post(toast::Level::Info, "cancelling the extraction");
    } else {
        app.notices.post(
            toast::Level::Warning,
            "that extraction has already finished",
        );
    }
}

/// Draw every panel, oldest first, stacked up from the bottom-left (`tick` owns the data, this the
/// pixels; both are called from the shell, never from a pane).
pub fn show(app: &mut ZenApp, ui: &mut Ui, tokens: &Tokens) {
    let width = tokens.action_log_width;
    let mut close = Vec::new();
    // Index-based: a panel's own button may mutate `app.extracts` (cancel) or remove it (close),
    // and an iterator borrow would forbid exactly that.
    for ix in 0..app.extracts.len() {
        let progress = app.extracts[ix].clone();
        let rows = app.extracts.len().max(1);
        // Stack oldest-to-newest upward: one `dialog_gap` per panel below this one.
        let below = (rows - ix - 1) as f32;
        let offset_y = below * tokens.dialog_gap;
        let title = progress
            .dest
            .to_string_lossy()
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("Extract")
            .to_string();
        let live = progress.live;
        let summary = progress.summary.clone();
        let frac = progress.frac;
        let label = progress.line_label();
        let id = progress.area_id(ui.scope_id());
        let mut clicked_close = false;
        let mut clicked_cancel = false;
        Area::new(id)
            .anchor(
                Align2::LEFT_BOTTOM,
                Vec2::new(tokens.dialog_padding, -offset_y - tokens.dialog_gap),
            )
            .order(Order::Foreground)
            .constrain(true)
            .show(ui.ctx(), |ui| {
                super::confirm::modal_frame(tokens, ui, width, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("Extract — {title}"))
                                .color(tokens.text_main)
                                .strong(),
                        );
                    });
                    if live {
                        ui.add(ProgressBar::new(frac).animate(true));
                    } else {
                        ui.add(ProgressBar::new(frac).animate(false));
                    }
                    ui.label(RichText::new(label).color(tokens.text_muted));
                    if !summary.is_empty() {
                        ui.label(RichText::new(summary).color(tokens.text_main));
                    }
                    ui.horizontal(|ui| {
                        if live
                            && ui
                                .button(RichText::new("Cancel").color(tokens.danger))
                                .clicked()
                        {
                            clicked_cancel = true;
                        }
                        if !live
                            && ui
                                .button(RichText::new("Close").color(tokens.text_main))
                                .clicked()
                        {
                            clicked_close = true;
                        }
                    });
                });
            });
        if clicked_cancel {
            cancel(app, progress.id);
        }
        if clicked_close {
            close.push(ix);
        }
    }
    // Removed last, so the indices above were still the drawing order.
    for ix in close.into_iter().rev() {
        if app.extracts.len() > ix {
            app.extracts.remove(ix);
        }
    }
}

/// The panel's own line: real byte counts, or the unknown, never `0 of 0`.
impl ExtractProgress {
    #[must_use]
    pub fn line_label(&self) -> String {
        match self.bytes_total {
            Some(total) => format!(
                "{} of {} extracted",
                format_size(self.bytes_done),
                format_size(total)
            ),
            None if self.live => "extracting".to_string(),
            None => String::new(),
        }
    }
}

/// The job's own fraction, from its byte counters (`0.0` when the total is not known yet — the
/// bar animates, which is the honest answer for an unmeasured container).
#[must_use]
pub fn fraction(job: &ExtractJob) -> f32 {
    let state = job.state();
    if state.bytes_total == 0 {
        return if state.done { 1.0 } else { 0.0 };
    }
    let frac = state.bytes_done as f64 / state.bytes_total as f64;
    frac.clamp(0.0, 1.0) as f32
}

/// Did the job end cancelled rather than done? (The report is the same shape either way.)
#[must_use]
pub fn was_cancelled(job: &ExtractJob) -> bool {
    job.state().error.as_deref() == Some("cancelled")
}

/// The terminal line: what landed, what was skipped, what was renamed.
#[must_use]
pub fn summary_of(
    report: &zenman_core::archive::extract::ExtractReport,
    cancelled: bool,
) -> String {
    let head = if cancelled {
        "Extraction cancelled"
    } else {
        "Extraction finished"
    };
    let mut parts = vec![format!(
        "{} files, {}",
        report.written.len(),
        format_size(report.bytes)
    )];
    if !report.skipped.is_empty() {
        parts.push(format!("{} skipped", report.skipped.len()));
    }
    if !report.renamed.is_empty() {
        parts.push(format!("{} renamed", report.renamed.len()));
    }
    if report.dirs > 0 {
        parts.push(format!("{} directories", report.dirs));
    }
    format!("{head}: {}", parts.join(", "))
}
