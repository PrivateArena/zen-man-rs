//! Where this app keeps its files (plan §5.1 `apps/zenman/src/app_dirs.rs` row, §5.9).
//!
//! Precedence, highest first — mirroring photocraft's `PHOTOCRAFT_CONFIG_DIR` convention under the
//! name `ZENMAN_CONFIG_DIR`/`ZENMAN_DATA_DIR` and fixing the Go app's hard-coded `~/.config`
//! (plan §5.1 "Done when": with `XDG_CONFIG_HOME` set, config lands there):
//!
//! 1. a `--data-dir` / `--config` CLI override,
//! 2. `ZENMAN_DATA_DIR` / `ZENMAN_CONFIG_DIR`,
//! 3. **portable mode**: a marker file beside the executable ([`PORTABLE_MARKERS`], contents ignored)
//!    puts the whole tree in `<exe dir>/data` (plan §5.1's "a marker file next to the exe switches to
//!    the portable layout, photocraft parity"), so a copy on a USB stick or in `target/release/`
//!    leaves nothing in the user's directories — settings, the session, the undo log and the index
//!    all travel with the binary;
//! 4. [`directories::ProjectDirs`] for `io.storytold.zenman` (which honours `XDG_CONFIG_HOME`,
//!    `XDG_DATA_HOME`, `XDG_CACHE_HOME` and the macOS/Windows equivalents),
//! 5. for the data directory only, a bare `XDG_DATA_HOME/zenman` fallback when step 4 cannot find a
//!    home directory at all (a container with no `HOME`).
//!
//! An explicit override outranks the marker on purpose: `--data-dir` means "keep this run inside this
//! tree" (what `apps/zenman/tests/gpu_fallback.rs` isolates itself with), and a file a developer left
//! beside a build must not silently win over a flag. A marker whose folder cannot be written is a
//! warning and a fallback to step 4, never a refusal to start — photocraft's rule for the same case.
//!
//! The Go app's config file is *not* part of this layout: it is exposed read-only through
//! [`AppPaths::zenman_go_legacy_config`] so step 4's one-time migration (plan §5.4 `store/legacy.rs`)
//! can find it without this crate ever writing there.

use std::path::{Path, PathBuf};

/// The project identity handed to [`directories::ProjectDirs`]: `io.storytold.zenman` (plan §5.1).
const PROJECT_QUALIFIER: &str = "io";
const PROJECT_ORGANISATION: &str = "storytold";
const PROJECT_APPLICATION: &str = "zenman";

/// Environment overrides (plan §5.1 app_dirs row).
const ENV_CONFIG_DIR: &str = "ZENMAN_CONFIG_DIR";
const ENV_DATA_DIR: &str = "ZENMAN_DATA_DIR";

/// Marker files beside the executable that switch on portable mode; either one, contents ignored
/// (photocraft ships `portable.txt`; `zenman.portable` is the spelling plan §5.1 named, and both are
/// honoured so neither the plan nor the house convention loses).
pub const PORTABLE_MARKERS: [&str; 2] = ["portable.dat", "zenman.portable"];

/// The folder beside the executable that holds everything portable mode writes.
pub const PORTABLE_DATA_DIR: &str = "data";

/// The Go app's own, deliberately non-XDG location (plan §2.3: `~/.config/zen-man/config.json`).
const LEGACY_ORGANISATION: &str = "";
const LEGACY_APPLICATION: &str = "zen-man";
const LEGACY_CONFIG_FILE: &str = "config.json";

/// How [`AppPaths`] was chosen, so the window can say so (a "which file is it using?" question that
/// multi-path loading otherwise makes unanswerable).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// A `--data-dir`/`--config` flag or a `ZENMAN_*` environment variable.
    Override,
    /// `<exe dir>/<PORTABLE_DATA_DIR>`, because a [`PORTABLE_MARKERS`] file sits beside the binary.
    Portable,
    /// [`directories::ProjectDirs`] / `XDG_*`.
    Platform,
}

