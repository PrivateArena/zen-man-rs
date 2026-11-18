//! `open`: hand a path to the desktop, or say that the pane should navigate instead (plan §5.3
//! `ops/open.rs`, Go `handleOpen` `api/ops.go:122-143` + `openWithDefaultApp` `:523-541`).
//!
//! Go's handler took a list, ran `xdg-open` on every element, remembered only the **last** error
//! (`lastErr`, `api/ops.go:130-133`) and reported the whole batch as failed when any one of them did. The
//! port keeps the batch and drops the collapse: each path gets its own outcome in
//! [`OpenReport`] and the call only fails when it could not even start.
//!
//! # Directories are navigation, not `open`
//!
//! `openWithDefaultApp` was reached for directories too, so double-clicking a folder asked the *desktop* to
//! open it — which works on a Linux desktop with a file manager registered and is a silent no-op
//! everywhere else. The port classifies first: [`OpenRoute::Navigate`] means "the pane should show this
//! directory", and the UI has to act on that rather than wait for a launcher that will never run.
//! [`OpenReport::navigated`] is the list it navigates to.
//!
//! # The missing-opener case is a user-facing error
//!
//! `open` 5.4 answers `io::Result` and has no `not_found()` to ask, so the presence of an opener is
//! probed with `open::commands()` — see [`crate::platform::linux::open_with`], which refuses with a
//! [`CoreError::Shell`] that names the programs the user can install. A [`CoreError::Unsupported`] from a
//! platform with no implementation is passed through unchanged: it is already typed and already says what
//! is missing. Neither is a panic, and neither is swallowed.
//!
//! The spawn is **detached** (plan §9.1's measured fact about `xclip`: a waiting spawn parks a job until the
//! external application exits, and `xdg-open` does not exit until its application does).

use std::sync::Arc;

use crate::error::CoreError;
use crate::jobs::CancelToken;
use crate::path::AbsPath;
use crate::platform::Env;

/// Where one path is meant to go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenRoute {
    /// A file (or anything that is not a directory): the desktop's default handler.
    Launch(AbsPath),
    /// A directory: the pane changes to it. Go had no such distinction — see the module docs.
    Navigate(AbsPath),
}

impl OpenRoute {
    /// The path this route is about.
    #[must_use]
    pub fn path(&self) -> &AbsPath {
        match self {
            Self::Launch(p) | Self::Navigate(p) => p,
        }
    }
}

/// Classify one path the way the file manager's own activation does: `stat` (following symlinks, so a
/// symlinked directory navigates — the plan §6.2 "symlinked directories report `is_dir: true`" deviation),
/// and a directory is [`OpenRoute::Navigate`].
///
/// A path that cannot be statted is offered to the desktop anyway: the launcher's own error is more useful
/// than a pre-flight guess, and a broken symlink is exactly the case where the user wants a real message.
#[must_use]
pub fn route(path: &AbsPath) -> OpenRoute {
    if std::fs::metadata(path.as_path()).is_ok_and(|m| m.is_dir()) {
        OpenRoute::Navigate(path.clone())
    } else {
        OpenRoute::Launch(path.clone())
    }
}

/// What a batch `open` did.
#[derive(Clone, Debug, Default)]
pub struct OpenReport {
    /// Paths handed to the desktop launcher.
    pub launched: Vec<AbsPath>,
    /// Directories the UI should show instead — the routing decision, reported rather than acted on
    /// silently, because only the caller knows which pane asked.
    pub navigated: Vec<AbsPath>,
    /// Paths that could not be opened, each with its own error (Go's single `lastErr`, made per-path).
    pub failed: Vec<(AbsPath, Arc<CoreError>)>,
}

impl OpenReport {
    /// `true` when nothing worked at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.launched.is_empty() && self.navigated.is_empty()
    }

    /// The jobs/status summary line.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{} opened, {} to show, {} failed",
            self.launched.len(),
            self.navigated.len(),
            self.failed.len()
        )
    }
}

impl std::cmp::PartialEq for OpenReport {
    fn eq(&self, other: &Self) -> bool {
        self.launched == other.launched
            && self.navigated == other.navigated
            && self.failed.len() == other.failed.len()
            && self
                .failed
                .iter()
                .zip(other.failed.iter())
                .all(|(a, b)| a.0 == b.0 && a.1.kind() == b.1.kind())
    }
}

/// Open every path, routing directories as navigation (Go's `handleOpen`, with the batch's failures kept
/// separate).
///
/// # Errors
/// [`CoreError::Empty`] for an empty selection and [`CoreError::Cancelled`]. A path that will not open is
/// in [`OpenReport::failed`], never in this `Err`.
pub fn open_paths(
    env: &dyn Env,
    paths: &[AbsPath],
    cancel: &CancelToken,
) -> Result<OpenReport, CoreError> {
    if paths.is_empty() {
        return Err(CoreError::Empty { what: "selection" });
    }
    let mut report = OpenReport::default();
    for path in paths {
        cancel.check()?;
        match route(path) {
            OpenRoute::Navigate(dir) => report.navigated.push(dir),
            OpenRoute::Launch(file) => match crate::jobs::block_on(env.open_with(&file)) {
                Ok(Ok(())) => report.launched.push(file),
                Ok(Err(e)) => report.failed.push((file, Arc::new(e))),
                Err(e) => report.failed.push((file, Arc::new(e))),
            },
        }
    }
    Ok(report)
}

/// Open one path with the desktop's default handler — plan §5.3's `open_with_default`. A directory is
/// routed, not launched: use [`open_paths`] (or check [`route`]) before calling this on a selection that
/// may hold one.
///
/// # Errors
/// Whatever the platform's opener reports, typed (module docs).
pub fn open_with_default(env: &dyn Env, path: &AbsPath) -> Result<(), CoreError> {
    crate::jobs::block_on(env.open_with(path))?
}

/// Show `path` in its parent directory (plan §5.3's `reveal`, a capability Go never had — there was no
/// endpoint, so this row has no Go reference line).
///
/// The freedesktop way is the `org.freedesktop.FileManager1.ShowItems` D-Bus call, which highlights the
/// item; that needs a D-Bus client, and plan §5.1's dependency rule keeps the crate free of one until a
/// step needs it. So this is the documented fallback the plan's own row allows: open the parent directory
/// through the platform opener. Step 16 can upgrade it in place — the signature is the one the UI calls.
///
/// # Errors
/// [`CoreError::NotFound`] for a path with no parent (the filesystem root), then whatever the opener
/// reports for that directory.
pub fn reveal(env: &dyn Env, path: &AbsPath) -> Result<(), CoreError> {
    let parent = path.parent().ok_or_else(|| CoreError::NotFound {
        path: path.to_string_lossy(),
        source_path: Some("the parent directory to reveal it in".to_string()),
    })?;
    open_with_default(env, &parent)
}
