//! The incremental writer (plan §5.5 `index/updater.rs` row): [`IndexOp`]s → SQL, and the queueing
//! [`IndexIntent`] that step 12 installs behind [`Session::set_intent`](crate::session::Session::set_intent).
//!
//! # The op → SQL matrix (each row is `contract_search.rs::incremental_update`)
//!
//! | [`IndexOp`] | SQL | `files_fts` |
//! |---|---|---|
//! | `Created` / `Refreshed` | `INSERT … ON CONFLICT(path) DO UPDATE` from a fresh `lstat` (a failed `lstat` degrades to `Deleted`: the op reported a world that is already gone) | delete + insert the one rowid |
//! | `Deleted` | `DELETE FROM files WHERE path = ?` | delete the one rowid first |
//! | `Renamed` | `UPDATE files SET path,name,name_orig,ext,parent WHERE path = ?` | re-insert the rowid under the new name |
//! | `Moved` | root row updated, then the subtree: `path = to \|\| substr(path, len(from)+1) WHERE path LIKE 'from/%' ESCAPE '\'` (same for `parent`) | untouched when the name is preserved (rowids survive an `UPDATE`) |
//! | `Pruned` | `DELETE … WHERE path = ? OR path LIKE ? \|\| '/%' ESCAPE '\'` — the `ESCAPE` form is bounded by the `path`/`parent` indexes, unlike a `GLOB` | rowids gathered before the delete |
//! | `AttributesChanged` | **no statement** — the schema stores no mode (`index/intent.rs`), the op exists so a future mode cache has one place to hook | — |
//!
//! Every FTS touch is the rowid delete+insert pair: FTS5 external-content tables can be `UPDATE`d, but
//! `files_fts` is a *contentless-by-duplication* table like Go's, and delete+insert is the one sequence
//! that cannot leave a ghost token behind.
//!
//! # The queue
//!
//! [`QueuedIntent`] is the step-12 [`IndexIntent`]: `note` pushes onto a mutex (the seam's contract is
//! "returns immediately"), `Session::poll_at` drains it at the 2 s cadence plan §5.5 names, coalescing
//! adjacent ops on one subject through [`IndexOp::merged`] first. Losing the queue to a crash is fine —
//! that is what `is_dirty` and a rebuild are for, and it is why nothing here is a journal.

use std::sync::Mutex;

use rusqlite::Connection;

use crate::error::CoreError;
use crate::index::intent::{IndexIntent, IndexOp};
use crate::index::walker::IndexedRow;

/// Apply one op. Public so `Session` and the tests can drive it directly; the FTS table is kept in step
/// by every path.
///
/// # Errors
/// [`CoreError::Sql`] for any statement failure.
pub fn apply_op(conn: &Connection, op: &IndexOp) -> Result<(), CoreError> {
    match op {
        IndexOp::Created { path } | IndexOp::Refreshed { path } => {
            match std::fs::symlink_metadata(path.as_path()) {
                Ok(meta) => upsert(conn, path.as_path(), &meta),
                // The subject is gone again; the honest reconciliation of "created then vanished" is the
                // row drop, exactly like the walker dropping an entry whose `d.Info()` fails.
                Err(_) => delete_row(conn, &shown(path.as_path())),
            }
        }
        IndexOp::Deleted { path } => delete_row(conn, &shown(path.as_path())),
        IndexOp::Renamed { from, to } => {
            rename_row(conn, &shown(from.as_path()), &shown(to.as_path()))
        }
        IndexOp::Moved { from, to } => {
            move_subtree(conn, &shown(from.as_path()), &shown(to.as_path()))
        }
        IndexOp::Pruned { path } => prune(conn, &shown(path.as_path())),
        // No mode column exists (`index/intent.rs`); nothing to write, and inventing one is schema drift.
        IndexOp::AttributesChanged { .. } => Ok(()),
    }
}

/// Apply ops in order, coalescing adjacent same-subject pairs first (plan §5.5: "cheap coalescing, not a
/// journal"). Returns the number of ops actually executed.
///
/// # Errors
/// As [`apply_op`]; a failed op aborts the batch — the session marks the index dirty and the next
/// rebuild reconciles, which is the documented recovery path.
pub fn apply_ops(conn: &Connection, ops: &[IndexOp]) -> Result<usize, CoreError> {
    let mut applied = 0;
    for op in coalesce(ops) {
        apply_op(conn, &op)?;
        applied += 1;
    }
    Ok(applied)
}

