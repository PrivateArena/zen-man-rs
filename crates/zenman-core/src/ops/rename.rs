//! Rename: validate the name, decide the collision, rename in place (plan §5.3 `ops/rename.rs`, Go
//! `handleRename` `api/ops.go:391-486`).
//!
//! Two Go behaviours are kept and one is dropped:
//!
//! * **kept** — the batch rename's *sources rewrite* (`api/ops.go:466-474`): after `/root/a` becomes
//!   `/root/A`, a later source `/root/a/b` is rewritten to `/root/A/b` before it is renamed, or a batch
//!   that renames a parent directory renames its children out from under themselves. Plan §5.3 names this
//!   as the case the port must reproduce; [`rename`] does, and [`contract_ops::a_batch_rename_renames_a_parent_and_its_children`]
//!   is the test.
//! * **kept** — Go's per-item `continue` on an empty name and on a rename to the identical path
//!   (`api/ops.go:448-456`); here each skip is a value in the report, never an absence.
//! * **dropped** — Go's batch failure aborted the HTTP request having already renamed the earlier items
//!   and appended a record for them (`api/ops.go:461-465`), so the client could not tell what had happened.
//!   Here every source is attempted and the report carries both lists.
//!
//! The plan's other deviation applies to the *request*, not the record: [`RenameTarget::Batch`] replaces
//! the JSON-string-in-`name` hack (`api/ops.go:437`), while the record that reaches the undo log keeps
//! Go's byte shape — a plain name for a single rename, a JSON array for a batch (`tests/golden/log.json`
//! records 2, 14, 15).

use std::sync::Arc;

use crate::error::CoreError;
use crate::history::codec::{ActionEntry, ActionType};
use crate::history::store::LogId;
use crate::jobs::CancelToken;
use crate::ops::r#move::MoveFs;
use crate::ops::{ConflictPolicy, ProgressSink};
use crate::path::{AbsPath, validate_dest_name};

/// What the selection is being renamed to (plan §5.3's structured target).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenameTarget {
    /// One new name, applied to the single source. Go's `req.Name` for a one-item selection.
    One(String),
    /// One name per source, positionally. Go's JSON array, parsed at the boundary instead of inside the
    /// handler; a short/long list is a request error, exactly as `api/ops.go:441-444` answered it.
    Batch(Vec<String>),
}

impl RenameTarget {
    /// The names this target carries, in order.
    #[must_use]
    pub fn names(&self) -> &[String] {
        match self {
            Self::One(name) => std::slice::from_ref(name),
            Self::Batch(names) => names,
        }
    }

    /// `true` for [`Self::One`].
    #[must_use]
    pub fn is_single(&self) -> bool {
        matches!(self, Self::One(_))
    }
}

/// One rename (or one batch rename).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameReq {
    /// The selection, in the order the UI listed it — that order is load-bearing for the batch case
    /// (a parent must precede its children for Go's rewrite to fire, and it does in a sorted listing).
    pub sources: Vec<AbsPath>,
    /// The new name(s).
    pub target: RenameTarget,
    /// What a taken name means. Go had no policy — `os.Rename` clobbered (POSIX) — so this is the plan's
    /// addition; [`ConflictPolicy::SkipExisting`] is what the v1 dialog passes.
    pub policy: ConflictPolicy,
}

impl RenameReq {
    /// Rename one path.
    #[must_use]
    pub fn one(source: AbsPath, name: impl Into<String>) -> Self {
        Self {
            sources: vec![source],
            target: RenameTarget::One(name.into()),
            policy: ConflictPolicy::SkipExisting,
        }
    }

    /// Rename a batch, positionally.
    #[must_use]
    pub fn batch(sources: Vec<AbsPath>, names: Vec<String>) -> Self {
        Self {
            sources,
            target: RenameTarget::Batch(names),
            policy: ConflictPolicy::SkipExisting,
        }
    }
}

/// One source's result, in request order.
#[derive(Clone, Debug)]
pub enum RenameOutcome {
    /// `from` is now at `to`.
    Renamed {
        /// Before.
        from: AbsPath,
        /// After.
        to: AbsPath,
    },
    /// Nothing happened, with the reason the UI has to say (`""` name, same name, taken name, a bad
    /// name, an I/O failure).
    Failed {
        /// The path that is still where it was.
        path: AbsPath,
        /// Why, shared so the report stays `Clone` (plan §9.1).
        error: Arc<CoreError>,
    },
}

impl RenameOutcome {
    /// The path this outcome started from.
    #[must_use]
    pub fn source(&self) -> &AbsPath {
        match self {
            Self::Renamed { from, .. } | Self::Failed { path: from, .. } => from,
        }
    }
}

/// Everything one rename call did.
#[derive(Clone, Debug, Default)]
pub struct RenameReport {
    /// One entry per source, in request order.
    pub outcomes: Vec<RenameOutcome>,
    /// The undo-log record's id, once the caller has appended it.
    pub log_id: Option<LogId>,
}

