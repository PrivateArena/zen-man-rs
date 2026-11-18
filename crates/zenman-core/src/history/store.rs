//! The append-only undo-log file (plan §5.4 `history/store.rs` row).
//!
//! One owner, one writer: an `ActionLogStore` holds the only handle its file will ever get for the
//! lifetime of the value, so the id counter, the path index and the byte length cannot drift from what
//! is on disk. That strengthens Go's design, which shared one `*os.File` between goroutines and needed a
//! `sync.Mutex` plus positional `WriteAt` to survive it (`api/log.go:96-104`, `:375-383`). In Rust
//! `&mut self` *is* that lock, so every mutating method takes it and the crate needs no
//! `Arc<Mutex<…>>` inside; the plan's `Arc<LogStore>` wrapper is applied by the caller (`ops::OpCtx`,
//! step 9) where sharing across jobs is actually required.
//!
//! # What is on disk
//!
//! Exactly the frames [`codec`] describes, concatenated — no header, no trailer, no end sentinel. A
//! brand new log is a **zero-byte** file (Go's `O_CREATE`, `api/log.go:143`) and an emptied one is too:
//! there is no "empty log" byte pattern to write.
//!
//! # Recovery, and where the port deliberately differs from Go
//!
//! Go's `rebuildIndex` (`api/log.go:165-203`) *skips* a bad-CRC record and keeps reading, and on a bad
//! magic it steps forward one byte at a time hunting for the next frame. It never repairs anything, so a
//! log with a hole keeps serving a history view with a hole in it.
//!
//! This type does both:
//!
//! * [`ActionLogStore::iter_after`] is the Go-parity walk — it skips damaged frames and resyncs on the
//!   magic, so a partially damaged log still yields everything readable (plan §6.1 F4's "never panics"),
//!   and
//! * [`ActionLogStore::resync`] is the repair: it walks a strict prefix, stops at the first frame that
//!   fails its CRC or cannot be parsed, and truncates the file there, leaving one valid frame after
//!   another. Because the log is append-only, the bytes after a damaged frame are the *newest* records;
//!   that trade (the newest go, the integrity of what stays) is exactly why `resync` is an explicit call
//!   and never happens implicitly on open. It also resets `next_id` to max-remaining-id+1, so ids of
//!   discarded records are reused — safe, because those bytes are gone.
//!
//! Opening a damaged log is non-destructive: the damage is reported by [`ActionLogStore::damage`] and
//! the app layer (step 6/9) decides whether to offer `resync`.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::CoreError;
use crate::history::codec::{
    self, ActionEntry, ActionLog, ActionRecord, ActionType, CRC_LEN, MAGIC, MAX_BODY,
    STATUS_BYTE_OFF,
};
use crate::limits::Limits;

/// A record's id, as the log stores and indexes it.
pub type LogId = i64;

/// The half-open byte span a call touched: [`ActionLogStore::append`] returns the span of what it just
/// wrote, which feeds [`ActionLogStore::read_range`] directly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LogRecordRange {
    /// First byte of the first frame.
    pub start: u64,
    /// One past the last byte of the last frame.
    pub end: u64,
}

impl LogRecordRange {
    /// A span of nothing, for a no-op call.
    pub const EMPTY: Self = Self { start: 0, end: 0 };

    /// Byte length of the span.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    /// Whether the span covers no bytes.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// How to open the log file. This is the "`OpenOptions`-style" injection the step-5 brief asks for:
/// the caller (or a test pointing at a temp dir) chooses the policy, and this module never discovers a
/// path of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogOpenOptions {
    /// Create the file and its parent directory if missing (Go's `MkdirAll` + `O_CREATE`,
    /// `api/log.go:134-143`).
    pub create: bool,
    /// Open read-only, so writes fail the way they do on a read-only data directory.
    pub read_only: bool,
}

impl Default for LogOpenOptions {
    fn default() -> Self {
        Self {
            create: true,
            read_only: false,
        }
    }
}

impl LogOpenOptions {
    /// Read an existing log, refusing to create one.
    #[must_use]
    pub const fn read_only() -> Self {
        Self {
            create: false,
            read_only: true,
        }
    }
}

/// Why the strict prefix walk stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogDamage {
    /// Byte offset of the first unusable byte — the truncate point, and the `offset` of the
    /// [`CoreError::LogCorrupt`] the UI shows.
    pub offset: u64,
    /// What was wrong, in words fit to show.
    pub reason: String,
    /// CRC-good frames the walk found *after* this point: what [`ActionLogStore::resync`] discards.
    pub records_after: usize,
    /// Bytes from `offset` to the end of file.
    pub bytes_after: u64,
}

/// What [`ActionLogStore::resync`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResyncReport {
    /// Frames in the surviving prefix.
    pub records_kept: usize,
    /// File size after the truncation.
    pub bytes_kept: u64,
    /// Readable frames that sat past the damage and were therefore discarded.
    pub records_dropped: usize,
    /// Bytes removed from the file.
    pub bytes_dropped: u64,
    /// `Some` when the file actually changed.
    pub damage: Option<LogDamage>,
    /// Id the next append will use.
    pub next_id: LogId,
}

impl ResyncReport {
    /// Whether nothing needed repairing.
    #[must_use]
    pub const fn was_clean(&self) -> bool {
        self.damage.is_none()
    }
}

/// How much history [`ActionLogStore::prune`] keeps. Go has no prune path — the log only ever grows —
/// so this is a port-side addition with no Go bytes to be identical to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PruneLimit {
    /// Keep the newest `n` records and drop the rest, oldest first.
    Newest(usize),
}