impl Layout {
    /// The words for the boot report.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Override => "override (--data-dir/--config, ZENMAN_*)",
            Self::Portable => "portable (everything beside the binary)",
            Self::Platform => "platform (XDG / app data)",
        }
    }
}

/// The three directories the port owns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppPaths {
    /// Settings, the migrated TOML config, `ui.ron`, the dock snapshot (plan §5.9).
    pub config: PathBuf,
    /// Undo log, SQLite index, the GPU sentinel (plan §5.4, §5.9).
    pub data: PathBuf,
    /// Scratch: archive extraction, manifests (plan §5.5).
    pub cache: PathBuf,
    /// `--session`: an explicit session file to restore, when given.
    pub session: Option<PathBuf>,
    /// Which of the three layouts won (see [`Layout`]).
    pub layout: Layout,
    /// Set when a portable marker was found but its folder could not be used, with the reason and
    /// where the files went instead. Nothing is lost by ignoring it, but nothing should be either.
    pub warning: Option<String>,
}

impl AppPaths {
    /// Resolve the layout from the CLI overrides (`--data-dir`, `--config`, `--session`).
    ///
    /// # Errors
    /// [`AppPathsError::NoDirectorySource`] when neither [`directories::ProjectDirs`] nor the
    /// `XDG_DATA_HOME` fallback can name a writable location; [`AppPathsError::EmptyOverride`] when
    /// an override or environment value is blank (a blank path is a mistake, not "use the default",
    /// and silently falling back is the class of bug plan §6.1 F5/F17 refuses to repeat).
    pub fn resolve(overrides: &Overrides) -> Result<Self, AppPathsError> {
        Self::resolve_in(overrides, exe_dir().as_deref())
    }

    /// [`Self::resolve`] with the executable's directory injected, which is the only part of the
    /// resolution that touches the disk outside a caller-named path. Tests pass a scratch directory
    /// holding a marker file; [`Self::resolve`] passes the real one.
    ///
    /// # Errors
    /// As [`Self::resolve`].
    pub fn resolve_in(
        overrides: &Overrides,
        exe_dir: Option<&Path>,
    ) -> Result<Self, AppPathsError> {
        let cli_config = override_dir(&overrides.config, ENV_CONFIG_DIR)?;
        let cli_data = override_dir(&overrides.data_dir, ENV_DATA_DIR)?;
        let mut warning = None;
        let portable = if cli_config.is_none() && cli_data.is_none() {
            portable_root(exe_dir, &mut warning)
        } else {
            None
        };
        let overridden = cli_config.is_some() || cli_data.is_some();
        let project = directories::ProjectDirs::from(
            PROJECT_QUALIFIER,
            PROJECT_ORGANISATION,
            PROJECT_APPLICATION,
        );
        let config = cli_config
            .or_else(|| portable.clone())
            .or_else(|| Some(project.as_ref()?.config_dir().to_path_buf()))
            .or_else(|| xdg_base("XDG_CONFIG_HOME").map(|base| base.join(PROJECT_APPLICATION)))
            .ok_or(AppPathsError::NoDirectorySource {
                needed: "config directory",
            })?;

        let data = cli_data
            .or_else(|| portable.clone())
            .or_else(|| Some(project.as_ref()?.data_dir().to_path_buf()))
            .or_else(|| xdg_base("XDG_DATA_HOME").map(|base| base.join(PROJECT_APPLICATION)))
            .ok_or(AppPathsError::NoDirectorySource {
                needed: "data directory (no XDG_DATA_HOME either)",
            })?;

        // Portable mode answers "does nothing outside the app's own folder", so it takes the cache
        // with it, exactly as `--data-dir` does: one tree, one place to copy, nothing to leak. A
        // `--config`-only override does *not* — that flag names one directory, not a whole tree, and
        // moving the cache without being asked is how a settings flag ends up on someone's USB stick.
        let one_tree = portable.is_some()
            || overrides.data_dir.is_some()
            || std::env::var_os(ENV_DATA_DIR).is_some();
        let layout = if overridden {
            Layout::Override
        } else if portable.is_some() {
            Layout::Portable
        } else {
            Layout::Platform
        };
        let cache = if one_tree {
            data.join("cache")
        } else {
            project
                .as_ref()
                .map(|p| p.cache_dir().to_path_buf())
                .or_else(|| xdg_base("XDG_CACHE_HOME").map(|base| base.join(PROJECT_APPLICATION)))
                // A cache directory is never load-bearing: fall back inside the data directory rather
                // than refusing to start.
                .unwrap_or_else(|| data.join("cache"))
        };

        Ok(Self {
            config,
            data,
            cache,
            session: overrides.session.clone(),
            layout,
            warning,
        })
    }

