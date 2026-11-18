//! The binary's window: `eframe::App` glue around `zenman_ui::app::ZenApp` (plan §5.9 `app.rs`
//! row — the shell itself is §5.7 and lives in the UI crate; the binary owns boot, the GPU ladder
//! and this adapter).
//!
//! Step 1's stub window is gone; what is deliberately *not* changed: the GPU ladder (step 1 owns
//! it) and the smoke self-close, which `tests/gpu_fallback.rs` and the step-1 acceptance criteria
//! drive.
//!
//! The framework's `App` trait is the fork's `logic` + `ui` pair, not `update` (plan §2.1; the
//! pinned fork declares `fn logic` at `egui/crates/eframe/src/epi.rs:170` and the mandatory
//! `fn ui` at `:185`).

use std::time::{Duration, Instant};

use egui::ViewportCommand;

use crate::app_dirs::AppPaths;

/// The window: the real shell plus the binary's own smoke timer.
pub struct ShellApp {
    /// The shell (`zenman-ui`'s `ZenApp`): header, dock, drawers, jobs, persistence.
    shell: zenman_ui::app::ZenApp,
    smoke_after: Option<Duration>,
    started: Instant,
    close_sent: bool,
}

impl ShellApp {
    #[must_use]
    pub fn new(
        paths: &AppPaths,
        boot: String,
        smoke_after: Option<Duration>,
        session: zenman_core::session::Session,
        notes: &[zenman_core::session::StartupNote],
    ) -> Self {
        // The window echo of the resolved paths: the smoke test proves `AppPaths::resolve` ran with
        // the CLI overrides applied, and a human sees the same lines the boot log printed.
        let mut lines = vec![boot];
        lines.push(format!("config: {}", paths.config.display()));
        lines.push(format!("data:   {}", paths.data.display()));
        lines.push(format!("cache:  {}", paths.cache.display()));
        if let Some(session_path) = &paths.session {
            lines.push(format!("session: {}", session_path.display()));
        }
        // The settings folder the boot resolved (`--config` included); `craft-settings` owns the
        // atomic debounced write, so the UI crate still never names a file (rule 7).
        let (persist, report) =
            zenman_ui::persist::open(craft_settings::Location::Dir(paths.config.clone()));
        let mut report_lines = vec![format!(
            "settings: {} ({} rule)",
            report
                .dir
                .as_ref()
                .map_or("<memory>".to_string(), |d| d.display().to_string()),
            report.rule
        )];
        report_lines.extend(
            report
                .section_errors
                .iter()
                .map(|e| format!("settings section error: {e:?}")),
        );
        for line in &report_lines {
            println!("boot note: {line}");
        }
        let mut shell = zenman_ui::app::ZenApp::with_persist(session, lines, persist);
        shell.post_startup_notes(notes);
        Self {
            shell,
            smoke_after,
            started: Instant::now(),
            close_sent: false,
        }
    }
}

impl eframe::App for ShellApp {
    /// `logic` runs every frame and may not draw: the shell polls its jobs there, and this is
    /// where the smoke timer is checked (`egui/crates/eframe/src/epi.rs:152-172`).
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.shell.logic(ctx);
        let Some(after) = self.smoke_after else {
            return;
        };
        if self.started.elapsed() >= after {
            if !self.close_sent {
                self.close_sent = true;
                // Flush the settings before the window goes away (plan §5.7 `persist`: a kill
                // during a write leaves the previous file valid; a clean exit must not lose the
                // last second of edits either).
                if let Err(e) = self.shell.persist.flush() {
                    eprintln!("zenman: could not write the settings: {e}");
                }
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            return;
        }
        // Keep the loop alive until the deadline, without spinning the CPU at full rate.
        ctx.request_repaint_after(Duration::from_millis(50));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.shell.ui(ui);
    }
}
