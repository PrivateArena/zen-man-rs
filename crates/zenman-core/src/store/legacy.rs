//! One-time import of the Go app's files (plan §5.4 `store/legacy.rs` row).
//!
//! The Go program's data lives in `~/.config/zen-man/` — `config.json`, `search-config.json`,
//! `action.log`, `zen-man.db` (`docs/contracts.md` → *Config and session files*). This module reads the
//! first two and turns them into a [`ZenConfig`].
//!
//! # Safety, in the order the code enforces it
//!
//! 1. every input path arrives in [`LegacySource`] from the caller. Nothing here discovers a directory:
//!    in particular nothing here reads the real `~/.config/zen-man` unless the caller names it.
//! 2. `dry_run == true` (which `apps/zenman`'s `--migrate-config` uses unless `--apply` is passed) means
//!    **zero writes** — not "writes somewhere harmless".
//! 3. the only file this module ever writes is `<directory of the source config>/config.json.bak`, and
//!    the code checks that the backup path is in that directory before writing. It never modifies,
//!    renames or deletes the source, and it never touches `action.log`/`zen-man.db` (steps 5 respec-
//!    tively own decoding and rebuilding those; copying them here would put bytes in the Rust database
//!    that nothing has verified).
//! 4. the backup is written **once**: an existing `.bak` is never overwritten (the second run reports it
//!    in `skipped`), because the first backup is the only copy that predates anything this program did.
//!
//! # `AppPaths` is not in the signature
//!
//! The step-4 brief writes the parameter as `&AppPaths`. `AppPaths` lives in `apps/zenman`, and
//! `zenman-core` may not depend on the binary (plan §5.1; `cargo xtask deps` fails on the reverse edge),
//! so the caller-supplied struct is [`LegacySource`] and `apps/zenman::app_dirs::AppPaths::legacy_source`
//! builds it. Same data, no dependency inversion.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::CoreError;
use crate::store::config::{
    Bookmark, CustomAction, GO_LEGACY_VERSION, GroupColor, IndexerSettings, LayoutMode,
    PaneSession, PaneSlot, TabSession, Workspace, ZenConfig,
};

/// The Go app's defaults, captured verbatim in `tests/golden/search-config.json` → `excludes`.
pub const GO_DEFAULT_EXCLUDES: &[&str] = &[
    "/proc",
    "/sys",
    "/dev",
    "**/.git",
    "**/node_modules",
    "**/__pycache__",
    "**/flatcap",
];

/// The three Go files step 4 can act on. Every field is a full file path the caller chose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacySource {
    /// `<go dir>/config.json` — bookmarks, workspaces, custom actions.
    pub config: PathBuf,
    /// `<go dir>/search-config.json`, when the user ever opened the search manager.
    pub search_config: Option<PathBuf>,
    /// `<go dir>/action.log`, when it exists. Step 4 only counts it (see [`MigrationReport::log_entries`]).
    pub action_log: Option<PathBuf>,
}

impl LegacySource {
    /// The Go directory, i.e. the parent of `config` — the only directory any write may land in.
    fn directory(&self) -> Option<&Path> {
        self.config.parent()
    }

    /// `<dir>/config.json.bak`, the one path this module writes.
    #[must_use]
    pub fn backup_path(&self) -> PathBuf {
        let name = self
            .config
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "config.json".to_string());
        self.config.with_file_name(format!("{name}.bak"))
    }
}

/// What a migration did, or (under `dry_run`) would do.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MigrationReport {
    /// Bookmarks carried across.
    pub bookmarks: usize,
    /// Workspaces carried across.
    pub workspaces: usize,
    /// Custom actions carried across.
    pub actions: usize,
    /// Undo-log records **seen**. A lower bound: step 4 counts `ZMLG` magic occurrences, it does not
    /// decode them — `history/codec.rs` (step 5) owns the CRC-checked decoder, and nothing is copied
    /// from the log until then.
    pub log_entries: usize,
    /// `log_entries` hit the scan budget, so the true count is higher.
    pub log_truncated: bool,
    /// The `.bak` this run created, or that already existed. `None` under `dry_run`, where nothing is
    /// written at all.
    pub backed_up: Option<PathBuf>,
    /// Every deliberate omission, in human-readable form: what the user should know was not copied.
    /// This includes the per-tab `color` drop the contract table calls a real data loss.
    pub skipped: Vec<String>,
}