/// [`IndexOp::merged`] over adjacent same-subject neighbours, keeping order otherwise.
#[must_use]
pub fn coalesce(ops: &[IndexOp]) -> Vec<IndexOp> {
    let mut out: Vec<IndexOp> = Vec::with_capacity(ops.len());
    for op in ops {
        match out.last() {
            Some(last) if last.subject() == op.subject() => {
                if let Some(merged) = IndexOp::merged(last, op) {
                    let _ = out.pop();
                    out.push(merged);
                    continue;
                }
                out.push(op.clone());
            }
            _ => out.push(op.clone()),
        }
    }
    out
}

fn shown(path: &std::path::Path) -> String {
    path.display().to_string()
}

/// SQLite's `LIKE` escaping, Go's `escapeLikePattern` (`api/search_index.go:394-399`): `\`, `%`, `_`.
#[must_use]
pub fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn sql_err(e: rusqlite::Error) -> CoreError {
    CoreError::Sql { msg: e.to_string() }
}

fn row_id(conn: &Connection, path: &str) -> Result<Option<i64>, CoreError> {
    match conn.query_row("SELECT id FROM files WHERE path = ?1;", [path], |r| {
        r.get::<_, i64>(0)
    }) {
        Ok(id) => Ok(Some(id)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(sql_err(e)),
    }
}

/// The rowid delete+insert pair that keeps `files_fts` in step for one `files.id`.
fn sync_fts(conn: &Connection, id: i64, name_lower: &str) -> Result<(), CoreError> {
    conn.execute("DELETE FROM files_fts WHERE rowid = ?1;", [id])
        .map_err(sql_err)?;
    conn.execute(
        "INSERT INTO files_fts (rowid, name) VALUES (?1, ?2);",
        rusqlite::params![id, name_lower],
    )
    .map_err(sql_err)?;
    Ok(())
}

fn upsert(
    conn: &Connection,
    path: &std::path::Path,
    meta: &std::fs::Metadata,
) -> Result<(), CoreError> {
    let row = IndexedRow::for_path(
        &crate::path::AbsPath::from_path(path.display().to_string()).map_err(|e| {
            CoreError::Sql {
                msg: e.user_message(),
            }
        })?,
        meta,
    );
    conn.execute(
        "INSERT INTO files (path, name, name_orig, ext, parent, size, mtime, is_dir, is_symlink)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(path) DO UPDATE SET
            name = excluded.name, name_orig = excluded.name_orig, ext = excluded.ext,
            parent = excluded.parent, size = excluded.size, mtime = excluded.mtime,
            is_dir = excluded.is_dir, is_symlink = excluded.is_symlink;",
        rusqlite::params![
            row.path,
            row.name,
            row.name_orig,
            row.ext,
            row.parent,
            row.size,
            row.mtime,
            i64::from(row.is_dir),
            i64::from(row.is_symlink)
        ],
    )
    .map_err(sql_err)?;
    if let Some(id) = row_id(conn, &row.path)? {
        sync_fts(conn, id, &row.name)?;
    }
    Ok(())
}

fn delete_row(conn: &Connection, path: &str) -> Result<(), CoreError> {
    if let Some(id) = row_id(conn, path)? {
        conn.execute("DELETE FROM files_fts WHERE rowid = ?1;", [id])
            .map_err(sql_err)?;
        conn.execute("DELETE FROM files WHERE id = ?1;", [id])
            .map_err(sql_err)?;
    }
    Ok(())
}

fn rename_row(conn: &Connection, from: &str, to: &str) -> Result<(), CoreError> {
    let Some(id) = row_id(conn, from)? else {
        return Ok(());
    };
    let base = std::path::Path::new(to)
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let parent = std::path::Path::new(to)
        .parent()
        .map_or_else(|| "/".to_string(), shown);
    let name_lower = base.to_lowercase();
    let ext = IndexedRow::extension_of(&base);
    conn.execute(
        "UPDATE files SET path = ?2, name = ?3, name_orig = ?4, ext = ?5, parent = ?6 WHERE id = ?1;",
        rusqlite::params![id, to, name_lower, base, ext, parent],
    )
    .map_err(sql_err)?;
    sync_fts(conn, id, &name_lower)
}

/// The subtree move: root row first, then the prefix rewrite over `path`/`parent` for everything under
/// `from`. The `LIKE … ESCAPE '\'` predicate is the bounded one (it is a prefix range on the
/// `idx_files_shadow_parent`/`path` indexes); `replace()` would rewrite mid-path occurrences too.
fn move_subtree(conn: &Connection, from: &str, to: &str) -> Result<(), CoreError> {
    if let Some(id) = row_id(conn, from)? {
        let base = std::path::Path::new(to)
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let parent = std::path::Path::new(to)
            .parent()
            .map_or_else(|| "/".to_string(), shown);
        let name_lower = base.to_lowercase();
        let ext = IndexedRow::extension_of(&base);
        conn.execute(
            "UPDATE files SET path = ?2, parent = ?3, name = ?4, name_orig = ?5, ext = ?6 WHERE id = ?1;",
            rusqlite::params![id, to, parent, name_lower, base, ext],
        )
        .map_err(sql_err)?;
        sync_fts(conn, id, &name_lower)?;
    }
    conn.execute(
        "UPDATE files SET path = ?1 || substr(path, length(?2) + 1) \
         WHERE path LIKE ?2 || '/%' ESCAPE '\\';",
        rusqlite::params![to, from],
    )
    .map_err(sql_err)?;
    conn.execute(
        "UPDATE files SET parent = ?1 || substr(parent, length(?2) + 1) \
         WHERE parent = ?2 OR parent LIKE ?2 || '/%' ESCAPE '\\';",
        rusqlite::params![to, from],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// Drop a whole subtree. The FTS rowids are gathered *before* the `files` delete, because afterwards
/// there is nothing left to join against.
fn prune(conn: &Connection, path: &str) -> Result<(), CoreError> {
    let escaped = escape_like(path);
    conn.execute(
        "DELETE FROM files_fts WHERE rowid IN
            (SELECT id FROM files WHERE path = ?1 OR path LIKE ?2 || '/%' ESCAPE '\\');",
        rusqlite::params![path, escaped],
    )
    .map_err(sql_err)?;
    conn.execute(
        "DELETE FROM files WHERE path = ?1 OR path LIKE ?2 || '/%' ESCAPE '\\';",
        rusqlite::params![path, escaped],
    )
    .map_err(sql_err)?;
    Ok(())
}

// ------------------------------------------------------------------------------------------------ queue

/// The step-12 [`IndexIntent`]: a queue, not a writer. `Session::poll_at` performs the drain; nothing
/// here owns a connection, which is the point of the seam (`index/intent.rs`).
#[derive(Debug, Default)]
pub struct QueuedIntent {
    pending: Mutex<Vec<IndexOp>>,
}

impl QueuedIntent {
    /// An empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ops waiting, in arrival order.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// `true` when nothing is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Take everything (the drain's half of [`drain_into`], for callers that must apply outside a lock).
    pub fn take(&self) -> Vec<IndexOp> {
        std::mem::take(
            &mut *self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// Take and apply in one coalesced pass; returns the number of ops executed.
    ///
    /// # Errors
    /// As [`apply_ops`].
    pub fn drain_into(&self, conn: &Connection) -> Result<usize, CoreError> {
        let ops = self.take();
        apply_ops(conn, &ops)
    }
}

impl IndexIntent for QueuedIntent {
    fn note(&self, op: IndexOp) {
        let mut guard = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Coalesce on arrival too, so a hot directory cannot grow the queue unboundedly between drains.
        if let Some(last) = guard.last()
            && last.subject() == op.subject()
            && let Some(merged) = IndexOp::merged(last, &op)
        {
            let _ = guard.pop();
            guard.push(merged);
            return;
        }
        guard.push(op);
    }

    fn note_all(&self, ops: &[IndexOp]) {
        for op in ops {
            self.note(op.clone());
        }
    }
}
