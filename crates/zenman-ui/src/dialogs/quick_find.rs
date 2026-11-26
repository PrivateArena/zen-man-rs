//! The live quick-find dialog (plan §5.8 `dialogs/quick_find.rs`, §7 row 11's UI half).
//!
//! Go's `frontend/js/quick-find.js`: an overlay with one field, a `.*` regex toggle, a
//! Both/Folders/Files scope switch, a `Recursive` checkbox, and a result list of
//! icon + name + `rel_path` (+ size for files) rows. <kbd>Enter</kbd> on a directory row
//! navigates the pane, on a file row runs the desktop open; <kbd>Escape</kbd> closes and aborts
//! (`quick-find.js:325-349`); the count line says `Searching...`, `N matches`, or
//! `N+ matches (capped)`, and an empty result list reads `No matching files or folders found`
//! (`:237`, `:275-279`, `:476-497`).
//!
//! # What the port does differently, and why
//!
//! * The **throttle lives in core** (step 11a, `services/search_live.rs`): every keystroke calls
//!   [`Session::search_live`](zenman_core::session::Session::search_live) and simply ignores
//!   [`SearchAdmission::Deferred`] except to keep the spinner up — Go's single 250 ms
//!   `debounceTimeout` (`:197-202`) is replaced by a rule the caller cannot forget.
//! * The **scope filter is server-side**: Go re-filtered the cached rows client-side
//!   (`:166-176`); here the filter goes into the [`SearchQuery::scope_filter`], so the rows on
//!   screen are always the rows the walk was asked for. (Go's filter also *hid* the truncation:
//!   capped rows vanished behind the filter and the count lied.)
//! * **An invalid regex is inline and starts nothing** — Go's `catch` printed
//!   `Error running search: …` after the request had already been made and the abort machinery
//!   had run; core refuses the pattern before spending a job slot, and the message paints under
//!   the field in the error colour, never as a toast ([`CoreError::BadRegex`] is not
//!   `is_expected`).
//! * **The footer is here** (`index.html:593-616`): `Copy Found`, `Cut Found`, `Copy Path`,
//!   `Copy Name` and `Delete Found` act on the *whole* match set, which is why the walk is
//!   uncapped (below) — Go capped at 500 and answered that by re-fetching with `limit=0` before
//!   every verb (`quick-find.js:499-534`); the port lifts the cap instead, so the rows on screen
//!   are the rows the verb uses. The four clipboard verbs are *requests* kept for [`drain`], so
//!   the dialog survives them (Go closed it, `:719`/`:785`) and one opening can chain
//!   `Copy Found` → `Delete Found`. `Delete Found` alone is a closing [`QuickOutcome`], because the
//!   answer it needs next is a modal of its own and the stack holds one ([`Dialogs::open`]).
//!   **Its two `Paste … Inside` rows are still missing**: they paste the live clipboard into every
//!   found folder, which is a *loop of paste jobs* each able to answer a conflict, and the
//!   conflict confirm cannot be raised while this dialog owns the modal slot. Plan §7 row 11b's
//!   other criterion — "URI list written to the system clipboard, X11/Wayland only" — was already
//!   met underneath: `Copy`/`Cut Found` go through [`Session::clipboard_store`], which publishes
//!   `file://` URIs through `ops::clipboard::publish_system` exactly as Go's `writeToSystemClipboard`
//!   did (`api/ops.go:549-575`), so no new clipboard dependency is involved.
//! * **A reveal takes two frames.** Go's right-click on a row (`:321-325`) called
//!   `executeItemAction(index, 'reveal')`, which navigated to the row's parent and selected the row on
//!   a 150 ms timer — its tab had every entry in memory already. Here the parent may never have been
//!   listed, so `QuickOutcome::Reveal` asks for the navigation and `ZenApp::apply_reveal` discharges
//!   the promise on the frame the row exists (widening a paged listing once, the way the pane's own
//!   `Load all items` does, when the row is past the page).
//! * There is **no hidden-files toggle**, because Go has none (its live search never filtered
//!   dot-files — plan §2.3), so the query keeps core's Go-parity [`HiddenPolicy::Include`].
//! * Go persists the three toggles in `localStorage`; the port keeps them per-opening (plan D-list:
//!   settings are step 12's `store/config` round-trip, not a dialog's business).
//!
//! # The stale-answer rule (plan §4.3, steps 9a/11a)
//!
//! Every dispatch takes a fresh `Origin` and remembers it; [`deliver`] paints a `SearchReady` only
//! when its origin is the dialog's current one. Core already suppresses superseded walks, so this
//! is the second half of the contract: the dialog cannot be repainted by a keystroke it no longer
//! has.
//!
//! [`Dialogs::open`]: crate::dialogs::Dialogs::open
//! [`Session::clipboard_store`]: zenman_core::session::Session::clipboard_store

use std::sync::Arc;
use std::time::Duration;

use egui::{
    Align2, Area, CornerRadius, Key, Order, Pos2, Rect, ScrollArea, Sense, TextEdit, Ui, Vec2,
};

