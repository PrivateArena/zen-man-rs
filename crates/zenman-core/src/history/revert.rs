//! The filesystem half of `/api/log/revert` (plan §5.3/§5.6, Go `api/log_revert.go` + `HandleLogRevert`
//! `api/log.go:662-718`).
//!
//! A revert is not a second implementation of the file work: every inverse here runs through the same
//! [`crate::ops::r#move`] / [`crate::ops::delete`] primitives the write path uses, so a revert of a move is
//! a move, and a revert of a copy is a permanent delete. That is the difference between this module and a
//! hand-rolled undo: the collision rules, the `EXDEV` fallback and the typed errors cannot drift from the
//! forward operation's, because they are the same code.
//!
//! # What the log says a revert does
//!
//! The frozen capture is the authority on the log side, and it says something the plan prose does not: a
//! revert **appends nothing**. Go's `MarkReverted` (`api/log.go:708`) patches the original frame in place —
//! `status = 1`, CRC recomputed — and the 19-record `tests/golden/action.log` still holds exactly 19 records
//! with one of them flagged (contract_history's `our_crc_reseal_reproduces_the_revert_the_capture_performed`,
//! step 5's byte-parity test). So [`crate::Session::revert`] calls
//! [`ActionLogStore::mark_reverted`](crate::history::store::ActionLogStore::mark_reverted) and never
//! `append`, and the response shape it reproduces is `tests/golden/log-revert-mkdir.json`:
//! `{"status":"reverted","record_id":1,"action":"mkdir"}` — carried here by
//! [`RevertReport::to_json`].
//!
//! # The five types, and the four that refuse
//!
//! | `action` | reverted by | Go |
//! |---|---|---|
//! | `paste-copy` | remove the copies at `dest/base(src)` | `revertPasteCopy` — `os.RemoveAll`, idempotent when already gone |
//! | `paste-move` | move `dest/base(src)` back to `src`, recreating `dir(src)` | `revertPasteMove` — `MkdirAll` + `moveItem`, **not** idempotent |
//! | `rename` | rename the new name back, batch in **reverse order** | `revertRename` |
//! | `mkdir` | remove `dest/name`, but only while it is empty | `revertMkdir` |
//! | `paste-link` | unlink `dest/base(src)` — the *link*, never its target | `revertPasteLink` — `os.Remove`, not `RemoveAll` |
//! | `delete` | **refused**: "delete is permanent on SSD — recovery requires a backup" | `RevertRecord` |
//! | `copy` / `chmod` | **refused**: no filesystem change of its own | `RevertRecord`'s `default` |
//!
//! The refusals are the [`ActionType::is_reversible`] set (`api/log.go:600-602`) — derived from the action
//! byte, never stored — so a caller can check before it asks.
//!
//! # Idempotence is Go's, per type
//!
//! Go's copy/link/mkdir reverts treat "already gone" as success and its move revert treats it as an error
//! (`log_revert.go:88`, "no longer exists at dest"). Both are kept: the difference is that a copy revert
//! cannot tell a removed copy from a copy that was never made, while a move revert *can* — and silently
//! succeeding there would leave the source missing and the record marked undone.

use std::sync::Arc;

use crate::error::{CoreError, PathReject};
use crate::history::codec::{ActionRecord, ActionType};
use crate::history::store::LogId;
use crate::jobs::CancelToken;
use crate::ops::delete::remove_permanently;
use crate::ops::r#move::{MoveFs, move_item};
use crate::ops::{ConflictPolicy, ProgressSink};
use crate::path::AbsPath;
use crate::platform::Env;

/// What a revert did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevertReport {
    /// The record that was undone.
    pub id: LogId,
    /// Its action — the `action` field of Go's revert response.
    pub action: ActionType,
    /// Pairs `(where the path is now, where it went back to)` for the inverses that *relocate*
    /// (`paste-move`, `rename`).
    pub restored: Vec<(AbsPath, AbsPath)>,
    /// Paths this revert removed (`paste-copy`, `mkdir`, `paste-link`).
    pub removed: Vec<AbsPath>,
    /// `true` once the record carries `status = reverted` in the log. Go only warned to stderr when this
    /// failed after the filesystem had already been reverted (`api/log.go:708-711`); here it is a field,
    /// so the caller can say the two halves apart.
    pub marked: bool,
}

