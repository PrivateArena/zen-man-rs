//! The `ZMLG` undo-log frame codec (plan §5.4 `history/codec.rs` row).
//!
//! # The wire format, as the Go binary actually writes it
//!
//! `api/log.go:246-300` (`encodeRecord`) and `:470-560` (`readRecord`), cross-checked byte for byte
//! against `tests/golden/action.log` (19 records, 7 661 bytes, all CRCs valid — see
//! `tests/golden/LOG-RECORDS.json` and `tests/contract_history.rs`):
//!
//! ```text
//! 0   4   magic  "ZMLG" = [0x5A,0x4D,0x4C,0x47]
//! 4   8   id         i64 BE  (monotonic, assigned by the store, never by the encoder's caller)
//! 12  8   timestamp  i64 BE  (Unix **nanoseconds**)
//! 20  1   action     u8      (`ActionType`, any byte; 1..=8 are named)
//! 21  1   status     u8      (`LogStatus`; 0 done, 1 reverted)
//! 22  4   recLen     u32 BE  = 26 + body, i.e. magic+header+body, **excluding** the CRC
//! 26      body       numSrc u16 BE, then [u16 BE len][raw bytes] per source, then dest, then name
//! recLen  4   crc32   u32 BE, IEEE, over every preceding byte of the record
//! ```
//!
//! There is no file header, no record-count field, no end-of-file sentinel and no padding: the file is
//! a bare concatenation of the frames above and its size *is* the log. An empty log is a zero-byte
//! file (or a file that does not exist — `api/log.go:143` opens with `O_CREATE`). Every integer is
//! **big-endian**, including the CRC, which is why `crc_of` returns the IEEE value but stores it BE.
//!
//! # Deliberations that are decided by the bytes, not by taste
//!
//! * **No canonicalisation.** `encode` writes exactly the fields it is given, in this order, with no
//!   normalisation of paths, no trimming, no re-sorting of `sources`, and it re-derives only the CRC
//!   and `recLen` (both of which are functions of the other bytes). `tests/contract_history.rs`
//!   asserts `decode → encode` reproduces the whole golden file byte for byte, which is the plan's
//!   step-5 acceptance gate.
//! * `readRecord` ignores any bytes left at the end of `body` after `name` (`api/log.go:534-559` never
//!   checks `p == len(body)`). Such a frame is legal to Go and would survive a CRC check, so `decode`
//!   reports them as [`Decoded::ignored_tail`] instead of rejecting the record — and says plainly that
//!   re-encoding it is **not** byte-identical (the tail is dropped). Go itself never writes one.
//! * Path fields are raw bytes in Go (`string(body[p:p+n])` is a byte slice, not a validated UTF-8
//!   string). Rust cannot keep invalid UTF-8 in a `String`, so decode is **lossy** and counts the
//!   substitutions in [`Decoded::lossy_fields`]. A log containing such a path therefore cannot
//!   round-trip byte-identically; that is a Rust representation limit, recorded rather than hidden.
//! * Timestamps carry no zone. `api/log.go:526` rebuilds `time.Unix(0, tsNano)`, which adopts the
//!   *reading* process's local zone, and `log.json` renders `+07:00` only because that was the capture
//!   host's zone. [`ActionRecord::to_json`] therefore emits the same instant in UTC with a `Z` suffix;
//!   the nanoseconds are identical, and that is the only zone-free comparison the format permits.
//! * Unknown `action`/`status` bytes are preserved verbatim: [`ActionType`] and [`LogStatus`] are
//!   newtypes over `u8` rather than the 8-variant `#[repr(u8)]` enum plan §5.4 sketches, because Go
//!   decodes the raw byte (`api/log.go:486-487`), yields `""` for an unlisted action name and `"done"`
//!   for any status but 1, and a re-encode of such a record must still be byte-identical.
//! * Field lengths are `u16` bytes, so 65 535 is the hard cap per path (`api/log.go:249-257` errors
//!   past it); the source *count* is also `u16`. Encode refuses to truncate silently: that would be a
//!   silent undo-data loss.
//!
//! [`CoreError::LogCorrupt`] carries the frame's file offset where the caller knows it, which is what
//! plan §6.1 F4 needs to point a user at the damaged region.

use chrono::Timelike as _;
use std::fmt::Write as _;

use serde::ser::SerializeStruct as _;

use crate::error::CoreError;

