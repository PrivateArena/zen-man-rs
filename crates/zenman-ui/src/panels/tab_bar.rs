//! The tab strip (plan §5.8 `tab_bar.rs` row): the coloured 38 px strip a pane owns because its
//! dock group is stripless.
//!
//! Widths come from [`craft_ui::tabs::fit`], which is the framework's answer to Go's `scrollWidth`
//! arithmetic (`tabs.js` measured the strip in DOM units and could push the `+` button outside the
//! pane). The plan's Done-when — "40 tabs: `fit` puts the overflow list and the `+` button in the
//! strip at all widths, `+` stays inside the pane bounds" — is
//! [`tests::forty_tabs_never_lose_the_new_tab_button`].
//!
//! # The strip is a list of *items*, not of tabs
//!
//! A tab group is one **chip** at the head of its run, so what the strip lays out is
//! [`StripItem::Tab`] and [`StripItem::Group`] ([`items`]). Two rules fall out of that, and both are why
//! [`layout`] is a function of its own, tested apart from the painting:
//!
//! * **a chip is never evicted.** `fit` keeps only the *selected* item when space runs out, so a mixed
//!   list of chips and tabs would push a chip into the overflow popup, where it is worthless. Chips are
//!   sized first; the tabs then `fit` into what is left, into which the chip's own minimum never intrudes.
//! * **a fold still shows the tab on screen.** Closing a group must not hide what the pane is listing
//!   ([`crate::state::Pane`] invariant 4), so the shown tab of a folded run paints beside its chip.
//!
//! Commands and widget ids are keyed by [`TabId`]/[`GroupId`] for the same reason as above: an index into
//! the tab list is not an index into the item list, and the strip re-lays itself between the click and
//! the apply.
//!
//! Group *runs* used to be computed here (`runs`/`GroupRun`); they were never called, never tested, and
//! keyed on the per-tab colour, which is no longer identity. [`items`] is what walks the strip.

use std::collections::BTreeSet;

use craft_ui::tabs::TabFit;
use egui::{Id, Rect, RichText, Ui, pos2, vec2};

use zenman_core::store::config::GroupColor;

use crate::state::{DropTarget, GroupId, Tab, TabGroup, TabId, TabSelKind};
use crate::theme::Tokens;

/// What the strip asked for.
///
/// Every variant names a [`TabId`] or a [`GroupId`] and never an index: the command is queued for the
/// shell, and the strip re-lays itself between the click and the apply (a close, a reorder, a fold), so
/// an index would name a different tab by the time it was read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabCommand {
    /// Show this tab, and select it the way the modifiers say (`docs/tab-groups-design.md` §6): a plain
    /// click selects it alone, <kbd>Ctrl</kbd> adds or removes it, <kbd>Shift</kbd> takes the run from
    /// the anchor. That is what makes "select three tabs, right-click, *New Tab Group*" a thing the strip
    /// can ask for at all.
    Select { id: TabId, kind: TabSelKind },
    /// Close it (step 9's write path is what makes a tab dirty; the button is here now).
    Close(TabId),
    /// The tab's own menu, opened over the tab `id` names — Go's `showTabContextMenu`
    /// (`tabs.js:351-386`). `at` is the pointer, because the menu opens under the cursor and not under
    /// the tab (`positionElementSmartly`, `utils.js:21-44`).
    Context { id: TabId, at: egui::Pos2 },
    /// A drag began on this tab (`docs/tab-groups-design.md` §8). The shell owns the drag; the strip
    /// only reports where it started and what the pointer was over when it let go.
    DragStart(TabId),
    /// The drag let go over that target, or on nothing at all (`None`, which cancels it).
    DragDrop(Option<DropTarget>),
    /// A new tab in this pane.
    NewTab,
    /// Open / shut the overflow list.
    ToggleOverflow,
    /// Fold or unfold a group's run: the chip's own click.
    ToggleGroup(GroupId),
    /// The group's menu, opened over its chip.
    GroupContext { id: GroupId, at: egui::Pos2 },
}

/// One thing the strip lays out: a tab, or a group's chip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StripItem {
    /// A tab, by its index in [`TabBarModel::tabs`] — the *real* tab index, never a position in the
    /// item list. That is what lets a fold hide a tab without moving one command.
    Tab(usize),
    /// A group's chip.
    Group(GroupId),
}