use zenman_core::error::CoreError;
use zenman_core::jobs::JobId;
use zenman_core::ops::clipboard::ClipOp;
use zenman_core::path::AbsPath;
use zenman_core::services::search_live::{
    HiddenPolicy, NameMode, ScopeFilter, SearchAdmission, SearchHit, SearchQuery, SearchResults,
    SearchStatus,
};
use zenman_core::session::Origin;
use zenman_core::store::config::PaneSlot;

use crate::app::ZenApp;
use crate::dialogs::confirm::modal_frame;
use crate::dialogs::prompt::consume_key;
use crate::dialogs::{Answer, Dialog, DialogKind};
use crate::theme::Tokens;
use crate::widgets::disk_meter::format_size;

/// Name column's share of the row (the rest carries the path, then the size).
const NAME_SHARE: f32 = 0.45;

/// Size column's share of the row.
const SIZE_SHARE: f32 = 0.18;

/// Result rows on screen before the list scrolls (Go's overlay is `max-height: 60vh`,
/// `quick-find.css`; ten 40 px rows is the same order at our window size).
const LIST_ROWS: f32 = 10.0;

/// The field's own horizontal inset, counted in cell paddings: the `.*` toggle's gap on one side,
/// the field's inner margin on the other, times two sides (`tokens.dialog_padding` already carries
/// the frame's half). Named because a bare `4.0` is a point measurement (plan §5.7).
const FIELD_INSET_PADDINGS: f32 = 4.0;

/// What the field is for: Go's `placeholder="Type to find files..."` (`index.html:575`).
const PLACEHOLDER: &str = "Type to find files...";

/// One footer button. The five are Go's, in Go's DOM order (`index.html:593-616`), minus its two
/// `Paste … Inside` rows (module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Footer {
    /// The label, in Go's copy.
    label: &'static str,
    /// What the button does, for the pointer that paused on it.
    tooltip: &'static str,
    /// Red text: the same `tokens.danger` the confirm card's destructive answer wears.
    danger: bool,
    /// The verb, or `None` for the one that answers the dialog instead of asking the shell.
    request: Option<QuickRequest>,
}

/// The footer, left to right as Go drew it.
const FOOTER: [Footer; 5] = [
    Footer {
        label: "Copy Found",
        tooltip: "Queue every match on the clipboard, for a paste into any pane",
        danger: false,
        request: Some(QuickRequest::CopyFound),
    },
    Footer {
        label: "Cut Found",
        tooltip: "Queue every match as a move (paste takes the bytes away from here)",
        danger: false,
        request: Some(QuickRequest::CutFound),
    },
    Footer {
        label: "Copy Path",
        tooltip: "Put every match's full path on the system clipboard, one per line",
        danger: false,
        request: Some(QuickRequest::CopyPaths),
    },
    Footer {
        label: "Copy Name",
        tooltip: "Put every match's name on the system clipboard, one per line",
        danger: false,
        request: Some(QuickRequest::CopyNames),
    },
    Footer {
        label: "Delete Found",
        tooltip: "Delete every match — asks first, and offers the trash",
        danger: true,
        request: None,
    },
];

/// How the dialog's answer reaches the engine (the `Answer::Quick` payload; dialogs never call
/// the engine themselves — plan §4.1's one-way door).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuickOutcome {
    /// <kbd>Enter</kbd> (or a double-click) on a row: navigate if `is_dir`, else the desktop
    /// open — Go's `executeItemAction(…, 'open')` (`quick-find.js:399-414`).
    Activate {
        /// The pane that owns the answer (frozen at open, plan §4.3).
        slot: PaneSlot,
        /// The hit's absolute path (`root + rel_path`).
        path: AbsPath,
        /// Directory rows navigate; file rows open.
        is_dir: bool,
    },
    /// <kbd>Escape</kbd> (or overlay dismissal): Go's `closeQuickFind` aborting the in-flight
    /// request (`:74-93`). The shell cancels `job` and resets the throttle.
    Closed {
        /// The pane to return focus to.
        slot: PaneSlot,
        /// The job whose rows, if any, were still coming.
        job: Option<JobId>,
    },
    /// `Delete Found`: every match, handed to the shell, which must ask before touching a byte
    /// (Go's `confirm()` of `quick-find.js:799` did the same, and offered only the permanent
    /// answer). The paths are already resolved, because the rows that named them are gone the
    /// moment this dialog closes.
    DeleteFound {
        /// The pane to report the job to.
        slot: PaneSlot,
        /// Every match, in list order.
        paths: Vec<AbsPath>,
    },
    /// Right-click on a row: Go's `executeItemAction(index, 'reveal')` (`:321-325`, `:405-421`) —
    /// close the overlay, navigate `slot` to the row's **parent** directory, and select the row
    /// there. The path is the row's own, not its parent's: *where* to look is the shell's business
    /// once it has the row.
    Reveal {
        /// The pane that owns the answer.
        slot: PaneSlot,
        /// The matched item, whose parent becomes the pane's directory.
        path: AbsPath,
    },
}

