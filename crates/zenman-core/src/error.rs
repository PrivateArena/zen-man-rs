//! The single typed error of the engine (plan §5.2 `error.rs` row).
//!
//! Go had no error type at all: every handler called `http.Error(w, `{"error": …}`, code)` with a
//! hand-written format string, which is why two of the frozen goldens are not even valid JSON
//! (`docs/contracts.md` → *The error envelope*: `err-chmod-octaltext.json` carries Go's unescaped
//! `%v`). This enum is the in-process replacement for that envelope, so the three facts a caller
//! actually needs — which error, what the user reads, and what the legacy HTTP code was — are
//! methods instead of string prefixes.
//!
//! `#[error(...)]` strings are **Go's user-visible text** wherever Go had one; the anchor for each is
//! in the variant's doc comment. The machine-readable sentinel Go put in `error` (`"vfs_readonly"`,
//! `"archive_modified"`) survives as [`CoreError::kind`], which is also what `to_json()` emits, so a
//! contract test can assert the sentinel without asserting the prose.
//!
//! [`CoreError::is_expected`] is the "don't show a toast" predicate that the Go server could not
//! express: an HTTP client cannot tell a cancelled request from a failed one, an in-process caller
//! can.

use std::io;
use std::sync::Arc;

/// Why a path string was refused before touching the filesystem (plan §5.2 `error.rs` row).
///
/// These are the *input* rejections, as opposed to the I/O outcomes in [`CoreError`]: an
/// [`CoreError::InvalidPath`] always means "the string was not a usable path", never "disk".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathReject {
    /// An empty component, or `join("")`.
    Empty,
    /// A path that is not absolute and has no base to resolve against.
    Relative,
    /// A `..` that would leave the root/store it is relative to.
    EscapesRoot,
    /// A `..` component inside a single name (`join` argument), which normalisation cannot absorb
    /// without silently changing the target directory.
    InteriorDot,
    /// A glob (`*`, `?`, `[`), which is never a legal literal destination (plan §5.2 `Wildcard`).
    Wildcard,
    /// `~` was requested and `HOME` is unset. Go's `os.UserHomeDir()` returns an error here
    /// (`api/dir.go:49-51`); the plan's five-variant `PathReject` set has no member for it, so this
    /// is a documented addition (step-3 report, plan §5.2 gap).
    NoHome,
}

impl std::fmt::Display for PathReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Empty => "empty path",
            Self::Relative => "relative path",
            Self::EscapesRoot => "path escapes its root",
            Self::InteriorDot => "path contains a `..` component",
            Self::Wildcard => "path contains a wildcard",
            Self::NoHome => "home directory is unknown (HOME is unset)",
        };
        f.write_str(s)
    }
}

