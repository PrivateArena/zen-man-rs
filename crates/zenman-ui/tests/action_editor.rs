//! Step 14b: the custom-action editor (plan §5.8 `dialogs/action_editor.rs`, §7 row 14).
//!
//! Three things are proven here and nowhere else in the UI suite:
//!
//! * the editor's Save really persists through `Session::{action_create, action_update}` — i.e. into
//!   the settings *file*, read back by a second reader, not just into the in-memory config;
//! * the applicability preview is `services::actions`' own predicate, so the dialog cannot claim a
//!   row the context menu will not show;
//! * **a test run spawns nothing**: the session's `Env` is 14a's recorder (every `ExecRequest` is
//!   captured, `platform::stub` refuses the rest), so the assertion is on the argv and the proof that
//!   no process started is that the recorder is the only thing that ever saw the request.
//!
//! The gates on a test run (see `dialogs/action_editor.rs`'s module doc) are: the action must be
//! *saved* (an id that exists in the config), and the target must be inside the pane's directory.
//! The first is asserted both ways below; the second is a code-level precondition of `run_action`
//! and is exercised only in its affirmative form here — its refusal branch is reachable only from a
//! context the pane itself would not build, which is the point of computing it in one place.

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zenman_core::error::CoreError;

use zenman_core::path::{AbsPath, FsPath};
use zenman_core::platform::{
    CopyItem, CopyReport, DiskSpace, Env, ExecOutput, ExecRequest, LinkItem, MountTable, Os,
    PlatformCaps, PlatformFuture, Shortcut, ready,
};
use zenman_core::services::actions::ActionContext;
use zenman_core::session::{Session, SessionPaths};
use zenman_core::store::config::{CustomAction, LayoutMode, PaneSlot, ZenConfig};
use zenman_ui::app::ZenApp;
use zenman_ui::dialogs::action_editor::ActionEditor;
use zenman_ui::state::panes_for;

fn scratch(tag: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("zenman-ui-fixtures")
        .join(format!("zenman-ui-fixture-{tag}"))
}

fn clean(tag: &str) -> PathBuf {
    let root = scratch(tag);
    common::remove_tree(&root);
    std::fs::create_dir_all(&root).expect("fixture directory");
    root
}

/// A session whose platform records instead of spawning (14a's harness, same shape).
fn recorder_session(tag: &str) -> (Session, Arc<Recorder>) {
    let dir = scratch(tag);
    std::fs::create_dir_all(dir.join("data")).expect("fixture directory");
    let env = Arc::new(Recorder::default());
    let paths = SessionPaths::new(
        dir.join("data/config.json"),
        dir.join("data/action.log"),
        None,
        dir.join("data/probe"),
    )
    .with_db_open(1, Duration::from_millis(1));
    let (session, _notes) = Session::open(
        paths,
        ZenConfig::default(),
        env.clone(),
        ZenConfig::default().limits(),
    )
    .expect("a session opens over a fresh directory");
    (session, env)
}

fn app_over(dir: &Path, session: Session) -> ZenApp {
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Single));
    app.open_tab(
        PaneSlot::Left,
        FsPath::from_abs(AbsPath::from_path(dir).expect("an absolute fixture path")),
    );
    app
}

fn draft(id: &str, name: &str, role: &str, patterns: &str) -> CustomAction {
    CustomAction {
        id: id.to_string(),
        name: name.to_string(),
        icon: String::new(),
        command: "echo {name}".to_string(),
        role: role.to_string(),
        patterns: patterns.to_string(),
        show_output: false,
    }
}

