//! `cargo xtask parity` — plan §7 row 15's **human gate**: every `docs/parity/*.png` Go reference
//! named against the `crates/zenman-ui/tests/snapshots/` baseline that reviews it, with each pair's
//! dimensions and byte size and a note on what the scene asserts.
//!
//! Two halves, deliberately different in strength:
//!
//! * **The pairing is a gate.** "Every reference screenshot has a named snapshot" is checkable, so
//!   `ci` runs [`check`] and fails on a reference with no baseline, a baseline the map does not name
//!   (an orphan artefact), a mapped PNG that is not on disk, and a mapped reference that is not on
//!   disk. A reference that genuinely cannot be rendered is tolerated only by name, in
//!   [`UNRENDERABLE_REFERENCES`], each with its reason written next to it.
//! * **The verdict is not.** The PNGs are never pixel-diffed (plan §7 step 2, §9.2 step-7): the Go
//!   captures are the *reference*, our baselines are drawn by egui, and the comparison is a person
//!   looking at two frames. That is what [`run`] prepares — a listing plus a review index page — and
//!   what it explicitly does **not** do.
//!
//! The pairing itself is not in this file: it lives in `crates/zenman-ui/tests/snapshots.rs`'s
//! `REFERENCE_MAP`, beside the `Scene` variants that produce the artefacts, and [`reference_map`]
//! reads it out of that source — the same single-source-of-truth discipline as
//! [`crate::harness_size`]/[`crate::harness_snapshot_names`], so a scene added without a map row is a
//! gate failure and not a silent gap.
//!
//! No GPU, no network, no pixel decode: this module reads file headers ([`crate::png`]) and text, and
//! it is O(files) — 13 references, 14 baselines.

use std::error::Error;
use std::path::{Path, PathBuf};

use crate::png;

/// Every `docs/parity/*.png` the port has no scene for, with the reason it cannot be rendered. The
/// gate accepts a reference that is listed here and nowhere else, and the reason is mandatory,
/// because an unexplained exception is how a gap becomes permanent. Empty at step 15d: all 13 frozen
/// references have a scene.
pub const UNRENDERABLE_REFERENCES: &[(&str, &str)] = &[];

/// References whose *content* is not what their file name claims — each measured against the Go
/// source in steps 15b/15c. They keep their baseline (the pairing is real); the note is printed so a
/// reviewer never compares the wrong two frames.
pub const MISLABELLED_REFERENCES: &[(&str, &str)] = &[
    (
        "05-left-drawer.png",
        "shows the drawer CLOSED (Go's is hover-triggered off canvas), so D8's verdict rests on \
         `frontend/index.html:24-34` + `sidebar.js`, not on this frame",
    ),
    (
        "06-right-drawer.png",
        "is not a disk panel: it is Go's Action History slide-in over the Search Manager, while \
         Go's right sidebar (`index.html:425-487`) is the custom-action manager that \
         `07-right-drawer.png` renders (plan §9.2 step-15b, D9)",
    ),
    (
        "10-context-menu-background.png",
        "shows Go's Search Manager with an empty query, not the background context menu; \
         `context-menu.js:112-131` is the source of truth for that menu (plan §9.2 step-15c, D11)",
    ),
];

/// One line of the review: a reference, the baseline that answers it, and what was measured.
#[derive(Debug, Clone)]
pub struct Row {
    /// The `docs/parity/` file, empty for a port-side baseline with no Go counterpart.
    pub reference: String,
    /// The `tests/snapshots/` file.
    pub baseline: String,
    /// What the scene asserts — the harness's own words, from `REFERENCE_MAP`.
    pub note: String,
    /// `WxH px, N B` as measured on disk, or why it is not there.
    pub reference_state: String,
    pub baseline_state: String,
    /// The reviewer's warning from [`MISLABELLED_REFERENCES`]; empty when the name says what it holds.
    pub review_note: String,
    /// A baseline with no Go reference at all (the empty-shell frame).
    pub port_only: bool,
    /// A reference carried by [`UNRENDERABLE_REFERENCES`] instead of by a baseline.
    pub exception: bool,
}

