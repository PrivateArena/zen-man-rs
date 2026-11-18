//! The session clipboard: what is queued, how it is spelled on the wire, and how it reaches the system
//! clipboard (plan §5.3 `ops/clipboard.rs`, porting `api/ops.go:144-235,549-635`).
//!
//! # What Go did, and what is kept
//!
//! * `handleClipboardSet` (`api/ops.go:144`) stored `{op, sources}`, pushed the sources to the system
//!   clipboard as a `text/uri-list`, appended one `ActionCopy` log record, and answered with one item per
//!   source (`name`/`path`/`isDir`/`size`, `size: 0` when the stat failed). The `0` is the one part we do
//!   **not** copy: plan §2.4's "a size you cannot read is not a size of zero" makes it
//!   [`ClipItem::size`] `None`, which is also what the degraded listing row already means in this crate.
//! * `copy_inside`/`cut_inside` (`api/ops.go:189`, `resolveInsideSources`) expand each source directory to
//!   its children before anything is stored — see [`expand_inside`].
//! * `handleClearClipboard` (`api/ops.go:216`) publishes an **empty** uri-list rather than leaving the old
//!   one behind, so a paste into another application after a clear cannot resurrect the old selection.
//!   [`publish_system`] takes `Option<&Clipboard>` for exactly that reason.
//! * `readFromSystemClipboard` (`api/ops.go:577`) is the fallback source when a paste request carries no
//!   `sources` of its own: `wl-paste` → `xclip`, URI list first, plain text second.
//!
//! # Why the system sync goes through [`Env`] and not a crate
//!
//! Plan §5.3 sketched `arboard`; the decision for step 9a is **no new dependency** (the Go app's behaviour
//! is Wayland/X11 `text/uri-list`, which `arboard` does not even claim to publish). So the three things
//! the shell-out needed are on the platform seam instead: [`ExecRequest::stdin`] to feed `wl-copy`,
//! [`Env::has_program`] for the `exec.LookPath` probes, and [`Env::exec_program`] to run it. That also keeps
//! this module free of process spawning, which is what makes every assertion below about *command lines*
//! rather than about a desktop session.
//!
//! [`ExecRequest::stdin`]: crate::platform::ExecRequest

use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::jobs;
use crate::path::AbsPath;
use crate::platform::{Env, ExecRequest};
use crate::services::dirs::FsSource;

/// Which way the selection is going. Go's `ClipboardState.Op` string (`api/ops.go:33`), spelled the same
/// (`"copy"`/`"cut"`) because the log and the frontend both carry it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClipOp {
    /// `copy` — the source survives the paste.
    #[default]
    Copy,
    /// `cut` — the source goes away with it (`api/ops.go:312` `moveItem`).
    Cut,
}

impl ClipOp {
    /// The wire/log spelling, identical to Go's `Op` string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Cut => "cut",
        }
    }
}

impl std::fmt::Display for ClipOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One queued entry. The `display`/`size` pair is exactly the item body Go returned to the frontend
/// (`api/ops.go:166-176`), kept as its own type so the UI does not re-`stat` a selection it just made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipItem {
    /// The absolute path the paste will read.
    pub path: AbsPath,
    /// `filepath.Base(source)` — the name the chip shows.
    pub display: String,
    /// `info.Size()`, or `None` when the stat failed. Go reported that case as `0`
    /// (`api/ops.go:163-171`); plan §2.4 makes the difference visible.
    pub size: Option<u64>,
}

/// The session's clipboard — session-owned, never global (plan §5.3's `Clipboard` row: two panes in two
/// instances must not share a queue).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Clipboard {
    /// Copy or cut.
    pub op: ClipOp,
    /// The queued items, in the order they were selected (the paste order, and what the system
    /// clipboard carries).
    pub items: Vec<ClipItem>,
    /// The parent of the first source — the directory the pane came from, which is what a paste back
    /// into it must refuse (`ops::guard`).
    pub origin: Option<AbsPath>,
}

impl Clipboard {
    /// The queued paths, in order — what `writeToSystemClipboard` and the paste loop consume.
    #[must_use]
    pub fn paths(&self) -> Vec<AbsPath> {
        self.items.iter().map(|i| i.path.clone()).collect()
    }

