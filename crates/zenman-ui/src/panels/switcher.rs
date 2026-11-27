//! The recent tab / recent tab-group switcher panel (`docs/tab-switcher-design.md` §4).
//!
//! **One panel, two drivers**: the same widget lists a pane's tabs (<kbd>Ctrl+Tab</kbd>) and its tab
//! groups (<kbd>Alt+Z</kbd>); only the heading, the row icon and the shell's commit differ
//! (§2, §6). It owns **presentation and the highlight** and never the engine — it reports what the
//! pointer and the chord did, and `ZenApp` decides what that means (the project's command-enum rule,
//! applied to a state machine: no callbacks leave this module).
//!
//! Geometry is **arithmetic** (§4.1): [`layout`] computes the frame, the heading and every visible
//! row's rect from tokens and the screen before anything is painted, so [`Switcher::hover`] answers
//! the pointer in the same frame it arrives, and the long-list window (`first..first + shown`) is a
//! pair of integers, not a scroll offset egui only knows after painting. There is no `ScrollArea`
//! here on purpose.
//!
//! The chord rule: releasing the **modifier** commits the highlighted row. The key lifting is not a
//! commit — it is half of the next tap, because the gesture is <kbd>Ctrl</kbd> held while
//! <kbd>Tab</kbd> is tapped, and Go's `keyupHandler` fires on `e.key === currentModifierKey` alone for
//! exactly that reason (`keyboard.js:126-131`). [`Hold`] therefore names the key (the *chord's* key,
//! for the one case with no modifier to read) and the modifiers that must stay down, and
//! [`Switcher::released`] is the shell's whole question.

use egui::{
    Align, Color32, CornerRadius, Id, InputState, Key, Layout, Order, Pos2, Rect, Response,
    RichText, Sense, Ui, Vec2, pos2, vec2,
};

use crate::icons;
use crate::state::{GroupId, PaneSlot, TabId};
use crate::theme::Tokens;

/// What the panel is a list of. The rows are built the same way; only the words and the icon differ
/// (design §4 — "written once and driven twice").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchKind {
    /// The pane's tabs, in [`crate::state::Pane::recent_tabs`] order.
    Tabs,
    /// The pane's tab groups, in [`crate::state::Pane::recent_groups`] order.
    Groups,
}

impl SwitchKind {
    /// The panel's heading, in Go's own words (`keyboard.js:33` "Recent Tabs"); `ZenApp`'s paint
    /// uppercases it and appends `" (N)"` with the row count, which is what `text-transform` and the
    /// count span do there.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Tabs => "Recent Tabs",
            Self::Groups => "Recent Tab Groups",
        }
    }

    /// The key whose press opens the panel, and whose release is half of the commit test.
    #[must_use]
    pub fn key(self) -> Key {
        match self {
            Self::Tabs => Key::Tab,
            Self::Groups => Key::Z,
        }
    }

    /// The chord that must stay held for this kind of panel, given the modifiers the opening press
    /// arrived with: <kbd>Ctrl+Tab</kbd>, <kbd>Shift+Tab</kbd> (Go's own trigger,
    /// `keyboard.js:169-183`) or <kbd>Alt+Z</kbd>.
    ///
    /// <kbd>Shift</kbd> is never required by the *backwards* entry: `Ctrl+Shift+Tab` steps the panel
    /// `Ctrl+Tab` opened (`keyboard.js:203-207` splits direction from the overlay), so requiring it
    /// would commit the panel the instant the user reached for the reverse walk. The rule is therefore
    /// "whichever non-<kbd>Shift</kbd> modifier opened it, and <kbd>Shift</kbd> only when it came with
    /// <kbd>Shift</kbd> alone" (design §4).
    ///
    /// A press that arrives with **no** modifier state (`egui::Modifiers::is_none`) is held by its key
    /// alone. That is the synthetic stream — a test's [`crate::keymap::Chord::event`], or a backend that
    /// reports no `ModifiersChanged` — and demanding <kbd>Ctrl</kbd> there would close the panel in the
    /// frame it opened, which reads as a bug in the panel rather than in the input.
    #[must_use]
    pub fn hold(self, modifiers: &egui::Modifiers) -> Hold {
        let ctrl = modifiers.ctrl || modifiers.command || modifiers.mac_cmd;
        match self {
            Self::Groups if modifiers.alt => Hold::Z_ALT,
            Self::Tabs if ctrl => Hold::TAB,
            Self::Tabs if modifiers.shift => Hold::TAB_SHIFT,
            _ => Hold::bare(self.key()),
        }
    }
}

/// One row's answer: what to switch to if this row is committed (design §4). The panel reports the
/// row; the shell turns it into a [`crate::state::PaneCommand`] — never the other way round.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchTarget {
    /// Show this tab.
    Tab(TabId),
    /// Activate this group's most recent member tab (§5's group-commit rule).
    Group(GroupId),
}

/// A row, ready to paint. Built by `ZenApp` from `Pane` (§6), so the widget never reads the model.
#[derive(Clone, Debug)]
pub struct SwitchRow {
    /// What committing this row means.
    pub target: SwitchTarget,
    /// `icons::paint` name — `Some("folder")` for a tab (`assets/icons/folder.svg`, Go's folder
    /// emoji, `keyboard.js:52`); `None` for a group, whose mark is its colour dot.
    pub icon: Option<&'static str>,
    /// The row's first line: the tab title, or the group's name (`Tab::title`, `TabGroup::label`).
    pub title: String,
    /// Second, muted line: the tab's path, or a group's `N tabs` (§6).
    pub detail: Option<String>,
    /// The group colour dot, from [`Tokens::group_color`] (`tabs.css:330-342`). On the right for a
    /// tab that belongs to a group; on the left, as the whole mark, for a group row.
    pub dot: Option<Color32>,
}