/// What a footer button asked the shell to do to the match set. Not an [`Answer`]: `Dialogs::show`
/// closes the dialog on any answer, and these four verbs leave the list up for the next one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuickRequest {
    /// `Copy Found` — Go's `performClipboardFound('copy')` (`:695`, `:747`).
    CopyFound,
    /// `Cut Found` — Go's `performClipboardFound('cut')` (`:699`).
    CutFound,
    /// `Copy Path` — Go's `onCopyPathFoundClick` (`:703`), via its `copyPaths` (`:582`).
    CopyPaths,
    /// `Copy Name` — Go's `onCopyNameFoundClick` (`:725`), via its `copyNames` (`:588`).
    CopyNames,
}

/// The dialog's state — the plan's `QuickFind { query, regex, scope, recursive, results, total,
/// status, cursor, job }`, with the fields the as-built core API needs to make the stale-answer
/// rule and the teardown assertable: `slot`/`root` (the subject, frozen at open), `origin` (the
/// correlation token), `error` (the inline refusal), `busy`/`wait` (the deferred-spinner state),
/// and the `started`/`deferred` admission counters that let a test count *walks* rather than
/// seconds. `cursor` is `Option<usize>`: Go's `selectedIndex` starts at `-1`
/// (`quick-find.js:12`) and `None` is that value spelled honestly.
#[derive(Clone, Debug)]
pub struct QuickFind {
    /// The pane this search belongs to; its directory is the scope and its tab takes the
    /// navigation. Frozen at open (plan §4.3).
    pub slot: PaneSlot,
    /// The directory the walk starts in (Go's `tab.currentPath`, `quick-find.js:222`).
    pub root: AbsPath,
    /// The field's text.
    pub query: String,
    /// The `.*` toggle (`quick-find.js:191-195`): off is a case-insensitive substring, on is a
    /// regex (Go's `NameMode::{Substring, Regex}` — Go's live search has no word mode).
    pub regex: bool,
    /// Both / Folders / Files (Go filters cached rows client-side; we ask the walk).
    pub scope: ScopeFilter,
    /// The `Recursive` checkbox — **off by default**, because Go's is
    /// (`localStorage` empty ⇒ unchecked, `quick-find.js:36-38`).
    pub recursive: bool,
    /// The rows currently on screen.
    pub results: Vec<SearchHit>,
    /// Go's `totalMatched` (`:251`): the walk's `total_matched`, `None` before any answer.
    pub total: Option<usize>,
    /// How the last walk ended (cap, budgets, timing).
    pub status: SearchStatus,
    /// The keyboard cursor (Go's `selectedIndex`, `-1` when there is none).
    pub cursor: Option<usize>,
    /// The job whose rows are still expected, `None` when nothing is in flight.
    pub job: Option<JobId>,
    /// The correlation token of the newest dispatch; the stale-answer rule compares against it.
    pub origin: Option<Origin>,
    /// The inline refusal (Go printed the failure after the fact; we show it *instead of* a
    /// walk for a pattern that does not compile).
    pub error: Option<String>,
    /// A walk is in flight or the throttle is holding the newest keystroke.
    pub busy: bool,
    /// When `busy` is a deferral: how long the throttle still holds (`SearchAdmission::Deferred`).
    pub wait: Option<Duration>,
    /// Until the first frame, the field takes focus (Go's `input.focus()`, `:63`).
    pub fresh: bool,
    /// Walks this opening has *admitted* (the test's counter; not timing-derived).
    pub started: usize,
    /// Keystrokes the throttle refused (a deferral is not a walk).
    pub deferred: usize,
    /// The last request handed to the engine — what makes "did the keystroke change anything?"
    /// a comparison instead of a guess.
    requested: Option<SearchQuery>,
    /// The footer's verbs, waiting for [`drain`] (which runs them as the shell, not as the dialog).
    pub requests: Vec<QuickRequest>,
}

impl QuickFind {
    /// A fresh dialog searching `root` for `slot`'s tab, every toggle at Go's default.
    #[must_use]
    pub fn new(slot: PaneSlot, root: AbsPath) -> Self {
        Self {
            slot,
            root,
            query: String::new(),
            regex: false,
            scope: ScopeFilter::Both,
            recursive: false,
            results: Vec::new(),
            total: None,
            status: SearchStatus::default(),
            cursor: None,
            job: None,
            origin: None,
            error: None,
            busy: false,
            wait: None,
            fresh: true,
            started: 0,
            deferred: 0,
            requested: None,
            requests: Vec::new(),
        }
    }

    /// The request the last keystroke described (`None` before the first dispatch).
    #[must_use]
    pub fn requested(&self) -> Option<&SearchQuery> {
        self.requested.as_ref()
    }

