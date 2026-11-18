//! Step 5 acceptance gate (plan §7 step 5): the Go undo log is the wire format, and the Go database is
//! the storage format.
//!
//! Three things have to be true, all of them measured against bytes a disposable Go `zen-man` instance
//! actually wrote (`tests/golden/`, provenance in `tests/golden/PROVENANCE.json`):
//!
//! 1. **Byte-identical re-encode.** Every record of `tests/golden/action.log` decodes and re-encodes to
//!    the same bytes — including the already-reverted record 1, whose CRC Go recomputed in place.
//! 2. **`resync` recovers the right count** from a truncated and a CRC-corrupted copy, and leaves a file
//!    that decodes cleanly. The golden itself is never opened for writing (rules: `tests/golden/**` is
//!    read-only); the destructive cases run on copies in a temp dir.
//! 3. **Opening the Go-produced `zen-man.db` succeeds** through `store::sql::connect_with_retry` and
//!    `index::schema::verify` reports zero issues — the gate `docs/contracts.md` names for the
//!    modernc→rusqlite driver change.
//!
//! `docs/contracts.md` §*ZMLG undo log* lists the cases the frozen log cannot cover; those are the
//! synthesised tests at the end: a bad-CRC frame, a truncated tail, a log that needs magic-resync, a
//! 65 535-byte path, non-UTF-8 path bytes, and reverts of the four other reversible action types (the
//! capture contains exactly one reverted record, and it is a `mkdir`).

use std::fs;
use std::path::{Path, PathBuf};

use zenman_core::history::codec::{
    self, ActionEntry, ActionLog, ActionRecord, ActionType, LogStatus, MAGIC, MAX_FIELD,
};
use zenman_core::history::store::{ActionLogStore, PruneLimit};
use zenman_core::index::schema;
use zenman_core::store::config::StoreMode;
use zenman_core::store::sql::{self, DbOpenError};

const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/golden");

fn golden(name: &str) -> PathBuf {
    Path::new(GOLDEN).join(name)
}

fn golden_log() -> Vec<u8> {
    fs::read(golden("action.log")).expect("the frozen undo log is readable")
}

fn golden_json(name: &str) -> serde_json::Value {
    let text = fs::read_to_string(golden(name)).expect("golden is readable");
    serde_json::from_str(&text).expect("golden is valid JSON")
}

/// The `${FIXTURE_ROOT}` ↔ real-path map `LOG-PATHS.json` carries, because `action.log` is deliberately
/// *not* placeholder-ified (its CRCs seal the original bytes).
fn fixture_paths() -> serde_json::Map<String, serde_json::Value> {
    golden_json("LOG-PATHS.json")["paths"]
        .as_object()
        .expect("LOG-PATHS.json has a paths map")
        .clone()
}

/// Remove a temp dir's files and the directory itself. `std::fs::remove_dir_all` is banned workspace-wide
/// (`clippy.toml`: deletion goes through `ops::delete`), and a `tests/golden/**` copy must never be the
/// thing a recursive delete reaches, so the test helper is deliberately one level deep.
fn cleanup(dir: &Path) {
    for entry in fs::read_dir(dir).expect("temp dir is readable") {
        fs::remove_file(entry.expect("dir entry").path()).ok();
    }
    fs::remove_dir(dir).ok();
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zenman-contract-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Decode every frame of `log`, in order. Panics with the byte offset on any framing error, so a
/// regression points at the record rather than at the test.
fn decode_all(log: &[u8]) -> Vec<(u64, ActionRecord, usize)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < log.len() {
        let offset = pos as u64;
        let decoded = codec::decode_at(&log[pos..], offset)
            .unwrap_or_else(|e| panic!("frame at byte {offset} does not decode: {e}"));
        assert!(
            decoded.crc_ok,
            "the frozen log must contain only CRC-valid frames (frame at {offset})"
        );
        out.push((offset, decoded.record, decoded.consumed));
        pos += decoded.consumed;
    }
    out
}

// ── (a) + (b) the byte-identity gate ─────────────────────────────────────────

#[test]
fn every_golden_record_reencodes_byte_identically() {
    let log = golden_log();
    let records = decode_all(&log);
    assert_eq!(records.len(), 19, "the frozen log has 19 frames");
    assert_eq!(log.len(), 7_661);

    let mut mismatched: Vec<(u64, String)> = Vec::new();
    let mut reencoded = Vec::new();
    for (offset, record, consumed) in &records {
        let bytes = codec::encode_vec(record).expect("a captured record always encodes");
        let original = &log[*offset as usize..*offset as usize + consumed];
        if bytes != original {
            let at = bytes
                .iter()
                .zip(original.iter())
                .position(|(a, b)| a != b)
                .unwrap_or_else(|| panic!("length differs at frame {offset}"));
            mismatched.push((
                *offset,
                format!(
                    "byte {at}: ours {:#04x}, Go {:#04x} (ours {} B, Go {} B)",
                    bytes[at],
                    original[at],
                    bytes.len(),
                    original.len()
                ),
            ));
        }
        reencoded.extend_from_slice(&bytes);
    }
    assert!(
        mismatched.is_empty(),
        "re-encode differs at: {mismatched:?}"
    );
    // (b) the whole file, in one comparison.
    assert_eq!(
        reencoded, log,
        "decode → encode is the identity on the whole log"
    );
}

