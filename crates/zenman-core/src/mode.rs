//! Mode bits and the `mode` string the wire format carries (plan §5.2 `mode.rs` row).
//!
//! Go never formatted a mode itself: `api/dir.go:235-247` writes `info.Mode().String()`, and the golden
//! inventory (`docs/contracts.md` → *fileMode strings*) is this module's spec. Two things there are not
//! what a Unix programmer expects:
//!
//! * a symlink renders with a **capital `L`** (`Lrwxrwxrwx`, `dir-root.json` `link-to-dir`) — that is Go's
//!   `fs.FileMode` type glyph, not `ls`'s lowercase `l`;
//! * `unknown` is a *mode value* on the wire, produced when `entry.Info()` failed (`api/dir.go:236`,
//!   `dir-perm-noexec.json`) — hence [`ModeKind::Unknown`] / [`ModeKind::from_error`], and the degraded
//!   row's `size: 0` + `mod_time: -62135596800` pair (plan F20, [`crate::metrics::GO_ZERO_TIME_UNIX`]).
//!
//! The algorithm reproduced is `fs.FileMode.String()` of the Go distribution on this machine —
//! `/media/jang/home/PortableApp/go/src/io/fs/fs.go:215-239`: the prefix-glyph run `"dalTLDpSugct?"`
//! tested against the high bits of a `uint32`, a single `-` when none matched, then nine `rwx`/`-` glyphs
//! from the low nine bits. Note what that implies for setuid and sticky: Go emits **prefix** letters
//! (`urwxr-xr-x`), not `ls`'s in-column `s`/`S`/`t`/`T` — so `docs/contracts.md`'s remark that the `S`/`T`
//! glyphs are untested is understated, they are unreachable (step-3 report).
//!
//! Everything here is `cfg`-neutral and takes or returns plain integers — [`mod_bits_to_finfo`],
//! [`attributes_to_mode`], [`mode_to_attributes`] — so the Windows mapping is unit-testable from Linux and
//! no `windows-sys` type reaches a public signature.
//!
//! [`EntryKind`] and [`EntryLike`] live in [`crate::entry`] (plan §5.2's `entry.rs` row): they describe an
//! *entry*, and [`ModeKind::from_entry`] is the one place mode reading needs them. They are re-exported
//! here because the mode and entry modules are read together.

use std::fmt::{self, Write as _};

use bitflags::bitflags;

use crate::entry::EntryLike;
use crate::error::CoreError;

bitflags! {
    /// The permission and setuid/setgid/sticky bits of a Unix mode, as raw octal values (not Go's
    /// high-bit layout: that translation is [`mod_bits_to_finfo`]).
    ///
    /// This is what [`crate::platform::Env::chmod`] receives and what [`parse_octal`] produces, so the
    /// octal a user typed is the octal that is stored — no implicit normalisation on the way.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct ModeBits: u32 {
        /// `0o400`.
        const READ_USER   = 0o400;
        /// `0o200`.
        const WRITE_USER  = 0o200;
        /// `0o100`.
        const EXEC_USER   = 0o100;
        /// `0o040`.
        const READ_GROUP  = 0o040;
        /// `0o020`.
        const WRITE_GROUP = 0o020;
        /// `0o010`.
        const EXEC_GROUP  = 0o010;
        /// `0o004`.
        const READ_OTHER  = 0o004;
        /// `0o002`.
        const WRITE_OTHER = 0o002;
        /// `0o001`.
        const EXEC_OTHER  = 0o001;
        /// `0o4000`, rendered as Go's `u` prefix glyph.
        const SET_UID     = 0o4000;
        /// `0o2000`, rendered as Go's `g` prefix glyph.
        const SET_GID     = 0o2000;
        /// `0o1000`, rendered as Go's `t` prefix glyph.
        const STICKY      = 0o1000;
        /// The nine permission bits, no special bits.
        const PERMS = Self::READ_USER.bits() | Self::WRITE_USER.bits() | Self::EXEC_USER.bits()
            | Self::READ_GROUP.bits() | Self::WRITE_GROUP.bits() | Self::EXEC_GROUP.bits()
            | Self::READ_OTHER.bits() | Self::WRITE_OTHER.bits() | Self::EXEC_OTHER.bits();
        /// Every bit this type models.
        const ALL = Self::PERMS.bits() | Self::SET_UID.bits() | Self::SET_GID.bits() | Self::STICKY.bits();
    }
}

