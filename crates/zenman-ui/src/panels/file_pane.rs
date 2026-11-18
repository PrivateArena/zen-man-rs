//! The file pane (plan §5.8 `file_pane.rs` row): the column header, the list, the grid, the
//! pagination banner and the empty row.
//!
//! The pane is the shell's biggest view and the smallest possible brain. It owns the **visual
//! order** (which rows exist, in what sequence, which are hidden behind a collapsed group) and the
//! **selection** (which is what that order ranges over), and nothing else. Every question with an
//! answer in the engine leaves as a [`PaneCommand`] — navigation, opening, sorting, paging,
//! refreshing — and the caller (`dock_content`'s adapter, then `ZenApp`) turns it into a `Session`
//! call.
//!
//! # Where each Go behaviour went
//!
//! | Thing | Go (`frontend/js/file-list.js`) | Here |
//! |---|---|---|
//! | Row | 40 px, `8px 20px`, four flex cells (`:69`, `file-list.css:31-62`) | the same, from `theme::Tokens` |
//! | Size cell | `40 B (undefined files)` for an un-warmed directory (`:159-165`) | `--` ([`entry_row::size_cell`]) |
//! | Sort | `data-sort` in the markup, **no click handler** | a header that sorts the loaded rows locally, dirs first in both directions |
//! | Selection | path-keyed set + `anchorPath` (`:475-516`) | [`Selection`], with the anchor kept by key (origin-preserving) |
//! | Pagination | a banner at the end of the list with two buttons (`:179-190`) | the same, *and* scrolling onto it fires `LoadMore` once |
//! | Grid | `minmax(110px,1fr)` tiles, `gap:15px` | the same rule, virtualised by tile row |
//! | Groups | `grouped` flat view with `▶`/`▼` and counts (`:71-96`) | the same |
//! | Empty | `Directory is empty` (`:104`) | the same |

use std::collections::BTreeSet;
use std::sync::Arc;

use egui::{Id, Pos2, Ui};

use zenman_core::entry::{DirPage, FileEntry};
use zenman_core::metrics::FileSizes;
use zenman_core::path::{AbsPath, FsPath};
use zenman_core::store::config::{PaneSlot, SortColumn, ViewMode};

use crate::dock::DockId;
use crate::jobs_ui::JobSlot;
use crate::state::{FlatMode, Selection, Tab};
use crate::theme::Tokens;
use crate::widgets::entry_row::{self, SelectionState, SizeLookup};
use crate::widgets::virtual_list::{
    self, GridSpec, ListSpec, RowColumns, RowEvent, RowHit, RowMetrics, ScrollState, SelKind,
};

/// What the pane asked the shell to do (plan row: `PaneCommand`).
#[derive(Clone, Debug, PartialEq)]
pub enum PaneCommand {
    /// List this path in this tab.
    Navigate(FsPath),
    /// Step the tab's history back.
    Back,
    /// …forward.
    Forward,
    /// `..` — the parent of the listed path.
    Up,
    /// The address bar held something [`FsPath::resolve`] refused (plan §4.2: that *is* the guard).
    BadAddress(String),
    /// One row was opened (Enter or double-click): a directory enters it, a file opens it.
    Open {
        /// Index into the listing, not the visual order.
        ix: usize,
    },
    /// Right-click, on a row or on the background.
    Context {
        /// The row under the cursor, `None` for the empty area.
        ix: Option<usize>,
        /// Where the menu should open.
        at: Pos2,
    },
    /// A column header was clicked.
    Sort(SortColumn),
    /// List ⇄ grid.
    ToggleGrid,
    /// The flat-view dropdown.
    SetFlat(FlatMode),
    /// One more page (`LoadMore` in the plan row).
    LoadMore,
    /// Every remaining row (Go's `loadMoreFiles(paneId, true)`).
    LoadAll,
    /// Re-read the directory (`F5`).
    Refresh,
    /// A new tab in this pane.
    NewTab,
    /// Show the tab at this index in the pane.
    SelectTab(usize),
    /// Close the tab at this index.
    CloseTab(usize),
    /// Select every loaded row (Go's `selectAll`, `file-list.js:679-691`).
    SelectAll,
    /// Clear the selection (Go's `Esc`, which its key registry never wired).
    SelectNone,
}

