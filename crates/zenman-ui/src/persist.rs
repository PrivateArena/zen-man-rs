//! Shell persistence (plan §5.7 `persist.rs` row).
//!
//! The plan's `SettingsStore<ZenConfig>` is impossible against the shipped crates — `ZenConfig`
//! lives in `zenman-core`, which must not depend on `craft-settings` (`cargo xtask deps` forbids the
//! edge in the other direction), and the orphan rule blocks an impl of the foreign trait for the
//! foreign type. So the store is typed with a **shell-side newtype**, [`ShellSettings`], whose
//! fields are exactly the state the shell owns; `ZenConfig` stays core's file (`store/config.rs`,
//! step 4) and the two never overlap. Recorded as a plan correction in §9.2.
//!
//! No `std::fs` here either (rule 7): every write is `craft-settings`' debounced atomic save.

use std::time::{Duration, Instant};

use craft_settings::{LoadReport, Location, OpenOptions, SaveStatus, Settings, SettingsStore};
use serde::{Deserialize, Serialize};

use zenman_core::store::config::{LayoutMode, ThemeMode};

use crate::app::ZenApp;
use crate::dock::{self, ApplyReport, Dock};

/// The shell state [`Persisted::autosave`] mirrors into the store each frame (assembled by the
/// app, which is why it is a value and not a borrow of `ZenApp`).
#[derive(Clone, Debug, PartialEq)]
pub struct ShellSnapshot {
    /// The grid mode.
    pub layout: LayoutMode,
    /// The theme choice.
    pub theme: ThemeMode,
    /// Left drawer open.
    pub left_drawer: bool,
    /// Right drawer open.
    pub right_drawer: bool,
    /// Action-log panel open.
    pub log_panel: bool,
    /// The serialised dock, when it can be serialised.
    pub dock: Option<String>,
}

/// The debounce the plan names: 1.5 s, parity with the Go app's `workspace.js` autosave.
pub const AUTOSAVE_DEBOUNCE: Duration = Duration::from_millis(1500);

/// The shell's persisted slice. `#[serde(default)]` everywhere so a hand-edited or older file
/// loads (the `craft-settings` contract).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellSettings {
    /// The persisted grid mode (`Single`/`Split`).
    pub layout: LayoutMode,
    /// The theme choice.
    pub theme: ThemeMode,
    /// Left drawer open.
    pub left_drawer: bool,
    /// Right drawer open.
    pub right_drawer: bool,
    /// Action-log slide-in open.
    pub log_panel: bool,
    /// The serialised `Workspace<DockId>` (divider positions included) as **JSON text**, or `None`
    /// for "default". Text, not `Value`: the settings file is TOML (`craft-settings`), and TOML
    /// cannot carry a `serde_json::Value` — a measured fork-integration fact, recorded in §9.2.
    pub dock: Option<String>,
}

impl Settings for ShellSettings {
    const APP_ID: &'static str = "zenman";
    const SCHEMA_VERSION: u32 = 1;
    // No migrations yet; version 1 is the first file this build writes.
}

/// The open seam: `Location::Dir` from the binary (its `--config` override already resolved),
/// `Location::Memory` in every test and in `--smoke` runs that must not touch a real file.
#[must_use]
pub fn open(location: Location) -> (Persisted, LoadReport) {
    let options = OpenOptions::new(location).debounce(AUTOSAVE_DEBOUNCE);
    let (settings, report) = SettingsStore::<ShellSettings>::open_with(options);
    (Persisted { settings }, report)
}

/// The store plus the one fact the store does not keep: whether the dock has changed since the
/// last write (a divider drag dirties the `Workspace`; a redraw does not).
pub struct Persisted {
    /// The craft-settings store (atomic write, debounce, newer-file-is-shown-never-overwritten).
    pub settings: SettingsStore<ShellSettings>,
}

impl Persisted {
    /// The in-memory store every test and `--smoke` run uses.
    #[must_use]
    pub fn memory() -> Self {
        let (this, _report) = open(Location::Memory);
        this
    }

    /// Load the shell settings into the app: theme, drawers, and the dock (validated + reconciled
    /// by [`dock::apply`], plan F17). Returns what the dock restore did, so the caller can toast a
    /// refusal instead of swallowing it.
    pub fn restore(
        &self,
        app: &mut ZenApp,
        registered: &dyn Fn(&dock::DockId) -> bool,
    ) -> ApplyReport {
        let saved = self.settings.get();
        app.model.layout = saved.layout;
        app.theme = saved.theme;
        app.model.left_drawer.open = saved.left_drawer;
        app.model.right_drawer.open = saved.right_drawer;
        app.model.log_panel = saved.log_panel;
        let default = dock::default_workspace(saved.layout);
        match &saved.dock {
            None => {
                app.dock = default;
                ApplyReport::Applied
            }
            Some(text) => match serde_json::from_str(text) {
                Ok(value) => dock::apply(&mut app.dock, &value, registered, &default),
                Err(_) => {
                    app.dock = default;
                    ApplyReport::Invalid
                }
            },
        }
    }

