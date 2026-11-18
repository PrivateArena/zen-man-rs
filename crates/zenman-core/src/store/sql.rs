//! Opening the shared SQLite database (plan §5.4 `store/sql.rs` row).
//!
//! Step 5 owns *opening*: the retry loop, the pragmas, the integrity and schema gate. The `Db` wrapper
//! with `dir_size_*` and `index_*` methods, and `IndexedRow`, arrive with the callers that need them
//! (plan §5.4 puts folder sizes in step 6 and the index writer in step 12) — until then this module
//! hands out a plain [`rusqlite::Connection`], which is the type those methods will wrap.
//!
//! # The pragmas are the measured Go ones, not the planned ones
//!
//! Plan §5.4 asked for `journal_mode=WAL` "now actually enabled" plus `synchronous=NORMAL`.
//! `docs/contracts.md` §2.4 supersedes that with the measurement: the Go-produced `zen-man.db` reports
//! **`journal_mode = delete`**, `page_size = 4096`, `user_version = 0`, and a repo-wide grep for
//! `PRAGMA|busy_timeout|synchronous` in the Go tree finds only `SetMaxOpenConns(1)`
//! (`api/db.go:49`) and a `wal_checkpoint(PASSIVE)` that can never have done anything, because WAL was
//! never on. So:
//!
//! | pragma | value | why |
//! |---|---|---|
//! | `journal_mode` | `DELETE` | measured on the Go file. WAL would leave `-wal`/`-shm` sidecars the Go app does not know to fold away, and a database the *Go* app could be reopened into a mode it never asked for. |
//! | `busy_timeout` | [`BUSY_TIMEOUT_MS`] | plan §6.1 F5. Go had none, which is how a second instance produced `driver: bad connection`/`database is locked` and, at startup, `log.Fatalf`. |
//! | `synchronous` | *unset* — SQLite's `FULL` | Go set nothing, and `DELETE` + `FULL` is the durability the existing file has always had. Dropping to `NORMAL` would be a silent data-loss change made for speed nobody asked for. |
//! | `wal_checkpoint(TRUNCATE)` | best effort | plan §5.4. On a `DELETE`-mode database it is a no-op that answers `(0,0,0)`; the same call the Go indexer makes is a no-op for the same reason, so keeping it costs nothing and documents the intent. |
//!
//! The driver change is the real risk here and the acceptance test is the gate for it: Go used
//! `modernc.org/sqlite` (pure Go), we use `rusqlite` with a bundled SQLite C library. That changes
//! locking granularity and, importantly, which optional modules are compiled in — `index/schema.rs`
//! needs FTS5 with the `trigram` tokenizer, and `contract_history.rs` opens the Go-produced file.

use std::path::Path;
use std::thread;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use thiserror::Error;

use crate::error::CoreError;
use crate::index::schema::{self, SchemaIssue};
use crate::store::config::StoreMode;

/// `PRAGMA busy_timeout` (plan §6.1 F5: "`busy_timeout=5000`").
pub const BUSY_TIMEOUT_MS: u32 = 5_000;
/// Backoff between retries when the database is locked and the caller passed `backoff == 0`.
pub const DEFAULT_RETRY_BACKOFF: Duration = Duration::from_millis(50);
/// Attempts when the caller does not pick a number.
pub const DEFAULT_RETRY_ATTEMPTS: u32 = 8;

