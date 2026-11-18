//! `cargo xtask fixture-tree` — the fixture-tree gate.
//!
//! `scripts/make-fixtures.py` is the source of truth for `tests/fixtures/tree/`, and that directory
//! is deliberately untracked (`.gitignore` step-2 block: git can only store modes 100644 / 100755 /
//! 120000, so a `chmod 000` file cannot survive a checkout). `docs/fixtures.md` is the checked-in promise
//! about what the generator produces, and this gate is the only thing that keeps that promise honest.
//!
//! It regenerates the tree into a throwaway directory and compares **entry count + kind + octal mode +
//! name** against the `fixture-tree/1` block of `docs/fixtures.md`. The comparison is on hex-encoded names
//! because the tree contains `weird\rname.txt` (a carriage return inside a file name), a backslash, a
//! `#`, a `'`, and two `café.txt` names that differ only in Unicode normalisation form; hex is the
//! only spelling of those names that survives copy/paste, markdown rendering and a diff.
//!
//! Two decisions worth keeping:
//!
//! * The scratch tree lives under `target/fixture-tree-gate/` and is **not** deleted: `clippy.toml`
//!   bans `std::fs::remove_dir_all` workspace-wide (deletion goes through the trash-first op, which a
//!   build gate must not smuggle), and `cargo clean` owns `target/` anyway. The path is printed.
//! * A missing `python3` or a missing `docs/fixtures.md` is an explicit, printed **skip** — never a silent
//!   pass. `tests/fixtures/` is ignored, so a fresh clone legitimately has no manifest to check.

use std::collections::BTreeMap;
use std::error::Error;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::workspace_root;

/// Opens the machine-readable block inside `docs/fixtures.md`.
const FENCE: &str = "```fixture-tree/1";

/// Relative path of the manifest: inside the ignored `tests/fixtures/`, hence optional.
const MANIFEST: &str = "docs/fixtures.md";

/// `name hex -> (kind, octal mode)`.
type Tree = BTreeMap<String, (String, String)>;

pub(crate) fn fixture_tree() -> Result<(), Box<dyn Error>> {
    let root = workspace_root()?;

    if !python3_usable()? {
        println!(
            "xtask fixture-tree: SKIPPED — `python3 --version` did not succeed, so the fixture tree \
             cannot be regenerated (this gate is not vacuous: it is simply not runnable here)"
        );
        return Ok(());
    }
    let manifest = root.join(MANIFEST);
    if !manifest.is_file() {
        println!(
            "xtask fixture-tree: SKIPPED — {} does not exist. It lives under the ignored \
             tests/fixtures/, so a fresh clone has none; run `python3 scripts/make-fixtures.py --root \
             tests/fixtures/tree` (and restore the {FENCE} block) to enable this gate.",
            manifest.display()
        );
        return Ok(());
    }

    let (declared, expected) = manifest_entries(&manifest)?;
    let fresh = regenerate_tree(&root)?;
    let got = read_tree(&fresh)?;

    let mut problems: Vec<String> = Vec::new();
    if declared != got.len() {
        problems.push(format!(
            "the block declares count {declared} but holds {} row(s)",
            expected.len()
        ));
    }
    if expected.len() != got.len() {
        problems.push(format!(
            "{MANIFEST} lists {} entr(ies), a freshly generated tree has {}",
            expected.len(),
            got.len()
        ));
    }
    for name in expected.keys().filter(|n| !got.contains_key(*n)) {
        problems.push(format!("generated tree is missing {name}"));
    }
    for name in got.keys().filter(|n| !expected.contains_key(*n)) {
        problems.push(format!("generated tree has an unlisted entry {name}"));
    }
    for (name, want) in &expected {
        if let Some((kind, mode)) = got.get(name)
            && (kind != &want.0 || mode != &want.1)
        {
            problems.push(format!(
                "{name}: manifest says kind {} mode {} but the generator makes kind {kind} mode {mode}",
                want.0, want.1
            ));
        }
    }
    if !problems.is_empty() {
        for p in problems.iter().take(20) {
            eprintln!("contracts: FAIL fixture-tree {p}");
        }
        return Err(format!(
            "contracts: the fixture tree no longer matches {MANIFEST} ({} problem(s); scratch tree \
             kept at {})",
            problems.len(),
            fresh.display()
        )
        .into());
    }
    println!(
        "contracts: fixture-tree ok — {} entr(ies) match {MANIFEST} on kind, octal mode and \
         hex-encoded name (scratch tree left at {})",
        got.len(),
        fresh.display()
    );
    Ok(())
}

/// `python3 --version`, so that a machine without it reports a skip rather than a spawn error.
fn python3_usable() -> Result<bool, Box<dyn Error>> {
    match Command::new("python3")
        .arg("--version")
        // --version prints on stdout: the probe must stay quiet, the gate's own line is the output.
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Err(_) => Ok(false),
        Ok(child) => Ok(child.wait_with_output()?.status.success()),
    }
}

