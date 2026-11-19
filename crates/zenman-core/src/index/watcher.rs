//! The filesystem watcher (plan §5.5 `index/watcher.rs` row): raw directory events → the *coalesced*
//! [`DirChanged`] the rest of the engine speaks.
//!
//! # What Go actually ships, and why this file does not look like `notify`
//!
//! The plan row names `notify` and a "Go `DebounceEventWatcher` 200 ms". Both need correcting against
//! the reference tree (measured, not assumed):
//!
//! * **Go's shipped code has no watcher at all.** `grep -rniE "watch|fsnotify" --include='*.go'` over
//!   `/media/jang/home/Deve/zen-man` returns nothing; `fsnotify` appears only in that repo's own
//!   `PLAN.md:164-196`, an unimplemented sketch that `watcher.Add(dir)`-s **one** directory and forwards
//!   every raw `fsnotify` op over a websocket — no coalescing, no index update, no recursion, no
//!   debouncer. So the "200 ms `DebounceEventWatcher`" is the *plan's* name for behaviour Go never had,
//!   and there is no Go event-merge rule to copy: the mapping below is ours, justified against the
//!   inotify mask set and against what a name index can actually use.
//! * **`notify` is not in `Cargo.lock` and adding a package is out of scope for this slice.** The
//!   backend is `rustix`'s inotify API (`rustix::fs::inotify`), which is *already* a pinned
//!   MIT/Apache-2.0 dependency and *already* carries the `fs` feature that exposes it — rustix 1.1.6 has
//!   **no `inotify` cargo feature**, so `watch = []` adds nothing to the dependency graph and
//!   `cargo xtask licenses` output is byte-identical with the feature on or off.
//!
//! # Consequences of that choice (all deliberate)
//!
//! * **Linux-only.** `rustix::fs::inotify` is `#[cfg(linux_raw_dep)]`. The real watcher is therefore
//!   `#[cfg(all(feature = "watch", target_os = "linux"))]`, and every other combination compiles to the
//!   stub whose `start` returns the same typed [`CoreError::Watcher`](crate::error::CoreError::Watcher)
//!   the feature-off build returns. Step 16 (`platform/windows`) owns the ReadDirectoryChangesW backend;
//!   it must keep *both* halves of this gate — a `#[cfg(feature)]`-only gate is a compile error on the
//!   MSVC target, which is exactly what §7 step 16's "`#[cfg]` stub audit" is for.
//! * **One watch per directory.** inotify takes one `inotify_add_watch` per directory, so a recursive watch
//!   over N roots costs N watches plus one per subdirectory that appears — `notify`'s recursive adapters
//!   and Go's `fsnotify` (which has no recursion at all, hence the sketch watching one dir) have exactly
//!   the same cost, it is only hidden. The ceiling is [`WatchConfig::max_watches`], taken from
//!   [`Limits`] (`limits::WATCH_MAX_WATCHES`), never a literal in this file: a root set over budget is
//!   refused at `start` with a typed error, and a subdirectory that arrives after the budget is exhausted
//!   is *not* watched but is reported through [`Watcher::unwatched`] — silently missing half a tree is
//!   the Go failure mode (`api/*.go`'s ignored errors) this port is not allowed to repeat.
//! * **The debounce is ours.** [`Coalescer`] is the pure, injectable-clock half of it (no threads, no
//!   filesystem — that is what makes "one event per burst, per path, after 200 ms" a unit test); the
//!   reader thread only feeds it and forwards what is due.
//!
//! # The mask → event table (Linux backend; `masks_map_onto_the_three_events` pins it)
//!
//! | inotify | → | why |
//! |---|---|---|
//! | `IN_CREATE`, `IN_MOVED_TO` | `Created` | a name appeared; the index upserts it |
//! | `IN_DELETE`, `IN_DELETE_SELF`, `IN_MOVED_FROM` | `Removed` | a name is gone; the index drops it (and its subtree) |
//! | `IN_ATTRIB` | `Metadata` | chmod/xattr; no index column changes, but a pane showing the mode does |
//! | `IN_MODIFY` | *nothing* | see below |
//! | `IN_ACCESS`, `IN_OPEN`, `IN_CLOSE_WRITE`, `IN_CLOSE_NOWRITE` | *nothing* | the entry did not change |
//! | `IN_MOVE_SELF` | *nothing* | the *watched directory* moved; its parent's watch delivers the `MOVED_FROM`/`MOVED_TO` pair, so forwarding this would double-report |
//! | `IN_IGNORED`, `IN_UNMOUNT`, `IN_Q_OVERFLOW` | *nothing* | bookkeeping about the watch, not about a path (`IGNORED` still retires the watch descriptor) |
//!
//! **Why `IN_MODIFY` is deliberately not forwarded.** It fires once per `write(2)` of a copy, so a
//! 2 000-file paste is thousands of masks; and the index cannot answer a content-only change anyway —
//! `files_fts` indexes `name` (`index/schema.rs`, `contract_search`'s FTS assertions), and the two other
//! columns a modify touches (`size`, `mtime`) are refreshed by the bracketing `IN_CREATE`/`IN_MOVED_TO`
//! upsert that every copy/delete sequence produces. A modify with no name change is therefore either
//! already covered or not answerable, which is the same conclusion Go reached by having no watcher at all
//! and re-walking the directory on every listing.
//!
//! # Who consumes the events
//!
//! The watcher owns a descriptor and one thread, and never touches a database. `Session` drains the
//! channel in `poll_at` and turns every event into BOTH a
//! [`ChangeCause`](crate::events::ChangeCause) invalidation (the pane reload path from step 9d) and an
//! [`IndexOp`](crate::index::intent::IndexOp) into the *same*
//! [`QueuedIntent`](crate::index::updater::QueuedIntent) the write path uses — plan §9.2 step-12a note
//! (2): a watcher that held a connection would serialise against a rebuild, which is the reason
//! [`intent`](crate::index::intent) exists.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::limits::Limits;
use crate::path::AbsPath;

