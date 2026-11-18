//! Sizes and dates — the two formatting decisions the Go frontend got wrong or duplicated (plan §5.2
//! `metrics.rs` row).
//!
//! [`FileSizes`] is the type that makes `(undefined files)` impossible: the field Go dropped with
//! `omitempty` (`api/dir.go:250-257`; `docs/contracts.md` §2.4 *`files_count`*) becomes an `Option`
//! that is always serialised, so "we do not know" is representable and distinct from `0`.
//!
//! Dates go through `chrono` only (house rule: *all dates through chrono*, `/tmp/craft-libs/AGENTS.md`,
//! quoted in plan §2.1). Encoding is UTC, per `docs/contracts.md` §2.4 row *Log timestamps* ("decode
//! accepts any offset; encode in UTC").

use chrono::TimeZone as _;
use serde::Serialize;

/// A folder-size answer: bytes, and how many files that byte count was computed over.
///
/// `files` is `Option` because the Go cache stores `folder_sizes(path, size, file_count)` and can
/// legitimately have no row (plan §5.2 *Done when*: `FileSizes{bytes:4096, files:None}` serialises to
/// `{"bytes":4096}`); the UI renders `--` for `None`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct FileSizes {
    /// Total bytes.
    pub bytes: u64,
    /// File count, `None` when no `folder_sizes` row exists.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub files: Option<u64>,
}

/// A byte count that knows how to introduce itself.
///
/// A newtype rather than `impl fmt::Display for u64` (impossible anyway, orphan rule) so the table
/// lives in exactly one place — Go had `formatSize` twice in the frontend (plan §2.3, *Known frontend
/// bugs*).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ByteSize(pub u64);

/// The units above bytes. `KiB` is binary (1024), which is what the divisor is; the plan's table names
/// the rest without a binary prefix and the port keeps that spelling rather than silently changing the
/// UI text later.
const UNITS: [&str; 5] = ["KiB", "MB", "GB", "TB", "PB"];

impl ByteSize {
    /// Human-readable size: a plain integer count of bytes below one kibibyte, otherwise one decimal
    /// place with a unit suffix, capped at `PB`.
    ///
    /// Shape reference: `frontend/js/utils.js` `formatSize` (the only size formatter in the Go app;
    /// plan §5.2 cites `utils.js:23-32`, which in this checkout is `positionElementSmartly` — the
    /// function is at `:1-7`, citation drift of the same kind as `docs/contracts.md` → *Known
    /// citation drift*). Go's table is `['B','KB','MB','GB','TB']` at two decimals; the plan fixes
    /// this port's table to `B` + one decimal `KiB/MB/GB/TB/PB`, and that table is what
    /// `metrics::tests::humanize_matches_go` pins.
    #[must_use]
    pub fn humanize(self) -> String {
        if self.0 < 1024 {
            return format!("{} B", self.0);
        }
        let bytes = self.0 as f64;
        let mut exp = 0usize;
        let mut scaled = bytes;
        while scaled >= 1024.0 && exp < UNITS.len() {
            scaled /= 1024.0;
            exp += 1;
        }
        // `exp` is the number of divisions, so it is at least 1 here (the value is >= 1024) and at most
        // `UNITS.len()` (the loop guard): `UNITS[exp - 1]` therefore spans exactly `KiB..=PB`, which also
        // gives the PB cap for free — anything past 1024 PB stays in `PB` with a large mantissa.
        format!("{scaled:.1} {}", UNITS[exp - 1])
    }
}

impl std::fmt::Display for ByteSize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.humanize())
    }
}

/// Go's zero `time.Time` in Unix seconds: `-62135596800`, observed as the `mod_time` of every degraded
/// row in `dir-perm-noexec.json` (`docs/contracts.md` → *fileMode strings*).
pub const GO_ZERO_TIME_UNIX: i64 = -62_135_596_800;

/// `true` for a timestamp Go produced when `entry.Info()` failed, i.e. "no mtime at all"
/// (plan §2.4 row *Permission errors*, plan F20).
#[must_use]
pub fn is_go_zero_time(unix: i64) -> bool {
    unix == GO_ZERO_TIME_UNIX
}

/// Modification time as `YYYY-MM-DD HH:MM` in UTC, or `--` when unknown.
///
/// The `--` is Go's own: `frontend/js/utils.js:9-10` `if (!timestamp) return '--'`.
#[must_use]
pub fn format_mod_time(unix: Option<i64>) -> String {
    format_mod_time_offset(unix, chrono::FixedOffset::east_opt(0))
}

/// Same as [`format_mod_time`] in the machine's local zone — what a file row shows. UTC is the tested
/// one; this one is the one a user reads.
#[must_use]
pub fn format_mod_time_local(unix: Option<i64>) -> String {
    let local = *chrono::Local::now().offset();
    format_mod_time_offset(unix, Some(local))
}