/// What [`ActionLogStore::prune`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PruneReport {
    /// Records in the file afterwards.
    pub records_kept: usize,
    /// Records removed.
    pub records_dropped: usize,
    /// Bytes removed.
    pub bytes_dropped: u64,
    /// File size afterwards.
    pub bytes_after: u64,
    /// The next id is never rewound by a prune: pruned ids are not reused by later appends, unlike
    /// ids of records `resync` discarded.
    pub next_id: LogId,
}

/// One indexed frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Frame {
    offset: u64,
    len: u32,
    id: LogId,
}

impl Frame {
    fn end(&self) -> u64 {
        self.offset + u64::from(self.len)
    }
}

/// The result of walking a file's frames.
struct Scan {
    /// Every frame whose CRC verified, in file order (Go parity: bad-CRC frames are skipped, not
    /// fatal).
    frames: Vec<Frame>,
    /// Where a strict prefix walk stops, if it stops before EOF.
    damage: Option<LogDamage>,
    /// File size when the walk ran.
    size: u64,
}

/// The undo log.
#[derive(Debug)]
pub struct ActionLogStore {
    path: PathBuf,
    file: File,
    /// CRC-good frames in file order, so "newest last" and "latest bytes" are the same ordering.
    frames: Vec<Frame>,
    /// Go's `index` map (`api/log.go:205-224`): path → frame offsets, including the synthesised
    /// `dest/base(src)` keys and the `mkdir` `dest/name` key.
    by_path: BTreeMap<String, Vec<u64>>,
    next_id: LogId,
    damage: Option<LogDamage>,
    limits: Limits,
}

impl ActionLogStore {
    /// Open (and by default create) the log at `path`, indexing it with the default [`Limits`].
    ///
    /// # Errors
    ///
    /// [`CoreError::Io`] on any filesystem failure.
    pub fn open(path: &Path) -> Result<Self, CoreError> {
        Self::open_with(path, &LogOpenOptions::default(), &Limits::default())
    }

    /// As [`open`](Self::open), with explicit create/read-only policy and durability settings.
    ///
    /// Scanning is read-only: a damaged log opens successfully and reports it through
    /// [`damage`](Self::damage), the in-process version of Go's "warn and carry on"
    /// (`api/log.go:154-158`). A file that cannot be opened *is* an error here, because unlike Go's
    /// no-op stub log we have a caller that can tell the user.
    pub fn open_with(
        path: &Path,
        options: &LogOpenOptions,
        limits: &Limits,
    ) -> Result<Self, CoreError> {
        if options.create {
            prepare_parent(path)?;
        }
        let file = match open_options(options).open(path) {
            Ok(f) => f,
            Err(e) => return Err(CoreError::io(display_path(path), e)),
        };
        let mut me = Self {
            path: path.to_path_buf(),
            file,
            frames: Vec::new(),
            by_path: BTreeMap::new(),
            next_id: 1,
            damage: None,
            limits: limits.clone(),
        };
        me.reindex()?;
        Ok(me)
    }

    /// Create the file, and its parent directory, if they do not exist; leave an existing file alone.
    ///
    /// This is the "will persistence actually work here" probe the session start (step 6) wants before
    /// committing to [`crate::store::config::StoreMode::Full`]. The empty file it leaves *is* the
    /// correct empty log — see the module docs.
    ///
    /// # Errors
    ///
    /// [`CoreError::Io`] when the path cannot be created (read-only filesystem, `EACCES`, `ENOSPC`).
    pub fn ensure_persisted(path: &Path) -> Result<(), CoreError> {
        prepare_parent(path)?;
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
        {
            Ok(f) => f,
            Err(e) => return Err(CoreError::io(display_path(path), e)),
        };
        // Make the directory entry durable: an un-synced `O_CREATE` can vanish along with it. A
        // filesystem that refuses to sync an empty file is not a reason to call a working path broken.
        let _ = file.sync_all();
        Ok(())
    }

    /// Where this log lives.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// File size in bytes — the whole log, damaged bytes included.
    ///
    /// # Errors
    ///
    /// [`CoreError::Io`] if the OS refuses to stat the open handle.
    pub fn len(&self) -> Result<u64, CoreError> {
        match self.file.metadata() {
            Ok(meta) => Ok(meta.len()),
            Err(e) => Err(CoreError::io(display_path(&self.path), e)),
        }
    }

    /// A fresh, independent handle to the same file (`File::try_clone`, so no new path is opened and a
    /// test's temp-dir isolation is preserved). This is the `Seek`/`Read`/`Write` the step-5 brief wants
    /// injected: a caller can hold one while this value appends.
    ///
    /// # Errors
    ///
    /// [`CoreError::Io`] when the OS refuses to duplicate the descriptor.
    pub fn reader(&self) -> Result<File, CoreError> {
        match self.file.try_clone() {
            Ok(f) => Ok(f),
            Err(e) => Err(CoreError::io(display_path(&self.path), e)),
        }
    }

    /// `true` when the file holds no bytes.
    ///
    /// # Errors
    ///
    /// As [`len`](Self::len).
    pub fn is_empty(&self) -> Result<bool, CoreError> {
        Ok(self.len()? == 0)
    }

    /// Number of intact records currently indexed.
    #[must_use]
    pub fn record_count(&self) -> usize {
        self.frames.len()
    }

    /// The id the next append will assign (Go's `nextID`, `api/log.go:199-201`).
    #[must_use]
    pub fn next_id(&self) -> LogId {
        self.next_id
    }

    /// The damage the prefix walk found, if any: `Some` means [`resync`](Self::resync) has work to do.
    #[must_use]
    pub fn damage(&self) -> Option<&LogDamage> {
        self.damage.as_ref()
    }

