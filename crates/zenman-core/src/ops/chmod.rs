//! `chmod`: parse the octal text, apply it, log the text (plan §5.3 `ops/chmod.rs`, Go `handleChmod`
//! `api/ops.go:925-955`).
//!
//! The whole of Go's mode handling was `strconv.ParseUint(modeStr, 8, 32)` (`api/ops.go:931`) followed by
//! `os.Chmod(src, os.FileMode(n))` — which means Go accepted `"600"` and `"0644"` and, because `FileMode`
//! is a *Go* file-mode, silently interpreted a fourth digit `1xxx` as its `ModeSetuid`-class bits rather
//! than as Unix `st_mode`. The port parses with [`crate::mode::parse_octal`] (the `^[0-7]{3,4}$` rule
//! `docs/contracts.md` records, stricter than Go in the direction nobody meant: `"75"` is refused, not
//! read as `0o075`) and applies only the low twelve bits, which is what `chmod(2)` takes
//! (`platform::linux::chmod`).
//!
//! # The record keeps the typed text
//!
//! Go logged `modeStr` **verbatim** (`api/ops.go:950`), and the frozen capture proves it: golden records
//! 9 and 10 of `tests/golden/log.json` carry `"600"` and `"0644"` — the same mode, spelled as the user
//! typed it. A canonicalising port cannot reproduce that byte for byte, so [`ChmodReq::parse`] keeps the
//! raw text alongside the parsed bits and [`ChmodReq::record_name`] returns it; the canonical form
//! ([`octal_text`], four digits) is the fallback for a request that was built programmatically.
//!
//! Chmod is not reversible (`ActionType::CHMOD` is outside the five `api/log.go:600-602` admits) because
//! Go stored no previous mode — the record says what was set, not what was lost.

use std::sync::Arc;

use crate::error::CoreError;
use crate::history::codec::{ActionEntry, ActionType};
use crate::jobs::CancelToken;
use crate::mode::{self, ModeBits};
use crate::ops::guard::{ensure_writable, vfs_write_guard};
use crate::path::{AbsPath, FsPath};
use crate::platform::Env;

/// Whether the chmod walks the selection's subtrees — the platform's own type, so there is exactly one
/// spelling of "recursive" in the crate (plan §5.3: the v1 UI always passes [`Recursive::No`], because Go
/// chmods exactly the paths it was given, `api/ops.go:940-944`).
pub use crate::platform::Recursive;

/// One chmod request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChmodReq {
    /// What to change.
    pub sources: Vec<AbsPath>,
    /// The bits.
    pub mode: ModeBits,
    /// The text the record carries — Go's `modeStr` (`api/ops.go:950`).
    pub text: String,
    /// Walk the subtrees?
    pub recursive: Recursive,
}

impl ChmodReq {
    /// Parse the UI's octal text and keep it for the record (Go parity, module docs).
    ///
    /// # Errors
    /// [`CoreError::InvalidMode`] for anything that is not three or four octal digits
    /// (`docs/contracts.md` → *Deviation ledger* #7, `err-chmod-octaltext.json`).
    pub fn parse(sources: Vec<AbsPath>, raw: &str) -> Result<Self, CoreError> {
        Ok(Self {
            sources,
            mode: mode::parse_octal(raw)?,
            text: raw.to_string(),
            recursive: Recursive::No,
        })
    }

    /// A request built from bits the caller already has; the record gets the canonical four-digit text.
    #[must_use]
    pub fn new(sources: Vec<AbsPath>, mode: ModeBits) -> Self {
        Self {
            sources,
            mode,
            text: octal_text(mode),
            recursive: Recursive::No,
        }
    }

    /// The `name` field of the undo-log record.
    #[must_use]
    pub fn record_name(&self) -> &str {
        &self.text
    }

