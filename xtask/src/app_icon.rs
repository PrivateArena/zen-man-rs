//! The application-icon gate — `cargo xtask app-icon`, run by `cargo xtask ci`.
//!
//! `assets/app-icon/` is the one asset directory nothing compiles: the `.ico` goes into the Windows
//! executable through `apps/zenman/build.rs`, the `.icns` into a macOS bundle through
//! `scripts/make-app-bundle.sh`, the `linux/` ladder into `$XDG_DATA_HOME` through
//! `zenman --install-desktop-entry`, and the 256 px PNG into every binary through `include_bytes!`.
//! A wrong size, a truncated container or a regenerated-but-not-committed artefact is therefore
//! invisible until somebody looks at a taskbar. So the gate says it here instead:
//!
//! * **every artefact the pipeline is supposed to emit exists**, and is a *valid* container of the
//!   geometry its name claims — PNG signature/chunks/CRCs through [`crate::png`], plus a real ICO
//!   directory and a real ICNS icon-type list;
//! * **the Windows and Linux sets match the code that reads them** — the sizes listed here are the
//!   sizes `apps/zenman/src/app_icon.rs` names in its `include_bytes!` paths and the sizes
//!   `apps/zenman/build.rs` compiles;
//! * **the master still describes the artwork** — by re-running `scripts/make-app-icon.py --check`,
//!   which re-derives the master's pixels from `assets/icons/prototype-icon-2.png` and diffs them.
//!   That rung is a printed **skip** where `python3` or Pillow is absent, exactly like
//!   [`crate::fixture_tree`]'s skip, because the structural rungs above need no toolchain at all.
//!
//! `cargo xtask assets` stays a separate gate: it answers "is this licensed?", this one answers "is
//! this the right picture at the right size?".

use std::error::Error;
use std::path::Path;
use std::process::Command;

use crate::png;

/// Master art: square, and the size `scripts/make-app-icon.py` names `MASTER_SIZE`.
const MASTER_SIDE: u32 = 1024;
/// The PNG the binary embeds, and therefore the largest side a window icon is asked for.
const RUNTIME_SIDE: u32 = 256;
/// hicolor ladder, and the `HICOLOR` table in `apps/zenman/src/app_icon.rs`.
const LINUX_SIDES: [u32; 5] = [16, 32, 48, 128, 256];
/// The ICO directory a Windows shell reads entries out of, largest last.
const ICO_SIDES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];
/// The ICNS icon types, in the order the container writes them.
const ICNS_TYPES: [&str; 6] = ["icp4", "icp5", "ic07", "ic08", "ic09", "ic10"];

/// Run the gate.
pub fn app_icon() -> Result<(), Box<dyn Error>> {
    let root = crate::workspace_root()?;
    let mut problems = check(&root)?;
    match artwork_still_matches(&root) {
        Ok(true) => {
            println!("xtask app-icon: master art re-derived from the prototype and identical");
        }
        Ok(false) => problems.push(
            "assets/app-icon/ no longer matches assets/icons/prototype-icon-2.png (the diff above is \
             the script's own words)"
                .to_string(),
        ),
        Err(why) => println!("xtask app-icon: INFO, pixel diff skipped — {why}"),
    }
    if !problems.is_empty() {
        problems.sort();
        return Err(format!(
            "xtask app-icon failed (regenerate with `python3 scripts/make-app-icon.py`):\n  {}",
            problems.join("\n  ")
        )
        .into());
    }
    println!(
        "xtask app-icon: {} artefact(s) — master {MASTER_SIDE}, runtime {RUNTIME_SIDE}, {} ICO entr(ies), \
         {} ICNS type(s), hicolor {} — all valid and at the size their name claims",
        artefact_count(),
        ICO_SIDES.len(),
        ICNS_TYPES.len(),
        LINUX_SIDES.len(),
    );
    Ok(())
}

/// The structural rungs, over a workspace root. Returns the problems, empty when the set is good.
pub fn check(root: &Path) -> Result<Vec<String>, Box<dyn Error>> {
    let dir = root.join("assets/app-icon");
    let mut problems = Vec::new();

    problems.extend(png_problem(&dir.join("zenman-master.png"), MASTER_SIDE));
    problems.extend(png_problem(&dir.join("zenman.png"), RUNTIME_SIDE));
    for side in LINUX_SIDES {
        let name = format!("{}x{}.png", side, side);
        problems.extend(png_problem(&dir.join("linux").join(&name), side));
    }
    problems.extend(ico_problems(&dir.join("zenman.ico")));
    problems.extend(icns_problems(&dir.join("zenman.icns")));

    // The embed site, because a renamed file is a compile error only for the one path `include_bytes!`
    // spells — the rest of the ladder would silently stop shipping.
    let source = root.join("apps/zenman/src/app_icon.rs");
    let text = std::fs::read_to_string(&source)?;
    for want in embedded_paths() {
        if !text.contains(&want) {
            problems.push(format!(
                "apps/zenman/src/app_icon.rs: no `include_bytes!` of `{want}` — the file ships but the \
                 binary does not embed it"
            ));
        }
    }
    Ok(problems)
}