impl RenameReport {
    /// The pairs that changed name, in order — what the log record carries.
    #[must_use]
    pub fn renamed(&self) -> Vec<(&AbsPath, &AbsPath)> {
        self.outcomes
            .iter()
            .filter_map(|o| match o {
                RenameOutcome::Renamed { from, to } => Some((from, to)),
                RenameOutcome::Failed { .. } => None,
            })
            .collect()
    }

    /// The sources that did not move, each with its reason (plan §5.3's `failed: Vec<(AbsPath,String)>`,
    /// typed rather than stringified per plan §6.2's "errors are typed").
    #[must_use]
    pub fn failed(&self) -> Vec<(&AbsPath, &CoreError)> {
        self.outcomes
            .iter()
            .filter_map(|o| match o {
                RenameOutcome::Failed { path, error } => Some((path, error.as_ref())),
                RenameOutcome::Renamed { .. } => None,
            })
            .collect()
    }

    /// `true` when two reports name the same sources, targets and — for failures — the same error
    /// *kind and message* (the reason [`crate::ops::paste::PasteOutcome`] compares the same way: a
    /// [`CoreError`] carrying an `io::Error` is neither `PartialEq` nor `Clone`).
    #[must_use]
    pub fn same_as(&self, other: &Self) -> bool {
        fn same(a: &RenameOutcome, b: &RenameOutcome) -> bool {
            if a.source() != b.source() {
                return false;
            }
            match (a, b) {
                (RenameOutcome::Renamed { to: x, .. }, RenameOutcome::Renamed { to: y, .. }) => {
                    x == y
                }
                (
                    RenameOutcome::Failed { error: x, .. },
                    RenameOutcome::Failed { error: y, .. },
                ) => x.kind() == y.kind() && x.user_message() == y.user_message(),
                _ => false,
            }
        }
        self.outcomes.len() == other.outcomes.len()
            && self
                .outcomes
                .iter()
                .zip(other.outcomes.iter())
                .all(|(a, b)| same(a, b))
            && self.log_id == other.log_id
    }

    /// The directory the names live in, when every source shares one — the parent each new name resolves
    /// against (Go's `filepath.Dir(src)`, `api/ops.go:400`).
    #[must_use]
    pub fn parent(&self) -> Option<AbsPath> {
        self.outcomes
            .iter()
            .map(|o| o.source().parent())
            .collect::<Option<Vec<_>>>()
            .and_then(|v| v.first().cloned())
    }

    /// `true` when anything was refused.
    #[must_use]
    pub fn incomplete(&self) -> bool {
        !self
            .outcomes
            .iter()
            .all(|o| matches!(o, RenameOutcome::Renamed { .. }))
    }

    /// The jobs/status summary line.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{} renamed, {} failed",
            self.renamed().len(),
            self.failed().len()
        )
    }

    /// The Go-parity log entry — `Append(ActionRename, renamedSources, "", name)` (`api/ops.go:414,478`),
    /// where `name` is the new name for a single rename and a **JSON array of the names that actually
    /// applied** for a batch (Go marshals `renamedNewNames`, `api/ops.go:477`, i.e. the successes only —
    /// which is what makes a partial batch revertible).
    ///
    /// `None` when nothing renamed anything: Go appended a record with empty sources and `name:"null"` in
    /// that case (`api/ops.go:477` marshals an empty slice to `null`), which is an undo entry for nothing.
    #[must_use]
    pub fn log_entry(&self) -> Option<ActionEntry> {
        let pairs = self.renamed();
        if pairs.is_empty() {
            return None;
        }
        let sources: Vec<String> = pairs
            .iter()
            .map(|(from, _)| (*from).to_string_lossy())
            .collect();
        let names: Vec<String> = pairs.iter().map(|(_, to)| file_name(to)).collect();
        let name = if sources.len() == 1 && names.len() == 1 {
            names[0].clone()
        } else {
            serde_json::to_string(&names).unwrap_or_default()
        };
        Some(ActionEntry::new(ActionType::RENAME, sources, "", name))
    }
}

/// Validate one proposed name the way a destination name must be (plan §5.3's `validate_name`): not empty,
/// not `.`/`..`, no path separator, no glob metacharacters, and — on Windows — not a reserved device name
/// or an alternate-data-stream colon. [`crate::path::validate_single_name`]'s NUL check and
/// [`crate::path::rejects_windows_reserved`] are the same rules the path layer already enforces, so there
/// is one implementation of "is this a name" in the crate.
///
/// Note what is *not* rejected: `foo..bar` (plan ledger #14, Go's substring `validateDest` false positive).
///
/// # Errors
/// [`CoreError::InvalidPath`] naming the reason.
pub fn validate_rename_name(name: &str) -> Result<(), CoreError> {
    validate_dest_name(name)
}

