//! The path model (plan §4.2, plan §5.2 `path.rs` row).
//!
//! The archive VFS is a **type**, not a string convention. That is the whole point: a write-capable
//! API takes [`AbsPath`], so "paste into a `.zip`" — which Go guarded with a `strings.Contains`
//! substring test that both over- and under-matched (`api/ops.go:60-80`, `docs/contracts.md`
//! *Deviation ledger* #13: the real directory `tree/fake.zip/inner.txt` was refused with 405) — is not a
//! case the compiler can be talked into, it is not expressible.
//!
//! ```text
//! FsPath { store: VfsKind, relative: String }
//!              │                └── path *inside* the store, `/`-separated, no leading/trailing
//!              │                    separator, `""` == store root
//!              └── which store: the local filesystem, or an archive (optionally a nested one)
//! ```
//!
//! # The `Resolve` table — the six input classes (plan §5.2 step-3 "6-case table")
//!
//! | # | input `raw` | result | Go anchor |
//! |---|---|---|---|
//! | 1 | `""` | the process `cwd` | `api/dir.go:41-46` |
//! | 2 | `~`, `~/x`, `~user`-shaped | `home` / `home/x`; [`PathReject::NoHome`](crate::error::PathReject::NoHome) when `HOME` is unset | `api/dir.go:48-54` |
//! | 3 | absolute — `/a/b`, `//a`, `/a/../b`, `/a/./b`, trailing `/` | cleaned absolute; `.`/`..` absorbed; never above `/` | `api/dir.go:57-64` |
//! | 4 | Windows-absolute — `C:\a`, `C:/a/b`, `\\host\share\x`, `\\?\C:\a` | the same, parsed host-independently by [`AbsPath::from_windows`] | `api/dir.go:58` (`filepath.Abs` on Windows) |
//! | 5 | relative — `a/b`, `./a`, `a/../b` | `cwd` + `raw`, cleaned | `api/dir.go:58` |
//! | 6 | anything naming an archive component — `/a/b.zip/x`, `/a/b.zip/inner.7z/x` | `VfsKind::Archive { host, nested }` with the remainder as `relative` | `frontend/js/utils.js:45-90` (`parseVfsPath`) |
//!
//! Rules that are not input classes, because they are invariants of the result:
//!
//! * **`Resolve`** — the first archive component becomes the store host, a second becomes `nested`, and
//!   everything after that is the member path (case 6). Components are recognised as archives when their
//!   *name ends in* `.zip`/`.rar`/`.7z` (case-insensitive, [`classify_archive_name`]); the JS original
//!   searched the whole string, so `/a/b.zipper/x` opened a `b.zip` with member `per/x`. See
//!   [`split_archive_path`].
//! * **`FromAbs`** — every [`AbsPath`] is an `Fs`-store path whose `relative` is the path below `/`
//!   ([`FsPath::from_abs`]), and every `Fs`-store path has an `AbsPath` back ([`FsPath::host_abs`]).
//! * **`Rel`** — `relative` is always *store*-relative. A listing's `rel_path` is *listing*-relative, so
//!   the caller derives it explicitly with [`FsPath::relative_to`] — plan §5.2 `entry.rs`:
//!   `rel_path = path.relative_to(store.root())`.
//! * **`Display`** — `Fs` renders as the absolute path; an archive as `host[/nested]/relative`, the
//!   concatenation Go showed in a tab title. It round-trips through [`FsPath::resolve`] (except for the
//!   `~` input, which is resolved on the way in).
//! * A `..` that cannot be absorbed — `join("..")`, a `..` in a joined path, a prefix strip that would
//!   leave the store — is [`PathReject::EscapesRoot`](crate::error::PathReject::EscapesRoot). A single
//!   `.` name is `InteriorDot`. `*`/`?`/`[` are legal *filename bytes* on Linux, so `join` accepts them
//!   and only [`validate_dest_name`] (the destination guard, plan F19) rejects them.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use serde::Serialize;

use crate::error::CoreError;
use crate::error::PathReject;

/// The extensions that open a new store, in the order Go's parsers try them
/// (`frontend/js/utils.js:46-52`, `api/ops.go:62-65`).
pub const ARCHIVE_EXTENSIONS: [&str; 3] = [".zip", ".rar", ".7z"];

/// An archive container format, decided by name alone (Go also chose its reader by extension,
/// `api/archive_parser.go:39-108`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveFormat {
    /// `.zip`
    Zip,
    /// `.rar`
    Rar,
    /// `.7z`
    SevenZip,
}

impl ArchiveFormat {
    /// The dotted extension, lowercase.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Zip => ".zip",
            Self::Rar => ".rar",
            Self::SevenZip => ".7z",
        }
    }
}

/// Which archive format, if any, a *name* claims. Case-insensitive, like Go's `indexOf(".zip")` on a
/// lowercased copy (`frontend/js/utils.js:47-49`).
#[must_use]
pub fn classify_archive_name(name: &str) -> Option<ArchiveFormat> {
    let lower = name.to_ascii_lowercase();
    ARCHIVE_EXTENSIONS
        .iter()
        .position(|ext| lower.ends_with(ext))
        .and_then(|ix| {
            [
                ArchiveFormat::Zip,
                ArchiveFormat::Rar,
                ArchiveFormat::SevenZip,
            ]
            .get(ix)
            .copied()
        })
}

/// `true` when a file *name* names an archive file.
#[must_use]
pub fn is_archive_name(name: &str) -> bool {
    classify_archive_name(name).is_some()
}

/// What an archive-internal path refers to, as far as a manifest can tell.
///
/// The Go manifest stored only `IsDir` (`api/archive_vfs.go:13-20`), which is why a zip symlink member
/// surfaced as a regular file whose size was the length of its target path (`archive-list-symlink.json`,
/// `docs/contracts.md` §2.4 row *Archive listing*). This is the shape the step-13 reader has to fill in;
/// the name-only cases come from the cleaned internal path (`api/archive_parser.go:32`
/// `cleanInternalPath`, which leaves a trailing `/` on directory members).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberKind {
    /// A directory member.
    Dir,
    /// A regular file member.
    File,
    /// A member that is itself an archive: entering it opens a new store.
    Archive(ArchiveFormat),
    /// A symlink/hardlink/device member, or the zip-slip root that surfaced as a directory literally
    /// named `..` (`archive-list-zipslip.json`).
    Other,
}

impl MemberKind {
    /// Classify a member from its internal path and the container's own `is_dir` bit. An archive-named
    /// member wins over `is_dir`, because the VFS cares whether entering it lists or opens.
    #[must_use]
    pub fn classify(internal_path: &str, is_dir: bool) -> Self {
        let last = internal_path.rsplit('/').next().unwrap_or(internal_path);
        if let Some(fmt) = classify_archive_name(last) {
            return Self::Archive(fmt);
        }
        if last == ".." {
            return Self::Other;
        }
        if is_dir || internal_path.ends_with('/') {
            return Self::Dir;
        }
        Self::File
    }

    /// `true` for [`MemberKind::Dir`].
    #[must_use]
    pub const fn is_dir(self) -> bool {
        matches!(self, Self::Dir)
    }
}

/// How a path sorts, for the row icon and for navigation.
///
/// The three `Lnk-*` classes are plan §5.2's fix for Go reporting every symlink with the same
/// `Lrwxrwxrwx` mode and `is_dir:false`, including the one that pointed at a directory
/// (`api/dir.go:259`, `dir-root.json` `link-to-dir`; *Deviation ledger* #5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileClass {
    /// A real directory.
    Dir,
    /// A regular file whose name claims an archive extension: entering it opens a store.
    Archive(ArchiveFormat),
    /// A regular file with an image extension.
    Image,
    /// A regular file with a text/source extension.
    Text,
    /// Any other regular file.
    File,
    /// A FIFO.
    Fifo,
    /// A socket.
    Socket,
    /// A character or block device.
    Device,
    /// A symlink to a directory.
    LnkDir,
    /// A symlink to a file.
    LnkFile,
    /// A symlink to something else, or a dangling symlink.
    LnkOther,
    /// Nothing could be determined (`symlink_metadata` failed).
    Unknown,
}

