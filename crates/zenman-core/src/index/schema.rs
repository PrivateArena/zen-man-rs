//! The search-index schema, copied from the Go binary's own DDL (plan §5.5 `index/schema.rs` row).
//!
//! # Where the text comes from
//!
//! `api/db.go:52-58` (`folder_sizes`), `api/search_index.go:141-196` (`schema_meta`, `files`, the five
//! indexes, `files_fts`), cross-checked against `sqlite_master` of the *Go-produced*
//! `tests/golden/zen-man.db` — 4 base tables, 5 indexes, 1 260 `files` rows, 31 `folder_sizes` rows,
//! `journal_mode = delete`, `page_size = 4096`, `user_version = 0`, `search_schema_version = '1'`.
//!
//! # The two things a reader will think are typos
//!
//! * **The index names say `shadow`.** Go builds a rebuild into `files_shadow`/`files_fts_shadow`, then
//!   `ALTER TABLE … RENAME TO files` (`api/search_index_walker.go:227-241`), and a rename carries the
//!   index names with it. Every Go-produced database therefore carries
//!   `idx_files_shadow_{path,name,ext,parent,mtime}` attached to `"files"` — confirmed in the golden's
//!   `sqlite_master`, and stated in `docs/contracts.md` §*SQLite storage*. This module writes the same
//!   names, because a schema our own `verify` would flag on the very database we must open is not a
//!   schema, it is a bug. Fresh databases therefore look Go-produced from the first commit; that is the
//!   persistence-compatibility decision (plan §8 D1 / §11.1).
//! * **There is no `watchers` table.** The step-5 brief and plan §5.5's prose both mention a
//!   `watchers` row with a `last_error TEXT` column; no such table exists in the Go database, in
//!   `api/*.go` (a repo-wide grep for `watchers` finds nothing) or in the plan text. Inotify-style
//!   watching was never persisted by Go, so inventing the table would be schema drift against a live
//!   database. If step 11 wants watcher state, it adds a versioned table here — it must not "fix" the
//!   absence of one now.
//!
//! [`SCHEMA_VERSION`] mirrors Go's `schema_meta.search_schema_version`, which is the *only* version
//! stamp Go writes (`api/search_index.go:150`, `:200-228`).

use rusqlite::{Connection, Row};
use thiserror::Error;

use crate::error::CoreError;

/// Go's `search_schema_version` value (`api/search_index.go:150`), and the value the golden carries.
pub const SCHEMA_VERSION: u32 = 1;
/// The `schema_meta` key Go stamps the version under.
pub const VERSION_KEY: &str = "search_schema_version";

/// The four base tables, verbatim from the Go source, in Go's creation order
/// (`folder_sizes` first: `InitDB` runs before `InitSearchTables`, `api/db.go:50-70`).
pub const DDL_TABLES: &str = r#"
CREATE TABLE IF NOT EXISTS folder_sizes (
		path TEXT PRIMARY KEY,
		size INTEGER,
		file_count INTEGER,
		updated_at INTEGER
	);
CREATE TABLE IF NOT EXISTS schema_meta (
		key   TEXT PRIMARY KEY,
		value TEXT
	);
CREATE TABLE IF NOT EXISTS files (
		id         INTEGER PRIMARY KEY,
		path       TEXT NOT NULL,
		name       TEXT NOT NULL,
		name_orig  TEXT NOT NULL,
		ext        TEXT NOT NULL,
		parent     TEXT NOT NULL,
		size       INTEGER NOT NULL,
		mtime      INTEGER NOT NULL,
		is_dir     INTEGER NOT NULL,
		is_symlink INTEGER NOT NULL
	);
CREATE VIRTUAL TABLE IF NOT EXISTS files_fts USING fts5(
		name,
		tokenize='trigram'
	);
"#;

