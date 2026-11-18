//! The asset gate — `cargo xtask assets`, run by `cargo xtask ci`.
//!
//! `ATTRIBUTION.md` is the human record of every non-code asset's licence (plan §5.1 house rule 3:
//! an asset and its attribution land in the *same* commit). Until step 7c that claim was carried by
//! prose alone: `cargo xtask licenses` reads `cargo metadata` and `deny.toml` and never looks at a
//! file under `assets/`. This is the check that claim always described:
//!
//! * **every file under `assets/**` is listed** in `ATTRIBUTION.md`, by literal repo-relative path —
//!   so an unattributed icon fails the gate the moment it is added;
//! * **every path `ATTRIBUTION.md` names exists** — so a rename or deletion cannot leave a phantom
//!   row (and a glob or a brace form cannot stand in for a row, which is how
//!   `` chevron-{left,right,up,down}.svg `` hid four files from the old table);
//! * **every listed row carries a licence id from `deny.toml`'s allowlist**, checked with the same
//!   SPDX satisfiability the Rust-dependency gate uses.

use std::error::Error;
use std::path::Path;

use crate::licence_rules;

/// A row of `ATTRIBUTION.md`: the asset it names and the licence it claims.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The literal repo-relative path, e.g. `assets/icons/folder.svg`.
    pub path: String,
    /// The row's licence expression, or empty when the row has none — which is a failure.
    pub licence: String,
    /// 1-based line of the row in `ATTRIBUTION.md`.
    pub line: usize,
}

/// What the gate read, for the summary line.
pub struct Report {
    pub files: Vec<String>,
    pub entries: Vec<Entry>,
    pub allow_ids: usize,
}

/// Run the gate.
pub fn assets() -> Result<(), Box<dyn Error>> {
    let root = crate::workspace_root()?;
    let report = check(&root)?;
    println!(
        "xtask assets: all {} file(s) under `assets/` are attributed by literal path with a licence \
         from `deny.toml` ({} row(s), {} allowlisted licence id(s))",
        report.files.len(),
        report.entries.len(),
        report.allow_ids
    );
    Ok(())
}

/// The gate itself, over a workspace root.
pub fn check(root: &Path) -> Result<Report, Box<dyn Error>> {
    let attribution = root.join("ATTRIBUTION.md");
    let text = std::fs::read_to_string(&attribution).map_err(|e| {
        format!(
            "{}: cannot read the attribution file: {e}",
            attribution.display()
        )
    })?;
    let rules = licence_rules(&root.join("deny.toml"))?;
    let mut problems: Vec<String> = Vec::new();

    let entries = table_entries(&text);

    // (b) every path the file names must be a literal path that exists.
    for (line, claim) in claimed_paths(&text) {
        if claim.contains(['*', '{', '}', ' ']) {
            problems.push(format!(
                "ATTRIBUTION.md:{line}: `{claim}` is a glob or brace form; the gate matches literal \
                 paths, so every asset needs its own row"
            ));
            continue;
        }
        // A claim ending in `/` is a directory heading; anything else must be a file that ships.
        let exists = if claim.ends_with('/') {
            root.join(&claim).is_dir()
        } else {
            root.join(&claim).is_file()
        };
        if !exists {
            problems.push(format!(
                "ATTRIBUTION.md:{line}: `{claim}` does not exist (renamed or deleted asset? the row \
                 must describe a file that ships)"
            ));
        }
    }

    // (c) every row carries an allowlisted licence.
    for entry in &entries {
        if entry.licence.is_empty() {
            problems.push(format!(
                "ATTRIBUTION.md:{}: the `{}` row names no licence",
                entry.line, entry.path
            ));
        } else if !crate::expression_is_satisfiable(&entry.licence, &rules.allow) {
            problems.push(format!(
                "ATTRIBUTION.md:{}: `{}` is not on `deny.toml`'s [licenses] allowlist (add the \
                 licence there with a plan reference, or fix the row)",
                entry.line, entry.licence
            ));
        }
    }

    // (a) every file under `assets/` is listed, exactly once.
    let files = asset_files(root)?;
    for path in &files {
        let hits = entries.iter().filter(|e| &e.path == path).count();
        if hits == 0 {
            problems.push(format!(
                "{path}: not attributed — add a row naming it by literal path with its licence \
                 (house rule 3: asset and attribution land in the same commit)"
            ));
        } else if hits > 1 {
            problems.push(format!(
                "{path}: attributed {hits} times (one row per asset)"
            ));
        }
    }
    let stray: Vec<&String> = entries
        .iter()
        .map(|e| &e.path)
        .filter(|path| !files.iter().any(|file| file == *path))
        .collect();
    if !stray.is_empty() {
        problems.push(format!(
            "ATTRIBUTION.md lists {} row(s) that are not files under `assets/`: {}",
            stray.len(),
            stray
                .iter()
                .map(|s| s.as_str())
                .take(4)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    if !problems.is_empty() {
        return Err(format!("xtask assets failed:\n  {}", problems.join("\n  ")).into());
    }
    Ok(Report {
        files,
        entries,
        allow_ids: rules.allow.len(),
    })
}

/// Every file under `assets/`, repo-relative and `/`-separated, sorted.
pub fn asset_files(root: &Path) -> Result<Vec<String>, Box<dyn Error>> {
    let dir = root.join("assets");
    let mut files = Vec::new();
    collect(&dir, root, &mut files)?;
    files.sort();
    if files.is_empty() {
        return Err(format!("{}: no assets at all?", dir.display()).into());
    }
    Ok(files)
}

fn collect(dir: &Path, root: &Path, out: &mut Vec<String>) -> Result<(), Box<dyn Error>> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, root, out)?;
        } else if let Ok(relative) = path.strip_prefix(root) {
            out.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

/// The markdown table rows whose first cell names an asset. The licence is read from the column
/// the table's own header calls `Licence`, so a description column can never be mistaken for one.
pub fn table_entries(text: &str) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut licence_column: Option<usize> = None;
    let mut previous: Option<(usize, Vec<String>)> = None;
    for (ix, raw) in text.lines().enumerate() {
        let line = raw.trim();
        let Some(rest) = line.strip_prefix('|') else {
            previous = None;
            continue;
        };
        let cells: Vec<String> = rest
            .split('|')
            .filter_map(|cell| {
                let cell = cell.trim();
                (!cell.is_empty() && !cell.chars().all(|c| c == '-'))
                    .then(|| cell.trim_matches('`').trim().to_string())
            })
            .collect();
        if cells.is_empty() {
            previous = None;
            continue;
        }
        if line.contains("---") {
            // The separator row: the row above it was the header.
            licence_column = previous.and_then(|(_, header)| {
                header.iter().position(|cell| {
                    cell.eq_ignore_ascii_case("licence") || cell.eq_ignore_ascii_case("license")
                })
            });
            previous = None;
            continue;
        }
        if first_is_asset_path(&cells) {
            let licence = match licence_column {
                Some(column) => cells.get(column).cloned().unwrap_or_default(),
                // A table with no Licence header column: the only cell that can be the licence is
                // one that reads like an SPDX expression.
                None => cells
                    .iter()
                    .skip(1)
                    .find(|cell| looks_like_licence(cell))
                    .cloned()
                    .unwrap_or_default(),
            };
            entries.push(Entry {
                path: cells[0].clone(),
                licence,
                line: ix + 1,
            });
        }
        previous = Some((ix + 1, cells));
    }
    entries
}

fn first_is_asset_path(cells: &[String]) -> bool {
    cells
        .first()
        .is_some_and(|cell| cell.starts_with("assets/"))
}

/// A licence cell is a short SPDX-ish expression with no prose in it.
fn looks_like_licence(cell: &str) -> bool {
    let words: Vec<&str> = cell.split_whitespace().collect();
    !words.is_empty()
        && words.iter().all(|word| {
            word.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | ':'))
        })
        && words.iter().any(|word| {
            word.chars().any(|c| c.is_ascii_digit())
                || matches!(word.to_uppercase().as_str(), "MIT" | "OFL" | "ZLIB" | "ISC")
        })
}