/// The 4-byte frame sentinel: `"ZMLG"` (`api/log.go:18`).
pub const MAGIC: [u8; 4] = [0x5A, 0x4D, 0x4C, 0x47];
/// `magic + id + timestamp + action + status + recLen` (`logHeaderFixed`, `api/log.go:86`).
pub const HEADER_FIXED: usize = 26;
/// Byte index of the status byte inside a frame (`statusByteOff`, `api/log.go:87`).
pub const STATUS_BYTE_OFF: usize = 21;
/// Byte index of the trailing CRC inside a frame.
pub const CRC_LEN: usize = 4;
/// Largest legal length of one path field (`u16` length prefix, `api/log.go:249-257`).
pub const MAX_FIELD: usize = u16::MAX as usize;
/// Largest legal source count.
pub const MAX_SOURCES: usize = u16::MAX as usize;
/// `readRecord`'s plausibility bound on the body (`api/log.go:497`): anything larger is treated as
/// corruption rather than read, so a bogus `recLen` cannot make the reader allocate wildly.
pub const MAX_BODY: usize = 10 * 1024 * 1024;
/// Go's zero `time.Time` in **seconds** (`-62135596800`, i.e. year 1). It appears in the wire format
/// of *listings* (`docs/contracts.md` → *fileMode strings*: a failed `entry.Info()` reports this
/// `mod_time`), never in the undo log, whose timestamps are nanoseconds. Kept here because plan §5.4
/// names it and step 9's `ops/list.rs` needs the same constant; see `entry.rs` for the consumer.
pub const GO_ZERO_TIME_UNIX: i64 = -62_135_596_800;

/// Offsets inside the fixed header.
mod off {
    pub const ID: usize = 4;
    pub const TIMESTAMP: usize = 12;
    pub const ACTION: usize = 20;
    pub const STATUS: usize = 21;
    pub const REC_LEN: usize = 22;
}

/// Compile-time CRC-32/IEEE (polynomial reflected `0xEDB88320`) table, matching Go's
/// `hash/crc32.ChecksumIEEE` (`api/log.go:296`). Hand-rolled rather than pulled from `crc32fast`
/// because plan §5.4 lists that crate but the step-5 brief forbids new dependencies, and this is the
/// one checksum in the engine.
const CRC_TABLE: [u32; 256] = {
    // `const` block evaluation: no `unsafe`, no lazy init, no allocation.
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

/// CRC-32/IEEE of `data` — the value Go stores in the last 4 bytes of a frame, big-endian.
#[must_use]
pub fn crc_of(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc = (crc >> 8) ^ CRC_TABLE[((crc ^ u32::from(byte)) & 0xFF) as usize];
    }
    !crc
}

/// Does `frame` (including its trailing CRC) verify? Port of `verifyCRC` (`api/log.go:583-589`).
#[must_use]
pub fn verify_crc(frame: &[u8]) -> bool {
    if frame.len() <= CRC_LEN {
        return false;
    }
    let stored = u32::from_be_bytes([
        frame[frame.len() - 4],
        frame[frame.len() - 3],
        frame[frame.len() - 2],
        frame[frame.len() - 1],
    ]);
    crc_of(&frame[..frame.len() - 4]) == stored
}

/// The class of file operation a record describes (`api/log.go:22-33`).
///
/// A newtype over `u8`, not an enum — see the module docs for why. Use the associated constants for
/// the eight values Go knows; anything else round-trips and renders as the empty action name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ActionType(pub u8);

impl ActionType {
    /// Clipboard set — no file mutation, not reversible (`ActionCopy`).
    pub const COPY: Self = Self(1);
    /// Files duplicated to dest — reversible by deleting the copies (`ActionPasteCopy`).
    pub const PASTE_COPY: Self = Self(2);
    /// Files relocated to dest — reversible by moving them back (`ActionPasteMove`).
    pub const PASTE_MOVE: Self = Self(3);
    /// Permanent delete — **never** reversible (`ActionDelete`).
    pub const DELETE: Self = Self(4);
    /// Renamed in place — reversible (`ActionRename`).
    pub const RENAME: Self = Self(5);
    /// Directory created — reversible (`ActionMkdir`).
    pub const MKDIR: Self = Self(6);
    /// Permissions changed — not reversible (`ActionChmod`).
    pub const CHMOD: Self = Self(7);
    /// Symlinks created at dest — reversible (`ActionPasteLink`).
    pub const PASTE_LINK: Self = Self(8);

    /// The eight known values, in numeric order: the iteration order of Go's `actionNames` map has no
    /// meaning, so the port fixes one for UI lists and tests.
    pub const KNOWN: [Self; 8] = [
        Self::COPY,
        Self::PASTE_COPY,
        Self::PASTE_MOVE,
        Self::DELETE,
        Self::RENAME,
        Self::MKDIR,
        Self::CHMOD,
        Self::PASTE_LINK,
    ];

    /// Go's `actionNames` lookup: the empty string for an unknown byte (`api/log.go:35-44`+`:528`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self.0 {
            1 => "copy",
            2 => "paste-copy",
            3 => "paste-move",
            4 => "delete",
            5 => "rename",
            6 => "mkdir",
            7 => "chmod",
            8 => "paste-link",
            _ => "",
        }
    }

    /// `isReversible` (`api/log.go:596-602`). Exactly these five, and nothing else, may be reverted.
    #[must_use]
    pub const fn is_reversible(self) -> bool {
        matches!(self.0, 2 | 3 | 5 | 6 | 8)
    }
}