#[test]
fn the_editor_saves_and_updates_through_the_settings_file() {
    let dir = clean("ae-save");
    let (session, _env) = recorder_session("ae-save");
    let mut app = app_over(&dir, session);
    // A brand-new draft has no id, and `save_action` gives it one (Go's `generate_id`).
    app.save_action(draft("", "Word it", "files", "*.txt"));
    let saved = app.session.custom_actions();
    assert_eq!(saved.len(), 1, "{saved:?}");
    assert_eq!(saved[0].name, "Word it");
    assert!(!saved[0].id.is_empty(), "the new action kept the empty id");
    // It reached the *file*: `WorkspaceService::reload` is a second, independent reader.
    let reloaded = app.session.workspace_service().reload().expect("re-read");
    assert_eq!(
        reloaded.custom_actions, saved,
        "the editor's save was in memory only"
    );

    // Same id, new name: the update arm, not a second row.
    let mut edited = saved[0].clone();
    edited.name = "Rename it".to_string();
    app.save_action(edited);
    let after = app.session.custom_actions();
    assert_eq!(after.len(), 1, "the update created a duplicate: {after:?}");
    assert_eq!(after[0].name, "Rename it");
    assert!(
        app.notices
            .queued()
            .iter()
            .any(|n| n.text.contains("action saved")),
        "the save was silent"
    );
}

#[test]
fn the_applicability_preview_is_the_menus_own_predicate() {
    let file = AbsPath::from_path("/tree/a.txt").expect("a constant path");
    let ctx = ActionContext::rows(None, vec![file.clone()], false);
    let mut editor = ActionEditor::new(
        PaneSlot::Left,
        draft("x", "Txt", "files", "*.txt"),
        ctx.clone(),
    );
    assert_eq!(
        editor.matched(),
        1,
        "the menu would show it and the editor says no"
    );
    assert_eq!(editor.matching_rows(), 1);
    editor.draft = draft("x", "Dirs", "dirs", "");
    assert_eq!(editor.matched(), 0, "a dirs action matched a file row");
    editor.draft = draft("x", "Png", "files", "*.png");
    assert_eq!(
        editor.matched(),
        0,
        "the glob half is not being applied to the preview"
    );
    // Validation: Save is blocked by the words Go would have sent to the server anyway.
    editor.draft = draft("x", "  ", "files", "");
    editor.validate();
    assert!(!editor.errors.is_empty(), "an empty name passed validation");
    editor.draft = draft("x", "Ok", "nonsense", "a,,b");
    editor.validate();
    assert!(
        editor.errors.iter().any(|e| e.contains("pattern")),
        "{:?}",
        editor.errors
    );
    editor.draft = draft("x", "Ok", "files", "*.txt");
    editor.validate();
    assert!(editor.errors.is_empty(), "{:?}", editor.errors);
}

#[test]
fn a_saved_action_test_runs_through_the_recorder_and_spawns_nothing() {
    let dir = clean("ae-run");
    let file = dir.join("normal.txt");
    std::fs::write(&file, "x\n").expect("fixture file");
    let (session, env) = recorder_session("ae-run");
    let mut app = app_over(&dir, session);
    app.save_action(draft("", "Echo", "files", ""));
    let saved = app.session.custom_actions()[0].clone();
    // The same engine seam `run_action` uses, with the context the pane would have built (the
    // listing is not loaded in a harness-free test, so the selection is spelled here).
    let abs = AbsPath::from_path(&file).expect("an absolute fixture path");
    let ctx = ActionContext::rows(
        Some(AbsPath::from_path(&dir).expect("an absolute fixture path")),
        vec![abs.clone()],
        false,
    );
    app.exec_action(PaneSlot::Left, &saved.id, ctx);
    let seen = env.requests();
    assert_eq!(
        seen.len(),
        1,
        "the test run reached something else: {seen:?}"
    );
    let _ = &file;
    let req = &seen[0];
    assert!(
        req.shell,
        "the action went out as an argv vector, not `sh -c`"
    );
    assert_eq!(
        req.program,
        format!(
            "echo '{}'",
            file.file_name().unwrap_or_default().to_string_lossy()
        ),
        "the substituted line is not the quoted one"
    );
    assert!(
        req.args.is_empty() && req.detached,
        "Go's `show_output: false` is `cmd.Start()`"
    );
    // The name is there exactly once, inside single quotes — the shape 14a's injection tests pin.
    assert_eq!(
        req.program.matches("normal.txt").count(),
        1,
        "the name is not the single-quoted substitution: {:?}",
        req.program
    );
    assert_eq!(
        req.cwd.as_ref().map(|p| p.to_string_lossy()),
        Some(dir.to_string_lossy().into_owned()),
        "the action ran somewhere other than the pane's directory"
    );
    assert!(
        app.notices
            .queued()
            .iter()
            .any(|n| n.text.contains("normal.txt")),
        "the recorded output never reached the user: {:?}",
        app.notices
            .queued()
            .iter()
            .map(|n| n.text.clone())
            .collect::<Vec<_>>()
    );
}

