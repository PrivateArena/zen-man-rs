//! Listing and streaming members out of a container (plan §5.5's `archive/reader.rs` row).
//!
//! Two rules shape this file:
//!
//! * **never buffer a whole archive.** `list` reads the central directory only; `write_stream` copies one
//!   member into the caller's sink. `read_to_vec` exists because the *nested* case genuinely needs the
//!   member bytes in hand (a `.7z` inside a `.zip` must be seekable), and it is capped.
//! * **every cap is a parameter.** `cap: u64` comes from
//!   [`Limits::nested_archive_max_bytes`](crate::limits::Limits::nested_archive_max_bytes) (plan F8): a
//!   7z bomb is a `CapacityExceeded`, never an OOM.

use std::collections::BTreeMap;
use std::io::{Cursor, Read, Seek, Write};
use std::path::Path;

use crate::archive::ArchiveError;
use crate::archive::detect_name;
use crate::error::CoreError;
use crate::path::ArchiveFormat;

/// One container member, in the shape the manifest and the golden listing need.
///
/// `mtime` is Unix **seconds** (Go's `FileEntry.mod_time`, `api/archive_vfs.go:22-31`). `mode` is the
/// unix mode bits when the container carries them — the only way a zip symlink member is distinguishable
/// at all (`tests/golden/archive-list-symlink.json`, where Go showed a regular file whose size was the
/// target's length).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawEntry {
    /// Cleaned archive-internal path (`a/b.txt`; no leading/trailing `/`, no `./`).
    pub path: String,
    /// Directory member.
    pub is_dir: bool,
    /// Uncompressed size.
    pub size: u64,
    /// Modification time, Unix seconds.
    pub mtime: i64,
    /// Unix mode bits, when the format stores them (`zip`'s external attributes; 7z stores none).
    pub mode: Option<u32>,
    /// The member's payload is encrypted.
    pub encrypted: bool,
}

impl RawEntry {
    /// `true` when the mode says symlink (`S_IFLNK`).
    #[must_use]
    pub fn is_symlink(&self) -> bool {
        self.mode.is_some_and(|m| m & 0o170_000 == 0o120_000)
    }
}

/// A readable container. `Send` because the extract job and the manifest builder both run off the UI
/// thread; `&mut self` because both backends keep decode state inside the reader.
pub trait ArchiveReader: Send {
    /// Every member, in container order, with the internal path cleaned the way
    /// [`crate::path::clean_internal_path`] does not (this is Go's `cleanInternalPath`, see
    /// [`crate::archive::vfs::clean_internal_path`]).
    ///
    /// # Errors
    /// [`CoreError::ArchiveEncrypted`], [`CoreError::UnsupportedArchive`],
    /// [`CoreError::ArchiveCorrupt`] or an I/O error.
    fn list(&mut self) -> Result<Vec<RawEntry>, CoreError>;

    /// Stream one member into `sink`, at most `cap` bytes. Returns the byte count written.
    ///
    /// # Errors
    /// As [`Self::list`], plus [`CoreError::CapacityExceeded`] when the member outgrows `cap` (the stream
    /// stops at that point; nothing already written is rolled back, which is the caller's business).
    fn write_stream(
        &mut self,
        inner: &str,
        sink: &mut dyn Write,
        cap: u64,
    ) -> Result<u64, CoreError>;

    /// Read one member into memory, capped. The nested-archive path.
    fn read_to_vec(&mut self, inner: &str, cap: u64) -> Result<Vec<u8>, CoreError> {
        let mut buf = Vec::new();
        self.write_stream(inner, &mut buf, cap)?;
        Ok(buf)
    }
}