/// The paths `apps/zenman/src/app_icon.rs` is expected to embed, as it spells them.
fn embedded_paths() -> Vec<String> {
    let mut out = vec!["assets/app-icon/zenman.png".to_string()];
    out.extend(
        LINUX_SIDES
            .iter()
            .map(|side| format!("assets/app-icon/linux/{side}x{side}.png")),
    );
    out
}

/// A valid RGBA PNG of `side` × `side`.
fn png_problem(path: &Path, side: u32) -> Vec<String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => return vec![format!("{}: {e}", display(path))],
    };
    match png::inspect(&bytes) {
        Ok(info) if info.width != side || info.height != side => vec![format!(
            "{}: {}×{}, expected {side}×{side}",
            display(path),
            info.width,
            info.height
        )],
        // Colour type 6 is RGBA8: the only input `build.rs`, `iconutil` and an icon theme can all use.
        Ok(info) if info.colour_type != 6 => vec![format!(
            "{}: PNG colour type {}, expected 6 (RGBA8)",
            display(path),
            info.colour_type
        )],
        Ok(_) => Vec::new(),
        Err(e) => vec![format!("{}: {e}", display(path))],
    }
}

/// The ICO container: header, then one 16-byte directory entry per image, `0` meaning 256.
fn ico_problems(path: &Path) -> Vec<String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => return vec![format!("{}: {e}", display(path))],
    };
    if bytes.len() < 6 {
        return vec![format!("{}: too short for an ICONDIR", display(path))];
    }
    let mut problems = Vec::new();
    if bytes[0..4] != [0, 0, 1, 0] {
        problems.push(format!(
            "{}: reserved/type are {:?}, expected a type-1 (icon) directory",
            display(path),
            &bytes[0..4]
        ));
        return problems;
    }
    let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    let mut sides = Vec::with_capacity(count);
    for ix in 0..count {
        let at = 6 + ix * 16;
        let Some(entry) = bytes.get(at..at + 16) else {
            problems.push(format!(
                "{}: entry {ix} runs off the end of the file",
                display(path)
            ));
            break;
        };
        let side = if entry[0] == 0 {
            256
        } else {
            u32::from(entry[0])
        };
        if entry[1] != side as u8 && side != 256 {
            problems.push(format!(
                "{}: entry {ix} disagrees about its height",
                display(path)
            ));
        }
        let (len, offset) = (
            u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]) as usize,
            u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]) as usize,
        );
        let Some(image) = bytes.get(offset..offset + len) else {
            problems.push(format!(
                "{}: entry {ix} claims {len} bytes at {offset}, the file ends early",
                display(path)
            ));
            continue;
        };
        let png = image.starts_with(&[0x89, b'P', b'N', b'G']);
        if !png && image.len() < 40 {
            problems.push(format!(
                "{}: entry {ix} is neither PNG nor BMP/DIB",
                display(path)
            ));
        }
        sides.push(side);
    }
    if sides != ICO_SIDES {
        problems.push(format!(
            "{}: entries {sides:?}, expected {ICO_SIDES:?}",
            display(path)
        ));
    }
    problems
}

/// The ICNS container: `icns`, a total length, then one sized icon type per entry.
fn icns_problems(path: &Path) -> Vec<String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => return vec![format!("{}: {e}", display(path))],
    };
    if bytes.len() < 8 || bytes[0..4] != *b"icns" {
        return vec![format!("{}: not an icns container", display(path))];
    }
    let mut problems = Vec::new();
    let total = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    if total != bytes.len() {
        problems.push(format!(
            "{}: header claims {total} bytes, the file is {}",
            display(path),
            bytes.len()
        ));
    }
    let mut types = Vec::new();
    let mut at = 8;
    while at + 8 <= bytes.len() {
        let name = String::from_utf8_lossy(&bytes[at..at + 4]).into_owned();
        let size = u32::from_be_bytes([bytes[at + 4], bytes[at + 5], bytes[at + 6], bytes[at + 7]])
            as usize;
        if size < 8 || at + size > bytes.len() {
            problems.push(format!(
                "{}: icon type {name} claims {size} bytes at {at}, which the file cannot hold",
                display(path)
            ));
            break;
        }
        // Every payload of a modern icns is a PNG; anything else is a legacy JPEG-2000/ARGB blob.
        if bytes[at + 8..at + 12] != [0x89, b'P', b'N', b'G'] {
            problems.push(format!(
                "{}: icon type {name} does not carry a PNG payload",
                display(path)
            ));
        }
        types.push(name);
        at += size;
    }
    if types != ICNS_TYPES {
        problems.push(format!(
            "{}: icon types {types:?}, expected {:?}",
            display(path),
            ICNS_TYPES
        ));
    }
    problems
}

