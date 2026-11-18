//! Boot plumbing between the paths and the window (plan §5.9 `main.rs` row: `AppPaths` → `SystemInfo` →
//! config load/migrate). Lives here rather than in `main` so `main.rs` stays a readable order of
//! operations — the step-4 brief's explicit requirement.
//!
//! The order matters and is asserted in the returned [`BootContext`] fields' types:
//!
//! 1. `apps/zenman::app_dirs::AppPaths::resolve` has already run — it is the only directory authority
//!    (plan §5.1), and nothing below discovers a path;
//! 2. [`SystemInfo::detect`] snapshots the environment. It is a *description* (see its own module docs):
//!    it tells the Properties panel and the crash report what the host looks like, and its
//!    `config_dir`/`data_dir` values are the *unoverridden* ones, deliberately not used as our paths;
//! 3. the settings file is read, and only then is anything allowed to walk a directory — the config
//!    carries the `limits.*` overrides, so validating it first is what makes the caps real (plan §5.4).

use std::path::{Path, PathBuf};

use zenman_core::error::CoreError;
use zenman_core::platform::system::SystemInfo;
use zenman_core::store::config::{CURRENT_CONFIG_VERSION, ZenConfig};
use zenman_core::store::legacy::{self, LegacySource};

use crate::app_dirs::AppPaths;
use crate::{BootError, EXIT_CONFIG};

/// The Rust settings file's name inside the config directory.
pub const CONFIG_FILE: &str = "config.json";

/// Everything the window needs that is not the window: the host description, the validated settings,
/// and the things the user has to be told about how they were obtained.
#[derive(Debug)]
pub struct BootContext {
    /// The host, snapshotted after the paths were resolved.
    pub system: SystemInfo,
    /// The settings this run uses — the file's contents, a migration of them, or the defaults.
    pub config: ZenConfig,
    /// The path the settings were read from, whether or not a file was there.
    pub config_path: PathBuf,
    /// One line per thing that is not the boring case: first run, a migration, a corrupt file that was
    /// preserved. Go discarded a broken config in silence; these lines are the difference (plan §5.4
    /// `store/config.rs` "Done when").
    pub notes: Vec<String>,
}

impl BootContext {
    /// The boot report, one line each, for stdout and for the crash log.
    #[must_use]
    pub fn report_lines(&self) -> Vec<String> {
        let mut out = vec![
            format!(
                "boot: os={} locale={} user={} config={}",
                match self.system.os {
                    zenman_core::platform::Os::Windows => "windows",
                    zenman_core::platform::Os::Linux => "linux",
                    zenman_core::platform::Os::Macos => "darwin",
                    zenman_core::platform::Os::Other => "other",
                },
                self.system.locale_tag,
                self.system.username,
                self.config_path.display()
            ),
            format!(
                "boot: config v{} — {} bookmark(s), {} workspace(s), {} action(s), store={:?}, delete={:?}",
                self.config.schema_version,
                self.config.bookmarks.len(),
                self.config.workspaces.len(),
                self.config.custom_actions.len(),
                self.config.store_mode,
                self.config.delete_strategy
            ),
            format!(
                "boot: locales embedded: {} ({}); i18n feature {}",
                crate::i18n::embedded_locale_count(),
                if crate::i18n::embedded_locales().is_empty() {
                    "source language only".to_string()
                } else {
                    crate::i18n::embedded_locales().join(", ")
                },
                if crate::i18n::translations_available() {
                    "on"
                } else {
                    "off"
                }
            ),
        ];
        out.extend(self.notes.iter().map(|n| format!("boot note: {n}")));
        out
    }
}

/// Where this run's settings live.
#[must_use]
pub fn config_path(paths: &AppPaths) -> PathBuf {
    paths.config.join(CONFIG_FILE)
}

