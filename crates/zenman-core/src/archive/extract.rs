//! Extraction: the only archive path that writes (plan §5.5's `archive/extract.rs` row).
//!
//! Three rules, each with a contract test behind it:
//!
//! * **zip-slip is refused before anything is written.** Every member target is resolved with
//!   [`crate::ops::guard::safe_join`] in a *planning* pass; a container that contains `../escape.txt`
//!   therefore produces an error and an empty destination, not a half-extracted tree with one file in
//!   `/etc`. Go checked too, but per member and with a substring rule that both over- and under-matched
//!   (`api/archive_extract.go:68-78`, plan F9).
//! * **a collision is a policy, never silence.** The destination is decided per member by
//!   [`ConflictPolicy`], the same enum the paste path uses — Go's `copyFile` skipped existing files and
//!   reported success (plan F12), and its extract handler had no conflict concept at all.
//! * **progress and cancellation ride the job seams.** Counters go through
//!   [`Progress`](crate::jobs::Progress) (throttled) and the walk checks
//!   [`CancelToken::check`] per member; the plan's `ExtractState` behind a mutex is the *status* view the
//!   dialog polls, and every mutation of it is under that mutex (Go wrote `job.Error` from the worker
//!   while the status handler read it unlocked — a real data race, `api/archive_extract.go:22-33`).
//!
//! Symlink members are written as ordinary files containing their target text — Go's behaviour
//! (`tests/golden/archive-list-symlink.json`) and the safe one: materialising the link is what lets a
//! later member escape through it, and [`safe_join`](crate::ops::guard::safe_join) refuses to walk
//! through one that already exists anyway.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::archive::vfs::{Manifest, ManifestCache, clean_internal_path};
use crate::error::CoreError;
use crate::events::{ExtractSummary, JobPayload};
use crate::jobs::{CancelToken, JobId, JobKind, JobRegistry, JobResult, JobSpec, Progress};
use crate::limits::Limits;
use crate::ops::ConflictDecision;
use crate::ops::ConflictPolicy;
use crate::path::{AbsPath, FsPath, VfsKind};

/// What the service calls with the finished report, on the worker thread, before the job's terminal
/// event. [`Session::archive_extract`] passes one that emits `DirChanged` for the destination and notes
/// the extracted files with the index intent — the only paths an *archive* may ever touch the index,
/// because they are real files (see the written decision in `services/archive.rs`).
pub type DoneHook = Option<Arc<dyn Fn(ExtractReport) + Send + Sync>>;

/// The status snapshot the dialog reads (plan §5.5's `ExtractState`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtractState {
    /// The worker has finished, with or without an error.
    pub done: bool,
    /// Uncompressed bytes written so far.
    pub bytes_done: u64,
    /// Uncompressed bytes selected for extraction (the manifest's own numbers).
    pub bytes_total: u64,
    /// Rendered failure text, `Some` exactly when the job failed.
    pub error: Option<String>,
}

/// One extraction, as the status endpoint knows it. `id` is the **job** id, so the jobs pane and the
/// dialog agree on what a job is.
#[derive(Debug)]
pub struct ExtractJob {
    /// The job this extraction runs as.
    pub id: JobId,
    /// Where it writes.
    pub dest: AbsPath,
    /// The plan's mutex-guarded state.
    state: Mutex<ExtractState>,
    /// Set when the worker returns; the `gc` window starts here.
    finished_at: Mutex<Option<Instant>>,
    /// The finished report, `Some` from the moment the walk returns. The dialog needs the skipped list
    /// after the job is gone (a toast that says "3 files skipped" cannot re-derive it).
    report: Mutex<Option<ExtractReport>>,
}