/// The chord that keeps the panel alive. Releasing the **modifier** commits — Go's `keyupHandler` →
/// `selectAndClose` watches `e.key === currentModifierKey` alone, which is what lets the user tap the
/// key to walk (`keyboard.js:126-131`) — and the shell asks
/// [`Switcher::released`] once per tick and never watches the keys itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hold {
    /// The non-modifier key of the chord (`Tab`, `Z`).
    pub key: Key,
    /// Whether <kbd>Ctrl</kbd> must stay down — the only signal when it is set, since the key lifting
    /// is a tap and not a release (see [`Hold::held`]). The platform key
    /// ([`egui::Modifiers::command`] / [`egui::Modifiers::mac_cmd`]) counts as <kbd>Ctrl</kbd>, exactly
    /// as in [`crate::keymap::Chord::matches`] — the same macOS lie.
    pub ctrl: bool,
    /// Whether <kbd>Alt</kbd> must stay down.
    pub alt: bool,
    /// Whether <kbd>Shift</kbd> must stay down — `true` only for [`Hold::TAB_SHIFT`], which is how
    /// <kbd>Shift+Tab</kbd> opens the panel and nothing else does (`keyboard.js:169-183`;
    /// [`SwitchKind::hold`] for why the backwards walk must not require it).
    pub shift: bool,
}

impl Hold {
    /// <kbd>Ctrl+Tab</kbd> — the forward *and* backward tab hold (design §2: `Ctrl+Shift+Tab` steps
    /// the panel `Ctrl+Tab` opened, so `Shift` may not be required).
    pub const TAB: Self = Self {
        key: Key::Tab,
        ctrl: true,
        alt: false,
        shift: false,
    };
    /// <kbd>Shift+Tab</kbd> — Go's original trigger, kept as a `SwitchTab` chord (§2). The shell
    /// builds the switcher opened by *this* press with this hold, so releasing `Shift` or `Tab`
    /// commits it.
    pub const TAB_SHIFT: Self = Self {
        key: Key::Tab,
        ctrl: false,
        alt: false,
        shift: true,
    };
    /// <kbd>Alt+Z</kbd> — the group hold, both directions (`Alt+Shift+Z` steps the same panel).
    pub const Z_ALT: Self = Self {
        key: Key::Z,
        ctrl: false,
        alt: true,
        shift: false,
    };

    /// The key alone: what [`SwitchKind::hold`] answers when the press carried no modifiers at all.
    pub const fn bare(key: Key) -> Self {
        Self {
            key,
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    /// The chord is still down. One `false` is the commit signal, and the signal is the
    /// **modifier**, never the key: Go's `keyupHandler` fires on `e.key === currentModifierKey`
    /// (`keyboard.js:126-129`), and the gesture is *hold <kbd>Ctrl</kbd>, tap
    /// <kbd>Tab</kbd> repeatedly* — every tap lifts the key, so a hold that demanded
    /// `key_down(self.key)` would commit the panel on the first tap's release and the walk could
    /// never go deeper than one step (§1.2, §1.3).
    ///
    /// [`Hold::bare`] is the exception that proves the rule: a hold with no modifier in it exists only
    /// for a synthetic stream (design §4), whose key is the only state it can be given.
    #[must_use]
    pub fn held(&self, input: &InputState) -> bool {
        if !(self.ctrl || self.alt || self.shift) {
            return input.key_down(self.key);
        }
        let modifiers = &input.modifiers;
        (!self.ctrl || modifiers.ctrl || modifiers.command || modifiers.mac_cmd)
            && (!self.alt || modifiers.alt)
            && (!self.shift || modifiers.shift)
    }
}

/// What the pointer did to the panel this frame (design §4). An enum, not a callback: the shell
/// decides whether `Picked` closes a pane or activates a tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchPointer {
    /// A row was clicked — commit it. The index is into [`Switcher::rows`] (not the visible window).
    Picked(usize),
    /// The panel was dismissed by a click outside it (§1.4: cancel, not switch).
    Dismissed,
}

/// Which way the highlight travels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// One place deeper into the past; wraps past the end (§1.2).
    Forward,
    /// One place back towards the shown row; wraps the other way.
    Back,
}

/// Where the panel and its rows land. `rects[i]` is row `first + i`, and only the visible window of
/// the list has a rect (§4.1) — a row outside it exists in `rows` but not in geometry.
#[derive(Clone, Debug, PartialEq)]
pub struct SwitchLayout {
    /// The panel's own rect, centred in the screen (Go's `align-items/justify-content: center`,
    /// `tabs.css:233-235`).
    pub frame: Rect,
    /// The heading line, inside the frame's padding.
    pub heading: Rect,
    /// The row the window starts at (as clamped to the list).
    pub first: usize,
    /// The rects of the visible rows, stacked top-down, each [`Tokens::switcher_row_height`] tall.
    pub rects: Vec<Rect>,
}

/// The height of the heading block: the title line plus the gap that separates it from the list.
/// The design's §4.1 formula writes this as `title`; the gap token lives here so the frame-height
/// arithmetic never has to say it twice.
fn heading_block(tokens: &Tokens) -> f32 {
    tokens.switcher_title_size + tokens.switcher_gap
}

