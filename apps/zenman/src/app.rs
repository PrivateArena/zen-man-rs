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

use zenman_core::path::{AbsPath, FsPath};

use crate::app_dirs::AppPaths;

/// How often an idle shell wakes itself to read the instance socket (see
/// [`ShellApp::poll_instance`]). Slow enough that the wake-up is not a load, quick enough that a
/// desktop "Open Folder" on an already-running manager is not experienced as lost.
const INSTANCE_POLL: Duration = Duration::from_millis(250);

/// The window: the real shell plus the binary's own smoke timer.
pub struct ShellApp {
    /// The shell (`zenman-ui`'s `ZenApp`): header, dock, drawers, jobs, persistence.
    shell: zenman_ui::app::ZenApp,
    smoke_after: Option<Duration>,
    started: Instant,
    close_sent: bool,
    /// Set once the frame guard has turned a panic into a notice, so a panic that repeats every
    /// frame posts one toast, not one per frame (step 16b, plan §5.9 `crash_guard` row).
    frame_panicked: bool,
    /// The single-instance claim, held for the whole run (its drop releases the socket) and polled
    /// every frame for what a second invocation asked for — plan §5.9's per-frame drain, which is where
    /// a desktop "open this folder" on a running file manager has to land.
    instance: crate::single_instance::SingleInstanceGuard,
}

impl ShellApp {
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "the boot report the shell is built from is a list"
    )]
    pub fn new(
        paths: &AppPaths,
        boot: String,
        smoke_after: Option<Duration>,
        session: zenman_core::session::Session,
        notes: &[zenman_core::session::StartupNote],
        targets: Vec<FsPath>,
        instance: crate::single_instance::SingleInstanceGuard,
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
        // Plan §7 step 8: the binary opens a **real tab**. With a target the desktop launched it at
        // (`zenman file:///tmp`, `zenman ~`) that target **is** the start tab — a folder manager opened
        // at a folder shows that folder, not the home directory beside it — and each further target
        // opens as another tab, in order. With no target the start is the user's home: the Go app opened
        // on the server's cwd, which is the kind of accident a desktop port does not repeat. When the
        // host has no home at all (a container with an unset `HOME`, the same case `AppPaths::resolve`
        // refuses for the config directory) the store root is the honest fallback, and it is listed
        // through the engine like any other path.
        let slot = shell.model.active_pane;
        let start = targets.first().cloned().unwrap_or_else(|| {
            paths
                .home()
                .and_then(|home| AbsPath::from_path(home).ok())
                .map(FsPath::from_abs)
                .unwrap_or(FsPath::ROOT)
        });
        shell.open_tab(slot, start);
        for folder in targets.into_iter().skip(1) {
            shell.open_tab(slot, folder);
        }
        Self {
            shell,
            smoke_after,
            started: Instant::now(),
            close_sent: false,
            frame_panicked: false,
            instance,
        }
    }

    /// Poll the instance socket: what a second `zenman` asked this one to do.
    ///
    /// A folder request is the desktop's `Open Folder`, honoured by listing the folder in the pane that
    /// has the focus — the same navigate command a click on a breadcrumb chip runs, so the two paths
    /// through the shell cannot drift. A bare request is the old `focus` lane, and egui 0.37 has no
    /// `ViewportCommand::Focus`: undoing a minimisation and making the window visible is what "raise it"
    /// can mean from inside the app, and is the state a user actually means.
    fn poll_instance(&mut self, ctx: &egui::Context) {
        for request in self.instance.take_requests() {
            match request {
                crate::single_instance::Request::Focus => {
                    ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                }
                crate::single_instance::Request::Open(raw) => self.open_folder(&raw),
            }
        }
    }

    /// List a folder a second invocation asked for.
    ///
    /// The wire carries a path the sender already resolved, and it is resolved **again** here rather
    /// than trusted: plan §4.2's guard is a type, and a type stays honest by being built at every
    /// boundary instead of by assuming who built it. The socket is a file in a user-writable directory;
    /// whatever turns up in it is input.
    fn open_folder(&mut self, raw: &str) {
        match FsPath::resolve(raw, std::path::Path::new("/"), None) {
            Ok(folder) => {
                let slot = self.shell.model.active_pane;
                self.shell.run_command(
                    slot,
                    zenman_ui::panels::file_pane::PaneCommand::Navigate(folder),
                );
            }
            Err(e) => self.shell.notices.post(
                zenman_ui::widgets::toast::Level::Warning,
                format!("open refused: {}", e.user_message()),
            ),
        }
    }

    /// The frame boundary (plan §5.9 `crash_guard`: "a deliberately panicking widget produces a
    /// report and the next start recovers"). A panic escaping the shell's per-frame code becomes a
    /// typed notice through `crash_guard::guard` — the report file is written by the hook, the
    /// process keeps running. `logic` and `ui` are guarded separately so a paint panic still gets
    /// its notice posted by the next `logic` pass.
    fn guarded_logic(&mut self, ctx: &egui::Context) {
        let outcome = crate::crash_guard::guard("UI logic", || {
            self.shell.logic(ctx);
            Ok::<(), std::convert::Infallible>(())
        });
        if let Err(report) = outcome {
            self.post_frame_panic_notice(report);
        }
    }

    /// One toast per distinct panic, not one per frame (a panicking logic path runs every frame).
    fn post_frame_panic_notice(&mut self, report: String) {
        if self.frame_panicked {
            return;
        }
        self.frame_panicked = true;
        self.shell.notices.post(
            zenman_ui::widgets::toast::Level::Error,
            format!("internal error in the UI logic — {report}"),
        );
    }
}

impl eframe::App for ShellApp {
    /// `logic` runs every frame and may not draw: the shell polls its jobs there, and this is
    /// where the smoke timer is checked (`egui/crates/eframe/src/epi.rs:152-172`).
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.guarded_logic(ctx);
        self.poll_instance(ctx);
        // The instance socket is not an input device: nothing written to it wakes this loop, so a second
        // `zenman <folder>` — what `xdg-open <dir>` becomes once this is the default file manager — would
        // otherwise be read whenever the user next touched the window. A slow timer is the honest way to
        // close that gap: the drain itself is one non-blocking `accept()`, and a shell with no jobs, no
        // animation and no hover does stop drawing entirely (this is the loop that would otherwise be
        // asleep), so the request needs a reason to be looked at.
        ctx.request_repaint_after(INSTANCE_POLL);
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
        if let Err(report) = crate::crash_guard::guard("UI paint", || {
            self.shell.ui(ui);
            Ok::<(), std::convert::Infallible>(())
        }) {
            // The notice lands on the next frame's logic pass; the paint panic itself is already
            // reported by the hook (`data_dir/crashes/`).
            log::error!("frame paint recovered: {report}");
        }
    }
}
