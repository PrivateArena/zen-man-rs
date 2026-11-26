//! The settings file: `ZenConfig` (plan §5.4 `store/config.rs` row).
//!
//! Two shapes live in this crate and they are deliberately different:
//!
//! * [`ZenConfig`] is the **Rust** shape, written by this app. Every struct here is
//!   `#[serde(rename_all = "camelCase", default)]`: camelCase is what the Go frontend spoke on the
//!   wire (`docs/contracts.md` §2.4 row *`/api/op` copy/cut*: `items[].isDir`) and it is the form the
//!   port's own files use from step 4 on. `default` everywhere means a field added in version 2 cannot
//!   break a version-1 file.
//! * the Go on-disk shape lives in [`crate::store::legacy`], verbatim snake_case, because that file is
//!   somebody else's data.
//!
//! The one struct that keeps Go's spelling is [`CustomAction`]: `docs/contracts.md`'s migration table
//! says `custom_actions[]` is **1:1**, and the captured bytes (`tests/golden/actions.json`) use
//! `show_output`, so renaming it to `showOutput` would break the 1:1 promise for a cosmetic win.
//!
//! # What is *not* here
//!
//! `ZenConfig` is data, not a service: nothing in this module opens a directory, resolves a home path
//! or decides where the file lives. `apps/zenman::app_dirs::AppPaths` is the authority for that and
//! hands a `Path` to [`load`]/[`save`] (plan §5.2 `platform/system.rs` row states the same rule).
//!
//! [`load`]: ZenConfig::load
//! [`save`]: ZenConfig::save

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::limits::{
    ARCHIVE_CACHE_ENTRIES, CONCURRENT_EXTRACTS, CONCURRENT_JOBS, EXEC_OUTPUT_CAP, INDEX_MAX_DEPTH,
    INDEX_MAX_ENTRIES, LIVE_SEARCH_CAP, LIVE_SEARCH_NODES, LOG_SCAN_CAP, Limits, MAX_PAGE_SIZE,
    MAX_WALK_FILES, NESTED_ARCHIVE_MAX_BYTES, TRACK_ENTRIES_MAX,
};

/// The version type of [`ZenConfig::schema_version`].
pub type ConfigVersion = u32;

/// The schema this build writes, and the newest it will read.
pub const CURRENT_CONFIG_VERSION: ConfigVersion = 1;

/// `schema_version` for a file that has none — i.e. the Go app's `config.json`, which carries exactly
/// three keys (`docs/contracts.md` → *Config and session files*). [`ZenConfig::migrate`] treats 0 as
/// "unversioned legacy" and upgrades it.
pub const GO_LEGACY_VERSION: ConfigVersion = 0;

/// The zoom the shell starts at when the config says nothing: the native scale.
pub const DEFAULT_ZOOM_PERCENT: u32 = 100;
/// One click of the zoom buttons: the shell's `−`/`+` tiles move by this much (`panels/app_menu.rs`
/// draws the row, `zenman-ui`'s `zoom.rs` does the arithmetic).
pub const ZOOM_STEP_PERCENT: u32 = 10;
/// The smallest [`ZenConfig::zoom_percent`] the shell will draw at, and the smallest egui's own
/// `Ctrl+-` chord goes to (`egui::gui_zoom::MIN_ZOOM_FACTOR`, 0.2). One bound for both front ends,
/// because two bounds would mean a keyboard zoom the config then refuses to save.
pub const MIN_ZOOM_PERCENT: u32 = 20;
/// The largest, for the same reason (`egui::gui_zoom::MAX_ZOOM_FACTOR`, 5.0).
pub const MAX_ZOOM_PERCENT: u32 = 500;

/// Where the theme comes from. The Go app had no theme setting at all (its web UI followed the
/// browser), so this is a port addition; `System` is the default so the port does not fight the
/// desktop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ThemeMode {
    /// Follow the OS (egui/`craft_ui` theme switching).
    #[default]
    System,
    /// Always light.
    Light,
    /// Always dark.
    Dark,
}

/// What a delete does. Plan §9.2 closes OQ2 by measuring it: a volume with its own `statvfs` fsid
/// (`/media/…`) has **no** native trash root, so `Permanent` is not a nicety — without it the user can
/// only fail. `Ask` is the Go UI's behaviour (`confirm_delete` was a separate boolean in the plan's
/// `Prefs`); the port keeps both so the escalation in `ops::delete` (plan §5.3) has a policy to read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DeleteStrategy {
    /// Move to the OS trash; escalate to [`DeleteStrategy::Permanent`] only when the user opted in.
    #[default]
    Trash,
    /// Ask before every delete (Go's `confirmDelete`).
    Ask,
    /// Delete for good, and say so in the confirmation text.
    Permanent,
}

/// What the engine may persist between runs. The Go app had exactly two stores — the SQLite index
/// (`api/db.go`) and the in-process archive manifest cache — and no way to turn either off, which is
/// why a locked or unreadable `zen-man.db` took the whole server down (`log.Fatalf`, plan §5.4
/// `session.rs` row names this as the thing the port must not repeat).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StoreMode {
    /// The shared index database plus the caches: the normal mode.
    #[default]
    Full,
    /// Never open `zen-man.db`. Used for a read-only data directory and for `--no-index`; search falls
    /// back to the live walker (plan §5.3 `search_live.rs`).
    NoDb,
    /// Nothing survives the run: caches only, under the cache directory. Tests and `--smoke` use it so
    /// a headless run cannot poison a real index.
    Ephemeral,
}

/// The tab-column a sort is applied to. The set is Go's `sort` query parameter vocabulary as listed in
/// plan §5.3 (`SortKey`); the *wire* mapping is `ops/list.rs`'s (step 9), which is where the golden
/// `dir-sort-*.json` comparisons live, so this enum is the storage-side spelling only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SortColumn {
    /// Name, dirs first, byte-compare of the lowercased name (deviation ledger rows 1-2).
    #[default]
    Name,
    /// Unpacked size.
    Size,
    /// Modification time.
    Date,
    /// Permission bits.
    Mode,
    /// Extension, case-folded.
    Ext,
}

/// A sort: column plus direction. Dirs-first is *not* part of this type — it is an invariant of
/// `ops::list::sort_entries` (deviation ledger row 1, CONFIRMED), so a config can never turn it off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SortSpec {
    /// Which column.
    pub column: SortColumn,
    /// Descending. Go's default is ascending (`dir-root.json`); `dir-sort-date-desc.json` is the
    /// explicit-descending capture.
    pub descending: bool,
}

