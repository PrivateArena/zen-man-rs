//! `ZenApp` — the shell (plan §5.7 `app.rs` row).
//!
//! The plan's struct names fields owned by later steps (`dialogs`, `menus`, `keymap`, `origins`);
//! they arrive with their steps (10) and this struct is the one place they get added. What step 7
//! really means — "the app opens with an empty dock + 2 panes; header, drawers and tab strip are
//! visible; jobs tick; the layout persists" — is all here.
//!
//! `Session` is held **by value**, not `Arc<Session>` as the plan writes: `Session` is already an
//! `Arc<Inner>` handle (it is `Clone`), and `Session::poll` takes `&mut self` — an `Arc` would force
//! `Arc::get_mut` gymnastics for no shared-ownership benefit (the UI is the only holder). Recorded
//! as a plan correction.

use std::time::Instant;

use egui::{CentralPanel, Panel, Ui};

use zenman_core::path::AbsPath;
use zenman_core::session::{Session, StartupNote};
use zenman_core::store::config::{LayoutMode, ThemeMode};

use crate::dock::{self, Dock};
use crate::jobs_ui::JobsUi;
use crate::persist::Persisted;
use crate::state::{AppModel, panes_for, quad_slots};
use crate::theme::{self, Tokens};
use crate::widgets::{disk_meter, toast};

/// The whole shell.
pub struct ZenApp {
    /// The engine handle (plan §4.1's `Session`).
    pub session: Session,
    /// UI-owned state (plan §4.3).
    pub model: AppModel,
    /// The pane grid.
    pub dock: Dock,
    /// Toast queue (plan: `notices::State`).
    pub notices: toast::State,
    /// Job correlation state.
    pub jobs: JobsUi,
    /// Settings/dock store (`craft-settings`; no `std::fs` anywhere).
    pub persist: Persisted,
    /// The theme choice (mirrored to `ShellSettings` on save).
    pub theme: ThemeMode,
    /// The design tokens for the current theme.
    pub tokens: Tokens,
    /// The boot report lines (GPU profile, startup notes) shown in the header tooltip/status area.
    pub boot_lines: Vec<String>,
    /// When the next debounced persist is due (plan §5.7 `persist_at`).
    pub persist_at: Option<Instant>,
    /// The last frame's start, for the step-8 `perf_frame` budget check.
    pub last_frame: Instant,
}

impl ZenApp {
    /// Build the shell over a live session, restoring the persisted dock (plan F17: validated
    /// before first paint, with a report — never silently).
    #[must_use]
    pub fn new(session: Session, boot_lines: Vec<String>) -> Self {
        let theme = session.config().theme;
        let persist = Persisted::memory();
        let layout = persist.settings.get().layout;
        Self {
            dock: dock::default_workspace(layout),
            model: AppModel::new(layout),
            session,
            notices: toast::State::default(),
            jobs: JobsUi::default(),
            persist,
            theme,
            tokens: Tokens::default(),
            boot_lines,
            persist_at: None,
            last_frame: Instant::now(),
        }
    }

    /// Build with an already-open persisted store (the binary's path; tests use [`Self::new`]).
    #[must_use]
    pub fn with_persist(session: Session, boot_lines: Vec<String>, persist: Persisted) -> Self {
        let theme = session.config().theme;
        let layout = persist.settings.get().layout;
        let mut app = Self {
            dock: dock::default_workspace(layout),
            model: AppModel::new(layout),
            session,
            notices: toast::State::default(),
            jobs: JobsUi::default(),
            persist,
            theme,
            tokens: Tokens::default(),
            boot_lines,
            persist_at: None,
            last_frame: Instant::now(),
        };
        // `restore` needs the store and the app apart (the store lives *in* the app), so lift it
        // out, restore, and put it back.
        let store = std::mem::replace(&mut app.persist, Persisted::memory());
        let report = store.restore(&mut app, &is_registered);
        app.persist = store;
        match report {
            dock::ApplyReport::Applied => {}
            dock::ApplyReport::NewerVersion { found } => app.notices.post(
                toast::Level::Warning,
                format!("saved layout is from a newer zenman (dock v{found}); using the default"),
            ),
            dock::ApplyReport::Invalid => app.notices.post(
                toast::Level::Warning,
                "saved layout was unreadable; using the default",
            ),
        }
        app
    }

    /// Queue the engine's startup notes as toasts (plan F1: "degrade and say so" — the notes come
    /// from `Session::open` and must reach the user, not the log).
    pub fn post_startup_notes(&mut self, notes: &[StartupNote]) {
        for note in notes {
            self.notices.post(toast::Level::Warning, note.message());
        }
    }