impl ModeBits {
    /// `0o644` — what the fixture generator writes for an ordinary file
    /// (`scripts/make-fixtures.py`, `mode=0o644` → `-rw-r--r--`).
    pub const DEFAULT_FILE: ModeBits = ModeBits::from_bits_retain(0o644);

    /// `0o755` — `executable.sh` in the same tree.
    pub const EXECUTABLE: ModeBits = ModeBits::from_bits_retain(0o755);

    /// The nine `rwx`/`-` glyphs, highest bit first (Go's `rwx = "rwxrwxrwx"` loop, `io/fs/fs.go:229-237`).
    #[must_use]
    pub fn rwx_glyphs(self) -> [char; 9] {
        let bits = self.bits();
        [
            glyph(bits, 0o400, 'r'),
            glyph(bits, 0o200, 'w'),
            glyph(bits, 0o100, 'x'),
            glyph(bits, 0o040, 'r'),
            glyph(bits, 0o020, 'w'),
            glyph(bits, 0o010, 'x'),
            glyph(bits, 0o004, 'r'),
            glyph(bits, 0o002, 'w'),
            glyph(bits, 0o001, 'x'),
        ]
    }

    /// The permission half of a raw `st_mode`, special bits included (`mode & 0o7777`).
    #[must_use]
    pub const fn from_unix_mode(raw: u32) -> Self {
        Self::from_bits_truncate(raw & 0o7777)
    }

    /// The raw octal value.
    #[must_use]
    pub const fn bits_octal(self) -> u32 {
        self.bits()
    }
}

const fn glyph(bits: u32, mask: u32, on: char) -> char {
    if bits & mask != 0 { on } else { '-' }
}

impl fmt::Display for ModeBits {
    /// The nine permission glyphs and nothing else. Setuid/setgid/sticky are *not* rendered here: Go puts
    /// them in the prefix run, which needs the file type, so see [`ModeKind`]'s `Display`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for c in self.rwx_glyphs() {
            f.write_char(c)?;
        }
        Ok(())
    }
}

/// Parse the octal text the UI accepts: exactly three or four octal digits (`^[0-7]{3,4}$`).
///
/// The measured Go rule is `strconv.ParseUint(s, 8, 32)` (`api/ops.go:931`), which accepts `"600"` and
/// `"0644"` and rejects `"0o755"` (`docs/contracts.md` *Deviation ledger* #7). The port is stricter than
/// Go in one direction — Go would take `"75"` as `0o75` = `0o075`, a mode nobody meant — and identical in
/// the direction the goldens pin.
pub fn parse_octal(raw: &str) -> Result<ModeBits, CoreError> {
    let bytes = raw.as_bytes();
    if !matches!(bytes.len(), 3 | 4) {
        return Err(invalid_mode(raw));
    }
    if !bytes.iter().all(|b| b.is_ascii_digit() && *b < b'8') {
        return Err(invalid_mode(raw));
    }
    let mut value = 0u32;
    for b in bytes {
        value = value * 8 + u32::from(b - b'0');
    }
    Ok(ModeBits::from_bits_truncate(value))
}

/// Go's `api/ops.go:931-933` text is `Invalid permission mode: <ParseUint error>`; because it was built
/// with `%v` it is the one golden that is not valid JSON (`err-chmod-octaltext.json`). Ours keeps the
/// prose and lets serde do the quoting — `docs/contracts.md` → *The error envelope*.
fn invalid_mode(raw: &str) -> CoreError {
    CoreError::InvalidMode {
        raw: raw.to_string(),
    }
}