    /// Whether the indexed file is clean start to finish.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.damage.is_none()
    }

    /// Append `entries`, assigning ids and timestamps the way Go's `Append` does — one frame each, ids
    /// in order from [`next_id`](Self::next_id), one timestamp per record (`api/log.go:247`).
    ///
    /// Bytes go at the physical end of the file, after any damaged tail (Go parity, `api/log.go:245`),
    /// so recovery always stays a deliberate [`resync`](Self::resync).
    ///
    /// When [`Limits::sync_each_write`] is set the batch is `flush`ed and `sync_all`ed before returning:
    /// plan §6.1 F4's promise that a killed process cannot lose an *acknowledged* record.
    ///
    /// # Errors
    ///
    /// [`CoreError::CapacityExceeded`] for an over-long field (nothing is written), [`CoreError::Io`]
    /// for a write or sync failure — in which case the in-memory index is rebuilt from the file, so it
    /// describes the partial write rather than the intended one.
    pub fn append(&mut self, entries: &[ActionEntry]) -> Result<LogRecordRange, CoreError> {
        if entries.is_empty() {
            return Ok(LogRecordRange::EMPTY);
        }
        let path = display_path(&self.path);
        match self.file.seek(SeekFrom::End(0)) {
            Ok(start) => start,
            Err(e) => return Err(CoreError::io(path, e)),
        };
        let start = match self.file.stream_position() {
            Ok(p) => p,
            Err(e) => return Err(CoreError::io(path, e)),
        };
        let mut appended: Vec<(ActionRecord, usize)> = Vec::with_capacity(entries.len());
        for entry in entries {
            let record = ActionRecord {
                id: self.next_id,
                timestamp: chrono::Utc::now(),
                action: entry.action,
                status: codec::LogStatus::DONE,
                sources: entry.sources.clone(),
                dest: entry.dest.clone(),
                name: entry.name.clone(),
            };
            let frame = codec::encode_vec(&record)?;
            if let Err(e) = self.file.write_all(&frame) {
                // Whatever already left the buffer is real: put the index back in sync with the file
                // before handing the error to the caller.
                drop(self.file.flush());
                self.reindex()?;
                return Err(CoreError::io(display_path(&self.path), e));
            }
            self.next_id += 1;
            appended.push((record, frame.len()));
        }
        if let Err(e) = self.file.flush() {
            return Err(CoreError::io(display_path(&self.path), e));
        }
        if self.limits.sync_each_write {
            self.file
                .sync_all()
                .map_err(|e| CoreError::io(display_path(&self.path), e))?;
        }
        let mut end = start;
        for (record, len) in appended {
            let len64 = u64::try_from(len).unwrap_or(u64::MAX);
            self.push_frame(record, end, len64);
            end += len64;
        }
        Ok(LogRecordRange { start, end })
    }

    /// Append one entry, returning its id and its byte span.
    ///
    /// # Errors
    ///
    /// As [`append`](Self::append).
    pub fn append_entry(
        &mut self,
        entry: &ActionEntry,
    ) -> Result<(LogId, LogRecordRange), CoreError> {
        let id = self.next_id;
        let range = self.append(std::slice::from_ref(entry))?;
        Ok((id, range))
    }

    /// Read the records whose frames lie in `[from, end)` bytes.
    ///
    /// A span that starts or ends inside a frame is refused rather than silently rounded, because the
    /// usual caller is replaying what [`append`](Self::append) just handed back.
    ///
    /// # Errors
    ///
    /// [`CoreError::LogCorrupt`] for an unaligned, reversed or damaged span; `from == end` yields no
    /// records.
    pub fn read_range(&self, from: u64, end: u64) -> Result<Vec<ActionRecord>, CoreError> {
        if from > end {
            return Err(CoreError::LogCorrupt {
                offset: from,
                reason: "range start is after its end".to_string(),
            });
        }
        if from == end {
            return Ok(Vec::new());
        }
        let bytes = self.read_bytes(from, end)?;
        let mut out = Vec::new();
        let mut p = 0usize;
        while p < bytes.len() {
            let abs = from + u64::try_from(p).unwrap_or(u64::MAX);
            let decoded = codec::decode_at(&bytes[p..], abs)?;
            if !decoded.crc_ok {
                return Err(CoreError::LogCorrupt {
                    offset: abs,
                    reason: "CRC mismatch".to_string(),
                });
            }
            out.push(decoded.record);
            p += decoded.consumed;
        }
        Ok(out)
    }

    /// The Go-parity streaming walk from `offset`: every frame whose CRC matches, skipping damaged
    /// frames and resyncing on the next magic (`api/log.go:165-203`).
    ///
    /// # Errors
    ///
    /// [`CoreError::Io`] reading the file, [`CoreError::LogCorrupt`] if `offset` is past EOF.
    pub fn iter_after(&self, offset: u64) -> Result<FrameIter, CoreError> {
        let size = self.len()?;
        if offset > size {
            return Err(CoreError::LogCorrupt {
                offset,
                reason: format!("offset is past the end of file ({size})"),
            });
        }
        Ok(FrameIter::new(self.read_bytes(offset, size)?, offset))
    }

    /// Every intact record, oldest first, taken from the frame index — so a damaged frame is skipped the
    /// way Go's reader skips it, rather than failing the whole read.
    ///
    /// # Errors
    ///
    /// [`CoreError::Io`] reading the file.
    pub fn records(&self) -> Result<Vec<ActionRecord>, CoreError> {
        let mut out = Vec::with_capacity(self.frames.len());
        for frame in self.frames.clone() {
            if let Ok(record) = self.frame_at(frame) {
                out.push(record);
            }
        }
        Ok(out)
    }

    /// The `limit` newest records, newest first — Go's `QueryRecent` (`api/log.go:471-505`) including its
    /// reversal, in the `{count, records}` shape `GET /api/log` answers with.
    ///
    /// # Errors
    ///
    /// As [`records`](Self::records).
    pub fn query_recent(&self, limit: usize) -> Result<ActionLog, CoreError> {
        let mut all = self.records()?;
        let start = all.len().saturating_sub(limit);
        let page = all.split_off(start);
        Ok(ActionLog::new(page.into_iter().rev().collect()))
    }

    /// The `limit` newest records touching `path`, newest first — Go's `QueryPath` over the same
    /// `dest/base(src)` / `dest/name` key synthesis (`api/log.go:205-224`, `:433-452`).
    ///
    /// # Errors
    ///
    /// [`CoreError::Io`] reading the file.
    pub fn query_path(&self, path: &str, limit: usize) -> Result<ActionLog, CoreError> {
        let mut out = Vec::new();
        if let Some(offsets) = self.by_path.get(path) {
            for offset in offsets.iter().rev().take(limit) {
                let found = self
                    .frames
                    .iter()
                    .find(|f| f.offset == *offset)
                    .copied()
                    .and_then(|f| self.frame_at(f).ok());
                if let Some(record) = found {
                    out.push(record);
                }
            }
        }
        Ok(ActionLog::new(out.into_iter().rev().collect()))
    }

    /// One record by id, with its byte offset — Go's `GetByID` (`api/log.go:511-536`), which returns the
    /// offset precisely so the revert can write back in place.
    ///
    /// # Errors
    ///
    /// [`CoreError::NotFound`] when no intact frame carries that id (Go: `"record %d not found"`, which
    /// the handler turns into 404 — `err-log-revert-unknown.json`).
    pub fn get(&self, id: LogId) -> Result<(ActionRecord, u64), CoreError> {
        let frame = self
            .frames
            .iter()
            .find(|f| f.id == id)
            .ok_or_else(|| CoreError::NotFound {
                path: format!("action log record {id}"),
                source_path: None,
            })?;
        let record = self.frame_at(*frame)?;
        Ok((record, frame.offset))
    }

    /// Mark record `id` reverted: patch the status byte and re-seal the CRC in place with exactly two
    /// targeted writes, then re-read the frame and verify it (`api/log.go:355-386`, plan §6.1 F4).
    ///
    /// The frame's id and timestamp are untouched — a revert **reuses the same action id** instead of
    /// appending a new record, which is why a second revert of that id is 409 `already reverted`
    /// (`err-log-revert-twice.json`). The captured golden proves the re-seal: `LOG-RECORDS.json`'s
    /// record 1 has `status:1, crc_ok:true`.
    ///
    /// # Errors
    ///
    /// [`CoreError::NotFound`] for an unknown id; [`CoreError::LogCorrupt`] if the frame does not verify
    /// before or after the patch (patching a damaged frame would bake the damage in).
    pub fn mark_reverted(&mut self, id: LogId) -> Result<ActionRecord, CoreError> {
        let frame = match self.frames.iter().copied().find(|f| f.id == id) {
            Some(f) => f,
            None => {
                return Err(CoreError::NotFound {
                    path: format!("action log record {id}"),
                    source_path: None,
                });
            }
        };
        let record = self.frame_at(frame)?;
        let raw = self.read_bytes(frame.offset, frame.end())?;
        let patched = codec::patch_reverted(&raw).map_err(|_| CoreError::LogCorrupt {
            offset: frame.offset,
            reason: format!("record {id} does not verify; cannot mark reverted"),
        })?;
        // The two writes, in Go's order: the status byte, then the trailing CRC. Nothing else in the
        // frame differs, so nothing else is touched. Go used `WriteAt` because goroutines share its
        // descriptor; with one writer per file, a seek is the same thing.
        let status_at = frame.offset + STATUS_BYTE_OFF as u64;
        let crc_at = frame.end() - CRC_LEN as u64;
        let path = display_path(&self.path);
        if let Err(e) = self.file.seek(SeekFrom::Start(status_at)) {
            return Err(CoreError::io(path, e));
        }
        if let Err(e) = self
            .file
            .write_all(&patched[STATUS_BYTE_OFF..STATUS_BYTE_OFF + 1])
        {
            return Err(CoreError::io(path, e));
        }
        if let Err(e) = self.file.seek(SeekFrom::Start(crc_at)) {
            return Err(CoreError::io(path, e));
        }
        if let Err(e) = self.file.write_all(&patched[frame.len_usize() - CRC_LEN..]) {
            return Err(CoreError::io(path, e));
        }
        if let Err(e) = self.file.flush() {
            return Err(CoreError::io(path, e));
        }
        if self.limits.sync_each_write {
            self.file
                .sync_all()
                .map_err(|e| CoreError::io(path.clone(), e))?;
        }
        // F4: read it back and confirm. A torn write is reported, never silently accepted.
        let reread = self.read_bytes(frame.offset, frame.end())?;
        if !codec::verify_crc(&reread)
            || reread.get(STATUS_BYTE_OFF) != Some(&codec::LogStatus::REVERTED.0)
        {
            return Err(CoreError::LogCorrupt {
                offset: frame.offset,
                reason: "the revert patch did not survive the write".to_string(),
            });
        }
        Ok(ActionRecord {
            status: codec::LogStatus::REVERTED,
            ..record
        })
    }

    /// Repair the file: truncate at the first frame that cannot be parsed or whose CRC fails, then
    /// reindex. This is the corrupt-tail recovery the acceptance test drives with a truncated copy and a
    /// flipped-CRC copy.
    ///
    /// A clean log is not written to at all — no truncation, no mtime churn.
    ///
    /// # Errors
    ///
    /// [`CoreError::Io`] if the file cannot be truncated, synced or re-read.
    pub fn resync(&mut self) -> Result<ResyncReport, CoreError> {
        let scan = self.scan()?;
        let damage = match scan.damage.clone() {
            Some(d) => d,
            None => {
                self.adopt(scan);
                return Ok(ResyncReport {
                    records_kept: self.frames.len(),
                    bytes_kept: self.len()?,
                    records_dropped: 0,
                    bytes_dropped: 0,
                    damage: None,
                    next_id: self.next_id,
                });
            }
        };
        let path = display_path(&self.path);
        if let Err(e) = self.file.set_len(damage.offset) {
            return Err(CoreError::io(path, e));
        }
        if let Err(e) = self.file.sync_all() {
            return Err(CoreError::io(path, e));
        }
        let after = self.scan()?;
        let kept = self.len()?;
        self.adopt(after);
        Ok(ResyncReport {
            records_kept: self.frames.len(),
            bytes_kept: kept,
            records_dropped: damage.records_after,
            bytes_dropped: damage.bytes_after,
            damage: Some(damage),
            next_id: self.next_id,
        })
    }

    /// Keep only the newest `max` records, rewriting the file atomically (`.tmp` + rename — the
    /// `store/config.rs` precedent). Surviving frames keep their bytes, ids and timestamps, so a pruned
    /// log still contains only what Go itself wrote.
    ///
    /// `None` means "no limit" and is a no-op, the way the rest of the engine spells an unset cap.
    ///
    /// # Errors
    ///
    /// [`CoreError::Io`] on any filesystem failure, including the rename; a failed prune leaves the
    /// original file in place.
    pub fn prune(&mut self, max: Option<PruneLimit>) -> Result<PruneReport, CoreError> {
        let size = self.len()?;
        let no_op = |me: &Self, dropped: usize| PruneReport {
            records_kept: me.frames.len(),
            records_dropped: dropped,
            bytes_dropped: 0,
            bytes_after: size,
            next_id: me.next_id,
        };
        let keep = match max {
            Some(PruneLimit::Newest(keep)) => keep,
            None => return Ok(no_op(self, 0)),
        };
        let before = self.frames.len();
        if before <= keep {
            return Ok(no_op(self, 0));
        }
        let cut = self.frames[before - keep].offset;
        let payload = self.read_bytes(cut, size)?;
        let tmp = sibling_tmp(&self.path);
        {
            let mut out = match OpenOptions::new().write(true).create_new(true).open(&tmp) {
                Ok(f) => f,
                Err(e) => return Err(CoreError::io(display_path(&tmp), e)),
            };
            if let Err(e) = out.write_all(&payload) {
                return Err(CoreError::io(display_path(&tmp), e));
            }
            if let Err(e) = out.flush() {
                return Err(CoreError::io(display_path(&tmp), e));
            }
            if let Err(e) = out.sync_all() {
                return Err(CoreError::io(display_path(&tmp), e));
            }
        }
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            drop(std::fs::remove_file(&tmp));
            return Err(CoreError::io(display_path(&self.path), e));
        }
        // The rename replaced our handle's inode: reopen before anything else touches the file.
        self.file = match OpenOptions::new().read(true).write(true).open(&self.path) {
            Ok(f) => f,
            Err(e) => return Err(CoreError::io(display_path(&self.path), e)),
        };
        let scan = self.scan()?;
        let after_bytes = scan.size;
        // `next_id` deliberately survives the prune: these records were *completed*, so their ids must
        // not come round again even though their bytes are gone.
        let keep_id = self.next_id;
        self.adopt(scan);
        self.next_id = keep_id.max(self.next_id);
        Ok(PruneReport {
            records_kept: self.frames.len(),
            records_dropped: before.saturating_sub(self.frames.len()),
            bytes_dropped: size.saturating_sub(after_bytes),
            bytes_after: after_bytes,
            next_id: self.next_id,
        })
    }

    // ── internals ────────────────────────────────────────────────────────────

    /// Read `[from, end)` without disturbing the append cursor (the handle is cloned, which is also what
    /// lets `read_range` work through a shared borrow). Short reads are reported by truncating the
    /// buffer, so a range past EOF yields the bytes that exist and the caller decides.
    fn read_bytes(&self, from: u64, end: u64) -> Result<Vec<u8>, CoreError> {
        if end < from {
            return Err(CoreError::LogCorrupt {
                offset: from,
                reason: "range end is before its start".to_string(),
            });
        }
        let want = usize::try_from(end - from).map_err(|_| CoreError::CapacityExceeded {
            what: "action log read",
            limit: MAX_BODY,
        })?;
        let mut buf = vec![0u8; want];
        let mut file = match self.file.try_clone() {
            Ok(f) => f,
            Err(e) => return Err(CoreError::io(display_path(&self.path), e)),
        };
        if let Err(e) = file.seek(SeekFrom::Start(from)) {
            return Err(CoreError::io(display_path(&self.path), e));
        }
        let mut read = 0usize;
        while read < want {
            match file.read(&mut buf[read..]) {
                Ok(0) => break,
                Ok(n) => read += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(CoreError::io(display_path(&self.path), e)),
            }
        }
        buf.truncate(read);
        Ok(buf)
    }

    /// Walk every frame: skip what cannot be used, resync on the magic, and record where a strict prefix
    /// walk would have stopped.
    fn scan(&self) -> Result<Scan, CoreError> {
        let size = self.len()?;
        let bytes = self.read_bytes(0, size)?;
        let mut frames = Vec::new();
        let mut damage: Option<LogDamage> = None;
        let mut pos = 0usize;
        while pos < bytes.len() {
            let offset = u64::try_from(pos).unwrap_or(u64::MAX);
            match codec::decode_at(&bytes[pos..], offset) {
                Ok(decoded) => {
                    if !decoded.crc_ok {
                        note_damage(&mut damage, offset, "CRC mismatch", size);
                        pos += decoded.consumed.max(1);
                        continue;
                    }
                    frames.push(Frame {
                        offset,
                        len: u32::try_from(decoded.consumed).unwrap_or(u32::MAX),
                        id: decoded.record.id,
                    });
                    pos += decoded.consumed;
                }
                Err(_) => {
                    // Bad magic or a truncated frame: resync forward to the next magic, which is what
                    // `rebuildIndex`'s byte-at-a-time step achieves (`api/log.go:196-202`).
                    note_damage(
                        &mut damage,
                        offset,
                        "no frame here (bad magic or truncated record)",
                        size,
                    );
                    match find_magic(&bytes[pos + 1..]) {
                        Some(delta) => pos += 1 + delta,
                        None => break,
                    }
                }
            }
        }
        if let Some(d) = damage.as_mut() {
            d.records_after = frames.iter().filter(|f| f.offset >= d.offset).count();
        }
        Ok(Scan {
            frames,
            damage,
            size,
        })
    }

    /// Adopt a scan: frame index, path index, `next_id`, damage report.
    fn adopt(&mut self, scan: Scan) {
        let Scan {
            frames,
            damage,
            size: _,
        } = scan;
        let max_id = frames.iter().map(|f| f.id).max().unwrap_or(0);
        self.next_id = max_id + 1;
        self.frames = frames;
        self.damage = damage;
        self.rebuild_path_index();
    }

    /// Re-read the file and rebuild everything from it.
    fn reindex(&mut self) -> Result<(), CoreError> {
        let scan = self.scan()?;
        self.adopt(scan);
        Ok(())
    }

    fn rebuild_path_index(&mut self) {
        self.by_path.clear();
        let frames = self.frames.clone();
        for frame in frames {
            if let Ok(record) = self.frame_at(frame) {
                self.index_record(&record, frame.offset);
            }
        }
    }

    fn push_frame(&mut self, record: ActionRecord, offset: u64, len: u64) {
        let len32 = u32::try_from(len).unwrap_or(u32::MAX);
        if !self.frames.iter().any(|f| f.offset == offset) {
            self.frames.push(Frame {
                offset,
                len: len32,
                id: record.id,
            });
        }
        self.index_record(&record, offset);
    }

    /// Go's `indexRecord` (`api/log.go:206-224`): the sources, the projected destinations, and the
    /// `mkdir` target.
    fn index_record(&mut self, record: &ActionRecord, offset: u64) {
        for src in &record.sources {
            push_offset(&mut self.by_path, src.clone(), offset);
        }
        if record.dest.is_empty() {
            return;
        }
        for src in &record.sources {
            push_offset(
                &mut self.by_path,
                join_basename(&record.dest, base_name(src)),
                offset,
            );
        }
        if record.action == ActionType::MKDIR && !record.name.is_empty() {
            push_offset(
                &mut self.by_path,
                join_basename(&record.dest, &record.name),
                offset,
            );
        }
    }

    fn frame_at(&self, frame: Frame) -> Result<ActionRecord, CoreError> {
        let raw = self.read_bytes(frame.offset, frame.end())?;
        let decoded = codec::decode_at(&raw, frame.offset)?;
        if !decoded.crc_ok {
            return Err(CoreError::LogCorrupt {
                offset: frame.offset,
                reason: "CRC mismatch".to_string(),
            });
        }
        Ok(decoded.record)
    }
}