#[test]
fn the_log_has_no_padding_no_trailer_and_no_gaps() {
    let log = golden_log();
    let records = decode_all(&log);
    let last = records.last().expect("non-empty");
    assert_eq!(
        last.0 as usize + last.2,
        log.len(),
        "the final frame ends exactly at EOF — there is no sentinel"
    );
    // And the offsets match the frozen decode table byte for byte.
    let table = golden_json("LOG-RECORDS.json");
    let rows = table["records"].as_array().expect("records array");
    assert_eq!(rows.len(), records.len());
    for (row, (offset, record, consumed)) in rows.iter().zip(&records) {
        assert_eq!(row["offset"].as_u64().expect("offset"), *offset);
        assert_eq!(row["id"].as_i64().expect("id"), record.id);
        assert_eq!(
            row["ts_ns"].as_i64().expect("ts"),
            record
                .timestamp
                .timestamp_nanos_opt()
                .expect("in range for i64 ns"),
            "the timestamp is Unix **nanoseconds**, not seconds, despite what plan §5.4's prose says"
        );
        assert_eq!(
            row["action"].as_u64().expect("action") as u8,
            record.action.0
        );
        assert_eq!(
            row["rec_len"].as_u64().expect("rec_len") as usize + 4,
            *consumed
        );
        assert_eq!(
            row["total_len"].as_u64().expect("total_len") as usize,
            *consumed
        );
        assert_eq!(
            row["status"].as_u64().expect("status") as u8,
            record.status.0
        );
        assert!(row["crc_ok"].as_bool().expect("crc_ok"));
    }
    assert_eq!(
        table["file_bytes"].as_u64().expect("file_bytes"),
        log.len() as u64
    );
}

// ── (c) counts, and what the frozen index actually says ─────────────────────

#[test]
fn the_frozen_manifests_agree_with_the_bytes_on_counts() {
    let log = golden_log();
    let records = decode_all(&log);
    let reverted = records
        .iter()
        .filter(|(_, r, _)| r.status == LogStatus::REVERTED)
        .count();
    // 19 records, and exactly **one** revert marker (record 1, the `mkdir` the capture reverted).
    assert_eq!(records.len(), 19);
    assert_eq!(reverted, 1);
    let paths = golden_json("LOG-PATHS.json");
    assert_eq!(paths["record_count"].as_u64().expect("count"), 19);
    assert!(paths["all_crc_ok"].as_bool().expect("crc"));
    assert_eq!(
        golden_json("LOG-RECORDS.json")["record_count"]
            .as_u64()
            .expect("count"),
        19
    );
    // `tests/golden/INDEX.md` does not exist; `INDEX.json`'s `count` is the *capture manifest* length
    // (138 rows: 122 HTTP captures + artefacts + superseded tombstones), not a log record count.
    assert_eq!(
        golden_json("INDEX.json")["count"].as_u64().expect("count"),
        138
    );
}

#[test]
fn decoded_paths_and_json_fields_match_the_captured_api_output() {
    let log = golden_log();
    let records = decode_all(&log);
    let map = fixture_paths();
    // The frozen `action.log` is the **post**-revert log (record 1 carries `status:1`), so the API
    // capture to compare against is `log-after-revert.json`; `log.json` was taken before the revert and
    // differs in exactly the two status fields of record 1 (see the revert test below).
    let api = golden_json("log-after-revert.json");
    let rows = api["records"].as_array().expect("records");
    assert_eq!(
        rows.len(),
        records.len(),
        "log-after-revert.json`s `count` is the whole log (19 ≤ the 50 default)"
    );
    // Go's `/api/log` answers newest-first; the file is oldest-first, so the zip is reversed.
    for (row, (_, record, _)) in rows.iter().zip(records.iter().rev()) {
        assert_eq!(row["id"].as_i64().expect("id"), record.id);
        assert_eq!(
            row["action"].as_u64().expect("action") as u8,
            record.action.0
        );
        assert_eq!(
            row["action_str"].as_str().expect("action_str"),
            record.action_str()
        );
        assert_eq!(
            row["status_str"].as_str().expect("status_str"),
            record.status_str()
        );
        assert_eq!(
            row["reversible"].as_bool().expect("reversible"),
            record.is_reversible()
        );
        assert_eq!(
            row["sources"].as_array().expect("sources").len(),
            record.sources.len()
        );
        for (ours, theirs) in record
            .sources
            .iter()
            .zip(row["sources"].as_array().expect("arr"))
        {
            let placeholder = map
                .get(ours.as_str())
                .and_then(|v| v.as_str())
                .unwrap_or(ours.as_str());
            assert_eq!(
                theirs.as_str().expect("string source"),
                placeholder,
                "{ours}"
            );
        }
        let check_path_field = |field: &str, ours: &str| {
            let theirs = row[field].as_str().expect(field);
            let want = map.get(ours).and_then(|v| v.as_str()).unwrap_or(ours);
            assert_eq!(theirs, want, "{field} of record {}", record.id);
        };
        check_path_field("dest", &record.dest);
        check_path_field("name", &record.name);
        // The instant is the same even though the golden renders the capture host's +07:00 and we have
        // no zone to render: compare the parsed nanoseconds, which is the only zone-free fact.
        let theirs: chrono::DateTime<chrono::FixedOffset> =
            chrono::DateTime::parse_from_rfc3339(row["timestamp"].as_str().expect("timestamp"))
                .expect("RFC3339 from Go");
        assert_eq!(
            theirs.timestamp_nanos_opt().expect("in range for i64 ns"),
            record
                .timestamp
                .timestamp_nanos_opt()
                .expect("in range for i64 ns")
        );
    }
}