/// Go's `config.json`, field-for-field (plan §5.4's `LegacyConfigJson`).
///
/// No `deny_unknown_fields`: the Go frontend writes what its JS object held, and a future Go build that
/// adds a key must not make the port refuse to start — the unknown keys are still surfaced by
/// [`walk_go_key_paths`], which is what the tests assert against.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct LegacyConfigJson {
    /// Plain path strings; no labels, no timestamps (unlike the port's [`Bookmark`]).
    #[serde(default)]
    pub bookmarks: Vec<String>,
    /// Workspace name → Go's flat pane struct.
    #[serde(default)]
    pub workspaces: BTreeMap<String, LegacyWorkspace>,
    /// Custom actions, already in the port's shape (contract table: 1:1).
    #[serde(default)]
    pub custom_actions: Vec<CustomAction>,
}

/// `workspaces.<name>`: `left_tabs`/`left_active`/`right_tabs`/`right_active`/`split`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct LegacyWorkspace {
    /// Left pane tabs.
    #[serde(default)]
    pub left_tabs: Vec<LegacyTabState>,
    /// Selected left tab id, `""` for none.
    #[serde(default)]
    pub left_active: String,
    /// Right pane tabs.
    #[serde(default)]
    pub right_tabs: Vec<LegacyTabState>,
    /// Selected right tab id, `""` for none.
    #[serde(default)]
    pub right_active: String,
    /// Two panes shown. Absent means single (contract table).
    #[serde(default)]
    pub split: bool,
}

/// `TabState{id,path,name,group,color}`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct LegacyTabState {
    /// Tab id.
    #[serde(default)]
    pub id: String,
    /// Directory shown.
    #[serde(default)]
    pub path: String,
    /// Title.
    #[serde(default)]
    pub name: String,
    /// Group letter, or `""`.
    #[serde(default)]
    pub group: String,
    /// Hex colour beside the group letter — parsed so the walk can prove it is *seen*, then dropped per
    /// the contract table.
    #[serde(default)]
    pub color: String,
}

/// Go's `search-config.json` (plan §5.4's `LegacySearchConfig`), including the runtime `index_state`
/// block that is deliberately not carried over (see `config::UNMAPPED_GO_KEYS`).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct LegacySearchConfig {
    /// Always 1 in the capture.
    #[serde(default)]
    pub schema_version: u32,
    /// Roots.
    #[serde(default)]
    pub roots: Vec<String>,
    /// Glob excludes.
    #[serde(default)]
    pub excludes: Vec<String>,
    /// Timer-driven reindex.
    #[serde(default)]
    pub auto_index: bool,
    /// Reindex at start.
    #[serde(default)]
    pub index_on_startup: bool,
    /// 0 = unlimited.
    #[serde(default)]
    pub max_depth: u32,
    /// Follow symlinks.
    #[serde(default)]
    pub follow_symlinks: bool,
    /// Walk threads.
    #[serde(default)]
    pub worker_count: u32,
    /// Retained as raw JSON: this module does not decide what `index_state` means, it only refuses to
    /// pretend it was migrated.
    #[serde(default)]
    pub index_state: Option<serde_json::Value>,
}

/// Read and map Go's `config.json`.
///
/// # Errors
/// [`CoreError::NotFound`] when the file is absent, [`CoreError::Io`] on any other read failure, and
/// [`CoreError::Config`] when the JSON cannot be read as Go's shape. `schema_version` comes back as
/// [`GO_LEGACY_VERSION`]; the caller runs [`ZenConfig::migrate`].
pub fn load_legacy_config(path: &Path) -> Result<ZenConfig, CoreError> {
    let legacy = read_legacy_config(path)?;
    Ok(zen_config_from(legacy))
}