impl Frame {
    fn len_usize(&self) -> usize {
        usize::try_from(self.len).unwrap_or(usize::MAX)
    }
}

/// `filepath.Join(dir, base(path))` for the log's path index (`api/log.go:211-221`), over
/// `/`-separated strings — which is what the log stores on Linux.
fn join_basename(dir: &str, base: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{base}")
    } else {
        format!("{dir}/{base}")
    }
}

fn base_name(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

fn push_offset(index: &mut BTreeMap<String, Vec<u64>>, key: String, offset: u64) {
    let slot = index.entry(key).or_default();
    if !slot.contains(&offset) {
        slot.push(offset);
    }
}

fn find_magic(bytes: &[u8]) -> Option<usize> {
    bytes.windows(MAGIC.len()).position(|w| w == MAGIC)
}

fn note_damage(damage: &mut Option<LogDamage>, offset: u64, reason: &str, size: u64) {
    if damage.is_none() {
        *damage = Some(LogDamage {
            offset,
            reason: reason.to_string(),
            records_after: 0,
            bytes_after: size.saturating_sub(offset),
        });
    }
}

fn prepare_parent(path: &Path) -> Result<(), CoreError> {
    let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(());
    };
    if let Err(e) = std::fs::create_dir_all(dir) {
        return Err(CoreError::io(display_path(dir), e));
    }
    Ok(())
}

