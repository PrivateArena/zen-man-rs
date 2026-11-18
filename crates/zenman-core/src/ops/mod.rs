//! The operation layer (plan §5.3): what the user asks the engine to *do*, as opposed to what it asks
//! it to *list*.
//!
//! Step 9a owned the two pieces that every operation needs before it can touch a byte:
//!
//! * [`guard`] — the pre-flight answers (write-target checks, the copy-into-itself containment rule);
//! * [`clipboard`] — the session clipboard model, its `text/uri-list` wire form, and the system-clipboard
//!   sync Go did with `wl-copy`/`xclip` (`api/ops.go:549-635`).
//!
//! Step 9b added the work itself, and the vocabulary it is decided with:
//!
//! * [`ConflictPolicy`] — one answer to "the name is taken", shared by copy and move (plan §5.3);
//! * [`ProgressSink`] — the walk's progress channel, implemented by [`crate::jobs::Progress`] and by a test;
//! * [`copy`] / [`r#move`] / [`paste`] — the recursive copy engine, the rename-or-copy+delete engine with its
//!   `EXDEV` seam and directory merge, and the paste that runs the guard, appends the Go-parity undo-log
//!   record and hands the report back through a [`JobKind::Paste`](crate::jobs::JobKind::Paste) job.
//!
//! Plan §5.3's `OpCtx` is deliberately not here yet: the only context a step-9b operation needs is the
//! platform seam, the move seam, the cancel token and the progress sink, and they are four named
//! parameters rather than a struct that would have to grow a `Db`/`Limits` field step 12+ can fill in.
//!
//! Step 9c added the rest of the write path, all of it on top of the same two pieces rather than
//! re-deriving any of it:
//!
//! * [`delete`] — trash-first deletion with the plan §6.2 **D6** fallback ladder and a per-source outcome;
//! * [`rename`] — single and batch rename, Go's parent-rename source rewrite included, through
//!   [`ConflictPolicy`] and [`crate::ops::r#move::move_item`];
//! * [`mkdir`] — the validated name, Go's `0755`, and the new row back;
//! * [`chmod`] — the octal text in, the mode bits out, with the record keeping the text the user typed;
//! * [`open`] — the desktop launcher, with a directory routed as navigation instead.
//!
//! [`crate::history::revert`] completes the pair: it drives *these* functions backwards, so an undo can
//! never disagree with what the forward operation did.
//!
//! Two rules bind everything under `ops/`:
//!
//! * **a rejection is typed, never a silent skip.** Go `continue`d past a copy-into-itself and reported
//!   success (plan §6.2); an `ops` function returns the [`crate::error::CoreError`] that names why.
//! * **no path logic of its own.** Containment, cleaning and name validation come from
//!   [`crate::path`] (`AbsPath::is_ancestor_of`, `contains_dot_component`, `validate_dest_name`), so there
//!   is exactly one implementation of "is this inside that" in the crate.
//!
//! [`crate::error::CoreError`]: crate::error::CoreError

pub mod chmod;
pub mod clipboard;
pub mod copy;
pub mod delete;
pub mod guard;
pub mod mkdir;
pub mod r#move;
pub mod open;
pub mod paste;
pub mod rename;

use std::sync::Arc;

use crate::error::CoreError;
use crate::path::AbsPath;

/// What to do when the name the operation wants is already taken (plan §5.3's `ConflictPolicy`).
///
/// One type, decided in one place ([`ConflictPolicy::decide`]), shared by [`copy::copy_item`] and
/// [`r#move::move_item`] — the reason the policy is not a boolean on the copy helper: Go's `copyFile`
/// silently skipped an existing destination (`api/ops.go:648-651`, ledger #9) while `moveItem` errored
/// (`api/ops.go:716-720`, ledger #10), and a paste that mixed the two reported both as success.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictPolicy {
    /// Leave the existing destination alone and report the item as skipped. Go's `copyFile`, made loud
    /// (plan F12): the skip is a value in the report, never an absent entry.
    SkipExisting,
    /// Clobber the existing destination. For a **directory onto a directory** this is a merge, not a
    /// delete — see [`ConflictDecision::Merge`] — because Go merged too (`api/ops.go:683` `MkdirAll`).
    Overwrite,
    /// Keep both: place the item under a fresh name (`name (1)`, `name (2)`, …).
    Rename,
}

impl ConflictPolicy {
    /// The policy a v1 paste runs with — the plan's note that only `SkipExisting` is reachable from the
    /// UI, kept as a named default instead of a magic variant at every call site.
    pub const DEFAULT: Self = Self::SkipExisting;