/// How many rows fit at all for this screen and token set — the cap of §4.1's
/// `(switcher_max_height - title - 2 * pad) / row_height`, with the screen's own height as a second
/// ceiling so a half-height window never gets a taller panel than it has room for. At least one.
fn visible_rows(screen: Rect, tokens: &Tokens) -> usize {
    let budget = tokens.switcher_max_height.min(screen.height());
    let room = budget - 2.0 * tokens.switcher_pad - heading_block(tokens);
    let cap = (room / tokens.switcher_row_height).floor().max(1.0);
    // `max(1.0)` above keeps this cast non-negative; a negative budget still lands on one row.
    cap as usize
}

/// The frame's height for `rows` *painted* rows (§4.1: `pad + title + visible * row_height + pad`).
fn frame_height(rows: usize, tokens: &Tokens) -> f32 {
    2.0 * tokens.switcher_pad + heading_block(tokens) + rows as f32 * tokens.switcher_row_height
}

/// The panel's geometry for one frame, with no painting in it — a function of its own so a test can
/// check the numbers without a harness, exactly like [`crate::panels::tab_bar::layout`] (design
/// §4.1). `rows` is the whole list and `first` the window's start; the returned `first` is the
/// clamped one, and `rects.len()` is `min(visible, rows - first)`.
#[must_use]
pub fn layout(rows: usize, first: usize, screen: Rect, tokens: &Tokens) -> SwitchLayout {
    let visible = visible_rows(screen, tokens);
    let first = first.min(rows.saturating_sub(1));
    let shown = visible.min(rows - first);
    let height = frame_height(shown, tokens);
    let frame = Rect::from_center_size(screen.center(), vec2(tokens.switcher_width, height));
    let heading = Rect::from_min_size(
        pos2(
            frame.min.x + tokens.switcher_pad,
            frame.min.y + tokens.switcher_pad,
        ),
        vec2(
            frame.width() - 2.0 * tokens.switcher_pad,
            tokens.switcher_title_size,
        ),
    );
    let mut rects = Vec::with_capacity(shown);
    let mut top = heading.max.y + tokens.switcher_gap;
    for _ in 0..shown {
        rects.push(Rect::from_min_size(
            pos2(frame.min.x + tokens.switcher_pad, top),
            vec2(
                frame.width() - 2.0 * tokens.switcher_pad,
                tokens.switcher_row_height,
            ),
        ));
        top += tokens.switcher_row_height;
    }
    SwitchLayout {
        frame,
        heading,
        first,
        rects,
    }
}

/// The panel that is open, or `None` in `ZenApp` when nothing is (§5's one field).
///
/// The state is the *list*, the highlight and the window — nothing else. `rows` is a snapshot: the
/// shell rebuilds it from `Pane` when it opens and calls [`Switcher::retain`] when tabs die under it,
/// which is the panel's only link to the model, and it is one-way.
#[derive(Clone, Debug)]
pub struct Switcher {
    /// Tabs or groups — decides the heading; the shell already picked the rows.
    pub kind: SwitchKind,
    /// The pane that owns the keys, like Go's `getRecentTabs(state.activePane)` — the shell routes
    /// every commit through it.
    pub slot: PaneSlot,
    /// The list, in recency order: row `0` is the tab (or group) on screen (§6).
    pub rows: Vec<SwitchRow>,
    /// The highlighted row. `0` means "the row on screen" — the initial highlight is `1`, the row
    /// you would go back to (Go's `selectedIndex = 1`, `keyboard.js:124`).
    pub selected: usize,
    /// First row of the visible window (§4.1), kept under `selected` by every move. Private
    /// because it is only ever meaningful together with [`Switcher::shown`] — the shell reads the
    /// rects via [`Switcher::row_rect`], never the window pair.
    first: usize,
    /// Rows the window could show when the panel was last laid out. [`step`] and [`nudge`] slide
    /// `first` against *this*, not against a fresh `layout` call, because they run in `handle_keys`
    /// — before the frame's geometry exists — and the screen cannot change between the two in one
    /// pass. Until the first [`Switcher::show`] it holds "everything fits".
    ///
    /// [`step`]: Self::step
    /// [`nudge`]: Self::nudge
    shown: usize,
    /// The chord that keeps the panel alive; [`released`](Self::released) answers for it.
    hold: Hold,
    /// Where the panel was laid out last frame, and where it will be laid out again: the rects are
    /// arithmetic, not paint output (§4.1).
    frame: Rect,
    /// The visible rows' rects, in the same order as `rects[i] == row(first + i)` of [`layout`].
    rects: Vec<Rect>,
    /// Where the pointer was last frame, and whether the panel has seen a frame at all: [`show`] moves
    /// the highlight on a *change* of position, which is what a `mouseover` event is
    /// (`keyboard.js:63-66`) — see §1.4.
    over: Option<Pos2>,
    /// `false` until the panel has been laid out once, i.e. until "the pointer did not move" is a
    /// statement that means anything.
    armed: bool,
}

impl Switcher {
    /// Open the panel. `None` when there is nothing to switch to — fewer than two rows answers
    /// nothing at all, no panel and no switch (feature 9; Go's `if (recentTabs.length <= 1)
    /// return;`, `keyboard.js:113`). Starts highlighted on row `1`, the one behind the shown row.
    #[must_use]
    pub fn open(
        kind: SwitchKind,
        slot: PaneSlot,
        rows: Vec<SwitchRow>,
        hold: Hold,
    ) -> Option<Self> {
        if rows.len() < 2 {
            return None;
        }
        Some(Self {
            kind,
            slot,
            selected: 1,
            first: 0,
            // Until the first `show` there is no measured window; "everything fits" is the one
            // assumption that cannot drop a row from the highlight's reach.
            shown: rows.len(),
            rows,
            hold,
            frame: Rect::NOTHING,
            rects: Vec::new(),
            over: None,
            armed: false,
        })
    }