fn open_options(options: &LogOpenOptions) -> OpenOptions {
    let mut o = OpenOptions::new();
    // Go: `O_RDWR|O_CREATE`, deliberately **not** `O_APPEND` (`api/log.go:142`) — appends happen after an
    // explicit `Seek(0, End)` so the same handle can also write status bytes in place.
    o.read(true).write(!options.read_only);
    if options.create {
        o.create(true).truncate(false);
    }
    o
}

fn sibling_tmp(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "action.log".to_string());
    path.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{name}.tmp"))
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
}

/// The Go-parity frame iterator behind [`ActionLogStore::iter_after`].
///
/// It owns its bytes — a snapshot of the file from `offset` on — so a later append cannot move the
/// ground under it. Go's readers lack that property: they share one `*os.File` cursor and can interleave
/// mid-record.
#[derive(Debug)]
pub struct FrameIter {
    buf: Vec<u8>,
    pos: usize,
    base: u64,
    skipped: usize,
}

impl FrameIter {
    fn new(buf: Vec<u8>, base: u64) -> Self {
        Self {
            buf,
            pos: 0,
            base,
            skipped: 0,
        }
    }

    /// Absolute file offset of the next byte that has not been returned.
    #[must_use]
    pub fn position(&self) -> u64 {
        self.base + u64::try_from(self.pos).unwrap_or(u64::MAX)
    }