impl std::fmt::Display for ActionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = self.name();
        if name.is_empty() {
            write!(f, "action({})", self.0)
        } else {
            f.write_str(name)
        }
    }
}

/// The lifecycle of a record (`api/log.go:46-50`). Newtype for the same reason as [`ActionType`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LogStatus(pub u8);

impl LogStatus {
    /// Completed (`StatusDone`).
    pub const DONE: Self = Self(0);
    /// Already undone (`StatusReverted`) — the only value `status_str` renders specially.
    pub const REVERTED: Self = Self(1);

    /// Go's `statusStr`: `"reverted"` for 1 and **`"done"` for every other byte**
    /// (`api/log.go:561-564`), so an unknown status is not reported as unknown.
    #[must_use]
    pub const fn name(self) -> &'static str {
        if self.0 == 1 { "reverted" } else { "done" }
    }
}

impl std::fmt::Display for LogStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// One decoded undo-log record (Go's `ActionRecord`, `api/log.go:53-64`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionRecord {
    /// Monotonic id assigned at append time (`id`); also the value `/api/log/revert` selects on.
    pub id: i64,
    /// `time.Unix(0, ts)` of the append, in nanoseconds. Normalised to UTC on decode — the frame has
    /// no zone (module docs).
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// What happened.
    pub action: ActionType,
    /// Whether it was already undone.
    pub status: LogStatus,
    /// Absolute source paths. Empty for `mkdir`; **never** absent (Go emits `[]`, `log.json` id 1).
    pub sources: Vec<String>,
    /// Destination directory (`""` when Go logged none, e.g. `rename`/`chmod`/`copy`).
    pub dest: String,
    /// New name for `rename` (a JSON array string for a batch rename — golden record 14), the mode
    /// text for `chmod` (golden records 9/10: `"600"`, `"0644"`), the directory name for `mkdir`, and
    /// `""` otherwise.
    pub name: String,
}

impl ActionRecord {
    /// Go's `action_str`.
    #[must_use]
    pub fn action_str(&self) -> &'static str {
        self.action.name()
    }

    /// Go's `status_str`.
    #[must_use]
    pub fn status_str(&self) -> &'static str {
        self.status.name()
    }

    /// Go's `reversible` (`api/log.go:600`): derived from `action`, never stored.
    #[must_use]
    pub fn is_reversible(&self) -> bool {
        self.action.is_reversible()
    }

    /// The id type the rest of the engine keys on (plan §5.4 `LogId`).
    #[must_use]
    pub fn id_key(&self) -> i64 {
        self.id
    }

    /// Wire size of this record: `recLen + 4`.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        body_len_of(&self.sources, &self.dest, &self.name) + HEADER_FIXED + CRC_LEN
    }

    /// The `/api/log` record shape: Go's field-declaration order (`id, timestamp, action, action_str,
    /// status, status_str, sources, dest, name, reversible`, `api/log.go:53-64` — confirmed in
    /// `log.json`), with `sources` always an array.
    ///
    /// Key order is *not* observable in the returned [`serde_json::Value`] (its map is sorted); the
    /// order is carried by [`ActionRecord`]'s `serde::Serialize` impl, so assert the order on
    /// `serde_json::to_string` output — `contract_history.rs` does.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

impl serde::Serialize for ActionRecord {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut st = serializer.serialize_struct("ActionRecord", 10)?;
        st.serialize_field("id", &self.id)?;
        st.serialize_field("timestamp", &GoNano(&self.timestamp))?;
        st.serialize_field("action", &self.action.0)?;
        st.serialize_field("action_str", &self.action_str())?;
        st.serialize_field("status", &self.status.0)?;
        st.serialize_field("status_str", &self.status_str())?;
        st.serialize_field("sources", &self.sources)?;
        st.serialize_field("dest", &self.dest)?;
        st.serialize_field("name", &self.name)?;
        st.serialize_field("reversible", &self.is_reversible())?;
        st.end()
    }
}

/// Go's `time.Time` JSON rendering: RFC 3339 with nanoseconds, trailing zeros of the fraction removed
/// and the fraction dropped entirely when zero (`RFC3339Nano`), rendered in UTC.
struct GoNano<'a>(&'a chrono::DateTime<chrono::Utc>);

impl std::fmt::Display for GoNano<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let naive = self.0.naive_utc();
        write!(f, "{}", naive.format("%Y-%m-%dT%H:%M:%S"))?;
        let nanos = self.0.nanosecond(); // already the sub-second part, 0..1e9
        if nanos == 0 {
            return f.write_str("Z");
        }
        let mut frac = nanos;
        let mut digits = 9;
        while frac.is_multiple_of(10) {
            frac /= 10;
            digits -= 1;
        }
        write!(f, ".{frac:0digits$}Z")
    }
}

