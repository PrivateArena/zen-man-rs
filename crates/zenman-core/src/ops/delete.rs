//! Delete: trash first, permanent only when the user said so (plan §5.3 `ops/delete.rs`, Go
//! `handleDelete` `api/ops.go:367-387`, plan §6.2 deviation **D6**).
//!
//! Go's delete was `os.RemoveAll` — the bytes were gone, and the undo-log record it appended
//! (`ActionDelete`, golden record 7/8 of `tests/golden/log.json`) was a *record of a loss*, which is why
//! [`ActionType::DELETE`] is not reversible and `api/log_revert.go` answers
//! `"delete is permanent on SSD — recovery requires a backup"`. The port keeps the record and drops the
//! irreversibility where the platform can: a desktop trash bin.
//!
//! # The fallback ladder (D6, as implemented)
//!
//! For each source, with [`DeleteMode::Trash`] requested:
//!
//! 1. [`Env::can_trash`] — is there a trash location for this path at all? If yes, [`Env::trash`] and,
//!    on `Ok(true)`, stop: the item is recoverable through the desktop's own bin.
//! 2. `Ok(false)` is `trash`'s [`trash::Error::TargetedRoot`] refusal (a root folder, or no trash
//!    destination for that volume). The item is *not* silently destroyed: it is removed permanently **only
//!    when the caller opted in** ([`DeleteReq::permanent_fallback`], the preference behind plan D6 /
//!    Shift+Delete), and the outcome says so (`Removed { fallback: true }`). Without the opt-in the source
//!    fails with [`CoreError::PermissionDenied`] — the user asked for a trash delete and this volume has
//!    none (plan §9.1's measured fact: a `/media/…` volume with its own `statvfs` fsid has no native trash,
//!    so this rung is reachable in ordinary use and [`DeleteMode::Permanent`] must be offered).
//! 3. `Err` from [`Env::trash`] is reported as a failure and **never** escalates: a trash error can mean
//!    the item moved half-way, and answering it by deleting is how a fallback ladder eats data.
//!
//! With [`DeleteMode::Permanent`] requested the ladder is skipped and [`remove_permanently`] runs directly.
//!
//! # Cancellation is truthful, not silent (plan F3)
//!
//! The token is checked per top-level entry. A cancel that lands after some entries went through returns
//! `Ok(report)` with [`DeleteReport::cancelled`] set, so the caller can still append the log record — for
//! the paths that *actually* disappeared, which is the plan's rule ("a cancelled delete leaves a truthful
//! record rather than an undo entry for work that never ran"). A cancel before the first entry is
//! [`CoreError::Cancelled`], because there would be nothing to record.

use std::sync::Arc;

use crate::error::CoreError;
use crate::history::codec::{ActionEntry, ActionType};
use crate::history::store::LogId;
use crate::jobs::CancelToken;
use crate::ops::ProgressSink;
use crate::path::AbsPath;
use crate::platform::Env;

/// How a delete is allowed to end (`plan §5.3`'s `DeleteMode`; the same type plan §5.6's
/// `Prefs::delete_mode` stores, and what plan §6.2 **D6** puts behind the preference and Shift+Delete).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DeleteMode {
    /// Move to the platform trash; escalate to a permanent removal only when
    /// [`DeleteReq::permanent_fallback`] says the user accepted that.
    #[default]
    Trash,
    /// Remove the bytes. Go's only behaviour (`api/ops.go:371` `os.RemoveAll`).
    Permanent,
}

impl DeleteMode {
    /// The undo-log action the record carries — the same for both modes, because Go logged one action
    /// type and the record has no field for the mode (golden records 7/8: `sources`, empty `dest`, empty
    /// `name`).
    #[must_use]
    pub const fn action(self) -> ActionType {
        ActionType::DELETE
    }
}

/// One delete request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteReq {
    /// The selected paths — directories and files mixed, as Go received them (`api/ops.go:369`).
    pub sources: Vec<AbsPath>,
    /// Trash or permanent.
    pub mode: DeleteMode,
    /// The plan D6 opt-in: when the trash refuses a path, may this delete destroy it instead?
    ///
    /// `false` is the safe default and what the confirmation dialog answers "No" to; `true` is the
    /// preference (`Prefs::delete_mode`) / Shift+Delete lane. It is *not* consulted when
    /// [`DeleteMode::Permanent`] was requested outright — there the user already asked for the bytes to go.
    pub permanent_fallback: bool,
}