/// One row of the visual order: a listing row, or a group header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VisualRow {
    /// `ix` indexes [`DirPage::entries`].
    Entry(usize),
    /// A flat listing's group header, with its item count.
    Group {
        /// The group's key (its listing-relative directory, `""` for the listing root).
        key: String,
        /// Rows under the header.
        count: usize,
    },
}

/// One frame's visual order: the rows the pane shows, in the sequence the user sees.
///
/// It owns an `Arc` clone of the page — never a copy of it — plus, in the grouped case, the
/// header/entry index built once per frame from the loaded page. That index is O(loaded rows), and
/// loaded rows are bounded by the flat-walk cap (`Limits::flat_walk_cap`, Go's 10 000), which is why
/// it cannot be driven by an arbitrary directory size.
#[derive(Clone, Debug)]
pub struct RowOrder {
    page: Arc<DirPage>,
    rows: Vec<VisualRow>,
    grouped: bool,
}

impl RowOrder {
    /// Whichever order the tab is in.
    #[must_use]
    pub fn build(tab: &Tab) -> Self {
        let page = Arc::clone(&tab.entries);
        if tab.flat != FlatMode::Grouped {
            return Self {
                page,
                rows: Vec::new(),
                grouped: false,
            };
        }
        Self {
            page,
            rows: group_index(&tab.entries.entries, &tab.collapsed_groups),
            grouped: true,
        }
    }

    /// Rows on screen, headers included.
    #[must_use]
    pub fn len(&self) -> usize {
        if self.grouped {
            self.rows.len()
        } else {
            self.page.entries.len()
        }
    }

    /// `true` when the listing has no rows at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The visual row at `ix`, borrowed. `None` past the end — the plain order has one row per
    /// listing entry, the grouped one has headers too.
    ///
    /// It used to hand back a `VisualRow::Entry(usize::MAX)` sentinel for the plain order, which any
    /// caller that trusted it would have turned into an out-of-bounds listing index; the borrowed
    /// enum says what it means and costs nothing.
    #[must_use]
    pub fn get(&self, ix: usize) -> Option<VisualRowRef<'_>> {
        if self.grouped {
            match *self.rows.get(ix)? {
                VisualRow::Entry(entry_ix) => Some(VisualRowRef::Entry(entry_ix)),
                VisualRow::Group { ref key, count } => Some(VisualRowRef::Group { key, count }),
            }
        } else {
            (ix < self.page.entries.len()).then_some(VisualRowRef::Entry(ix))
        }
    }

    /// The listing index a visual row shows, `None` for a header.
    #[must_use]
    pub fn entry_ix(&self, ix: usize) -> Option<usize> {
        if self.grouped {
            match *self.rows.get(ix)? {
                VisualRow::Entry(entry_ix) => Some(entry_ix),
                VisualRow::Group { .. } => None,
            }
        } else {
            (ix < self.page.entries.len()).then_some(ix)
        }
    }

    /// The entry a visual row shows, `None` for a header.
    #[must_use]
    pub fn entry_at(&self, ix: usize) -> Option<&FileEntry> {
        self.entry_ix(ix)
            .and_then(|entry_ix| self.page.entries.get(entry_ix))
    }

    /// The selection key of a visual row — `None` for a group header, which is not selectable
    /// (Go's `getOrderedSelectablePaths`, `file-list.js:451-471`, skips headers too).
    #[must_use]
    pub fn key_at(&self, ix: usize) -> Option<&str> {
        self.entry_at(ix).map(Tab::row_key)
    }

    /// The selected rows' names, in display order — the status bar's tooltip, and the fix to Go's
    /// insertion-ordered `Array.from(set)` (`file-list.js:569`).
    #[must_use]
    pub fn selected_names(&self, sel: &Selection) -> Vec<String> {
        (0..self.len())
            .filter_map(|ix| self.entry_at(ix))
            .filter(|entry| sel.contains(Tab::row_key(entry)))
            .map(|entry| entry.name.clone())
            .collect()
    }

    /// The keys of every selectable row, in display order.
    #[must_use]
    pub fn keys(&self) -> Vec<String> {
        (0..self.len())
            .filter_map(|ix| self.key_at(ix))
            .map(str::to_string)
            .collect()
    }
}

