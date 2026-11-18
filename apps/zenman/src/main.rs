//! `zenman` — the native binary (plan §5.9). Step 1 is the GPU-safe scaffold: CLI, path resolution,
//! crash guard, sentinel/probe/bless boot order, and a window that can prove it rendered.
//!
//! Boot order, plan §5.1 row for this file and the `gpu_startup` row: **sentinel → probe → bless →
//! run**. There is no MCP/CLI subcommand surface: plan §10 decision 6 keeps the headless target out of
//! v1, so `main` is one flat flag set, not a subcommand tree.
//!
//! Exit codes (also in `docs/crash-reports.md`, and what `apps/zenman/tests/gpu_fallback.rs` asserts):
//!
//! | code | meaning |
//! |---|---|
//! | 0 | ran, or a documented stub printed its landing step |
//! | 1 | the run failed for a non-graphics reason (window/event loop) |
//! | 2 | the layout could not be resolved (`--data-dir`/`--config` unusable) |
//! | 3 | **no GPU profile had an adapter** — the ladder was walked and every rung came back empty |
//! | 4 | a device was created for the chosen profile and `run_native` still failed: a driver fault, so
//! |    the sentinel marker is deliberately left behind for the next start (plan F18) |
//! | 5 | a panic escaped into [`crash_guard`] |

// Plan §2.1/§5.1 per-crate header: the app returns errors, it does not panic. `clippy::` prefix is
// required — bare `wildcard_imports` is an unknown lint to rustc (that was the tree's warning).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]
#![deny(clippy::wildcard_imports)]

mod app;
mod app_dirs;
mod crash_guard;
mod gpu_startup;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;

/// The plan step this binary implements; echoed in the window title area (see `app::ZenApp::show`).
pub const PLAN_STEP: u32 = 1;

/// App id and window title (plan §5.9 `main.rs` row).
const APP_ID: &str = "ZenMan";
const APP_TITLE: &str = "Zen-Man";

/// Exit codes; see the table in the module docs.
const EXIT_OK: u8 = 0;
const EXIT_RUNTIME: u8 = 1;
const EXIT_CONFIG: u8 = 2;
const EXIT_NO_GPU_PROFILE: u8 = 3;
const EXIT_GPU_DEVICE_FAILED: u8 = 4;
const EXIT_INTERNAL: u8 = 5;

/// zen-man: a native egui file manager (port of zen-man; see `ARCHITECTURE_PLAN_zen-man-rs-egui-port.md`).
#[derive(Parser, Debug)]
#[command(name = "zenman", version, about)]
struct Cli {
    /// Use this data directory instead of the platform default (also ZENMAN_DATA_DIR).
    #[arg(long, value_name = "DIR")]
    data_dir: Option<PathBuf>,

    /// Use this config directory instead of the platform default (also ZENMAN_CONFIG_DIR).
    #[arg(long, value_name = "DIR")]
    config: Option<PathBuf>,

    /// Restore this session file on boot.
    #[arg(long, value_name = "FILE")]
    session: Option<PathBuf>,

    /// Print every adapter wgpu can see (`backend | device_type | name`) and exit 0.
    #[arg(long)]
    print_adapters: bool,

    /// Skip the top rung of the GPU ladder (Vulkan/Metal/DX12) and start from GL.
    #[arg(long)]
    safe_gpu: bool,

    /// Close the window after SECONDS and exit 0: the headless proof that the app booted.
    #[arg(long, value_name = "SECONDS")]
    smoke: Option<f64>,

    /// Do not use a hardware GPU: try only the last-resort (software-capable) profile.
    #[arg(long)]
    disable_gpu: bool,

    /// Force the legacy-GL profile (a driver bug workaround; not the glow renderer).
    #[arg(long)]
    gl: bool,

    /// Run the golden/parity suite. Stub: `xtask snapshots` and the `egui_kittest` harness land in
    /// step 7, and `xtask parity` in step 15 (plan §7).
    #[arg(long, hide = true)]
    test: bool,

    /// Regenerate the snapshot baselines. Stub: step 7 (`xtask snapshots`).
    #[arg(long, hide = true)]
    update_golden: bool,

    /// Record a new reference capture for human review. Stub: step 15 (`xtask parity`).
    #[arg(long, hide = true)]
    record_golden: bool,

    /// Restrict the golden run to tests whose name contains FILTER. Stub: step 7.
    #[arg(long, value_name = "FILTER", hide = true)]
    filter: Option<String>,