    /// Move the highlight, wrapping: `Step::Forward` past the end lands on `0`, i.e. back on the row
    /// that is on screen. Go's `(selectedIndex + 1) % length` (`keyboard.js:136`). Then slides the
    /// window (§4.1) the minimal amount so the highlight is inside it.
    pub fn step(&mut self, step: Step) {
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        self.selected = match step {
            Step::Forward => (self.selected + 1) % n,
            Step::Back => (self.selected + n - 1) % n,
        };
        self.slide();
    }

    /// Arrow keys (§1.2's mouse-hand convenience): the same travel as [`Self::step`], clamped instead
    /// of wrapped — `↑` on row `0` stays on the row that is on screen.
    pub fn nudge(&mut self, delta: i32) {
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        let want = self.selected as i32 + delta;
        self.selected = want.clamp(0, n as i32 - 1) as usize;
        self.slide();
    }

    /// The rows the panel can show at once for this screen and token set — the window's capacity,
    /// whether or not the list is that long. `layout` is the same arithmetic; this is the question
    /// the shell asks when it builds rows.
    #[must_use]
    pub fn visible(&self, screen: Rect, tokens: &Tokens) -> usize {
        visible_rows(screen, tokens)
    }

    /// The row the pointer is over, if it is over one; also moves the highlight to it (§1.4: hover
    /// *is* selection). Answers against [`layout`]'s rects, so it needs no painted output — this is
    /// what [`Switcher::show`] calls, and only when the position changed.
    pub fn hover(&mut self, at: Pos2) -> Option<usize> {
        // `rposition`, not `position`: the rows butt-join, so a point on a shared edge lies in two
        // rects, and in this fork the *last-allocated* overlapping rect wins the pointer (plan §9.2
        // step 8) — the row below. The click hit-test below agrees with it.
        let off = self.rects.iter().rposition(|rect| rect.contains(at))?;
        self.selected = self.first + off;
        Some(self.selected)
    }

    /// The highlighted row's target — what the shell commits on release. `None` only when the list
    /// has been emptied under the panel (§9's `the_panel_dies_with_its_tab`).
    #[must_use]
    pub fn target(&self) -> Option<SwitchTarget> {
        self.rows.get(self.selected).map(|row| row.target)
    }

    /// The chord's **modifier** came up (§1.3, Go's `keyupHandler` on `currentModifierKey`). The key
    /// lifting is not a release — that is a tap, and a tap is a step.
    #[must_use]
    pub fn released(&self, input: &InputState) -> bool {
        !self.hold.held(input)
    }

    /// Drop rows that no longer exist (§9's `the_panel_dies_with_its_tab`: the shell hands back the
    /// survivors through the predicate).
    ///
    /// The highlight follows **its own row**: the id it stood on is looked up again among the
    /// survivors, because rows dying *above* the highlight must not silently move it to whatever ended
    /// up at that index — the promise the panel made is a tab, not a number. When the highlighted row
    /// is itself the one that died, the highlight keeps its **place**: the row that took over that
    /// index is the next thing back in the past, which is where the finger was heading. Row `0` keeps
    /// meaning "the row on screen" either way, because `Pane::close_tabs` re-touched the rings
    /// (`docs/tab-switcher-design.md` §3) and this list's row 0 is therefore still the shown tab.
    /// `first` slides to keep the highlight inside the window (§4.1).
    pub fn retain<F: FnMut(&SwitchRow) -> bool>(&mut self, keep: F) {
        let (wanted, place) = (
            self.rows.get(self.selected).map(|row| row.target),
            self.selected,
        );
        self.rows.retain(keep);
        let last = self.rows.len().saturating_sub(1);
        self.selected = wanted
            .and_then(|target| self.rows.iter().position(|row| row.target == target))
            .unwrap_or(place)
            .min(last);
        self.slide();
    }

    /// The panel's frame as laid out by the last [`Self::show`] (`Rect::NOTHING` before it). The
    /// behavioural suite hit-tests against the widget's own geometry, not the a11y tree (§9).
    #[must_use]
    pub fn frame(&self) -> Rect {
        self.frame
    }

    /// The rect of row `ix` when it is in the visible window — the same rect [`Self::hover`] and the
    /// click hit-test answer to, so a test that drives the pointer drives the arithmetic.
    #[must_use]
    pub fn row_rect(&self, ix: usize) -> Option<Rect> {
        let off = ix.checked_sub(self.first)?;
        self.rects.get(off).copied()
    }

