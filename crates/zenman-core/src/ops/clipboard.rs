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
//! # Queued paths are [`FsPath`]s, not [`AbsPath`]s
//!
//! A selection can be an *archive member* — the pane browses `box.zip/inner` and Ctrl+C queues what it
//! shows. Go queued the container's absolute path because its `ClipboardState` had nothing else to hold,
//! which is the bug this type fixes: an `FsPath` names a member *and* its store, so the chip shows the
//! member's own name and uncompressed size, and the paste extracts it ([`Clipboard::is_archive`] is how
//! the caller routes it). The one thing an archive path cannot do is reach an external application —
//! no other program can open `file:///a/b.zip/inner` — so [`uri_list`] and [`publish_system`] publish
//! the host items only, and an all-member queue publishes the empty list (the same body a clear
//! writes) rather than a stale container URI.
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
use crate::path::{AbsPath, FsPath, VfsKind};
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
    /// The path the paste will read — a member of an archive store when the selection came from a
    /// pane browsing one, a plain filesystem path otherwise.
    pub path: FsPath,
    /// `filepath.Base(source)` — the name the chip shows (for a member, the member's own last segment,
    /// not the container's).
    pub display: String,
    /// `info.IsDir()` — Go answered it per item (`api/ops.go:166-176`), and the status bar's
    /// in-action popup needs it for the row glyph. `false` when the stat failed: a path nobody could
    /// read is not a directory, and the popup must not claim one.
    pub is_dir: bool,
    /// `info.Size()` (for a member: the *uncompressed* size, the number the pane painted), or `None`
    /// when the stat failed. Go reported that case as `0` (`api/ops.go:163-171`); plan §2.4 makes the
    /// difference visible.
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
    /// into it must refuse (`ops::guard`). The parent is *in the store*: a member queue from
    /// `box.zip/inner` origins at the `box.zip` store root, a host queue at its host directory.
    pub origin: Option<FsPath>,
}

impl Clipboard {
    /// The queued paths, in order — what `writeToSystemClipboard` and the paste loop consume.
    #[must_use]
    pub fn paths(&self) -> Vec<FsPath> {
        self.items.iter().map(|i| i.path.clone()).collect()
    }

    /// `true` when nothing is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// `true` when any queued item lives in an archive store. A `cut` queue never is (the caller
    /// refuses that before storing); a `copy` queue with any member pastes by *extracting* —
    /// see `Session::paste`, which routes on this.
    #[must_use]
    pub fn is_archive(&self) -> bool {
        self.items.iter().any(|i| i.path.store.is_archive())
    }

    /// The queued host paths (members contribute nothing) — the system uri-list payload, which an
    /// external application can only be given paths its own filesystem has.
    #[must_use]
    pub fn host_paths(&self) -> Vec<AbsPath> {
        self.items
            .iter()
            .filter(|i| matches!(i.path.store, VfsKind::Fs))
            .filter_map(|i| i.path.host_abs())
            .collect()
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
    /// What the user selected (or, with [`ClipScope::Inside`], the directories to open up) — host
    /// paths and archive members alike, in the pane's order.
    pub sources: Vec<FsPath>,
    /// Which of the two Go handlers this request answers.
    pub scope: ClipScope,
}

/// The read-only seam that makes an archive *member* statable, in the shape [`store`] and
/// [`expand_inside`] need: the same rows the pane painted, addressed by [`FsPath`] and nothing else.
///
/// Implemented by [`crate::archive::vfs::ArchiveStore`] (its `stat`/`list` *are* these two methods);
/// the trait exists so this module never names `Session` or the archive service. A `Fs`-store path
/// never reaches it — [`FsSource`] answers those.
pub trait MemberFacts: Send + Sync {
    /// One member's row — `VirtualNode::to_entry`, the source of truth for `is_dir`/`size`.
    ///
    /// # Errors
    /// [`CoreError::NotFound`] for an absent member, plus the store's own open/verify errors.
    fn stat_member(&self, path: &FsPath) -> Result<crate::entry::FileEntry, CoreError>;