impl FileClass {
    /// The stable label — the icon key step 7's `SvgIconSet` registry is keyed by, hence the
    /// `Lnk-Dir`/`Lnk-File`/`Lnk-Other` spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dir => "Dir",
            Self::Archive(_) => "Archive",
            Self::Image => "Image",
            Self::Text => "Text",
            Self::File => "File",
            Self::Fifo => "Fifo",
            Self::Socket => "Socket",
            Self::Device => "Device",
            Self::LnkDir => "Lnk-Dir",
            Self::LnkFile => "Lnk-File",
            Self::LnkOther => "Lnk-Other",
            Self::Unknown => "Unknown",
        }
    }
}

impl std::fmt::Display for FileClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Extensions treated as images. Ours, not Go's: the icon choice lived in `frontend/js` and is not part
/// of the wire contract.
const IMAGE_EXTENSIONS: [&str; 12] = [
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "svg", "ico", "tif", "tiff", "avif", "heic",
];

/// Extensions treated as text/source. Same provenance as [`IMAGE_EXTENSIONS`].
const TEXT_EXTENSIONS: [&str; 18] = [
    "txt", "md", "rs", "go", "js", "ts", "json", "toml", "yaml", "yml", "xml", "html", "css", "py",
    "sh", "c", "h", "log",
];

/// The class of a bare *name*, by extension only (no I/O, so never a `Dir`/`Lnk-*`: those need the file
/// type, which [`classify`] reads from the filesystem).
#[must_use]
pub fn class_from_name(name: &str) -> FileClass {
    if name.is_empty() {
        return FileClass::Unknown;
    }
    class_from_extension(name)
}

/// The icon-class label of a bare name: [`class_from_name`], flattened to the string form (`"Archive"`,
/// `"Image"`, `"Text"`, `"File"`, `"Unknown"`).
#[must_use]
pub fn file_class(name: &str) -> &'static str {
    class_from_name(name).as_str()
}

fn class_from_extension(name: &str) -> FileClass {
    let lower = name.to_ascii_lowercase();
    // `filepath.Ext(".hidden")` is "" (the API the index used for its `ext` column,
    // `api/search_index_walker.go`), so a leading dot is not an extension separator.
    let ext = match lower.rfind('.') {
        Some(0) | None => "",
        Some(ix) => &lower[ix + 1..],
    };
    if let Some(fmt) = classify_archive_name(&lower) {
        return FileClass::Archive(fmt);
    }
    if IMAGE_EXTENSIONS.contains(&ext) {
        FileClass::Image
    } else if TEXT_EXTENSIONS.contains(&ext) {
        FileClass::Text
    } else {
        FileClass::File
    }
}

/// Classify a real path: the type from `symlink_metadata` (never follows), and for a symlink the type of
/// its target from `metadata` (follows, so a dangling link is [`FileClass::LnkOther`]).
#[must_use]
pub fn classify(path: &Path) -> FileClass {
    let Ok(meta) = path.symlink_metadata() else {
        return FileClass::Unknown;
    };
    let file_type = meta.file_type();
    if file_type.is_dir() {
        return FileClass::Dir;
    }
    if file_type.is_symlink() {
        return classify_link(path);
    }
    // The FIFO/socket/device predicates are a Unix extension trait; on Windows the classifier falls
    // through to the extension-based rule, which is all the Go path could do there either.
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt as _;
        if file_type.is_fifo() {
            return FileClass::Fifo;
        }
        if file_type.is_socket() {
            return FileClass::Socket;
        }
        if file_type.is_block_device() || file_type.is_char_device() {
            return FileClass::Device;
        }
    }
    class_from_extension(&name_of(path))
}

fn classify_link(path: &Path) -> FileClass {
    match path.metadata() {
        // A link to a directory is a distinct class: the row must stay a symlink *and* be navigable.
        Ok(target) if target.is_dir() => FileClass::LnkDir,
        Ok(target) if target.is_file() => class_from_extension(&name_of(path)),
        Ok(_) => FileClass::LnkOther,
        Err(_) => FileClass::LnkOther,
    }
}

fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `true` when `s` has a `..` **component** — not a `..` substring, which is what made Go reject
/// `name..txt` (`archive-extract-dotdot-falsepositive.json`, *Deviation ledger* #14).
#[must_use]
pub fn contains_dot_component(s: &str) -> bool {
    s.split('/').any(|c| c == "..")
}

/// Reject a *destination* name: on top of [`validate_single_name`], no glob metacharacters. Legal
/// filename bytes on Linux, but never a legal destination (plan F19: the guard is component-wise and
/// exact, unlike Go's substring `validateDest`).
pub fn validate_dest_name(name: &str) -> Result<(), CoreError> {
    validate_single_name(name)?;
    if name.contains(['*', '?', '[']) {
        return Err(CoreError::InvalidPath {
            raw: name.to_string(),
            reason: PathReject::Wildcard,
        });
    }
    Ok(())
}

/// Reject anything that is not one plain name component. `\` is only a separator on Windows, where a
/// name containing it cannot exist anyway.
pub fn validate_single_name(name: &str) -> Result<(), CoreError> {
    let bad = |reason: PathReject| CoreError::InvalidPath {
        raw: name.to_string(),
        reason,
    };
    if name.is_empty() {
        return Err(bad(PathReject::Empty));
    }
    if name.contains('\0') {
        return Err(bad(PathReject::Wildcard));
    }
    if name == "." {
        return Err(bad(PathReject::InteriorDot));
    }
    if name == ".." {
        return Err(bad(PathReject::EscapesRoot));
    }
    if name.contains('/') || (cfg!(windows) && name.contains('\\')) {
        return Err(bad(PathReject::InteriorDot));
    }
    if cfg!(windows) && rejects_windows_reserved(name) {
        return Err(bad(PathReject::Wildcard));
    }
    Ok(())
}

/// Windows-only name hazards: an alternate data stream (`name:stream`), a path prefix that is not a name
/// (`\`, `/`, `\\?\`, `\\.\`), and the reserved device names (`CON`, `COM1`… — Go's frontend never
/// checked any of these).
///
/// Host-independent and public so the Windows input handling is unit-testable from Linux;
/// [`validate_single_name`] calls it under `cfg!(windows)`, since `:` is a legal Linux filename byte.
#[must_use]
pub fn rejects_windows_reserved(name: &str) -> bool {
    if name.contains(':') || name.starts_with('\\') || name.starts_with('/') {
        return true;
    }
    if name.starts_with("\\\\?\\") || name.starts_with("\\\\.\\") {
        return true;
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$") {
        return true;
    }
    let tail = stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"));
    match tail {
        Some(digits) if digits.len() == 1 => digits.bytes().all(|b| b.is_ascii_digit()),
        _ => false,
    }
}

// --------------------------------------------------------------------------- AbsPath

/// A resolved, cleaned, absolute local-filesystem path (plan §4.2).
///
/// Opaque by design: the field is private and every constructor validates, so `..`, `.`, a missing root
/// and (on Windows) the alternate-data-stream and `\\?\` forms cannot be represented. That is what lets
/// `AbsPath` be the only cache/log key type in the port — Go stored `filepath.Clean`ed *strings*, so two
/// spellings of one directory were two rows in `folder_sizes`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AbsPath {
    inner: PathBuf,
}

impl AbsPath {
    /// Go's `ResolvePath` (`api/dir.go:41-64`): empty → `cwd`, leading `~` → home, then absolute and
    /// cleaned. `cwd` and `home` are injected, not read from the process, so all six table rows are
    /// unit-testable; [`FsPath::resolve`] is the same function with the VFS split applied.
    pub fn resolve(raw: &str, cwd: &Path, home: Option<&Path>) -> Result<Self, CoreError> {
        let joined = match resolve_kind(raw) {
            // Case 1: Go returns `os.Getwd()`.
            ResolveKind::Empty => cwd.to_path_buf(),
            // Case 2: Go does `filepath.Join(home, pathStr[1:])`, i.e. *any* leading tilde is stripped,
            // so `~user` means `home/user`. Reproduced rather than improved: `parseVfsPath` fed the same
            // string to the same resolver, and a UI-visible difference would break the goldens.
            ResolveKind::Tilde => {
                let home = home.ok_or_else(|| CoreError::InvalidPath {
                    raw: raw.to_string(),
                    reason: PathReject::NoHome,
                })?;
                // `Path::join` would *replace* `home` for a tail that starts with `/`, and after `~` is
                // stripped `~/notes.txt` does exactly that. Go's `filepath.Join` treats the tail as
                // relative regardless of its separators (`api/dir.go:52`), so strip them here.
                let tail = raw.get(1..).unwrap_or_default();
                home.join(tail.trim_start_matches(['/', '\\']))
            }
            // Case 3.
            ResolveKind::AbsoluteUnix => PathBuf::from(raw),
            // Case 4.
            ResolveKind::AbsoluteWindows => return Self::from_windows(raw),
            // Case 5: Go's `filepath.Abs` joins the cwd and cleans.
            ResolveKind::Relative => cwd.join(raw),
        };
        Self::clean(&joined, raw)
    }

