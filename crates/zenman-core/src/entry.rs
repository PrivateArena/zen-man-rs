//! Directory entries — the wire type and the value types that produce it (plan §5.2 `entry.rs` row).
//!
//! The Go reference is `api/dir.go:17-37` (`FileEntry` + `DirResponse`) and
//! `api/archive_vfs.go:13-32` (`VirtualNode.ToFileEntry`). Three things in this module are *not* a
//! transcription of Go, and each is a bug fix the plan records in §2.4 / §6.2:
//!
//! * [`FileEntry::files_count`] is `Option<u64>` and **always serialised** (Go had `int64` with
//!   `omitempty`, so the field vanished unless `folder_sizes` had a cache hit — the root cause of the
//!   frontend's `(undefined files)`, `frontend/js/file-list.js:159-165`, `docs/contracts.md` §2.4 row
//!   *`files_count`*);
//! * [`FileEntry::is_dir`] follows plan F5 — `kind.is_dir() || kind.is_symlink_to_dir()` — where Go used
//!   the `lstat` bit and so showed a symlinked directory as a file you could `cd` into
//!   (`docs/contracts.md` *Deviation ledger* #5, §6.2 row 2);
//! * [`FileEntry::mod_time`] is `Option<DateTime<Utc>>`; Go's zero `time.Time` reached the wire as
//!   `-62135596800` next to `mode:"unknown"` and `size:0` (`dir-perm-noexec.json`), which is plan F20's
//!   "no mtime at all" and is what [`EntryLike::mtime_secs`]'s `None` means.
//!
//! # The sealed `EntryLike`
//!
//! Four producers exist and no fifth is wanted, so [`EntryLike`] is sealed (private supertrait):
//!
//! | type | Go equivalent | who builds it |
//! |---|---|---|
//! | [`DirEntryLike`] | `os.DirEntry` (`entry.Name()`, `entry.Type()`, `entry.IsDir()`) | `ops/list.rs` (step 9) walking a directory |
//! | [`InfoEntry`] | `fs.FileInfo` (`entry.Info()` — the half that can fail, `api/dir.go:236`) | the same walk, plus `archive` stats |
//! | [`EntryStat`] | the pair, keyed by a store path | `Session::props`-equivalent, `ops::*` results |
//! | [`VirtualNode`] | `archive_vfs.go:13-20` | `archive/vfs.rs` (step 13) |
//!
//! [`EntryBuilder`] is the single place the wire row is derived from one of them.

use std::borrow::Cow;
use std::path::Path;

use chrono::{DateTime, TimeZone as _, Utc};
use serde::{Deserialize, Serialize};

use crate::cursor::Cursor;
use crate::metrics::GO_ZERO_TIME_UNIX;
use crate::mode::ModeKind;
use crate::path::FsPath;

/// The seal. `EntryLike` is implemented by exactly the four producers in this module, which is what
/// lets [`ModeKind::from_entry`] and [`EntryBuilder`] treat "no unix mode" as "the platform has no
/// `st_mode` or the stat failed" instead of guessing.
mod sealed {
    /// Implemented once per entry producer, inside this module.
    pub trait Sealed {}
}

/// How far the type of an entry is known, link-to-directory distinguished.
///
/// Go had two booleans here and got the second one wrong: `entry.IsDir()` is `lstat`-based, so a
/// symlink to a directory reported `is_dir:false` while `open` happily followed it
/// (`docs/contracts.md` *Deviation ledger* #5). One enum with a `SymlinkDir` variant makes that state
/// nameable, which is what plan F5's rule needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// A regular file — or anything this enum does not classify (FIFO, socket, device: the *mode
    /// string* distinguishes those, [`crate::mode::ModeKind`] does).
    File,
    /// A directory.
    Dir,
    /// A symlink whose target is not a directory.
    Symlink,
    /// A symlink to a directory: `L…` in the mode string, `is_dir:true` on the wire.
    SymlinkDir,
}

impl EntryKind {
    /// `true` only for a real directory (`drwxr-xr-x`).
    #[must_use]
    pub const fn is_dir(self) -> bool {
        matches!(self, Self::Dir)
    }

    /// `true` for either symlink variant.
    #[must_use]
    pub const fn is_symlink(self) -> bool {
        matches!(self, Self::Symlink | Self::SymlinkDir)
    }

    /// `true` only for [`Self::SymlinkDir`] — the state Go could not express.
    #[must_use]
    pub const fn is_symlink_to_dir(self) -> bool {
        matches!(self, Self::SymlinkDir)
    }

    /// Plan F5: the rule behind [`FileEntry::is_dir`] — a directory *or a link to one*, which is what
    /// "can this row be entered" means to the pane.
    #[must_use]
    pub const fn is_navigable_dir(self) -> bool {
        matches!(self, Self::Dir | Self::SymlinkDir)
    }

    /// From the `lstat` file type plus the caller's follow-the-link answer. `target_is_dir` is only
    /// consulted for a symlink; the caller resolved it (`std::fs::metadata` on Unix, Go's `os.Stat`).
    #[must_use]
    pub fn from_file_type(file_type: &std::fs::FileType, target_is_dir: bool) -> Self {
        if file_type.is_symlink() {
            if target_is_dir {
                return Self::SymlinkDir;
            }
            return Self::Symlink;
        }
        if file_type.is_dir() {
            return Self::Dir;
        }
        Self::File
    }

    /// From a raw `st_mode` (type bits included) plus the follow-the-link answer — the `#[cfg(unix)]`
    /// producer; [`crate::mode::ModeKind::from_unix`] does the same job for the mode string.
    #[must_use]
    pub const fn from_unix_mode(st_mode: u32, target_is_dir: bool) -> Self {
        let ty = st_mode & 0o170000;
        if ty == 0o120000 {
            if target_is_dir {
                return Self::SymlinkDir;
            }
            return Self::Symlink;
        }
        if ty == 0o040000 {
            return Self::Dir;
        }
        Self::File
    }
}

