//! Job/event plumbing for the shell (plan §5.7 `jobs_ui.rs` row).
//!
//! The single place that resolves an `Origin` to `(PaneSlot, TabId)` — the plan §4.3 correlation
//! rule — so a finished listing updates *its* tab's spinner and *its* tab's error, and four tabs on
//! one directory stay four independent stories.

use std::collections::HashMap;
use std::time::Duration;

use zenman_core::events::EngineEvent;
use zenman_core::jobs::{JobId, JobKind, JobStatus};
use zenman_core::session::Origin;

use crate::app::ZenApp;
use crate::state::{AppModel, TabId};
use crate::widgets::toast;

/// One live or just-finished job, as the shell knows it.
#[derive(Clone, Debug)]
pub struct JobSlot {
    /// Engine identity.
    pub id: JobId,
    /// The correlation key it answers to.
    pub origin: Origin,
    /// What it is.
    pub kind: JobKind,
    /// The widget label, already worded.
    pub label: String,
    /// Which pane asked (resolved from the origin when the job started).
    pub pane: Option<zenman_core::store::config::PaneSlot>,
    /// Which tab asked, when a tab did.
    pub tab: Option<TabId>,
    /// Last reported fraction, 0..=1 (0 when the total is unknown).
    pub frac: f32,
    /// Spinner rather than bar (an uncapped walk).
    pub spinner: bool,
    /// Still running.
    pub live: bool,
}

/// The shell's job view state.
#[derive(Clone, Debug, Default)]
pub struct JobsUi {
    /// Jobs in start order, for the status strip.
    pub foreground: Vec<JobSlot>,
    /// The correlation table: origin → slot (plan §4.3).
    pub by_origin: HashMap<Origin, JobSlot>,
}

impl JobsUi {
    /// Register a job the shell itself started, binding its origin to the asking tab.
    pub fn track(
        &mut self,
        info: zenman_core::jobs::JobInfo,
        pane: Option<zenman_core::store::config::PaneSlot>,
        tab: Option<TabId>,
    ) {
        let slot = JobSlot {
            id: info.id,
            origin: info.origin,
            kind: info.kind,
            label: info.label,
            pane,
            tab,
            frac: 0.0,
            spinner: true,
            live: true,
        };
        self.by_origin.insert(slot.origin, slot.clone());
        self.foreground.push(slot);
    }

    /// Drain the engine and route every event (plan row: `tick(app, ctx)`), then keep the loop
    /// alive if anything is still running.
    pub fn tick(app: &mut ZenApp, ctx: &egui::Context) {
        let events = app.session.poll();
        if !events.is_empty() {
            // Something answered: repaint now so the answer shows this frame, not after the TTL.
            ctx.request_repaint();
        }
        for event in events {
            // The quick-find dialog owns this answer (step 11b): it goes to the dialog's lane,
            // where the stale-origin rule filters it — never to a tab.
            if let EngineEvent::SearchReady {
                origin,
                job,
                result,
            } = event
            {
                crate::dialogs::quick_find::deliver(app, origin, job, result);
                continue;
            }
            let notices = route(&mut app.jobs, &mut app.model, event);
            for (level, text) in notices {
                app.notices.post(level, text);
            }
        }
        let _ = keep_polling(app, ctx);
    }

    /// The live count, for the status strip.
    #[must_use]
    pub fn live_count(&self) -> usize {
        self.foreground.iter().filter(|slot| slot.live).count()
    }
}

/// What one event produced for the user: notices to post (kept as data so `tick` can borrow
/// `app.notices` afterwards, and so a test can assert the words without a context).
type Routed = Vec<(toast::Level, String)>;