    /// `true` when nothing is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Whether the request names the sources, or names directories whose *contents* are the sources.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClipScope {
    /// `copy`/`cut`: the selection itself is queued.
    #[default]
    Selected,
    /// `copy_inside`/`cut_inside` (`api/ops.go:120-137`): each source directory's children are queued.
    Inside,
}

/// A clipboard request, before it becomes a [`Clipboard`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardStore {
    /// Copy or cut.
    pub op: ClipOp,
    /// What the user selected (or, with [`ClipScope::Inside`], the directories to open up).
    pub sources: Vec<AbsPath>,
    /// Which of the two Go handlers this request answers.
    pub scope: ClipScope,
}

/// What the system clipboard handed us: paths, and whether they came with a type.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SystemClipboard {
    /// The paths, in clipboard order.
    pub paths: Vec<AbsPath>,
    /// `true` when they came from a `text/uri-list` — Go's second return value
    /// (`api/ops.go:577`), which decides whether the paste treats the payload as typed content.
    pub is_uri_list: bool,
}

/// The tool this machine uses to reach the system clipboard.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SystemClipTool {
    /// `wl-copy`/`wl-paste` — preferred, and the branch a Wayland session takes (`api/ops.go:563,579`).
    Wayland,
    /// `xclip -selection clipboard` — the X11 fallback (`api/ops.go:569,592`).
    Xclip,
    /// Neither. Publishing is a silent no-op exactly as in Go; reading is an error.
    #[default]
    None,
}

/// The program names Go probed (`api/ops.go:563,569,579,592`). They are `str`, not resolved paths: the
/// `PATH` scan is the platform's job ([`Env::has_program`]), not this module's.
const WL_COPY: &str = "wl-copy";
const WL_PASTE: &str = "wl-paste";
const XCLIP: &str = "xclip";
/// The one MIME type the Go app both wrote and read (`api/ops.go:564,580`).
const URI_LIST_MIME: &str = "text/uri-list";

// --------------------------------------------------------------------------- store

/// Queue a selection, exactly as `handleClipboardSet`/`resolveInsideSources` did (`api/ops.go:144,189`).
///
/// An empty selection is [`CoreError::Empty`] — Go's `"No files selected"` (`api/ops.go:146`) — and with
/// [`ClipScope::Inside`], an empty directory is refused too (see [`expand_inside`]). The returned
/// [`Clipboard`] is the value the session stores; appending the log record, pushing the event and calling
/// [`publish_system`] are the session's business (plan §5.3 keeps I/O ordering out of `ops`).
pub fn store(source: &dyn FsSource, req: &ClipboardStore) -> Result<Clipboard, CoreError> {
    if req.sources.is_empty() {
        return Err(CoreError::Empty { what: "selection" });
    }
    let paths = match req.scope {
        ClipScope::Selected => req.sources.clone(),
        ClipScope::Inside => {
            let mut expanded = Vec::new();
            for dir in &req.sources {
                expanded.extend(expand_inside(source, dir)?);
            }
            expanded
        }
    };
    if paths.is_empty() {
        // Unreachable through `expand_inside` (it refuses an empty directory), but the invariant is the
        // caller's, not the expander's: an empty queue is never a success.
        return Err(CoreError::Empty { what: "selection" });
    }
    let origin = paths.first().and_then(AbsPath::parent);
    let items = paths
        .into_iter()
        .map(|path| {
            let display = path.file_name().map_or_else(
                || path.to_string_lossy(),
                |n| n.to_string_lossy().into_owned(),
            );
            // Go statted with `os.Stat` and used `0` on failure (`api/ops.go:163-166`); the failure is
            // kept as `None` here, and the stat is `lstat` because that is the seam this crate has.
            let size = source
                .lstat(path.as_path())
                .ok()
                .and_then(|info| u64::try_from(info.size).ok());
            ClipItem {
                path,
                display,
                size,
            }
        })
        .collect();
    Ok(Clipboard {
        op: req.op,
        items,
        origin,
    })
}

