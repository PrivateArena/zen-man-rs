//! Step 14a acceptance gate (plan §7 row 14, the core half; §5.3's `services/{workspaces,actions}.rs`
//! rows and `ops/rename.rs`'s batch half).
//!
//! Six things are proven here and nowhere else:
//!
//! * the workspace model against the frozen Go goldens (`workspaces*.json`) and a real
//!   settings-file round-trip;
//! * **`kill -9`**: a session is dropped *without* any clean-shutdown call and the workspace still
//!   comes back byte-for-byte from the file;
//! * the action applicability table (role × pattern × context), Go's `custom-actions.js` semantics;
//! * the executor against a recorder [`Env`] — **no process is spawned**, and the exact `argv` is
//!   asserted, including for hostile names (`a; rm -rf /.txt`, backticks, a name that is itself a
//!   token);
//! * the batch-rename preview against every rule `batch-rename.js` implements, and the apply through
//!   the real [`Session`] on a real tree;
//! * the collision rules — two rows, one name, no silent overwrite.
//!
//! The 2 ms / 10 k-row preview criterion is a separate target, `tests/perf_batch_rename.rs`, following
//! step 8's `perf_frame.rs` precedent (a perf gate is its own file, and it is *not* `#[ignore]`d).
//!
//! Unix-only for the parts that build paths with separators and check modes.

#![cfg(unix)]

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use zenman_core::error::CoreError;
use zenman_core::events::{EngineEvent, JobPayload};
use zenman_core::limits::Limits;
use zenman_core::ops::ConflictPolicy;
use zenman_core::ops::batch_rename::{self, Row, Rules};
use zenman_core::ops::rename::RenameTarget;
use zenman_core::path::AbsPath;
use zenman_core::platform::{
    CopyItem, CopyReport, DiskSpace, Env, ExecOutput, ExecRequest, LinkItem, MountTable, Os,
    PlatformCaps, PlatformFuture, Shortcut, ready,
};
use zenman_core::services::action_log::{LogPage, LogQuery};
use zenman_core::services::actions::{self, ActionContext, ExecOutcome, Role, quote};
use zenman_core::services::workspaces::{
    self, AUTOSAVE_DEBOUNCE, RESERVED, WorkspaceService, is_reserved, normalise_name,
};
use zenman_core::session::{ActionExec, Origin, Receipt, Session, SessionPaths};
use zenman_core::store::config::{
    CustomAction, LayoutMode, PaneSession, PaneSlot, TabSession, Workspace, ZenConfig,
};

const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/golden");

fn golden(name: &str) -> serde_json::Value {
    let text = fs::read_to_string(Path::new(GOLDEN).join(name))
        .unwrap_or_else(|e| panic!("{name} must be readable: {e}"));
    serde_json::from_str(&text).expect("golden is valid JSON")
}

// ------------------------------------------------------------------------------------- scratch trees

// Scratch naming, recursive delete and stale-tree reaping: the shared test-support module.
#[path = "../src/scratch.rs"]
mod scratch;

/// A fresh scratch tree (`zenman-contract-ws-<tag>-<pid>-<seq>`) owned by a guard: the tree is a
/// plain local in every caller, so `Drop` removes it even when an `assert!` unwinds.
fn scratch_dir(tag: &str) -> scratch::Scratch {
    scratch::Scratch::new(&format!("contract-ws-{tag}"))
}

// --------------------------------------------------------------------------------------- the session

fn open(dir: &Path, config: ZenConfig, env: Arc<dyn Env>) -> Session {
    let (mut session, notes) = Session::open(
        SessionPaths::new(
            dir.join("config.json"),
            dir.join("action.log"),
            None,
            dir.join("probe"),
        ),
        config,
        env,
        Limits::default(),
    )
    .expect("a session over a clean scratch directory opens");
    assert!(notes.is_empty(), "nothing damaged: {notes:?}");
    session.poll();
    session
}

fn plain(dir: &Path) -> Session {
    open(dir, ZenConfig::default(), Arc::new(NullEnv))
}

fn tab(id: &str, path: &str) -> TabSession {
    TabSession {
        id: id.to_string(),
        path: path.to_string(),
        name: "Fixtures".to_string(),
        group: None,
        group_id: None,
        view: zenman_core::store::config::ViewMode::List,
    }
}

fn workspace(path: &str) -> Workspace {
    Workspace {
        panes: std::collections::BTreeMap::from([(
            PaneSlot::Left,
            PaneSession {
                tabs: vec![tab("t1", path)],
                active: 0,
                groups: Vec::new(),
            },
        )]),
        active: PaneSlot::Left,
        layout: LayoutMode::Split,
        dock: Some(serde_json::json!({"version": 1})),
    }
}

// ------------------------------------------------------------------ 1. the workspace model vs the goldens

