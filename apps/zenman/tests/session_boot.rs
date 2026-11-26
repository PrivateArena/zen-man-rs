//! The two things a user of this build can check for themselves: I closed the window and my tabs
//! came back, and this copy of the app kept its files beside itself.
//!
//! Both are driven through the **real binary**, in a directory of its own, because neither is
//! observable in-process: a reload is `AppPaths` → settings file → shell → report line, and portable
//! mode is a stat of the directory the executable actually ran from — which only a copy of the binary
//! beside a marker file can show. `apps/zenman/tests/gpu_fallback.rs` is the model for the subprocess
//! plumbing and for the skip rule: a host where `wgpu` sees no adapter has no window to close, and
//! that is plan OQ3, not a failure.
//!
//! Every run is isolated twice over: `--config` *and* `--data-dir` into a scratch tree (a run that
//! wrote the platform settings tree would overwrite the developer's own saved tabs), and the portable
//! case additionally redirects `HOME`/`XDG_*` into a folder it then proves stayed empty.

use std::path::{Path, PathBuf};
use std::process::Command;

// Scratch naming and stale-tree reaping: the shared test-support module (same file as `zenman-core`'s).
#[path = "../../../crates/zenman-core/src/scratch.rs"]
mod scratch;

/// The boot report the reload assertions read (`apps/zenman/src/app.rs` prints it).
const SESSION_NOTE: &str = "boot note: session";
/// The layout report, which is what makes portable mode checkable from stdout.
const LAYOUT_NOTE: &str = "boot note: layout:";

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    /// Everything the run said, for an assertion message.
    fn text(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }

    /// The first stdout line with this prefix, if the run printed one.
    fn line(&self, prefix: &str) -> Option<&str> {
        self.stdout.lines().find(|l| l.starts_with(prefix))
    }
}

/// Run a `zenman` binary and capture what it reported.
fn run(exe: &Path, args: &[&str], env: &[(&str, &str)]) -> Run {
    let mut cmd = Command::new(exe);
    cmd.args(args);
    // Never inherit a launcher's backend override or an ambient data directory: both would change the
    // very things these cases read — the exit code, and where the files land.
    for name in [
        "WGPU_BACKEND",
        "ZENMAN_GPU_BACKEND",
        "ZENMAN_DATA_DIR",
        "ZENMAN_CONFIG_DIR",
    ] {
        cmd.env_remove(name);
    }
    for (key, value) in env {
        // Set on the child, not on the test process, so a stray `HOME` in the caller's shell cannot
        // leak into the isolation the case depends on.
        cmd.env(key, value);
    }
    // `Command::output` is on this workspace's ban list (`clippy.toml`, plan F10: every spawn goes
    // through one audited executor); piped `spawn` + `wait_with_output` is the same thing explicitly.
    let output = match cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|child| child.wait_with_output())
    {
        Ok(output) => output,
        Err(e) => panic!("the zenman binary must be built to run this test: {e}"),
    };
    Run {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn zenman(args: &[&str]) -> Run {
    run(Path::new(env!("CARGO_BIN_EXE_zenman")), args, &[])
}

/// A tree per case, so two cases never share a settings file. The shared module makes each name
/// `zenman-session-boot-<tag>-<pid>-<seq>`: fresh, and reapable by the sweep if the run dies.
fn scratch(tag: &str) -> PathBuf {
    scratch::create(&format!("session-boot-{tag}"))
}

/// A tree beside the built binary, for the one case that needs *the executable's own directory* to
/// be somewhere else: portable layout is detected from `current_exe()`, so the marker has to sit
/// next to the file that runs.
///
/// `target/tmp` rather than the temp dir for one reason: it is the filesystem the binary is on, so
/// [`exe_beside_marker`] can hard-link it. A copy of a debug binary is ~76 MB, which on a
/// `tmpfs`-backed `/tmp` is 76 MB of RAM per run of this case.
fn exe_tree(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("zenman-session-boot-{tag}-{}", std::process::id()));
    // Creation-time pre-clean: the tree outlives a green run (the hard link costs nothing and
    // `cargo clean` takes the rest), so this is what stops the previous run's data files being read
    // as this run's report.
    let _ = scratch::remove_tree(&dir);
    std::fs::create_dir_all(&dir).expect("the exe tree is creatable");
    dir
}