impl StripItem {
    /// The tab this item paints, when it is one.
    #[must_use]
    pub fn tab(&self) -> Option<usize> {
        match self {
            Self::Tab(ix) => Some(*ix),
            Self::Group(..) => None,
        }
    }

    /// The group this item is the chip of, when it is one.
    #[must_use]
    pub fn group(&self) -> Option<GroupId> {
        match self {
            Self::Group(id) => Some(*id),
            Self::Tab(..) => None,
        }
    }
}

/// What the strip shows, in the order it paints (`docs/tab-groups-design.md` §5).
///
/// A group is one chip at the head of its run, and the tabs a fold hides are simply not items — except
/// the shown one, which stays beside the chip. Pure, and unit-tested, because the whole layout hangs off
/// this list.
#[must_use]
pub fn items(tabs: &[Tab], groups: &[TabGroup], active: usize) -> Vec<StripItem> {
    let mut out: Vec<StripItem> = Vec::with_capacity(tabs.len());
    let mut headed: Vec<GroupId> = Vec::new();
    let shown = tabs.get(active).map(|tab| tab.id);
    for (ix, tab) in tabs.iter().enumerate() {
        let Some(id) = tab.group else {
            out.push(StripItem::Tab(ix));
            continue;
        };
        let Some(group) = groups.iter().find(|group| group.id == id) else {
            // A tab naming a group this pane has not got paints as an ordinary tab rather than
            // vanishing: `Pane::debug_assert_invariants` calls that state a bug, and this is the answer
            // to it in a release build.
            out.push(StripItem::Tab(ix));
            continue;
        };
        // One chip per group, ever. Testing the *previous* item would put a second chip after the first
        // member of any run of three or more, and egui would report the two chips as an id clash.
        if !headed.contains(&id) {
            headed.push(id);
            out.push(StripItem::Group(id));
        }
        if group.collapsed && Some(tab.id) != shown {
            continue;
        }
        out.push(StripItem::Tab(ix));
    }
    out
}

/// One frame's strip geometry: the rect of every item, and the tabs that did not fit.
///
/// Exposed as **output** of [`show`] so the hover flyout and the drag hit-test read the same rects the
/// pixels came from instead of each deriving its own (design §5).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StripLayout {
    /// Every item the strip paints, left to right, with the rect it occupies.
    pub rects: Vec<(StripItem, Rect)>,
    /// The tabs the overflow popup holds, as indices into [`TabBarModel::tabs`].
    pub overflow: Vec<usize>,
    /// Width the overflow button actually took — what the strip leaves the tabs, which `fit` may shrink
    /// below [`Tokens::tab_overflow_width`] in a narrow row. The `+` and the button are placed from this.
    pub overflow_width: f32,
}

impl StripLayout {
    /// The rect of one item, when the strip painted it.
    #[must_use]
    pub fn rect_of(&self, want: StripItem) -> Option<Rect> {
        self.rects
            .iter()
            .find(|(item, _)| *item == want)
            .map(|(_, rect)| *rect)
    }

    /// Where a dragged tab let go: a chip means "join that group", and which half of a tab the pointer
    /// is in says whether it lands in front of that tab or behind it.
    #[must_use]
    pub fn drop_target(&self, at: egui::Pos2, tabs: &[Tab]) -> Option<DropTarget> {
        let (item, rect) = self.rects.iter().find(|(_, rect)| rect.contains(at))?;
        match *item {
            StripItem::Group(id) => Some(DropTarget::OntoChip(id)),
            StripItem::Tab(ix) => Some(if at.x < rect.center().x {
                DropTarget::Before(tabs.get(ix)?.id)
            } else {
                DropTarget::After(tabs.get(ix)?.id)
            }),
        }
    }

    /// The same question, with the one place outside the items that means something: the strip's own
    /// floor **after the last item** is the end of the strip ([`DropTarget::End`]), which is the only way
    /// to reach it. Anywhere else off-item is a cancel, not a guess.
    #[must_use]
    pub fn drop_target_or_end(
        &self,
        at: egui::Pos2,
        tabs: &[Tab],
        strip: Rect,
    ) -> Option<DropTarget> {
        if let Some(target) = self.drop_target(at, tabs) {
            return Some(target);
        }
        let (_, last) = self.rects.last()?;
        let past_the_tail = at.x >= last.min.x && at.x <= strip.max.x;
        let on_the_row = (at.y - last.center().y).abs() <= last.height();
        (strip.contains(at) && past_the_tail && on_the_row).then_some(DropTarget::End)
    }
}