impl serde::Serialize for GoNano<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// The `{count, records}` body of `GET /api/log` (`api/log.go:690-693` — a `map[string]interface{}`,
/// hence alphabetised keys, and `records` is forced to `[]` at `:685-687`).
///
/// Plan §5.4 calls this wrapper's accessor `entries()`; the *wire* name is `records`, and there is no
/// `total` key anywhere in the frozen goldens (`log.json` has exactly `count` and `records`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActionLog {
    /// Number of records in this page (`count`).
    pub count: usize,
    /// The records, newest first as Go returns them (`api/log.go:527` reverses).
    pub records: Vec<ActionRecord>,
}

impl ActionLog {
    /// Build the wrapper, setting `count` from `records` so the two can never disagree.
    #[must_use]
    pub fn new(records: Vec<ActionRecord>) -> Self {
        Self {
            count: records.len(),
            records,
        }
    }

    /// The records (plan §5.4's name for the accessor).
    #[must_use]
    pub fn entries(&self) -> &[ActionRecord] {
        &self.records
    }

    /// `true` when the page is empty — the case where Go still emits `[]`, never `null`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// The response body value: `{"count":N,"records":[…]}`.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

impl serde::Serialize for ActionLog {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut st = serializer.serialize_struct("ActionLog", 2)?;
        st.serialize_field("count", &self.count)?;
        st.serialize_field("records", &self.records)?;
        st.end()
    }
}

/// A record to append, before the store has given it an id and a timestamp — Go's
/// `Append(action, sources, dest, name)` argument list (`api/log.go:226`), and plan §5.4's
/// `DraftRecord` (named `ActionEntry` here, per the step-5 brief).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionEntry {
    /// What happened.
    pub action: ActionType,
    /// Sources, in the order the operation performed them.
    pub sources: Vec<String>,
    /// Destination directory, `""` when there is none.
    pub dest: String,
    /// New name / mode text / directory name, `""` when unused.
    pub name: String,
}

impl ActionEntry {
    /// Go's argument order: action, sources, dest, name.
    #[must_use]
    pub fn new(
        action: ActionType,
        sources: Vec<String>,
        dest: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            action,
            sources,
            dest: dest.into(),
            name: name.into(),
        }
    }

    /// The `mkdir` shape the capture exercised: no sources, dest + name (`op-mkdir` → golden record 1).
    #[must_use]
    pub fn mkdir(dest: impl Into<String>, name: impl Into<String>) -> Self {
        Self::new(ActionType::MKDIR, Vec::new(), dest, name)
    }
}

/// Byte length of a body: `numSrc + Σ(2+len) + (2+dest) + (2+name)`.
fn body_len_of(sources: &[String], dest: &str, name: &str) -> usize {
    2 + sources.iter().map(|s| 2 + s.len()).sum::<usize>() + 2 + dest.len() + 2 + name.len()
}

fn check_field(what: &'static str, bytes: usize, max: usize) -> Result<(), CoreError> {
    if bytes > max {
        return Err(CoreError::CapacityExceeded { what, limit: max });
    }
    Ok(())
}

/// Append one encoded frame to `out`, returning its total length in bytes (Go's `totalSize`).
///
/// `recLen` and the CRC are derived; nothing else is touched, and no field is normalised — that is the
/// byte-identity promise in the module docs.
///
/// # Errors
///
/// [`CoreError::CapacityExceeded`] when a path field or the source count exceeds 65 535, which is what
/// `api/log.go:249-257,278-280` returns as an error (Go's `Append` then surfaces it to the handler;
/// nothing is written).
pub fn encode(out: &mut Vec<u8>, record: &ActionRecord) -> Result<usize, CoreError> {
    check_field("source path", record.dest.len(), MAX_FIELD)?;
    check_field("action log name", record.name.len(), MAX_FIELD)?;
    for src in &record.sources {
        check_field("action log source", src.len(), MAX_FIELD)?;
    }
    check_field("action log sources", record.sources.len(), MAX_SOURCES)?;

    let body_len = body_len_of(&record.sources, &record.dest, &record.name);
    // Go checks the body bound only on read (`api/log.go:497`); the port checks it on write too so a
    // hand-built record cannot produce a frame its own reader would refuse.
    check_field("action log record body", body_len, MAX_BODY)?;
    let rec_len =
        u32::try_from(HEADER_FIXED + body_len).map_err(|_| CoreError::CapacityExceeded {
            what: "action log record",
            limit: MAX_BODY + HEADER_FIXED,
        })?;

    let start = out.len();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&record.id.to_be_bytes());
    let nanos = record
        .timestamp
        .timestamp_nanos_opt()
        .ok_or_else(|| CoreError::LogCorrupt {
            offset: 0,
            reason: "timestamp is out of i64 nanosecond range".to_string(),
        })?;
    out.extend_from_slice(&nanos.to_be_bytes());
    out.push(record.action.0);
    out.push(record.status.0);
    out.extend_from_slice(&rec_len.to_be_bytes());
    out.extend_from_slice(&(record.sources.len() as u16).to_be_bytes());
    for src in &record.sources {
        out.extend_from_slice(&(src.len() as u16).to_be_bytes());
        out.extend_from_slice(src.as_bytes());
    }
    out.extend_from_slice(&(record.dest.len() as u16).to_be_bytes());
    out.extend_from_slice(record.dest.as_bytes());
    out.extend_from_slice(&(record.name.len() as u16).to_be_bytes());
    out.extend_from_slice(record.name.as_bytes());
    let frame_len = out.len() - start;
    let crc = crc_of(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
    Ok(frame_len + CRC_LEN)
}