/// Put a runnable `zenman` at `exe` without copying the bytes.
fn exe_beside_marker(exe: &Path) {
    let built = Path::new(env!("CARGO_BIN_EXE_zenman"));
    // A hard link is a second name for the same inode, so `current_exe()` reports *this* path — a
    // symlink would resolve back to the build directory and the marker would not be found. Linking
    // fails across filesystems (`EXDEV`), which is the only case that pays for a copy.
    std::fs::hard_link(built, exe)
        .or_else(|_| std::fs::copy(built, exe).map(|_| ()))
        .expect("the binary is beside its marker");
}

/// Every file under `root`, as paths (an empty answer is what "nothing was written" looks like).
fn files_under(root: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    read.flatten()
        .flat_map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                files_under(&path)
            } else {
                vec![path]
            }
        })
        .collect()
}

/// The tail of the count line `--print-adapters` prints (`apps/zenman/src/main.rs`).
const ADAPTER_COUNT_SUFFIX: &str = "adapter(s)";

/// `true` when `wgpu` sees an adapter — the same question as "can this host close a window".
///
/// The count is taken from its own line on **stdout**, not from the last line of the merged output: a
/// Mesa or ALSA message on stderr used to hide it, every case below then reported SKIP, and the file
/// passed without running anything. A missing count is a failure of the probe, not a skip.
fn skip_if_no_window() -> bool {
    let adapters = zenman(&["--print-adapters"]);
    assert_eq!(
        adapters.code,
        Some(0),
        "`--print-adapters` must work even where nothing can render (plan OQ3): {}",
        adapters.text()
    );
    let line = adapters
        .stdout
        .lines()
        .rev()
        .find(|l| l.trim_end().ends_with(ADAPTER_COUNT_SUFFIX))
        .unwrap_or_else(|| {
            panic!(
                "no adapter count line (… {ADAPTER_COUNT_SUFFIX}) on stdout, so this host's ability to open a window is unknowable and the cases below would be vacuous:\n{}",
                adapters.text()
            )
        });
    let count: usize = line
        .split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("an unreadable adapter count: {line:?}"));
    if count > 0 {
        return true;
    }
    println!(
        "SKIP: `wgpu` sees no adapter on this host, so there is no window to close (plan OQ3)."
    );
    false
}

/// One booted, self-closing run against an isolated settings tree.
///
/// `--safe-gpu` is the sanctioned headless boot (README): the GL rung presents to a plain X server,
/// where the Vulkan rung has no surface to present to. `--no-daemon` because each run of a case must
/// be its own first instance — otherwise it hands its folders to the previous one and exits.
fn smoke_run(tree: &Path, extra: &[&str]) -> Run {
    let mut args = vec![
        "--safe-gpu".to_string(),
        "--config".to_string(),
        tree.join("config").display().to_string(),
        "--data-dir".to_string(),
        tree.join("data").display().to_string(),
        "--no-daemon".to_string(),
    ];
    args.extend(extra.iter().map(|s| (*s).to_string()));
    args.extend(["--smoke".to_string(), "1".to_string()]);
    let owned: Vec<&str> = args.iter().map(String::as_str).collect();
    zenman(&owned)
}

/// The session file the shell writes, as text: the assertions read the state back off disk rather
/// than trusting the report line alone.
fn saved_session(tree: &Path) -> String {
    let path = tree.join("config").join("config.json");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{} must hold the session: {e}", path.display()))
}