// ── (d) corrupt-log recovery ────────────────────────────────────────────────

#[test]
fn resync_on_a_truncated_copy_recovers_the_record_count() {
    let log = golden_log();
    let dir = temp_dir("trunc");
    let path = dir.join("action.log");
    // Chop the last record (offset 7 567, 94 bytes) in half.
    let cut = log.len() - 40;
    fs::write(&path, &log[..cut]).expect("copy is writable");
    let mut store = ActionLogStore::open(&path).expect("a damaged log opens");
    assert!(!store.is_clean());
    let damage = store.damage().expect("damage reported").clone();
    assert_eq!(
        damage.offset, 7_567,
        "the last whole frame's end is the truncate point"
    );
    // The copy is 40 bytes shorter than the golden, so only 54 of the last frame remain.
    assert_eq!(damage.bytes_after, (cut - 7_567) as u64);
    let report = store.resync().expect("resync");
    assert_eq!(
        report.records_kept, 18,
        "34-record logs do not exist; the golden has 19"
    );
    assert_eq!(report.bytes_kept, 7_567);
    assert_eq!(report.records_dropped, 0, "the tail was not a frame at all");
    assert_eq!(report.next_id, 19, "the discarded record's id is reusable");
    assert!(store.is_clean());
    // The file is decodable and its prefix is untouched.
    let after = fs::read(&path).expect("read back");
    assert_eq!(
        after,
        log[..7_567],
        "truncation removed bytes, it did not rewrite any"
    );
    let reopened = ActionLogStore::open(&path).expect("reopen");
    assert_eq!(reopened.record_count(), 18);
    assert!(reopened.is_clean());
    assert!(
        reopened
            .query_recent(50)
            .expect("page")
            .entries()
            .iter()
            .all(|r| r.id <= 18)
    );
    fs::remove_file(&path).ok();
    fs::remove_dir(&dir).ok();
}

#[test]
fn resync_on_a_crc_flipped_copy_stops_at_the_bad_frame() {
    let log = golden_log();
    let dir = temp_dir("badcrc");
    let path = dir.join("action.log");
    let mut damaged = log.clone();
    // Frame 5 (offset 438, `paste-move`): flip a byte inside its `dest` field, leaving the frame's
    // structure intact so only the CRC can catch it.
    damaged[438 + 30] ^= 0xFF;
    fs::write(&path, &damaged).expect("copy is writable");
    let mut store = ActionLogStore::open(&path).expect("a damaged log opens");
    let damage = store.damage().expect("damage reported").clone();
    // 438 is the *seventh* frame's offset (0,59,132,197,282,350,438), so the intact prefix is ids
    // 1..=6 and ids 8..=19 sit past the damage.
    assert_eq!(damage.offset, 438);
    assert!(damage.reason.contains("CRC"), "{:?}", damage.reason);
    assert_eq!(damage.records_after, 12);
    // Go-parity reading: 18 of the 19 frames still verify, and the index keeps all of them.
    assert_eq!(store.record_count(), 18);
    let mut iter = store.iter_after(0).expect("iter");
    let walked: Vec<ActionRecord> = iter.by_ref().filter_map(Result::ok).collect();
    assert_eq!(walked.len(), 18, "the damaged frame is skipped, not fatal");
    assert_eq!(
        iter.skipped(),
        1,
        "and the skip is observable, unlike Go's silent `continue`"
    );
    assert!(iter.is_exhausted());
    let report = store.resync().expect("resync");
    assert_eq!(report.records_kept, 6, "the intact prefix only: ids 1..=6");
    assert_eq!(report.records_dropped, 12);
    assert_eq!(report.bytes_dropped, (log.len() - 438) as u64);
    assert_eq!(report.next_id, 7);
    assert!(store.is_clean());
    let after = fs::read(&path).expect("read back");
    assert_eq!(
        after,
        log[..438],
        "everything before the bad frame is preserved verbatim"
    );
    let reopened = ActionLogStore::open(&path).expect("reopen");
    assert_eq!(reopened.record_count(), 6);
    assert!(reopened.is_clean());
    fs::remove_file(&path).ok();
    fs::remove_dir(&dir).ok();
}