/// The five indexes, with the names Go's shadow→live rename leaves behind (module docs). `path` is the
/// unique one; the rest are plain (`api/search_index.go:174-180`).
pub const DDL_INDEXES: [&str; 5] = [
    "CREATE UNIQUE INDEX IF NOT EXISTS idx_files_shadow_path   ON files(path);",
    "CREATE        INDEX IF NOT EXISTS idx_files_shadow_name   ON files(name);",
    "CREATE        INDEX IF NOT EXISTS idx_files_shadow_ext    ON files(ext);",
    "CREATE        INDEX IF NOT EXISTS idx_files_shadow_parent ON files(parent);",
    "CREATE        INDEX IF NOT EXISTS idx_files_shadow_mtime  ON files(mtime);",
];

/// What [`apply`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SchemaReport {
    /// Number of statements executed.
    pub statements: usize,
    /// The `schema_meta` version found afterwards.
    pub version: u32,
    /// `true` when this call created `files` (a brand-new database), `false` when it was already there
    /// — the Go-database case, which must be a no-op.
    pub created_tables: Vec<String>,
    /// The `PRAGMA user_version` value stamped after the transaction (see [`apply`]).
    pub user_version: u32,
}

/// One disagreement between the database and [`DDL_TABLES`]/[`DDL_INDEXES`].
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum SchemaIssue {
    /// A table Go always has is missing.
    #[error("table {0} is missing")]
    MissingTable(String),
    /// A column is missing, or has a different declared type / NOT NULL / PK role.
    #[error("table {table} column {column}: expected {expected}, found {found}")]
    Column {
        /// Which table.
        table: String,
        /// Which column.
        column: String,
        /// What this module's DDL asks for.
        expected: String,
        /// What the database has.
        found: String,
    },
    /// An index Go always has is missing.
    #[error("index {0} is missing")]
    MissingIndex(String),
    /// An index exists but on different columns, or with a different uniqueness.
    #[error("index {index}: expected {expected}, found {found}")]
    Index {
        /// Which index.
        index: String,
        /// What this module's DDL asks for.
        expected: String,
        /// What the database has.
        found: String,
    },
    /// `schema_meta.search_schema_version` is absent or not a number.
    #[error("schema version is {0:?}, expected {SCHEMA_VERSION}")]
    Version(Option<String>),
    /// The database could not be introspected (an unknown virtual-table module, a malformed row).
    #[error("cannot check {what}: {why}")]
    Unavailable {
        /// What was being read.
        what: String,
        /// Why it failed.
        why: String,
    },
}

/// Expected column shape of one table: `(name, declared type, notnull, pk)`.
type ColumnSpec = (&'static str, &'static str, bool, i32);

/// `folder_sizes` — the Go app's *only* pre-existing store (`api/db.go:52-58`).
///
/// `path` is declared `TEXT PRIMARY KEY` and reports **`notnull = 0`**, measured on both the golden and
/// this DDL: in a rowid table SQLite does not turn a `PRIMARY KEY TEXT` column into `NOT NULL` (only
/// `INTEGER PRIMARY KEY` becomes the rowid alias), so expecting `NOT NULL` here would make `verify`
/// reject the very database it is meant to accept. Same for `schema_meta.key` below.
const FOLDER_SIZES: &[ColumnSpec] = &[
    ("path", "TEXT", false, 1),
    ("size", "INTEGER", false, 0),
    ("file_count", "INTEGER", false, 0),
    ("updated_at", "INTEGER", false, 0),
];

/// `schema_meta` — Go's key/value stamp table (`api/search_index.go:142-145`).
const SCHEMA_META: &[ColumnSpec] = &[("key", "TEXT", false, 1), ("value", "TEXT", false, 0)];

/// `files` — the live index (`api/search_index.go:157-168`).
const FILES: &[ColumnSpec] = &[
    ("id", "INTEGER", false, 1),
    ("path", "TEXT", true, 0),
    ("name", "TEXT", true, 0),
    ("name_orig", "TEXT", true, 0),
    ("ext", "TEXT", true, 0),
    ("parent", "TEXT", true, 0),
    ("size", "INTEGER", true, 0),
    ("mtime", "INTEGER", true, 0),
    ("is_dir", "INTEGER", true, 0),
    ("is_symlink", "INTEGER", true, 0),
];