impl SortSpec {
    /// The listing order a fresh install uses: name, ascending.
    ///
    /// The plan §5.4 row the step-4 brief cites for this name is `EntryBuilder::default_sort`; kept
    /// where the data is (`entry.rs` builds rows, it does not order them), so the constant is on
    /// [`SortSpec`] and `ops/list.rs` (step 9) reads `config.sort` instead of a builder.
    pub const DEFAULT_SORT: SortSpec = SortSpec {
        column: SortColumn::Name,
        descending: false,
    };
}

/// Pixel widths of the list columns, so the migration of a hand-tuned layout survives a restart.
/// `f32` because these are egui logical points, not file offsets; [`ZenConfig::validate`] rejects
/// non-finite and absurd values rather than letting a corrupt file reach the layout pass.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ColumnWidths {
    /// Name column.
    pub name: f32,
    /// Size column.
    pub size: f32,
    /// Date column.
    pub date: f32,
    /// Mode column.
    pub mode: f32,
}

impl Default for ColumnWidths {
    fn default() -> Self {
        // Chosen to fit `ViewportBuilder::with_min_inner_size([900., 480.])` (plan §5.9): name takes the
        // slack, the three fixed columns keep Go's relative order (name, size, date/mode).
        Self {
            name: 420.0,
            size: 110.0,
            date: 170.0,
            mode: 110.0,
        }
    }
}

/// A pane in the two/three-pane layout. Go only ever had `left` and `right`
/// (`docs/contracts.md` → migration table: `PaneSlot::LeftBottom/RightBottom` have **no** Go
/// counterpart and start empty).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PaneSlot {
    /// Primary pane.
    #[default]
    Left,
    /// The second pane; only visible when [`Workspace::layout`] is [`LayoutMode::Split`].
    Right,
    /// Bottom of the left column (no Go counterpart).
    LeftBottom,
    /// Bottom of the right column (no Go counterpart).
    RightBottom,
}

/// One-or-two-pane layout. `split` is the Go key; absent means single
/// (`docs/contracts.md` → migration table).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LayoutMode {
    /// One pane.
    #[default]
    Single,
    /// Left and right panes.
    Split,
}

/// How a tab renders. Go's frontend was a list always (`docs/contracts.md` §*dir*); the grid is the
/// port's addition, so `List` is both the default and the migration target.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ViewMode {
    /// Detail list.
    #[default]
    List,
    /// Icon grid.
    Grid,
    /// Flat recursive listing (Go's `flat` mode, deviation ledger row 4).
    Flat,
}

/// A named tab group. Go stored the group letter *and* a hex colour; the port keeps the letter and
/// derives the colour, because a user re-picking a colour for a group in the Go app left the two
/// stores able to disagree (the migration table calls `color` a **real data loss to record**).
///
/// The letter is the on-disk spelling (`#[serde(rename_all = "UPPERCASE")]` is Go's `group` key, so it
/// is **not** cosmetic and must not be renamed to a colour word), and it is only ever an accent for
/// the port: group *identity* is [`TabSession::group_id`] / [`TabGroupSession::id`]. `A..E` are Go's
/// alphabet; `F..I` are the port's, added for the tab-group palette (`docs/tab-groups-design.md` §3) and
/// unknown to Go, which reads them as no group.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum GroupColor {
    /// Group `A` — red, the one colour with a captured Go hex.
    #[default]
    A,
    /// Group `B` — blue.
    B,
    /// Group `C` — green.
    C,
    /// Group `D` — yellow.
    D,
    /// Group `E` — purple.
    E,
    /// Group `F` — orange (no Go counterpart).
    F,
    /// Group `G` — cyan (no Go counterpart).
    G,
    /// Group `H` — pink (no Go counterpart).
    H,
    /// Group `I` — teal (no Go counterpart).
    I,
}

impl GroupColor {
    /// Every colour a tab group can be painted with, in menu order.
    pub const ALL: [Self; 9] = [
        Self::A,
        Self::B,
        Self::C,
        Self::D,
        Self::E,
        Self::F,
        Self::G,
        Self::H,
        Self::I,
    ];

    /// The colour this group paints with.
    ///
    /// Only [`GroupColor::A`] has a captured value: `docs/contracts.md` and `tests/golden/config.json`
    /// pin `group:"A"` with `color:"#ff4a4a"`. The other letters' Go colours were never captured (no
    /// fixture uses them), and inventing hex digits would be a fidelity lie, so they answer `None` and
    /// the theme's palette decides. Recorded as a divergence in the step-4 report.
    #[must_use]
    pub const fn color(self) -> Option<&'static str> {
        match self {
            Self::A => Some("#ff4a4a"),
            Self::B | Self::C | Self::D | Self::E | Self::F | Self::G | Self::H | Self::I => None,
        }
    }

    /// The colour's own word, for a menu row that offers it (`"Red"`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::A => "Red",
            Self::B => "Blue",
            Self::C => "Green",
            Self::D => "Yellow",
            Self::E => "Purple",
            Self::F => "Orange",
            Self::G => "Cyan",
            Self::H => "Pink",
            Self::I => "Teal",
        }
    }

    /// The colour's menu row label (`"Red Group"`) — Go's own spelling for its tab menu's group rows
    /// (`tabs.js:353-359`), kept so the port's menu reads like the reference.
    #[must_use]
    pub fn label(self) -> String {
        format!("{} Group", self.name())
    }

    /// Read one group letter, in either case. `None` for empty text and for anything that is not a
    /// letter of the alphabet — which is what makes an unknown letter a *no colour* rather than a
    /// broken config file (see [`deserialize_group_color`]).
    #[must_use]
    pub fn parse_letter(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "A" => Some(Self::A),
            "B" => Some(Self::B),
            "C" => Some(Self::C),
            "D" => Some(Self::D),
            "E" => Some(Self::E),
            "F" => Some(Self::F),
            "G" => Some(Self::G),
            "H" => Some(Self::H),
            "I" => Some(Self::I),
            _ => None,
        }
    }

    /// Parse a Go `group` value: `""` (or whitespace) is *no group* — `legacy-config-real.json` proves
    /// the Go app wrote `""`, not an absent key. Only Go's own alphabet `a..e` parses: `F..I` are the
    /// port's letters, so a Go file that somehow carries one is not data this app has to honour.
    #[must_use]
    pub fn parse_go(raw: &str) -> Option<Self> {
        let colour = Self::parse_letter(raw)?;
        if matches!(colour, Self::A | Self::B | Self::C | Self::D | Self::E) {
            Some(colour)
        } else {
            None
        }
    }
}