#[test]
fn the_walk_resyncs_on_the_magic_and_reports_the_garbage() {
    let log = golden_log();
    let dir = temp_dir("resync");
    let path = dir.join("action.log");
    let mut mixed = log.clone();
    let first_damage = log.len();
    mixed.extend_from_slice(b"\x00\xff garbage between two valid frames \x00");
    mixed.extend_from_slice(&log[..200]);
    fs::write(&path, &mixed).expect("write");
    let store = ActionLogStore::open(&path).expect("open");
    assert_eq!(store.damage().expect("damage").offset, first_damage as u64);
    assert_eq!(
        store.record_count(),
        19 + 3,
        "the frames after the garbage are found by scanning for the magic, exactly as \
         api/log.go:196-202 does byte by byte"
    );
    fs::remove_file(&path).ok();
    fs::remove_dir(&dir).ok();
}

// ── (e) the Go-produced database ────────────────────────────────────────────

#[test]
fn opening_the_go_produced_database_succeeds_and_verifies() {
    let dir = temp_dir("db");
    // The golden is read-only in every sense: work on a copy, because `connect_with_retry` stamps
    // `user_version` and `apply` may insert the version row.
    let path = dir.join("zen-man.db");
    fs::copy(golden("zen-man.db"), &path).expect("copy the Go database");
    let conn = sql::connect_with_retry(
        &path,
        StoreMode::Full,
        4,
        std::time::Duration::from_millis(5),
    )
    .expect("rusqlite opens the modernc-written file");

    // Schema gate.
    let issues = schema::verify(&conn).expect("verify runs");
    assert!(
        issues.is_empty(),
        "the Go database must satisfy our schema: {issues:?}"
    );
    assert_eq!(
        schema::version_of(&conn).expect("version"),
        schema::SCHEMA_VERSION
    );

    // The measured pragmas (`docs/contracts.md` §*SQLite storage*).
    let journal: String = conn
        .query_row("PRAGMA journal_mode;", [], |r| r.get(0))
        .expect("pragma");
    assert_eq!(journal.to_lowercase(), "delete", "not WAL");
    let page: i64 = conn
        .query_row("PRAGMA page_size;", [], |r| r.get(0))
        .expect("pragma");
    assert_eq!(page, 4096);
    let busy: i64 = conn
        .query_row("PRAGMA busy_timeout;", [], |r| r.get(0))
        .expect("pragma");
    assert_eq!(busy, i64::from(sql::BUSY_TIMEOUT_MS));
    // `connect_with_retry` already insisted on `integrity_check = ok`; re-read it for the report.
    let check: String = conn
        .query_row("PRAGMA integrity_check;", [], |r| r.get(0))
        .expect("pragma");
    assert_eq!(check, "ok");
    // The stamp we add, which Go never did: 0 on the golden, SCHEMA_VERSION after our open.
    let uv: i64 = conn
        .query_row("PRAGMA user_version;", [], |r| r.get(0))
        .expect("pragma");
    assert_eq!(uv, i64::from(schema::SCHEMA_VERSION));

    // Row counts from `docs/contracts.md`, proving the file is not merely openable but readable.
    let counts = [
        ("files", 1_260),
        ("folder_sizes", 31),
        ("files_fts", 1_260),
        ("files_fts_content", 1_260),
    ];
    for (table, want) in counts {
        let n: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap_or_else(|e| panic!("cannot read {table} (fts5 compiled in?): {e}"));
        assert_eq!(n, want, "{table}");
    }
    // The five indexes carry the `shadow` names Go's rename left behind, attached to `files`.
    let names: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='files' ORDER BY name;")
            .expect("prepare");
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .expect("query");
        rows.filter_map(Result::ok).collect()
    };
    assert_eq!(
        names,
        vec![
            "idx_files_shadow_ext",
            "idx_files_shadow_mtime",
            "idx_files_shadow_name",
            "idx_files_shadow_parent",
            "idx_files_shadow_path",
        ]
    );
    // A trigram search — the capability plan §2.2 bets on — works against Go's FTS5 content.
    let hits: i64 = conn
        .query_row(
            "SELECT count(*) FROM files_fts WHERE files_fts MATCH ?1",
            ["leaf"],
            |r| r.get(0),
        )
        .expect("fts5 trigram query runs");
    assert!(
        hits >= 1,
        "expected `leaf.txt` in the FTS index, got {hits}"
    );
    cleanup(&dir);
}

