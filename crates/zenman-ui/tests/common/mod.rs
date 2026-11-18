//! Shared fixture for the `zenman-ui` integration tests: a live `Session` over a private
//! temporary data directory, and the **deterministic** listing tree every file-pane test renders.
//!
//! The *tests* do IO — the crate under test never does (rule 7); the engine IO is the sanctioned
//! path (`Session::open` is core's own seam).
//!
//! The listing tree is the reason these tests can assert an *order*: fixed names, fixed count and
//! **fixed modification times**, so no test depends on when it ran or on the developer's home
//! directory. That is also what makes the snapshot baselines (`tests/snapshots.rs`) comparable
//! between machines.
//!
//! `allow(dead_code)`: this module is shared by every `zenman-ui` integration target, and each
//! target uses a different subset of it — the gate is `cargo xtask ci`'s zero-warning rule, and a
//! helper file cannot satisfy it per-target any other way.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use zenman_core::session::{Session, SessionPaths};
use zenman_core::store::config::ZenConfig;

/// Where every fixture of this test binary lives: cargo's own per-integration-test scratch
/// directory (`CARGO_TARGET_TMPDIR`, i.e. `<workspace>/target/tmp`) under `zenman-ui-fixtures`.
///
/// It is **deterministic**: no process id, so the *path painted into a snapshot baseline* — the
/// breadcrumb of `02-file-list.png` / `03-file-grid.png` — is the same string on every run of the
/// same checkout, which is what makes those PNGs byte-reproducible (plan §9.2 step-8c review 3).
/// Collisions are impossible the other way: the directory is per *cargo test invocation* of one
/// target, and every scene/test already passes its own unique tagged subdirectory, so two tests
/// never share a tree. Only if the scratch directory cannot be created at all (a read-only or
/// missing `target/`) does this fall back to the system temp directory — still pid-free.
fn fixture_base() -> PathBuf {
    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("zenman-ui-fixtures");
    if std::fs::create_dir_all(&scratch).is_ok() {
        return scratch;
    }
    let fallback = std::env::temp_dir().join("zenman-ui-fixtures");
    std::fs::create_dir_all(&fallback)
        .expect("neither cargo's scratch directory nor the temp directory is creatable");
    fallback
}

/// A session in `<scratch>/zenman-ui-fixtures/zenman-ui-test-<tag>`; two tags never collide.
#[must_use]
pub fn session(tag: &str) -> Session {
    let dir = fixture_base().join(format!("zenman-ui-test-{tag}"));
    std::fs::create_dir_all(&dir).expect("the fixture directory is creatable");
    let paths = SessionPaths::new(
        dir.join("config.json"),
        dir.join("action.log"),
        None,
        dir.join("probe"),
    )
    .with_db_open(1, std::time::Duration::from_millis(1));
    let (session, _notes) = Session::open(
        paths,
        ZenConfig::default(),
        zenman_core::platform::default_env(),
        ZenConfig::default().limits(),
    )
    .expect("a session opens over a fresh directory");
    session
}

// --------------------------------------------------------------------------- the deterministic tree

/// Rows of generated filler: enough that one page (`Limits::dir_page_default`, Go's 200) does not
/// hold the listing, so the pagination banner and the `LoadMore` path are both reachable.
pub const BULK_ROWS: usize = 450;

/// Every row of [`listing_tree`]'s listing directory: 2 directories + 5 named files + [`BULK_ROWS`].
pub const ROW_COUNT: usize = BULK_ROWS + 7;

/// The first mtime the fixture assigns (2023-11-14 22:13:20 UTC), one *day* apart per row.
const MTIME_BASE: u64 = 1_700_000_000;

/// One day, the fixture's mtime step.
const DAY: u64 = 86_400;