/// Read an optional group colour without ever failing the file.
///
/// `#[serde(rename_all = "UPPERCASE")]` alone would make a lowercase `"a"` — which is exactly what the
/// Go app wrote (`store::legacy::LegacyTabState::group`) — a `Deserialize` **error**, and an error here
/// is not a lost colour: it takes the whole `TabSession`, and with it the whole workspace, down with
/// it. So a value that is not a port letter is `None`, silently, the way an unknown key already is.
pub fn deserialize_group_color<'de, D>(deserializer: D) -> Result<Option<GroupColor>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?
        .as_deref()
        .and_then(GroupColor::parse_letter))
}

/// One open tab of a session.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TabSession {
    /// The Go tab id (`tab_<ms>_<rand>`); opaque, but stable so `activeTab` can point at it.
    pub id: String,
    /// Directory this tab shows — a raw string here because a VFS path (`a.zip::inner`) is legal and
    /// `path::FsPath` validation happens when the tab is restored, not when the file is parsed
    /// (plan §5.4's `Session::open` must never fail on a bad config).
    pub path: String,
    /// Tab title.
    pub name: String,
    /// The colour of this tab's group, if any. Kept because it is Go's spelling of group membership
    /// (`docs/contracts.md` §2.3) and because a session whose `groups` rows were lost still restores
    /// its accents — and, via [`TabGroupSession`]'s absence, its groups.
    #[serde(default, deserialize_with = "deserialize_group_color")]
    pub group: Option<GroupColor>,
    /// Which group this tab belongs to, as the opaque [`TabGroupSession::id`] of the row. `None` is
    /// "no group", and also "every config written before tab groups" — which is why [`Self::group`]
    /// stays authoritative for anything that has no row to look a letter up in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    /// View mode. No Go counterpart (migration table), so it always defaults to `List`.
    pub view: ViewMode,
}

/// One tab group of one pane (`docs/tab-groups-design.md` §4).
///
/// Per pane rather than per workspace because `TabSession.group` is per pane too: the same letter in
/// two panes is two groups there, and a group whose tabs are all closed has no reason to live on.
///
/// `name` is empty for an **unnamed** group, whose chip shows [`GroupColor::name`]; storing the colour's
/// word would leave an unnamed group called "Red group" forever after the user re-coloured it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TabGroupSession {
    /// The group's id, matched against [`TabSession::group_id`]. Opaque and per pane, like a tab id.
    pub id: String,
    /// The label on the chip; empty means "name it after the colour".
    pub name: String,
    /// The chip's colour. `None` (or a letter Go wrote in lowercase) is repaired to the first free
    /// colour on restore rather than dropped.
    #[serde(default, deserialize_with = "deserialize_group_color")]
    pub group: Option<GroupColor>,
    /// Whether the run is folded to just the chip.
    pub collapsed: bool,
}

/// A pane's tab strip.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PaneSession {
    /// Tabs, in order.
    pub tabs: Vec<TabSession>,
    /// Index of the selected tab; `usize::MAX` means "none" (Go's `left_active: ""`), which the UI
    /// clamps rather than trusting.
    pub active: usize,
    /// This pane's tab groups. Not `skip_serializing_if`-free by accident: an empty list is written
    /// nowhere, so a session with no groups serialises byte-for-byte as it did before tab groups.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<TabGroupSession>,
}

/// A named workspace: the whole window state of one task.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Workspace {
    /// Every pane, keyed by slot. Missing slots are empty panes, which is why this is a map and not a
    /// fixed array (migration table: the two bottom slots have no Go source).
    pub panes: BTreeMap<PaneSlot, PaneSession>,
    /// The pane that has keyboard focus.
    pub active: PaneSlot,
    /// Single or split.
    pub layout: LayoutMode,
    /// A `craft_ui` `Workspace<DockId>` snapshot, kept as opaque JSON: `zenman-core` must not depend on
    /// the UI crate (plan §5.1, `xtask deps`), so the dock tree round-trips rather than being modelled.
    pub dock: Option<serde_json::Value>,
}

/// A bookmark row. Go stored `bookmarks: ["/path", …]` — plain strings, no label, no timestamp — so the
/// two extra fields are `None` for everything the migration produces and are here for the port's own
/// edits (plan §5.3 `services/places.rs` "normalize_bookmark").
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Bookmark {
    /// Absolute path, or an archive VFS path.
    pub path: String,
    /// Display label; `None` means "use the file name".
    pub label: Option<String>,
    /// Unix seconds when the user added it; `None` for migrated rows (Go recorded nothing).
    pub added_at: Option<i64>,
}

/// A user-defined action (Go's `custom_actions`). Snake_case on purpose — see the module docs and
/// `tests/golden/actions.json`; this struct is the *same bytes* the Go app wrote.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", default)]
pub struct CustomAction {
    /// Go's 16-hex id.
    pub id: String,
    /// Menu label.
    pub name: String,
    /// Icon name (Go's `icon`).
    pub icon: String,
    /// The template line, with `{file}`/`{files}`/`{dir}`/… substitutions (`services/actions.rs`, step 9).
    pub command: String,
    /// `"files"`, `"dirs"`, `"background"`, `|`-combined (plan §5.3 `matches`).
    pub role: String,
    /// Comma-separated lowercased globs.
    pub patterns: String,
    /// Show the command's output in a dialog.
    pub show_output: bool,
}

/// The indexer's persisted settings: Go's `search-config.json` minus `index_state` (runtime state, not
/// settings — `docs/contracts.md` → *Config and session files*).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct IndexerSettings {
    /// Root directories to index.
    pub roots: Vec<String>,
    /// Glob excludes; an invalid pattern is reported, not ignored (plan §5.3 `compile_excludes`).
    pub excludes: Vec<String>,
    /// Reindex on a timer.
    pub auto_index: bool,
    /// Reindex when the app starts.
    pub index_on_startup: bool,
    /// 0 = unlimited depth (Go's meaning; plan §5.3 `WalkCfg.max_depth`).
    pub max_depth: u32,
    /// Follow symlinks while walking.
    pub follow_symlinks: bool,
    /// Walk threads (Go's `worker_count`; capped by `Limits::concurrent_jobs`).
    pub worker_count: u32,
}

impl Default for IndexerSettings {
    /// Go's own defaults, from `tests/golden/search-config.json` minus the captured roots.
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            excludes: crate::store::legacy::GO_DEFAULT_EXCLUDES
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            auto_index: false,
            index_on_startup: false,
            max_depth: 0,
            follow_symlinks: false,
            worker_count: 2,
        }
    }
}

