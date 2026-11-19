//! The Search Manager dialog (plan §5.8 `dialogs/search_manager.rs`, §7 row 12's UI half — the last
//! slice of the index step).
//!
//! Go's `frontend/js/search-manager.js` + `sidebar.js`: one overlay with the index query field, the
//! **status block** (`is_indexing` / `total_files` / `last_indexed` / `is_dirty` / `progress.indexed` /
//! `config{roots,excludes,auto_index,worker_count,follow_symlinks}`, the `error?` line — the exact key
//! set of `tests/golden/index-status.json`), a **Rebuild** button that polls to `indexing` within one
//! tick, paging over the results table, and the **config editor** whose save is Go's
//! `POST /api/index/config` (golden `index-config-apply.json`, `{"saved": true}` — the port's
//! [`Session::set_index_config`](zenman_core::session::Session::set_index_config) returns the value it
//! actually wrote, which is strictly more useful and costs nothing).
//!
//! # The operator set is Go's, and there is no `size:`
//!
//! [`parse_query`](zenman_core::services::search_index::parse_query) accepts `ext:`, `in:`, `type:` and
//! nothing else; `size:txt` is *ordinary text* in Go, so it is ordinary text here — the pill scanner
//! below deliberately leaves it alone, and the add-pill row offers only the three real operators. An
//! unrecognised operator therefore stays in the word list, exactly as the Go handler leaves it.
//!
//! # What the port does differently, and why
//!
//! * Go's status poll was a `setInterval` per open overlay; here the poll is 1 s **and** an
//!   event-driven refresh on `EngineEvent::IndexChanged` ([`notify`]), so pressing Rebuild repaints the
//!   status on the job's own terminal event instead of up to a second later — the same improvement
//!   plan §5.8's action-log row made over the Go log panel.
//! * The config editor is **inline**, not `prompt()`, as the plan's row requires: roots and excludes
//!   are lists, which a single-line prompt cannot express, and the validation error from
//!   `compile_excludes` / `ZenConfig::validate` paints under the field in the error colour instead of
//!   the toast Go had (Go's handler returned `500` and the frontend printed it into the console).
//! * The dialog still never touches the engine: a button sets a [`ManagerRequest`] and [`drain`] (run
//!   by `ZenApp::ui` after the draw, exactly like the pane commands) performs it. That is what lets a
//!   root change take the *invalidation* path — `AppModel::mark_stale`, the same queue `DirChanged`
//!   fills — instead of a manual refresh the user would have to ask for.
//! * Watcher on/off is surfaced where the platform supports it; a refusal is the typed
//!   [`CoreError::Watcher`](zenman_core::error::CoreError::Watcher) message shown inline, never
//!   swallowed (`watch_error`), and the descriptor report (`watch_count`,
//!   [`Session::failed_roots`], [`Session::unwatched`]) is printed under it.
//!
//! # The stale-answer rule (plan §4.3, copied from `quick_find`)
//!
//! Every dispatch takes a fresh `Origin` and records it; [`SearchManager::deliver`] paints an answer
//! only when its origin is the dialog's current one. The index *search* is synchronous in core (Go's
//! handler was too), so the rule earns its keep on the asynchronous answers — the rebuild's terminal
//! event and the status poll — which must not land behind a query the user has since typed.

use std::time::{Duration, Instant};

use egui::{
    Align2, Area, CornerRadius, Key, Order, Rect, ScrollArea, Sense, TextEdit, Ui, Vec2, vec2,
};

use zenman_core::jobs::JobId;
use zenman_core::metrics::format_mtime;
use zenman_core::path::{AbsPath, FsPath};
use zenman_core::services::search_index::{IndexEntry, IndexSearchResults, IndexStatus};
use zenman_core::session::{IndexConfig, Origin, WatchPolicy};
use zenman_core::store::config::PaneSlot;

use crate::app::ZenApp;
use crate::dialogs::confirm::modal_frame;
use crate::dialogs::prompt::consume_key;
use crate::dialogs::{Answer, Dialog, DialogKind};
use crate::theme::Tokens;
use crate::widgets::disk_meter::format_size;

/// Plan §5.8's debounce for the index query — Go's `searchDebounce` (250 ms for quick-find, 300 ms
/// here; `search-manager.js`).
const QUERY_DEBOUNCE: Duration = Duration::from_millis(300);

/// Go's status poll (`statusInterval = 1000`, `search-manager.js`).
const STATUS_POLL: Duration = Duration::from_secs(1);

/// Plan §5.8's `limit` default — Go's handler constant (`api/search_index_api.go:26`).
pub const DEFAULT_LIMIT: usize = 200;

/// The page sizes the limit control offers: Go's page-size selector, with its 200 in the middle.
const LIMITS: [usize; 3] = [50, DEFAULT_LIMIT, 1000];

/// Result rows on screen before the list scrolls (Go's table is `max-height: 45vh`).
const LIST_ROWS: f32 = 9.0;

/// Size column's share of the space after the path (the remainder is the `Modified` column).
const SIZE_OF_META: f32 = 0.5;

/// Go's four result columns, in its words (`search-manager.html`'s table head).
const COLUMNS: [&str; 4] = ["Name", "Path", "Size", "Modified"];

/// The field's own horizontal inset, counted in cell paddings (the pill buttons live beside it).
const FIELD_INSET_PADDINGS: f32 = 6.0;

/// Name column share of a result row; the rest carries the absolute path, then size and mode.
const NAME_SHARE: f32 = 0.28;

/// Path column share of the remainder after the name.
const PATH_SHARE: f32 = 0.55;

/// Size/mode column share of that same remainder.
const META_SHARE: f32 = 0.45;

/// The worker-count field's width, counted in cell paddings.
const WORKER_FIELD_PADDINGS: f32 = 6.0;

/// What Go's placeholder says (`search-manager.html`: "Search the index…").
const PLACEHOLDER: &str = "Search the index...";

/// The three operators Go's filter regex knows, in its order.
const PILL_KEYS: [(&str, PillKind); 3] = [
    ("ext", PillKind::Ext),
    ("in", PillKind::In),
    ("type", PillKind::Type),
];