/// The read-only view the mode string and the wire row are derived from (plan §5.2 `mode.rs` row:
/// `ModeKind::from_entry(&impl EntryLike)`).
///
/// Deliberately tiny and `cfg`-neutral: [`EntryLike::unix_mode`] is `None` on a Windows build *or* when
/// the stat failed (Go's `entry.Info()` error → `"unknown"`, `docs/contracts.md` → *fileMode strings*),
/// and [`EntryLike::windows_attributes`] is `None` everywhere else.
pub trait EntryLike: sealed::Sealed {
    /// The entry's name, without its parent.
    fn file_name(&self) -> &str;
    /// The entry's kind, link-to-directory distinguished.
    fn kind(&self) -> EntryKind;
    /// Byte size of the entry (Go's `info.Size()`, which is the *dirent* size when the stat failed: 0).
    fn size(&self) -> i64;
    /// Modification time in Unix seconds, `None` when the stat failed (plan F20).
    fn mtime_secs(&self) -> Option<i64>;
    /// The raw `st_mode` when the platform produced one and the stat worked.
    fn unix_mode(&self) -> Option<u32>;
    /// Windows file attributes, when the platform produced them (step 16).
    fn windows_attributes(&self) -> Option<u32> {
        None
    }
}

// --------------------------------------------------------------------------- producers

/// Go's `os.DirEntry`: the name and the `lstat` type, without a second stat.
///
/// On its own it can answer `name` and `is_dir` but nothing else, so `unix_mode()` is `None` and
/// [`ModeKind::from_entry`] answers `unknown` — which is precisely the degraded row
/// (`dir-perm-noexec.json`), where Go had a name from the readdir and a failed `Info()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntryLike {
    /// The entry name (Go's `entry.Name()`).
    pub name: String,
    /// The kind, with the link-to-directory case resolved by the builder.
    pub kind: EntryKind,
}

impl DirEntryLike {
    /// One name + kind pair.
    #[must_use]
    pub fn new(name: impl Into<String>, kind: EntryKind) -> Self {
        Self {
            name: name.into(),
            kind,
        }
    }

    /// Go's degraded row: `api/dir.go:235-247` leaves `mode = "unknown"`, `size = 0`,
    /// `mod_time = -62135596800` when `entry.Info()` fails.
    #[must_use]
    pub fn degraded(name: impl Into<String>) -> Self {
        Self::new(name, EntryKind::File)
    }
}

impl sealed::Sealed for DirEntryLike {}

impl EntryLike for DirEntryLike {
    fn file_name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> EntryKind {
        self.kind
    }
    fn size(&self) -> i64 {
        0
    }
    fn mtime_secs(&self) -> Option<i64> {
        None
    }
    fn unix_mode(&self) -> Option<u32> {
        None
    }
}

/// Go's `fs.FileInfo` — the half of a directory read that can fail (`entry.Info()`, `api/dir.go:236`).
///
/// This is the type a real `std::fs::Metadata` becomes; everything else in the entry comes from here
/// (`size`, `mod_time`, `mode`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InfoEntry {
    /// The entry name.
    pub name: String,
    /// The kind: `lstat` type plus the caller's follow answer.
    pub kind: EntryKind,
    /// `info.Size()` (`std::fs::Metadata::len()`).
    pub size: i64,
    /// Unix seconds, `None` = the stat failed.
    pub mtime: Option<i64>,
    /// Raw `st_mode`, `None` off-Unix or when the stat failed.
    pub unix_mode: Option<u32>,
    /// Windows file attributes; `None` on every platform today except step 16.
    pub windows_attributes: Option<u32>,
}

impl InfoEntry {
    /// A successfully-statted entry.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        kind: EntryKind,
        size: i64,
        mtime: Option<i64>,
        unix_mode: Option<u32>,
    ) -> Self {
        Self {
            name: name.into(),
            kind,
            size,
            mtime,
            unix_mode,
            windows_attributes: None,
        }
    }

    /// The failed-`Info()` entry: same name, no size, no mtime, no mode ([`ModeKind::Unknown`]).
    #[must_use]
    pub fn degraded(name: impl Into<String>, kind: EntryKind) -> Self {
        Self::new(name, kind, 0, None, None)
    }

    /// From a `std::fs::Metadata` (the *followed* one for a symlink) — the `#[cfg(unix)]` producer.
    /// `link_type` is the `lstat` file type, which is what keeps `is_symlink` true while `is_dir`
    /// follows the target (plan F5).
    #[must_use]
    pub fn from_metadata(
        name: impl Into<String>,
        link_type: &std::fs::FileType,
        meta: &std::fs::Metadata,
    ) -> Self {
        let kind = EntryKind::from_file_type(link_type, meta.is_dir());
        Self::new(
            name,
            kind,
            i64::try_from(meta.len()).unwrap_or(i64::MAX),
            meta.modified().map(system_time_to_unix).ok(),
            unix_st_mode(meta),
        )
    }
}

impl sealed::Sealed for InfoEntry {}

impl EntryLike for InfoEntry {
    fn file_name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> EntryKind {
        self.kind
    }
    fn size(&self) -> i64 {
        self.size
    }
    fn mtime_secs(&self) -> Option<i64> {
        self.mtime
    }
    fn unix_mode(&self) -> Option<u32> {
        self.unix_mode
    }
    fn windows_attributes(&self) -> Option<u32> {
        self.windows_attributes
    }
}

/// The `lstat`/`stat` pair keyed by a store path: what the Properties dialog and every `ops::*` result
/// carries (plan §2.4 row *`/api/props`*: "Properties must be built from `page_dir` + `dir_size` +
/// [`EntryStat`]").
///
/// `dirent` is the name and type the directory listing produced; `info` is the follow-the-link stat
/// when there was one. `info: None` is the degraded row again — a name we may see but may not `stat`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryStat {
    /// Where the entry is, in store coordinates.
    pub path: FsPath,
    /// The root `rel_path` is computed against — the *listing* root, which for a local listing is the
    /// directory that was listed (plan §4.2's `Rel` rule; `dir-flat-subdir.json` shows `rel_path = "b"`,
    /// not the store-relative tail).
    pub rel_base: FsPath,
    /// The `os.DirEntry` half.
    pub dirent: DirEntryLike,
    /// The `fs.FileInfo` half, `None` when the stat failed.
    pub info: Option<InfoEntry>,
}

impl EntryStat {
    /// A stat with no listing context (`rel_path` then comes out empty).
    #[must_use]
    pub fn standalone(path: FsPath, dirent: DirEntryLike, info: Option<InfoEntry>) -> Self {
        let rel_base = path.store_root();
        Self {
            path,
            rel_base,
            dirent,
            info,
        }
    }