    /// Validate an already-absolute path (the `FromAbs` producer: `std::fs` results, directory probes).
    pub fn from_path(path: impl Into<PathBuf>) -> Result<Self, CoreError> {
        let path = path.into();
        let raw = path.to_string_lossy().into_owned();
        Self::clean(&path, &raw)
    }

    /// A Windows-absolute path, parsed identically on every host so the Windows port's input handling is
    /// testable from Linux (plan §5.2 `platform/windows.rs` row: same signatures, different backing).
    ///
    /// Accepts `C:\a\b`, `C:/a/b`, `C:\`, `\\host\share\x` and `\\?\C:\a`; rejects a `..` that escapes the
    /// volume root, and a drive letter with no path at all (`C:`, which is drive-*relative*).
    pub fn from_windows(raw: &str) -> Result<Self, CoreError> {
        if !looks_windows_absolute(raw) {
            return Err(CoreError::InvalidPath {
                raw: raw.to_string(),
                reason: PathReject::Relative,
            });
        }
        let normalized = raw.replace('/', "\\");
        let (prefix, body) = match normalized.strip_prefix("\\\\?\\") {
            Some(rest) => ("\\\\?\\", rest),
            None => ("", normalized.as_str()),
        };
        let unc = body.starts_with("\\\\");
        // A UNC path needs host *and* share; a drive path needs the volume and may stop there (`C:\`).
        let minimal = if unc { 2 } else { 1 };
        let mut stack: Vec<&str> = Vec::new();
        // The volume / host+share components are the root: a `..` may not pop them, any more than
        // `/..` leaves `/`.
        let mut root_len = 0usize;
        for comp in body.split('\\') {
            match comp {
                "" | "." => {}
                ".." => {
                    if stack.len() <= root_len {
                        return Err(CoreError::InvalidPath {
                            raw: raw.to_string(),
                            reason: PathReject::EscapesRoot,
                        });
                    }
                    stack.pop();
                }
                other => {
                    stack.push(other);
                    if stack.len() <= minimal {
                        root_len = stack.len();
                    }
                }
            }
        }
        if stack.len() < minimal {
            return Err(CoreError::InvalidPath {
                raw: raw.to_string(),
                reason: PathReject::Relative,
            });
        }
        // `C:\` is the volume root and absolute, while a bare `C:` is drive-relative (refused above);
        // re-attach the separator the split dropped so the two stay distinguishable, including behind
        // the extended-length prefix.
        let mut text = format!(
            "{prefix}{}",
            // `split` dropped the UNC lead: `\host\share` is the *text* `\\host\share`.
            if unc {
                format!("\\\\{}", stack.join("\\"))
            } else {
                stack.join("\\")
            }
        );
        if !unc && stack.len() == 1 && text.ends_with(':') {
            text.push('\\');
        }
        Ok(Self {
            inner: PathBuf::from(text),
        })
    }

    /// Absolutise, clean, and validate. This is Go's `filepath.Abs` + `filepath.Clean` pair
    /// (`api/dir.go:58`, `:63`), including `Clean`'s collapsing of `//`, `.` and interior `..`.
    fn clean(path: &Path, raw: &str) -> Result<Self, CoreError> {
        let relative = CoreError::InvalidPath {
            raw: raw.to_string(),
            reason: PathReject::Relative,
        };
        if !path.has_root() {
            return Err(relative);
        }
        let mut out = PathBuf::new();
        let mut depth = 0usize;
        for comp in path.components() {
            match comp {
                Component::Prefix(_) | Component::RootDir => {
                    out.push(comp.as_os_str());
                    depth = 0;
                }
                Component::CurDir => {}
                Component::ParentDir => {
                    // `filepath.Clean` clamps at the root — `/../../x` is `/x` (`api/dir.go:63`, the
                    // Abs+Clean-first behaviour of plan §2.4 row *validateDest*). An escaping `..` is
                    // refused where cleaning would silently retarget the caller: [`Self::join`],
                    // [`Self::join_rel`] and `FsPath::new`.
                    if depth == 0 {
                        continue;
                    }
                    if !out.pop() {
                        return Err(relative);
                    }
                    depth -= 1;
                }
                Component::Normal(name) => {
                    out.push(name);
                    depth += 1;
                }
            }
        }
        if !out.has_root() {
            return Err(relative);
        }
        Ok(Self { inner: out })
    }

    /// The borrowed filesystem path.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.inner
    }

    /// The lossy string form: paths are user-visible text in this app, so no `Result` at the boundary.
    #[must_use]
    pub fn to_string_lossy(&self) -> String {
        self.inner.to_string_lossy().into_owned()
    }

    /// `..`, or `None` at the filesystem root.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        self.inner.parent().and_then(|p| Self::from_path(p).ok())
    }

    /// The last component (`None` for `/`).
    ///
    /// A Windows-absolute [`AbsPath`] keeps `\` separators, which on a Unix host are ordinary filename
    /// bytes, so `Path::file_name` there sees one long name; off-Windows the tail of a
    /// Windows-absolute path is therefore recovered by hand (plan §5.2 `platform/windows.rs` row: the
    /// Windows shape stays testable from Linux).
    #[must_use]
    pub fn file_name(&self) -> Option<&OsStr> {
        Self::windows_tail(&self.inner).or_else(|| self.inner.file_name())
    }

    /// Join one *name*, never a path (plan §4.2's `AbsPath::join`): rejects `..`, `.`, separators, NUL, and
    /// — on Windows builds — the reserved/ADS forms. This replaces `validateDest`'s substring test.
    pub fn join(&self, name: &str) -> Result<Self, CoreError> {
        validate_single_name(name)?;
        Self::from_path(self.inner.join(name))
    }

    /// Join a validated multi-component relative path: every component goes through
    /// [`validate_single_name`], so `a/../../b` is refused at the first `..` instead of cleaning away.
    pub fn join_rel(&self, rel: &str) -> Result<Self, CoreError> {
        let mut out = self.inner.clone();
        for comp in rel.split('/') {
            validate_single_name(comp)?;
            out.push(comp);
        }
        Self::from_path(out)
    }

    /// Every component, with no `.` and no `..` (there can be none: type invariant).
    pub fn components_clean(&self) -> impl Iterator<Item = &OsStr> {
        self.inner.components().filter_map(|c| match c {
            Component::Normal(name) => Some(name),
            _ => None,
        })
    }

    /// `self` is an ancestor of, or equal to, `other` — component-wise, which is the fix for Go's
    /// `isSubdir` string prefix compare (`api/ops.go`; correct there only because both sides were
    /// `Clean`ed, and ours are cleaned by construction).
    #[must_use]
    pub fn is_ancestor_of(&self, other: &Self) -> bool {
        let mut mine = self.components_clean();
        let mut theirs = other.components_clean();
        loop {
            match (mine.next(), theirs.next()) {
                (None, _) => return true,
                (Some(a), Some(b)) if a == b => continue,
                _ => return false,
            }
        }
    }

    /// `self` below `root`, `/`-separated; `""` when equal; `None` when `root` is not an ancestor. This
    /// is the `rel_path` producer of plan §5.2's `entry.rs` row.
    #[must_use]
    pub fn relative_to(&self, root: &Self) -> Option<String> {
        let mut mine = self.components_clean();
        for base in root.components_clean() {
            match mine.next() {
                Some(m) if m == base => {}
                _ => return None,
            }
        }
        Some(
            mine.map(|c| c.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/"),
        )
    }

    /// The store-relative form of this path (what an `Fs`-store `FsPath` keeps).
    #[must_use]
    pub fn to_relative(&self) -> String {
        self.components_clean()
            .map(|c| c.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/")
    }

    /// The `\`-separated tail of a Windows-absolute path on a host where `\` is not a separator.
    #[cfg(unix)]
    fn windows_tail(path: &Path) -> Option<&OsStr> {
        use std::os::unix::ffi::OsStrExt as _;

        if !looks_windows_absolute(&path.to_string_lossy()) {
            return None;
        }
        let bytes = path.as_os_str().as_bytes();
        let last = bytes.iter().rposition(|b| *b != b'\\')?;
        let start = bytes[..=last].iter().rposition(|b| *b == b'\\')? + 1;
        <OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(&bytes[start..last + 1]).into()
    }

    /// A Windows host already splits on `\`, and no other host produces Windows-absolute paths.
    #[cfg(not(unix))]
    fn windows_tail(_path: &Path) -> Option<&OsStr> {
        None
    }

    /// `true` for the filesystem root (`/`).
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.components_clean().next().is_none()
    }
}

