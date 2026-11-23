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
mod config_boot;
mod crash_guard;
mod gpu_startup;
mod i18n;
mod single_instance;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use zenman_core::path::{FsPath, VfsKind};

/// The plan step this binary implements; echoed in the window title area (see `app::ZenApp::show`).
pub const PLAN_STEP: u32 = 7;

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

    /// Report what importing the Go app's `~/.config/zen-man` would do, and exit. Dry run unless
    /// `--apply` is given (plan §5.4 `store/legacy.rs`).
    #[arg(long)]
    migrate_config: bool,

    /// With `--migrate-config`: actually write the `config.json.bak` backup and import the data.
    #[arg(long, requires = "migrate_config")]
    apply: bool,

    /// Do not claim the single-instance socket: run a second, independent copy (plan §5.9).
    #[arg(long)]
    no_daemon: bool,

    /// Synonym of `--no-daemon`, spelled the way the plan's flag is (plan §5.9 `single_instance` row).
    #[arg(long)]
    new_instance: bool,

    /// Suffix this instance's identity, and therefore its single-instance socket, for parallel dev
    /// and test runs (`--app-variant=steptest`).
    #[arg(long, value_name = "VARIANT", default_value = "")]
    app_variant: String,

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

    /// Open these folders — the desktop's file-manager contract.
    ///
    /// A file manager is launched *at a target*: `inode/directory=…zenman.desktop` makes the panel,
    /// Thunar's "open in file manager" and `xdg-open <dir>` all run `zenman <uri>`. So the target is a
    /// positional argument, accepted more than once (each one opens a tab), spelled either as a path
    /// (`zenman ~/notes`) or as the `file://` URI a desktop passes for `%u` (`zenman file:///tmp`).
    /// Without this, every one of those launches is a clap error and the desktop's answer is a modal
    /// "failed to execute default file manager" box.
    #[arg(value_name = "PATH")]
    paths: Vec<String>,
}

impl Cli {
    /// The instance identity: `ZenMan`, or `ZenMan-<variant>` (plan §5.9: the socket name must follow the
    /// variant so two runs do not steal each other's claim).
    fn app_id(&self) -> String {
        if self.app_variant.is_empty() {
            APP_ID.to_string()
        } else {
            format!("{APP_ID}-{}", self.app_variant)
        }
    }

    /// Either spelling of the bypass flag (plan §5.9 names `--new-instance`; the step-4 brief names
    /// `--no-daemon`).
    fn claims_nothing(&self) -> bool {
        self.no_daemon || self.new_instance
    }

    /// The positional targets, as the folders to list: a `file://` URI decoded, `~` and a relative path
    /// put through plan §4.2's resolver, and a *file* replaced by the folder that holds it (that is what
    /// "show me this file" means to a file manager, short of selecting it).
    ///
    /// A target that names nothing openable is reported on stderr and skipped, never fatal: two folders
    /// were asked for and one of them moved, the other still opens.
    fn targets(&self, home: Option<&Path>) -> Vec<FsPath> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let mut out = Vec::with_capacity(self.paths.len());
        for arg in &self.paths {
            // A URI is decoded, a path is not: a directory name may legitimately contain a `%`, so the
            // scheme is what decides — and a scheme that is not `file:` is refused rather than turned
            // into the folder `./https:/example.com/x`, which is what resolving it against the cwd did.
            let decoded = match uri_scheme(arg).as_deref() {
                None => Some(arg.to_owned()),
                Some("file") => zenman_core::path::decode_file_uri(arg),
                Some(other) => {
                    eprintln!("zenman: {other}: is not a folder on this machine: {arg}");
                    None
                }
            };
            let Some(raw) = decoded else {
                continue;
            };
            let Ok(path) = FsPath::resolve(&raw, &cwd, home) else {
                eprintln!("zenman: cannot open {arg}");
                continue;
            };
            // A file's parent, when there is one: `/` has none, and `/` is never a plain file anyway.
            out.push(match (is_plain_file(&path), path.parent()) {
                (true, Some(parent)) => parent,
                _ => path,
            });
        }
        out
    }

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

/// The scheme of a URI argument, lower-cased, or `None` for something that is a path.
///
/// The test is [RFC 3986]'s `scheme ":" //", with one exception this program cannot make without being
/// wrong twice: a single letter followed by a colon is a Windows drive (`C:\Users\zm\notes`), and a file
/// manager on that platform is handed those all day. So a one-letter "scheme" is a path.
///
/// [RFC 3986]: https://www.rfc-editor.org/rfc/rfc3986#section-3.1
fn uri_scheme(raw: &str) -> Option<String> {
    let (head, _) = raw.split_once(':')?;
    if head.len() < 2 {
        return None;
    }
    let letters = head.to_ascii_lowercase();
    let scheme = letters.as_bytes();
    let named = !scheme.is_empty()
        && scheme[0].is_ascii_alphabetic()
        && scheme
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'));
    named.then_some(letters)
}