/// Copy `src` into `sink`, refusing to pass `cap` (plan F8's single enforcement point, shared by both
/// backends so a zip and a 7z answer a cap the same way).
fn copy_capped<R: Read + ?Sized>(
    src: &mut R,
    sink: &mut dyn Write,
    cap: u64,
    archive: &str,
    known: u64,
) -> Result<u64, CoreError> {
    if known > cap {
        return Err(CoreError::CapacityExceeded {
            what: "archive bytes",
            limit: usize::try_from(cap).unwrap_or(usize::MAX),
        });
    }
    let mut buf = [0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = src.read(&mut buf).map_err(|e| {
            ArchiveError::Io {
                archive: archive.to_string(),
                reason: e.to_string(),
            }
            .into_core()
        })?;
        if n == 0 {
            return Ok(total);
        }
        total += n as u64;
        if total > cap {
            return Err(CoreError::CapacityExceeded {
                what: "archive bytes",
                limit: usize::try_from(cap).unwrap_or(usize::MAX),
            });
        }
        sink.write_all(&buf[..n]).map_err(|e| ArchiveError::Io {
            archive: archive.to_string(),
            reason: e.to_string(),
        })?;
    }
}

// --------------------------------------------------------------------------- zip

/// Map a `zip` failure onto the core's archive vocabulary.
///
/// The variant match is the honest mapping; the one string test (an `InvalidArchive` text that mentions a
/// password) is there because the backend reports *some* encryption methods that way rather than through
/// [`ZipError::InvalidPassword`]. A misclassification degrades to `ArchiveCorrupt`, never to a wrong
/// `Encrypted`.
fn map_zip_error(e: zip::result::ZipError, archive: &str) -> CoreError {
    use zip::result::ZipError as Z;

    let named = |d: String| ArchiveError::Unsupported {
        archive: archive.to_string(),
        detail: d,
    };
    match e {
        Z::Io(io) => ArchiveError::Io {
            archive: archive.to_string(),
            reason: io.to_string(),
        }
        .into_core(),
        Z::InvalidPassword => ArchiveError::Encrypted {
            archive: archive.to_string(),
        }
        .into_core(),
        // The backend reports "Password required to decrypt file" through this variant, so the text is
        // inspected before the variant name: an encrypted archive must stay `archive_encrypted` (plan
        // §5.5), and everything else this variant carries really is an unsupported format.
        Z::UnsupportedArchive(why) => {
            let text = why.to_ascii_lowercase();
            if text.contains("password") || text.contains("encrypt") {
                ArchiveError::Encrypted {
                    archive: archive.to_string(),
                }
                .into_core()
            } else {
                named(why.to_string()).into_core()
            }
        }
        Z::CompressionMethodNotSupported(m) => {
            named(format!("compression method {m:#04x}")).into_core()
        }
        Z::FileNotFound => CoreError::NotFound {
            path: archive.to_string(),
            source_path: None,
        },
        Z::DecompressedSizeLimitExceeded { limit, .. } => ArchiveError::Capacity {
            archive: archive.to_string(),
            detail: format!("declared size exceeds the {limit} byte cap"),
        }
        .into_core(),
        Z::DecompressedSizeUnknown => ArchiveError::Capacity {
            archive: archive.to_string(),
            detail: "declared size is unknown (data descriptors)".to_string(),
        }
        .into_core(),
        Z::InvalidArchive(reason) => {
            let text = reason.to_ascii_lowercase();
            if text.contains("password") || text.contains("encrypt") {
                ArchiveError::Encrypted {
                    archive: archive.to_string(),
                }
                .into_core()
            } else {
                ArchiveError::Corrupt {
                    archive: archive.to_string(),
                    reason: reason.to_string(),
                }
                .into_core()
            }
        }
        // `ZipError` is `#[non_exhaustive]`: an unknown future variant is reported as a corrupt
        // archive, which is the honest default for "we cannot say what this was".
        _ => ArchiveError::Corrupt {
            archive: archive.to_string(),
            reason: e.to_string(),
        }
        .into_core(),
    }
}

/// The zip backend over the `zip` crate (MIT, plan §10 item 3).
pub struct ZipReader<R> {
    archive: zip::ZipArchive<R>,
    name: String,
    /// cleaned internal path -> container index; built on first use, because the manifest and the
    /// extract job both list before they read, and reading by *name* would otherwise re-scan.
    index: BTreeMap<String, usize>,
}