/// Every base table Go creates, with its columns. `files_fts` is handled separately: it is a virtual
/// table, so its column list is only readable when the FTS5 module is loaded (see [`verify`]).
pub const TABLES: [(&str, &[ColumnSpec]); 3] = [
    ("folder_sizes", FOLDER_SIZES),
    ("schema_meta", SCHEMA_META),
    ("files", FILES),
];

/// The FTS table's name and its single indexed column (`files_fts(name)`).
pub const FTS_TABLE: &str = "files_fts";

/// Every index Go leaves behind, with its uniqueness and column list.
pub const INDEXES: [(&str, &str, bool, &[&str]); 5] = [
    ("idx_files_shadow_path", "files", true, &["path"]),
    ("idx_files_shadow_name", "files", false, &["name"]),
    ("idx_files_shadow_ext", "files", false, &["ext"]),
    ("idx_files_shadow_parent", "files", false, &["parent"]),
    ("idx_files_shadow_mtime", "files", false, &["mtime"]),
];

/// Create whatever is missing, in **one transaction**, and stamp the version.
///
/// `CREATE … IF NOT EXISTS` everywhere means this is a no-op on a Go-produced database — which is the
/// acceptance gate (`contract_history.rs` opens the golden `zen-man.db` copy and asserts
/// [`verify`] returns nothing). The version row is written with Go's own
/// `INSERT OR IGNORE`, so an existing value is never overwritten: Go's `runSearchMigrations`
/// (`api/search_index.go:200-228`) is the only writer, and a future migration must go through it.
///
/// `PRAGMA user_version` is written **after** the commit, not inside it: it lives in the database file
/// header, where SQLite does not treat it as transactional, so claiming otherwise would make the
/// one-transaction promise false. The value is `0` on every Go-produced database (measured on the
/// golden) and `SCHEMA_VERSION` on ours, which is a useful fingerprint and nothing more — the
/// authority stays `schema_meta`, exactly as in Go.
///
/// # Errors
///
/// [`CoreError::Sql`] for any SQLite failure.
pub fn apply(conn: &Connection) -> Result<SchemaReport, CoreError> {
    let before = table_names(conn)?;
    let sql = format!("{DDL_TABLES}{}", DDL_INDEXES.concat());
    let opened = conn.execute_batch("BEGIN;").is_ok();
    let done = conn
        .execute_batch(&sql)
        .and_then(|()| conn.execute_batch("COMMIT;"));
    if let Err(e) = done {
        if opened {
            let _ = conn.execute_batch("ROLLBACK;");
        }
        return Err(CoreError::Sql { msg: e.to_string() });
    }
    let version = stamp_version(conn)?;
    conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))
        .map_err(|e| CoreError::Sql { msg: e.to_string() })?;
    let user_version = user_version(conn)?;
    let after = table_names(conn)?;
    let created_tables = after.into_iter().filter(|t| !before.contains(t)).collect();
    Ok(SchemaReport {
        // One transaction wrapping the four tables and five indexes, then the version row and the
        // header stamp.
        statements: 1 + DDL_TABLES.matches("CREATE").count() + DDL_INDEXES.len() + 2,
        version,
        created_tables,
        user_version,
    })
}

/// Ensure `schema_meta.search_schema_version` exists, Go's `INSERT OR IGNORE` (`api/search_index.go:150`).
fn stamp_version(conn: &Connection) -> Result<u32, CoreError> {
    conn.execute(
        "INSERT OR IGNORE INTO schema_meta (key, value) VALUES (?1, ?2);",
        rusqlite::params![VERSION_KEY, SCHEMA_VERSION.to_string()],
    )
    .map_err(|e| CoreError::Sql { msg: e.to_string() })?;
    version_of(conn)
}