/// The strip's input, and the three things it reports back.
pub struct TabBarModel<'a> {
    /// The pane's tabs, left to right.
    pub tabs: &'a [Tab],
    /// The shown one.
    pub active: usize,
    /// The overflow list is open.
    pub overflow_open: bool,
    /// The pane's tab groups, which is where a tab's colour comes from now that `Tab::group` names a
    /// group rather than a colour (design §2).
    pub groups: &'a [TabGroup],
    /// The tabs the shell has multi-selected: the strip paints them, it does not own them.
    pub selected: &'a BTreeSet<TabId>,
    /// Out: this frame's geometry.
    pub layout: StripLayout,
    /// Out: the chip the pointer rested on, with its rect — the hover flyout's anchor. An **output**
    /// and not a command, because a pointer rests on a chip for sixty frames a second (design §7).
    pub hovered: Option<(GroupId, Rect)>,
    /// The tab a drag is carrying, when the shell has one in flight. The strip does not own the drag —
    /// `ZenApp::tab_drag` does, because two strips can be on screen and only one pane's tabs move.
    pub dragging: Option<TabId>,
}

impl TabBarModel<'_> {
    /// The colour a tab's accent bar takes: its group's, or none.
    #[must_use]
    pub fn tab_color(&self, tab: &Tab) -> Option<GroupColor> {
        self.group_color(tab.group)
    }

    /// The colour of a group, by id.
    #[must_use]
    pub fn group_color(&self, id: Option<GroupId>) -> Option<GroupColor> {
        id.and_then(|id| self.group(id)).map(|group| group.color)
    }

    /// One group, by id.
    #[must_use]
    pub fn group(&self, id: GroupId) -> Option<&TabGroup> {
        self.groups.iter().find(|group| group.id == id)
    }
}

/// The strip's geometry pass, with no painting in it (design §5).
///
/// Chips are measured and clamped first — a group's name may cost the strip a tab, never the strip —
/// and the tabs then go through [`craft_ui::tabs::fit`], whose single `minimum_width` therefore only ever
/// shrinks tabs and whose eviction can never reach a chip.
#[must_use]
pub fn layout(ui: &Ui, m: &TabBarModel<'_>, tokens: &Tokens, strip: Rect) -> StripLayout {
    let items = items(m.tabs, m.groups, m.active);
    let measure = |text: &str, cap: f32| {
        let galley = ui
            .painter()
            .layout_no_wrap(text.to_string(), tokens.font(), tokens.text_main);
        galley.size().x.min(cap)
    };
    // 1. every chip, at its label's width, capped so one long name cannot eat the row.
    let mut chips: Vec<f32> = items
        .iter()
        .filter_map(StripItem::group)
        .filter_map(|id| m.group(id))
        .map(|group| {
            tokens.group_chevron
                + tokens.group_chip_padding * 2.0
                + measure(group.label(), tokens.group_label_max_width)
        })
        .collect();
    let button_room = tokens.tab_new_button + tokens.tab_gap;
    let room = (strip.width() - button_room).max(0.0);
    // The strip keeps `tab_strip_min_width`'s floor: the `+`, the overflow button and one narrow tab.
    let floor = tokens.tab_overflow_width + tokens.tab_min_width;
    let budget = (room - chips.len() as f32 * tokens.tab_gap - floor).max(0.0);
    let mut total: f32 = chips.iter().sum();
    if total > budget && total > 0.0 {
        // Every chip loses the same fraction of its label, never below its chevron: the chip is the
        // group's only handle, so it shrinks instead of vanishing.
        let scale = budget / total;
        let least = tokens.group_chevron + tokens.group_chip_padding * 2.0;
        for width in &mut chips {
            *width = (*width * scale).max(least);
        }
        total = chips.iter().sum();
    }
    // 2. the tabs, fitted into what the chips left.
    let tab_ix: Vec<usize> = items
        .iter()
        .filter_map(StripItem::tab)
        .filter(|ix| *ix < m.tabs.len())
        .collect();
    let natural: Vec<f32> = tab_ix
        .iter()
        .map(|ix| {
            (measure(&m.tabs[*ix].title, tokens.tab_max_width) + tokens.tab_fixed_chrome)
                .min(tokens.tab_max_width)
        })
        .collect();
    let shown_tab = tab_ix
        .iter()
        .position(|ix| *ix == m.active)
        .unwrap_or(usize::MAX);
    let TabFit {
        shown,
        overflow,
        overflow_width,
    } = craft_ui::tabs::fit(
        &natural,
        shown_tab,
        (room - total).max(0.0),
        tokens.tab_min_width,
        tokens.tab_overflow_width,
    );
    // 3. one walk, left to right, giving every surviving item its rect.
    let bottom = strip.max.y - tokens.tab_vertical_inset;
    let height = strip.height() - 2.0 * tokens.tab_vertical_inset;
    let mut out = StripLayout {
        overflow_width,
        ..Default::default()
    };
    let mut x = strip.min.x + tokens.tab_gap;
    let mut chips = chips.into_iter();
    for item in items {
        let width = match item {
            StripItem::Group(..) => chips.next().unwrap_or(0.0),
            StripItem::Tab(ix) => {
                let Some(pos) = tab_ix.iter().position(|have| *have == ix) else {
                    continue;
                };
                match shown.iter().find(|(have, _)| *have == pos) {
                    // Evicted: the tab lives in the overflow popup now, and its group's chip stayed here.
                    None => continue,
                    Some((_, width)) => *width,
                }
            }
        };
        out.rects.push((
            item,
            Rect::from_min_size(pos2(x, bottom - height), vec2(width, height)),
        ));
        x += width + tokens.tab_gap;
    }
    out.overflow = overflow
        .iter()
        .filter_map(|pos| tab_ix.get(*pos).copied())
        .collect();
    out
}

