//! What machine we are on, described rather than decided (plan §5.2 `platform/system.rs` row, added to
//! step 4 by §5.2's amendment).
//!
//! The sketched `Env` in the pre-step-3 plan was a data holder; step 3 rebuilt `Env` as an *action*
//! trait (`platform/mod.rs`), so the data fields it used to carry — scratch dir, locale tag, config dir —
//! live here instead.
//!
//! # This is a description, not an authority
//!
//! [`crate::platform::Env`] answers "what can this OS do"; [`AppPaths`] answers "where this run keeps its
//! files"; `SystemInfo` answers only "what does the environment look like", and it is computed from
//! environment variables, which any process can set. So:
//!
//! * nothing in the engine **discovers** a directory by calling this module — `apps/zenman::app_dirs` is
//!   the authority, resolves the real layout from the CLI overrides first, and passes the result into
//!   every service (plan §5.2 `platform/system.rs` "Done when": *nothing guesses a directory twice*);
//! * `SystemInfo` is what the Properties panel shows, what the crash report header prints, and what a
//!   test injects when it wants a locale without editing the process environment.
//!
//! [`AppPaths`]: ../../zenman/app_dirs/struct.AppPaths.html

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::platform::{Os, Platform, detect as detect_platform};

/// The application directory name the Go app used, and the bare fallback name this module uses when
/// `XDG_CONFIG_HOME` is unset (plan §5.2: "`config_dir` honours `XDG_CONFIG_HOME` **and** falls back to
/// `$HOME/.config/zen-man`").
const GO_APPLICATION_DIR: &str = "zen-man";

/// The environment values [`SystemInfo::detect`] reads, captured once.
///
/// A struct rather than live `std::env::var` calls, because the interesting part of this module is its
/// precedence rules and those must be testable — mutating the process environment from a test is `unsafe`
/// in edition 2024 and would race with every other test in the binary.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SystemEnv {
    home: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
    xdg_data_home: Option<PathBuf>,
    xdg_cache_home: Option<PathBuf>,
    lang: Option<String>,
    lc_all: Option<String>,
    user: Option<String>,
    username: Option<String>,
}

impl SystemEnv {
    /// Read the real process environment (the `detect()` path).
    #[must_use]
    pub fn capture() -> Self {
        Self {
            home: var_path("HOME"),
            xdg_config_home: var_path("XDG_CONFIG_HOME"),
            xdg_data_home: var_path("XDG_DATA_HOME"),
            xdg_cache_home: var_path("XDG_CACHE_HOME"),
            lang: var_string("LANG"),
            lc_all: var_string("LC_ALL"),
            user: var_string("USER"),
            username: var_string("USERNAME"),
        }
    }

    /// A synthetic environment: the test seam, and the shape a remote/diagnostic report decodes to.
    #[must_use]
    pub fn synthetic(home: Option<PathBuf>) -> Self {
        Self {
            home,
            ..Default::default()
        }
    }

    /// With `XDG_CONFIG_HOME` set (the deliberate deviation from Go, tested in `tests::…`).
    #[must_use]
    pub fn with_xdg_config(mut self, dir: impl Into<PathBuf>) -> Self {
        self.xdg_config_home = Some(dir.into());
        self
    }

    /// With `LANG` set.
    #[must_use]
    pub fn with_lang(mut self, lang: impl Into<String>) -> Self {
        self.lang = Some(lang.into());
        self
    }
}

/// The host, as observed. Cloneable and serialisable because it is a *snapshot*: it is taken at boot,
/// put in the crash report, and never re-read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SystemInfo {
    /// Which OS family, from [`Platform::detect`] — the same value `/api/props` published.
    pub os: Os,
    /// `$HOME`, when the environment names one. `None` is legal (a container without `HOME`), which is
    /// exactly why the fields below are not simply derived on demand.
    pub home: Option<PathBuf>,
    /// The working directory at boot, or the empty path if the question failed (a deleted cwd).
    pub cwd: PathBuf,
    /// Settings directory. `XDG_CONFIG_HOME/zen-man` when set, else `$HOME/.config/zen-man`.
    ///
    /// The Go app hard-coded the latter and ignored XDG — a real bug the plan calls out (§5.1
    /// `app_dirs` row: "with `XDG_CONFIG_HOME` set, config lands there"), so the fallback keeps Go's
    /// spelling and the override is the fix.
    pub config_dir: PathBuf,
    /// Data directory: `XDG_DATA_HOME/zen-man`, else `$HOME/.local/share/zen-man`, else the config
    /// directory's sibling.
    pub data_dir: PathBuf,
    /// Cache directory: `XDG_CACHE_HOME/zen-man`, else `$HOME/.cache/zen-man`.
    pub cache_dir: PathBuf,
    /// Best available user name: `$USER`, then `$USERNAME` (Windows), then `"unknown"`. Never an error
    /// path — a missing name is a cosmetic gap in a report, not a failure.
    pub username: String,
    /// BCP-47-ish language tag for date/number formatting, e.g. `en-US`. See [`locale_tag`].
    pub locale_tag: String,
}