fn format_mod_time_offset(unix: Option<i64>, offset: Option<chrono::FixedOffset>) -> String {
    let Some(dt) = at(unix, offset) else {
        return "--".to_string();
    };
    dt.format("%Y-%m-%d %H:%M").to_string()
}

/// RFC 3339 timestamp in UTC (or `--`), the encoding half of *Log timestamps* in `docs/contracts.md`.
#[must_use]
pub fn format_iso_date(unix: Option<i64>) -> String {
    let offset = chrono::FixedOffset::east_opt(0);
    match at(unix, offset) {
        Some(dt) => dt.to_rfc3339(),
        None => "--".to_string(),
    }
}

/// A machine timestamp, or `None` for unknown / Go's zero `time.Time`.
fn at(
    unix: Option<i64>,
    offset: Option<chrono::FixedOffset>,
) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    let secs = unix?;
    if is_go_zero_time(secs) {
        return None;
    }
    let utc = chrono::DateTime::from_timestamp(secs, 0)?;
    let offset = offset.or_else(|| chrono::FixedOffset::east_opt(0))?;
    Some(offset.from_utc_datetime(&utc.naive_utc()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_matches_go() {
        // The two edges the plan names, then the whole table.
        assert_eq!(ByteSize(0).humanize(), "0 B");
        assert_eq!(ByteSize(1023).humanize(), "1023 B");
        assert_eq!(ByteSize(1).humanize(), "1 B");
        assert_eq!(ByteSize(1024).humanize(), "1.0 KiB");
        assert_eq!(ByteSize(1536).humanize(), "1.5 KiB");
        assert_eq!(ByteSize(10_485_759).humanize(), "10.0 MB"); // 10 MiB - 1 byte
        assert_eq!(ByteSize(1_048_576).humanize(), "1.0 MB");
        assert_eq!(ByteSize(1_073_741_824).humanize(), "1.0 GB");
        assert_eq!(ByteSize(1_099_511_627_776).humanize(), "1.0 TB");
        assert_eq!(ByteSize(1_125_899_906_842_624).humanize(), "1.0 PB");
        // Cap: never a unit past PB.
        assert!(ByteSize(u64::MAX).humanize().ends_with(" PB"));
        // The fixture values, straight from `scripts/make-fixtures.py`: sparse.bin is 1 MiB logical and
        // the `many/` directory is the 200-entry page size.
        assert_eq!(ByteSize(1_048_576).humanize(), "1.0 MB");
        assert_eq!(ByteSize(4096).humanize(), "4.0 KiB");
        assert_eq!(ByteSize(3).humanize(), "3 B");
    }

    #[test]
    fn byte_size_displays_as_humanized() {
        assert_eq!(ByteSize(2048).to_string(), "2.0 KiB");
    }

    #[test]
    fn files_count_none_is_the_undefined_files_fix() {
        // Plan §5.2 `metrics.rs` *Done when*.
        let json = serde_json::to_string(&FileSizes {
            bytes: 4096,
            files: None,
        })
        .unwrap();
        assert_eq!(json, r#"{"bytes":4096}"#);
        let with = serde_json::to_string(&FileSizes {
            bytes: 4096,
            files: Some(12),
        })
        .unwrap();
        assert_eq!(with, r#"{"bytes":4096,"files":12}"#);
    }

    #[test]
    fn go_zero_time_is_recognised() {
        // docs/contracts.md → fileMode strings: `mod_time:-62135596800`.
        assert_eq!(GO_ZERO_TIME_UNIX, -62_135_596_800);
        assert!(is_go_zero_time(-62_135_596_800));
        assert!(!is_go_zero_time(0));
    }

    #[test]
    fn mod_time_formats_in_utc() {
        // 1_767_225_600 is FIXTURE_MTIME in scripts/make-fixtures.py: 2026-01-01T00:00:00Z.
        assert_eq!(format_mod_time(Some(1_767_225_600)), "2026-01-01 00:00");
        assert_eq!(
            format_mod_time(Some(1_767_225_600 + 3_661)),
            "2026-01-01 01:01"
        );
        assert_eq!(
            format_iso_date(Some(1_767_225_600)),
            "2026-01-01T00:00:00+00:00"
        );
    }

    #[test]
    fn unknown_times_render_as_dashes() {
        for f in [format_mod_time, format_iso_date] {
            assert_eq!(f(None), "--");
            // A degraded row from Go arrives as the zero time, not as None: same answer either way.
            assert_eq!(f(Some(GO_ZERO_TIME_UNIX)), "--");
        }
    }

    #[test]
    fn local_formatting_produces_a_timestamp_of_the_same_instant() {
        let s = format_mod_time_local(Some(1_767_225_600));
        // Same shape, whatever the machine's zone: YYYY-MM-DD HH:MM.
        assert_eq!(s.len(), "2026-01-01 00:00".len(), "{s}");
        assert!(s.as_bytes()[4] == b'-');
        assert_eq!(format_mod_time_local(None), "--");
    }
}