/// Encode a fresh `Vec` for one record.
///
/// # Errors
///
/// As [`encode`].
pub fn encode_vec(record: &ActionRecord) -> Result<Vec<u8>, CoreError> {
    let mut buf = Vec::with_capacity(record.encoded_len());
    encode(&mut buf, record)?;
    Ok(buf)
}

/// A decoded frame plus the facts `decode` learned about it that the record type cannot hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decoded {
    /// The record.
    pub record: ActionRecord,
    /// Total frame bytes consumed, CRC included — advance the stream by this.
    pub consumed: usize,
    /// Body bytes after `name` that Go ignores (`api/log.go:534-559`). Non-zero means re-encoding the
    /// record will **not** reproduce the original bytes.
    pub ignored_tail: usize,
    /// Path fields whose bytes were not valid UTF-8 and are stored lossily. Same consequence.
    pub lossy_fields: usize,
    /// Whether the frame's CRC matched. `decode` keeps the frame rather than rejecting it, because
    /// `api/log.go:186-203` reads the frame first and *then* decides what corruption means.
    pub crc_ok: bool,
}

impl Decoded {
    /// The frame's bytes, CRC included.
    #[must_use]
    pub fn frame<'s>(&self, stream: &'s [u8]) -> &'s [u8] {
        &stream[..self.consumed.min(stream.len())]
    }
}

/// Decode the frame at the start of `bytes`, with the frame's own offset as error context.
///
/// # Errors
///
/// [`CoreError::LogCorrupt`] with `offset` set to `offset` when the magic is wrong, the header or body
/// is truncated, `recLen` is implausible, or a length prefix overruns the body. A CRC mismatch is
/// **not** an error here — it is [`Decoded::crc_ok`] = `false`, because Go reads such frames and then
/// decides per-caller (skip in `rebuildIndex`, hard stop in `readAt`).
pub fn decode_at(bytes: &[u8], offset: u64) -> Result<Decoded, CoreError> {
    let corrupt = |reason: &str| CoreError::LogCorrupt {
        offset,
        reason: reason.to_string(),
    };
    if bytes.len() < HEADER_FIXED {
        return Err(corrupt(&format!(
            "header truncated: {} of {HEADER_FIXED} bytes",
            bytes.len()
        )));
    }
    if bytes[..4] != MAGIC {
        return Err(corrupt(&format!(
            "bad magic {:02X?}, expected ZMLG",
            &bytes[..4]
        )));
    }
    let rec_len = u32::from_be_bytes([
        bytes[off::REC_LEN],
        bytes[off::REC_LEN + 1],
        bytes[off::REC_LEN + 2],
        bytes[off::REC_LEN + 3],
    ]) as usize;
    if rec_len < HEADER_FIXED {
        return Err(corrupt(&format!("implausible recLen {rec_len}")));
    }
    let body_len = rec_len - HEADER_FIXED;
    if !(2..=MAX_BODY).contains(&body_len) {
        return Err(corrupt(&format!(
            "implausible recLen {rec_len} (bodyLen {body_len})"
        )));
    }
    let total = rec_len + CRC_LEN;
    if bytes.len() < total {
        return Err(corrupt(&format!(
            "record truncated: {} of {total} bytes",
            bytes.len()
        )));
    }
    let body = &bytes[HEADER_FIXED..rec_len];
    let mut lossy_fields = 0usize;

    let mut p = 0usize;
    let num_src = read_u16(body, &mut p).ok_or_else(|| corrupt("body too short for numSrc"))?;
    let mut sources = Vec::with_capacity(num_src as usize);
    for i in 0..num_src {
        let len = read_u16(body, &mut p)
            .ok_or_else(|| corrupt(&format!("body truncated in source {i}")))?
            as usize;
        let bytes_left = body.len().saturating_sub(p);
        if len > bytes_left {
            return Err(corrupt(&format!("source {i} data truncated")));
        }
        sources.push(take_str(&mut lossy_fields, &body[p..p + len]));
        p += len;
    }
    let dest_len =
        read_u16(body, &mut p).ok_or_else(|| corrupt("body truncated before dest"))? as usize;
    if dest_len > body.len().saturating_sub(p) {
        return Err(corrupt("dest data truncated"));
    }
    let dest = take_str(&mut lossy_fields, &body[p..p + dest_len]);
    p += dest_len;
    let name_len =
        read_u16(body, &mut p).ok_or_else(|| corrupt("body truncated before name"))? as usize;
    if name_len > body.len().saturating_sub(p) {
        return Err(corrupt("name data truncated"));
    }
    let name = take_str(&mut lossy_fields, &body[p..p + name_len]);
    p += name_len;

    let id = read_i64(bytes, off::ID);
    let ts_nanos = read_i64(bytes, off::TIMESTAMP);
    let timestamp = ts_nanos
        .checked_div(1_000_000_000)
        .and_then(|secs| {
            chrono::DateTime::from_timestamp(secs, (ts_nanos - secs * 1_000_000_000) as u32)
        })
        .ok_or_else(|| corrupt("timestamp is out of range"))?;

    Ok(Decoded {
        record: ActionRecord {
            id,
            timestamp,
            action: ActionType(bytes[off::ACTION]),
            status: LogStatus(bytes[off::STATUS]),
            sources,
            dest,
            name,
        },
        consumed: total,
        ignored_tail: body.len() - p,
        lossy_fields,
        crc_ok: verify_crc(&bytes[..total]),
    })
}