/// Re-derive the master from the prototype. `Err` is "this machine cannot run the check", `Ok(false)`
/// is "the check ran and said the artefacts are stale" (already printed by the script).
fn artwork_still_matches(root: &Path) -> Result<bool, String> {
    let script = root.join("scripts/make-app-icon.py");
    if !script.is_file() {
        return Err(format!("{} is missing", display(&script)));
    }
    let mut cmd = Command::new("python3");
    cmd.arg(&script).arg("--check");
    let output = capture(&mut cmd).map_err(|e| format!("cannot run python3: {e}"))?;
    let text = String::from_utf8_lossy(&output.stderr).into_owned();
    if output.status.success() {
        return Ok(true);
    }
    if looks_like_a_missing_module(&text) {
        return Err(format!(
            "python3 has no Pillow/numpy ({})",
            first_line(&text).unwrap_or_else(|| "import failed".to_string())
        ));
    }
    eprintln!(
        "{}",
        text.trim()
            .lines()
            .map(|line| format!("xtask app-icon: {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    Ok(false)
}

fn looks_like_a_missing_module(text: &str) -> bool {
    text.contains("ModuleNotFoundError") || text.contains("No module named")
}

fn first_line(text: &str) -> Option<String> {
    text.trim().lines().next().map(str::to_string)
}

fn artefact_count() -> usize {
    2 + LINUX_SIDES.len() + 2
}

/// Repo-relative, because a gate message that starts with the developer's home directory is noise.
fn display(path: &Path) -> String {
    crate::workspace_root()
        .ok()
        .and_then(|root| {
            path.strip_prefix(&root)
                .ok()
                .map(|p| p.display().to_string())
        })
        .unwrap_or_else(|| path.display().to_string())
}

/// Run and capture, the way this workspace is allowed to (`Command::output` is banned by
/// `clippy.toml`; the same helper `cross_target` keeps for itself).
fn capture(cmd: &mut Command) -> std::io::Result<std::process::Output> {
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    cmd.spawn()?.wait_with_output()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_committed_icon_set_is_the_shipped_one() {
        // The root from the source tree, not from `current_exe`: a unit test lives in
        // `target/debug/build/…`, which is not two levels under the workspace.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask is a workspace member")
            .to_path_buf();
        let problems = check(&root).expect("the gate itself is readable");
        assert!(
            problems.is_empty(),
            "committed icon set is wrong: {problems:#?}"
        );
    }

    #[test]
    fn a_truncated_icns_is_caught() {
        // A guarded scratch tree (`zenman-xtask-app-icon-<pid>-<seq>`): the guard's `Drop` does what
        // the hand-rolled file/dir removals used to, via the shared `remove_tree`.
        let dir = crate::scratch::Scratch::new("xtask-app-icon");
        let path = dir.join("zenman.icns");
        // Right magic, wrong total length, no icon entries.
        std::fs::write(&path, [b'i', b'c', b'n', b's', 0, 0, 0, 40]).expect("fixture");
        let problems = icns_problems(&path);
        assert!(
            problems.iter().any(|p| p.contains("icon types")),
            "{problems:?}"
        );
    }

    #[test]
    fn an_ico_missing_its_largest_entry_is_caught() {
        // A 16-only .ico: the header says one entry, so `sides` cannot equal `ICO_SIDES`.
        let mut bytes = vec![0u8, 0, 1, 0, 1, 0];
        bytes.extend_from_slice(&[16, 16, 0, 0, 1, 0, 32, 0, 40, 0, 0, 0, 22, 0, 0, 0]);
        bytes.extend_from_slice(&[0u8; 40]);
        let dir = crate::scratch::Scratch::new("xtask-app-icon-ico");
        let path = dir.join("zenman.ico");
        std::fs::write(&path, &bytes).expect("fixture");
        let problems = ico_problems(&path);
        assert!(
            problems.iter().any(|p| p.contains("entries")),
            "{problems:?}"
        );
    }
}
