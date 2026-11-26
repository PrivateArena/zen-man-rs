//! Step 11a acceptance gate (plan §5.10 `contract_search.rs` row, §7 step 11): the **live** search walk
//! against the frozen `tests/golden/search-*.json` captures of Go's `GET /api/search`
//! (`/media/jang/home/Deve/zen-man/api/search.go`).
//!
//! The FTS half of that plan row (`fts_query_parity`, `rebuild_is_atomic`, `incremental_update`,
//! `excludes_glob_parity`) belongs to step 12 and is deliberately not stubbed here.
//!
//! # Where the tree comes from
//!
//! Exactly as in `contract_dirs.rs`: `${FIXTURE_ROOT}` is `/tmp/zm-fix` on the capture machine
//! (`tests/golden/PROVENANCE.json`), and the tree under it is `scripts/make-fixtures.py`'s output. This
//! file regenerates it into a scratch directory and points a real `SearchService`/`Session` at it, so
//! the expectations are the captured bytes and not a previous run of this test.
//!
//! # What the goldens pin
//!
//! | Golden | Request (`tests/golden/INDEX.json`) | What it proves here |
//! |---|---|---|
//! | `search-live-flat` | `path=tree&q=.txt` | non-recursive scan, name order, 17 rows, `total_matched`, every `SearchEntry` field |
//! | `search-live-regex` | `path=tree&q=^a&regex=true` | regex mode, `^` anchor, case-insensitivity |
//! | `search-live-multi` | `path=tree/subdir&q=b` | the multi-token branch's request line (see the `&c` note below) |
//! | `search-live-capped` | `path=tree/many&q=f&limit=3` | truncation at the cap, and that `total_matched` is the *kept* count |
//! | `search-live-crlf` | `path=tree&q=weird` | a name containing `CR` survives matching and the row |
//! | `search-live-empty-q` | `path=tree/subdir&q=` | an empty query matches everything, in name order, dirs not first |
//! | `search-live-recursive` | `path=tree&q=leaf&recursive=true` | the empty answer: Go's `entries: null` / `total_matched: 0` |
//! | `err-search-bad-regex` | `path=tree&q=[&regex=true` | the typed error, with Go's message byte-for-byte |
//! | `err-search-nonexistent` | `path=nope&q=x` | a missing scope is `not_found` (Go's 404) |
//!
//! Two things the captures themselves teach, recorded because they change what "parity" means:
//!
//! * **`search-live-multi` is not a multi-token capture.** Its recorded request is
//!   `…&q=b&c`, and `&c` is a *separate, empty query parameter* — the capture script's quoting bug — so
//!   Go answered `q=b`. This test therefore runs `q=b` and says so, rather than inventing a token split
//!   the golden never exercised. The all-tokens rule itself is proven by `name_match`'s unit tests and by
//!   `search-live-empty-q`'s "empty matches everything".
//! * **`entries: null` becomes `[]`.** Plan §2.4 row *`/api/search` (live)* records that Go emits JSON
//!   `null` when nothing matched; §6.2 fixes that to always-`[]`. The comparison asserts an empty row set
//!   where the golden has `null` and a row-for-row match everywhere else.
//!
//! Directory `size` fields belong to the filesystem the capture ran on (tmpfs reports 60 for that
//! directory), so on a host that does not size directories the same way the test prints a `SKIPPED`
//! line naming that single field and asserts everything else — the "explicit skip, never a silent pass"
//! rule `cargo xtask fixture-tree` uses.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

// Scratch naming, recursive delete and stale-tree reaping: the shared test-support module.
#[path = "../src/scratch.rs"]
mod scratch;

use zenman_core::error::CoreError;
use zenman_core::events::{EngineEvent, JobPayload};
use zenman_core::jobs::JobId;
use zenman_core::limits::Limits;
use zenman_core::path::AbsPath;
use zenman_core::platform::Env;
use zenman_core::services::dirs::OsDirRead;
use zenman_core::services::search_live::{
    DEFAULT_LIMIT, HiddenPolicy, NameMode, ScopeFilter, SearchHit, SearchQuery, SearchResults,
    SearchService, THROTTLE_WINDOW, UNBOUNDED_LIMIT,
};
use zenman_core::session::{Origin, Session, SessionPaths};
use zenman_core::store::config::ZenConfig;

const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/golden");
const REPO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

// ----------------------------------------------------------------------------------- the fixture tree

struct Fixtures {
    /// What `${FIXTURE_ROOT}` stands for in this run.
    root: PathBuf,
    /// `true` when a fresh directory here reports the tmpfs `st_size` the capture recorded.
    go_directory_sizes: bool,
}

fn fixtures() -> &'static Fixtures {
    static TREE: OnceLock<Fixtures> = OnceLock::new();
    TREE.get_or_init(build_fixtures)
}