    /// Compare the current render against a committed baseline and exit non-zero on a diff.
    /// Stub: step 7.
    #[arg(long, hide = true)]
    snapshot_compare: bool,

    /// Directory holding the golden fixtures. Stub: step 2 captures them, step 7 reads them.
    #[arg(long, value_name = "DIR", hide = true)]
    golden_dir: Option<PathBuf>,
}

impl Cli {
    fn path_overrides(&self) -> app_dirs::Overrides {
        app_dirs::Overrides {
            data_dir: self.data_dir.clone(),
            config: self.config.clone(),
            session: self.session.clone(),
        }
    }

    /// The six parity flags, which need fixtures that do not exist until steps 2/7/15. They are
    /// accepted today so scripts and CI matrices can be written against the final CLI now.
    fn parity_stub(&self) -> Option<String> {
        let mut asked = Vec::new();
        if self.test {
            asked.push("--test (step 7)");
        }
        if self.update_golden {
            asked.push("--update-golden (step 7: xtask snapshots)");
        }
        if self.record_golden {
            asked.push("--record-golden (step 15: xtask parity)");
        }
        if self.filter.is_some() {
            asked.push("--filter (step 7)");
        }
        if self.snapshot_compare {
            asked.push("--snapshot-compare (step 7)");
        }
        if self.golden_dir.is_some() {
            asked.push("--golden-dir (step 2 captures the fixtures, step 7 reads them)");
        }
        (!asked.is_empty()).then(|| {
            format!(
                "{} land in steps 2/7/15 of the plan; nothing to compare against yet.",
                asked.join(", ")
            )
        })
    }
}

/// A boot failure with the exit code that describes it.
struct BootError {
    code: u8,
    message: String,
}