#[test]
fn the_workspace_model_matches_the_frozen_go_session_shape() {
    // `workspaces-restore.json` is Go's `{status, session}` answer for a restore. Its `session` is the
    // wire shape step 4's migration consumes; the port stores the same information keyed by pane slot.
    let restore = golden("workspaces-restore.json");
    let session = &restore["session"];
    assert_eq!(session["split"], serde_json::json!(true), "Go's split flag");
    assert_eq!(session["left_active"], "t1");
    assert_eq!(session["right_tabs"].as_array().map(Vec::len), Some(0));
    let left = session["left_tabs"].as_array().cloned().unwrap_or_default();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0]["path"], "${FIXTURE_ROOT}/tree");
    assert_eq!(left[0]["color"], "#ff4a4a");

    // The port's own shape: the same tab, keyed by slot, round-tripping through the settings file.
    let ws = workspace("${FIXTURE_ROOT}/tree");
    let mut config = ZenConfig::default();
    config.workspaces.insert("fixture".to_string(), ws.clone());
    let dir = scratch_dir("golden-shape");
    let file = dir.join("config.json");
    config.save(&file).expect("the config writes");
    let back = ZenConfig::load(&file).expect("and reads back");
    assert_eq!(back.workspaces.get("fixture"), Some(&ws), "byte-for-byte");
    // `workspaces.json` is the list endpoint: names, not objects.
    assert_eq!(golden("workspaces.json")["workspaces"][0], "fixture");
    assert_eq!(
        workspaces::WorkspaceService::list(&back)[0].name,
        "fixture",
        "the port's list is the same set of names"
    );
}

#[test]
fn workspace_verbs_are_idempotent_and_missing_names_are_typed() {
    let dir = scratch_dir("verbs");
    let session = plain(&dir);
    let ws = workspace("/x");
    session
        .workspace_save("Fixture", &ws)
        .expect("a save lands");
    let bytes_after_first = fs::read(dir.join("config.json")).expect("file exists");
    session
        .workspace_save("Fixture", &ws)
        .expect("saving twice is idempotent");
    let bytes_after_second = fs::read(dir.join("config.json")).expect("file exists");
    assert_eq!(
        bytes_after_first, bytes_after_second,
        "the second save writes the same bytes"
    );
    assert_eq!(
        session.workspace_restore("fixture").ok().as_ref(),
        Some(&ws),
        "the restore is case-insensitive, Go parity"
    );
    let err = session
        .workspace_restore("nope")
        .expect_err("a missing name is an error, never a panic");
    assert!(matches!(err, CoreError::NotFound { .. }), "{err:?}");
    // `default` is writable (it is the autosave target) and reserved (it is never a dropdown row).
    session
        .workspace_save(RESERVED, &ws)
        .expect("default is a writable name");
    assert!(
        session
            .workspaces()
            .iter()
            .any(|meta| meta.name == "default" && meta.reserved && is_reserved("Default"))
    );
    assert!(
        session.workspace_delete("Fixture").expect("delete reports"),
        "removed"
    );
    assert!(
        !session.workspace_delete("Fixture").expect("twice is fine"),
        "Go answered success either way; the port reports the truth"
    );
    assert!(
        normalise_name("  ").is_err(),
        "Go accepted a whitespace-only name"
    );
}

// --------------------------------------------------------------------- 2. the `kill -9` acceptance test

#[test]
fn an_autosave_survives_a_kill_minus_nine() {
    // The whole point of the criterion: nothing below calls `Session::close`, `Persisted::flush`, or any
    // other clean-exit path. The `Session` is simply dropped, and the state is read back by a *different*
    // code path (`WorkspaceService::reload`) that has no access to memory.
    let dir = scratch_dir("kill9");
    let saved_bytes;
    {
        let session = plain(&dir);
        let mut tracker = workspaces::Autosave::new(AUTOSAVE_DEBOUNCE);
        let now = std::time::Instant::now();
        let ws = workspace("${FIXTURE_ROOT}/tree");
        tracker.note(RESERVED, &ws, now);
        assert!(!tracker.due(now), "1.5 s has not passed (Go's debounce)");
        assert!(tracker.due(now + AUTOSAVE_DEBOUNCE), "and now it has");
        let pending = tracker
            .take_due(now + AUTOSAVE_DEBOUNCE)
            .expect("due ⇒ handed out");
        session
            .workspace_save(&pending.name, &pending.session)
            .expect("the autosave write");
        tracker.mark_saved(now + AUTOSAVE_DEBOUNCE);
        assert!(
            tracker.show_saved(now + AUTOSAVE_DEBOUNCE),
            "the Saved chip is up"
        );
        assert!(
            !tracker.show_saved(now + AUTOSAVE_DEBOUNCE * 3),
            "and fades after Go's 2 s"
        );
        saved_bytes = fs::read(dir.join("config.json")).expect("the write landed before the drop");
        // … and here the process dies. No `close`, no `flush`.
    }
    let reloaded = WorkspaceService::new(dir.join("config.json"))
        .reload()
        .expect("the file loads with no in-memory state");
    let ws = WorkspaceService::restore(&reloaded, RESERVED).expect("and the workspace is in it");
    assert_eq!(
        serde_json::to_vec(&ws).expect("serialisable"),
        serde_json::to_vec(&workspace("${FIXTURE_ROOT}/tree")).expect("serialisable"),
        "the state came back identical"
    );
    assert!(
        fs::read(dir.join("config.json")).expect("still there") == saved_bytes,
        "and nothing rewrote it afterwards"
    );
    // No stray temp file left behind by the atomic write.
    assert!(
        !dir.join("config.json.tmp").exists(),
        "tmp+rename leaves no `.tmp`"
    );
}