/// `AppPaths → SystemInfo → config`, the order of operations plan §5.9 pins.
///
/// Never fails on a broken settings file: a config the app cannot read is a note and the defaults, not
/// an exit code (Go's `log.Fatalf` on a damaged file is the behaviour plan §5.4 forbids repeating). It
/// does fail when a *preserved* copy could not be written, because then the user's bytes are genuinely at
/// risk and starting anyway would be the lie.
///
/// # Errors
/// [`crate::BootError`] with `EXIT_CONFIG` when a corrupt settings file could not be copied aside before
/// the defaults replaced it.
pub fn boot(paths: &AppPaths) -> Result<BootContext, BootError> {
    let system = SystemInfo::detect();
    let path = config_path(paths);
    let mut notes = Vec::new();

    let config = match ZenConfig::load(&path) {
        Ok(loaded) => {
            if loaded.schema_version < CURRENT_CONFIG_VERSION {
                let from = loaded.schema_version;
                match loaded.migrate(from) {
                    Ok(migrated) => {
                        notes.push(format!(
                            "migrated the settings from version {from} to {CURRENT_CONFIG_VERSION}"
                        ));
                        migrated
                    }
                    Err(e) => {
                        notes.push(format!(
                            "the settings could not be migrated from version {from}: {e}; using the \
                             built-in defaults, and the file will not be overwritten"
                        ));
                        ZenConfig::default()
                    }
                }
            } else if let Err(e) = loaded.validate() {
                notes.push(format!(
                    "the settings failed validation ({e}); the file is kept untouched and the \
                     built-in defaults are used for this run"
                ));
                ZenConfig::default()
            } else {
                loaded
            }
        }
        Err(CoreError::NotFound { .. }) => {
            notes.push(format!(
                "no settings file at {} yet: this run uses the built-in defaults and will write the \
                 file when something changes",
                path.display()
            ));
            ZenConfig::default()
        }
        Err(e) => {
            let kept = preserve_corrupt(&path).map_err(|err| {
                BootError::new(
                    EXIT_CONFIG,
                    format!(
                        "{path}: {e}, and the damaged file could not be preserved ({err})",
                        path = path.display()
                    ),
                )
            })?;
            notes.push(format!(
                "the settings file is unreadable ({e}); it was copied to {} and the built-in defaults \
                 are used for this run",
                kept.display()
            ));
            ZenConfig::default()
        }
    };

    Ok(BootContext {
        system,
        config,
        config_path: path,
        notes,
    })
}

/// The Go app's directory, as the migration sees it. `None` when the home directory is unknown.
#[must_use]
pub fn legacy_source(paths: &AppPaths) -> Option<LegacySource> {
    let config = paths.zenman_go_legacy_config()?;
    let dir = config.parent()?;
    let beside = |name: &str| {
        let p = dir.join(name);
        p.is_file().then_some(p)
    };
    Some(LegacySource {
        search_config: beside("search-config.json"),
        action_log: beside("action.log"),
        config,
    })
}

/// The first-run note about the Go app's data: read-only, and never written (plan §5.4 wants the `.bak`
/// "once, guarded by a `migrated_at` marker"; a first boot has no such marker yet, so the *write* stays
/// behind `--migrate-config --apply`).
///
/// This is the only function in the boot path that reads the Go directory, and it is separate from
/// [`boot`] for exactly that reason: everything else takes its paths from `AppPaths` and is testable
/// without touching a home directory.
#[must_use]
pub fn legacy_preview_note(paths: &AppPaths) -> Option<String> {
    let source = legacy_source(paths)?;
    if !source.config.is_file() {
        return None;
    }
    Some(match legacy::plan_migration(&source) {
        Ok((_, report)) => format!(
            "found the Go app's config at {}: {} bookmark(s), {} workspace(s), {} action(s) would import. Nothing was copied — run `zenman --migrate-config` for the full report, or `--migrate-config --apply` to write it.",
            source.config.display(),
            report.bookmarks,
            report.workspaces,
            report.actions
        ),
        Err(e) => format!(
            "the Go app's config at {} could not be read: {e}",
            source.config.display()
        ),
    })
}

/// `--migrate-config [--apply]`, resolving the Go directory from `paths` first.
///
/// # Errors
/// As [`migrate_from`].
pub fn migrate(paths: &AppPaths, apply: bool) -> Result<String, BootError> {
    let Some(source) = legacy_source(paths) else {
        return Err(BootError::new(
            EXIT_CONFIG,
            "the home directory is unknown, so the Go app's config cannot be located",
        ));
    };
    migrate_from(&source, apply)
}