/// Every path `ATTRIBUTION.md` claims: backticked spans and table cells that start with `assets/`.
pub fn claimed_paths(text: &str) -> Vec<(usize, String)> {
    let mut claims: Vec<(usize, String)> = Vec::new();
    for (ix, raw) in text.lines().enumerate() {
        let spans: Vec<&str> = backticked(raw).collect();
        for span in &spans {
            if span.starts_with("assets/") {
                claims.push((ix + 1, (*span).to_string()));
            }
        }
        if let Some(rest) = raw.trim().strip_prefix('|') {
            for cell in rest.split('|') {
                let cell = cell.trim().trim_matches('`').trim();
                if cell.starts_with("assets/") && !spans.contains(&cell) {
                    claims.push((ix + 1, cell.to_string()));
                }
            }
        }
    }
    claims
}

/// The contents of each `` `…` `` span of a line.
fn backticked(line: &str) -> impl Iterator<Item = &str> {
    let mut rest = line;
    std::iter::from_fn(move || {
        let start = rest.find('`')?;
        let after = &rest[start + 1..];
        let end = after.find('`')?;
        let found = &after[..end];
        rest = &after[end + 1..];
        Some(found)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The workspace root as seen from a test binary (cwd is `xtask/`, not `<root>/target/…`).
    fn workspace_from_manifest() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask is a workspace member")
            .to_path_buf()
    }

    #[test]
    fn rows_are_read_by_literal_path_from_the_licence_column() {
        let text = "| File | Licence | Used for |\n|---|---|---|\n| `assets/icons/folder.svg` | MIT OR Apache-2.0 | rows |\n";
        let entries = table_entries(text);
        assert_eq!(
            entries
                .first()
                .map(|e| (e.path.as_str(), e.licence.as_str())),
            Some(("assets/icons/folder.svg", "MIT OR Apache-2.0"))
        );
    }

    #[test]
    fn a_row_with_no_licence_is_reported_not_dropped() {
        let text = "| File | Used for |\n|---|---|\n| `assets/icons/folder.svg` | rows |\n";
        let entries = table_entries(text);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].licence.is_empty(), "{entries:?}");
    }

    #[test]
    fn brace_and_glob_forms_are_claims_the_gate_rejects() {
        let text = "## `assets/icons/*.svg`\n| `assets/icons/chevron-{left,right}.svg` | MIT |\n";
        let claims = claimed_paths(text);
        assert_eq!(claims.len(), 2, "{claims:?}");
        assert!(claims.iter().all(|(_, path)| path.starts_with("assets/")));
        assert!(claims.iter().any(|(_, path)| path.contains('{')));
    }

    #[test]
    fn the_real_attribution_file_passes_the_gate() {
        let root = workspace_from_manifest();
        let report = check(&root).expect("gate is green");
        assert_eq!(report.files.len(), report.entries.len());
        assert!(report.files.iter().any(|f| f == "assets/icons/folder.svg"));
    }
}