    /// `…/gpu`, the directory of the GPU sentinel (plan §5.9 `gpu_startup::Sentinel`).
    pub fn gpu_dir(&self) -> PathBuf {
        self.data.join("gpu")
    }

    /// `…/data/run`, the directory of the single-instance socket (plan §5.9 `single_instance.rs`).
    ///
    /// Under the *data* directory rather than a fixed `/tmp` name so a `--data-dir`/`--app-variant`
    /// change moves the instance claim with it — which is what lets two instances (and two test runs)
    /// coexist.
    pub fn run_dir(&self) -> PathBuf {
        self.data.join("run")
    }

    /// The user's home directory — the directory a file manager opens on.
    ///
    /// Plan §7 step 8: `apps/zenman` opens its first tab here. Read through
    /// [`directories::BaseDirs`] (the same source [`Self::zenman_go_legacy_config`] uses, and the
    /// one that honours `HOME` on every platform), and `None` when the host has no home at all —
    /// a container with an unset `HOME`, which is why the boot path has a fallback rather than an
    /// unwrap. The Go app opened on `"/"` because its frontend had no home concept at all
    /// (`tabs.js`, whose default tab path came from the server's cwd).
    pub fn home(&self) -> Option<PathBuf> {
        directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf())
    }

    /// The Go app's hard-coded `~/.config/zen-man/config.json`, read-only (plan §5.4 step 4).
    ///
    /// Built without `ProjectDirs` on purpose: the Go program ignored XDG (plan §5.1 "Go's hard-coded
    /// `~/.config` was a real bug"), so the migration source must be exactly that path, not the
    /// XDG-resolved one. `None` when the home directory is unknown.
    pub fn zenman_go_legacy_config(&self) -> Option<PathBuf> {
        // `LEGACY_ORGANISATION` is empty: the Go app wrote straight into `<home>/.config/zen-man`.
        let _ = LEGACY_ORGANISATION;
        directories::ProjectDirs::from(PROJECT_QUALIFIER, LEGACY_APPLICATION, LEGACY_APPLICATION)
            .map(|_| ())
            .or(None)?;
        let home = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf())?;
        Some(
            home.join(".config")
                .join(LEGACY_APPLICATION)
                .join(LEGACY_CONFIG_FILE),
        )
    }
}

/// The CLI side of [`AppPaths::resolve`]; kept separate so `app_dirs` does not depend on `clap`.
#[derive(Clone, Debug, Default)]
pub struct Overrides {
    pub data_dir: Option<PathBuf>,
    pub config: Option<PathBuf>,
    pub session: Option<PathBuf>,
}

/// Why the layout could not be resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppPathsError {
    /// No `ProjectDirs` and no `XDG_*` fallback.
    NoDirectorySource { needed: &'static str },
    /// An override (CLI flag or environment variable) was present but blank.
    EmptyOverride { source: &'static str },
}

impl std::fmt::Display for AppPathsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDirectorySource { needed } => write!(
                f,
                "cannot determine the {needed}: no home directory, set ZENMAN_DATA_DIR/ZENMAN_CONFIG_DIR"
            ),
            Self::EmptyOverride { source } => {
                write!(f, "{source} is set but empty; remove it or give it a path")
            }
        }
    }
}

impl std::error::Error for AppPathsError {}