    /// Paint. Returns the pointer's answer, and remembers the rects for the next frame (§4.1:
    /// layout first, *then* the pointer, *then* the pixels — the highlight never lags a frame).
    ///
    /// The backdrop is allocated before the panel because in this egui fork the last-allocated
    /// overlapping rect wins the pointer: the panel's rows beat the wash, and a click on the wash
    /// outside the frame is the dismissal (§1.4) without the file rows underneath lighting up.
    pub fn show(&mut self, ctx: &egui::Context, tokens: &Tokens) -> Option<SwitchPointer> {
        let screen = ctx.viewport_rect();
        let laid = layout(self.rows.len(), self.first, screen, tokens);
        self.frame = laid.frame;
        self.first = laid.first;
        // The window's *capacity*, not the rows painted into it: `step`/`nudge` run in `handle_keys`,
        // before this frame's geometry exists, and sliding against a short last window (four rows in a
        // panel that fits eight) would drop rows out of the highlight's reach.
        self.shown = self.visible(screen, tokens);
        self.rects = laid.rects.clone();
        if laid.rects.is_empty() {
            // Every row died between `retain` and this frame — nothing to show, nothing to answer.
            return None;
        }
        // The pointer votes only when it *moves*, which is the whole of Go's `mouseover` handler
        // (`keyboard.js:63-66`) and the reason the panel cannot simply ask "what is under the pointer"
        // every frame: the panel is centred, so it opens **under** wherever the mouse happens to rest,
        // and a resting pointer would outvote every step of the keyboard walk. The first frame only
        // records the position — a mouse that was already there when the chord was pressed has not
        // said anything yet (design §1.4).
        let here = ctx.input(|i| i.pointer.hover_pos());
        if self.armed
            && here != self.over
            && let Some(at) = here
        {
            self.hover(at);
        }
        self.over = here;
        self.armed = true;
        let mut pick: Option<SwitchPointer> = None;
        let id = Id::unique("zenman-switcher");
        let frame = laid.frame;
        // 1. the backdrop: paint-only fill, but a `Sense::click()` response — "click outside
        // cancels" reads that response (§4.2; Go's overlay click handler, `keyboard.js:118`).
        let _ = egui::Area::new(id.with("backdrop"))
            .order(Order::Foreground)
            .movable(false)
            .fixed_pos(screen.min)
            .show(ctx, |ui| {
                let wash = ui.allocate_response(screen.size(), Sense::click());
                ui.painter()
                    .rect_filled(wash.rect, 0.0, tokens.switcher_backdrop);
                if wash.clicked_by(egui::PointerButton::Primary)
                    && wash
                        .interact_pointer_pos()
                        .is_none_or(|at| !frame.contains(at))
                {
                    pick = Some(SwitchPointer::Dismissed);
                }
            });
        // 2. the panel, allocated *after* the wash so it wins the pointer over it; the same
        // Frame/Stroke/Shadow recipe as the fold flyout (`app.rs:1137-1210`).
        let heading = format!("{} ({})", self.kind.title().to_uppercase(), self.rows.len());
        let selected = self.selected;
        let _ = egui::Area::new(id.with("panel"))
            .order(Order::Foreground)
            .movable(false)
            .fixed_pos(frame.min)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(tokens.bg_sidebar)
                    .stroke(egui::Stroke::new(1.0, tokens.border))
                    .corner_radius(CornerRadius::same(tokens.switcher_corner_radius))
                    .inner_margin(egui::Margin::same(tokens.switcher_pad as i8))
                    .shadow(egui::Shadow {
                        offset: [0, tokens.menu_shadow_offset_y],
                        blur: tokens.menu_shadow_blur,
                        spread: 0,
                        color: tokens.menu_shadow,
                    })
                    .show(ui, |ui| {
                        // A click inside the panel but not on a row must NOT dismiss (§4's row
                        // rule): this rect is allocated over the wash, so the press dies here.
                        let _ = ui.interact(frame, id.with("panel-bg"), Sense::click());
                        // The Frame sizes itself to what its child *used*, and the rows are painted at
                        // arithmetic rects that need no space from the cursor — so the box is claimed
                        // here, once, or the painted panel comes out shorter than the panel the pointer
                        // hit-tests (design §4.1's promise, kept).
                        let inner = Rect::from_min_size(
                            frame.min + Vec2::splat(tokens.switcher_pad),
                            frame.size() - Vec2::splat(2.0 * tokens.switcher_pad),
                        );
                        let _ = ui.allocate_rect(inner, Sense::hover());
                        // A real Label, never painted text — the accessibility tree and the kittest
                        // suites read the heading (`status_bar.rs:334-400`).
                        label_left(
                            ui,
                            laid.heading,
                            &heading,
                            tokens.switcher_title_size,
                            tokens.text_muted,
                        );
                        for (off, rect) in laid.rects.iter().enumerate() {
                            let ix = laid.first + off;
                            let Some(row) = self.rows.get(ix) else {
                                continue;
                            };
                            let response =
                                paint_row(ui, id, ix, *rect, row, ix == selected, tokens);
                            // Assigned, not `get_or_insert`: two rows share an edge, and the one
                            // painted *last* wins the pointer in this fork — so the lowest clicked row
                            // is the answer, which is the rule [`Switcher::hover`] uses too (§11.5).
                            if response.clicked() {
                                pick = Some(SwitchPointer::Picked(ix));
                            }
                        }
                    });
            });
        pick
    }

    /// Slide the window the minimal amount so `first <= selected < first + shown` (§4.1) — the
    /// same way the file list's scroller follows its cursor, one integer instead of a scroll offset.
    fn slide(&mut self) {
        let win = self.shown.max(1).min(self.rows.len().max(1));
        if self.selected < self.first {
            self.first = self.selected;
        } else if self.selected >= self.first + win {
            self.first = self.selected + 1 - win;
        }
    }
}