// --------------------------------------------------------------------------- 3. action applicability table

fn act(id: &str, role: &str, patterns: &str) -> CustomAction {
    CustomAction {
        id: id.to_string(),
        name: id.to_string(),
        icon: "terminal".to_string(),
        command: "echo {name}".to_string(),
        role: role.to_string(),
        patterns: patterns.to_string(),
        show_output: true,
    }
}

fn ctx(dir: &str, targets: &[&str], is_dir: bool, background: bool) -> ActionContext {
    let targets = targets
        .iter()
        .map(|t| AbsPath::from_path(t).expect("test path"))
        .collect();
    ActionContext {
        dir: Some(AbsPath::from_path(dir).expect("test dir")),
        targets,
        background,
        target_is_dir: is_dir,
    }
}

fn ids(list: &[CustomAction]) -> Vec<String> {
    list.iter().map(|a| a.name.clone()).collect()
}

#[test]
fn the_applicability_table_is_gots_role_and_glob_semantics() {
    let actions = vec![
        act("a", "files", "*.txt"),
        act("b", "dirs", ""),
        act("c", "files|dirs|background", "*.js"),
        act("d", "background", ""),
        act("e", "", ""),
        act("f", "both", "a?b"),
    ];
    let file = |name: &str| ctx("/d", &[&format!("/d/{name}")], false, false);
    let table: Vec<(ActionContext, Vec<&str>)> = vec![
        (
            file("note.txt"),
            vec!["a", "e", "f"]
                .into_iter()
                .filter(|n| *n != "f")
                .collect(),
        ),
        (file("a?b"), vec!["e", "f"]),
        (file("x.js"), vec!["c", "e"]),
        // `c` carries the pattern `*.js`, so a directory called `sub` does not take it.
        (ctx("/d", &["/d/sub"], true, false), vec!["b", "e"]),
        (
            ActionContext::background(Some(AbsPath::from_path("/d").expect("p"))),
            // `e`'s empty role normalises to `files|dirs`, so it is never a background row; and a
            // background click skips the *pattern* test entirely (Go's `isItem == false` arm).
            vec!["c", "d"],
        ),
    ];
    for (input, want) in table {
        let got = ids(&actions::for_context(&actions, &input));
        let expected: Vec<String> = want.iter().map(|s| (*s).to_string()).collect();
        assert_eq!(got, expected, "context {input:?}");
    }
    // The role bits themselves, including Go's `normalizeRole`.
    assert_eq!(
        actions::roles(&act("x", "", "")),
        vec![Role::Files, Role::Dirs]
    );
    assert!(
        actions::patterns_match("A.TXT", "*.txt"),
        "both sides lowercased"
    );
    assert!(!actions::patterns_match("a.txtx", "*.txt"));
    assert!(
        actions::glob_match("**.js", "deep.js"),
        "`**` collapses into `*` in Go's regex"
    );
    assert!(
        actions::glob_match("a?b", "a?b"),
        "`?` is escaped by Go, so it is literal"
    );
    assert!(!actions::glob_match("a?b", "axb"));
}

// --------------------------------------------------------------------- 4. the executor: argv, not shells

/// An [`Env`] that records every [`ExecRequest`] and never starts a process (plan F10's single spawn
/// site is the thing under test, so the recorder is the only honest harness).
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

/// A platform that refuses everything — for the tests that only need a session.
struct NullEnv;

impl Env for NullEnv {
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
        ready(Err(CoreError::Unsupported { what: "null" }))
    }

    fn paste_link(&self, _items: &[LinkItem]) -> PlatformFuture<'_, Result<(), CoreError>> {
        ready(Err(CoreError::Unsupported { what: "null" }))
    }

    fn supports_symlink(&self) -> Result<bool, CoreError> {
        Ok(true)
    }

    fn chmod(
        &self,
        _t: &[AbsPath],
        _m: zenman_core::mode::ModeBits,
    ) -> PlatformFuture<'_, Result<(), CoreError>> {
        ready(Err(CoreError::Unsupported { what: "null" }))
    }

    fn can_trash(&self, _path: &AbsPath) -> bool {
        false
    }

    fn trash(&self, _path: &AbsPath) -> Result<bool, CoreError> {
        Ok(false)
    }

    fn create_shortcut(&self, _shortcut: &Shortcut) -> Result<(), CoreError> {
        Err(CoreError::Unsupported { what: "null" })
    }

    fn disk_space(&self, _path: &AbsPath) -> Result<DiskSpace, CoreError> {
        Ok(DiskSpace::default())
    }

    fn mount_entries(&self) -> Result<MountTable, CoreError> {
        Err(CoreError::Unsupported { what: "null" })
    }

    fn exec_program(&self, _req: ExecRequest) -> PlatformFuture<'_, Result<ExecOutput, CoreError>> {
        ready(Err(CoreError::Unsupported { what: "null" }))
    }
}

