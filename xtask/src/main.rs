//! `cargo xtask <task>` — the in-repo gates (plan §5.1 `xtask/src/main.rs` row, §7 step 1).
//!
//! `ci` is the gate a reviewer trusts; `deps` and `licenses` are the two custom linters it ends
//! with, and since step 7 it opens with two more: the colour guard (`Color32::` literals only in
//! `zenman-ui/src/theme.rs`, and since step 7c the *geometry* half — an unnamed point measurement
//! anywhere else fails) and the IO guard (`std::fs` nowhere in `zenman-ui/src` outside a
//! `#[cfg(test)]` item). Step 7c added the two gates that had been claimed but not built: `assets`
//! (every file under `assets/**` attributed by literal path — `ATTRIBUTION.md`'s own claim) and the
//! snapshot-artefact check (step 8c: *every* PNG under `crates/zenman-ui/tests/snapshots/` must
//! exist, decode, and be the size the test's own `SIZE` implies, and no PNG may sit there that the
//! test does not render). `snapshots` renders them (or prints a skip reason when no adapter
//! exists); `parity` is the one remaining stub, landing with the human review in step 15.
//! `contracts` is real since step 2: it guards the frozen Go golden capture in
//! `tests/golden/` (`xtask/src/contracts.rs`), and since step 2 it also runs the two completeness
//! gates: `golden-index` (every file in `tests/golden/` is accounted for in `INDEX.json`) and
//! `fixture-tree` (`scripts/make-fixtures.py` still produces the tree `MANIFEST.md` promises).
//!
//! `cargo-deny` is **not installed** on this machine (`command -v cargo-deny` → nothing), so
//! `licenses` is pure Rust over `cargo metadata --format-version 1`, with `deny.toml` as its data file
//! (plan §9 disposition 17).
//!
//! Two `clippy.toml` bans shape this file: `std::process::Command::output` is disallowed, so captured
//! output goes through `spawn` + `Child::wait_with_output`; and nothing here deletes a directory tree.

use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

mod assets;
mod contracts;
mod fixture_tree;
mod png;
mod source_guards;