/// Decode the frame at the start of `bytes`, reporting offset 0 on failure.
///
/// Returns the record and the number of bytes consumed — the plan §5.4 signature.
///
/// # Errors
///
/// As [`decode_at`].
pub fn decode(bytes: &[u8]) -> Result<(ActionRecord, usize), CoreError> {
    let d = decode_at(bytes, 0)?;
    Ok((d.record, d.consumed))
}

fn read_i64(bytes: &[u8], at: usize) -> i64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[at..at + 8]);
    i64::from_be_bytes(buf)
}

fn read_u16(body: &[u8], p: &mut usize) -> Option<u16> {
    if *p + 2 > body.len() {
        return None;
    }
    let v = u16::from_be_bytes([body[*p], body[*p + 1]]);
    *p += 2;
    Some(v)
}

fn take_str(lossy: &mut usize, raw: &[u8]) -> String {
    match std::str::from_utf8(raw) {
        Ok(s) => s.to_string(),
        Err(_) => {
            *lossy += 1;
            String::from_utf8_lossy(raw).into_owned()
        }
    }
}

/// Flip a frame to [`LogStatus::REVERTED`] and re-seal it, exactly as `MarkReverted`
/// (`api/log.go:355-386`) does in place: status byte at [`STATUS_BYTE_OFF`], then a recomputed CRC
/// over everything before the last four bytes.
///
/// This is the only mutation the format ever allows. The record's `id` and `timestamp` are untouched —
/// the revert **reuses the same action id** rather than appending a new record, which is what makes
/// `/api/log/revert` of the same id answer 409 `already reverted` (golden `err-log-revert-twice.json`).
///
/// # Errors
///
/// [`CoreError::LogCorrupt`] if `frame` is too short, has the wrong magic, or its CRC does not match
/// (patching an already-damaged frame would bake the damage in).
pub fn patch_reverted(frame: &[u8]) -> Result<Vec<u8>, CoreError> {
    if frame.len() < HEADER_FIXED + CRC_LEN || frame[..4] != MAGIC {
        return Err(CoreError::LogCorrupt {
            offset: 0,
            reason: "cannot patch: not a ZMLG frame".to_string(),
        });
    }
    if !verify_crc(frame) {
        return Err(CoreError::LogCorrupt {
            offset: 0,
            reason: "cannot patch: CRC mismatch".to_string(),
        });
    }
    let mut out = frame.to_vec();
    out[STATUS_BYTE_OFF] = LogStatus::REVERTED.0;
    let at = out.len() - CRC_LEN;
    let crc = crc_of(&out[..at]).to_be_bytes();
    out[at..].copy_from_slice(&crc);
    Ok(out)
}