/// User-overridable subset of [`Limits`].
///
/// Every field is `Option`: `None` inherits the engine's compiled default, which is what keeps
/// `Limits::default()` — and therefore every golden comparison — independent of what a user typed.
/// [`ZenConfig::validate`] checks these against the hard caps in `crate::limits` **before any walk
/// runs**: a `flatWalkCap` of 4_000_000_000 must fail at boot, not after a directory walk has already
/// started (plan §6.1 F8/F12: the caps exist to bound a hostile or mistaken input, and a cap that is
/// checked mid-walk is not a bound).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConfigLimits {
    /// Page size for a directory listing (cap `limits::MAX_PAGE_SIZE`).
    pub dir_page_default: Option<usize>,
    /// Largest page size the UI may ask for.
    pub dir_page_max: Option<usize>,
    /// Flat-walk cap (Go's 10 000; `limits::MAX_WALK_FILES` is the hard ceiling).
    pub flat_walk_cap: Option<usize>,
    /// `track_entries` cap (deviation ledger row 12).
    pub track_entries_max: Option<usize>,
    /// Live-search result cap.
    pub live_search_cap: Option<i64>,
    /// Live-search visited-node cap.
    pub live_search_nodes: Option<usize>,
    /// Archive manifest cache entries.
    pub archive_cache_entries: Option<usize>,
    /// Concurrent extractions (Go's semaphore was a literal 3).
    pub concurrent_extracts: Option<usize>,
    /// Concurrent jobs.
    pub concurrent_jobs: Option<usize>,
    /// Nested-archive byte cap (plan F8).
    pub nested_archive_max_bytes: Option<u64>,
    /// Custom-action output cap.
    pub exec_output_cap: Option<usize>,
    /// Undo-log scan cap.
    pub log_scan_cap: Option<usize>,
    /// Index walk depth.
    pub index_max_depth: Option<usize>,
    /// Index entries per page.
    pub index_max_entries: Option<usize>,
}

/// The settings document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ZenConfig {
    /// Schema stamp; see [`CURRENT_CONFIG_VERSION`] and [`ZenConfig::migrate`].
    pub schema_version: ConfigVersion,
    /// Sidebar bookmarks.
    pub bookmarks: Vec<Bookmark>,
    /// Named workspaces. `BTreeMap` (plan §5.4) so a saved file is byte-stable and the workspace list
    /// is alphabetised without the UI sorting it.
    pub workspaces: BTreeMap<String, Workspace>,
    /// User actions, in menu order.
    pub custom_actions: Vec<CustomAction>,
    /// Indexer settings (migrated from `search-config.json`, not from `config.json`).
    pub indexer: IndexerSettings,
    /// Theme.
    pub theme: ThemeMode,
    /// Delete policy.
    pub delete_strategy: DeleteStrategy,
    /// Confirm every destructive action.
    pub confirm_delete: bool,
    /// What may be persisted between runs.
    pub store_mode: StoreMode,
    /// Show dotfiles (Go never filtered them — deviation ledger row 6 — so `true` is parity).
    pub show_hidden: bool,
    /// Default sort for a new tab.
    pub sort: SortSpec,
    /// List column widths.
    pub column_widths: ColumnWidths,
    /// The shell's UI zoom, in per cent of the native scale: the `− | 100% | +` row of the parent
    /// menu (`zenman-ui`'s `zoom.rs`). One value, two front ends — the shell sets egui's
    /// `zoom_factor` from it, and egui's own <kbd>Ctrl</kbd>+<kbd>+</kbd> chord is read back into it,
    /// so a keyboard zoom survives a restart too. The range is egui's own (`gui_zoom`'s 0.2–5.0),
    /// which is what makes the chord unable to leave a level this file refuses to hold.
    ///
    /// Out of range is a config error rather than a silent clamp, the house rule for every validated
    /// field: a hand-edited `5000` is a mistake the user deserves to hear about. Note the standing
    /// consequence of that rule (`apps/zenman`'s `config_boot`): a file that fails validation boots
    /// on the defaults and the *next* save rewrites it, so a hand-edit is best made with the app
    /// closed and the value inside `zoomPercent`'s own range.
    pub zoom_percent: u32,
    /// Directory page size; `None` = `Limits::dir_page_default`.
    pub page_size: Option<usize>,
    /// Interface language tag (`en-US`), or empty for "follow the system".
    pub language: String,
    /// Rows the undo panel keeps. `None` = **unlimited**, which is the Go behaviour: the action log is
    /// an append-only file the app never truncated (`api/log.go`), and plan **R10** keeps it that way —
    /// the UI paginates instead (the Go frontend did the same: `frontend/js/action-log.js:288` slices
    /// the fetched list rather than the server trimming it). `Some(n)` trims *displayed* history only;
    /// it never deletes log records.
    pub history_limit: Option<usize>,
    /// Overrides applied over [`Limits::default()`] (see [`ConfigLimits`]).
    pub limits: ConfigLimits,
    /// Set by the migration once it has backed the Go file up, so a restart does not re-run the import
    /// (plan §5.4 `store/legacy.rs` row: "writes `config.json.bak` once (guarded by a `migrated_at`
    /// marker)"). Unix seconds.
    pub migrated_at: Option<i64>,
}

impl Default for ZenConfig {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_CONFIG_VERSION,
            bookmarks: Vec::new(),
            workspaces: BTreeMap::new(),
            custom_actions: Vec::new(),
            indexer: IndexerSettings::default(),
            theme: ThemeMode::default(),
            delete_strategy: DeleteStrategy::default(),
            // Go always confirmed a delete (`app.js` confirm dialog); keeping that as the default is
            // the difference between a port and a data-loss report.
            confirm_delete: true,
            store_mode: StoreMode::default(),
            show_hidden: true,
            sort: SortSpec::DEFAULT_SORT,
            column_widths: ColumnWidths::default(),
            zoom_percent: DEFAULT_ZOOM_PERCENT,
            page_size: None,
            language: String::new(),
            history_limit: None,
            limits: ConfigLimits::default(),
            migrated_at: None,
        }
    }
}

