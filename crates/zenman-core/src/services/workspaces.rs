//! Workspaces (plan §5.3's `services/workspaces.rs` row, §7 step 14a).
//!
//! A workspace is a named window state: the tab strips of every pane, which tab is active in each,
//! which pane has focus, the single/split layout, and the opaque `craft_ui` dock tree. All of that is
//! already modelled — `store::config::Workspace` is the type step 4's migration produces and the type
//! the settings file carries — so **this module adds no file format of its own**: it is the writer and
//! reader of the `workspaces` half of `config.json`, through the same atomic tmp+rename
//! [`ZenConfig::save`] every other settings change uses (plan F20's guard comes with it for free).
//!
//! # The Go contract
//!
//! Go kept workspaces in `~/.config/zen-man/config.json` under `workspaces.<name>`
//! (`api/places.go:26,197-266`) and the frontend did three things with them
//! (`frontend/js/workspace.js`):
//!
//! * the dropdown always offers `Default` first and hides any stored name that case-insensitively
//!   equals it (`:141-152`) — hence [`RESERVED`], and hence `default` is a *writable* name (it is what
//!   the autosave writes) that is never listed as a user workspace;
//! * every tab/pane/layout change called `scheduleAutoSave()` (`:126-133`), a **1.5 s debounce** that is
//!   re-armed by each change, so a session that keeps moving is saved once it settles;
//! * a successful autosave showed the "Saved" chip for 2 s (`:112-124`) — a *timer*, not a flag, which
//!   is why [`Autosave::show_saved`] takes `now`.
//!
//! # Where the debounce lives
//!
//! Not in a thread. `crates/zenman-ui/src/persist.rs` already owns the shell's debounced write
//! (`craft-settings`' `SettingsStore<ShellSettings>`), and core must not depend on that crate
//! (`xtask deps`). Duplicating a timer here would give two clocks and a race. So this module ships the
//! *rule* — [`Autosave`], a pure state machine driven by the `Instant` the shell already has each frame
//! — and the shell decides when to call it, on the same cadence it calls `Persisted::autosave`.
//! [`AUTOSAVE_DEBOUNCE`] is one constant, defined here because the state it debounces is core's.
//!
//! # `kill -9`
//!
//! The acceptance criterion is not "the app saves on exit" — it is that *whatever is on disk after an
//! autosave is loadable with no in-memory state*. [`WorkspaceService::reload`] is exactly that read:
//! no cache, no session, no shutdown path. `crates/zenman-core/tests/contract_workspaces.rs` proves the
//! property by dropping a `Session` without calling [`Session::close`](crate::session::Session::close)
//! and re-reading the file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::error::CoreError;
use crate::store::config::{PaneSlot, Workspace, ZenConfig};

/// The name the frontend always offers and never lists (`workspace.js:141-152`); Go's own
/// `RESERVED`-equivalent is implicit in that filter. Case-insensitive, Go parity.
pub const RESERVED: &str = "default";

/// The autosave debounce — Go's `setTimeout(…, 1500)` (`workspace.js:126-133`).
pub const AUTOSAVE_DEBOUNCE: Duration = Duration::from_millis(1_500);

/// How long the "Saved" chip stays up after a write (Go's `_fadeTimer`, `workspace.js:112-124`).
pub const SAVED_INDICATOR_TTL: Duration = Duration::from_millis(2_000);

/// `true` for the name the dropdown reserves (`"default"`, any case).
#[must_use]
pub fn is_reserved(name: &str) -> bool {
    name.eq_ignore_ascii_case(RESERVED)
}

/// Trim and validate a workspace name.
///
/// Go accepted any non-empty string (`places.go:235-239`) — including one made only of spaces, which
/// then produced a dropdown entry the user could not see. Control characters and `/` are refused here
/// for the same reason the path model refuses them: they cannot be rendered or re-selected reliably.
///
/// # Errors
/// [`CoreError::Empty`] for an empty or whitespace-only name, [`CoreError::Config`] for a name with a
/// control character, a `/`, or more than 128 characters.
pub fn normalise_name(raw: &str) -> Result<String, CoreError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(CoreError::Empty {
            what: "workspace name",
        });
    }
    if trimmed.chars().any(char::is_control) || trimmed.contains('/') {
        return Err(CoreError::Config {
            msg: format!("workspace name {raw:?} contains a control character or a separator"),
        });
    }
    if trimmed.chars().count() > 128 {
        return Err(CoreError::Config {
            msg: format!(
                "workspace name is {} characters; 128 is the cap",
                trimmed.len()
            ),
        });
    }
    Ok(trimmed.to_string())
}