impl SystemInfo {
    /// Snapshot the current process environment.
    #[must_use]
    pub fn detect() -> Self {
        Self::from_env(&SystemEnv::capture(), cwd(), detect_platform())
    }

    /// The pure half of [`detect`]: everything is a parameter, so the precedence rules are testable.
    #[must_use]
    pub fn from_env(env: &SystemEnv, cwd: PathBuf, platform: Platform) -> Self {
        let home = env.home.clone();
        let config_dir = env
            .xdg_config_home
            .as_deref()
            .and_then(absolute)
            .map(|base| base.join(GO_APPLICATION_DIR))
            .or_else(|| {
                home.as_ref()
                    .map(|h| h.join(".config").join(GO_APPLICATION_DIR))
            })
            // No `HOME` and no XDG: name something that will fail loudly rather than a path in the cwd.
            .unwrap_or_else(|| PathBuf::from(GO_APPLICATION_DIR));
        let data_dir = env
            .xdg_data_home
            .as_deref()
            .and_then(absolute)
            .map(|base| base.join(GO_APPLICATION_DIR))
            .or_else(|| {
                home.as_ref()
                    .map(|h| h.join(".local").join("share").join(GO_APPLICATION_DIR))
            })
            .unwrap_or_else(|| config_dir.clone());
        let cache_dir = env
            .xdg_cache_home
            .as_deref()
            .and_then(absolute)
            .map(|base| base.join(GO_APPLICATION_DIR))
            .or_else(|| {
                home.as_ref()
                    .map(|h| h.join(".cache").join(GO_APPLICATION_DIR))
            })
            .unwrap_or_else(|| data_dir.clone());
        Self {
            os: platform.os,
            home,
            cwd,
            config_dir,
            data_dir,
            cache_dir,
            username: env
                .user
                .clone()
                .or_else(|| env.username.clone())
                .unwrap_or_else(|| "unknown".to_string()),
            locale_tag: locale_tag(env.lc_all.as_deref(), env.lang.as_deref()),
        }
    }
}

/// The tag the UI formats dates and sizes with.
///
/// `LC_ALL` beats `LANG` (the C library's own precedence), and both are normalised: the codeset and
/// modifier suffixes are dropped and `_` becomes `-`, so `en_US.UTF-8` → `en-US` and
/// `zh_Hans_CN.utf8@pinyin` → `zh-Hans-CN`. `C`, `POSIX` and an unset locale have no territory and no
/// date convention to inherit, so they answer the language-only tag `"en"` — the port's fallback, which
/// is also what `craft_i18n::Lang::EN` means: show the source text.
#[must_use]
pub fn locale_tag(lc_all: Option<&str>, lang: Option<&str>) -> String {
    for raw in [lc_all, lang] {
        let Some(raw) = raw.map(str::trim).filter(|v| !v.is_empty()) else {
            continue;
        };
        if raw == "C" || raw == "POSIX" {
            return "en".to_string();
        }
        let base = raw
            .split(['.', '@'])
            .next()
            .unwrap_or(raw)
            .replace('_', "-");
        if !base.is_empty() {
            return base;
        }
    }
    "en".to_string()
}

/// An XDG value counts only when it is absolute (a relative `XDG_CONFIG_HOME` is meaningless, and
/// resolving it against the cwd would put settings somewhere the user never pointed).
fn absolute(path: &Path) -> Option<PathBuf> {
    path.is_absolute().then(|| path.to_path_buf())
}

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_default()
}