/// Go's `fs.FileMode` bit layout (`io/fs/fs.go:191-212`), the numeric form of the strings on the wire.
///
/// Kept as its own module because the values are part of *Go's* public ABI ("may be used in wire
/// protocols or disk representations", the comment on that const block) and must not drift with ours.
pub mod finfo {
    /// `1 << 31` — directory.
    pub const MODE_DIR: u32 = 1 << 31;
    /// `1 << 30` — append-only.
    pub const MODE_APPEND: u32 = 1 << 30;
    /// `1 << 29` — exclusive use.
    pub const MODE_EXCLUSIVE: u32 = 1 << 29;
    /// `1 << 28` — temporary (Plan 9 only).
    pub const MODE_TEMPORARY: u32 = 1 << 28;
    /// `1 << 27` — symbolic link.
    pub const MODE_SYMLINK: u32 = 1 << 27;
    /// `1 << 26` — device file.
    pub const MODE_DEVICE: u32 = 1 << 26;
    /// `1 << 25` — named pipe (FIFO).
    pub const MODE_NAMED_PIPE: u32 = 1 << 25;
    /// `1 << 24` — Unix domain socket.
    pub const MODE_SOCKET: u32 = 1 << 24;
    /// `1 << 23` — setuid.
    pub const MODE_SETUID: u32 = 1 << 23;
    /// `1 << 22` — setgid.
    pub const MODE_SETGID: u32 = 1 << 22;
    /// `1 << 21` — character device, when [`MODE_DEVICE`] is also set.
    pub const MODE_CHAR_DEVICE: u32 = 1 << 21;
    /// `1 << 20` — sticky.
    pub const MODE_STICKY: u32 = 1 << 20;
    /// `1 << 19` — non-regular file, nothing else known.
    pub const MODE_IRREGULAR: u32 = 1 << 19;
    /// `0o777`.
    pub const MODE_PERM: u32 = 0o777;

    /// The prefix-glyph run of `FileMode.String()` (`io/fs/fs.go:216`), bit 31 downwards.
    pub const PREFIX_GLYPHS: &str = "dalTLDpSugct?";
}

/// Go's `fs.FileMode.String()` (`io/fs/fs.go:215-239`), verbatim: the prefix glyph run tested against the
/// high bits, `-` when no prefix bit is set, then the nine permission glyphs.
#[must_use]
pub fn finfo_string(mode: u32) -> String {
    let mut out = String::with_capacity(11);
    for (i, c) in finfo::PREFIX_GLYPHS.char_indices() {
        if mode & (1 << (31 - i)) != 0 {
            out.push(c);
        }
    }
    if out.is_empty() {
        out.push('-');
    }
    for c in ModeBits::from_bits_truncate(mode & finfo::MODE_PERM).rwx_glyphs() {
        out.push(c);
    }
    out
}

/// Translate a raw Unix `st_mode` into Go's `fs.FileMode` value: exactly
/// `os/stat_linux.go:20-40` (`fillFileStatFromSys`) — `mode & 0777` then the type bits from `S_IFMT`, then
/// the setuid/setgid/sticky bits.
#[must_use]
pub const fn mod_bits_to_finfo(st_mode: u32) -> u32 {
    const S_IFMT: u32 = 0o170000;
    const S_IFSOCK: u32 = 0o140000;
    const S_IFLNK: u32 = 0o120000;
    const S_IFBLK: u32 = 0o060000;
    const S_IFDIR: u32 = 0o040000;
    const S_IFCHR: u32 = 0o020000;
    const S_IFIFO: u32 = 0o010000;
    let mut out = st_mode & finfo::MODE_PERM;
    match st_mode & S_IFMT {
        S_IFBLK => out |= finfo::MODE_DEVICE,
        S_IFCHR => out |= finfo::MODE_DEVICE | finfo::MODE_CHAR_DEVICE,
        S_IFDIR => out |= finfo::MODE_DIR,
        S_IFIFO => out |= finfo::MODE_NAMED_PIPE,
        S_IFLNK => out |= finfo::MODE_SYMLINK,
        S_IFSOCK => out |= finfo::MODE_SOCKET,
        // S_IFREG and anything else: no type bits, exactly as Go's switch falls through.
        _ => {}
    }
    if st_mode & 0o2000 != 0 {
        out |= finfo::MODE_SETGID;
    }
    if st_mode & 0o4000 != 0 {
        out |= finfo::MODE_SETUID;
    }
    if st_mode & 0o1000 != 0 {
        out |= finfo::MODE_STICKY;
    }
    out
}