/// Is `path` a file on a real filesystem, as opposed to a directory, a store member or something that
/// cannot be asked? Only the first makes "open the folder that holds it" the right reading of the
/// command line; an archive path is a folder to *enter*, and a missing path is left to the listing to
/// report in its own tab.
fn is_plain_file(path: &FsPath) -> bool {
    matches!(path.store, VfsKind::Fs)
        && path
            .host_abs()
            .is_some_and(|abs| std::fs::metadata(abs.as_path()).is_ok_and(|m| !m.is_dir()))
}

/// A boot failure with the exit code that describes it.
#[derive(Debug)]
pub(crate) struct BootError {
    pub(crate) code: u8,
    pub(crate) message: String,
}

impl BootError {
    pub(crate) fn new(code: u8, message: impl Into<String>) -> Self {
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

    // The migration runs before any window work and never opens one: it is the one boot operation that
    // touches a file the port does not own, so it is a command, not a side effect (`--apply` opts in).
    if cli.migrate_config {
        return match config_boot::migrate(&paths, cli.apply) {
            Ok(report) => {
                println!("{report}");
                ExitCode::from(EXIT_OK)
            }
            Err(BootError { code, message }) => {
                eprintln!("zenman: {message}");
                ExitCode::from(code)
            }
        };
    }

    match crash_guard::guard("startup", || run_with_context(&cli, &paths)) {
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

/// Everything after argument parsing, in plan §5.9's order: **`AppPaths` → `SystemInfo` → config →
/// single instance → GPU ladder → window.** The first three are [`config_boot`]'s job and are kept out of
/// this function so the boot order stays readable; the nested `Ok` from [`crash_guard::guard`] carries a
/// code that is not 0 for the planned early exits (`--print-adapters` is handled in `main` before this
/// runs).
fn run_with_context(cli: &Cli, paths: &app_dirs::AppPaths) -> Result<u8, BootError> {
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

    // 1. SystemInfo, then the settings — validating them here is what makes the `limits.*` caps real
    // before any directory is walked (plan §5.4).
    let context = config_boot::boot(paths)?;
    for line in context.report_lines() {
        println!("{line}");
    }
    // The one read of the Go app's directory, and it is a read: the import itself is `--migrate-config`.
    if let Some(note) = config_boot::legacy_preview_note(paths) {
        println!("boot note: {note}");
    }

    // 2. one instance per data directory. The undo log and the SQLite index are shared files, so a
    // second window is a corruption bug rather than a convenience: hand the request to the first and
    // exit 0. `_instance` is handed to the shell, which holds it for the whole run and releases the
    // socket on the way out — and which drains it every frame, because the desktop's answer to
    // "open this folder" on a running file manager has to arrive somewhere.
    let app_id = cli.app_id();
    let targets = cli.targets(paths.home().as_deref());
    let wire: Vec<String> = targets.iter().map(FsPath::to_string).collect();
    let _instance = match single_instance::claim_or_focus(
        &app_id,
        paths,
        cli.claims_nothing(),
        &wire,
    ) {
        Ok(single_instance::Claim::AlreadyRunning { socket }) => {
            let what = match wire.len() {
                0 => "to focus".to_string(),
                1 => format!("to open {}", wire[0]),
                n => format!("to open {n} folders"),
            };
            println!(
                "zenman: another instance owns this data directory ({}); asked it {what} and exiting",
                socket.display()
            );
            return Ok(EXIT_OK);
        }
        Ok(single_instance::Claim::First(guard)) => {
            if let Some(path) = guard.socket_path() {
                println!("zenman: instance socket {}", path.display());
            } else {
                println!("zenman: --no-daemon/--new-instance: no instance socket was claimed");
            }
            guard
        }
        Err(e) => {
            return Err(BootError::new(
                EXIT_RUNTIME,
                format!("could not take the single-instance claim: {e}"),
            ));
        }
    };
    for folder in &targets {
        println!("zenman: opening {folder}");
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

    // 2b. the engine session (plan §4.1): the single owner of the log, the database, the job
    // table and the directory cache. It opens *after* the instance claim, because those are the
    // shared files the claim protects (plan F5). A failure here is a boot failure — the log could
    // not be opened at all — and the honest answer is exit 1 with the reason, not a window that
    // cannot undo.
    let session_paths = zenman_core::session::SessionPaths::new(
        config_boot::config_path(paths),
        paths.data.join("action.log"),
        (context.config.store_mode != zenman_core::store::config::StoreMode::Ephemeral)
            .then(|| paths.data.join("zen-man.db")),
        paths.data.clone(),
    );
    let (session, startup_notes) = match zenman_core::session::Session::open(
        session_paths,
        context.config.clone(),
        zenman_core::platform::default_env(),
        context.config.limits(),
    ) {
        Ok(opened) => opened,
        Err(e) => {
            return Err(BootError::new(
                EXIT_RUNTIME,
                format!("the engine could not start: {}", e.user_message()),
            ));
        }
    };
    for note in &startup_notes {
        println!("startup note: {}", note.message());
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
            // Image loaders are part of plan §5.9's boot list; the SVG registry that needs them
            // landed with step 7 (`zenman_ui::icons`).
            egui_extras::install_image_loaders(&cc.egui_ctx);
            // Fonts (plan §10 R9: egui defaults + system fallback, no vendored Outfit) and the
            // persisted theme, before the first frame draws anything.
            zenman_ui::theme::fonts::install(&cc.egui_ctx);
            zenman_ui::theme::apply(&cc.egui_ctx, session.config().theme);
            Ok(Box::new(app::ShellApp::new(
                &app_paths,
                boot,
                smoke,
                session,
                &startup_notes,
                targets,
                _instance,
            )))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Cli {
        // `expect`, and that is the test: before the positional target existed every one of these
        // command lines was a clap error, and the desktop answered it with a modal dialog.
        Cli::try_parse_from(args).unwrap_or_else(|e| panic!("{args:?}: {e}"))
    }

    /// The desktop's file-manager contract, at the front door.
    #[test]
    fn a_launched_target_is_accepted_and_resolved() {
        let home = Path::new("/home/zm");
        assert!(
            cli(&["zenman"]).targets(None).is_empty(),
            "no target is the ordinary start"
        );
        assert_eq!(
            cli(&["zenman", "/tmp"]).targets(None)[0].to_string(),
            "/tmp",
            "a path from a terminal"
        );
        assert_eq!(
            cli(&["zenman", "file:///tmp/My%20Docs"]).targets(None)[0].to_string(),
            "/tmp/My Docs",
            "a `%u` URI from a .desktop file"
        );
        assert_eq!(
            cli(&["zenman", "~/notes"]).targets(Some(home))[0].to_string(),
            "/home/zm/notes",
            "the home the boot resolved, not the process's"
        );
        let two: Vec<String> = cli(&["zenman", "/tmp", "/var"])
            .targets(None)
            .iter()
            .map(FsPath::to_string)
            .collect();
        assert_eq!(two, ["/tmp", "/var"], "two targets open in order");
    }

    /// A target that is not a folder does not stop the launch: a file is shown as its folder, and
    /// anything the desktop invented is reported on stderr and skipped, because the other tab was asked
    /// for too and still deserves to open.
    #[test]
    fn a_file_target_opens_the_folder_that_holds_it_and_a_foreign_one_is_dropped() {
        let dir = std::env::temp_dir().join(format!("zenman-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp fixture directory");
        let file = dir.join("notes.txt");
        std::fs::write(&file, b"x").expect("temp fixture file");
        let lossy = file.to_string_lossy().into_owned();
        assert_eq!(
            cli(&["zenman", &lossy]).targets(None)[0].to_string(),
            dir.to_string_lossy(),
            "`zenman notes.txt` means show me the folder holding it"
        );
        // The same folder spelled as the URI a .desktop file sends, and a nested folder whose name
        // needs an escape: the decoder is what puts the space back.
        std::fs::create_dir_all(dir.join("My Docs")).expect("a fixture folder needing an escape");
        let uri = format!("file://{}/My%20Docs", dir.to_string_lossy());
        assert_eq!(
            cli(&["zenman", &uri]).targets(None)[0].to_string(),
            format!("{}/My Docs", dir.to_string_lossy()),
            "the escape is decoded before the path is resolved"
        );
        assert_eq!(
            cli(&["zenman", "https://example.com/x", "/tmp"])
                .targets(None)
                .len(),
            1,
            "a web address is not a folder, and the real target still opens"
        );
        // Named, explicit removals: `remove_dir_all` is banned workspace-wide (product deletion goes
        // through `ops::delete`, trash-first and logged), and a three-entry fixture tree does not need a
        // recursive walk to be tidied.
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(dir.join("My Docs"));
        let _ = std::fs::remove_dir(&dir);
    }
}