/// Read Go's separate bookmark file, if a user has one.
///
/// The Go app only ever kept bookmarks inside `config.json`; this accepts the two shapes a hand-made or
/// third-party file realistically has — `["/a", "/b"]` and `[{"path":"/a","label":"A"}]` — and rejects
/// anything else rather than silently producing empty bookmarks (Go's `json.Unmarshal` into `[]string`
/// failed the whole file).
///
/// # Errors
/// [`CoreError::NotFound`], [`CoreError::Io`], or [`CoreError::Config`] for a JSON value that is neither
/// shape, or for an entry whose path is blank.
pub fn load_bookmarks_file(path: &Path) -> Result<Vec<Bookmark>, CoreError> {
    let text = read_text(path)?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| CoreError::Config {
        msg: format!("{}: {e}", path.display()),
    })?;
    let items = value.as_array().ok_or_else(|| CoreError::Config {
        msg: format!("{}: expected a JSON array of bookmarks", path.display()),
    })?;
    items
        .iter()
        .map(|item| {
            let raw = match item {
                serde_json::Value::String(s) => (s.clone(), None),
                serde_json::Value::Object(map) => {
                    let path = map
                        .get("path")
                        .or_else(|| map.get("bookMark"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let label = map
                        .get("label")
                        .or_else(|| map.get("name"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string);
                    (path, label)
                }
                other => {
                    return Err(CoreError::Config {
                        msg: format!(
                            "{}: bookmark entry {other} is neither a path nor an object",
                            path.display()
                        ),
                    });
                }
            };
            if raw.0.trim().is_empty() {
                return Err(CoreError::Config {
                    msg: format!("{}: bookmark entry has no path", path.display()),
                });
            }
            Ok(Bookmark {
                path: raw.0,
                label: raw.1,
                added_at: None,
            })
        })
        .collect()
}

/// Read the Go files and produce the config plus the report — **without writing anything**.
///
/// This is the function `--migrate-config` (dry run) shows the user, and the one `migrate_from_go`
/// reuses when it applies, so what the dry run promised is exactly what the apply does.
///
/// # Errors
/// As [`load_legacy_config`], plus [`CoreError::Io`] reading `search-config.json`/`action.log`.
pub fn plan_migration(source: &LegacySource) -> Result<(ZenConfig, MigrationReport), CoreError> {
    let legacy = read_legacy_config(&source.config)?;
    let mut report = MigrationReport {
        bookmarks: legacy.bookmarks.len(),
        workspaces: legacy.workspaces.len(),
        actions: legacy.custom_actions.len(),
        ..Default::default()
    };
    let mut cfg = zen_config_from(legacy);

    // Per-tab colour is dropped by design (contract table); say so once, not once per tab.
    let coloured = cfg
        .workspaces
        .values()
        .flat_map(|w| w.panes.values())
        .flat_map(|p| &p.tabs)
        .filter(|t| t.group.is_some())
        .count();
    if coloured > 0 {
        report.skipped.push(format!(
            "{coloured} tab(s) had a Go hex colour: the group letter is kept and the colour is derived \
             from it (GroupColor::color()), so a custom colour does not survive — docs/contracts.md \
             migration table"
        ));
    }

    if let Some(search) = &source.search_config {
        match read_text(search) {
            Ok(text) => {
                let legacy_index: LegacySearchConfig =
                    serde_json::from_str(&text).map_err(|e| CoreError::Config {
                        msg: format!("{}: {e}", search.display()),
                    })?;
                cfg.indexer = IndexerSettings {
                    roots: legacy_index.roots,
                    excludes: legacy_index.excludes,
                    auto_index: legacy_index.auto_index,
                    index_on_startup: legacy_index.index_on_startup,
                    max_depth: legacy_index.max_depth,
                    follow_symlinks: legacy_index.follow_symlinks,
                    worker_count: legacy_index.worker_count,
                };
                if legacy_index.index_state.is_some() {
                    report.skipped.push(
                        "search-config.json index_state{last_indexed_unix,total_files,is_dirty} is Go's \
                         runtime cache and is not imported: the Rust index reports its own"
                            .to_string(),
                    );
                }
            }
            Err(CoreError::NotFound { .. }) => report.skipped.push(format!(
                "no {} — the indexer keeps the built-in defaults",
                search.display()
            )),
            Err(e) => return Err(e),
        }
    }

    if let Some(log) = &source.action_log {
        match scan_log_magic(log) {
            Ok((count, truncated)) => {
                report.log_entries = count;
                report.log_truncated = truncated;
                report.skipped.push(format!(
                    "action.log: {count} record(s) counted, none copied — decoding is step 5's \
                     history/codec.rs and the undo log is byte-format-sensitive (ZMLG + CRC-32)"
                ));
            }
            Err(CoreError::NotFound { .. }) => report
                .skipped
                .push(format!("no {} — the undo log starts empty", log.display())),
            Err(e) => return Err(e),
        }
    }

    Ok((cfg, report))
}

/// Read the Go files, then (unless `dry_run`) write `<dir>/config.json.bak` once.
///
/// `dry_run == true` writes nothing anywhere: this is what `--migrate-config` does by default, and
/// `apps/zenman` requires an explicit `--apply` before it becomes a write. The returned report's
/// `backed_up` is `None` for a dry run by construction.
///
/// The migrated [`ZenConfig`] is *not* written here: where the Rust config lives is
/// `apps/zenman::app_dirs`' decision, and writing it would put bytes outside the directory the caller
/// passed in. The caller saves it (and stamps [`ZenConfig::migrated_at`]).
///
/// # Errors
/// As [`plan_migration`], plus [`CoreError::Io`] if the backup copy fails and [`CoreError::Config`] if
/// the source path has no parent directory to hold the backup.
pub fn migrate_from_go(source: &LegacySource, dry_run: bool) -> Result<MigrationReport, CoreError> {
    let (cfg, mut report) = plan_migration(source)?;
    // Deliberately dropped: writing the *Rust* config file is the caller's job (its location is
    // `apps/zenman::app_dirs`' decision), so this function cannot put bytes outside the source directory.
    drop(cfg);
    let backup = source.backup_path();
    if backup.parent() != source.directory() {
        // Cannot happen with a `#[derive]`-built path, and is the exact invariant the safety rules turn
        // on, so it is checked rather than assumed.
        return Err(CoreError::Config {
            msg: format!(
                "refusing to write the backup at {}: it is not beside {}",
                backup.display(),
                source.config.display()
            ),
        });
    }
    if dry_run {
        if backup.exists() {
            report.skipped.push(format!(
                "{} already exists: it would not be overwritten",
                backup.display()
            ));
        } else {
            report.skipped.push(format!(
                "dry run: {} would be written beside the source config; nothing has been changed",
                backup.display()
            ));
        }
        return Ok(report);
    }
    if backup.exists() {
        report.backed_up = Some(backup.clone());
        report.skipped.push(format!(
            "{} already exists, so it was kept as it is",
            backup.display()
        ));
        return Ok(report);
    }
    std::fs::copy(&source.config, &backup).map_err(|e| {
        CoreError::io(
            format!("{} -> {}", source.config.display(), backup.display()),
            e,
        )
    })?;
    report.backed_up = Some(backup);
    Ok(report)
}

/// Parse Go's config file into the legacy DTO.
fn read_legacy_config(path: &Path) -> Result<LegacyConfigJson, CoreError> {
    let text = read_text(path)?;
    serde_json::from_str(&text).map_err(|e| CoreError::Config {
        msg: format!("{}: not a zen-man config ({e})", path.display()),
    })
}

/// Map the legacy DTO onto `ZenConfig` (the contract table, implemented).
fn zen_config_from(legacy: LegacyConfigJson) -> ZenConfig {
    let mut cfg = ZenConfig {
        schema_version: GO_LEGACY_VERSION,
        ..Default::default()
    };
    cfg.bookmarks = legacy
        .bookmarks
        .iter()
        .filter(|b| !b.trim().is_empty())
        .map(|path| Bookmark {
            path: path.clone(),
            label: None,
            added_at: None,
        })
        .collect();
    cfg.custom_actions = legacy.custom_actions;
    cfg.workspaces = legacy
        .workspaces
        .into_iter()
        .map(|(name, ws)| {
            let left = pane_from(ws.left_tabs, &ws.left_active);
            let right = pane_from(ws.right_tabs, &ws.right_active);
            let layout = if ws.split {
                LayoutMode::Split
            } else {
                LayoutMode::Single
            };
            let mut out = Workspace {
                layout,
                active: if layout == LayoutMode::Split && left.tabs.is_empty() {
                    PaneSlot::Right
                } else {
                    PaneSlot::Left
                },
                ..Default::default()
            };
            out.panes.insert(PaneSlot::Left, left);
            // The migration table is explicit: `LeftBottom`/`RightBottom` have no Go counterpart and
            // **start empty**, so an empty right pane is still recorded (it is what `split:false` means)
            // but nothing is invented for the bottom slots.
            out.panes.insert(PaneSlot::Right, right);
            (name, out)
        })
        .collect();
    cfg
}

/// One Go pane → one [`PaneSession`].
fn pane_from(tabs: Vec<LegacyTabState>, active: &str) -> PaneSession {
    let tabs: Vec<TabSession> = tabs
        .into_iter()
        .map(|t| TabSession {
            id: t.id,
            path: t.path,
            name: t.name,
            // `group: ""` is "no group" — proven by `tests/golden/legacy-config-real.json`.
            group: GroupColor::parse_go(&t.group),
            view: crate::store::config::ViewMode::default(),
        })
        .collect();
    let index = tabs
        .iter()
        .position(|t| t.id == active)
        .unwrap_or(usize::MAX);
    PaneSession {
        tabs,
        active: index,
    }
}

/// How many bytes of `action.log` one scan may read: enough for a normal session, bounded so a 200 MB
/// log cannot stall a migration report.
const LOG_SCAN_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Count `ZMLG` magic occurrences in the first [`LOG_SCAN_MAX_BYTES`] of the file.
fn scan_log_magic(path: &Path) -> Result<(usize, bool), CoreError> {
    const MAGIC: [u8; 4] = [0x5A, 0x4D, 0x4C, 0x47]; // "ZMLG" — `history/codec.rs` (step 5) owns the format
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(CoreError::NotFound {
                path: path.display().to_string(),
                source_path: None,
            });
        }
        Err(e) => return Err(CoreError::io(path.display().to_string(), e)),
    };
    let truncated = bytes.len() > LOG_SCAN_MAX_BYTES;
    let window = &bytes[..bytes.len().min(LOG_SCAN_MAX_BYTES)];
    let count = window.windows(MAGIC.len()).filter(|w| *w == MAGIC).count();
    Ok((count, truncated))
}