/// `FILE_ATTRIBUTE_READONLY` (`api` equivalent: `syscall/types_windows.go:123`).
pub const FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;
/// `FILE_ATTRIBUTE_DIRECTORY`.
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
/// `FILE_ATTRIBUTE_REPARSE_POINT` (the form Go uses for symlinks/junctions).
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// Windows file attributes to permission bits: a file is `0o666` (or `0o444` when read-only), a directory
/// keeps the execute bits that make it traversable (`Go's Windows `os.FileInfo.Mode()` shape:
/// `FILE_ATTRIBUTE_DIRECTORY` → dir, `FILE_ATTRIBUTE_READONLY` → no write bits).
///
/// Behaviour-derived rather than copied — the exact Windows `Mode()` source was not re-read in this step
/// (see the step-3 report); the *values* of the attribute constants are from the Go distribution
/// (`syscall/types_windows.go:123-129`).
#[must_use]
pub const fn attributes_to_mode(attributes: u32) -> ModeBits {
    let mut bits = if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        0o777
    } else {
        0o666
    };
    if attributes & FILE_ATTRIBUTE_READONLY != 0 {
        bits &= !0o222;
    }
    ModeBits::from_bits_truncate(bits)
}

/// The inverse direction, for the Windows `chmod` of step 16. Windows has one read-only flag, so the
/// rule is coarse: a *file* is read-only unless every write bit is set (the base permission set Go
/// reads back for a non-readonly file is `0666`, so anything less is a restriction), while a
/// *directory* is read-only only when its owner cannot write it (`drwxr-xr-x` is a normal, writable
/// directory on the Unix side and must not map to `READONLY`).
#[must_use]
pub const fn mode_to_attributes(bits: ModeBits, is_dir: bool) -> u32 {
    let mut attrs = if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { 0 };
    let write_mask = if is_dir {
        ModeBits::WRITE_USER.bits()
    } else {
        ModeBits::WRITE_USER.bits() | ModeBits::WRITE_GROUP.bits() | ModeBits::WRITE_OTHER.bits()
    };
    if bits.bits() & write_mask != write_mask {
        attrs |= FILE_ATTRIBUTE_READONLY;
    }
    attrs
}

/// The kind of a mode, with its permission bits: the typed form of the wire `mode` string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModeKind {
    /// A regular file: `-rw-r--r--`.
    Regular(ModeBits),
    /// A directory: `drwxr-xr-x`.
    Dir(ModeBits),
    /// A symlink: `Lrwxrwxrwx` (capital `L`).
    Symlink(ModeBits),
    /// A FIFO: `prw-r--r--`.
    Fifo(ModeBits),
    /// A Unix domain socket: `S…`.
    Socket(ModeBits),
    /// A character device: `Dc…`.
    CharDevice(ModeBits),
    /// A block device: `D…`.
    BlockDevice(ModeBits),
    /// The hardcoded archive-member mode, `-r--r--r--` ([`ArchiveMode`]).
    Archive,
    /// `entry.Info()` failed, so the string on the wire is literally `unknown` (`api/dir.go:236`).
    Unknown,
}

impl ModeKind {
    /// Derive the mode of one directory entry: its `st_mode` when the stat worked,
    /// [`ModeKind::Unknown`] when it did not (Go's `mode = "unknown"` default, `api/dir.go:236`).
    ///
    /// The `is_dir` field of the wire entry is *not* derived here: that is plan §5.2's `entry.rs` rule
    /// (`is_dir = kind.is_dir() || kind.is_symlink_to_dir()`), which needs the link target and so lives in
    /// plan §5.2's `entry.rs` `EntryBuilder`.
    #[must_use]
    pub fn from_entry(entry: &impl EntryLike) -> Self {
        if let Some(raw) = entry.unix_mode() {
            return Self::from_unix(raw);
        }
        if let Some(attrs) = entry.windows_attributes() {
            return Self::from_windows(attrs);
        }
        Self::from_error()
    }

    /// From a raw Unix `st_mode` (type bits included) — the `#[cfg(unix)]` producer.
    #[must_use]
    pub const fn from_unix(st_mode: u32) -> Self {
        let perm = ModeBits::from_unix_mode(st_mode);
        match st_mode & 0o170000 {
            0o040000 => Self::Dir(perm),
            0o120000 => Self::Symlink(perm),
            0o010000 => Self::Fifo(perm),
            0o140000 => Self::Socket(perm),
            0o020000 => Self::CharDevice(perm),
            0o060000 => Self::BlockDevice(perm),
            _ => Self::Regular(perm),
        }
    }