#[test]
fn a_database_we_create_is_indistinguishable_from_a_go_database_as_far_as_verify_can_tell() {
    let dir = temp_dir("db2");
    let path = dir.join("zen-man.db");
    // `connect_with_retry` already ran `apply`, so a bare connection is the honest way to watch the
    // schema get created: nine tables (four declared + five FTS5 shadow tables) and five indexes.
    let raw = rusqlite::Connection::open(&path).expect("bare open");
    let created = schema::apply(&raw).expect("apply");
    assert_eq!(
        created.created_tables.len(),
        9,
        "{:?}",
        created.created_tables
    );
    assert_eq!(created.version, schema::SCHEMA_VERSION);
    let again = schema::apply(&raw).expect("apply again");
    assert!(again.created_tables.is_empty(), "twice is a no-op");
    drop(raw);
    // And reopening through the port's own front door is clean, on a file Go never touched.
    let conn = sql::connect_with_retry(
        &path,
        StoreMode::Full,
        2,
        std::time::Duration::from_millis(5),
    )
    .expect("reopen");
    assert!(schema::verify(&conn).expect("verify").is_empty());
    let journal: String = conn
        .query_row("PRAGMA journal_mode;", [], |r| r.get(0))
        .expect("pragma");
    assert_eq!(journal.to_lowercase(), "delete");
    cleanup(&dir);
}

// ── (f) the empty log and the revert marker ─────────────────────────────────

#[test]
fn an_empty_log_is_zero_bytes_and_answers_with_an_empty_array() {
    let dir = temp_dir("empty");
    let path = dir.join("action.log");
    ActionLogStore::ensure_persisted(&path).expect("ensure");
    let bytes = fs::read(&path).expect("read");
    assert_eq!(
        bytes.len(),
        0,
        "Go writes no header and no sentinel: an empty log is an empty file"
    );
    let store = ActionLogStore::open(&path).expect("open");
    let page = store.query_recent(50).expect("page");
    assert_eq!(
        page.to_json(),
        serde_json::json!({"count": 0, "records": []}),
        "`records` is `[]`, never `null` (`api/log.go:685-687`, `log.json`)"
    );
    assert!(page.entries().is_empty());
    fs::remove_file(&path).ok();
    fs::remove_dir(&dir).ok();
}

#[test]
fn our_crc_reseal_reproduces_the_revert_the_capture_performed() {
    // The capture reverted record 1 (`mkdir`), and Go patched it **in place** — so the frozen log holds
    // the post-revert bytes: `status = 1`, CRC recomputed, everything else identical
    // (`api/log.go:355-386`, `LOG-RECORDS.json` row 1: `status:1, crc_ok:true`).
    let log = golden_log();
    let (offset, record, consumed) = decode_all(&log)
        .into_iter()
        .find(|(_, r, _)| r.id == 1)
        .expect("record 1");
    assert_eq!(offset, 0);
    assert_eq!(record.status, LogStatus::REVERTED);
    assert_eq!(record.action, ActionType::MKDIR);
    let golden_frame = &log[offset as usize..offset as usize + consumed];

    // Undo Go's patch to recover what was written at append time…
    let mut pre = golden_frame.to_vec();
    pre[codec::STATUS_BYTE_OFF] = LogStatus::DONE.0;
    let at = pre.len() - codec::CRC_LEN;
    let crc = codec::crc_of(&pre[..at]).to_be_bytes();
    pre[at..].copy_from_slice(&crc);
    assert!(
        codec::verify_crc(&pre),
        "the reconstructed original verifies"
    );
    // …and re-apply ours. It must produce the frozen bytes exactly: same status byte, same CRC.
    let patched = codec::patch_reverted(&pre).expect("patch");
    assert_eq!(patched, golden_frame, "our CRC is Go's CRC, byte for byte");
    // The store does the same thing to a file copy, with the same id.
    let dir = temp_dir("revert");
    let path = dir.join("action.log");
    fs::write(&path, &log).expect("copy");
    let mut store = ActionLogStore::open(&path).expect("open");
    let (second, _) = store.get(2).expect("record 2 is not yet reverted");
    assert_eq!(second.status, LogStatus::DONE);
    let marked = store.mark_reverted(2).expect("mark");
    assert_eq!(
        marked.id, 2,
        "the revert reuses the same action id — no new record is appended"
    );
    assert_eq!(marked.status, LogStatus::REVERTED);
    assert_eq!(
        store.len().expect("size"),
        log.len() as u64,
        "an in-place patch cannot grow the log"
    );
    // Record 2 spans [59, 59+73). Everything outside it, and inside it apart from the status byte and
    // the trailing CRC, is byte-identical to the golden.
    let after = fs::read(&path).expect("read back");
    assert_eq!(after.len(), log.len());
    let (lo, hi) = (59usize, 59 + 73);
    assert_eq!(
        &after[..lo],
        &log[..lo],
        "the frames before it are untouched"
    );
    assert_eq!(
        &after[hi..],
        &log[hi..],
        "the frames after it are untouched"
    );
    assert_eq!(
        after[lo + 4..lo + codec::STATUS_BYTE_OFF],
        log[lo + 4..lo + codec::STATUS_BYTE_OFF],
        "the header before the status byte is untouched"
    );
    assert_eq!(
        after[lo + codec::STATUS_BYTE_OFF + 1..hi - codec::CRC_LEN],
        log[lo + codec::STATUS_BYTE_OFF + 1..hi - codec::CRC_LEN],
        "the body is untouched"
    );
    assert_eq!(after[lo + codec::STATUS_BYTE_OFF], LogStatus::REVERTED.0);
    assert_eq!(
        log[lo + codec::STATUS_BYTE_OFF],
        LogStatus::DONE.0,
        "the golden starts undone"
    );
    let changed = (lo..hi).filter(|k| after[*k] != log[*k]).count();
    assert!((1..=5).contains(&changed), "{changed} bytes changed");
    // The revert response the capture recorded, cross-checked against our decode.
    let resp = golden_json("log-revert-mkdir.json");
    assert_eq!(resp["action"].as_str().expect("action"), "mkdir");
    assert_eq!(
        resp["action"].as_str().expect("action"),
        record.action_str()
    );
    assert_eq!(resp["record_id"].as_i64().expect("record id"), record.id);
    assert_eq!(resp["status"].as_str().expect("status"), "reverted");
    // And the API shape after the revert (`log-after-revert.json`), parsed not byte-compared.
    let after_api = golden_json("log-after-revert.json");
    let row = after_api["records"]
        .as_array()
        .expect("records")
        .iter()
        .find(|r| r["id"].as_i64() == Some(1))
        .expect("record 1");
    assert_eq!(row["status"].as_u64().expect("status"), 1);
    assert_eq!(row["status_str"].as_str().expect("status_str"), "reverted");
    // …and `log.json`, captured before the revert, is the same log with that one record `done`.
    let pre = golden_json("log.json");
    let pre_row = pre["records"]
        .as_array()
        .expect("records")
        .iter()
        .find(|r| r["id"].as_i64() == Some(1))
        .expect("record 1");
    assert_eq!(pre_row["status"].as_u64().expect("status"), 0);
    assert_eq!(pre_row["status_str"].as_str().expect("status_str"), "done");
    fs::remove_file(&path).ok();
    fs::remove_dir(&dir).ok();
}

