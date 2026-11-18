//! Paste: guard, place, log, invalidate (plan §5.3 `ops/paste.rs`, Go `handlePaste` `api/ops.go:226-366`
//! and `handlePasteLink` `:839-917`).
//!
//! This is the piece that turns a selection and a destination into work, and it is deliberately *not*
//! where the copying lives — [`crate::ops::copy`] and [`crate::ops::r#move`] own that, so a collision is
//! answered identically whichever way the bytes go.
//!
//! The shape of the answer differs from Go's in one load-bearing way: Go accumulated one `lastErr` and
//! returned a 500 that named the last failure while having already placed the earlier files
//! (`api/ops.go:313-338,346-349`). Here every source gets its own [`PasteOutcome`] — placed, skipped with
//! a reason, or failed with a typed error — and the operation as a whole only fails when *nothing could
//! even be attempted* (a wholly refused selection, a cancelled job, a platform that cannot make links).
//!
//! The undo-log record is appended by the caller ([`crate::Session::paste`]), after the walk, and carries
//! the sources that actually landed — plan F3's rule for a delete is used here too: the record describes
//! the work that ran, so a revert never has to guess.

use std::sync::Arc;

use crate::error::CoreError;
use crate::history::codec::{ActionEntry, ActionType};
use crate::history::store::LogId;
use crate::jobs::{CancelToken, block_on};
use crate::ops::copy::{CopyStats, copy_item};
use crate::ops::guard::{SourceGuard, SourceReject};
use crate::ops::r#move::{MoveStats, move_item};
use crate::ops::{ConflictPolicy, ProgressSink};
use crate::path::AbsPath;
use crate::platform::{Env, LinkItem};

/// Go's `trackEntries` threshold (`api/ops.go:311`): past 100 sources it stopped describing the results and
/// told the client to reload the directory instead. Ledger #12 — kept, because it is a real payload
/// decision and the UI's "reload quietly" path is built on it.
pub const TRACK_ENTRIES_CAP: usize = 100;

/// Which of Go's three paste handlers this request is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteMode {
    /// `handlePaste` with a `copy` clipboard (`api/ops.go:325`).
    Copy,
    /// `handlePaste` with a `cut` clipboard (`api/ops.go:321`): the sources are gone when it returns.
    Cut,
    /// `handlePasteLink` (`api/ops.go:839`): symlinks at the destination, sources untouched.
    Link,
}

impl PasteMode {
    /// The action type the undo-log record carries (`ActionPasteCopy`/`ActionPasteMove`/`ActionPasteLink`,
    /// `api/ops.go:352-355` and `:913`).
    #[must_use]
    pub const fn action(self) -> ActionType {
        match self {
            Self::Copy => ActionType::PASTE_COPY,
            Self::Cut => ActionType::PASTE_MOVE,
            Self::Link => ActionType::PASTE_LINK,
        }
    }
}

/// One paste, already resolved to concrete sources.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PasteReq {
    /// What the selection holds — the guard expands and checks these.
    pub sources: Vec<AbsPath>,
    /// The destination **directory**, already known writable.
    pub dest: AbsPath,
    /// Copy, cut or link.
    pub mode: PasteMode,
    /// What a taken destination name means.
    pub policy: ConflictPolicy,
}

/// One source's result.
#[derive(Clone, Debug)]
pub enum PasteOutcome {
    /// The bytes/link landed at `target`.
    Placed {
        /// What was read.
        source: AbsPath,
        /// Where it landed — [`ConflictPolicy::Rename`] can move it off the proposed name.
        target: AbsPath,
    },
    /// Nothing was written, for a reason the UI has to say out loud (plan F12).
    Skipped {
        /// The source that was left where it was.
        source: AbsPath,
        /// The destination that was already occupied.
        target: AbsPath,
        /// Why.
        reason: SkipReason,
    },
    /// The attempt failed; the other sources are unaffected.
    Failed {
        /// The source that could not be placed.
        source: AbsPath,
        /// The destination it failed against.
        target: AbsPath,
        /// Why, shared so the report stays `Clone` (plan §9.1).
        error: Arc<CoreError>,
    },
}