impl RevertReport {
    /// The `POST /api/log/revert` body Go returned (`api/log.go:713-717`), reproduced field for field —
    /// `tests/golden/log-revert-mkdir.json`.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "status": "reverted",
            "record_id": self.id,
            "action": self.action.name(),
        })
    }

    /// The jobs-pane / toast line.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "reverted {}: {} restored, {} removed",
            self.action,
            self.restored.len(),
            self.removed.len()
        )
    }

    /// Every directory the revert changed: the parents of what moved, what was removed and what arrived.
    #[must_use]
    pub fn affected_parents(&self) -> Vec<AbsPath> {
        let mut parents: Vec<AbsPath> = Vec::new();
        let mut note = |path: &AbsPath| {
            if let Some(parent) = path.parent()
                && !parents.contains(&parent)
            {
                parents.push(parent);
            }
        };
        for (from, to) in &self.restored {
            note(from);
            note(to);
        }
        for path in &self.removed {
            note(path);
        }
        parents
    }
}

/// Refuse a record that cannot be reverted (the four types outside
/// [`ActionType::is_reversible`], and one that is already reverted).
///
/// Go's two refusals were HTTP 422 `"action '%s' cannot be reverted"` and 409 `"already reverted"`
/// (`api/log.go:693-701`); both are [`CoreError::Unsupported`] here (kind `unsupported`, 501) because
/// `CoreError` has no 422-shaped "refused by policy" variant and inventing one for a message the UI already
/// renders would be a wider change than this step owns. The two texts are distinct, so the UI can tell them
/// apart by `kind()`/message.
///
/// # Errors
/// [`CoreError::Unsupported`] naming why, before any filesystem call.
pub fn ensure_reversible(record: &ActionRecord) -> Result<(), CoreError> {
    if record.status == crate::history::codec::LogStatus::REVERTED {
        return Err(CoreError::Unsupported {
            what: "this record has already been reverted",
        });
    }
    if !record.is_reversible() {
        return Err(if record.action == ActionType::DELETE {
            CoreError::Unsupported {
                what: "delete is permanent on SSD — recovery requires a backup",
            }
        } else {
            CoreError::Unsupported {
                what: "this action produces no filesystem change to revert",
            }
        });
    }
    Ok(())
}

/// Undo one record. The caller ([`crate::Session::revert`]) has already checked
/// [`ensure_reversible`]; this checks it again, because the function is public and the answer must not
/// depend on who asked.
///
/// The inverse is applied **in the order Go used** — and for a batch rename, in reverse, which is what makes
/// a parent/child batch come back correctly (`api/log_revert.go:120`, "revert in reverse order to handle
/// dependencies cleanly").
///
/// # Errors
/// [`CoreError::Unsupported`] for a non-reversible type, [`CoreError::InvalidPath`] for a record whose paths
/// no longer parse (a log written on another machine — the capture's `${FIXTURE_ROOT}` case),
/// [`CoreError::NotFound`]/[`CoreError::Io`] for the state the inverse needs and does not find, and
/// [`CoreError::Cancelled`]. Go failed fast on the first bad item; so does this, and for Go's reason
/// ("to avoid partial reverts", `log_revert.go:52`).
pub fn revert(
    env: &dyn Env,
    fs: &dyn MoveFs,
    record: &ActionRecord,
    cancel: &CancelToken,
    progress: &dyn ProgressSink,
) -> Result<RevertReport, CoreError> {
    ensure_reversible(record)?;
    let dest = if record.dest.is_empty() {
        None
    } else {
        Some(abs(&record.dest)?)
    };
    let sources = paths(&record.sources)?;
    let mut report = RevertReport {
        id: record.id,
        action: record.action,
        restored: Vec::new(),
        removed: Vec::new(),
        marked: false,
    };
    match record.action {
        ActionType::PASTE_COPY | ActionType::PASTE_LINK => {
            let dest = dest.ok_or(CoreError::Unsupported {
                what: "this paste record carries no destination directory",
            })?;
            for src in &sources {
                cancel.check()?;
                let target = placed_at(&dest, src)?;
                if !exists(&target) {
                    continue; // Go's idempotent branch (`log_revert.go:36,66`).
                }
                if record.action == ActionType::PASTE_LINK {
                    // Unlink the link alone: `os.Remove`, never a recursive delete, or reverting a symlink
                    // would delete the file it points at (`log_revert.go:39-41`).
                    std::fs::remove_file(target.as_path()).map_err(|io| CoreError::Io {
                        path: target.to_string_lossy(),
                        io: Arc::new(io),
                    })?;
                } else {
                    remove_permanently(&target)?;
                }
                let _ = env;
                report.removed.push(target);
                progress.advance(0, 1, "");
            }
        }
        ActionType::PASTE_MOVE => {
            let dest = dest.ok_or(CoreError::Unsupported {
                what: "this move record carries no destination directory",
            })?;
            for src in &sources {
                cancel.check()?;
                let placed = placed_at(&dest, src)?;
                if !exists(&placed) {
                    return Err(CoreError::NotFound {
                        path: placed.to_string_lossy(),
                        source_path: Some("the moved copy, at the paste destination".to_string()),
                    });
                }
                // Go's `os.MkdirAll(srcDir, 0755)` (`log_revert.go:92`): the original folder may itself
                // have been deleted since the paste.
                if let Some(parent) = src.parent() {
                    std::fs::create_dir_all(parent.as_path()).map_err(|io| CoreError::Io {
                        path: parent.to_string_lossy(),
                        io: Arc::new(io),
                    })?;
                }
                let stats = move_item(
                    &placed,
                    src,
                    ConflictPolicy::SkipExisting,
                    fs,
                    cancel,
                    progress,
                )?;
                if stats.renamed || stats.dest.is_some() {
                    report.restored.push((placed, src.clone()));
                }
            }
        }
        ActionType::RENAME => {
            let names = rename_targets(record, sources.len())?;
            for (src, name) in sources.iter().zip(names.iter()).rev() {
                cancel.check()?;
                let dir = src.parent().ok_or_else(|| CoreError::InvalidPath {
                    raw: src.to_string_lossy(),
                    reason: PathReject::Empty,
                })?;
                let current = dir.join(name)?;
                if !exists(&current) {
                    // Go's batch branch skipped a missing name (`log_revert.go:126`); its single branch
                    // errored (`:140`). The batch behaviour is the safe one for both: a partial revert
                    // that stops at the first already-renamed-back item would refuse a retry of a revert
                    // that had half succeeded.
                    continue;
                }
                move_item(
                    &current,
                    src,
                    ConflictPolicy::SkipExisting,
                    fs,
                    cancel,
                    progress,
                )?;
                report.restored.push((current, src.clone()));
                progress.advance(0, 1, name);
            }
        }
        ActionType::MKDIR => {
            let (dest, name) = match (&dest, record.name.is_empty()) {
                (Some(dest), false) => (dest, record.name.as_str()),
                _ => {
                    return Err(CoreError::Unsupported {
                        what: "this mkdir record carries no destination and name",
                    });
                }
            };
            let target = dest.join(name)?;
            if !exists(&target) {
                return Ok(report); // already gone — Go's idempotent success (`log_revert.go:153`).
            }
            let entries = std::fs::read_dir(target.as_path()).map_err(|io| CoreError::Io {
                path: target.to_string_lossy(),
                io: Arc::new(io),
            })?;
            if entries.flatten().next().is_some() {
                // Go's data-loss guard (`log_revert.go:160-162`): the user put things in it since.
                return Err(CoreError::AlreadyExists {
                    path: target.to_string_lossy(),
                });
            }
            remove_permanently(&target)?;
            report.removed.push(target);
        }
        other => {
            // `ensure_reversible` admitted only the five above, so this is unreachable by construction —
            // but the set is a byte comparison, so name it rather than trusting it.
            let _ = other;
            return Err(CoreError::Unsupported {
                what: "this action produces no filesystem change to revert",
            });
        }
    }
    Ok(report)
}

