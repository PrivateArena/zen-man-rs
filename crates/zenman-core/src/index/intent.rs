//! The index-intent seam (plan §5.5 `index/updater.rs`'s `IndexOp`, re-homed in `index/intent.rs` for
//! plan §7 step 6, and §5.3's `ops/rename.rs` row).
//!
//! # Why a trait and not a direct call
//!
//! The rename/delete/move code in `ops/*` must be able to tell the index "this path moved" without any of
//! those services owning a database connection. That is plan §5.3's `MoveIntent` constraint, stated there
//! as part of the rename row: **a service that mutates the filesystem must not hold a `Connection`**.
//! Holding one is how a long-running paste ends up serialised against a rebuild, how an `EXDEV`
//! copy-then-delete can leave the database locked at the moment the process is killed, and how a
//! `Session` would end up with two owners of the single connection Go's `SetMaxOpenConns(1)` exists to
//! protect (plan §5.4 `store/sql.rs`).
//!
//! So `ops/*` and `services/*` get an [`IndexIntent`], whose only method is non-blocking and whose only
//! step-6 implementation is [`NoopIntent`]. Step 12 swaps in the queueing implementation — the 2 s
//! coalescing drain of plan §5.5's updater row — with no change to any caller, which is the whole point
//! of the seam.
//!
//! # Contract of an implementation
//!
//! * `note` **returns immediately**: no I/O, no lock held across a filesystem call, no wait for a
//!   rebuild. A blocking implementation is a bug in the implementation, not a caller problem — the
//!   callers include the paste job's item loop.
//! * `note` is called *after* the filesystem change succeeded (plan F3's "log append after the walk"
//!   rule, generalised): the index is told about the world as it now is.
//! * Ordering between two `note`s from the same job is preserved; ordering across jobs is not, and
//!   implementations must not assume it (two pastes into one directory are independent).
//! * Losing an op must be recoverable by a rebuild, so ops are idempotent by construction: the enum
//!   names paths, never row ids.

use crate::path::AbsPath;

/// What happened to a path, in the shapes step 12's writer needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexOp {
    /// A new entry appeared at `path` (mkdir, a paste in, an extract).
    Created {
        /// The new path.
        path: AbsPath,
    },
    /// `path` is gone (a delete, an overwrite).
    Deleted {
        /// The removed path.
        path: AbsPath,
    },
    /// `from` was renamed to `to` in place: same directory, new name (`api/ops.go` rename).
    Renamed {
        /// Before.
        from: AbsPath,
        /// After.
        to: AbsPath,
    },
    /// `from` moved out of its directory into `to_dir` with the name it had (paste-move, `move_item`).
    ///
    /// Distinct from [`Self::Renamed`] because the index's `parent` column changes for every row under
    /// a moved directory, which is the difference between one `UPDATE` and a subtree rewrite.
    Moved {
        /// Before.
        from: AbsPath,
        /// After (the full destination path, name included).
        to: AbsPath,
    },
    /// Everything under `path` must be dropped from the index — the parent directory was removed, or a
    /// subtree moved away.
    Pruned {
        /// The subtree root.
        path: AbsPath,
    },
    /// The entry at `path` changed size or mtime, so its row's numbers are stale (chmod is *not* one of
    /// these: the index stores no mode, and `docs/contracts.md`'s schema has no column for it).
    Refreshed {
        /// The touched path.
        path: AbsPath,
    },
    /// The entry's **attributes** (mode, and nothing else) changed — `ops::chmod`.
    ///
    /// Distinct from [`Self::Refreshed`] for exactly the reason that comment gives: the index has no mode
    /// column, so a writer has nothing to update. The op is still emitted, because plan §5.3's rule is that
    /// every mutation reports itself through this seam — and a step-12 implementation that *does* cache
    /// modes (or that invalidates a row's `is_executable` filter) has one place to learn about it.
    AttributesChanged {
        /// The re-moded path.
        path: AbsPath,
    },
}

impl IndexOp {
    /// The path this op is about, i.e. the one to look up when coalescing.
    #[must_use]
    pub fn subject(&self) -> &AbsPath {
        match self {
            Self::Created { path }
            | Self::Deleted { path }
            | Self::Pruned { path }
            | Self::Refreshed { path }
            | Self::AttributesChanged { path } => path,
            Self::Renamed { to, .. } | Self::Moved { to, .. } => to,
        }
    }

    /// Coalesce two ops on the same subject into the one that must survive: plan §5.5's "cheap coalescing,
    /// not a journal". `create` then `delete` is a delete, and `rename` then `delete` is a delete.
    #[must_use]
    pub fn merged(earlier: &IndexOp, later: &IndexOp) -> Option<IndexOp> {
        match (earlier, later) {
            // A path that was added and then removed never existed as far as a row-by-row writer is
            // concerned, but the delete still has to be attempted: a stale row from before the create
            // may exist.
            (IndexOp::Created { .. }, IndexOp::Deleted { path })
            | (IndexOp::Renamed { .. }, IndexOp::Deleted { path })
            | (IndexOp::Moved { .. }, IndexOp::Deleted { path }) => {
                Some(IndexOp::Deleted { path: path.clone() })
            }
            // Anything followed by a prune is a prune: the subtree drop covers every earlier claim.
            (_, IndexOp::Pruned { path }) => Some(IndexOp::Pruned { path: path.clone() }),
            // Identical ops collapse.
            (a, b) if a == b => Some(a.clone()),
            _ => None,
        }
    }
}

/// The seam `ops/*` and `services/*` hold instead of a database connection.
pub trait IndexIntent: Send + Sync {
    /// Record one filesystem change. Non-blocking by contract (see the module docs).
    fn note(&self, op: IndexOp);