impl ExtractJob {
    /// A snapshot of the progress counters.
    #[must_use]
    pub fn state(&self) -> ExtractState {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn update(&self, f: impl FnOnce(&mut ExtractState)) {
        if let Ok(mut g) = self.state.lock() {
            f(&mut g);
        }
    }

    /// The report of a finished extraction.
    #[must_use]
    pub fn report(&self) -> Option<ExtractReport> {
        self.report
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_report(&self, report: &ExtractReport) {
        if let Ok(mut g) = self.report.lock() {
            *g = Some(report.clone());
        }
    }

    fn mark_finished(&self, error: Option<String>) {
        self.update(|s| {
            s.done = true;
            s.error = error;
        });
        if let Ok(mut g) = self.finished_at.lock() {
            *g = Some(Instant::now());
        }
    }

    /// `true` once the worker returned, and for how long ago.
    #[must_use]
    pub fn finished_since(&self, now: Instant) -> Option<Duration> {
        self.finished_at
            .lock()
            .ok()
            .and_then(|g| *g)
            .map(|at| now.saturating_duration_since(at))
    }
}

/// What to extract, from where, to where, and what to do about a collision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractReq {
    /// The archive file.
    pub host: AbsPath,
    /// The inner archive when the source is a nested store (plan §5.5's `nested`).
    pub nested: Option<String>,
    /// Members to extract; empty means the whole archive (Go's `sources: []` convention,
    /// `api/archive_extract.go:96`).
    pub sources: Vec<String>,
    /// The destination directory, which must be a real filesystem path (`dest_guard`).
    pub dest: AbsPath,
    /// What to do when the destination already holds a member's name.
    pub policy: ConflictPolicy,
}

impl ExtractReq {
    /// The request for "extract this store, everything in it".
    #[must_use]
    pub fn all(host: AbsPath, nested: Option<String>, dest: AbsPath) -> Self {
        Self {
            host,
            nested,
            sources: Vec::new(),
            dest,
            policy: ConflictPolicy::DEFAULT,
        }
    }

    /// The store this request reads from.
    #[must_use]
    pub fn store(&self) -> VfsKind {
        VfsKind::Archive {
            host: self.host.clone(),
            nested: self.nested.clone(),
        }
    }
}

/// What an extraction did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtractReport {
    /// Files written, in the order they were written.
    pub written: Vec<AbsPath>,
    /// Members the [`ConflictPolicy`](crate::ops::ConflictPolicy) refused to clobber.
    pub skipped: Vec<String>,
    /// Members renamed on collision (`policy == Rename`).
    pub renamed: Vec<(String, AbsPath)>,
    /// Symlink members materialised as ordinary files (Go parity; see the module docs).
    pub symlinks: u64,
    /// Directories created.
    pub dirs: u64,
    /// Uncompressed bytes written.
    pub bytes: u64,
}

impl From<ExtractReport> for ExtractSummary {
    fn from(r: ExtractReport) -> Self {
        Self {
            files: u64::try_from(r.written.len()).unwrap_or(u64::MAX),
            bytes: r.bytes,
            dest: r.written.first().and_then(|p| p.parent()),
        }
    }
}

impl From<ExtractReport> for JobPayload {
    fn from(r: ExtractReport) -> Self {
        Self::Extract(r.into())
    }
}

/// The progress seam [`extract_all`] reports through: the job's throttled
/// [`Progress`](crate::jobs::Progress) in production, a silent one in tests.
pub trait ProgressSink: Sync {
    /// Add counters and name the current member.
    fn add(&self, bytes: u64, files: u64, current: Option<String>);
}

impl ProgressSink for Progress {
    fn add(&self, bytes: u64, files: u64, current: Option<String>) {
        Progress::add(self, bytes, files, current);
    }
}

/// The sink that throws counters away.
pub struct NoopProgress;

impl ProgressSink for NoopProgress {
    fn add(&self, _bytes: u64, _files: u64, _current: Option<String>) {}
}

/// The plan's `Ask | Merge | Rename` mapping onto the one policy enum the write path already has.
///
/// `Ask` is the UI's business, not the engine's: by the time a request reaches here the dialog has
/// answered, and the answer *is* a policy. `Merge` is what a directory-onto-directory does under every
/// policy ([`ConflictDecision::Merge`]), so it needs no variant of its own — which is why there is no
/// second extract enum in this crate.
#[must_use]
pub const fn policy_of(ask_rename: bool) -> ConflictPolicy {
    if ask_rename {
        ConflictPolicy::Rename
    } else {
        ConflictPolicy::SkipExisting
    }
}