/// One row of the workspace dropdown.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceMeta {
    /// The stored name, exactly as saved.
    pub name: String,
    /// `true` for a name that collides with [`RESERVED`] case-insensitively — the UI hides those rows
    /// from the list and still lets them be restored (Go's filter was silent about this).
    pub reserved: bool,
    /// Tabs across every pane; the dropdown's tooltip and the delete confirmation both want it.
    pub tabs: usize,
    /// The pane that had focus when the session was saved.
    pub active_pane: PaneSlot,
}

impl WorkspaceMeta {
    /// Describe one stored workspace.
    #[must_use]
    pub fn new(name: &str, workspace: &Workspace) -> Self {
        Self {
            name: name.to_string(),
            reserved: is_reserved(name),
            tabs: workspace.panes.values().map(|pane| pane.tabs.len()).sum(),
            active_pane: workspace.active,
        }
    }
}

/// The workspace half of `config.json`, read and written (plan §5.3's `WorkspaceService`).
///
/// Plan §5.3 sketches `save(&self, name, session)`; the port's methods take the **current config** and
/// return the new one, because the config is `Session`'s (`session.rs`'s `Mutex<ZenConfig>`) and a
/// service that re-read the file behind it would silently discard an unsaved settings change made in
/// the same frame. The write happens first, on the cloned value; the caller swaps its copy in only on
/// `Ok`, so a failed write leaves the running session exactly as it was.
pub struct WorkspaceService {
    path: PathBuf,
}

impl WorkspaceService {
    /// Point the service at a `config.json` (`SessionPaths::config` is the production argument).
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The file this service reads and writes.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the settings file from disk with **no in-memory state** — the `kill -9` path, and the only
    /// way to observe what an autosave actually left behind.
    ///
    /// # Errors
    /// As [`ZenConfig::load`]: [`CoreError::NotFound`] for a missing file, [`CoreError::Config`] for a
    /// malformed one.
    pub fn reload(&self) -> Result<ZenConfig, CoreError> {
        ZenConfig::load(&self.path)
    }

    /// The dropdown's rows, in stored order (`BTreeMap` ⇒ alphabetical, so the list is stable without
    /// the UI sorting it). Reserved names are included and flagged; Go filtered them out here, which is
    /// why a workspace saved as `Default` was unrestorable by name — the port flags instead.
    #[must_use]
    pub fn list(config: &ZenConfig) -> Vec<WorkspaceMeta> {
        config
            .workspaces
            .iter()
            .map(|(name, ws)| WorkspaceMeta::new(name, ws))
            .collect()
    }

    /// Look a workspace up.
    ///
    /// # Errors
    /// [`CoreError::NotFound`] with the name in the path slot — Go's `{"error":"Workspace not found"}`
    /// 404 (golden `err-workspaces-restore-missing.json`), typed.
    pub fn restore(config: &ZenConfig, name: &str) -> Result<Workspace, CoreError> {
        let key = lookup_key(config, name).ok_or_else(|| not_found(name))?;
        config
            .workspaces
            .get(&key)
            .cloned()
            .ok_or_else(|| not_found(name))
    }

    /// Save `session` under `name`, writing through. Idempotent: saving the same pair twice leaves the
    /// file byte-identical (plan §5.3's Done-when).
    ///
    /// # Errors
    /// From [`normalise_name`] and from [`ZenConfig::save`] (validation, directory creation, write,
    /// rename).
    pub fn save(
        &self,
        config: &ZenConfig,
        name: &str,
        session: &Workspace,
    ) -> Result<ZenConfig, CoreError> {
        let name = normalise_name(name)?;
        let mut next = config.clone();
        // An existing name keeps its original spelling: `save("Fixture")` then `save("fixture")`
        // updates one workspace, not two, because the lookup is case-insensitive like the restore.
        let key = lookup_key(config, &name).unwrap_or(name);
        next.workspaces.insert(key, session.clone());
        next.save(&self.path)?;
        Ok(next)
    }