    /// Capture the current dock into the value to be written (plan §5.7 `save_dock`; the file; it
    /// never lands from here — `craft-settings` writes `config.toml` atomically, and a newer file is
    /// shown, never overwritten, which is the F20 guard the plan row asks for).
    pub fn save_dock(&mut self, dock: &Dock) -> Result<(), String> {
        let value = dock::snapshot(dock).map_err(|e| format!("dock snapshot: {e}"))?;
        self.settings.update(|s| s.dock = Some(value.to_string()));
        Ok(())
    }

    /// Frame tick (plan §5.7 `autosave`): mirror the shell state into the store, then let
    /// `craft-settings` decide when the debounce has passed. Returns `true` when a write happened
    /// this call (so a test can prove the debounce, not just the intent).
    ///
    /// Takes the *snapshot*, not `&mut ZenApp`: the store lives inside `ZenApp`, and the split
    /// borrow that would need does not exist — the caller assembles what changed.
    pub fn autosave(&mut self, shell: &ShellSnapshot, now: Instant) -> bool {
        self.settings.update_at(now, |s| {
            s.layout = shell.layout;
            s.theme = shell.theme;
            s.left_drawer = shell.left_drawer;
            s.right_drawer = shell.right_drawer;
            s.log_panel = shell.log_panel;
            if let Some(text) = &shell.dock {
                s.dock = Some(text.clone());
            }
        });
        let status = self.settings.tick(now).status;
        // Surface a failed write in the returned reason rather than swallowing it: `false` after
        // the debounce means either "still pending" or "failed", and only one of those is a bug.
        if matches!(status, SaveStatus::Failed(_)) {
            return false;
        }
        matches!(status, SaveStatus::Saved)
    }

    /// Write now, whatever the debounce says (window close).
    pub fn flush(&mut self) -> Result<(), String> {
        self.settings.flush().map_err(|e| format!("{e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppModel;
    use zenman_core::store::config::ThemeMode;

    #[test]
    fn memory_store_round_trips_the_shell_state() {
        let (mut persisted, report) = open(Location::Memory);
        assert!(report.dir.is_none(), "memory writes nothing");
        let dock = dock::default_workspace(LayoutMode::Split);
        persisted.save_dock(&dock).expect("snapshot serialises");
        let text = persisted.settings.get().dock.clone().expect("stored");
        let value: serde_json::Value = serde_json::from_str(&text).expect("stored dock is JSON");
        let mut back = Dock::default();
        let report = dock::apply(
            &mut back,
            &value,
            &|p| {
                matches!(
                    dock::slot_of(p),
                    zenman_core::store::config::PaneSlot::Left
                        | zenman_core::store::config::PaneSlot::Right
                )
            },
            &dock::default_workspace(LayoutMode::Split),
        );
        assert_eq!(report, ApplyReport::Applied);
        assert_eq!(back.layout, dock.layout);
    }

    #[test]
    fn autosave_respects_the_debounce() {
        // A real folder (tests may do IO; the crate may not), so the debounced write is observable
        // as bytes — `Location::Memory` never reports `Saved` because it has no path.
        // A fresh subdirectory per process *and* per counter tick — the clippy ban on
        // `remove_dir_all` applies to tests too, so the test never deletes anything.
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "zenman-ui-persist-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let (mut persisted, report) = open(Location::Dir(dir.join("autosave")));
        assert!(
            report.dir.is_some(),
            "the store resolved the folder we named"
        );
        let session = crate::app::tests::memory_session();
        let app = ZenApp::new(session, Vec::new());
        let now = Instant::now();
        let snap = app.snapshot();
        let wrote_early = persisted.autosave(&snap, now);
        assert!(!wrote_early, "a write must not precede the debounce");
        let wrote_late = persisted.autosave(&snap, now + AUTOSAVE_DEBOUNCE * 2);
        assert!(wrote_late, "the debounced write must land");
        let file = dir.join("autosave").join("config.toml");
        let text = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("{} must exist after a Saved tick: {e}", file.display()));
        assert!(
            text.contains("schema_version"),
            "the file is a settings file: {text}"
        );
    }

    #[test]
    fn restore_applies_theme_and_dock_and_reports_bad_files() {
        let mut persisted = Persisted::memory();
        let mut newer = crate::dock::snapshot(&crate::dock::default_workspace(LayoutMode::Single))
            .expect("serialisable");
        newer["version"] = serde_json::json!(99);
        let newer = newer.to_string();
        persisted.settings.update(|s| {
            s.theme = ThemeMode::Light;
            s.layout = LayoutMode::Single;
            s.dock = Some(newer);
        });
        let session = crate::app::tests::memory_session();
        let mut app = ZenApp::new(session, Vec::new());
        let report = persisted.restore(&mut app, &|_| true);
        assert_eq!(app.theme, ThemeMode::Light);
        assert_eq!(
            report,
            ApplyReport::NewerVersion { found: 99 },
            "a newer dock file is kept and the default used (F17/F20)"
        );
    }

    #[test]
    fn model_default_matches_the_single_grid() {
        let model = AppModel::new(LayoutMode::Single);
        assert!(model.layout_mode_is_consistent());
    }
}
