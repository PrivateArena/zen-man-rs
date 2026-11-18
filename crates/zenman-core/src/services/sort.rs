//! The one listing order (plan §5.3 `ops/list.rs::sort_entries`, §5.8's local re-sort).
//!
//! Go's `sortEntries` (`api/dir.go:292-320`) is the reference, and it has two properties the port
//! must keep and one it must not:
//!
//! * **directories first under both orders** — the dirs-first arm of the comparator is not
//!   inverted by `sortOrder`, so `dir-sort-date-desc.json` still starts with the directories;
//! * **name is a byte compare of the lowercased name**, so `a10.txt < a9.txt`
//!   (`dir-sortcase-asc.json`);
//! * the third property is Go's *bug*: `sort.Slice` is unstable, so two rows with equal keys have no
//!   defined order and a page boundary can show the same file twice. The tie-break on the raw name
//!   makes the order total, which is what lets a re-sort of the loaded rows agree with a re-fetch.
//!
//! It lives in the engine, not in the pane, because the pane must produce *exactly* the order the
//! engine would: the header click re-sorts the loaded rows without a refetch (plan §5.8's
//! Done-when), and a second implementation is how those two drift apart.

use crate::entry::FileEntry;
use crate::metrics::GO_ZERO_TIME_UNIX;
use crate::store::config::{SortColumn, SortSpec};

/// The four facts the ordering reads. Anything that can be listed — a pre-projection `Row`, a
/// wire [`FileEntry`], an archive member — implements this and gets the one order for free.
pub trait SortRow {
    /// The lowercased name the compare uses (plan §2.3: byte compare, so `a10 < a9`).
    fn sort_name(&self) -> String;
    /// The raw name, the tie-break that makes the order total.
    fn raw_name(&self) -> &str;
    /// `true` for a directory (and, per plan F5, for a link to one).
    fn is_dir(&self) -> bool;
    /// Unpacked size for [`SortColumn::Size`].
    fn sort_size(&self) -> i64;
    /// Modification seconds for [`SortColumn::Date`]; [`GO_ZERO_TIME_UNIX`] when unknown.
    fn sort_mtime(&self) -> i64;
}

impl SortRow for FileEntry {
    fn sort_name(&self) -> String {
        self.name.to_lowercase()
    }

    fn raw_name(&self) -> &str {
        &self.name
    }

    fn is_dir(&self) -> bool {
        self.is_dir
    }

    fn sort_size(&self) -> i64 {
        self.size
    }

    fn sort_mtime(&self) -> i64 {
        self.mtime_secs().unwrap_or(GO_ZERO_TIME_UNIX)
    }
}

/// Sort `rows` into the listing order `spec` names.
pub fn sort_by<T: SortRow>(rows: &mut [T], spec: SortSpec) {
    rows.sort_by(|a, b| {
        a.is_dir()
            .cmp(&b.is_dir())
            .reverse()
            .then_with(|| match spec.column {
                SortColumn::Size => a.sort_size().cmp(&b.sort_size()),
                SortColumn::Date => a.sort_mtime().cmp(&b.sort_mtime()),
                // Go's `default` arm is the name compare, and `mode`/`ext` never reach a
                // `sortableEntry` (it has no mode or extension accessor): both sort by name, as
                // `api/dir.go:315` does.
                SortColumn::Name | SortColumn::Mode | SortColumn::Ext => {
                    a.sort_name().cmp(&b.sort_name())
                }
            })
            // The stability fix: equal keys keep a defined order across pages and re-sorts.
            .then_with(|| a.raw_name().cmp(b.raw_name()))
    });
    if spec.descending {
        // Reverse *within* each class, so dirs-first survives both directions exactly as Go's
        // non-inverted first arm does (`dir-sort-date-desc.json` starts with the directory too).
        // The ascending pass already partitioned the rows, so the class boundary is the first
        // non-directory — which is why this needs neither `Clone` nor an allocation.
        let split = rows
            .iter()
            .position(|row| !row.is_dir())
            .unwrap_or(rows.len());
        rows[..split].reverse();
        rows[split..].reverse();
    }
}