    /// From Windows file attributes — the `#[cfg(windows)]` producer.
    #[must_use]
    pub const fn from_windows(attributes: u32) -> Self {
        let perm = attributes_to_mode(attributes);
        if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Self::Symlink(perm);
        }
        if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            return Self::Dir(perm);
        }
        Self::Regular(perm)
    }

    /// The archive-member mode (`archive_vfs.go:29`).
    #[must_use]
    pub const fn from_archive() -> Self {
        Self::Archive
    }

    /// The degraded-row mode, `unknown` (`api/dir.go:236`).
    #[must_use]
    pub const fn from_error() -> Self {
        Self::Unknown
    }

    /// Parse a wire `mode` string back into a kind — a best-effort inverse of `Display`, used for
    /// restoring a deserialised entry. An unrecognised string is [`ModeKind::Unknown`], which is also the
    /// value that produces the `unknown` spelling, so the pair round-trips. When the numeric mode is
    /// available (a live `stat`), use [`ModeKind::from_unix`] instead: it is exact.
    #[must_use]
    pub fn parse_mode(s: &str) -> Self {
        match s {
            "unknown" => Self::Unknown,
            "-r--r--r--" => Self::Archive,
            other => match other.chars().next() {
                Some('d') => Self::Dir(perm_of(other)),
                Some('L') => Self::Symlink(perm_of(other)),
                Some('p') => Self::Fifo(perm_of(other)),
                Some('S') => Self::Socket(perm_of(other)),
                // Go renders a character device as the two-glyph prefix `Dc`.
                Some('D') if other.starts_with("Dc") => Self::CharDevice(perm_of(other)),
                Some('D') => Self::BlockDevice(perm_of(other)),
                Some('-') => Self::Regular(perm_of(other)),
                // Go's multi-glyph prefixes (`u`, `g`, `t`, and combinations) carry the type in the
                // trailing glyph position; the simple cases here cover every value the goldens contain.
                _ => Self::Unknown,
            },
        }
    }

    /// The permission bits, `None` for [`ModeKind::Unknown`].
    #[must_use]
    pub const fn bits(self) -> Option<ModeBits> {
        match self {
            Self::Regular(b)
            | Self::Dir(b)
            | Self::Symlink(b)
            | Self::Fifo(b)
            | Self::Socket(b)
            | Self::CharDevice(b)
            | Self::BlockDevice(b) => Some(b),
            Self::Archive => Some(ModeBits::from_bits_retain(0o444)),
            Self::Unknown => None,
        }
    }

    /// `true` for a directory mode (`drwxr-xr-x`), never for a symlink to one — that distinction is
    /// [`EntryKind::SymlinkDir`] and plan §5.2's `is_dir` rule.
    #[must_use]
    pub const fn is_dir(self) -> bool {
        matches!(self, Self::Dir(_))
    }

    /// `true` for a symlink mode.
    #[must_use]
    pub const fn is_symlink(self) -> bool {
        matches!(self, Self::Symlink(_))
    }

    /// Go's `fs.FileMode` numeric value, `None` for [`ModeKind::Unknown`] (which has no numeric form: it
    /// is a Go *string* fallback, not a mode).
    #[must_use]
    pub const fn finfo_bits(self) -> Option<u32> {
        let (perm, extra) = match self {
            Self::Regular(b) => (b, 0),
            Self::Dir(b) => (b, finfo::MODE_DIR),
            Self::Symlink(b) => (b, finfo::MODE_SYMLINK),
            Self::Fifo(b) => (b, finfo::MODE_NAMED_PIPE),
            Self::Socket(b) => (b, finfo::MODE_SOCKET),
            Self::CharDevice(b) => (b, finfo::MODE_DEVICE | finfo::MODE_CHAR_DEVICE),
            Self::BlockDevice(b) => (b, finfo::MODE_DEVICE),
            Self::Archive => (ModeBits::from_bits_retain(0o444), 0),
            Self::Unknown => return None,
        };
        let special = perm.bits() & 0o7000;
        Some((perm.bits() & finfo::MODE_PERM) | extra | special_prefix(special))
    }
}

/// Translate the raw `0o7000` special bits into Go's high-bit positions.
const fn special_prefix(special: u32) -> u32 {
    let mut out = 0;
    if special & 0o4000 != 0 {
        out |= finfo::MODE_SETUID;
    }
    if special & 0o2000 != 0 {
        out |= finfo::MODE_SETGID;
    }
    if special & 0o1000 != 0 {
        out |= finfo::MODE_STICKY;
    }
    out
}

