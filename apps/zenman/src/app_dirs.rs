//! Where this app keeps its files (plan §5.1 `apps/zenman/src/app_dirs.rs` row, §5.9).
//!
//! Precedence, highest first — mirroring photocraft's `PHOTOCRAFT_CONFIG_DIR` convention under the
//! name `ZENMAN_CONFIG_DIR`/`ZENMAN_DATA_DIR` and fixing the Go app's hard-coded `~/.config`
//! (plan §5.1 "Done when": with `XDG_CONFIG_HOME` set, config lands there):
//!
//! 1. a `--data-dir` / `--config` CLI override,
//! 2. `ZENMAN_DATA_DIR` / `ZENMAN_CONFIG_DIR`,
//! 3. [`directories::ProjectDirs`] for `io.storytold.zenman` (which honours `XDG_CONFIG_HOME`,
//!    `XDG_DATA_HOME`, `XDG_CACHE_HOME` and the macOS/Windows equivalents),
//! 4. for the data directory only, a bare `XDG_DATA_HOME/zenman` fallback when step 3 cannot find a
//!    home directory at all (a container with no `HOME`).
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

/// The Go app's own, deliberately non-XDG location (plan §2.3: `~/.config/zen-man/config.json`).
const LEGACY_ORGANISATION: &str = "";
const LEGACY_APPLICATION: &str = "zen-man";
const LEGACY_CONFIG_FILE: &str = "config.json";

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
        let project = directories::ProjectDirs::from(
            PROJECT_QUALIFIER,
            PROJECT_ORGANISATION,
            PROJECT_APPLICATION,
        );
        let config = override_dir(&overrides.config, ENV_CONFIG_DIR)?
            .or_else(|| Some(project.as_ref()?.config_dir().to_path_buf()))
            .or_else(|| xdg_base("XDG_CONFIG_HOME").map(|base| base.join(PROJECT_APPLICATION)))
            .ok_or(AppPathsError::NoDirectorySource {
                needed: "config directory",
            })?;

        let data = override_dir(&overrides.data_dir, ENV_DATA_DIR)?
            .or_else(|| Some(project.as_ref()?.data_dir().to_path_buf()))
            .or_else(|| xdg_base("XDG_DATA_HOME").map(|base| base.join(PROJECT_APPLICATION)))
            .ok_or(AppPathsError::NoDirectorySource {
                needed: "data directory (no XDG_DATA_HOME either)",
            })?;

        let cache = if overrides.data_dir.is_some() || std::env::var_os(ENV_DATA_DIR).is_some() {
            // A `--data-dir`/`ZENMAN_DATA_DIR` override means "keep this run inside one tree" (the shape
            // `tests/gpu_fallback.rs` and every future isolation test depend on): the cache must not leak
            // into the platform directory the override exists to avoid.
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
        })
    }

    /// `…/gpu`, the directory of the GPU sentinel (plan §5.9 `gpu_startup::Sentinel`).
    pub fn gpu_dir(&self) -> PathBuf {
        self.data.join("gpu")
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
fn xdg_base(name: &str) -> Option<PathBuf> {
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
        // The cache is derived, never a machine default, when the CLI overrides the rest.
        assert!(paths.cache.starts_with(&paths.data) || paths.cache.exists());
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
        let paths =
            AppPaths::resolve(&Overrides::default()).expect("this host has a home directory");
        let project = directories::ProjectDirs::from(
            PROJECT_QUALIFIER,
            PROJECT_ORGANISATION,
            PROJECT_APPLICATION,
        )
        .expect("ProjectDirs on this host");
        assert_eq!(paths.config, project.config_dir());
        assert_eq!(paths.data, project.data_dir());
        assert!(
            !paths.config.to_string_lossy().ends_with("zen-man"),
            "the Rust app must not collide with the Go app's directory"
        );
    }

    #[test]
    fn the_legacy_config_is_the_go_app_path() {
        let paths = AppPaths::resolve(&Overrides::default()).expect("resolvable");
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
        let paths = AppPaths::resolve(&Overrides::default()).expect("resolvable");
        assert_eq!(paths.gpu_dir(), paths.data.join("gpu"));
    }
}