/// Which operator a pill carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PillKind {
    /// `ext:` — the extension without its dot.
    Ext,
    /// `in:` — a folder the path must live under.
    In,
    /// `type:` — `dir`/`file` (anything else is Go's silent "any").
    Type,
}

impl PillKind {
    /// The operator's name as it is typed.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Ext => "ext",
            Self::In => "in",
            Self::Type => "type",
        }
    }
}

/// One operator token lifted out of the query field, with the byte span it occupies in it (so
/// removing the pill is an exact edit and not a re-render of the whole string).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pill {
    /// Which operator.
    pub kind: PillKind,
    /// The value as typed (quotes stripped, case kept — the *field* is the record, core lowercases
    /// for matching).
    pub value: String,
    /// Start of the token in [`SearchManager::query`].
    pub start: usize,
    /// End of the token, exclusive.
    pub end: usize,
}

impl Pill {
    /// The token as it reads on the chip.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{}:{}", self.kind.key(), self.value)
    }
}

fn is_word_byte(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

/// Lift the operator tokens out of `query`, with the same rules core's
/// `services::search_index::parse_query` applies: a word character may not precede the keyword, the
/// value is `"…"` or the next non-space run, and **an unrecognised operator is not a pill** — `size:`
/// stays ordinary text, because Go has no `size:` operator to parse.
#[must_use]
pub fn scan_pills(query: &str) -> Vec<Pill> {
    let chars: Vec<char> = query.chars().collect();
    let mut offsets: Vec<usize> = Vec::with_capacity(chars.len() + 1);
    let mut at = 0usize;
    for c in &chars {
        offsets.push(at);
        at += c.len_utf8();
    }
    offsets.push(at);
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        let mut found: Option<(PillKind, String, usize)> = None;
        for (key, kind) in PILL_KEYS {
            let klen = key.chars().count();
            let head = match key.chars().next() {
                Some(c) => c,
                None => continue,
            };
            if chars[i] != head || i + klen >= chars.len() || chars[i + klen] != ':' {
                continue;
            }
            if !key
                .chars()
                .enumerate()
                .all(|(k, c)| chars.get(i + k) == Some(&c))
            {
                continue;
            }
            // Go's `\b`: a word character may not precede the keyword.
            if i > 0 && is_word_byte(chars[i - 1]) {
                continue;
            }
            let vstart = i + klen + 1;
            let Some(&first) = chars.get(vstart) else {
                continue;
            };
            if first.is_whitespace() {
                continue;
            }
            let (value, end) = if first == '"' {
                let close = chars[vstart + 1..]
                    .iter()
                    .position(|&c| c == '"')
                    .map(|p| vstart + 1 + p);
                match close {
                    Some(stop) => (
                        chars[vstart + 1..stop].iter().collect::<String>(),
                        (stop + 1).min(chars.len()),
                    ),
                    // Go's unterminated-quote divergence (plan §6.2); the pill shows what the field
                    // holds and the search itself stays core's answer.
                    None => (chars[vstart + 1..].iter().collect::<String>(), chars.len()),
                }
            } else {
                let mut j = vstart;
                while j < chars.len() && !chars[j].is_whitespace() {
                    j += 1;
                }
                (chars[vstart..j].iter().collect::<String>(), j)
            };
            if value.trim().is_empty() {
                continue;
            }
            found = Some((kind, value.trim().to_string(), end));
            break;
        }
        match found {
            Some((kind, value, end)) => {
                out.push(Pill {
                    kind,
                    value,
                    start: offsets[i],
                    end: offsets[end.min(chars.len())],
                });
                i = end;
            }
            None => i += 1,
        }
    }
    out
}

/// What the dialog asked the shell to do (a dialog never calls the engine — plan §4.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManagerRequest {
    /// `POST /api/index/rebuild` through [`Session::index_rebuild`].
    Rebuild,
    /// `POST /api/index/cancel` through [`Session::index_cancel`].
    Cancel,
    /// `POST /api/index/config` through [`Session::set_index_config`] with the draft as it stands.
    Apply,
    /// Re-read the draft from the live config (a refusal's "nothing changed" has to be visible).
    Revert,
    /// [`Session::watch_start`] — the typed refusal is shown, not swallowed.
    WatchStart,
    /// [`Session::watch_stop`].
    WatchStop,
}

/// What the dialog answered with, when the answer *closes* it (the requests above do not).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexOutcome {
    /// <kbd>Enter</kbd> (or a double-click) on a row: navigate a directory, open a file — Go's
    /// `searchResults.forEach(... executeItemAction)` on the manager's own table.
    Activate {
        /// The pane that owns the answer (frozen at open, plan §4.3).
        slot: PaneSlot,
        /// The row's absolute path (the index's `rel_path` *is* absolute — plan §2.4).
        path: AbsPath,
        /// Directory rows navigate; file rows open.
        is_dir: bool,
    },
    /// <kbd>Escape</kbd>: Go's `closeSearchManager`, which stops the rebuild poll. The shell cancels
    /// the rebuild job when one is still running.
    Closed {
        /// The rebuild job this opening started, if any.
        rebuild: Option<JobId>,
    },
}