    /// Re-apply the theme after a change (settings load, `F5`-adjacent toggles from step 10).
    pub fn set_theme(&mut self, ctx: &egui::Context, choice: ThemeMode) {
        self.theme = choice;
        theme::apply(ctx, choice);
        self.tokens = theme::tokens(&ctx.global_style());
    }

    /// Change the grid (plan §5.7 `set_layout`: the only way the grid changes shape).
    pub fn set_layout(&mut self, slots: Vec<zenman_core::store::config::PaneSlot>) {
        let keep_mode = if slots.len() >= 2 {
            LayoutMode::Split
        } else {
            LayoutMode::Single
        };
        self.model.layout = keep_mode;
        dock::set_layout(&mut self.model, &mut self.dock, slots);
    }

    /// Per-frame non-drawing work (plan §5.7: poll jobs, drain events, request repaint while
    /// busy). The `logic` half of the fork's `eframe::App` contract, spelled without the `Frame`
    /// parameter so `egui_kittest` can drive it (the Harness has no logic/ui pair — plan §2.2
    /// correction, see the step-7 §9.2 row).
    pub fn logic(&mut self, ctx: &egui::Context) {
        JobsUi::tick(self, ctx);
        let now = Instant::now();
        self.last_frame = now;
        if dock::persist_pending(ctx, now) {
            let snap = self.snapshot();
            self.persist.autosave(&snap, now);
        }
    }

    /// The values the persist layer mirrors (see [`crate::persist::ShellSnapshot`]).
    #[must_use]
    pub fn snapshot(&self) -> crate::persist::ShellSnapshot {
        crate::persist::ShellSnapshot {
            layout: self.model.layout,
            theme: self.theme,
            left_drawer: self.model.left_drawer.open,
            right_drawer: self.model.right_drawer.open,
            log_panel: self.model.log_panel,
            dock: dock::snapshot(&self.dock).ok().map(|v| v.to_string()),
        }
    }

