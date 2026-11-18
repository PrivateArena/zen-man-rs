//! `cargo xtask contracts` — the plan §7 step 2c guard over the frozen Go golden capture.
//!
//! Four assertions, all cheap enough to run on every CI job:
//!
//! 1. `tests/golden/PROVENANCE.json` exists, parses, and names a full Go commit SHA;
//! 2. no `tests/golden/*.json` contains a capture-machine absolute path (the only
//!    file allowed to hold originals is `LOG-PATHS.json`, which exists precisely to
//!    record them, and `action.log`, which is a CRC-sealed binary artefact);
//! 3. every `tests/golden/*.json` parses as JSON, except the two files listed in
//!    [`KNOWN_NOT_JSON`] — which are themselves evidence of a Go wire-format bug;
//! 4. `action.log` starts with the `ZMLG` magic, every record's trailing CRC-32
//!    (IEEE) verifies, and the record count equals `log.json`'s `count`.

use std::error::Error;
use std::path::{Path, PathBuf};

use crate::workspace_root;

use serde_json::Value;

/// Absolute paths that only ever exist on the machine that ran the capture.
const BANNED_PATHS: &[&str] = &[
    "/home/jang",
    "/media/jang",
    "/tmp/zm-fix",
    // The same roots as curl -G --data-urlencode writes them into a query
    // string (see tests/golden/INDEX.json, which records the exact request).
    "%2Ftmp%2Fzm-fix",
    "%2Ftmp%2Fzm-go-home",
    "%2Fhome%2Fjang",
    "%2Fmedia%2Fjang",
];

/// Captured Go responses that are **not** valid JSON. Both are findings, not bugs
/// in this gate: `err-chmod-octaltext.json` is `http.Error` + `fmt.Sprintf` with an
/// unescaped `%v`, so the embedded `strconv` quotes break the body (plan §6.2 row 4),
/// and `action-exec.json` shows the same shape when `sh` is missing.
const KNOWN_NOT_JSON: &[(&str, &str)] = &[
    (
        "err-chmod-octaltext.json",
        "Go emits `Invalid permission mode: strconv.ParseUint: parsing \"0o755\": invalid syntax` \
         with unescaped quotes — invalid JSON by construction (api/ops.go handleChmod)",
    ),
    (
        "archive-stream-plain.txt",
        "not JSON: the /api/archive/stream capture is the raw file payload",
    ),
];

const LOG_MAGIC: [u8; 4] = [0x5A, 0x4D, 0x4C, 0x47]; // "ZMLG"
const LOG_HEADER_FIXED: usize = 26;