/// The scan: every row, and every way the pairing can be wrong.
pub struct Report {
    pub rows: Vec<Row>,
    pub problems: Vec<String>,
    pub matched: usize,
    pub missing: usize,
    pub orphans: usize,
    pub unmatched: usize,
    pub port_only: usize,
    pub exceptions: usize,
    pub mislabelled: usize,
}

impl Report {
    /// `13 matched, 0 missing snapshot, …` — the counts row 15's acceptance criterion is read from.
    pub fn summary(&self) -> String {
        format!(
            "{} matched, {} missing snapshot, {} orphan snapshot, {} unmatched reference, {} \
             port-side baseline, {} declared exception, {} mislabelled reference",
            self.matched,
            self.missing,
            self.orphans,
            self.unmatched,
            self.port_only,
            self.exceptions,
            self.mislabelled,
        )
    }

    /// The body of the human report: one block per pair, with the reviewer's note under it.
    pub fn listing(&self) -> String {
        let mut out = String::new();
        for row in &self.rows {
            let reference = if row.exception {
                "EXCEPTION"
            } else if row.port_only {
                "no Go reference"
            } else {
                "reference"
            };
            out.push_str(&format!(
                "  {} [{}]\n    -> {}    {}\n       reference: {}   baseline: {}\n",
                display("docs/parity", &row.reference),
                reference,
                display("crates/zenman-ui/tests/snapshots", &row.baseline),
                row.note,
                row.reference_state,
                row.baseline_state,
            ));
            if !row.review_note.is_empty() {
                out.push_str(&format!("       NOTE: {}\n", row.review_note));
            }
        }
        out
    }
}