    /// One entry of a listing whose root is `rel_base`.
    #[must_use]
    pub fn in_listing(
        path: FsPath,
        rel_base: FsPath,
        dirent: DirEntryLike,
        info: Option<InfoEntry>,
    ) -> Self {
        Self {
            path,
            rel_base,
            dirent,
            info,
        }
    }

    /// Go's degraded row: the name is known, nothing else is (`dir-perm-noexec.json`).
    #[must_use]
    pub fn degraded(path: FsPath, name: impl Into<String>) -> Self {
        let name = name.into();
        let dirent = DirEntryLike::degraded(name.clone());
        let info = InfoEntry::degraded(name, EntryKind::File);
        // The name came from a listing, so the listing root is the parent: `rel_path` is the name, which
        // is the coordinate the degraded row of `dir-perm-noexec.json` belongs to.
        let rel_base = path.parent().unwrap_or_else(|| path.store_root());
        Self::in_listing(path, rel_base, dirent, Some(info))
    }

    /// The kind, preferring the stat (which knows whether a link was followed) over the dirent.
    #[must_use]
    pub fn kind(&self) -> EntryKind {
        self.info.as_ref().map_or(self.dirent.kind, |i| i.kind)
    }

    /// The `rel_path` this stat will produce.
    #[must_use]
    pub fn rel_path(&self) -> String {
        self.path.relative_to(&self.rel_base).unwrap_or_default()
    }
}

impl sealed::Sealed for EntryStat {}

impl EntryLike for EntryStat {
    fn file_name(&self) -> &str {
        self.dirent.file_name()
    }
    fn kind(&self) -> EntryKind {
        EntryStat::kind(self)
    }
    fn size(&self) -> i64 {
        self.info.as_ref().map_or(0, EntryLike::size)
    }
    fn mtime_secs(&self) -> Option<i64> {
        self.info.as_ref().and_then(EntryLike::mtime_secs)
    }
    fn unix_mode(&self) -> Option<u32> {
        self.info.as_ref().and_then(EntryLike::unix_mode)
    }
    fn windows_attributes(&self) -> Option<u32> {
        self.info.as_ref().and_then(EntryLike::windows_attributes)
    }
}

/// One archive member — Go's `VirtualNode` (`api/archive_vfs.go:13-20`), which the step-13
/// `archive/vfs.rs` manifest owns and this module gives a shape to.
///
/// An archive member has no `st_mode`, so its mode string is the **hardcoded** `-r--r--r--`
/// (`archive_vfs.go:29`, `ArchiveMode`): every `archive-list-*.json` row carries it, directories
/// included. `docs/contracts.md` §2.4 row *Archive listing* also records that Go lost the symlink bit,
/// which is why the kind is an [`EntryKind`] and not a bool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualNode {
    /// Member name, without the parent directory.
    pub name: String,
    /// `Dir` / `File` / `Symlink` as the reader's mode bits say (Go only stored `IsDir`).
    pub kind: EntryKind,
    /// Uncompressed size in bytes.
    pub size: i64,
    /// Unix seconds from the container header.
    pub mod_time: Option<i64>,
    /// The clean slash path inside the archive — the wire `rel_path` (`archive_vfs.go:31`).
    pub internal_path: String,
    /// Child `internal_path`s of a directory member (`archive_vfs.go:19`); empty for a file.
    pub children: Vec<String>,
}

impl VirtualNode {
    /// Go's `VirtualNode.IsDir`.
    #[must_use]
    pub const fn is_dir(&self) -> bool {
        self.kind.is_navigable_dir()
    }

    /// `to_entry()` of plan §5.5's `archive/vfs.rs` row — the port of `ToFileEntry`
    /// (`archive_vfs.go:23-32`): `mode = "-r--r--r--"` hardcoded, `rel_path = internal_path`, no
    /// `depth`, no `files_count`, and `is_symlink` only when the reader actually found a link member
    /// (Go never reported one — `docs/contracts.md` §2.4 row *Archive listing*).
    #[must_use]
    pub fn to_file_entry(&self) -> FileEntry {
        FileEntry {
            name: self.name.clone(),
            is_dir: self.kind.is_navigable_dir(),
            is_symlink: self.kind.is_symlink(),
            size: self.size,
            mod_time: self.mod_time.and_then(unix_to_datetime),
            mode: ModeKind::Archive.to_string(),
            rel_path: self.internal_path.clone(),
            depth: 0,
            files_count: None,
        }
    }
}

impl sealed::Sealed for VirtualNode {}

impl EntryLike for VirtualNode {
    fn file_name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> EntryKind {
        self.kind
    }
    fn size(&self) -> i64 {
        self.size
    }
    fn mtime_secs(&self) -> Option<i64> {
        self.mod_time
    }
    fn unix_mode(&self) -> Option<u32> {
        None
    }
}

/// `SystemTime` → Unix seconds, with Go's zero `time.Time` standing in for "unknown" (`None`).
///
/// The fallback is deliberate: the *wire* type has no way to say "no mtime" other than the sentinel, so
/// a filesystem with no mtime must not serialise `1970-01-01`.
#[must_use]
pub fn system_time_to_unix(t: std::time::SystemTime) -> i64 {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => -(i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX)),
    }
}

/// The raw `st_mode` of a `std::fs::Metadata` — Unix only; other platforms have no `st_mode` and get
/// `None`, which is what makes [`ModeKind::from_entry`] answer `unknown` honestly rather than guess.
#[cfg(unix)]
#[must_use]
pub fn unix_st_mode(meta: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt as _;
    Some(meta.permissions().mode())
}

/// Non-Unix: there is no `st_mode`; step 16 fills [`InfoEntry::windows_attributes`] instead.
#[cfg(not(unix))]
#[must_use]
pub fn unix_st_mode(_meta: &std::fs::Metadata) -> Option<u32> {
    None
}

/// Unix seconds → the typed mtime, mapping Go's zero `time.Time` (`-62135596800`, plan F20) and any
/// out-of-range value to `None`.
#[must_use]
pub fn unix_to_datetime(secs: i64) -> Option<DateTime<Utc>> {
    if crate::metrics::is_go_zero_time(secs) {
        return None;
    }
    Utc.timestamp_opt(secs, 0).single()
}