/// `--migrate-config [--apply]`: report, and write the `.bak` only when told to.
///
/// Returns the text to print. The dry run is the default because the migration is the one operation in
/// the boot path that touches somebody else's file, and because `plan_migration` is shared by both
/// branches, what the dry run promised is what `--apply` does.
///
/// # Errors
/// [`crate::BootError`] with `EXIT_CONFIG` when there is no Go settings file to migrate, or when the
/// migration itself failed.
pub fn migrate_from(source: &LegacySource, apply: bool) -> Result<String, BootError> {
    if !source.config.is_file() {
        return Err(BootError::new(
            EXIT_CONFIG,
            format!(
                "no Go settings file at {}: there is nothing to migrate",
                source.config.display()
            ),
        ));
    }
    let report = legacy::migrate_from_go(source, !apply).map_err(|e| {
        BootError::new(
            EXIT_CONFIG,
            format!("migrating {}: {e}", source.config.display()),
        )
    })?;
    let mut out = Vec::new();
    out.push(format!(
        "{} Go data in {}:",
        if apply {
            "imported the"
        } else {
            "DRY RUN — would import the"
        },
        source.config.display()
    ));
    out.push(format!(
        "  bookmarks={} workspaces={} custom_actions={} undo-log records counted={}{}",
        report.bookmarks,
        report.workspaces,
        report.actions,
        report.log_entries,
        if report.log_truncated {
            " (scan truncated: more records exist)"
        } else {
            ""
        }
    ));
    match &report.backed_up {
        Some(p) => out.push(format!("  source preserved at {}", p.display())),
        None => out.push("  no backup written (a dry run writes nothing)".to_string()),
    }
    for skip in &report.skipped {
        out.push(format!("  not migrated: {skip}"));
    }
    if !apply {
        out.push(
            "  nothing was changed; re-run with --apply to write the backup and import this data"
                .to_string(),
        );
    } else {
        out.push(
            "  the Rust settings file is written by the next start that changes a setting"
                .to_string(),
        );
    }
    Ok(out.join("\n"))
}