/// The dialog's state — the plan's `SearchManager { query, pills, page, limit, hits, matched, capped,
/// status, settings_open }` plus what the as-built core API needs to make the stale-answer rule and
/// the write path assertable: `draft` (the config under edit), `requests` (the engine's inbox),
/// `error`/`watch_error` (the two inline refusals), `origin` (the correlation token), `rebuild` (the
/// job whose progress the status panel paints).
#[derive(Clone, Debug)]
pub struct SearchManager {
    /// The pane a row's navigation lands in. Frozen at open (plan §4.3).
    pub slot: PaneSlot,
    /// The field's text — the authority for the search, exactly as in Go.
    pub query: String,
    /// The operator chips parsed out of it (`scan_pills`).
    pub pills: Vec<Pill>,
    /// 0-based page (Go's `currentPage - 1`; its `offset = page * limit`).
    pub page: usize,
    /// Rows per page — Go's 200.
    pub limit: usize,
    /// The rows on screen.
    pub hits: Vec<IndexEntry>,
    /// Go's `total_matched`.
    pub matched: usize,
    /// Go's `capped` — `total_matched > limit + offset`, its own expression, computed by core.
    pub capped: bool,
    /// The last status read (`index-status.json`'s fields).
    pub status: IndexStatus,
    /// The inline config editor, open?
    pub settings_open: bool,
    /// The config under edit.
    pub draft: IndexConfig,
    /// The text beside the roots list, awaiting **Add**.
    pub new_root: String,
    /// The text beside the excludes list, awaiting **Add**.
    pub new_exclude: String,
    /// `worker_count` as typed (a `u32` field has to be parsed before it can be refused).
    pub worker_text: String,
    /// The config/search refusal (the value that was refused, in the error colour).
    pub error: Option<String>,
    /// The typed `CoreError::Watcher` message from the watch toggle.
    pub watch_error: Option<String>,
    /// The rebuild job, while it runs.
    pub rebuild: Option<JobId>,
    /// The rebuild job's own fraction, `None` while the walk cannot know a total (15c D4).
    pub progress_fraction: Option<f32>,
    /// The stage the walking job is on — `JobProgress::current`, read out of the job by [`tick`]
    /// (15c D4: the per-root progress note).
    pub progress_stage: Option<String>,
    /// The correlation token of the newest dispatch (the stale-answer rule compares against it).
    pub origin: Option<Origin>,
    /// The keyboard cursor.
    pub cursor: Option<usize>,
    /// What the watcher is doing (read once per poll by [`tick`], printed by the status block).
    pub watch: WatchReport,
    /// The request the screen answers, so "did anything change?" is a comparison, not a guess.
    requested: Option<(String, usize, usize)>,
    /// When the newest keystroke happened (the debounce's start).
    since: Option<Instant>,
    /// When the status was last read (the poll's start).
    polled: Option<Instant>,
    /// A status refresh has been asked for by an event (`notify`).
    refresh: bool,
    /// Until the first frame, the field takes focus (Go's `input.focus()`).
    pub fresh: bool,
    /// Searches this opening has handed the engine (a test's counter, like `quick_find`'s `started`).
    pub dispatched: usize,
    /// Answers the origin rule has painted (the pair makes "is this frame's table the current one?"
    /// a fact a test can wait for instead of a frame count it hopes is enough).
    pub painted: usize,
    /// The engine's inbox (see [`drain`]).
    pub requests: Vec<ManagerRequest>,
}

impl SearchManager {
    /// A fresh dialog over the live config for `slot`'s tab.
    #[must_use]
    pub fn new(slot: PaneSlot, config: IndexConfig, status: IndexStatus) -> Self {
        let draft = config;
        let worker_text = draft.worker_count.to_string();
        Self {
            slot,
            query: String::new(),
            pills: Vec::new(),
            page: 0,
            limit: DEFAULT_LIMIT,
            hits: Vec::new(),
            matched: 0,
            capped: false,
            status: IndexStatus {
                is_indexing: status.is_indexing,
                total_files: status.total_files,
                last_indexed: status.last_indexed,
                is_dirty: status.is_dirty,
                config: status.config.clone(),
                progress: status.progress.clone(),
                error: status.error.clone(),
            },
            settings_open: false,
            draft,
            new_root: String::new(),
            new_exclude: String::new(),
            worker_text,
            error: None,
            watch_error: None,
            rebuild: None,
            progress_fraction: None,
            progress_stage: None,
            origin: None,
            cursor: None,
            watch: WatchReport::default(),
            requested: None,
            since: None,
            polled: None,
            refresh: false,
            fresh: true,
            dispatched: 0,
            painted: 0,
            requests: Vec::new(),
        }
    }