/// The `schema_meta` version Go records, or `0` when the row is absent or unreadable — Go's own
/// `strconv.Atoi` on a failed scan yields 0 too (`api/search_index.go:202-206`).
///
/// # Errors
///
/// [`CoreError::Sql`] when `schema_meta` itself is missing or unreadable.
pub fn version_of(conn: &Connection) -> Result<u32, CoreError> {
    let raw: Option<String> = match conn.query_row(
        "SELECT value FROM schema_meta WHERE key = ?1;",
        [VERSION_KEY],
        |row| row.get::<_, String>(0),
    ) {
        Ok(v) => Some(v),
        // A missing row means Go's `INSERT OR IGNORE` never ran. A missing *table* is a real error,
        // which the probe below distinguishes.
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => {
            let table_exists = conn
                .query_row(
                    "SELECT name FROM sqlite_master WHERE type='table' AND name='schema_meta';",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .is_ok();
            if !table_exists {
                return Err(CoreError::Sql {
                    msg: format!("schema_meta table is missing: {e}"),
                });
            }
            None
        }
    };
    Ok(raw
        .as_deref()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0))
}

/// `PRAGMA user_version`.
fn user_version(conn: &Connection) -> Result<u32, CoreError> {
    let v: i64 = conn
        .query_row("PRAGMA user_version;", [], |row| row.get(0))
        .map_err(|e| CoreError::Sql { msg: e.to_string() })?;
    Ok(u32::try_from(v).unwrap_or(0))
}

/// Check the database against the Go schema without creating anything.
///
/// An empty vector means the database has every table, every column and every index Go's `sqlite_master`
/// has — which is what `contract_history.rs` asserts of the golden `zen-man.db`.
///
/// Checks are structural (`PRAGMA table_index`/`index_list`), never textual: `docs/contracts.md` records
/// that Go's own DDL text survived into the golden with its tab indentation, but comparing whitespace
/// would make a cosmetic SQLite version change look like schema drift. Extra objects — the five FTS5
/// shadow tables (`files_fts_{content,docsize,idx,data,config}`) and the `sqlite_autoindex_*` entries —
/// are *not* issues; they are what FTS5 and primary keys create by themselves, and the golden has them.
///
/// # Errors
///
/// [`CoreError::Sql`] when the database cannot be read at all.
pub fn verify(conn: &Connection) -> Result<Vec<SchemaIssue>, CoreError> {
    let mut issues = Vec::new();
    let present = table_names(conn)?;
    for (table, expected) in TABLES {
        if !present.iter().any(|t| t == table) {
            issues.push(SchemaIssue::MissingTable(table.to_string()));
            continue;
        }
        match columns_of(conn, table) {
            Ok(found) => {
                for (name, ty, notnull, pk) in expected {
                    match found.iter().find(|c| &c.name == name) {
                        None => issues.push(SchemaIssue::Column {
                            table: table.to_string(),
                            column: (*name).to_string(),
                            expected: describe(*ty, *notnull, *pk),
                            found: "absent".to_string(),
                        }),
                        Some(col) if col.mismatch(*ty, *notnull, *pk) => {
                            issues.push(SchemaIssue::Column {
                                table: table.to_string(),
                                column: (*name).to_string(),
                                expected: describe(*ty, *notnull, *pk),
                                found: describe(&col.declared_type, col.notnull, col.pk),
                            });
                        }
                        Some(_) => {}
                    }
                }
            }
            Err(why) => issues.push(SchemaIssue::Unavailable {
                what: format!("columns of {table}"),
                why,
            }),
        }
    }
    // The FTS table: presence is required, its column list only when the module can be consulted.
    if !present.iter().any(|t| t == FTS_TABLE) {
        issues.push(SchemaIssue::MissingTable(FTS_TABLE.to_string()));
    } else if let Err(why) = columns_of(conn, FTS_TABLE) {
        // `no such module: fts5` on a build without it: the table is *there* (SQLite kept it in
        // `sqlite_master`), so this is reported rather than guessed at.
        issues.push(SchemaIssue::Unavailable {
            what: format!("columns of {FTS_TABLE}"),
            why,
        });
    }
    for (index, table, unique, cols) in INDEXES {
        if !present.iter().any(|t| t == table) {
            continue; // already reported
        }
        match index_info(conn, index, table) {
            None => issues.push(SchemaIssue::MissingIndex(index.to_string())),
            Some((found_unique, found_cols)) => {
                if found_unique != unique
                    || found_cols != cols.iter().map(|s| (*s).to_string()).collect::<Vec<_>>()
                {
                    issues.push(SchemaIssue::Index {
                        index: index.to_string(),
                        expected: format!(
                            "{}unique({})",
                            if unique { "" } else { "non" },
                            cols.join(",")
                        ),
                        found: format!(
                            "{}unique({})",
                            if found_unique { "" } else { "non" },
                            found_cols.join(",")
                        ),
                    });
                }
            }
        }
    }
    match version_of(conn) {
        Ok(v) if v == SCHEMA_VERSION => {}
        Ok(v) => issues.push(SchemaIssue::Version(Some(v.to_string()))),
        Err(_) => issues.push(SchemaIssue::Version(None)),
    }
    Ok(issues)
}