/// Copy an unreadable settings file to `config.json.bak`, once. An existing `.bak` is never overwritten:
/// the first preserved copy is the only one that predates this program.
fn preserve_corrupt(path: &Path) -> Result<PathBuf, std::io::Error> {
    let mut bak = path.as_os_str().to_os_string();
    bak.push(".bak");
    let bak = PathBuf::from(bak);
    if bak.exists() {
        return Ok(bak);
    }
    std::fs::copy(path, &bak).map(|_| bak)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(tag: &str) -> AppPaths {
        let dir = std::env::temp_dir().join(format!(
            "zenman-boot-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        AppPaths::resolve(&crate::app_dirs::Overrides {
            data_dir: Some(dir.clone()),
            config: Some(dir.join("config")),
            ..Default::default()
        })
        .expect("explicit overrides resolve")
    }

    #[test]
    fn a_first_boot_reports_the_defaults_and_writes_nothing() {
        let p = paths("first");
        let ctx = boot(&p).expect("a missing config never fails the boot");
        assert_eq!(ctx.config, ZenConfig::default());
        assert!(
            ctx.notes.iter().any(|n| n.contains("no settings file")),
            "{:?}",
            ctx.notes
        );
        assert!(
            !config_path(&p).exists(),
            "a boot does not create the settings file"
        );
        assert!(ctx.config_path.ends_with(CONFIG_FILE));
        // The report is printable and mentions the host, not a path we invented.
        let lines = ctx.report_lines();
        assert!(lines[0].starts_with("boot: os="), "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("boot note: ")));
    }

    #[test]
    fn a_good_config_round_trips_and_a_newer_one_is_never_rewritten() {
        let p = paths("round");
        std::fs::create_dir_all(&p.config).expect("config dir");
        let path = config_path(&p);
        let mut cfg = ZenConfig::default();
        cfg.bookmarks.push(zenman_core::store::config::Bookmark {
            path: "/tmp/one".into(),
            label: None,
            added_at: None,
        });
        cfg.save(&path).expect("saved");
        let ctx = boot(&p).expect("boots");
        assert_eq!(ctx.config, cfg);
        assert!(ctx.notes.is_empty(), "{:?}", ctx.notes);

        // A file from a future version: refuse, keep the bytes, do not "fix" it.
        let future = format!(
            "{{\"schemaVersion\":{},\"bookmarks\":[{{\"path\":\"/x\"}}]}}",
            CURRENT_CONFIG_VERSION + 1
        );
        std::fs::write(&path, &future).expect("written");
        let ctx = boot(&p).expect("a future version still boots");
        assert_eq!(ctx.config, ZenConfig::default());
        assert!(
            ctx.notes
                .iter()
                .any(|n| n.contains("validation") && n.contains("kept untouched")),
            "{:?}",
            ctx.notes
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("still the future file"),
            future
        );

        // A version-0 (Go-shaped) stamp is migrated, not rejected.
        std::fs::write(&path, "{\"schemaVersion\":0,\"bookmarks\":[]}").expect("written");
        let ctx = boot(&p).expect("v0 boots");
        assert_eq!(ctx.config.schema_version, CURRENT_CONFIG_VERSION);
        assert!(
            ctx.notes.iter().any(|n| n.contains("migrated")),
            "{:?}",
            ctx.notes
        );
    }

    #[test]
    fn a_corrupt_config_is_preserved_before_the_defaults_replace_it() {
        let p = paths("corrupt");
        std::fs::create_dir_all(&p.config).expect("config dir");
        let path = config_path(&p);
        std::fs::write(&path, b"{\"bookmarks\": [").expect("corrupt written");
        let ctx = boot(&p).expect("boots with defaults");
        assert_eq!(ctx.config, ZenConfig::default());
        let bak = PathBuf::from(format!("{}.bak", path.display()));
        assert_eq!(
            std::fs::read(&bak).expect("preserved bytes"),
            b"{\"bookmarks\": [",
            "the first preserved copy is the only one that predates this program"
        );
        assert!(
            ctx.notes.iter().any(|n| n.contains("copied to")),
            "{:?}",
            ctx.notes
        );
        // And a second corrupt save does not clobber the first backup.
        std::fs::write(&path, b"second damage").expect("written again");
        let ctx = boot(&p).expect("boots again");
        assert_eq!(std::fs::read(&bak).expect("backup"), b"{\"bookmarks\": [");
        assert!(ctx.notes.iter().any(|n| n.contains("unreadable")));
    }

    /// The Go directory is named by the test, never resolved from `$HOME` (rule: a test may not read or
    /// migrate the user's real `~/.config/zen-man`).
    fn go_dir(tag: &str) -> (AppPaths, LegacySource) {
        let p = paths(tag);
        let dir = p.data.join("go");
        std::fs::create_dir_all(&dir).expect("go dir");
        (
            p.clone(),
            LegacySource {
                config: dir.join("config.json"),
                search_config: None,
                action_log: None,
            },
        )
    }

    #[test]
    fn migrate_reports_and_backs_up_only_when_told_to() {
        let (_p, source) = go_dir("migrate");
        // Nothing to migrate: an explicit error, not an empty success.
        let err = migrate_from(&source, false).expect_err("no Go config is an error");
        assert!(
            err.message.contains("nothing to migrate"),
            "{}",
            err.message
        );
        assert_eq!(err.code, EXIT_CONFIG);

        std::fs::write(
            &source.config,
            br#"{"bookmarks":["/tmp/a"],"workspaces":{},"custom_actions":[]}"#,
        )
        .expect("go config");

        let dry = migrate_from(&source, false).expect("dry report");
        assert!(dry.contains("DRY RUN — would import the"), "{dry}");
        assert!(
            dry.contains("--apply"),
            "the report says how to actually do it"
        );
        assert!(
            !dir_of(&source).join("config.json.bak").exists(),
            "a dry run writes nothing"
        );

        let applied = migrate_from(&source, true).expect("applied");
        assert!(applied.contains("imported"), "{applied}");
        assert!(
            dir_of(&source).join("config.json.bak").is_file(),
            "the acceptance criterion: the *source* file gets a .bak"
        );
        assert!(applied.contains("bookmarks=1"), "{applied}");
    }

    fn dir_of(source: &LegacySource) -> PathBuf {
        source.config.parent().expect("parent").to_path_buf()
    }
}