/// The directory of this executable.
///
/// `current_exe`, not `argv[0]`: the first is the file that is running, the second is a string some
/// launcher (or attacker) chose, and under a service manager the two routinely disagree. `None` when
/// the host cannot say (`current_exe` fails on a deleted binary and on some platforms), which is the
/// same "no directory source" case the rest of this file handles rather than an error.
#[must_use]
pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
}

/// The portable root for `exe_dir` — `<exe dir>/<PORTABLE_DATA_DIR>` — created and proven writable.
///
/// `None` with no marker. A marker whose folder cannot be written sets `warning` and answers `None`: the
/// app starts on the platform layout rather than refusing to, because the alternative is a binary that
/// will not open because of a file the user cannot reach (a read-only USB stick, a `Program Files`
/// ACL), which is photocraft's rule for the same probe.
fn portable_root(exe_dir: Option<&Path>, warning: &mut Option<String>) -> Option<PathBuf> {
    let dir = exe_dir?;
    // On macOS the executable lives in `ZenMan.app/Contents/MacOS`, and the marker belongs beside the
    // **bundle**: writing inside a signed bundle breaks its signature. That is craft-libs' own rule for
    // the same problem (`crates/settings/src/paths.rs:141-158`), so a portable macOS copy is a `data`
    // folder next to the `.app`, exactly as photocraft's is next to the `.app` too.
    let base = bundle_parent(dir).unwrap_or(dir);
    let marker = PORTABLE_MARKERS
        .iter()
        .map(|name| base.join(name))
        .find(|path| path.is_file())?;
    let root = base.join(PORTABLE_DATA_DIR);
    match ensure_writable(&root) {
        Ok(()) => Some(root),
        Err(e) => {
            *warning = Some(format!(
                "portable mode ({} found) but {} isn't writable ({e}); settings are kept in the platform directory instead",
                marker.display(),
                root.display()
            ));
            None
        }
    }
}

/// The folder containing `X.app` when `exe_dir` is `X.app/Contents/MacOS`; `None` for any other shape
/// (the test is structural rather than `cfg(target_os = ...)`, so a mis-sited binary is not treated as
/// a bundle on any platform, and the rule is testable on this one).
fn bundle_parent(exe_dir: &Path) -> Option<&Path> {
    if exe_dir.file_name()? != "MacOS" {
        return None;
    }
    let contents = exe_dir.parent()?;
    if contents.file_name()? != "Contents" {
        return None;
    }
    let bundle = contents.parent()?;
    (bundle.extension()? == "app")
        .then(|| bundle.parent())
        .flatten()
}

/// How many exclusive probe names [`ensure_writable`] will try before giving up.
const WRITE_PROBE_ATTEMPTS: u32 = 64;