impl PasteOutcome {
    /// The source this outcome is about.
    #[must_use]
    pub fn source(&self) -> &AbsPath {
        match self {
            Self::Placed { source, .. }
            | Self::Skipped { source, .. }
            | Self::Failed { source, .. } => source,
        }
    }

    /// Two outcomes are equal when they name the same source, target and — for a failure — the same
    /// error *kind and message*. `CoreError` itself is not `PartialEq` (it carries an `io::Error`), and
    /// making it so would mean comparing OS error strings; the two accessors that are already the UI's
    /// answer are what a test should be comparing anyway.
    #[must_use]
    pub fn same_as(&self, other: &Self) -> bool {
        let (sp, op) = (self.source(), other.source());
        if sp != op {
            return false;
        }
        match (self, other) {
            (Self::Placed { target: a, .. }, Self::Placed { target: b, .. }) => a == b,
            (
                Self::Skipped {
                    target: a,
                    reason: ra,
                    ..
                },
                Self::Skipped {
                    target: b,
                    reason: rb,
                    ..
                },
            ) => a == b && ra == rb,
            (Self::Failed { error: a, .. }, Self::Failed { error: b, .. }) => {
                a.kind() == b.kind() && a.user_message() == b.user_message()
            }
            _ => false,
        }
    }
}

/// Why a source was not placed even though nothing failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// The destination name was occupied and the policy was [`ConflictPolicy::SkipExisting`] — Go's silent
    /// `copyFile` skip (ledger #9), now reported.
    Existing,
}

/// Everything one paste did.
#[derive(Clone, Debug)]
pub struct PasteReport {
    /// Which handler this was.
    pub mode: PasteMode,
    /// The destination directory.
    pub dest: AbsPath,
    /// One entry per allowed source, in guard order.
    pub outcomes: Vec<PasteOutcome>,
    /// The sources the containment guard refused, with the reason (`api/ops.go:309`'s silent `continue`).
    pub rejected: Vec<(AbsPath, SourceReject)>,
    /// Names of the directory-onto-directory pairings that merged — the payload of Go's
    /// `status:"conflict"` answer (ledger #8), which the UI turns into the merge confirmation instead of
    /// a second round trip.
    pub conflicts: Vec<String>,
    /// Bytes written (0 for a same-device cut, which renames).
    pub bytes: u64,
    /// Entries written or moved.
    pub files: u64,
    /// Go's `reload_required` (`api/ops.go:363`): `true` when the selection was too large to describe.
    pub reload_required: bool,
    /// The undo-log record's id, once the caller has appended it.
    pub log_id: Option<LogId>,
}

impl PasteReport {
    /// The sources that actually landed, in order — what the log record carries and what a revert reads.
    #[must_use]
    pub fn placed(&self) -> Vec<(&AbsPath, &AbsPath)> {
        self.outcomes
            .iter()
            .filter_map(|o| match o {
                PasteOutcome::Placed { source, target } => Some((source, target)),
                _ => None,
            })
            .collect()
    }

    /// The skipped sources.
    #[must_use]
    pub fn skipped(&self) -> Vec<&AbsPath> {
        self.outcomes
            .iter()
            .filter_map(|o| match o {
                PasteOutcome::Skipped { source, .. } => Some(source),
                _ => None,
            })
            .collect()
    }

    /// The failed sources, each with its error.
    #[must_use]
    pub fn failed(&self) -> Vec<(&AbsPath, &CoreError)> {
        self.outcomes
            .iter()
            .filter_map(|o| match o {
                PasteOutcome::Failed { source, error, .. } => Some((source, error.as_ref())),
                _ => None,
            })
            .collect()
    }

    /// `true` when nothing landed.
    #[must_use]
    pub fn placed_any(&self) -> bool {
        !self.placed().is_empty()
    }