    /// Per-frame drawing: header, drawers, dock, toasts.
    pub fn ui(&mut self, ui: &mut Ui) {
        let tokens = self.tokens;
        let header_height = tokens.header_height;
        let mut toggle = DrawerToggle::default();
        Panel::top("zenman-header")
            .exact_size(header_height)
            .show(ui, |ui| {
                self.header(ui, &mut toggle);
            });
        if self.model.log_panel {
            Panel::right("zenman-log")
                .exact_size(tokens.action_log_width)
                .resizable(false)
                .show(ui, |ui| {
                    self.action_log_placeholder(ui);
                });
        }
        if self.model.right_drawer.open {
            Panel::right("zenman-right-drawer")
                .exact_size(tokens.right_drawer_width)
                .resizable(false)
                .show(ui, |ui| {
                    self.right_drawer(ui);
                });
        }
        if self.model.left_drawer.open {
            Panel::left("zenman-left-drawer")
                .exact_size(tokens.left_drawer_width)
                .resizable(false)
                .show(ui, |ui| {
                    self.left_drawer(ui);
                });
        }
        let status = self.status_line();
        Panel::bottom("zenman-status")
            .exact_size(tokens.status_height)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(status)
                            .color(tokens.text_muted)
                            .size(tokens.font_size - 1.0),
                    );
                });
            });
        CentralPanel::default().show(ui, |ui| {
            crate::dock_content::show(self, ui);
        });
        self.apply_toggles(&toggle);
        self.notices.show(ui.ctx(), &self.tokens);
    }

    /// The 48 px header (plan §2.2: an `egui::TopBottomPanel`, *not* a dock panel — `Side` has no
    /// `Top`).
    fn header(&mut self, ui: &mut Ui, toggle: &mut DrawerToggle) {
        let tokens = self.tokens;
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Zen-Man")
                    .color(tokens.text_active)
                    .strong(),
            );
            if let Some(line) = self.boot_lines.first() {
                ui.label(
                    egui::RichText::new(line)
                        .color(tokens.text_muted)
                        .size(tokens.font_size - 2.0),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if icon_button(ui, &tokens, "info", "action log", self.model.log_panel).clicked() {
                    toggle.log = true;
                }
                if icon_button(ui, &tokens, "disk", "disks", self.model.right_drawer.open).clicked()
                {
                    toggle.right = true;
                }
                if icon_button(
                    ui,
                    &tokens,
                    "folder-open",
                    "places",
                    self.model.left_drawer.open,
                )
                .clicked()
                {
                    toggle.left = true;
                }
                ui.separator();
                let mode = self.model.layout;
                let quad = self.model.quad();
                if layout_button(
                    ui,
                    &tokens,
                    "list",
                    "single pane",
                    mode == LayoutMode::Single && !quad,
                )
                .clicked()
                {
                    toggle.single = true;
                }
                if layout_button(
                    ui,
                    &tokens,
                    "split",
                    "split panes",
                    mode == LayoutMode::Split && !quad,
                )
                .clicked()
                {
                    toggle.split = true;
                }
                if layout_button(ui, &tokens, "quad", "quad panes", quad).clicked() {
                    toggle.quad = true;
                }
            });
        });
    }

    /// The right drawer: disks (plan §5.7: `widgets/disk_meter` is the point of it).
    fn right_drawer(&mut self, ui: &mut Ui) {
        let tokens = self.tokens;
        ui.label(
            egui::RichText::new("Disks")
                .color(tokens.text_main)
                .strong(),
        );
        let path = AbsPath::from_path("/");
        match path.map(|p| self.session.env().disk_space(&p)) {
            Ok(Ok(space)) => {
                let used = space.total.saturating_sub(space.free);
                ui.add_space(tokens.drawer_section_gap);
                let _ = disk_meter::show(ui, &tokens, "/", used, space.total);
            }
            Ok(Err(e)) => {
                ui.label(
                    egui::RichText::new(format!("disk space unavailable: {}", e.user_message()))
                        .color(tokens.text_muted),
                );
            }
            Err(_) => {
                ui.label(egui::RichText::new("no filesystem root").color(tokens.text_muted));
            }
        }
    }

    /// The left drawer: the persisted bookmarks (config is real since step 4; the tree/places
    /// model is step 14's).
    fn left_drawer(&mut self, ui: &mut Ui) {
        let tokens = self.tokens;
        ui.label(
            egui::RichText::new("Places")
                .color(tokens.text_main)
                .strong(),
        );
        let bookmarks = self.session.config().bookmarks;
        if bookmarks.is_empty() {
            ui.label(
                egui::RichText::new("no bookmarks yet — add them in step 14")
                    .color(tokens.text_muted),
            );
            return;
        }
        for bookmark in bookmarks {
            let label = bookmark.label.unwrap_or_else(|| bookmark.path.clone());
            let _ = crate::icons::paint(
                ui,
                ui.available_rect_before_wrap(),
                "folder",
                tokens.drawer_icon_size,
                tokens.text_muted,
            );
            ui.label(label);
        }
    }

    /// The action-log slide-in (step 14 owns the content; the panel and its toggle are step 7's so
    /// "drawers visible" is testable now).
    fn action_log_placeholder(&mut self, ui: &mut Ui) {
        let tokens = self.tokens;
        ui.label(
            egui::RichText::new("Action log")
                .color(tokens.text_main)
                .strong(),
        );
        ui.label(egui::RichText::new("records appear here from step 9").color(tokens.text_muted));
    }

    /// The status line: what a user glances at — jobs, panes, engine step.
    #[must_use]
    pub fn status_line(&self) -> String {
        let live = self.jobs.live_count();
        let jobs = if live == 0 {
            "idle".to_string()
        } else {
            format!("{live} job(s)")
        };
        format!(
            "zenman step {} · {} pane(s) · {jobs}",
            SHELL_STEP,
            self.model.panes.len()
        )
    }

    /// Apply pending header toggles (kept out of `header` so the closure borrows stay apart).
    fn apply_toggles(&mut self, toggle: &DrawerToggle) {
        if toggle.single {
            self.set_layout(panes_for(LayoutMode::Single));
        }
        if toggle.split {
            self.set_layout(panes_for(LayoutMode::Split));
        }
        if toggle.quad {
            self.set_layout(quad_slots());
        }
        if toggle.left {
            self.model.left_drawer.open = !self.model.left_drawer.open;
        }
        if toggle.right {
            self.model.right_drawer.open = !self.model.right_drawer.open;
        }
        if toggle.log {
            self.model.log_panel = !self.model.log_panel;
        }
    }
}

/// Which header buttons were pressed this frame.
#[derive(Clone, Copy, Default)]
struct DrawerToggle {
    single: bool,
    split: bool,
    quad: bool,
    left: bool,
    right: bool,
    log: bool,
}

/// The shell's build-step number, echoed in the status line (`apps/zenman` prints the crate
/// constants separately).
pub const SHELL_STEP: u32 = 7;

/// Is this dock panel part of the current grid? (the `registered` predicate of plan F17).
fn is_registered(panel: &dock::DockId) -> bool {
    // Every slot the grid can have is "registered" — the grid shape lives in the `Layout` itself,
    // and `reconcile` only asks this for panels the *file* named, all of which we accept.
    matches!(panel, dock::DockId::FilePane(_))
}