// ── the codec gaps `docs/contracts.md` says the frozen log cannot cover ─────

fn made_up(id: i64, action: ActionType) -> ActionRecord {
    ActionRecord {
        id,
        timestamp: chrono::DateTime::from_timestamp(1_794_991_944, 369_498_928).expect("in range"),
        action,
        status: LogStatus::DONE,
        sources: vec!["/src/a.txt".into()],
        dest: "/dst".into(),
        name: String::new(),
    }
}

#[test]
fn a_revert_of_every_reversible_action_type_reuses_the_frame() {
    // `log_revert.go` handles five action types; the capture froze only the `mkdir` one. The *log* side
    // of a revert is identical for all five, and it is what this step owns.
    let reversible = [
        ActionType::PASTE_COPY,
        ActionType::PASTE_MOVE,
        ActionType::RENAME,
        ActionType::MKDIR,
        ActionType::PASTE_LINK,
    ];
    for (i, action) in reversible.iter().enumerate() {
        let mut rec = made_up(i as i64 + 1, *action);
        if *action == ActionType::RENAME {
            rec.name = "renamed-one.txt".into();
        }
        let frame = codec::encode_vec(&rec).expect("encodes");
        let patched = codec::patch_reverted(&frame).expect("patches");
        assert!(codec::verify_crc(&patched), "{action}");
        assert_eq!(patched.len(), frame.len());
        let (decoded, _) = codec::decode(&patched).expect("decodes");
        assert_eq!(decoded.id, rec.id, "the same action id, not a new one");
        assert_eq!(decoded.timestamp, rec.timestamp);
        assert_eq!(decoded.status, LogStatus::REVERTED);
        assert_eq!(decoded.name, rec.name);
        assert_eq!(decoded.sources, rec.sources);
        assert!(decoded.is_reversible());
        // Everything but the status byte and the CRC is byte-identical.
        let changed: Vec<usize> = (0..frame.len())
            .filter(|k| frame[*k] != patched[*k])
            .collect();
        assert!(changed.len() <= 5, "{action} touched {changed:?}");
        assert!(
            changed
                .iter()
                .all(|k| *k == codec::STATUS_BYTE_OFF || *k >= frame.len() - codec::CRC_LEN),
            "{action} touched {changed:?}, which is outside the status byte and the CRC"
        );
    }
    // And the three Go refuses to revert (422 `action 'copy' cannot be reverted`).
    for action in [ActionType::COPY, ActionType::DELETE, ActionType::CHMOD] {
        assert!(!action.is_reversible(), "{action}");
        let frame = codec::encode_vec(&made_up(9, action)).expect("encodes");
        let (decoded, _) = codec::decode(&frame).expect("decodes");
        assert!(!decoded.is_reversible());
    }
    // The batch-rename shape the golden really has: `name` is a JSON array string (record 14).
    let mut batch = made_up(14, ActionType::RENAME);
    batch.sources = vec!["/ops/two.txt".into(), "/ops/three.txt".into()];
    batch.name = "[\"renamed-one.txt\",\"renamed-two.txt\"]".into();
    let bytes = codec::encode_vec(&batch).expect("encodes");
    let (back, _) = codec::decode(&bytes).expect("decodes");
    assert_eq!(back.name, batch.name);
    let parsed: Vec<String> =
        serde_json::from_str(&back.name).expect("Go's batch rename is JSON in `name`");
    assert_eq!(
        parsed.len(),
        back.sources.len(),
        "the sniff at api/log_revert.go:110"
    );
}