/// The plan's coalescing window (the plan's "DebounceEventWatcher 200 ms"). Trailing-edge: an event
/// arriving inside an open window **restarts** it, so a burst into one directory is one event.
pub const COALESCE_WINDOW: Duration = Duration::from_millis(200);

/// How often the reader thread wakes to poll the non-blocking inotify fd and to age the coalescer. The
/// fd is `NONBLOCK` rather than blocking so that [`Watcher::stop`] never has to race a parked `read(2)`
/// (no self-pipe, no `eventfd`, nothing to leak); the cost is that an event is at most one tick late,
/// invisible next to a 200 ms window.
#[cfg(all(feature = "watch", target_os = "linux"))]
pub const POLL_TICK: Duration = Duration::from_millis(20);

// ------------------------------------------------------------------------------------------- events

/// A directory entry changed, already coalesced (plan §5.5's `DirChanged`).
///
/// Three kinds only, because that is all a *name* index can act on: the index stores path, size and
/// mtime (`index/schema.rs`), so "content changed" is the same question as "the name list changed", and
/// "attributes changed" has no column to update but does have a pane to repaint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirChanged {
    /// Something appeared at `path` (`IN_CREATE`, `IN_MOVED_TO`).
    Created {
        /// What appeared.
        path: AbsPath,
    },
    /// `path` is gone (`IN_DELETE`, `IN_DELETE_SELF`, `IN_MOVED_FROM`).
    Removed {
        /// What vanished.
        path: AbsPath,
    },
    /// `path`'s attributes changed (`IN_ATTRIB` — chmod, xattr, a ctime-only touch).
    Metadata {
        /// What was re-moded.
        path: AbsPath,
    },
}

impl DirChanged {
    /// The path this event names.
    #[must_use]
    pub fn path(&self) -> &AbsPath {
        match self {
            Self::Created { path } | Self::Removed { path } | Self::Metadata { path } => path,
        }
    }

    /// The stable kind label (tests, and the diagnostics the 12c search-manager dialog wants).
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Created { .. } => "created",
            Self::Removed { .. } => "removed",
            Self::Metadata { .. } => "metadata",
        }
    }

    /// The end state of two events on one path inside one window.
    ///
    /// The later event wins — with one exception: [`Self::Metadata`] is the *weakest* signal, because it
    /// asserts nothing about whether the name exists, so a create/delete that it would overwrite
    /// survives it. `Created` then `Removed` therefore collapses to `Removed` (the honest end state of a
    /// file written and deleted, and the safe one for the index: the row must go), and `Removed` then
    /// `Created` to `Created`.
    #[must_use]
    pub fn merge(earlier: &Self, later: &Self) -> Self {
        match (earlier, later) {
            (Self::Metadata { .. }, _) => later.clone(),
            (_, Self::Metadata { .. }) => earlier.clone(),
            _ => later.clone(),
        }
    }
}

// --------------------------------------------------------------------------------------- coalescing

/// The pure debounce unit: path → the merged event and the instant its window closes.
///
/// No threads, no clock reads, no filesystem: every method takes `now`, which is what makes the 200 ms
/// boundary a unit test rather than a sleep.
#[derive(Debug, Clone)]
pub struct Coalescer {
    window: Duration,
    pending: BTreeMap<AbsPath, (DirChanged, Instant)>,
}

impl Coalescer {
    /// A coalescer with the given quiet period.
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            pending: BTreeMap::new(),
        }
    }

    /// The window in force.
    #[must_use]
    pub const fn window(&self) -> Duration {
        self.window
    }

    /// Record one event, merging it into an open window on the same path and restarting that window.
    /// Returns `true` when this opened a window (the path was not already pending) — the count a "a burst
    /// is one event" test wants.
    pub fn record(&mut self, event: DirChanged, now: Instant) -> bool {
        let due = now + self.window;
        let path = event.path().clone();
        match self.pending.get_mut(&path) {
            Some((held, deadline)) => {
                *held = DirChanged::merge(held, &event);
                *deadline = due;
                false
            }
            None => {
                self.pending.insert(path, (event, due));
                true
            }
        }
    }

    /// Every event whose window closed at or before `now` (inclusive: exactly 200 ms is due), in path
    /// order, removed from the map.
    pub fn take_due(&mut self, now: Instant) -> Vec<DirChanged> {
        let ready: Vec<AbsPath> = self
            .pending
            .iter()
            .filter(|(_, (_, deadline))| *deadline <= now)
            .map(|(path, _)| path.clone())
            .collect();
        let mut out = Vec::with_capacity(ready.len());
        for path in ready {
            if let Some((event, _)) = self.pending.remove(&path) {
                out.push(event);
            }
        }
        out
    }

    /// Everything pending regardless of deadline — the flush [`Watcher::stop`] does so a caller that is
    /// shutting down does not silently drop the last burst.
    pub fn take_all(&mut self) -> Vec<DirChanged> {
        std::mem::take(&mut self.pending)
            .into_values()
            .map(|(event, _)| event.clone())
            .collect()
    }

    /// When the next window closes, if any — the reader thread's sleep bound.
    #[must_use]
    pub fn next_due(&self) -> Option<Instant> {
        self.pending.values().map(|(_, deadline)| *deadline).min()
    }

    /// Paths with an open window.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// `true` when nothing is pending.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

// ----------------------------------------------------------------------------------------- settings

/// The two knobs plan §5.5's watcher row leaves implicit, so that neither is a literal at a call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WatchConfig {
    /// The quiet period of [`Coalescer`].
    pub window: Duration,
    /// Watch-descriptor ceiling for this watcher (one per watched directory, roots included).
    pub max_watches: usize,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self::from_limits(&Limits::default())
    }
}

impl WatchConfig {
    /// Derive the knobs from the engine limits — the budget is a [`Limits`] number, not a constant in
    /// this module (plan §5.2's single-tuning-point rule).
    #[must_use]
    pub fn from_limits(limits: &Limits) -> Self {
        Self {
            window: COALESCE_WINDOW,
            max_watches: limits.watch_max_watches,
        }
    }
}