    /// Go's `offset` for the current page.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.page.saturating_mul(self.limit)
    }

    /// Re-parse the chips (the field is the authority; the chips are a view of it).
    pub fn sync_pills(&mut self) {
        self.pills = scan_pills(&self.query);
    }

    /// Remove a chip's token from the field (the pill's `x`).
    pub fn remove_pill(&mut self, ix: usize) {
        let Some(pill) = self.pills.get(ix) else {
            return;
        };
        let (start, mut end) = (pill.start, pill.end);
        if end > self.query.len() || start > end {
            return;
        }
        // Swallow the separator that followed the token, so a removal cannot leave `a  b`.
        if self.query.as_bytes().get(end) == Some(&b' ') && end + 1 < self.query.len() {
            end += 1;
        }
        self.query.replace_range(start..end, "");
        self.sync_pills();
        self.touch();
    }

    /// Append an operator token the user then completes in the field (the **Add** row).
    pub fn add_pill(&mut self, kind: PillKind) {
        if !self.query.is_empty() && !self.query.ends_with(' ') {
            self.query.push(' ');
        }
        self.query.push_str(kind.key());
        self.query.push(':');
        self.sync_pills();
        self.touch();
    }

    /// A keystroke: restart the debounce and drop the rows the new answer has not earned yet.
    fn touch(&mut self) {
        self.since = Some(Instant::now());
        self.clear_rows();
    }

    fn clear_rows(&mut self) {
        self.hits.clear();
        self.matched = 0;
        self.capped = false;
        self.cursor = None;
    }

    /// The request the field describes, once the debounce has elapsed (`None` while it holds, or when
    /// nothing changed). Paging and a limit change are immediate — Go paged without a timer.
    fn take_pending_request(&mut self) -> Option<(String, usize, usize)> {
        let req = (self.query.clone(), self.page, self.limit);
        if self.requested.as_ref() == Some(&req) {
            return None;
        }
        // Go's manager never lists an empty query: on open it reads the *status*, and the table stays
        // empty until something is typed (`search-manager.js`'s input handler). Emptying the field is
        // therefore a clear, not a search of the whole index.
        if req.0.trim().is_empty() {
            self.requested = Some(req);
            self.clear_rows();
            return None;
        }
        let elapsed = self.since.is_none_or(|at| at.elapsed() >= QUERY_DEBOUNCE);
        if !elapsed {
            return None;
        }
        self.requested = Some(req.clone());
        Some(req)
    }

    /// Paint an answer the origin check accepted. A token that is not the newest is a request this
    /// dialog has already overwritten, and is dropped (the `quick_find` rule).
    pub fn deliver(&mut self, origin: Origin, result: &IndexSearchResults) -> bool {
        if self.origin != Some(origin) {
            return false;
        }
        self.hits = result.entries.clone();
        self.matched = usize::try_from(result.total_matched).unwrap_or(usize::MAX);
        self.capped = result.capped;
        self.cursor = if result.entries.is_empty() {
            None
        } else {
            Some(0)
        };
        self.origin = None;
        self.error = None;
        self.painted += 1;
        true
    }

    /// The settings/status view (15c D4): Go's mid-rebuild frame *is* the settings view — INDEX
    /// ROOTS / EXCLUDE PATTERNS / Auto index / Follow symlinks above the results table — so an active
    /// rebuild opens it whether or not the user asked for it.
    #[must_use]
    pub fn shows_settings(&self) -> bool {
        self.settings_open || self.status.is_indexing
    }

    /// The progress line beside the bar (15c D4). Core owns the counting and the stage; this only
    /// words them, and says nothing it does not know.
    #[must_use]
    pub fn progress_text(&self) -> String {
        let indexed = self.status.progress.indexed;
        match &self.progress_stage {
            Some(stage) => format!("indexing {indexed} — {stage}"),
            None => format!("indexing {indexed}"),
        }
    }

    /// The status line: Go's `#index-status` text, in its words.
    #[must_use]
    pub fn status_text(&self) -> String {
        if self.status.is_indexing {
            return format!("indexing ({} indexed)", self.status.progress.indexed);
        }
        if self.status.is_dirty {
            return format!(
                "dirty — {} file(s) indexed",
                if self.status.total_files == 0 {
                    0
                } else {
                    self.status.total_files
                }
            );
        }
        format!("{} files indexed", self.status.total_files)
    }

    /// The count line under the table (Go's `N results` / `N results (more)`).
    #[must_use]
    pub fn result_text(&self) -> String {
        if self.query.trim().is_empty() && self.hits.is_empty() {
            return String::new();
        }
        if self.capped {
            return format!("{}+ results (capped)", self.matched);
        }
        format!("{} results", self.matched)
    }

    /// The row the cursor is on, as the path Go would act on.
    #[must_use]
    pub fn activate_target(&self) -> Option<(AbsPath, bool)> {
        let hit = self.cursor.and_then(|ix| self.hits.get(ix))?;
        let path = AbsPath::from_path(&hit.rel_path).ok()?;
        Some((path, hit.is_dir))
    }

    /// Fold the draft's text buffers back into it (`worker_text` is a field of the plan's config).
    fn sync_draft(&mut self) -> Result<IndexConfig, String> {
        let worker: u32 = self
            .worker_text
            .trim()
            .parse()
            .map_err(|_| format!("worker count {:?} is not a number", self.worker_text))?;
        let mut cfg = self.draft.clone();
        cfg.worker_count = worker;
        Ok(cfg)
    }

    /// Move the keyboard cursor (Go's arrow keys on the manager's table).
    fn move_cursor(&mut self, delta: i32) {
        let len = i32::try_from(self.hits.len()).unwrap_or(i32::MAX);
        if len == 0 {
            self.cursor = None;
            return;
        }
        let cur = self.cursor.map_or(-1, |ix| i32::try_from(ix).unwrap_or(0));
        self.cursor = usize::try_from((cur + delta).rem_euclid(len)).ok();
    }

    /// Draw it; `Some(answer)` only when the user has closed it (the requests stay open — a config
    /// panel that vanished on Save would be a worse editor than Go's).
    pub fn show(&mut self, ui: &mut Ui, tokens: &Tokens) -> Option<Answer> {
        let enter = consume_key(ui, Key::Enter);
        let escape = consume_key(ui, Key::Escape);
        if consume_key(ui, Key::ArrowDown) {
            self.move_cursor(1);
        }
        if consume_key(ui, Key::ArrowUp) {
            self.move_cursor(-1);
        }
        let mut outcome = if escape {
            Some(IndexOutcome::Closed {
                rebuild: self.rebuild,
            })
        } else if enter {
            self.activate_target()
                .map(|(path, is_dir)| IndexOutcome::Activate {
                    slot: self.slot,
                    path,
                    is_dir,
                })
        } else {
            None
        };

        let screen_width = ui.ctx().input(|i| i.viewport_rect().width());
        let width =
            (screen_width * tokens.search_manager_fraction).min(tokens.search_manager_max_width);
        let status = self.status_text();
        let results = self.result_text();
        let error = self.error.clone();
        let watch_error = self.watch_error.clone();
        let mut clicked: Option<usize> = None;
        let mut activated: Option<usize> = None;
        let mut fresh = self.fresh;
        let area = Area::new(ui.scope_id().with("zenman-search-manager"))
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .order(Order::Foreground)
            .constrain(true)
            .show(ui.ctx(), |ui| {
                modal_frame(tokens, ui, width, |ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(0.0, tokens.dialog_gap);
                    // ---- the query field and its operator chips
                    ui.horizontal(|ui| {
                        let before = self.query.clone();
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
                        if self.query != before {
                            self.sync_pills();
                            self.touch();
                        }
                        for (key, kind) in PILL_KEYS {
                            if ui
                                .selectable_label(
                                    false,
                                    egui::RichText::new(format!("{key}:")).color(tokens.text_muted),
                                )
                                .on_hover_text(format!("Add a {key}: operator"))
                                .clicked()
                            {
                                self.add_pill(kind);
                            }
                        }
                    });
                    if !self.pills.is_empty() {
                        ui.horizontal(|ui| {
                            for ix in 0..self.pills.len() {
                                let label =
                                    self.pills.get(ix).map_or_else(String::new, Pill::label);
                                if ui
                                    .selectable_label(
                                        true,
                                        egui::RichText::new(format!("{label} x"))
                                            .color(tokens.text_active),
                                    )
                                    .on_hover_text("Remove this operator")
                                    .clicked()
                                {
                                    self.remove_pill(ix);
                                    break;
                                }
                            }
                        });
                    }
                    // ---- paging, limit, the count line
                    ui.horizontal(|ui| {
                        if ui
                            .selectable_label(
                                self.page > 0,
                                egui::RichText::new("< prev").color(if self.page > 0 {
                                    tokens.text_active
                                } else {
                                    tokens.disabled_text
                                }),
                            )
                            .clicked()
                            && self.page > 0
                        {
                            self.page -= 1;
                            self.touch();
                        }
                        ui.label(
                            egui::RichText::new(format!("page {}", self.page + 1))
                                .color(tokens.text_muted),
                        );
                        if ui
                            .selectable_label(
                                self.capped,
                                egui::RichText::new("next >").color(if self.capped {
                                    tokens.text_active
                                } else {
                                    tokens.disabled_text
                                }),
                            )
                            .clicked()
                            && self.capped
                        {
                            self.page += 1;
                            self.touch();
                        }
                        for limit in LIMITS {
                            if ui
                                .selectable_label(
                                    self.limit == limit,
                                    egui::RichText::new(format!("{limit}")).color(
                                        if self.limit == limit {
                                            tokens.text_active
                                        } else {
                                            tokens.text_muted
                                        },
                                    ),
                                )
                                .on_hover_text("Rows per page")
                                .clicked()
                                && self.limit != limit
                            {
                                self.limit = limit;
                                self.page = 0;
                                self.touch();
                            }
                        }
                        if !results.is_empty() {
                            ui.label(egui::RichText::new(results).color(tokens.text_muted));
                        }
                    });
                    if let Some(err) = error.clone() {
                        ui.label(egui::RichText::new(err).color(tokens.danger));
                    }
                    // ---- the results table (name, absolute path, size, mode where present). The
                    // header and its block are drawn **always** (15c D2): Go's empty-query frame is a
                    // table that says `No items matched your query`, not a blank card.
                    result_header(ui, tokens);
                    if !self.hits.is_empty() {
                        let row_h = tokens.row_height;
                        ScrollArea::vertical()
                            // 15c D6's lesson, applied here too: shrinking to the content (up to the
                            // cap) is what makes the list show `LIST_ROWS` rows; a non-shrinking axis
                            // inside an `Area` collapses to the row it happened to measure.
                            .auto_shrink([false, true])
                            .max_height(row_h * LIST_ROWS)
                            .show_rows(ui, row_h, self.hits.len(), |ui, range| {
                                let painter = ui.painter().clone();
                                let font = tokens.font();
                                for ix in range {
                                    let hit = &self.hits[ix];
                                    let selected = self.cursor == Some(ix);
                                    let (rect, resp) = ui.allocate_exact_size(
                                        Vec2::new(ui.available_width(), row_h),
                                        Sense::click(),
                                    );
                                    // Paint decoration only — one interaction rect per row (rule 10).
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
                                    let [name_cell, path_cell, size_cell, date_cell] =
                                        result_cells(rect);
                                    painter.text(
                                        name_cell.left_center() + Vec2::X * pad,
                                        Align2::LEFT_CENTER,
                                        &hit.name,
                                        font.clone(),
                                        tokens.text_main,
                                    );
                                    painter.text(
                                        path_cell.left_center() + Vec2::X * pad,
                                        Align2::LEFT_CENTER,
                                        &hit.rel_path,
                                        font.clone(),
                                        tokens.text_muted,
                                    );
                                    let meta_x = size_cell.right() - pad;
                                    let meta = if hit.is_dir {
                                        String::new()
                                    } else {
                                        format_size(u64::try_from(hit.size).unwrap_or(0))
                                    };
                                    painter.text(
                                        egui::pos2(meta_x, rect.center().y),
                                        Align2::RIGHT_CENTER,
                                        meta,
                                        font.clone(),
                                        tokens.text_muted,
                                    );
                                    // Go's index rows carry `mode: ""` (plan §2.4) and no date, so its
                                    // `Modified` cell is the placeholder Go printed — and the mode is
                                    // only painted if a value ever arrives in it.
                                    let date = if hit.mode.is_empty() {
                                        "--".to_string()
                                    } else {
                                        hit.mode.clone()
                                    };
                                    painter.text(
                                        egui::pos2(date_cell.right() - pad, rect.center().y),
                                        Align2::RIGHT_CENTER,
                                        date,
                                        font.clone(),
                                        tokens.text_muted,
                                    );
                                    if resp.clicked() {
                                        clicked = Some(ix);
                                    } else if resp.double_clicked() {
                                        activated = Some(ix);
                                    }
                                }
                            });
                    } else {
                        // Go's own empty-state words (`search-manager.html`'s no-results line).
                        ui.label(
                            egui::RichText::new("No items matched your query")
                                .color(tokens.text_muted),
                        );
                    }
                    // ---- the rebuild's progress bar (15c D4), above the status line
                    if self.status.is_indexing {
                        ui.add(
                            egui::ProgressBar::new(self.progress_fraction.unwrap_or(0.0)).text(
                                egui::RichText::new(self.progress_text()).color(tokens.text_main),
                            ),
                        );
                    }
                    // ---- the status block (`index-status.json`'s fields) and the actions
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(status).color(
                            if self.status.is_indexing || self.status.is_dirty {
                                tokens.warning
                            } else {
                                tokens.success
                            },
                        ));
                        ui.label(
                            egui::RichText::new(format!(
                                "last indexed {}",
                                last_indexed_text(self.status.last_indexed)
                            ))
                            .color(tokens.text_muted)
                            .size(tokens.font_size - 2.0),
                        );
                        if let Some(err) = self.status.error.clone() {
                            ui.label(egui::RichText::new(err).color(tokens.danger));
                        }
                    });
                    ui.horizontal(|ui| {
                        if ui
                            .selectable_label(
                                false,
                                egui::RichText::new("Rebuild").color(tokens.text_active),
                            )
                            .on_hover_text("Rebuild the index from its roots")
                            .clicked()
                        {
                            self.requests.push(ManagerRequest::Rebuild);
                        }
                        if self.rebuild.is_some()
                            && ui
                                .selectable_label(
                                    false,
                                    egui::RichText::new("Cancel").color(tokens.danger),
                                )
                                .on_hover_text("Cancel the running rebuild")
                                .clicked()
                            && !self.requests.contains(&ManagerRequest::Cancel)
                        {
                            self.requests.push(ManagerRequest::Cancel);
                        }
                        if ui
                            .selectable_label(
                                self.settings_open,
                                egui::RichText::new("Settings").color(if self.settings_open {
                                    tokens.text_active
                                } else {
                                    tokens.text_muted
                                }),
                            )
                            .on_hover_text("Edit the index configuration")
                            .clicked()
                        {
                            self.settings_open = !self.settings_open;
                        }
                        if ui
                            .selectable_label(
                                false,
                                egui::RichText::new("Watch").color(tokens.text_muted),
                            )
                            .on_hover_text("Start the filesystem watcher on the index roots")
                            .clicked()
                        {
                            self.requests.push(ManagerRequest::WatchStart);
                        }
                        if ui
                            .selectable_label(
                                false,
                                egui::RichText::new("Unwatch").color(tokens.text_muted),
                            )
                            .on_hover_text("Stop the filesystem watcher")
                            .clicked()
                        {
                            self.requests.push(ManagerRequest::WatchStop);
                        }
                    });
                    if let Some(err) = watch_error {
                        // The typed `CoreError::Watcher` message, painted — not swallowed.
                        ui.label(egui::RichText::new(err).color(tokens.danger));
                    }
                    ui.label(
                        egui::RichText::new(format!(
                            "watcher: {} ({} descriptor(s), {} failed root(s), {} unwatched)",
                            watch_policy_text(self.watch.policy),
                            self.watch.count,
                            self.watch.failed,
                            self.watch.unwatched
                        ))
                        .color(tokens.text_muted)
                        .size(tokens.font_size - 2.0),
                    );
                    if self.shows_settings() {
                        settings(self, ui, tokens);
                    }
                });
            });
        let _ = area;
        if let Some(ix) = activated {
            self.cursor = Some(ix);
            outcome = self
                .activate_target()
                .map(|(path, is_dir)| IndexOutcome::Activate {
                    slot: self.slot,
                    path,
                    is_dir,
                });
        } else if let Some(ix) = clicked {
            self.cursor = Some(ix);
        }
        self.fresh = fresh;
        outcome.map(|outcome| Answer::Index {
            kind: DialogKind::SearchManager { slot: self.slot },
            outcome,
        })
    }
}

