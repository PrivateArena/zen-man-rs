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
//! * **The footer's clipboard buttons are not here yet** (Copy/Cut/Copy Path/Copy Name/Delete/
//!   Paste Inside, `index.html:592-612`): they act on *all* results with a capped-result refetch
//!   (`quick-find.js:501-537`), which is step 12+ territory, and the plan's field list does not
//!   name them. There is also **no hidden-files toggle**, because Go has none (its live search
//!   never filtered dot-files — plan §2.3), so the query keeps core's Go-parity
//!   [`HiddenPolicy::Include`].
//! * Go persists the three toggles in `localStorage`; the port keeps them per-opening (plan D-list:
//!   settings are step 12's `store/config` round-trip, not a dialog's business).
//!
//! # The stale-answer rule (plan §4.3, steps 9a/11a)
//!
//! Every dispatch takes a fresh `Origin` and remembers it; [`deliver`] paints a `SearchReady` only
//! when its origin is the dialog's current one. Core already suppresses superseded walks, so this
//! is the second half of the contract: the dialog cannot be repainted by a keystroke it no longer
//! has.

use std::sync::Arc;
use std::time::Duration;

use egui::{
    Align2, Area, CornerRadius, Key, Order, Pos2, Rect, ScrollArea, Sense, TextEdit, Ui, Vec2,
};

use zenman_core::error::CoreError;
use zenman_core::jobs::JobId;
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
        let hit = self.cursor.and_then(|ix| self.results.get(ix))?;
        let path = self.root.join_rel(&hit.rel_path).ok()?;
        Some((path, hit.is_dir()))
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
            Some(total) => format!("{total} match{}", if total == 1 { "" } else { "es" }),
            None => String::new(),
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
                        ScrollArea::vertical()
                            .auto_shrink([false, true])
                            .max_height(row_h * LIST_ROWS)
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
                                    let [name_cell, path_cell, size_cell] = row_cells(rect, tokens);
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
                                        format_size(u64::try_from(hit.entry.size).unwrap_or(0))
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
                                    if resp.clicked() {
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
                                        let mut info = egui::WidgetInfo::new(egui::Role::Button);
                                        info.label = Some(label.as_str().into());
                                        info
                                    });
                                }
                            });
                    } else if show_empty {
                        // Go's `.quick-find-empty` line, verbatim (`quick-find.js:277`).
                        ui.label(
                            egui::RichText::new("No matching files or folders found")
                                .color(tokens.text_muted),
                        );
                    }
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