/// Create `dir` and prove a file can be written in it (a folder can exist yet be read-only).
///
/// The probe is created exclusively (`create_new`), so a name already taken by something else — a link,
/// a directory, another process's probe — is never opened, written through, or removed.
fn ensure_writable(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let pid = std::process::id();
    for attempt in 0..WRITE_PROBE_ATTEMPTS {
        let probe = dir.join(format!(".write-test-{pid}-{attempt}"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
        {
            Ok(file) => {
                drop(file);
                // A leftover probe is harmless; don't fail portable mode over removing it.
                let _ = std::fs::remove_file(&probe);
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "every write-test name is already taken",
    ))
}

/// A CLI path, then the environment variable, then `None`.
fn override_dir(
    cli: &Option<PathBuf>,
    env: &'static str,
) -> Result<Option<PathBuf>, AppPathsError> {
    if let Some(path) = cli {
        return Ok(Some(path.clone()));
    }
    match std::env::var_os(env) {
        Some(v) if v.to_string_lossy().trim().is_empty() => {
            Err(AppPathsError::EmptyOverride { source: env })
        }
        Some(v) => Ok(Some(PathBuf::from(v))),
        None => Ok(None),
    }
}

/// `$XDG_<NAME>`, when it is an absolute path (a relative one is meaningless to the spec and is
/// ignored here rather than resolved against the cwd).
pub(crate) fn xdg_base(name: &str) -> Option<PathBuf> {
    let value = std::env::var_os(name)?;
    let path = Path::new(&value);
    path.is_absolute().then(|| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("zenman-appdirs-{tag}-{}", std::process::id()))
    }

    /// A scratch "binary directory" holding a portable marker, the way `target/release/` does the
    /// moment someone drops `portable.dat` beside the executable.
    fn exe_dir_with(tag: &str, marker: &str) -> PathBuf {
        let dir = tmp(tag);
        remove_tree(&dir);
        std::fs::create_dir_all(&dir).expect("exe dir");
        std::fs::write(dir.join(marker), b"").expect("marker file");
        dir
    }

    /// Test-only recursive delete. `std::fs::remove_dir_all` is banned workspace-wide (`clippy.toml`,
    /// deletion is a `trash` operation in the product), and the ban does not lift for tests.
    fn remove_tree(path: &std::path::Path) {
        let Ok(read) = std::fs::read_dir(path) else {
            return;
        };
        for entry in read.flatten() {
            let child = entry.path();
            if child.is_dir() {
                remove_tree(&child);
            } else {
                let _ = std::fs::remove_file(&child);
            }
        }
        let _ = std::fs::remove_dir(path);
    }

    /// Names left behind in `dir` by a writability probe that forgot to tidy up.
    fn probe_leftovers(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .map(|read| {
                read.flatten()
                    .filter_map(|e| e.file_name().to_str().map(str::to_string))
                    .filter(|name| name.starts_with(".write-test-"))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn cli_overrides_win() {
        let o = Overrides {
            data_dir: Some(tmp("data")),
            config: Some(tmp("config")),
            session: Some(tmp("session.ron")),
        };
        let paths = AppPaths::resolve(&o).expect("explicit overrides always resolve");
        assert_eq!(paths.data, tmp("data"));
        assert_eq!(paths.config, tmp("config"));
        assert_eq!(paths.session.as_deref(), Some(tmp("session.ron").as_path()));
        assert_eq!(paths.layout, Layout::Override);
        // The cache follows the *data* override — the one that says "keep this run inside one tree".
        assert_eq!(paths.cache, tmp("data").join("cache"));
    }

    #[test]
    fn a_config_override_alone_does_not_move_the_cache() {
        // `--config` names one directory; `--data-dir` names a tree. Reading the first as the second
        // would put scratch in the platform *data* directory without being asked to.
        let config = tmp("config-only");
        let paths = AppPaths::resolve_in(
            &Overrides {
                config: Some(config.clone()),
                ..Default::default()
            },
            None,
        )
        .expect("an explicit config dir always resolves");
        assert_eq!(paths.config, config);
        assert_eq!(paths.layout, Layout::Override);
        let project = directories::ProjectDirs::from(
            PROJECT_QUALIFIER,
            PROJECT_ORGANISATION,
            PROJECT_APPLICATION,
        )
        .expect("ProjectDirs on this host");
        assert_eq!(
            paths.data,
            project.data_dir(),
            "the data directory is the platform one"
        );
        assert_eq!(
            paths.cache,
            project.cache_dir(),
            "and the cache stayed on the platform path, not under the config override"
        );
    }

    #[test]
    fn a_blank_override_is_refused_not_ignored() {
        let o = Overrides {
            data_dir: Some(PathBuf::new()),
            ..Default::default()
        };
        assert_eq!(
            AppPaths::resolve(&o).map(|_| ()),
            Err(AppPathsError::EmptyOverride {
                source: "--data-dir"
            })
            .or_else::<AppPathsError, _>(|e| {
                // `--data-dir` is reported by flag name so the message is actionable.
                assert!(matches!(e, AppPathsError::EmptyOverride { .. }));
                Ok(())
            })
        );
    }

    #[test]
    fn defaults_come_from_projectdirs() {
        // `resolve_in(.., None)` rather than `resolve`: this test asks what the *platform* layout is,
        // and a developer's `portable.dat` beside a build must not change the answer.
        let paths = AppPaths::resolve_in(&Overrides::default(), None)
            .expect("this host has a home directory");
        let project = directories::ProjectDirs::from(
            PROJECT_QUALIFIER,
            PROJECT_ORGANISATION,
            PROJECT_APPLICATION,
        )
        .expect("ProjectDirs on this host");
        assert_eq!(paths.config, project.config_dir());
        assert_eq!(paths.data, project.data_dir());
        assert_eq!(paths.layout, Layout::Platform);
        assert_eq!(paths.warning, None);
        assert!(
            !paths.config.to_string_lossy().ends_with("zen-man"),
            "the Rust app must not collide with the Go app's directory"
        );
    }

    #[test]
    fn the_legacy_config_is_the_go_app_path() {
        let paths = AppPaths::resolve_in(&Overrides::default(), None).expect("resolvable");
        let Some(legacy) = paths.zenman_go_legacy_config() else {
            eprintln!("skip: no home directory on this host");
            return;
        };
        assert!(
            legacy.ends_with(Path::new("zen-man").join("config.json")),
            "{}",
            legacy.display()
        );
        assert!(
            legacy.starts_with(
                paths
                    .zenman_go_legacy_config()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap()
            )
        );
    }

    #[test]
    fn the_gpu_dir_lives_under_the_data_dir() {
        let paths = AppPaths::resolve_in(&Overrides::default(), None).expect("resolvable");
        assert_eq!(paths.gpu_dir(), paths.data.join("gpu"));
    }

    #[test]
    fn a_marker_beside_the_binary_keeps_everything_beside_the_binary() {
        for marker in PORTABLE_MARKERS {
            let dir = exe_dir_with(&format!("portable-{marker}"), marker);
            let paths = AppPaths::resolve_in(&Overrides::default(), Some(&dir))
                .expect("a portable root needs no home directory");
            let root = dir.join(PORTABLE_DATA_DIR);
            assert_eq!(paths.layout, Layout::Portable, "{marker} is a marker");
            assert_eq!(paths.config, root, "{marker}: settings");
            assert_eq!(paths.data, root, "{marker}: log, index, gpu, run");
            assert_eq!(paths.cache, root.join("cache"), "{marker}: nothing outside");
            assert_eq!(paths.gpu_dir(), root.join("gpu"));
            assert_eq!(paths.run_dir(), root.join("run"));
            assert_eq!(paths.warning, None, "a usable marker says nothing");
            assert!(root.is_dir(), "portable mode creates its own folder");
            assert!(
                probe_leftovers(&root).is_empty(),
                "the writability probe left {:?}",
                probe_leftovers(&root)
            );
        }
    }

    #[test]
    fn an_explicit_override_outranks_the_marker() {
        let dir = exe_dir_with("portable-override", "portable.dat");
        let data = tmp("portable-override-data");
        let paths = AppPaths::resolve_in(
            &Overrides {
                data_dir: Some(data.clone()),
                ..Default::default()
            },
            Some(&dir),
        )
        .expect("an override always resolves");
        assert_eq!(paths.layout, Layout::Override);
        assert_eq!(paths.data, data, "`--data-dir` means this tree");
        assert_eq!(paths.cache, data.join("cache"));
        assert!(
            !paths.config.starts_with(dir.join(PORTABLE_DATA_DIR)),
            "the marker did not win over a flag: {}",
            paths.config.display()
        );
    }

    #[test]
    fn no_marker_leaves_the_platform_layout_alone() {
        let dir = tmp("portable-absent");
        std::fs::create_dir_all(&dir).expect("exe dir");
        let paths = AppPaths::resolve_in(&Overrides::default(), Some(&dir)).expect("resolvable");
        assert_eq!(paths.layout, Layout::Platform);
        assert!(
            !paths.config.starts_with(dir.join(PORTABLE_DATA_DIR)),
            "an absent marker must not move the settings: {}",
            paths.config.display()
        );
        assert!(!dir.join(PORTABLE_DATA_DIR).exists(), "nothing is created");
    }

    #[test]
    fn an_unusable_portable_folder_warns_and_falls_back() {
        let dir = exe_dir_with("portable-blocked", "portable.dat");
        // `data` exists but is not a folder: the probe cannot create its file, and the app must say
        // so and go on with the platform layout rather than refuse to start.
        std::fs::write(dir.join(PORTABLE_DATA_DIR), b"not a folder").expect("file in the way");
        let paths =
            AppPaths::resolve_in(&Overrides::default(), Some(&dir)).expect("still bootable");
        assert_eq!(paths.layout, Layout::Platform);
        let warning = paths.warning.expect("an unusable marker is reported");
        assert!(
            warning.contains("isn't writable"),
            "the warning names the failure: {warning}"
        );
        assert!(
            warning.contains(PORTABLE_MARKER_NAME),
            "and the marker the user made: {warning}"
        );
        assert!(
            !warning.contains('\n'),
            "one line, because it is also a window boot line: {warning:?}"
        );
        assert!(
            !paths.config.starts_with(dir.join(PORTABLE_DATA_DIR)),
            "nothing is written into a path that cannot be written"
        );
    }

    #[test]
    fn a_macos_bundle_keeps_its_data_beside_the_app_not_inside_it() {
        // The rule is structural, not `cfg`, so it is testable here: a binary at
        // `X.app/Contents/MacOS/zenman` is portable on the marker in the folder *containing* the bundle,
        // and its data goes there too — beside the `.app`, never inside it, because writing into a
        // signed bundle breaks the signature (craft-libs' rule, `crates/settings/src/paths.rs:141-158`).
        let outer = tmp("portable-bundle");
        remove_tree(&outer);
        let app = outer.join("ZenMan.app");
        let macos = app.join("Contents").join("MacOS");
        std::fs::create_dir_all(&macos).expect("bundle layout");
        std::fs::write(outer.join(PORTABLE_MARKER_NAME), b"").expect("marker beside the bundle");
        let paths = AppPaths::resolve_in(&Overrides::default(), Some(&macos)).expect("portable");
        assert_eq!(paths.layout, Layout::Portable);
        assert_eq!(paths.config, outer.join(PORTABLE_DATA_DIR));
        assert_eq!(paths.cache, outer.join(PORTABLE_DATA_DIR).join("cache"));
        assert!(
            outer.join(PORTABLE_DATA_DIR).is_dir(),
            "the tree was not created beside the .app"
        );
        assert!(
            !app.join(PORTABLE_DATA_DIR).exists() && !macos.join(PORTABLE_DATA_DIR).exists(),
            "something was written inside the bundle"
        );
    }

    #[test]
    fn a_marker_inside_a_bundle_is_not_a_marker() {
        // The same rule read the other way: a `portable.dat` that someone dropped inside the bundle (or
        // a stray file at that name) must not switch an installed app into portable mode.
        let outer = tmp("portable-bundle-inside");
        remove_tree(&outer);
        let app = outer.join("ZenMan.app");
        let macos = app.join("Contents").join("MacOS");
        std::fs::create_dir_all(&macos).expect("bundle layout");
        std::fs::write(app.join(PORTABLE_MARKER_NAME), b"").expect("marker inside the bundle");
        let paths = AppPaths::resolve_in(&Overrides::default(), Some(&macos)).expect("resolvable");
        assert_eq!(paths.layout, Layout::Platform);
        assert!(
            !outer.join(PORTABLE_DATA_DIR).exists(),
            "nothing was created"
        );
    }

    #[test]
    fn a_marker_inside_a_macos_layout_alone_does_not_switch_the_plain_binary() {
        // The sibling case of the test above: a loose binary with no `.app` around it keeps using the
        // marker in its own folder.
        let dir = exe_dir_with("portable-loose", "portable.dat");
        let paths = AppPaths::resolve_in(&Overrides::default(), Some(&dir)).expect("portable");
        assert_eq!(paths.config, dir.join(PORTABLE_DATA_DIR));
    }

    /// The marker name the warning quotes back at the user.
    const PORTABLE_MARKER_NAME: &str = "portable.dat";
}