impl DeleteReq {
    /// A trash delete with no escalation — the default, and what a plain <kbd>Del</kbd> presses.
    #[must_use]
    pub fn to_trash(sources: Vec<AbsPath>) -> Self {
        Self {
            sources,
            mode: DeleteMode::Trash,
            permanent_fallback: false,
        }
    }

    /// A permanent delete — Go's behaviour, and what Shift+Delete presses (plan §6.2 D6).
    #[must_use]
    pub fn permanent(sources: Vec<AbsPath>) -> Self {
        Self {
            sources,
            mode: DeleteMode::Permanent,
            permanent_fallback: false,
        }
    }
}

/// One source's result. Go had one `lastErr` for the whole batch and a `deleted` list (`api/ops.go:368`),
/// so a partial delete reported the *last* failure only; here every source answers for itself.
#[derive(Clone, Debug)]
pub enum DeleteOutcome {
    /// Moved to the platform trash: recoverable, and the only outcome `Trash` produces on success.
    Trashed {
        /// What disappeared from its directory.
        path: AbsPath,
    },
    /// The bytes are gone.
    Removed {
        /// What disappeared.
        path: AbsPath,
        /// `true` when this was the ladder's escalation — the user asked for trash and this volume has
        /// none, so the item is *not* in any bin. The UI has to say that out loud.
        fallback: bool,
    },
    /// Nothing was removed.
    Failed {
        /// What is still where it was.
        path: AbsPath,
        /// Why, shared so the report stays `Clone` (plan §9.1).
        error: Arc<CoreError>,
    },
}

impl DeleteOutcome {
    /// The path this outcome is about.
    #[must_use]
    pub fn path(&self) -> &AbsPath {
        match self {
            Self::Trashed { path } | Self::Removed { path, .. } | Self::Failed { path, .. } => path,
        }
    }

    /// `true` when the path is gone (by either route).
    #[must_use]
    pub fn is_gone(&self) -> bool {
        matches!(self, Self::Trashed { .. } | Self::Removed { .. })
    }
}

/// Everything one delete did.
#[derive(Clone, Debug)]
pub struct DeleteReport {
    /// The mode that was asked for (plan §5.3's `mode_used`: it is the *request*, because a per-source
    /// escalation lives in the outcomes, not in a single answer for the batch).
    pub mode_used: DeleteMode,
    /// One entry per source, in request order.
    pub outcomes: Vec<DeleteOutcome>,
    /// The walk stopped on the user's cancel: the outcomes are complete for what ran, and the log record
    /// still describes the truth (plan F3).
    pub cancelled: bool,
    /// The undo-log record's id, once the caller has appended it.
    pub log_id: Option<LogId>,
}

impl DeleteReport {
    /// The paths that are gone, whichever route removed them — what the log record carries.
    #[must_use]
    pub fn deleted(&self) -> Vec<&AbsPath> {
        self.outcomes
            .iter()
            .filter(|o| o.is_gone())
            .map(DeleteOutcome::path)
            .collect()
    }

    /// The paths that reached the trash.
    #[must_use]
    pub fn trashed(&self) -> Vec<&AbsPath> {
        self.outcomes
            .iter()
            .filter_map(|o| match o {
                DeleteOutcome::Trashed { path } => Some(path),
                _ => None,
            })
            .collect()
    }

    /// The paths whose bytes are gone, with the escalation flag.
    #[must_use]
    pub fn removed(&self) -> Vec<(&AbsPath, bool)> {
        self.outcomes
            .iter()
            .filter_map(|o| match o {
                DeleteOutcome::Removed { path, fallback } => Some((path, *fallback)),
                _ => None,
            })
            .collect()
    }

    /// The paths still on disk, each with its error.
    #[must_use]
    pub fn failed(&self) -> Vec<(&AbsPath, &CoreError)> {
        self.outcomes
            .iter()
            .filter_map(|o| match o {
                DeleteOutcome::Failed { path, error } => Some((path, error.as_ref())),
                _ => None,
            })
            .collect()
    }