impl BootError {
    fn new(code: u8, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    if let Some(stub) = cli.parity_stub() {
        println!("zenman: {stub}");
        return ExitCode::from(EXIT_OK);
    }

    // Before any path or window work: `--print-adapters` must work on a machine whose app data is
    // unusable, because that is exactly when it gets run (plan OQ3).
    if cli.print_adapters {
        return gpu_startup::probe_print_and_exit();
    }

    let paths = match app_dirs::AppPaths::resolve(&cli.path_overrides()) {
        Ok(paths) => paths,
        Err(e) => {
            eprintln!("zenman: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    crash_guard::install_hook(&paths.data.join("crashes"));

    match crash_guard::guard("startup", || run(&cli, &paths)) {
        Ok(Ok(code)) => ExitCode::from(code),
        Ok(Err(BootError { code, message })) => {
            eprintln!("zenman: {message}");
            ExitCode::from(code)
        }
        Err(report) => {
            eprintln!("zenman: {report}");
            ExitCode::from(EXIT_INTERNAL)
        }
    }
}

/// Everything after argument parsing. The nested `Ok` from [`crash_guard::guard`] carries a code that
/// is not 0 for the planned early exits (`--print-adapters` is handled in `main` before this runs).
fn run(cli: &Cli, paths: &app_dirs::AppPaths) -> Result<u8, BootError> {
    let smoke = match cli.smoke {
        Some(seconds) if !seconds.is_finite() || seconds < 0.0 => {
            return Err(BootError::new(
                EXIT_CONFIG,
                format!("--smoke needs a non-negative number of seconds, got {seconds}"),
            ));
        }
        Some(seconds) => Some(Duration::from_secs_f64(seconds)),
        None => None,
    };

    // First-run migration is step 4's job (plan §5.4 `store/legacy.rs`), but noticing the Go app's file
    // is step 1's: a user who sees this line knows the port will bring the bookmarks/workspaces across
    // (and that nothing has been copied yet, because nothing has been).
    if let Some(legacy) = paths.zenman_go_legacy_config()
        && legacy.is_file()
    {
        println!(
            "zenman: found the Go app's config at {}; its one-time import is step 4 (plan §5.4) — nothing was copied",
            legacy.display()
        );
    }

    let mut ladder = gpu_startup::select_ladder(cli.safe_gpu, cli.gl, cli.disable_gpu);
    let sentinel = gpu_startup::Sentinel::new(&paths.gpu_dir());

    // 1. sentinel. A marker left behind means the previous start died inside a driver: drop the top
    // rung so this one does not touch it (plan §5.9: "keep the marker so the next start drops a
    // backend"), unless the user already pinned a narrower ladder.
    let died_last_time = sentinel.exists();
    if died_last_time && ladder.len() > 1 {
        println!(
            "zenman: {} was left by an unfinished start; dropping the `{}` rung",
            sentinel.marker_path().display(),
            ladder[0].name
        );
        ladder.remove(0);
    }
    // A profile that completed a run before is tried first, unless it is the one that just died.
    if !died_last_time
        && let Some(blessed) = sentinel.read_blessed()
        && let Some(at) = ladder.iter().position(|p| p.name == blessed.profile)
        && at > 0
    {
        let profile = ladder.remove(at);
        println!(
            "zenman: trying the blessed profile `{}` first",
            profile.name
        );
        ladder.insert(0, profile);
    }

    // 2. probe. No window exists yet, so this is the last point at which a driver can be avoided.
    let (index, adapters) = match gpu_startup::probe_first_available(&ladder) {
        gpu_startup::Probe::Found { index, adapters } => (index, adapters),
        gpu_startup::Probe::Empty { tried } => {
            let seen: Vec<String> = tried
                .iter()
                .map(|(name, n)| format!("{name}={n}"))
                .collect();
            let suffix = if cli.disable_gpu {
                ""
            } else {
                " Retry with --safe-gpu or --gl, or install a software rasteriser (lavapipe/llvmpipe)."
            };
            return Err(BootError::new(
                EXIT_NO_GPU_PROFILE,
                format!(
                    "no GPU profile had an adapter (adapters per rung: {}); `zenman --print-adapters` lists what wgpu sees.{suffix}",
                    seen.join(" ")
                ),
            ));
        }
    };
    let chosen = &ladder[index];
    let winner = &adapters[0];
    println!(
        "gpu: profile={} kind={} backend={} device_type={} adapter=\"{}\"",
        chosen.name,
        chosen.kind.as_str(),
        winner.backend,
        winner.device_type,
        winner.name
    );
    println!("{LAST_ADAPTER_COUNT_NOTE}{}", adapters.len());

    // 3. bless-pending: record the intent before the driver is touched, and remember the last profile
    // that made it through a whole run.
    if let Err(e) = sentinel.create(chosen, winner) {
        println!(
            "zenman: warning: could not write {}: {e} (a driver crash will not be detected next start)",
            sentinel.marker_path().display()
        );
    }

    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id(APP_ID)
            .with_title(APP_TITLE)
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([900.0, 480.0])
            .with_drag_and_drop(true),
        // Window/UI state, plan §2.2's craft-settings policy. `ui.ron` needs the `persistence` feature,
        // which `egui`'s own dock state also uses from step 7 on.
        persistence_path: Some(paths.config.join("ui.ron")),
        centered: paths.session.is_none(),
        ..Default::default()
    };
    chosen.setup.apply(&mut options.wgpu_options.wgpu_setup);

    let boot = format!(
        "gpu: {} / {} / {}",
        chosen.name, winner.backend, winner.name
    );
    let app_paths = paths.clone();

    match eframe::run_native(
        APP_TITLE,
        options,
        Box::new(move |cc: &eframe::CreationContext<'_>| {
            // Image loaders are part of plan §5.9's boot list; the SVG registry that needs them is step 7.
            egui_extras::install_image_loaders(&cc.egui_ctx);
            Ok(Box::new(app::ZenApp::new(app_paths, boot, smoke)))
        }),
    ) {
        Ok(()) => {
            // 4. bless. Only a run that returned normally gets to say the driver is good.
            if let Err(e) = sentinel.bless(chosen, winner) {
                println!(
                    "zenman: warning: could not update {}: {e}",
                    sentinel.path().display()
                );
            }
            Ok(EXIT_OK)
        }
        Err(e @ eframe::Error::Wgpu(_)) => Err(BootError::new(
            EXIT_GPU_DEVICE_FAILED,
            format!(
                "the `{}` profile could not create a rendering context: {e}. The marker {} is kept, so the next start drops this rung; `--safe-gpu` does it immediately.",
                chosen.name,
                sentinel.marker_path().display()
            ),
        )),
        Err(e) => Err(BootError::new(
            EXIT_RUNTIME,
            format!("the event loop failed: {e}"),
        )),
    }
}

/// Label for the adapter-count line, kept as a constant so the machine-readable boot report is
/// greppable from `tests/gpu_fallback.rs`.
const LAST_ADAPTER_COUNT_NOTE: &str = "gpu: adapters visible to this profile: ";