/// Run the generator and return the scratch tree it built. The name is stable on purpose:
/// `make-fixtures.py` replaces an existing `--root` wholesale (`relax` + `rmtree`), so re-running the
/// gate never accumulates scratch trees under `target/`.
fn regenerate_tree(root: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let scratch_parent = root.join("target").join("fixture-tree-gate");
    let fresh = scratch_parent.join("tree");
    std::fs::create_dir_all(&scratch_parent).map_err(|e| {
        format!(
            "xtask fixture-tree: cannot create {}: {e}",
            scratch_parent.display()
        )
    })?;
    let mut cmd = Command::new("python3");
    cmd.current_dir(root)
        .arg("scripts/make-fixtures.py")
        .arg("--root")
        .arg(&fresh)
        .stderr(Stdio::piped());
    let output = cmd
        .spawn()
        .map_err(|e| format!("xtask fixture-tree: cannot run the generator: {e}"))?
        .wait_with_output()?;
    if !output.status.success() {
        return Err(format!(
            "xtask fixture-tree: `python3 scripts/make-fixtures.py --root {}` failed with {}: {}",
            fresh.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(fresh)
}

/// Parse the `fixture-tree/1` block: `{"count": n, "entries": [[hex, kind, mode], ...]}`.
fn manifest_entries(path: &Path) -> Result<(usize, Tree), Box<dyn Error>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("xtask fixture-tree: cannot read {}: {e}", path.display()))?;
    let after = text.split_once(FENCE).ok_or_else(|| {
        format!(
            "xtask fixture-tree: {} has no {FENCE} block — the manifest is prose-only, so nothing is \
             machine-checkable (add the generated block)",
            path.display()
        )
    })?;
    let body = after
        .1
        .split_once("\n```")
        .ok_or(format!(
            "xtask fixture-tree: {} has an unterminated {FENCE} block",
            path.display()
        ))?
        .0;
    let value: Value = serde_json::from_str(body.trim()).map_err(|e| {
        format!(
            "xtask fixture-tree: the {FENCE} block of {} is not valid JSON: {e}",
            path.display()
        )
    })?;
    let declared = value["count"]
        .as_u64()
        .ok_or("xtask fixture-tree: the manifest block has no numeric `count`")?;
    let rows = value["entries"]
        .as_array()
        .ok_or("xtask fixture-tree: the manifest block has no `entries` array")?;
    let mut tree: Tree = BTreeMap::new();
    for (i, row) in rows.iter().enumerate() {
        let row = row.as_array().ok_or_else(|| {
            format!("xtask fixture-tree: manifest entry {i} is not a 3-element array")
        })?;
        if row.len() != 3 {
            return Err(format!(
                "xtask fixture-tree: manifest entry {i} must be [name-hex, kind, mode], found {} field(s)",
                row.len()
            )
            .into());
        }
        let name = row[0].as_str().unwrap_or("");
        let kind = row[1].as_str().unwrap_or("");
        let mode = row[2].as_str().unwrap_or("");
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_hexdigit()) || name.len() % 2 != 0 {
            return Err(format!(
                "xtask fixture-tree: manifest entry {i} has a non-hex name {name:?}"
            )
            .into());
        }
        if !matches!(kind, "f" | "d" | "l" | "p") {
            return Err(format!(
                "xtask fixture-tree: manifest entry {i} has unknown kind {kind:?}"
            )
            .into());
        }
        if mode.len() != 4 || !mode.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
            return Err(format!(
                "xtask fixture-tree: manifest entry {i} has a non-octal mode {mode:?}"
            )
            .into());
        }
        if tree
            .insert(name.to_string(), (kind.to_string(), mode.to_string()))
            .is_some()
        {
            return Err(format!("xtask fixture-tree: manifest lists {name} twice").into());
        }
    }
    Ok((
        usize::try_from(declared).map_err(|_| "manifest count is huge")?,
        tree,
    ))
}

/// Walk a generated tree without following symlinks, keyed by hex of the root-relative path.
fn read_tree(root: &Path) -> Result<Tree, Box<dyn Error>> {
    let mut tree: Tree = BTreeMap::new();
    walk(root, &PathBuf::new(), &mut tree)?;
    Ok(tree)
}

fn walk(dir: &Path, rel: &Path, tree: &mut Tree) -> Result<(), Box<dyn Error>> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("xtask fixture-tree: cannot read {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let rel_child = rel.join(entry.file_name());
        let kind = if entry.file_type()?.is_symlink() {
            "l"
        } else if entry.file_type()?.is_dir() {
            "d"
        } else if entry.file_type()?.is_fifo() {
            "p"
        } else {
            "f"
        };
        let mode = entry.metadata()?.permissions().mode() & 0o777;
        tree.insert(
            hex_name(&rel_child),
            (kind.to_string(), format!("{mode:04o}")),
        );
        if kind == "d" {
            walk(&entry.path(), &rel_child, tree)?;
        }
    }
    Ok(())
}

/// Lower-case hex of a path's raw name bytes, `/`-separated relative to the tree root.
fn hex_name(path: &Path) -> String {
    path.as_os_str()
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
