//! Engine → UI events (plan §4.1's `events.rs` block, §5.6, §6.1 F2/F13/F14).
//!
//! This is the in-process replacement for "the browser re-fetches after a request returns": every
//! mutation, every finished job and every cache invalidation arrives here, correlated by
//! [`Origin`](crate::session::Origin) so the widget that asked is the widget whose spinner stops
//! (plan §4.3's correlation rule).
//!
//! Two rules the shapes below encode:
//!
//! * **`DirChanged` names an [`FsPath`], never an [`AbsPath`](crate::path::AbsPath)** — an archive
//!   member has to be invalidatable, and a paste into or a refresh of an archive is otherwise
//!   unrepresentable (plan §9.1's correction to §4.1; §5.9's `DirService::invalidate(&FsPath)` row).
//! * **`JobFinished` carries a typed [`JobPayload`]**, so the UI never re-parses `JobOutcome::summary`
//!   to find out what happened. Each `JobKind` has exactly one payload variant; a mismatch is a
//!   programming error the debug assertions in `session.rs` catch, not a runtime parse failure.

use std::sync::Arc;

use serde::Serialize;

use crate::entry::DirPage;
use crate::error::CoreError;
use crate::history::store::LogId;
use crate::jobs::{JobId, JobKind, JobOutcome, JobProgress};
use crate::ops::clipboard::Clipboard;
use crate::ops::delete::DeleteReport;
use crate::ops::paste::PasteReport;
use crate::path::AbsPath;
use crate::services::dirs::DirSizeStat;
use crate::session::Origin;

/// Everything the engine may tell the UI. `Clone` because one event can feed several listeners (the
/// jobs pane and the tab that asked both care about a `JobFinished`).
#[derive(Clone, Debug)]
pub enum EngineEvent {
    /// A job was accepted and is running.
    JobStarted {
        /// The job.
        id: JobId,
        /// Its kind.
        kind: JobKind,
        /// Human label, already worded ("Paste 3 items").
        label: String,
        /// The widget that asked.
        origin: Origin,
    },
    /// Counters moved. Coalesced in `JobRegistry::poll` to at most one per
    /// [`PROGRESS_THROTTLE`](crate::limits::PROGRESS_THROTTLE) per job (plan F2).
    JobProgress {
        /// The job.
        id: JobId,
        /// Who asked.
        origin: Origin,
        /// The counters — never anything but counters (see [`JobOutcome`]'s note on `Finished`).
        progress: JobProgress,
    },
    /// A job ended. Exactly one of these per job.
    JobFinished {
        /// The job.
        id: JobId,
        /// Its kind, so the listener can assert the payload type without a lookup.
        kind: JobKind,
        /// Who asked. `Receipt::Running`'s origin, returned to sender.
        origin: Origin,
        /// The summary, moved pairs, warnings and byte count.
        outcome: JobOutcome,
        /// The typed result.
        payload: JobPayload,
        /// The typed failure, `Some` exactly when `outcome.status` is `Failed`/`Cancelled`/`TimedOut`.
        /// An `Arc` because [`CoreError`] is not `Clone` (it carries an `Arc<io::Error>` for the
        /// `Io` variant, and cloning the whole enum would mean cloning that path text).
        error: Option<Arc<CoreError>>,
    },
    /// A directory page produced by a job (cold cache / flat walk / archive manifest build).
    DirPageReady {
        /// Who asked.
        origin: Origin,
        /// What was listed.
        path: crate::path::FsPath,
        /// The page.
        page: Arc<DirPage>,
    },
    /// The asynchronous answer to `dir_size`, keyed by path and broadcast to every row showing it.
    DirSizeReady {
        /// The directory that was walked.
        path: AbsPath,
        /// The walk's answer, in the `/api/dir/size` fields.
        stat: DirSizeStat,
    },
    /// The clipboard queue changed (`Session::clipboard_store` / [`crate::session::Session::clipboard_clear`],
    /// plan §4.1). Broadcast, not origin-routed: every pane showing a clipboard badge must agree at once.
    ClipboardChanged {
        /// The new queue; `None` after a clear (Go's `handleClearClipboard`).
        clip: Option<Clipboard>,
    },
    /// A listing went stale. `FsPath` (not `AbsPath`) so archive members can be named — plan §9.1.
    DirChanged {
        /// The directory that must be re-read.
        path: crate::path::FsPath,
        /// Why, which decides whether the pane resets or refreshes quietly (plan §4.1's note).
        cause: ChangeCause,
        /// The originating widget when the change came from a job.
        origin: Option<Origin>,
    },
    /// The FTS index changed shape (a rebuild landed, rows were added by the intent queue).
    IndexChanged {
        /// Rows now in `files`.
        total: u64,
        /// Whether the index is behind the filesystem.
        dirty: bool,
    },
    /// An undo-log record was appended, so the history pane can refresh (plan §4.1).
    LogAppended {
        /// The new record.
        id: LogId,
    },
    /// Something the core wants the user to see and cannot deliver through a return value: a
    /// degraded startup, a skipped-collision list (plan F12), a toast.
    Notice {
        /// How loud.
        level: NoticeLevel,
        /// The text, already in the user's language where the caller had one.
        message: String,
    },
}