fn build_fixtures() -> Fixtures {
    // Fresh, pid+seq-unique, and reapable by the shared sweep — see `contract_dirs.rs`'s copy of
    // this fixture-building dance for the full rationale (same eleven-thousand-file problem).
    let root = scratch::create("contract-search");
    let tree = root.join("tree");
    let generator = Path::new(REPO).join("scripts/make-fixtures.py");
    assert!(
        generator.is_file(),
        "{} is missing: the fixture tree is its output",
        generator.display()
    );
    let output = Command::new("python3")
        .current_dir(REPO)
        .arg(generator.display().to_string())
        .arg("--root")
        .arg(tree.display().to_string())
        .arg("--hostile")
        .arg("--with-heavy")
        .arg("--fifo")
        .spawn()
        .and_then(|child| child.wait_with_output())
        .expect("python3 runs the repository's own fixture generator");
    assert!(
        output.status.success(),
        "make-fixtures.py failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let go_directory_sizes = directory_sizes_are_go_like(&root);
    eprintln!(
        "contract_search: fixture tree at {} (go-shaped directory sizes: {go_directory_sizes})",
        root.display()
    );
    Fixtures {
        root,
        go_directory_sizes,
    }
}

fn directory_sizes_are_go_like(dir: &Path) -> bool {
    let one = dir.join("size-probe/a");
    if std::fs::create_dir_all(&one).is_err() {
        return false;
    }
    let sized = std::fs::metadata(&one).map(|m| m.len()).unwrap_or(u64::MAX);
    let _ = scratch::remove_tree(&one);
    let _ = std::fs::remove_dir(dir.join("size-probe"));
    sized == 40
}

// --------------------------------------------------------------------------------------- the harness

fn golden(name: &str) -> Value {
    let path = Path::new(GOLDEN).join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("golden {} is missing and unreadable: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("golden {}: {e}", path.display()))
}

/// The golden's `${FIXTURE_ROOT}`-carrying text, made real.
fn materialise(value: Value) -> Value {
    let text = value.to_string();
    let swapped = text.replace("${FIXTURE_ROOT}", &fixtures().root.to_string_lossy());
    serde_json::from_str(&swapped).unwrap_or_else(|e| panic!("materialised golden is invalid: {e}"))
}

fn abs(path: &Path) -> AbsPath {
    AbsPath::try_from(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn tree() -> AbsPath {
    abs(&fixtures().root.join("tree"))
}

fn service() -> SearchService {
    SearchService::new(Arc::new(OsDirRead), Arc::new(Limits::default()))
}

/// A request shaped like one Go URL: `path`, `q`, `recursive`, `regex`, `limit`.
fn request(scope: AbsPath, q: &str) -> SearchQuery {
    SearchQuery {
        scope,
        query: q.to_string(),
        mode: NameMode::Substring,
        scope_filter: ScopeFilter::Both,
        recursive: false,
        limit: DEFAULT_LIMIT,
        cursor: zenman_core::cursor::Cursor::FIRST,
        hidden: HiddenPolicy::Include,
        budget: zenman_core::services::search_live::SearchBudget::default(),
    }
}

/// Run a search and hand back the rows *and* the streamed-into vector, which is the same thing: the
/// streaming sink is what a cancellable job consumes, so every golden test goes through it.
fn run(req: &SearchQuery) -> SearchResults {
    let svc = service();
    let svc_rows = svc
        .search(req, &zenman_core::jobs::CancelToken::new())
        .unwrap_or_else(|e| panic!("search {:?} in {}: {e}", req.query, req.scope));
    // The same walk through `run`'s sink must produce byte-identical rows: that is what makes the
    // streaming path (step 11b's) and the collecting path one implementation rather than two.
    let mut streamed: Vec<SearchHit> = Vec::new();
    let status = svc
        .run(req, &zenman_core::jobs::CancelToken::new(), &mut |h| {
            streamed.push(h)
        })
        .unwrap_or_else(|e| panic!("streaming search failed: {e}"));
    assert_eq!(streamed, svc_rows.hits, "the sink and the payload disagree");
    // `elapsed_ms` is a wall-clock measurement, so it is the one field two runs may differ in.
    let (mut streamed_status, mut collected_status) = (status.clone(), svc_rows.status.clone());
    streamed_status.elapsed_ms = 0;
    collected_status.elapsed_ms = 0;
    assert_eq!(streamed_status, collected_status);
    SearchResults {
        query: req.clone(),
        hits: streamed,
        status,
    }
}

/// The golden's wire shape, rebuilt from our rows.
fn wire(results: &SearchResults) -> Value {
    let entries: Vec<Value> = results
        .hits
        .iter()
        .map(|h| {
            json!({
                "name": h.entry.name,
                "is_dir": h.entry.is_dir,
                "size": h.entry.size,
                "mod_time": h.entry.mtime_secs(),
                "mode": h.entry.mode,
                "rel_path": h.rel_path,
            })
        })
        .collect();
    json!({
        "entries": entries,
        "total_matched": results.status.total_matched,
        "capped": results.status.capped(),
    })
}

/// Compare one capture, field by field, with the two documented normalisations applied.
fn assert_matches_golden(results: &SearchResults, golden_name: &str) {
    let want = materialise(golden(golden_name));
    let got = wire(results);
    assert_eq!(
        got["total_matched"], want["total_matched"],
        "{golden_name}: `total_matched` is Go's kept-row count (`api/search.go:225`)"
    );
    assert_eq!(
        got["capped"], want["capped"],
        "{golden_name}: `capped` is Go's flag (`api/search.go:75,120`)"
    );
    let want_entries = match want["entries"].as_array() {
        Some(list) => list,
        // Go's `entries: null` (`search-live-recursive.json`); plan §2.4/§6.2 keep the key and emit
        // `[]`, so the parity assertion is "no rows", not "no value".
        None => {
            assert!(
                got["entries"].as_array().is_some_and(|e| e.is_empty()),
                "{golden_name}: Go returned no rows, we returned {}",
                got["entries"]
            );
            return;
        }
    };
    let got_entries = got["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("{golden_name}: we emitted no entries array"));
    assert_eq!(
        got_entries.len(),
        want_entries.len(),
        "{golden_name}: row count (order is part of the contract): got {got_entries:?}"
    );
    for (i, (want, got)) in want_entries.iter().zip(got_entries).enumerate() {
        let tag = format!("{golden_name}[{i}]");
        assert_eq!(got["name"], want["name"], "{tag}: name");
        assert_eq!(got["is_dir"], want["is_dir"], "{tag}: is_dir");
        assert_eq!(got["mode"], want["mode"], "{tag}: mode");
        assert_eq!(got["mod_time"], want["mod_time"], "{tag}: mod_time");
        assert_eq!(got["rel_path"], want["rel_path"], "{tag}: rel_path");
        if want["is_dir"] == Value::Bool(true) && !fixtures().go_directory_sizes {
            eprintln!("SKIPPED {tag}: `size` of a directory row (tmpfs-sized in the capture)");
        } else {
            assert_eq!(got["size"], want["size"], "{tag}: size");
        }
    }
}

// ------------------------------------------------------------------------------------- the goldens

#[test]
fn live_search_matches_go_goldens() {
    let cases: [(&str, AbsPath, &str, NameMode, bool, usize); 7] = [
        (
            "search-live-flat",
            tree(),
            ".txt",
            NameMode::Substring,
            false,
            500,
        ),
        (
            "search-live-regex",
            tree(),
            "^a",
            NameMode::Regex,
            false,
            500,
        ),
        // `…&q=b&c` in `INDEX.json`: `c` is a separate empty parameter, so Go saw `q=b`.
        (
            "search-live-multi",
            abs(&fixtures().root.join("tree/subdir")),
            "b",
            NameMode::Substring,
            false,
            500,
        ),
        (
            "search-live-capped",
            abs(&fixtures().root.join("tree/many")),
            "f",
            NameMode::Substring,
            false,
            3,
        ),
        (
            "search-live-crlf",
            tree(),
            "weird",
            NameMode::Substring,
            false,
            500,
        ),
        (
            "search-live-empty-q",
            abs(&fixtures().root.join("tree/subdir")),
            "",
            NameMode::Substring,
            false,
            500,
        ),
        (
            "search-live-recursive",
            tree(),
            "leaf",
            NameMode::Substring,
            // Despite its file name the capture is **not** recursive: `INDEX.json`'s request line is
            // `…/api/search?path=${FIXTURE_ROOT}/tree&q=leaf` with no `recursive` parameter, so Go ran
            // the flat `ReadDir` branch and found nothing. See the test's tail for the recursive case.
            false,
            500,
        ),
    ];
    for (name, scope, query, mode, recursive, limit) in cases {
        let mut req = request(scope, query);
        req.mode = mode;
        req.recursive = recursive;
        req.limit = limit;
        let results = run(&req);
        assert_matches_golden(&results, name);
    }
}

#[test]
fn live_walk_limits() {
    // 1. The numbers, pinned to their sources (plan §5.2 `limits.rs`, `docs/contracts.md` ledger #5).
    assert_eq!(
        DEFAULT_LIMIT, 500,
        "Go's default `limit` (`api/search.go:66`)"
    );
    assert_eq!(
        UNBOUNDED_LIMIT, 1_000_000,
        "Go's `limit <= 0` ceiling (`api/search.go:72`)"
    );
    assert_eq!(
        UNBOUNDED_LIMIT,
        Limits::default().live_search_cap as usize,
        "`limits.rs` and the service must not drift"
    );
    assert_eq!(
        Limits::default().flat_walk_cap,
        10_000,
        "the *listing* walk's cap is a different number (`api/dir.go:132`)"
    );
    assert_eq!(
        Limits::default().live_search_nodes,
        100_000,
        "plan §5.2's `live_search_nodes`"
    );

    // 2. Truncation at the row cap, against the capture that was made for it: three rows out of 1 200
    //    matches, kept in walk order, `capped` set, and `total_matched` reporting the *kept* count.
    let mut req = request(abs(&fixtures().root.join("tree/many")), "f");
    req.limit = 3;
    let results = run(&req);
    assert_matches_golden(&results, "search-live-capped");
    assert_eq!(
        results
            .hits
            .iter()
            .map(|h| h.entry.name.as_str())
            .collect::<Vec<&str>>(),
        ["f0001.txt", "f0002.txt", "f0003.txt"],
        "the cap keeps Go's first rows, not an arbitrary three"
    );
    // Exactly `limit` matches is *not* `capped` — Go only flags a `(limit+1)`-th match.
    let mut exact = request(tree(), ".txt");
    exact.limit = 17;
    assert_eq!(run(&exact).len(), 17);
    assert!(!run(&exact).status.capped());
    // `limit = 0` is Go's "unlimited", not "nothing".
    let mut zero = request(abs(&fixtures().root.join("tree/many")), "f");
    zero.limit = 0;
    assert_eq!(run(&zero).len(), 1_200);
    assert!(!run(&zero).status.limited());

    // 3. The node budget: the walk stops and *says which budget* stopped it.
    let mut req = request(tree(), "");
    req.recursive = true;
    req.budget.max_nodes = 50;
    let results = run(&req);
    assert!(results.status.node_budget_hit);
    assert_eq!(results.status.nodes_visited, 51, "the budget is exact");
    assert!(results.len() < 1_200);

    // 4. The depth budget, on a tree this test owns (the fixture tree is 20 deep in `deep/`).
    let deep = fixtures().root.join("depth-probe");
    let _ = scratch::remove_tree(&deep);
    let mut leaf = deep.clone();
    for d in 0..10 {
        leaf = leaf.join(format!("l{d}"));
    }
    std::fs::create_dir_all(&leaf).expect("deep tree");
    std::fs::write(leaf.join("bottom.txt"), b"deep").expect("deep file");
    let mut req = request(abs(&deep), "bottom");
    req.recursive = true;
    req.budget.max_depth = 3;
    let results = run(&req);
    assert!(
        results.status.depth_truncated,
        "a 10-level tree must be cut by a depth budget of 3"
    );
    assert!(results.is_empty());
    req.budget.max_depth = zenman_core::services::search_live::MAX_DEPTH;
    assert!(!run(&req).status.depth_truncated);
    assert_eq!(run(&req).len(), 1);

    // 5. The time budget, exercised at the value that cannot be flaky: `Some(0)` trips at the first
    //    boundary check, so the flag is set and the walk is short.
    let mut req = request(abs(&fixtures().root.join("tree/many")), "f");
    req.budget.max_elapsed_ms = Some(0);
    let results = run(&req);
    assert!(results.status.timed_out);
    assert!(results.status.limited());
    assert!(results.len() < 1_200);

    // 6. Order stability — the property step 11b needs so a list does not reshuffle between keystrokes:
    //    the same query twice is identical, and a narrower query is a *subsequence* of a broader one.
    let broad = run(&request(abs(&fixtures().root.join("tree/many")), "f"));
    let again = run(&request(abs(&fixtures().root.join("tree/many")), "f"));
    assert_eq!(broad.hits, again.hits, "a re-run must not reshuffle");
    let mut narrow = request(abs(&fixtures().root.join("tree/many")), "f000");
    narrow.limit = 20;
    let narrow = run(&narrow);
    let broad_names: Vec<&str> = broad.hits.iter().map(|h| h.name()).collect();
    let mut cursor = 0usize;
    for hit in narrow.hits() {
        let at = broad_names[cursor..]
            .iter()
            .position(|n| *n == hit.name())
            .unwrap_or_else(|| panic!("{} vanished from the broader walk", hit.name()));
        cursor += at + 1;
    }
    // 7. The walk's own accounting. With a cap in the way, the walk stops at the first name *past* the
    //    cap — Go's `filepath.SkipAll` at `api/search.go:120` — so it visits `limit + 1` names; with Go's
    //    "unlimited" it visits all 1 200 and enters nothing (a flat scan is one `readdir`).
    let capped = run(&request(abs(&fixtures().root.join("tree/many")), "f"));
    assert!(capped.status.truncated);
    assert_eq!(capped.status.nodes_visited, 501);
    let mut uncapped = request(abs(&fixtures().root.join("tree/many")), "f");
    uncapped.limit = 0;
    let flat = run(&uncapped);
    assert_eq!(flat.len(), 1_200);
    assert_eq!(flat.status.nodes_visited, 1_200);
    assert_eq!(flat.status.dirs_visited, 0);
    assert_eq!(flat.status.dirs_skipped, 0);
}

/// The recursive branch, which the frozen capture never actually exercised (see the module doc): Go's
/// `filepath.WalkDir` (`api/search.go:108-160`) descends the 20-level `deep/` chain and finds the leaf,
/// in pre-order, with a root-relative `rel_path`.
#[test]
fn live_search_recursion_descends_where_go_walked() {
    let mut req = request(tree(), "leaf");
    req.recursive = true;
    let results = run(&req);
    assert_eq!(
        results
            .hits
            .iter()
            .map(|h| h.rel_path.as_str())
            .collect::<Vec<&str>>(),
        ["deep/d0/d1/d2/d3/d4/d5/d6/d7/d8/d9/d10/d11/d12/d13/d14/d15/d16/d17/d18/d19/leaf.txt"],
        "the 20-level chain is inside the cap budget and must be reached"
    );
    let hit = results.hits.first().expect("one leaf");
    assert_eq!(
        hit.entry.size, 9,
        "`dir-deep.json` records the same 9-byte file"
    );
    assert_eq!(hit.entry.mode, "-rw-r--r--");
    assert_eq!(hit.entry.depth, 21, "pre-order depth from the search root");

    // The unreadable directory is skipped, not fatal (`api/search.go:115-119`), and the count says so.
    let mut all = request(tree(), "");
    all.recursive = true;
    all.limit = 0;
    let results = run(&all);
    assert_eq!(
        results.status.dirs_skipped, 1,
        "`noread-dir` is the one refusal"
    );
    assert!(!results.status.limited());
    // Directories are rows, and they are *not* hoisted: Go's live search keeps `WalkDir` order (§2.4),
    // which is (a) byte-ordered sibling names inside one directory and (b) a directory immediately
    // followed by its own children. Both properties are asserted, because a `Vec::sort` slipped in
    // anywhere — including the listing's dirs-first comparator — would break them and not the count.
    let mut by_parent: std::collections::BTreeMap<&str, Vec<&str>> =
        std::collections::BTreeMap::new();
    for hit in &results.hits {
        let parent = match hit.rel_path.rsplit_once('/') {
            Some((p, _)) => p,
            None => "",
        };
        by_parent.entry(parent).or_default().push(&hit.entry.name);
    }
    for (parent, names) in &by_parent {
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(
            *names, sorted,
            "children of {parent:?} must be in `os.ReadDir` byte order (dirs-first would hoist `deep`)"
        );
    }
    let rows: Vec<&str> = results.hits.iter().map(|h| h.rel_path.as_str()).collect();
    for (i, hit) in results.hits.iter().enumerate() {
        if !hit.is_dir() {
            continue;
        }
        let prefix = format!("{}/", hit.rel_path);
        let has_children = rows.iter().any(|r| r.starts_with(prefix.as_str()));
        if has_children {
            let next = rows.get(i + 1).copied().unwrap_or("");
            assert!(
                next.starts_with(prefix.as_str()),
                "pre-order broken: {} at {i} is followed by {next:?}, not by its own child",
                hit.rel_path
            );
        }
    }
    assert!(
        results.hits.iter().any(|h| h.is_dir()),
        "the recursive search dropped directory rows entirely"
    );
}

#[test]
fn live_search_hidden_file_policy() {
    // Go's backend never filtered hidden files (plan §2.3), so `Include` is the parity default. The
    // fixture tree's own dot-entry is inside `tree/.dot/`, which the goldens never reach — so this test
    // builds the case it needs, next to the tree it shares with the goldens.
    let dir = fixtures().root.join("hidden-probe");
    let _ = scratch::remove_tree(&dir);
    std::fs::create_dir_all(&dir).expect("hidden probe dir");
    std::fs::write(dir.join(".secret.txt"), b"h").expect("hidden file");
    std::fs::write(dir.join("visible.txt"), b"v").expect("visible file");
    let scope = abs(&dir);

    let mut req = request(scope.clone(), "txt");
    let shown = run(&req);
    assert_eq!(
        shown.hits.iter().map(|h| h.name()).collect::<Vec<&str>>(),
        [".secret.txt", "visible.txt"],
        "Go returns hidden rows (`api/search.go` has no hidden-file test at all)"
    );
    req.hidden = HiddenPolicy::Hide;
    assert_eq!(
        run(&req)
            .hits
            .iter()
            .map(|h| h.name())
            .collect::<Vec<&str>>(),
        ["visible.txt"]
    );
}

#[test]
fn invalid_regex_is_typed_with_gos_message() {
    // `err-search-bad-regex.json`: `Invalid regex: error parsing regexp: missing closing ]: `[``.
    let mut req = request(tree(), "[");
    req.mode = NameMode::Regex;
    let e = zenman_core::services::search_live::compile_name_match(&req.query, req.mode)
        .expect_err("`[` is not a pattern");
    let want = golden("err-search-bad-regex")["error"]
        .as_str()
        .expect("golden error")
        .to_string();
    assert_eq!(
        e.to_string(),
        want,
        "the UI shows this string inline in step 11b"
    );
    assert_eq!(e.kind(), "bad_regex");
    assert_eq!(e.status_code(), 400, "Go answered 400 (`api/search.go:85`)");
    match &e {
        CoreError::BadRegex { pattern, .. } => assert_eq!(pattern, "["),
        other => panic!("wrong variant: {other:?}"),
    }
    // The service refuses it too, and refuses it *before* walking: the scope error must not mask it.
    let err = service()
        .run(&req, &zenman_core::jobs::CancelToken::new(), &mut |_| {})
        .expect_err("an unparseable pattern never walks");
    assert_eq!(err.kind(), "bad_regex");
    // …and a missing scope is still Go's 404 (`err-search-nonexistent.json`).
    let gone = request(abs(&fixtures().root.join("nope")), "x");
    let err = service()
        .run(&gone, &zenman_core::jobs::CancelToken::new(), &mut |_| {})
        .expect_err("a missing scope is an error");
    assert_eq!(err.kind(), "not_found");
    assert_eq!(err.status_code(), 404);
    let want = materialise(golden("err-search-nonexistent"));
    assert!(
        want["error"]
            .as_str()
            .unwrap_or("")
            .starts_with("Path not found:"),
        "Go's text is {want}"
    );
    // A file is not a scope: Go's 400 `Path is not a directory` (`api/search.go:57-60`).
    let notadir = request(abs(&fixtures().root.join("tree/normal.txt")), "x");
    let err = service()
        .run(
            &notadir,
            &zenman_core::jobs::CancelToken::new(),
            &mut |_| {},
        )
        .expect_err("a file is not a scope");
    assert_eq!(err.kind(), "not_a_directory");
    assert_eq!(err.status_code(), 400);
}

// ------------------------------------------------------------------- the session façade: throttle + staleness

fn open_session(tag: &str) -> Session {
    let dir = fixtures().root.join(tag);
    std::fs::create_dir_all(dir.join("probe")).expect("probe dir");
    let env: Arc<dyn Env> = zenman_core::platform::default_env();
    let (session, _notes) = Session::open(
        SessionPaths::new(
            dir.join("config.json"),
            dir.join("action.log"),
            None,
            dir.join("probe"),
        ),
        ZenConfig::default(),
        env,
        Limits::default(),
    )
    .expect("a session over a clean scratch directory opens");
    session
}

fn real_now() -> Instant {
    Instant::now()
}

/// Poll until `pred` is satisfied or the deadline passes, returning every event seen (the `settle_until`
/// rule from `zenman-ui`'s test helpers: core coalesces on wall-clock time and walks are real jobs).
fn settle(session: &mut Session, pred: &impl Fn(&[EngineEvent]) -> bool) -> Vec<EngineEvent> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        seen.extend(session.poll());
        if pred(&seen) {
            return seen;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the engine never emitted the expected search events; saw: {seen:?}");
}

fn ready_for(events: &[EngineEvent], job: JobId) -> Option<&EngineEvent> {
    events
        .iter()
        .find(|e| matches!(e, EngineEvent::SearchReady { job: j, .. } if *j == job))
}

#[test]
fn typing_is_throttled_to_one_walk_per_250ms() {
    let mut session = open_session("throttle");
    let scope = abs(&fixtures().root.join("tree/subdir"));
    assert_eq!(session.search_window(), THROTTLE_WINDOW);
    assert_eq!(THROTTLE_WINDOW, Duration::from_millis(250));

    // Five keystrokes inside one window, on an injected clock so the *decision* is exact. The clock is
    // taken from "now minus a second" so no injected instant is ever in the future of the job table.
    let base = real_now()
        .checked_sub(Duration::from_millis(1_000))
        .unwrap_or_else(real_now);
    let mut admissions = Vec::new();
    for (i, q) in ["a", "ab", "abc", "abcd", "abcde"].iter().enumerate() {
        let now = base + Duration::from_millis(u64::try_from(i).expect("small") * 10);
        let admission = session
            .search_live_at(
                &request(scope.clone(), q),
                Origin(10 + u64::try_from(i).expect("small")),
                now,
            )
            .expect("a substring query always compiles");
        admissions.push(admission);
    }
    let running: Vec<JobId> = admissions.iter().filter_map(|a| a.job()).collect();
    assert_eq!(running.len(), 1, "five keystrokes start one walk");
    assert_eq!(
        admissions[1],
        zenman_core::services::search_live::SearchAdmission::Deferred {
            origin: Origin(11),
            // The boundary is asserted, not slept: 10 ms into a 250 ms window leaves exactly 240 ms.
            after: Duration::from_millis(240),
        }
    );
    assert_eq!(admissions[4].job(), None);
    assert_eq!(
        session.pending_search().map(|q| q.query),
        Some("abcde".to_string()),
        "the newest keystroke is the one that walks"
    );
    assert_eq!(
        session
            .jobs()
            .iter()
            .filter(|j| j.kind == zenman_core::jobs::JobKind::SearchLive)
            .count(),
        1,
        "and the job table holds exactly one walk"
    );

    // The deferred request starts on its own once the window closes — the UI keeps no timer. The window
    // closed 750 ms ago in wall-clock terms (the base was a second back), so the next poll promotes it.
    std::thread::sleep(Duration::from_millis(300));
    let events = session.poll();
    assert!(
        events.iter().any(|e| matches!(
            e,
            EngineEvent::JobStarted {
                kind: zenman_core::jobs::JobKind::SearchLive,
                ..
            }
        )),
        "the deferred walk never started: {events:?}"
    );
    assert!(session.pending_search().is_none());
    assert_eq!(
        session
            .jobs()
            .iter()
            .filter(|j| j.kind == zenman_core::jobs::JobKind::SearchLive)
            .count(),
        2,
        "one more walk, and still only one per window"
    );
    let live = session.live_search().expect("a walk is live");
    assert_ne!(live, running[0], "the deferred walk superseded the first");
    let events = settle(&mut session, &|seen| ready_for(seen, live).is_some());
    let ready = ready_for(&events, live).expect("the promoted walk answers");
    let EngineEvent::SearchReady { origin, result, .. } = ready else {
        panic!("wrong event shape")
    };
    assert_eq!(
        *origin,
        Origin(14),
        "the answer is addressed to the newest keystroke"
    );
    assert_eq!(result.query.query, "abcde");
}

#[test]
fn cancelled_and_superseded_searches_emit_nothing_stale() {
    let mut session = open_session("stale");
    let big = abs(&fixtures().root.join("tree"));
    let mut req = request(big.clone(), ".txt");
    req.recursive = true;
    let stale_origin = Origin(21);
    let fresh_origin = Origin(22);

    let stale = session
        .search_live(&req, stale_origin)
        .expect("accepted")
        .job()
        .expect("a walk started");
    // The user typed again: cancel the old walk and start a new one (this is what step 11b does on the
    // next keystroke once the window has closed; `reset_search_throttle` stands in for waiting 250 ms).
    session.cancel(stale);
    session.reset_search_throttle();
    req.query = "normal".to_string();
    let fresh = session
        .search_live(&req, fresh_origin)
        .expect("accepted")
        .job()
        .expect("a walk started");
    assert_ne!(stale, fresh);

    let events = settle(&mut session, &|seen: &[EngineEvent]| {
        ready_for(seen, fresh).is_some()
            && seen
                .iter()
                .any(|e| matches!(e, EngineEvent::JobFinished { id, .. } if *id == stale))
    });

    // The cancelled walk still gets its one terminal event — that is the job table's contract — but it
    // carries no rows, and it never gets a `SearchReady`.
    let finished = events
        .iter()
        .find(|e| matches!(e, EngineEvent::JobFinished { id, .. } if *id == stale))
        .expect("the cancelled walk terminated");
    let EngineEvent::JobFinished { payload, error, .. } = finished else {
        panic!("wrong event shape")
    };
    assert!(
        matches!(payload, JobPayload::None),
        "a cancelled walk produced rows anyway: {payload:?}"
    );
    assert!(
        error.as_ref().is_some_and(|e| e.kind() == "cancelled"),
        "cancellation is reported as such"
    );
    assert!(
        ready_for(&events, stale).is_none(),
        "a cancelled search emitted rows"
    );
    let stale_reads: Vec<&EngineEvent> = events
        .iter()
        .filter(|e| matches!(e, EngineEvent::SearchReady { origin, .. } if *origin == stale_origin))
        .collect();
    assert!(
        stale_reads.is_empty(),
        "a superseded origin received an answer: {stale_reads:?}"
    );
    // …and the surviving walk answers for its own origin, with its own query.
    let ready = events
        .iter()
        .find(|e| matches!(e, EngineEvent::SearchReady { .. }))
        .expect("the fresh walk answered");
    let EngineEvent::SearchReady {
        origin,
        job,
        result,
    } = ready
    else {
        panic!("wrong event shape")
    };
    assert_eq!(*origin, fresh_origin);
    assert_eq!(*job, fresh);
    assert_eq!(result.query.query, "normal");
    assert_eq!(EngineEvent::origin(ready), Some(fresh_origin));
    assert_eq!(EngineEvent::job_id(ready), Some(fresh));
}

#[test]
fn search_ready_carries_the_rows_the_dialog_needs() {
    // The 11b contract, asserted rather than documented: one event, carrying the query, the rows in walk
    // order, and the status with its truncation flag.
    let mut session = open_session("shape");
    let mut req = request(tree(), ".txt");
    req.limit = 5;
    let origin = session.next_origin();
    let job = session
        .search_live(&req, origin)
        .expect("accepted")
        .job()
        .expect("a walk started");
    let events = settle(&mut session, &|seen| ready_for(seen, job).is_some());
    let EngineEvent::SearchReady { result, .. } = ready_for(&events, job).expect("answer") else {
        panic!("wrong event shape")
    };
    assert_eq!(result.query.scope, req.scope);
    assert_eq!(result.len(), 5);
    assert!(result.status.truncated);
    assert_eq!(
        result
            .hits
            .iter()
            .map(|h| h.entry.name.as_str())
            .collect::<Vec<&str>>(),
        // The fixture tree carries both spellings of "café": NFD (`e` + U+0301) first, then NFC, which
        // is the order the golden's `search-live-flat.json` records them in.
        [
            "a10.txt",
            "a9.txt",
            "back\\slash.txt",
            "cafe\u{301}.txt",
            "caf\u{e9}.txt"
        ],
        "the job's rows are the walk's rows, in the golden's order"
    );
    assert!(result.hits.iter().all(|h| !h.entry.mode.is_empty()));
}
// ============================================================================================= 12a
//
// Step 12's half of this plan row: the FTS index, asserted two ways — against the captured
// `search-index-*.json` / `index-status*.json` goldens *and* against the captured `zen-man.db`
// itself (`api/search_index*.go`). The live-tree goldens were captured on a tree without the
// `--with-heavy` additions (the capture's `many` holds 1 200 files and `index-status.json` counts
// 1 260 rows; the heavy directory would add 10 005 more), so this section builds its own
// `--hostile --fifo` tree and leaves the shared `fixtures()` one to the live tests.

use std::sync::atomic::{AtomicBool, Ordering};

use rusqlite::Connection;
use zenman_core::index::intent::{IndexIntent, IndexOp};
use zenman_core::index::rebuild::{self, Stage};
use zenman_core::index::updater::{self, QueuedIntent};
use zenman_core::index::walker::WalkCfg;
use zenman_core::jobs::CancelToken;
use zenman_core::services::search_index;
use zenman_core::store::config::StoreMode;
use zenman_core::store::sql::connect_with_retry;

/// The seven excludes the capture ran with (`index-status.json` → `config.excludes`): Go's six
/// defaults plus the fixture config's `**/flatcap`.
const GO_EXCLUDES: [&str; 7] = [
    "/proc",
    "/sys",
    "/dev",
    "**/.git",
    "**/node_modules",
    "**/__pycache__",
    "**/flatcap",
];

/// The capture machine's fixture root (`search-index-*.json` header note; the captured database was
/// never normalised, so its rows still carry it literally).
const CAPTURE_ROOT: &str = "/tmp/zm-fix";

struct IdxFixtures {
    root: PathBuf,
}

fn idx_fixtures() -> &'static IdxFixtures {
    static TREE: OnceLock<IdxFixtures> = OnceLock::new();
    TREE.get_or_init(|| {
        let root = scratch::create("contract-index");
        let tree = root.join("tree");
        let output = Command::new("python3")
            .current_dir(REPO)
            .arg(
                Path::new(REPO)
                    .join("scripts/make-fixtures.py")
                    .display()
                    .to_string(),
            )
            .arg("--root")
            .arg(tree.display().to_string())
            .arg("--hostile")
            .arg("--fifo")
            .spawn()
            .and_then(|child| child.wait_with_output())
            .expect("python3 runs the repository's own fixture generator");
        assert!(
            output.status.success(),
            "make-fixtures.py failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        // NOTE: deliberately NOT `relax`ed at rest — the hostile permissions (noexec-dir, noread-dir) are what make the
        // count 1 260; the shared `remove_tree` relaxes on the way out. eprintln for the location:
        eprintln!("contract_search: index fixture tree at {}", tree.display());
        IdxFixtures { root }
    })
}

fn idx_tree() -> AbsPath {
    abs(&idx_fixtures().root.join("tree"))
}

/// The golden's `${FIXTURE_ROOT}` text, made real against the *index* tree (not the shared live one).
fn materialise_idx(value: Value) -> Value {
    let text = value.to_string();
    let swapped = text.replace("${FIXTURE_ROOT}", &idx_fixtures().root.to_string_lossy());
    serde_json::from_str(&swapped).expect("materialised golden is valid")
}

fn idx_conn(name: &str) -> Connection {
    let dir = idx_fixtures().root.join(name);
    std::fs::create_dir_all(&dir).expect("index db dir");
    connect_with_retry(
        &dir.join("zen-man.db"),
        StoreMode::Full,
        8,
        Duration::from_millis(50),
    )
    .expect("a database for the index tests")
}

fn idx_walk_cfg() -> WalkCfg {
    WalkCfg::from_settings(
        &[idx_tree().to_string_lossy().to_owned()],
        &GO_EXCLUDES
            .iter()
            .map(|s| (*s).to_string())
            .collect::<Vec<_>>(),
        0,
        false,
        2,
    )
    .expect("the capture's settings compile")
}

/// A session with a real database over the index fixture tree, rebuilt once through
/// `Session::index_rebuild` — so the job plumbing, the `IndexChanged` synthesis and the dirty-flag
/// transition are all on the tested path (Go's rebuild-then-status flow).
fn indexed_session(tag: &str) -> Session {
    let dir = idx_fixtures().root.join(tag);
    std::fs::create_dir_all(&dir).expect("index dir");
    let mut cfg = ZenConfig::default();
    cfg.indexer.roots = vec![idx_tree().to_string_lossy().to_owned()];
    cfg.indexer.excludes = GO_EXCLUDES.iter().map(|s| (*s).to_string()).collect();
    let env: Arc<dyn Env> = zenman_core::platform::default_env();
    let (mut session, _notes) = Session::open(
        SessionPaths::new(
            dir.join("config.json"),
            dir.join("action.log"),
            Some(dir.join("zen-man.db")),
            dir.join("probe"),
        )
        .with_db_open(8, Duration::from_millis(50)),
        cfg,
        env,
        Limits::default(),
    )
    .expect("a session with a database opens");
    let job = session.index_rebuild().expect("the rebuild is accepted");
    let seen = settle(&mut session, &|evs: &[EngineEvent]| {
        evs.iter()
            .any(|e| matches!(e, EngineEvent::JobFinished { id, .. } if *id == job))
    });
    assert!(
        seen.iter().any(
            |e| matches!(e, EngineEvent::IndexChanged { dirty: false, total } if *total == 1_260)
        ),
        "a landed rebuild announces itself with the golden's row count; saw {seen:?}"
    );
    session
}

#[test]
fn excludes_glob_parity() {
    // Go's seven default excludes, compiled by the hand-rolled NFA (`(^|/)P($|/)`, whole-component
    // semantics, matched against the absolute AND the root-relative path), produce the golden count.
    let sess = indexed_session("idx-excludes");
    let st = sess.index_status();
    assert_eq!(st.total_files, 1_260, "the golden's row count");
    assert!(
        !st.is_dirty,
        "a successful rebuild clears the dirty flag (Go's IndexState)"
    );
    assert!(!st.is_indexing);
    assert_eq!(st.progress.indexed, 1_260);
    assert_eq!(
        st.config.excludes,
        GO_EXCLUDES
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(st.config.worker_count, 2);
    assert_eq!(
        st.last_indexed,
        unix_seconds(),
        "the rebuild stamps the time (±1 s tolerance aside)"
    );
    // The `noexec-dir` drop: `noexec-dir/inner.txt` cannot be `lstat`ed (the directory has no +x), so
    // Go's `d.Info()` error path drops the row — that is what makes the count 1 260 and not 1 261.
    // (The *name* `inner.txt` also exists under `fake.zip/`, which IS indexed, so the check has to
    // be path-scoped, exactly like Go's `in:` operator.)
    let scoped = sess
        .index_search(
            &format!("in:{}/tree/noexec-dir", idx_fixtures().root.display()),
            None,
            None,
        )
        .expect("search");
    assert_eq!(
        scoped.total_matched, 0,
        "`in:` lists the directory's CONTENTS (Go's parent-prefix semantics) — and it has no
         indexed children, because inner.txt's lstat fails"
    );
    let noexec = sess.index_search("noexec-dir", None, None).expect("search");
    assert_eq!(
        noexec.total_matched, 1,
        "the directory row itself is indexed"
    );
    // The `/proc`-style absolute excludes must match whole components, not substrings: a tree
    // directory named `dev-tools` is NOT excluded by `/dev`.
    let ours = sess.index_search("flatcap", None, None).expect("search");
    assert_eq!(
        ours.total_matched, 0,
        "`**/flatcap` prunes the fixture's flatcap directory"
    );
    // The depth trap: `leaf.txt` sits 21 levels under the root. `limits.rs`' INDEX_MAX_DEPTH = 20 is a
    // config-validation bound, not a walk default — Go's default depth is unlimited, and applying 20
    // here would make leaf.txt vanish and break the word golden.
    let leaf = sess.index_search("leaf", None, None).expect("leaf");
    assert_eq!(
        leaf.total_matched, 1,
        "the 21-level-deep leaf.txt must be indexed"
    );
    // And the 30 directories (root included) are all there.
    let dirs = sess.index_search("type:dir", None, None).expect("dirs");
    assert_eq!(dirs.total_matched, 30);
}

fn unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(0))
        .unwrap_or(0)
}

#[test]
fn fts_query_parity() {
    // (a) our query engine against OUR index of the capture's tree.
    let sess = indexed_session("idx-fts");
    let paged = sess.index_search(".txt", Some(3), Some(3)).expect("paged");
    let want = materialise_idx(golden("search-index-paged"));
    assert_eq!(paged.total_matched, 1_223);
    assert_eq!(want["total_matched"], 1_223);
    assert!(paged.capped);
    assert_eq!(want["capped"], true);
    let want_entries = want["entries"].as_array().expect("golden entries");
    assert_eq!(paged.entries.len(), want_entries.len());
    let names: Vec<&str> = paged.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        ["a9.txt", "B.txt", "back\\slash.txt"],
        "ranks 4-6 of the lowercased-name order"
    );
    for (got, w) in paged.entries.iter().zip(want_entries) {
        assert_eq!(got.name, w["name"].as_str().expect("name"));
        assert_eq!(got.is_dir, w["is_dir"].as_bool().expect("is_dir"));
        assert_eq!(got.size, w["size"].as_i64().expect("size"));
        // §2.4: `rel_path` carries the ABSOLUTE path and `mode` is always "" on this endpoint.
        assert_eq!(got.rel_path, w["rel_path"].as_str().expect("rel_path"));
        assert_eq!(got.mode, "");
    }

    let word = sess.index_search("leaf", None, None).expect("word");
    let want = materialise_idx(golden("search-index-word"));
    assert_eq!(
        word.total_matched,
        want["total_matched"].as_i64().expect("total")
    );
    assert_eq!(word.entries[0].rel_path, want["entries"][0]["rel_path"]);

    let ext = sess.index_search("ext:sh", None, None).expect("ext");
    assert_eq!(ext.total_matched, 1);
    assert_eq!(ext.entries[0].name, "executable.sh");

    let dirs = sess.index_search("type:dir", None, None).expect("type");
    let want = materialise_idx(golden("search-index-type-dir"));
    assert_eq!(dirs.total_matched, 30);
    let got: Vec<&str> = dirs.entries.iter().map(|e| e.name.as_str()).collect();
    let want_names: Vec<&str> = want["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| e["name"].as_str().expect("name"))
        .collect();
    assert_eq!(
        got, want_names,
        "the full 30-row order, lowercased name ascending"
    );

    let inf = sess
        .index_search(
            &format!("in:{}/tree/subdir", idx_fixtures().root.display()),
            None,
            None,
        )
        .expect("in");
    let got: Vec<&str> = inf.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        got,
        ["b", "a.txt", "c.txt"],
        "dirs first, then files, name order"
    );
    assert_eq!(inf.total_matched, 3);

    let none = sess.index_search("zzzznope", None, None).expect("empty");
    assert_eq!(none.total_matched, 0);
    assert!(none.entries.is_empty(), "§6.2: always-[], never JSON null");
    assert!(!none.capped);

    captured_db_parity();
}

/// The same query engine against the captured Go database, byte-for-byte against the capture.
fn captured_db_parity() {
    let cap = idx_fixtures().root.join("captured/zen-man.db");
    std::fs::create_dir_all(cap.parent().expect("parent")).expect("captured dir");
    std::fs::copy(Path::new(GOLDEN).join("zen-man.db"), &cap).expect("copy the captured database");
    let conn = connect_with_retry(&cap, StoreMode::Full, 8, Duration::from_millis(50))
        .expect("the Go-produced database opens (schema gate and all)");
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM files;", [], |r| r.get(0))
        .expect("count");
    assert_eq!(rows, 1_260, "the captured index holds the golden's rows");
    let paged = search_index::search(&conn, ".txt", 3, 3, false, 0).expect("captured search");
    let raw = golden("search-index-paged");
    assert_eq!(
        paged.total_matched, 1_223,
        "the FTS trigram count over Go's own data"
    );
    let want_entries = raw["entries"].as_array().expect("entries");
    assert_eq!(paged.entries.len(), want_entries.len());
    for (got, w) in paged.entries.iter().zip(want_entries) {
        assert_eq!(got.name, w["name"].as_str().expect("name"));
        assert_eq!(
            got.rel_path,
            w["rel_path"]
                .as_str()
                .expect("rel_path")
                .replace("${FIXTURE_ROOT}", CAPTURE_ROOT),
            "the captured absolute path, backslash and all"
        );
        assert_eq!(got.mode, "");
    }
    // Ranks 4-6 are the golden's own rows — the same engine, Go's data, Go's answer.
    let names: Vec<&str> = paged.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["a9.txt", "B.txt", "back\\slash.txt"]);
}

#[test]
fn rebuild_is_atomic() {
    let sess = indexed_session("idx-atomic");
    let db = sess
        .paths()
        .database
        .clone()
        .expect("this session has a database");
    // A dedicated writer connection, exactly like `Session::index_rebuild`'s job — `with_db` would
    // hold the session mutex for the whole pipeline.
    let conn = connect_with_retry(&db, StoreMode::Full, 8, Duration::from_millis(50))
        .expect("a second connection to the same database");
    let walk_cfg = idx_walk_cfg();
    let cancel = CancelToken::new();
    let reached = AtomicBool::new(false);
    let res = rebuild::run_with_hook(&conn, &walk_cfg, &cancel, &|p| {
        if p.stage == Stage::PopulateFts {
            reached.store(true, Ordering::SeqCst);
            let _ = cancel.cancel();
        }
    });
    assert!(
        reached.load(Ordering::SeqCst),
        "the pipeline reached PopulateFts"
    );
    assert!(
        matches!(res, Err(CoreError::Cancelled)),
        "cancelling at the PopulateFts boundary stops the rebuild: {res:?}"
    );
    // The OLD index still answers, with the OLD numbers: the swap never ran, so `files` is the first
    // rebuild, untouched.
    let hits = search_index::search(&conn, ".txt", 3, 0, false, 0).expect("the old index answers");
    assert_eq!(hits.total_matched, 1_223);
    // And the shadow tables are gone — cleaned up on the cancel path, not left for the next run.
    let left: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('files_shadow', 'files_fts_shadow');",
            [],
            |r| r.get(0),
        )
        .expect("sqlite_master");
    assert_eq!(
        left, 0,
        "a cancelled rebuild leaves no shadow tables behind"
    );
    let counts: (i64, i64) = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM files), (SELECT COUNT(*) FROM files_fts);",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("counts");
    assert_eq!(counts, (1_260, 1_260));
    // A second, complete rebuild lands: the pipeline is re-runnable after a cancelled one.
    let again =
        rebuild::run(&conn, &walk_cfg, &CancelToken::new()).expect("the next rebuild works");
    assert_eq!(again.rows, 1_260);
}

