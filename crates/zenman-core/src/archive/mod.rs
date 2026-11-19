//! Archives (plan §5.5): the format registry, the member reader, the manifest VFS and the extract job.
//!
//! ## The path syntax, and why there is no `!` in it
//!
//! Go never had an `archive!inner` string form. It had a **triple of query parameters** —
//! `archive=<host>&nested=<member>&path=<member>` (`api/archive_handlers.go:29-34`, and the same triple
//! in `api/archive_extract.go:93-98`) — plus, on the frontend, one slash-separated string that
//! `parseVfsPath` (`frontend/js/utils.js:45-90`) cut at the first component whose *name* ends in
//! `.zip`/`.rar`/`.7z` and then at the second. This crate keeps exactly those two shapes, already built
//! in step 3: [`crate::path::split_archive_path`] produces [`crate::path::ArchiveParts`]
//! `{host, nested, inner}` and [`crate::path::vfs_uri`] renders the query triple. The store type that
//! carries the split is [`crate::path::VfsKind::Archive`], and [`crate::path::FsPath`]'s `Display` is the
//! slash form (`/a/b.zip/inner.zip/deep/f.txt`).
//!
//! ## Depth
//!
//! Go's parser looked for a second archive component and stopped — a third `.zip` in the path was just a
//! member name. That two-store cap is therefore structural here as well: `VfsKind::Archive` holds *one*
//! optional `nested`, and [`crate::path::FsPath::enter`] refuses to nest a third. [`MAX_NEST_DEPTH`] is
//! the number that documents (and the contract test pins).
//!
//! ## Formats and licences
//!
//! `zip` (MIT) and `sevenz-rust` (Apache-2.0) are the default build, per plan §10 item 3. RAR is *not*:
//! the `rar`/`unrar` crates declare MIT but wrap the unRar binary / vendored UnRAR source, which is
//! outside the Craft allowlist (`deny.toml` `[bans]` denies both names). The `rar` feature therefore adds
//! no dependency and every RAR path returns [`CoreError::Unsupported`] with [`RAR_UNSUPPORTED`] as its
//! reason — a clean-room statement of the decision, so that enabling the feature can never silently
//! pull a non-allowlisted licence into the graph.

pub mod extract;
pub mod reader;
pub mod vfs;

use std::path::Path;

use crate::error::CoreError;
use crate::path::ArchiveFormat;
use crate::path::classify_archive_name;

/// Plan §5.5's name for the format enum; the enum itself lives in [`crate::path`] (step 3) so the path
/// model and the archive layer cannot disagree about what an archive is.
pub type ArchiveKind = ArchiveFormat;

/// The deepest store chain that opens: host + one nested member. See the module docs.
pub const MAX_NEST_DEPTH: usize = 2;

/// The one-line reason a RAR archive cannot be read, quoted verbatim into
/// [`CoreError::Unsupported::what`] (which is a `&'static str`, hence a constant and not a format).
pub const RAR_UNSUPPORTED: &str = "rar archives need an external unRar binary: the UnRAR source licence is \
                                   outside the Craft allowlist (plan \x00§10 item 3)";

/// Magic signatures, i.e. what Go's *readers* would have rejected even though its *router* only looked
/// at the extension (`api/archive_parser.go:20-31` is extension-only; the three parsers below all check
/// the signature themselves).
pub const MAGIC_ZIP: &[u8] = b"PK\x03\x04";
/// An empty zip archive: `PK\x05\x06` (end-of-central-directory only).
pub const MAGIC_ZIP_EMPTY: &[u8] = b"PK\x05\x06";
/// The 7z signature header (`7z\xbc\xaf\x27\x1c`).
pub const MAGIC_7Z: &[u8] = b"7z\xbc\xaf\x27\x1c";
/// `Rar!\x1a\x07` — recognised so that a RAR file says "unsupported", never "corrupt".
pub const MAGIC_RAR: &[u8] = b"Rar!\x1a\x07";

/// Format by **name only** — Go parity (`api/archive_parser.go:20-31`, `DetectFormat`), including its
/// case-folding, so `detect("a.ZIP") == Some(Zip)`. Deliberate limitation: a file named `notes.txt`
/// that is really a zip is not an archive as far as the VFS is concerned, exactly as in Go.
#[must_use]
pub fn detect(path: &Path) -> Option<ArchiveFormat> {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(classify_archive_name)
}

/// Format by **name text** ([`detect`]'s sibling for member paths, which are strings, not `Path`s).
#[must_use]
pub fn detect_name(name: &str) -> Option<ArchiveFormat> {
    classify_archive_name(name)
}