    /// The row the cursor is on, as the path Go would act on and its kind
    /// (`quick-find.js:389-396`: `currentPath + rel_path`).
    #[must_use]
    pub fn activate_target(&self) -> Option<(AbsPath, bool)> {
        self.cursor.and_then(|ix| self.hit_target(ix))
    }

    /// One row of the list, as the path it names and its kind. The two row verbs (Enter, and the
    /// right-click's reveal) both need it, and a `rel_path` the root cannot answer is no row.
    #[must_use]
    pub fn hit_target(&self, ix: usize) -> Option<(AbsPath, bool)> {
        let hit = self.results.get(ix)?;
        let path = self.root.join_rel(&hit.rel_path).ok()?;
        Some((path, hit.is_dir()))
    }

    /// Are the footer's verbs live? Go's `updateActionButtonsState`: no rows, no verbs
    /// (`:437-451`). A walk in flight has already dropped its rows (`clear_rows`), so this answers
    /// `false` for it too, and the buttons cannot act on a set that is being replaced.
    #[must_use]
    pub fn actions_enabled(&self) -> bool {
        !self.results.is_empty()
    }

    /// Every match as an absolute path, in list order — Go's `resolvePathsForAction` (`:499-534`)
    /// without its refetch, because this dialog does not cap the walk: what the list shows *is* the
    /// whole match set. A `rel_path` the root cannot answer is skipped rather than fatal, the same
    /// rule [`Self::activate_target`] applies to the single row it was given.
    #[must_use]
    pub fn found_paths(&self) -> Vec<AbsPath> {
        self.results
            .iter()
            .filter_map(|hit| self.root.join_rel(&hit.rel_path).ok())
            .collect()
    }

    /// Lift the footer's pending verbs out of the dialog, resolved to the paths they act on.
    /// Called by [`drain`] once per frame, and *only* there: the rows belong to the dialog, the
    /// engine does not.
    fn take_batch(&mut self) -> Option<QuickBatch> {
        let requests = std::mem::take(&mut self.requests);
        if requests.is_empty() {
            return None;
        }
        Some(QuickBatch {
            requests,
            paths: self.found_paths(),
        })
    }

    /// The request the current state describes, or `None` when it is the one already handed
    /// over. Emptying the field is itself a request (Go aborts and clears, `:218-230`), and it
    /// is the one the engine must *not* walk.
    fn take_pending_request(&mut self) -> Option<SearchQuery> {
        let text = self.query.trim();
        let req = self.build_request(text);
        if self.requested.as_ref() == Some(&req) {
            return None;
        }
        self.requested = Some(req.clone());
        Some(req)
    }

    fn build_request(&self, text: &str) -> SearchQuery {
        let mut req = SearchQuery::new(self.root.clone(), text);
        req.mode = if self.regex {
            NameMode::Regex
        } else {
            NameMode::Substring
        };
        req.scope_filter = self.scope;
        req.recursive = self.recursive;
        req.hidden = HiddenPolicy::Include;
        // **No row cap.** Go capped the walk at 500 (`api/search.go:66`) and made the footer pay for
        // it with a re-fetch of the whole set before every verb (`quick-find.js:499-534`); the port
        // lifts the cap here, so the filter is the truth and the verbs need no second pass. `0` is
        // Go's own spelling of unlimited ([`SearchQuery::result_cap`]), and the walk still stops at
        // the node/depth budgets, which [`Self::status_text`] reports instead of hiding.
        req.limit = 0;
        req
    }

    /// Drop the rows (a new keystroke, an emptied field, a refusal): the pane never shows rows
    /// that are not the current request's answer.
    fn clear_rows(&mut self) {
        self.results.clear();
        self.total = None;
        self.status = SearchStatus::default();
        self.cursor = None;
    }

    /// Paint an answer the origin check accepted: rows, count, cursor to Go's first row.
    fn apply(&mut self, result: Arc<SearchResults>) {
        self.results = result.hits.clone();
        self.status = result.status.clone();
        self.total = Some(result.status.total_matched);
        self.cursor = if result.hits.is_empty() {
            None
        } else {
            Some(0)
        };
        self.job = None;
        self.busy = false;
        self.wait = None;
        self.error = None;
    }

    /// Go's arrow wrap (`quick-find.js:329-343`): the cursor cycles, and from "nothing selected"
    /// Down lands on the first row and Up on the last.
    fn move_cursor(&mut self, delta: i32) {
        let len = i32::try_from(self.results.len()).unwrap_or(i32::MAX);
        if len == 0 {
            self.cursor = None;
            return;
        }
        let cur = self.cursor.map_or(-1, |ix| i32::try_from(ix).unwrap_or(0));
        self.cursor = usize::try_from((cur + delta).rem_euclid(len)).ok();
    }