/// The nine permission glyphs of a mode string: Go always emits exactly nine, last, whatever prefix
/// letters came before them (`io/fs/fs.go:229-237`).
fn perm_of(mode: &str) -> ModeBits {
    let mut bits = 0u32;
    let mut perms: Vec<char> = mode.chars().rev().take(9).collect();
    perms.reverse();
    for (i, c) in perms.into_iter().enumerate() {
        let on = match i % 3 {
            0 => c == 'r',
            1 => c == 'w',
            _ => c == 'x',
        };
        if on {
            bits |= 1 << (8 - i);
        }
    }
    ModeBits::from_bits_truncate(bits)
}

impl fmt::Display for ModeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Archive => f.write_str("-r--r--r--"),
            Self::Unknown => f.write_str("unknown"),
            other => match other.finfo_bits() {
                Some(bits) => f.write_str(&finfo_string(bits)),
                None => f.write_str("unknown"),
            },
        }
    }
}

/// The archive-member mode, whose `Display` is the string Go hardcodes for **every** archive row:
/// `Mode: "-r--r--r--", // read-only visual cue` (`api/archive_vfs.go:29`, and every
/// `archive-list-*.json`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ArchiveMode;

/// The hardcoded archive mode string, for tests and for anything that must not go through `Display`.
pub const ARCHIVE_MODE_STR: &str = "-r--r--r--";

impl ArchiveMode {
    /// The mode string Go writes.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        ARCHIVE_MODE_STR
    }

    /// The same value as a [`ModeKind`].
    #[must_use]
    pub const fn to_mode_kind(self) -> ModeKind {
        ModeKind::Archive
    }
}