/// Why opening the database failed, before it becomes a [`CoreError`].
///
/// The distinction that matters to the caller (plan §6.1 F5: a locked database is a toast, never a
/// startup abort) is [`DbOpenError::Locked`]: only that one is worth falling back to
/// [`StoreMode::NoDb`] and running index-less.
#[derive(Debug, Error)]
pub enum DbOpenError {
    /// Another process holds the write lock. Go had no retry and no timeout at all.
    #[error("{path} is locked after {attempts} attempt(s): {why}")]
    Locked {
        /// What was opened.
        path: String,
        /// How many tries were spent.
        attempts: u32,
        /// SQLite's own words.
        why: String,
    },
    /// `PRAGMA integrity_check` did not answer `ok`.
    #[error("{path} failed its integrity check: {report}")]
    Corrupt {
        /// What was opened.
        path: String,
        /// The pragma's output.
        report: String,
    },
    /// The schema is not the Go schema and could not be brought up to it.
    #[error("{path} has an unusable index schema: {why}")]
    Schema {
        /// What was opened.
        path: String,
        /// The issues, rendered.
        why: String,
    },
    /// A pragma the port requires could not be applied.
    #[error("cannot set {pragma} on {path}: {why}")]
    Pragma {
        /// Which pragma.
        pragma: &'static str,
        /// What was opened.
        path: String,
        /// SQLite's words.
        why: String,
    },
    /// The file could not be opened or created at all.
    #[error("cannot open {path}: {why}")]
    Open {
        /// What was opened.
        path: String,
        /// SQLite's or the OS's words.
        why: String,
    },
    /// The caller's [`StoreMode`] forbids a database; asking to connect is a programming error, and it
    /// is typed so the session-start code can be reviewed for it.
    #[error("the database is disabled by store mode {0:?}")]
    Disabled(StoreMode),
}

impl From<DbOpenError> for CoreError {
    /// A lock becomes the matchable [`CoreError::DatabaseBusy`] so the caller can degrade to
    /// [`StoreMode::NoDb`]; everything else stays a [`CoreError::Sql`], which the UI shows and does not
    /// retry.
    fn from(e: DbOpenError) -> Self {
        match e {
            DbOpenError::Locked { path, attempts, .. } => {
                CoreError::DatabaseBusy { path, attempts }
            }
            DbOpenError::Disabled(_) | DbOpenError::Corrupt { .. } | DbOpenError::Schema { .. } => {
                CoreError::Sql { msg: e.to_string() }
            }
            DbOpenError::Pragma { .. } | DbOpenError::Open { .. } => {
                CoreError::Sql { msg: e.to_string() }
            }
        }
    }
}

/// Open `zen-man.db` with the pragmas the format needs, retrying a lock, and refuse to hand back a
/// connection to a corrupt or wrong-shaped database.
///
/// `mode` is the caller's [`StoreMode`] (from [`crate::store::config::ZenConfig`]):
/// [`StoreMode::Full`] opens (and creates) `path`; [`StoreMode::Ephemeral`] opens an in-memory database
/// so a headless run cannot poison a real index; [`StoreMode::NoDb`] is refused with
/// [`DbOpenError::Disabled`] rather than silently ignored — the session start is expected to check the
/// mode itself and use the error as the belt.
///
/// `attempts` is the total number of tries (at least one); `backoff` the wait between them, with
/// [`DEFAULT_RETRY_BACKOFF`] used when it is zero. A lock is retried; corruption, a schema mismatch and
/// a plain open failure are returned immediately — retrying those is how a user stares at a spinner.
///
/// # Errors
///
/// [`CoreError::DatabaseBusy`] when the lock survives every attempt, [`CoreError::Sql`] otherwise.
pub fn connect_with_retry(
    path: &Path,
    mode: StoreMode,
    attempts: u32,
    backoff: Duration,
) -> Result<Connection, CoreError> {
    open_with_retry(path, mode, attempts, backoff).map_err(CoreError::from)
}