impl ZenConfig {
    /// Read `path`, refusing a file this build cannot understand rather than starting with defaults
    /// quietly (which is what Go did — `docs/contracts.md` → *Config and session files*).
    ///
    /// A missing file is [`CoreError::NotFound`] (which [`CoreError::is_expected`] answers `true` for):
    /// the caller turns that into "first run", not an error toast. A *corrupt* file is
    /// [`CoreError::Config`], and the caller is expected to keep the bytes (`.bak`) and boot with
    /// defaults — see `apps/zenman::config_boot`.
    ///
    /// # Errors
    /// [`CoreError::NotFound`] when `path` does not exist; [`CoreError::Io`] for any other read
    /// failure; [`CoreError::Config`] when the JSON is malformed or a value is out of range.
    pub fn load(path: &Path) -> Result<Self, CoreError> {
        let text = read_to_string(path)?;
        serde_json::from_str::<Self>(&text).map_err(|e| CoreError::Config {
            msg: format!("{}: {e}", path.display()),
        })
    }

    /// Write `path` atomically: serialise, write `path.tmp`, `rename` over `path`.
    ///
    /// A half-written settings file was the Go app's failure mode (it wrote `config.json` directly and
    /// had no version key to detect the damage); the `*.tmp` + rename pair is the standard defence and
    /// costs one syscall. The parent directory is created if it is missing — which is a convenience, not
    /// a decision: this module still never *chooses* a directory, it only materialises the one the caller
    /// handed it (`AppPaths` stays the authority, plan §5.2).
    ///
    /// # Errors
    /// [`CoreError::Io`] when the directory cannot be created, the temp file written, synced or
    /// renamed.
    pub fn save(&self, path: &Path) -> Result<(), CoreError> {
        self.validate()?;
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| CoreError::Config {
            msg: format!("cannot serialise the config: {e}"),
        })?;
        let tmp = tmp_path(path);
        if let Some(dir) = path.parent()
            && !dir.exists()
        {
            std::fs::create_dir_all(dir)
                .map_err(|e| CoreError::io(dir.display().to_string(), e))?;
        }
        write_all(&tmp, &bytes).and_then(|()| {
            std::fs::rename(&tmp, path)
                .map_err(|e| CoreError::io(format!("{} -> {}", tmp.display(), path.display()), e))
        })
    }

    /// Check every value that could make the engine walk, allocate or delete too much.
    ///
    /// Called before `Session::open` and before any directory walk (plan §7 step 4 acceptance: the
    /// `limits.*` enforcement happens "before any filesystem walk runs").
    ///
    /// # Errors
    /// [`CoreError::Config`] for a bad schema version, a non-finite or absurd column width, a page size
    /// above [`MAX_PAGE_SIZE`]; [`CoreError::CapacityExceeded`] for any `limits.*` override outside its
    /// compiled ceiling.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.schema_version != CURRENT_CONFIG_VERSION {
            return Err(CoreError::Config {
                msg: format!(
                    "config schema version {} is not {}; run the migration (plan §5.4)",
                    self.schema_version, CURRENT_CONFIG_VERSION
                ),
            });
        }
        let w = &self.column_widths;
        for (name, value) in [
            ("columnWidths.name", w.name),
            ("columnWidths.size", w.size),
            ("columnWidths.date", w.date),
            ("columnWidths.mode", w.mode),
        ] {
            if !value.is_finite() || value <= 0.0 || value > 100_000.0 {
                return Err(CoreError::Config {
                    msg: format!("{name} must be a finite width in 0.0..=100000.0, got {value}"),
                });
            }
        }
        if self.zoom_percent < MIN_ZOOM_PERCENT || self.zoom_percent > MAX_ZOOM_PERCENT {
            return Err(CoreError::Config {
                msg: format!(
                    "zoomPercent must be a percentage in {MIN_ZOOM_PERCENT}..={MAX_ZOOM_PERCENT} \
                     (egui's own zoom range), got {}",
                    self.zoom_percent
                ),
            });
        }
        if let Some(size) = self.page_size
            && (size == 0 || size > MAX_PAGE_SIZE)
        {
            return Err(CoreError::Config {
                msg: format!(
                    "page_size must be 1..={MAX_PAGE_SIZE} (Go's own page cap), got {size}"
                ),
            });
        }
        if self.indexer.max_depth as usize > INDEX_MAX_DEPTH {
            return Err(CoreError::CapacityExceeded {
                what: "index depth (0 means unlimited)",
                limit: INDEX_MAX_DEPTH,
            });
        }
        let l = &self.limits;
        bounded(&l.dir_page_default, "dirPageDefault", 1, MAX_PAGE_SIZE)?;
        bounded(&l.dir_page_max, "dirPageMax", 1, MAX_PAGE_SIZE)?;
        if let (Some(d), Some(m)) = (l.dir_page_default, l.dir_page_max)
            && d > m
        {
            return Err(CoreError::Config {
                msg: format!("limits.dirPageDefault {d} exceeds limits.dirPageMax {m}"),
            });
        }
        bounded(&l.flat_walk_cap, "flatWalkCap", 1, MAX_WALK_FILES)?;
        bounded(
            &l.track_entries_max,
            "trackEntriesMax",
            1,
            TRACK_ENTRIES_MAX,
        )?;
        bounded(
            &l.live_search_nodes,
            "liveSearchNodes",
            1,
            LIVE_SEARCH_NODES,
        )?;
        bounded(
            &l.archive_cache_entries,
            "archiveCacheEntries",
            1,
            ARCHIVE_CACHE_ENTRIES,
        )?;
        bounded(
            &l.concurrent_extracts,
            "concurrentExtracts",
            1,
            CONCURRENT_EXTRACTS,
        )?;
        bounded(&l.concurrent_jobs, "concurrentJobs", 1, CONCURRENT_JOBS)?;
        bounded(&l.exec_output_cap, "execOutputCap", 1, EXEC_OUTPUT_CAP)?;
        bounded(&l.log_scan_cap, "logScanCap", 1, LOG_SCAN_CAP)?;
        bounded(&l.index_max_depth, "indexMaxDepth", 0, INDEX_MAX_DEPTH)?;
        bounded(
            &l.index_max_entries,
            "indexMaxEntries",
            1,
            INDEX_MAX_ENTRIES,
        )?;
        if let Some(cap) = l.live_search_cap
            && !(1..=LIVE_SEARCH_CAP).contains(&cap)
        {
            return Err(CoreError::CapacityExceeded {
                what: "liveSearchCap",
                limit: LIVE_SEARCH_CAP as usize,
            });
        }
        if let Some(bytes) = l.nested_archive_max_bytes
            && bytes > NESTED_ARCHIVE_MAX_BYTES
        {
            return Err(CoreError::CapacityExceeded {
                what: "nestedArchiveMaxBytes",
                limit: NESTED_ARCHIVE_MAX_BYTES as usize,
            });
        }
        Ok(())
    }

    /// Raise this document to [`CURRENT_CONFIG_VERSION`].
    ///
    /// Version 0 means "the Go app wrote this" ([`GO_LEGACY_VERSION`]): the keys are snake_case and
    /// there is no stamp, so the import path (`crate::store::legacy`) rather than this function builds
    /// the fields — `migrate` only stamps the version and applies the v0 → v1 defaults, which is why it
    /// is `self`-consuming: after it, the document is ours.
    ///
    /// A **downgrade is refused**, not truncated: opening a version-2 file with a version-1 build would
    /// silently delete whatever version 2 added on the next save.
    ///
    /// # Errors
    /// [`CoreError::Config`] when `from_version` is above [`CURRENT_CONFIG_VERSION`].
    pub fn migrate(self, from_version: ConfigVersion) -> Result<Self, CoreError> {
        if from_version > CURRENT_CONFIG_VERSION {
            return Err(CoreError::Config {
                msg: format!(
                    "refusing to downgrade the config: it is version {from_version}, this build writes version {CURRENT_CONFIG_VERSION}"
                ),
            });
        }
        let mut cfg = self;
        // v0 -> v1: the schema stamp, plus the fields the Go file never had. Everything else was
        // already defaulted by `#[serde(default)]` when the legacy document was parsed.
        cfg.schema_version = CURRENT_CONFIG_VERSION;
        cfg.validate()?;
        Ok(cfg)
    }

    /// The engine limits this config asks for: [`Limits::default()`] with the [`ConfigLimits`]
    /// overrides applied on top. The overrides are already validated (see [`validate`][Self::validate]),
    /// which is what lets this return a plain value instead of a `Result`.
    #[must_use]
    pub fn limits(&self) -> Limits {
        let base = Limits::default();
        let l = &self.limits;
        Limits {
            // `page_size` is the plain user setting, `limits.dirPageDefault` the power-user override; the
            // override wins because it lives in the block that is explicitly about ceilings.
            dir_page_default: l
                .dir_page_default
                .or(self.page_size)
                .unwrap_or(base.dir_page_default),
            dir_page_max: l.dir_page_max.unwrap_or(base.dir_page_max),
            flat_walk_cap: l.flat_walk_cap.unwrap_or(base.flat_walk_cap),
            track_entries_max: l.track_entries_max.unwrap_or(base.track_entries_max),
            live_search_cap: l.live_search_cap.unwrap_or(base.live_search_cap),
            live_search_nodes: l.live_search_nodes.unwrap_or(base.live_search_nodes),
            archive_cache_entries: l
                .archive_cache_entries
                .unwrap_or(base.archive_cache_entries),
            concurrent_extracts: l.concurrent_extracts.unwrap_or(base.concurrent_extracts),
            concurrent_jobs: l.concurrent_jobs.unwrap_or(base.concurrent_jobs),
            nested_archive_max_bytes: l
                .nested_archive_max_bytes
                .unwrap_or(base.nested_archive_max_bytes),
            exec_output_cap: l.exec_output_cap.unwrap_or(base.exec_output_cap),
            log_scan_cap: l.log_scan_cap.unwrap_or(base.log_scan_cap),
            index_max_depth: l.index_max_depth.unwrap_or(base.index_max_depth),
            index_max_entries: l.index_max_entries.unwrap_or(base.index_max_entries),
            ..base
        }
    }
}

