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

/// The SQL predicate "this column is `exact`'s row or a *proper* descendant of it", over the SQL text
/// `exact`/`esc` (the same path, raw and [`escape_like`]d).
///
/// Two guards, both needed:
/// * the prefix is `LIKE`-escaped (Go's `escapeLikePattern`), because `%`/`_` are wildcards there — an
///   unescaped `from` would rewrite or delete unrelated siblings of a path containing either byte;
/// * `LIKE` folds ASCII case while `=` does not, so the `substr(…) = … || sep` conjunction is what keeps
///   `/root/Mix` from being pruned by `Pruned { path: "/root/mix" }`. The `LIKE` half is the indexed
///   range scan (plan §5.5); the `substr` half only rejects rows the range already selected.
fn subtree_cond(column: &str, exact: &str, esc: &str) -> String {
    let sep = std::path::MAIN_SEPARATOR;
    format!(
        "({col} = {exact} OR ({col} LIKE {esc} || '{sep}%' ESCAPE '\\' \
         AND substr({col}, 1, length({exact}) + 1) = {exact} || '{sep}'))",
        col = column
    )
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
        &format!(
            "UPDATE files SET path = ?1 || substr(path, length(?2) + 1) WHERE {};",
            subtree_cond("path", "?2", "?3")
        ),
        rusqlite::params![to, from, escape_like(from)],
    )
    .map_err(sql_err)?;
    conn.execute(
        &format!(
            "UPDATE files SET parent = ?1 || substr(parent, length(?2) + 1) WHERE {};",
            subtree_cond("parent", "?2", "?3")
        ),
        rusqlite::params![to, from, escape_like(from)],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// Drop a whole subtree. The FTS rowids are gathered *before* the `files` delete, because afterwards
/// there is nothing left to join against.
fn prune(conn: &Connection, path: &str) -> Result<(), CoreError> {
    let cond = subtree_cond("path", "?1", "?2");
    let escaped = escape_like(path);
    conn.execute(
        &format!("DELETE FROM files_fts WHERE rowid IN (SELECT id FROM files WHERE {cond});"),
        rusqlite::params![path, escaped],
    )
    .map_err(sql_err)?;
    conn.execute(
        &format!("DELETE FROM files WHERE {cond};"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::schema;
    use crate::path::AbsPath;
    use std::path::Path;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory database");
        conn.execute_batch("PRAGMA journal_mode = DELETE;")
            .expect("journal mode");
        schema::apply(&conn).expect("the Go schema");
        conn
    }

    fn abs(p: &str) -> AbsPath {
        AbsPath::from_path(p).expect("absolute")
    }

    /// A row that exists only in the index (the updater's `Pruned`/`Moved` predicates never touch the
    /// filesystem, so the nasty path spellings do not have to be creatable on disk).
    fn insert_row(conn: &Connection, path: &str, is_dir: bool) -> i64 {
        let p = Path::new(path);
        let base = p
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let parent = p
            .parent()
            .map_or_else(|| "/".to_string(), |x| x.display().to_string());
        conn.execute(
            "INSERT INTO files (path,name,name_orig,ext,parent,size,mtime,is_dir,is_symlink) \
             VALUES (?1,?2,?3,'',?4,1,1700000000,?5,0);",
            rusqlite::params![path, base.to_lowercase(), base, parent, i64::from(is_dir)],
        )
        .expect("row");
        let id: i64 = conn
            .query_row("SELECT id FROM files WHERE path = ?1;", [path], |r| {
                r.get(0)
            })
            .expect("id");
        conn.execute(
            "INSERT INTO files_fts (rowid, name) VALUES (?1, ?2);",
            rusqlite::params![id, base.to_lowercase()],
        )
        .expect("fts row");
        id
    }

    fn listed(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT path FROM files ORDER BY path;")
            .expect("prepared");
        stmt.query_map([], |r| r.get::<_, String>(0))
            .expect("query")
            .filter_map(Result::ok)
            .collect()
    }

    fn fts_rowids(conn: &Connection) -> Vec<i64> {
        let mut stmt = conn
            .prepare("SELECT rowid FROM files_fts ORDER BY rowid;")
            .expect("prepared");
        stmt.query_map([], |r| r.get::<_, i64>(0))
            .expect("query")
            .filter_map(Result::ok)
            .collect()
    }

    fn fts_names(conn: &Connection, rowid: i64) -> String {
        conn.query_row(
            "SELECT name FROM files_fts WHERE rowid = ?1;",
            [rowid],
            |r| r.get::<_, String>(0),
        )
        .unwrap_or_default()
    }

    /// The fixture the wildcards are aimed at: `%`, `_`, `\`, quotes and a case twin of one directory.
    fn wildcard_fixture(conn: &Connection) {
        for path in [
            "/root/100%",
            "/root/100%/in.txt",
            "/root/100x",
            "/root/100x/in.txt",
            "/root/a_b",
            "/root/a_b/in.txt",
            "/root/aXb",
            "/root/aXb/in.txt",
            "/root/mix",
            "/root/mix/in.txt",
            "/root/MIX",
            "/root/MIX/in.txt",
            "/root/back\\slash",
            "/root/back\\slash/in.txt",
            "/root/quote'\"%/in.txt",
        ] {
            insert_row(conn, path, path.ends_with("100%"));
        }
    }

    /// A fresh scratch tree in a guard that deletes it on drop (so a panicking assert still cleans
    /// up); the naming and the reap come from `crate::scratch`.
    fn scratch(tag: &str) -> crate::scratch::Scratch {
        crate::scratch::Scratch::new(&format!("updater-{tag}"))
    }

    #[test]
    fn created_and_refreshed_write_gos_columns_and_sync_fts() {
        let dir = scratch("create");
        let file = dir.join("Report.TXT");
        std::fs::write(&file, b"hello").expect("file");
        let path = abs(&file.display().to_string());
        let conn = db();
        apply_op(&conn, &IndexOp::Created { path: path.clone() }).expect("upsert");
        let row: (String, String, String, String, i64) = conn
            .query_row(
                "SELECT path,name,name_orig,ext,is_dir FROM files;",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .expect("one row");
        assert_eq!(row.1, "report.txt", "`NameLower` is the lowercase column");
        assert_eq!(row.2, "Report.TXT", "the raw name survives beside it");
        assert_eq!(
            row.3, "txt",
            "Go's `filepath.Ext` minus the dot, lowercased"
        );
        assert_eq!(row.4, 0);
        assert_eq!(
            fts_names(&conn, 1),
            "report.txt",
            "FTS is keyed to the rowid"
        );

        // A refresh of the same path is an update, not a second row (`ON CONFLICT(path)`).
        apply_op(&conn, &IndexOp::Refreshed { path: path.clone() }).expect("refresh");
        assert_eq!(listed(&conn).len(), 1);
        assert_eq!(fts_rowids(&conn), vec![1], "one rowid, one FTS entry");
        // And a directory is indexed with `is_dir` from the readdir type, like the walker's row.
        apply_op(
            &conn,
            &IndexOp::Created {
                path: abs(&dir.display().to_string()),
            },
        )
        .expect("dir");
        let is_dir: i64 = conn
            .query_row(
                "SELECT is_dir FROM files WHERE path = ?1;",
                [dir.display().to_string()],
                |r| r.get(0),
            )
            .expect("row");
        assert_eq!(is_dir, 1);
    }

    #[test]
    fn a_vanished_subject_degrades_to_a_delete_and_absence_is_idempotent() {
        let dir = scratch("vanished");
        let file = dir.join("gone.txt");
        std::fs::write(&file, b"x").expect("file");
        let path = abs(&file.display().to_string());
        let conn = db();
        apply_op(&conn, &IndexOp::Created { path: path.clone() }).expect("created");
        assert_eq!(listed(&conn).len(), 1);
        std::fs::remove_file(&file).expect("rm");
        // "Created then vanished" reconciles to the row drop — the same answer the walker gives for an
        // entry whose `lstat` fails.
        apply_op(&conn, &IndexOp::Refreshed { path: path.clone() }).expect("degrades");
        assert!(listed(&conn).is_empty());
        assert!(fts_rowids(&conn).is_empty(), "no ghost token behind");

        // A delete of a path the index never knew is a no-op, not an error, twice over.
        for _ in 0..2 {
            apply_op(
                &conn,
                &IndexOp::Deleted {
                    path: abs("/never/existed.txt"),
                },
            )
            .expect("idempotent");
        }
        assert!(listed(&conn).is_empty());
    }

    #[test]
    fn rename_writes_the_new_name_and_keeps_the_rowid() {
        let conn = db();
        let id = insert_row(&conn, "/root/old.txt", false);
        apply_op(
            &conn,
            &IndexOp::Renamed {
                from: abs("/root/old.txt"),
                to: abs("/root/new.md"),
            },
        )
        .expect("rename");
        let row: (String, String, String, String) = conn
            .query_row(
                "SELECT path,name,ext,parent FROM files WHERE id = ?1;",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .expect("row");
        assert_eq!(row.0, "/root/new.md");
        assert_eq!(row.1, "new.md");
        assert_eq!(row.2, "md", "the extension is re-derived, not carried over");
        assert_eq!(row.3, "/root");
        assert_eq!(
            fts_rowids(&conn),
            vec![id],
            "the FTS rowid coupling survives"
        );
        assert_eq!(fts_names(&conn, id), "new.md");
    }

    #[test]
    fn a_rename_onto_a_taken_path_is_refused_and_moves_nothing() {
        // `files.path` is UNIQUE (Go's `idx_files_shadow_path`), and two rows may not claim one path; the
        // batch aborts, the session marks the index dirty, and a rebuild reconciles (module docs).
        let conn = db();
        insert_row(&conn, "/root/a.txt", false);
        insert_row(&conn, "/root/b.txt", false);
        let before = listed(&conn);
        let err = apply_op(
            &conn,
            &IndexOp::Renamed {
                from: abs("/root/a.txt"),
                to: abs("/root/b.txt"),
            },
        )
        .expect_err("the unique index refuses it");
        assert!(matches!(err, CoreError::Sql { .. }), "{err:?}");
        assert_eq!(listed(&conn), before, "neither row moved");
        assert_eq!(fts_rowids(&conn).len(), 2, "and FTS was not half-updated");
    }

    #[test]
    fn moved_rewrites_the_subtree_and_keeps_the_fts_rowids() {
        let conn = db();
        let dir = insert_row(&conn, "/root/pkg", true);
        let inner = insert_row(&conn, "/root/pkg/lib/in.txt", false);
        let deep = insert_row(&conn, "/root/pkg/lib/deep/down.txt", false);
        let outside = insert_row(&conn, "/root/pkgish/in.txt", false);
        apply_op(
            &conn,
            &IndexOp::Moved {
                from: abs("/root/pkg"),
                to: abs("/root/vendor/pkg"),
            },
        )
        .expect("move");
        assert_eq!(
            listed(&conn),
            vec![
                "/root/pkgish/in.txt",
                "/root/vendor/pkg",
                "/root/vendor/pkg/lib/deep/down.txt",
                "/root/vendor/pkg/lib/in.txt",
            ]
        );
        let parents: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT parent FROM files ORDER BY path;")
                .expect("prepared");
            stmt.query_map([], |r| r.get::<_, String>(0))
                .expect("query")
                .filter_map(Result::ok)
                .collect()
        };
        assert_eq!(
            parents,
            vec![
                "/root/pkgish",
                "/root/vendor",
                "/root/vendor/pkg/lib/deep",
                "/root/vendor/pkg/lib"
            ],
            "`parent` is rewritten alongside `path`, in Go's `filepath.Dir` shape"
        );
        assert_eq!(
            fts_rowids(&conn),
            vec![dir, inner, deep, outside],
            "rowids survive an UPDATE, so FTS needs no touch when the name is preserved"
        );
        assert_eq!(
            (fts_names(&conn, inner), fts_names(&conn, deep)),
            ("in.txt".to_string(), "down.txt".to_string()),
            "the tokens the surviving rowids carry are the file names, unchanged by the move"
        );
    }

    #[test]
    fn the_prune_prefix_is_like_escaped_and_case_exact() {
        // Go's `escapeLikePattern` (`api/search_index.go:394-399`) exists because `%` and `_` are `LIKE`
        // wildcards: unescaped, a prune of `/root/100%` would also delete `/root/100x`, and `a_b` would
        // reach `aXb`. Case is the other half — SQLite's `LIKE` folds ASCII, `=` does not.
        let conn = db();
        wildcard_fixture(&conn);
        let all = listed(&conn).len();
        apply_op(
            &conn,
            &IndexOp::Pruned {
                path: abs("/root/100%"),
            },
        )
        .expect("prune");
        let after = listed(&conn);
        assert!(
            after.iter().all(|p| !p.starts_with("/root/100%")),
            "{after:?}"
        );
        assert!(
            after.iter().any(|p| p == "/root/100x/in.txt"),
            "the sibling that a wildcard `%` would have swallowed"
        );
        assert_eq!(
            after.len(),
            all - 2,
            "exactly the directory and its one child"
        );

        apply_op(
            &conn,
            &IndexOp::Pruned {
                path: abs("/root/a_b"),
            },
        )
        .expect("prune");
        let after = listed(&conn);
        assert!(after.iter().all(|p| !p.starts_with("/root/a_b")));
        assert!(
            after.iter().any(|p| p == "/root/aXb/in.txt"),
            "`_` is one wildcard byte"
        );

        apply_op(
            &conn,
            &IndexOp::Pruned {
                path: abs("/root/mix"),
            },
        )
        .expect("prune");
        let after = listed(&conn);
        assert!(
            after.iter().any(|p| p == "/root/MIX/in.txt"),
            "a prune of `/root/mix` must not eat `/root/MIX` (LIKE folds case, the guard does not): {after:?}"
        );

        apply_op(
            &conn,
            &IndexOp::Pruned {
                path: abs("/root/back\\slash"),
            },
        )
        .expect("prune");
        assert!(listed(&conn).iter().all(|p| !p.contains("back\\slash")));

        // A quote in the path is data, not syntax: the row is still there and still findable.
        assert!(listed(&conn).iter().any(|p| p == "/root/quote'\"%/in.txt"));
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM files_fts WHERE rowid = (SELECT id FROM files WHERE path = ?1);",
                ["/root/quote'\"%/in.txt"],
                |r| r.get::<_, i64>(0),
            )
            .expect("fts"),
            1,
            "the surviving row keeps its FTS entry through every prune"
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM files_fts WHERE rowid NOT IN (SELECT id FROM files);",
                [],
                |r| r.get::<_, i64>(0)
            )
            .expect("fts"),
            0,
            "FTS rowids were gathered before the delete: no ghost entry outlives a pruned row"
        );
    }

    #[test]
    fn a_moved_prefix_containing_wildcards_rewrites_only_its_own_subtree() {
        // The same class on the `UPDATE … WHERE path LIKE from || '/%'` statement: the bound `from` is a
        // *pattern* there, so it must arrive escaped (Go's `escapeLikePattern`), or `/root/100x` is
        // rewritten into the new tree.
        let conn = db();
        wildcard_fixture(&conn);
        apply_op(
            &conn,
            &IndexOp::Moved {
                from: abs("/root/100%"),
                to: abs("/root/200%"),
            },
        )
        .expect("move");
        let after = listed(&conn);
        assert!(after.iter().any(|p| p == "/root/200%/in.txt"), "{after:?}");
        assert!(
            after.iter().any(|p| p == "/root/100x/in.txt"),
            "the wildcard-free sibling was rewritten too: {after:?}"
        );
        assert!(after.iter().all(|p| !p.starts_with("/root/100%/")));
        let parent: String = conn
            .query_row(
                "SELECT parent FROM files WHERE path = '/root/200%/in.txt';",
                [],
                |r| r.get(0),
            )
            .expect("row");
        assert_eq!(parent, "/root/200%");
    }

    #[test]
    fn attributes_changed_writes_no_statement() {
        let conn = db();
        insert_row(&conn, "/root/x.txt", false);
        let before = (listed(&conn), fts_rowids(&conn));
        apply_op(
            &conn,
            &IndexOp::AttributesChanged {
                path: abs("/root/x.txt"),
            },
        )
        .expect("accepted");
        assert_eq!(listed(&conn), before.0);
        assert_eq!(
            fts_rowids(&conn),
            before.1,
            "the schema stores no mode, so there is nothing to write"
        );
    }

    #[test]
    fn coalescing_collapses_one_subject_and_keeps_the_order() {
        let a = abs("/root/a.txt");
        let b = abs("/root/b.txt");
        let ops = [
            IndexOp::Created { path: a.clone() },
            IndexOp::Refreshed { path: a.clone() },
            IndexOp::Deleted { path: a.clone() },
            IndexOp::Created { path: b.clone() },
            IndexOp::Deleted { path: b.clone() },
        ];
        let out = coalesce(&ops);
        assert_eq!(
            out,
            vec![
                // `Created` and `Refreshed` are different claims and `IndexOp::merged` (intent.rs, pinned
                // by its own tests) only collapses identical ops or a subject that ends in a delete/prune,
                // so both survive; the *trailing* delete is the one that absorbs its predecessors. On `/b`
                // the run is create→delete, which collapses to the single op that has to touch the DB.
                IndexOp::Created { path: a.clone() },
                IndexOp::Refreshed { path: a.clone() },
                IndexOp::Deleted { path: a.clone() },
                IndexOp::Deleted { path: b.clone() },
            ],
            "N ops on one path stay in order; two paths never interleave"
        );
        // A prune absorbs everything before it on the same subject.
        let pruned = coalesce(&[
            IndexOp::Renamed {
                from: a.clone(),
                to: b.clone(),
            },
            IndexOp::Pruned { path: b.clone() },
        ]);
        assert_eq!(pruned, vec![IndexOp::Pruned { path: b.clone() }]);
        assert_eq!(coalesce(&[]), Vec::<IndexOp>::new());
    }

    #[test]
    fn the_queue_drains_in_arrival_order_and_reports_what_it_applied() {
        let conn = db();
        insert_row(&conn, "/root/gone.txt", false);
        insert_row(&conn, "/root/kept.txt", false);
        let queue = QueuedIntent::new();
        assert!(queue.is_empty(), "a fresh queue holds nothing");
        assert_eq!(queue.len(), 0, "and `len` agrees with `is_empty`");
        queue.note(IndexOp::Deleted {
            path: abs("/root/gone.txt"),
        });
        queue.note(IndexOp::Deleted {
            path: abs("/root/gone.txt"),
        });
        queue.note_all(&[
            IndexOp::Pruned {
                path: abs("/root/gone"),
            },
            IndexOp::Deleted {
                path: abs("/root/absent.txt"),
            },
        ]);
        assert_eq!(queue.len(), 3, "the duplicate delete collapsed on arrival");
        assert_eq!(queue.drain_into(&conn).expect("drain"), 3);
        assert!(queue.is_empty(), "a drain takes everything");
        assert_eq!(listed(&conn), vec!["/root/kept.txt"]);
        assert_eq!(queue.drain_into(&conn).expect("an empty drain"), 0);
        // `take` is the caller's half of the same contract: nothing is lost, order is kept.
        queue.note(IndexOp::Deleted { path: abs("/x") });
        queue.note(IndexOp::Deleted { path: abs("/y") });
        assert_eq!(
            queue
                .take()
                .iter()
                .map(IndexOp::subject)
                .map(|p| p.to_string_lossy())
                .collect::<Vec<_>>(),
            vec!["/x", "/y"]
        );
    }
}