/// Hex-encode `bytes` the way `LOG-HEXDUMP.txt` does, for failure messages and the golden test's
/// diagnostics.
pub fn hexdump(bytes: &[u8]) -> String {
    let mut s = String::new();
    for (i, chunk) in bytes.chunks(16).enumerate() {
        let _ = write!(s, "{i:08x}  ");
        for b in chunk {
            let _ = write!(s, "{b:02x} ");
        }
        for _ in chunk.len()..16 {
            s.push_str("   ");
        }
        s.push('|');
        for b in chunk {
            s.push(if b.is_ascii_graphic() || *b == b' ' {
                *b as char
            } else {
                '.'
            });
        }
        s.push_str("|\n");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> ActionRecord {
        ActionRecord {
            id: 1,
            timestamp: chrono::DateTime::from_timestamp(1_794_991_944, 369_498_928).unwrap(),
            action: ActionType::MKDIR,
            status: LogStatus::DONE,
            sources: vec![],
            dest: "/tmp/zm-fix/ops".into(),
            name: "made-dir".into(),
        }
    }

    #[test]
    fn crc32_matches_the_known_ieee_vector() {
        // The standard check value for "123456789" under CRC-32/IEEE.
        assert_eq!(crc_of(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc_of(b""), 0);
    }

    #[test]
    fn golden_record_1_reencodes_to_the_first_59_bytes_of_the_log() {
        // The first frame of `tests/golden/action.log`, straight from `LOG-HEXDUMP.txt`:
        // 26-byte header + body (`numSrc=0`, dest 15 B, name 8 B) + CRC = 59.
        let bytes = encode_vec(&record()).unwrap();
        assert_eq!(bytes.len(), 59);
        assert_eq!(&bytes[..4], b"ZMLG");
        assert_eq!(
            u32::from_be_bytes([bytes[22], bytes[23], bytes[24], bytes[25]]),
            55
        );
        assert_eq!(bytes[20], 6, "mkdir");
        assert_eq!(bytes[21], 0, "done");
        assert!(verify_crc(&bytes));
        // `status:1` in the golden: the capture's revert. Patching must reach the golden's CRC.
        let reverted = patch_reverted(&bytes).unwrap();
        assert_eq!(reverted[STATUS_BYTE_OFF], 1);
        assert!(verify_crc(&reverted));
    }

    #[test]
    fn decode_round_trips_a_synthetic_frame() {
        let mut rec = record();
        rec.sources = vec!["/a.txt".into(), "/b.txt".into()];
        rec.action = ActionType::PASTE_COPY;
        let bytes = encode_vec(&rec).unwrap();
        let d = decode_at(&bytes, 0).unwrap();
        assert_eq!(d.consumed, bytes.len());
        assert!(d.crc_ok);
        assert_eq!(d.ignored_tail, 0);
        assert_eq!(d.lossy_fields, 0);
        assert_eq!(d.record, rec);
        assert_eq!(encode_vec(&d.record).unwrap(), bytes);
    }

    #[test]
    fn encode_refuses_an_over_long_field_rather_than_truncating() {
        let mut rec = record();
        rec.name = "x".repeat(MAX_FIELD + 1);
        assert!(matches!(
            encode_vec(&rec),
            Err(CoreError::CapacityExceeded {
                limit: MAX_FIELD,
                ..
            })
        ));
        let mut many = record();
        many.sources = vec!["s".into(); MAX_SOURCES + 1];
        assert!(matches!(
            encode_vec(&many),
            Err(CoreError::CapacityExceeded { .. })
        ));
    }

    #[test]
    fn a_full_u16_field_round_trips() {
        let mut rec = record();
        rec.name = "x".repeat(MAX_FIELD);
        let bytes = encode_vec(&rec).unwrap();
        let d = decode_at(&bytes, 0).unwrap();
        assert_eq!(d.record.name.len(), MAX_FIELD);
        assert!(d.crc_ok);
    }

    #[test]
    fn bad_magic_and_bad_crc_are_distinguished() {
        let bytes = encode_vec(&record()).unwrap();
        let mut junk = b"XXXX".to_vec();
        junk.extend_from_slice(&bytes[4..]);
        let e = decode_at(&junk, 7).unwrap_err();
        assert!(e.to_string().contains("bad magic"), "{e}");
        assert!(matches!(e, CoreError::LogCorrupt { offset: 7, .. }));

        let mut flipped = bytes.clone();
        flipped[10] ^= 0xFF;
        let d = decode_at(&flipped, 0).unwrap();
        assert!(!d.crc_ok);
        assert!(patch_reverted(&flipped).is_err());
    }

    #[test]
    fn truncated_and_implausible_frames_are_corrupt() {
        let bytes = encode_vec(&record()).unwrap();
        for cut in [0, 4, 26, bytes.len() - 1] {
            let e = decode_at(&bytes[..cut], 0).unwrap_err();
            assert!(matches!(e, CoreError::LogCorrupt { .. }), "{cut}: {e:?}");
        }
        let mut tiny = bytes.clone();
        tiny[22..26].copy_from_slice(&1u32.to_be_bytes());
        assert!(decode_at(&tiny, 0).is_err());
    }

    #[test]
    fn non_utf8_path_bytes_survive_lossily_and_are_counted() {
        let mut rec = record();
        rec.name = "ok".into();
        let mut bytes = encode_vec(&rec).unwrap();
        // Replace the two name bytes with an invalid UTF-8 sequence is impossible without changing the
        // length prefix, so build the frame by hand.
        let _ = &mut bytes;
        let mut raw: Vec<u8> = Vec::new();
        raw.extend_from_slice(&MAGIC);
        raw.extend_from_slice(&7i64.to_be_bytes());
        raw.extend_from_slice(&1_794_991_944_369_498_928i64.to_be_bytes());
        raw.push(ActionType::CHMOD.0);
        raw.push(LogStatus::DONE.0);
        // numSrc(2) + destLen(2) + dest(0) + nameLen(2) + name(2) = 8
        let body_len = 2 + 2 + 2 + 2;
        raw.extend_from_slice(
            &u32::try_from(HEADER_FIXED + body_len)
                .unwrap()
                .to_be_bytes(),
        );
        raw.extend_from_slice(&0u16.to_be_bytes());
        raw.extend_from_slice(&0u16.to_be_bytes());
        raw.extend_from_slice(&2u16.to_be_bytes());
        raw.extend_from_slice(&[0xFF, 0xFE]);
        let crc = crc_of(&raw);
        raw.extend_from_slice(&crc.to_be_bytes());
        let d = decode_at(&raw, 0).unwrap();
        assert!(d.crc_ok);
        assert_eq!(d.lossy_fields, 1);
        assert_eq!(d.record.name, "\u{fffd}\u{fffd}");
    }

    #[test]
    fn action_and_status_names_follow_go_including_the_unknown_ones() {
        for (a, n) in ActionType::KNOWN.iter().zip([
            "copy",
            "paste-copy",
            "paste-move",
            "delete",
            "rename",
            "mkdir",
            "chmod",
            "paste-link",
        ]) {
            assert_eq!(a.name(), n);
        }
        assert_eq!(ActionType(0).name(), "");
        assert_eq!(ActionType(9).name(), "");
        assert!(!ActionType(9).is_reversible());
        assert_eq!(ActionType::DELETE.name(), "delete");
        assert!(!ActionType::DELETE.is_reversible());
        assert!(!ActionType::COPY.is_reversible());
        assert!(!ActionType::CHMOD.is_reversible());
        for k in [2, 3, 5, 6, 8] {
            assert!(ActionType(k).is_reversible());
        }
        assert_eq!(LogStatus(0).name(), "done");
        assert_eq!(LogStatus(1).name(), "reverted");
        assert_eq!(LogStatus(2).name(), "done", "Go only special-cases 1");
        assert_eq!(ActionType(0).to_string(), "action(0)");
    }

    #[test]
    fn json_shape_is_gos_key_order_and_never_null_sources() {
        let mut rec = record();
        rec.status = LogStatus::REVERTED;
        let text = serde_json::to_string(&rec).unwrap();
        let keys: Vec<&str> = text
            .split('"')
            .filter(|s| {
                [
                    "id",
                    "timestamp",
                    "action",
                    "action_str",
                    "status",
                    "status_str",
                    "sources",
                    "dest",
                    "name",
                    "reversible",
                ]
                .contains(s)
            })
            .collect();
        assert_eq!(
            keys,
            vec![
                "id",
                "timestamp",
                "action",
                "action_str",
                "status",
                "status_str",
                "sources",
                "dest",
                "name",
                "reversible"
            ],
            "{text}"
        );
        assert!(text.contains("\"sources\":[]"), "{text}");
        assert!(
            text.contains("\"timestamp\":\"2026-11-18T08:52:24.369498928Z\""),
            "{text}"
        );
        assert_eq!(rec.status_str(), "reverted");
        assert!(rec.is_reversible());
    }

    #[test]
    fn timestamp_fraction_trims_zeros_like_rfc3339nano() {
        let rec_no_frac = ActionRecord {
            timestamp: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            ..record()
        };
        let v = serde_json::to_value(&rec_no_frac).unwrap();
        assert_eq!(v["timestamp"], serde_json::json!("2023-11-14T22:13:20Z"));
        let rec_milli = ActionRecord {
            timestamp: chrono::DateTime::from_timestamp(1_700_000_000, 500_000_000).unwrap(),
            ..record()
        };
        let v = serde_json::to_value(&rec_milli).unwrap();
        assert_eq!(
            v["timestamp"],
            serde_json::json!("2023-11-14T22:13:20.5Z"),
            "Go's RFC3339Nano trims trailing zeros"
        );
    }

    #[test]
    fn log_wrapper_is_the_measured_count_records_shape() {
        let empty = ActionLog::new(vec![]);
        assert_eq!(empty.to_json(), serde_json::json!({"count":0,"records":[]}));
        assert!(empty.is_empty());
        assert!(empty.entries().is_empty());
        let page = ActionLog::new(vec![record(), record()]);
        assert_eq!(page.count, 2);
        assert_eq!(page.entries().len(), 2);
        let text = serde_json::to_string(&page).unwrap();
        assert!(text.starts_with(r#"{"count":2,"records":["#), "{text}");
    }

    #[test]
    fn hexdump_is_stable_width() {
        let d = hexdump(b"ZMLG");
        assert!(d.starts_with("00000000  5a 4d 4c 47"), "{d}");
        assert!(d.contains("|ZMLG|"));
    }
}