/// One planned member: where it goes, and what kind it is.
#[derive(Clone, Debug)]
pub struct PlanItem {
    /// The cleaned member path.
    pub member: String,
    /// The resolved destination (already proven to be under `dest`).
    pub target: AbsPath,
    /// Directory member?
    pub is_dir: bool,
    /// Symlink member (written as a file).
    pub is_symlink: bool,
    /// Uncompressed size.
    pub size: u64,
}

/// Resolve every member under `dest`, refusing the whole plan if any one of them escapes.
///
/// This is the pass that makes "extracts to nothing" true: it writes nothing, so a refusal cannot leave
/// a partial tree behind.
///
/// # Errors
/// [`CoreError::InvalidPath`] with [`PathReject::EscapesRoot`](crate::error::PathReject::EscapesRoot)
/// for a member that resolves outside `dest` or through a symlink.
pub fn plan(
    manifest: &Manifest,
    dest: &AbsPath,
    sources: &[String],
) -> Result<Vec<PlanItem>, CoreError> {
    let selected = |path: &str| {
        sources.is_empty()
            || sources.iter().any(|s| {
                let s = clean_internal_path(s);
                path == s || path.starts_with(&format!("{s}/"))
            })
    };
    let mut out = Vec::new();
    for (path, node) in manifest.iter() {
        if !selected(path) {
            continue;
        }
        out.push(PlanItem {
            member: path.clone(),
            target: crate::ops::guard::safe_join(dest, path)?,
            is_dir: node.is_dir,
            is_symlink: false,
            size: u64::try_from(node.size.max(0)).unwrap_or(0),
        });
    }
    Ok(out)
}

/// Extract a planned member list from an opened container.
///
/// `reader` is opened once for the whole job: streaming N members out of one handle is the difference
/// between O(N) seeks and O(N) opens.
///
/// # Errors
/// The first failure (zip-slip refusal, cap, I/O, cancellation), after which nothing further is
/// written.
pub fn extract_all(
    reader: &mut dyn crate::archive::reader::ArchiveReader,
    items: &[PlanItem],
    dest: &AbsPath,
    policy: ConflictPolicy,
    cancel: &CancelToken,
    progress: &dyn ProgressSink,
) -> Result<ExtractReport, CoreError> {
    let total: u64 = items.iter().filter(|i| !i.is_dir).map(|i| i.size).sum();
    let mut report = ExtractReport::default();
    // The destination exists even when the container declares no directory members (`symlink.zip`),
    // because every member target is resolved *under* it.
    std::fs::create_dir_all(dest.as_path()).map_err(core_io)?;

    for item in items {
        cancel.check()?;
        if item.is_dir {
            // A directory onto a directory is a merge under every policy, exactly as in the paste path
            // (`ConflictDecision::Merge`, `api/ops.go:683` `MkdirAll`).
            std::fs::create_dir_all(item.target.as_path()).map_err(core_io)?;
            report.dirs += 1;
            continue;
        }
        let mut target = item.target.clone();
        match policy.decide(false, &target)? {
            ConflictDecision::Skip => {
                report.skipped.push(item.member.clone());
                continue;
            }
            ConflictDecision::Rename(fresh) => {
                report.renamed.push((item.member.clone(), fresh.clone()));
                target = fresh;
            }
            ConflictDecision::Proceed | ConflictDecision::Merge => {}
        }
        let mut sink = LimitedSink {
            path: target.clone(),
            file: None,
        };
        sink.open()?;
        let n = reader.write_stream(&item.member, &mut sink, total.max(item.size))?;
        sink.finish()?;
        report.written.push(target);
        report.bytes += n;
        progress.add(n, 1, Some(item.member.clone()));
    }
    Ok(report)
}

/// An I/O failure keeps the path it was about (the `CoreError::Io` rule, not `?`-defaulting to
/// `System`, because "which file" is the whole message here).
fn core_io(e: std::io::Error) -> CoreError {
    CoreError::System(e)
}

/// The writer `extract_all` streams into: one file per member, created *without* following a symlink at
/// the final component (`symlink_metadata` pre-check + `create_new`-free open is not enough on its own,
/// so the check runs immediately before the open and again after, on the handle's own metadata).
struct LimitedSink {
    path: AbsPath,
    file: Option<std::fs::File>,
}

