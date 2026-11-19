//! The action-log service surface (plan §5.3/§5.8's `panels/action_log.rs` needs, §7 step 14a).
//!
//! Almost everything the undo panel wants already existed before this step, and this module's first job
//! is to say so rather than duplicate it:
//!
//! | Need | Where it already lives |
//!|---|---|
//! | the records themselves | [`Session::log_recent`] (step 6) and `ActionLogStore::records` (step 5) |
//! | one record by id | [`Session::log_record`] (step 6) |
//! | the reversible flag | [`ActionRecord::is_reversible`] / [`ActionType::is_reversible`] (step 5, Go's byte) |
//! | the "already reverted" marker | [`ActionRecord::status`] == [`LogStatus::REVERTED`] (step 5), written by `mark_reverted` |
//! | the per-record source list | `ActionRecord::sources` (step 5, the Go frame's field) |
//! | reverting | [`Session::revert`] (step 9c), a job whose report marks the record |
//! | the append event | [`EngineEvent::LogAppended`] (step 5), pushed by `Session::append_log` |
//!
//! What was missing is the four things a *panel* needs and a log file does not have: an offset window
//! ([`LogQuery`]/[`LogPage`], because the log is append-only and never truncated — plan R10, so "show me
//! rows 40..60" is the only paging there is), a per-row verdict that folds the two flags into one
//! enabled/disable decision ([`LogRow::can_revert`], which the UI must not re-derive from raw bytes), a
//! one-line human label (Go built its row text in `action-log.js`, not in the server), and the
//! newest-first order the panel paints in.
//!
//! # Ordering, and why it is a port decision
//!
//! Go's `/api/log` returned records in file order and `action-log.js:288` sliced the tail; the port
//! pages from the **newest** end (`offset: 0` is the most recent record) because that is the only window
//! a user can reach without knowing the record count, and because the panel's own refresh — a
//! [`EngineEvent::LogAppended`] while the panel is open — shifts a newest-first page by one row visibly
//! but shifts an oldest-first page invisibly. The deviation is listed in plan §6.2's ledger as of this
//! step.

use serde::Serialize;

use crate::history::codec::{ActionRecord, ActionType, LogStatus};
use crate::history::store::LogId;

/// A window over the undo log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogQuery {
    /// Rows to return; `None` means the panel default ([`DEFAULT_LIMIT`]).
    pub limit: Option<usize>,
    /// Rows to skip, counted from the newest.
    pub offset: usize,
    /// Keep only records that name this path in `dest` or in any source — Go's `/api/log?path=` filter
    /// (`api/log.go`), which the panel's "this folder only" toggle wants.
    pub path: Option<String>,
}

/// The default page size — Go's frontend fetched 100 rows per open (`action-log.js`).
pub const DEFAULT_LIMIT: usize = 100;

/// One panel row: the record, plus the three answers the widget asks about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LogRow {
    /// The record id (`/api/log/revert`'s key, and the value `LogAppended` carries).
    pub id: LogId,
    /// Go's `action_str` (`"PasteCopy"`, `"Rename"`, …).
    pub action: &'static str,
    /// Go's `status_str` (`"done"` / `"reverted"`).
    pub status: &'static str,
    /// RFC 3339 timestamp, Go's `time.Time` JSON shape.
    pub timestamp: String,
    /// Go's `timeAgo` bucket for this record (`metrics::format_relative_time`): `42s ago`, `5m ago`,
    /// `3h ago`, `9d ago`. Computed in core because `zenman-ui` has no `chrono` edge — the reason
    /// step 14b printed the RFC 3339 stamp instead, retired by step 15b for parity (defect D10).
    pub ago: String,
    /// The action type has an undo at all (`ActionType::is_reversible`).
    pub reversible: bool,
    /// The record was already undone.
    pub reverted: bool,
    /// `reversible && !reverted` — the row's button state, in one field.
    pub can_revert: bool,
    /// Go's `sources` array, verbatim (empty for `mkdir`, never absent).
    pub sources: Vec<String>,
    /// Go's `dest`.
    pub dest: String,
    /// Go's `name` — a new name, an octal mode string, or (for a batch) a JSON array.
    pub name: String,
    /// The one-line label: Go's `describeAction` equivalent, built here so the panel is a list of rows
    /// and not a template engine.
    pub label: String,
}

impl LogRow {
    /// Fold a record into a row.
    #[must_use]
    pub fn new(record: &ActionRecord) -> Self {
        let reversible = record.is_reversible();
        let reverted = record.status == LogStatus::REVERTED;
        Self {
            id: record.id,
            action: record.action_str(),
            status: record.status_str(),
            timestamp: record.timestamp.to_rfc3339(),
            ago: crate::metrics::format_relative_time(
                record.timestamp.timestamp(),
                chrono::Utc::now().timestamp(),
            ),
            reversible,
            reverted,
            can_revert: reversible && !reverted,
            sources: record.sources.clone(),
            dest: record.dest.clone(),
            name: record.name.clone(),
            label: label(record),
        }
    }
}