/// [`VisualRow`] without the `String`: what [`RowOrder::get`] answers, borrowing the group key out
/// of the order instead of cloning it per painted row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisualRowRef<'a> {
    /// `ix` indexes [`DirPage::entries`].
    Entry(usize),
    /// A flat listing's group header.
    Group {
        /// The group's key.
        key: &'a str,
        /// Rows under the header.
        count: usize,
    },
}

/// The group index of a flat listing.
fn group_index(entries: &[FileEntry], collapsed: &BTreeSet<String>) -> Vec<VisualRow> {
    let mut rows = Vec::new();
    let mut index = 0usize;
    while index < entries.len() {
        let key = group_of(&entries[index]);
        let mut count = 0usize;
        while index < entries.len() && group_of(&entries[index]) == key {
            count += 1;
            index += 1;
        }
        let is_collapsed = collapsed.contains(&key);
        rows.push(VisualRow::Group { key, count });
        if is_collapsed {
            continue;
        }
        rows.extend((index - count..index).map(VisualRow::Entry));
    }
    rows
}

/// The group a flat row belongs to: the directory half of its listing-relative path, `""` for the
/// listing root (Go's `.`, whose header is labelled `Root` — `file-list.js:74-79`).
#[must_use]
pub fn group_of(entry: &FileEntry) -> String {
    match entry.rel_path.rfind('/') {
        Some(slash) => entry.rel_path[..slash].to_string(),
        None => String::new(),
    }
}

/// Everything the pane draws from (plan row's `PaneModel`, over the as-built `Tab` rather than a
/// second copy of its fields — a `PaneModel` of borrowed halves could not re-sort or select).
pub struct PaneModel<'a> {
    /// Which pane this is; commands come back addressed with it.
    pub slot: PaneSlot,
    /// The shown tab.
    pub tab: &'a mut Tab,
    /// The tab is loading its first page.
    pub loading: bool,
    /// The pane's live jobs (Go had none; the banner's wording uses it).
    pub jobs: &'a [JobSlot],
    /// The directory-size answers, from `DirService`'s cache (decision D11).
    pub sizes: &'a dyn Fn(&AbsPath) -> Option<FileSizes>,
    /// The row count the last frame showed, for the status bar.
    pub visible_rows: usize,
}

/// Draw one pane body, returning the commands the shell must carry out.
pub fn show(
    ui: &mut Ui,
    panel: &DockId,
    model: &mut PaneModel<'_>,
    tokens: &Tokens,
) -> Vec<PaneCommand> {
    let mut out = Vec::new();
    let _ = panel;
    toolbar(ui, model, tokens, &mut out);
    header_row(ui, model, tokens, &mut out);
    if let Some(error) = &model.tab.error {
        error_row(ui, tokens, &error.user_message());
    }
    if model.tab.entries.entries.is_empty() {
        empty_row(ui, tokens, model.loading);
        return out;
    }
    let order = RowOrder::build(model.tab);
    // The banner is painted *after* the scroller, so the scroller must leave its space: a
    // `has_more` pane reserves [`Tokens::load_more_height`] and the banner lands inside it.
    // Unreserved, the banner falls off the bottom of the window, where its buttons sit under
    // later-allocated chrome and eat nothing (plan §9.2 step 8: last-allocated rect wins).
    let reserve = if model.tab.has_more {
        tokens.load_more_height
    } else {
        0.0
    };
    let list_height = (ui.available_height() - reserve).max(tokens.row_height);
    let is_grid = model.tab.view == ViewMode::Grid;
    let events = ui
        .allocate_ui(egui::vec2(ui.available_width(), list_height), |ui| {
            if is_grid {
                grid(ui, model, tokens)
            } else {
                list(ui, model, tokens)
            }
        })
        .inner;
    model.visible_rows = order.len();
    apply_events(model, events, &order, &mut out);
    if model.tab.has_more {
        banner(ui, model, tokens, &mut out);
    }
    out
}