/// Route one event (plan §5.7 `route`): the single place origin → tab resolution happens.
pub fn route(jobs: &mut JobsUi, model: &mut AppModel, event: EngineEvent) -> Routed {
    let mut notices = Routed::new();
    match event {
        EngineEvent::JobStarted {
            id,
            kind,
            label,
            origin,
        } => {
            if !jobs.by_origin.contains_key(&origin) {
                jobs.foreground.push(JobSlot {
                    id,
                    origin,
                    kind,
                    label,
                    pane: None,
                    tab: None,
                    frac: 0.0,
                    spinner: true,
                    live: true,
                });
            }
        }
        EngineEvent::JobProgress {
            id: _,
            origin,
            progress,
        } => {
            let frac = match progress.total_bytes {
                Some(total) if total > 0 => (progress.bytes_done as f64 / total as f64) as f32,
                _ => match progress.total_files {
                    Some(total) if total > 0 => (progress.files_done as f64 / total as f64) as f32,
                    _ => 0.0,
                },
            };
            if let Some(slot) = jobs.by_origin.get_mut(&origin) {
                slot.frac = frac;
                slot.spinner = frac <= 0.0;
            }
        }
        EngineEvent::DirPageReady { origin, path, page } => {
            let slot = jobs.by_origin.get_mut(&origin);
            if let Some((_, tab)) = slot
                .and_then(|s| s.tab)
                .and_then(|id| find_tab_mut(model, id))
            {
                // A flat listing is *always* this event (`page_dir` turns a flat miss into a
                // `JobKind::List` job), including its `LoadMore` pages — so the append that
                // `ZenApp::list_at` does synchronously for a cached page has to happen here too.
                // `loaded_so_far > entries.len()` is the data-side test for "this is page two or
                // later" (a first page's `loaded_so_far` is its own length).
                if tab.loading_more && page.loaded_so_far > page.entries.len() {
                    tab.append_page(page);
                    tab.loading_more = false;
                } else {
                    tab.total = page.total_count;
                    tab.has_more = page.has_more;
                    tab.entries = page;
                    // The reload keeps what still exists (plan F21); keys are listing-relative paths.
                    tab.prune_selection();
                }
                tab.pending.remove(&origin);
                if tab.path != path {
                    // A page for another path is a race the correlation rule exists to ignore —
                    // but `path` here is the truth of what was listed, so keep the tab honest.
                    tab.path = path;
                }
                tab.error = None;
                if let Some(pane) = jobs.by_origin.get(&origin).and_then(|s| s.pane)
                    && let Some(p) = model.panes.get_mut(&pane)
                {
                    p.loading.remove(&origin);
                }
            } else {
                // Nobody is waiting on it (a background refresh, or the asking tab closed): the
                // core cache already holds the page, so dropping the event loses nothing.
            }
        }
        EngineEvent::JobFinished {
            id,
            kind: _,
            origin,
            outcome,
            payload: _,
            error,
        } => {
            // `by_origin` and `foreground` hold *copies* of the slot, so the terminal event must
            // mark both: `live_count()` and the status strip read `foreground`, and a route that
            // only flipped the correlation-table copy left every tracked job spinning forever.
            let slot = jobs.by_origin.get_mut(&origin).map(|s| {
                s.live = false;
                s.frac = 1.0;
                (s.pane, s.tab)
            });
            for fg in jobs.foreground.iter_mut().filter(|s| s.origin == origin) {
                fg.live = false;
                fg.frac = 1.0;
            }
            match outcome.status {
                JobStatus::Done => {
                    notices.push((toast::Level::Success, outcome.summary));
                }
                JobStatus::Partial => {
                    notices.push((toast::Level::Warning, outcome.summary));
                }
                JobStatus::Failed | JobStatus::Cancelled | JobStatus::TimedOut => {
                    let text = error
                        .as_ref()
                        .map(|e| e.user_message())
                        .or(outcome.error.clone())
                        .unwrap_or_else(|| format!("job {:?} failed", id));
                    if let Some((_, tab)) = slot
                        .and_then(|(_, tab)| tab)
                        .and_then(|tab| find_tab_mut(model, tab))
                    {
                        // The error belongs to the asking tab only (plan §5.6: Go showed the last
                        // error globally and it followed the user between panes). `Arc` because
                        // `CoreError` is not `Clone` (plan §9.2, step 3).
                        tab.error = error.clone();
                        tab.pending.remove(&origin);
                        notices.push((toast::Level::Error, text));
                    } else {
                        notices.push((toast::Level::Error, text));
                    }
                }
            }
            for warning in outcome.warnings {
                notices.push((toast::Level::Warning, warning));
            }
        }
        EngineEvent::Notice { level, message } => {
            let mapped = match level {
                zenman_core::events::NoticeLevel::Info => toast::Level::Info,
                zenman_core::events::NoticeLevel::Warn => toast::Level::Warning,
                zenman_core::events::NoticeLevel::Error => toast::Level::Error,
            };
            notices.push((mapped, message));
        }
        // Step 11a's search rows are addressed to a dialog, not to a tab: `dialogs/quick_find` (step 11b)
        // consumes them where it owns the `Origin`, and the jobs strip has already stopped its spinner on
        // the terminal `JobFinished`, so routing them here would be a second owner for one answer.
        EngineEvent::SearchReady { .. } => {}
        EngineEvent::DirSizeReady { .. }
        | EngineEvent::IndexChanged { .. }
        | EngineEvent::LogAppended { .. } => {
            // Disk sizes paint from the cache (step 8+); the index's shape is step 12's UI; the log
            // panel is step 14's.
        }
        EngineEvent::DirChanged {
            path,
            cause,
            origin,
        } => {
            // The reload queue, not a reload: `ZenApp::reload_stale` drains it once per frame, so ten
            // of these (a paste through a subtree, a watcher burst) cost one re-list per tab. Every
            // tab listing `path` *or anything inside it* is marked (plan §4.3's invalidation rule).
            //
            // `cause` does not change the answer here, and that is a deliberate deviation from plan
            // §4.1's "resets or refreshes quietly": a reload already keeps scroll and the path-keyed
            // selection, so the quiet form *is* the only form. The origin still matters to the
            // spinner, and `Tab::pending` is the tab's own record of it.
            let _ = (cause, origin);
            model.mark_stale(&path);
        }
        EngineEvent::ClipboardChanged { .. } => {
            // The clipboard badge is step 10's UI, so there is nothing to route here yet.
        }
    }
    notices
}