/// The result table's four columns (15c D2/D6): one function owns them, so the header and the
/// painted rows cannot disagree, and so "the columns are disjoint" is a fact a test can assert instead
/// of a property a reviewer has to eyeball in a PNG.
#[must_use]
pub fn result_cells(rect: Rect) -> [Rect; 4] {
    let name_w = rect.width() * NAME_SHARE;
    let rest = (rect.width() - name_w).max(0.0);
    let path_w = rest * PATH_SHARE;
    let meta_w = (rest * META_SHARE).max(0.0);
    let size_w = meta_w * SIZE_OF_META;
    let date_w = (meta_w - size_w).max(0.0);
    let mut cells = [
        Rect::from_min_size(rect.min, vec2(name_w, rect.height())),
        Rect::NOTHING,
        Rect::NOTHING,
        Rect::NOTHING,
    ];
    let mut x = rect.left() + name_w;
    for (slot, width) in cells[1..].iter_mut().zip([path_w, size_w, date_w]) {
        *slot = Rect::from_min_size(
            egui::pos2(x, rect.min.y),
            vec2(width.max(0.0), rect.height()),
        );
        x += width;
    }
    cells
}

/// The header row, allocated as labels (not painted): the a11y tree sees it, which is what makes
/// "the table is there before anything is typed" testable (15c D2).
fn result_header(ui: &mut Ui, tokens: &Tokens) {
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), tokens.header_row_height),
        Sense::hover(),
    );
    for (label, cell) in COLUMNS.iter().zip(result_cells(rect)) {
        crate::widgets::in_rect(
            ui,
            cell,
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.label(egui::RichText::new(*label).color(tokens.text_muted));
            },
        );
    }
    // The band is reserved after its cells are filled: the header owns `header_row_height` of the
    // frame whatever the labels measured (15c D2/D7).
    ui.advance_cursor_after_rect(rect);
}