#[test]
fn the_sixty_five_kilobyte_path_and_the_empty_field_edge_cases_round_trip() {
    let dir = temp_dir("big");
    let path = dir.join("action.log");
    let mut store = ActionLogStore::open(&path).expect("open");
    let long = "x".repeat(MAX_FIELD);
    let cases = [
        ActionEntry::new(ActionType::CHMOD, vec![long.clone()], "/dst", "600"),
        ActionEntry::new(ActionType::PASTE_COPY, vec![], "", ""),
        ActionEntry::new(
            ActionType::RENAME,
            vec!["a\ntext\nwith newline.txt".into()],
            "",
            "[\"a\",\"b\"]",
        ),
        ActionEntry::mkdir("/dst", "caf\u{e9}"),
    ];
    let range = store.append(&cases).expect("appended");
    assert!(store.is_clean());
    let read = store.read_range(range.start, range.end).expect("read back");
    assert_eq!(read.len(), 4);
    assert_eq!(
        read[0].sources[0].len(),
        MAX_FIELD,
        "a u16::MAX-length path fits exactly"
    );
    assert!(read[1].sources.is_empty() && read[1].dest.is_empty() && read[1].name.is_empty());
    assert_eq!(
        read[2].name, "[\"a\",\"b\"]",
        "the single-source batch-rename lookalike"
    );
    assert_eq!(
        read[3].name, "caf\u{e9}",
        "non-ASCII is stored as UTF-8 bytes"
    );
    // The over-long field is refused, not truncated.
    let too_long = "x".repeat(MAX_FIELD + 1);
    let e = store
        .append(&[ActionEntry::new(ActionType::CHMOD, vec![too_long], "", "")])
        .expect_err("65 536 bytes cannot be stored in a u16 length prefix");
    match e {
        zenman_core::error::CoreError::CapacityExceeded { limit, .. } => {
            assert_eq!(limit, MAX_FIELD)
        }
        other => panic!("expected CapacityExceeded, got {other}"),
    }
    assert_eq!(store.len().expect("nothing was written"), range.end);
    fs::remove_file(&path).ok();
    fs::remove_dir(&dir).ok();
}

#[test]
fn a_non_utf8_path_is_lossy_and_say_so_rather_than_panicking() {
    let mut raw: Vec<u8> = Vec::new();
    raw.extend_from_slice(&MAGIC);
    raw.extend_from_slice(&42i64.to_be_bytes());
    raw.extend_from_slice(&1_794_991_944_369_498_928i64.to_be_bytes());
    raw.push(ActionType::CHMOD.0);
    raw.push(LogStatus::DONE.0);
    let body: Vec<u8> = [
        &0u16.to_be_bytes()[..], // numSrc = 0
        &2u16.to_be_bytes()[..],
        &[0xFFu8, 0xFE], // dest: invalid UTF-8
        &1u16.to_be_bytes()[..],
        b"m", // name
    ]
    .concat();
    let rec_len = (codec::HEADER_FIXED + body.len()) as u32;
    raw.extend_from_slice(&rec_len.to_be_bytes());
    raw.extend_from_slice(&body);
    let crc = codec::crc_of(&raw).to_be_bytes();
    raw.extend_from_slice(&crc);

    let decoded =
        codec::decode_at(&raw, 0).expect("Go's byte-slice paths decode without panicking");
    assert!(decoded.crc_ok);
    assert_eq!(
        decoded.lossy_fields, 1,
        "the invalid dest is counted; the ASCII name is not"
    );
    assert_eq!(decoded.record.dest, "\u{fffd}\u{fffd}");
    assert_eq!(decoded.record.name, "m");
    assert_eq!(decoded.consumed, raw.len());
    // Which means the round trip is *not* byte-identical, and the codec says so instead of hiding it.
    assert_ne!(codec::encode_vec(&decoded.record).expect("re-encodes"), raw);
}