/// Draw the strip into the rect its row's owner reserved for it.
///
/// The caller owns the row because the row is shared: the pane's workspace button is the first
/// thing in it (plan §5.8 `tab_bar.rs` row, browser-style chrome), so the strip is what is left over.
#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut Ui,
    id: Id,
    strip: Rect,
    m: &mut TabBarModel<'_>,
    tokens: &Tokens,
    out: &mut Vec<TabCommand>,
) {
    if ui.is_rect_visible(strip) {
        ui.painter().rect_filled(strip, 0.0, tokens.bg_sidebar);
        ui.painter()
            .hline(strip.x_range(), strip.max.y - 1.0, tokens.hairline());
    }
    let mut laid = layout(ui, m, tokens, strip);
    let (overflow, rects) = (laid.overflow.clone(), laid.rects.clone());
    for (item, rect) in &rects {
        if !ui.is_rect_visible(*rect) {
            continue;
        }
        match *item {
            StripItem::Group(gid) => paint_chip(ui, id, *rect, gid, m, tokens, out),
            StripItem::Tab(ix) => paint_tab(ui, id, *rect, ix, m, tokens, out),
        }
    }
    // The overflow list, then the `+` button — both *after* the tabs, inside the strip's own rect.
    let bottom = strip.max.y - tokens.tab_vertical_inset;
    let height = strip.height() - 2.0 * tokens.tab_vertical_inset;
    let mut right_max = strip.max.x - tokens.tab_gap;
    if !overflow.is_empty() {
        right_max -= laid.overflow_width;
        let button = Rect::from_min_size(
            pos2(right_max, bottom - height),
            vec2(laid.overflow_width, height),
        );
        let response = ui
            .interact(button, id.with("tab-overflow"), egui::Sense::click())
            .on_hover_text(format!("{} more tabs", overflow.len()));
        if ui.is_rect_visible(button) {
            ui.painter().text(
                button.center(),
                egui::Align2::CENTER_CENTER,
                if m.overflow_open { "▴" } else { "▾" },
                tokens.font(),
                if response.hovered() {
                    tokens.text_main
                } else {
                    tokens.text_muted
                },
            );
        }
        if response.clicked() {
            out.push(TabCommand::ToggleOverflow);
        }
        if m.overflow_open {
            let hidden = overflow.clone();
            let response =
                ui.interact(button, id.with("tab-overflow-anchor"), egui::Sense::hover());
            egui::Popup::from_response(&response).show(|ui| {
                for ix in &hidden {
                    if let Some(tab) = m.tabs.get(*ix)
                        && ui.selectable_label(false, &tab.title).clicked()
                    {
                        out.push(TabCommand::Select {
                            id: tab.id,
                            kind: TabSelKind::Replace,
                        });
                    }
                }
            });
        }
    }
    let new = Rect::from_min_size(
        pos2(right_max - tokens.tab_new_button, bottom - height),
        vec2(tokens.tab_new_button, height),
    );
    let response = ui
        .interact(new, id.with("tab-new"), egui::Sense::click())
        .on_hover_text("New tab");
    if ui.is_rect_visible(new) {
        ui.painter().text(
            new.center(),
            egui::Align2::CENTER_CENTER,
            "+",
            tokens.font(),
            if response.hovered() {
                tokens.text_active
            } else {
                tokens.text_muted
            },
        );
    }
    if response.clicked() {
        out.push(TabCommand::NewTab);
    }
    show_drag(ui, strip, &mut laid, m, tokens, out);
    m.layout = laid;
}

