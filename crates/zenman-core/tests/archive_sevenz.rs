//! Step 13b's honest gap, closed: **a real 7z stream, decoded by the port.**
//!
//! Plan §7 row 13's archive criteria have always been proven against the captured fixtures, and the
//! captured `inner.7z` is *not* a 7z archive — `scripts/make-fixtures.py:257-267` records that no
//! `7z`/`7za`/`p7zip` existed on the capture host and that none may be downloaded, so the member is a
//! six-byte signature header plus padding. `contract_archive.rs` therefore uses it as the *corrupt
//! container* case, and nothing in the workspace has ever decoded a genuine 7z stream.
//!
//! This file is the port-side round trip that closes that gap: it writes a genuine 7z with
//! `sevenz-rust`'s own writer (the `compress` feature, which is the crate's writer half — there is no
//! feature named `write`; it is a **dev-dependency feature only**, so the shipped build still reads
//! 7z and never writes one), then reads it back through [`zenman_core::archive::reader`] — the exact
//! path the VFS uses — and lists and streams a member.
//!
//! It is stated plainly, in the test name as well as here: **this is a port-side round trip, not a Go
//! golden.** It proves the 7z backend parses a container the 7z format actually produces; it does not
//! prove byte-for-byte agreement with what Go's `bodgit/sevenzip` answered for the same file, because
//! no such capture exists. The golden comparison stays owed (plan §9.2's step-13 row).

use std::fs;
use std::io::Cursor;
use std::path::PathBuf;

use sevenz_rust::{SevenZArchiveEntry, SevenZWriter};

use zenman_core::archive::reader;
use zenman_core::archive::sniff;
use zenman_core::path::ArchiveFormat;

/// The two members: one at the root, one under a directory (the container stores the path text, so
/// this is also the "does the backend hand back the cleaned internal path" question).
const MEMBERS: [(&str, &[u8]); 2] = [
    ("hello.txt", b"hello from a real 7z container\n"),
    ("docs/nested.txt", b"nested body, also compressed\n"),
];

/// A scratch directory of its own, removed first (the target dir persists between runs).
fn scratch(tag: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("zenman-7z-{tag}"));
    // No pre-clean: `SevenZWriter::create` truncates, so a leftover from a previous run cannot be
    // mistaken for this one's output (`remove_dir_all` is banned workspace-wide, plan §5.1).
    fs::create_dir_all(&root).expect("scratch directory");
    root
}

#[test]
fn a_genuine_7z_written_here_decodes_through_the_port_reader() {
    let dir = scratch("roundtrip");
    let path = dir.join("roundtrip.7z");

    // 1. Write a real 7z. `compress` is what makes this possible at all; it is a dev-only feature.
    let mut writer = SevenZWriter::create(&path).expect("sevenz writer");
    for (name, body) in MEMBERS {
        let mut entry = SevenZArchiveEntry::default();
        entry.name = name.to_string();
        entry.has_stream = true;
        entry.is_directory = false;
        writer
            .push_archive_entry(entry, Some(Cursor::new(body.to_vec())))
            .expect("push a member");
    }
    writer.finish().expect("finish the container");

    // 2. The signature is the one `sniff` claims, i.e. this is not another signature stub.
    let bytes = fs::read(&path).expect("read the container back");
    assert!(
        bytes.starts_with(zenman_core::archive::MAGIC_7Z),
        "the writer did not emit a 7z header"
    );
    assert_eq!(sniff(&bytes), Some(ArchiveFormat::SevenZip));
    assert!(
        bytes.len() > zenman_core::archive::MAGIC_7Z.len() + 8,
        "a stub-sized container is not a round trip: {} bytes",
        bytes.len()
    );

    // 3. Read it through the port's own reader — the same `open` the VFS store calls.
    let file = fs::File::open(&path).expect("open the container");
    let size = file.metadata().expect("stat the container").len();
    let mut reader = reader::open(ArchiveFormat::SevenZip, file, size, "roundtrip.7z")
        .expect("a genuine 7z opens through the 7z backend");

    let listed = reader.list().expect("list the members");
    let names: Vec<&str> = listed.iter().map(|e| e.path.as_str()).collect();
    for (name, _) in MEMBERS {
        assert!(
            names.contains(&name),
            "{name} is missing from {names:?}: a 7z the reader cannot list is a 7z the UI cannot browse"
        );
    }
    let hello = listed
        .iter()
        .find(|e| e.path == "hello.txt")
        .expect("hello.txt was just asserted present");
    assert_eq!(
        hello.size,
        u64::try_from(MEMBERS[0].1.len()).expect("a test-sized length"),
        "the manifest's size is what the extract progress total is computed from"
    );

    // 4. Stream one member out (the `stream_file` criterion's 7z side).
    let mut out: Vec<u8> = Vec::new();
    let written = reader
        .write_stream("hello.txt", &mut out, 4096)
        .expect("stream the member");
    assert_eq!(
        usize::try_from(written).expect("a test-sized count"),
        out.len()
    );
    assert_eq!(out, MEMBERS[0].1, "the member decoded to the wrong bytes");

    // 5. And the whole-container path the nested store uses.
    let mut reader = reader::open(
        ArchiveFormat::SevenZip,
        fs::File::open(&path).expect("reopen"),
        size,
        "roundtrip.7z",
    )
    .expect("reopen through the 7z backend");
    let nested = reader
        .read_to_vec("docs/nested.txt", 4096)
        .expect("read the nested member");
    assert_eq!(nested, MEMBERS[1].1);
}