/// Ask the engine whether the loop must stay alive, and say for how long (plan F14).
#[must_use]
pub fn keep_polling(app: &ZenApp, ctx: &egui::Context) -> bool {
    if let Some(wait) = app.session.wants_repaint() {
        ctx.request_repaint_after(wait.max(REPAINT_CADENCE));
        return true;
    }
    false
}

/// The floor of the repaint delay: `wants_repaint` may answer "now"; the loop still coalesces to
/// roughly one frame (plan §2.2: 16 ms).
pub const REPAINT_CADENCE: Duration = Duration::from_millis(16);

/// Locate a tab by id across the pane grid (the plan row's `(PaneSlot, TabId)` resolution).
fn find_tab_mut(
    model: &mut AppModel,
    id: TabId,
) -> Option<(zenman_core::store::config::PaneSlot, &mut crate::state::Tab)> {
    let slot = model.tab_index(id)?.0;
    Some((slot, model.tab_at_mut((slot, model.tab_index(id)?.1))?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Tab, TabId};
    use zenman_core::entry::DirPage;
    use zenman_core::events::JobPayload;
    use zenman_core::jobs::JobOutcome;
    use zenman_core::path::FsPath;
    use zenman_core::store::config::PaneSlot;

    fn model_with_tab() -> (AppModel, TabId, Origin) {
        let mut model = AppModel::new(zenman_core::store::config::LayoutMode::Single);
        let id = model.next_tab_id();
        let origin = Origin(7);
        {
            let pane = model.pane_mut(PaneSlot::Left);
            let mut tab = Tab::new(id, FsPath::ROOT, "/");
            tab.pending.insert(origin);
            pane.tabs.push(tab);
        }
        (model, id, origin)
    }

    #[test]
    fn page_ready_lands_only_on_the_asking_tab() {
        let (mut model, id, origin) = model_with_tab();
        let mut jobs = JobsUi::default();
        jobs.by_origin.insert(
            origin,
            JobSlot {
                id: zenman_core::jobs::JobId(1),
                origin,
                kind: JobKind::List,
                label: "listing".into(),
                pane: Some(PaneSlot::Left),
                tab: Some(id),
                frac: 0.0,
                spinner: true,
                live: true,
            },
        );
        // A second tab on the same directory, not asking.
        let other = model.next_tab_id();
        model
            .pane_mut(PaneSlot::Left)
            .tabs
            .push(Tab::new(other, FsPath::ROOT, "/"));
        let page = std::sync::Arc::new(DirPage {
            path: FsPath::ROOT,
            entries: Vec::new(),
            cursor: zenman_core::cursor::Cursor(3),
            has_more: false,
            total_count: 3,
            loaded_so_far: 3,
        });
        let notices = route(
            &mut jobs,
            &mut model,
            EngineEvent::DirPageReady {
                origin,
                path: FsPath::ROOT,
                page: page.clone(),
            },
        );
        assert!(notices.is_empty());
        let tab = model.tab_at((PaneSlot::Left, 0)).expect("tab");
        assert_eq!(tab.total, 3);
        assert!(tab.entries.is_complete());
        assert!(tab.pending.is_empty(), "the spinner's owner is cleared");
        assert_eq!(model.tab_at((PaneSlot::Left, 1)).expect("other").total, 0);
    }

    #[test]
    fn failure_reaches_the_asking_tab_and_a_notice() {
        let (mut model, id, origin) = model_with_tab();
        let mut jobs = JobsUi::default();
        jobs.by_origin.insert(
            origin,
            JobSlot {
                id: zenman_core::jobs::JobId(2),
                origin,
                kind: JobKind::DirSize,
                label: "size".into(),
                pane: Some(PaneSlot::Left),
                tab: Some(id),
                frac: 0.0,
                spinner: true,
                live: true,
            },
        );
        let notices = route(
            &mut jobs,
            &mut model,
            EngineEvent::JobFinished {
                id: zenman_core::jobs::JobId(2),
                kind: JobKind::DirSize,
                origin,
                outcome: JobOutcome {
                    status: JobStatus::Failed,
                    summary: String::new(),
                    moved: Vec::new(),
                    warnings: Vec::new(),
                    error: Some("boom".into()),
                    bytes_copied: 0,
                },
                payload: JobPayload::None,
                error: Some(std::sync::Arc::new(
                    zenman_core::error::CoreError::Cancelled,
                )),
            },
        );
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].0, toast::Level::Error);
        let tab = model.tab_at((PaneSlot::Left, 0)).expect("tab");
        assert!(tab.error.is_some(), "the asking tab carries the error");
        assert!(!jobs.by_origin[&origin].live);
    }

    #[test]
    fn a_dir_change_queues_one_reload_per_tab() {
        // The coalescing rule, at the routing seam: the event marks, it does not reload, and the mark
        // is a set — a watcher burst of three costs one re-list.
        let (mut model, id, _origin) = model_with_tab();
        let mut jobs = JobsUi::default();
        for _ in 0..3 {
            let notices = route(
                &mut jobs,
                &mut model,
                EngineEvent::DirChanged {
                    path: FsPath::ROOT,
                    cause: zenman_core::events::ChangeCause::Watcher,
                    origin: None,
                },
            );
            assert!(notices.is_empty(), "a reload is not a toast");
        }
        assert_eq!(model.stale.len(), 1, "three events, one queued reload");
        assert!(model.stale.contains(&id));
        assert_eq!(model.take_stale(), vec![id]);
        assert!(
            model.stale.is_empty(),
            "and the queue is drained in one pass"
        );
    }

    #[test]
    fn progress_fractions_are_last_value_wins() {
        let (mut model, _id, origin) = model_with_tab();
        let mut jobs = JobsUi::default();
        jobs.by_origin.insert(
            origin,
            JobSlot {
                id: zenman_core::jobs::JobId(3),
                origin,
                kind: JobKind::Paste,
                label: "paste".into(),
                pane: Some(PaneSlot::Left),
                tab: None,
                frac: 0.0,
                spinner: true,
                live: true,
            },
        );
        for done in [100u64, 400, 700] {
            route(
                &mut jobs,
                &mut model,
                EngineEvent::JobProgress {
                    id: zenman_core::jobs::JobId(3),
                    origin,
                    progress: zenman_core::jobs::JobProgress {
                        bytes_done: done,
                        files_done: 0,
                        total_bytes: Some(1000),
                        total_files: None,
                        current: None,
                    },
                },
            );
        }
        assert!((jobs.by_origin[&origin].frac - 0.7).abs() < 1e-6);
        assert!(!jobs.by_origin[&origin].spinner);
    }
}