impl std::fmt::Display for AbsPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_string_lossy())
    }
}

impl TryFrom<&Path> for AbsPath {
    type Error = CoreError;
    fn try_from(value: &Path) -> Result<Self, Self::Error> {
        Self::from_path(value.to_path_buf())
    }
}

/// Which row of the `Resolve` table a raw input falls in (rows 1-5; row 6 is an orthogonal
/// transformation applied afterwards, see [`split_archive_path`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolveKind {
    /// Row 1.
    Empty,
    /// Row 2.
    Tilde,
    /// Row 3.
    AbsoluteUnix,
    /// Row 4.
    AbsoluteWindows,
    /// Row 5.
    Relative,
}

/// Classify a raw input string into `Resolve` rows 1-5.
#[must_use]
pub fn resolve_kind(raw: &str) -> ResolveKind {
    if raw.is_empty() {
        return ResolveKind::Empty;
    }
    // Go's test is `strings.HasPrefix(pathStr, "~")` — no separator required (`api/dir.go:48`).
    if raw.starts_with('~') {
        return ResolveKind::Tilde;
    }
    if raw.starts_with('/') {
        return ResolveKind::AbsoluteUnix;
    }
    if looks_windows_absolute(raw) {
        return ResolveKind::AbsoluteWindows;
    }
    ResolveKind::Relative
}

/// `C:\…`, `C:/…`, `\\host\share…`, `\\?\…` — recognised on every host, see [`AbsPath::from_windows`].
#[must_use]
pub fn looks_windows_absolute(raw: &str) -> bool {
    let b = raw.as_bytes();
    if b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/') {
        return true;
    }
    raw.starts_with("\\\\")
}

// --------------------------------------------------------------------------- VfsKind / FsPath

/// Which store a listing comes from (plan §4.2's `FsPath` enum, re-cut so the *store* is the axis the
/// cache, the log and the archive reader all key on).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VfsKind {
    /// The local filesystem.
    Fs,
    /// An archive. `host` is the archive file on the local filesystem, so it can be `stat`ed for the
    /// `archive_modified` re-check (`api/archive_vfs.go:176-195`, plan F7); `nested` is the member path of
    /// a second archive inside `host` — exactly the `archive` + `nested` pair of `GET /api/archive/dir`.
    Archive {
        /// The archive file.
        host: AbsPath,
        /// The nested archive member; `None` when `host` is opened directly.
        nested: Option<String>,
    },
}

impl VfsKind {
    /// `true` for [`VfsKind::Archive`].
    #[must_use]
    pub const fn is_archive(&self) -> bool {
        matches!(self, Self::Archive { .. })
    }

    /// The archive file of an archive store, `None` for the filesystem.
    #[must_use]
    pub fn host(&self) -> Option<&AbsPath> {
        match self {
            Self::Fs => None,
            Self::Archive { host, .. } => Some(host),
        }
    }

    /// The nested-archive member of an archive store.
    #[must_use]
    pub fn nested(&self) -> Option<&str> {
        match self {
            Self::Fs => None,
            Self::Archive { nested, .. } => nested.as_deref(),
        }
    }

    /// The store root: this store with an empty relative path.
    #[must_use]
    pub fn root_path(&self) -> FsPath {
        FsPath {
            store: self.clone(),
            relative: String::new(),
        }
    }
}

/// A path inside a store: `store` picks the filesystem or one archive, `relative` says where.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FsPath {
    /// The store.
    pub store: VfsKind,
    /// Store-relative, `/`-separated, `""` == store root. Normalised by every constructor here.
    pub relative: String,
}

impl FsPath {
    /// The filesystem root: `Fs` store, empty relative part.
    pub const ROOT: FsPath = FsPath {
        store: VfsKind::Fs,
        relative: String::new(),
    };

    /// Build from a store and a relative path, absorbing `.`/`//`/leading-separator noise and rejecting
    /// a `..` that escapes the store.
    pub fn new(store: VfsKind, relative: &str) -> Result<Self, CoreError> {
        let mut kept: Vec<&str> = Vec::new();
        for comp in relative.split('/') {
            match comp {
                "" | "." => {}
                ".." => {
                    if kept.pop().is_none() {
                        return Err(CoreError::InvalidPath {
                            raw: relative.to_string(),
                            reason: PathReject::EscapesRoot,
                        });
                    }
                }
                other => kept.push(other),
            }
        }
        Ok(Self {
            store,
            relative: kept.join("/"),
        })
    }

    /// An `Fs`-store path from an absolute-ish path (`"/tmp/x"`, `"tmp/x"` or a real [`AbsPath`] text).
    pub fn fs(relative: &str) -> Result<Self, CoreError> {
        Self::new(VfsKind::Fs, relative)
    }

    /// Build an archive-store path directly.
    pub fn archive(host: AbsPath, nested: Option<String>, inner: &str) -> Result<Self, CoreError> {
        Self::new(VfsKind::Archive { host, nested }, inner)
    }

    /// Rows 1-6 of the `Resolve` table, with the VFS split applied and `cwd`/`home` injected.
    pub fn resolve(raw: &str, cwd: &Path, home: Option<&Path>) -> Result<Self, CoreError> {
        let host = AbsPath::resolve(raw, cwd, home)?;
        let rel = host.to_relative();
        match split_archive_path(&rel) {
            None => Ok(Self::from_abs(host)),
            Some(parts) => {
                // The store is the *archive component*, not the whole input: `/a/b.zip/x` has host
                // `/a/b.zip` and member `x` (`docs/contracts.md` → *fileMode strings* archives, and the
                // `archive=/…&nested=…&path=…` triple the Go handler took).
                let store_host = AbsPath::from_path(format!("/{}", parts.host))?;
                let store = VfsKind::Archive {
                    host: store_host,
                    nested: parts.nested,
                };
                Self::new(store, &parts.inner)
            }
        }
    }

    /// The `FromAbs` rule: an absolute local path is always an `Fs`-store path.
    #[must_use]
    pub fn from_abs(abs: AbsPath) -> Self {
        let relative = abs.to_relative();
        Self {
            store: VfsKind::Fs,
            relative,
        }
    }

    /// The absolute local path of the store's host — for an `Fs` path, itself. This is the `stat`-able
    /// path (`archive_modified` compares its mtime); `None` only for text that cannot be an `AbsPath`.
    #[must_use]
    pub fn host_abs(&self) -> Option<AbsPath> {
        match &self.store {
            VfsKind::Fs => AbsPath::from_path(format!("/{}", self.relative)).ok(),
            VfsKind::Archive { host, .. } => Some(host.clone()),
        }
    }

    /// The store root as a path in that store.
    #[must_use]
    pub fn store_root(&self) -> Self {
        self.store.root_path()
    }

    /// The `Rel` rule: the store-relative path (`""` at the store root).
    #[must_use]
    pub fn rel(&self) -> &str {
        &self.relative
    }

    /// `true` when this is the store root.
    #[must_use]
    pub fn is_store_root(&self) -> bool {
        self.relative.is_empty()
    }

    /// `self` below `base` in the same store; `""` when equal, `None` when `base` is not an ancestor
    /// (including the cross-store case, which is not a prefix relation at all).
    #[must_use]
    pub fn relative_to(&self, base: &Self) -> Option<String> {
        if self.store != base.store {
            return None;
        }
        if base.relative.is_empty() {
            return Some(self.relative.clone());
        }
        let mut mine = self.relative.split('/');
        for b in base.relative.split('/') {
            match mine.next() {
                Some(m) if m == b => {}
                _ => return None,
            }
        }
        Some(mine.collect::<Vec<_>>().join("/"))
    }