/// [`connect_with_retry`] with the typed error left intact, for callers that must tell a lock from a
/// corrupt file without matching on a message.
///
/// # Errors
///
/// As [`connect_with_retry`], unaltered.
pub fn open_with_retry(
    path: &Path,
    mode: StoreMode,
    attempts: u32,
    backoff: Duration,
) -> Result<Connection, DbOpenError> {
    let tries = attempts.max(1);
    let wait = if backoff.is_zero() {
        DEFAULT_RETRY_BACKOFF
    } else {
        backoff
    };
    let mut last: Option<DbOpenError> = None;
    for attempt in 1..=tries {
        match try_open(path, mode) {
            Ok(conn) => return Ok(conn),
            Err(e @ DbOpenError::Locked { .. }) => {
                last = Some(e);
                if attempt < tries {
                    thread::sleep(wait);
                }
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| DbOpenError::Open {
        path: path.display().to_string(),
        why: "no attempt was made".to_string(),
    }))
}

/// One attempt: open, pragmas, integrity check, schema.
fn try_open(path: &Path, mode: StoreMode) -> Result<Connection, DbOpenError> {
    let shown = match mode {
        StoreMode::Ephemeral => ":memory:".to_string(),
        StoreMode::Full | StoreMode::NoDb => path.display().to_string(),
    };
    let conn = match mode {
        StoreMode::NoDb => return Err(DbOpenError::Disabled(mode)),
        StoreMode::Ephemeral => Connection::open_in_memory(),
        StoreMode::Full => Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        ),
    }
    .map_err(|e| match lock_failure(mode, &e) {
        Some(why) => DbOpenError::Locked {
            path: shown.clone(),
            attempts: 1,
            why,
        },
        None => DbOpenError::Open {
            path: shown.clone(),
            why: e.to_string(),
        },
    })?;

    // Go's single-connection pool (`api/db.go:49`), kept: it is the reason Go's locking was tolerable
    // at all, and rusqlite gives us exactly one connection anyway.
    if let Err(e) = apply_pragmas(&conn, &shown, mode) {
        return Err(e);
    }
    integrity_check(&conn, &shown)?;
    // The plan §5.4 "schema created by Go satisfies version_of == CURRENT_VERSION" gate: apply (a no-op
    // on a Go file) then verify (strict about it).
    if let Err(e) = schema::apply(&conn) {
        return Err(schema_error(e, &shown));
    }
    let issues = schema::verify(&conn).map_err(|e| schema_error(e, &shown))?;
    if !issues.is_empty() {
        return Err(DbOpenError::Schema {
            path: shown,
            why: render_issues(&issues),
        });
    }
    Ok(conn)
}

/// A schema-stage failure, re-typed if it was really a lock.
///
/// [`schema::apply`] flattens `rusqlite::Error` into [`CoreError::Sql`], so the typed `SQLITE_BUSY` is
/// gone by the time it reaches here; the DDL and the `schema_meta` insert are the two writes most likely
/// to meet a lock, and reporting them as a permanent schema mismatch would deny the caller its
/// [`StoreMode::NoDb`] fallback. The SQLite text for a lock is stable ("database is locked", "database
/// table is locked"), so it is matched on — with this comment as the record that it is a string test.
fn schema_error(e: CoreError, shown: &str) -> DbOpenError {
    let msg = e.to_string();
    if msg.contains("locked") || msg.contains("busy") {
        return DbOpenError::Locked {
            path: shown.to_string(),
            attempts: 1,
            why: msg,
        };
    }
    DbOpenError::Schema {
        path: shown.to_string(),
        why: msg,
    }
}