/// Go's `last_indexed` rendering: `0` is "never", anything else is the date it stored. **Formatted by
/// core** (15c D3): `metrics::format_mtime` is the port's one date voice — the UI owns no calendar,
/// and printing the raw unix second (what this function used to do) is the defect the review found.
#[must_use]
pub fn last_indexed_text(secs: i64) -> String {
    if secs <= 0 {
        return "never".to_string();
    }
    format_mtime(Some(secs))
}

/// The inline config editor (plan §5.8's `settings` — inline, **not** a `prompt()`).
fn settings(m: &mut SearchManager, ui: &mut Ui, tokens: &Tokens) {
    ui.separator();
    ui.label(
        egui::RichText::new("Index roots")
            .color(tokens.text_active)
            .strong(),
    );
    let mut remove_root: Option<usize> = None;
    for ix in 0..m.draft.roots.len() {
        ui.horizontal(|ui| {
            ui.add(
                TextEdit::singleline(&mut m.draft.roots[ix])
                    .font(tokens.font())
                    .desired_width(ui.available_width() * PATH_SHARE),
            );
            if ui
                .selectable_label(false, egui::RichText::new("remove").color(tokens.danger))
                .clicked()
            {
                remove_root = Some(ix);
            }
        });
    }
    if let Some(ix) = remove_root {
        m.draft.roots.remove(ix);
    }
    ui.horizontal(|ui| {
        let add = ui
            .add(
                TextEdit::singleline(&mut m.new_root)
                    .hint_text("/path/to/index")
                    .font(tokens.font())
                    .desired_width(ui.available_width() * PATH_SHARE),
            )
            .clicked();
        if ui
            .selectable_label(
                false,
                egui::RichText::new("add root").color(tokens.text_active),
            )
            .clicked()
            || add
        {
            let text = m.new_root.trim().to_string();
            if !text.is_empty() {
                m.draft.roots.push(text);
                m.new_root.clear();
            }
        }
    });
    ui.label(
        egui::RichText::new("Exclude patterns")
            .color(tokens.text_active)
            .strong(),
    );
    let mut remove_exclude: Option<usize> = None;
    for ix in 0..m.draft.excludes.len() {
        ui.horizontal(|ui| {
            ui.add(
                TextEdit::singleline(&mut m.draft.excludes[ix])
                    .font(tokens.font())
                    .desired_width(ui.available_width() * PATH_SHARE),
            );
            if ui
                .selectable_label(false, egui::RichText::new("remove").color(tokens.danger))
                .clicked()
            {
                remove_exclude = Some(ix);
            }
        });
    }
    if let Some(ix) = remove_exclude {
        m.draft.excludes.remove(ix);
    }
    ui.horizontal(|ui| {
        let add = ui
            .add(
                TextEdit::singleline(&mut m.new_exclude)
                    .hint_text("**/node_modules")
                    .font(tokens.font())
                    .desired_width(ui.available_width() * PATH_SHARE),
            )
            .clicked();
        if ui
            .selectable_label(
                false,
                egui::RichText::new("add exclude").color(tokens.text_active),
            )
            .clicked()
            || add
        {
            let text = m.new_exclude.trim().to_string();
            if !text.is_empty() {
                m.draft.excludes.push(text);
                m.new_exclude.clear();
            }
        }
    });
    ui.horizontal(|ui| {
        ui.checkbox(
            &mut m.draft.auto_index,
            egui::RichText::new("auto_index").color(tokens.text_main),
        )
        .on_hover_text("Watch the index roots and reindex on change");
        ui.checkbox(
            &mut m.draft.follow_symlinks,
            egui::RichText::new("follow symlinks").color(tokens.text_main),
        )
        .on_hover_text("Descend into symlinked directories while walking");
        ui.label(egui::RichText::new("workers").color(tokens.text_muted));
        ui.add(
            TextEdit::singleline(&mut m.worker_text)
                .font(tokens.font())
                .desired_width(tokens.cell_padding * WORKER_FIELD_PADDINGS),
        );
    });
    ui.horizontal(|ui| {
        if ui
            .selectable_label(
                false,
                egui::RichText::new("Apply").color(tokens.text_active),
            )
            .on_hover_text("Save the index configuration")
            .clicked()
        {
            m.error = m.sync_draft().err();
            if m.error.is_none() {
                m.requests.push(ManagerRequest::Apply);
            }
        }
        if ui
            .selectable_label(
                false,
                egui::RichText::new("Revert").color(tokens.text_muted),
            )
            .on_hover_text("Discard the edits and re-read the saved configuration")
            .clicked()
        {
            m.requests.push(ManagerRequest::Revert);
        }
    });
}