    /// The paths the mode will be applied to — the selection, plus its subtrees when asked.
    ///
    /// # Errors
    /// [`CoreError::Io`] when a directory cannot be read, [`CoreError::CapacityExceeded`] past the depth
    /// cap (the same 128 the copy walk uses, so a symlink loop cannot spin here either).
    pub fn targets(&self, cancel: &CancelToken) -> Result<Vec<AbsPath>, CoreError> {
        if self.recursive == Recursive::No {
            return Ok(self.sources.clone());
        }
        let mut out = Vec::new();
        let mut stack: Vec<(AbsPath, usize)> = self
            .sources
            .iter()
            .map(|p| (p.clone(), 0usize))
            .collect::<Vec<_>>();
        while let Some((path, depth)) = stack.pop() {
            cancel.check()?;
            if depth > crate::ops::copy::MAX_COPY_DEPTH {
                return Err(CoreError::CapacityExceeded {
                    what: "chmod recursion depth",
                    limit: crate::ops::copy::MAX_COPY_DEPTH,
                });
            }
            let meta = std::fs::symlink_metadata(path.as_path()).map_err(|io| CoreError::Io {
                path: path.to_string_lossy(),
                io: Arc::new(io),
            })?;
            out.push(path.clone());
            if !meta.is_dir() {
                continue;
            }
            let read = std::fs::read_dir(path.as_path()).map_err(|io| CoreError::Io {
                path: path.to_string_lossy(),
                io: Arc::new(io),
            })?;
            for entry in read.flatten() {
                if let Ok(child) = AbsPath::from_path(entry.path()) {
                    stack.push((child, depth + 1));
                }
            }
        }
        Ok(out)
    }
}

/// The canonical octal text the UI shows and a programmatic request logs: four digits, leading zeros.
#[must_use]
pub fn octal_text(mode: ModeBits) -> String {
    format!("{:04o}", mode.bits_octal())
}

/// Everything one chmod did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChmodReport {
    /// The paths the mode was applied to (a recursive chmod touches more than the selection).
    pub applied: Vec<AbsPath>,
    /// The bits that were set.
    pub mode: ModeBits,
    /// The text the record carried.
    pub text: String,
}

impl ChmodReport {
    /// The distinct parent directories that changed — one
    /// [`DirChanged`](crate::events::EngineEvent::DirChanged) each (a mode change alters the row a listing
    /// shows: Go's `Mode` column).
    #[must_use]
    pub fn affected_parents(&self) -> Vec<AbsPath> {
        let mut parents = Vec::new();
        for path in &self.applied {
            if let Some(parent) = path.parent()
                && !parents.contains(&parent)
            {
                parents.push(parent);
            }
        }
        parents
    }

    /// The Go-parity log entry: `Append(ActionChmod, sources, "", modeStr)` (`api/ops.go:950`) — the
    /// sources as requested (not the expanded recursive set: Go had no recursion, so the record names the
    /// selection the user made), empty `dest`, the mode text as `name`.
    #[must_use]
    pub fn log_entry(&self, req: &ChmodReq) -> Option<ActionEntry> {
        if self.applied.is_empty() {
            return None;
        }
        let sources: Vec<String> = req.sources.iter().map(|p| p.to_string_lossy()).collect();
        Some(ActionEntry::new(
            ActionType::CHMOD,
            sources,
            "",
            req.record_name(),
        ))
    }
}

/// Apply a mode to a selection.
///
/// The whole batch is applied by one [`Env::chmod`] call, because that is the seam that knows whether the
/// platform has an `st_mode` at all (plan §5.2: on a host without one this is
/// [`CoreError::Unsupported`], not a silent success).
///
/// # Errors
/// [`CoreError::Empty`] for an empty selection, [`CoreError::VfsReadOnly`] for an archive path,
/// [`CoreError::PermissionDenied`] when the platform says a target is not writable, and the `chmod(2)`
/// failure as [`CoreError::Io`] — Go's `lastErr` (`api/ops.go:941-947`), but naming the path it failed on
/// instead of the last one it tried.
pub fn chmod(
    env: &dyn Env,
    req: &ChmodReq,
    cancel: &CancelToken,
) -> Result<ChmodReport, CoreError> {
    if req.sources.is_empty() {
        return Err(CoreError::Empty { what: "selection" });
    }
    let fs_paths: Vec<FsPath> = req.sources.iter().cloned().map(FsPath::from_abs).collect();
    let refs: Vec<&FsPath> = fs_paths.iter().collect();
    vfs_write_guard(&refs)?;
    let targets = req.targets(cancel)?;
    for path in &targets {
        cancel.check()?;
        // Pre-flight only: the real answer comes from `chmod(2)`, and a file's own mode is not what
        // decides whether its owner may change it.
        if let Err(e) = ensure_writable(env, path)
            && e.kind() != "permission_denied"
        {
            return Err(e);
        }
    }
    crate::jobs::block_on(env.chmod(&targets, req.mode))??;
    Ok(ChmodReport {
        applied: targets,
        mode: req.mode,
        text: req.text.clone(),
    })
}