impl<R: Read + Seek + Send> ZipReader<R> {
    /// Wrap an opened zip archive. `name` is the archive's display name, for error messages.
    pub fn new(archive: zip::ZipArchive<R>, name: String) -> Self {
        Self {
            archive,
            name,
            index: BTreeMap::new(),
        }
    }

    fn ensure_index(&mut self) -> Result<(), CoreError> {
        if !self.index.is_empty() {
            return Ok(());
        }
        for i in 0..self.archive.len() {
            // `by_index_raw` never decrypts, so an encrypted member lists instead of failing — which is
            // what the pane needs (a row it can grey out), and what makes `encrypted` observable.
            let file = self
                .archive
                .by_index_raw(i)
                .map_err(|e| map_zip_error(e, &self.name))?;
            let Ok(path) = file.name() else {
                continue;
            };
            let cleaned = crate::archive::vfs::clean_internal_path(&path);
            if !cleaned.is_empty() {
                self.index.insert(cleaned, i);
            }
        }
        Ok(())
    }

    fn locate(&mut self, inner: &str) -> Result<usize, CoreError> {
        self.ensure_index()?;
        let want = crate::archive::vfs::clean_internal_path(inner);
        self.index
            .get(&want)
            .copied()
            .ok_or_else(|| CoreError::NotFound {
                path: format!("{}{want}", self.name),
                source_path: None,
            })
    }
}

impl<R: Read + Seek + Send> ArchiveReader for ZipReader<R> {
    fn list(&mut self) -> Result<Vec<RawEntry>, CoreError> {
        self.ensure_index()?;
        let mut out = Vec::with_capacity(self.index.len());
        // Container order, as Go's `zip.Reader` produced it (`api/archive_parser.go:41-62`).
        let mut order: Vec<(String, usize)> =
            self.index.iter().map(|(k, v)| (k.clone(), *v)).collect();
        order.sort_by_key(|(_, i)| *i);
        for (path, i) in order {
            let file = self
                .archive
                .by_index_raw(i)
                .map_err(|e| map_zip_error(e, &self.name))?;
            out.push(RawEntry {
                path,
                is_dir: file.is_dir(),
                size: file.size(),
                mtime: file
                    .last_modified()
                    .map_or(0, |dt| zip_datetime_seconds(&dt)),
                mode: file.unix_mode(),
                encrypted: file.encrypted(),
            });
        }
        Ok(out)
    }

    fn write_stream(
        &mut self,
        inner: &str,
        sink: &mut dyn Write,
        cap: u64,
    ) -> Result<u64, CoreError> {
        let i = self.locate(inner)?;
        let known = self
            .archive
            .by_index_raw(i)
            .map_err(|e| map_zip_error(e, &self.name))?
            .size();
        let mut file = self
            .archive
            .by_index(i)
            .map_err(|e| map_zip_error(e, &self.name))?;
        if file.encrypted() {
            return Err(ArchiveError::Encrypted {
                archive: format!("{}:{}", self.name, inner),
            }
            .into_core());
        }
        copy_capped(&mut file, sink, cap, &self.name, known)
    }
}

/// A zip DOS timestamp, in Unix seconds. Zip stores *local* time with no zone (`zip::DateTime`), and Go
/// handed the same naive value to `Modified` (`api/archive_parser.go:53`), so this is parity, not a guess.
fn zip_datetime_seconds(dt: &zip::DateTime) -> i64 {
    chrono::NaiveDate::from_ymd_opt(
        i32::from(dt.year()),
        u32::from(dt.month()),
        u32::from(dt.day()),
    )
    .and_then(|d| {
        d.and_hms_opt(
            u32::from(dt.hour()),
            u32::from(dt.minute()),
            u32::from(dt.second()),
        )
    })
    .map_or(0, |nd| nd.and_utc().timestamp())
}