/// One `PRAGMA table_info` row.
#[derive(Clone, Debug)]
struct Column {
    name: String,
    declared_type: String,
    notnull: bool,
    pk: i32,
}

impl Column {
    fn mismatch(&self, ty: &str, notnull: bool, pk: i32) -> bool {
        self.declared_type != ty || self.notnull != notnull || self.pk != pk
    }
}

fn describe(ty: &str, notnull: bool, pk: i32) -> String {
    format!(
        "{ty}{}{}",
        if notnull { " NOT NULL" } else { "" },
        if pk > 0 { " PK" } else { "" }
    )
}

fn row_string(row: &Row<'_>, idx: usize) -> rusqlite::Result<String> {
    row.get::<_, String>(idx)
}

fn table_names(conn: &Connection) -> Result<Vec<String>, CoreError> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type IN ('table','view');")
        .map_err(|e| CoreError::Sql { msg: e.to_string() })?;
    let rows = stmt
        .query_map([], |row| row_string(row, 0))
        .map_err(|e| CoreError::Sql { msg: e.to_string() })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| CoreError::Sql { msg: e.to_string() })?);
    }
    Ok(out)
}

fn columns_of(conn: &Connection, table: &str) -> Result<Vec<Column>, String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table});"))
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            Ok(Column {
                name: row_string(row, 1).unwrap_or_default(),
                declared_type: row
                    .get::<_, Option<String>>(2)
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
                notnull: row.get::<_, i64>(3).unwrap_or(0) != 0,
                pk: row.get::<_, i64>(5).unwrap_or(0) as i32,
            })
        })
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| e.to_string())?);
    }
    if out.is_empty() {
        return Err("no columns reported".to_string());
    }
    Ok(out)
}