/// Go's row text (`frontend/js/action-log.js`'s per-action `switch`), ported rather than invented:
/// the same nouns, so a user comparing the two apps reads the same sentence.
#[must_use]
pub fn label(record: &ActionRecord) -> String {
    let count = record.sources.len();
    let base = match record.action {
        ActionType::COPY => format!("Copied {count} item(s)"),
        ActionType::PASTE_COPY => format!(
            "Pasted {} item(s) into {}",
            record.sources.len(),
            record.dest
        ),
        ActionType::PASTE_MOVE => format!(
            "Moved {} item(s) into {}",
            record.sources.len(),
            record.dest
        ),
        ActionType::PASTE_LINK => format!(
            "Linked {} item(s) into {}",
            record.sources.len(),
            record.dest
        ),
        ActionType::DELETE => format!("Deleted {} item(s)", record.sources.len()),
        ActionType::RENAME => match record.sources.first() {
            Some(from) => format!("Renamed {from} to {}", record.name),
            None => format!("Renamed to {}", record.name),
        },
        ActionType::MKDIR => format!("Created folder {} in {}", record.name, record.dest),
        ActionType::CHMOD => format!(
            "Changed {} item(s) to mode {}",
            record.sources.len(),
            record.name
        ),
        other => other.name().to_string(),
    };
    if record.status == LogStatus::REVERTED {
        format!("{base} (reverted)")
    } else {
        base
    }
}

/// A page of rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogPage {
    /// The window, newest first.
    pub rows: Vec<LogRow>,
    /// How many records the log holds *after* the path filter — the panel's "of N".
    pub total: usize,
    /// Older records exist beyond this page (Go's `hasMore`, which it never actually computed).
    pub has_more: bool,
}

impl LogPage {
    /// Slice `records` (file order, oldest first) into a newest-first page.
    #[must_use]
    pub fn new(records: &[ActionRecord], query: &LogQuery) -> Self {
        let filtered: Vec<&ActionRecord> = match query.path.as_deref() {
            None => records.iter().collect(),
            Some(path) => records
                .iter()
                .filter(|record| {
                    record.dest == path || record.sources.iter().any(|src| src == path)
                })
                .collect(),
        };
        let limit = query.limit.unwrap_or(DEFAULT_LIMIT).max(1);
        let total = filtered.len();
        let end = total.saturating_sub(query.offset);
        let start = end.saturating_sub(limit);
        let rows = filtered[start..end]
            .iter()
            .rev()
            .map(|record| LogRow::new(record))
            .collect::<Vec<_>>();
        Self {
            rows,
            total,
            has_more: start > 0,
        }
    }

    /// An empty page (an empty log).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            rows: Vec::new(),
            total: 0,
            has_more: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::codec::ActionType;
    use chrono::{TimeZone, Utc};

    fn record(action: ActionType, status: LogStatus, sources: &[&str]) -> ActionRecord {
        ActionRecord {
            id: 7,
            timestamp: Utc
                .timestamp_opt(1_700_000_000, 0)
                .single()
                .expect("in range"),
            action,
            status,
            sources: sources.iter().map(|s| (*s).to_string()).collect(),
            dest: "/d".to_string(),
            name: String::new(),
        }
    }

    #[test]
    fn a_reverted_record_is_not_revertible_twice() {
        let done = LogRow::new(&record(ActionType::PASTE_COPY, LogStatus::DONE, &["/a"]));
        assert!(done.can_revert && done.reversible && !done.reverted);
        let back = LogRow::new(&record(
            ActionType::PASTE_COPY,
            LogStatus::REVERTED,
            &["/a"],
        ));
        assert!(!back.can_revert, "reversible but already undone ⇒ disabled");
        assert!(back.reverted);
        assert!(back.label.ends_with("(reverted)"));
        let copy = LogRow::new(&record(ActionType::COPY, LogStatus::DONE, &["/a"]));
        assert!(
            !copy.can_revert,
            "a plain copy has no undo, Go parity (`is_reversible`)"
        );
    }

    #[test]
    fn paging_is_newest_first_and_reports_whether_more_exists() {
        let records: Vec<ActionRecord> = (0..10)
            .map(|i| {
                let mut r = record(ActionType::MKDIR, LogStatus::DONE, &[]);
                r.id = i;
                r
            })
            .collect();
        let page = LogPage::new(
            &records,
            &LogQuery {
                limit: Some(4),
                offset: 0,
                path: None,
            },
        );
        assert_eq!(page.rows.len(), 4);
        assert_eq!(page.rows[0].id, 9, "the newest record leads");
        assert!(page.has_more);
        assert_eq!(page.total, 10);
        let last = LogPage::new(
            &records,
            &LogQuery {
                limit: Some(4),
                offset: 8,
                path: None,
            },
        );
        assert_eq!(last.rows.len(), 2);
        assert!(!last.has_more);
        let none = LogPage::new(&[], &LogQuery::default());
        assert_eq!(none.total, 0);
        assert!(!none.has_more);
    }

    #[test]
    fn the_path_filter_keeps_records_that_name_it() {
        let records = vec![
            record(ActionType::DELETE, LogStatus::DONE, &["/a/x"]),
            record(ActionType::MKDIR, LogStatus::DONE, &[]),
        ];
        let page = LogPage::new(
            &records,
            &LogQuery {
                path: Some("/a/x".to_string()),
                ..LogQuery::default()
            },
        );
        assert_eq!(page.total, 1);
        assert!(page.rows[0].label.starts_with("Deleted"));
    }
}