    /// Record several at once, so a batch rename makes one call. The default loops, which is exactly
    /// as correct — the point of the hook is that an implementation may do better (one transaction).
    fn note_all(&self, ops: &[IndexOp]) {
        for op in ops {
            self.note(op.clone());
        }
    }
}

/// The step-6 implementation: it drops every op.
///
/// Used by every service until step 12's queue lands, because the alternative — having `ops/*` write the
/// index directly — is the coupling this module exists to avoid, and a dropped op is recoverable by a
/// rebuild (which is what the indexer's own "dirty" state is for).
#[derive(Debug, Default)]
pub struct NoopIntent {
    /// How many ops were dropped, which is the one thing a test needs to see.
    noted: std::sync::atomic::AtomicU64,
}

impl NoopIntent {
    /// A fresh sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ops that have passed through and been dropped.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.noted.load(std::sync::atomic::Ordering::Acquire)
    }
}

impl IndexIntent for NoopIntent {
    fn note(&self, _op: IndexOp) {
        let _ = self.noted.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    fn note_all(&self, ops: &[IndexOp]) {
        let _ = self
            .noted
            .fetch_add(ops.len() as u64, std::sync::atomic::Ordering::AcqRel);
    }
}

/// A shared, boxed intent sink of the kind a service field holds.
pub type SharedIntent = std::sync::Arc<dyn IndexIntent>;

/// The step-6 shared sink.
#[must_use]
pub fn noop_intent() -> SharedIntent {
    std::sync::Arc::new(NoopIntent::new())
}

/// An [`IndexIntent`] that keeps every op it was handed — [`NoopIntent`]'s test twin.
///
/// `NoopIntent` can only prove that a call *happened* (its counter); this proves *what* was reported,
/// which is the assertion every step-9c mutation test needs: that a delete of a directory is a
/// [`IndexOp::Pruned`] and not a [`IndexOp::Deleted`], that a rename reports `from`/`to` in the right
/// order. It is also what a step-12 caller installs through
/// [`Session::set_intent`](crate::session::Session::set_intent) to watch a live session.
#[derive(Debug, Default)]
pub struct RecordingIntent {
    ops: std::sync::Mutex<Vec<IndexOp>>,
}

impl RecordingIntent {
    /// A fresh recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything noted so far, in call order.
    #[must_use]
    pub fn recorded(&self) -> Vec<IndexOp> {
        self.ops
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// How many ops have arrived.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ops.lock().map(|g| g.len()).unwrap_or_default()
    }

    /// `true` when nothing has arrived — the assertion a "this op must not touch the index" test makes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl IndexIntent for RecordingIntent {
    fn note(&self, op: IndexOp) {
        self.ops
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(op);
    }

    fn note_all(&self, ops: &[IndexOp]) {
        let mut guard = self
            .ops
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.extend(ops.iter().cloned());
    }
}

/// The shared recorder — the pair of [`Session::set_intent`](crate::session::Session::set_intent) in a
/// test.
#[must_use]
pub fn recording_intent() -> SharedIntent {
    std::sync::Arc::new(RecordingIntent::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    fn abs(p: &str) -> AbsPath {
        AbsPath::from_path(p).expect("absolute")
    }

    #[test]
    fn noop_intent_accepts_every_op_shape_and_counts_them() {
        let noop = NoopIntent::new();
        let concrete = Arc::new(NoopIntent::new());
        let shared: SharedIntent = Arc::clone(&concrete) as SharedIntent;
        let ops = [
            IndexOp::Created { path: abs("/a/n") },
            IndexOp::Deleted { path: abs("/a/n") },
            IndexOp::Renamed {
                from: abs("/a/n"),
                to: abs("/a/m"),
            },
            IndexOp::Moved {
                from: abs("/a/n"),
                to: abs("/b/n"),
            },
            IndexOp::Pruned { path: abs("/a") },
            IndexOp::Refreshed { path: abs("/a/n") },
        ];
        for op in &ops {
            noop.note(op.clone());
        }
        assert_eq!(noop.dropped(), ops.len() as u64);
        // One call for the batch, and the counter proves it: `note_all` is overridden precisely so a
        // queueing implementation can coalesce in one transaction.
        shared.note_all(&ops);
        assert_eq!(concrete.dropped(), ops.len() as u64);
        // And the trait object is usable as the only thing a service holds.
        shared.note(IndexOp::Refreshed { path: abs("/a/n") });
        assert_eq!(concrete.dropped(), ops.len() as u64 + 1);
    }

    #[test]
    fn subjects_and_merging_follow_the_updater_rules() {
        let created = IndexOp::Created { path: abs("/a/n") };
        let deleted = IndexOp::Deleted { path: abs("/a/n") };
        assert_eq!(
            IndexOp::merged(&created, &deleted),
            Some(IndexOp::Deleted { path: abs("/a/n") })
        );
        let pruned = IndexOp::Pruned { path: abs("/a") };
        assert_eq!(IndexOp::merged(&created, &pruned), Some(pruned.clone()));
        assert_eq!(
            IndexOp::merged(&pruned, &created),
            None,
            "prune then create is both"
        );
        // A rename's subject is the *new* path: that is the row the reader will look for.
        assert_eq!(
            IndexOp::Renamed {
                from: abs("/a/n"),
                to: abs("/a/m"),
            }
            .subject()
            .to_string_lossy(),
            "/a/m"
        );
        assert!(
            IndexOp::merged(&deleted, &deleted).is_some(),
            "identical ops collapse"
        );
    }
}