fn main() {
    if let Err(e) = run() {
        eprintln!("xtask: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let task = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "help".to_string());
    match task.as_str() {
        "ci" => ci(),
        "deps" => deps(),
        "licenses" => licenses(),
        "assets" => assets::assets(),
        "snapshots" => snapshots(),
        "parity" => stub(
            "parity",
            15,
            "the human reference review over `docs/parity/*.png` (plan §7 step 15)",
        ),
        "contracts" => contracts::contracts(),
        "fixture-tree" => fixture_tree::fixture_tree(),
        "golden-index" => contracts::golden_index(&workspace_root()?.join("tests/golden")),
        "help" | "-h" | "--help" => {
            println!(
                "usage: cargo xtask <ci|deps|licenses|assets|snapshots|parity|contracts|fixture-tree|golden-index>"
            );
            println!(
                "  fixture-tree and golden-index are the two step-2 gates that `contracts` also runs"
            );
            Ok(())
        }
        other => Err(format!("unknown task {other:?} (try `cargo xtask help`)").into()),
    }
}

fn stub(name: &str, step: u32, why: &str) -> Result<(), Box<dyn Error>> {
    println!("xtask {name}: stub — this task lands in step {step} of the plan: {why}");
    Ok(())
}

/// The full gate: format, lints, tests, then the two provenance/licence linters.
fn ci() -> Result<(), Box<dyn Error>> {
    step("cargo fmt --all -- --check")?;
    colour_guard()?;
    io_guard()?;
    step("cargo clippy --workspace --all-targets -- -D warnings")?;
    step("cargo test --workspace")?;
    // Step 12b: the `watch` feature is off by default, so nothing above compiles `index/watcher.rs`'s
    // inotify half at all. Plan §7 step 12's "watcher feature compiles on and off" is only an acceptance
    // criterion if the gate runs it — a feature nothing builds rots silently (§9.2 step-12a).
    step("cargo check -p zenman-core --features watch")?;
    step("cargo clippy -p zenman-core --features watch --all-targets -- -D warnings")?;
    step("cargo test -p zenman-core --features watch")?;
    contracts::contracts()?;
    deps()?;
    licenses()?;
    // Step 7c: `licenses` is the *dependency* gate; the asset claim in `ATTRIBUTION.md` needs its
    // own check, and it runs here (and as `cargo xtask assets`) rather than inside `licenses`, so
    // that `licenses` stays the one thing its name says — cargo metadata against `deny.toml`.
    assets::assets()?;
    snapshot_artefact_gate()?;
    println!("xtask ci: all gates green");
    Ok(())
}

/// Plan §5.7 `theme.rs` row, in two halves.
///
/// **Colour**: the literals are `Color32::<anything>` — associated constructors (`from_rgb`, named
/// colours) — and `theme.rs` is their one home. A test module inside a file is fine to use them
/// only when the file *is* `theme.rs`; anywhere else, even a test literal would drift silently, so
/// the rule is absolute outside `theme.rs`.
///
/// **Geometry** (`xtask::source_guards::geometry_offences`, step 7c): a *point* value — a float of
/// [`source_guards::MIN_MEASUREMENT`] or more, or an integer of that size inside one of egui's
/// integer-point constructors — must be named. The rule is deliberately narrow, because a growing
/// allowlist is the failure mode it exists to prevent:
///
/// 1. values below the threshold (`0.0`, `1.0`, `2.0`, `0.5`, `1.5`) are arithmetic identities and
///    ratios, not measurements off the token table;
/// 2. integers are read only inside the integer-point constructors listed in
///    [`source_guards::INTEGER_GEOMETRY_CALLS`] — elsewhere an integer is a count or an index (the
///    dock slot order, the four-pane check, `MRU_CAP`), and the gate would only teach people to
///    write `4.0` where `4` belongs;
/// 3. a `const`/`static` declaration line may name its own number — the shape
///    `widgets/disk_meter.rs::BAR_HEIGHT` already had. The gate asks for a *name*; `theme.rs` is
///    where the names more than one widget uses live;
/// 4. lines inside a `#[cfg(test)]` item are exempt: a test that asserts on the numbers is the point.
fn colour_guard() -> Result<(), Box<dyn Error>> {
    let root = workspace_root()?;
    let ui_src = root.join("crates/zenman-ui/src");
    let offenders = scan_rust_files(&ui_src, &["Color32::"], &["theme.rs"])?;
    if !offenders.is_empty() {
        return Err(format!(
            "xtask ci: colour literals outside `theme.rs` (plan §5.7: the theme owns every colour):\n  {}",
            offenders.join("\n  ")
        )
        .into());
    }
    let mut geometry = Vec::new();
    let mut files = Vec::new();
    collect_rs(&ui_src, &mut files)?;
    files.sort();
    for file in files {
        if file.file_name().is_some_and(|n| n == "theme.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&file)?;
        let shown = file.strip_prefix(&root).unwrap_or(file.as_path()).display();
        geometry.extend(
            source_guards::geometry_offences(&text)
                .into_iter()
                .map(|o| format!("{shown}:{}: {}", o.line, o.text)),
        );
    }
    if !geometry.is_empty() {
        return Err(format!(
            "xtask ci: unnamed point measurements outside `theme.rs` (plan §5.7: the token table owns \
             every measurement — add the token, or name the value `const` in the module that alone \
             uses it; the whole rule is in the geometry-guard note above):\n  {}",
            geometry.join("\n  ")
        )
        .into());
    }
    println!(
        "xtask ci: colour + geometry guards — `Color32::` and every unnamed point measurement are kept out of `crates/zenman-ui/src` outside `theme.rs`"
    );
    Ok(())
}

/// Rule 7 (plan §5.7): the UI crate does no IO; `zenman-core` and `craft-settings` own every file
/// operation. The scan is [`source_guards::io_offences`], which reads the whole `std::fs` surface —
/// `use std::fs`, `std::fs` with any following character, `std::io::fs`, and `use std::{… fs …}`
/// import lists including multi-line ones — and exempts only the lines inside a `#[cfg(test)]`
/// **item**, tracked by brace depth from the attribute to that item's own closing brace.
///
/// The step-7 version grepped the literal `std::fs::` and exempted everything from the first
/// `#[cfg(test)]` line to end of file, so `use std::fs;` + `fs::read_dir(..)`, a bare `std::fs`
/// path, and a mid-file test module that whitelisted every line below it all walked straight past
/// it. Both halves are unit-tested in `xtask/src/source_guards.rs`.
fn io_guard() -> Result<(), Box<dyn Error>> {
    let root = workspace_root()?;
    let ui_src = root.join("crates/zenman-ui/src");
    let mut offenders = Vec::new();
    let mut files = Vec::new();
    collect_rs(&ui_src, &mut files)?;
    files.sort();
    for file in files {
        let text = std::fs::read_to_string(&file)?;
        let shown = file.strip_prefix(&root).unwrap_or(file.as_path()).display();
        offenders.extend(
            source_guards::io_offences(&text)
                .into_iter()
                .map(|o| format!("{shown}:{}: {}", o.line, o.text)),
        );
    }
    if !offenders.is_empty() {
        return Err(format!(
            "xtask ci: `std::fs` in `zenman-ui` outside `#[cfg(test)]` items (rule 7: IO goes through zenman-core/craft-settings):\n  {}",
            offenders.join("\n  ")
        )
        .into());
    }
    println!("xtask ci: IO guard — `zenman-ui/src` opens no files outside `#[cfg(test)]` items");
    Ok(())
}

/// Every committed snapshot artefact, checked without rendering (plan §7 step 7's artefact, step
/// 7c's gate, step 8c's extension to the whole directory).
///
/// `crates/zenman-ui/tests/snapshots.rs` still *skips with a printed reason* when the machine has
/// no rasteriser — that is the plan's acceptance path — but that skip is only honest while the
/// artefacts the last machine left behind are the ones they claim. So this gate fails when
/// a baseline is missing, corrupt, truncated, not the size the shell geometry implies, or when the
/// directory carries a PNG the test does not render (a renamed or stale artefact, which would
/// otherwise sit in review forever).
///
/// Both expectations are read out of the test's own source — its `SIZE` constant and its
/// `"NN-<what>.png"` literals — rather than repeated here, so the artefacts and the harness cannot
/// drift apart unnoticed as steps 10/12/14 add their own baselines.
fn snapshot_artefact_gate() -> Result<(), Box<dyn Error>> {
    let root = workspace_root()?;
    println!("xtask ci: {}", check_snapshot_artefacts(&root)?);
    Ok(())
}

/// The gate's body, rooted at `root` so it is testable against a copy of the artefacts in a
/// temporary directory (`tests::a_corrupt_or_stale_artefact_fails_the_gate`). On success, the summary
/// line; on any problem, every problem at once — one run must report all of them.
fn check_snapshot_artefacts(root: &Path) -> Result<String, Box<dyn Error>> {
    let dir = root.join("crates/zenman-ui/tests/snapshots");
    let want = harness_size(root)?;
    let expected = harness_snapshot_names(root)?;
    let mut problems = Vec::new();
    let mut on_disk = Vec::new();
    match std::fs::read_dir(&dir) {
        Ok(entries) => {
            for entry in entries {
                let path = entry?.path();
                if path.is_file() && path.extension().is_some_and(|e| e == "png") {
                    on_disk.push(
                        path.file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default(),
                    );
                }
            }
        }
        Err(e) => problems.push(format!("{}: {e}", dir.display())),
    }
    on_disk.sort();
    let relative = |name: &str| -> String {
        let path = dir.join(name);
        path.strip_prefix(root)
            .unwrap_or(path.as_path())
            .display()
            .to_string()
    };
    for name in expected.iter().filter(|n| !on_disk.contains(n)) {
        problems.push(format!(
            "{} is missing — run `cargo xtask snapshots` on a machine with a rasteriser and \
             commit the PNG (plan §7 step 7: the artefact is part of the step)",
            relative(name)
        ));
    }
    for name in on_disk.iter().filter(|n| !expected.contains(n)) {
        problems.push(format!(
            "{} is not a baseline `snapshots.rs` renders ({}) — delete it or add the scene it \
             belongs to",
            relative(name),
            expected.join(", ")
        ));
    }
    let mut lines = Vec::new();
    for name in &on_disk {
        if !snapshot_name_is_recognised(name) {
            problems.push(format!(
                "{} does not follow the `NN-<what>.png` baseline scheme",
                relative(name)
            ));
            continue;
        }
        let path = dir.join(name);
        let bytes = std::fs::read(&path)?;
        match png::check_dimensions(&relative(name), &bytes, want) {
            Ok(info) => lines.push(format!(
                "  {}: {}x{} px, {} bytes",
                relative(name),
                info.width,
                info.height,
                bytes.len()
            )),
            Err(e) => problems.push(e.to_string()),
        }
    }
    if !problems.is_empty() {
        return Err(format!("xtask ci: snapshot artefacts:\n  {}", problems.join("\n  ")).into());
    }
    Ok(format!(
        "snapshot artefacts — {} valid PNG(s) at the harness size {}x{}:\n{}",
        lines.len(),
        want.0,
        want.1,
        lines.join("\n")
    ))
}

/// The `NN-<what>.png` baseline scheme: two digits, a `-`, a lowercase slug of letters, digits and
/// inner hyphens, and the `.png` extension in lower case.
fn snapshot_name_is_recognised(name: &str) -> bool {
    let Some(rest) = name.strip_suffix(".png") else {
        return false;
    };
    let Some((digits, slug)) = rest.split_once('-') else {
        return false;
    };
    digits.len() == 2
        && digits.bytes().all(|b| b.is_ascii_digit())
        && !slug.is_empty()
        && !slug.starts_with('-')
        && !slug.ends_with('-')
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The `const SIZE: egui::Vec2 = egui::vec2(w, h)` of the snapshot test — the size the token
/// geometry (48 px header + 38 px strip + 30 px status bar at the reference window size) implies.
fn harness_size(root: &Path) -> Result<(u32, u32), Box<dyn Error>> {
    let file = root.join("crates/zenman-ui/tests/snapshots.rs");
    let text = std::fs::read_to_string(&file)?;
    let shown = file.strip_prefix(root).unwrap_or(file.as_path()).display();
    for line in text.lines() {
        let tight = line.replace(' ', "");
        let Some(rest) = tight.strip_prefix("constSIZE:egui::Vec2=egui::vec2(") else {
            continue;
        };
        let args = rest.trim_end_matches(");");
        let mut numbers = args.split(',').map(|n| n.trim().parse::<f64>());
        let (Some(w), Some(h)) = (
            numbers.next().and_then(Result::ok),
            numbers.next().and_then(Result::ok),
        ) else {
            break;
        };
        if w < 1.0 || h < 1.0 || w != w.round() || h != h.round() {
            return Err(format!("{shown}: SIZE is not an integer pixel size ({w}x{h})").into());
        }
        return Ok((w as u32, h as u32));
    }
    Err(format!(
        "{shown}: no `const SIZE: egui::Vec2 = egui::vec2(w, h)` to size the artefact against"
    )
    .into())
}

/// The baseline file names `crates/zenman-ui/tests/snapshots.rs` renders: every `"….png"` string
/// literal outside its comments. The test is the only place a baseline is named, so the artefact
/// set is derived from it — a scene added there is demanded here, and a PNG left there is refused.
fn harness_snapshot_names(root: &Path) -> Result<Vec<String>, Box<dyn Error>> {
    let file = root.join("crates/zenman-ui/tests/snapshots.rs");
    let text = std::fs::read_to_string(&file)?;
    let shown = file.strip_prefix(root).unwrap_or(file.as_path()).display();
    let names = png_string_literals(&text);
    if names.is_empty() {
        return Err(format!("{shown}: no `\"NN-<what>.png\"` baseline to validate").into());
    }
    Ok(names)
}

/// The `"….png"` string literals of a Rust source text, ignoring comment lines (a doc comment that
/// quotes a path is prose, not a baseline). Sorted, deduplicated.
fn png_string_literals(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in text.lines() {
        let code = line.trim_start();
        if code.starts_with("//") {
            continue;
        }
        let mut rest = code;
        while let Some(from_quote) = rest.find('"') {
            let after = &rest[from_quote + 1..];
            let Some(to_quote) = after.find('"') else {
                break;
            };
            let token = &after[..to_quote];
            if token.ends_with(".png")
                && !token.contains('/')
                && !names.contains(&token.to_string())
            {
                names.push(token.to_string());
            }
            rest = &after[to_quote + 1..];
        }
    }
    names.sort();
    names
}

/// Grep a directory tree of `.rs` files for needles, skipping files whose name is exempt.
fn scan_rust_files(
    dir: &Path,
    needles: &[&str],
    exempt_names: &[&str],
) -> Result<Vec<String>, Box<dyn Error>> {
    let root = workspace_root()?;
    let mut files = Vec::new();
    collect_rs(dir, &mut files)?;
    files.sort();
    let mut hits = Vec::new();
    for file in files {
        if file
            .file_name()
            .is_some_and(|n| exempt_names.contains(&n.to_string_lossy().as_ref()))
        {
            continue;
        }
        let text = std::fs::read_to_string(&file)?;
        for (n, line) in text.lines().enumerate() {
            if needles.iter().any(|needle| line.contains(needle)) {
                hits.push(format!(
                    "{}:{}: {}",
                    file.strip_prefix(&root).unwrap_or(&file).display(),
                    n + 1,
                    line.trim()
                ));
            }
        }
    }
    Ok(hits)
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Box<dyn Error>> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_rs(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// Plan §7 step 7 (+ step 8c's baselines): write every PNG under
/// `crates/zenman-ui/tests/snapshots/` from the kittest+wgpu rasteriser, or skip with the reason the
/// test printed. This box has Vulkan (AMD integrated) and
/// GL and no lavapipe (plan OQ3), so the real path is the hardware adapter head-lessly — which is
/// also what `xvfb-run -a cargo xtask snapshots` exercises on CI images.
fn snapshots() -> Result<(), Box<dyn Error>> {
    let root = workspace_root()?;
    println!("$ cargo test -p zenman-ui --test snapshots -- --nocapture (ZENMAN_SNAPSHOT_WRITE=1)");
    let status = Command::new(cargo_bin())
        .current_dir(&root)
        .args([
            "test",
            "-p",
            "zenman-ui",
            "--test",
            "snapshots",
            "--",
            "--nocapture",
        ])
        .env("ZENMAN_SNAPSHOT_WRITE", "1")
        .status()?;
    if !status.success() {
        return Err("`cargo xtask snapshots`: the snapshot test failed (above)".into());
    }
    let dir = root.join("crates/zenman-ui/tests/snapshots");
    let mut written = 0usize;
    if dir.is_dir() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_file() && path.extension().is_some_and(|e| e == "png") {
                let bytes = std::fs::metadata(&path)?.len();
                println!(
                    "xtask snapshots: wrote {} ({bytes} bytes)",
                    path.strip_prefix(&root).unwrap_or(path.as_path()).display()
                );
                written += 1;
            }
        }
    }
    if written > 0 {
        println!(
            "xtask snapshots: {written} baseline(s) — human review against `docs/parity/` is              step 15's gate, never a pixel diff"
        );
        // The same gate `ci` runs, so an artefact that came out at the wrong size is reported here
        // and not only at the next `cargo xtask ci`.
        check_snapshot_artefacts(&root)?;
        Ok(())
    } else {
        println!(
            "xtask snapshots: no PNG written — the test printed its skip reason above (no adapter).              That is the plan §7 acceptance path for a machine without a rasteriser."
        );
        Ok(())
    }
}

fn step(args: &str) -> Result<(), Box<dyn Error>> {
    let mut parts = args.split_whitespace();
    let program = parts.next().unwrap_or("cargo");
    let mut cmd = Command::new(program);
    cmd.args(parts);
    println!("$ {args}");
    let status = cmd.status()?;
    if !status.success() {
        return Err(format!("`{args}` failed with {status}").into());
    }
    Ok(())
}

/// `cargo metadata --format-version 1`, from the workspace root this binary was built in.
fn metadata() -> Result<Value, Box<dyn Error>> {
    let root = workspace_root()?;
    let mut cmd = Command::new(cargo_bin());
    cmd.current_dir(&root)
        .args(["metadata", "--format-version", "1", "--locked"])
        .stdout(Stdio::piped());
    let output = cmd.spawn()?.wait_with_output()?;
    if !output.status.success() {
        return Err(format!(
            "`cargo metadata` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

/// The workspace root: this binary lives in `<root>/target/<profile>/xtask`.
pub(crate) fn workspace_root() -> Result<PathBuf, Box<dyn Error>> {
    let exe = std::env::current_exe()?;
    let mut dir = exe
        .parent()
        .ok_or("xtask executable has no parent directory")?
        .to_path_buf();
    for _ in 0..2 {
        dir = dir
            .parent()
            .ok_or("xtask is not inside a target directory")?
            .to_path_buf();
    }
    if !dir.join("Cargo.toml").is_file() {
        return Err(format!(
            "{} has no Cargo.toml; run `cargo xtask` from the workspace",
            dir.display()
        )
        .into());
    }
    Ok(dir)
}

fn cargo_bin() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

// --------------------------------------------------------------------------- deps

/// Rev provenance (plan F10: "mixed revs = error"). Every git source in the resolved graph must be
/// pinned to one full 40-character commit, and every `[patch.crates-io]` entry must agree with what
/// the graph actually used. An unused patch entry is INFO: the verbatim table from photocraft carries
/// members (e.g. `egui_kittest`, step 7) that this step does not depend on yet.
fn deps() -> Result<(), Box<dyn Error>> {
    let meta = metadata()?;
    let mut problems: Vec<String> = Vec::new();
    let mut infos: Vec<String> = Vec::new();

    // 1. one rev per git repository, and every rev a full commit hash. `packages[]` carries the source
    // string (`git+<url>?rev=<rev>#<commit>`); in this cargo version `resolve.nodes[].source` is null, so
    // the package list is the authoritative view of what the graph actually resolved.
    let packages = as_array(&meta["packages"]);
    let mut by_repo: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut git_rev_of: BTreeMap<String, String> = BTreeMap::new();
    for package in &packages {
        let Some(source) = package["source"].as_str() else {
            continue;
        };
        let Some(rest) = source.strip_prefix("git+") else {
            continue;
        };
        let Some((url_and_query, fragment)) = rest.split_once('#') else {
            problems.push(format!("git source without a commit fragment: {source}"));
            continue;
        };
        let Some((url, query)) = url_and_query.split_once("?rev=") else {
            problems.push(format!("git source is not pinned by `rev`: {source}"));
            continue;
        };
        if !is_full_rev(query) {
            problems.push(format!(
                "`rev = \"{query}\"` is not a full 40-character commit hash ({source})"
            ));
        }
        if !is_full_rev(fragment) {
            problems.push(format!(
                "resolved commit `\'{fragment}\'` is not a full hash ({source})"
            ));
        }
        let name = display(&package["name"]);
        git_rev_of.insert(name.clone(), query.to_string());
        by_repo.entry(url.to_string()).or_default().push((
            query.to_string(),
            format!("{name} {}", display(&package["version"])),
        ));
    }
    for (url, entries) in &by_repo {
        let mut revs: Vec<&str> = Vec::new();
        for (rev, _) in entries {
            if !revs.contains(&rev.as_str()) {
                revs.push(rev);
            }
        }
        println!(
            "git source {}: {} package(s), rev(s): {}",
            url,
            entries.len(),
            revs.iter()
                .map(|r| format!(
                    "{r} ({} pkg)",
                    entries.iter().filter(|(rev, _)| rev == r).count()
                ))
                .collect::<Vec<_>>()
                .join(", ")
        );
        if revs.len() > 1 {
            problems.push(format!(
                "{url} is used at {} different revs: {}",
                revs.len(),
                revs.join(" + ")
            ));
        }
    }
    if by_repo.is_empty() {
        problems.push(
            "the graph has no git sources at all: the craft-libs/egui/winit pins are missing"
                .to_string(),
        );
    }

    // 2. `[patch.crates-io]` must describe the graph, and the graph must not disagree with it.
    let patched = patch_table(&workspace_root()?.join("Cargo.toml"))?;
    for (name, patch_rev) in &patched {
        let used = git_rev_of.get(name).cloned();
        match used {
            Some(rev) => {
                if &rev != patch_rev {
                    problems.push(format!(
                        "[patch] {name} = \"{patch_rev}\" but the graph resolved \"{rev}\""
                    ));
                }
            }
            None => {
                infos.push(format!("[patch] {name} (rev {patch_rev}) is not in the crate graph yet: INFO, it becomes a gate when the step that needs it lands"));
            }
        }
    }

    // 3. the crate boundary (plan §5.1: "xtask deps fails if zenman-core depends on egui/eframe/craft-ui").
    let packages = as_array(&meta["packages"]);
    if let Some(core) = packages
        .iter()
        .find(|p| p["name"].as_str() == Some("zenman-core"))
    {
        for dep in core["dependencies"].as_array().unwrap_or(&Vec::new()) {
            if matches!(
                dep["name"].as_str(),
                Some("egui")
                    | Some("eframe")
                    | Some("craft-ui")
                    | Some("wgpu")
                    | Some("egui_extras")
            ) {
                problems.push(format!("zenman-core depends on `{}`: the headless crate must stay free of the UI stack (plan §5.1)", display(&dep["name"])));
            }
        }
    }

    for info in &infos {
        println!("xtask deps: INFO: {info}");
    }
    if !problems.is_empty() {
        return Err(format!("xtask deps failed:\n  {}", problems.join("\n  ")).into());
    }
    println!(
        "xtask deps: every git source is pinned to one full rev, [patch] agrees, and zenman-core is UI-free"
    );
    Ok(())
}

fn is_full_rev(rev: &str) -> bool {
    rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit()) && rev == rev.to_ascii_lowercase()
}

/// `value.as_array()` or nothing, owned so callers can keep the elements past a temporary.
fn as_array(value: &Value) -> Vec<Value> {
    value.as_array().cloned().unwrap_or_default()
}

fn display(value: &Value) -> String {
    value.as_str().unwrap_or("<none>").to_string()
}

/// Read the `rev = "..."` of every entry of the root `[patch.crates-io]` table.
fn patch_table(path: &Path) -> Result<Vec<(String, String)>, Box<dyn Error>> {
    let text = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    let mut in_patch = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_patch = line == "[patch.crates-io]";
            continue;
        }
        if !in_patch || line.starts_with('#') {
            continue;
        }
        let Some((name, rest)) = line.split_once('=') else {
            continue;
        };
        let Some(rev_start) = rest.find("rev = \"") else {
            continue;
        };
        let rev: String = rest[rev_start + "rev = \"".len()..]
            .chars()
            .take_while(|c| *c != '"')
            .collect();
        if rev.is_empty() {
            return Err(format!(
                "{}: `[patch]` entry `{}` has no rev",
                path.display(),
                name.trim()
            )
            .into());
        }
        out.push((name.trim().to_string(), rev));
    }
    if out.is_empty() {
        return Err(format!("{}: no [patch.crates-io] entries found", path.display()).into());
    }
    Ok(out)
}

// --------------------------------------------------------------------------- licenses

/// Every non-local package's declared licence must be satisfiable from the allow list, or be
/// explicitly waived in `deny.toml`, and no `[bans] deny` entry may be present in the graph.
fn licenses() -> Result<(), Box<dyn Error>> {
    let meta = metadata()?;
    let rules = licence_rules(&workspace_root()?.join("deny.toml"))?;
    let members = as_array(&meta["workspace_members"]);
    let local: Vec<String> = members
        .iter()
        .filter_map(|v| v["id"].as_str().and_then(|id| id.split_whitespace().next()))
        .map(|name: &str| name.to_string())
        .collect();

    let mut problems: Vec<String> = Vec::new();
    let mut waivers = 0;
    let mut checked = 0;
    let packages = as_array(&meta["packages"]);
    for package in &packages {
        let name = display(&package["name"]);
        if local.contains(&name) {
            continue;
        }
        let version = display(&package["version"]);
        let expression = package["license"].as_str().unwrap_or("").trim().to_string();
        if expression.is_empty() {
            problems.push(format!("{name} {version} declares no licence"));
            continue;
        }
        checked += 1;
        let denied = rules.denied.iter().any(|d| d == &name);
        if denied {
            problems.push(format!("{name} {version} is in deny.toml's [bans] deny list: a licence-clean wrapper is not a licence for the code it bundles"));
        }
        let waived = rules
            .waived
            .iter()
            .any(|(package_name, _)| package_name == &name);
        if waived {
            waivers += 1;
            continue;
        }
        if !expression_is_satisfiable(&expression, &rules.allow) {
            problems.push(format!("{name} {version} licence {expression:?} is not allowlisted (add an explicit [[licenses.waiver]] with a reason, or a licence entry with a plan reference)"));
        }
    }
    if !problems.is_empty() {
        return Err(format!("xtask licenses failed:\n  {}", problems.join("\n  ")).into());
    }
    println!(
        "xtask licenses: {checked} package licence expressions accepted ({} allowlisted ids, {waivers} waived)",
        rules.allow.len()
    );
    Ok(())
}

#[derive(Default)]
struct LicenceRules {
    allow: Vec<String>,
    waived: Vec<(String, String)>,
    denied: Vec<String>,
}

/// Parse `deny.toml` as data: the `[licenses] allow` list, the `[[licenses.waiver]]` package names and
/// the `[bans] deny` names. A real TOML parser would be tidier; a second dependency inside the licence
/// gate itself is the sort of thing the gate exists to notice, so this is a deliberately narrow reader
/// (arrays of quoted strings, and `key = "value"` lines inside tables).
pub(crate) fn licence_rules(path: &Path) -> Result<LicenceRules, Box<dyn Error>> {
    let text = std::fs::read_to_string(path)?;
    let mut rules = LicenceRules::default();
    let mut section = String::new();
    let mut in_allow = false;
    for raw in text.lines() {
        let line = strip_comment(raw).trim().to_string();
        if line.is_empty() {
            continue;
        }
        if let Some(header) = line.strip_prefix("[[") {
            section = header.trim_end_matches("]]").to_string();
            in_allow = false;
            continue;
        }
        if line.starts_with('[') {
            section = line.trim_matches('[').trim_matches(']').to_string();
            in_allow = false;
            continue;
        }
        if section == "licenses" && line.starts_with("allow") && line.contains('[') {
            in_allow = true;
            if line.contains(']') {
                in_allow = false; // a one-line array: not the shape we ship, but do not hang
            }
            continue;
        }
        if in_allow {
            if line.starts_with(']') {
                in_allow = false;
                continue;
            }
            let id = line.trim_end_matches(',').trim();
            if id.starts_with('"') && id.ends_with('"') && id.len() > 1 {
                rules.allow.push(id.trim_matches('"').to_string());
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        if section == "licenses.waiver" && key.trim() == "package" {
            rules.waived.push((unquote(value), String::new()));
        }
        if section == "bans"
            && let Some(name) = line
                .split("name =")
                .nth(1)
                .and_then(|rest| rest.trim().strip_prefix('"'))
                .and_then(|rest| rest.split('"').next())
        {
            rules.denied.push(name.to_string());
        }
    }
    if rules.allow.is_empty() {
        return Err(format!("{}: no [licenses] allow entries", path.display()).into());
    }
    Ok(rules)
}

/// Drop a trailing `#` comment (no `#` appears inside any licence id or path we read).
fn strip_comment(line: &str) -> &str {
    line.split('#').next().unwrap_or(line)
}

fn unquote(value: &str) -> String {
    value.trim().trim_matches('"').to_string()
}

/// SPDX satisfiability, in the direction that matters for a gate: every `AND` term must be covered, and
/// an `OR` group passes when one alternative is. `(...)` is flattened; `/` is cargo's old `AND`.
pub(crate) fn expression_is_satisfiable(expression: &str, allow: &[String]) -> bool {
    let cleaned = expression.replace(['(', ')'], " ");
    let mut and_terms: Vec<String> = Vec::new();
    for chunk in cleaned.split(" AND ") {
        and_terms.push(chunk.trim().to_string());
    }
    let mut flat: Vec<String> = Vec::new();
    for term in and_terms {
        if term.contains('/') {
            flat.extend(term.split('/').map(|s| s.trim().to_string()));
        } else {
            flat.push(term);
        }
    }
    flat.iter().filter(|t| !t.is_empty()).all(|term| {
        if term.contains(" OR ") {
            term.split(" OR ").any(|id| allow.contains(&normalise(id)))
        } else {
            allow.contains(&normalise(term))
        }
    })
}

fn normalise(id: &str) -> String {
    id.trim().trim_matches('"').to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        check_snapshot_artefacts, harness_size, harness_snapshot_names, png_string_literals,
        snapshot_name_is_recognised,
    };

    /// A copy of the committed artefacts under `<temp>/xtask-snap-<pid>-<tag>`, so the gate can be
    /// seen failing without touching the tree. `size_override` rewrites the copied `SIZE`, which is
    /// how a test can make every artefact "the wrong dimensions" the way a real size change does.
    /// (The copy is what the gate reads; the committed tree is never touched.)
    fn artefact_tree(tag: &str, size_override: Option<(u32, u32)>) -> std::path::PathBuf {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask is a workspace member")
            .to_path_buf();
        let root = std::env::temp_dir().join(format!("xtask-snap-{}-{tag}", std::process::id()));
        let dir = root.join("crates/zenman-ui/tests/snapshots");
        std::fs::create_dir_all(&dir).expect("the copy directory is creatable");
        let mut source = harness_snapshot_names(&repo).expect("the test names its baselines");
        for name in &source {
            std::fs::copy(
                repo.join("crates/zenman-ui/tests/snapshots").join(name),
                dir.join(name),
            )
            .expect("the committed artefact is copyable");
        }
        source.sort();
        assert!(!source.is_empty(), "there are committed baselines to copy");
        let text = std::fs::read_to_string(repo.join("crates/zenman-ui/tests/snapshots.rs"))
            .expect("the snapshot test is readable");
        let text = match size_override {
            None => text,
            Some((w, h)) => {
                let (cur_w, cur_h) = harness_size(&repo).expect("the test states its size");
                text.replace(
                    &format!("egui::vec2({}.0, {}.0)", cur_w, cur_h),
                    &format!("egui::vec2({}.0, {}.0)", w, h),
                )
            }
        };
        std::fs::write(root.join("crates/zenman-ui/tests/snapshots.rs"), text)
            .expect("the copied test is writable");
        root
    }

    #[test]
    fn a_corrupt_or_stale_artefact_fails_the_gate() {
        // The green case first: the committed artefacts, copied verbatim.
        let good = artefact_tree("green", None);
        let summary = check_snapshot_artefacts(&good).expect("the committed artefacts pass");
        println!("gate, faithful copy: {summary}");

        // A truncated PNG.
        let truncated = good.join("crates/zenman-ui/tests/snapshots/02-file-list.png");
        let mut bytes = std::fs::read(&truncated).expect("the artefact is readable");
        let cut = bytes.len() - 40;
        bytes.truncate(cut);
        std::fs::write(&truncated, &bytes).expect("the copy is writable");
        let error = check_snapshot_artefacts(&good).expect_err("a truncated PNG must fail");
        println!("gate, truncated: {error}");
        assert!(
            error.to_string().contains("ends early"),
            "the reason must name the fault"
        );

        // A stale artefact the test does not render.
        let stale = artefact_tree("stale", None);
        let bytes = std::fs::read(stale.join("crates/zenman-ui/tests/snapshots/01-empty.png"))
            .expect("the artefact is readable");
        std::fs::write(
            stale.join("crates/zenman-ui/tests/snapshots/09-removed-scene.png"),
            &bytes,
        )
        .expect("the copy is writable");
        let error = check_snapshot_artefacts(&stale).expect_err("a stray PNG must fail");
        println!("gate, stale: {error}");
        assert!(error.to_string().contains("not a baseline"));

        // A missing artefact.
        let missing = artefact_tree("missing", None);
        std::fs::remove_file(missing.join("crates/zenman-ui/tests/snapshots/03-file-grid.png"))
            .expect("the copy is deletable");
        let error = check_snapshot_artefacts(&missing).expect_err("a missing baseline must fail");
        println!("gate, missing: {error}");
        assert!(error.to_string().contains("03-file-grid.png is missing"));

        // And the dimension property, driven from the *test's* own `SIZE`: a harness that changes
        // size without a re-render is exactly the drift the gate exists to catch.
        let resized = artefact_tree("resized", Some((640, 480)));
        let error = check_snapshot_artefacts(&resized).expect_err("a wrong-size PNG must fail");
        println!("gate, wrong dimensions: {error}");
        assert!(
            error
                .to_string()
                .contains("the shell geometry says 640x480"),
            "the expected size must come from the test's SIZE"
        );
    }

    #[test]
    fn the_baseline_naming_scheme_is_accepted_and_strays_are_not() {
        for good in ["01-empty.png", "02-file-list.png", "14-batch-rename.png"] {
            assert!(snapshot_name_is_recognised(good), "{good} is a baseline");
        }
        for bad in [
            "empty.png",           // no order prefix
            "1-empty.png",         // one digit
            "01-.png",             // empty slug
            "01-file_.png",        // not a lowercase slug
            "01-file-list.PNG",    // the extension is lowercase
            "01-file-list.jpg",    // not a PNG
            "01-file-list.png.rs", // not an artefact
            "README.png",
        ] {
            assert!(
                !snapshot_name_is_recognised(bad),
                "{bad} must be refused by the artefact gate"
            );
        }
    }

    #[test]
    fn the_expected_baselines_come_from_the_test_source_not_from_comments() {
        let source = "//! writes `tests/snapshots/01-empty.png` from the rasteriser\n\
                      const SIZE: egui::Vec2 = egui::vec2(1440.0, 900.0);\n\
                      \x20   Self::Empty => \"01-empty.png\",\n\
                          } => \"02-file-list.png\",\n\
                      let other = \"notes.txt\";\n";
        assert_eq!(
            png_string_literals(source),
            vec!["01-empty.png".to_string(), "02-file-list.png".to_string()],
            "the artefact set is what the test renders, deduplicated and in order"
        );
        assert!(
            png_string_literals("// only a comment mentions \"09-stale.png\"").is_empty(),
            "prose about a PNG must not create a required artefact"
        );
    }
}