/// Every key path in a Go JSON document, normalised so fixtures with different workspace names, tab
/// counts and array lengths produce the same list:
///
/// * a key under `workspaces` becomes `<name>`,
/// * one element of an array of objects is walked and written `[]key`,
/// * an array of scalars, or an empty container, is a leaf.
///
/// This is the machinery behind the "no Go key is silently dropped" test.
#[must_use]
pub fn walk_go_key_paths(file_label: &str, value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    walk(file_label, value, "", &mut out);
    out.sort();
    out.dedup();
    out
}

/// The last path segment, which is what decides whether a key is a user-chosen workspace name.
fn last_segment(prefix: &str) -> &str {
    prefix.rsplit('.').next().unwrap_or(prefix)
}

/// One node of [`walk_go_key_paths`]. `prefix` is the path so far, without the `{label}: ` header.
fn walk(label: &str, value: &serde_json::Value, prefix: &str, out: &mut Vec<String>) {
    let mut leaf = true;
    match value {
        serde_json::Value::Object(map) if !map.is_empty() => {
            leaf = false;
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for key in keys {
                // A key under `workspaces` is a *user-chosen name*, so it normalises away; nothing else
                // in either file is.
                let written = if last_segment(prefix) == "workspaces" {
                    "<name>"
                } else {
                    key
                };
                let next = if prefix.is_empty() {
                    written.to_string()
                } else {
                    format!("{prefix}.{written}")
                };
                walk(label, &map[key], &next, out);
            }
        }
        serde_json::Value::Array(items) if items.iter().any(serde_json::Value::is_object) => {
            leaf = false;
            for item in items.iter().filter(|i| i.is_object()) {
                walk(label, item, &format!("{prefix}[]"), out);
            }
        }
        _ => {}
    }
    if leaf {
        out.push(format!("{label}: {prefix}"));
    }
}