fn var_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn var_string(name: &str) -> Option<String> {
    // A value that is not UTF-8 is treated as unset: every consumer here wants a `String`, and a
    // lossy user name in a report is worse than "unknown".
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

// (`SystemEnv::capture` converts every `OsString` it reads to `PathBuf`/`String` immediately, so nothing
// here stores one.)

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with(home: Option<&str>) -> SystemEnv {
        SystemEnv::synthetic(home.map(PathBuf::from))
    }

    fn platform() -> Platform {
        detect_platform()
    }

    #[test]
    fn config_dir_honours_xdg_config_home() {
        let env = env_with(Some("/home/u")).with_xdg_config("/xdg/config");
        let info = SystemInfo::from_env(&env, PathBuf::from("/tmp"), platform());
        assert_eq!(info.config_dir, PathBuf::from("/xdg/config/zen-man"));
        // The deliberate change from Go, stated as an assertion so it cannot regress silently.
        assert!(
            !info.config_dir.starts_with("/home/u"),
            "XDG_CONFIG_HOME must win over $HOME/.config"
        );
    }

    #[test]
    fn config_dir_falls_back_to_the_go_path_without_xdg() {
        let env = env_with(Some("/home/u"));
        let info = SystemInfo::from_env(&env, PathBuf::from("/tmp"), platform());
        assert_eq!(info.config_dir, PathBuf::from("/home/u/.config/zen-man"));
        assert_eq!(info.data_dir, PathBuf::from("/home/u/.local/share/zen-man"));
        assert_eq!(info.cache_dir, PathBuf::from("/home/u/.cache/zen-man"));
    }

    #[test]
    fn a_relative_xdg_value_is_ignored_not_resolved_against_the_cwd() {
        let env = env_with(Some("/home/u")).with_xdg_config("relative/config");
        let info = SystemInfo::from_env(&env, PathBuf::from("/tmp"), platform());
        assert_eq!(info.config_dir, PathBuf::from("/home/u/.config/zen-man"));
    }

    #[test]
    fn no_home_at_all_still_names_something() {
        let env = env_with(None);
        let info = SystemInfo::from_env(&env, PathBuf::from("/tmp"), platform());
        assert_eq!(info.home, None);
        assert_eq!(info.config_dir, PathBuf::from(GO_APPLICATION_DIR));
        // …and it is not a path inside the cwd, which is the failure mode that matters.
        assert!(!info.config_dir.starts_with("/tmp"));
    }

    #[test]
    fn the_locale_tag_is_normalised_and_lc_all_wins() {
        assert_eq!(locale_tag(None, Some("en_US.UTF-8")), "en-US");
        assert_eq!(locale_tag(None, Some("en_US.utf8")), "en-US");
        assert_eq!(locale_tag(None, Some("de_DE")), "de-DE");
        assert_eq!(
            locale_tag(Some("fr_FR.UTF-8"), Some("en_US.UTF-8")),
            "fr-FR"
        );
        assert_eq!(locale_tag(Some("  "), Some("ja_JP.eucJP")), "ja-JP");
        assert_eq!(
            locale_tag(Some("zh_Hans_CN.utf8@pinyin"), None),
            "zh-Hans-CN"
        );
        assert_eq!(locale_tag(Some("C"), Some("en_US.UTF-8")), "en");
        assert_eq!(locale_tag(None, Some("POSIX")), "en");
        assert_eq!(locale_tag(None, None), "en");
    }

    #[test]
    fn detect_agrees_with_the_process_environment_it_just_read() {
        let info = SystemInfo::detect();
        // Whatever this host is, the invariants hold: absolute paths where a home exists, a sane tag.
        assert!(!info.locale_tag.is_empty() && !info.locale_tag.contains('_'));
        assert!(!info.username.trim().is_empty());
        if info.home.is_some() {
            assert!(info.config_dir.is_absolute());
            assert!(info.data_dir.is_absolute());
            assert!(info.cache_dir.is_absolute());
        }
        assert_eq!(info.os, platform().os);
    }

    #[test]
    fn the_username_falls_back_to_username_then_unknown() {
        let mut env = env_with(Some("/home/u"));
        env.username = Some("pw".into());
        assert_eq!(
            SystemInfo::from_env(&env, PathBuf::from("/"), platform()).username,
            "pw",
            "$USERNAME is the Windows spelling and is the fallback"
        );
        env.user = Some("posix".into());
        assert_eq!(
            SystemInfo::from_env(&env, PathBuf::from("/"), platform()).username,
            "posix",
            "$USER wins when both are set"
        );
        let bare = SystemInfo::from_env(&env_with(Some("/h")), PathBuf::from("/"), platform());
        assert_eq!(bare.username, "unknown");
    }
}