/// `path` + `.tmp`, the rename source of [`ZenConfig::save`].
#[must_use]
pub fn tmp_path(path: &Path) -> std::path::PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    name.push_str(".tmp");
    path.with_file_name(name)
}

/// Check `value` against `[min, max]` when it is set. Above `max` is the cap failure the engine exists
/// to catch; below `min` (a 0 page size, a 0-way extract semaphore) is a plain bad value, and the two
/// get different errors because the remediation differs — lower it, versus do not start at all.
fn bounded(
    value: &Option<usize>,
    what: &'static str,
    min: usize,
    max: usize,
) -> Result<(), CoreError> {
    match value {
        None => Ok(()),
        Some(v) if *v < min => Err(CoreError::Config {
            msg: format!("limits.{what} must be at least {min}, got {v}"),
        }),
        Some(v) if *v > max => Err(CoreError::CapacityExceeded { what, limit: max }),
        Some(_) => Ok(()),
    }
}

/// `read_to_string` that reports `NotFound` for a missing file (the caller must not confuse "no config
/// yet" with "unreadable config").
fn read_to_string(path: &Path) -> Result<String, CoreError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(CoreError::NotFound {
            path: path.display().to_string(),
            source_path: None,
        }),
        Err(e) => Err(CoreError::io(path.display().to_string(), e)),
    }
}

/// Write with the error shaped for the caller's path.
fn write_all(path: &Path, bytes: &[u8]) -> Result<(), CoreError> {
    std::fs::write(path, bytes).map_err(|e| CoreError::io(path.display().to_string(), e))
}

/// Every Go key path the importer consumes, in the normalised form `legacy::walk_go_key_paths` emits.
/// Kept beside [`UNMAPPED_GO_KEYS`] so the two lists are reviewed together: a new Go key must land in
/// exactly one of them (the step-4 test walks the real fixtures and fails otherwise).
pub const MAPPED_GO_KEY_PATHS: &[&str] = &[
    "config.json: bookmarks",
    // Leaves a capture can have when the container is empty (`"workspaces":{}`, `"custom_actions":[]`).
    "config.json: workspaces",
    "config.json: custom_actions",
    "config.json: workspaces.<name>.left_tabs",
    "config.json: workspaces.<name>.right_tabs",
    "config.json: workspaces.<name>.left_tabs[].id",
    "config.json: workspaces.<name>.left_tabs[].path",
    "config.json: workspaces.<name>.left_tabs[].name",
    "config.json: workspaces.<name>.left_tabs[].group",
    "config.json: workspaces.<name>.left_active",
    "config.json: workspaces.<name>.right_tabs[].id",
    "config.json: workspaces.<name>.right_tabs[].path",
    "config.json: workspaces.<name>.right_tabs[].name",
    "config.json: workspaces.<name>.right_tabs[].group",
    "config.json: workspaces.<name>.right_active",
    "config.json: workspaces.<name>.split",
    "config.json: custom_actions[].id",
    "config.json: custom_actions[].name",
    "config.json: custom_actions[].icon",
    "config.json: custom_actions[].command",
    "config.json: custom_actions[].role",
    "config.json: custom_actions[].patterns",
    "config.json: custom_actions[].show_output",
    // `search-config.json` (the indexer's own file, migration table row `indexer`).
    "search-config.json: schema_version",
    "search-config.json: roots",
    "search-config.json: excludes",
    "search-config.json: auto_index",
    "search-config.json: index_on_startup",
    "search-config.json: max_depth",
    "search-config.json: follow_symlinks",
    "search-config.json: worker_count",
];