    /// The parent directories that lost a child — one [`DirChanged`](crate::events::EngineEvent::DirChanged)
    /// per distinct parent (plan F13).
    #[must_use]
    pub fn affected_parents(&self) -> Vec<AbsPath> {
        let mut parents: Vec<AbsPath> = Vec::new();
        for outcome in self.outcomes.iter().filter(|o| o.is_gone()) {
            if let Some(parent) = DeleteOutcome::path(outcome).parent()
                && !parents.contains(&parent)
            {
                parents.push(parent);
            }
        }
        parents
    }

    /// `true` when anything failed or the walk was cut short — the condition for the warning toast.
    #[must_use]
    pub fn incomplete(&self) -> bool {
        self.cancelled || self.outcomes.iter().any(|o| !o.is_gone())
    }

    /// The jobs-pane summary line.
    #[must_use]
    pub fn summary(&self) -> String {
        let trashed = self.trashed().len();
        let removed = self.removed().len();
        let failed = self.failed().len();
        format!("{trashed} trashed, {removed} deleted, {failed} failed")
    }

    /// The Go-parity log entry: `Append(ActionDelete, deleted, "", "")` (`api/ops.go:378`), appended
    /// **after** the walk and carrying only the paths that actually went (Go's comment at
    /// `api/ops.go:376`: "Log only confirmed deletions — partial success is still logged").
    ///
    /// The record is *not* reversible: [`ActionType::DELETE`] is outside the five
    /// `api/log.go:600-602` admits, and `is_reversible()` on the decoded record says so. It exists so the
    /// history pane can show what was lost and say which of them cannot come back.
    #[must_use]
    pub fn log_entry(&self) -> Option<ActionEntry> {
        let deleted: Vec<String> = self
            .deleted()
            .into_iter()
            .map(|p| p.to_string_lossy())
            .collect();
        if deleted.is_empty() {
            return None;
        }
        Some(ActionEntry::new(self.mode_used.action(), deleted, "", ""))
    }
}

impl std::cmp::PartialEq for DeleteReport {
    fn eq(&self, other: &Self) -> bool {
        fn same(a: &DeleteOutcome, b: &DeleteOutcome) -> bool {
            if a.path() != b.path() {
                return false;
            }
            match (a, b) {
                (DeleteOutcome::Trashed { .. }, DeleteOutcome::Trashed { .. }) => true,
                (
                    DeleteOutcome::Removed { fallback: x, .. },
                    DeleteOutcome::Removed { fallback: y, .. },
                ) => x == y,
                (
                    DeleteOutcome::Failed { error: x, .. },
                    DeleteOutcome::Failed { error: y, .. },
                ) => x.kind() == y.kind() && x.user_message() == y.user_message(),
                _ => false,
            }
        }
        self.mode_used == other.mode_used
            && self.outcomes.len() == other.outcomes.len()
            && self
                .outcomes
                .iter()
                .zip(other.outcomes.iter())
                .all(|(a, b)| same(a, b))
            && self.cancelled == other.cancelled
            && self.log_id == other.log_id
    }
}

/// Delete a batch. `Err` only when *nothing* ran: an empty selection, a cancel before the first entry, or
/// a cancellation that removed nothing. Per-source failures are in the report (plan F3's partial results).
///
/// # Errors
/// [`CoreError::Empty`] for an empty selection and [`CoreError::Cancelled`]; every inner failure is a
/// [`DeleteOutcome::Failed`].
pub fn delete(
    env: &dyn Env,
    req: &DeleteReq,
    cancel: &CancelToken,
    progress: &dyn ProgressSink,
) -> Result<DeleteReport, CoreError> {
    if req.sources.is_empty() {
        return Err(CoreError::Empty { what: "selection" });
    }
    let mut report = DeleteReport {
        mode_used: req.mode,
        outcomes: Vec::with_capacity(req.sources.len()),
        cancelled: false,
        log_id: None,
    };
    for (i, source) in req.sources.iter().enumerate() {
        if i > 0 && cancel.is_cancelled() {
            // Plan F3: keep what happened, record it, and stop — the *first* entry is not special-cased
            // because a delete of one file is too fast to cancel anyway.
            report.cancelled = true;
            break;
        }
        let outcome = remove_one(env, req, source);
        let gone = outcome.is_gone();
        report.outcomes.push(outcome);
        progress.advance(u64::from(gone), 1, &file_name(source));
    }
    if report.deleted().is_empty() && report.cancelled {
        // A cancel that removed nothing leaves no truthful record behind, so it is a plain cancellation
        // (module docs, plan F3). A *partial* cancel returns the report, because that one can be logged.
        return Err(CoreError::Cancelled);
    }
    Ok(report)
}