    /// The count line, in Go's words (`quick-find.js:237`, `:476-497`).
    fn status_text(&self) -> String {
        if self.busy || self.job.is_some() {
            return match self.wait {
                Some(after) => format!("Searching... (next walk in {} ms)", after.as_millis()),
                None => "Searching...".to_string(),
            };
        }
        match self.total {
            Some(total) if self.status.truncated => format!("{total}+ matches (capped)"),
            Some(total) => {
                let noun = if total == 1 { "match" } else { "matches" };
                match self.stop_note() {
                    Some(note) => format!("{total} {noun} ({note})"),
                    None => format!("{total} {noun}"),
                }
            }
            None => String::new(),
        }
    }

    /// Why the walk stopped short of the whole tree when it was *not* the row cap — with the cap
    /// gone ([`Self::build_request`]) these budgets are the only thing standing between a query and
    /// an answer, so the count line owns up to them rather than printing a number that reads
    /// complete. `None` when the walk ran to the end, which is the normal case.
    fn stop_note(&self) -> Option<String> {
        let status = &self.status;
        if status.node_budget_hit {
            Some(format!(
                "stopped after {} names examined",
                status.nodes_visited
            ))
        } else if status.timed_out {
            Some("stopped at the time budget".to_string())
        } else if status.depth_truncated {
            Some("stopped at the depth limit".to_string())
        } else {
            None
        }
    }