impl LimitedSink {
    fn open(&mut self) -> Result<(), CoreError> {
        if let Ok(md) = std::fs::symlink_metadata(self.path.as_path())
            && md.file_type().is_symlink()
        {
            return Err(CoreError::InvalidPath {
                raw: self.path.to_string(),
                reason: crate::error::PathReject::EscapesRoot,
            });
        }
        let f = std::fs::File::create(self.path.as_path()).map_err(|e| CoreError::Io {
            path: self.path.to_string(),
            io: Arc::new(e),
        })?;
        self.file = Some(f);
        Ok(())
    }

    fn finish(&mut self) -> Result<(), CoreError> {
        if let Some(f) = &self.file {
            f.sync_all().ok();
        }
        self.file = None;
        Ok(())
    }
}

impl std::io::Write for LimitedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match &mut self.file {
            Some(f) => f.write(buf),
            None => Err(std::io::Error::other("extract sink is closed")),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match &mut self.file {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

/// The plan's three-at-a-time rule as a token channel (a fourth request **queues**, it is not
/// rejected — Go's `extractSem` had exactly that behaviour and no way to observe it).
#[derive(Clone)]
struct Permits {
    tx: Arc<std::sync::mpsc::Sender<()>>,
    // `Receiver` is `Send` but not `Sync`, and the pool is shared by every worker thread.
    rx: Arc<Mutex<std::sync::mpsc::Receiver<()>>>,
}

impl Permits {
    fn new(total: usize) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        for _ in 0..total.max(1) {
            let _ = tx.send(());
        }
        Self {
            tx: Arc::new(tx),
            rx: Arc::new(Mutex::new(rx)),
        }
    }

    /// Block until a slot frees. `None` only if the pool was dropped, i.e. the service is going away;
    /// the caller then runs unthrottled rather than failing the job.
    fn acquire(&self) -> Option<Permit> {
        let got = {
            let guard = self
                .rx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.recv().ok()
        };
        got.map(|()| Permit {
            tx: Arc::clone(&self.tx),
        })
    }
}

struct Permit {
    tx: Arc<std::sync::mpsc::Sender<()>>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let _ = self.tx.send(());
    }
}

/// The extract service: the job table's client, the manifest cache's other user, and the owner of the
/// plan's three-at-a-time rule.
pub struct ExtractService {
    jobs: Arc<JobRegistry>,
    store: Arc<crate::archive::vfs::ArchiveStore>,
    inner: Mutex<BTreeMap<u64, Arc<ExtractJob>>>,
    permits: Arc<Permits>,
    done_ttl: Duration,
    seq: AtomicU64,
}

impl ExtractService {
    /// A service over the session's job table, manifest cache and limits.
    #[must_use]
    pub fn new(
        jobs: Arc<JobRegistry>,
        store: Arc<crate::archive::vfs::ArchiveStore>,
        limits: &Limits,
    ) -> Self {
        Self {
            jobs,
            store,
            inner: Mutex::new(BTreeMap::new()),
            permits: Arc::new(Permits::new(limits.concurrent_extracts)),
            done_ttl: limits.extract_done_ttl,
            seq: AtomicU64::new(0),
        }
    }

    /// The manifest cache the plan's caps are read from.
    #[must_use]
    pub fn manifests(&self) -> &ManifestCache {
        self.store.cache()
    }