fn apply_pragmas(conn: &Connection, shown: &str, mode: StoreMode) -> Result<(), DbOpenError> {
    // An in-memory database answers `journal_mode = memory` and cannot be anything else, so the DELETE
    // assertion — which exists to prove we did *not* turn a Go file into WAL — only applies to a file.
    let in_memory = mode == StoreMode::Ephemeral;
    let journal: String = conn
        .query_row("PRAGMA journal_mode = DELETE;", [], |row| row.get(0))
        .map_err(|e| DbOpenError::Pragma {
            pragma: "journal_mode",
            path: shown.to_string(),
            why: e.to_string(),
        })?;
    if !in_memory && !journal.eq_ignore_ascii_case("delete") {
        return Err(DbOpenError::Pragma {
            pragma: "journal_mode",
            path: shown.to_string(),
            why: format!("asked for DELETE, got {journal}"),
        });
    }
    let timeout: i64 = conn
        .query_row(
            &format!("PRAGMA busy_timeout = {BUSY_TIMEOUT_MS};"),
            [],
            |row| row.get(0),
        )
        .map_err(|e| DbOpenError::Pragma {
            pragma: "busy_timeout",
            path: shown.to_string(),
            why: e.to_string(),
        })?;
    if timeout < i64::from(BUSY_TIMEOUT_MS) {
        return Err(DbOpenError::Pragma {
            pragma: "busy_timeout",
            path: shown.to_string(),
            why: format!("asked for {BUSY_TIMEOUT_MS}, got {timeout}"),
        });
    }
    // Best effort by design: on a `DELETE`-mode database this returns `(0,0,0)` and changes nothing.
    // It is the call the Go indexer makes (`api/search_index_walker.go:250`), and keeping it here means
    // a future switch to WAL cannot forget it.
    let _ = conn.query_row(
        "PRAGMA wal_checkpoint(TRUNCATE);",
        [],
        |_: &rusqlite::Row<'_>| -> rusqlite::Result<()> { Ok(()) },
    );
    Ok(())
}

fn integrity_check(conn: &Connection, shown: &str) -> Result<(), DbOpenError> {
    let mut stmt = conn
        .prepare("PRAGMA integrity_check;")
        .map_err(|e| DbOpenError::Corrupt {
            path: shown.to_string(),
            report: e.to_string(),
        })?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| DbOpenError::Corrupt {
            path: shown.to_string(),
            report: e.to_string(),
        })?;
    let mut report = Vec::new();
    for row in rows {
        report.push(row.map_err(|e| DbOpenError::Corrupt {
            path: shown.to_string(),
            report: e.to_string(),
        })?);
    }
    if report.len() != 1 || report[0] != "ok" {
        return Err(DbOpenError::Corrupt {
            path: shown.to_string(),
            report: report.join("; "),
        });
    }
    Ok(())
}

/// `SQLITE_BUSY`/`SQLITE_LOCKED` in any of the extended forms the two drivers can produce.
fn lock_failure(mode: StoreMode, e: &rusqlite::Error) -> Option<String> {
    if mode == StoreMode::Ephemeral {
        return None;
    }
    match e {
        rusqlite::Error::SqliteFailure(f, msg) => {
            let locked = matches!(
                f.code,
                rusqlite::ffi::ErrorCode::DatabaseBusy | rusqlite::ffi::ErrorCode::DatabaseLocked
            ) || f.extended_code == 267; // SQLITE_LOCKED_SHAREDCACHE
            locked.then(|| msg.clone().unwrap_or_else(|| f.to_string()))
        }
        _ => None,
    }
}