    /// Draw it; `Some(answer)` once the user has answered (which closes the dialog — the
    /// teardown runs in `ZenApp::answer`, not here).
    pub fn show(&mut self, ui: &mut Ui, tokens: &Tokens) -> Option<Answer> {
        // Read the decisive keys *before* the field, and consume them (the prompt's rule: a
        // focused `TextEdit` would otherwise swallow <kbd>Enter</kbd>, and the arrows belong to
        // the list — `quick-find.js:329-343` binds them on the input itself).
        let enter = consume_key(ui, Key::Enter);
        let escape = consume_key(ui, Key::Escape);
        if consume_key(ui, Key::ArrowDown) {
            self.move_cursor(1);
        }
        if consume_key(ui, Key::ArrowUp) {
            self.move_cursor(-1);
        }
        let mut outcome = if escape {
            Some(QuickOutcome::Closed {
                slot: self.slot,
                job: self.job,
            })
        } else if enter {
            self.activate_target()
                .map(|(path, is_dir)| QuickOutcome::Activate {
                    slot: self.slot,
                    path,
                    is_dir,
                })
        } else {
            None
        };

        let screen_width = ui.ctx().input(|i| i.viewport_rect().width());
        let width = (screen_width * tokens.quick_find_fraction).min(tokens.dialog_max_width);
        let title = format!("Finding in: {}", self.root.as_path().display());
        let mut clicked: Option<usize> = None;
        let mut activated: Option<usize> = None;
        let mut revealed: Option<usize> = None;
        let mut delete_found = false;
        let mut fresh = self.fresh;
        let status = self.status_text();
        let error = self.error.clone();
        let show_empty = !self.busy
            && self.job.is_none()
            && !self.query.trim().is_empty()
            && error.is_none()
            && self.total.is_some()
            && self.results.is_empty();
        let area = Area::new(ui.scope_id().with("zenman-quick-find"))
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .order(Order::Foreground)
            .constrain(true)
            .show(ui.ctx(), |ui| {
                modal_frame(tokens, ui, width, |ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(0.0, tokens.dialog_gap);
                    ui.horizontal(|ui| {
                        let field = ui.add(
                            TextEdit::singleline(&mut self.query)
                                .hint_text(PLACEHOLDER)
                                .desired_width(
                                    width
                                        - tokens.dialog_padding * 2.0
                                        - tokens.cell_padding * FIELD_INSET_PADDINGS,
                                )
                                .font(tokens.font()),
                        );
                        if fresh {
                            field.request_focus();
                            fresh = false;
                        }
                        if ui
                            .selectable_label(
                                self.regex,
                                egui::RichText::new(".*").color(if self.regex {
                                    tokens.text_active
                                } else {
                                    tokens.text_muted
                                }),
                            )
                            .on_hover_text("Toggle regular expression mode")
                            .clicked()
                        {
                            self.regex = !self.regex;
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(&title).color(tokens.text_muted));
                    });
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("Scope:")
                                .color(tokens.text_muted)
                                .size(tokens.font_size - 2.0),
                        );
                        for (label, filter) in [
                            ("Both", ScopeFilter::Both),
                            ("Folders", ScopeFilter::Dirs),
                            ("Files", ScopeFilter::Files),
                        ] {
                            if ui
                                .selectable_label(
                                    self.scope == filter,
                                    egui::RichText::new(label).color(if self.scope == filter {
                                        tokens.text_active
                                    } else {
                                        tokens.text_muted
                                    }),
                                )
                                .on_hover_text("Limit search scope")
                                .clicked()
                            {
                                self.scope = filter;
                            }
                        }
                        ui.checkbox(
                            &mut self.recursive,
                            egui::RichText::new("Recursive").color(tokens.text_main),
                        )
                        .on_hover_text("Search all sub-folders recursively");
                    });
                    if let Some(error) = error {
                        // The criterion: the refusal is *here*, in the error colour, and no walk ran.
                        ui.label(egui::RichText::new(error).color(tokens.danger));
                    } else if !status.is_empty() {
                        ui.label(egui::RichText::new(status).color(tokens.text_muted));
                    }
                    if !self.results.is_empty() {
                        let row_h = tokens.row_height;
                        // 15c D6: the list owns a *reserved* band of LIST_ROWS rows. A ScrollArea
                        // loose in a free-floating `Area` had nothing to measure against and realised
                        // a single row; the band is placed by `in_rect` (the D2/D7/D12 cell idiom),
                        // then reserved.
                        let list_h = row_h * LIST_ROWS;
                        let list_rect = Rect::from_min_size(
                            ui.cursor().min,
                            Vec2::new(ui.available_width(), list_h),
                        );
                        crate::widgets::in_rect(
                            ui,
                            list_rect,
                            egui::Layout::top_down(egui::Align::LEFT),
                            |ui| {
                                ScrollArea::vertical()
                                    .auto_shrink([false, false])
                                    .max_height(list_h)
                                    .show_rows(ui, row_h, self.results.len(), |ui, range| {
                                        let painter = ui.painter().clone();
                                        let font = tokens.font();
                                        for ix in range {
                                            let hit = &self.results[ix];
                                            let selected = self.cursor == Some(ix);
                                            let (rect, resp) = ui.allocate_exact_size(
                                                Vec2::new(ui.available_width(), row_h),
                                                Sense::click(),
                                            );
                                            // Paint decoration only — the row's *one* interaction rect is the
                                            // allocated one (plan §9.2 step 8's painting rule).
                                            let [name_cell, path_cell, size_cell] =
                                                row_cells(rect, tokens);
                                            // Paint decoration only — the row's *one* interaction rect is the
                                            // allocated one (plan §9.2 step 8's painting rule) — and every cell
                                            // is disjoint, so the name can never print over the path again
                                            // (15c D6: `row-0000txt0000.txt`).
                                            if selected {
                                                painter.rect_filled(
                                                    rect,
                                                    CornerRadius::ZERO,
                                                    tokens.selected_row_bg,
                                                );
                                            } else if resp.hovered() {
                                                painter.rect_filled(
                                                    rect,
                                                    CornerRadius::ZERO,
                                                    tokens.bg_hover,
                                                );
                                            }
                                            let pad = tokens.cell_padding;
                                            let name = hit.name().to_string();
                                            painter.text(
                                                name_cell.left_center() + Vec2::X * pad,
                                                Align2::LEFT_CENTER,
                                                fit_text(&name, name_cell.width() - pad * 2.0),
                                                font.clone(),
                                                tokens.text_main,
                                            );
                                            painter.text(
                                                path_cell.left_center() + Vec2::X * pad,
                                                Align2::LEFT_CENTER,
                                                fit_text(
                                                    hit.rel_path.as_str(),
                                                    path_cell.width() - pad * 2.0,
                                                ),
                                                font.clone(),
                                                tokens.text_muted,
                                            );
                                            let meta = if hit.is_dir() {
                                                String::new()
                                            } else {
                                                format_size(
                                                    u64::try_from(hit.entry.size).unwrap_or(0),
                                                )
                                            };
                                            if !meta.is_empty() {
                                                painter.text(
                                                    size_cell.right_center() - Vec2::X * pad,
                                                    Align2::RIGHT_CENTER,
                                                    meta,
                                                    font.clone(),
                                                    tokens.text_muted,
                                                );
                                            }
                                            if resp.secondary_clicked() {
                                                // Go's right-click on a row (`:321-325`): not a menu, the
                                                // reveal itself — `executeItemAction(index, 'reveal')`.
                                                revealed = Some(ix);
                                            } else if resp.clicked() {
                                                if resp.double_clicked() {
                                                    activated = Some(ix);
                                                } else {
                                                    clicked = Some(ix);
                                                }
                                            }
                                            // One accessible node per row, named like the pane's own rows
                                            // (`entry_row`'s pattern): it is what lets a test *count* the rows
                                            // the list realised and compare their rects (15c D6).
                                            let label = name.clone();
                                            resp.widget_info(|| {
                                                let mut info =
                                                    egui::WidgetInfo::new(egui::Role::Button);
                                                info.label = Some(label.as_str().into());
                                                info
                                            });
                                        }
                                    });
                            },
                        );
                        ui.advance_cursor_after_rect(list_rect);
                    } else if show_empty {
                        // Go's `.quick-find-empty` line, verbatim (`quick-find.js:277`).
                        ui.label(
                            egui::RichText::new("No matching files or folders found")
                                .color(tokens.text_muted),
                        );
                    }
                    // The footer (Go's `index.html:593-616`): the batch verbs over the *whole* match
                    // set, right-aligned like its `justify-content: flex-end`, and greyed exactly when
                    // Go greyed them (`updateActionButtonsState`, `:437-451`). The four clipboard verbs
                    // are kept as requests for [`drain`] — an [`Answer`] would close the dialog, and
                    // `Copy Found` then `Delete Found` is the point of having them. `Delete Found` is
                    // the closing one, because the question it must ask next wants the modal slot this
                    // dialog is standing in (`Dialogs::open` refuses a second dialog).
                    let enabled = self.actions_enabled();
                    ui.add_space(tokens.dialog_gap);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        for footer in FOOTER.iter().rev() {
                            let colour = if footer.danger {
                                tokens.danger
                            } else if enabled {
                                tokens.text_main
                            } else {
                                tokens.text_muted
                            };
                            let response = ui
                                .add_enabled(
                                    enabled,
                                    egui::Button::new(
                                        egui::RichText::new(footer.label)
                                            .color(colour)
                                            .size(tokens.font_size),
                                    ),
                                )
                                .on_hover_text(footer.tooltip);
                            if response.clicked() {
                                match footer.request {
                                    Some(request) => self.requests.push(request),
                                    None => delete_found = true,
                                }
                            }
                        }
                    });
                });
            });
        let _ = area;
        if let Some(ix) = activated {
            self.cursor = Some(ix);
            outcome = self
                .activate_target()
                .map(|(path, is_dir)| QuickOutcome::Activate {
                    slot: self.slot,
                    path,
                    is_dir,
                });
        } else if let Some(ix) = clicked {
            self.cursor = Some(ix);
        }
        // The reveal is Go's other row verb, and it closes the overlay for the same reason Enter
        // does: the answer is a *navigation* of the pane, which the shell owns.
        if revealed.is_some() && outcome.is_none() {
            outcome = revealed
                .and_then(|ix| self.hit_target(ix))
                .map(|(path, _)| QuickOutcome::Reveal {
                    slot: self.slot,
                    path,
                });
        }
        // <kbd>Escape</kbd> already answered the dialog above; a footer click in the same frame as a
        // dismissal is not a second answer, so the key wins.
        if delete_found && outcome.is_none() {
            let paths = self.found_paths();
            if !paths.is_empty() {
                outcome = Some(QuickOutcome::DeleteFound {
                    slot: self.slot,
                    paths,
                });
            }
        }
        self.fresh = fresh;
        outcome.map(|outcome| Answer::Quick {
            kind: DialogKind::QuickFind { slot: self.slot },
            outcome,
        })
    }
}