    /// `..`, or `None` at the store root (Go's browser could not go above `/`; the port can go above a
    /// store, which is a step-8 navigation decision, not a path-model one).
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        if self.relative.is_empty() {
            return None;
        }
        let head = match self.relative.rfind('/') {
            None => "",
            Some(0) => "",
            Some(ix) => &self.relative[..ix],
        };
        Some(Self {
            store: self.store.clone(),
            relative: head.to_string(),
        })
    }

    /// Append one validated name.
    pub fn join(&self, name: &str) -> Result<Self, CoreError> {
        validate_single_name(name)?;
        Ok(Self {
            store: self.store.clone(),
            relative: push_segment(&self.relative, name),
        })
    }

    /// Append a validated multi-component relative path.
    pub fn join_rel(&self, rel: &str) -> Result<Self, CoreError> {
        let mut out = self.relative.clone();
        for comp in rel.split('/') {
            validate_single_name(comp)?;
            out = push_segment(&out, comp);
        }
        Ok(Self {
            store: self.store.clone(),
            relative: out,
        })
    }

    /// `self` with `base` removed as a prefix, borrowing from `self` (the owning form is
    /// [`FsPath::relative_to`]).
    #[must_use]
    pub fn strip_prefix(&self, base: &Self) -> Option<&str> {
        let rel = self.relative_to(base)?;
        // `relative_to` only ever returns a suffix of `self.relative`, so the offset is exact.
        let offset = self.relative.len() - rel.len();
        self.relative.get(offset..)
    }

    /// The components of the display form: `/`, then the host's, the nested archive's and the member's
    /// segments. Owned because the host part needs lossy conversion.
    #[must_use]
    pub fn components(&self) -> Vec<String> {
        let mut out = vec!["/".to_string()];
        if let Some(host) = self.store.host() {
            out.extend(
                host.components_clean()
                    .map(|c| c.to_string_lossy().into_owned()),
            );
            if let Some(nested) = self.store.nested() {
                out.extend(
                    nested
                        .split('/')
                        .filter(|s| !s.is_empty())
                        .map(String::from),
                );
            }
        }
        out.extend(
            self.relative
                .split('/')
                .filter(|s| !s.is_empty())
                .map(String::from),
        );
        out
    }

    /// The store-relative segments (`relative` split on `/`); empty at the store root.
    pub fn segment_iter(&self) -> impl Iterator<Item = &str> {
        if self.relative.is_empty() {
            Vec::new().into_iter()
        } else {
            self.relative.split('/').collect::<Vec<_>>().into_iter()
        }
    }

    /// The last segment, `None` at the store root.
    #[must_use]
    pub fn last_segment(&self) -> Option<&str> {
        if self.relative.is_empty() {
            None
        } else {
            self.relative.rsplit('/').next()
        }
    }

    /// `true` when `name` is the last segment.
    #[must_use]
    pub fn ends_with(&self, name: &str) -> bool {
        self.last_segment() == Some(name)
    }

    /// Component-wise prefix test inside one store.
    #[must_use]
    pub fn starts_with(&self, base: &Self) -> bool {
        self.relative_to(base).is_some()
    }

    /// `true` when this path is, or lives inside, an archive store — i.e. entering it opens a container
    /// rather than listing a directory. The store root itself counts: its host *is* the archive file
    /// (`/a/b.zip` → `archive=/a/b.zip`). Note that a *real directory* named `fake.zip` matches too —
    /// that is the `dir-root.json` fixture, and Go's guard could not tell them apart either (the
    /// difference here is that this predicate is honest about *what it knows*, and the step-13 reader
    /// confirms by opening).
    #[must_use]
    pub fn is_archive_file(&self) -> bool {
        match &self.store {
            VfsKind::Archive { .. } => true,
            VfsKind::Fs => self.last_segment().is_some_and(is_archive_name),
        }
    }

    /// The archive store this path belongs to, as the `Fs`-store path of its host file: `None` for a
    /// plain filesystem path. Used by the manifest cache key and the mtime re-check (plan F7).
    #[must_use]
    pub fn archive_root(&self) -> Option<Self> {
        self.store.host().map(|host| Self::from_abs(host.clone()))
    }

    /// The canonical cache/log key for this path (Go keyed `manifestCache` on a `host::nested` string,
    /// `api/archive_vfs.go:49`, and `folder_sizes` on a cleaned path).
    #[must_use]
    pub fn dir_token(&self) -> DirToken {
        DirToken::of(self)
    }

    /// The archive-store key: host + nested, ignoring the member path — the unit the LRU cache and the
    /// open handle are keyed by.
    #[must_use]
    pub fn archive_token(&self) -> Option<ArchiveToken> {
        ArchiveToken::of_store(&self.store)
    }
}

impl std::fmt::Display for FsPath {
    /// The `Display` rule: an absolute path for the filesystem, `host[/nested]/relative` for an archive;
    /// round-trips through [`FsPath::resolve`].
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.store {
            VfsKind::Fs => {
                f.write_str("/")?;
                f.write_str(&self.relative)
            }
            VfsKind::Archive { host, nested } => {
                f.write_str(&host.to_string_lossy())?;
                if let Some(n) = nested {
                    f.write_str("/")?;
                    f.write_str(n)?;
                }
                if !self.relative.is_empty() {
                    f.write_str("/")?;
                    f.write_str(&self.relative)?;
                }
                Ok(())
            }
        }
    }
}

/// `a` + `/` + `b` for store-relative paths.
fn push_segment(a: &str, b: &str) -> String {
    if a.is_empty() {
        b.to_string()
    } else {
        format!("{a}/{b}")
    }
}

// --------------------------------------------------------------------------- VFS parsing

/// The pieces of a store-relative path that named one or two archives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveParts {
    /// The host archive: the components up to and including the first archive component (`"a/b.zip"`
    /// for `"a/b.zip/n.7z/deep/f.txt"`).
    pub host: String,
    /// The second archive component, if any (`"n.7z"`).
    pub nested: Option<String>,
    /// The member path inside the (nested) archive (`"deep/f.txt"`).
    pub inner: String,
}

/// Split a store-relative path at its first archive component (everything up to and including it is the
/// host, which the caller already has) and then at the second (the nested archive), leaving the rest as
/// the member path.
///
/// `frontend/js/utils.js:45-90` (`parseVfsPath`) is the shape reference — first `.zip`/`.rar`/`.7z` wins,
/// then the same again inside the remainder — with the deliberate component-wise fix described in the
/// module docs.
#[must_use]
pub fn split_archive_path(relative: &str) -> Option<ArchiveParts> {
    let segs: Vec<&str> = relative.split('/').filter(|s| !s.is_empty()).collect();
    let first = segs.iter().position(|s| is_archive_name(s))?;
    let host = segs[..=first].join("/");
    let rest = &segs[first + 1..];
    Some(match rest.iter().position(|s| is_archive_name(s)) {
        Some(second) => ArchiveParts {
            host,
            nested: Some(rest[..=second].join("/")),
            inner: rest[second + 1..].join("/"),
        },
        None => ArchiveParts {
            host,
            nested: None,
            inner: rest.join("/"),
        },
    })
}

/// `parse_vfs` — the Go frontend's `parseVfsPath` (`frontend/js/utils.js:45`), resolved against the
/// process `cwd` (plan §5.2's signature). [`FsPath::resolve`] is the injectable form used by tests and by
/// every job that knows its own base.
pub fn parse_vfs(raw: &str, home: Option<&Path>) -> Result<FsPath, CoreError> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    FsPath::resolve(raw, &cwd, home)
}

/// The query form of the archive triple, for anything that still thinks in `GET /api/archive/dir` terms:
/// `archive=<host>&nested=<member>&path=<member path>` (empty parts are empty values, as the Go handler
/// received them). For a filesystem path this is `path=<abs>` (`api/archive_handlers.go:31-34`).
#[must_use]
pub fn vfs_uri(path: &FsPath) -> String {
    let mut out = String::new();
    match &path.store {
        VfsKind::Fs => {
            out.push_str("path=/");
            out.push_str(&path.relative);
        }
        VfsKind::Archive { host, nested } => {
            out.push_str("archive=");
            out.push_str(&host.to_string_lossy());
            out.push_str("&nested=");
            out.push_str(nested.as_deref().unwrap_or(""));
            out.push_str("&path=");
            out.push_str(&path.relative);
        }
    }
    out
}

// --------------------------------------------------------------------------- tokens

/// Why a token string could not be read back.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid path token {raw:?}: {reason}")]
pub struct TokenError {
    /// The text that failed.
    pub raw: String,
    /// What was wrong.
    pub reason: &'static str,
}

const TOKEN_FS_PREFIX: &str = "fs:";
const TOKEN_ARCHIVE_PREFIX: &str = "ar:";
/// The token field separator. `|` is a legal Linux filename byte, so it is escaped inside values —
/// without that the encoding would not be injective.
const TOKEN_SEPARATOR: char = '|';