/// The tab drag: the bar that says where the tab will land, and the release that decides.
///
/// Painted after every item so it reads over their edges, and hit-tested against the raw pointer rather
/// than a `hovered()` response — egui suppresses hover on the other widgets of a strip while a drag is
/// running, which is exactly when the target is needed (design §8). The model is never reordered mid-drag:
/// `Pane::move_tabs` runs once, on release.
fn show_drag(
    ui: &mut Ui,
    strip: Rect,
    laid: &mut StripLayout,
    m: &mut TabBarModel<'_>,
    tokens: &Tokens,
    out: &mut Vec<TabCommand>,
) {
    let Some(_dragged) = m.dragging else {
        return;
    };
    let pointer = ui.input(|i| i.pointer.interact_pos());
    let target = pointer.and_then(|at| laid.drop_target_or_end(at, m.tabs, strip));
    if let Some(target) = target {
        paint_drop_bar(ui, laid, target, m.tabs, tokens);
    }
    if ui.input(|i| i.pointer.any_released()) {
        out.push(TabCommand::DragDrop(target));
    }
}

/// One tab of the strip: fill, border, group accent, label, close box, and the two clicks.
fn paint_tab(
    ui: &mut Ui,
    id: Id,
    tab_rect: Rect,
    ix: usize,
    m: &mut TabBarModel<'_>,
    tokens: &Tokens,
    out: &mut Vec<TabCommand>,
) {
    let Some(tab) = m.tabs.get(ix) else { return };
    let id = id.with(("tab", tab.id));
    let selected = ix == m.active;
    let multi = m.selected.contains(&tab.id) && !selected;
    let response = ui.interact(tab_rect, id, egui::Sense::click_and_drag());
    let fill = if selected {
        tokens.bg_card
    } else if multi || response.hovered() {
        tokens.bg_hover
    } else {
        tokens.bg_main
    };
    // The tab's top corners are rounded, its bottom edge is open onto the listing
    // (`tabs.css:16-17`, `border-radius: 6px 6px 0 0`).
    let radius = egui::CornerRadius {
        nw: tokens.corner_radius,
        ne: tokens.corner_radius,
        sw: 0,
        se: 0,
    };
    ui.painter().rect_filled(tab_rect, radius, fill);
    // A multi-selected tab wears its group's border, so a selection inside a run reads as the run
    // moving, and a selection outside one as a tab waiting to be grouped.
    let stroke = match (multi, m.tab_color(tab)) {
        (true, Some(color)) => {
            egui::Stroke::new(tokens.hairline().width, tokens.group_color(color))
        }
        (true, None) => egui::Stroke::new(tokens.hairline().width, tokens.accent),
        (false, _) => tokens.hairline(),
    };
    ui.painter()
        .rect_stroke(tab_rect, radius, stroke, egui::StrokeKind::Inside);
    if let Some(color) = m.tab_color(tab) {
        ui.painter().rect_filled(
            Rect::from_min_size(
                tab_rect.min,
                vec2(tokens.accent_bar_width, tab_rect.height()),
            ),
            0.0,
            tokens.group_color(color),
        );
    }
    let close = Rect::from_min_size(
        pos2(
            tab_rect.max.x - tokens.tab_close_size - 2.0,
            tab_rect.center().y - tokens.tab_close_size / 2.0,
        ),
        vec2(tokens.tab_close_size, tokens.tab_close_size),
    );
    let label = Rect::from_min_size(
        pos2(
            tab_rect.min.x + tokens.tab_label_indent,
            tab_rect.center().y - tokens.font_size,
        ),
        vec2(
            (close.min.x - tab_rect.min.x - tokens.tab_label_indent).max(0.0),
            tokens.font_size * 2.0,
        ),
    );
    ui.put(
        label,
        egui::Label::new(
            egui::RichText::new(&tab.title)
                .color(if selected {
                    tokens.text_active
                } else {
                    tokens.text_muted
                })
                .size(tokens.font_size),
        )
        .truncate()
        // Chrome text, never content text. A *selectable* label takes `Sense::click_and_drag()`,
        // it is allocated after the tab's own rect (so it is the front-most hit), and it paints
        // the selection highlight over the title — which is what left only the tab's four edges
        // clickable: the title swallowed the click the tab was waiting for.
        .selectable(false),
    );
    let close_response = ui
        .interact(close, id.with("close"), egui::Sense::click())
        .on_hover_text("Close tab");
    if ui.is_rect_visible(close) {
        ui.painter().text(
            close.center(),
            egui::Align2::CENTER_CENTER,
            "×",
            tokens.font(),
            if close_response.hovered() {
                tokens.danger
            } else {
                tokens.text_muted
            },
        );
    }
    if close_response.clicked() {
        out.push(TabCommand::Close(tab.id));
    } else if response.drag_started() {
        // The strip does not carry a drag, it starts one: the shell owns the state, because the dock has
        // two strips and only one pane's tabs are moving (design §8).
        out.push(TabCommand::DragStart(tab.id));
    } else {
        // One response for both buttons: `Sense::click()` answers *either*, and a right-click used
        // to *select* the tab here. The menu owns the tab under the cursor instead — Go's
        // `showTabContextMenu` never called `switchTab` — so the secondary press must not switch to
        // it, and the primary press still does.
        if response.secondary_clicked() {
            out.push(TabCommand::Context {
                id: tab.id,
                at: response.interact_pointer_pos().unwrap_or(tab_rect.min),
            });
        } else if response.clicked() {
            out.push(TabCommand::Select {
                id: tab.id,
                kind: click_kind(ui),
            });
        }
    }
}