impl fmt::Display for ArchiveMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::entry::{DirEntryLike, EntryKind, InfoEntry};

    /// Every mode string in `docs/contracts.md` → *fileMode strings*, plus the special-bit cases the
    /// goldens could not contain. The expected values are **generated**: `go run` of a walker over the
    /// fixture tree (Go 1.x on this machine, `/media/jang/home/PortableApp/go`) printed
    /// `info.Mode().String()` for each entry, and the synthetic rows below are the same
    /// `fs.FileMode.String()` algorithm (`io/fs/fs.go:215-239`) applied by hand to modes the tree has no
    /// file for — see `finfo_string_is_the_go_algorithm` for the numeric half of the check.
    #[test]
    fn bits_match_go_glyphs() {
        let cases: [(u32, &str); 12] = [
            // Measured on this machine against the Go implementation:
            (0o100644, "-rw-r--r--"), // tree/a9.txt & friends
            (0o100755, "-rwxr-xr-x"), // tree/executable.sh
            (0o100000, "----------"), // tree/noperm.bin, mode 000
            (0o040755, "drwxr-xr-x"), // tree/subdir
            (0o040444, "dr--r--r--"), // tree/noexec-dir
            (0o040000, "d---------"), // tree/noread-dir
            (0o120777, "Lrwxrwxrwx"), // tree/link-to-dir — capital L
            (0o010644, "prw-r--r--"), // tree/myfifo
            // Synthetic (the tree has no such object, by design — contracts.md *Fixtures we cannot
            // generate*: no setuid binary, no socket, no device node).
            (0o140777, "Srwxrwxrwx"),  // AF_UNIX socket
            (0o020666, "Dcrw-rw-rw-"), // character device: Go sets ModeDevice *and* ModeCharDevice
            (0o060644, "Drw-r--r--"),  // block device
            (0o104755, "urwxr-xr-x"),  // setuid: a *prefix* glyph, not `ls`'s `s`
        ];
        for (raw, expected) in cases {
            let kind = ModeKind::from_unix(raw);
            assert_eq!(kind.to_string(), expected, "st_mode {raw:#o}");
            assert_eq!(
                finfo_string(mod_bits_to_finfo(raw)),
                expected,
                "finfo path disagrees for {raw:#o}"
            );
        }
        // setgid + sticky + setuid together: the prefix run is in bit order, `u`, `g`, `t`.
        // setgid+setuid+sticky together: the prefix run is in bit order, `u` `g` `t`, and there is no
        // `-` at all (Go's `w != 0` guard, io/fs/fs.go:225).
        assert_eq!(ModeKind::from_unix(0o107777).to_string(), "ugtrwxrwxrwx");
        // The two non-numeric wire values.
        assert_eq!(ModeKind::from_error().to_string(), "unknown");
        assert_eq!(ModeKind::from_archive().to_string(), "-r--r--r--");
        assert_eq!(ArchiveMode.to_string(), "-r--r--r--");
        assert_eq!(ArchiveMode.as_str(), ARCHIVE_MODE_STR);
        assert_eq!(ArchiveMode.to_mode_kind(), ModeKind::Archive);
    }

    #[test]
    fn finfo_string_is_the_go_algorithm() {
        // The bit positions are Go's, and they are ABI (io/fs/fs.go:191-212).
        assert_eq!(finfo::MODE_DIR, 1 << 31);
        assert_eq!(finfo::MODE_SYMLINK, 1 << 27);
        assert_eq!(finfo::MODE_NAMED_PIPE, 1 << 25);
        assert_eq!(finfo::MODE_SOCKET, 1 << 24);
        assert_eq!(finfo::MODE_CHAR_DEVICE, 1 << 21);
        assert_eq!(finfo::MODE_STICKY, 1 << 20);
        assert_eq!(finfo::MODE_PERM, 0o777);
        assert_eq!(finfo::PREFIX_GLYPHS, "dalTLDpSugct?");
        // The glyph run is exactly one letter per defined high bit, in that order.
        assert_eq!(finfo::PREFIX_GLYPHS.len(), 13);
        // A mode with no type bits gets the single `-` (`io/fs/fs.go:225-228`).
        assert_eq!(finfo_string(0o644), "-rw-r--r--");
        assert_eq!(finfo_string(0), "----------");
        // Multiple prefix glyphs concatenate: no `-` is emitted (`w != 0` after the loop).
        assert_eq!(
            finfo_string(finfo::MODE_DEVICE | finfo::MODE_CHAR_DEVICE | 0o666),
            "Dcrw-rw-rw-"
        );
        // mod_bits_to_finfo is the Go mapping, not a reimplementation of the string.
        assert_eq!(mod_bits_to_finfo(0o120777), finfo::MODE_SYMLINK | 0o777);
        assert_eq!(mod_bits_to_finfo(0o040755), finfo::MODE_DIR | 0o755);
        assert_eq!(
            mod_bits_to_finfo(0o020666),
            finfo::MODE_DEVICE | finfo::MODE_CHAR_DEVICE | 0o666
        );
        assert_eq!(mod_bits_to_finfo(0o100644), 0o644);
        assert_eq!(
            mod_bits_to_finfo(0o102755),
            finfo::MODE_SETGID | 0o755,
            "setgid moves to the high bit, permission copy is masked off"
        );
    }

    #[test]
    fn mode_bits_render_and_roundtrip() {
        assert_eq!(ModeBits::DEFAULT_FILE.to_string(), "rw-r--r--");
        assert_eq!(ModeBits::EXECUTABLE.to_string(), "rwxr-xr-x");
        assert_eq!(ModeBits::empty().to_string(), "---------");
        assert!(ModeBits::empty().is_empty());
        assert_eq!(ModeBits::from_unix_mode(0o104755).bits_octal(), 0o4755);
        // Every permission triple over 0..=0o777 renders the right glyph count (a cheap invariant sweep
        // rather than 512 assertions).
        for perm in 0..0o1000u32 {
            let bits = ModeBits::from_bits_truncate(perm);
            let s = bits.to_string();
            assert_eq!(s.chars().count(), 9);
            assert!(
                s.chars().all(|c| matches!(c, 'r' | 'w' | 'x' | '-')),
                "{perm:#o} -> {s}"
            );
            let back = perm_of(&format!("-{s}"));
            assert_eq!(back & ModeBits::PERMS, bits & ModeBits::PERMS);
        }
    }

    #[test]
    fn parse_octal_follows_the_measured_chmod_rule() {
        // op-chmod.json ("600" -> 200) and op-chmod-leading-zero.json ("0644" -> 200).
        assert_eq!(parse_octal("600").unwrap().bits_octal(), 0o600);
        assert_eq!(parse_octal("0644").unwrap().bits_octal(), 0o644);
        assert_eq!(parse_octal("0000").unwrap().bits_octal(), 0);
        assert_eq!(parse_octal("7777").unwrap().bits_octal(), 0o7777);
        // err-chmod-octaltext.json ("0o755" -> 400).
        for bad in ["0o755", "75", "75555", "888", "77z", "", "-1", " 755"] {
            let err = parse_octal(bad).err();
            match err {
                Some(CoreError::InvalidMode { raw }) => assert_eq!(raw, bad),
                other => panic!("expected InvalidMode for {bad:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn archive_mode_is_hardcoded_and_has_no_type_bits() {
        // archive_vfs.go:29 — every archive row, whatever the container says.
        assert_eq!(ModeKind::from_archive(), ModeKind::Archive);
        assert_eq!(ModeKind::parse_mode("-r--r--r--"), ModeKind::Archive);
        assert_eq!(ModeKind::Archive.finfo_bits(), Some(0o444));
        assert_eq!(
            ModeKind::Archive.bits().map(|b| b.bits_octal()),
            Some(0o444)
        );
    }

    #[test]
    fn parse_mode_covers_every_golden_value() {
        // The complete inventory of docs/contracts.md → fileMode strings.
        let goldens = [
            "-rw-r--r--",
            "-rwxr-xr-x",
            "----------",
            "drwxr-xr-x",
            "dr--r--r--",
            "d---------",
            "Lrwxrwxrwx",
            "prw-r--r--",
            "-r--r--r--",
            "unknown",
        ];
        for g in goldens {
            let kind = ModeKind::parse_mode(g);
            assert_eq!(kind.to_string(), g, "round trip {g}");
        }
        assert_eq!(ModeKind::parse_mode(""), ModeKind::Unknown);
        assert_eq!(ModeKind::Unknown.to_string(), "unknown");
        assert_eq!(ModeKind::Unknown.finfo_bits(), None);
    }

    #[test]
    fn windows_attribute_mapping_is_pure() {
        // No windows-sys type in sight: the whole mapping is over u32 (plan §5.2 windows row).
        assert_eq!(
            attributes_to_mode(FILE_ATTRIBUTE_DIRECTORY).bits_octal(),
            0o777
        );
        assert_eq!(attributes_to_mode(0).bits_octal(), 0o666);
        assert_eq!(
            attributes_to_mode(FILE_ATTRIBUTE_READONLY).bits_octal(),
            0o444
        );
        assert_eq!(
            ModeKind::from_windows(FILE_ATTRIBUTE_DIRECTORY).to_string(),
            "drwxrwxrwx"
        );
        assert_eq!(
            ModeKind::from_windows(FILE_ATTRIBUTE_READONLY).to_string(),
            "-r--r--r--"
        );
        // A reparse point is Go's symlink form, and the base permission set for a non-directory is 0666.
        assert_eq!(
            ModeKind::from_windows(FILE_ATTRIBUTE_REPARSE_POINT).to_string(),
            "Lrw-rw-rw-"
        );
        assert_eq!(
            mode_to_attributes(ModeBits::from_bits_retain(0o644), false),
            FILE_ATTRIBUTE_READONLY
        );
        assert_eq!(
            mode_to_attributes(ModeBits::from_bits_retain(0o666), false),
            0
        );
        assert_eq!(
            mode_to_attributes(ModeBits::from_bits_retain(0o755), true),
            FILE_ATTRIBUTE_DIRECTORY
        );
    }

    /// [`ModeKind::from_entry`] reads an [`EntryLike`]; the three cases that matter are a directory, a
    /// link to one, and a stat that failed (`unknown`, plan F20). `EntryLike` is sealed in
    /// [`crate::entry`], so this uses one of the four producers rather than a local stub.
    #[test]
    fn from_entry_uses_the_kind_and_the_stat_result() {
        let entry = |kind, st| InfoEntry::new("stub", kind, 0, None, st);
        assert_eq!(
            ModeKind::from_entry(&entry(EntryKind::Dir, Some(0o040_755))),
            ModeKind::Dir(ModeBits::from_bits_retain(0o755))
        );
        assert_eq!(
            ModeKind::from_entry(&entry(EntryKind::SymlinkDir, Some(0o120_777))),
            ModeKind::Symlink(ModeBits::from_bits_retain(0o777))
        );
        // The stat failed: `unknown`, which is the whole point of F20's degraded row.
        assert_eq!(
            ModeKind::from_entry(&entry(EntryKind::File, None)),
            ModeKind::Unknown
        );
        // A dirent that has never been statted answers the same way (`DirEntryLike` has no mode at all).
        assert_eq!(
            ModeKind::from_entry(&DirEntryLike::new("stub", EntryKind::Dir)),
            ModeKind::Unknown
        );
    }
}