// --------------------------------------------------------------------------- 7z

/// The 7z backend over `sevenz-rust` (Apache-2.0, plan §10 item 3).
pub struct SevenZBackend<R: Read + Seek + Send> {
    reader: sevenz_rust::SevenZReader<R>,
    name: String,
}

impl<R: Read + Seek + Send> SevenZBackend<R> {
    /// Wrap an opened 7z. An unencrypted archive needs an empty password.
    #[must_use]
    pub fn new(reader: sevenz_rust::SevenZReader<R>, name: String) -> Self {
        Self { reader, name }
    }

    fn entries(&self) -> Result<Vec<RawEntry>, CoreError> {
        let mut out = Vec::new();
        for e in &self.reader.archive().files {
            let path = crate::archive::vfs::clean_internal_path(e.name());
            if path.is_empty() {
                continue;
            }
            out.push(RawEntry {
                is_dir: e.is_directory(),
                size: if e.has_stream { e.size() } else { 0 },
                mtime: sevenz_mtime(e),
                mode: None,
                path,
                encrypted: false,
            });
        }
        Ok(out)
    }
}

/// 7z stores NT timestamps (100 ns ticks since 1601-01-01); 0 means "no timestamp recorded", which the
/// manifest renders as the epoch, exactly as Go's zero `time.Time` did.
fn sevenz_mtime(e: &sevenz_rust::SevenZArchiveEntry) -> i64 {
    if !e.has_last_modified_date {
        return 0;
    }
    let raw = e.last_modified_date().to_raw();
    if raw == 0 {
        return 0;
    }
    i64::try_from(raw / 10_000_000).unwrap_or(i64::MAX) - 11_644_473_600
}

fn map_7z_error(e: sevenz_rust::Error, archive: &str) -> CoreError {
    let msg = e.to_string().to_ascii_lowercase();
    if msg.contains("password") || msg.contains("encrypt") {
        return ArchiveError::Encrypted {
            archive: archive.to_string(),
        }
        .into_core();
    }
    if msg.contains("not yet supported") || msg.contains("unsupported") {
        return ArchiveError::Unsupported {
            archive: archive.to_string(),
            detail: e.to_string(),
        }
        .into_core();
    }
    ArchiveError::Corrupt {
        archive: archive.to_string(),
        reason: e.to_string(),
    }
    .into_core()
}

impl<R: Read + Seek + Send> ArchiveReader for SevenZBackend<R> {
    fn list(&mut self) -> Result<Vec<RawEntry>, CoreError> {
        self.entries()
    }

    fn write_stream(
        &mut self,
        inner: &str,
        sink: &mut dyn Write,
        cap: u64,
    ) -> Result<u64, CoreError> {
        let want = crate::archive::vfs::clean_internal_path(inner);
        let known = self
            .entries()?
            .into_iter()
            .find(|e| e.path == want)
            .map(|e| e.size)
            .ok_or_else(|| CoreError::NotFound {
                path: format!("{archive}{want}", archive = self.name),
                source_path: None,
            })?;
        let mut written: Option<u64> = None;
        let mut err: Option<CoreError> = None;
        let name = self.name.clone();
        // 7z is a solid format: decoding one member means walking its folder, so this is a stream, not a
        // seek — but nothing here buffers the *archive*, which is the plan's `stream_file` criterion.
        let r = self.reader.for_each_entries(
            &mut |e: &sevenz_rust::SevenZArchiveEntry, reader: &mut dyn Read| {
                if crate::archive::vfs::clean_internal_path(e.name()) != want {
                    return Ok(true);
                }
                let mut counting = CountingSink {
                    sink,
                    total: 0,
                    cap,
                };
                match copy_capped(reader, &mut counting, cap, &name, known) {
                    Ok(n) => {
                        written = Some(n);
                        Ok(false)
                    }
                    Err(core) => {
                        err = Some(core);
                        Ok(false)
                    }
                }
            },
        );
        if let Err(e) = r {
            return Err(map_7z_error(e, &self.name));
        }
        if let Some(e) = err {
            return Err(e);
        }
        written.ok_or_else(|| CoreError::NotFound {
            path: format!("{archive}{want}", archive = self.name),
            source_path: None,
        })
    }
}