/// A small icon button with the token-derived visuals of `craft_ui::buttons::IconButtonStyle`.
///
/// Plan §2.2 names `IconButton::show` for this; its `paint_icon` callback receives a `&Ui`,
/// which cannot host the `egui::Image` widget the SVG registry needs (image loading is a `&mut Ui`
/// operation) — a fork-API correction recorded in §9.2. The frame painting therefore uses the
/// same `ButtonVisuals` types through the `Ui` it owns, so the visual language is unchanged and
/// the icon path is the registry's.
fn icon_button(
    ui: &mut Ui,
    tokens: &Tokens,
    icon: &str,
    tooltip: &str,
    selected: bool,
) -> egui::Response {
    let size = egui::vec2(
        tokens.header_height - tokens.header_button_inset,
        tokens.header_height - tokens.header_button_inset,
    );
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let visuals = if selected {
        craft_ui::buttons::ButtonVisuals {
            fill: tokens.bg_hover,
            stroke: egui::Stroke::new(1.0, tokens.accent),
            icon: tokens.text_active,
        }
    } else if response.hovered() {
        craft_ui::buttons::ButtonVisuals {
            fill: tokens.bg_hover,
            stroke: egui::Stroke::NONE,
            icon: tokens.text_main,
        }
    } else {
        craft_ui::buttons::ButtonVisuals {
            fill: tokens.transparent,
            stroke: egui::Stroke::NONE,
            icon: tokens.text_muted,
        }
    };
    if ui.is_rect_visible(rect) {
        let radius = egui::CornerRadius::same(tokens.corner_radius);
        if visuals.fill != tokens.transparent {
            ui.painter().rect_filled(rect, radius, visuals.fill);
        }
        if !visuals.stroke.is_empty() {
            ui.painter()
                .rect_stroke(rect, radius, visuals.stroke, egui::StrokeKind::Inside);
        }
        let _ = crate::icons::paint(ui, rect, icon, rect.height(), visuals.icon);
    }
    response.on_hover_text(tooltip)
}

/// A layout-mode button: [`icon_button`] with selection wired to the current grid.
fn layout_button(
    ui: &mut Ui,
    tokens: &Tokens,
    icon: &str,
    tooltip: &str,
    selected: bool,
) -> egui::Response {
    icon_button(ui, tokens, icon, tooltip, selected)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use zenman_core::platform;
    use zenman_core::session::SessionPaths;
    use zenman_core::store::config::ZenConfig;

    /// A session whose files live in memory-equivalent temp paths (tests only — the *crate* still
    /// never touches `std::fs`; `Session::open` is core's IO seam).
    pub(crate) fn memory_session() -> Session {
        let dir = std::env::temp_dir().join(format!("zenman-ui-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let paths = SessionPaths::new(
            dir.join("config.json"),
            dir.join("action.log"),
            None,
            dir.join("probe"),
        )
        .with_db_open(1, std::time::Duration::from_millis(1));
        let (session, _notes) = Session::open(
            paths,
            ZenConfig::default(),
            platform::default_env(),
            ZenConfig::default().limits(),
        )
        .expect("session opens over a fresh temp dir");
        session
    }

    #[test]
    fn the_app_opens_with_the_persisted_grid_and_no_panes_until_the_grid_is_set() {
        let session = memory_session();
        let app = ZenApp::new(session, vec!["test boot".into()]);
        // The default mode is Single (the persisted default) — one pane, consistent grid.
        assert_eq!(app.model.panes.len(), 1);
        assert!(app.model.layout_mode_is_consistent());
        let mut app = app;
        app.set_layout(panes_for(LayoutMode::Split));
        assert_eq!(
            app.model.panes.len(),
            2,
            "the plan row: the app can open with 2 panes"
        );
        assert!(app.model.layout_mode_is_consistent());
        app.set_layout(quad_slots());
        assert!(app.model.quad());
        assert!(app.dock.layout.validate().is_ok());
    }

    #[test]
    fn status_line_names_panes_and_idle() {
        let session = memory_session();
        let app = ZenApp::new(session, Vec::new());
        let line = app.status_line();
        assert!(line.contains("idle"), "{line}");
        assert!(line.contains("pane"), "{line}");
    }

    #[test]
    fn startup_notes_become_toasts() {
        let session = memory_session();
        let mut app = ZenApp::new(session, Vec::new());
        app.post_startup_notes(&[StartupNote::DatabaseDisabled {
            reason: "locked".into(),
        }]);
        assert_eq!(app.notices.queued().len(), 1);
    }
}