pub(crate) fn contracts() -> Result<(), Box<dyn Error>> {
    let root = workspace_root()?;
    let golden = root.join("tests/golden");
    if !golden.is_dir() {
        return Err(format!(
            "contracts: {} is missing — run `bash scripts/capture-golden.sh` (plan §7 step 2)",
            golden.display()
        )
        .into());
    }

    // (1) provenance ------------------------------------------------------------
    let prov_path = golden.join("PROVENANCE.json");
    let prov = read_json(&prov_path)?;
    let sha = prov["go_commit"].as_str().unwrap_or("");
    if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "contracts: PROVENANCE.json must name a 40-hex-digit Go commit SHA, found {sha:?}"
        )
        .into());
    }
    println!("contracts: provenance ok (go_commit {sha})");

    // (2) + (3) scan the goldens ------------------------------------------------
    let mut files: Vec<PathBuf> = std::fs::read_dir(&golden)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    if files.is_empty() {
        return Err("contracts: no golden JSON files found".into());
    }

    let mut parsed = 0usize;
    let mut problems: Vec<String> = Vec::new();
    for path in &files {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        let body = std::fs::read_to_string(path)
            .map_err(|e| format!("contracts: cannot read {name}: {e}"))?;

        if name != "LOG-PATHS.json" {
            for banned in BANNED_PATHS {
                if body.contains(banned) {
                    problems.push(format!("{name}: contains capture-machine path {banned}"));
                }
            }
        }

        if let Some((_, why)) = KNOWN_NOT_JSON.iter().find(|(n, _)| *n == name) {
            println!("contracts: {name} is deliberately not JSON — {why}");
            continue;
        }
        match serde_json::from_str::<Value>(&body) {
            Ok(_) => parsed += 1,
            Err(e) => problems.push(format!("{name}: does not parse as JSON ({e})")),
        }
    }
    if !problems.is_empty() {
        for p in &problems {
            eprintln!("contracts: FAIL {p}");
        }
        return Err(format!("contracts: {} problem(s)", problems.len()).into());
    }
    println!(
        "contracts: {} golden JSON file(s) path-clean and parseable",
        parsed
    );

    // (4) the binary undo log ---------------------------------------------------
    let log = std::fs::read(golden.join("action.log"))
        .map_err(|e| format!("contracts: cannot read tests/golden/action.log: {e}"))?;
    if log.len() < LOG_HEADER_FIXED || log[..4] != LOG_MAGIC {
        return Err("contracts: action.log does not start with the ZMLG magic".into());
    }
    let (records, bad_crc, reverted) = walk_log(&log)?;
    if bad_crc > 0 {
        return Err(format!("contracts: action.log has {bad_crc} record(s) with a bad CRC").into());
    }
    let log_json = read_json(&golden.join("log.json"))?;
    let counted = log_json["count"]
        .as_i64()
        .ok_or("contracts: log.json has no numeric `count`")?;
    if i64::try_from(records).map_err(|_| "records overflow isize")? != counted {
        return Err(format!(
            "contracts: action.log holds {records} record(s) but log.json reports {counted} \
             (log.json is captured with limit=500; a longer log means the capture ran twice \
             against a dirty instance — re-run it against a fresh one)"
        )
        .into());
    }
    println!("contracts: action.log ok — {records} record(s), all CRCs valid, {reverted} reverted");
    if reverted == 0 {
        return Err(
            "contracts: no reverted record in action.log — the capture must exercise /api/log/revert"
                .into(),
        );
    }
    println!("contracts: all golden-fixture gates green");
    Ok(())
}

/// Walk a `ZMLG` log exactly like `api/log.go:rebuildIndex`, returning
/// (record count, bad-CRC count, reverted-record count).
fn walk_log(raw: &[u8]) -> Result<(usize, usize, usize), Box<dyn Error>> {
    let mut off = 0usize;
    let mut records = 0usize;
    let mut bad = 0usize;
    let mut reverted = 0usize;
    while off + LOG_HEADER_FIXED + 4 <= raw.len() {
        if raw[off..off + 4] != LOG_MAGIC {
            off += 1; // Go resyncs one byte at a time
            continue;
        }
        let rec_len = u32::from_be_bytes([
            raw[off + 22],
            raw[off + 23],
            raw[off + 24],
            raw[off + 25],
        ]) as usize;
        if rec_len < LOG_HEADER_FIXED || off + rec_len + 4 > raw.len() {
            off += 1;
            continue;
        }
        let frame = &raw[off..off + rec_len + 4];
        let stored = u32::from_be_bytes([
            frame[rec_len],
            frame[rec_len + 1],
            frame[rec_len + 2],
            frame[rec_len + 3],
        ]);
        if crc32_ieee(&frame[..rec_len]) == stored {
            records += 1;
            if frame[21] == 1 {
                reverted += 1;
            }
        } else {
            bad += 1;
        }
        off += rec_len + 4;
    }
    Ok((records, bad, reverted))
}

/// CRC-32/IEEE (the `hash/crc32` polynomial Go's `crc32.ChecksumIEEE` uses).
fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn read_json(path: &Path) -> Result<Value, Box<dyn Error>> {
    let body = std::fs::read_to_string(path)
        .map_err(|e| format!("contracts: cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&body)
        .map_err(|e| format!("contracts: {} is not JSON: {e}", path.display()).into())
}