/// The inverse: `None` becomes Go's sentinel so a re-serialised row is still byte-comparable with the
/// goldens.
#[must_use]
pub const fn datetime_to_unix(dt: Option<DateTime<Utc>>) -> i64 {
    match dt {
        Some(dt) => dt.timestamp(),
        None => GO_ZERO_TIME_UNIX,
    }
}

// --------------------------------------------------------------------------- the wire row

/// One row of `GET /api/dir` — Go's `FileEntry` (`api/dir.go:17-37`), field for field and in that
/// declaration order (struct responses keep declaration order, `docs/contracts.md` §2.4 row *Response
/// key order*, proven by `dir-root.json`).
///
/// Serde notes, each matching one Go tag:
///
/// * `is_symlink` — `json:"is_symlink,omitempty"`: absent for the 99 % of rows that are not links
///   (`dir-root.json` carries it on `link-to-dir` only);
/// * `rel_path` / `depth` — `omitempty` on a string/int: absent unless this came from a flat walk;
/// * `files_count` — **not** skipped: Go's `omitempty` on `int64` is the `(undefined files)` bug
///   (`docs/contracts.md` §2.4), and `None` serialises to `null`, which the port renders `--`;
/// * `mode` — the `fs.FileMode.String()` text, `unknown` included (see [`crate::mode`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FileEntry {
    /// `json:"name"`.
    pub name: String,
    /// `json:"is_dir"` — plan F5's rule, so a symlink to a directory is `true` here.
    pub is_dir: bool,
    /// `json:"is_symlink,omitempty"` — derived from the kind, never typed separately.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_symlink: bool,
    /// `json:"size"`.
    pub size: i64,
    /// `json:"mod_time"` (Unix seconds on the wire; `null`-equivalent is Go's `-62135596800`).
    #[serde(
        default,
        deserialize_with = "deserialize_mod_time",
        serialize_with = "serialize_mod_time"
    )]
    pub mod_time: Option<DateTime<Utc>>,
    /// `json:"mode"`.
    pub mode: String,
    /// `json:"rel_path,omitempty"` — listing-relative (plan §4.2 `Rel`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rel_path: String,
    /// `json:"depth,omitempty"` — 0 means "the listing root itself", which Go omitted.
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub depth: i32,
    /// `json:"files_count"` — always present, unlike Go's `omitempty` (see the module doc).
    pub files_count: Option<u64>,
}

fn is_zero_i32(v: &i32) -> bool {
    *v == 0
}

fn deserialize_mod_time<'de, D>(d: D) -> Result<Option<DateTime<Utc>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = i64::deserialize(d)?;
    Ok(unix_to_datetime(raw))
}

fn serialize_mod_time<S>(v: &Option<DateTime<Utc>>, s: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    s.serialize_i64(datetime_to_unix(*v))
}

impl FileEntry {
    /// The typed form of the `mode` string; `"unknown"` and `-r--r--r--` round-trip
    /// (`crate::mode::ModeKind::parse_mode`).
    #[must_use]
    pub fn mode_kind(&self) -> ModeKind {
        ModeKind::parse_mode(&self.mode)
    }

    /// `mod_time` as Unix seconds (the wire integer), `None` for the sentinel.
    #[must_use]
    pub fn mtime_secs(&self) -> Option<i64> {
        self.mod_time.map(|dt| dt.timestamp())
    }

    /// Go's `int64` `files_count` as the frontend saw it: `0` when unknown. Only for a parity test —
    /// nothing in the port may use it, because it is the `(undefined files)` shape.
    #[must_use]
    pub fn go_files_count(&self) -> u64 {
        self.files_count.unwrap_or(0)
    }
}

impl From<&EntryStat> for FileEntry {
    fn from(stat: &EntryStat) -> Self {
        EntryBuilder::new(Cow::Borrowed(&stat.path), &stat.rel_base, stat).build()
    }
}

impl From<&VirtualNode> for FileEntry {
    fn from(node: &VirtualNode) -> Self {
        node.to_file_entry()
    }
}

// --------------------------------------------------------------------------- builder

/// The one place a wire row is derived from a stat (plan §5.2:
/// `EntryBuilder::from_stat(path).mode_override(m).build()`).
///
/// It fills the four derived fields — `name`, `is_dir` (plan F5), `rel_path`, `mode` — and nothing
/// else: `size`/`mod_time` come straight from the entry, while `files_count` and `depth` are listing
/// context the caller knows and the entry does not.
#[derive(Clone, Debug)]
pub struct EntryBuilder<'a, 'e, E: EntryLike> {
    path: Cow<'a, FsPath>,
    rel_base: Cow<'a, FsPath>,
    entry: &'e E,
    mode_override: Option<ModeKind>,
    files_count: Option<u64>,
    depth: i32,
}

impl<'e> EntryBuilder<'static, 'e, EntryStat> {
    /// `from_stat`: `stat` is the pair the listing produced, and it carries both the entry path and the
    /// listing root `rel_path` is computed against.
    ///
    /// This sits in its own `impl` block so `E` is pinned by `Self`: inside the generic `impl` below the
    /// return type does not mention `Self`, so `E` was not inferable at a call site.
    pub fn from_stat(stat: &'e EntryStat) -> EntryBuilder<'static, 'e, EntryStat> {
        EntryBuilder {
            path: Cow::Owned(stat.path.clone()),
            rel_base: Cow::Owned(stat.rel_base.clone()),
            entry: stat,
            mode_override: None,
            files_count: None,
            depth: 0,
        }
    }
}

impl<'a, 'e, E: EntryLike> EntryBuilder<'a, 'e, E> {
    /// The general form: any producer, with the paths supplied by the caller.
    pub fn new(
        path: impl Into<Cow<'a, FsPath>>,
        rel_base: &FsPath,
        entry: &'e E,
    ) -> EntryBuilder<'a, 'e, E> {
        EntryBuilder {
            path: path.into(),
            rel_base: Cow::Owned(rel_base.clone()),
            entry,
            mode_override: None,
            files_count: None,
            depth: 0,
        }
    }

    /// `.mode_override(m)` — the archive member's hardcoded `-r--r--r--`, or a mode the caller read out
    /// of a container instead of a `stat`.
    #[must_use]
    pub fn mode_override(mut self, mode: ModeKind) -> Self {
        self.mode_override = Some(mode);
        self
    }