/// Open the dialog (plan §5.8's row): a *new* one, reading the live config, so the draft is the
/// value the engine will refuse or accept.
pub fn open(app: &mut ZenApp, slot: PaneSlot) {
    let config = app.session.index_config();
    let status = app.session.index_status();
    app.open_dialog(Dialog::SearchManager(Box::new(SearchManager::new(
        slot, config, status,
    ))));
}

/// Per-frame non-drawing work (plan §5.8's `tick`): the 1 s status poll and the debounced query.
pub fn tick(app: &mut ZenApp, ctx: &egui::Context) {
    let now = Instant::now();
    let poll_due = match app.dialogs.active_mut() {
        Some(Dialog::SearchManager(m)) => {
            m.refresh
                || m.polled
                    .is_none_or(|at| now.duration_since(at) >= STATUS_POLL)
        }
        _ => return,
    };
    if poll_due {
        let status = app.session.index_status();
        let report = watch_report(app);
        if let Some(Dialog::SearchManager(m)) = app.dialogs.active_mut() {
            m.refresh = false;
            m.polled = Some(now);
            m.watch = report;
            let indexing = status.is_indexing;
            let was_indexing = m.status.is_indexing;
            m.status = status;
            if m.rebuild.is_some() && !indexing {
                // The job reached a terminal state on its own; the panel keeps the progress line for
                // the frame the event lands, then forgets the job.
                m.rebuild = None;
                m.progress_stage = None;
            }
            if !was_indexing && indexing {
                // 15c D5: the previous rebuild's terminal toast is history the moment a new one is
                // running — the frame must not say `indexing` beside `462 row(s) indexed`.
                app.notices.dismiss_containing("row(s) indexed");
            }
            // 15c D4: the bar follows the *job*, whose `JobInfo` carries both the fraction core
            // computed and the current item (`JobProgress::current`).
            let info = m.rebuild.and_then(|job| app.session.job_info(job));
            m.progress_fraction = info
                .as_ref()
                .filter(|job| job.state == "running")
                .map(|job| job.progress.clamp(0.0, 1.0));
            m.progress_stage = info
                .as_ref()
                .map(|job| job.message.clone())
                .filter(|message| !message.is_empty());
        }
        ctx.request_repaint_after(STATUS_POLL);
    }
    let req = match app.dialogs.active_mut() {
        Some(Dialog::SearchManager(m)) => m.take_pending_request(),
        _ => return,
    };
    let Some((query, page, limit)) = req else {
        return;
    };
    let offset = page.saturating_mul(limit);
    let origin = app.session.next_origin();
    if let Some(Dialog::SearchManager(m)) = app.dialogs.active_mut() {
        m.origin = Some(origin);
        m.clear_rows();
    }
    let offset_i64 = i64::try_from(offset).unwrap_or(i64::MAX);
    if let Some(Dialog::SearchManager(m)) = app.dialogs.active_mut() {
        m.dispatched += 1;
    }
    match app
        .session
        .index_search(&query, i64::try_from(limit).ok(), Some(offset_i64))
    {
        Ok(result) => {
            if let Some(Dialog::SearchManager(m)) = app.dialogs.active_mut() {
                m.deliver(origin, &result);
            }
        }
        Err(e) => {
            let text = e.user_message();
            if let Some(Dialog::SearchManager(m)) = app.dialogs.active_mut() {
                m.error = Some(text);
                m.origin = None;
            }
        }
    }
}