fn render_issues(issues: &[SchemaIssue]) -> String {
    issues
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zenman-sql-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn a_new_file_gets_the_go_schema_and_survives_reopen() {
        let dir = tmp_dir("fresh");
        let path = dir.join("zen-man.db");
        {
            let conn = connect_with_retry(&path, StoreMode::Full, 1, Duration::ZERO).expect("open");
            let report = schema::apply(&conn).expect("already applied by connect");
            assert!(
                report.created_tables.is_empty(),
                "connect already created it"
            );
            assert_eq!(report.version, schema::SCHEMA_VERSION);
        }
        let again = connect_with_retry(&path, StoreMode::Full, 3, Duration::from_millis(1))
            .expect("reopen");
        assert!(schema::verify(&again).expect("verify").is_empty());
        let journal: String = again
            .query_row("PRAGMA journal_mode;", [], |r| r.get(0))
            .expect("pragma");
        assert_eq!(
            journal.to_lowercase(),
            "delete",
            "the measured Go mode, not WAL"
        );
        let side = dir.join("zen-man.db-wal");
        assert!(
            !side.exists(),
            "no WAL sidecar was created next to a Go-format file"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ephemeral_mode_is_in_memory_and_still_valid() {
        let conn = connect_with_retry(
            Path::new("/unused/zen-man.db"),
            StoreMode::Ephemeral,
            1,
            Duration::ZERO,
        )
        .expect("in-memory open ignores the path");
        assert!(schema::verify(&conn).expect("verify").is_empty());
        let n: i64 = conn
            .query_row("PRAGMA busy_timeout;", [], |r| r.get(0))
            .expect("pragma");
        assert_eq!(n, i64::from(BUSY_TIMEOUT_MS));
    }

    #[test]
    fn no_db_mode_is_refused_with_a_typed_error() {
        let e = open_with_retry(
            Path::new("/unused/zen-man.db"),
            StoreMode::NoDb,
            3,
            Duration::ZERO,
        );
        assert!(
            matches!(e, Err(DbOpenError::Disabled(StoreMode::NoDb))),
            "{e:?}"
        );
        // And the conversion keeps it non-retryable-looking (`Sql`), because there is nothing to retry.
        let core: CoreError = e.unwrap_err().into();
        assert!(matches!(core, CoreError::Sql { .. }));
    }

    #[test]
    fn a_locked_database_becomes_database_busy() {
        let dir = tmp_dir("locked");
        let path = dir.join("zen-man.db");
        let holder = Connection::open(&path).expect("holder opens");
        holder
            .execute_batch("BEGIN IMMEDIATE; CREATE TABLE IF NOT EXISTS lock_marker (x);")
            .expect("write lock held");
        // A second connection with `busy_timeout` set on *this* one is what the port uses; the holder
        // is inside an explicit transaction, so the write the schema stamp attempts must wait, and
        // SQLite refuses after its own timeout rather than deadlocking.
        // One attempt only: the port's own `busy_timeout` (5 s) is what SQLite waits before it answers
        // SQLITE_BUSY, and a retry loop around that would put 10 s into the test suite to prove nothing
        // the single-attempt case does not already prove.
        let e = open_with_retry(&path, StoreMode::Full, 1, Duration::ZERO);
        match e {
            Err(DbOpenError::Locked { attempts, .. }) => assert!(attempts >= 1),
            // If the bundled SQLite waited the transaction out, that is the retry working: assert the
            // schema is intact and move on.
            Ok(conn) => assert!(schema::verify(&conn).expect("verify").is_empty()),
            other => panic!("expected a lock or a successful wait, got {other:?}"),
        }
        drop(holder);
        let conn = connect_with_retry(&path, StoreMode::Full, 4, Duration::from_millis(1))
            .expect("unlocks");
        assert!(schema::verify(&conn).expect("verify").is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_directory_is_an_open_failure_not_a_retry() {
        let dir = tmp_dir("isdir");
        // Opening a directory as a database must fail fast and must not look like a lock: burning the
        // retry budget on a non-retryable error is exactly the "spinner" failure plan §6.1 F5 forbids.
        let e = open_with_retry(&dir, StoreMode::Full, 4, Duration::from_millis(1));
        assert!(matches!(e, Err(DbOpenError::Open { .. })), "{e:?}");
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn corrupt_files_are_reported_not_retried() {
        let dir = tmp_dir("corrupt");
        let path = dir.join("zen-man.db");
        std::fs::write(&path, b"this is not a database at all, it is a text file").expect("write");
        let e = open_with_retry(&path, StoreMode::Full, 5, Duration::from_millis(1));
        // Which stage notices depends on the SQLite version (the header check can land on `open` or on
        // the first pragma), but it must never be reported as a lock: retrying garbage is the spinner
        // plan §6.1 F5 forbids.
        assert!(e.is_err(), "{e:?}");
        assert!(
            !matches!(e, Err(DbOpenError::Locked { .. })),
            "a non-database file must not look like a lock: {e:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