/// How a strip click changes the selection: the one place the modifier keys are read, so the strip's
/// clicks agree with each other (and with the listing's own `SelKind`, which is the same three-way rule
/// one row further down).
#[must_use]
fn click_kind(ui: &Ui) -> TabSelKind {
    let modifiers = ui.input(|i| i.modifiers);
    if modifiers.shift {
        TabSelKind::Range
    } else if modifiers.ctrl || modifiers.command {
        TabSelKind::Toggle
    } else {
        TabSelKind::Replace
    }
}

/// A group's chip: the label the user clicks to fold the run, in the group's own colour.
fn paint_chip(
    ui: &mut Ui,
    id: Id,
    rect: Rect,
    gid: GroupId,
    m: &mut TabBarModel<'_>,
    tokens: &Tokens,
    out: &mut Vec<TabCommand>,
) {
    let Some(group) = m.group(gid) else { return };
    let (fill, ink, label, collapsed) = (
        tokens.group_chip_fill(group.color, group.collapsed),
        tokens.group_chip_ink(group.color),
        group.label().to_string(),
        group.collapsed,
    );
    // Click only: a chip is a handle, and dragging one is out of scope (design §10).
    let response = ui.interact(rect, id.with(("group", gid)), egui::Sense::click());
    if response.hovered() {
        m.hovered = Some((gid, rect));
    }
    // A chip is a pill inside the row, not a block of it — that is what says "handle", before any text
    // does.
    let pill = Rect::from_center_size(
        rect.center(),
        vec2(rect.width(), tokens.group_chip_height.min(rect.height())),
    );
    ui.painter().rect_filled(
        pill,
        egui::CornerRadius::same(tokens.group_chip_corner),
        fill,
    );
    // The chevron is a triangle and not a glyph: the bundled fonts do not promise a `▸`.
    let size = tokens.group_chevron;
    let x = pill.min.x + tokens.group_chip_padding;
    let mid = pill.center().y;
    let points = if collapsed {
        vec![
            pos2(x, mid - size / 2.0),
            pos2(x + size * 0.75, mid),
            pos2(x, mid + size / 2.0),
        ]
    } else {
        vec![
            pos2(x, mid - size * 0.28),
            pos2(x + size, mid - size * 0.28),
            pos2(x + size / 2.0, mid + size * 0.45),
        ]
    };
    ui.painter()
        .add(egui::Shape::convex_polygon(points, ink, egui::Stroke::NONE));
    let text = Rect::from_min_size(
        pos2(x + size, pill.center().y - tokens.font_size),
        vec2(
            (pill.max.x - tokens.group_chip_padding - x - size).max(0.0),
            tokens.font_size * 2.0,
        ),
    );
    // A painted `Label`, exactly like a tab's title: it is how kittest — and a screen reader — finds
    // the chip at all in a hand-drawn strip.
    ui.put(
        text,
        egui::Label::new(RichText::new(label).color(ink).size(tokens.font_size))
            .truncate()
            .selectable(false),
    );
    if response.secondary_clicked() {
        out.push(TabCommand::GroupContext {
            id: gid,
            at: response.interact_pointer_pos().unwrap_or(rect.min),
        });
    } else if response.clicked() {
        out.push(TabCommand::ToggleGroup(gid));
    }
}