fn display(dir: &str, name: &str) -> String {
    if name.is_empty() {
        "-".to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// `cargo xtask parity` — the human gate. Lists every pair, writes the review index page, prints why
/// it composes no stitched image, and fails on a broken pairing.
pub fn run(root: &Path, baselines: &[String]) -> Result<(), Box<dyn Error>> {
    let report = scan(root, baselines)?;
    println!("xtask parity: {}", report.summary());
    print!("{}", report.listing());
    for problem in &report.problems {
        println!("xtask parity: PROBLEM {problem}");
    }
    let index = write_index_page(root, &report)?;
    println!(
        "xtask parity: review index written to {} — open it beside the two PNGs of each pair",
        index
            .strip_prefix(root)
            .unwrap_or(index.as_path())
            .display()
    );
    println!("xtask parity: {}", compose_skip_reason());
    if report.problems.is_empty() {
        println!("xtask parity: pairing complete — the verdict on each frame is still a human's");
        Ok(())
    } else {
        Err(format!(
            "xtask parity: {} pairing problem(s) above",
            report.problems.len()
        )
        .into())
    }
}

/// The half `cargo xtask ci` runs: no GPU, no display probe, O(files). Returns the summary line and
/// fails on any problem, so row 15's "every reference has a named snapshot" is a gate, not a wish.
pub fn check(root: &Path, baselines: &[String]) -> Result<String, Box<dyn Error>> {
    let report = scan(root, baselines)?;
    if !report.problems.is_empty() {
        return Err(format!(
            "xtask ci: parity pairing:\n  {}",
            report.problems.join("\n  ")
        )
        .into());
    }
    Ok(report.summary())
}

/// One `REFERENCE_MAP` row: the baseline, the Go reference it is reviewed against (empty when it has
/// none), and the note `xtask parity` prints.
pub type MapRow = (String, String, String);

/// The pairing, read out of `crates/zenman-ui/tests/snapshots.rs`'s own `REFERENCE_MAP`:
/// `(baseline, reference, note)` triples. A row of the wrong arity is an error, not a shrug — the
/// note is part of the report.
pub fn reference_map(root: &Path) -> Result<Vec<MapRow>, Box<dyn Error>> {
    let file = root.join("crates/zenman-ui/tests/snapshots.rs");
    let shown = file.strip_prefix(root).unwrap_or(file.as_path()).display();
    let text = std::fs::read_to_string(&file)?;
    let mut rows = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let code = line.trim_start();
        if !inside {
            inside = code.starts_with("const REFERENCE_MAP");
            continue;
        }
        if code.starts_with("];") {
            break;
        }
        let fields = quoted_literals(code);
        if fields.is_empty() {
            continue;
        }
        if fields.len() != 3 {
            return Err(format!(
                "{shown}: a REFERENCE_MAP row has {} field(s), expected 3 (baseline, reference, note)",
                fields.len()
            )
            .into());
        }
        rows.push((fields[0].clone(), fields[1].clone(), fields[2].clone()));
    }
    if rows.is_empty() {
        return Err(format!(
            "{shown}: no `const REFERENCE_MAP` — the parity pairing has no source of truth"
        )
        .into());
    }
    let mut seen: Vec<String> = Vec::new();
    for (baseline, _, _) in &rows {
        if seen.contains(baseline) {
            return Err(format!("{shown}: {baseline} is paired twice in REFERENCE_MAP").into());
        }
        seen.push(baseline.clone());
    }
    Ok(rows)
}

/// The `"…"` string literals of one line. Escapes are unhandled on purpose: the map has none, and a
/// map row that needs an escape is a note that belongs in a comment.
fn quoted_literals(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(from_quote) = rest.find('"') {
        let after = &rest[from_quote + 1..];
        let Some(to_quote) = after.find('"') else {
            break;
        };
        out.push(after[..to_quote].to_string());
        rest = &after[to_quote + 1..];
    }
    out
}

/// Walk the two directories and reconcile them against the map. Every problem is reported at once.
pub fn scan(root: &Path, baselines: &[String]) -> Result<Report, Box<dyn Error>> {
    let map = reference_map(root)?;
    let refs_dir = root.join("docs/parity");
    let snap_dir = root.join("crates/zenman-ui/tests/snapshots");
    let on_disk_refs = png_names(&refs_dir)?;
    let on_disk_snaps = png_names(&snap_dir)?;
    let mut report = Report {
        rows: Vec::new(),
        problems: Vec::new(),
        matched: 0,
        missing: 0,
        orphans: 0,
        unmatched: 0,
        port_only: 0,
        exceptions: 0,
        mislabelled: 0,
    };
    let mut mapped_refs: Vec<String> = Vec::new();
    for (baseline, reference, note) in &map {
        if !baselines.contains(baseline) {
            report.problems.push(format!(
                "`{baseline}` has a REFERENCE_MAP row but is not a baseline `snapshots.rs` renders \
                 — rename the row or add the scene"
            ));
            continue;
        }
        let mut baseline_state = String::new();
        measure(
            &snap_dir,
            baseline,
            "baseline",
            &mut baseline_state,
            &mut report.problems,
        );
        let missing_baseline = baseline_state == MISSING;
        report.missing += usize::from(missing_baseline);
        let review_note = note_for(reference).unwrap_or_default();
        let port_only = reference.is_empty();
        let reference_state = if port_only {
            report.port_only += 1;
            "- (port-side baseline, no Go counterpart)".to_string()
        } else {
            mapped_refs.push(reference.clone());
            let mut state = String::new();
            measure(
                &refs_dir,
                reference,
                "reference",
                &mut state,
                &mut report.problems,
            );
            state
        };
        report.matched += usize::from(!port_only);
        report.mislabelled += usize::from(!review_note.is_empty());
        report.rows.push(Row {
            reference: reference.clone(),
            baseline: baseline.clone(),
            note: note.clone(),
            reference_state,
            baseline_state,
            review_note,
            port_only,
            exception: false,
        });
    }
    for name in &on_disk_snaps {
        if !map.iter().any(|(baseline, _, _)| baseline == name) {
            report.orphans += 1;
            report.problems.push(format!(
                "{} is on disk but no REFERENCE_MAP row names it — an orphan artefact sits in \
                 review forever",
                snap_dir.join(name).display()
            ));
        }
    }
    for name in &on_disk_refs {
        if mapped_refs.contains(name) {
            continue;
        }
        match UNRENDERABLE_REFERENCES.iter().find(|(r, _)| r == name) {
            Some((_, why)) => {
                report.exceptions += 1;
                report.rows.push(Row {
                    reference: name.clone(),
                    baseline: String::new(),
                    note: (*why).to_string(),
                    reference_state: format!("{} B", byte_size(&refs_dir.join(name))?),
                    baseline_state: "- (declared exception)".to_string(),
                    review_note: note_for(name).unwrap_or_default(),
                    port_only: false,
                    exception: true,
                });
            }
            None => {
                report.unmatched += 1;
                report.problems.push(format!(
                    "{} has no baseline — give it a scene in `snapshots.rs` plus a REFERENCE_MAP \
                     row, or declare it in `xtask/src/parity.rs`'s UNRENDERABLE_REFERENCES with its \
                     reason",
                    refs_dir.join(name).display()
                ));
            }
        }
    }
    report.rows.sort_by(|a, b| {
        a.reference
            .cmp(&b.reference)
            .then_with(|| a.baseline.cmp(&b.baseline))
    });
    Ok(report)
}

/// The reviewer's warning for a reference, if it has one.
pub fn note_for(reference: &str) -> Option<String> {
    MISLABELLED_REFERENCES
        .iter()
        .find(|(name, _)| *name == reference)
        .map(|(_, why)| (*why).to_string())
}

const MISSING: &str = "MISSING";

/// `WxH px, N B` for one artefact, or the problem it caused: a named file that is not on disk, or
/// one that does not decode, is a pairing problem either way.
fn measure(dir: &Path, name: &str, kind: &str, state: &mut String, problems: &mut Vec<String>) {
    let path = dir.join(name);
    match std::fs::read(&path) {
        Ok(bytes) => match png::inspect(&bytes) {
            Ok(info) => {
                *state = format!("{}x{} px, {} B", info.width, info.height, bytes.len());
            }
            Err(why) => {
                problems.push(format!("{}: {why}", path.display()));
                *state = "unreadable".to_string();
            }
        },
        Err(_) => {
            problems.push(format!(
                "the {kind} {} is named by the parity map and is not on disk — {}",
                path.display(),
                if kind == "baseline" {
                    "run `cargo xtask snapshots` on a machine with a rasteriser and commit the PNG"
                } else {
                    "the frozen reference itself has gone; `git checkout docs/parity`"
                }
            ));
            *state = MISSING.to_string();
        }
    }
}

fn byte_size(path: &Path) -> Result<u64, Box<dyn Error>> {
    Ok(std::fs::metadata(path)?.len())
}

/// The sorted PNG names of a directory.
fn png_names(dir: &Path) -> Result<Vec<String>, Box<dyn Error>> {
    let mut names = Vec::new();
    match std::fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries {
                let path = entry?.path();
                if path.is_file()
                    && path.extension().is_some_and(|e| e == "png")
                    && let Some(name) = path.file_name()
                {
                    names.push(name.to_string_lossy().to_string());
                }
            }
        }
        Err(e) => return Err(format!("{}: {e}", dir.display()).into()),
    }
    names.sort();
    Ok(names)
}