/// The pane toolbar: view, flat mode, refresh, select-all / select-none.
///
/// Go kept these in a 50 px toolbar per pane (`main.js`'s `pane-toolbar`) with a flat-view dropdown
/// and a grid checkbox; the same controls, in the same order, as tokens.
fn toolbar(ui: &mut Ui, model: &PaneModel<'_>, tokens: &Tokens, out: &mut Vec<PaneCommand>) {
    let grid = model.tab.view == ViewMode::Grid;
    let flat = model.tab.flat;
    ui.horizontal(|ui| {
        let toggle = if grid { "list" } else { "grid" };
        let tip = if grid {
            "Switch to the list view"
        } else {
            "Switch to the grid view"
        };
        if tool_icon(ui, tokens, toggle, tip).clicked() {
            out.push(PaneCommand::ToggleGrid);
        }
        if tool_icon(ui, tokens, "refresh", "Re-read the directory (F5)").clicked() {
            out.push(PaneCommand::Refresh);
        }
        flat_buttons(ui, flat, out, tokens);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .button("Select none")
                .on_hover_text("Clear the selection")
                .clicked()
            {
                out.push(PaneCommand::SelectNone);
            }
            if ui
                .button("Select all")
                .on_hover_text("Select every loaded row")
                .clicked()
            {
                out.push(PaneCommand::SelectAll);
            }
        });
    });
}

/// The column header: four sortable cells, the active one carrying the arrow.
///
/// Go's markup had `data-sort="name|size|date"` and **no handler at all** — plan §2.3's bug list,
/// §6.2's "Sort is clickable". Clicking the active column flips the direction; the rows re-sort
/// locally (`PaneCommand::Sort` → [`Tab::resort`]), which is what keeps the scroll position.
fn header_row(ui: &mut Ui, model: &PaneModel<'_>, tokens: &Tokens, out: &mut Vec<PaneCommand>) {
    let columns = columns_of(model.tab.flat);
    let sort = model.tab.sort;
    let (row_rect, _) = ui.allocate_at_least(
        egui::vec2(ui.available_width(), tokens.header_row_height),
        egui::Sense::hover(),
    );
    if ui.is_rect_visible(row_rect) {
        ui.painter().rect_filled(row_rect, 0.0, tokens.bg_card);
        ui.painter()
            .hline(row_rect.x_range(), row_rect.max.y - 1.0, tokens.hairline());
    }
    let cells = entry_row::column_rects(row_rect, &columns, tokens);
    // Go's own four labels (`index.html:188-191`): *Name*, *Relative Path*, *Size*, *Modified*. The
    // rel-path column is **not** a sort control — Go's markup gives it no `data-sort`, and core has
    // no "relative path" sort column (`SortColumn` is `Name|Size|Date|Mode|Ext`, and its `Ext` arm
    // sorts by name, `api/dir.go:315`), so labelling it "Path" and making it sortable would promise
    // a sort that cannot happen. It is a header, like Go's.
    let labels: [(&str, Option<SortColumn>); 4] = [
        ("Name", Some(SortColumn::Name)),
        ("Relative Path", None),
        ("Size", Some(SortColumn::Size)),
        ("Modified", Some(SortColumn::Date)),
    ];
    for (rect, (label, column)) in cells.iter().zip(labels) {
        let Some(column) = column else {
            if columns.rel > 0.0 {
                // The flat listing's non-sortable path header (`file-list.css:206-211`) —
                // painted, never allocated: decoration that allocates a rect over an interactive
                // one steals the click in this egui fork (plan §9.2 step 8, the row-painting rule).
                let mut job = egui::text::LayoutJob::simple(
                    (*label).to_owned(),
                    egui::FontId::proportional(tokens.font_size - 1.0),
                    tokens.text_muted,
                    rect.width().max(1.0),
                );
                job.wrap = egui::text::TextWrapping {
                    max_width: rect.width().max(1.0),
                    max_rows: 1,
                    break_anywhere: true,
                    overflow_character: Some('…'),
                    ..Default::default()
                };
                let galley = ui.painter().layout_job(job);
                ui.painter().galley(
                    egui::pos2(rect.min.x, rect.center().y - galley.size().y / 2.0),
                    galley,
                    tokens.text_muted,
                );
            }
            continue;
        };
        let active = sort.column == column;
        let glyph = if !active {
            ""
        } else if sort.descending {
            " ▼"
        } else {
            " ▲"
        };
        let button = egui::Button::new(
            egui::RichText::new(format!("{label}{glyph}"))
                .color(if active {
                    tokens.text_main
                } else {
                    tokens.text_muted
                })
                .size(tokens.font_size - 1.0)
                .strong(),
        )
        .frame(false);
        let text = ui.put(*rect, button);
        let response = text
            .clone()
            .on_hover_text(format!("Sort by {label} (click again to reverse)"));
        if response.clicked() {
            out.push(PaneCommand::Sort(column));
        }
    }
}