    /// Queued or running extractions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.locked().len()
    }

    /// `true` when no extraction has been started.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Arc<ExtractJob>>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Start an extraction. Returns the job id; progress and the terminal event come out of
    /// `Session::poll`, and [`Self::status`] answers the dialog's polling.
    ///
    /// # Errors
    /// `VfsReadOnly`/`InvalidPath` for a bad destination (`dest_guard`), `ArchiveModified` when the
    /// cached manifest is stale, and the open errors — all *before* a thread is spawned, because a
    /// request that can be refused synchronously must not become a job that fails asynchronously.
    pub fn start(
        &self,
        req: &ExtractReq,
        origin: crate::session::Origin,
        on_done: DoneHook,
    ) -> Result<JobId, CoreError> {
        crate::ops::guard::dest_guard(&FsPath::from_abs(req.dest.clone()))?;
        let manifest = self.store.manifest_of(&req.store())?;
        let items = plan(&manifest, &req.dest, &req.sources)?;
        let total: u64 = items.iter().filter(|i| !i.is_dir).map(|i| i.size).sum();

        let spec = JobSpec::new(
            self.jobs.next_id(),
            JobKind::Extract,
            format!(
                "Extract {} ({} members)",
                req.host.to_string_lossy(),
                items.len()
            ),
            origin,
        );
        let id = spec.id;
        let job = Arc::new(ExtractJob {
            id,
            dest: req.dest.clone(),
            state: Mutex::new(ExtractState {
                done: false,
                bytes_done: 0,
                bytes_total: total,
                error: None,
            }),
            finished_at: Mutex::new(None),
            report: Mutex::new(None),
        });
        self.locked().insert(id.as_u64(), Arc::clone(&job));
        self.seq.fetch_add(1, Ordering::Relaxed);

        // The worker owns its inputs outright (`attach` needs a `'static` closure): the plan, the manifest
        // it was planned from, and the request's own fields.
        let host = req.host.clone();
        let nested = req.nested.clone();
        let dest = req.dest.clone();
        let policy = req.policy;
        let limits = Arc::clone(self.store.limits());
        let permits = Arc::clone(&self.permits);
        let manifest = Arc::clone(&manifest);
        let items = Arc::new(items);
        let on_done_hook = on_done.clone();
        self.jobs.attach(spec, move |cancel, progress| {
            // The queue point: the thread exists, and waits here for a slot.
            let _permit = permits.acquire();
            let outcome = run(
                &host,
                nested.as_deref(),
                &manifest,
                &items,
                &job,
                cancel,
                progress,
                &limits,
                policy,
            );
            match outcome {
                Ok(report) => {
                    job.set_report(&report);
                    job.mark_finished(None);
                    let summary =
                        format!("Extracted {} member(s) to {dest}", report.written.len(),);
                    if let Some(hook) = &on_done_hook {
                        hook(report.clone());
                    }
                    Ok(JobResult::done(report, summary))
                }
                Err(e) => {
                    job.mark_finished(Some(e.user_message()));
                    Err(e)
                }
            }
        });
        Ok(id)
    }

    /// The status record for one extraction, `None` once it has been collected.
    #[must_use]
    pub fn status(&self, id: JobId) -> Option<Arc<ExtractJob>> {
        self.locked().get(&id.as_u64()).map(Arc::clone)
    }

    /// Forget finished extractions older than [`Limits::extract_done_ttl`] (Go's 30-minute cleanup
    /// ticker, `api/archive_extract.go:39-52`, turned into a call the session's tick makes).
    pub fn gc(&self) {
        let now = Instant::now();
        let ttl = self.done_ttl;
        self.locked().retain(|_, j| match j.finished_since(now) {
            None => true,
            Some(age) => age < ttl,
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    host: &AbsPath,
    nested: Option<&str>,
    manifest: &Manifest,
    items: &[PlanItem],
    job: &ExtractJob,
    cancel: &CancelToken,
    progress: &Progress,
    limits: &Limits,
    policy: ConflictPolicy,
) -> Result<ExtractReport, CoreError> {
    let _ = manifest;
    let mut reader =
        crate::archive::vfs::open_store(host, nested, limits.nested_archive_max_bytes)?;
    let report = extract_all(reader.as_mut(), items, &job.dest, policy, cancel, progress)?;
    job.update(|s| s.bytes_done = report.bytes);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::reader::RawEntry;
    use crate::archive::vfs::{VirtualNode, build_manifest};

    struct FakeReader;

    impl crate::archive::reader::ArchiveReader for FakeReader {
        fn list(&mut self) -> Result<Vec<RawEntry>, CoreError> {
            Ok(Vec::new())
        }

        fn write_stream(
            &mut self,
            inner: &str,
            sink: &mut dyn std::io::Write,
            _cap: u64,
        ) -> Result<u64, CoreError> {
            let data = format!("body of {inner}");
            sink.write_all(data.as_bytes()).map_err(CoreError::System)?;
            Ok(data.len() as u64)
        }
    }

    fn manifest_of(paths: &[&str]) -> Manifest {
        let entries: Vec<RawEntry> = paths
            .iter()
            .map(|p| RawEntry {
                path: (*p).to_string(),
                is_dir: p.ends_with('/'),
                size: 4,
                mtime: 0,
                mode: None,
                encrypted: false,
            })
            .collect();
        struct R(Vec<RawEntry>);
        impl crate::archive::reader::ArchiveReader for R {
            fn list(&mut self) -> Result<Vec<RawEntry>, CoreError> {
                Ok(self.0.clone())
            }

            fn write_stream(
                &mut self,
                _i: &str,
                _s: &mut dyn std::io::Write,
                _c: u64,
            ) -> Result<u64, CoreError> {
                Ok(0)
            }
        }
        let mut r = R(entries);
        build_manifest(&mut r, &Limits::default()).expect("manifest")
    }

    #[test]
    fn plan_refuses_a_member_that_escapes() {
        let dest = AbsPath::from_path("/tmp/dest").expect("abs");
        let m = manifest_of(&["../evil.txt", "ok.txt"]);
        let err = plan(&m, &dest, &[]).expect_err("the slip member must be refused");
        assert_eq!(err.kind(), "invalid_path");
    }

    #[test]
    fn plan_resolves_a_dot_dot_inside_a_name_the_way_go_s_false_positive_did_not() {
        // `archive-extract-dotdot-falsepositive.json`: `a..b.txt` is a legal name.
        let dest = AbsPath::from_path("/tmp/dest").expect("abs");
        let m = manifest_of(&["a..b.txt", "deep/nested/x.txt"]);
        let items = plan(&m, &dest, &[]).expect("legal names plan");
        assert!(
            items
                .iter()
                .any(|i| i.member == "a..b.txt"
                    && i.target.to_string_lossy() == "/tmp/dest/a..b.txt")
        );
    }

    #[test]
    fn plan_selects_a_subtree_by_source() {
        let dest = AbsPath::from_path("/tmp/dest").expect("abs");
        let m = manifest_of(&["docs/a.txt", "docs/b.txt", "other.txt"]);
        let items = plan(&m, &dest, &["docs".into()]).expect("plans");
        // The subtree *and* its directory node (`docs` itself, plus the synthesised parent of a selection
        // is exactly what a recursive extract has to create first).
        assert_eq!(items.len(), 3);
        assert!(
            items
                .iter()
                .all(|i| i.member == "docs" || i.member.starts_with("docs/"))
        );
        assert!(items.iter().any(|i| i.is_dir));
    }

    #[test]
    fn extract_writes_only_under_the_destination() {
        let dir = crate::scratch::create("extract");
        let dest = AbsPath::from_path(dir.join("out")).expect("abs");
        std::fs::create_dir_all(dest.as_path()).expect("dest");
        let m = manifest_of(&["docs/", "docs/a.txt"]);
        let items = plan(&m, &dest, &[]).expect("plans");
        let report = extract_all(
            &mut FakeReader,
            &items,
            &dest,
            ConflictPolicy::SkipExisting,
            &CancelToken::new(),
            &NoopProgress,
        )
        .expect("extracts");
        assert_eq!(report.written.len(), 1);
        assert!(dest.as_path().join("docs/a.txt").exists());
        let _ = crate::scratch::remove_tree(&dir);
    }

    #[test]
    fn a_virtual_node_carries_its_symlink_bit_from_the_mode() {
        // `plan` reads the manifest, which has no mode column; the symlink decision therefore comes from
        // the raw entry, and this pins the helper the manifest builder uses.
        let mut n = VirtualNode {
            name: "link".into(),
            is_dir: false,
            size: 6,
            mod_time: 0,
            internal_path: "link".into(),
            children: Vec::new(),
            encrypted: false,
        };
        n.children.push("x".into());
        assert_eq!(n.parent_key(), "");
        assert_eq!(n.to_entry().mode, "-r--r--r--");
    }
}