    /// The rows of one archive directory, sorted by name.
    ///
    /// # Errors
    /// The store's errors (a non-directory is a typed refusal there, matching what a pane listing it
    /// would have shown).
    fn list_members(&self, path: &FsPath) -> Result<Vec<crate::entry::FileEntry>, CoreError>;
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
///
/// Copying out of an archive is a **read**, so a member source is legal here and gets the member's own
/// facts: `display` is the member's last segment, `is_dir`/`size` come from [`MemberFacts`] (the same
/// [`crate::archive::vfs::VirtualNode::to_entry`] rows the pane painted, so the chip cannot disagree
/// with the row). A `cut` of a member is *not* refused here — the caller knows the op
/// (`Session::clipboard_store` refuses it); this function only answers "what is queued".
pub fn store(
    source: &dyn FsSource,
    archives: &dyn MemberFacts,
    req: &ClipboardStore,
) -> Result<Clipboard, CoreError> {
    if req.sources.is_empty() {
        return Err(CoreError::Empty { what: "selection" });
    }
    let paths = match req.scope {
        ClipScope::Selected => req.sources.clone(),
        ClipScope::Inside => {
            let mut expanded = Vec::new();
            for dir in &req.sources {
                expanded.extend(expand_inside(source, archives, dir)?);
            }
            expanded
        }
    };
    if paths.is_empty() {
        // Unreachable through `expand_inside` (it refuses an empty directory), but the invariant is the
        // caller's, not the expander's: an empty queue is never a success.
        return Err(CoreError::Empty { what: "selection" });
    }
    let origin = paths.first().and_then(FsPath::parent);
    let items = paths
        .into_iter()
        .map(|path| {
            // One stat answers *both* the size and Go's `isDir` (`api/ops.go:166-176`), as before.
            let (is_dir, size) = facts(source, archives, &path);
            ClipItem {
                display: display_name(&path),
                is_dir,
                size,
                path,
            }
        })
        .collect();
    Ok(Clipboard {
        op: req.op,
        items,
        origin,
    })
}

/// The name the chip shows: the last segment of the path in its own store (for a member, the member's
/// basename — Go's `filepath.Base` of a host path is exactly `FsPath::last_segment` there). The store
/// roots have no last segment, and keep Go's full-text fallback (`api/ops.go:166`).
fn display_name(path: &FsPath) -> String {
    if let Some(name) = path.last_segment() {
        return name.to_string();
    }
    path.host_abs()
        .map_or_else(|| path.to_string(), |p| p.to_string_lossy())
}

/// One path's `(is_dir, size)`, asked of whichever store owns it — **once**, so the two can never
/// disagree about the same row.
///
/// `Fs`: Go statted with `os.Stat` and used `0` on failure (`api/ops.go:163-166`); the failure is kept
/// as `None` here, and the stat is `lstat` because that is the seam this crate has.
/// `Archive`: [`MemberFacts::stat_member`], whose `size` is the *uncompressed* member size and whose
/// `is_dir` is the manifest's own bit — a failed stat is the same `false`/`None` pair (a member that
/// vanished since the listing is not a directory the popup may claim).
fn facts(source: &dyn FsSource, archives: &dyn MemberFacts, path: &FsPath) -> (bool, Option<u64>) {
    if path.store.is_archive() {
        return archives
            .stat_member(path)
            .map(|entry| (entry.is_dir, u64::try_from(entry.size.max(0)).ok()))
            .unwrap_or((false, None));
    }
    // Go answered `isDir` per queued item (`api/ops.go:166-176`), and the status bar's in-action popup
    // draws its glyph from it; the one stat answers both, the same `is_navigable_dir` rule
    // [`expand_inside`] uses below.
    let info = path
        .host_abs()
        .and_then(|host| source.lstat(host.as_path()).ok());
    let size = info.as_ref().and_then(|info| u64::try_from(info.size).ok());
    let is_dir = info
        .as_ref()
        .is_some_and(|info| info.kind.is_navigable_dir());
    (is_dir, size)
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
/// paste order — and therefore the undo record — follows it. The same rule answers an archive
/// directory: [`MemberFacts::list_members`] is the manifest's name-sorted `children_of`, and the
/// children keep the store (`FsPath::join` carries host *and* nested, so a nested store stays correct).
pub fn expand_inside(
    source: &dyn FsSource,
    archives: &dyn MemberFacts,
    dir: &FsPath,
) -> Result<Vec<FsPath>, CoreError> {
    if dir.store.is_archive() {
        // The store root is a directory by construction (it is the open container); anything else has
        // to be *proved* one, the way the `lstat` branch below does.
        if !dir.is_store_root() {
            let entry = archives.stat_member(dir)?;
            if !entry.is_dir {
                return Err(CoreError::NotADir {
                    path: dir.to_string(),
                });
            }
        }
        let mut entries = archives.list_members(dir)?;
        if entries.is_empty() {
            return Err(CoreError::Empty { what: "folder" });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        return entries.iter().map(|e| dir.join(&e.name)).collect();
    }
    let host = dir.host_abs().ok_or_else(|| CoreError::NotADir {
        path: dir.to_string(),
    })?;
    let info = source.lstat(host.as_path())?;
    if !info.kind.is_navigable_dir() {
        return Err(CoreError::NotADir {
            path: dir.to_string(),
        });
    }
    let mut entries = source.read_dir(&host)?;
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
/// Only the **host** paths are spelled out: an external application cannot open `file:///a/b.zip/inner`,
/// so a member contributes nothing (its container is *not* published either — that would offer the other
/// app a paste of the whole zip, the exact bug this queue used to be). An all-member list is therefore
/// empty, which is the same body a clipboard *clear* publishes (`api/ops.go:216`).
#[must_use]
pub fn uri_list(paths: &[FsPath]) -> String {
    paths
        .iter()
        .filter(|p| matches!(p.store, VfsKind::Fs))
        .filter_map(FsPath::host_abs)
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
/// An **all-member** queue publishes that same empty body — an external app cannot read a member URI,
/// and publishing the container instead would offer a paste of the whole archive.
/// With neither `wl-copy` nor `xclip` installed this is a **silent no-op**, exactly as in Go
/// (`api/ops.go:573` has no `else`), so a headless box does not fail every copy; the answer then is
/// `Ok(None)`, and `Ok(Some(tool))` names the tool a publish actually went through.
///
/// The child's exit status is ignored, as Go's `_ = cmd.Run()` ignored it: a clipboard manager that
/// refuses the payload must not turn a successful copy into a failed one. An `Err` here therefore means
/// only "the platform could not even spawn", which the session reports as a degraded sync.
pub fn publish_system(
    env: &dyn Env,
    clip: Option<&Clipboard>,
) -> Result<Option<SystemClipTool>, CoreError> {
    let paths = clip.map_or_else(Vec::new, |c| c.paths());
    let body = uri_list(&paths);
    let tool = detect_tool(env);
    let req = match tool {
        SystemClipTool::Wayland => {
            ExecRequest::argv(WL_COPY, ["-t", URI_LIST_MIME])
                .with_stdin(body)
                // `wl-copy` hands the payload to the compositor and leaves a helper behind; waiting for its
                // pipes is waiting for a process that is not the one we started (step 9b, plan F1).
                .detached()
        }
        SystemClipTool::Xclip => ExecRequest::argv(
            XCLIP,
            ["-i", "-selection", "clipboard", "-t", URI_LIST_MIME],
        )
        .with_stdin(body)
        // As above: `xclip -i` forks the selection owner, which inherits the output pipes.
        .detached(),
        SystemClipTool::None => return Ok(None),
    };
    // Output discarded on purpose — see the doc above. Both layers of the `Result` count: the outer is
    // the job runner's, the inner the platform's refusal to spawn.
    jobs::block_on(env.exec_program(req))??;
    Ok(Some(tool))
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

    /// A unique temp dir, removed on drop — the `linux.rs` probe-test pattern (`OsDirRead` needs a real
    /// filesystem, and the suite runs in parallel, so the name carries a counter *and* the pid, via
    /// `crate::scratch`).
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            Self(crate::scratch::create(&format!("clip-{tag}")))
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
            let _ = crate::scratch::remove_tree(&self.0);
        }
    }

    fn abs(text: &str) -> AbsPath {
        AbsPath::from_path(text).expect("test path is absolute and clean")
    }

    fn fs(text: &str) -> FsPath {
        FsPath::from_abs(abs(text))
    }

    /// The [`MemberFacts`] of a test that never queues a member: an archive call here is a bug in the
    /// test, so it answers the typed refusal rather than a panic.
    struct NoMembers;

    impl MemberFacts for NoMembers {
        fn stat_member(&self, path: &FsPath) -> Result<crate::entry::FileEntry, CoreError> {
            Err(CoreError::UnsupportedArchive {
                archive: path.to_string(),
            })
        }
        fn list_members(&self, path: &FsPath) -> Result<Vec<crate::entry::FileEntry>, CoreError> {
            Err(CoreError::UnsupportedArchive {
                archive: path.to_string(),
            })
        }
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
        let queued: Vec<FsPath> = originals.iter().cloned().map(FsPath::from_abs).collect();
        let body = uri_list(&queued);
        assert_eq!(body.lines().count(), originals.len());
        assert!(
            !body.ends_with('\n'),
            "Go joined with \\n, no trailing newline"
        );
        assert_eq!(parse_uri_list(&body), originals.to_vec());
    }

    #[test]
    fn uri_list_publishes_host_paths_only() {
        // Decision 5: an external app cannot read `file:///a/b.zip/inner`, and it must not be handed
        // the container either — only plain host items reach the wire.
        let member = FsPath::archive(abs("/a/b.zip"), None, "inner").expect("member path");
        assert_eq!(
            uri_list(std::slice::from_ref(&member)),
            "",
            "an all-member list is empty"
        );
        let host = fs("/a/host.txt");
        assert_eq!(
            uri_list(&[member, host.clone()]),
            uri_list(&[host]),
            "a mixed list carries the host item and nothing else"
        );
    }

    // ------------------------------------------------------------------ system-clipboard commands

    #[test]
    fn publish_system_builds_the_go_command_line() {
        let clip = Clipboard {
            op: ClipOp::Copy,
            items: vec![
                ClipItem {
                    path: fs("/tree/a b.txt"),
                    display: "a b.txt".into(),
                    is_dir: false,
                    size: None,
                },
                ClipItem {
                    path: fs("/tree/c"),
                    display: "c".into(),
                    is_dir: true,
                    size: Some(7),
                },
            ],
            origin: Some(fs("/tree")),
        };
        let want = uri_list(&clip.paths());

        let wayland = ClipEnv::new(&["wl-copy", "wl-paste"], vec![ClipEnv::out(0, "")]);
        assert_eq!(
            publish_system(&wayland, Some(&clip)).expect("wayland publish"),
            Some(SystemClipTool::Wayland),
            "the answer names the tool the publish went through"
        );
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

        // No tool at all: the Go no-op — Ok, nothing spawned, and the answer says so.
        let none = ClipEnv::new(&[], Vec::new());
        assert_eq!(
            publish_system(&none, Some(&clip)).expect("headless publish is a no-op"),
            None
        );
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
        let dir = FsPath::from_abs(scratch.path().join("payload").expect("join"));

        let clip = store(
            &OsDirRead,
            &NoMembers,
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
        let children: Vec<FsPath> = ["alpha.txt", "beta.txt", "gamma"]
            .iter()
            .map(|n| dir.join(n).expect("join"))
            .collect();
        assert_eq!(clip.paths(), children);
        // `origin` is the parent of the first *queued* item — here the opened directory itself.
        assert_eq!(clip.origin.as_ref(), Some(&dir));
        let sizes = clip.items.iter().map(|i| i.size).collect::<Vec<_>>();
        assert_eq!(sizes[0], Some(1), "alpha.txt is one byte");
        assert_eq!(sizes[1], Some(3), "beta.txt is three bytes");
        assert!(
            sizes[2].is_some(),
            "a directory stat has a size on this host; None would mean the stat failed"
        );
        // Go answered `isDir` per queued item (`api/ops.go:166-176`), and the status bar's in-action
        // popup draws its glyph from it. One `lstat` serves the size and this, so the two can never
        // disagree about the same row.
        let dirs = clip.items.iter().map(|i| i.is_dir).collect::<Vec<_>>();
        assert_eq!(
            dirs,
            vec![false, false, true],
            "alpha.txt/beta.txt are files, gamma is the directory"
        );

        // The Selected scope queues the selection itself, and an empty selection is `Empty` (Go's
        // "No files selected").
        let selected = store(
            &OsDirRead,
            &NoMembers,
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
            &NoMembers,
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
        let err = expand_inside(&OsDirRead, &NoMembers, &FsPath::from_abs(file))
            .expect_err("a file is not inside-able");
        assert_eq!(err.kind(), "not_a_directory");

        let empty_dir = AbsPath::from_path(scratch.empty_dir("empty")).expect("abs");
        let empty = expand_inside(&OsDirRead, &NoMembers, &FsPath::from_abs(empty_dir))
            .expect_err("an empty folder is Go's 'folder is empty, nothing to copy/cut'");
        assert_eq!(empty.kind(), "empty");
    }
}