    /// Delete a workspace, writing through. `false` when there was nothing to delete — Go's `delete`
    /// answered `{"status":"success"}` either way (`places.go:259-264`), so the truth goes into a
    /// boolean instead of into a lie the UI would have to distrust.
    ///
    /// # Errors
    /// From [`normalise_name`] and from [`ZenConfig::save`].
    pub fn delete(&self, config: &ZenConfig, name: &str) -> Result<(ZenConfig, bool), CoreError> {
        let name = normalise_name(name)?;
        let mut next = config.clone();
        let removed = match lookup_key(config, &name) {
            Some(key) => next.workspaces.remove(&key).is_some(),
            None => false,
        };
        next.save(&self.path)?;
        Ok((next, removed))
    }

    /// Rename a workspace, writing through. The contents move untouched; Go had no rename at all (the
    /// frontend's Save-As was a save under a new name plus a delete), which is why this is the only
    /// extra verb here.
    ///
    /// # Errors
    /// [`CoreError::NotFound`] for a missing `from`, [`CoreError::AlreadyExists`] for a taken `to`,
    /// plus [`normalise_name`] and [`ZenConfig::save`].
    pub fn rename(&self, config: &ZenConfig, from: &str, to: &str) -> Result<ZenConfig, CoreError> {
        let from = normalise_name(from)?;
        let to = normalise_name(to)?;
        let mut next = config.clone();
        let Some(key) = lookup_key(config, &from) else {
            return Err(not_found(&from));
        };
        if lookup_key(config, &to).is_some() {
            return Err(CoreError::AlreadyExists { path: to });
        }
        if let Some(ws) = next.workspaces.remove(&key) {
            next.workspaces.insert(to, ws);
        }
        next.save(&self.path)?;
        Ok(next)
    }
}

fn not_found(name: &str) -> CoreError {
    CoreError::NotFound {
        path: format!("workspace {name:?}"),
        source_path: None,
    }
}

/// Case-insensitive key lookup (the restore half of Go's name handling).
fn lookup_key(config: &ZenConfig, name: &str) -> Option<String> {
    if config.workspaces.contains_key(name) {
        return Some(name.to_string());
    }
    config
        .workspaces
        .keys()
        .find(|key| key.eq_ignore_ascii_case(name))
        .cloned()
}

/// The autosave rule, as a state machine the shell drives once per frame (see the module docs for why
/// the timer is not inside this crate).
///
/// Feed it the state you *want* persisted with [`Autosave::note`]; once the debounce has elapsed since
/// the last real change, [`Autosave::take_due`] hands the pending write out exactly once. A state that
/// equals the pending one does **not** re-arm the clock — otherwise a repaint loop would postpone the
/// save forever, which is the failure Go's `clearTimeout`-per-event had one step away from.
#[derive(Clone, Debug)]
pub struct Autosave {
    debounce: Duration,
    pending: Option<PendingState>,
    last_saved: Option<Instant>,
}

/// The write an [`Autosave`] is holding.
#[derive(Clone, Debug, PartialEq)]
pub struct Pending {
    /// Workspace to write.
    pub name: String,
    /// Session to write.
    pub session: Workspace,
}

#[derive(Clone, Debug)]
struct PendingState {
    target: Pending,
    since: Instant,
}

impl Default for Autosave {
    fn default() -> Self {
        Self::new(AUTOSAVE_DEBOUNCE)
    }
}

impl Autosave {
    /// A tracker with the production debounce.
    #[must_use]
    pub fn new(debounce: Duration) -> Self {
        Self {
            debounce,
            pending: None,
            last_saved: None,
        }
    }

    /// The debounce in force.
    #[must_use]
    pub fn debounce(&self) -> Duration {
        self.debounce
    }

    /// Record the desired state. A no-op when nothing changed since the last note.
    pub fn note(&mut self, name: &str, session: &Workspace, now: Instant) {
        let target = Pending {
            name: name.to_string(),
            session: session.clone(),
        };
        match &mut self.pending {
            Some(state) if state.target == target => {}
            Some(state) => {
                state.target = target;
                state.since = now;
            }
            None => {
                self.pending = Some(PendingState { target, since: now });
            }
        }
    }