/// The no-backend answer, shared by the feature-off build and every non-Linux target: the watcher is
/// unavailable, and that is a typed error, not a silent `Ok` (plan §5.5's stub row).
#[cfg(not(all(feature = "watch", target_os = "linux")))]
#[must_use]
fn watcher_unavailable() -> crate::error::CoreError {
    #[cfg(not(feature = "watch"))]
    let why = "built without the `watch` feature";
    #[cfg(all(feature = "watch", not(target_os = "linux")))]
    let why = "the `watch` backend is inotify, which this platform does not have";
    crate::error::CoreError::Watcher {
        msg: format!("filesystem watcher unavailable: {why}"),
    }
}

#[cfg(all(feature = "watch", target_os = "linux"))]
pub use linux::{Kind, WATCH_MASK, classify_kind};
#[cfg(all(feature = "watch", target_os = "linux"))]
pub use linux::{Watcher, start};

#[cfg(not(all(feature = "watch", target_os = "linux")))]
pub use stub::{Watcher, start};

/// The feature-off / non-Linux half: the same API surface, every call a typed error.
#[cfg(not(all(feature = "watch", target_os = "linux")))]
mod stub {
    use std::sync::mpsc::Sender;

    use super::watcher_unavailable;
    use crate::error::CoreError;
    use crate::path::AbsPath;

    /// A watcher that cannot watch (plan §5.5's `#[cfg(not(feature = "watch"))]` row). It exists so
    /// `Session` compiles identically with and without the feature: the failure is `start`'s
    /// [`CoreError::Watcher`], never a missing type.
    #[derive(Debug)]
    pub struct Watcher;

    impl Watcher {
        /// Nothing to stop, nothing to flush.
        #[must_use]
        pub fn stop(self) -> Vec<super::DirChanged> {
            Vec::new()
        }

        /// Always `false`.
        #[must_use]
        pub const fn is_running(&self) -> bool {
            false
        }

        /// Always `0` — nothing is watched, so nothing costs a descriptor.
        #[must_use]
        pub const fn watch_count(&self) -> usize {
            0
        }

        /// Always `Err(CoreError::Watcher)`.
        ///
        /// # Errors
        /// Always — see the module docs.
        pub fn add_root(&mut self, _root: &AbsPath) -> Result<(), CoreError> {
            Err(watcher_unavailable())
        }
    }

    /// Always `Err(CoreError::Watcher)`.
    ///
    /// # Errors
    /// Always — see the module docs.
    pub fn start(
        _roots: &[AbsPath],
        _cfg: &super::WatchConfig,
        _tx: Sender<super::DirChanged>,
    ) -> Result<Watcher, CoreError> {
        Err(watcher_unavailable())
    }
}

/// The Linux `watch` half: the inotify descriptor, the reader thread, the mask mapping.
#[cfg(all(feature = "watch", target_os = "linux"))]
mod linux {
    use std::collections::HashMap;
    use std::mem::MaybeUninit;
    use std::os::unix::ffi::OsStrExt;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc::Sender;
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::thread::JoinHandle;
    use std::time::Instant;

    use rustix::fd::{AsFd, OwnedFd};
    use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
    use rustix::io::Errno;

    use super::{Coalescer, DirChanged, POLL_TICK, WatchConfig};
    use crate::error::CoreError;
    use crate::path::AbsPath;

    /// The mask a directory watch is registered with: everything, and the filtering happens in
    /// [`classify_kind`] — inotify delivers the interesting bits *and* the boring ones, and a watch mask
    /// that omitted `IN_ATTRIB` would silently lose every chmod.
    pub const WATCH_MASK: WatchFlags = WatchFlags::ALL_EVENTS;

    /// One inotify read buffer: ~130 `inotify_event`s. The loop drains until `WOULDBLOCK`, so the size is
    /// an allocation/latency trade, not a correctness bound.
    const READ_BUF: usize = 8 * 1024;