/// Go keys the migration **does not** carry over, each with the reason. This is the honesty list the
/// step-4 acceptance criterion asks for: a key here is a decision a reviewer can disagree with, a key
/// that is simply gone is a bug.
pub const UNMAPPED_GO_KEYS: &[(&str, &str)] = &[
    (
        "config.json: workspaces.<name>.left_tabs[].color",
        "The migration table drops it deliberately: the group letter is the single source of truth \
         (`GroupColor::color()`, §2.3), and keeping Go's per-tab hex would leave two stores that can \
         disagree. The loss is reported in `MigrationReport::skipped`, not hidden.",
    ),
    (
        "config.json: workspaces.<name>.right_tabs[].color",
        "Same reason as the left pane's `color`.",
    ),
    (
        "search-config.json: index_state.last_indexed_unix",
        "Runtime state, not a setting: `docs/contracts.md` records that the `/api/index/status` copy of \
         the config is a 5-field subset, so this value is a cache stamp the Rust index rebuilds \
         (plan §5.4 `index/rebuild.rs`) recomputes. Kept here so the fixture walk stays honest.",
    ),
    (
        "search-config.json: index_state.total_files",
        "Same as `last_indexed_unix`: derived row count, and the port's own count is authoritative once \
         it has walked the tree.",
    ),
    (
        "search-config.json: index_state.is_dirty",
        "Same class. Inheriting a Go `dirty` flag would make the port rebuild on first start for a \
         reason the user cannot see; the Rust index's dirtiness is its own state.",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zenman-config-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn sample() -> ZenConfig {
        let mut cfg = ZenConfig::default();
        cfg.bookmarks.push(Bookmark {
            path: "/tmp/zen".into(),
            label: Some("zen".into()),
            added_at: None,
        });
        let mut tabs = PaneSession::default();
        tabs.tabs.push(TabSession {
            id: "t1".into(),
            path: "/tmp".into(),
            name: "tmp".into(),
            group: Some(GroupColor::A),
            group_id: None,
            view: ViewMode::List,
        });
        tabs.active = 0;
        let mut ws = Workspace::default();
        ws.panes.insert(PaneSlot::Left, tabs);
        ws.layout = LayoutMode::Split;
        cfg.workspaces.insert("dev".into(), ws);
        cfg.custom_actions.push(CustomAction {
            id: "e6f7c823c3f57bb9".into(),
            name: "Fixture echo".into(),
            icon: "terminal".into(),
            command: "printf %s {name}".into(),
            role: "files".into(),
            patterns: "*.txt".into(),
            show_output: true,
        });
        cfg
    }

    #[test]
    fn default_config_validates_and_round_trips() {
        let cfg = sample();
        cfg.validate().expect("the sample is valid");
        let json = serde_json::to_string(&cfg).expect("serialisable");
        let back: ZenConfig = serde_json::from_str(&json).expect("deserialisable");
        assert_eq!(cfg, back);
    }

    #[test]
    fn the_wire_names_are_camel_case_except_custom_actions() {
        let json = serde_json::to_value(sample()).expect("value");
        for key in [
            "schemaVersion",
            "bookmarks",
            "workspaces",
            "columnWidths",
            "deleteStrategy",
            "storeMode",
            "historyLimit",
            "migratedAt",
        ] {
            assert!(json.get(key).is_some(), "missing {key} in {json}");
        }
        let action = &json["customActions"][0];
        // The 1:1 promise (`tests/golden/actions.json`) beats the camelCase convention here.
        assert!(action.get("show_output").is_some(), "{action}");
        assert!(action.get("showOutput").is_none());
        assert!(
            json["workspaces"]["dev"]["panes"]["left"]["tabs"][0]
                .get("view")
                .is_some()
        );
    }

    #[test]
    fn save_is_atomic_and_load_reads_it_back() {
        let dir = tmp_dir("save");
        let path = dir.join("config.json");
        let cfg = sample();
        cfg.save(&path).expect("saved");
        // The temp name is gone, the real name exists, and there is exactly one stray file.
        assert!(path.is_file());
        assert!(!tmp_path(&path).exists());
        assert_eq!(ZenConfig::load(&path).expect("loads"), cfg);
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .expect("readable")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["config.json".to_string()]);
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn a_missing_file_is_not_found_not_a_corrupt_config() {
        let dir = tmp_dir("missing");
        assert!(matches!(
            ZenConfig::load(&dir.join("nope.json")),
            Err(CoreError::NotFound { .. })
        ));
    }

    #[test]
    fn a_corrupt_file_is_a_config_error_the_caller_can_distinguish() {
        let dir = tmp_dir("corrupt");
        let path = dir.join("config.json");
        std::fs::write(&path, b"{\"bookmarks\": [").expect("written");
        assert!(matches!(
            ZenConfig::load(&path),
            Err(CoreError::Config { .. })
        ));
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn limits_are_enforced_before_any_walk_and_the_default_config_is_the_engine_default() {
        let cfg = ZenConfig::default();
        assert_eq!(cfg.limits(), Limits::default());
        let mut hot = ZenConfig::default();
        // 10 005 files is Go's own cap (`dir-flat-cap.json`); 10 001 walks past it.
        hot.limits.flat_walk_cap = Some(MAX_WALK_FILES + 1);
        assert!(matches!(
            hot.validate(),
            Err(CoreError::CapacityExceeded {
                what: "flatWalkCap",
                ..
            })
        ));
        let mut zero = ZenConfig::default();
        zero.limits.concurrent_extracts = Some(0);
        assert!(matches!(zero.validate(), Err(CoreError::Config { .. })));
        let mut inverted = ZenConfig::default();
        inverted.limits.dir_page_default = Some(900);
        inverted.limits.dir_page_max = Some(10);
        assert!(matches!(inverted.validate(), Err(CoreError::Config { .. })));
        let sized = ZenConfig {
            page_size: Some(MAX_PAGE_SIZE + 1),
            ..Default::default()
        };
        assert!(matches!(sized.validate(), Err(CoreError::Config { .. })));
        // And an in-range override actually reaches `Limits`.
        let mut tuned = ZenConfig::default();
        tuned.limits.flat_walk_cap = Some(500);
        tuned.page_size = Some(42);
        tuned.validate().expect("in range");
        assert_eq!(tuned.limits().flat_walk_cap, 500);
        assert_eq!(tuned.limits().page_size(Some(7)), 7);
        assert_eq!(
            tuned.limits().page_size(None),
            42,
            "the config's own page size is the default when the caller asks for none"
        );
        assert_eq!(Limits::default().flat_walk_cap, MAX_WALK_FILES);
    }

    #[test]
    fn migrate_stamps_v0_and_refuses_a_downgrade() {
        let legacy = ZenConfig {
            schema_version: GO_LEGACY_VERSION,
            ..Default::default()
        };
        assert!(
            legacy.validate().is_err(),
            "an unstamped file must not validate as current"
        );
        let moved = legacy
            .migrate(GO_LEGACY_VERSION)
            .expect("v0 upgrades to v1");
        assert_eq!(moved.schema_version, CURRENT_CONFIG_VERSION);
        let newer = CURRENT_CONFIG_VERSION + 1;
        assert!(
            matches!(ZenConfig::default().migrate(newer), Err(CoreError::Config { msg })
                if msg.contains("downgrade")),
            "a version-2 file must never be opened as version 1"
        );
    }

    #[test]
    fn history_limit_defaults_to_unlimited_go_keeps_everything() {
        let cfg = ZenConfig::default();
        assert_eq!(
            cfg.history_limit, None,
            "plan R10: Go keeps the full log; the UI paginates"
        );
        let json = serde_json::to_value(&cfg).expect("value");
        // Absent rather than `null`: `None` is "unlimited", and a literal null would re-read as 0 rows.
        assert!(json.get("historyLimit").is_none() || json["historyLimit"].is_null());
        // A JSON file that omits the key still reads back as `None`.
        let back: ZenConfig = serde_json::from_str("{}").expect("all defaults");
        assert_eq!(back.history_limit, None);
        assert_eq!(back.sort, SortSpec::DEFAULT_SORT);
        assert!(back.confirm_delete);
    }

    #[test]
    fn group_colour_only_pauses_on_the_captured_value() {
        assert_eq!(GroupColor::A.color(), Some("#ff4a4a"));
        assert_eq!(GroupColor::B.color(), None);
        assert_eq!(GroupColor::parse_go(""), None);
        assert_eq!(GroupColor::parse_go("a"), Some(GroupColor::A));
        assert_eq!(GroupColor::parse_go("z"), None);
    }

    #[test]
    fn the_alphabet_went_past_go_and_the_disk_still_holds_letters() {
        assert_eq!(GroupColor::ALL.len(), 9, "red..teal");
        assert_eq!(
            GroupColor::ALL.last().copied(),
            Some(GroupColor::I),
            "the menu walks ALL, so its order is the palette order"
        );
        assert_eq!(GroupColor::F.name(), "Orange");
        assert_eq!(GroupColor::F.label(), "Orange Group", "Go's spelling");
        // A..E are Go's; F..I are the port's, and Go must not be able to produce them.
        assert_eq!(GroupColor::parse_go("f"), None, "no such group in Go");
        assert_eq!(GroupColor::parse_letter("i"), Some(GroupColor::I));
        // The on-disk spelling stays the letter, not the colour word — Go's `group` key reads it.
        let json = serde_json::to_value(GroupColor::F).expect("a letter");
        assert_eq!(json, serde_json::json!("F"));
        assert_eq!(
            GroupColor::color(GroupColor::F),
            None,
            "the port's letters have no captured Go hex to lie with"
        );
    }

    #[test]
    fn a_foreign_group_letter_costs_the_colour_and_not_the_session() {
        // The bug this guards: `rename_all = "UPPERCASE"` alone makes Go's lowercase `"a"` a
        // Deserialize *error*, and an error here would drop the whole workspace at load.
        let raw = r#"{"workspaces":{"default":{"panes":{"left":{"tabs":[
             {"id":"t1","path":"/tmp","name":"tmp","group":"a"},
             {"id":"t2","path":"/var","name":"var","group":"Z"},
             {"id":"t3","path":"/opt","name":"opt","group":null}]}}}}}"#;
        let cfg: ZenConfig = serde_json::from_str(raw).expect("the file still reads");
        let tabs = &cfg.workspaces["default"].panes[&PaneSlot::Left].tabs;
        assert_eq!(tabs.len(), 3, "no tab was lost to a bad letter");
        assert_eq!(tabs[0].group, Some(GroupColor::A), "Go's lowercase letter");
        assert_eq!(tabs[1].group, None, "an unknown letter is no colour");
        assert_eq!(tabs[2].group, None, "and so is an explicit null");
        assert_eq!(tabs[1].group_id, None, "and nothing was invented for it");
    }

    #[test]
    fn a_group_round_trips_and_no_group_costs_no_bytes() {
        let mut pane = PaneSession::default();
        pane.tabs.push(TabSession {
            id: "t1".into(),
            path: "/tmp".into(),
            name: "tmp".into(),
            group: Some(GroupColor::F),
            group_id: Some("g1".into()),
            view: ViewMode::List,
        });
        pane.active = 0;
        pane.groups.push(TabGroupSession {
            id: "g1".into(),
            name: String::new(),
            group: Some(GroupColor::F),
            collapsed: true,
        });
        let value = serde_json::to_value(&pane).expect("a pane writes");
        assert_eq!(value["groups"][0]["collapsed"], serde_json::json!(true));
        assert_eq!(value["groups"][0]["group"], serde_json::json!("F"));
        assert_eq!(
            value["tabs"][0]["groupId"],
            serde_json::json!("g1"),
            "camelCase"
        );
        let back: PaneSession = serde_json::from_value(value).expect("and reads");
        assert_eq!(back, pane, "byte-for-byte through the file format");

        // An empty pane list and an ungrouped tab write **nothing extra**, so a session that never had
        // a group serialises exactly as it did before the feature existed.
        let bare = PaneSession {
            tabs: vec![TabSession {
                id: "t1".into(),
                path: "/tmp".into(),
                name: "tmp".into(),
                group: None,
                group_id: None,
                view: ViewMode::List,
            }],
            active: 0,
            groups: Vec::new(),
        };
        let value = serde_json::to_value(&bare).expect("a pane writes");
        assert!(value.get("groups").is_none(), "no groups key: {value:#?}");
        assert!(value["tabs"][0].get("groupId").is_none(), "no groupId key");
        // And the inverse: a file written before tab groups still reads.
        let old: PaneSession =
            serde_json::from_str(r#"{"tabs":[],"active":18446744073709551615}"#).expect("old file");
        assert!(old.groups.is_empty());
    }
}