/// One row: the selection chrome, the mark, the two lines — and the click.
///
/// The interaction rect is taken **first**, the way the strip does it (`tab_bar.rs:490-560`): the
/// labels are allocated on top of it, and hover-only widgets never join the click hit-test
/// (`egui/hit_test.rs`, "we should only return click hits if they sense click"), so the row keeps the
/// press. The fill and stroke are Go's `.selected` (`tabs.css:286-290`); hover *is* selection
/// (§1.4), so one rule covers both.
fn paint_row(
    ui: &mut Ui,
    id: Id,
    ix: usize,
    rect: Rect,
    row: &SwitchRow,
    selected: bool,
    tokens: &Tokens,
) -> Response {
    let response = ui.interact(rect, id.with(("switcher-row", ix)), Sense::click());
    if !ui.is_rect_visible(rect) {
        return response;
    }
    if selected {
        // Go's row radius (`tabs.css:279`, `border-radius: 8px`) is the shell's own
        // `corner_radius`, so no new token: the panel has one, the rows have the other.
        let radius = CornerRadius::same(tokens.corner_radius);
        ui.painter().rect_filled(rect, radius, tokens.bg_hover);
        ui.painter().rect_stroke(
            rect,
            radius,
            egui::Stroke::new(1.0, tokens.accent),
            egui::StrokeKind::Inside,
        );
    }
    let mark = Rect::from_min_size(
        pos2(
            rect.min.x + tokens.switcher_row_pad_x,
            rect.center().y - tokens.switcher_icon_size / 2.0,
        ),
        Vec2::splat(tokens.switcher_icon_size),
    );
    match (row.icon, row.dot) {
        // Paint-only (`icons.rs:196-206`): the row's own interact above owns the click.
        (Some(name), _) => {
            icons::paint(ui, mark, name, tokens.switcher_icon_size, tokens.text_muted);
        }
        // A group's mark *is* its dot (`tabs.css:330-342`), centred in the same box.
        (None, Some(colour)) => {
            ui.painter()
                .circle_filled(mark.center(), tokens.switcher_dot_size / 2.0, colour);
        }
        (None, None) => {}
    }
    let text_min = mark.max.x + tokens.switcher_gap;
    let mut text_max = rect.max.x - tokens.switcher_row_pad_x;
    if row.icon.is_some()
        && let Some(colour) = row.dot
    {
        // A tab that belongs to a group wears its colour at the right edge — the strip's accent bar
        // in a panel that has no bars.
        ui.painter().circle_filled(
            pos2(text_max - tokens.switcher_dot_size / 2.0, rect.center().y),
            tokens.switcher_dot_size / 2.0,
            colour,
        );
        text_max -= tokens.switcher_dot_size + tokens.switcher_gap;
    }
    // Two lines, at the row's own padding: `.recent-tab-item { padding: 10px 14px }` over a flex
    // column of `.recent-tab-name` then `.recent-tab-path` with `gap: 2px` (`tabs.css:274-328`) — the
    // name's 0.85rem and the path's 0.72rem are [`Tokens::switcher_name_size`] and
    // [`Tokens::switcher_path_size`], so the block sits where Go's sits rather than being centred on a
    // height the CSS never mentions. Elision is the house recipe (`tab_bar.rs:490-513`): a rect
    // narrower than the row plus `.truncate()`.
    let width = (text_max - text_min).max(0.0);
    let top = rect.min.y + tokens.switcher_row_pad_y;
    label_left(
        ui,
        Rect::from_min_size(pos2(text_min, top), vec2(width, tokens.switcher_name_size)),
        &row.title,
        tokens.switcher_name_size,
        tokens.text_active,
    );
    if let Some(detail) = &row.detail {
        let top = top + tokens.switcher_name_size + LINE_GAP;
        label_left(
            ui,
            Rect::from_min_size(pos2(text_min, top), vec2(width, tokens.switcher_path_size)),
            detail,
            tokens.switcher_path_size,
            tokens.text_muted,
        );
    }
    response
}

/// The gap between a row's two lines — `.recent-tab-details { gap: 2px }` (`tabs.css:309`), the one
/// measurement of the panel that has no design token because nothing else in the shell uses it.
const LINE_GAP: f32 = 2.0;

