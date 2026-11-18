//! `cargo xtask <task>` — the in-repo gates (plan §5.1 `xtask/src/main.rs` row, §7 step 1).
//!
//! `ci` is the gate a reviewer trusts; `deps` and `licenses` are the two custom linters it ends with.
//! `snapshots`/`parity` are honest stubs that print the step that lands them, because the plan
//! sequences the `egui_kittest` harness (step 7) and the human parity review (step 15) after this
//! commit. `contracts` is real since step 2: it guards the frozen Go golden capture in
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

mod contracts;
mod fixture_tree;

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
        "snapshots" => stub(
            "snapshots",
            7,
            "`egui_kittest` baselines from the CI software rasteriser (plan §7 step 7, §5.10)",
        ),
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
                "usage: cargo xtask <ci|deps|licenses|snapshots|parity|contracts|fixture-tree|golden-index>"
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
    step("cargo clippy --workspace --all-targets -- -D warnings")?;
    step("cargo test --workspace")?;
    contracts::contracts()?;
    deps()?;
    licenses()?;
    println!("xtask ci: all gates green");
    Ok(())
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
fn licence_rules(path: &Path) -> Result<LicenceRules, Box<dyn Error>> {
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
fn expression_is_satisfiable(expression: &str, allow: &[String]) -> bool {
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
