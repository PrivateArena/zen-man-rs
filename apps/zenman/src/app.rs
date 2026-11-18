//! The step-1 stub window: enough of a real app to prove that a `wgpu` device was created, a frame
//! was drawn, and the process exited on its own (plan §5.1 "the first commit is bootable on a VM").
//!
//! The real shell — `craft_ui::docking::DockArea`, panes, tabs, the header — is plan §5.7/§5.8 and
//! lands in step 7. What is deliberately *not* stubbed here: the GPU ladder (step 1 owns it) and the
//! smoke self-close, which is what `tests/gpu_fallback.rs` and the step-1 acceptance criteria drive.
//!
//! The framework's `App` trait is the fork's `logic` + `ui` pair, not `update` (plan §2.1:
//! `crates/ui-egui/src/app.rs:248,305`; the pinned fork declares `fn logic` at
//! `egui/crates/eframe/src/epi.rs:170` and the mandatory `fn ui` at `:185`).

use std::time::{Duration, Instant};

use egui::ViewportCommand;

use crate::app_dirs::AppPaths;

/// The whole app, for now: the resolved paths, the boot-time GPU choice, and the smoke timer.
pub struct ZenApp {
    paths: AppPaths,
    /// One line describing the profile/adapter the boot chose, echoed into the window so a screenshot
    /// or a human can confirm which rung of the ladder rendered.
    boot: String,
    smoke_after: Option<Duration>,
    started: Instant,
    close_sent: bool,
}

impl ZenApp {
    #[must_use]
    pub fn new(paths: AppPaths, boot: String, smoke_after: Option<Duration>) -> Self {
        Self {
            paths,
            boot,
            smoke_after,
            started: Instant::now(),
            close_sent: false,
        }
    }

    /// The window contents: one label, then the resolved [`AppPaths`] — the smoke test proves that
    /// `AppPaths::resolve` ran with the CLI overrides applied, not merely that a window appeared.
    fn show(&mut self, ui: &mut egui::Ui) {
        let mut heading = egui::Label::new(format!(
            "zenman — step {} scaffold (core {}, ui {})",
            crate::PLAN_STEP,
            zenman_core::PLAN_STEP,
            zenman_ui::PLAN_STEP
        ));
        heading = heading.selectable(false);
        ui.add(heading);
        ui.label(&self.boot);
        ui.separator();
        ui.label("resolved paths:");
        ui.monospace(format!("config: {}", self.paths.config.display()));
        ui.monospace(format!("data:   {}", self.paths.data.display()));
        ui.monospace(format!("cache:  {}", self.paths.cache.display()));
        if let Some(session) = &self.paths.session {
            ui.monospace(format!("session: {}", session.display()));
        }
        if let Some(after) = self.smoke_after {
            ui.monospace(format!(
                "smoke: {:.1}s of {:.1}s",
                self.started.elapsed().as_secs_f64(),
                after.as_secs_f64()
            ));
        }
    }
}

impl eframe::App for ZenApp {
    /// `logic` runs every frame and may not draw, so this is where the smoke timer is checked
    /// (`egui/crates/eframe/src/epi.rs:152-172`).
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let Some(after) = self.smoke_after else {
            return;
        };
        if self.started.elapsed() >= after {
            if !self.close_sent {
                self.close_sent = true;
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            return;
        }
        // Keep the loop alive until the deadline, without spinning the CPU at full rate.
        ctx.request_repaint_after(Duration::from_millis(50));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}