/// A toolbar icon button: its own square in the horizontal layout (not the pane's centre — the
/// icon is allocated, then painted inside that rect, exactly as the header's buttons do).
fn tool_icon(ui: &mut Ui, tokens: &Tokens, icon: &str, tooltip: &str) -> egui::Response {
    let side = tokens.row_icon_size;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let tint = if response.hovered() {
            tokens.text_main
        } else {
            tokens.text_muted
        };
        crate::icons::paint(ui, rect, icon, side, tint);
    }
    response.on_hover_text(tooltip)
}

/// The recursive-listing buttons, in Go's own words (`main.js`'s flat-view dropdown: *mixed*,
/// *mixed_no_folders*, *grouped*).
///
/// They are **disabled**, and the tooltip says why: the walk is the engine's, and `ListDir` has no
/// `flat` field until step 9, so a click here could only pretend to fetch. Everything *downstream*
/// of the fetch is live and tested — [`FlatMode`], the group index in [`RowOrder`], the `FLAT`
/// column table. Recorded in plan §9.2's step-8 row as the one §7-row-8 sub-item that step 9
/// finishes.
fn flat_buttons(ui: &mut Ui, flat: FlatMode, out: &mut Vec<PaneCommand>, tokens: &Tokens) {
    let _ = (out, tokens);
    let reason = "the recursive listing needs the engine's flat walk, which lands with the step-9 \
                  write path (`ListDir::flat`); the view state, grouping and columns are live now";
    for (mode, label) in [
        (FlatMode::Mixed, "Mixed"),
        (FlatMode::MixedNoFolders, "No folders"),
        (FlatMode::Grouped, "Grouped"),
    ] {
        // Disabled, and worded: `flat` is *view* state the pane can hold (and the group index and
        // the `FLAT` column table follow it), but no listing can be *fetched* in this mode until
        // step 9 gives `ListDir` its `flat` field.
        let selected = flat == mode;
        ui.add_enabled_ui(false, |ui| {
            let _ = ui.selectable_label(selected, label).on_hover_text(reason);
        });
    }
}

/// The flex table this listing uses: the flat columns appear exactly when rows carry a `rel_path`
/// (`file-list.css:204-224`).
#[must_use]
pub fn columns_of(flat: FlatMode) -> RowColumns {
    match flat {
        FlatMode::Off => RowColumns::LIST,
        FlatMode::Mixed | FlatMode::MixedNoFolders | FlatMode::Grouped => RowColumns::FLAT,
    }
}

/// The row list.
fn list(ui: &mut Ui, model: &mut PaneModel<'_>, tokens: &Tokens) -> Vec<RowEvent> {
    let metrics = row_metrics(tokens, model.tab.flat);
    let mut scroll = take_scroll(model.tab);
    let order = RowOrder::build(model.tab);
    let tab: &Tab = model.tab;
    let sizes = model.sizes;
    let slot = model.slot;
    let events = virtual_list::show_list(
        ui,
        ListSpec {
            id: pane_id(slot),
            rows: order.len(),
            metrics: &metrics,
            has_more: tab.has_more,
        },
        |ix| row_id(slot, ix),
        |ui, ix, hit| paint_row(ui, hit, ix, &order, tab, sizes, tokens),
        &mut scroll,
    );
    model.tab.scroll = scroll;
    events
}