/// A sink that refuses to pass `cap` (used inside the 7z closure, where the writer is borrowed).
struct CountingSink<'a> {
    sink: &'a mut dyn Write,
    total: u64,
    cap: u64,
}

impl Write for CountingSink<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.total.saturating_add(buf.len() as u64) > self.cap {
            return Err(std::io::Error::other("capacity exceeded"));
        }
        let n = self.sink.write(buf)?;
        self.total += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.sink.flush()
    }
}

// --------------------------------------------------------------------------- opening

/// Open a container by format over an already-`stat`ed file.
///
/// # Errors
/// [`CoreError::Unsupported`] for RAR (plan §10 item 3), `ArchiveCorrupt` when the file does not parse.
pub fn open(
    fmt: ArchiveFormat,
    file: std::fs::File,
    size: u64,
    name: &str,
) -> Result<Box<dyn ArchiveReader>, CoreError> {
    match fmt {
        ArchiveFormat::Zip => {
            let archive = zip::ZipArchive::new(file).map_err(|e| map_zip_error(e, name))?;
            Ok(Box::new(ZipReader::new(archive, name.to_string())))
        }
        ArchiveFormat::SevenZip => {
            let reader = sevenz_rust::SevenZReader::new(file, size, sevenz_rust::Password::empty())
                .map_err(|e| map_7z_error(e, name))?;
            Ok(Box::new(SevenZBackend::new(reader, name.to_string())))
        }
        ArchiveFormat::Rar => Err(open_rar(name)),
    }
}

/// Open a container held in memory — the nested-archive path, where the inner store is the bytes of one
/// member of the outer one (see [`crate::archive::vfs::open_store`]).
///
/// # Errors
/// As [`open`].
pub fn open_bytes(
    fmt: ArchiveFormat,
    bytes: Vec<u8>,
    name: &str,
) -> Result<Box<dyn ArchiveReader>, CoreError> {
    let size = bytes.len() as u64;
    let cursor: Cursor<Vec<u8>> = Cursor::new(bytes);
    match fmt {
        ArchiveFormat::Zip => {
            let archive = zip::ZipArchive::new(cursor).map_err(|e| map_zip_error(e, name))?;
            Ok(Box::new(ZipReader::new(archive, name.to_string())))
        }
        ArchiveFormat::SevenZip => {
            let reader =
                sevenz_rust::SevenZReader::new(cursor, size, sevenz_rust::Password::empty())
                    .map_err(|e| map_7z_error(e, name))?;
            Ok(Box::new(SevenZBackend::new(reader, name.to_string())))
        }
        ArchiveFormat::Rar => Err(open_rar(name)),
    }
}

/// The RAR seam. With `#[cfg(feature = "rar")]` off — the default — this is the whole story, and the
/// feature being *on* changes nothing but the code path: an external `unrar`/`7z` binary is the planned
/// reader (plan §10 item 3), and this step does not spawn anything (`clippy.toml` keeps every spawn in
/// `services/actions.rs::exec`, and adding a second spawn site is out of scope for 13a).
#[cfg(feature = "rar")]
fn open_rar(_name: &str) -> CoreError {
    ArchiveError::rar_unsupported()
}

#[cfg(not(feature = "rar"))]
fn open_rar(_name: &str) -> CoreError {
    ArchiveError::rar_unsupported()
}