/// The line that says where the dragged tab will land — or, for a chip, the chip lighting up.
fn paint_drop_bar(ui: &Ui, laid: &StripLayout, target: DropTarget, tabs: &[Tab], tokens: &Tokens) {
    let found = |want: StripItem| {
        laid.rects
            .iter()
            .find(|(item, _)| *item == want)
            .map(|(_, rect)| *rect)
    };
    // The target names a tab by id and the layout knows it by index; the strip is where the two meet.
    let rect_of = |want: TabId| {
        tabs.iter()
            .position(|tab| tab.id == want)
            .and_then(|ix| found(StripItem::Tab(ix)))
    };
    let bar = |edge: f32, top: f32, bottom: f32| {
        Rect::from_min_size(
            pos2(edge - tokens.group_drop_bar / 2.0, top),
            vec2(tokens.group_drop_bar, bottom - top),
        )
    };
    let rect =
        match target {
            DropTarget::Before(id) => rect_of(id)
                .map(|rect| bar(rect.min.x - tokens.tab_gap / 2.0, rect.min.y, rect.max.y)),
            DropTarget::After(id) => rect_of(id)
                .map(|rect| bar(rect.max.x + tokens.tab_gap / 2.0, rect.min.y, rect.max.y)),
            DropTarget::OntoChip(gid) => found(StripItem::Group(gid)).map(|rect| {
                // A chip is not a gap: the whole pill says "into this group".
                Rect::from_center_size(
                    rect.center(),
                    vec2(rect.width(), tokens.group_chip_height.min(rect.height())),
                )
            }),
            DropTarget::End => laid
                .rects
                .last()
                .map(|(_, rect)| bar(rect.max.x + tokens.tab_gap, rect.min.y, rect.max.y)),
        };
    let Some(rect) = rect else { return };
    if matches!(target, DropTarget::OntoChip(..)) {
        ui.painter().rect_stroke(
            rect,
            egui::CornerRadius::same(tokens.group_chip_corner),
            egui::Stroke::new(tokens.group_drop_bar / 2.0, tokens.accent),
            egui::StrokeKind::Outside,
        );
    } else {
        ui.painter().rect_filled(rect, 0.0, tokens.accent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(id: u64, group: Option<GroupId>) -> Tab {
        let mut tab = Tab::new(
            TabId(id),
            zenman_core::path::FsPath::ROOT,
            format!("tab{id}"),
        );
        tab.group = group;
        tab
    }

    fn group(id: u64, color: GroupColor, collapsed: bool) -> TabGroup {
        let mut group = TabGroup::new(GroupId(id), color);
        group.collapsed = collapsed;
        group
    }

    #[test]
    fn an_ungrouped_strip_is_its_tabs_in_order() {
        let tabs = vec![tab(1, None), tab(2, None), tab(3, None)];
        assert_eq!(
            items(&tabs, &[], 0),
            vec![StripItem::Tab(0), StripItem::Tab(1), StripItem::Tab(2)]
        );
    }

    #[test]
    fn a_group_is_one_chip_at_the_head_of_its_run() {
        let id = GroupId(1);
        let tabs = vec![
            tab(1, None),
            tab(2, Some(id)),
            tab(3, Some(id)),
            tab(4, None),
        ];
        let groups = [group(1, GroupColor::A, false)];
        assert_eq!(
            items(&tabs, &groups, 0),
            vec![
                StripItem::Tab(0),
                StripItem::Group(id),
                StripItem::Tab(1),
                StripItem::Tab(2),
                StripItem::Tab(3),
            ]
        );
    }

    #[test]
    fn folding_a_group_hides_its_tabs_but_never_the_tab_on_screen() {
        let id = GroupId(1);
        let tabs = vec![tab(1, Some(id)), tab(2, Some(id)), tab(3, None)];
        let folded = [group(1, GroupColor::A, true)];
        // Nothing of the group on screen: the chip is the whole run.
        assert_eq!(
            items(&tabs, &folded, 2),
            vec![StripItem::Group(id), StripItem::Tab(2)]
        );
        // Something of it on screen: the chip *and* that tab, because the pane is listing it.
        assert_eq!(
            items(&tabs, &folded, 1),
            vec![StripItem::Group(id), StripItem::Tab(1), StripItem::Tab(2)]
        );
    }

    #[test]
    fn two_folded_groups_are_two_chips() {
        let (a, b) = (GroupId(1), GroupId(2));
        let tabs = vec![tab(1, Some(a)), tab(2, Some(b)), tab(3, None)];
        let groups = [group(1, GroupColor::A, true), group(2, GroupColor::B, true)];
        assert_eq!(
            items(&tabs, &groups, 2),
            vec![StripItem::Group(a), StripItem::Group(b), StripItem::Tab(2)]
        );
    }

    #[test]
    fn a_drop_lands_on_the_half_of_the_item_the_pointer_is_in() {
        let laid = StripLayout {
            rects: vec![
                (
                    StripItem::Group(GroupId(1)),
                    Rect::from_min_size(pos2(0.0, 0.0), vec2(40.0, 20.0)),
                ),
                (
                    StripItem::Tab(0),
                    Rect::from_min_size(pos2(44.0, 0.0), vec2(100.0, 20.0)),
                ),
            ],
            ..Default::default()
        };
        let tabs = [tab(1, Some(GroupId(1))), tab(2, None)];
        assert_eq!(
            laid.drop_target(pos2(10.0, 5.0), &tabs),
            Some(DropTarget::OntoChip(GroupId(1))),
            "a chip is a group, so it is a target of its own"
        );
        assert_eq!(
            laid.drop_target(pos2(60.0, 5.0), &tabs),
            Some(DropTarget::Before(TabId(1))),
            "the left half of a tab is in front of it"
        );
        assert_eq!(
            laid.drop_target(pos2(120.0, 5.0), &tabs),
            Some(DropTarget::After(TabId(1))),
            "and the right half behind it"
        );
        let strip = Rect::from_min_size(pos2(0.0, -5.0), vec2(400.0, 30.0));
        // Past the last tab, on the row: the end of the strip, which has no rect of its own.
        assert_eq!(
            laid.drop_target_or_end(pos2(200.0, 5.0), &tabs, strip),
            Some(DropTarget::End)
        );
        // Above the row, or out of the strip: nothing, and so a cancelled drag.
        assert_eq!(
            laid.drop_target_or_end(pos2(200.0, 2_000.0), &tabs, strip),
            None
        );
        assert_eq!(
            laid.drop_target_or_end(pos2(-40.0, 5.0), &tabs, strip),
            None,
            "in front of the first chip is not a gap the user can aim at"
        );
    }

    #[test]
    fn a_tab_naming_a_group_that_is_not_there_is_still_a_tab() {
        let tabs = vec![tab(1, Some(GroupId(9))), tab(2, None)];
        assert_eq!(
            items(&tabs, &[], 0),
            vec![StripItem::Tab(0), StripItem::Tab(1)]
        );
    }
}
