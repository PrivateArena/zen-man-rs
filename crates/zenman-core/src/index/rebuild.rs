//! The staged rebuild pipeline (plan §5.5 `index/rebuild.rs` row, a port of `runRebuild`,
//! `api/search_index_walker.go:74-286`).
//!
//! # The stage list, in Go's real order
//!
//! 1. [`Stage::PrepareShadow`] — drop leftovers, create `files_shadow` + `files_fts_shadow`
//!    (Go `:82-124`). **No unique index yet**: Go creates the indexes only after the walk (`:227-234`),
//!    which is why bulk insert is fast and why duplicate paths from a racing walker collapse via the
//!    writer's `INSERT OR REPLACE` instead of failing.
//! 2. [`Stage::Walk`] — [`crate::index::walker::walk`] feeds the batched `INSERT OR REPLACE` writer
//!    (Go's channel + single writer goroutine, `:160-205` + `:390-459`).
//! 3. [`Stage::IndexShadow`] — the five `idx_files_shadow_*` indexes on `files_shadow`.
//! 4. [`Stage::PopulateFts`] — [`schema::FTS_POPULATE_SQL`], one statement, shadow rowids copied.
//! 5. [`Stage::Swap`] — one transaction: drop live, rename shadow (Go's "atomic table swap", `:251-281`),
//!    then the `wal_checkpoint(PASSIVE)` that Go runs and that a `DELETE`-mode database ignores.
//!
//! Cancellation is checked at **every stage boundary** (the hook fires *before* the check, so a test can
//! cancel at a named stage deterministically) and inside the walk per entry. Any early exit drops both
//! shadow tables — Go's `db.Exec("DROP TABLE IF EXISTS files_shadow;")` cleanup on every error path — so
//! the live `files`/`files_fts` answer queries throughout and the next rebuild starts clean.
//!
//! # The connection
//!
//! [`run`] takes a connection it **owns the write lock on**: `Session::index_rebuild` opens one through
//! `store::sql::connect_with_retry` on the job thread rather than borrowing `Session::with_db`, because
//! `with_db` holds the session's connection mutex for the whole closure and a rebuild is long — a
//! rebuild inside `with_db` would freeze every queued intent drain and every read for its whole duration.

use std::time::{Duration, Instant};

use rusqlite::Connection;

use crate::error::CoreError;
use crate::index::schema;
use crate::index::walker::{self, IndexedRow, WalkCfg};
use crate::jobs::CancelToken;

/// One pipeline stage, in Go's execution order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Drop + create the shadow tables.
    PrepareShadow,
    /// Walk the roots, insert batches into `files_shadow`.
    Walk,
    /// Create the five indexes on `files_shadow`.
    IndexShadow,
    /// Fill `files_fts_shadow` from `files_shadow`.
    PopulateFts,
    /// The rename swap in one transaction, then the checkpoint.
    Swap,
}

impl Stage {
    /// The stable name (job progress text, tests).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PrepareShadow => "prepare_shadow",
            Self::Walk => "walk",
            Self::IndexShadow => "index_shadow",
            Self::PopulateFts => "populate_fts",
            Self::Swap => "swap",
        }
    }
}

/// Where the pipeline is and how far it has got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RebuildProgress {
    /// The stage about to run.
    pub stage: Stage,
    /// Rows written to `files_shadow` so far.
    pub rows: u64,
}

/// What a finished rebuild did (plan F6: per-root failure text survives, Go's single string does not).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RebuildSummary {
    /// Rows written (Go's `ProgressCount`).
    pub rows: u64,
    /// Roots that walked cleanly.
    pub roots_ok: usize,
    /// `(root, reason)` for every root that failed.
    pub roots_failed: Vec<(String, String)>,
    /// Roots that ran out of their per-root deadline.
    pub timed_out_roots: Vec<String>,
    /// Wall-clock length of the pipeline.
    pub elapsed: Duration,
}

/// Run the full pipeline on a connection the caller dedicated to it.
///
/// # Errors
/// [`CoreError::Cancelled`] when `cancel` fires, [`CoreError::Sql`] for any SQLite or sink failure;
/// the shadow tables are dropped on every error path.
pub fn run(
    conn: &Connection,
    cfg: &WalkCfg,
    cancel: &CancelToken,
) -> Result<RebuildSummary, CoreError> {
    run_with_hook(conn, cfg, cancel, &|_| {})
}

/// [`run`] with the per-stage observer. The hook runs at each stage boundary, *before* the cancel
/// check, so a test that cancels the token inside the hook stops the pipeline exactly at that stage —
/// which is what `contract_search.rs`'s `rebuild_is_atomic` uses to stop between `PopulateFts` and
/// `Swap`.
///
/// # Errors
/// As [`run`].
pub fn run_with_hook(
    conn: &Connection,
    cfg: &WalkCfg,
    cancel: &CancelToken,
    hook: &dyn Fn(&RebuildProgress),
) -> Result<RebuildSummary, CoreError> {
    let started = Instant::now();
    match build(conn, cfg, cancel, hook) {
        Ok(mut summary) => {
            // Go runs the checkpoint best-effort after the swap (`:283`); a `DELETE`-mode database
            // answers it as a no-op, and a failure there must not undo a committed swap.
            let _ = conn.execute_batch(schema::CHECKPOINT_SQL);
            summary.elapsed = started.elapsed();
            Ok(summary)
        }
        Err(e) => {
            cleanup(conn);
            Err(e)
        }
    }
}