impl EngineEvent {
    /// The origin to route this event to, when it has one (plan §4.3: nothing may look a result up by
    /// path alone).
    #[must_use]
    pub const fn origin(&self) -> Option<Origin> {
        match self {
            Self::JobStarted { origin, .. }
            | Self::JobProgress { origin, .. }
            | Self::JobFinished { origin, .. }
            | Self::DirPageReady { origin, .. } => Some(*origin),
            Self::DirChanged { origin, .. } => *origin,
            Self::DirSizeReady { .. }
            | Self::IndexChanged { .. }
            | Self::LogAppended { .. }
            | Self::ClipboardChanged { .. } => None,
            Self::Notice { .. } => None,
        }
    }

    /// The job this event belongs to, when it is one of the three job events.
    #[must_use]
    pub const fn job_id(&self) -> Option<JobId> {
        match self {
            Self::JobStarted { id, .. }
            | Self::JobProgress { id, .. }
            | Self::JobFinished { id, .. } => Some(*id),
            _ => None,
        }
    }

    /// `true` for the terminal job event — the one that clears a `Tab::pending` entry.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::JobFinished { .. })
    }
}

/// Why a listing went stale (plan §4.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeCause {
    /// A job in this process wrote there. Tabs that asked for the job already know (their
    /// `Tab::pending` holds the origin), so the *other* tabs refresh quietly, keeping scroll and
    /// selection (plan §4.1's sequence note).
    LocalJob,
    /// The filesystem watcher saw it (plan §5.5 `index/watcher.rs`).
    Watcher,
    /// The index intent queue reconciled it.
    Index,
    /// Somebody else changed the directory.
    External,
}

/// The typed result of a job, per [`JobKind`] (plan §4.1's `JobPayload`).
///
/// Step 6 defines the variants whose result types exist yet — the directory page and the size walk —
/// plus the three the plan names for this step's acceptance (`extract`/`index`/`search`), whose
/// result shapes are the small summaries below. Step 9+ adds `Paste`/`Delete`/`Revert`/`Exec`
/// alongside their real report types rather than overloading these.
#[derive(Clone, Debug, PartialEq)]
pub enum JobPayload {
    /// A directory page (`JobKind::List`).
    DirPage(Arc<DirPage>),
    /// A completed paste (`JobKind::Paste`).
    Paste(PasteReport),
    /// A completed delete (`JobKind::Delete`).
    Delete(DeleteReport),
    /// A completed undo-log revert (`JobKind::Revert`).
    Revert(crate::history::revert::RevertReport),
    /// A completed size walk (`JobKind::DirSize`).
    DirSize(DirSizeStat),
    /// Live search hits (`JobKind::SearchLive`).
    Search(SearchSummary),
    /// Archive extraction (`JobKind::Extract`).
    Extract(ExtractSummary),
    /// Index rebuild (`JobKind::IndexRebuild`).
    Index(IndexSummary),
    /// A job whose answer lives entirely in its [`JobOutcome`](crate::jobs::JobOutcome).
    None,
}