/// One source, through the ladder of the module docs.
fn remove_one(env: &dyn Env, req: &DeleteReq, source: &AbsPath) -> DeleteOutcome {
    let failed = |e: CoreError| DeleteOutcome::Failed {
        path: source.clone(),
        error: Arc::new(e),
    };
    if req.mode == DeleteMode::Trash {
        if !env.can_trash(source) {
            // Rung 2 of the ladder: no trash location for this path at all.
            return if req.permanent_fallback {
                match remove_permanently(source) {
                    Ok(()) => DeleteOutcome::Removed {
                        path: source.clone(),
                        fallback: true,
                    },
                    Err(e) => failed(e),
                }
            } else {
                failed(CoreError::PermissionDenied {
                    path: source.to_string_lossy(),
                })
            };
        }
        match env.trash(source) {
            Ok(true) => {
                return DeleteOutcome::Trashed {
                    path: source.clone(),
                };
            }
            // `trash::Error::TargetedRoot` (`platform::linux::trash`'s documented `Ok(false)`): the trash
            // exists but refuses this path. Same rung as above.
            Ok(false) => {
                return if req.permanent_fallback {
                    match remove_permanently(source) {
                        Ok(()) => DeleteOutcome::Removed {
                            path: source.clone(),
                            fallback: true,
                        },
                        Err(e) => failed(e),
                    }
                } else {
                    failed(CoreError::PermissionDenied {
                        path: source.to_string_lossy(),
                    })
                };
            }
            // Rung 3: a real failure while trying. Never escalated (module docs).
            Err(e) => return failed(e),
        }
    }
    match remove_permanently(source) {
        Ok(()) => DeleteOutcome::Removed {
            path: source.clone(),
            fallback: false,
        },
        Err(e) => failed(e),
    }
}

/// Remove the bytes: a file/symlink unlink, a directory recursively.
///
/// The recursive half is the one `clippy.toml`-banned call in the crate, and this is its named exception:
/// the ban exists so no code path deletes a tree *except* through this module, where the caller had to ask
/// for [`DeleteMode::Permanent`] (or accept the fallback) and the removal lands in a log record.
///
/// # Errors
/// [`CoreError::Io`] with the path that refused, or [`CoreError::NotFound`] when it is already gone (an
/// idempotent answer the revert path relies on).
// The one `remove_dir_all` in the crate: plan §5.3 names this call site as the sole exception to the
// `clippy.toml` ban, because permanent deletion lives here and nowhere else.
#[allow(
    clippy::disallowed_methods,
    reason = "the one named permanent-delete call site: plan §5.3"
)]
pub fn remove_permanently(path: &AbsPath) -> Result<(), CoreError> {
    let meta = std::fs::symlink_metadata(path.as_path()).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            CoreError::NotFound {
                path: path.to_string_lossy(),
                source_path: None,
            }
        } else {
            CoreError::Io {
                path: path.to_string_lossy(),
                io: Arc::new(e),
            }
        }
    })?;
    if meta.is_dir() {
        // The single sanctioned `remove_dir_all` call site (see the `#[allow]` on this function): permanent
        // deletion is this function, and only this function, and only ever reached from an explicit
        // permanent request or the plan §6.2 D6 fallback.
        std::fs::remove_dir_all(path.as_path())
    } else {
        std::fs::remove_file(path.as_path())
    }
    .map_err(|e| CoreError::Io {
        path: path.to_string_lossy(),
        io: Arc::new(e),
    })
}

fn file_name(path: &AbsPath) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}