/// Drop both shadow tables, ignoring failures (Go's cleanup uses bare `db.Exec` for the same reason:
/// the cleanup must not mask the error that caused it).
fn cleanup(conn: &Connection) {
    let _ = conn.execute_batch(schema::SHADOW_DROP_SQL);
    let _ = conn.execute_batch(schema::SHADOW_FTS_DROP_SQL);
}

fn stage(
    hook: &dyn Fn(&RebuildProgress),
    stage: Stage,
    rows: u64,
    cancel: &CancelToken,
) -> Result<(), CoreError> {
    hook(&RebuildProgress { stage, rows });
    cancel.check()
}

fn build(
    conn: &Connection,
    cfg: &WalkCfg,
    cancel: &CancelToken,
    hook: &dyn Fn(&RebuildProgress),
) -> Result<RebuildSummary, CoreError> {
    // 1. Prepare shadow.
    stage(hook, Stage::PrepareShadow, 0, cancel)?;
    conn.execute_batch(schema::SHADOW_DROP_SQL)
        .map_err(|e| CoreError::Sql { msg: e.to_string() })?;
    conn.execute_batch(schema::DDL_SHADOW_TABLE)
        .map_err(|e| CoreError::Sql { msg: e.to_string() })?;
    conn.execute_batch(schema::SHADOW_FTS_DROP_SQL)
        .map_err(|e| CoreError::Sql { msg: e.to_string() })?;
    conn.execute_batch(schema::DDL_SHADOW_FTS)
        .map_err(|e| CoreError::Sql { msg: e.to_string() })?;

    // 2. Walk + write. The sink cannot return, so the first insert error is parked and raised after.
    stage(hook, Stage::Walk, 0, cancel)?;
    let mut written: u64 = 0;
    let mut sink_error: Option<CoreError> = None;
    let stats = walker::walk(cfg, cancel, &mut |batch: &[IndexedRow]| {
        if sink_error.is_some() {
            return;
        }
        match insert_batch(conn, batch) {
            Ok(n) => written += n,
            Err(e) => sink_error = Some(e),
        }
    })?;
    if let Some(e) = sink_error {
        return Err(e);
    }

    // 3. Index the shadow table (Go creates these *after* the walk, `:226-241`). The same-named
    // indexes are dropped off the live table first — see `schema::SHADOW_INDEX_NAMES`.
    stage(hook, Stage::IndexShadow, written, cancel)?;
    for name in schema::SHADOW_INDEX_NAMES {
        conn.execute_batch(&format!("DROP INDEX IF EXISTS {name};"))
            .map_err(|e| CoreError::Sql {
                msg: format!("drop colliding index {name}: {e}"),
            })?;
    }
    for idx in schema::DDL_SHADOW_INDEXES {
        conn.execute_batch(idx).map_err(|e| CoreError::Sql {
            msg: format!("shadow index: {e}"),
        })?;
    }

    // 4. FTS.
    stage(hook, Stage::PopulateFts, written, cancel)?;
    conn.execute_batch(schema::FTS_POPULATE_SQL)
        .map_err(|e| CoreError::Sql {
            msg: format!("fts populate: {e}"),
        })?;

    // 5. The swap, in one transaction (Go's `tx` with the deferred rollback, `:251-281`).
    stage(hook, Stage::Swap, written, cancel)?;
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| CoreError::Sql { msg: e.to_string() })?;
    for stmt in schema::SWAP_SQL.split(';').filter(|s| !s.trim().is_empty()) {
        if let Err(e) = tx.execute_batch(stmt) {
            drop(tx);
            return Err(CoreError::Sql {
                msg: format!("swap: {e}"),
            });
        }
    }
    tx.commit().map_err(|e| CoreError::Sql {
        msg: format!("swap commit: {e}"),
    })?;

    Ok(RebuildSummary {
        rows: written,
        roots_ok: stats.roots_ok,
        roots_failed: stats
            .roots_failed
            .iter()
            .map(|(r, why)| (r.to_string_lossy(), why.clone()))
            .collect(),
        timed_out_roots: stats
            .timed_out_roots
            .iter()
            .map(|r| r.to_string_lossy())
            .collect(),
        elapsed: Duration::ZERO,
    })
}

/// One 1 000-row transaction, Go's writer (`:390-441`). `INSERT OR REPLACE` is what makes a duplicate
/// path (two roots overlapping, a racing create) collapse instead of failing.
fn insert_batch(conn: &Connection, batch: &[IndexedRow]) -> Result<u64, CoreError> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| CoreError::Sql { msg: e.to_string() })?;
    let mut n: u64 = 0;
    {
        let mut stmt = tx
            .prepare(
                "INSERT OR REPLACE INTO files_shadow \
                 (path, name, name_orig, ext, parent, size, mtime, is_dir, is_symlink) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9);",
            )
            .map_err(|e| CoreError::Sql { msg: e.to_string() })?;
        for row in batch {
            stmt.execute(rusqlite::params![
                row.path,
                row.name,
                row.name_orig,
                row.ext,
                row.parent,
                row.size,
                row.mtime,
                i64::from(row.is_dir),
                i64::from(row.is_symlink),
            ])
            .map_err(|e| CoreError::Sql { msg: e.to_string() })?;
            n += 1;
        }
    }
    tx.commit()
        .map_err(|e| CoreError::Sql { msg: e.to_string() })?;
    Ok(n)
}
