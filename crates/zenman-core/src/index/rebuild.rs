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
/// the cleanup must not mask the error that caused it). The live table's index set is re-ensured here
/// too: `IndexShadow` drops the colliding `idx_files_shadow_*` names off `files` before creating them on
/// the shadow, and this path runs for every cancel/error, so the live table must leave it indexed.
fn cleanup(conn: &Connection) {
    let _ = conn.execute_batch(schema::SHADOW_DROP_SQL);
    let _ = conn.execute_batch(schema::SHADOW_FTS_DROP_SQL);
    for idx in schema::DDL_LIVE_INDEX_RESTORE {
        let _ = conn.execute_batch(idx);
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::schema;
    use crate::path::AbsPath;
    use std::path::{Path, PathBuf};

    /// Go's `runRebuild` statement order (`api/search_index_walker.go:74-286`).
    const ALL_STAGES: [Stage; 5] = [
        Stage::PrepareShadow,
        Stage::Walk,
        Stage::IndexShadow,
        Stage::PopulateFts,
        Stage::Swap,
    ];

    fn db() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory database");
        conn.execute_batch("PRAGMA journal_mode = DELETE;")
            .expect("journal mode");
        schema::apply(&conn).expect("the Go schema");
        conn
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zenman-rebuild-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        for rel in [
            "one.txt",
            "two.md",
            "leaf.txt",
            "sub/three.txt",
            "sub/deeper/four.txt",
        ] {
            let path = dir.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("parent");
            }
            std::fs::write(path, b"x").expect("file");
        }
        // The walked root is the scratch dir itself: the tree above is what the row counts expect.
        dir
    }

    fn cfg(root: &Path) -> WalkCfg {
        WalkCfg {
            roots: vec![AbsPath::from_path(root.display().to_string()).expect("absolute")],
            ..WalkCfg::default()
        }
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table};"), [], |r| r.get(0))
            .unwrap_or(0)
    }

    /// Every `sqlite_master` object a half-finished rebuild could leave behind.
    fn leftovers(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type IN ('table','index');")
            .expect("prepared");
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .expect("query");
        rows.filter_map(Result::ok)
            .filter(|n| n.starts_with("files_shadow") || n.starts_with("files_fts_shadow"))
            .collect()
    }

    fn seed(conn: &Connection, path: &str) {
        conn.execute(
            "INSERT INTO files (path,name,name_orig,ext,parent,size,mtime,is_dir,is_symlink) \
             VALUES (?1,'seeded.txt','seeded.txt','txt','/seeded',1,1700000000,0,0);",
            [path],
        )
        .expect("seed row");
        let id: i64 = conn
            .query_row("SELECT id FROM files WHERE path = ?1;", [path], |r| {
                r.get(0)
            })
            .expect("id");
        conn.execute(
            "INSERT INTO files_fts (rowid, name) VALUES (?1, 'seeded.txt');",
            [id],
        )
        .expect("seed fts");
    }

    fn index_names(conn: &Connection, table: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM pragma_index_list(?1);")
            .expect("prepared");
        stmt.query_map([table], |r| r.get::<_, String>(0))
            .expect("query")
            .filter_map(Result::ok)
            .collect()
    }

    fn explain(conn: &Connection, sql: &str) -> String {
        let stmt = conn.prepare(sql).expect("prepared");
        let _ = stmt;
        let mut out = String::new();
        let mut plan = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql};"))
            .expect("plan");
        for row in plan
            .query_map([], |r| r.get::<_, String>(3))
            .expect("rows")
            .filter_map(Result::ok)
        {
            out.push_str(&row);
            out.push('\n');
        }
        out
    }

    #[test]
    fn the_stages_run_in_gos_order_and_report_their_row_counts() {
        let root = scratch("order");
        let conn = db();
        let seen = std::cell::RefCell::new(Vec::new());
        let summary = run_with_hook(&conn, &cfg(&root), &CancelToken::new(), &|p| {
            seen.borrow_mut().push((p.stage, p.rows));
        })
        .expect("the pipeline runs");
        let stages: Vec<Stage> = seen.borrow().iter().map(|(s, _)| *s).collect();
        assert_eq!(stages, ALL_STAGES, "no stage skipped, none reordered");
        // The row counter the progress line shows is the walk's output: nothing before the walk, the
        // full count from the next boundary on.
        assert_eq!(seen.borrow()[0].1, 0);
        assert_eq!(
            seen.borrow()[1].1,
            0,
            "the walk reports its rows when it is done"
        );
        for (stage, rows) in seen.borrow().iter().skip(2) {
            assert_eq!(
                *rows,
                summary.rows,
                "{} saw the walk's count",
                stage.as_str()
            );
        }
        assert_eq!(summary.rows, 8, "root + 4 files + 3 directories");
        let rows = i64::try_from(summary.rows).expect("the row count fits an i64");
        assert_eq!(count(&conn, "files"), rows, "walked == swapped in");
        assert_eq!(
            count(&conn, "files_fts"),
            rows,
            "FTS is filled from the shadow"
        );
        assert!(summary.roots_failed.is_empty() && summary.roots_ok == 1);
    }

    #[test]
    fn every_stage_boundary_is_a_clean_cancel_point() {
        // The atomicity guarantee: whatever the stage, cancelling there leaves the *live* tables exactly
        // as they were and no shadow object behind (Go's `DROP TABLE IF EXISTS` cleanup on each error
        // path, `:82-124` + `:286`).
        let root = scratch("cancel");
        for target in ALL_STAGES {
            let conn = db();
            seed(&conn, "/seeded/old.txt");
            let cancel = CancelToken::new();
            let reached = std::cell::Cell::new(false);
            let result = run_with_hook(&conn, &cfg(&root), &cancel, &|p| {
                if p.stage == target {
                    reached.set(true);
                    let _ = cancel.cancel();
                }
            });
            assert!(
                reached.get(),
                "the pipeline never offered {} as a cancel point",
                target.as_str()
            );
            assert!(
                matches!(result, Err(CoreError::Cancelled)),
                "{target:?}: {result:?}"
            );
            assert_eq!(
                leftovers(&conn),
                Vec::<String>::new(),
                "shadow objects survive a cancel at {}",
                target.as_str()
            );
            assert_eq!(
                count(&conn, "files"),
                1,
                "the live rows a cancel must not touch ({}), all: {:?}",
                target.as_str(),
                leftovers(&conn)
            );
            assert_eq!(
                count(&conn, "files_fts"),
                1,
                "FTS still answers for {}",
                target.as_str()
            );
            assert!(
                index_names(&conn, "files").len() >= 5,
                "the live table keeps its indexes through a cancel at {}",
                target.as_str()
            );
            assert!(
                schema::verify(&conn)
                    .map(|issues| issues.is_empty())
                    .unwrap_or(false),
                "the schema is intact after a cancel at {}",
                target.as_str()
            );
        }
    }

    #[test]
    fn the_hook_fires_before_the_cancel_check() {
        // The ordering is what makes `contract_search::rebuild_is_atomic` deterministic instead of racy:
        // an observer that cancels inside the hook stops the pipeline *at the named stage*, before that
        // stage's statements run — and a token that already fired still lets the first boundary report
        // itself, so the progress line never lies about where the walk died.
        let root = scratch("hook-first");
        let conn = db();
        seed(&conn, "/seeded/old.txt");
        let cancel = CancelToken::new();
        let _ = cancel.cancel();
        let seen = std::cell::RefCell::new(Vec::new());
        let result = run_with_hook(&conn, &cfg(&root), &cancel, &|p| {
            seen.borrow_mut().push(p.stage);
        });
        assert!(matches!(result, Err(CoreError::Cancelled)));
        assert_eq!(
            *seen.borrow(),
            vec![Stage::PrepareShadow],
            "one boundary, then the stop"
        );
        assert_eq!(count(&conn, "files"), 1, "nothing was written");
        assert_eq!(leftovers(&conn), Vec::<String>::new());
    }

    #[test]
    fn a_rebuild_after_a_rebuild_leaves_the_swapped_table_indexed() {
        // The §9.2 collision: a fresh database carries `idx_files_shadow_*` **on `files`**, so a rebuild
        // that only says `CREATE … IF NOT EXISTS` silently skips them, and the swap then drops the live
        // table with its indexes — leaving the renamed shadow unindexed. Go carries this from its second
        // rebuild on; the port drops the colliding names first.
        let root = scratch("twice");
        let conn = db();
        run(&conn, &cfg(&root), &CancelToken::new()).expect("first rebuild");
        run(&conn, &cfg(&root), &CancelToken::new()).expect("second rebuild");
        let names = index_names(&conn, "files");
        for want in schema::SHADOW_INDEX_NAMES {
            assert!(
                names.iter().any(|n| n == want),
                "{want} is missing after the second swap: {names:?}"
            );
        }
        assert_eq!(names.len(), 5, "{names:?}");
        // "Indexed" is proven by the planner, not by the name list alone.
        let plan = explain(&conn, "SELECT id FROM files WHERE path = '/x/y'");
        assert!(
            plan.contains("idx_files_shadow_path"),
            "a path lookup must not full-scan the swapped table: {plan}"
        );
        assert!(schema::verify(&conn).expect("verify").is_empty());
        assert_eq!(
            leftovers(&conn),
            Vec::<String>::new(),
            "a swap eats its shadow"
        );
    }

    #[test]
    fn a_cancelled_rebuild_leaves_the_pipeline_re_runnable() {
        let root = scratch("rerun");
        let conn = db();
        seed(&conn, "/seeded/old.txt");
        let cancel = CancelToken::new();
        let stop = run_with_hook(&conn, &cfg(&root), &cancel, &|p| {
            if p.stage == Stage::Swap {
                let _ = cancel.cancel();
            }
        });
        assert!(matches!(stop, Err(CoreError::Cancelled)));
        assert_eq!(
            count(&conn, "files"),
            1,
            "the cancelled run changed nothing"
        );

        let summary = run(&conn, &cfg(&root), &CancelToken::new()).expect("the second run lands");
        assert_eq!(
            count(&conn, "files"),
            i64::try_from(summary.rows).expect("the row count fits an i64")
        );
        assert_eq!(
            count(&conn, "files"),
            8,
            "the seeded row is gone: the swap replaces the table"
        );
        let hit: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files_fts WHERE files_fts MATCH '\"leaf\"';",
                [],
                |r| r.get(0),
            )
            .expect("fts query");
        assert_eq!(hit, 1, "FTS answers for the swapped-in rows");
        let parent: String = conn
            .query_row(
                "SELECT parent FROM files WHERE name = 'four.txt';",
                [],
                |r| r.get(0),
            )
            .expect("row");
        assert!(parent.ends_with("/sub/deeper"), "{parent}");
    }
}