    /// The `folder_sizes` cache answer (`api/dir.go:250-257`): `None` when there is no row, which is
    /// serialised as `null` rather than dropped.
    #[must_use]
    pub fn files_count(mut self, count: Option<u64>) -> Self {
        self.files_count = count;
        self
    }

    /// The flat-walk depth (`dir-flat-deep.json`: `d0` → 0/absent, `d0/d1` → 1).
    #[must_use]
    pub fn depth(mut self, depth: i32) -> Self {
        self.depth = depth;
        self
    }

    /// The entry's own path — the same value as `DirPage::path` for the row that is the listing root.
    #[must_use]
    pub fn path(mut self, path: FsPath) -> Self {
        self.path = Cow::Owned(path);
        self
    }

    /// Assemble the row.
    #[must_use]
    pub fn build(self) -> FileEntry {
        let kind = self.entry.kind();
        let mode = self
            .mode_override
            .unwrap_or_else(|| ModeKind::from_entry(self.entry));
        FileEntry {
            name: self.entry.file_name().to_string(),
            is_dir: kind.is_navigable_dir(),
            is_symlink: kind.is_symlink(),
            size: self.entry.size(),
            mod_time: unix_to_datetime(self.entry.mtime_secs().unwrap_or(GO_ZERO_TIME_UNIX)),
            mode: mode.to_string(),
            rel_path: self.path.relative_to(&self.rel_base).unwrap_or_default(),
            depth: self.depth,
            files_count: self.files_count,
        }
    }
}

// --------------------------------------------------------------------------- the page

/// The exact `GET /api/dir` payload — Go's `DirResponse` (`api/dir.go:29-37`), field order included.
///
/// `path` is a `String` here rather than a typed path *because the goldens are strings*: the captures
/// carry the `${FIXTURE_ROOT}` placeholder, which is not a valid absolute path until a contract test
/// substitutes it. [`DirPage`] is the typed form the rest of the port passes around.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DirListing {
    /// `json:"path"`.
    pub path: String,
    /// `json:"entries"` — `[]`, never `null` (a Go `[]FileEntry` made with `make(…, 0, …)`; plan §2.4
    /// row *Response key order* / `dir-empty.json`).
    pub entries: Vec<FileEntry>,
    /// `json:"cursor"` — the next decimal index, `""` when `has_more` is false (`api/dir.go:276`).
    pub cursor: String,
    /// `json:"has_more"`.
    pub has_more: bool,
    /// `json:"total_count"` — computed before pagination (`api/dir.go:274`).
    pub total_count: usize,
    /// `json:"loaded_so_far"` — `== endIndex`.
    pub loaded_so_far: usize,
}

impl DirListing {
    /// Resolve the wire path and type the cursor: the boundary the contract tests cross.
    pub fn into_page(
        self,
        cwd: &Path,
        home: Option<&Path>,
    ) -> Result<DirPage, crate::error::CoreError> {
        let path = FsPath::resolve(&self.path, cwd, home)?;
        Ok(DirPage {
            path,
            entries: self.entries,
            cursor: Cursor::decode(&self.cursor),
            has_more: self.has_more,
            total_count: self.total_count,
            loaded_so_far: self.loaded_so_far,
        })
    }
}

/// A page of a directory, typed: plan §5.2's `DirPage`, and the value `Session::page_dir` publishes
/// (§4.1) — the same fields as [`DirListing`] with the path resolved and the cursor decoded.
#[derive(Clone, Debug, PartialEq)]
pub struct DirPage {
    /// The listed directory.
    pub path: FsPath,
    /// The rows of this page.
    pub entries: Vec<FileEntry>,
    /// The *next* index; the wire form is `""` when `has_more` is false ([`DirPage::cursor_wire`]).
    pub cursor: Cursor,
    /// More rows after this page.
    pub has_more: bool,
    /// Rows in the whole listing, before pagination.
    pub total_count: usize,
    /// Rows delivered up to and including this page.
    pub loaded_so_far: usize,
}

impl DirPage {
    /// The wire spelling of [`Self::cursor`].
    #[must_use]
    pub fn cursor_wire(&self) -> String {
        self.cursor.wire(self.has_more)
    }

    /// Downgrade to the wire payload (`path` in its display form).
    #[must_use]
    pub fn to_listing(&self) -> DirListing {
        DirListing {
            path: self.path.to_string(),
            entries: self.entries.clone(),
            cursor: self.cursor_wire(),
            has_more: self.has_more,
            total_count: self.total_count,
            loaded_so_far: self.loaded_so_far,
        }
    }

    /// `true` when every row of the listing is present (`loaded_so_far == total_count`).
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.loaded_so_far >= self.total_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeSet;

    use serde_json::Value;

    use crate::mode::ModeBits;