#[test]
fn a_pruned_log_keeps_the_surviving_records_byte_for_byte() {
    let log = golden_log();
    let dir = temp_dir("prune");
    let path = dir.join("action.log");
    fs::write(&path, &log).expect("copy");
    let mut store = ActionLogStore::open(&path).expect("open");
    let report = store.prune(Some(PruneLimit::Newest(3))).expect("prune");
    assert_eq!(report.records_kept, 3);
    assert_eq!(report.records_dropped, 16);
    assert_eq!(report.next_id, 20, "ids are not rewound by a prune");
    // The last three frames of the golden start at offset 4 319 (LOG-RECORDS.json) and run to EOF.
    assert_eq!(report.bytes_after, (7_661 - 4_319) as u64);
    assert_eq!(report.bytes_dropped, 4_319);
    let after = fs::read(&path).expect("read back");
    assert_eq!(
        after,
        log[4_319..],
        "the three survivors are Go's own bytes, verbatim"
    );
    let page = store.query_recent(50).expect("page");
    let ids: Vec<i64> = page.entries().iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![19, 18, 17], "newest first, as /api/log answers");
    fs::remove_file(&path).ok();
    fs::remove_dir(&dir).ok();
}

#[test]
fn a_locked_database_is_reported_as_busy_and_a_directory_is_not() {
    // The other half of the plan §6.1 F5 gate: the *type* of failure decides whether the caller can
    // fall back to `StoreMode::NoDb`, so it is asserted here rather than only in the unit tests.
    let dir = temp_dir("locked");
    let db = dir.join("zen-man.db");
    let conn =
        sql::connect_with_retry(&db, StoreMode::Full, 2, std::time::Duration::from_millis(1))
            .expect("first open works");
    let again = sql::open_with_retry(&db, StoreMode::Full, 2, std::time::Duration::from_millis(1))
        .expect("a second reader is legal in DELETE mode when nothing is writing");
    drop(again);
    let core: zenman_core::error::CoreError = DbOpenError::Locked {
        path: db.display().to_string(),
        attempts: 4,
        why: "database is locked".into(),
    }
    .into();
    assert!(matches!(
        core,
        zenman_core::error::CoreError::DatabaseBusy { attempts: 4, .. }
    ));
    assert_eq!(core.kind(), "database_busy");
    assert!(
        !core.is_expected(),
        "the user must be told the index is unavailable"
    );
    let e = sql::open_with_retry(
        &dir,
        StoreMode::Full,
        4,
        std::time::Duration::from_millis(1),
    )
    .expect_err("a directory is not a database");
    assert!(
        !matches!(e, DbOpenError::Locked { .. }),
        "a non-database must never look retryable: {e:?}"
    );
    drop(conn);
    cleanup(&dir);
}

#[test]
fn the_store_reports_the_same_page_the_http_handler_would_have() {
    let log = golden_log();
    let dir = temp_dir("page");
    let path = dir.join("action.log");
    fs::write(&path, &log).expect("copy");
    let store = ActionLogStore::open(&path).expect("open");
    // `limit` clamping is the service layer's job (plan §5.3), the shape is ours: 5 records, newest
    // first, and the same `{count, records}` body `log-limit1.json` captured.
    let page: ActionLog = store.query_recent(5).expect("page");
    assert_eq!(page.count, 5);
    assert_eq!(page.entries()[0].id, 19);
    assert_eq!(page.entries()[4].id, 15);
    let one = golden_json("log-limit1.json");
    assert_eq!(one["count"].as_u64().expect("count"), 1);
    let text = serde_json::to_string(&ActionLog::new(vec![page.entries()[0].clone()]))
        .expect("serialisable");
    let ours: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    assert_eq!(ours["records"][0]["id"], one["records"][0]["id"]);
    assert_eq!(ours["records"][0]["name"], one["records"][0]["name"]);
    assert_eq!(ours["records"][0]["action_str"], "paste-link");
    // Key order is a struct-order fact, so it is checked on the text, not the `Value` map.
    assert!(
        text.starts_with(r#"{"count":1,"records":[{"id":19,"timestamp":"#),
        "{text}"
    );
    fs::remove_file(&path).ok();
    fs::remove_dir(&dir).ok();
}

#[test]
fn the_golden_log_is_never_written_to() {
    // A guard against a future test taking the golden as its working file.
    let path = golden("action.log");
    let before = fs::metadata(&path).expect("stat").len();
    let store = ActionLogStore::open_with(
        &path,
        &zenman_core::history::store::LogOpenOptions::read_only(),
        &zenman_core::limits::Limits::default(),
    )
    .expect("read-only open of the golden works");
    assert_eq!(store.record_count(), 19);
    assert_eq!(fs::metadata(&path).expect("stat").len(), before);
    assert!(Path::new(GOLDEN).join("zen-man.db").is_file());
}