// ------------------------------------------------------------------------------------------- internals

fn abs(raw: &str) -> Result<AbsPath, CoreError> {
    AbsPath::from_path(raw).map_err(|_| CoreError::InvalidPath {
        raw: raw.to_string(),
        reason: PathReject::Relative,
    })
}

fn paths(raw: &[String]) -> Result<Vec<AbsPath>, CoreError> {
    raw.iter().map(|p| abs(p)).collect()
}

fn exists(path: &AbsPath) -> bool {
    std::fs::symlink_metadata(path.as_path()).is_ok()
}

/// `dest/base(source)` — the path the forward operation wrote, rebuilt the way Go rebuilt it
/// (`filepath.Join(rec.Dest, filepath.Base(src))`).
fn placed_at(dest: &AbsPath, source: &AbsPath) -> Result<AbsPath, CoreError> {
    let name = source
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    dest.join(&name)
}

/// The `name` field decoded for a rename record: a JSON array for a batch (golden record 14,
/// `'["renamed-one.txt","renamed-two.txt"]'`), one plain name for a single rename (record 2).
///
/// The count rule is Go's (`log_revert.go:118`): the JSON form is only accepted when it has exactly one
/// entry per source, otherwise the record falls back to the single-name reading — which for a multi-source
/// record means "nothing usable", refused rather than guessed.
fn rename_targets(record: &ActionRecord, count: usize) -> Result<Vec<String>, CoreError> {
    if record.name.is_empty() {
        return Err(CoreError::Unsupported {
            what: "this rename record carries no new name",
        });
    }
    if count > 1 || (record.name.starts_with('[') && record.name.ends_with(']')) {
        if let Ok(parsed) = serde_json::from_str::<Vec<String>>(&record.name)
            && parsed.len() == count
        {
            return Ok(parsed);
        }
        if count > 1 {
            return Err(CoreError::Unsupported {
                what: "this rename record's name list does not match its source list",
            });
        }
    }
    Ok(vec![record.name.clone()])
}