#[test]
fn the_tabs_of_the_last_run_are_the_tabs_of_this_one() {
    if !skip_if_no_window() {
        return;
    }
    let tree = scratch("reload");

    // Run one: nothing saved yet, so the shell falls back to the home directory — and on the way out
    // it writes the session the next run reads.
    let first = smoke_run(&tree, &[]);
    assert_eq!(first.code, Some(0), "run one: {}", first.text());
    let report = first
        .line(SESSION_NOTE)
        .unwrap_or_else(|| panic!("no session report in:\n{}", first.text()))
        .to_string();
    assert!(
        report.contains("session: none saved yet"),
        "a first run should say there was nothing to restore, said: {report}"
    );
    assert!(
        saved_session(&tree).contains("\"workspaces\""),
        "run one saved no session file"
    );

    // Run two: the same tree, a new process, and a report that says what came back.
    let second = smoke_run(&tree, &[]);
    assert_eq!(second.code, Some(0), "run two: {}", second.text());
    let report = second
        .line(SESSION_NOTE)
        .unwrap_or_else(|| panic!("no session report in:\n{}", second.text()))
        .to_string();
    // The exact shape, because "could not be restored" contains "restored" too.
    assert!(
        report.contains("session: restored 1 tab(s) in 1 pane(s)"),
        "the session run one saved did not come back: {report}"
    );
}

#[test]
fn a_folder_on_the_command_line_opens_rather_than_restores() {
    if !skip_if_no_window() {
        return;
    }
    let tree = scratch("target");
    let folder = scratch("target-folder");
    std::fs::write(folder.join("note.txt"), b"opened from argv\n").expect("a file in the folder");
    // Seed a session first: the case is about a launch that must *not* restore it.
    assert!(
        smoke_run(&tree, &[]).code == Some(0),
        "the seeding run must boot"
    );

    let run = smoke_run(&tree, &[&folder.display().to_string()]);
    assert_eq!(run.code, Some(0), "{}", run.text());
    assert!(
        run.line(SESSION_NOTE).is_none(),
        "a launched folder must not be reported as a restored session: {:?}",
        run.line(SESSION_NOTE)
    );
    // And the folder really did open: the session that run saved names it, which is also how the next
    // plain launch will know what the user last asked for.
    let saved = saved_session(&tree);
    assert!(
        saved.contains(&folder.display().to_string()),
        "the launched folder is not what the run saved:\n{saved}"
    );
}

#[test]
fn a_session_file_on_the_command_line_is_the_window_that_opens() {
    if !skip_if_no_window() {
        return;
    }
    let tree = scratch("session-flag");
    let folder = scratch("session-folder");
    // The format is one entry of `config.json`'s `workspaces` object, spelled as the contract spells
    // it (`docs/contracts.md` → "Session save and restore"), which is how a user makes one: copy
    // `workspaces.default` out of the settings file.
    let file = tree.join("window.json");
    let written = format!(
        r#"{{
  "panes": {{ "left": {{ "tabs": [ {{ "id": "t1", "path": "{}", "name": "from file",
                                        "group": null, "view": "list" }} ], "active": 0 }} }},
  "active": "left",
  "layout": "single",
  "dock": null
}}"#,
        folder.display()
    );
    std::fs::write(&file, &written).expect("session file");

    let run = smoke_run(&tree, &["--session", &file.display().to_string()]);
    assert_eq!(run.code, Some(0), "{}", run.text());
    let report = run
        .line(SESSION_NOTE)
        .unwrap_or_else(|| panic!("no session report in:\n{}", run.text()));
    assert!(
        report.contains(&format!("session file {}", file.display()))
            && report.contains("restored 1 tab(s) in 1 pane(s)"),
        "the named session file was not applied: {report}"
    );
    // A session file is *read*, never written: it is a window to open, not a second place this build
    // keeps state.
    let after = std::fs::read_to_string(&file).expect("the session file is still there");
    assert_eq!(after, written, "`--session` rewrote the file it was handed");
    assert!(
        saved_session(&tree).contains("\"workspaces\""),
        "the settings tree lost its own autosave target"
    );
}