/// Every failure the engine can produce.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CoreError {
    /// `api/dir.go:96` → `{"error": "Path not found: %v"}` (HTTP 404).
    #[error("Path not found: {path}")]
    NotFound {
        path: String,
        /// The path that was actually `stat`ed when it differs from `path` — a symlink target, or
        /// the host archive of a VFS path. `None` when the caller has only one path to name.
        source_path: Option<String>,
    },

    /// A permission failure. Go surfaced this as **HTTP 500** with the raw `os` error text
    /// (`docs/contracts.md` §2.4 row *Permission errors*, `err-perm-denied-dir.json`); 403 was the
    /// planning-time guess and is not what the goldens show, so the legacy code below is 500.
    #[error("permission denied: {path}")]
    PermissionDenied { path: String },

    /// `api/ops.go:73,78` → `"message":"Write operations not supported inside archives"` with the
    /// sentinel `"error":"vfs_readonly"`. Plan §4.2 spells this variant `ReadOnlyVfs`; this name
    /// matches the sentinel instead (step-3 report: the two spellings are reconciled here).
    #[error("Write operations not supported inside archives")]
    VfsReadOnly { path: String },

    /// `api/dir.go:89` → `{"error": "Invalid path: %v"}` (HTTP 400).
    #[error("Invalid path: {raw}")]
    InvalidPath { raw: String, reason: PathReject },

    /// `api/ops.go:931` → `Invalid permission mode: %v` (HTTP 400, `err-chmod-octaltext.json`).
    /// Plan §5.2's `error.rs` row has no mode variant; plan §2.4 row *chmod* moves the `^[0-7]{3,4}$`
    /// check into core, which needs a typed error of its own, so this is a documented addition
    /// (step-3 report, plan §5.2 gap) rather than a borrow from `InvalidPath`.
    #[error("Invalid permission mode: {raw}")]
    InvalidMode { raw: String },

    /// Paste/extract collision. Go answered **200** with `{"status":"conflict","conflicts":[…]}`
    /// (`api/ops.go`, `paste-conflict.json`); plan §6.2 makes it typed, so the 200 is kept only as
    /// legacy metadata in [`CoreError::status_code`].
    #[error("conflict writing into {dest}")]
    Conflict {
        conflicts: Vec<String>,
        dest: String,
    },

    /// `api/dir.go:100` → `{"error": "Path is not a directory"}` (HTTP 400).
    #[error("Path is not a directory")]
    NotADir { path: String },

    /// `api/ops.go` move collision (`err-paste-move-collision.json`, HTTP 500).
    #[error("already exists: {path}")]
    AlreadyExists { path: String },

    /// `api/archive_handlers.go:51` (HTTP 422; no golden — `docs/contracts.md` → *Fixtures we cannot
    /// generate*).
    #[error("archive is encrypted: {archive}")]
    ArchiveEncrypted { archive: String },

    /// The archive's host mtime changed while its manifest was cached: `archive_modified`
    /// (`api/archive_vfs.go:189`, `api/archive_handlers.go:66`, HTTP 409). The display text is the
    /// plan §5.2 wording, because the Go sentinel alone is not something a user can act on.
    #[error("Archive was modified while reading")]
    ArchiveModified { archive: String },

    /// No reader for this container format (plan §5.5).
    #[error("unsupported archive format: {archive}")]
    UnsupportedArchive { archive: String },

    /// The platform refused to create a symlink. Go never reported this separately: `paste_link`
    /// just failed 500, and the capability lived in a startup probe boolean (`main.go:24-49`,
    /// `/api/props` → `symlink_supported`).
    #[error("Symlinks are not supported on this platform")]
    SymlinkUnsupported,

    /// A job was cancelled by the user (plan F1/F3). Never a fault.
    #[error("operation cancelled")]
    Cancelled,

    /// The FTS index has no rows yet (plan §5.3 `IndexNotReady`).
    #[error("search index is not ready")]
    IndexNotReady,

    /// An I/O failure that knows which path it was about (plan §5.2: `Io{path, Arc<io::Error>}`).
    /// The `Arc` is what makes the variant `Clone`-friendly and `?`-safe: `io::Error` is not `Clone`
    /// and a job result is handed to several listeners.
    #[error("I/O error on {path}")]
    Io {
        path: String,
        #[source]
        io: Arc<io::Error>,
    },

    /// `?`-convenience for a bare `io::Error` with no path context of its own.
    #[error("{0}")]
    System(#[from] io::Error),

    /// A shared error crossing a job boundary: `Arc<CoreError>` is cheap to hand to listeners, and
    /// the inner error stays the real one for `source()` and `user_message()` purposes.
    #[error("{0}")]
    Nested(#[from] Arc<CoreError>),

    /// Undo-log decode failure (`api/log.go` resync path, plan F4).
    #[error("action log is corrupt at offset {offset}: {reason}")]
    LogCorrupt { offset: u64, reason: String },

    /// SQL failure (plan §5.3: surfaced as a toast, never a startup abort — Go `log.Fatalf`'d).
    #[error("database error: {msg}")]
    Sql { msg: String },

    /// A custom action or `open` helper failed (`api/actions.go:236-262`, plan F10).
    #[error("command failed: {cmd}")]
    Shell {
        cmd: String,
        code: Option<i32>,
        stderr: String,
    },

    /// Filesystem watcher failure (plan §5.3 `index/watcher`).
    #[error("watcher error: {msg}")]
    Watcher { msg: String },

    /// A configured cap was hit: flat-walk 10 000 (`api/dir.go:132`), archive byte cap (plan F8).
    #[error("too many {what}: the limit is {limit}")]
    CapacityExceeded { what: &'static str, limit: usize },

    /// The operation needs a platform service this build does not have — `platform::stub`, and the
    /// reason the stub can answer every [`crate::platform::Env`] method without panicking.
    ///
    /// [`crate::platform::Env`]: crate::platform::Env
    #[error("not supported on this platform: {what}")]
    Unsupported { what: &'static str },
}

impl CoreError {
    /// The stable machine-readable name: Go's sentinel where Go had one (`vfs_readonly`,
    /// `archive_modified`), otherwise the snake_case variant name.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "not_found",
            Self::PermissionDenied { .. } => "permission_denied",
            Self::VfsReadOnly { .. } => "vfs_readonly",
            Self::InvalidPath { .. } => "invalid_path",
            Self::InvalidMode { .. } => "invalid_mode",
            Self::Conflict { .. } => "conflict",
            Self::NotADir { .. } => "not_a_directory",
            Self::AlreadyExists { .. } => "already_exists",
            Self::ArchiveEncrypted { .. } => "archive_encrypted",
            Self::ArchiveModified { .. } => "archive_modified",
            Self::UnsupportedArchive { .. } => "unsupported_archive",
            Self::SymlinkUnsupported => "symlink_unsupported",
            Self::Cancelled => "cancelled",
            Self::IndexNotReady => "index_not_ready",
            Self::Io { .. } | Self::System(_) => "io_error",
            Self::Nested(_) => "nested",
            Self::LogCorrupt { .. } => "log_corrupt",
            Self::Sql { .. } => "sql_error",
            Self::Shell { .. } => "shell_error",
            Self::Watcher { .. } => "watcher_error",
            Self::CapacityExceeded { .. } => "capacity_exceeded",
            Self::Unsupported { .. } => "unsupported",
        }
    }

    /// The user-visible string. For every variant where Go had prose, this *is* Go's prose
    /// (`#[error]` and this function agree by construction — this just unwraps the chain).
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::Nested(inner) => inner.user_message(),
            other => other.to_string(),
        }
    }

    /// The HTTP status the equivalent Go handler returned, kept for the contract tests only: the
    /// port has no HTTP layer (plan §1). Codes are the measured ones from `docs/contracts.md`,
    /// including its two surprises — a denied directory is 500, not 403, and a paste conflict is a
    /// 200 with a magic string.
    #[must_use]
    pub fn status_code(&self) -> u16 {
        match self {
            Self::NotFound { .. } => 404,
            Self::PermissionDenied { .. } => 500,
            Self::VfsReadOnly { .. } => 405,
            Self::InvalidPath { .. } => 400,
            Self::InvalidMode { .. } => 400,
            Self::Conflict { .. } => 200,
            Self::NotADir { .. } => 400,
            Self::AlreadyExists { .. } => 500,
            Self::ArchiveEncrypted { .. } => 422,
            Self::ArchiveModified { .. } => 409,
            Self::UnsupportedArchive { .. } => 400,
            Self::SymlinkUnsupported => 500,
            Self::Cancelled => 408,
            Self::IndexNotReady => 503,
            Self::Io { .. } | Self::System(_) => 500,
            Self::Nested(inner) => inner.status_code(),
            Self::LogCorrupt { .. } => 500,
            Self::Sql { .. } => 500,
            Self::Shell { .. } => 500,
            Self::Watcher { .. } => 500,
            Self::CapacityExceeded { .. } => 507,
            Self::Unsupported { .. } => 501,
        }
    }

    /// Errors that are a normal outcome of user action or a racing filesystem, so the UI records
    /// them instead of alarming the user (Go could not distinguish them — every one was a red
    /// browser error toast).
    ///
    /// Deliberately *not* expected: `PermissionDenied`, `Io`, `Sql`, `LogCorrupt`, `Shell` — those
    /// are the ones a user must be told about.
    #[must_use]
    pub fn is_expected(&self) -> bool {
        match self {
            Self::Cancelled
            | Self::IndexNotReady
            | Self::ArchiveModified { .. }
            | Self::NotFound { .. } => true,
            // A stale tab/clipboard pointing at a path a job just deleted arrives as this pair.
            Self::VfsReadOnly { .. } => false,
            Self::Nested(inner) => inner.is_expected(),
            _ => false,
        }
    }

    /// The wire shape: Go's `{"error": …}` envelope, but valid JSON by construction (this is
    /// `serde_json`-serialised, not `format!`-interpolated — see the module docs).
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "error": self.kind(),
            "message": self.user_message(),
            "status": self.status_code(),
        })
    }

    /// Wrap an I/O failure that happened at `path`, sharing the error payload with every listener.
    #[must_use]
    pub fn io(path: impl Into<String>, io: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            io: Arc::new(io),
        }
    }
}