/// Read one member of one archive, opening the file for exactly that (plan §5.5's `read_entry`: the
/// nested-archive path, which must not parse a manifest it is not going to show).
///
/// # Errors
/// As [`open`], plus `NotFound` when the member is absent.
pub fn read_entry(file: &Path, inner: &str, cap: u64) -> Result<Vec<u8>, CoreError> {
    let fmt = crate::archive::detect(file).ok_or_else(|| CoreError::UnsupportedArchive {
        archive: file.to_string_lossy().into_owned(),
    })?;
    let size = std::fs::metadata(file)
        .map_err(|e| CoreError::Io {
            path: file.to_string_lossy().into_owned(),
            io: std::sync::Arc::new(e),
        })?
        .len();
    let handle = std::fs::File::open(file).map_err(|e| CoreError::Io {
        path: file.to_string_lossy().into_owned(),
        io: std::sync::Arc::new(e),
    })?;
    let mut r = open(fmt, handle, size, &file.to_string_lossy())?;
    r.read_to_vec(inner, cap)
}

/// The format a member path claims, defaulting to the host's own when the member name carries no
/// archive extension (which cannot happen for a store, but can for a caller that passes `""`).
#[must_use]
pub fn member_format(member: &str, fallback: ArchiveFormat) -> ArchiveFormat {
    detect_name(member).unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zip_written_here_lists_and_streams() {
        // A one-member zip, built inline: this module's *unit* test, not the contract test (which uses
        // the captured fixtures). `include_bytes!` of a committed .zip is not an option — the fixture
        // archives are generated by `scripts/make-fixtures.py`, not stored.
        let bytes = minimal_zip(b"hello", "a.txt");
        let cursor = Cursor::new(bytes);
        let archive = zip::ZipArchive::new(cursor).expect("test zip parses");
        let mut r = ZipReader::new(archive, "mem.zip".into());
        let listed = r.list().expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, "a.txt");
        assert_eq!(listed[0].size, 5);
        let mut out = Vec::new();
        let n = r
            .write_stream("a.txt", &mut out, 1024)
            .expect("stream reads");
        assert_eq!(n, 5);
        assert_eq!(out, b"hello");
        // The cap bites before the member is copied.
        let err = r.write_stream("a.txt", &mut Vec::new(), 1).unwrap_err();
        assert_eq!(err.kind(), "capacity_exceeded");
    }

    #[test]
    fn missing_member_is_a_typed_not_found() {
        let cursor = Cursor::new(minimal_zip(b"x", "a.txt"));
        let archive = zip::ZipArchive::new(cursor).expect("test zip parses");
        let mut r = ZipReader::new(archive, "m.zip".into());
        let err = r.write_stream("nope.txt", &mut Vec::new(), 10).unwrap_err();
        assert_eq!(err.kind(), "not_found");
    }

    #[test]
    fn rar_is_unsupported_with_its_licence_reason_on_both_feature_rungs() {
        let e = open_rar("a.rar");
        assert_eq!(e.kind(), "unsupported");
        assert!(format!("{e}").contains("allowlist"));
    }

    #[test]
    fn member_format_prefers_the_member_extension() {
        assert_eq!(
            member_format("inner.7z", ArchiveFormat::Zip),
            ArchiveFormat::SevenZip
        );
        assert_eq!(member_format("", ArchiveFormat::Zip), ArchiveFormat::Zip);
    }

    /// The bytes of a one-member zip, written by the *fixture generator* the goldens use
    /// (`scripts/make-fixtures.py`, python's `zipfile`) — reproduced here as the `zip` crate's own writer
    /// because the unit tests must not touch the filesystem. The contract tests (`contract_archive.rs`)
    /// are the ones that open the captured archives; a container written by the crate under test proves
    /// the round trip, not the parser, and is labelled as such.
    fn minimal_zip(data: &[u8], name: &str) -> Vec<u8> {
        use zip::write::SimpleFileOptions;

        let mut out: Vec<u8> = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut out));
            let opts: SimpleFileOptions = SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
                .unix_permissions(0o644);
            w.start_file(name, opts).expect("test zip: add file");
            std::io::Write::write_all(&mut w, data).expect("test zip: write");
            w.finish().expect("test zip: finish");
        }
        out
    }
}