    /// `true` when any source failed or was skipped — the condition for a warning toast (plan F12).
    #[must_use]
    pub fn incomplete(&self) -> bool {
        self.outcomes
            .iter()
            .any(|o| !matches!(o, PasteOutcome::Placed { .. }))
            || !self.rejected.is_empty()
    }

    /// The jobs-pane summary line.
    #[must_use]
    pub fn summary(&self) -> String {
        let placed = self.placed().len();
        let skipped = self.skipped().len();
        let failed = self.failed().len();
        format!("{placed} placed, {skipped} skipped, {failed} failed")
    }

    /// The Go-parity log entry: `(action, placed sources, dest, "")` — the field roles of
    /// `GetLog().Append(logAction, paths, req.Dest, "")` (`api/ops.go:356`) and of the frozen
    /// `paste-copy`/`paste-move`/`paste-link` records in `tests/golden/log.json`.
    ///
    /// `None` when nothing landed: Go appended a record for a paste that placed nothing (it appended
    /// *before* checking `lastErr` in the link handler, `api/ops.go:913`), which is an undo entry for work
    /// that never ran.
    #[must_use]
    pub fn log_entry(&self) -> Option<ActionEntry> {
        let sources: Vec<String> = self
            .placed()
            .into_iter()
            .map(|(source, _)| source.to_string_lossy())
            .collect();
        if sources.is_empty() {
            return None;
        }
        Some(ActionEntry::new(
            self.mode.action(),
            sources,
            self.dest.to_string_lossy(),
            "",
        ))
    }
}

impl std::cmp::PartialEq for PasteOutcome {
    fn eq(&self, other: &Self) -> bool {
        self.same_as(other)
    }
}

/// The report's `PartialEq` is field-wise, with [`PasteOutcome`]'s error comparison inside it — needed
/// because [`crate::events::JobPayload`] is `PartialEq` and carries this.
impl std::cmp::PartialEq for PasteReport {
    fn eq(&self, other: &Self) -> bool {
        self.mode == other.mode
            && self.dest == other.dest
            && self.outcomes.len() == other.outcomes.len()
            && self
                .outcomes
                .iter()
                .zip(other.outcomes.iter())
                .all(|(a, b)| a.same_as(b))
            && self.rejected == other.rejected
            && self.conflicts == other.conflicts
            && self.bytes == other.bytes
            && self.files == other.files
            && self.reload_required == other.reload_required
            && self.log_id == other.log_id
    }
}