/// A rough stand-in for "name column width": the rel path starts after the name run, and the
/// painter has no layout engine here. Two character widths per row are Go's proportion.
/// The row's three columns (15c D6): the name cell carries the icon slot, the path cell starts where
/// the name cell **ends**, and the size cell hugs the right edge. The old code put the path at an
/// *estimated* advance from the left edge, and the two texts painted on top of each other.
#[must_use]
pub fn row_cells(rect: Rect, tokens: &Tokens) -> [Rect; 3] {
    let pad = tokens.cell_padding;
    let inner = Rect::from_min_size(
        rect.min + Vec2::X * pad,
        Vec2::new((rect.width() - pad * 2.0).max(0.0), rect.height()),
    );
    let size_w = inner.width() * SIZE_SHARE;
    let name_w = (inner.width() - size_w) * NAME_SHARE;
    let path_w = (inner.width() - size_w - name_w).max(0.0);
    [
        Rect::from_min_size(inner.min, Vec2::new(name_w, inner.height())),
        Rect::from_min_size(
            Pos2::new(inner.min.x + name_w, inner.min.y),
            Vec2::new(path_w, inner.height()),
        ),
        Rect::from_min_size(
            Pos2::new(inner.max.x - size_w, inner.min.y),
            Vec2::new(size_w, inner.height()),
        ),
    ]
}

/// Trim `text` to the cell's width (15c D6): a painted label has no clip rect in this fork, so an
/// over-long name would run under the next column exactly as the defect's `row-0000txt0000.txt` did.
/// Three ASCII dots, not `…` — rule 11 says a symbol needs a face in the installed stack.
fn fit_text(text: &str, max: f32) -> String {
    let per_char = estimate_name_advance(1);
    if text.is_empty() || per_char <= 0.0 || text.chars().count() as f32 * per_char <= max {
        return text.to_string();
    }
    let keep = (max / per_char).floor() as usize;
    let head: String = text.chars().take(keep.saturating_sub(3)).collect();
    format!("{head}...")
}

fn estimate_name_advance(chars: usize) -> f32 {
    2.0 * chars as f32
}

/// The footer's verbs, lifted out of the dialog with the paths they act on: one batch per frame,
/// resolved while the dialog still owns the rows (`drain` runs after the draw, when the rows are
/// still the ones the user clicked on).
struct QuickBatch {
    /// What was asked, in click order.
    requests: Vec<QuickRequest>,
    /// Every match, in list order — the whole set, because the walk is uncapped.
    paths: Vec<AbsPath>,
}

