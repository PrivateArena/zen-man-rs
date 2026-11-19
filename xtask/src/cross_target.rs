//! Plan §7 row 16's cross-target half: `cargo xtask ci` proves the **Windows story** as far as this box
//! can prove it, and says out loud how far that is.
//!
//! Three rungs, weakest last:
//!
//! 1. **Target installed?** Probed with `rustup target list --installed`. Absent (a checkout on a machine
//!    that never fetched the target, CI images included) prints the reason and skips — the alternative, a
//!    gate that silently never runs, is worse than no gate.
//! 2. **The platform layer's own dependencies** are checked for `x86_64-pc-windows-msvc` (`trash`, `open`,
//!    `zip`, `sevenz-rust`, `rustix`). This is a *hard* gate: it is the rung that catches the realistic
//!    regression, someone adding a unix-only crate to `zenman-core` and only noticing it at release time.
//!    Step 16a measured all five as clean for this target.
//! 3. **The whole crate** (`cargo check -p zenman-core --target … --all-targets`). This one is *blocked
//!    upstream on a Linux host* and is reported as such rather than faked: `rusqlite`'s `bundled` feature
//!    compiles the SQLite amalgamation for the target, and `cc` needs the MSVC toolchain (`cl.exe`,
//!    `lib.exe` — or a mingw SDK, also absent here). `rustc` never reaches `zenman-core`, so this rung
//!    prints a documented skip while the build script is the only thing that failed; the moment the
//!    failure list says anything else, or the C toolchain appears, it becomes a hard gate with no change
//!    to this file. The rustc-level coverage of *our* code that this cannot give is partly recovered by
//!    `platform::windows`' own test module, which compiles and runs on the host (the module is
//!    target-neutral `std`, by design — see its doc).

/// The target this gate exists for.
pub const WINDOWS_TARGET: &str = "x86_64-pc-windows-msvc";

/// The crates `zenman-core`'s platform layer stands on, all of which must build for Windows.
/// `rustix` is deliberately **not** here: step 16a moved it to `[target.'cfg(unix)'.dependencies]`, so it
/// is no longer part of the Windows graph at all (and `-p rustix` is ambiguous besides — two majors are
/// in the lockfile). If it ever reappears as a plain dependency, this rung is what catches that.
const PLATFORM_DEPS: [&str; 4] = ["trash", "open", "zip", "sevenz-rust"];

/// The build-script failure that a Linux host cannot get past, and the reason it is not our bug.
const BUNDLED_SQLITE_BLOCKER: &str = "failed to run custom build command for `libsqlite3-sys";

/// The `xtask ci` rung. `Ok(())` also covers the documented skips, each of which prints why.
pub fn check() -> Result<(), Box<dyn std::error::Error>> {
    let Some(targets) = installed_targets()? else {
        println!(
            "xtask ci: cross-target check SKIPPED — `rustup target list --installed` is unavailable, so              {WINDOWS_TARGET} cannot be probed on this machine"
        );
        return Ok(());
    };
    if !targets.lines().any(|l| l.trim() == WINDOWS_TARGET) {
        println!(
            "xtask ci: cross-target check SKIPPED — `{WINDOWS_TARGET}` is not installed \
             (`rustup target add {WINDOWS_TARGET}` enables the whole rung)"
        );
        return Ok(());
    }

    // Rung 2: the hard one.
    let mut cmd = std::process::Command::new("cargo");
    cmd.arg("check");
    for package in PLATFORM_DEPS {
        cmd.arg("-p").arg(package);
    }
    cmd.arg("--target").arg(WINDOWS_TARGET);
    println!(
        "$ cargo check -p {} --target {WINDOWS_TARGET}",
        PLATFORM_DEPS.join(" -p ")
    );
    let status = cmd.status()?;
    if !status.success() {
        return Err(format!(
            "xtask ci: a platform-layer dependency does not build for {WINDOWS_TARGET} (status \
             {status}) — `zenman-core` cannot be first-class on Windows with a unix-only crate in its \
             graph (plan §10 item 4)"
        )
        .into());
    }
    println!(
        "xtask ci: cross-target — the platform-layer dependencies ({PLATFORM_DEPS:?}) all check clean \
         for {WINDOWS_TARGET}"
    );

    // Rung 3: the whole crate, or the honest reason it cannot be reached from here.
    // `Command::output` is banned workspace-wide (`clippy.toml`), so the capture is spelled out.
    let output = capture(
        std::process::Command::new("cargo")
            .args(["check", "-p", "zenman-core", "--all-targets", "--target"])
            .arg(WINDOWS_TARGET),
    )?;
    if output.status.success() {
        println!(
            "xtask ci: cross-target — `cargo check -p zenman-core --target {WINDOWS_TARGET} --all-targets` is clean"
        );
        return Ok(());
    }
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let ours = text.contains("error[E") || text.contains("could not compile `zenman-core");
    if !text.contains(BUNDLED_SQLITE_BLOCKER) || ours {
        return Err(format!(
            "xtask ci: cross-target check failed for {WINDOWS_TARGET} (the failure is not the known \
             bundled-SQLite toolchain block, so it is ours):\n{}",
            tail_of(&text)
        )
        .into());
    }
    println!(
        "xtask ci: cross-target PARTIAL — `zenman-core` itself is never reached for {WINDOWS_TARGET}: \
         `rusqlite`'s `bundled` SQLite needs the MSVC C toolchain (`cl.exe`/`lib.exe`), which a Linux \
         host does not have. Run this on Windows (or install an SDK + a `CC_{WINDOWS_TARGET}` wrapper) \
         to promote the rung; `platform::windows`' tests run on the host in the meantime."
    );
    Ok(())
}

/// The installed targets, or `None` when `rustup` is not on PATH (a distro toolchain, a vendored rust).
fn installed_targets() -> Result<Option<String>, Box<dyn std::error::Error>> {
    match capture(std::process::Command::new("rustup").args(["target", "list", "--installed"])) {
        Ok(output) if output.status.success() => {
            Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
        }
        // No `rustup` (a distro toolchain) or a failed probe: the rung cannot decide, so it says so.
        _ => Ok(None),
    }
}

/// Run and capture, the way this workspace is allowed to (`Command::output` is banned by `clippy.toml`).
fn capture(cmd: &mut std::process::Command) -> std::io::Result<std::process::Output> {
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    cmd.spawn()?.wait_with_output()
}

/// The last few lines of a failing build, for the error message.
fn tail_of(text: &str) -> String {
    text.lines().rev().take(12).collect::<Vec<_>>().join(
        "
",
    )
}