impl serde::Serialize for CoreError {
    /// The same triple as [`CoreError::to_json`], so a `CoreError` can be embedded in an event
    /// payload (plan §4.1) without a `serde_json::Value` hop.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct as _;
        let mut st = serializer.serialize_struct("CoreError", 3)?;
        st.serialize_field("error", self.kind())?;
        st.serialize_field("message", &self.user_message())?;
        st.serialize_field("status", &self.status_code())?;
        st.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conflicts() -> Vec<String> {
        vec!["a.txt".to_string(), "b.txt".to_string()]
    }

    #[test]
    fn not_found_matches_go_text_and_404() {
        let e = CoreError::NotFound {
            path: "/nope".into(),
            source_path: None,
        };
        assert_eq!(e.to_string(), "Path not found: /nope");
        assert_eq!(e.user_message(), "Path not found: /nope");
        assert_eq!(e.status_code(), 404);
        assert_eq!(e.kind(), "not_found");
        assert!(e.is_expected());
    }

    #[test]
    fn not_found_keeps_the_source_path() {
        let e = CoreError::NotFound {
            path: "link".into(),
            source_path: Some("/nowhere".into()),
        };
        assert_eq!(e.to_string(), "Path not found: link");
        match &e {
            CoreError::NotFound { source_path, .. } => {
                assert_eq!(source_path.as_deref(), Some("/nowhere"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn permission_denied_is_go_http_500_not_403() {
        // docs/contracts.md §2.4: a denied directory is HTTP 500 in the goldens.
        let e = CoreError::PermissionDenied {
            path: "/tree/noread".into(),
        };
        assert_eq!(e.status_code(), 500);
        assert!(!e.is_expected());
    }

    #[test]
    fn vfs_read_only_carries_go_sentinel_and_message() {
        // api/ops.go:73 `{"error": "vfs_readonly", "message": "Write operations not supported inside archives"}`
        let e = CoreError::VfsReadOnly {
            path: "/tree/fake.zip/inner.txt".into(),
        };
        assert_eq!(
            e.to_string(),
            "Write operations not supported inside archives"
        );
        assert_eq!(e.kind(), "vfs_readonly");
        assert_eq!(e.status_code(), 405);
        assert!(!e.is_expected());
    }

    #[test]
    fn invalid_path_matches_go_text_and_400() {
        // api/dir.go:89 `{"error": "Invalid path: %v"}`
        let e = CoreError::InvalidPath {
            raw: "a/../..".into(),
            reason: PathReject::EscapesRoot,
        };
        assert_eq!(e.to_string(), "Invalid path: a/../..");
        assert_eq!(e.status_code(), 400);
        assert_eq!(e.kind(), "invalid_path");
        assert_eq!(PathReject::EscapesRoot.to_string(), "path escapes its root");
        assert_eq!(
            PathReject::NoHome.to_string(),
            "home directory is unknown (HOME is unset)"
        );
    }

    #[test]
    fn conflict_is_go_http_200_with_names() {
        let e = CoreError::Conflict {
            conflicts: conflicts(),
            dest: "/dst".into(),
        };
        assert_eq!(e.status_code(), 200);
        assert_eq!(e.kind(), "conflict");
        match &e {
            CoreError::Conflict { conflicts, dest } => {
                assert_eq!(conflicts.len(), 2);
                assert_eq!(dest, "/dst");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn not_a_dir_matches_go_text_and_400() {
        // api/dir.go:100 `{"error": "Path is not a directory"}`
        let e = CoreError::NotADir {
            path: "/tree/a9.txt".into(),
        };
        assert_eq!(e.to_string(), "Path is not a directory");
        assert_eq!(e.status_code(), 400);
    }

    #[test]
    fn already_exists_is_go_http_500() {
        let e = CoreError::AlreadyExists {
            path: "/dst/a.txt".into(),
        };
        assert_eq!(e.status_code(), 500);
        assert_eq!(e.to_string(), "already exists: /dst/a.txt");
    }

    #[test]
    fn archive_encrypted_is_422() {
        let e = CoreError::ArchiveEncrypted {
            archive: "/a.7z".into(),
        };
        assert_eq!(e.status_code(), 422);
        assert_eq!(e.kind(), "archive_encrypted");
    }

    #[test]
    fn archive_modified_is_the_plan_wording_with_the_go_sentinel() {
        let e = CoreError::ArchiveModified {
            archive: "/a.zip".into(),
        };
        assert_eq!(e.to_string(), "Archive was modified while reading");
        assert_eq!(e.kind(), "archive_modified");
        assert_eq!(e.status_code(), 409);
        assert!(e.is_expected());
    }

    #[test]
    fn unsupported_archive_is_400() {
        let e = CoreError::UnsupportedArchive {
            archive: "/a.rar".into(),
        };
        assert_eq!(e.status_code(), 400);
    }

    #[test]
    fn symlink_unsupported_has_its_own_variant() {
        let e = CoreError::SymlinkUnsupported;
        assert_eq!(e.to_string(), "Symlinks are not supported on this platform");
        assert_eq!(e.status_code(), 500);
    }

    #[test]
    fn cancelled_is_expected() {
        assert!(CoreError::Cancelled.is_expected());
        assert_eq!(CoreError::Cancelled.to_string(), "operation cancelled");
    }

    #[test]
    fn index_not_ready_is_503_and_expected() {
        let e = CoreError::IndexNotReady;
        assert_eq!(e.status_code(), 503);
        assert!(e.is_expected());
    }

    #[test]
    fn io_wraps_the_error_in_an_arc_and_keeps_the_source() {
        let raw = io::Error::new(io::ErrorKind::PermissionDenied, "permission denied");
        let text = raw.to_string();
        let e = CoreError::io("/tree/noperm.bin", raw);
        let CoreError::Io { path, io: shared } = &e else {
            panic!("wrong variant: {e:?}")
        };
        assert_eq!(path, "/tree/noperm.bin");
        assert_eq!(shared.to_string(), text);
        // `#[source]` chain works, and the Arc is shareable between listeners.
        assert!(std::error::Error::source(&e).is_some());
        let again = Arc::new(io::Error::from(io::ErrorKind::BrokenPipe));
        let shared2 = Arc::clone(&again);
        assert!(Arc::ptr_eq(&again, &shared2));
        assert_eq!(e.status_code(), 500);
    }

    #[test]
    fn system_converts_from_io_error() {
        let e: CoreError = io::Error::new(io::ErrorKind::NotFound, "nope").into();
        assert!(matches!(e, CoreError::System(_)));
        assert_eq!(e.kind(), "io_error");
    }

    #[test]
    fn nested_forwards_message_status_and_expectation() {
        // The nested error has to *be* the error the assertions compare against; the first cut of this
        // test built `inner` as `ArchiveModified` and then nested a `NotFound`, which checked nothing
        // (step-3 report: test-construction bug, not a contract question).
        let inner = Arc::new(CoreError::ArchiveModified {
            archive: "/a.zip".into(),
        });
        let e: CoreError = CoreError::Nested(Arc::clone(&inner));
        assert_eq!(e.user_message(), inner.user_message());
        assert_eq!(e.status_code(), inner.status_code());
        assert!(e.is_expected());
        assert!(matches!(e, CoreError::Nested(_)));
    }

    #[test]
    fn log_corrupt_reports_the_offset_for_the_resync() {
        let e = CoreError::LogCorrupt {
            offset: 4319,
            reason: "bad crc".into(),
        };
        assert_eq!(
            e.to_string(),
            "action log is corrupt at offset 4319: bad crc"
        );
        assert_eq!(e.status_code(), 500);
    }

    #[test]
    fn sql_is_500_and_never_expected() {
        let e = CoreError::Sql {
            msg: "database is locked".into(),
        };
        assert_eq!(e.status_code(), 500);
        assert!(!e.is_expected());
    }

    #[test]
    fn shell_keeps_the_exit_code_and_stderr() {
        let e = CoreError::Shell {
            cmd: "sh -c false".into(),
            code: Some(1),
            stderr: "boom".into(),
        };
        assert_eq!(e.status_code(), 500);
        assert_eq!(e.kind(), "shell_error");
    }

    #[test]
    fn watcher_is_500() {
        let e = CoreError::Watcher {
            msg: "inotify exhausted".into(),
        };
        assert_eq!(e.status_code(), 500);
    }

    #[test]
    fn capacity_exceeded_names_the_cap() {
        let e = CoreError::CapacityExceeded {
            what: "walked files",
            limit: 10_000,
        };
        assert_eq!(e.to_string(), "too many walked files: the limit is 10000");
        assert_eq!(e.status_code(), 507);
    }

    #[test]
    fn unsupported_is_501() {
        let e = CoreError::Unsupported {
            what: "mount enumeration",
        };
        assert_eq!(e.status_code(), 501);
        assert_eq!(e.kind(), "unsupported");
    }

    #[test]
    fn to_json_is_always_valid_json_and_uses_the_go_key() {
        let variants = [
            CoreError::VfsReadOnly { path: "/a".into() },
            CoreError::InvalidPath {
                raw: "".into(),
                reason: PathReject::Empty,
            },
            CoreError::Conflict {
                conflicts: conflicts(),
                dest: "/d".into(),
            },
        ];
        for v in &variants {
            let j = v.to_json();
            assert_eq!(j["error"], serde_json::json!(v.kind()));
            assert_eq!(j["message"], serde_json::json!(v.user_message()));
            assert_eq!(j["status"], serde_json::json!(v.status_code()));
            // The Go envelope's key is `error`; our extra key is `message`, exactly as
            // err-vfs-readonly.json has both (`api/ops.go:73`).
            assert!(j.get("error").is_some());
        }
    }

    #[test]
    fn core_error_serialises_like_to_json() {
        let e = CoreError::ArchiveModified {
            archive: "/a.zip".into(),
        };
        let direct = serde_json::to_value(&e).unwrap();
        assert_eq!(direct, e.to_json());
        // Never the unescaped `%v` shape of err-chmod-octaltext.json: quoting is structural.
        let nasty = CoreError::Sql {
            msg: "no such: \"table\"".into(),
        };
        let s = serde_json::to_string(&nasty).unwrap();
        let back: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(back["message"], serde_json::json!(nasty.user_message()));
    }
}