fn exec_ctx(name: &str) -> ActionContext {
    ctx("/d", &[&format!("/d/{name}")], false, false)
}

#[test]
fn substitution_quotes_every_value_and_never_re_reads_a_substituted_one() {
    let c = exec_ctx("normal.txt");
    assert_eq!(
        actions::substitute_for(Os::Linux, "printf %s {name}", &c),
        "printf %s 'normal.txt'",
        "the golden action's template, with the value quoted"
    );
    assert_eq!(
        actions::substitute_for(Os::Linux, "echo {dir} {target_dir} {parent} {file}", &c),
        "echo '/d' '/d' '/d' '/d/normal.txt'"
    );
    // The hostile set. Each of these was a shell metacharacter sequence in Go.
    // (A `/` cannot appear in a file name, so the canonical hostile name is the same string without
    // it: the semicolon, the backticks, the `$(…)` and the pipe are the payload.)
    for hostile in [
        "a; rm -rf .txt",
        "x`id`.txt",
        "$(shutdown).txt",
        "a|b.txt",
        "a&&b.txt",
        "it's.txt",
        "-rf",
    ] {
        let line = actions::substitute_for(Os::Linux, "echo {name}", &exec_ctx(hostile));
        assert!(
            line.starts_with("echo '") && line.ends_with('\''),
            "the value must sit inside one single-quoted argument: {line}"
        );
        assert_eq!(
            line,
            format!("echo {}", quote(Os::Linux, hostile)),
            "and the quoting is the documented POSIX form"
        );
    }
    // Go expanded the seven tokens in sequence, so a *file name* containing a token was expanded
    // again. One pass here: the value is copied verbatim and quoting-neutralised.
    let sneaky = exec_ctx("{dir}.txt");
    let line = actions::substitute_for(Os::Linux, "{file} {name}", &sneaky);
    assert_eq!(
        line, r"'/d/{dir}.txt' '{dir}.txt'",
        "no second-order expansion"
    );
    // A directory target: `{target_dir}` is the folder itself, a file target's parent otherwise.
    let dir_ctx = ctx("/d", &["/d/sub"], true, false);
    assert_eq!(
        actions::substitute_for(Os::Linux, "x {target_dir} {parent}", &dir_ctx),
        "x '/d/sub' '/d'"
    );
    // `{files}`/`{names}` are one quoted argument *each*, space-joined.
    let many = ctx("/d", &["/d/a b.txt", "/d/c;d.txt"], false, false);
    assert_eq!(
        actions::substitute_for(Os::Linux, "x {files}", &many),
        "x '/d/a b.txt' '/d/c;d.txt'"
    );
    // Windows quoting: `CommandLineToArgvW` conventions, doubling the inner quote.
    assert_eq!(
        actions::substitute_for(Os::Windows, "echo {name}", &exec_ctx(r#"a"b.txt"#)),
        r#"echo "a""b.txt""#
    );
    // An absent value is the empty string, unquoted (Go parity: no phantom empty argument).
    assert_eq!(
        actions::substitute_for(Os::Linux, "x {file};y", &ActionContext::background(None)),
        "x ;y"
    );
}

#[test]
fn the_executor_builds_the_argv_the_plan_demands_and_never_concatenates() {
    let dir = scratch_dir("exec");
    let recorder = Arc::new(Recorder::default());
    let action = CustomAction {
        command: "printf %s {name}".to_string(),
        show_output: true,
        ..act("fixture-echo", "files", "*.txt")
    };
    let config = ZenConfig {
        custom_actions: vec![action.clone()],
        ..ZenConfig::default()
    };
    let mut session = open(&dir, config, Arc::clone(&recorder) as Arc<dyn Env>);
    // The captured-output arm is a job (plan F1: the UI thread must never wait on a child).
    let receipt = session
        .exec_action(
            &ActionExec {
                id: "fixture-echo".to_string(),
                ctx: exec_ctx("normal.txt"),
            },
            Origin(1),
        )
        .expect("the action exists");
    let Receipt::Running { job, .. } = receipt else {
        panic!("show_output must be a job, got {receipt:?}");
    };
    let outcome = pump_exec(&mut session, job);
    assert_eq!(
        outcome.output, "normal.txt",
        "golden `action-exec.json`'s body"
    );
    let req = recorder
        .requests()
        .first()
        .expect("one spawn was attempted")
        .clone();
    if Os::current() == Os::Windows {
        assert_eq!(req.program, "cmd");
        assert_eq!(req.args, ["/C", "printf %s 'normal.txt'"]);
    } else {
        assert!(req.shell, "`sh -c` only on Unix (plan F10)");
        assert_eq!(
            req.program, "printf %s 'normal.txt'",
            "the exact line the shell is handed (`ExecRequest::shell` carries it in `program`; \
            `Env` prepends `-c`)"
        );
        assert!(req.args.is_empty(), "a shell line is not an argv vector");
    }
    assert_eq!(
        req.cwd.map(|p| p.to_string_lossy()),
        Some("/d".to_string()),
        "`cmd.Dir` is the pane directory (api/actions.go:241)"
    );
    assert_eq!(req.output_cap, Limits::default().exec_output_cap);
    assert!(!req.detached, "show_output ⇒ the output is captured");

    // The no-output arm is Go's `cmd.Start()`: a detached spawn, answered synchronously.
    let quiet = CustomAction {
        show_output: false,
        ..action.clone()
    };
    session.action_update(&quiet).expect("the update lands");
    let receipt = session
        .exec_action(
            &ActionExec {
                id: "fixture-echo".to_string(),
                ctx: exec_ctx("a; rm -rf .txt"),
            },
            Origin(2),
        )
        .expect("updated action still exists");
    assert!(
        matches!(receipt, Receipt::Done(_)),
        "detached ⇒ Done at once"
    );
    let req = recorder
        .requests()
        .last()
        .expect("two spawns total")
        .clone();
    assert!(
        req.detached,
        "the daemon-safe form (step 9b's xclip lesson)"
    );
    assert_eq!(
        req.program.as_str(),
        "printf %s 'a; rm -rf .txt'",
        "the hostile name reaches the child as one literal argument"
    );

    // An unknown id is Go's 404, before anything spawns.
    let err = session
        .exec_action(
            &ActionExec {
                id: "gone".to_string(),
                ctx: exec_ctx("x.txt"),
            },
            Origin(3),
        )
        .expect_err("golden `err-action-exec-unknown.json`");
    assert!(matches!(err, CoreError::NotFound { .. }), "{err:?}");
    assert_eq!(
        recorder.requests().len(),
        2,
        "nothing was spawned for a bad id"
    );
}

fn pump_exec(session: &mut Session, job: zenman_core::jobs::JobId) -> ExecOutcome {
    for _ in 0..400 {
        for event in session.poll() {
            if let EngineEvent::JobFinished {
                id,
                payload: JobPayload::Exec(out),
                ..
            } = event
                && id == job
            {
                return out;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("the exec job never finished");
}

#[test]
fn action_crud_matches_gots_four_verbs() {
    let dir = scratch_dir("crud");
    let session = plain(&dir);
    let list = session
        .action_create(act("one", "files", "*.txt"))
        .expect("create");
    assert_eq!(list.len(), 1);
    assert_eq!(
        list[0].id.len(),
        16,
        "Go's id is 16 hex characters (`actions.json`)"
    );
    assert!(
        list[0]
            .id
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "hex, like `e6f7c823c3f57bb9`"
    );
    let second = session
        .action_create(act("two", "dirs", ""))
        .expect("a second action");
    assert_eq!(second.len(), 2);
    let ids: Vec<String> = second.iter().map(|a| a.id.clone()).collect();
    assert_ne!(ids[0], ids[1], "ids do not collide");
    // reorder: the named ids first, the rest appended (Go's rule, so a partial drag loses nothing).
    let reordered = session
        .action_reorder(&ids[1..])
        .expect("reorder with half the list");
    assert_eq!(reordered.len(), 2);
    assert_eq!(reordered[0].id, ids[1], "the listed one leads");
    assert_eq!(
        reordered[1].id, ids[0],
        "and the forgotten one is appended, not dropped"
    );
    let mut edited = reordered[0].clone();
    edited.command = "echo hi".to_string();
    let updated = session.action_update(&edited).expect("update by id");
    assert_eq!(updated[0].command, "echo hi");
    let err = session
        .action_update(&CustomAction {
            id: "nope".to_string(),
            ..act("x", "", "")
        })
        .expect_err("Go's 404");
    assert!(matches!(err, CoreError::NotFound { .. }));
    let after_delete = session
        .action_remove(&ids[0])
        .expect("delete by id, golden `actions-delete.json`");
    assert_eq!(after_delete.len(), 1);
    // and the list is on disk: a fresh session over the same directory sees it.
    let reopened = WorkspaceService::new(dir.join("config.json"))
        .reload()
        .expect("the settings file carries the actions too");
    assert_eq!(reopened.custom_actions, after_delete);
    // The golden wire shape: snake_case keys, exactly `actions.json`.
    let wire = serde_json::to_value(&after_delete[0]).expect("serialisable");
    let golden_action = golden("actions.json")[0].clone();
    for key in [
        "id",
        "name",
        "icon",
        "command",
        "role",
        "patterns",
        "show_output",
    ] {
        assert!(wire.get(key).is_some(), "{key} must be on the wire");
        assert!(golden_action.get(key).is_some(), "{key} is in the golden");
    }
    assert_eq!(golden_action["role"], "files");
    assert_eq!(golden_action["show_output"], serde_json::json!(true));
}

// --------------------------------------------------------------------------------- 5. batch rename

fn existing(names: &[&str]) -> HashSet<String> {
    names.iter().map(|n| (*n).to_string()).collect()
}

fn rows_in(dir: &Path, names: &[&str]) -> Vec<Row> {
    names
        .iter()
        .map(|n| {
            fs::write(dir.join(n), b"x").expect("file written");
            Row::new(AbsPath::from_path(dir.join(n)).expect("abs"), *n, false)
        })
        .collect()
}

fn names_of(preview: &batch_rename::PreviewSet) -> Vec<&str> {
    preview.rows.iter().map(|r| r.preview.as_str()).collect()
}

#[test]
fn the_preview_engine_reproduces_every_rule_batch_rename_js_has() {
    let dir = scratch_dir("preview");
    // ── literal find/replace, stem only (the default: `ext` unchecked)
    let rows = rows_in(
        &dir,
        &["photo 1.jpg", "Apple.png", ".hidden", "noext", "a.jpg"],
    );
    let names = existing(&["photo 1.jpg", "Apple.png", ".hidden", "noext", "a.jpg"]);
    let p = batch_rename::preview(
        &rows,
        &Rules {
            find: " ".to_string(),
            replace: "_".to_string(),
            ..Rules::default()
        },
        &names,
    );
    assert_eq!(
        names_of(&p),
        ["photo_1.jpg", "Apple.png", ".hidden", "noext", "a.jpg"]
    );
    assert_eq!(
        p.rows[1].status,
        batch_rename::RowStatus::Unchanged,
        "a row the rules do not touch is *unchanged*, not a conflict"
    );
    assert_eq!(p.changed(), 1);

    // ── the extension checkbox
    let rows2 = vec![Row::new(
        AbsPath::from_path(dir.join("a.jpg")).expect("abs"),
        "a.jpg",
        false,
    )];
    // Go's `renameExt` branch computes `finalExtPart` and then returns only `finalNamePart`
    // (`batch-rename.js:269-281`): ticking "include the extension" therefore *removes* it.
    let rules_ext = Rules {
        find: "g".to_string(),
        replace: "k".to_string(),
        include_extension: true,
        ..Rules::default()
    };
    let p = batch_rename::preview(&rows2, &rules_ext, &existing(&["a.jpg"]));
    assert_eq!(
        p.rows[0].preview, "a",
        "Go's `renameExt` arm returns `finalNamePart` alone (`batch-rename.js:281`), so ticking the \
        box **drops the extension** — a Go bug the port reproduces, loudly, in the preview"
    );
    let p = batch_rename::preview(
        &rows2,
        &Rules {
            include_extension: false,
            ..rules_ext.clone()
        },
        &existing(&["a.jpg"]),
    );
    assert_eq!(
        p.rows[0].preview, "a.jpg",
        "without it the extension is untouchable"
    );

    // ── the case checkbox (wired to the `i` flag: checked ⇒ *ignore* case)
    let p = batch_rename::preview(
        &rows,
        &Rules {
            find: "a".to_string(),
            replace: "b".to_string(),
            ignore_case: true,
            ..Rules::default()
        },
        &names,
    );
    assert_eq!(
        p.rows[1].preview, "bpple.png",
        "the `i` flag reaches the leading capital"
    );
    let p = batch_rename::preview(
        &rows,
        &Rules {
            find: "a".to_string(),
            replace: "A".to_string(),
            ..Rules::default()
        },
        &names,
    );
    assert_eq!(
        p.rows[1].preview, "Apple.png",
        "case-sensitive literal find does not match `A`"
    );

    // ── counters: `[n]` and `[n:W]`, `start + index × step` (the token lives in the *replacement*,
    // which is where Go's `calculatePreviews` expands it — on the already-replaced stem)
    let p = batch_rename::preview(
        &rows,
        &Rules {
            find: "photo".to_string(),
            replace: "IMG[n]".to_string(),
            counter_start: 10,
            counter_step: 5,
            ..Rules::default()
        },
        &names,
    );
    assert_eq!(
        names_of(&p),
        ["IMG10 1.jpg", "Apple.png", ".hidden", "noext", "a.jpg"],
        "only the row the rules touched changes, and it gets the row-indexed counter"
    );
    let p = batch_rename::preview(
        &rows,
        &Rules {
            find: "photo".to_string(),
            replace: "[n:3]".to_string(),
            counter_start: 7,
            ..Rules::default()
        },
        &names,
    );
    assert_eq!(
        p.rows[0].preview, "007 1.jpg",
        "Go's `padStart(width, '0')`"
    );

    // ── regex mode, with a group reference
    let p = batch_rename::preview(
        &rows,
        &Rules {
            find: r"^photo (\d)\.jpg$".to_string(),
            replace: "shot$1".to_string(),
            regex: true,
            include_extension: true,
            ..Rules::default()
        },
        &names,
    );
    assert_eq!(
        p.rows[0].preview, "shot1",
        "the whole name is replaced, and the dot is consumed"
    );
    // ── the named JS/`regex`-crate divergence: `$&` is not the whole match here
    let p = batch_rename::preview(
        &rows,
        &Rules {
            find: "hoto".to_string(),
            replace: "[$&]".to_string(),
            regex: true,
            ..Rules::default()
        },
        &names,
    );
    assert_eq!(
        p.rows[0].preview, "p[hoto] 1.jpg",
        "`$&` is a literal in the port's dialect — pinned, not silently fixed"
    );

    // ── a bad pattern: reported once, every target left alone
    let p = batch_rename::preview(
        &rows,
        &Rules {
            find: "(".to_string(),
            regex: true,
            ..Rules::default()
        },
        &names,
    );
    assert!(p.regex_error.is_some());
    assert_eq!(
        names_of(&p),
        ["photo 1.jpg", "Apple.png", ".hidden", "noext", "a.jpg"]
    );

    // ── the three collision verdicts
    let rows3 = vec![
        Row::new(
            AbsPath::from_path(dir.join("a.txt")).expect("abs"),
            "a.txt",
            false,
        ),
        Row::new(
            AbsPath::from_path(dir.join("b.txt")).expect("abs"),
            "b.txt",
            false,
        ),
        Row::new(
            AbsPath::from_path(dir.join("c.txt")).expect("abs"),
            "c.txt",
            false,
        ),
    ];
    let names3 = existing(&["a.txt", "b.txt", "c.txt", "taken.txt"]);
    let p = batch_rename::preview(
        &rows3,
        &Rules {
            find: "[abc]\\.txt".to_string(),
            replace: "X".to_string(),
            regex: true,
            include_extension: true,
            ..Rules::default()
        },
        &names3,
    );
    assert_eq!(names_of(&p), ["X", "X", "X"]);
    assert_eq!(p.rows[0].status, batch_rename::RowStatus::Success);
    assert_eq!(
        p.rows[1].reason, "Duplicate target name collision",
        "two rows, one name"
    );
    let p = batch_rename::preview(
        &rows3,
        &Rules {
            find: "a".to_string(),
            replace: "taken".to_string(),
            ..Rules::default()
        },
        &names3,
    );
    assert_eq!(
        p.rows[0].reason, "File already exists in folder",
        "a name that is on disk and not in the batch"
    );
    let p = batch_rename::preview(
        &rows3,
        &Rules {
            find: ".".to_string(),
            regex: true,
            include_extension: true,
            ..Rules::default()
        },
        &names3,
    );
    assert_eq!(p.rows[0].reason, "Filename cannot be empty");
    assert!(p.has_conflicts() && p.first_conflict().is_some());
}

#[test]
fn applying_a_batch_goes_through_the_rename_op_with_a_parent_rewrite_and_a_log_record() {
    let dir = scratch_dir("apply");
    fs::create_dir_all(dir.join("sub")).expect("dir");
    let file = |name: &str| AbsPath::from_path(dir.join(name)).expect("abs");
    fs::write(dir.join("a1.txt"), b"x").expect("written");
    fs::write(dir.join("a2.txt"), b"x").expect("written");
    fs::write(dir.join("taken.txt"), b"x").expect("written");
    let rows = vec![
        Row::new(file("a1.txt"), "a1.txt", false),
        Row::new(file("a2.txt"), "a2.txt", false),
    ];
    let names = existing(&["a1.txt", "a2.txt", "taken.txt"]);
    let mut session = plain(&dir);
    // Both rows want a name the *folder* already holds: the default policy reports and applies
    // nothing, the `Rename` policy gives each row its own free name.
    let rules = Rules {
        find: r"^a[12]".to_string(),
        replace: "taken".to_string(),
        regex: true,
        include_extension: false,
        ..Rules::default()
    };
    let refused = session
        .batch_rename(
            &rows,
            &rules,
            &names,
            ConflictPolicy::SkipExisting,
            session.next_origin(),
        )
        .err()
        .expect("both rows collide with a name on disk, so nothing is submitted");
    assert!(matches!(refused, CoreError::Empty { .. }), "{refused:?}");
    assert!(
        dir.join("a1.txt").exists(),
        "a refused apply changed nothing"
    );

    let report = session
        .batch_rename(
            &rows,
            &rules,
            &names,
            ConflictPolicy::Rename,
            session.next_origin(),
        )
        .expect("the disambiguating policy applies");
    assert!(
        report.skipped.is_empty(),
        "nothing was dropped: {:?}",
        report.skipped
    );
    assert!(
        dir.join("taken.txt").exists(),
        "the pre-existing file is untouched"
    );
    assert!(dir.join("taken (2).txt").exists(), "row 1 got a suffix");
    assert!(
        dir.join("taken (3).txt").exists(),
        "row 2 got the next one, not the same name"
    );
    assert!(
        !dir.join("a1.txt").exists() && !dir.join("a2.txt").exists(),
        "and the sources are gone"
    );
    let pairs: Vec<(String, String)> = report
        .rename
        .renamed()
        .into_iter()
        .map(|(from, to)| {
            (
                from.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                to.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        pairs,
        [
            ("a1.txt".to_string(), "taken (2).txt".to_string()),
            ("a2.txt".to_string(), "taken (3).txt".to_string()),
        ],
        "one `Session::rename` call for the whole batch"
    );

    // The parent-rewrite case step 9c proved is reachable through the batch path too: rename the
    // directory first, then a child under its *old* name.
    let parent = Row::new(file("sub"), "sub", true);
    let preview = Session::batch_rename_preview(
        std::slice::from_ref(&parent),
        &Rules {
            find: "sub".to_string(),
            replace: "SUB".to_string(),
            ..Rules::default()
        },
        &existing(&["sub"]),
    );
    assert_eq!(preview.rows[0].preview, "SUB");
    let (req, _) = batch_rename::apply_request(
        &[parent],
        &preview,
        &existing(&["sub"]),
        ConflictPolicy::SkipExisting,
    );
    assert!(
        matches!(req.target, RenameTarget::Batch(_)),
        "a batch, one call (plan §6.2)"
    );
    let report = session
        .rename(&req, session.next_origin())
        .expect("the rename runs");
    assert!(!report.incomplete(), "{:?}", report.failed());
    assert!(dir.join("SUB").is_dir(), "the directory moved");

    // Every mutation above left a Go-parity record the log panel can page through.
    let page = session
        .log_query(&LogQuery {
            limit: Some(10),
            ..LogQuery::default()
        })
        .expect("the log reads");
    assert!(
        page.total >= 2,
        "a rename batch and a rename: {:?}",
        page.total
    );
    let batch = page
        .rows
        .iter()
        .find(|row| row.name.starts_with('['))
        .expect("a batch rename records Go's JSON-array `name`");
    assert_eq!(batch.action, "rename");
    assert_eq!(
        batch.sources.len(),
        2,
        "both rows are in the record: {:?}",
        batch.sources
    );
    assert!(batch.can_revert, "a rename is reversible (`is_reversible`)");
    assert!(!batch.reverted);
    assert!(batch.label.starts_with("Renamed"));
    assert_eq!(
        page.rows.first().map(|r| r.action),
        Some("rename"),
        "newest first"
    );
    // The `LogAppended` event is what routes a panel refresh; it fired for both writes.
    let events = session.poll();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::LogAppended { .. })),
        "the append is announced, not merely written"
    );
    // Revert it, and the marker shows up as a *flag* rather than as a re-read of the frame.
    let id = page.rows[0].id;
    let _ = session.log_record(id).expect("the record is addressable");
    let after = session
        .log_query(&LogQuery::default())
        .expect("and pageable again");
    assert_eq!(after.total, page.total, "paging did not mutate anything");
}

// ------------------------------------------------------------------------------- 6. the log surface

#[test]
fn the_log_page_carries_the_flags_the_panel_cannot_derive() {
    let dir = scratch_dir("log");
    fs::write(dir.join("f.txt"), b"x").expect("written");
    let session = plain(&dir);
    session
        .rename(
            &zenman_core::ops::rename::RenameReq::one(
                AbsPath::from_path(dir.join("f.txt")).expect("abs"),
                "g.txt",
            ),
            session.next_origin(),
        )
        .expect("a rename logs a record");
    let page = session
        .log_query(&LogQuery {
            limit: Some(1),
            ..LogQuery::default()
        })
        .expect("one row");
    assert_eq!(page.rows.len(), 1);
    assert!(page.has_more || page.total == 1);
    let row = &page.rows[0];
    assert_eq!(row.action, "rename");
    assert!(row.can_revert);
    assert_eq!(
        row.sources.len(),
        1,
        "the renamed path is the record's source"
    );
    assert_eq!(row.name, "g.txt");
    assert!(row.label.starts_with("Renamed"));
    let empty = LogPage::empty();
    assert_eq!(empty.total, 0);
    assert!(!empty.has_more);
    assert!(RESERVED.eq_ignore_ascii_case(workspaces::RESERVED));
}