/// The listing tree the file-pane and snapshot tests render.
///
/// ```text
/// <scratch>/zenman-ui-fixtures/zenman-ui-fixture-<tag>/listing/
///   docs/                    (dir, three files inside)
///   reports/                 (dir, empty)
///   .hidden                  (a dot file: the backend never filters hidden rows)
///   a.txt  b.txt  c.md
///   twenty-eight-chars-of-name.txt   (the ellipsis case)
///   row-0000.txt … row-0449.txt      (the bulk, and the page boundary)
/// ```
///
/// Every *file* gets a distinct mtime, assigned in **reverse alphabetical order**: that makes
/// "sort by date ascending" mean "reverse alphabetical", which is an assertion a test can state
/// exactly. The two directories get their own distinct pair, oldest in listing order, so a date
/// sort of the directory class is deterministic too.
///
/// Rebuilt on every call — idempotent, and cheap enough (hundreds of small files) that a test can
/// not compare against a stale tree.
pub fn listing_tree(tag: &str) -> PathBuf {
    let root = fixture_base().join(format!("zenman-ui-fixture-{tag}"));
    let listing = root.join("listing");
    std::fs::create_dir_all(listing.join("docs")).expect("fixture directory");
    std::fs::create_dir_all(listing.join("reports")).expect("fixture directory");
    write(&listing.join("docs/readme.md"), b"# readme\n");
    write(&listing.join("docs/notes.txt"), b"notes\n");
    write(&listing.join("docs/big.bin"), &vec![b'x'; 4096]);
    write(&listing.join("reports/empty"), b"\n");
    for name in [
        ".hidden",
        "a.txt",
        "b.txt",
        "c.md",
        "twenty-eight-chars-of-name.txt",
    ] {
        write(&listing.join(name), b"fixture\n");
    }
    for index in 0..BULK_ROWS {
        write(&listing.join(format!("row-{index:04}.txt")), b"bulk row\n");
    }
    // The mtimes, in reverse alphabetical order of the *displayed* rows: the last row of the
    // listing is the oldest. The index comes from the position (`last - index`), never from
    // `.rev().enumerate()` — an `.enumerate()` on a reversed iterator counts in the *displayed*
    // order, and two rows that end up with the same mtime leave their relative order to the
    // comparator, which is how a directory listing can reshuffle between frames.
    let mut rows = listing_names();
    rows.retain(|name| name != "docs" && name != "reports");
    let last = rows.len() - 1;
    for (index, name) in rows.iter().enumerate() {
        set_mtime(
            &listing.join(name),
            MTIME_BASE + (last - index) as u64 * DAY,
        );
    }
    // The two directories get their own distinct pair, oldest in *listing* order (`docs` first),
    // so a date-ascending listing reads `docs, reports` and a descending one reverses the class.
    for (index, name) in ["docs", "reports"].iter().enumerate() {
        set_mtime(&listing.join(name), MTIME_BASE + index as u64 * DAY);
    }
    listing
}

/// The rows of [`listing_tree`], in the listing order the engine produces: directories first, then
/// the lowercase byte compare of plan §2.3 (so `.hidden` before `a.txt`, `row-…` before `t`).
#[must_use]
pub fn listing_names() -> Vec<String> {
    let mut files: Vec<String> = vec![
        ".hidden".into(),
        "a.txt".into(),
        "b.txt".into(),
        "c.md".into(),
        "twenty-eight-chars-of-name.txt".into(),
    ];
    for index in 0..BULK_ROWS {
        files.push(format!("row-{index:04}.txt"));
    }
    files.sort_by(|a, b| {
        a.to_lowercase()
            .cmp(&b.to_lowercase())
            .then_with(|| a.cmp(b))
    });
    let mut names = vec!["docs".to_string(), "reports".to_string()];
    names.extend(files);
    names
}

/// The date-sort order of **the loaded rows** — what a local re-sort of a page must produce, as
/// opposed to what a refetch of the whole listing would (`listing_tree`'s mtimes are assigned over
/// all rows, so a page is a *prefix* of the listing, not a prefix of the date order).
///
/// `loaded` must be in listing order. Dirs first (their class keeps listing order when ascending,
/// reversed when descending — plan §2.3's dirs-first invariant), then the loaded files in reverse
/// listing order, because [`listing_tree`] makes mtime rise with the listing position.
#[must_use]
pub fn loaded_by_date(loaded: &[String], descending: bool) -> Vec<String> {
    let (mut dirs, files): (Vec<String>, Vec<String>) = loaded
        .iter()
        .cloned()
        .partition(|name| name == "docs" || name == "reports");
    if descending {
        dirs.reverse();
    }
    let mut ordered = dirs;
    // Ascending: oldest first, i.e. reverse listing order. Descending: the listing order itself
    // (newest mtime first *is* the listing order, because the fixture's mtimes rise with it).
    let mut files = files;
    if !descending {
        files.reverse();
    }
    ordered.extend(files);
    ordered
}

/// [`loaded_by_date`]'s ascending half, named for the assertion it writes.
#[must_use]
pub fn loaded_by_date_ascending(loaded: &[String]) -> Vec<String> {
    loaded_by_date(loaded, false)
}

/// [`loaded_by_date`]'s descending half (newest mtime first, dirs still first).
#[must_use]
pub fn loaded_by_date_descending(loaded: &[String]) -> Vec<String> {
    loaded_by_date(loaded, true)
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
}

/// Pin a path's mtime (files *and* directories — `File::open` on a directory is read-only and
/// `futimens` accepts it, which is what makes a directory row's date cell deterministic too).
fn set_mtime(path: &Path, unix_secs: u64) {
    let when = SystemTime::UNIX_EPOCH + Duration::from_secs(unix_secs);
    let open = std::fs::File::options()
        .write(false)
        .read(true)
        .open(path)
        .or_else(|e| panic!("opening {} to set its mtime: {e}", path.display()));
    match open.and_then(|file| file.set_modified(when)) {
        Ok(()) => {}
        Err(e) => panic!("setting {} to {unix_secs}: {e}", path.display()),
    }
}