/// `read_to_string` with the Go-app's missing file reported as `NotFound`.
fn read_text(path: &Path) -> Result<String, CoreError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(CoreError::NotFound {
            path: path.display().to_string(),
            source_path: None,
        }),
        Err(e) => Err(CoreError::io(path.display().to_string(), e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::config::{MAPPED_GO_KEY_PATHS, UNMAPPED_GO_KEYS};

    const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/golden");

    fn golden(name: &str) -> PathBuf {
        Path::new(GOLDEN).join(name)
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zenman-legacy-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// The expectation table, `docs/contracts.md` → *config.json → ZenConfig migration table*, applied
    /// to `tests/golden/legacy-config-real.json`.
    #[test]
    fn the_real_go_config_imports_to_the_expected_zen_config() {
        let cfg = load_legacy_config(&golden("legacy-config-real.json")).expect("imports");
        assert_eq!(
            cfg.schema_version, GO_LEGACY_VERSION,
            "a Go file is version 0 until `migrate` says otherwise"
        );
        assert!(cfg.bookmarks.is_empty(), "Go had none here");
        assert_eq!(cfg.workspaces.len(), 1);
        let ws = &cfg.workspaces["default"];
        assert_eq!(ws.layout, LayoutMode::Single, "split:false");
        assert_eq!(ws.active, PaneSlot::Left);
        let left = &ws.panes[&PaneSlot::Left];
        assert_eq!(left.tabs.len(), 4, "four tabs in the capture");
        assert_eq!(left.tabs[0].id, "tab_1794479063471_19hja");
        assert_eq!(left.tabs[0].name, "zen-man");
        assert!(
            left.tabs[0].path.ends_with("/home/Deve/zen-man"),
            "{}",
            left.tabs[0].path
        );
        assert_eq!(
            left.tabs[0].group, None,
            "`group:\"\"` means no group, not group 0"
        );
        let active = left.tabs[3].id.clone();
        assert_eq!(
            left.tabs[left.active].id, active,
            "left_active resolves to the 4th tab"
        );
        assert_eq!(
            ws.panes[&PaneSlot::Right].tabs,
            Vec::new(),
            "right_tabs was captured empty and stays empty"
        );
        assert!(!ws.panes.contains_key(&PaneSlot::LeftBottom));
        assert!(!ws.panes.contains_key(&PaneSlot::RightBottom));
        for tab in &left.tabs {
            assert_eq!(
                tab.view,
                crate::store::config::ViewMode::List,
                "view has no Go counterpart"
            );
        }
        assert!(cfg.custom_actions.is_empty());
        cfg.clone().migrate(GO_LEGACY_VERSION).expect("v0 → v1");
    }

    /// The other golden: `split:true`, a group letter, and a hex colour that must be *seen and dropped*.
    #[test]
    fn the_fixture_config_imports_the_split_workspace_and_its_group() {
        let cfg = load_legacy_config(&golden("config.json")).expect("imports");
        let ws = &cfg.workspaces["fixture"];
        assert_eq!(ws.layout, LayoutMode::Split);
        assert_eq!(ws.active, PaneSlot::Left);
        let left = &ws.panes[&PaneSlot::Left];
        assert_eq!(left.tabs.len(), 1);
        assert_eq!(left.tabs[0].group, Some(GroupColor::A));
        assert_eq!(GroupColor::A.color(), Some("#ff4a4a"));
        assert_eq!(left.active, 0);
        // and after `migrate`, the document is a legal Rust config
        let moved = cfg.migrate(GO_LEGACY_VERSION).expect("migrated");
        moved.validate().expect("valid");
    }

    /// The honesty check: walk every key path in both goldens and require it to be either mapped or in
    /// the explicit drop list with a reason.
    #[test]
    fn no_go_key_is_silently_dropped() {
        let mut walked = Vec::new();
        for (label, file) in [
            ("config.json", "config.json"),
            ("config.json", "legacy-config-real.json"),
            ("search-config.json", "search-config.json"),
        ] {
            let text = std::fs::read_to_string(golden(file)).expect("golden readable");
            let value: serde_json::Value = serde_json::from_str(&text).expect("golden parses");
            walked.extend(walk_go_key_paths(label, &value));
        }
        walked.sort();
        walked.dedup();
        let mapped: Vec<&str> = MAPPED_GO_KEY_PATHS.to_vec();
        let unmapped: Vec<&str> = UNMAPPED_GO_KEYS.iter().map(|(k, _)| *k).collect();
        let unaccounted: Vec<&String> = walked
            .iter()
            .filter(|p| !mapped.contains(&p.as_str()) && !unmapped.contains(&p.as_str()))
            .collect();
        assert!(
            unaccounted.is_empty(),
            "Go keys the migration neither maps nor lists as dropped: {unaccounted:?}\nwalked: {walked:?}"
        );
        // (The drop list is checked against a document that actually contains every dropped key in
        // `the_extended_go_shape_is_fully_accounted`: the two goldens have no populated right pane.)
    }

    #[test]
    fn the_extended_go_shape_is_fully_accounted() {
        // `tests/golden/` has no extended capture (the plan names one; step 2 never wrote it), so the
        // wider Go shape — a split workspace whose *right* pane has grouped, coloured tabs — is exercised
        // from an inline document instead. It is the same bytes the frontend would write.
        let extended = serde_json::json!({
            "bookmarks": ["/a"],
            "workspaces": {
                "two-pane": {
                    "left_tabs": [{"id":"l1","path":"/l","name":"L","group":"A","color":"#ff4a4a"}],
                    "left_active": "l1",
                    "right_tabs": [{"id":"r1","path":"/r","name":"R","group":"C","color":"#3aa757"}],
                    "right_active": "r1",
                    "split": true
                }
            },
            "custom_actions": [{"id":"e6f7c823c3f57bb9","name":"echo","icon":"terminal",
                "command":"true","role":"files","patterns":"*.txt","show_output":true}]
        });
        let walked = walk_go_key_paths("config.json", &extended);
        let mut all = walked.clone();
        for (label, file) in [
            ("config.json", "config.json"),
            ("config.json", "legacy-config-real.json"),
            ("search-config.json", "search-config.json"),
        ] {
            let text = std::fs::read_to_string(golden(file)).expect("golden readable");
            let value: serde_json::Value = serde_json::from_str(&text).expect("golden parses");
            all.extend(walk_go_key_paths(label, &value));
        }
        all.sort();
        all.dedup();
        let unmapped = UNMAPPED_GO_KEYS.iter().map(|(k, _)| *k).collect::<Vec<_>>();
        let unaccounted: Vec<&String> = walked
            .iter()
            .filter(|p| {
                !MAPPED_GO_KEY_PATHS.contains(&p.as_str()) && !unmapped.contains(&p.as_str())
            })
            .collect();
        assert!(
            unaccounted.is_empty(),
            "unaccounted Go keys: {unaccounted:?}"
        );
        // Every entry of the drop list is real: each one is seen in the goldens or in this document.
        for entry in &unmapped {
            assert!(
                all.iter().any(|p| p == entry),
                "UNMAPPED_GO_KEYS names {entry}, which nothing writes:\n{all:?}"
            );
        }
        assert!(
            walked.iter().any(|p| p.ends_with("right_tabs[].color")),
            "the right pane's colour is walked: {walked:?}"
        );
    }

    #[test]
    fn a_dry_run_writes_absolutely_nothing() {
        let dir = scratch("dry");
        let source = copy_go_tree(&dir, "dry");
        let before = listing(&dir);
        let report = migrate_from_go(&source, true).expect("planned");
        assert_eq!(report.backed_up, None, "a dry run writes nothing");
        assert_eq!(listing(&dir), before, "the directory is byte-identical");
        assert!(
            !dir.join("config.json.bak").exists(),
            "no .bak from a dry run"
        );
        assert!(
            report
                .skipped
                .iter()
                .any(|s| s.contains("dry run") && s.contains("nothing has been changed")),
            "{:?}",
            report.skipped
        );
        assert_eq!(report.bookmarks, 2);
        assert_eq!(report.workspaces, 1);
        assert_eq!(report.actions, 1);
    }

    #[test]
    fn applying_leaves_a_backup_beside_the_source_and_keeps_the_source() {
        let dir = scratch("apply");
        let source = copy_go_tree(&dir, "apply");
        let original = std::fs::read(&source.config).expect("source bytes");
        let report = migrate_from_go(&source, false).expect("migrated");
        let bak = dir.join("config.json.bak");
        assert_eq!(report.backed_up.as_deref(), Some(bak.as_path()));
        assert!(bak.is_file(), "config.json → config.json.bak");
        assert_eq!(
            std::fs::read(&bak).expect("backup bytes"),
            original,
            "the backup is the untouched original"
        );
        assert_eq!(
            std::fs::read(&source.config).expect("source still there"),
            original,
            "the migration never modifies its input"
        );
        assert!(dir.join("search-config.json").is_file());
        // Nothing else appeared.
        assert_eq!(
            listing(&dir),
            [
                "config.json".to_string(),
                "config.json.bak".to_string(),
                "search-config.json".to_string(),
                "seen".to_string()
            ],
            "the migration added exactly one file: the backup"
        );
    }

    #[test]
    fn a_second_migration_keeps_the_first_backup() {
        let dir = scratch("twice");
        let source = copy_go_tree(&dir, "twice");
        migrate_from_go(&source, false).expect("first");
        let bak = dir.join("config.json.bak");
        let first = std::fs::read(&bak).expect("first backup");
        std::fs::write(&source.config, b"{\"bookmarks\":[\"/changed\"]}").expect("source edited");
        let report = migrate_from_go(&source, false).expect("second");
        assert_eq!(report.backed_up.as_deref(), Some(bak.as_path()));
        assert_eq!(
            std::fs::read(&bak).expect("backup"),
            first,
            "the only pre-port bytes are the first backup's"
        );
        assert!(
            report.skipped.iter().any(|s| s.contains("kept as it is")),
            "{:?}",
            report.skipped
        );
    }

    #[test]
    fn the_log_is_counted_not_copied_and_the_report_says_so() {
        let dir = scratch("log");
        let source = copy_go_tree(&dir, "log");
        // Two ZMLG records' worth of magic, however malformed the bodies are: step 4 only counts.
        let log = dir.join("action.log");
        let source = LegacySource {
            action_log: Some(log.clone()),
            ..source
        };
        std::fs::write(
            &log,
            [b"ZMLG".as_slice(), b"\0garbage", b"ZMLG", b"\0more"].concat(),
        )
        .expect("log written");
        let report = migrate_from_go(&source, false).expect("migrated");
        assert_eq!(report.log_entries, 2);
        assert!(!report.log_truncated, "a small log is fully scanned");
        assert!(
            report
                .skipped
                .iter()
                .any(|s| s.contains("history/codec.rs")),
            "{:?}",
            report.skipped
        );
        assert!(log.is_file(), "the source log is untouched");
    }

    #[test]
    fn missing_search_config_and_log_are_reported_not_fatal() {
        let dir = scratch("sparse");
        std::fs::write(
            dir.join("config.json"),
            b"{\"bookmarks\":[],\"workspaces\":{},\"custom_actions\":[]}",
        )
        .expect("minimal config");
        let source = LegacySource {
            config: dir.join("config.json"),
            search_config: Some(dir.join("search-config.json")),
            action_log: Some(dir.join("action.log")),
        };
        let report = migrate_from_go(&source, true).expect("a bare Go directory still migrates");
        assert_eq!(report.workspaces, 0);
        // The two absent files plus the dry-run promise, and nothing else.
        assert_eq!(report.skipped.len(), 3, "{:?}", report.skipped);
        assert!(
            report
                .skipped
                .iter()
                .any(|s| s.contains("search-config.json"))
        );
        assert!(report.skipped.iter().any(|s| s.contains("action.log")));
    }

    #[test]
    fn a_missing_config_is_not_found_rather_than_a_silent_default() {
        let dir = scratch("absent");
        let source = LegacySource {
            config: dir.join("config.json"),
            search_config: None,
            action_log: None,
        };
        assert!(matches!(
            migrate_from_go(&source, true),
            Err(CoreError::NotFound { .. })
        ));
    }

    #[test]
    fn a_bookmarks_file_accepts_both_shapes_and_rejects_junk() {
        let dir = scratch("bookmarks");
        let plain = dir.join("plain.json");
        std::fs::write(&plain, b"[\"/a\",\"/b b\"]").expect("written");
        assert_eq!(
            load_bookmarks_file(&plain).expect("plain"),
            vec![
                Bookmark {
                    path: "/a".into(),
                    label: None,
                    added_at: None
                },
                Bookmark {
                    path: "/b b".into(),
                    label: None,
                    added_at: None
                }
            ]
        );
        let rich = dir.join("rich.json");
        std::fs::write(&rich, br#"[{"path":"/x","name":"X"}]"#).expect("written");
        assert_eq!(
            load_bookmarks_file(&rich).expect("rich")[0],
            Bookmark {
                path: "/x".into(),
                label: Some("X".into()),
                added_at: None
            }
        );
        for (name, body) in [
            ("obj.json", br#"{"/a":1}"#.as_slice()),
            ("blank.json", br#"[{"path":"  "}]"#.as_slice()),
            ("num.json", br#"[3]"#.as_slice()),
        ] {
            let path = dir.join(name);
            std::fs::write(&path, body).expect("written");
            assert!(
                matches!(load_bookmarks_file(&path), Err(CoreError::Config { .. })),
                "{name} must be refused"
            );
        }
    }

    #[test]
    fn the_search_config_seeds_the_indexer_and_the_built_in_defaults_match_the_capture() {
        let dir = scratch("search");
        let source = copy_go_tree(&dir, "search");
        let (cfg, report) = plan_migration(&source).expect("planned");
        assert_eq!(cfg.indexer.worker_count, 2);
        assert_eq!(
            cfg.indexer.excludes, GO_DEFAULT_EXCLUDES,
            "captured verbatim"
        );
        assert!(!cfg.indexer.auto_index);
        assert!(cfg.indexer.roots[0].ends_with("/tree"));
        assert!(
            report.skipped.iter().any(|s| s.contains("index_state")),
            "the runtime block is named as not-imported: {:?}",
            report.skipped
        );
        // The port's own default excludes are the capture's, so a fresh install behaves like Go.
        assert_eq!(IndexerSettings::default().excludes, GO_DEFAULT_EXCLUDES);
    }

    /// A Go directory in a temp dir — never the real `~/.config/zen-man`.
    fn copy_go_tree(dir: &Path, tag: &str) -> LegacySource {
        let mut config = std::fs::read_to_string(golden("config.json")).expect("golden");
        // The goldens carry capture-machine placeholders; a migration test wants plausible paths.
        config = config.replace("${FIXTURE_ROOT}", &dir.join("seen").display().to_string());
        let config = config.replace(
            "\"bookmarks\": [],",
            &format!(
                "\"bookmarks\": [\"{}\", \"{}\"],",
                dir.join("b1").display(),
                dir.join("b2").display()
            ),
        );
        let config = config.replace(
            "\"custom_actions\": []",
            &format!(
                "\"custom_actions\": [{{\"id\":\"e6f7c823c3f57bb9\",\"name\":\"{tag}\",\"icon\":\"terminal\",\"command\":\"true\",\"role\":\"files\",\"patterns\":\"*.txt\",\"show_output\":true}}]"
            ),
        );
        std::fs::create_dir_all(dir.join("seen")).expect("seen dir");
        std::fs::write(dir.join("config.json"), config).expect("config");
        let search = std::fs::read_to_string(golden("search-config.json")).expect("golden");
        let search = search.replace("${FIXTURE_ROOT}", &dir.join("seen").display().to_string());
        std::fs::write(dir.join("search-config.json"), search).expect("search config");
        LegacySource {
            config: dir.join("config.json"),
            search_config: Some(dir.join("search-config.json")),
            action_log: None,
        }
    }

    fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("readable")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}