/// The icon grid.
fn grid(ui: &mut Ui, model: &mut PaneModel<'_>, tokens: &Tokens) -> Vec<RowEvent> {
    let metrics = row_metrics(tokens, model.tab.flat);
    let mut scroll = take_scroll(model.tab);
    let order = RowOrder::build(model.tab);
    let tab: &Tab = model.tab;
    let sizes = model.sizes;
    let slot = model.slot;
    let events = virtual_list::show_grid(
        ui,
        GridSpec {
            base: ListSpec {
                id: pane_id(slot),
                rows: order.len(),
                metrics: &metrics,
                has_more: tab.has_more,
            },
            min_tile: tokens.grid_tile_min,
            gap: tokens.grid_gap,
        },
        |ix| row_id(slot, ix),
        |ui, ix, hit| paint_row(ui, hit, ix, &order, tab, sizes, tokens),
        &mut scroll,
    );
    model.tab.scroll = scroll;
    events
}

/// Draw one visual row and read its click.
fn paint_row(
    ui: &mut Ui,
    hit: &RowHit<'_>,
    ix: usize,
    order: &RowOrder,
    tab: &Tab,
    sizes: &dyn Fn(&AbsPath) -> Option<FileSizes>,
    tokens: &Tokens,
) -> Option<RowEvent> {
    let Some(entry) = order.entry_at(ix) else {
        // A group header row.
        let Some(VisualRowRef::Group { key, count }) = order.get(ix) else {
            return None;
        };
        let collapsed = tab.collapsed_groups.contains(key);
        entry_row::paint_group_header(
            ui,
            hit,
            if key.is_empty() { "Root" } else { key },
            count,
            collapsed,
            tokens,
        );
        return hit.response.clicked().then(|| RowEvent::GroupToggle {
            key: key.to_string(),
        });
    };
    let entry_ix = order.entry_ix(ix)?;
    let key = Tab::row_key(entry);
    let sel = SelectionState::of(tab.sel.contains(key), false);
    let path = tab.row_abs(entry);
    if tab.view == ViewMode::Grid {
        entry_row::paint_grid_tile(ui, hit, entry, sel, tokens);
    } else {
        entry_row::paint_list_row(
            ui,
            hit,
            entry_row::RowSpec {
                entry,
                path: path.as_ref(),
                sel,
            },
            &hit.metrics.columns,
            &SizeLookup(sizes),
            tokens,
        );
    }
    // The decoration paints and never allocates, so the row's a11y node comes from here: one
    // node per row, labelled with the entry name (`Response::widget_info`, plan §9.2 step 8).
    let name = entry.name.clone();
    hit.response.widget_info(|| {
        let mut info = egui::WidgetInfo::new(egui::Role::Button);
        info.label = Some(name.as_str().into());
        info
    });
    let _ = key;
    row_event(hit, entry_ix)
}

/// The row's own click, before the pane interprets it.
#[must_use]
pub fn row_event(hit: &RowHit<'_>, ix: usize) -> Option<RowEvent> {
    if hit.response.double_clicked() {
        return Some(RowEvent::Activate { ix });
    }
    if hit.response.secondary_clicked() {
        return Some(RowEvent::Context {
            ix: Some(ix),
            at: hit.response.interact_pointer_pos().unwrap_or(hit.rect.min),
        });
    }
    if hit.response.clicked() {
        return Some(RowEvent::Select {
            ix,
            kind: SelKind::from_modifiers(&hit.modifiers),
        });
    }
    if hit.response.drag_started() {
        return Some(RowEvent::DndGrab { ix });
    }
    None
}

/// Apply what the rows said: selection here (it is the pane's own state), commands to the caller.
fn apply_events(
    model: &mut PaneModel<'_>,
    events: Vec<RowEvent>,
    order: &RowOrder,
    out: &mut Vec<PaneCommand>,
) {
    let mut sel = std::mem::take(&mut model.tab.sel);
    for event in events {
        match event {
            RowEvent::Select { ix, kind } => sel.apply(kind, ix, order.len(), |i| order.key_at(i)),
            RowEvent::Activate { ix } => out.push(PaneCommand::Open { ix }),
            RowEvent::Context { ix, at } => out.push(PaneCommand::Context { ix, at }),
            RowEvent::LoadMore => out.push(PaneCommand::LoadMore),
            RowEvent::GroupToggle { key } => {
                // A group header is pure view state: it toggles here, no command needed.
                if !model.tab.collapsed_groups.remove(&key) {
                    model.tab.collapsed_groups.insert(key);
                }
            }
            RowEvent::DndGrab { ix } => {
                // Step 9's payload goes through `DockContent::resolve_drag`; the pane only records
                // which row the grab started on, so the drag has an honest source to claim.
                model.tab.drag_row = Some(ix);
            }
        }
    }
    model.tab.sel = sel;
}