/// `(unique, columns)` of one index, or `None` when it does not exist.
fn index_info(conn: &Connection, index: &str, table: &str) -> Option<(bool, Vec<String>)> {
    let unique: i64 = conn
        .query_row(
            "SELECT \"unique\" FROM pragma_index_list(?1) WHERE name = ?2;",
            rusqlite::params![table, index],
            |row| row.get(0),
        )
        .ok()?;
    let mut stmt = conn
        .prepare("SELECT name FROM pragma_index_info(?1) ORDER BY seqno;")
        .ok()?;
    let rows = stmt.query_map([index], |row| row_string(row, 0)).ok()?;
    let mut cols = Vec::new();
    for r in rows {
        cols.push(r.ok()?);
    }
    Some((unique != 0, cols))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch("PRAGMA journal_mode = DELETE;")
            .expect("journal mode");
        conn
    }

    #[test]
    fn apply_creates_everything_and_verify_is_clean() {
        let conn = fresh();
        let report = apply(&conn).expect("apply");
        assert_eq!(report.version, SCHEMA_VERSION);
        assert_eq!(report.user_version, SCHEMA_VERSION);
        // Four declared tables plus the five FTS5 shadow tables (`files_fts_{data,idx,content,docsize,
        // config}`), which the golden has too — see `docs/contracts.md` §*SQLite storage*.
        assert_eq!(
            report.created_tables.len(),
            9,
            "{:?}",
            report.created_tables
        );
        for base in ["folder_sizes", "schema_meta", "files", "files_fts"] {
            assert!(report.created_tables.contains(&base.to_string()));
        }
        assert!(verify(&conn).expect("verify").is_empty());
        // Idempotent: applying twice changes nothing.
        let again = apply(&conn).expect("apply again");
        assert!(again.created_tables.is_empty());
        assert_eq!(again.version, SCHEMA_VERSION);
    }

    #[test]
    fn verify_names_every_missing_object() {
        let conn = fresh();
        // `verify` itself succeeds; it is the *content* of the vector that must be non-empty, since a
        // database with no schema at all has to name every missing object rather than stop at one.
        let issues = verify(&conn).expect("verify runs on an empty database");
        // Four tables missing + the version row: five issues, and each names what a user must fix.
        assert_eq!(issues.len(), 5, "{issues:?}");
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, SchemaIssue::MissingTable(t) if t == "files"))
        );
        assert!(matches!(issues.last(), Some(SchemaIssue::Version(None))));
    }

    #[test]
    fn verify_spots_a_wrong_column_and_a_missing_index() {
        let conn = fresh();
        conn.execute_batch(
            "CREATE TABLE folder_sizes (path TEXT PRIMARY KEY, size TEXT, file_count INTEGER, updated_at INTEGER);
             CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT);
             CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT NOT NULL, name TEXT NOT NULL,
                                 name_orig TEXT NOT NULL, ext TEXT NOT NULL, parent TEXT NOT NULL,
                                 size INTEGER NOT NULL, mtime INTEGER NOT NULL, is_dir INTEGER NOT NULL,
                                 is_symlink INTEGER NOT NULL);
             INSERT INTO schema_meta VALUES ('search_schema_version','1');",
        )
        .expect("hand-built schema");
        let issues = verify(&conn).expect("verify");
        // One wrong column type, the absent `files_fts`, and all five indexes Go would have made.
        assert_eq!(issues.len(), 7, "{issues:?}");
        assert!(
            !issues
                .iter()
                .any(|i| matches!(i, SchemaIssue::Column { column, .. } if column == "path")),
            "`TEXT PRIMARY KEY` is nullable in a rowid table, and must not be flagged"
        );
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, SchemaIssue::Column { column, .. } if column == "size"))
        );
        assert_eq!(
            issues
                .iter()
                .filter(|i| matches!(i, SchemaIssue::MissingIndex(_)))
                .count(),
            5
        );
    }

    #[test]
    fn version_of_reads_go_stamp_and_survives_a_weird_value() {
        let conn = fresh();
        conn.execute_batch(
            "CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT);
             INSERT INTO schema_meta VALUES ('search_schema_version','1');",
        )
        .expect("row");
        assert_eq!(version_of(&conn).expect("version"), 1);
        conn.execute("UPDATE schema_meta SET value='two';", [])
            .expect("bad value");
        assert_eq!(
            version_of(&conn).expect("unreadable version is 0, as Go's Atoi yields"),
            0
        );
    }

    #[test]
    fn the_ddl_text_is_the_go_source_not_a_rewriting() {
        // The golden's `sqlite_master` keeps Go's tab-indented body; ours must too, or the two
        // databases are observably different files.
        assert!(
            DDL_TABLES.contains("\n\t\tpath TEXT PRIMARY KEY,"),
            "{DDL_TABLES}"
        );
        assert!(DDL_TABLES.contains("tokenize='trigram'"));
        assert!(DDL_INDEXES[0].contains("idx_files_shadow_path"));
        assert!(DDL_INDEXES.iter().all(|s| s.contains("IF NOT EXISTS")));
    }
}