/// Open up one directory into its children — `resolveInsideSources` (`api/ops.go:189-211`).
///
/// The three Go answers, in Go's order: an unreadable source fails with its error, a non-directory is
/// [`CoreError::NotADir`] (`"source is not a directory"`), and an empty one is [`CoreError::Empty`]
/// (`"folder is empty, nothing to copy/cut"`). Two deliberate differences from Go:
///
/// * Go only reported the empty case when the *whole* expansion came out empty (`api/ops.go:207` checks the
///   total), so a two-directory `copy_inside` with one empty folder silently pasted the other; here the
///   empty directory is named. Plan §5.3's `clipboard.rs` row asks for exactly that ("empty dir →
///   `CoreError::Empty` instead of silently clearing").
/// * Go's `os.Stat` followed symlinks; [`FsSource::lstat`] does not, so a symlink to a directory is refused
///   here where Go would have expanded its target. Following would need a second, resolving stat the
///   listing seam does not offer.
///
/// Children come back sorted by name, because Go's `os.ReadDir` is lexical (`api/ops.go:198`) and the
/// paste order — and therefore the undo record — follows it.
pub fn expand_inside(source: &dyn FsSource, dir: &AbsPath) -> Result<Vec<AbsPath>, CoreError> {
    let info = source.lstat(dir.as_path())?;
    if !info.kind.is_navigable_dir() {
        return Err(CoreError::NotADir {
            path: dir.to_string_lossy(),
        });
    }
    let mut entries = source.read_dir(dir)?;
    if entries.is_empty() {
        return Err(CoreError::Empty { what: "folder" });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries.iter().map(|e| dir.join(&e.name)).collect()
}

// --------------------------------------------------------------------------- uri-list wire form

/// The `text/uri-list` body Go wrote (`api/ops.go:549-561`): one `file:` URI per path, joined by `\n`,
/// **no trailing newline** (`strings.Join`, not a per-line write — `wl-copy` is told where the list ends by
/// stdin closing).
///
/// An empty selection yields the empty string, which is what a clipboard *clear* publishes
/// (`api/ops.go:216`).
#[must_use]
pub fn uri_list(paths: &[AbsPath]) -> String {
    paths
        .iter()
        .map(|p| format!("file://{}", percent_encode_path(&p.to_string_lossy())))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Read a `text/uri-list` body — `parseURIList` (`api/ops.go:611-629`).
///
/// Lines are trimmed; empty ones and `#` comments are skipped; anything that is not a `file:` URI is
/// dropped (Go's `if strings.HasPrefix(line, "file://")`); the remainder is percent-decoded, keeping the
/// raw text when the escape sequence is malformed (Go's `if decoded, err := …; err == nil`); and the path
/// is cleaned — which [`AbsPath`] does by construction. A `file:` URI whose tail is not absolute is
/// dropped rather than resolved against a cwd this module does not have (Go would have kept the relative
/// string; nothing downstream of it could have used it).
#[must_use]
pub fn parse_uri_list(text: &str) -> Vec<AbsPath> {
    let mut paths = Vec::new();
    for line in text.split('\n') {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix("file://") else {
            continue;
        };
        let decoded = match percent_decode(rest) {
            Some(decoded) => decoded,
            None => rest.to_string(),
        };
        if let Ok(path) = AbsPath::from_path(decoded) {
            paths.push(path);
        }
    }
    paths
}

/// Read a plain-text clipboard body — `parsePlainPaths` (`api/ops.go:630-646`): one path per line, each of
/// which must be absolute **and** exist (Go's `filepath.IsAbs` + `os.Stat` pair), so a stale line naming
/// a deleted file never becomes a paste source.
#[must_use]
pub fn parse_plain_paths(text: &str, source: &dyn FsSource) -> Vec<AbsPath> {
    let mut paths = Vec::new();
    for line in text.split('\n') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(path) = AbsPath::from_path(line) else {
            continue;
        };
        if source.lstat(path.as_path()).is_ok() {
            paths.push(path);
        }
    }
    paths
}

/// Percent-encode an absolute path the way `url.URL{Scheme:"file", Path: p}.String()` does
/// (`api/ops.go:551-556`).
///
/// The keep-set is Go's `encodePath` set: unreserved characters (`A-Za-z0-9-._~`) plus the reserved ones a
/// path may carry unescaped (`$&+,/:;=@`) — `?` being the exception Go escapes, and everything else
/// (space, `#`, `[`, `%`, non-ASCII bytes) becoming `%XX` with uppercase hex.
#[must_use]
pub fn percent_encode_path(path: &str) -> String {
    const UNRESERVED: &[u8] = b"-._~";
    // Go's `shouldEscape(c, encodePath)`; `?` is deliberately absent (it *is* escaped).
    const PATH_KEEP: &[u8] = b"$&+,/:;=@";
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || UNRESERVED.contains(&byte) || PATH_KEEP.contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Percent-decode a URI component — `url.PathUnescape` (`api/ops.go:620`).
///
/// `+` is **not** a space: that mapping belongs to `application/x-www-form-urlencoded`, and Go's path
/// unescape does not do it, so a filename containing a literal `+` survives a clipboard round trip.
/// `None` for a malformed escape, which is Go's `err != nil` branch — the caller keeps the raw text.
#[must_use]
pub fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                // Two hex digits are required, and the `%` must not start the final pair of the string.
                if i + 2 >= bytes.len() {
                    return None;
                }
                let h = hex_value(bytes[i + 1])?;
                let l = hex_value(bytes[i + 2])?;
                out.push(h * 16 + l);
                i += 3;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// One hex digit's value, `None` for anything that is not one (either case, as `url.PathUnescape` accepts).
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

// --------------------------------------------------------------------------- system clipboard

/// Which system clipboard this machine has, by the same probes Go ran (`api/ops.go:563,569,578,592`):
/// `exec.LookPath`, never a spawn. A platform whose [`Env`] cannot answer [`Env::has_program`] is answered
/// as "absent" — the conservative reading, and the one that leaves the user's selection in the session
/// clipboard rather than half-publishing it.
#[must_use]
pub fn detect_tool(env: &dyn Env) -> SystemClipTool {
    // Wayland is preferred when either half exists, which is what the two Go handlers do independently of
    // each other: publishing needs `wl-copy`, reading needs `wl-paste`.
    if has(env, WL_COPY) || has(env, WL_PASTE) {
        SystemClipTool::Wayland
    } else if has(env, XCLIP) {
        SystemClipTool::Xclip
    } else {
        SystemClipTool::None
    }
}

/// `true` when the platform says the program is there; an error (including the trait's default
/// `Unsupported`) means we cannot know, which is treated as no.
fn has(env: &dyn Env, program: &str) -> bool {
    matches!(env.has_program(program), Ok(true))
}

/// Publish the selection to the system clipboard — `writeToSystemClipboard` (`api/ops.go:549-575`).
///
/// `None` (or an empty clipboard) publishes an empty uri-list, which is how Go cleared it
/// (`api/ops.go:216`): another application's paste then yields nothing instead of the old selection.
/// With neither `wl-copy` nor `xclip` installed this is a **silent no-op**, exactly as in Go
/// (`api/ops.go:573` has no `else`), so a headless box does not fail every copy.
///
/// The child's exit status is ignored, as Go's `_ = cmd.Run()` ignored it: a clipboard manager that
/// refuses the payload must not turn a successful copy into a failed one. An `Err` here therefore means
/// only "the platform could not even spawn", which the session reports as a degraded sync.
pub fn publish_system(env: &dyn Env, clip: Option<&Clipboard>) -> Result<(), CoreError> {
    let paths = clip.map_or_else(Vec::new, |c| c.paths());
    let body = uri_list(&paths);
    let req = match detect_tool(env) {
        SystemClipTool::Wayland => {
            ExecRequest::argv(WL_COPY, ["-t", URI_LIST_MIME]).with_stdin(body)
        }
        SystemClipTool::Xclip => ExecRequest::argv(
            XCLIP,
            ["-i", "-selection", "clipboard", "-t", URI_LIST_MIME],
        )
        .with_stdin(body),
        SystemClipTool::None => return Ok(()),
    };
    // Output discarded on purpose — see the doc above. Both layers of the `Result` count: the outer is
    // the job runner's, the inner the platform's refusal to spawn.
    jobs::block_on(env.exec_program(req))??;
    Ok(())
}

/// Read the system clipboard — `readFromSystemClipboard` (`api/ops.go:577-610`), the fallback source of a
/// paste that carried none (`api/ops.go:233-248`).
///
/// Order is Go's: `wl-paste -t text/uri-list`, then plain `wl-paste`, then the same pair through `xclip`.
/// "Plain" means the *command* failed (no `text/uri-list` offer on the clipboard, which is what
/// `wl-paste -t` reports), not the payload being empty — an empty plain clipboard therefore answers zero
/// paths, not an error. With no clipboard program at all: Go's `"no clipboard utility found"`.
pub fn read_system(env: &dyn Env, source: &dyn FsSource) -> Result<SystemClipboard, CoreError> {
    if has(env, WL_PASTE) {
        if let Some(body) = capture(env, ExecRequest::argv(WL_PASTE, ["-t", URI_LIST_MIME])) {
            return Ok(SystemClipboard {
                paths: parse_uri_list(&body),
                is_uri_list: true,
            });
        }
        if let Some(body) = capture(env, ExecRequest::argv(WL_PASTE, Vec::<String>::new())) {
            return Ok(SystemClipboard {
                paths: parse_plain_paths(&body, source),
                is_uri_list: false,
            });
        }
    }
    if has(env, XCLIP) {
        if let Some(body) = capture(
            env,
            ExecRequest::argv(
                XCLIP,
                ["-o", "-selection", "clipboard", "-t", URI_LIST_MIME],
            ),
        ) {
            return Ok(SystemClipboard {
                paths: parse_uri_list(&body),
                is_uri_list: true,
            });
        }
        if let Some(body) = capture(
            env,
            ExecRequest::argv(XCLIP, ["-o", "-selection", "clipboard"]),
        ) {
            return Ok(SystemClipboard {
                paths: parse_plain_paths(&body, source),
                is_uri_list: false,
            });
        }
    }
    Err(CoreError::Unsupported {
        what: "no clipboard utility found (install wl-clipboard or xclip)",
    })
}

/// Run one capture command; `None` when it could not run or exited non-zero — Go's `if err != nil`
/// branch, which falls through to the next candidate.
fn capture(env: &dyn Env, req: ExecRequest) -> Option<String> {
    // A spawn failure is not an error *here*: it is this candidate being unavailable, and the last
    // candidate's failure still surfaces as `read_system`'s `Err`. Two `Result` layers to unwrap: the
    // runner's and the platform's, and either failing means "this candidate is out".
    let out = jobs::block_on(env.exec_program(req))
        .ok()
        .and_then(|r| r.ok())?;
    if out.succeeded() {
        Some(out.stdout)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    //! The eight step-9a clipboard tests (plan §5.3): the two Go parse vectors, the uri-list round trip,
    //! the two command-line assertions through a recorder `Env`, and the two `store`/`expand_inside`
    //! behaviours over a real scratch directory.
    //!
    //! [`ClipEnv`] wraps [`Unsupported`] and overrides only the two methods the clipboard adds
    //! ([`Env::has_program`], [`Env::exec_program`]) plus the recording — every other platform call is the
    //! stub's typed refusal, so a clipboard code path that grew a new one would fail loudly here.

    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::mode::ModeBits;
    use crate::platform::{
        CopyItem, CopyReport, DiskSpace, ExecOutput, LinkItem, MountTable, Os, PlatformCaps,
        PlatformFuture, Shortcut, ready, stub::Unsupported,
    };
    use crate::services::dirs::OsDirRead;

    // ------------------------------------------------------------------ the recorder Env

    struct ClipEnv {
        inner: Unsupported,
        /// The fake `PATH`: [`Env::has_program`] answers `true` for exactly these names.
        programs: &'static [&'static str],
        calls: Mutex<Vec<ExecRequest>>,
        /// Canned answers, popped per spawn; empty means "no further candidate succeeds".
        replies: Mutex<VecDeque<ExecOutput>>,
    }

    impl ClipEnv {
        fn new(programs: &'static [&'static str], replies: Vec<ExecOutput>) -> Self {
            Self {
                inner: Unsupported::labelled("clip-test-lane"),
                programs,
                calls: Mutex::new(Vec::new()),
                replies: Mutex::new(VecDeque::from(replies)),
            }
        }

        fn calls(&self) -> Vec<ExecRequest> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        fn out(code: i32, stdout: &str) -> ExecOutput {
            ExecOutput {
                code: Some(code),
                stdout: stdout.to_string(),
                stderr: String::new(),
                truncated: false,
            }
        }
    }

    impl Env for ClipEnv {
        fn os(&self) -> Os {
            self.inner.os()
        }
        fn caps(&self) -> PlatformCaps {
            self.inner.caps()
        }
        fn open_with(&self, path: &AbsPath) -> PlatformFuture<'_, Result<(), CoreError>> {
            self.inner.open_with(path)
        }
        fn copy_paths(
            &self,
            items: &[CopyItem],
        ) -> PlatformFuture<'_, Result<CopyReport, CoreError>> {
            self.inner.copy_paths(items)
        }
        fn paste_link(&self, items: &[LinkItem]) -> PlatformFuture<'_, Result<(), CoreError>> {
            self.inner.paste_link(items)
        }
        fn supports_symlink(&self) -> Result<bool, CoreError> {
            self.inner.supports_symlink()
        }
        fn chmod(
            &self,
            targets: &[AbsPath],
            mode: ModeBits,
        ) -> PlatformFuture<'_, Result<(), CoreError>> {
            self.inner.chmod(targets, mode)
        }
        fn can_trash(&self, path: &AbsPath) -> bool {
            self.inner.can_trash(path)
        }
        fn trash(&self, path: &AbsPath) -> Result<bool, CoreError> {
            self.inner.trash(path)
        }
        fn create_shortcut(&self, shortcut: &Shortcut) -> Result<(), CoreError> {
            self.inner.create_shortcut(shortcut)
        }
        fn disk_space(&self, path: &AbsPath) -> Result<DiskSpace, CoreError> {
            self.inner.disk_space(path)
        }
        fn mount_entries(&self) -> Result<MountTable, CoreError> {
            self.inner.mount_entries()
        }
        fn exec_program(
            &self,
            req: ExecRequest,
        ) -> PlatformFuture<'_, Result<ExecOutput, CoreError>> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(req);
            let reply = self
                .replies
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front();
            ready(match reply {
                Some(out) => Ok(out),
                None => Err(CoreError::Unsupported {
                    what: "clip-test recorder ran out of canned outputs",
                }),
            })
        }
        fn has_program(&self, name: &str) -> Result<bool, CoreError> {
            Ok(self.programs.contains(&name))
        }
    }

    // ------------------------------------------------------------------ the scratch directory

    static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

    /// A unique temp dir, removed on drop — the `linux.rs` probe-test pattern (`OsDirRead` needs a real
    /// filesystem, and the suite runs in parallel, so the name carries a counter *and* the pid).
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let seq = SCRATCH_SEQ.fetch_add(1, Ordering::AcqRel);
            let dir = std::env::temp_dir()
                .join(format!("zenman-clip-{tag}-{}-{seq}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("create the scratch dir");
            Self(dir)
        }

        fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("parents");
            std::fs::write(&path, bytes).expect("write");
            path
        }

        fn empty_dir(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::create_dir_all(&path).expect("mkdir");
            path
        }

        fn path(&self) -> AbsPath {
            AbsPath::from_path(&self.0).expect("temp dir is absolute")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            remove_scratch(&self.0);
        }
    }

    /// The scratch tree is ours, so it is deleted by hand: `std::fs::remove_dir_all` is banned
    /// workspace-wide (clippy.toml, plan §6.1) — the same helper shape `linux.rs` tests use.
    fn remove_scratch(root: &std::path::Path) {
        let Ok(entries) = std::fs::read_dir(root) else {
            let _ = std::fs::remove_dir(root);
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && !path.is_symlink() {
                remove_scratch(&path);
            } else {
                let _ = std::fs::remove_file(&path);
            }
        }
        let _ = std::fs::remove_dir(root);
    }

    fn abs(text: &str) -> AbsPath {
        AbsPath::from_path(text).expect("test path is absolute and clean")
    }

    // ------------------------------------------------------------------ parse/round-trip vectors

    #[test]
    fn parse_uri_list_matches_go_vector() {
        // The frozen Go vector: spaces as `%20`, brackets as `%5B`/`%5D`, nothing else escaped.
        let line = "file:///media/Evolution%20Rock%20Standard%201.3.0%20%5BOrange%20Tree%20Samples%5D/tracks/Bridge_SwoopUp_rr2.flac";
        let paths = parse_uri_list(line);
        assert_eq!(
            paths.iter().map(AbsPath::as_path).collect::<Vec<_>>(),
            [std::path::Path::new(
                "/media/Evolution Rock Standard 1.3.0 [Orange Tree Samples]/tracks/Bridge_SwoopUp_rr2.flac"
            )]
        );
    }

    #[test]
    fn parse_uri_list_ignores_comments_and_non_file_lines() {
        let text = concat!(
            "# comment line\n",
            "\n",
            "   \n",
            "https://example.com/page\n",
            "mailto:someone@example.com\n",
            "file:relative/not/absolute\n",
            "file:///tree/ok.txt\n",
        );
        assert_eq!(parse_uri_list(text), vec![abs("/tree/ok.txt")]);
    }

    #[test]
    fn parse_plain_paths_requires_existing() {
        let scratch = Scratch::new("plain");
        let here = scratch.file("here.txt", b"x");
        let missing = scratch.0.join("gone.txt");
        let text = format!(
            "{}\n{}\nrelative.txt\n\n",
            here.display(),
            missing.display()
        );
        let paths = parse_plain_paths(&text, &OsDirRead);
        assert_eq!(paths, vec![abs(&here.display().to_string())]);
    }

    #[test]
    fn uri_list_round_trips() {
        let originals = [
            "/tree/plain.txt",
            "/tree/Evolution Rock [2024]/Bridge_SwoopUp_rr2.flac",
            "/tree/hash#tag.txt",
            "/tree/plus+and%25.txt",
            "/tree/ünïcode-é.txt",
        ]
        .map(abs);
        let body = uri_list(&originals);
        assert_eq!(body.lines().count(), originals.len());
        assert!(
            !body.ends_with('\n'),
            "Go joined with \\n, no trailing newline"
        );
        assert_eq!(parse_uri_list(&body), originals.to_vec());
    }

    // ------------------------------------------------------------------ system-clipboard commands

    #[test]
    fn publish_system_builds_the_go_command_line() {
        let clip = Clipboard {
            op: ClipOp::Copy,
            items: vec![
                ClipItem {
                    path: abs("/tree/a b.txt"),
                    display: "a b.txt".into(),
                    size: None,
                },
                ClipItem {
                    path: abs("/tree/c"),
                    display: "c".into(),
                    size: Some(7),
                },
            ],
            origin: Some(abs("/tree")),
        };
        let want = uri_list(&clip.paths());

        let wayland = ClipEnv::new(&["wl-copy", "wl-paste"], vec![ClipEnv::out(0, "")]);
        publish_system(&wayland, Some(&clip)).expect("wayland publish");
        let calls = wayland.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "wl-copy");
        assert_eq!(calls[0].args, ["-t", URI_LIST_MIME]);
        assert_eq!(calls[0].stdin.as_deref(), Some(want.as_str()));

        let x11 = ClipEnv::new(&["xclip"], vec![ClipEnv::out(0, ""), ClipEnv::out(0, "")]);
        publish_system(&x11, Some(&clip)).expect("xclip publish");
        let calls = x11.calls();
        assert_eq!(calls[0].program, "xclip");
        assert_eq!(
            calls[0].args,
            ["-i", "-selection", "clipboard", "-t", URI_LIST_MIME]
        );
        assert_eq!(calls[0].stdin.as_deref(), Some(want.as_str()));

        // A clear publishes the *empty* uri-list with stdin still fed (Go's empty reader, not /dev/null).
        publish_system(&x11, None).expect("clear publish");
        let calls = x11.calls();
        assert_eq!(calls[1].stdin.as_deref(), Some(""));

        // No tool at all: the Go no-op — Ok, and nothing spawned.
        let none = ClipEnv::new(&[], Vec::new());
        publish_system(&none, Some(&clip)).expect("headless publish is a no-op");
        assert!(none.calls().is_empty());
    }

    #[test]
    fn read_system_prefers_wayland_then_xclip() {
        let scratch = Scratch::new("read");
        let plain = scratch.file("plain.txt", b"x");
        let plain_text = plain.display().to_string();

        // (a) The uri-list offer wins outright; xclip is never spawned.
        let env = ClipEnv::new(
            &["wl-paste", "xclip"],
            vec![ClipEnv::out(0, "file:///tree/a\nfile:///tree/b\n")],
        );
        let clip = read_system(&env, &OsDirRead).expect("wayland uri-list");
        assert!(clip.is_uri_list);
        assert_eq!(clip.paths, vec![abs("/tree/a"), abs("/tree/b")]);
        let calls = env.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, WL_PASTE);
        assert_eq!(calls[0].args, ["-t", URI_LIST_MIME]);

        // (b) The `-t` offer fails (no uri-list on the clipboard) → the plain wl-paste fallback.
        let env = ClipEnv::new(
            &["wl-paste", "xclip"],
            vec![
                ClipEnv::out(1, ""),
                ClipEnv::out(0, &format!("{plain_text}\n/stale/deleted.txt\n")),
            ],
        );
        let clip = read_system(&env, &OsDirRead).expect("wayland plain");
        assert!(!clip.is_uri_list);
        assert_eq!(
            clip.paths,
            vec![abs(&plain_text)],
            "the stale line is dropped"
        );
        assert_eq!(env.calls().len(), 2);

        // (c) Both wl-paste attempts fail → the xclip pair, uri-list first (Go's full fall-through).
        let env = ClipEnv::new(
            &["wl-paste", "xclip"],
            vec![
                ClipEnv::out(1, ""),
                ClipEnv::out(1, ""),
                ClipEnv::out(0, "file:///tree/c\n"),
            ],
        );
        let clip = read_system(&env, &OsDirRead).expect("xclip uri-list");
        assert!(clip.is_uri_list);
        assert_eq!(clip.paths, vec![abs("/tree/c")]);
        let calls = env.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[2].program, XCLIP);
        assert_eq!(
            calls[2].args,
            ["-o", "-selection", "clipboard", "-t", URI_LIST_MIME]
        );

        // (d) Nothing installed: Go's "no clipboard utility found".
        let env = ClipEnv::new(&[], Vec::new());
        let err = read_system(&env, &OsDirRead).expect_err("no tool is an error");
        assert_eq!(err.kind(), "unsupported");
        assert!(err.to_string().contains("no clipboard utility found"));
    }

    // ------------------------------------------------------------------ store / expand_inside

    #[test]
    fn store_expands_inside_like_go() {
        let scratch = Scratch::new("inside");
        scratch.empty_dir("payload/gamma");
        scratch.file("payload/beta.txt", b"123");
        scratch.file("payload/alpha.txt", b"x");
        let dir = scratch.path().join("payload").expect("join");

        let clip = store(
            &OsDirRead,
            &ClipboardStore {
                op: ClipOp::Cut,
                sources: vec![dir.clone()],
                scope: ClipScope::Inside,
            },
        )
        .expect("copy_inside");
        // Go's `os.ReadDir` order: lexical, so the paste order is alpha, beta, gamma.
        assert_eq!(
            clip.items
                .iter()
                .map(|i| i.display.as_str())
                .collect::<Vec<_>>(),
            ["alpha.txt", "beta.txt", "gamma"]
        );
        assert_eq!(clip.op, ClipOp::Cut);
        let children = ["alpha.txt", "beta.txt", "gamma"].map(|n| dir.join(n).expect("join"));
        assert_eq!(clip.paths(), children.to_vec());
        // `origin` is the parent of the first *queued* item — here the opened directory itself.
        assert_eq!(clip.origin.as_ref(), Some(&dir));
        let sizes = clip.items.iter().map(|i| i.size).collect::<Vec<_>>();
        assert_eq!(sizes[0], Some(1), "alpha.txt is one byte");
        assert_eq!(sizes[1], Some(3), "beta.txt is three bytes");
        assert!(
            sizes[2].is_some(),
            "a directory stat has a size on this host; None would mean the stat failed"
        );

        // The Selected scope queues the selection itself, and an empty selection is `Empty` (Go's
        // "No files selected").
        let selected = store(
            &OsDirRead,
            &ClipboardStore {
                op: ClipOp::Copy,
                sources: vec![dir.clone()],
                scope: ClipScope::Selected,
            },
        )
        .expect("copy");
        assert_eq!(selected.paths(), vec![dir]);
        let empty = store(
            &OsDirRead,
            &ClipboardStore {
                op: ClipOp::Copy,
                sources: Vec::new(),
                scope: ClipScope::Selected,
            },
        );
        assert_eq!(empty.expect_err("empty selection").kind(), "empty");
    }

    #[test]
    fn expand_inside_rejects_a_file_and_an_empty_dir() {
        let scratch = Scratch::new("reject");
        let file = scratch.path().join("f.txt").expect("join");
        std::fs::write(scratch.0.join("f.txt"), b"x").expect("write");
        let err = expand_inside(&OsDirRead, &file).expect_err("a file is not inside-able");
        assert_eq!(err.kind(), "not_a_directory");

        let empty_dir = AbsPath::from_path(scratch.empty_dir("empty")).expect("abs");
        let empty = expand_inside(&OsDirRead, &empty_dir)
            .expect_err("an empty folder is Go's 'folder is empty, nothing to copy/cut'");
        assert_eq!(empty.kind(), "empty");
    }
}
