//! Plan §6.1 **F18** / §7 step 1 acceptance: the GPU ladder and its failure code, driven through the
//! real CLI in a subprocess (an in-process test could not prove that a rejected ladder exits with a
//! code, and a mock would prove nothing about `wgpu` at all).
//!
//! Both cases run with `--data-dir` pointed at a fresh temporary directory so the developer's real
//! sentinel is never touched, and each `--smoke 1` self-closes after one second (a real window on a
//! desktop, which the step-1 criteria explicitly ask for).
//!
//! Nothing here can pass falsely: case (a) first asks `--print-adapters` what this machine has and
//! *skips with a printed reason* when `wgpu` sees no adapter, exactly as plan §9 disposition 11 wants
//! `xtask snapshots` to behave.

use std::path::PathBuf;
use std::process::Command;

// Scratch naming and stale-tree reaping: the shared test-support module (same file as `zenman-core`'s).
#[path = "../../../crates/zenman-core/src/scratch.rs"]
mod scratch;

/// The boot report line `main::run` prints (also matched by `docs/crash-reports.md`).
const BOOT_REPORT: &str = "gpu: profile=";
/// Plan §5.9: the code for "every rung of the ladder had no adapter".
const EXIT_NO_GPU_PROFILE: i32 = 3;

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    fn combined(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

fn zenman(args: &[&str], env: &[(&str, &str)]) -> Run {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zenman"));
    cmd.args(args);
    for (key, value) in env {
        cmd.env(key, value);
    }
    // Never inherit the caller's `WGPU_BACKEND`: it would silently retarget the ladder under test.
    cmd.env_remove("WGPU_BACKEND");
    // This suite overrides the data and config directories but not `HOME` (the ladder's own answer
    // must come from the host), so without this Mesa would cache the shaders of every case into the
    // developer's real `~/.cache`.
    cmd.env("MESA_SHADER_CACHE_DISABLE", "1");
    // `Command::output` is on this workspace's ban list (`clippy.toml`, plan F10: every spawn goes
    // through one audited executor); piped `spawn` + `wait_with_output` is the same thing explicitly.
    let spawned = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let output = match spawned.and_then(|child| child.wait_with_output()) {
        Ok(output) => output,
        Err(e) => panic!("the zenman binary must be built to run this test: {e}"),
    };
    Run {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// A data directory per test, so the two cases cannot see each other's sentinel — named and
/// reaped by the shared scratch module (`zenman-gpu-fallback-<tag>-<pid>-<seq>`).
fn scratch(tag: &str) -> PathBuf {
    scratch::create(&format!("gpu-fallback-{tag}"))
}

/// What `wgpu` can see on this machine, as `--print-adapters` reports it.
#[test]
fn print_adapters_reports_and_exits_zero() {
    let run = zenman(&["--print-adapters"], &[]);
    assert_eq!(
        run.code,
        Some(0),
        "--print-adapters must never fail, even with no adapter: {}",
        run.combined()
    );
    assert!(
        run.stdout.contains("backend | device_type | name"),
        "missing the table header: {}",
        run.stdout
    );
    assert!(
        run.stdout.contains(" adapter(s)"),
        "missing the adapter count: {}",
        run.stdout
    );
}

/// Case (a): the ladder's first rung is accepted and the app boots and self-closes.
#[test]
fn an_available_profile_is_accepted_and_the_smoke_run_exits_zero() {
    let adapters = zenman(&["--print-adapters"], &[]);
    let count: usize = adapters
        .stdout
        .lines()
        .next_back()
        .and_then(|line| line.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    if count == 0 {
        println!(
            "SKIP: `wgpu` sees no adapter on this host ({}), so there is no profile to accept. That is OQ3, not a failure.",
            adapters.stdout.lines().next_back().unwrap_or("")
        );
        return;
    }

    let data = scratch("profile0");
    let run = zenman(
        &[
            "--data-dir",
            &data.display().to_string(),
            // `--config` too, not just `--data-dir`: the settings tree is where the session lives, and
            // a smoke run that wrote it would overwrite the developer's own saved tabs.
            "--config",
            &data.join("config").display().to_string(),
            "--smoke",
            "1",
        ],
        &[],
    );
    let text = run.combined();
    assert_eq!(
        run.code,
        Some(0),
        "a machine with {count} adapter(s) must boot and self-close; output:\n{text}"
    );
    let line = text
        .lines()
        .find(|l| l.starts_with(BOOT_REPORT))
        .unwrap_or_else(|| panic!("no boot report ({BOOT_REPORT}…) in:\n{text}"));
    println!("{line}");
    let chosen = line
        .trim_start_matches(BOOT_REPORT)
        .split_whitespace()
        .next()
        .unwrap_or("");
    let has_hardware = adapters.stdout.lines().any(|l| {
        l.starts_with("Vulkan | ")
            || l.starts_with("Metal | ")
            || l.starts_with("DX12 | ")
            || l.starts_with("Dx12 | ")
    });
    if has_hardware {
        assert_eq!(
            chosen, "gpu",
            "a hardware adapter exists, so profile index 0 (`gpu`) must be the accepted one; got `{chosen}` from:\n{line}"
        );
    } else {
        println!(
            "NOTE: no Vulkan/Metal/DX12 adapter, so the ladder correctly fell through to `{chosen}`."
        );
        assert!(
            !chosen.is_empty(),
            "a fallback profile must still be reported"
        );
    }

    // The blessed record is what makes the next start try this rung first (plan §5.9 "bless").
    let last_good = data.join("gpu").join("last-good.toml");
    let record = std::fs::read_to_string(&last_good).unwrap_or_else(|e| {
        panic!(
            "{} should record the working profile: {e}",
            last_good.display()
        )
    });
    assert!(
        record.contains(&format!("profile = \"{chosen}\"")),
        "unexpected last-good.toml:\n{record}"
    );
    assert!(
        !data.join("gpu").join("starting.toml").exists(),
        "a clean exit must clear the pending marker"
    );
}

/// Case (b): every rung rejected ⇒ the documented code 3, before any window is created.
#[test]
fn all_profiles_rejected_exits_with_code_three() {
    // Force a backend this build cannot have: `metal` is not compiled on Linux/Windows, `dx12` not on
    // Linux/macOS. The rejection is then `wgpu`'s own, not a mock (see `gpu_startup::select_ladder`).
    let forced = if cfg!(target_os = "macos") {
        "dx12"
    } else {
        "metal"
    };
    let data = scratch("exhausted");
    let run = zenman(
        &[
            "--data-dir",
            &data.display().to_string(),
            "--config",
            &data.join("config").display().to_string(),
            "--smoke",
            "1",
        ],
        &[
            ("ZENMAN_GPU_BACKEND", forced),
            ("ZENMAN_DATA_DIR", &data.display().to_string()),
            (
                "ZENMAN_CONFIG_DIR",
                &data.join("config").display().to_string(),
            ),
        ],
    );
    let text = run.combined();
    assert!(
        text.contains("no GPU profile had an adapter"),
        "expected the exhausted-ladder message, got:\n{text}"
    );
    assert_eq!(
        run.code,
        Some(EXIT_NO_GPU_PROFILE),
        "an exhausted ladder must exit {EXIT_NO_GPU_PROFILE}; got {:?} from:\n{text}",
        run.code
    );
    assert!(
        !data.join("gpu").join("starting.toml").exists(),
        "the marker is only written once a rung is accepted"
    );
}