    /// Which of the three events a mask means (the variant of [`DirChanged`] without its path).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Kind {
        /// `IN_CREATE` / `IN_MOVED_TO`.
        Created,
        /// `IN_DELETE` / `IN_DELETE_SELF` / `IN_MOVED_FROM`.
        Removed,
        /// `IN_ATTRIB`.
        Metadata,
    }

    /// The inotify mask → event mapping, i.e. the whole event policy of the watcher in one pure
    /// function (the table and its rationale are in the module docs).
    ///
    /// Removal is checked before creation because a mask that carries both must resolve to the state the
    /// reader can act on safely: dropping a row that may not exist is harmless, keeping a row that is
    /// gone is not.
    #[must_use]
    pub fn classify_kind(mask: ReadFlags) -> Option<Kind> {
        if mask.intersects(ReadFlags::DELETE | ReadFlags::DELETE_SELF | ReadFlags::MOVED_FROM) {
            return Some(Kind::Removed);
        }
        if mask.intersects(ReadFlags::CREATE | ReadFlags::MOVED_TO) {
            return Some(Kind::Created);
        }
        if mask.contains(ReadFlags::ATTRIB) {
            return Some(Kind::Metadata);
        }
        None
    }

    /// A live inotify watch set (plan §5.5's `Watcher`).
    ///
    /// Owns one descriptor and one thread; [`Self::stop`] (or `Drop`) joins the thread, so no thread can
    /// outlive the value — the assertion `dropping_the_watcher_leaves_no_thread_running` makes.
    #[derive(Debug)]
    pub struct Watcher {
        /// The inotify fd; dropping the last `Arc` closes it, which is what releases the watches.
        fd: Arc<OwnedFd>,
        /// wd → the directory it watches. inotify gives no path back, so this map *is* the watch set.
        watches: Arc<Mutex<HashMap<i32, AbsPath>>>,
        /// Roots that could not be watched, and why (plan §5.5's `roots_failed` principle: a dead mount
        /// is data, not a failed start — unless *every* root failed, which is an error).
        failed: Arc<Mutex<Vec<(AbsPath, String)>>>,
        /// Directories that appeared after the budget was exhausted, and so are deliberately unwatched.
        unwatched: Arc<Mutex<Vec<AbsPath>>>,
        stop: Arc<AtomicBool>,
        /// The [`WatchConfig::max_watches`] this watcher was started with, kept for [`Self::add_root`].
        budget: usize,
        /// Threads currently inside the reader loop (1 from `start_with` until the reader exits, 0 after).
        live: Arc<AtomicUsize>,
        shared: Arc<Mutex<Coalescer>>,
        worker: Mutex<Option<JoinHandle<()>>>,
    }

    impl Drop for Watcher {
        fn drop(&mut self) {
            let _ = self.shutdown();
        }
    }

    impl Watcher {
        /// Watch every root and start the reader thread, delivering coalesced events to `tx`.
        ///
        /// Every root must be an existing directory; a root that cannot be watched is recorded in
        /// [`Self::failed_roots`] and skipped, and the call fails only when **none** of them could be
        /// watched — a session whose whole root set is unmounted has nothing to watch, and returning
        /// `Ok` would be a lie.
        ///
        /// # Errors
        /// [`CoreError::Watcher`] when inotify cannot be initialised, when the root set exceeds
        /// [`WatchConfig::max_watches`] or none of it can be watched, or when the reader thread cannot be
        /// spawned.
        pub fn start_with(
            roots: &[AbsPath],
            cfg: &WatchConfig,
            tx: Sender<DirChanged>,
        ) -> Result<Self, CoreError> {
            if roots.len() > cfg.max_watches {
                return Err(CoreError::Watcher {
                    msg: format!(
                        "{} roots exceed the {}-watch budget",
                        roots.len(),
                        cfg.max_watches
                    ),
                });
            }
            let fd = Arc::new(
                inotify::init(CreateFlags::NONBLOCK | CreateFlags::CLOEXEC).map_err(io_err)?,
            );
            let watches: Arc<Mutex<HashMap<i32, AbsPath>>> = Arc::new(Mutex::new(HashMap::new()));
            let failed: Arc<Mutex<Vec<(AbsPath, String)>>> = Arc::new(Mutex::new(Vec::new()));
            {
                let mut map = lock(&watches);
                for root in roots {
                    match inotify::add_watch(fd.as_fd(), root.as_path(), WATCH_MASK) {
                        // Two spellings of one directory return one wd (rustix's own note); `insert` of
                        // the second name is then a harmless overwrite, since the events are identical.
                        Ok(wd) => {
                            map.insert(wd, root.clone());
                        }
                        Err(e) => lock(&failed).push((root.clone(), e.to_string())),
                    }
                }
            }
            if lock(&watches).is_empty() {
                let why = lock(&failed)
                    .first()
                    .map(|(_, msg)| msg.clone())
                    .unwrap_or_else(|| "no roots were given to watch".to_string());
                return Err(CoreError::Watcher { msg: why });
            }
            let stop = Arc::new(AtomicBool::new(false));
            // Liveness is published here, not by the worker: `Session::watch_is_running` is the surface a
            // dialog reads immediately after `watch_start`, and a thread that has not been scheduled yet
            // must not make a live watcher look idle. The reader clears it on its way out, so `false`
            // still means "joined or exiting".
            let live = Arc::new(AtomicUsize::new(1));
            let shared = Arc::new(Mutex::new(Coalescer::new(cfg.window)));
            let unwatched = Arc::new(Mutex::new(Vec::new()));
            let worker = Worker {
                fd: Arc::clone(&fd),
                watches: Arc::clone(&watches),
                unwatched: Arc::clone(&unwatched),
                stop: Arc::clone(&stop),
                live: Arc::clone(&live),
                shared: Arc::clone(&shared),
                max_watches: cfg.max_watches,
                tx,
            };
            let handle = std::thread::Builder::new()
                .name("zenman-watcher".to_string())
                .spawn(move || reader(worker))
                .map_err(|e| CoreError::Watcher {
                    msg: format!("cannot start the watcher thread: {e}"),
                })?;
            Ok(Self {
                fd,
                watches,
                failed,
                unwatched,
                stop,
                budget: cfg.max_watches,
                live,
                shared,
                worker: Mutex::new(Some(handle)),
            })
        }

        /// The roots that could not be watched, and why.
        #[must_use]
        pub fn failed_roots(&self) -> Vec<(AbsPath, String)> {
            lock(&self.failed).clone()
        }

        /// Directories that appeared inside the tree but were not added because the watch budget was
        /// exhausted — the honest half of the descriptor story.
        #[must_use]
        pub fn unwatched(&self) -> Vec<AbsPath> {
            lock(&self.unwatched).clone()
        }

        /// How many directories are watched right now (the descriptor spend, roots included).
        #[must_use]
        pub fn watch_count(&self) -> usize {
            lock(&self.watches).len()
        }

        /// Watch one more directory while running — the seam step 12c's SearchManager needs when the
        /// user adds an index root, and the reason this type holds the descriptor rather than lending it.
        ///
        /// # Errors
        /// [`CoreError::Watcher`] when the budget is spent or the kernel refuses the path (it is not a
        /// directory, it disappeared, inotify is out of resources).
        pub fn add_root(&mut self, root: &AbsPath) -> Result<(), CoreError> {
            let mut map = lock(&self.watches);
            if map.len() >= self.budget {
                return Err(CoreError::Watcher {
                    msg: format!(
                        "{} watches already hold the {}-watch budget",
                        map.len(),
                        self.budget
                    ),
                });
            }
            let wd =
                inotify::add_watch(self.fd.as_fd(), root.as_path(), WATCH_MASK).map_err(io_err)?;
            map.entry(wd).or_insert_with(|| root.clone());
            Ok(())
        }

        /// `true` while the reader thread is inside its loop.
        #[must_use]
        pub fn is_running(&self) -> bool {
            self.live.load(Ordering::Acquire) > 0
        }

        /// Stop the reader thread (joined, so nothing is in flight afterwards) and return every event
        /// that had not yet aged out of its coalescing window.
        #[must_use]
        pub fn stop(self) -> Vec<DirChanged> {
            let flushed = self.shutdown();
            drop(self);
            flushed
        }

        /// Set the flag, join the worker, then flush — in that order, so a late `record` cannot land
        /// behind the flush.
        fn shutdown(&self) -> Vec<DirChanged> {
            self.stop.store(true, Ordering::Release);
            let handle = match self.worker.lock() {
                Ok(mut slot) => slot.take(),
                Err(poisoned) => poisoned.into_inner().take(),
            };
            if let Some(handle) = handle {
                // The loop leaves within one `POLL_TICK`; a failed join means the worker panicked, and a
                // destructor can do nothing better than not make that worse.
                let _ = handle.join();
            }
            lock(&self.shared).take_all()
        }
    }

    /// The plan's `start(roots, tx)`, with the budget/window pair the plan leaves implicit.
    ///
    /// # Errors
    /// As [`Watcher::start_with`].
    pub fn start(
        roots: &[AbsPath],
        cfg: &WatchConfig,
        tx: Sender<DirChanged>,
    ) -> Result<Watcher, CoreError> {
        Watcher::start_with(roots, cfg, tx)
    }

    /// Everything the reader thread needs, as one struct so the spawn closure moves names and not nine
    /// captures.
    struct Worker {
        fd: Arc<OwnedFd>,
        watches: Arc<Mutex<HashMap<i32, AbsPath>>>,
        unwatched: Arc<Mutex<Vec<AbsPath>>>,
        stop: Arc<AtomicBool>,
        live: Arc<AtomicUsize>,
        shared: Arc<Mutex<Coalescer>>,
        max_watches: usize,
        tx: Sender<DirChanged>,
    }

    fn reader(args: Worker) {
        let Worker {
            fd,
            watches,
            unwatched,
            stop,
            live,
            shared,
            max_watches,
            tx,
        } = args;
        let mut buf = [MaybeUninit::<u8>::uninit(); READ_BUF];
        loop {
            if stop.load(Ordering::Acquire) {
                break;
            }
            {
                // One `Reader` per tick: it borrows the buffer, and that borrow must end before the send
                // below, which can block on a receiver the session is not draining this frame.
                let mut reader = inotify::Reader::new(fd.as_fd(), &mut buf);
                loop {
                    match reader.next() {
                        Ok(event) => on_event(
                            event.wd(),
                            event.events(),
                            event.file_name(),
                            &fd,
                            &watches,
                            &unwatched,
                            &shared,
                            max_watches,
                        ),
                        // The fd is NONBLOCK: "nothing more right now" is the normal exit.
                        Err(err) if err == Errno::WOULDBLOCK => break,
                        // Anything else (EBADF, EFAULT) means this watcher is broken; stop rather than
                        // spin. The session learns through the events it stops receiving.
                        Err(_) => {
                            live.fetch_sub(1, Ordering::AcqRel);
                            return;
                        }
                    }
                }
            }
            let due = lock(&shared).take_due(Instant::now());
            for event in due {
                if tx.send(event).is_err() {
                    // The receiver is gone: the session stopped, so the watcher is orphaned.
                    live.fetch_sub(1, Ordering::AcqRel);
                    return;
                }
            }
            std::thread::sleep(POLL_TICK);
        }
        live.fetch_sub(1, Ordering::AcqRel);
    }

    #[allow(clippy::too_many_arguments)]
    fn on_event(
        wd: i32,
        mask: ReadFlags,
        name: Option<&rustix::ffi::CStr>,
        fd: &OwnedFd,
        watches: &Arc<Mutex<HashMap<i32, AbsPath>>>,
        unwatched: &Arc<Mutex<Vec<AbsPath>>>,
        shared: &Arc<Mutex<Coalescer>>,
        max_watches: usize,
    ) {
        // `IN_IGNORED` is the kernel saying the watch is gone (the directory was deleted, or the
        // descriptor was removed); keeping a stale wd would make every later event resolve against a
        // path that no longer exists.
        if mask.contains(ReadFlags::IGNORED) {
            lock(watches).remove(&wd);
        }
        if mask.contains(ReadFlags::QUEUE_OVERFLOW) {
            // The kernel queue overflowed and events were lost. There is no path to name; the recovery
            // is the session's own `is_dirty`/rebuild path, not a fabricated event.
            return;
        }
        let base = match lock(watches).get(&wd).cloned() {
            Some(base) => base,
            None => return,
        };
        let path = match name {
            // No name ⇒ the event is about the watched directory itself (`IN_DELETE_SELF`,
            // `IN_ATTRIB` on the dir, `IN_IGNORED`).
            None => base,
            Some(raw) => {
                let os = std::ffi::OsStr::from_bytes(raw.to_bytes());
                match AbsPath::from_path(base.as_path().join(os)) {
                    Ok(path) => path,
                    // A name inotify handed us that `AbsPath` refuses (a `.` component, a path that
                    // cannot survive validation) is dropped, not guessed at.
                    Err(_) => return,
                }
            }
        };
        // Subdirectory handling — Go's shipped sketch never did this (it watched one directory), and
        // `notify`'s recursive adapters do it the same way: a new directory inside the tree becomes
        // watched, within the budget.
        if mask.contains(ReadFlags::CREATE) && mask.contains(ReadFlags::ISDIR) {
            let mut map = lock(watches);
            if map.len() < max_watches {
                if let Ok(new_wd) = inotify::add_watch(fd.as_fd(), path.as_path(), WATCH_MASK) {
                    map.entry(new_wd).or_insert_with(|| path.clone());
                }
            } else {
                drop(map);
                lock(unwatched).push(path.clone());
            }
        }
        let Some(kind) = classify_kind(mask) else {
            return;
        };
        let event = match kind {
            Kind::Created => DirChanged::Created { path },
            Kind::Removed => DirChanged::Removed { path },
            Kind::Metadata => DirChanged::Metadata { path },
        };
        lock(shared).record(event, Instant::now());
    }

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn io_err(e: Errno) -> CoreError {
        CoreError::Watcher { msg: e.to_string() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abs(p: &str) -> AbsPath {
        AbsPath::from_path(p).expect("absolute")
    }

    #[test]
    fn a_burst_on_one_path_is_one_event_after_the_window() {
        let t0 = Instant::now();
        let mut c = Coalescer::new(COALESCE_WINDOW);
        for i in 0..20 {
            let fresh = c.record(
                DirChanged::Created {
                    path: abs("/root/new.txt"),
                },
                t0 + Duration::from_millis(i * 5),
            );
            assert_eq!(fresh, i == 0, "only the first event opens a window");
        }
        assert_eq!(c.len(), 1, "twenty writes are one pending path");
        // Trailing-edge: the last of the twenty arrived at t0 + 95 ms, so the window closes at 295 ms.
        assert!(c.take_due(t0 + Duration::from_millis(294)).is_empty());
        let due = c.take_due(t0 + Duration::from_millis(295));
        assert_eq!(
            due,
            vec![DirChanged::Created {
                path: abs("/root/new.txt")
            }],
            "the boundary is inclusive: exactly one quiet period after the last event is due"
        );
        assert!(c.is_empty());
        assert!(c.take_due(t0 + Duration::from_secs(9)).is_empty());
    }

    #[test]
    fn distinct_paths_each_get_their_own_event() {
        let t0 = Instant::now();
        let mut c = Coalescer::new(COALESCE_WINDOW);
        for name in ["a", "b", "c"] {
            c.record(
                DirChanged::Created {
                    path: abs(&format!("/root/{name}")),
                },
                t0,
            );
        }
        assert_eq!(c.len(), 3);
        let due = c.take_due(t0 + COALESCE_WINDOW);
        assert_eq!(due.len(), 3, "three paths, three events");
        let paths: Vec<String> = due.iter().map(|e| e.path().to_string_lossy()).collect();
        assert_eq!(
            paths,
            vec!["/root/a", "/root/b", "/root/c"],
            "the drain is in path order, so a test can name the events"
        );
    }

    #[test]
    fn an_event_inside_the_window_restarts_it() {
        let t0 = Instant::now();
        let mut c = Coalescer::new(COALESCE_WINDOW);
        c.record(DirChanged::Created { path: abs("/f") }, t0);
        assert_eq!(c.next_due(), Some(t0 + COALESCE_WINDOW));
        c.record(
            DirChanged::Created { path: abs("/f") },
            t0 + Duration::from_millis(150),
        );
        assert_eq!(c.next_due(), Some(t0 + Duration::from_millis(350)));
        assert!(c.take_due(t0 + Duration::from_millis(300)).is_empty());
        assert_eq!(c.take_due(t0 + Duration::from_millis(350)).len(), 1);
    }

    #[test]
    fn merging_takes_the_end_state_and_metadata_is_the_weakest_signal() {
        let created = DirChanged::Created { path: abs("/f") };
        let removed = DirChanged::Removed { path: abs("/f") };
        let meta = DirChanged::Metadata { path: abs("/f") };
        assert_eq!(DirChanged::merge(&created, &removed), removed);
        assert_eq!(DirChanged::merge(&removed, &created), created);
        assert_eq!(DirChanged::merge(&created, &meta), created);
        assert_eq!(DirChanged::merge(&meta, &created), created);
        assert_eq!(DirChanged::merge(&meta, &meta), meta);
        assert_eq!(created.kind(), "created");
        assert_eq!(removed.kind(), "removed");
        // A write/delete burst inside one window must end as a delete: the row has to go.
        let t0 = Instant::now();
        let mut c = Coalescer::new(COALESCE_WINDOW);
        c.record(created.clone(), t0);
        c.record(meta.clone(), t0 + Duration::from_millis(10));
        c.record(removed.clone(), t0 + Duration::from_millis(20));
        // The window restarted with the last event, so it closes at t0 + 20 ms + the quiet period.
        assert_eq!(
            c.take_due(t0 + Duration::from_millis(20) + COALESCE_WINDOW),
            vec![removed]
        );
    }

    #[test]
    fn take_all_flushes_regardless_of_the_deadline() {
        let t0 = Instant::now();
        let mut c = Coalescer::new(COALESCE_WINDOW);
        c.record(DirChanged::Metadata { path: abs("/a") }, t0);
        c.record(DirChanged::Removed { path: abs("/b") }, t0);
        assert!(c.take_due(t0).is_empty());
        assert_eq!(c.take_all().len(), 2);
        assert!(c.is_empty(), "the flush empties the map");
    }

    #[test]
    fn the_window_and_budget_come_from_limits() {
        let cfg = WatchConfig::from_limits(&Limits::default());
        assert_eq!(cfg.window, COALESCE_WINDOW);
        assert_eq!(cfg.max_watches, crate::limits::WATCH_MAX_WATCHES);
        assert_eq!(WatchConfig::default(), cfg);
    }

    #[test]
    #[cfg(not(all(feature = "watch", target_os = "linux")))]
    fn the_unavailable_backend_reports_the_typed_error() {
        use crate::error::CoreError;

        // With the feature off (and on every non-Linux target) this is the only path, and it must be a
        // typed `CoreError::Watcher`, not a panic and not an `Ok` that watches nothing.
        let (tx, _rx) = std::sync::mpsc::channel();
        let err = start(&[abs("/root")], &WatchConfig::default(), tx)
            .expect_err("the unavailable backend reports an error");
        assert!(matches!(err, CoreError::Watcher { .. }), "{err:?}");
        assert!(err.user_message().contains("watcher"), "{err:?}");
        assert!(!err.user_message().is_empty());
    }

    #[cfg(all(feature = "watch", target_os = "linux"))]
    mod linux {
        use std::fs;
        use std::path::{Path, PathBuf};
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        use rustix::fs::inotify::ReadFlags;

        use crate::error::CoreError;
        use crate::events::{ChangeCause, EngineEvent};
        use crate::index::intent::{IndexIntent, IndexOp, RecordingIntent};
        use crate::index::watcher::{COALESCE_WINDOW, Kind, WatchConfig, classify_kind, start};
        use crate::limits::Limits;
        use crate::path::AbsPath;
        use crate::session::{Session, SessionPaths};
        use crate::store::config::ZenConfig;

        fn abs(p: &str) -> AbsPath {
            AbsPath::from_path(p).expect("absolute")
        }

        #[test]
        fn masks_map_onto_the_three_events() {
            let cases: &[(ReadFlags, Option<Kind>)] = &[
                (ReadFlags::CREATE, Some(Kind::Created)),
                (ReadFlags::MOVED_TO, Some(Kind::Created)),
                (ReadFlags::CREATE | ReadFlags::ISDIR, Some(Kind::Created)),
                (ReadFlags::DELETE, Some(Kind::Removed)),
                (ReadFlags::DELETE_SELF, Some(Kind::Removed)),
                (ReadFlags::MOVED_FROM, Some(Kind::Removed)),
                (ReadFlags::ATTRIB, Some(Kind::Metadata)),
                (ReadFlags::ATTRIB | ReadFlags::ISDIR, Some(Kind::Metadata)),
                // A mask carrying both resolves to the state a writer can act on safely.
                (ReadFlags::CREATE | ReadFlags::DELETE, Some(Kind::Removed)),
                // Deliberately silent, each for the reason the module docs give.
                (ReadFlags::MODIFY, None),
                (ReadFlags::ACCESS, None),
                (ReadFlags::OPEN, None),
                (ReadFlags::CLOSE_WRITE, None),
                (ReadFlags::CLOSE_NOWRITE, None),
                (ReadFlags::MOVE_SELF, None),
                (ReadFlags::IGNORED, None),
                (ReadFlags::UNMOUNT, None),
                (ReadFlags::QUEUE_OVERFLOW, None),
                (ReadFlags::empty(), None),
            ];
            for (mask, want) in cases {
                assert_eq!(classify_kind(*mask), *want, "mask {mask:?}");
            }
        }

        #[test]
        fn a_created_file_is_one_coalesced_event_and_one_index_op() {
            // The plan §5.5 acceptance line, on the real filesystem: a created file in a watched dir
            // emits exactly one coalesced event — and, per §9.2 step-12a note (2), exactly one op into
            // the write path's own intent.
            let dir = scratch("inotify");
            let watched = dir.join("tree");
            fs::create_dir_all(&watched).expect("watched dir");
            let mut session = session_in(&dir);
            let recorder = Arc::new(RecordingIntent::new());
            session.set_intent(Arc::clone(&recorder) as Arc<dyn IndexIntent>);
            session
                .watch_start_at(&[AbsPath::from_path(watched.clone()).expect("abs")])
                .expect("a real temp dir is watchable");
            // Drain the events the scratch tree's own creation produced.
            let _ = pump(&mut session);

            fs::write(watched.join("new.txt"), b"hi").expect("create a file");
            let events = wait_for(&mut session, |seen| seen.iter().any(is_dir_changed));
            let changed: Vec<&EngineEvent> = events.iter().filter(|e| is_dir_changed(e)).collect();
            assert_eq!(
                changed.len(),
                1,
                "one created file is one coalesced DirChanged, got {changed:?}"
            );
            match changed.first() {
                Some(EngineEvent::DirChanged { path, cause, .. }) => {
                    assert_eq!(
                        *cause,
                        ChangeCause::External,
                        "an out-of-process change is not a local job"
                    );
                    assert_eq!(
                        path.host_abs().map(|p| p.to_string_lossy()),
                        Some(watched.display().to_string()),
                        "the pane that must reload is the directory, not the file"
                    );
                }
                other => panic!("expected a DirChanged, got {other:?}"),
            }
            let ops = recorder.recorded();
            assert_eq!(
                ops.len(),
                1,
                "the watcher feeds the write path's intent, once: {ops:?}"
            );
            match ops.first() {
                Some(IndexOp::Created { path }) => assert_eq!(
                    path.as_path(),
                    watched.join("new.txt").as_path(),
                    "the op names the new file"
                ),
                other => panic!("expected one Created op, got {other:?}"),
            }

            // A removal arrives as Removed, and the op is the subtree-safe prune.
            let before = recorder.len();
            fs::remove_file(watched.join("new.txt")).expect("remove the file");
            let removed = wait_for(&mut session, |seen| {
                // Both halves, in one condition: `pump_watcher` records the index op immediately, but the
                // pane event goes through the dir cache's own coalescing window, so the op lands first.
                seen.iter().any(is_dir_changed) && recorder.len() > before
            });
            assert!(
                removed.iter().any(is_dir_changed),
                "the removal invalidates the directory too"
            );
            let ops = recorder.recorded();
            assert_eq!(ops.len(), before + 1, "one removal is one op");
            assert!(
                matches!(ops.last(), Some(IndexOp::Pruned { .. })),
                "a removed path is a prune (a directory's subtree has to go with it): {ops:?}"
            );

            assert!(
                session.watch_is_running(),
                "the worker is live until stopped"
            );
            session.watch_stop();
            assert!(
                !session.watch_is_running(),
                "watch_stop joins the reader thread — no thread may outlive it"
            );
        }

        #[test]
        fn dropping_the_watcher_leaves_no_thread_running() {
            let dir = scratch("drop");
            let watched = dir.join("tree");
            fs::create_dir_all(&watched).expect("watched dir");
            let mut session = session_in(&dir);
            session
                .watch_start_at(&[AbsPath::from_path(&watched).expect("abs")])
                .expect("watchable");
            let _ = pump(&mut session);
            assert!(session.watch_is_running());
            // `watch_stop` returns whatever had not yet aged out of its window; the content is timing-
            // dependent (a stray `IN_ATTRIB` on the scratch tree is legitimate), so the assertion this
            // test is about is the thread, below.
            let _ = session.watch_stop();
            assert!(!session.watch_is_running());
            session
                .watch_start_at(&[AbsPath::from_path(watched).expect("abs")])
                .expect("re-watchable");
            assert!(session.watch_is_running());
            let _ = session.watch_stop();
            assert!(!session.watch_is_running(), "and stopped again");
        }

        #[test]
        fn an_over_budget_root_set_is_a_typed_error() {
            let cfg = WatchConfig::from_limits(&Limits::default());
            let mut roots = Vec::new();
            for i in 0..cfg.max_watches + 1 {
                roots.push(abs(&format!("/root/{i}")));
            }
            let (tx, _rx) = std::sync::mpsc::channel();
            let err = start(&roots, &cfg, tx).expect_err("the descriptor budget is enforced");
            assert!(matches!(err, CoreError::Watcher { .. }), "{err:?}");
            assert!(err.user_message().contains("budget"), "{err:?}");

            // And a root set that cannot be watched at all is the same typed error, never `Ok`.
            let dir = scratch("dead-root");
            let (tx2, _rx2) = std::sync::mpsc::channel();
            let err = start(
                &[AbsPath::from_path(dir.join("nope")).expect("abs")],
                &cfg,
                tx2,
            )
            .expect_err("a missing root is refused");
            assert!(matches!(err, CoreError::Watcher { .. }), "{err:?}");
        }

        #[test]
        fn a_subdirectory_becomes_watched_within_the_budget() {
            let dir = scratch("recursive");
            let watched = dir.join("tree");
            fs::create_dir_all(&watched).expect("watched dir");
            let mut session = session_in(&dir);
            let recorder = Arc::new(RecordingIntent::new());
            session.set_intent(Arc::clone(&recorder) as Arc<dyn IndexIntent>);
            let started = session
                .watch_start_at(&[AbsPath::from_path(&watched).expect("abs")])
                .expect("watchable");
            assert_eq!(started, 1, "one root, one watch");
            let _ = pump(&mut session);

            fs::create_dir(watched.join("sub")).expect("mkdir inside the tree");
            let _ = wait_for(&mut session, |_| false);
            assert_eq!(
                session.watch_count(),
                2,
                "the new directory is watched too (Go's sketch never did this; notify's adapters do)"
            );
            let before = recorder.len();
            fs::write(watched.join("sub").join("deep.txt"), b"x")
                .expect("write inside the new dir");
            wait_for(&mut session, |_| recorder.len() > before);
            assert!(
                recorder.recorded().iter().any(|op| {
                    matches!(op, IndexOp::Created { path }
                        if path.to_string_lossy().ends_with("sub/deep.txt"))
                }),
                "an event inside a directory created after the start is delivered: {:?}",
                recorder.recorded()
            );
            session.watch_stop();
        }

        fn is_dir_changed(ev: &EngineEvent) -> bool {
            matches!(ev, EngineEvent::DirChanged { .. })
        }

        /// Poll for one quiet period plus a few ticks, and hand back whatever came out. Used to drain
        /// the events the *test's own* `mkdir` produced before the interesting part starts.
        fn pump(session: &mut Session) -> Vec<EngineEvent> {
            let until = Instant::now() + COALESCE_WINDOW + Duration::from_millis(100);
            let mut seen = Vec::new();
            loop {
                seen.extend(session.poll());
                if Instant::now() > until {
                    return seen;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        /// Pump the session until `done` is satisfied (or 5 s pass). The watcher runs on the real clock,
        /// so this test cannot use fake time — the same reason `settle_until` exists in `zenman-ui`.
        fn wait_for(
            session: &mut Session,
            done: impl Fn(&[EngineEvent]) -> bool,
        ) -> Vec<EngineEvent> {
            let deadline = Instant::now() + Duration::from_millis(5_000);
            let mut seen = Vec::new();
            loop {
                seen.extend(session.poll());
                if done(&seen) || Instant::now() > deadline {
                    return seen;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn session_in(dir: &Path) -> Session {
            fs::create_dir_all(dir.join("probe")).expect("probe dir");
            let paths = SessionPaths::new(
                dir.join("config.json"),
                dir.join("action.log"),
                Some(dir.join("zen-man.db")),
                dir.join("probe"),
            )
            .with_db_open(1, Duration::ZERO);
            let (session, _notes) = Session::open(
                paths,
                ZenConfig::default(),
                Arc::new(crate::platform::Unsupported::new()),
                Limits::default(),
            )
            .expect("session");
            session
        }

        fn scratch(tag: &str) -> PathBuf {
            let root =
                std::env::temp_dir().join(format!("zenman-watcher-{tag}-{}", std::process::id()));
            cleanup(&root);
            fs::create_dir_all(&root).expect("scratch dir");
            root
        }

        /// Remove a scratch tree. `std::fs::remove_dir_all` is banned workspace-wide (`clippy.toml`:
        /// deletion goes through `ops::delete`), so the walk is spelled out here — these trees are the
        /// test's own, a level or two deep, and a leftover is cleared by the next run.
        fn cleanup(dir: &Path) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    cleanup(&path);
                } else {
                    let _ = fs::remove_file(path);
                }
            }
            let _ = fs::remove_dir(dir);
        }

        #[test]
        fn the_gated_default_window_is_the_plan_number() {
            // The plan's number, in one place, asserted from the gated half too so a `--features watch`
            // run pins it as well as the default one.
            assert_eq!(COALESCE_WINDOW, Duration::from_millis(200));
            assert_eq!(WatchConfig::default().window, COALESCE_WINDOW);
        }
    }
}