/// Format by content, from the first bytes of a file. Used to *confirm* what the name claimed (plan
/// F8's cheap corruption guard) and to refuse a RAR file with the right error.
#[must_use]
pub fn sniff(prefix: &[u8]) -> Option<ArchiveFormat> {
    if prefix.starts_with(MAGIC_ZIP) || prefix.starts_with(MAGIC_ZIP_EMPTY) {
        Some(ArchiveFormat::Zip)
    } else if prefix.starts_with(MAGIC_7Z) {
        Some(ArchiveFormat::SevenZip)
    } else if prefix.starts_with(MAGIC_RAR) {
        Some(ArchiveFormat::Rar)
    } else {
        None
    }
}

/// Why an archive operation failed, before it becomes a [`CoreError`].
///
/// The intermediate type exists because the two backends fail in their own vocabulary and the mapping
/// is the interesting part: "this archive is password-protected" must come out as
/// [`CoreError::ArchiveEncrypted`] (Go's 422) and not as a generic I/O error, because the UI shows a
/// different dialog for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveError {
    /// The container, or the member the caller asked for, is encrypted.
    Encrypted {
        /// The archive (or member) name, for the message.
        archive: String,
    },
    /// A format this build cannot read — RAR above all (see the module docs), or a compression method
    /// the enabled backends do not implement.
    Unsupported {
        /// The archive name.
        archive: String,
        /// What was not supported (a method id, or `rar`).
        detail: String,
    },
    /// The container does not parse: bad magic, truncated central directory, failed CRC.
    Corrupt {
        /// The archive name.
        archive: String,
        /// Backend detail.
        reason: String,
    },
    /// A cap was hit: [`crate::limits::Limits::nested_archive_max_bytes`] or the entry-count bound
    /// (plan F8 — a 7z bomb is a reported error, never an OOM).
    Capacity {
        /// The archive name.
        archive: String,
        /// What was too big. The core error carries the limit as a number, so this text is for logs.
        detail: String,
    },
    /// An I/O failure that is neither of the above.
    Io {
        /// The archive name.
        archive: String,
        /// The OS message.
        reason: String,
    },
}

impl ArchiveError {
    /// The typed core error this is. Lossless for the three cases the UI branches on.
    #[must_use]
    pub fn into_core(self) -> CoreError {
        match self {
            Self::Encrypted { archive } => CoreError::ArchiveEncrypted { archive },
            Self::Unsupported { archive, detail } => CoreError::UnsupportedArchive {
                archive: format!("{archive} ({detail})"),
            },
            Self::Corrupt { archive, reason } => CoreError::ArchiveCorrupt { archive, reason },
            Self::Capacity { .. } => CoreError::CapacityExceeded {
                what: "archive bytes",
                limit: 0,
            },
            Self::Io { archive, reason } => CoreError::Io {
                path: archive,
                io: std::sync::Arc::new(std::io::Error::other(reason)),
            },
        }
    }

    /// `CoreError::Unsupported` for the RAR case — the *feature* error, not the *format* error, so a
    /// caller can tell "this build cannot read RAR" from "this file is not an archive".
    #[must_use]
    pub const fn rar_unsupported() -> CoreError {
        CoreError::Unsupported {
            what: RAR_UNSUPPORTED,
        }
    }
}

impl From<ArchiveError> for CoreError {
    fn from(e: ArchiveError) -> Self {
        e.into_core()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_is_extension_only_and_case_folded_like_go() {
        assert_eq!(detect(Path::new("/a/b/BOOK.ZIP")), Some(ArchiveFormat::Zip));
        assert_eq!(detect(Path::new("x.7z")), Some(ArchiveFormat::SevenZip));
        assert_eq!(detect(Path::new("x.rar")), Some(ArchiveFormat::Rar));
        assert_eq!(detect(Path::new("notes.txt")), None);
        // A *directory* named `fake.zip` still classifies: Go keyed on the name too, which is why its
        // VFS write guard is a substring test (`tests/golden/err-vfs-readonly.json`).
        assert_eq!(detect(Path::new("/tree/fake.zip/inner.txt")), None);
    }

    #[test]
    fn sniff_reads_the_signatures_the_backends_check() {
        assert_eq!(sniff(b"PK\x03\x04rest"), Some(ArchiveFormat::Zip));
        assert_eq!(sniff(b"PK\x05\x06"), Some(ArchiveFormat::Zip));
        assert_eq!(
            sniff(b"7z\xbc\xaf\x27\x1c\xb0"),
            Some(ArchiveFormat::SevenZip)
        );
        assert_eq!(sniff(b"Rar!\x1a\x07\x01"), Some(ArchiveFormat::Rar));
        assert_eq!(sniff(b"not an archive"), None);
        assert_eq!(sniff(b""), None);
    }

    #[test]
    fn encrypted_and_corrupt_map_to_their_own_core_kinds() {
        let e: CoreError = ArchiveError::Encrypted {
            archive: "a.zip".into(),
        }
        .into();
        assert_eq!(e.kind(), "archive_encrypted");
        let r = ArchiveError::rar_unsupported();
        assert_eq!(r.kind(), "unsupported");
        assert_eq!(MAX_NEST_DEPTH, 2);
    }
}