impl JobPayload {
    /// The `DirPage` payload, when that is what this is.
    #[must_use]
    pub fn dir_page(&self) -> Option<&Arc<DirPage>> {
        match self {
            Self::DirPage(p) => Some(p),
            _ => None,
        }
    }

    /// The `DirSizeStat` payload, when that is what this is.
    #[must_use]
    pub fn dir_size(&self) -> Option<&DirSizeStat> {
        match self {
            Self::DirSize(s) => Some(s),
            _ => None,
        }
    }

    /// The `PasteReport` payload, when that is what this is.
    #[must_use]
    pub fn paste(&self) -> Option<&PasteReport> {
        match self {
            Self::Paste(r) => Some(r),
            _ => None,
        }
    }

    /// The `DeleteReport` payload, when that is what this is.
    #[must_use]
    pub fn delete(&self) -> Option<&DeleteReport> {
        match self {
            Self::Delete(r) => Some(r),
            _ => None,
        }
    }

    /// The `RevertReport` payload, when that is what this is.
    #[must_use]
    pub fn revert(&self) -> Option<&crate::history::revert::RevertReport> {
        match self {
            Self::Revert(r) => Some(r),
            _ => None,
        }
    }
}

impl From<PasteReport> for JobPayload {
    fn from(value: PasteReport) -> Self {
        Self::Paste(value)
    }
}

impl From<DeleteReport> for JobPayload {
    fn from(value: DeleteReport) -> Self {
        Self::Delete(value)
    }
}

impl From<crate::history::revert::RevertReport> for JobPayload {
    fn from(value: crate::history::revert::RevertReport) -> Self {
        Self::Revert(value)
    }
}

impl From<DirPage> for JobPayload {
    fn from(value: DirPage) -> Self {
        Self::DirPage(Arc::new(value))
    }
}

impl From<Arc<DirPage>> for JobPayload {
    fn from(value: Arc<DirPage>) -> Self {
        Self::DirPage(value)
    }
}

impl From<DirSizeStat> for JobPayload {
    fn from(value: DirSizeStat) -> Self {
        Self::DirSize(value)
    }
}

impl From<()> for JobPayload {
    fn from(_: ()) -> Self {
        Self::None
    }
}

/// What a live search produced. Step 11 replaces the hit rows with its own `SearchHit`; the shape the
/// UI needs *now* is "how many, and were we cut off".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchSummary {
    /// Hits collected.
    pub hits: usize,
    /// The result limit was reached before the walk ended (plan §5.3 `live_search_cap`).
    pub truncated: bool,
    /// Directories visited.
    pub nodes_seen: usize,
}

/// What an extract produced (plan §5.5 `ExtractState`, whose byte counters are the same ones but
/// read live; this is the terminal answer).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtractSummary {
    /// Members written.
    pub files: u64,
    /// Uncompressed bytes written — the number the archive-bomb cap is checked against (plan F8).
    pub bytes: u64,
    /// Where they went.
    pub dest: Option<AbsPath>,
}

/// What an index rebuild produced (plan §5.5 `index/rebuild.rs::RebuildSummary`, same reason: the
/// `roots_failed` text is per-mount, and Go collapsed it into one generic string).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexSummary {
    /// Rows written.
    pub rows: u64,
    /// Roots that walked cleanly.
    pub roots_ok: usize,
    /// Per-root failure text, kept per root (plan F6).
    pub roots_failed: Vec<(String, String)>,
}

/// How loud a [`EngineEvent::Notice`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    /// Informational (a job finished with warnings).
    Info,
    /// Something degraded but works (the database is disabled, the log was repaired).
    Warn,
    /// Something the user asked for did not happen.
    Error,
}