/// Run the footer's non-closing verbs (plan §4.1's one-way door: the dialog asks, the shell acts).
/// Called from `ZenApp::ui` beside [`search_manager::drain`](super::search_manager::drain), for the
/// same reason — a config panel
/// and a find footer both have work to do that must not cost them their window.
pub fn drain(app: &mut ZenApp, ctx: &egui::Context) {
    let batch = match app.dialogs.active_mut() {
        Some(Dialog::QuickFind(find)) => find.take_batch(),
        _ => None,
    };
    let Some(batch) = batch else { return };
    for request in &batch.requests {
        match request {
            QuickRequest::CopyFound => app.found_clipboard(&batch.paths, ClipOp::Copy),
            QuickRequest::CutFound => app.found_clipboard(&batch.paths, ClipOp::Cut),
            QuickRequest::CopyPaths => app.found_copy_text(&batch.paths, false, ctx),
            QuickRequest::CopyNames => app.found_copy_text(&batch.paths, true, ctx),
        }
    }
}

/// Open the dialog (plan §5.8's `open`): a *new* search session, so the throttle's window from a
/// previous opening is forgotten — the lane must not make the first keystroke of a new dialog
/// wait out the old one's typing.
pub fn open(app: &mut ZenApp, slot: PaneSlot, scope: AbsPath) {
    app.session.reset_search_throttle();
    app.open_dialog(Dialog::QuickFind(QuickFind::new(slot, scope)));
}

/// Per-frame non-drawing work (plan §5.8's `tick`): hand the newest request to the engine.
/// Every keystroke *does* call `search_live`; a `Deferred` answer is ignored except that the
/// spinner stays up, because core re-starts the newest deferred query itself when the window
/// closes (`Session::poll` → `promote_due_search`).
pub fn tick(app: &mut ZenApp, ctx: &egui::Context) {
    let req = match app.dialogs.active_mut() {
        Some(Dialog::QuickFind(q)) => q.take_pending_request(),
        _ => return,
    };
    let Some(req) = req else { return };
    if req.query.is_empty() {
        // Go aborts the in-flight request and clears the list (`quick-find.js:213-230`).
        if let Some(job) = app.session.live_search() {
            app.session.cancel(job);
        }
        app.session.drop_pending_search();
        if let Some(Dialog::QuickFind(q)) = app.dialogs.active_mut() {
            // A fresh origin retires any answer already on its way; the rows go with it.
            q.origin = Some(app.session.next_origin());
            q.job = None;
            q.busy = false;
            q.wait = None;
            q.error = None;
            q.clear_rows();
        }
        return;
    }
    if let Some(Dialog::QuickFind(q)) = app.dialogs.active_mut() {
        q.busy = true;
        q.wait = None;
        q.error = None;
    }
    let origin = app.session.next_origin();
    match app.session.search_live(&req, origin) {
        // The only synchronous refusal a search gets, and the criterion's target: no job, no
        // toast, the message in the field's error colour.
        Err(e @ CoreError::BadRegex { .. }) => {
            if let Some(Dialog::QuickFind(q)) = app.dialogs.active_mut() {
                q.error = Some(e.user_message());
                q.busy = false;
                q.job = None;
                q.clear_rows();
            }
        }
        Err(e) => {
            if let Some(Dialog::QuickFind(q)) = app.dialogs.active_mut() {
                q.error = Some(e.user_message());
                q.busy = false;
            }
        }
        Ok(SearchAdmission::Running { job, origin }) => {
            if let Some(Dialog::QuickFind(q)) = app.dialogs.active_mut() {
                // A dispatch is a new request: the rows on screen are the *previous* one's answer,
                // and the stale-answer rule says they must not survive into the new one.
                q.clear_rows();
                q.job = Some(job);
                q.origin = Some(origin);
                q.busy = true;
                q.wait = None;
                q.started += 1;
            }
        }
        Ok(SearchAdmission::Deferred { origin, after }) => {
            if let Some(Dialog::QuickFind(q)) = app.dialogs.active_mut() {
                // *Ignored* except for the spinner — the walk is core's promise, not this frame's.
                q.clear_rows();
                q.origin = Some(origin);
                q.busy = true;
                q.wait = Some(after);
                q.deferred += 1;
                ctx.request_repaint_after(after);
            }
        }
    }
}

/// Take a `SearchReady` out of the event stream (the `JobsUi::tick` seam): paint it only if it
/// answers *this* dialog's newest request. Any other origin is a keystroke this dialog has
/// already overwritten — the stale-answer rule, spelled a second time on the side that displays.
pub fn deliver(app: &mut ZenApp, origin: Origin, _job: JobId, result: Arc<SearchResults>) {
    let Some(Dialog::QuickFind(q)) = app.dialogs.active_mut() else {
        return;
    };
    if q.origin != Some(origin) {
        return;
    }
    // The belt behind the suspenders: the result carries its own query (11a), and rows are only
    // ever painted for the request that produced them.
    if q.requested.as_ref() != Some(&result.query) {
        return;
    }
    q.apply(result);
}