/// The `! Loading limited…` banner (`file-list.js:179-190`, `file-list.css:115-146`).
///
/// Go's marker is `⚠️`; the port paints `!` because the installed font stack (Ubuntu-Light,
/// egui-icons, Hack — plan §9.2 step-7c row) has **no glyph for U+26A0**, and this fork of egui
/// *panics* on a missing glyph rather than drawing tofu (`epaint/src/text/fonts.rs:825-851`). The
/// warning colour and stroke carry the meaning; `theme::fonts::tests` guards the glyph set.
fn banner(ui: &mut Ui, model: &PaneModel<'_>, tokens: &Tokens, out: &mut Vec<PaneCommand>) {
    let remaining = model.tab.rows_left();
    let loaded = model.tab.entries.entries.len();
    egui::Frame::new()
        .fill(tokens.banner_bg)
        .stroke(egui::Stroke::new(1.0, tokens.warning))
        .inner_margin(egui::Margin::symmetric(
            tokens.cell_padding as i8,
            tokens.row_pad_y as i8,
        ))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "! Loading limited to {loaded} items to prevent interface lag. \
                         {remaining} more are available."
                    ))
                    .color(tokens.warning)
                    .size(tokens.font_size - 1.0),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button("Load all items (may cause lag)")
                        .on_hover_text("Load every remaining row at once (may be slow)")
                        .clicked()
                    {
                        out.push(PaneCommand::LoadAll);
                    }
                    if ui
                        .button(format!("Load next {} items", model.tab.page_size))
                        .on_hover_text("Load one more page")
                        .clicked()
                    {
                        out.push(PaneCommand::LoadMore);
                    }
                });
            });
        });
}

/// The "Directory is empty" row (`file-list.js:104`).
fn empty_row(ui: &mut Ui, tokens: &Tokens, loading: bool) {
    let text = if loading {
        "Loading…"
    } else {
        "Directory is empty"
    };
    ui.add_space(tokens.empty_pane_space);
    ui.vertical_centered(|ui| {
        ui.label(egui::RichText::new(text).color(tokens.text_muted));
    });
}

/// The tab's error, shown *in this tab only* (plan §5.6: this tab only). `!`, not `⚠`: see
/// [`banner`] for why the warning triangle is not drawable here.
fn error_row(ui: &mut Ui, tokens: &Tokens, text: &str) {
    ui.label(
        egui::RichText::new(format!("! {text}"))
            .color(tokens.danger)
            .size(tokens.font_size - 1.0),
    );
}

/// The row geometry, straight off the token table (the plan's "show the row-height token").
#[must_use]
pub fn row_metrics(tokens: &Tokens, flat: FlatMode) -> RowMetrics {
    RowMetrics {
        height: tokens.row_height,
        overscan: tokens.overscan_rows,
        columns: columns_of(flat),
    }
}

/// The scroll area's identity for one pane.
#[must_use]
pub fn pane_id(slot: PaneSlot) -> Id {
    Id::unique(("zenman-pane", slot as u8))
}

/// One row's identity: stable for a given listing position, and per pane, so two panes on one
/// directory cannot share a hover state.
#[must_use]
pub fn row_id(slot: PaneSlot, ix: usize) -> Id {
    Id::unique(("zenman-row", slot as u8, ix))
}

/// Move the scroll state out of the tab and back in, so the scroller can take `&mut ScrollState`
/// while the paint callback reads the tab.
fn take_scroll(tab: &mut Tab) -> ScrollState {
    std::mem::take(&mut tab.scroll)
}