#[test]
fn an_unsaved_draft_never_reaches_the_executor() {
    let dir = clean("ae-unsaved");
    let (session, env) = recorder_session("ae-unsaved");
    let mut app = app_over(&dir, session);
    // The dialog gates this (`Test run` is disabled without a stored id), and the engine gates it
    // again: an id that is not in the config is a typed `NotFound` before anything spawns.
    app.run_action(PaneSlot::Left, &draft("no-such-id", "Ghost", "files", ""));
    assert!(
        env.requests().is_empty(),
        "an unknown action spawned: {:?}",
        env.requests()
    );
    assert!(
        app.notices
            .queued()
            .iter()
            .any(|n| n.text.contains("no-such-id") || n.text.contains("ot found")),
        "the refusal was silent"
    );
}

// --------------------------------------------------------------------- the recorder

/// An [`Env`] that records every [`ExecRequest`] and never starts a process (14a's harness: the
/// executor's single spawn site is the thing under test, so the recorder is the only honest one).
#[derive(Default)]
struct Recorder {
    seen: Mutex<Vec<ExecRequest>>,
}

impl Recorder {
    fn requests(&self) -> Vec<ExecRequest> {
        self.seen.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

impl Env for Recorder {
    fn os(&self) -> Os {
        Os::current()
    }

    fn caps(&self) -> PlatformCaps {
        PlatformCaps::empty()
    }

    fn open_with(&self, _path: &AbsPath) -> PlatformFuture<'_, Result<(), CoreError>> {
        ready(Ok(()))
    }

    fn copy_paths(&self, _items: &[CopyItem]) -> PlatformFuture<'_, Result<CopyReport, CoreError>> {
        ready(Err(CoreError::Unsupported {
            what: "the recorder does not copy",
        }))
    }

    fn paste_link(&self, _items: &[LinkItem]) -> PlatformFuture<'_, Result<(), CoreError>> {
        ready(Err(CoreError::Unsupported {
            what: "the recorder does not link",
        }))
    }

    fn supports_symlink(&self) -> Result<bool, CoreError> {
        Ok(true)
    }

    fn chmod(
        &self,
        _t: &[AbsPath],
        _m: zenman_core::mode::ModeBits,
    ) -> PlatformFuture<'_, Result<(), CoreError>> {
        ready(Err(CoreError::Unsupported {
            what: "the recorder does not chmod",
        }))
    }

    fn can_trash(&self, _path: &AbsPath) -> bool {
        false
    }

    fn trash(&self, _path: &AbsPath) -> Result<bool, CoreError> {
        Ok(false)
    }

    fn create_shortcut(&self, _shortcut: &Shortcut) -> Result<(), CoreError> {
        Err(CoreError::Unsupported {
            what: "the recorder makes no shortcuts",
        })
    }

    fn disk_space(&self, _path: &AbsPath) -> Result<DiskSpace, CoreError> {
        Ok(DiskSpace::default())
    }

    fn mount_entries(&self) -> Result<MountTable, CoreError> {
        Err(CoreError::Unsupported {
            what: "the recorder has no mounts",
        })
    }

    fn exec_program(&self, req: ExecRequest) -> PlatformFuture<'_, Result<ExecOutput, CoreError>> {
        if let Ok(mut seen) = self.seen.lock() {
            seen.push(req.clone());
        }
        ready(Ok(ExecOutput {
            code: Some(0),
            stdout: "normal.txt".to_string(),
            stderr: String::new(),
            truncated: false,
        }))
    }
}