/// Escape only what the token grammar needs: `%` itself and the separator.
fn token_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '%' => out.push_str("%25"),
            TOKEN_SEPARATOR => out.push_str("%7C"),
            other => out.push(other),
        }
    }
    out
}

fn token_unescape(s: &str, raw: &str) -> Result<String, TokenError> {
    let bad = || TokenError {
        raw: raw.to_string(),
        reason: "bad percent escape",
    };
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let mut hex = String::new();
        hex.extend(chars.next());
        hex.extend(chars.next());
        let byte = u8::from_str_radix(&hex, 16).map_err(|_| bad())?;
        out.push(byte as char);
    }
    Ok(out)
}

/// A canonical, reversible key for a path in a store.
///
/// This is plan §4.2's "cache keys are canonical by construction" made a type: `folder_sizes`,
/// `DirCacheEntry` and the undo-log path index take this, so `/a/./b` and `/a/b` cannot be two rows for one
/// directory (Go stored `filepath.Clean`ed strings and got away with it only because it re-read
/// everything).
///
/// Grammar: `fs:<absolute path>` or `ar:<host>|<nested>|<inner>`, with `|` → `%7C` and `%` → `%25` inside
/// every field.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DirToken(String);

impl DirToken {
    /// The token for `path`.
    #[must_use]
    pub fn of(path: &FsPath) -> Self {
        match &path.store {
            VfsKind::Fs => Self(format!(
                "{TOKEN_FS_PREFIX}{}",
                token_escape(&format!("/{}", path.relative))
            )),
            VfsKind::Archive { host, nested } => Self(format!(
                "{TOKEN_ARCHIVE_PREFIX}{}{}{}{}{}",
                token_escape(&host.to_string_lossy()),
                TOKEN_SEPARATOR,
                token_escape(nested.as_deref().unwrap_or("")),
                TOKEN_SEPARATOR,
                token_escape(&path.relative)
            )),
        }
    }

    /// Parse, keeping the offending text verbatim on error.
    pub fn parse(raw: &str) -> Result<Self, TokenError> {
        let path = if let Some(body) = raw.strip_prefix(TOKEN_FS_PREFIX) {
            let text = token_unescape(body, raw)?;
            let abs = AbsPath::from_path(text).map_err(|_| TokenError {
                raw: raw.to_string(),
                reason: "not an absolute path",
            })?;
            FsPath::from_abs(abs)
        } else if let Some(body) = raw.strip_prefix(TOKEN_ARCHIVE_PREFIX) {
            archive_from_token_body(body, raw)?
        } else {
            return Err(TokenError {
                raw: raw.to_string(),
                reason: "unknown token prefix",
            });
        };
        Ok(Self::of(&path))
    }

    /// The token text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Read it back.
    pub fn to_fs_path(&self) -> Result<FsPath, TokenError> {
        if let Some(body) = self.0.strip_prefix(TOKEN_FS_PREFIX) {
            let text = token_unescape(body, &self.0)?;
            let abs = AbsPath::from_path(text).map_err(|_| TokenError {
                raw: self.0.clone(),
                reason: "not an absolute path",
            })?;
            return Ok(FsPath::from_abs(abs));
        }
        let body = self
            .0
            .strip_prefix(TOKEN_ARCHIVE_PREFIX)
            .ok_or_else(|| TokenError {
                raw: self.0.clone(),
                reason: "unknown token prefix",
            })?;
        archive_from_token_body(body, &self.0)
    }
}

impl std::fmt::Display for DirToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&FsPath> for DirToken {
    fn from(value: &FsPath) -> Self {
        Self::of(value)
    }
}

/// The archive-store half of a [`DirToken`]: what the manifest cache and the open handle are keyed by
/// (Go: `BuildCacheKey(hostPath, nestedPath)`, `api/archive_vfs.go`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ArchiveToken {
    /// The archive file.
    pub host: AbsPath,
    /// The nested archive member, if any.
    pub nested: Option<String>,
}

impl ArchiveToken {
    /// Derive from a store; `None` for the filesystem.
    #[must_use]
    pub fn of_store(store: &VfsKind) -> Option<Self> {
        match store {
            VfsKind::Fs => None,
            VfsKind::Archive { host, nested } => Some(Self {
                host: host.clone(),
                nested: nested.clone(),
            }),
        }
    }

    /// Token text: `ar:<host>|<nested>`.
    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "{TOKEN_ARCHIVE_PREFIX}{}{}{}",
            token_escape(&self.host.to_string_lossy()),
            TOKEN_SEPARATOR,
            token_escape(self.nested.as_deref().unwrap_or(""))
        )
    }

    /// Parse the two-field form.
    pub fn parse(raw: &str) -> Result<Self, TokenError> {
        let body = raw
            .strip_prefix(TOKEN_ARCHIVE_PREFIX)
            .ok_or_else(|| TokenError {
                raw: raw.to_string(),
                reason: "not an archive token",
            })?;
        let parts = split_token_fields(body, raw, 2)?;
        Ok(Self {
            host: token_host(&parts[0], raw)?,
            nested: optional_field(&parts[1]),
        })
    }

    /// The store this token names.
    #[must_use]
    pub fn to_store(&self) -> VfsKind {
        VfsKind::Archive {
            host: self.host.clone(),
            nested: self.nested.clone(),
        }
    }
}

impl std::fmt::Display for ArchiveToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.encode())
    }
}

/// Split a token body into exactly `want` unescaped fields.
fn split_token_fields(body: &str, raw: &str, want: usize) -> Result<Vec<String>, TokenError> {
    let parts = body
        .split(TOKEN_SEPARATOR)
        .map(|p| token_unescape(p, raw))
        .collect::<Result<Vec<_>, _>>()?;
    if parts.len() != want {
        return Err(TokenError {
            raw: raw.to_string(),
            reason: "wrong number of token fields",
        });
    }
    Ok(parts)
}

fn token_host(text: &str, raw: &str) -> Result<AbsPath, TokenError> {
    AbsPath::from_path(text).map_err(|_| TokenError {
        raw: raw.to_string(),
        reason: "host is not absolute",
    })
}