#[test]
fn incremental_update() {
    // A small tree of our own: the shared fixture trees are read-only for the other tests here.
    let root = idx_fixtures().root.join("idx-incr-tree");
    let _ = scratch::remove_tree(&root);
    std::fs::create_dir_all(root.join("sub")).expect("sub");
    std::fs::write(root.join("a.txt"), b"one").expect("a");
    std::fs::write(root.join("keep.txt"), b"keep").expect("keep");
    std::fs::write(root.join("sub/b.txt"), b"bee").expect("b");
    std::fs::write(root.join("sub/c.txt"), b"cee").expect("c");
    let conn = idx_conn("idx-incr");
    let cfg = WalkCfg::from_settings(&[root.display().to_string()], &[], 0, false, 2)
        .expect("no excludes here");
    let seed = rebuild::run(&conn, &cfg, &CancelToken::new()).expect("seed rebuild");
    assert_eq!(seed.rows, 6, "root + a + keep + sub + b + c");

    let path =
        |rel: &str| AbsPath::from_path(root.join(rel).display().to_string()).expect("absolute");
    let total = |q: &str| -> i64 {
        search_index::search(&conn, q, search_index::DEFAULT_LIMIT, 0, false, 0)
            .expect("query")
            .total_matched
    };
    let scalar = |sql: &str, arg: &str| -> Option<i64> {
        conn.query_row(sql, [arg], |r| r.get::<_, i64>(0)).ok()
    };

    // Created: the row appears and FTS learns the name in the same op.
    std::fs::write(root.join("new.txt"), b"hello").expect("write new");
    updater::apply_op(
        &conn,
        &IndexOp::Created {
            path: path("new.txt"),
        },
    )
    .expect("created");
    assert_eq!(
        total("new.txt"),
        1,
        "the new row is findable, not just present"
    );

    // Refreshed: the numbers are re-read from the filesystem.
    std::fs::write(root.join("new.txt"), b"hello world!").expect("write new2");
    updater::apply_op(
        &conn,
        &IndexOp::Refreshed {
            path: path("new.txt"),
        },
    )
    .expect("refreshed");
    assert_eq!(
        scalar(
            "SELECT size FROM files WHERE path = ?1;",
            &path("new.txt").to_string_lossy()
        ),
        Some(12)
    );

    // Renamed: path/name/ext move, the rowid (and so the FTS link) survives, under the NEW name.
    std::fs::rename(root.join("new.txt"), root.join("renamed.txt")).expect("rename");
    updater::apply_op(
        &conn,
        &IndexOp::Renamed {
            from: path("new.txt"),
            to: path("renamed.txt"),
        },
    )
    .expect("renamed");
    assert_eq!(total("renamed.txt"), 1);
    assert_eq!(total("new.txt"), 0);

    // Moved: the subtree — the root row AND every descendant's path/parent change.
    std::fs::rename(root.join("sub"), root.join("moved")).expect("move");
    updater::apply_op(
        &conn,
        &IndexOp::Moved {
            from: path("sub"),
            to: path("moved"),
        },
    )
    .expect("moved");
    assert_eq!(
        scalar("SELECT COUNT(*) FROM files WHERE path LIKE ?1;", "%moved/%"),
        Some(2),
        "b.txt and c.txt carry the new prefix"
    );
    assert_eq!(
        scalar(
            "SELECT COUNT(*) FROM files WHERE path = ?1;",
            &root.join("moved").display().to_string()
        ),
        Some(1),
        "the moved directory's own row moved too"
    );
    assert_eq!(
        scalar(
            "SELECT COUNT(*) FROM files WHERE parent = ?1;",
            &root.join("moved").display().to_string()
        ),
        Some(2),
        "both children now report the new parent"
    );
    assert_eq!(total("b.txt"), 1, "descendants keep their FTS entries");

    // Deleted: one row, its FTS entry with it.
    std::fs::remove_file(root.join("keep.txt")).expect("rm");
    updater::apply_op(
        &conn,
        &IndexOp::Deleted {
            path: path("keep.txt"),
        },
    )
    .expect("deleted");
    assert_eq!(total("keep"), 0);

    // Pruned: the bounded `LIKE 'x/%' ESCAPE '\'` subtree delete, FTS rowids gathered first.
    let _ = scratch::remove_tree(&root.join("moved"));
    updater::apply_op(
        &conn,
        &IndexOp::Pruned {
            path: path("moved"),
        },
    )
    .expect("pruned");
    assert_eq!(
        scalar("SELECT COUNT(*) FROM files WHERE path LIKE ?1;", "%moved%"),
        Some(0)
    );
    let files: i64 = conn
        .query_row("SELECT COUNT(*) FROM files;", [], |r| r.get(0))
        .expect("files");
    let fts: i64 = conn
        .query_row("SELECT COUNT(*) FROM files_fts;", [], |r| r.get(0))
        .expect("fts");
    assert_eq!(files, fts, "no ghost FTS rowids after the prune");

    // AttributesChanged: a real op with no statement — the schema stores no mode (`index/intent.rs`).
    let before = scalar(
        "SELECT size FROM files WHERE path = ?1;",
        &path("a.txt").to_string_lossy(),
    );
    updater::apply_op(
        &conn,
        &IndexOp::AttributesChanged {
            path: path("a.txt"),
        },
    )
    .expect("a chmod is accepted and writes nothing");
    assert_eq!(
        before,
        scalar(
            "SELECT size FROM files WHERE path = ?1;",
            &path("a.txt").to_string_lossy()
        )
    );

    // The queued intent: coalescing on arrival, the drain applying the rest.
    let queue = QueuedIntent::new();
    queue.note(IndexOp::Created {
        path: path("a.txt"),
    });
    queue.note(IndexOp::Refreshed {
        path: path("a.txt"),
    });
    assert_eq!(
        queue.len(),
        2,
        "create-then-refresh is not a mergeable pair"
    );
    queue.note(IndexOp::Created {
        path: path("q.txt"),
    });
    queue.note(IndexOp::Created {
        path: path("q.txt"),
    });
    assert_eq!(queue.len(), 3, "identical ops collapse");
    std::fs::write(root.join("q.txt"), b"queued").expect("write q");
    let applied = queue.drain_into(&conn).expect("drain");
    assert!(
        applied >= 3,
        "every queued op reached the database: {applied}"
    );
    assert_eq!(total("q.txt"), 1, "the drained op is searchable");
    assert!(queue.is_empty());
}