    /// Frames skipped because they did not verify — Go's silent `continue`, made observable.
    #[must_use]
    pub fn skipped(&self) -> usize {
        self.skipped
    }

    /// Whether the walk consumed the whole snapshot.
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        self.pos >= self.buf.len()
    }
}

impl Iterator for FrameIter {
    type Item = Result<ActionRecord, CoreError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.pos >= self.buf.len() {
                return None;
            }
            let offset = self.base + u64::try_from(self.pos).unwrap_or(u64::MAX);
            match codec::decode_at(&self.buf[self.pos..], offset) {
                Ok(decoded) => {
                    if decoded.crc_ok {
                        self.pos += decoded.consumed;
                        return Some(Ok(decoded.record));
                    }
                    self.skipped += 1;
                    self.pos += decoded.consumed.max(1);
                }
                Err(e) => match find_magic(&self.buf[self.pos + 1..]) {
                    Some(delta) => self.pos += 1 + delta,
                    None => {
                        self.pos = self.buf.len();
                        return Some(Err(e));
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zenman-log-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn entries(n: usize) -> Vec<ActionEntry> {
        (0..n)
            .map(|i| ActionEntry::mkdir("/tmp/dst", format!("dir-{i}")))
            .collect()
    }

    /// The exact frame size of the entries this module's tests append: 26-byte header + body
    /// (`numSrc` 2, dest 2+8, name 2+5) + 4-byte CRC = 49. Derived from the codec, never hand-added, so
    /// the arithmetic below cannot drift from the encoder.
    fn frame_len() -> usize {
        ActionRecord {
            id: 1,
            timestamp: chrono::Utc::now(),
            action: ActionType::MKDIR,
            status: codec::LogStatus::DONE,
            sources: vec![],
            dest: "/tmp/dst".into(),
            name: "dir-0".into(),
        }
        .encoded_len()
    }

    fn mk(dir: &Path, tag: &str, n: usize) -> (PathBuf, ActionLogStore) {
        let path = dir.join(tag);
        std::fs::write(&path, []).expect("fresh file");
        let mut store = ActionLogStore::open(&path).expect("open");
        store.append(&entries(n)).expect("append");
        (path, store)
    }

    #[test]
    fn a_new_log_is_a_zero_byte_file() {
        let dir = tmp_dir("fresh");
        let path = dir.join("nested/deeper/action.log");
        ActionLogStore::ensure_persisted(&path).expect("created");
        assert_eq!(std::fs::metadata(&path).expect("stat").len(), 0);
        let store = ActionLogStore::open(&path).expect("open");
        assert!(store.is_empty().expect("len"));
        assert_eq!(store.record_count(), 0);
        assert_eq!(
            store.next_id(),
            1,
            "Go starts ids at maxID+1 over an empty log"
        );
        assert!(store.is_clean());
        drop(store);
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(dir.join("nested/deeper")).ok();
        std::fs::remove_dir(dir.join("nested")).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn append_then_reopen_matches_the_encoded_frames() {
        let dir = tmp_dir("append");
        let (path, store) = mk(&dir, "action.log", 3);
        let range = LogRecordRange {
            start: 0,
            end: store.len().expect("len"),
        };
        assert_eq!(range.end, 3 * frame_len() as u64);
        assert_eq!(frame_len(), 49);
        assert_eq!(
            store
                .read_range(range.start, range.end)
                .expect("read")
                .len(),
            3
        );
        assert_eq!(store.next_id(), 4);
        drop(store);
        let again = ActionLogStore::open(&path).expect("reopen");
        assert_eq!(again.record_count(), 3);
        assert_eq!(again.next_id(), 4);
        assert!(again.is_clean());
        let page = again.query_recent(2).expect("page");
        assert_eq!(page.count, 2);
        assert_eq!(
            page.entries()[0].id,
            3,
            "newest first, as Go's QueryRecent reverses"
        );
        assert_eq!(again.query_recent(99).expect("all").count, 3);
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn the_path_index_carries_the_mkdir_target_and_the_projected_dest() {
        let dir = tmp_dir("index");
        let (path, mut store) = mk(&dir, "action.log", 0);
        store
            .append(&[
                ActionEntry::mkdir("/dst", "made-dir"),
                ActionEntry::new(
                    ActionType::PASTE_COPY,
                    vec!["/src/a.txt".into()],
                    "/dst",
                    "",
                ),
            ])
            .expect("append");
        // Go indexes `dest/base(src)` (`api/log.go:212-214`) and `dest/name` for mkdir (`:217-220`).
        assert_eq!(store.query_path("/dst/made-dir", 10).expect("q").count, 1);
        assert_eq!(store.query_path("/dst/a.txt", 10).expect("q").count, 1);
        assert_eq!(store.query_path("/src/a.txt", 10).expect("q").count, 1);
        assert_eq!(store.query_path("/nowhere", 10).expect("q").count, 0);
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn mark_reverted_patches_in_place_and_survives_reopen() {
        let dir = tmp_dir("revert");
        let (path, mut store) = mk(&dir, "action.log", 2);
        let size = store.len().expect("len");
        let rec = store.mark_reverted(1).expect("patched");
        assert_eq!(rec.status, codec::LogStatus::REVERTED);
        assert_eq!(rec.id, 1, "the revert reuses the same action id");
        assert_eq!(store.len().expect("size unchanged"), size);
        assert!(store.mark_reverted(99).is_err(), "unknown id is NotFound");
        drop(store);
        let again = ActionLogStore::open(&path).expect("reopen");
        let (r, _) = again.get(1).expect("id 1");
        assert_eq!(r.status_str(), "reverted");
        assert!(again.is_clean(), "the re-sealed CRC verifies");
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn resync_on_a_clean_log_writes_nothing() {
        let dir = tmp_dir("clean-resync");
        let (path, mut store) = mk(&dir, "action.log", 4);
        let before = std::fs::read(&path).expect("read");
        let report = store.resync().expect("resync");
        assert!(report.was_clean());
        assert_eq!(report.records_kept, 4);
        assert_eq!(report.bytes_dropped, 0);
        assert_eq!(std::fs::read(&path).expect("read"), before);
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn resync_truncates_a_partial_tail_frame() {
        let dir = tmp_dir("trunc");
        let (path, store) = mk(&dir, "action.log", 3);
        let whole = std::fs::read(&path).expect("read");
        let boundary = 2 * frame_len();
        assert_eq!(frame_len(), 49, "the documented frame size, checked once");
        std::fs::write(&path, &whole[..boundary + 10]).expect("truncate");
        drop(store);
        let mut store = ActionLogStore::open(&path).expect("open");
        assert!(!store.is_clean());
        let report = store.resync().expect("resync");
        assert_eq!(report.records_kept, 2);
        assert_eq!(report.bytes_kept, boundary as u64);
        assert_eq!(report.bytes_dropped, 10);
        assert_eq!(
            report.records_dropped, 0,
            "the tail was unreadable, not a whole frame"
        );
        assert!(store.is_clean());
        assert_eq!(store.next_id(), 3, "the discarded record's id is reusable");
        let reopened = ActionLogStore::open(&path).expect("reopen");
        assert_eq!(reopened.record_count(), 2);
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn resync_stops_at_a_flipped_crc() {
        let dir = tmp_dir("badcrc");
        let (path, store) = mk(&dir, "action.log", 5);
        let mut bytes = std::fs::read(&path).expect("read");
        // Third frame's `dest` bytes: damage at frame index 2, so the prefix is ids 1 and 2.
        bytes[2 * frame_len() + 30] ^= 0xFF;
        std::fs::write(&path, &bytes).expect("write");
        drop(store);
        let mut store = ActionLogStore::open(&path).expect("open");
        assert!(!store.is_clean());
        // Go-parity reading still sees the good records past the damage.
        let walked: Vec<ActionRecord> = store
            .iter_after(0)
            .expect("iter")
            .filter_map(Result::ok)
            .collect();
        assert_eq!(
            walked.len(),
            4,
            "one frame is unreadable, four survive a skip walk"
        );
        assert_eq!(
            store.record_count(),
            4,
            "and the index skips it the same way"
        );
        let report = store.resync().expect("resync");
        assert_eq!(report.records_kept, 2, "the intact prefix only");
        assert_eq!(report.records_dropped, 2);
        assert_eq!(report.bytes_dropped, 3 * frame_len() as u64);
        assert!(store.is_clean());
        assert_eq!(store.record_count(), 2);
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn the_walk_resyncs_past_garbage_and_reports_it() {
        let dir = tmp_dir("garbage");
        let (path, store) = mk(&dir, "action.log", 2);
        let whole = std::fs::read(&path).expect("read");
        let mut mixed = whole.clone();
        mixed.extend_from_slice(b"not a frame at all");
        mixed.extend_from_slice(&whole);
        std::fs::write(&path, &mixed).expect("write");
        drop(store);
        let store = ActionLogStore::open(&path).expect("open");
        assert_eq!(
            store.record_count(),
            4,
            "the Go-parity index resyncs to the next magic"
        );
        assert!(!store.is_clean());
        assert_eq!(store.damage().expect("d").offset, whole.len() as u64);
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn prune_keeps_the_newest_and_leaves_their_bytes_untouched() {
        let dir = tmp_dir("prune");
        let (path, mut store) = mk(&dir, "action.log", 6);
        let whole = std::fs::read(&path).expect("read");
        let report = store.prune(Some(PruneLimit::Newest(2))).expect("prune");
        assert_eq!(report.records_kept, 2);
        assert_eq!(report.records_dropped, 4);
        assert_eq!(report.bytes_after, 2 * frame_len() as u64);
        assert_eq!(report.bytes_dropped, 4 * frame_len() as u64);
        assert_eq!(report.next_id, 7, "ids are not rewound by a prune");
        let after = std::fs::read(&path).expect("read");
        assert_eq!(
            after,
            whole[whole.len() - after.len()..],
            "the surviving frames keep their exact bytes"
        );
        assert_eq!(store.record_count(), 2);
        assert!(store.get(1).is_err(), "the pruned record is gone");
        assert!(store.get(5).is_ok(), "the newest two remain");
        assert!(!store.is_empty().expect("non-empty"));
        let noop = store.prune(None).expect("no limit is a no-op");
        assert_eq!(noop.records_dropped, 0);
        assert_eq!(noop.records_kept, 2);
        assert!(
            !dir.join("action.log.tmp").exists(),
            "the temp file was renamed away"
        );
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn read_range_is_strict_about_frame_boundaries() {
        let dir = tmp_dir("range");
        let (path, mut store) = mk(&dir, "action.log", 2);
        let range = store.append(&entries(1)).expect("append");
        assert_eq!(
            store
                .read_range(range.start, range.start)
                .expect("empty")
                .len(),
            0
        );
        assert!(
            store.read_range(range.end, range.start).is_err(),
            "reversed"
        );
        assert!(store.read_range(1, range.end).is_err(), "starts mid-frame");
        assert!(
            store.read_range(0, store.len().expect("len")).is_ok(),
            "the whole log"
        );
        assert!(
            store.iter_after(store.len().expect("len")).is_ok(),
            "EOF offset is legal"
        );
        assert!(matches!(
            store.iter_after(store.len().expect("len") + 1),
            Err(CoreError::LogCorrupt { .. })
        ));
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }
}