/// Rename a batch. Nothing is renamed when the request itself is malformed: an empty selection is
/// [`CoreError::Empty`] (`api/ops.go:392`'s `"Invalid rename arguments"`), and a batch whose name count
/// differs from its source count is [`CoreError::InvalidPath`]
/// (`api/ops.go:441-444`'s `"Mismatch between sources count and new names count"`).
///
/// Per-source failures are in the report, not the `Err`.
///
/// # Errors
/// As above, plus [`CoreError::Cancelled`].
pub fn rename(
    fs: &dyn MoveFs,
    req: &RenameReq,
    cancel: &CancelToken,
    progress: &dyn ProgressSink,
) -> Result<RenameReport, CoreError> {
    if req.sources.is_empty() {
        return Err(CoreError::Empty { what: "selection" });
    }
    let names: Vec<String> = match &req.target {
        RenameTarget::One(name) if req.sources.len() > 1 => {
            // A single name for several sources is a batch of the same name — which cannot work (every
            // item would land on one path), so it is refused rather than silently applied to the first.
            let _ = name;
            return Err(CoreError::Empty {
                what: "a new name for each source",
            });
        }
        RenameTarget::One(name) => vec![name.clone()],
        RenameTarget::Batch(names) if names.len() != req.sources.len() => {
            return Err(CoreError::Empty {
                what: "rename names (the count must match the selection)",
            });
        }
        RenameTarget::Batch(names) => names.clone(),
    };

    // The working copy Go mutated in place (`api/ops.go:466-474`): a renamed parent rewrites the paths of
    // the sources that live under it, so their own rename targets the path they now have.
    let mut current: Vec<AbsPath> = req.sources.clone();
    let mut report = RenameReport::default();
    for i in 0..current.len() {
        cancel.check()?;
        let source = current[i].clone();
        let name = &names[i];
        match rename_one(fs, req.policy, &source, name, cancel) {
            Ok(Some(to)) => {
                let prefix_source = join_source(&source);
                let prefix_target = join_source(&to);
                report.outcomes.push(RenameOutcome::Renamed {
                    from: source.clone(),
                    to: to.clone(),
                });
                for later in current.iter_mut().skip(i + 1) {
                    let text = later.to_string_lossy();
                    if let Some(tail) = text.strip_prefix(&prefix_source)
                        && let Ok(rewritten) = AbsPath::from_path(format!("{prefix_target}{tail}"))
                    {
                        *later = rewritten;
                    }
                }
            }
            Ok(None) => report.outcomes.push(RenameOutcome::Failed {
                path: source.clone(),
                error: Arc::new(CoreError::Empty { what: "new name" }),
            }),
            Err(error) => report.outcomes.push(RenameOutcome::Failed {
                path: source.clone(),
                error: Arc::new(error),
            }),
        }
        progress.advance(0, 1, name);
    }
    Ok(report)
}

/// One item's rename. `Ok(None)` is Go's "empty name, skip it" (`api/ops.go:448-450`);
/// `Ok(Some(target))` moved it.
fn rename_one(
    fs: &dyn MoveFs,
    policy: ConflictPolicy,
    source: &AbsPath,
    name: &str,
    cancel: &CancelToken,
) -> Result<Option<AbsPath>, CoreError> {
    if name.is_empty() {
        return Ok(None);
    }
    validate_rename_name(name)?;
    let dir = source.parent().ok_or_else(|| CoreError::InvalidPath {
        raw: source.to_string_lossy(),
        reason: crate::error::PathReject::Empty,
    })?;
    let target = dir.join(name)?;
    if target == *source {
        // Go's `src != dst` guard (`api/ops.go:398`): a rename to itself is a successful no-op, and must
        // not appear in the log record (or a revert would try to undo nothing).
        return Ok(None);
    }
    let is_dir = std::fs::symlink_metadata(source.as_path()).is_ok_and(|m| m.is_dir());
    match crate::ops::ConflictPolicy::decide(policy, is_dir, &target)? {
        crate::ops::ConflictDecision::Skip => Err(CoreError::AlreadyExists {
            path: target.to_string_lossy(),
        }),
        crate::ops::ConflictDecision::Rename(new) => {
            apply(fs, source, &new, cancel).map(|()| Some(new))
        }
        _ => apply(fs, source, &target, cancel).map(|()| Some(target)),
    }
}

/// Land the rename through the same primitive the paste uses, so a rename that happens to cross a device
/// boundary gets `move_item`'s copy+delete-with-rollback instead of a bare `EXDEV` error.
fn apply(
    fs: &dyn MoveFs,
    source: &AbsPath,
    target: &AbsPath,
    cancel: &CancelToken,
) -> Result<(), CoreError> {
    let stats = crate::ops::r#move::move_item(
        source,
        target,
        ConflictPolicy::Overwrite,
        fs,
        cancel,
        &crate::ops::NoProgress,
    )?;
    if stats.renamed || stats.dest.is_some() {
        return Ok(());
    }
    Err(CoreError::AlreadyExists {
        path: target.to_string_lossy(),
    })
}

impl std::cmp::PartialEq for RenameReport {
    fn eq(&self, other: &Self) -> bool {
        self.same_as(other)
    }
}

fn file_name(path: &AbsPath) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `path` plus the separator, for the prefix rewrite (Go's `src + string(filepath.Separator)`).
fn join_source(path: &AbsPath) -> String {
    format!("{}/", path.to_string_lossy().trim_end_matches('/'))
}