/// One line of text at the **left** of `rect`, elided there.
///
/// Not `Ui::put`, which lays its child out `centered_and_justified`: the galley is shrunk to the text
/// and the shrunk box is then centred, so [`egui::Label::halign`] has nothing left to align (measured
/// on `18-recent-tabs.png`, which is why this function exists). Go's heading and both row lines start
/// at the left of their box (`tabs.css:271,305-328`), so the alignment comes from the layout instead.
fn label_left(ui: &mut Ui, rect: Rect, text: &str, size: f32, colour: Color32) {
    let _ = ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::top_down(Align::Min)),
        |ui| {
            ui.add(
                egui::Label::new(RichText::new(text).color(colour).size(size))
                    .truncate()
                    .selectable(false),
            );
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(ix: usize) -> SwitchRow {
        SwitchRow {
            target: SwitchTarget::Tab(TabId(ix as u64 + 1)),
            icon: Some("folder"),
            title: format!("tab {ix}"),
            detail: Some(format!("/path/{ix}")),
            dot: None,
        }
    }

    fn rows(n: usize) -> Vec<SwitchRow> {
        (0..n).map(row).collect()
    }

    fn switcher(n: usize) -> Switcher {
        Switcher::open(SwitchKind::Tabs, PaneSlot::Left, rows(n), Hold::TAB)
            .expect("two or more rows open a panel")
    }

    fn tokens() -> Tokens {
        Tokens::dark()
    }

    fn screen() -> Rect {
        Rect::from_min_size(Pos2::ZERO, vec2(1_200.0, 800.0))
    }

    #[test]
    fn open_refuses_a_one_row_list() {
        // Feature 9: a pane with one row to offer answers nothing at all.
        assert!(Switcher::open(SwitchKind::Tabs, PaneSlot::Left, rows(0), Hold::TAB).is_none());
        assert!(Switcher::open(SwitchKind::Tabs, PaneSlot::Left, rows(1), Hold::TAB).is_none());
        assert!(Switcher::open(SwitchKind::Groups, PaneSlot::Left, rows(2), Hold::Z_ALT).is_some());
    }

    #[test]
    fn open_starts_on_the_row_behind_the_shown_one() {
        let s = switcher(3);
        // Go's `selectedIndex = 1` (`keyboard.js:124`): row 0 is the tab on screen, row 1 is the
        // one a release would land on.
        assert_eq!(s.selected, 1);
        assert_eq!(s.first, 0);
    }

    #[test]
    fn step_forward_walks_the_ring_and_wraps() {
        let mut s = switcher(3);
        let mut seen = Vec::new();
        for _ in 0..4 {
            s.step(Step::Forward);
            seen.push(s.selected);
        }
        assert_eq!(
            seen,
            vec![2, 0, 1, 2],
            "1 → 2 → 0 → 1 for three rows (§9.3)"
        );
    }

    #[test]
    fn step_back_walks_the_ring_the_other_way() {
        let mut s = switcher(3);
        s.step(Step::Back);
        assert_eq!(s.selected, 0);
        s.step(Step::Back);
        assert_eq!(s.selected, 2);
        s.step(Step::Back);
        assert_eq!(s.selected, 1);
    }

    #[test]
    fn nudge_clamps_at_both_ends() {
        let mut s = switcher(3);
        s.nudge(5);
        assert_eq!(s.selected, 2, "the last row, not past it");
        s.nudge(1);
        assert_eq!(s.selected, 2, "and no wrap on an arrow");
        s.nudge(-9);
        assert_eq!(s.selected, 0, "the row on screen, not before it");
    }

    #[test]
    fn step_slides_the_window_minimally() {
        let mut s = switcher(5);
        s.shown = 2; // pretend the panel only fits two rows (§4.1's window).
        s.slide();
        assert_eq!((s.first, s.selected), (0, 1));
        s.step(Step::Forward); // selected 2 leaves the window at its bottom edge…
        assert_eq!((s.first, s.selected), (1, 2));
        s.step(Step::Forward);
        assert_eq!((s.first, s.selected), (2, 3));
        s.step(Step::Forward); // …and the wrap lands *before* the window.
        assert_eq!(s.selected, 4);
        s.step(Step::Forward);
        assert_eq!(
            (s.first, s.selected),
            (0, 0),
            "wrapping to the shown row slides back in one step"
        );
    }

    #[test]
    fn hovering_outside_every_rect_moves_nothing() {
        let mut s = switcher(3);
        s.rects = vec![Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 46.0))];
        let before = s.selected;
        assert_eq!(s.hover(pos2(500.0, 500.0)), None, "off every row");
        assert_eq!(s.hover(pos2(50.0, 60.0)), None, "just below the only row");
        assert_eq!(s.selected, before, "and the highlight did not move");
        assert_eq!(s.hover(pos2(10.0, 10.0)), Some(0), "inside the row's rect");
        assert_eq!(s.selected, 0, "the highlight followed the pointer (§1.4)");
    }

    #[test]
    fn layout_centres_the_frame_and_stacks_the_rows() {
        let t = tokens();
        let laid = layout(3, 0, screen(), &t);
        assert_eq!(
            laid.rects.len(),
            3,
            "fewer rows than the window: every row has a rect"
        );
        assert_eq!(laid.frame.width(), t.switcher_width);
        assert_eq!(
            laid.frame.height(),
            2.0 * t.switcher_pad
                + t.switcher_title_size
                + t.switcher_gap
                + 3.0 * t.switcher_row_height,
            "pad + title (+ its gap) + visible * row_height + pad (§4.1)"
        );
        assert_eq!(
            laid.frame.center(),
            screen().center(),
            "centred, Go's flex center"
        );
        for w in laid.rects.windows(2) {
            assert_eq!(
                w[0].max.y, w[1].min.y,
                "rows butt-stack, no gap between them"
            );
            assert_eq!(w[0].min.x, w[1].min.x);
        }
        for rect in &laid.rects {
            assert!(
                laid.frame.contains_rect(*rect),
                "a row never leaves the frame"
            );
            assert_eq!(rect.height(), t.switcher_row_height);
            assert_eq!(rect.width(), t.switcher_width - 2.0 * t.switcher_pad);
        }
        assert_eq!(laid.rects[0].min.y, laid.heading.max.y + t.switcher_gap);
    }

    #[test]
    fn layout_grows_the_frame_until_the_max_height_and_not_beyond() {
        let t = tokens();
        let cap = visible_rows(screen(), &t);
        assert_eq!(cap, 8, "420 − 32 − 19 leaves eight 46 px rows");
        let mut last = 0.0;
        for n in 1..=cap {
            let laid = layout(n, 0, screen(), &t);
            assert_eq!(laid.rects.len(), n);
            assert!(laid.frame.height() <= t.switcher_max_height);
            if n > 1 {
                assert_eq!(laid.frame.height() - last, t.switcher_row_height);
            }
            last = laid.frame.height();
        }
        // One row past the cap: the frame stops growing and the list becomes a window.
        let over = layout(cap + 1, 0, screen(), &t);
        assert_eq!(
            over.frame.height(),
            last,
            "the max height is a ceiling, not a suggestion"
        );
        assert_eq!(
            over.rects.len(),
            cap,
            "and the window shows exactly `cap` rows"
        );
    }

    #[test]
    fn layout_clamps_a_first_past_the_end_of_the_list() {
        // A row died between frames: the window may name a row the list no longer has, and the
        // geometry pass answers by pulling it back, never by panicking.
        let laid = layout(3, 99, screen(), &tokens());
        assert_eq!(laid.first, 2);
        assert_eq!(laid.rects.len(), 1);
    }

    #[test]
    fn retain_follows_the_highlighted_row_and_never_names_a_dead_one() {
        // Rows dying *above* the highlight must not move it: the promise is a tab, not an index.
        let mut s = switcher(4);
        s.selected = 3;
        s.retain(|r| matches!(r.target, SwitchTarget::Tab(id) if id.0 >= 3));
        assert_eq!(s.rows.len(), 2, "the two surviving rows");
        assert_eq!(
            s.target(),
            Some(SwitchTarget::Tab(TabId(4))),
            "the highlight followed its own row to its new index"
        );
        assert!(s.first <= s.selected, "and the window still contains it");
        // The highlighted row itself dies: the answer that was already true is row 0.
        let mut s = switcher(4);
        s.selected = 3;
        s.retain(|r| matches!(r.target, SwitchTarget::Tab(id) if id.0 <= 2));
        assert_eq!(
            s.selected, 1,
            "a dead highlight keeps its place — clamped to the last row, never left on a dead id"
        );
        assert_eq!(
            s.target(),
            Some(SwitchTarget::Tab(TabId(2))),
            "and it names whoever took that place"
        );
        s.retain(|_| false);
        assert_eq!(
            s.selected, 0,
            "an emptied panel is a clamped panel, not a panic"
        );
        assert_eq!(s.target(), None, "and it names no target at all");
    }

    /// A key event. The `modifiers` field rides along as `egui-winit` would set it, but see
    /// [`input`]: `InputState::modifiers` is fed by `Event::ModifiersChanged`, not by key events.
    fn press(key: Key, pressed: bool) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        }
    }

    /// One pass of input: the chord's modifiers, then these key events — the order a real stream
    /// delivers them (`ModifiersChanged` first, then the key).
    fn input(modifiers: egui::Modifiers, events: &[egui::Event]) -> InputState {
        let mut evs = vec![egui::Event::ModifiersChanged(modifiers)];
        evs.extend_from_slice(events);
        let raw = egui::RawInput {
            events: evs,
            ..Default::default()
        };
        InputState::default().begin_pass(raw, false, 1.0, egui::InputOptions::default())
    }

    fn m(f: impl FnOnce(&mut egui::Modifiers)) -> egui::Modifiers {
        let mut modifiers = egui::Modifiers::default();
        f(&mut modifiers);
        modifiers
    }

    #[test]
    fn hold_held_demands_the_whole_chord() {
        let ctrl = m(|k| k.ctrl = true);
        let held = input(ctrl, &[press(Key::Tab, true)]);
        assert!(Hold::TAB.held(&held), "Ctrl down, Tab down");
        assert!(
            !Hold::TAB_SHIFT.held(&held),
            "the Shift+Tab hold wants Shift, not Ctrl"
        );
        assert!(
            Hold::TAB.held(&input(m(|k| k.command = true), &[press(Key::Tab, true)])),
            "the platform key counts as Ctrl (`Chord::matches`' macOS lie)"
        );
        assert!(
            Hold::TAB.held(&input(m(|k| k.mac_cmd = true), &[press(Key::Tab, true)])),
            "and so does `mac_cmd`"
        );
        assert!(
            !Hold::bare(Key::Tab).held(&input(
                egui::Modifiers::default(),
                &[press(Key::Tab, true), press(Key::Tab, false)]
            )),
            "a bare hold is the synthetic stream: with no modifier in it, the key's release commits"
        );
        assert!(
            Hold::TAB.held(&input(
                ctrl,
                &[press(Key::Tab, true), press(Key::Tab, false)]
            )),
            "Tab up while Ctrl stays down is NOT a commit: the gesture is hold Ctrl and tap Tab, so every              tap lifts the key (`keyboard.js:126-129` watches the modifier alone)"
        );
        assert!(
            !Hold::TAB.held(&input(egui::Modifiers::default(), &[press(Key::Tab, true)])),
            "Ctrl up while Tab is down — the commit case (§1.3)"
        );
        let alt = m(|k| k.alt = true);
        assert!(Hold::Z_ALT.held(&input(alt, &[press(Key::Z, true)])));
        assert!(
            Hold::Z_ALT.held(&input(
                m(|k| {
                    k.alt = true;
                    k.shift = true;
                }),
                &[press(Key::Z, true)]
            )),
            "Alt+Shift+Z steps the panel Alt+Z opened, so Shift must not be required (§2)"
        );
        assert!(
            !Hold::Z_ALT.held(&input(m(|k| k.shift = true), &[press(Key::Z, true)])),
            "and Alt really is one of the demanded modifiers"
        );
    }

    #[test]
    fn released_follows_the_hold() {
        let s = switcher(3);
        let ctrl = m(|k| k.ctrl = true);
        assert!(
            !s.released(&input(ctrl, &[press(Key::Tab, true)])),
            "the chord is still fully down"
        );
        // Ctrl came up; Tab has not moved. `keyupHandler` fires on the *first* of the two (§5).
        assert!(
            s.released(&input(egui::Modifiers::default(), &[press(Key::Tab, true)])),
            "a modifier lifting is the commit signal"
        );
        assert!(
            !s.released(&input(ctrl, &[press(Key::Tab, false)])),
            "but the KEY lifting is not: the walk is Ctrl held while Tab is tapped, so a tap's release              must leave the panel standing (`keyboard.js:126-129`)"
        );
        let bare = Switcher::open(
            SwitchKind::Tabs,
            PaneSlot::Left,
            rows(3),
            Hold::bare(Key::Tab),
        );
        assert!(
            bare.map(|s| s.released(&input(
                egui::Modifiers::default(),
                &[press(Key::Tab, true), press(Key::Tab, false)]
            ))) == Some(true),
            "a synthetic stream that reports no modifiers is held by its key alone"
        );
    }
}