    /// A golden capture, parsed. `${FIXTURE_ROOT}` stays a literal: these tests compare *shapes*, and a
    /// placeholder path is exactly what a contract test has to substitute before resolving.
    fn golden(name: &str) -> Value {
        let path = format!(
            "{}/../../tests/golden/{name}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"))
    }

    fn keys(obj: &Value) -> BTreeSet<String> {
        obj.as_object()
            .unwrap_or_else(|| panic!("an object, got {obj}"))
            .keys()
            .cloned()
            .collect()
    }

    fn minus(a: &BTreeSet<String>, b: &BTreeSet<String>) -> Vec<String> {
        a.difference(b).cloned().collect()
    }

    fn entry_key_union(page: &Value) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for entry in page["entries"]
            .as_array()
            .unwrap_or_else(|| panic!("entries array in {page}"))
        {
            out.extend(keys(entry));
        }
        out
    }

    /// The step-3 gate: the captured `dir-root.json` deserialises into [`DirListing`]/[`FileEntry`], and
    /// re-serialising produces the same field-name sets — with exactly the deliberate additions
    /// `docs/contracts.md` §2.4 records. A per-key compare, not a byte compare: our values are typed
    /// (`mod_time` is a `DateTime`, `files_count` is always present).
    #[test]
    fn wire_diff_against_the_frozen_golden() {
        let captured = golden("dir-root");
        let listing: DirListing = serde_json::from_value(captured.clone()).expect("DirListing");
        let back = serde_json::to_value(&listing).expect("re-serialise");

        // ---- top level -------------------------------------------------------------
        let top_golden = keys(&captured);
        let top_ours = keys(&back);
        let top_missing = minus(&top_golden, &top_ours);
        let top_extra = minus(&top_ours, &top_golden);
        println!("dir-root top level, golden: {top_golden:?}");
        println!("dir-root top level,  ours: {top_ours:?}");
        println!(
            "dir-root top level, only in golden: {top_missing:?}; only in ours: {top_extra:?}"
        );
        assert!(
            top_missing.is_empty(),
            "fields the capture has and we dropped: {top_missing:?}"
        );
        assert!(
            top_extra.is_empty(),
            "fields we invented at the top level: {top_extra:?}"
        );

        // ---- entries ---------------------------------------------------------------
        let entry_golden = entry_key_union(&captured);
        let entry_ours = entry_key_union(&back);
        let entry_missing = minus(&entry_golden, &entry_ours);
        let entry_extra = minus(&entry_ours, &entry_golden);
        println!("dir-root entries, golden key union: {entry_golden:?}");
        println!("dir-root entries,  ours key union: {entry_ours:?}");
        println!(
            "dir-root entries, only in golden: {entry_missing:?}; only in ours: {entry_extra:?}"
        );
        assert!(
            entry_missing.is_empty(),
            "captured entry fields our `FileEntry` cannot carry: {entry_missing:?}"
        );
        assert_eq!(
            entry_extra,
            vec!["files_count".to_string()],
            "the only deliberate entry-level addition is the `(undefined files)` fix"
        );

        // Values survive the round trip, not just the key names.
        let rows = captured["entries"].as_array().expect("rows");
        assert_eq!(listing.entries.len(), rows.len(), "dir-root row count");
        assert_eq!(listing.total_count, 33);
        assert_eq!(listing.cursor, "");
        assert!(!listing.has_more);
        assert_eq!(listing.loaded_so_far, 33);
        for (ours, theirs) in listing.entries.iter().zip(rows) {
            let back = serde_json::to_value(ours).expect("row");
            for field in ["name", "is_dir", "size", "mod_time", "mode"] {
                assert_eq!(
                    back[field], theirs[field],
                    "{field} survives: {}",
                    ours.name
                );
            }
            match theirs.get("is_symlink") {
                Some(v) => assert_eq!(back["is_symlink"], *v, "is_symlink survives"),
                None => assert!(
                    back.get("is_symlink").is_none(),
                    "a non-link row must not grow `is_symlink`: {}",
                    ours.name
                ),
            }
        }
    }

    /// Plan F20: Go's zero `time.Time` (`-62135596800`) is *not* a date, and `dir-perm-noexec.json` is
    /// the capture that proves it — the whole listing is HTTP 200 with `mode:"unknown"`, `size:0` and
    /// that sentinel.
    #[test]
    fn zero_mtime_is_none_and_stays_the_sentinel() {
        let captured = golden("dir-perm-noexec");
        let listing: DirListing = serde_json::from_value(captured.clone()).expect("DirListing");
        assert_eq!(listing.entries.len(), 1);
        let row = &listing.entries[0];
        assert_eq!(row.mod_time, None, "F20: the sentinel decodes to None");
        assert_eq!(row.mtime_secs(), None);
        assert_eq!(row.mode, "unknown");
        assert_eq!(row.mode_kind(), ModeKind::Unknown);
        assert_eq!(row.size, 0);
        assert!(!row.is_dir);
        assert_eq!(row.files_count, None, "no cache row in the capture");

        // And it re-serialises to the sentinel, so a contract test still sees Go's bytes.
        let back = serde_json::to_value(row).expect("row");
        assert_eq!(
            back["mod_time"],
            Value::from(GO_ZERO_TIME_UNIX),
            "None must re-encode as Go's sentinel"
        );
        assert_eq!(back["mode"], "unknown");
        assert_eq!(back["files_count"], Value::Null, "always serialised");
        assert_eq!(GO_ZERO_TIME_UNIX, -62_135_596_800);
    }

    /// The `(undefined files)` fix, isolated: `None` is present-and-null, not absent.
    #[test]
    fn files_count_is_always_serialised() {
        let row = FileEntry {
            name: "subdir".into(),
            is_dir: true,
            is_symlink: false,
            size: 60,
            mod_time: unix_to_datetime(1_767_225_600),
            mode: "drwxr-xr-x".into(),
            rel_path: String::new(),
            depth: 0,
            files_count: None,
        };
        let json = serde_json::to_string(&row).expect("json");
        assert!(json.contains("\"files_count\":null"), "{json}");
        assert!(!json.contains("is_symlink"), "{json}");
        assert!(!json.contains("rel_path"), "{json}");
        assert!(!json.contains("depth"), "{json}");
        // `dir-size-subdir.json` is the capture where Go *did* have a cache row.
        let mut with_count = row.clone();
        with_count.files_count = Some(3);
        let json = serde_json::to_string(&with_count).expect("json");
        assert!(json.contains("\"files_count\":3"), "{json}");
        assert_eq!(
            row.go_files_count(),
            0,
            "Go's omitempty shape, kept only for parity tests"
        );
        // Round trip through the wire form keeps `None` distinct from `Some(0)`.
        let back: FileEntry = serde_json::from_str(&json).expect("row");
        assert_eq!(back.files_count, Some(3));
        let back: FileEntry =
            serde_json::from_str(&serde_json::to_string(&row).expect("j")).expect("row");
        assert_eq!(back.files_count, None);
    }

    /// Plan F5, the actual fix: a symlink whose target is a directory is a directory on the wire *and*
    /// still a symlink — the state Go's two independent booleans could not express.
    #[test]
    fn symlink_to_a_directory_is_a_directory_and_a_link() {
        let link = InfoEntry::new(
            "link-to-dir",
            EntryKind::SymlinkDir,
            4096,
            Some(1_767_225_600),
            Some(0o120_777),
        );
        let row = EntryBuilder::new(Cow::Owned(FsPath::ROOT.clone()), &FsPath::ROOT, &link).build();
        assert!(
            row.is_dir,
            "F5: is_dir = kind.is_dir() || kind.is_symlink_to_dir()"
        );
        assert!(row.is_symlink);
        assert_eq!(row.mode, "Lrwxrwxrwx", "capital L, as in dir-root.json");
        assert_eq!(row.mod_time, unix_to_datetime(1_767_225_600));

        // A link to a file is not a directory.
        let to_file = InfoEntry::new(
            "link-to-file",
            EntryKind::Symlink,
            12,
            Some(1_767_225_600),
            Some(0o120_777),
        );
        let row =
            EntryBuilder::new(Cow::Owned(FsPath::ROOT.clone()), &FsPath::ROOT, &to_file).build();
        assert!(!row.is_dir);
        assert!(row.is_symlink);
        assert_eq!(row.mode, "Lrwxrwxrwx");
    }

    /// The derived-field rule of plan §5.2, checked against a real `st_mode` rather than a string, plus
    /// `rel_path` in *listing* coordinates (`dir-flat-subdir.json`: `b`, `b/c.txt`).
    #[test]
    fn from_stat_fills_the_four_derived_fields() {
        let base = FsPath::fs("fixtures/tree/subdir").expect("listing root");
        let child = FsPath::fs("fixtures/tree/subdir/b/c.txt").expect("entry path");
        let stat = EntryStat::in_listing(
            child.clone(),
            base.clone(),
            DirEntryLike::new("c.txt", EntryKind::File),
            Some(InfoEntry::new(
                "c.txt",
                EntryKind::File,
                2,
                Some(1_767_225_600),
                Some(0o100_644),
            )),
        );
        let row = EntryBuilder::from_stat(&stat).depth(1).build();
        assert_eq!(row.name, "c.txt", "name = file_name");
        assert!(!row.is_dir);
        assert!(!row.is_symlink);
        assert_eq!(row.mode, "-rw-r--r--", "mode = ModeKind::from_entry(entry)");
        assert_eq!(
            row.rel_path, "b/c.txt",
            "rel_path = path.relative_to(listing root)"
        );
        assert_eq!(row.depth, 1);
        assert_eq!(row.files_count, None);
        assert_eq!(
            EntryBuilder::from_stat(&stat).build(),
            FileEntry::from(&stat),
            "the `From` is the builder"
        );

        // `mode_override` is how an archive member keeps Go's hardcoded string.
        let overridden = EntryBuilder::from_stat(&stat)
            .mode_override(ModeKind::Archive)
            .build();
        assert_eq!(overridden.mode, "-r--r--r--");
        assert_eq!(
            overridden.rel_path, "b/c.txt",
            "the override touches one field"
        );

        // A stat whose `rel_base` is the entry's own store root yields the `rel_path`-free row of a
        // non-flat listing.
        let plain = EntryStat::standalone(
            child,
            DirEntryLike::new("c.txt", EntryKind::File),
            Some(InfoEntry::new(
                "c.txt",
                EntryKind::File,
                2,
                Some(1_767_225_600),
                Some(0o100_644),
            )),
        );
        assert!(plain.rel_path().contains("c.txt"), "store-relative");
    }

    /// `archive_vfs.go:23-32` — `ToFileEntry` with its hardcoded mode and `rel_path = InternalPath`.
    #[test]
    fn virtual_node_reproduces_the_archive_rows() {
        let captured = golden("archive-list-plain");
        let rows = captured["entries"].as_array().expect("rows");
        let first = &rows[0];
        let node = VirtualNode {
            name: first["name"].as_str().unwrap_or_default().to_string(),
            kind: EntryKind::Dir,
            size: first["size"].as_i64().unwrap_or_default(),
            mod_time: first["mod_time"].as_i64(),
            internal_path: first["rel_path"].as_str().unwrap_or_default().to_string(),
            children: vec![],
        };
        let row = node.to_file_entry();
        assert_eq!(
            row.mode, "-r--r--r--",
            "hardcoded, even for a directory member"
        );
        assert_eq!(row.mode_kind(), ModeKind::Archive);
        assert_eq!(row.rel_path, node.internal_path);
        assert!(row.is_dir);
        assert!(
            !row.is_symlink,
            "Go lost the symlink bit; ours can carry it"
        );
        assert_eq!(row.files_count, None);
        assert_eq!(row.depth, 0);
        let back = serde_json::to_value(&row).expect("row");
        assert_eq!(back["mod_time"], first["mod_time"]);
        assert_eq!(back["name"], first["name"]);
        assert_eq!(back["is_dir"], first["is_dir"]);
        assert_eq!(back["mode"], first["mode"]);
        // The whole capture, row by row, through the typed node.
        for theirs in rows {
            let n = VirtualNode {
                name: theirs["name"].as_str().unwrap_or_default().to_string(),
                kind: if theirs["is_dir"].as_bool().unwrap_or(false) {
                    EntryKind::Dir
                } else {
                    EntryKind::File
                },
                size: theirs["size"].as_i64().unwrap_or_default(),
                mod_time: theirs["mod_time"].as_i64(),
                internal_path: theirs["rel_path"].as_str().unwrap_or_default().to_string(),
                children: vec![],
            };
            let mine = serde_json::to_value(n.to_file_entry()).expect("row");
            for field in ["name", "is_dir", "size", "mod_time", "mode", "rel_path"] {
                assert_eq!(mine[field], theirs[field], "{}", n.name);
            }
        }
    }

    /// The degraded row, produced rather than parsed: `dir-perm-noexec.json`.
    #[test]
    fn degraded_entry_reproduces_the_perm_noexec_row() {
        let path = FsPath::fs("fixtures/tree/noexec-dir/inner.txt").expect("path");
        let stat = EntryStat::degraded(path, "inner.txt");
        let row = EntryBuilder::from_stat(&stat).build();
        assert_eq!(row.mode, "unknown");
        assert_eq!(row.size, 0);
        assert_eq!(row.mod_time, None);
        assert_eq!(
            serde_json::to_value(&row).expect("row")["mod_time"],
            Value::from(-62_135_596_800_i64)
        );
        assert_eq!(row.rel_path, "inner.txt");
        assert!(!row.is_dir);
        assert!(!row.is_symlink);
    }

    /// The typed page: `DirListing` → `DirPage` → `DirListing`, cursor included. `dir-many-p1.json` is
    /// the capture whose cursor is `"200"` and whose last page is `""`.
    #[test]
    fn page_cursor_is_typed_and_wire_equal() {
        let captured = golden("dir-many-p1");
        let listing: DirListing = serde_json::from_value(captured.clone()).expect("listing");
        assert_eq!(listing.cursor, "200");
        assert!(listing.has_more);
        let listing_path = listing.path.clone();
        let page = listing
            .into_page(Path::new("/"), None)
            .expect("the captured path is absolute");
        assert_eq!(page.cursor, Cursor(200));
        assert_eq!(page.cursor_wire(), "200");
        assert!(!page.is_complete());
        let wire = page.to_listing();
        assert_eq!(wire.cursor, "200");
        assert_eq!(
            wire.path,
            format!("/{listing_path}"),
            "the captured path is the `${{FIXTURE_ROOT}}` placeholder, so `into_page` resolved it against `/`"
        );
        assert_eq!(wire.entries.len(), 200);

        // The last page has no cursor at all (`dir-many-last.json`).
        let last: DirListing = serde_json::from_value(golden("dir-many-last")).expect("listing");
        assert_eq!(last.cursor, "");
        assert!(!last.has_more);
        let page = last.into_page(Path::new("/"), None).expect("page");
        assert_eq!(page.cursor, Cursor::FIRST);
        assert_eq!(page.cursor_wire(), "");

        // `dir-root.json`'s path is the `${FIXTURE_ROOT}` placeholder, so it only resolves once a test
        // substitutes it — which is what a step-10 contract test does.
        let raw = golden("dir-root");
        assert!(
            raw["path"].as_str().unwrap_or_default().starts_with("${"),
            "placeholder, as captured"
        );
        let substituted = DirListing {
            path: "/fixture/tree".into(),
            ..serde_json::from_value(raw).expect("listing")
        };
        let page = substituted.into_page(Path::new("/"), None).expect("page");
        assert_eq!(page.path.to_string(), "/fixture/tree");
        assert_eq!(page.cursor, Cursor::FIRST);
        assert!(page.is_complete());
    }

    #[test]
    fn entry_kind_predicates_are_exclusive() {
        for (kind, dir, symlink, symlink_dir, navigable) in [
            (EntryKind::File, false, false, false, false),
            (EntryKind::Dir, true, false, false, true),
            (EntryKind::Symlink, false, true, false, false),
            (EntryKind::SymlinkDir, false, true, true, true),
        ] {
            assert_eq!(kind.is_dir(), dir, "{kind:?}");
            assert_eq!(kind.is_symlink(), symlink, "{kind:?}");
            assert_eq!(kind.is_symlink_to_dir(), symlink_dir, "{kind:?}");
            assert_eq!(kind.is_navigable_dir(), navigable, "{kind:?}");
        }
        // The `lstat` type alone cannot tell a link to a directory from a link to a file: the follow
        // answer distinguishes them, and that answer is the caller's (`EntryKind::from_file_type`).
        let temp_type = std::fs::metadata(std::env::temp_dir())
            .expect("temp dir metadata")
            .file_type();
        assert_eq!(EntryKind::from_file_type(&temp_type, false), EntryKind::Dir);
        assert_eq!(
            EntryKind::from_unix_mode(0o040_755, false),
            EntryKind::Dir,
            "a directory needs no follow answer"
        );
        assert_eq!(
            EntryKind::from_unix_mode(0o120_777, true),
            EntryKind::SymlinkDir
        );
        assert_eq!(
            EntryKind::from_unix_mode(0o120_777, false),
            EntryKind::Symlink
        );
        assert_eq!(EntryKind::from_unix_mode(0o100_644, true), EntryKind::File);
    }

    /// The four producers answer the same trait the same way (`mode` from the `st_mode`, `unknown`
    /// without one), which is what lets `EntryBuilder` be one function.
    #[test]
    fn all_four_producers_agree_on_the_mode_string() {
        let st = 0o040_755u32;
        let dirent = DirEntryLike::new("subdir", EntryKind::Dir);
        let info = InfoEntry::new("subdir", EntryKind::Dir, 60, Some(1_767_225_600), Some(st));
        let path = FsPath::fs("fixtures/tree/subdir").expect("path");
        let stat = EntryStat::standalone(path, dirent.clone(), Some(info.clone()));
        let node = VirtualNode {
            name: "subdir".into(),
            kind: EntryKind::Dir,
            size: 60,
            mod_time: Some(1_767_225_600),
            internal_path: "subdir".into(),
            children: vec![],
        };
        assert_eq!(
            ModeKind::from_entry(&dirent),
            ModeKind::Unknown,
            "a dirent alone has never been statted"
        );
        assert_eq!(
            ModeKind::from_entry(&info),
            ModeKind::Dir(ModeBits::from_bits_retain(0o755))
        );
        assert_eq!(ModeKind::from_entry(&stat), ModeKind::from_entry(&info));
        assert_eq!(ModeKind::from_entry(&node), ModeKind::Unknown);
        assert_eq!(node.to_file_entry().mode, "-r--r--r--");

        // The trait's own accessors, not just the mode.
        assert_eq!(dirent.file_name(), "subdir");
        assert_eq!(dirent.size(), 0);
        assert_eq!(dirent.mtime_secs(), None);
        assert_eq!(stat.mtime_secs(), Some(1_767_225_600));
        assert_eq!(stat.unix_mode(), Some(st));
        assert_eq!(node.size(), 60);
        assert_eq!(node.unix_mode(), None);
        assert_eq!(info.windows_attributes(), None);
    }

    #[test]
    fn mtime_conversions_cover_the_sentinel_and_the_era() {
        assert_eq!(unix_to_datetime(GO_ZERO_TIME_UNIX), None);
        assert_eq!(datetime_to_unix(None), GO_ZERO_TIME_UNIX);
        assert_eq!(
            unix_to_datetime(1_767_225_600).map(|d| d.timestamp()),
            Some(1_767_225_600)
        );
        assert_eq!(
            datetime_to_unix(unix_to_datetime(0)),
            0,
            "the epoch is a real date"
        );
        assert_eq!(
            system_time_to_unix(std::time::UNIX_EPOCH),
            0,
            "and SystemTime agrees"
        );
        assert!(
            unix_to_datetime(i64::MIN).is_none(),
            "an unrepresentable time is unknown, never a panic"
        );
    }
}