    /// Decide what happens to one candidate destination. `src_is_dir` is the *source's* kind, which is
    /// what makes a directory onto a directory a [`ConflictDecision::Merge`] rather than a collision:
    /// that pairing is the one Go never treated as a conflict on either the copy or the move path.
    ///
    /// `Err` only for the case no policy can answer — [`ConflictPolicy::Rename`] finding no free name
    /// within [`MAX_RENAME_ATTEMPTS`].
    ///
    /// # Errors
    /// [`CoreError::CapacityExceeded`] when every generated name is taken.
    pub fn decide(self, src_is_dir: bool, target: &AbsPath) -> Result<ConflictDecision, CoreError> {
        let existing = std::fs::symlink_metadata(target.as_path());
        let Ok(meta) = existing else {
            // Nothing there (or nothing knowable there): a later `create`/`rename` produces the real
            // error, with the OS's own errno, if the parent is unwritable.
            return Ok(ConflictDecision::Proceed);
        };
        if src_is_dir && meta.is_dir() {
            return Ok(ConflictDecision::Merge);
        }
        match self {
            Self::SkipExisting => Ok(ConflictDecision::Skip),
            Self::Overwrite => Ok(ConflictDecision::Proceed),
            Self::Rename => Ok(ConflictDecision::Rename(unique_target(target)?)),
        }
    }
}

/// The answer [`ConflictPolicy::decide`] gave for one destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConflictDecision {
    /// The destination is free (or the caller said to clobber it).
    Proceed,
    /// A directory met a directory: fold the source's children into the existing one.
    Merge,
    /// The destination is taken and the policy is [`ConflictPolicy::SkipExisting`].
    Skip,
    /// The destination is taken and the policy is [`ConflictPolicy::Rename`]; the payload is the new path.
    Rename(AbsPath),
}

/// How many `name (n)` candidates [`unique_target`] tries before giving up.
pub const MAX_RENAME_ATTEMPTS: usize = 1000;

/// The first free sibling of `taken`: `a.txt` → `a (1).txt`, `.hidden` → `.hidden (1)`.
///
/// The split point is the last `.` that is not the first character, so a dotfile keeps its whole name as
/// the stem (Go had no equivalent — it had no rename policy at all).
///
/// # Errors
/// [`CoreError::CapacityExceeded`] after [`MAX_RENAME_ATTEMPTS`] taken candidates.
pub fn unique_target(taken: &AbsPath) -> Result<AbsPath, CoreError> {
    let dir = taken.parent().ok_or_else(|| CoreError::InvalidPath {
        raw: taken.to_string_lossy(),
        reason: crate::error::PathReject::Empty,
    })?;
    let name = taken
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dot = name.rfind('.').filter(|i| *i > 0);
    let (stem, ext) = match dot {
        Some(i) => name.split_at(i),
        None => (name.as_str(), ""),
    };
    for n in 1..=MAX_RENAME_ATTEMPTS {
        let candidate = dir.join(&format!("{stem} ({n}){ext}"))?;
        if std::fs::symlink_metadata(candidate.as_path()).is_err() {
            return Ok(candidate);
        }
    }
    Err(CoreError::CapacityExceeded {
        what: "rename-policy attempts",
        limit: MAX_RENAME_ATTEMPTS,
    })
}

/// The progress channel an `ops` walk reports into.
///
/// A trait so a unit test can count reports without spawning a job; [`crate::jobs::Progress`] is the
/// implementation a real job passes, and it applies the plan's 50 ms coalescing (F2) on its own.
pub trait ProgressSink: Send + Sync {
    /// `bytes` newly written, `files` newly completed items, `current` the name being worked on (`""`
    /// when the walk is between items).
    fn advance(&self, bytes: u64, files: u64, current: &str);
}

impl ProgressSink for crate::jobs::Progress {
    fn advance(&self, bytes: u64, files: u64, current: &str) {
        crate::jobs::Progress::add(
            self,
            bytes,
            files,
            (!current.is_empty()).then(|| current.to_string()),
        );
    }
}

/// A sink that drops everything: for a walk nobody is watching.
pub struct NoProgress;

impl ProgressSink for NoProgress {
    fn advance(&self, _bytes: u64, _files: u64, _current: &str) {}
}

/// The `Arc` the ops layer hands across a job boundary: `CoreError` is not `Clone`, and a report is
/// (plan §9.1's measured fact — "a report struct wrapping `CoreError` cannot derive `Clone`").
pub(crate) type SharedError = Arc<CoreError>;

/// Wrap an error for a report field.
#[must_use]
pub fn shared(error: CoreError) -> SharedError {
    Arc::new(error)
}