/// Take a status refresh out of the event stream (the `jobs_ui::tick` seam): the panel follows the
/// job, not the timer.
pub fn notify(app: &mut ZenApp) {
    if let Some(Dialog::SearchManager(m)) = app.dialogs.active_mut() {
        m.refresh = true;
    }
}

/// Run the dialog's requests (the shell's half of the one-way door: this is where the engine is
/// called, after the draw, exactly like the pane commands).
pub fn drain(app: &mut ZenApp) {
    let requests = match app.dialogs.active_mut() {
        Some(Dialog::SearchManager(m)) => std::mem::take(&mut m.requests),
        _ => return,
    };
    for request in requests {
        match request {
            ManagerRequest::Rebuild => match app.session.index_rebuild() {
                Ok(job) => {
                    // 15c D5: same rule as the poll's, applied the frame the button was pressed.
                    app.notices.dismiss_containing("row(s) indexed");
                    if let Some(Dialog::SearchManager(m)) = app.dialogs.active_mut() {
                        m.rebuild = Some(job);
                        m.error = None;
                        m.refresh = true;
                    }
                }
                Err(e) => report(app, &e.user_message()),
            },
            ManagerRequest::Cancel => {
                let cancelled = app.session.index_cancel();
                if let Some(Dialog::SearchManager(m)) = app.dialogs.active_mut() {
                    if cancelled {
                        m.rebuild = None;
                    }
                    m.refresh = true;
                }
            }
            ManagerRequest::Apply => {
                let draft = match app.dialogs.active_mut() {
                    Some(Dialog::SearchManager(m)) => m.sync_draft(),
                    _ => continue,
                };
                let draft = match draft {
                    Ok(draft) => draft,
                    Err(msg) => {
                        report(app, &msg);
                        continue;
                    }
                };
                let before = app.session.index_config();
                let origin = app.session.next_origin();
                match app.session.set_index_config(draft, origin) {
                    Ok(applied) => {
                        // A root change moves what every pane may need to show: the *invalidation*
                        // path, the same queue `DirChanged` fills — not a manual refresh.
                        if applied.roots != before.roots {
                            invalidate_all(app);
                        }
                        if let Some(Dialog::SearchManager(m)) = app.dialogs.active_mut() {
                            m.draft = applied;
                            m.worker_text = m.draft.worker_count.to_string();
                            m.error = None;
                            m.refresh = true;
                        }
                    }
                    Err(e) => report(app, &e.user_message()),
                }
            }
            ManagerRequest::Revert => {
                let cfg = app.session.index_config();
                if let Some(Dialog::SearchManager(m)) = app.dialogs.active_mut() {
                    m.worker_text = cfg.worker_count.to_string();
                    m.draft = cfg;
                    m.error = None;
                }
            }
            ManagerRequest::WatchStart => {
                let outcome = app.session.watch_start();
                match outcome {
                    Ok(count) => {
                        if let Some(Dialog::SearchManager(m)) = app.dialogs.active_mut() {
                            m.watch_error = None;
                            let _ = count;
                        }
                    }
                    // The typed refusal, painted (`CoreError::Watcher`, plan §5.5's stub row: no
                    // feature, no backend, no roots, over budget — all one honest message).
                    Err(e) => report_watch(app, &e.user_message()),
                }
            }
            ManagerRequest::WatchStop => {
                app.session.watch_stop();
                if let Some(Dialog::SearchManager(m)) = app.dialogs.active_mut() {
                    m.watch_error = None;
                }
            }
        }
    }
}

/// Refuse a config/action value **inline**, and leave the saved value alone.
fn report(app: &mut ZenApp, msg: &str) {
    if let Some(Dialog::SearchManager(m)) = app.dialogs.active_mut() {
        m.error = Some(msg.to_string());
    }
}

/// Refuse a watch action **inline**, in its own lane so a watch failure never hides a config error.
fn report_watch(app: &mut ZenApp, msg: &str) {
    if let Some(Dialog::SearchManager(m)) = app.dialogs.active_mut() {
        m.watch_error = Some(msg.to_string());
    }
}

/// Mark every open tab stale (the reload queue `DirChanged` fills, drained by `ZenApp::reload_stale`
/// on the next `logic`).
fn invalidate_all(app: &mut ZenApp) {
    let paths: Vec<FsPath> = app
        .model
        .panes
        .values()
        .flat_map(|pane| pane.tabs.iter().map(|tab| tab.path.clone()))
        .collect();
    for path in paths {
        app.model.mark_stale(&path);
    }
}

/// The policy's name, as the status block prints it.
#[must_use]
fn watch_policy_text(policy: WatchPolicy) -> &'static str {
    match policy {
        WatchPolicy::Off => "off",
        WatchPolicy::AutoIndex => "auto",
        WatchPolicy::On => "on",
    }
}

/// The watch report the status block prints — kept out of `show` so the drawing reads as a list of
/// panels, and so the `Session` reads happen once per frame.
#[must_use]
pub fn watch_report(app: &ZenApp) -> WatchReport {
    WatchReport {
        running: app.session.watch_is_running(),
        count: app.session.watch_count(),
        policy: app.session.watch_policy(),
        failed: app.session.failed_roots().len(),
        unwatched: app.session.unwatched().len(),
    }
}

/// What the watcher is doing, in the shape the panel prints.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WatchReport {
    /// Is a reader thread running?
    pub running: bool,
    /// How many descriptors it holds.
    pub count: usize,
    /// The policy that let it start.
    pub policy: WatchPolicy,
    /// Roots the backend refused.
    pub failed: usize,
    /// Subtrees over the descriptor budget.
    pub unwatched: usize,
}