fn optional_field(text: &str) -> Option<String> {
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// Rebuild an archive path from `<host>|<nested>|<inner>`.
fn archive_from_token_body(body: &str, raw: &str) -> Result<FsPath, TokenError> {
    let parts = split_token_fields(body, raw, 3)?;
    let host = token_host(&parts[0], raw)?;
    FsPath::archive(host, optional_field(&parts[1]), &parts[2]).map_err(|_| TokenError {
        raw: raw.to_string(),
        reason: "inner path is not valid",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CWD: &str = "/work/tree";
    const HOME: &str = "/home/zm";

    fn resolve(raw: &str) -> Result<FsPath, CoreError> {
        FsPath::resolve(raw, Path::new(CWD), Some(Path::new(HOME)))
    }

    fn text(p: &FsPath) -> String {
        p.to_string()
    }

    /// Rows 1-6 of the `Resolve` table, plus the four adversarial inputs plan §5.2's step-3 *Done when*
    /// names (the `..`-escape rejection, `a/../b`, a CRLF name, a 255-byte name).
    #[test]
    fn vfs_table() {
        // Row 1: empty -> cwd.
        assert_eq!(text(&resolve("").unwrap()), CWD);
        // Row 2: tilde -> home, and `~x` is home/x (Go's `pathStr[1:]`, api/dir.go:52).
        assert_eq!(text(&resolve("~").unwrap()), HOME);
        assert_eq!(text(&resolve("~/notes.txt").unwrap()), "/home/zm/notes.txt");
        assert_eq!(text(&resolve("~x").unwrap()), "/home/zm/x");
        // Row 2, no home: rejected, not guessed.
        let no_home = FsPath::resolve("~/x", Path::new(CWD), None);
        assert!(matches!(
            no_home,
            Err(CoreError::InvalidPath {
                reason: PathReject::NoHome,
                ..
            })
        ));
        // Row 3: absolute, cleaned the way filepath.Clean cleans.
        assert_eq!(text(&resolve("/a/b").unwrap()), "/a/b");
        assert_eq!(text(&resolve("/a//b/").unwrap()), "/a/b");
        assert_eq!(text(&resolve("/a/./b").unwrap()), "/a/b");
        assert_eq!(text(&resolve("/a/b/../c").unwrap()), "/a/c");
        assert_eq!(text(&resolve("/").unwrap()), "/");
        // Row 4: `windows_abs` (asserted on AbsPath: on a Linux host a Windows path is opaque text).
        // Row 5: relative -> cwd-joined.
        assert_eq!(text(&resolve("sub/dir").unwrap()), "/work/tree/sub/dir");
        assert_eq!(text(&resolve("./sub").unwrap()), "/work/tree/sub");
        assert_eq!(text(&resolve(".").unwrap()), "/work/tree");
        // Row 6: the VFS split, single and nested (`/a/b.zip/inner.7z/x` is plan §5.2's own case).
        let single = resolve("/a/b.zip/inner/file.txt").unwrap();
        assert!(single.store.is_archive());
        assert_eq!(text(&single), "/a/b.zip/inner/file.txt");
        assert_eq!(single.rel(), "inner/file.txt");
        assert_eq!(
            single.archive_root().map(|p| text(&p)).as_deref(),
            Some("/a/b.zip")
        );
        let nested = resolve("/a/b.zip/inner.7z/x").unwrap();
        assert_eq!(
            nested.store.nested(),
            Some("inner.7z"),
            "second archive component is the nested store"
        );
        assert_eq!(nested.rel(), "x");
        assert_eq!(vfs_uri(&nested), "archive=/a/b.zip&nested=inner.7z&path=x");
        // ... and the store root itself.
        let root = resolve("/a/b.zip").unwrap();
        assert!(root.is_store_root());
        assert!(root.is_archive_file());
        assert_eq!(vfs_uri(&root), "archive=/a/b.zip&nested=&path=");
    }

    #[test]
    fn resolve_rejects_escaping_and_accepts_dotdot_inside() {
        // `a/../b` (row 5, then absorbed): no rejection, no escape.
        assert_eq!(text(&resolve("a/../b").unwrap()), "/work/tree/b");
        // `/../..` cannot escape `/`: filepath.Clean("/../../x") == "/x" (api/dir.go:63).
        assert_eq!(text(&resolve("/../../x").unwrap()), "/x");
        assert_eq!(text(&resolve("..").unwrap()), "/work");
        // The rejection lives in the *name* APIs, where cleaning would silently change the target.
        let dir = resolve("/work/tree").unwrap();
        assert!(matches!(
            dir.join(".."),
            Err(CoreError::InvalidPath {
                reason: PathReject::EscapesRoot,
                ..
            })
        ));
        assert!(dir.join_rel("a/../b").is_err());
        assert!(matches!(
            dir.join_rel("a/../b"),
            Err(CoreError::InvalidPath {
                reason: PathReject::EscapesRoot,
                ..
            })
        ));
        // `FsPath::new` is the same guard for a whole relative path.
        assert!(FsPath::new(VfsKind::Fs, "../x").is_err());
        assert!(FsPath::new(VfsKind::Fs, "a/../../b").is_err());
        assert!(FsPath::new(VfsKind::Fs, "a/../b").is_ok());
        // The empty name.
        assert!(matches!(
            dir.join(""),
            Err(CoreError::InvalidPath {
                reason: PathReject::Empty,
                ..
            })
        ));
    }

    #[test]
    fn dotdot_substring_is_fine() {
        // Go's `validateDest` rejected `name..txt`: the substring test is not ported.
        assert_eq!(
            text(&resolve("/tree/name..txt").unwrap()),
            "/tree/name..txt"
        );
        assert!(!contains_dot_component("name..txt"));
        assert!(!contains_dot_component("a/b..c/d"));
        assert!(contains_dot_component("a/../b"));
        assert!(contains_dot_component(".."));
        assert!(contains_dot_component("a/.."));
        assert!(!contains_dot_component("a/..b"));
    }

    #[test]
    fn crlf_and_oversized_names_survive_byte_for_byte() {
        // The fixture tree has both: `weird\rname.txt` and a 205-byte name
        // (scripts/make-fixtures.py); ext4's limit is 255 *bytes*.
        let crlf = resolve("/tree/weird\rname.txt").unwrap();
        assert_eq!(crlf.last_segment(), Some("weird\rname.txt"));
        assert_eq!(text(&crlf), "/tree/weird\rname.txt");
        let long = "x".repeat(251) + ".txt";
        assert_eq!(long.len(), 255);
        let p = resolve(&format!("/tree/{long}")).unwrap();
        assert_eq!(p.last_segment(), Some(long.as_str()));
        assert!(p.join(&long).is_ok());
        // A NUL is refused (it cannot reach a syscall at all).
        assert!(p.join("a\0b").is_err());
    }

    #[test]
    fn windows_abs() {
        // Row 4, parsed on this host too (see `AbsPath::from_windows`'s doc).
        let drive = AbsPath::from_windows("C:\\Users\\zm\\a.txt").unwrap();
        assert_eq!(drive.to_string_lossy(), "C:\\Users\\zm\\a.txt");
        assert_eq!(drive.file_name().and_then(|n| n.to_str()), Some("a.txt"));
        assert_eq!(drive.to_relative(), "C:/Users/zm/a.txt".replace('/', "\\"));
        // Mixed separators normalise to `\`.
        assert_eq!(
            AbsPath::from_windows("C:/Users/zm/x")
                .unwrap()
                .to_string_lossy(),
            "C:\\Users\\zm\\x"
        );
        // Interior `..` collapses; an escaping one is refused.
        assert_eq!(
            AbsPath::from_windows("C:\\a\\b\\..\\c")
                .unwrap()
                .to_string_lossy(),
            "C:\\a\\c"
        );
        assert!(AbsPath::from_windows("C:\\..\\x").is_err());
        // A volume root is absolute; a bare drive letter is not.
        assert_eq!(
            AbsPath::from_windows("C:\\").unwrap().to_string_lossy(),
            "C:\\"
        );
        assert!(AbsPath::from_windows("C:").is_err());
        // UNC needs host and share.
        assert_eq!(
            AbsPath::from_windows("\\\\nas\\share\\d")
                .unwrap()
                .to_string_lossy(),
            "\\\\nas\\share\\d"
        );
        assert!(AbsPath::from_windows("\\\\nas").is_err());
        // The extended-length prefix survives.
        assert_eq!(
            AbsPath::from_windows("\\\\?\\C:\\a\\b")
                .unwrap()
                .to_string_lossy(),
            "\\\\?\\C:\\a\\b"
        );
        // Row 4 dispatch: resolve() sends these to from_windows, not to the unix branch.
        assert_eq!(resolve_kind("C:\\a\\b"), ResolveKind::AbsoluteWindows);
        // `AbsPath::resolve` dispatches row 4 to `from_windows`; wrapping it in an `FsPath` on a Linux
        // host would root it at `/`, which is why the Windows rows are asserted here instead.
        assert_eq!(
            AbsPath::resolve("C:/Users/zm/x", Path::new(CWD), Some(Path::new(HOME)))
                .unwrap()
                .to_string_lossy(),
            "C:\\Users\\zm\\x"
        );
        assert_eq!(resolve_kind("/a/b"), ResolveKind::AbsoluteUnix);
        assert_eq!(resolve_kind(""), ResolveKind::Empty);
        assert_eq!(resolve_kind("~/x"), ResolveKind::Tilde);
        assert_eq!(resolve_kind("a/b"), ResolveKind::Relative);
        // Windows-only name hazards, checked where they can be checked from Linux.
        assert!(rejects_windows_reserved("data:stream"));
        assert!(rejects_windows_reserved("CON"));
        assert!(rejects_windows_reserved("com1.txt"));
        assert!(rejects_windows_reserved("\\\\?\\C:\\a"));
        assert!(!rejects_windows_reserved("normal.txt"));
        assert!(!rejects_windows_reserved("com10.txt"));
        // …and they are NOT refused on this host, where ':' is a legal filename byte.
        assert!(validate_single_name("data:stream").is_ok());
    }

    #[test]
    fn archive_cases() {
        // The component-wise split, i.e. the fake.zip fix.
        let real_dir = resolve("/tree/fake.zip/inner.txt").unwrap();
        assert!(real_dir.store.is_archive());
        assert_eq!(real_dir.rel(), "inner.txt");
        // `.zipper` is not an archive component; Go/JS would have split there.
        let not_archive = resolve("/a/b.zipper/x").unwrap();
        assert!(!not_archive.store.is_archive());
        assert_eq!(text(&not_archive), "/a/b.zipper/x");
        // Case-insensitive, like the JS lowercasing.
        assert!(resolve("/A.B.ZIP/x").unwrap().is_archive_file());
        assert!(classify_archive_name("X.RAR").is_some());
        assert_eq!(classify_archive_name("a.7z"), Some(ArchiveFormat::SevenZip));
        assert_eq!(classify_archive_name("a.zip"), Some(ArchiveFormat::Zip));
        assert_eq!(classify_archive_name("a.tar.gz"), None);
        assert!(!is_archive_name("zip"));
        // No archive component: a plain Fs path.
        assert!(!resolve("/tree/subdir/a.txt").unwrap().store.is_archive());
        // split_archive_path directly.
        let parts = split_archive_path("a/b.zip/n.7z/deep/f.txt").unwrap();
        assert_eq!(parts.nested.as_deref(), Some("n.7z"));
        assert_eq!(parts.inner, "deep/f.txt");
        assert!(split_archive_path("a/b.txt").is_none());
        // Member kinds.
        assert_eq!(MemberKind::classify("d/", true), MemberKind::Dir);
        assert_eq!(MemberKind::classify("d", true), MemberKind::Dir);
        assert_eq!(MemberKind::classify("f.txt", false), MemberKind::File);
        assert_eq!(
            MemberKind::classify("inner.7z", false),
            MemberKind::Archive(ArchiveFormat::SevenZip)
        );
        assert_eq!(MemberKind::classify("..", true), MemberKind::Other);
        assert!(!MemberKind::File.is_dir());
        // An archive store path cannot be turned into an AbsPath of itself: only the host is local.
        let arc = resolve("/a/b.zip/x").unwrap();
        assert_eq!(
            arc.host_abs().map(|p| p.to_string_lossy()),
            Some("/a/b.zip".to_string())
        );
        assert!(arc.archive_token().is_some());
        assert!(resolve("/a/b.txt").unwrap().archive_token().is_none());
    }

    #[test]
    fn fs_path_navigation() {
        let dir = FsPath::fs("/work/tree/subdir").unwrap();
        assert_eq!(dir.last_segment(), Some("subdir"));
        assert!(dir.ends_with("subdir"));
        assert!(!dir.ends_with("tree"));
        let joined = dir.join("c.txt").unwrap();
        assert_eq!(text(&joined), "/work/tree/subdir/c.txt");
        assert_eq!(joined.parent().as_ref(), Some(&dir));
        assert_eq!(
            dir.parent().map(|p| text(&p)).as_deref(),
            Some("/work/tree")
        );
        assert_eq!(
            joined.strip_prefix(&dir),
            Some("c.txt"),
            "strip_prefix borrows the suffix"
        );
        assert!(dir.strip_prefix(&joined).is_none());
        assert!(joined.starts_with(&dir));
        assert!(!dir.starts_with(&joined));
        assert_eq!(joined.relative_to(&dir).as_deref(), Some("c.txt"));
        // Root behaviour: no parent, empty rel, "/".
        assert_eq!(text(&FsPath::ROOT), "/");
        assert!(FsPath::ROOT.is_store_root());
        assert_eq!(FsPath::ROOT.parent(), None);
        assert_eq!(FsPath::ROOT.segment_iter().count(), 0);
        // relative_to with a cross-store base is not a prefix relation.
        let arc = FsPath::archive(AbsPath::from_path("/a/b.zip").unwrap(), None, "x").unwrap();
        assert!(arc.relative_to(&FsPath::ROOT).is_none());
        assert_eq!(
            arc.store_root(),
            FsPath::archive(AbsPath::from_path("/a/b.zip").unwrap(), None, "",).unwrap()
        );
        // join_rel walks several levels.
        assert_eq!(text(&FsPath::ROOT.join_rel("a/b/c").unwrap()), "/a/b/c");
        // Destination guard vs ordinary name.
        assert!(FsPath::ROOT.join("a*b.txt").is_ok());
        assert!(validate_dest_name("a*b.txt").is_err());
        assert!(validate_dest_name("a b.txt").is_ok());
        // components(): display-form components.
        assert_eq!(
            FsPath::fs("/a/b/c").unwrap().components(),
            vec!["/".to_string(), "a".into(), "b".into(), "c".into()]
        );
    }

    #[test]
    fn abs_path_ancestry_and_relativity() {
        let root = AbsPath::from_path("/").unwrap();
        let a = AbsPath::from_path("/a/b").unwrap();
        let b = AbsPath::from_path("/a/b/c/d.txt").unwrap();
        let other = AbsPath::from_path("/a/bb/c").unwrap();
        assert!(root.is_root());
        assert!(root.is_ancestor_of(&a));
        assert!(
            a.is_ancestor_of(&a),
            "equal paths are ancestors (Go's isSubdir too)"
        );
        assert!(a.is_ancestor_of(&b));
        assert!(!other.is_ancestor_of(&b), "a/bb is not a/b");
        assert!(!b.is_ancestor_of(&a));
        assert_eq!(b.relative_to(&a).as_deref(), Some("c/d.txt"));
        assert_eq!(a.relative_to(&a).as_deref(), Some(""));
        assert_eq!(a.relative_to(&b), None);
        assert_eq!(b.relative_to(&root).as_deref(), Some("a/b/c/d.txt"));
        assert_eq!(
            a.join("c").unwrap(),
            b.parent().unwrap().parent().unwrap().join("c").unwrap()
        );
        assert_eq!(
            b.parent().unwrap().file_name().and_then(|n| n.to_str()),
            Some("c")
        );
        // Non-absolute input is refused, never silently joined.
        assert!(AbsPath::from_path("a/b").is_err());
        assert!(AbsPath::from_path("").is_err());
        // Components carry no `.`/`..`.
        assert_eq!(
            AbsPath::from_path("/a/b/../c")
                .unwrap()
                .components_clean()
                .count(),
            2
        );
        assert_eq!(a.to_relative(), "a/b");
        assert_eq!(FsPath::from_abs(b.clone()).rel(), "a/b/c/d.txt");
    }

    #[test]
    fn tokens_round_trip_including_the_nasty_names() {
        let cases = [
            "/work/tree/subdir",
            "/",
            "/work/weird\rname.txt",
            "/work/pipe|name",
            "/work/pct%name",
            "/work/pipe|with%both",
            "/work/日本.txt",
        ];
        for raw in cases {
            let p = FsPath::fs(raw).unwrap();
            let token = p.dir_token();
            assert_eq!(DirToken::parse(token.as_str()).unwrap(), token);
            assert_eq!(token.to_fs_path().unwrap(), p, "roundtrip {raw}");
        }
        // Archive tokens, nested included.
        let arc = FsPath::archive(
            AbsPath::from_path("/a/b.zip").unwrap(),
            Some("inner.7z".to_string()),
            "deep/f.txt",
        )
        .unwrap();
        let token = arc.dir_token();
        assert!(token.as_str().starts_with("ar:"));
        assert_eq!(token.to_fs_path().unwrap(), arc);
        let store = arc.archive_token().unwrap();
        assert_eq!(ArchiveToken::parse(&store.encode()).unwrap(), store);
        assert_eq!(store.to_store(), arc.store);
        // Rejections.
        assert_eq!(
            DirToken::parse("nope:/x").err().map(|e| e.reason),
            Some("unknown token prefix")
        );
        assert!(
            DirToken::parse("ar:/a.zip|inner").is_err(),
            "missing inner field"
        );
        assert!(
            DirToken::parse("ar:relative|x|y").is_err(),
            "host must be absolute"
        );
        // Equality is the path equality the cache needs, not the spelling.
        let a = FsPath::fs("/a/./b").unwrap();
        let b = FsPath::fs("/a/b").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.dir_token(), b.dir_token());
    }

    #[test]
    fn classes_from_names() {
        assert_eq!(file_class("a.zip"), "Archive");
        assert_eq!(file_class("A.RAR"), "Archive");
        assert_eq!(file_class("a.png"), "Image");
        assert_eq!(file_class("a.rs"), "Text");
        assert_eq!(file_class("a.unknownext"), "File");
        assert_eq!(
            file_class(".hidden"),
            "File",
            "a leading dot is not an extension"
        );
        assert_eq!(file_class(""), "Unknown");
        assert_eq!(FileClass::LnkDir.as_str(), "Lnk-Dir");
        assert_eq!(FileClass::LnkFile.as_str(), "Lnk-File");
        assert_eq!(FileClass::LnkOther.as_str(), "Lnk-Other");
        assert_eq!(FileClass::LnkDir.to_string(), "Lnk-Dir");
        assert_eq!(
            class_from_name("a.zip"),
            FileClass::Archive(ArchiveFormat::Zip)
        );
    }
}