/// The listing order of wire rows — what the pane calls when a header is clicked.
pub fn sort_entries(entries: &mut [FileEntry], spec: SortSpec) {
    sort_by(entries, spec);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::unix_to_datetime;

    fn row(name: &str, is_dir: bool, size: i64, mtime: i64) -> FileEntry {
        FileEntry {
            name: name.into(),
            is_dir,
            is_symlink: false,
            size,
            mod_time: unix_to_datetime(mtime),
            mode: if is_dir { "drwxr-xr-x" } else { "-rw-r--r--" }.into(),
            rel_path: String::new(),
            depth: 0,
            files_count: None,
        }
    }

    fn names(rows: &[FileEntry]) -> Vec<&str> {
        rows.iter().map(|r| r.name.as_str()).collect()
    }

    /// The fixture tree's `SortCase` directory, whose two captures pin the case rule.
    #[test]
    fn name_sort_is_a_byte_compare_of_the_lowercase() {
        let mut rows = vec![
            row("a9.txt", false, 3, 0),
            row("a10.txt", false, 4, 0),
            row("B.txt", false, 2, 0),
            row("b10.txt", false, 2, 0),
        ];
        sort_entries(&mut rows, SortSpec::DEFAULT_SORT);
        // `a10` before `a9` is Go's byte compare, and the `dir-sortcase-asc.json` shape.
        assert_eq!(names(&rows), ["a10.txt", "a9.txt", "B.txt", "b10.txt"]);
        // …and descending reverses it (`dir-sortcase-desc.json`).
        sort_entries(
            &mut rows,
            SortSpec {
                column: SortColumn::Name,
                descending: true,
            },
        );
        assert_eq!(names(&rows), ["b10.txt", "B.txt", "a9.txt", "a10.txt"]);
    }

    #[test]
    fn directories_stay_first_in_both_orders() {
        let mut rows = vec![
            row("zeta", true, 100, 5),
            row("alpha.txt", false, 1, 9),
            row("mid", true, 10, 1),
            row("beta.txt", false, 50, 3),
        ];
        for descending in [false, true] {
            sort_entries(
                &mut rows,
                SortSpec {
                    column: SortColumn::Size,
                    descending,
                },
            );
            let dirs_first = rows.iter().position(|r| !r.is_dir).unwrap_or(rows.len());
            assert!(
                rows[..dirs_first].iter().all(|r| r.is_dir),
                "descending={descending}: {:?}",
                names(&rows)
            );
            assert!(
                rows[dirs_first..].iter().all(|r| !r.is_dir),
                "descending={descending}: {:?}",
                names(&rows)
            );
        }
        // Ascending by size: 10 then 100 for the dirs, 1 then 50 for the files.
        sort_entries(
            &mut rows,
            SortSpec {
                column: SortColumn::Size,
                descending: false,
            },
        );
        assert_eq!(names(&rows), ["mid", "zeta", "alpha.txt", "beta.txt"]);
        sort_entries(
            &mut rows,
            SortSpec {
                column: SortColumn::Size,
                descending: true,
            },
        );
        assert_eq!(names(&rows), ["zeta", "mid", "beta.txt", "alpha.txt"]);
    }

    #[test]
    fn date_sort_reads_the_mtime_and_unknowns_come_first() {
        let mut rows = vec![
            row("later", false, 1, 1_000),
            row("earlier", false, 1, 10),
            row("unknown", false, 1, GO_ZERO_TIME_UNIX),
        ];
        sort_entries(
            &mut rows,
            SortSpec {
                column: SortColumn::Date,
                descending: false,
            },
        );
        assert_eq!(names(&rows), ["unknown", "earlier", "later"]);
    }

    #[test]
    fn equal_date_keys_keep_one_tie_order_under_both_input_orders() {
        // The determinism the pane depends on: the tie order is a function of the *rows*, never of
        // the order they arrived in. `sort_by` (a **stable** sort — `sort_unstable_by` would leave
        // this to the pattern) plus the raw-name tie-break is what stops a directory listing from
        // reshuffling between two frames, or a page boundary from repeating a row.
        let build = || {
            vec![
                row("zeta.txt", false, 1, 500),
                row("Alpha.txt", false, 1, 500),
                row("mid.txt", false, 1, 500),
                // Same lowercase name as the first row: the tie `sort_name` cannot separate, and
                // the one a date sort over equal mtimes reaches the tie-break for.
                row("Zeta.txt", false, 1, 500),
            ]
        };
        let spec = SortSpec {
            column: SortColumn::Date,
            descending: false,
        };
        let mut forward = build();
        let mut backward = build();
        backward.reverse();
        sort_entries(&mut forward, spec);
        sort_entries(&mut backward, spec);
        assert_eq!(
            names(&forward),
            names(&backward),
            "the tie order does not depend on the input order"
        );
        assert_eq!(
            names(&forward),
            ["Alpha.txt", "Zeta.txt", "mid.txt", "zeta.txt"],
            "raw-name byte compare, so the tie order is stated exactly"
        );
    }

    #[test]
    fn equal_keys_have_one_order() {
        // The instability fix: two rows with the same size sort the same way every time, which is
        // what makes a page boundary unable to repeat or drop a row.
        let build = || {
            vec![
                row("zzz", false, 7, 0),
                row("aaa", false, 7, 0),
                row("mmm", false, 7, 0),
            ]
        };
        let mut left = build();
        let mut right = build();
        let spec = SortSpec {
            column: SortColumn::Size,
            descending: false,
        };
        sort_entries(&mut left, spec);
        right.reverse();
        sort_entries(&mut right, spec);
        assert_eq!(names(&left), names(&right));
        assert_eq!(names(&left), ["aaa", "mmm", "zzz"]);
    }
}