/// Run one paste.
///
/// `env` is asked only for the two platform answers the engine cannot derive: whether symlinks exist
/// (a copied tree recreates them or follows them) and, in [`PasteMode::Link`], the link creation itself.
/// `fs` is the [`MoveFs`] seam the cut path renames through.
///
/// `Err` means the paste did not start: an empty selection, a wholly refused one
/// ([`CoreError::DestInsideSource`]), an unsupported link platform, or cancellation. Per-source failures
/// are in the report.
///
/// # Errors
/// See above; every inner failure is a [`PasteOutcome::Failed`], never an `Err`.
pub fn paste(
    env: &dyn Env,
    fs: &dyn crate::ops::r#move::MoveFs,
    req: &PasteReq,
    guard: &SourceGuard,
    cancel: &CancelToken,
    progress: &dyn ProgressSink,
) -> Result<PasteReport, CoreError> {
    let symlinks = env.supports_symlink().unwrap_or(false);
    let mut report = PasteReport {
        mode: req.mode,
        dest: req.dest.clone(),
        outcomes: Vec::with_capacity(guard.allowed.len()),
        rejected: guard.rejected.to_vec(),
        conflicts: Vec::new(),
        bytes: 0,
        files: 0,
        reload_required: guard.allowed.len() > TRACK_ENTRIES_CAP,
        log_id: None,
    };

    if req.mode == PasteMode::Link {
        let items = link_items(guard, req.policy, &mut report)?;
        if !items.is_empty() {
            block_on(env.paste_link(&items))??;
        }
        return Ok(report);
    }

    for allowed in &guard.allowed {
        cancel.check()?;
        if dir_onto_dir(&allowed.source, &allowed.target) {
            report.conflicts.push(name_of(&allowed.source));
        }
        let outcome = if req.mode == PasteMode::Cut {
            move_item(
                &allowed.source,
                &allowed.target,
                req.policy,
                fs,
                cancel,
                progress,
            )
            .map(|stats| (stats.dest.clone(), stats_into(&stats)))
        } else {
            copy_item(
                &allowed.source,
                &allowed.target,
                req.policy,
                symlinks,
                cancel,
                progress,
            )
            .map(|stats| (stats.dest.clone(), stats_into_copy(&stats)))
        };
        match outcome {
            Err(ref e) if matches!(e, CoreError::Cancelled) => return Err(CoreError::Cancelled),
            Err(e) => report.outcomes.push(PasteOutcome::Failed {
                source: allowed.source.clone(),
                target: allowed.target.clone(),
                error: Arc::new(e),
            }),
            Ok((dest, (bytes, files, dirs, skipped))) => {
                report.bytes += bytes;
                report.files += files + dirs;
                if let Some(reason) = skipped {
                    report.outcomes.push(PasteOutcome::Skipped {
                        source: allowed.source.clone(),
                        target: reason,
                        reason: SkipReason::Existing,
                    });
                } else {
                    report.outcomes.push(PasteOutcome::Placed {
                        source: allowed.source.clone(),
                        target: dest.unwrap_or_else(|| allowed.target.clone()),
                    });
                }
            }
        }
    }
    progress.advance(0, 0, "");
    Ok(report)
}

// -------------------------------------------------------------------------------------- internals

fn name_of(path: &AbsPath) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Go's conflict rule (`api/ops.go:257-262`): `os.Stat` on both sides, and both must be directories.
fn dir_onto_dir(src: &AbsPath, dst: &AbsPath) -> bool {
    std::fs::metadata(src.as_path())
        .is_ok_and(|m| m.is_dir() && std::fs::metadata(dst.as_path()).is_ok_and(|d| d.is_dir()))
}

/// Flatten the two stats shapes into the four numbers the report keeps.
fn stats_into(stats: &MoveStats) -> (u64, u64, u64, Option<AbsPath>) {
    (
        stats.bytes,
        stats.files,
        stats.dirs,
        stats.skipped.first().cloned(),
    )
}

fn stats_into_copy(stats: &CopyStats) -> (u64, u64, u64, Option<AbsPath>) {
    (
        stats.bytes,
        stats.files,
        stats.dirs,
        stats.skipped.first().cloned(),
    )
}

fn link_items(
    guard: &SourceGuard,
    policy: ConflictPolicy,
    report: &mut PasteReport,
) -> Result<Vec<LinkItem>, CoreError> {
    let mut items = Vec::new();
    for allowed in &guard.allowed {
        let target = match policy.decide(false, &allowed.target)? {
            crate::ops::ConflictDecision::Skip => {
                report.outcomes.push(PasteOutcome::Skipped {
                    source: allowed.source.clone(),
                    target: allowed.target.clone(),
                    reason: SkipReason::Existing,
                });
                continue;
            }
            crate::ops::ConflictDecision::Rename(new) => new,
            _ => allowed.target.clone(),
        };
        items.push(LinkItem {
            link: target,
            target: allowed.source.clone(),
        });
    }
    // The link handler has no per-item error channel (`Env::paste_link` reports one answer for the batch),
    // so every item it was given is reported as placed and the batch failure is the operation's.
    for item in &items {
        let source = guard
            .allowed
            .iter()
            .find(|a| a.target == item.link || a.source == item.target)
            .map(|a| a.source.clone());
        if let Some(source) = source {
            report.outcomes.push(PasteOutcome::Placed {
                source,
                target: item.link.clone(),
            });
        }
    }
    report.files += items.len() as u64;
    Ok(items)
}