#[test]
fn a_session_file_that_is_not_a_session_is_said_not_guessed() {
    if !skip_if_no_window() {
        return;
    }
    let tree = scratch("session-bad");
    let file = tree.join("broken.json");
    std::fs::write(&file, b"{").expect("a broken session file");
    let run = smoke_run(&tree, &["--session", &file.display().to_string()]);
    assert_eq!(
        run.code,
        Some(0),
        "a bad session file is a note, not an exit: {}",
        run.text()
    );
    let report = run
        .line(SESSION_NOTE)
        .unwrap_or_else(|| panic!("no session report in:\n{}", run.text()));
    assert!(
        report.contains("not restored") && report.contains(&file.display().to_string()),
        "the refusal did not name the file: {report}"
    );
}

#[test]
fn a_marker_beside_the_binary_keeps_the_session_beside_the_binary() {
    if !skip_if_no_window() {
        return;
    }
    // Not the build's own path: a `portable.dat` beside `target/debug/zenman` would silently switch
    // every later run of that binary — including this suite's other cases — to portable mode.
    let tree = exe_tree("portable");
    let exe = tree.join("zenman");
    exe_beside_marker(&exe);
    std::fs::write(tree.join("portable.dat"), b"").expect("the marker is an empty file");
    // A home that stays free of this app's files is the whole claim of portable mode: the platform
    // layout would have written all three of these (`directories::ProjectDirs` for `io.storytold.zenman`).
    // A driver's shader cache under `.cache` is not this app's state, and not part of the claim.
    let fake_home = tree.join("home");
    std::fs::create_dir_all(&fake_home).expect("fake home");
    let home = fake_home.display().to_string();
    let config = fake_home.join(".config").display().to_string();
    let data = fake_home.join(".local/share").display().to_string();
    let cache = fake_home.join(".cache").display().to_string();
    let env = [
        ("HOME", home.as_str()),
        ("XDG_CONFIG_HOME", config.as_str()),
        ("XDG_DATA_HOME", data.as_str()),
        ("XDG_CACHE_HOME", cache.as_str()),
        // The software/legacy GL ladder compiles shaders through Mesa, which caches 2 MB of them
        // under `XDG_CACHE_HOME` — inside the tree whose emptiness this case is asserting about.
        // The cache is the driver's, not the app's, so switch it off rather than move it out.
        ("MESA_SHADER_CACHE_DISABLE", "1"),
    ];
    let env: Vec<(&str, &str)> = env.to_vec();

    let first = run(&exe, &["--safe-gpu", "--no-daemon", "--smoke", "1"], &env);
    assert_eq!(first.code, Some(0), "portable run: {}", first.text());
    let layout = first
        .line(LAYOUT_NOTE)
        .unwrap_or_else(|| panic!("no layout report in:\n{}", first.text()));
    assert!(
        layout.contains("portable"),
        "the marker beside the binary was not honoured: {layout}"
    );
    let tree_data = tree.join("data");
    for name in ["config.json", "config.toml", "action.log"] {
        assert!(
            tree_data.join(name).is_file(),
            "portable mode must keep {name} beside the binary, not in a home directory: {}",
            tree_data.display()
        );
    }
    for platform_dir in [".config/zenman", ".local/share/zenman", ".cache/zenman"] {
        assert!(
            !fake_home.join(platform_dir).exists(),
            "portable mode wrote {} under the home directory it was told about: {}",
            platform_dir,
            fake_home.display()
        );
    }
    let ours = [
        "config.json",
        "config.toml",
        "ui.ron",
        "action.log",
        "zen-man.db",
    ];
    let leaked: Vec<PathBuf> = files_under(&fake_home)
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| ours.contains(&name))
        })
        .collect();
    assert!(
        leaked.is_empty(),
        "portable mode wrote its own files under the home directory it was told about: {leaked:?}"
    );

    // The reload, out of the tree beside the binary.
    let second = run(&exe, &["--safe-gpu", "--no-daemon", "--smoke", "1"], &env);
    assert_eq!(
        second.code,
        Some(0),
        "second portable run: {}",
        second.text()
    );
    let report = second
        .line(SESSION_NOTE)
        .unwrap_or_else(|| panic!("no session report in:\n{}", second.text()));
    assert!(
        report.contains("restored 1 tab(s)"),
        "the portable session did not reload: {report}"
    );
}
