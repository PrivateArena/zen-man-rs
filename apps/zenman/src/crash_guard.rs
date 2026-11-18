//! The never-crash boundary around the window (plan §5.9 `crash_guard.rs` row; the framework
//! precedent is photocraft's `apps/photocraft/src/crash_guard.rs`, adapted here to write a report
//! file because zen-man has no job list to dump yet — that arrives with `jobs_ui` in plan §5.7).
//!
//! Two halves, neither of which aborts the process:
//!
//! * [`install_hook`] replaces the panic hook so a panic that escapes anyway is written to
//!   `data_dir/crashes/panic-<unix>-<pid>.txt` before it is printed. `panic = "unwind"` stays on in
//!   every profile (root `Cargo.toml`) so unwinding reaches [`guard`].
//! * [`guard`] turns a panic inside `f` into an `Err` naming `f`, with the report path in the text,
//!   so `main` reports it and picks an exit code instead of dying mid-GPU-init.
//!
//! Code in this workspace is still expected to *return* errors (`unsafe_code` is forbidden, and
//! `clippy::unwrap_used`/`expect_used`/`panic`/`todo`/`unimplemented` are denied at the crate root);
//! this is the safety net for the panic that escapes those rules, not a licence to panic.

use std::io::Write as _;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Where reports go: `data_dir/crashes`. Set once by [`install_hook`].
static REPORT_DIR: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

/// Install the panic hook and remember where to write. `dir` is created lazily, on the first panic,
/// so a successful run never creates `data_dir/crashes/`.
pub fn install_hook(dir: &Path) {
    let slot = REPORT_DIR.get_or_init(|| Mutex::new(None));
    match slot.lock() {
        Ok(mut guard) => *guard = Some(dir.to_path_buf()),
        // A poisoned lock means an earlier hook panicked: keep the previous directory, which is
        // still the right one, rather than losing the report target entirely.
        Err(poisoned) => *poisoned.into_inner() = Some(dir.to_path_buf()),
    }
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let report = write_report(&location(info), &info.to_string());
        match report {
            Some(path) => log::error!(
                "zen-man internal error (report written to {}): {info}",
                path.display()
            ),
            None => log::error!("zen-man internal error: {info}"),
        }
        default(info);
    }));
}

/// The report path for the current hook target, if any. Used by [`guard`]'s message and by
/// `docs/crash-reports.md`'s instructions to the user.
pub fn report_dir() -> Option<PathBuf> {
    REPORT_DIR
        .get()
        .and_then(|slot| slot.lock().ok())
        .and_then(|guard| guard.clone())
}

/// Run `f`; a panic inside it becomes `Err(report)` naming `what`, and an `Err` from `f` passes
/// through untouched. The nested `Result` keeps "returned an error" and "panicked" distinguishable,
/// which matters because only the second one means the process state is unknown.
pub fn guard<T, E>(what: &str, f: impl FnOnce() -> Result<T, E>) -> Result<Result<T, E>, String> {
    match std::panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(value) => Ok(value),
        Err(_) => {
            let detail = match report_dir() {
                Some(dir) => format!(
                    "a report should have been written under {}; if it is empty, the panic payload itself was the problem",
                    dir.display()
                ),
                None => "no report directory was installed".to_string(),
            };
            Err(format!(
                "{what} failed with an internal error ({detail}); nothing you had saved was changed"
            ))
        }
    }
}

/// `file:line` of the panic, when the payload carries it.
fn location(info: &std::panic::PanicHookInfo<'_>) -> String {
    info.location()
        .map(|l| format!("{}:{}", l.file(), l.line()))
        .unwrap_or_else(|| "<unknown location>".to_string())
}

/// Write one report and return its path, or `None` when there is no target directory or the write
/// failed. A crash reporter that itself panics would turn a reportable error into an abort, so every
/// failure here is swallowed after being logged.
fn write_report(location: &str, message: &str) -> Option<PathBuf> {
    let dir = report_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let path = dir.join(format!("panic-{seconds}-{}.txt", std::process::id()));
    let body = format!(
        "zen-man crash report\n\
         version: zenman {}\n\
         panic: {message}\n\
         location: {location}\n\
         pid: {}\n\n\
         The process did not abort: the panic was caught by crash_guard::guard and reported.\n\
         Attach this file (and data_dir/gpu/starting.toml if it exists) to a bug report.\n",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    );
    let mut file = std::fs::File::create(&path).ok()?;
    file.write_all(body.as_bytes()).ok()?;
    file.flush().ok()?;
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_turns_a_panic_into_an_error_without_aborting() {
        let caught: Result<Result<u8, &str>, String> = guard("Smoke", || panic!("boom"));
        let Err(report) = caught else {
            panic!("the panic must surface as Err")
        };
        assert!(
            report.contains("Smoke failed with an internal error"),
            "{report}"
        );
        assert_eq!(guard("Smoke", || Ok::<u8, &str>(3)), Ok(Ok(3)));
        let returned: Result<Result<u8, &str>, String> = guard("Smoke", || Err("bad path"));
        assert_eq!(
            returned.map(|r| r.map_err(|e: &str| e.to_string())),
            Ok(Err("bad path".to_string()))
        );
    }

    #[test]
    fn a_panic_writes_a_report_when_a_directory_is_installed() {
        let dir = std::env::temp_dir().join(format!("zenman-crash-{}", std::process::id()));
        let before = std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
        install_hook(&dir);
        let report: Result<Result<(), &str>, String> =
            guard("Report", || panic!("deliberate panic from a test"));
        assert!(report.is_err(), "the panic must surface as the outer Err");
        let after = std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
        assert!(
            after > before,
            "a report file should have appeared in {}",
            dir.display()
        );
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