/// The review artefact: one markdown page — every pair, the sizes, the notes, and the reason no
/// stitched image is produced here. Written under `target/`, which git ignores (plan §7 step 1 put
/// `/target` in `.gitignore` precisely so generated review output never lands in a commit).
fn write_index_page(root: &Path, report: &Report) -> Result<PathBuf, Box<dyn Error>> {
    let dir = root.join("target/parity-review");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("index.md");
    let mut page = String::new();
    page.push_str("# Parity review - Go references vs egui baselines\n\n");
    page.push_str(&format!("{}\n\n", report.summary()));
    page.push_str(&format!("- {}\n\n", compose_skip_reason()));
    page.push_str("Plan §7 step 15: this is a human gate. Never pixel-diff the two files.\n\n");
    page.push_str(
        "| docs/parity | tests/snapshots | reference | baseline | what the scene asserts |\n\
         |---|---|---|---|---|\n",
    );
    for row in &report.rows {
        page.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            if row.reference.is_empty() {
                "—"
            } else {
                &row.reference
            },
            if row.baseline.is_empty() {
                "—"
            } else {
                &row.baseline
            },
            row.reference_state,
            row.baseline_state,
            row.note,
        ));
        if !row.review_note.is_empty() {
            page.push_str(&format!("| | | NOTE | {} | |\n", row.review_note));
        }
    }
    std::fs::write(&path, page)?;
    Ok(path)
}