    /// `true` when a write is outstanding and its debounce has elapsed.
    #[must_use]
    pub fn due(&self, now: Instant) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|state| now.duration_since(state.since) >= self.debounce)
    }

    /// `true` while a write is outstanding, debounced or not (the status bar's "unsaved" dot).
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.pending.is_some()
    }

    /// Take the outstanding write, clearing it. `None` when nothing is due.
    pub fn take_due(&mut self, now: Instant) -> Option<Pending> {
        if !self.due(now) {
            return None;
        }
        self.pending.take().map(|state| state.target)
    }

    /// Discard the outstanding write (the user switched workspaces; saving the old one under the new
    /// name would be data loss).
    pub fn discard(&mut self) {
        self.pending = None;
    }

    /// Record that a write landed at `now` — starts the [`SAVED_INDICATOR_TTL`] clock.
    pub fn mark_saved(&mut self, now: Instant) {
        self.last_saved = Some(now);
    }

    /// When the last successful write happened.
    #[must_use]
    pub fn last_saved(&self) -> Option<Instant> {
        self.last_saved
    }

    /// Whether the "Saved" chip is showing: Go held it for 2 s after each autosave.
    #[must_use]
    pub fn show_saved(&self, now: Instant) -> bool {
        self.last_saved
            .is_some_and(|at| now.duration_since(at) < SAVED_INDICATOR_TTL)
    }
}

/// The pane map a shell builds when it has no stored workspace to restore (Go's
/// `restoreDefaultWorkspace` fallback created one blank tab per visible pane).
#[must_use]
pub fn blank_session(layout: crate::store::config::LayoutMode, path: &str) -> Workspace {
    Workspace {
        panes: BTreeMap::from([(
            PaneSlot::Left,
            crate::store::config::PaneSession {
                tabs: vec![crate::store::config::TabSession {
                    id: "tab-1".to_string(),
                    path: path.to_string(),
                    name: String::new(),
                    group: None,
                    view: crate::store::config::ViewMode::List,
                }],
                active: 0,
            },
        )]),
        active: PaneSlot::Left,
        layout,
        dock: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(path: &str) -> Workspace {
        Workspace {
            panes: BTreeMap::from([(
                PaneSlot::Left,
                crate::store::config::PaneSession {
                    tabs: vec![crate::store::config::TabSession {
                        id: "t1".to_string(),
                        path: path.to_string(),
                        name: "Fixtures".to_string(),
                        group: None,
                        view: crate::store::config::ViewMode::List,
                    }],
                    active: 0,
                },
            )]),
            active: PaneSlot::Left,
            layout: crate::store::config::LayoutMode::Split,
            dock: None,
        }
    }

    #[test]
    fn names_are_trimmed_reserved_matching_is_case_insensitive() {
        assert_eq!(normalise_name("  Work  ").ok().as_deref(), Some("Work"));
        assert!(
            normalise_name("   ").is_err(),
            "Go accepted a blank, invisible name"
        );
        assert!(normalise_name("a/b").is_err());
        assert!(is_reserved("Default") && is_reserved("DEFAULT") && !is_reserved("work"));
    }

    #[test]
    fn autosave_debounces_rearms_only_on_a_real_change_and_shows_saved() {
        let base = Instant::now();
        let mut save = Autosave::new(AUTOSAVE_DEBOUNCE);
        save.note("default", &workspace("/a"), base);
        // A repaint at t+1 s with the same state must not postpone the write.
        save.note("default", &workspace("/a"), base + AUTOSAVE_DEBOUNCE / 2);
        assert!(
            save.take_due(base + AUTOSAVE_DEBOUNCE).is_some(),
            "the clock runs from the first note, not from the last repaint"
        );
        assert!(!save.is_dirty(), "taking the write clears the flag");
        save.mark_saved(base + AUTOSAVE_DEBOUNCE);
        assert!(save.show_saved(base + AUTOSAVE_DEBOUNCE));
        assert!(
            !save.show_saved(base + AUTOSAVE_DEBOUNCE + SAVED_INDICATOR_TTL),
            "the chip fades after Go's 2 s"
        );
        // A changed state re-arms.
        save.note("default", &workspace("/b"), base);
        assert!(!save.due(base + AUTOSAVE_DEBOUNCE - Duration::from_millis(1)));
        assert!(save.due(base + AUTOSAVE_DEBOUNCE));
    }

    #[test]
    fn blank_session_is_an_empty_split_grid() {
        let ws = blank_session(crate::store::config::LayoutMode::Single, "/");
        assert_eq!(ws.panes.values().map(|p| p.tabs.len()).sum::<usize>(), 1);
        assert_eq!(ws.active, PaneSlot::Left);
    }
}