/// Why this command composes nothing: an optional stitched image that renders stripes is worse than
/// none, and the previous attempt at one did exactly that (8-bit RGBA, no de-interlace).
fn compose_skip_reason() -> String {
    let head = if std::env::var("DISPLAY").is_ok() || std::env::var("WAYLAND_DISPLAY").is_ok() {
        "side-by-side image composition skipped".to_string()
    } else {
        "no display ($DISPLAY/$WAYLAND_DISPLAY unset), and side-by-side image composition is \
         skipped"
            .to_string()
    };
    format!(
        "{head}: `xtask`'s PNG layer reads chunk headers only (`xtask/src/png.rs`) — the gate layer \
         carries no inflate/decoder dependency, and a hand-rolled compositor mis-renders 8-bit RGBA. \
         Open the two PNGs of each pair side by side instead; the index page lists them in review \
         order."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask is a workspace member")
            .to_path_buf()
    }

    fn baselines(root: &Path) -> Vec<String> {
        crate::harness_snapshot_names(root).expect("the harness names its baselines")
    }

    /// A copy of the committed references, baselines and `snapshots.rs` under
    /// `<temp>/xtask-parity-<pid>-<tag>`, so the gate can be seen failing without touching the tree.
    fn tree(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("xtask-parity-{}-{tag}", std::process::id()));
        for dir in ["docs/parity", "crates/zenman-ui/tests/snapshots"] {
            let target = root.join(dir);
            std::fs::create_dir_all(&target).expect("the copy directory is creatable");
            for entry in std::fs::read_dir(repo().join(dir)).expect("the committed artefacts exist")
            {
                let path = entry.expect("a readable entry").path();
                if path.extension().is_some_and(|e| e == "png") {
                    let name = path.file_name().expect("a file name");
                    std::fs::copy(&path, target.join(name)).expect("the artefact copies");
                }
            }
        }
        std::fs::copy(
            repo().join("crates/zenman-ui/tests/snapshots.rs"),
            root.join("crates/zenman-ui/tests/snapshots.rs"),
        )
        .expect("the harness source copies");
        root
    }

    /// The real tree: every reference paired, nothing orphan, nothing unmatched.
    #[test]
    fn every_committed_reference_has_a_named_baseline() {
        let root = repo();
        let report = scan(&root, &baselines(&root)).expect("the tree scans");
        assert!(report.problems.is_empty(), "{:?}", report.problems);
        assert_eq!(report.matched, 13, "one baseline per frozen reference");
        assert_eq!(report.port_only, 1, "the empty-shell frame is ours alone");
        assert_eq!(report.orphans, 0);
        assert_eq!(report.unmatched, 0);
        assert_eq!(report.missing, 0);
        assert_eq!(report.exceptions, UNRENDERABLE_REFERENCES.len());
        assert_eq!(report.mislabelled, MISLABELLED_REFERENCES.len());
        assert!(
            report.summary().contains("13 matched"),
            "{}",
            report.summary()
        );
        assert!(check(&root, &baselines(&root)).is_ok());
    }

    /// A baseline the map names but that is not on disk is `missing snapshot` — row 15's failure mode.
    #[test]
    fn a_missing_baseline_fails_the_gate() {
        let root = tree("missing");
        std::fs::remove_file(root.join("crates/zenman-ui/tests/snapshots/02-file-list.png"))
            .expect("the copy is there");
        let err = check(&root, &baselines(&root)).expect_err("it must fail");
        assert!(err.to_string().contains("02-file-list.png"), "{err}");
        assert!(err.to_string().contains("not on disk"), "{err}");
    }

    /// A scene PNG with no map row is an orphan, and the reference it used to answer is unmatched:
    /// one deleted row shows both.
    #[test]
    fn an_orphaned_artefact_and_its_unmatched_reference_both_fail() {
        let root = tree("orphan");
        let source = root.join("crates/zenman-ui/tests/snapshots.rs");
        let text = std::fs::read_to_string(&source).expect("the copy is readable");
        let cut: Vec<&str> = text
            .lines()
            .filter(|line| !line.contains("\"02-file-list.png\""))
            .collect();
        std::fs::write(&source, cut.join("\n")).expect("the row is removed");
        let names = baselines(&root);
        let report = scan(&root, &names).expect("the copy scans");
        assert_eq!(report.orphans, 1, "the artefact is now unclaimed");
        assert_eq!(report.unmatched, 1, "01-main-list.png is now unanswered");
        let joined = report.problems.join("\n");
        assert!(joined.contains("orphan"), "{joined}");
        assert!(joined.contains("01-main-list.png"), "{joined}");
        assert!(check(&root, &names).is_err());
    }

    /// A reference the map does not name is `unmatched reference`; the only way out is a declared
    /// exception, and every declared exception must carry a reason.
    #[test]
    fn an_unmatched_reference_fails_and_declared_exceptions_have_reasons() {
        let root = tree("unmatched");
        let refs = root.join("docs/parity");
        std::fs::copy(
            refs.join("01-main-list.png"),
            refs.join("99-never-rendered.png"),
        )
        .expect("the extra reference is written");
        let err = check(&root, &baselines(&root)).expect_err("it must fail");
        assert!(err.to_string().contains("99-never-rendered.png"), "{err}");
        assert!(err.to_string().contains("UNRENDERABLE_REFERENCES"), "{err}");
        assert!(
            UNRENDERABLE_REFERENCES
                .iter()
                .all(|(_, why)| !why.is_empty()),
            "an exception without a reason is not an exception"
        );
    }

    /// A map row that names a scene the harness does not render is refused: the table cannot drift
    /// from the scenes.
    #[test]
    fn a_map_row_must_name_a_scene_the_harness_renders() {
        let root = tree("ghostrow");
        let source = root.join("crates/zenman-ui/tests/snapshots.rs");
        let text = std::fs::read_to_string(&source).expect("the copy is readable");
        std::fs::write(
            &source,
            text.replace("\"01-empty.png\", \"\",", "\"99-ghost.png\", \"\","),
        )
        .expect("the row is rewritten");
        let report = scan(&root, &baselines(&root)).expect("the copy scans");
        assert!(
            report
                .problems
                .iter()
                .any(|p| p.contains("99-ghost.png") && p.contains("not a baseline")),
            "{:?}",
            report.problems
        );
    }

    /// A map row of the wrong arity, and a tree with no map at all, are both refused rather than
    /// silently shortening the report.
    #[test]
    fn a_malformed_map_is_an_error_not_a_shrug() {
        let root =
            std::env::temp_dir().join(format!("xtask-parity-{}-malformed", std::process::id()));
        std::fs::create_dir_all(root.join("crates/zenman-ui/tests/snapshots"))
            .expect("the scratch tree is creatable");
        std::fs::create_dir_all(root.join("docs/parity")).expect("the scratch refs are creatable");
        let source = root.join("crates/zenman-ui/tests/snapshots.rs");
        std::fs::write(
            &source,
            "const REFERENCE_MAP: &[(&str, &str, &str)] = &[\n\
             (\"02-file-list.png\", \"01-main-list.png\")\n\
             ];\n",
        )
        .expect("a two-field row is written");
        let err = reference_map(&root).expect_err("two fields are not three");
        assert!(err.to_string().contains("expected 3"), "{err}");
        std::fs::write(&source, "fn main() {}\n").expect("the map is deleted");
        assert!(
            reference_map(&root).is_err(),
            "a tree with no map has no pairing"
        );
    }
}
