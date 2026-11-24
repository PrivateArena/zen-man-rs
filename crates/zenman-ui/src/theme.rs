//! The zen-man design tokens (plan §5.7 `theme.rs` row).
//!
//! **This module owns every colour literal in the crate** — the `xtask ci` colour guard greps the
//! rest of `crates/zenman-ui/src` for `Color32::` constructions and fails on any hit. The values are
//! the §2.3 visual-spec table, quoted verbatim from the Go app's `frontend/css/base.css:2-18`
//! (and re-measured against `docs/parity/*.png` by step 15's human gate, never by pixel diff).
//!
//! Geometry is here too: the 40 px row / 34 px tab row / 38 px navigation bar / 30 px status bar
//! constants are the same table, and every widget that lays itself out reads them from [`Tokens`]
//! rather than repeating the numbers.

use egui::{Color32, FontId, Stroke, Visuals};

use zenman_core::store::config::{GroupColor, ThemeMode};

/// The full token set: colours, radii, and the Go app's pixel geometry. All fields are `Copy` so
/// [`Tokens`] can be cached in the app and passed by value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tokens {
    // ---- colours (§2.3, `--bg-main` … in the order the CSS lists them) --------------------------
    /// `--bg-main #0f1117`: the window background and pane bodies.
    pub bg_main: Color32,
    /// `--bg-sidebar #090a0f`: drawers and the status bar.
    pub bg_sidebar: Color32,
    /// `--bg-card #161821`: cards, the header, tab strips.
    pub bg_card: Color32,
    /// `--bg-hover #222533`: hover fills.
    pub bg_hover: Color32,
    /// `--border-color #212534`: all hairlines.
    pub border: Color32,
    /// `--accent-color #5865f2`: selection, focus, the active pane border base.
    pub accent: Color32,
    /// `--accent-hover #4752c4`.
    pub accent_hover: Color32,
    /// `--text-main #e3e5e8`.
    pub text_main: Color32,
    /// `--text-muted #959ba4`.
    pub text_muted: Color32,
    /// `--text-active #ffffff`.
    pub text_active: Color32,
    /// `--danger #f23f43`.
    pub danger: Color32,
    /// `--warning #f0b232`.
    pub warning: Color32,
    /// `--success #23a55a`.
    pub success: Color32,
    /// File-row selected fill, `rgba(88,101,242,0.15)` — the accent at 15 %.
    pub selected_row_bg: Color32,
    /// File-row selected border, `rgba(88,101,242,0.3)`.
    pub selected_row_border: Color32,
    /// Active-pane border, `rgba(88,101,242,0.4)`.
    pub active_pane_border: Color32,
    /// A grouped flat listing's header fill: `rgba(33, 37, 52, 0.4)` (`file-list.css:230`).
    pub group_header_bg: Color32,
    /// The `⚠ Loading limited…` banner's wash: `rgba(240, 178, 50, 0.05)` (`file-list.css:124`).
    pub banner_bg: Color32,
    /// The column-header row's height: the `padding: 10px 20px` of `.file-list-header`
    /// (`file-list.css:11-17`) around one line of text.
    pub header_row_height: f32,
    /// Menu shadow wash, the paint of `0 10px 25px rgba(0,0,0,.5)`; egui has no shadow blur, so the
    /// shadow is a darkened panel fill (deviation recorded in the step-7 §9.2 row).
    pub menu_shadow: Color32,
    /// A disabled control's label (`--text-muted` at half weight; the CSS has no token for it).
    pub disabled_text: Color32,
    /// A fully transparent colour, re-exported so no other module names `Color32::TRANSPARENT`.
    pub transparent: Color32,
    // ---- group colours (§2.3 `#ff4a4a #3b82f6 #10b981 #fbbf24 #8b5cf6`) -------------------------
    /// Group A (`#ff4a4a`; the one group colour with a captured Go value, `tests/golden/config.json`).
    pub group_red: Color32,
    /// Group B (`#3b82f6`).
    pub group_blue: Color32,
    /// Group C (`#10b981`).
    pub group_green: Color32,
    /// Group D (`#fbbf24`).
    pub group_yellow: Color32,
    /// Group E (`#8b5cf6`).
    pub group_purple: Color32,
    /// Group `F` (orange; the port's own letter, no Go value to be faithful to).
    pub group_orange: Color32,
    /// Group `G` (cyan).
    pub group_cyan: Color32,
    /// Group `H` (pink).
    pub group_pink: Color32,
    /// Group `I` (teal).
    pub group_teal: Color32,
    // ---- geometry (§2.3 px table) ---------------------------------------------------------------
    /// File row height: 40.
    pub row_height: f32,
    /// Tab bar height: 34 (the row a pane's tabs and its workspace button live in).
    pub tab_bar_height: f32,
    /// Navigation bar height: 38 (the row the address bar and the tool ribbon live in).
    pub toolbar_height: f32,
    /// Status bar height: 30.
    pub status_height: f32,
    /// The least room the status bar's middle cell needs before the boot note is printed in it —
    /// the point at which a truncation would say less than nothing.
    pub status_note_min_width: f32,
    /// Left drawer width: 250.
    pub left_drawer_width: f32,
    /// Right drawer width: 300 (off-canvas in Go; an egui `SidePanel` here).
    pub right_drawer_width: f32,
    /// Action-log slide-in width: 360.
    pub action_log_width: f32,
    /// Tab strip max width: 150.
    pub tab_max_width: f32,
    /// Grid tile minimum edge: `minmax(110px, 1fr)`.
    pub grid_tile_min: f32,
    /// Grid gap: 15.
    pub grid_gap: f32,
    /// A grid tile's glyph, as a fraction of the tile's width (`file-list.css:88`, `font-size:
    /// 2.5rem` against a 16 px root).
    pub grid_glyph_fraction: f32,
    /// …and of its height, so a narrow pane never gives the glyph more room than the tile has.
    pub grid_glyph_height_fraction: f32,
    /// Gap between a tile's glyph and its label (`file-list.css:87`, `margin-top: 5px`).
    pub grid_label_gap: f32,
    /// The share of a tile its two-line label may take.
    pub grid_label_height_fraction: f32,
    /// A tile's label size (`file-list.css:84`, `font-size: 0.85rem`).
    pub grid_font_size: f32,
    /// Quick-find dialog width fraction: 0.8 (of the viewport, capped by [`Self::dialog_max_width`]).
    pub quick_find_fraction: f32,
    /// Search-manager dialog area fraction: 0.85.
    pub search_manager_fraction: f32,
    /// Dialog max width: 1200 (quick-find) — the search manager caps at 1100.
    pub dialog_max_width: f32,
    /// Search-manager max width: 1100.
    pub search_manager_max_width: f32,
    // ---- shell geometry the widgets ask for (step 7c: the literals `dock_content`/`app` used to
    //      carry; the `xtask ci` geometry guard is what keeps them here) -------------------------
    /// Smallest rect a dock pane accepts (`PanelLimits::min`): enough for the 34 px tab row, the
    /// 38 px navigation bar, a column header and a placeholder line, and wide enough that the tab
    /// row can give the strip its floor ([`Self::tab_strip_min_width`]) *and* the workspace control
    /// its two icon boxes. Small enough that the quad grid still fits the 900x480 window minimum.
    pub pane_min: egui::Vec2,
    /// "No ceiling" for a pane: `PanelLimits::max` has no infinity, so this is the value the dock
    /// treats as unbounded — far larger than any viewport.
    pub pane_unbounded_max: f32,
    /// Gap the dock leaves between panes (`DockStyle::gap`).
    pub dock_gap: f32,
    /// Gap before the first tab and between two tabs in a pane's own strip.
    pub tab_gap: f32,
    /// How far a tab is inset from the top and bottom of its strip (the tab is this much shorter
    /// than the strip on each edge).
    pub tab_vertical_inset: f32,
    /// Corner radius of a tab.
    pub tab_corner_radius: f32,
    /// Width of the group-colour bar a tab paints down its left edge.
    pub accent_bar_width: f32,
    /// How far a tab's label sits from the tab's left edge — the room for the accent bar plus the
    /// text's own bearing, so a label never touches the group colour.
    pub tab_label_indent: f32,
    /// Width subtracted from a tab's budget before a label is ellipsised (accent bar + indent +
    /// the ellipsis itself).
    pub tab_label_reserve: f32,
    /// Nominal per-character advance of the placeholder tab-width estimate (step 8 replaces the
    /// estimate with galleys, and this token with it).
    pub tab_glyph_advance: f32,
    /// Nominal advance as a fraction of the font size, for the breadcrumb's width estimate.
    pub tab_glyph_advance_fraction: f32,
    /// Smallest width [`craft_ui::tabs::fit`] may squeeze a tab to before it overflows.
    pub tab_min_width: f32,
    /// Width the strip reserves for its `▾` overflow button.
    pub tab_overflow_width: f32,
    /// Width the strip reserves for its `+` button.
    pub tab_new_button: f32,
    /// The least a pane's tab strip may be left with: the `+`, the overflow button and one tab at
    /// [`Self::tab_min_width`]. The workspace control shrinks itself to what the row has beyond this.
    pub tab_strip_min_width: f32,
    /// The `×` of a tab.
    pub tab_close_size: f32,
    /// Widest single breadcrumb segment.
    pub crumb_max_width: f32,
    /// Horizontal padding inside a crumb chip (`toolbar.css:85`, `padding: 2px 6px`).
    pub crumb_chip_padding: f32,
    /// Gap on each side of a crumb separator (`toolbar.css:70`, `gap: 4px`).
    pub crumb_separator_gap: f32,
    /// How far a chip is inset from the top and bottom of the navigation bar (the chip is the bar's
    /// height minus twice this), so it reads as a chip and not as a block of the address bar.
    pub crumb_chip_inset: f32,
    /// Corner radius of a crumb chip (`toolbar.css:86`, `border-radius: 4px`).
    pub crumb_corner_radius: u8,
    /// Fixed chrome the same estimate adds to a tab's text width (padding on both sides).
    pub tab_fixed_chrome: f32,
    /// Space the empty-pane placeholder leaves above its first line.
    pub empty_pane_space: f32,
    /// Gap between the sections of a drawer (label, then the rows under it).
    pub drawer_section_gap: f32,
    /// Icon size a drawer row draws (bookmarks, places).
    pub drawer_icon_size: f32,
    // ---- drawer rows (`frontend/css/sidebar.css`, step 15b geometry) ---------------------------
    /// `.sidebar-nav { padding: 15px 10px }` (`:43-46`) — a drawer's own horizontal inset.
    pub drawer_pad_x: f32,
    /// `.sidebar-header { padding: 20px }` (`:30`) — the title row's horizontal inset, deeper than
    /// the nav's because the title is not a row.
    pub drawer_head_pad_x: f32,
    /// Height of that title row (`20px` padding around a 1.2rem line, plus its 1 px border).
    pub drawer_header_height: f32,
    /// `.sidebar-header h2 { font-size: 1.2rem }` (`:35-39`), on our 13 px base.
    pub drawer_title_size: f32,
    /// `.sidebar-section-title { padding: 15px 20px 5px 20px }` (`:79-81`) — the section title's
    /// inset, which sits between the nav's and a row's text column.
    pub drawer_section_pad_x: f32,
    /// The same rule's `padding-top: 15px` (`:80`) — the air above a section title, which is what
    /// separates one section from the rows of the section before it.
    pub drawer_section_top: f32,
    /// `.sidebar-item { padding: 10px 12px }` (`:55-60`) — a row's inner horizontal padding.
    pub drawer_row_pad_x: f32,
    /// `.sidebar-item { border-radius: 6px }` (`:58`) — the hover background's radius.
    pub drawer_row_corner: u8,
    /// `.sidebar-nav { gap: 5px }` (`:46`) — the vertical gap between two rows of a section.
    pub drawer_row_gap: f32,
    /// `.sidebar-item .icon { width: 18px }` (`:72-76`): the glyph is *centred* in this box, which
    /// is what `text-align: center` does to Go's emoji, and [`Self::row_icon_gap`] is its margin.
    pub drawer_icon_box: f32,
    /// Width of the capacity meter a mounted-drive row paints in place of its long text — the
    /// 250 px panel cannot hold `home · 369.8 GiB free of 915.8 GiB` on Go's single line, so the
    /// numbers move to the row's tooltip and the fraction is what is painted.
    pub drawer_meter_width: f32,
    /// Height of that bar (the header meter's own bar is [`Self::drawer_meter_height`]'s 3× — a
    /// sidebar row is chrome, not the subject of the frame).
    pub drawer_meter_height: f32,
    /// Gap between a row's label and its right-hand column (meter, capacity, role).
    pub drawer_trailing_gap: f32,
    // ---- pane chrome (the tab row and the navigation bar a pane draws above its listing) ---------
    /// The square of a chrome icon button: 24.
    pub nav_button: f32,
    /// Gap between two icon buttons of one chrome ribbon group.
    pub nav_gap: f32,
    /// Gap between two groups of a chrome ribbon (nav | tools | globals).
    pub nav_group_gap: f32,
    /// The floor of a pane's address strip: the ribbon may have whatever the row has left over.
    /// This is what keeps a collapsed trail down to its root, its marker and the last two segments
    /// however narrow the pane gets — a number of buttons is not a budget.
    pub nav_trail_min_width: f32,
    /// Horizontal inset of a pane's chrome rows.
    pub chrome_pad_x: f32,
    // ---- step 14b (workspace vocabulary, the action log, the two new dialogs) --------------------
    /// Width of the workspace popup.
    pub workspace_select_width: f32,
    /// Inner padding of the `Saved` chip.
    pub chip_pad: f32,
    /// One action-log row's height (the drawer's rows are two lines plus their padding).
    pub log_row_height: f32,
    /// Width of the accent strip an action-log row paints.
    pub log_accent_width: f32,
    /// Gap inside an action-log row (label, time, sources, button).
    pub log_inner_gap: f32,
    /// Width of a dialog form's label column.
    pub form_label_width: f32,
    /// Width of a dialog form's text field.
    pub form_field_width: f32,
    /// Gap between two rows of a dialog form.
    pub form_gap: f32,
    /// How many rows the workspace popup shows before it scrolls.
    pub dropdown_visible_rows: f32,
    // ---- menus (step 10b; `frontend/css/context-menu.css`) --------------------------------------
    /// `min-width: 180px` (`context-menu.css:8`); the frame grows to fit the widest label + chord.
    pub menu_min_width: f32,
    /// `border-radius: 6px` (`:7`).
    pub menu_corner_radius: u8,
    /// `padding: 5px 0` (`:9`) — the vertical inner margin of the frame.
    pub menu_frame_pad_y: u8,
    /// `padding: 8px 15px` (`:14`) — a row's horizontal padding. The row's own height is
    /// [`Self::menu_item_height`], the same padding plus the label's line box.
    pub menu_item_pad_x: f32,
    /// One menu row's height (`8px 15px` around a 13 px line, rounded to the 4 px grid).
    pub menu_item_height: f32,
    /// The icon column (`Self::row_icon_size` is its width); Go's menu has none — plan §5.8's
    /// `MenuSpec::icon` is the port's own.
    pub menu_icon_gap: f32,
    /// Smallest gap between a label and its chord hint (CSS uses `justify-content: space-between`,
    /// which is a *minimum* separation once the row is wide enough).
    pub menu_hint_gap: f32,
    /// `.context-menu-shortcut { font-size: 0.75rem }` (`:35-38`), on our 13 px base.
    pub menu_hint_font_size: f32,
    /// `.context-menu-separator { margin: 5px 0 }` (`:29-33`), on each side of the 1 px rule.
    pub menu_separator_gap: u8,
    /// Widest a menu may grow: Go's box was content-sized and unbounded, so an action with a long
    /// name pushed the whole frame off-screen; here the label wraps instead.
    pub menu_max_width: f32,
    // ---- dialogs (step 10c) ---------------------------------------------------------------------
    /// How wide a modal card is. Go had no modal of its own — every one of its dialogs was the
    /// browser's `window.prompt`/`confirm`, sized by the OS — so this is the port's own measure,
    /// between the 360 px action-log panel and the 320 px menu cap.
    pub dialog_width: f32,
    /// The card's inner padding (the CSS cards' 16 px; the menus use 12).
    pub dialog_padding: f32,
    /// Vertical gap between two lines of a dialog.
    pub dialog_gap: f32,
    /// How far the drag ghost's card sits from the pointer (the desktop convention's hotspot offset).
    pub drag_ghost_offset: egui::Vec2,
    /// `box-shadow: 0 10px 25px rgba(0,0,0,.5)` (`:10`) — the blur and the y-offset; the colour is
    /// [`Self::menu_shadow`].
    pub menu_shadow_blur: u8,
    /// …and its vertical offset.
    pub menu_shadow_offset_y: i8,
    // ---- file-list geometry (step 8; `frontend/css/file-list.css`) ------------------------------
    /// Horizontal padding inside a file row: the `20px` of `padding: 8px 20px` (`file-list.css:33`).
    pub row_pad_x: f32,
    /// Vertical padding inside a file row: the `8px` of the same rule.
    pub row_pad_y: f32,
    /// Gap between two cells of a row (the name's `gap: 10px`, the right-aligned cells' inset).
    pub cell_padding: f32,
    /// The glyph a row or a drawer line paints.
    pub row_icon_size: f32,
    /// Gap between a row's glyph and its label (`file-list.css:49`, `gap: 10px`).
    pub row_icon_gap: f32,
    /// The width a group header gives its `▶`/`▼` toggle.
    pub group_arrow_width: f32,
    /// Rows built beyond the viewport in each direction (`file-list.js:127`, `const buffer = 5`).
    pub overscan_rows: usize,
    /// The height of the `⚠️ Loading limited…` banner (`file-list.js:120`, `warningHeight = 80`).
    pub load_more_height: f32,
    // ---- type -----------------------------------------------------------------------------------
    /// Base UI font size (egui's default; the Go app's body text is 13 px Outfit, decision R9
    /// records that the port ships no bundled font).
    pub font_size: f32,
    /// Corner radius, `--radius` family: the CSS cards use 8 px.
    pub corner_radius: u8,
}

impl Tokens {
    /// The dark token set — the reference's only theme (§10 decision 7: match the current dark
    /// theme exactly).
    #[must_use]
    pub fn dark() -> Self {
        Self {
            bg_main: Color32::from_rgb(0x0f, 0x11, 0x17),
            bg_sidebar: Color32::from_rgb(0x09, 0x0a, 0x0f),
            bg_card: Color32::from_rgb(0x16, 0x18, 0x21),
            bg_hover: Color32::from_rgb(0x22, 0x25, 0x33),
            border: Color32::from_rgb(0x21, 0x25, 0x34),
            accent: Color32::from_rgb(0x58, 0x65, 0xf2),
            accent_hover: Color32::from_rgb(0x47, 0x52, 0xc4),
            text_main: Color32::from_rgb(0xe3, 0xe5, 0xe8),
            text_muted: Color32::from_rgb(0x95, 0x9b, 0xa4),
            text_active: Color32::from_rgb(0xff, 0xff, 0xff),
            danger: Color32::from_rgb(0xf2, 0x3f, 0x43),
            warning: Color32::from_rgb(0xf0, 0xb2, 0x32),
            success: Color32::from_rgb(0x23, 0xa5, 0x5a),
            selected_row_bg: Color32::from_rgba_unmultiplied(0x58, 0x65, 0xf2, 38),
            selected_row_border: Color32::from_rgba_unmultiplied(0x58, 0x65, 0xf2, 77),
            active_pane_border: Color32::from_rgba_unmultiplied(0x58, 0x65, 0xf2, 102),
            menu_shadow: Color32::from_rgba_unmultiplied(0x00, 0x00, 0x00, 128),
            group_header_bg: Color32::from_rgba_unmultiplied(0x21, 0x25, 0x33, 102),
            banner_bg: Color32::from_rgba_unmultiplied(0xf0, 0xb2, 0x32, 13),
            disabled_text: Color32::from_rgba_unmultiplied(0x95, 0x9b, 0xa4, 110),
            transparent: Color32::TRANSPARENT,
            group_red: Color32::from_rgb(0xff, 0x4a, 0x4a),
            group_blue: Color32::from_rgb(0x3b, 0x82, 0xf6),
            group_green: Color32::from_rgb(0x10, 0xb9, 0x81),
            group_yellow: Color32::from_rgb(0xfb, 0xbf, 0x24),
            group_purple: Color32::from_rgb(0x8b, 0x5c, 0xf6),
            group_orange: Color32::from_rgb(0xf9, 0x73, 0x16),
            group_cyan: Color32::from_rgb(0x06, 0xb6, 0xd4),
            group_pink: Color32::from_rgb(0xec, 0x48, 0x99),
            group_teal: Color32::from_rgb(0x14, 0xb8, 0xa6),
            row_height: 40.0,
            tab_bar_height: 34.0,
            toolbar_height: 38.0,
            status_height: 30.0,
            status_note_min_width: 120.0,
            left_drawer_width: 250.0,
            right_drawer_width: 300.0,
            action_log_width: 360.0,
            tab_max_width: 150.0,
            grid_tile_min: 110.0,
            grid_gap: 15.0,
            quick_find_fraction: 0.8,
            search_manager_fraction: 0.85,
            dialog_max_width: 1200.0,
            search_manager_max_width: 1100.0,
            pane_min: egui::vec2(200.0, 150.0),
            pane_unbounded_max: 100_000.0,
            dock_gap: 4.0,
            tab_gap: 4.0,
            tab_vertical_inset: 3.0,
            tab_corner_radius: 4.0,
            accent_bar_width: 3.0,
            tab_label_indent: 10.0,
            tab_label_reserve: 20.0,
            tab_glyph_advance: 8.0,
            tab_glyph_advance_fraction: 0.62,
            tab_min_width: 60.0,
            tab_overflow_width: 22.0,
            tab_new_button: 22.0,
            tab_strip_min_width: 120.0,
            tab_close_size: 14.0,
            crumb_max_width: 220.0,
            crumb_chip_padding: 6.0,
            crumb_separator_gap: 4.0,
            crumb_chip_inset: 6.0,
            crumb_corner_radius: 4,
            tab_fixed_chrome: 32.0,
            empty_pane_space: 24.0,
            drawer_section_gap: 4.0,
            drawer_icon_size: 16.0,
            drawer_pad_x: 10.0,
            drawer_head_pad_x: 20.0,
            drawer_header_height: 60.0,
            drawer_title_size: 16.0,
            drawer_section_pad_x: 20.0,
            drawer_section_top: 15.0,
            drawer_row_pad_x: 12.0,
            drawer_row_corner: 6,
            drawer_row_gap: 5.0,
            drawer_icon_box: 18.0,
            drawer_meter_width: 56.0,
            drawer_meter_height: 5.0,
            drawer_trailing_gap: 8.0,
            nav_button: 24.0,
            nav_gap: 2.0,
            nav_group_gap: 8.0,
            nav_trail_min_width: 340.0,
            chrome_pad_x: 6.0,
            workspace_select_width: 240.0,
            chip_pad: 6.0,
            log_row_height: 54.0,
            log_accent_width: 3.0,
            log_inner_gap: 8.0,
            form_label_width: 130.0,
            form_field_width: 260.0,
            form_gap: 6.0,
            dropdown_visible_rows: 6.0,
            menu_min_width: 180.0,
            menu_max_width: 420.0,
            dialog_width: 420.0,
            dialog_padding: 16.0,
            dialog_gap: 8.0,
            drag_ghost_offset: egui::Vec2::new(12.0, 12.0),
            menu_corner_radius: 6,
            menu_frame_pad_y: 5,
            menu_item_pad_x: 15.0,
            menu_item_height: 26.0,
            menu_icon_gap: 10.0,
            menu_hint_gap: 24.0,
            menu_hint_font_size: 11.0,
            menu_separator_gap: 5,
            menu_shadow_blur: 25,
            menu_shadow_offset_y: 10,
            row_pad_x: 20.0,
            row_pad_y: 8.0,
            cell_padding: 8.0,
            row_icon_size: 16.0,
            row_icon_gap: 10.0,
            group_arrow_width: 16.0,
            overscan_rows: 5,
            header_row_height: 30.0,
            load_more_height: 80.0,
            grid_glyph_fraction: 0.45,
            grid_glyph_height_fraction: 0.55,
            grid_label_gap: 6.0,
            grid_label_height_fraction: 0.35,
            grid_font_size: 11.0,
            font_size: 13.0,
            corner_radius: 8,
        }
    }

    /// A light variant. **Not** captured from the Go app — there is no light CSS to quote — so this
    /// is the dark palette inverted by hand, and step 15's human review owns its fidelity.
    #[must_use]
    pub fn light() -> Self {
        Self {
            bg_main: Color32::from_rgb(0xf4, 0xf5, 0xf7),
            bg_sidebar: Color32::from_rgb(0xe9, 0xeb, 0xef),
            bg_card: Color32::from_rgb(0xff, 0xff, 0xff),
            bg_hover: Color32::from_rgb(0xdf, 0xe2, 0xe9),
            border: Color32::from_rgb(0xc9, 0xcd, 0xd8),
            accent: Color32::from_rgb(0x4a, 0x56, 0xd6),
            accent_hover: Color32::from_rgb(0x39, 0x44, 0xb8),
            text_main: Color32::from_rgb(0x1d, 0x21, 0x28),
            text_muted: Color32::from_rgb(0x5c, 0x63, 0x70),
            text_active: Color32::from_rgb(0x00, 0x00, 0x00),
            danger: Color32::from_rgb(0xd3, 0x2f, 0x33),
            warning: Color32::from_rgb(0xc2, 0x8a, 0x1d),
            success: Color32::from_rgb(0x1a, 0x82, 0x47),
            selected_row_bg: Color32::from_rgba_unmultiplied(0x4a, 0x56, 0xd6, 38),
            selected_row_border: Color32::from_rgba_unmultiplied(0x4a, 0x56, 0xd6, 77),
            active_pane_border: Color32::from_rgba_unmultiplied(0x4a, 0x56, 0xd6, 102),
            menu_shadow: Color32::from_rgba_unmultiplied(0x00, 0x00, 0x00, 64),
            disabled_text: Color32::from_rgba_unmultiplied(0x5c, 0x63, 0x70, 110),
            group_header_bg: Color32::from_rgba_unmultiplied(0xc9, 0xcd, 0xd8, 102),
            banner_bg: Color32::from_rgba_unmultiplied(0xc2, 0x8a, 0x1d, 13),
            transparent: Color32::TRANSPARENT,
            group_red: Color32::from_rgb(0xd6, 0x32, 0x32),
            group_blue: Color32::from_rgb(0x25, 0x63, 0xd0),
            group_green: Color32::from_rgb(0x0d, 0x94, 0x67),
            group_yellow: Color32::from_rgb(0xd6, 0x9c, 0x12),
            group_purple: Color32::from_rgb(0x6f, 0x44, 0xd3),
            group_orange: Color32::from_rgb(0xc2, 0x60, 0x0b),
            group_cyan: Color32::from_rgb(0x0e, 0x74, 0x90),
            group_pink: Color32::from_rgb(0xbe, 0x18, 0x5d),
            group_teal: Color32::from_rgb(0x0f, 0x76, 0x6e),
            ..Self::dark()
        }
    }

    /// The colour a tab group paints with. One source of truth (plan §5.7 `state.rs` row: Go
    /// duplicated these in three CSS blocks); core's `GroupColor::color` answers only for the one
    /// captured hex, so the rest come from here.
    #[must_use]
    pub fn group_color(self, group: GroupColor) -> Color32 {
        match group {
            GroupColor::A => self.group_red,
            GroupColor::B => self.group_blue,
            GroupColor::C => self.group_green,
            GroupColor::D => self.group_yellow,
            GroupColor::E => self.group_purple,
            GroupColor::F => self.group_orange,
            GroupColor::G => self.group_cyan,
            GroupColor::H => self.group_pink,
            GroupColor::I => self.group_teal,
        }
    }

    /// The dim that goes between the shell and an open dialog card (15c D1: Go darkened the whole
    /// page behind its overlay instead of letting the listing read through the card). Translucent
    /// **by design** — the card itself is [`Self::modal_fill`], which is not.
    #[must_use]
    pub fn modal_backdrop(&self) -> Color32 {
        let base = self.bg_main;
        Color32::from_rgba_unmultiplied(base.r() / 2, base.g() / 2, base.b() / 2, 200)
    }

    /// The dialog card's fill, forced **opaque** (15c D1: the snapshot review found the listing's
    /// columns painting into the Search Manager's body). `bg_card` with its alpha channel discarded,
    /// so no future token edit can make a modal translucent again.
    #[must_use]
    pub fn modal_fill(&self) -> Color32 {
        let base = self.bg_card;
        Color32::from_rgb(base.r(), base.g(), base.b())
    }
    /// The proportional font at the token size (the `DockStyle` font, table rows, tab labels).
    #[must_use]
    pub fn font(&self) -> FontId {
        FontId::proportional(self.font_size)
    }

    /// A colour at the file row's cut opacity — the `opacity: 0.5` of `.cut-state`
    /// (`file-list.css:41-43`), which egui cannot express per widget, so it is a colour: the mix of
    /// the row's own colour with the card it sits on. The one place that blends two colours, which is
    /// why it lives with the colours.
    #[must_use]
    pub fn dimmed(self, colour: Color32) -> Color32 {
        /// The CSS opacity of a cut row.
        const CUT_OPACITY: f32 = 0.5;
        let mix = |a: u8, b: u8| {
            let v = f32::from(a) * CUT_OPACITY + f32::from(b) * (1.0 - CUT_OPACITY);
            v.round().clamp(0.0, 255.0) as u8
        };
        let bg = self.bg_card;
        Color32::from_rgb(
            mix(colour[0], bg[0]),
            mix(colour[1], bg[1]),
            mix(colour[2], bg[2]),
        )
    }

    /// The hairline stroke every panel border uses.
    #[must_use]
    pub fn hairline(&self) -> Stroke {
        Stroke::new(1.0, self.border)
    }
}

impl Default for Tokens {
    fn default() -> Self {
        Self::dark()
    }
}

/// The widget-state palette derived from tokens (plan §5.7: `palette_from` + `apply_to`, then the
/// zen-man state exceptions).
#[must_use]
pub fn palette_from(tokens: &Tokens) -> craft_ui::theme::WidgetPalette {
    craft_ui::theme::WidgetPalette {
        noninteractive: tokens.bg_card,
        inactive: tokens.bg_hover,
        hovered: tokens.bg_hover,
        active: tokens.accent_hover,
        open: tokens.bg_hover,
    }
}

/// Read the tokens back out of an already-installed `Visuals`.
///
/// The plan's `tokens(style)` cache seam: the values themselves are constants (the CSS table is not
/// dynamic), so this returns the token set the current theme installed — the identity of the
/// visuals *is* the identity of the tokens. Step 8+ widgets take `&Tokens` and never re-read style.
#[must_use]
pub fn tokens(style: &egui::Style) -> Tokens {
    if style.visuals.dark_mode {
        Tokens::dark()
    } else {
        Tokens::light()
    }
}

/// Install the zen-man visuals for one theme choice (plan §5.7: `apply(ctx, choice)`; the plan's
/// `ThemeChoice` is core's persisted `ThemeMode`, so the config and the shell cannot disagree).
///
/// `System` resolves to dark: egui has no way to read the desktop theme, the reference is dark-only,
/// and guessing from the accent colour would be worse than saying so — the window title stays the
/// same either way.
pub fn apply(ctx: &egui::Context, choice: ThemeMode) {
    let tokens = match choice {
        ThemeMode::Light => Tokens::light(),
        ThemeMode::Dark | ThemeMode::System => Tokens::dark(),
    };
    let mut visuals = if choice == ThemeMode::Light {
        Visuals::light()
    } else {
        Visuals::dark()
    };
    visuals.panel_fill = tokens.bg_main;
    visuals.extreme_bg_color = tokens.bg_sidebar;
    visuals.faint_bg_color = tokens.bg_card;
    visuals.window_fill = tokens.bg_card;
    visuals.window_stroke = tokens.hairline();
    visuals.override_text_color = Some(tokens.text_main);
    visuals.hyperlink_color = tokens.accent;
    visuals.selection.bg_fill = tokens.accent;
    visuals.selection.stroke.color = tokens.text_active;
    palette_from(&tokens).apply_to(
        &mut visuals.widgets,
        egui::CornerRadius::same(tokens.corner_radius),
        Stroke::new(1.0, tokens.text_main),
    );
    // The selected-file-row exception, applied *after* the palette (plan §5.7: the row tint is a
    // zen-man state exception, not a widget state).
    visuals.widgets.hovered.weak_bg_fill = tokens.bg_hover;
    ctx.set_visuals(visuals);
    ctx.global_style_mut(|style| {
        style.spacing.item_spacing = egui::vec2(6.0, 4.0);
        style.text_styles.insert(
            egui::TextStyle::Body,
            FontId::proportional(tokens.font_size),
        );
        style.text_styles.insert(
            egui::TextStyle::Button,
            FontId::proportional(tokens.font_size),
        );
        style.text_styles.insert(
            egui::TextStyle::Monospace,
            FontId::monospace(tokens.font_size),
        );
    });
}

/// The font stack (plan §5.7 `theme::fonts` row; §10 decision R9: **no bundled Outfit**, so the
/// families come from the framework's shared stack and the operating system).
///
/// Step 7 left this a no-op with a test that could not fail. Step 7c wired the decision R9 actually
/// names: [`craft_ui::fonts::UiFonts`] builds the Craft fallback order — the app's own faces (it has
/// none: R9 forbids vendoring), then the system faces, then egui's defaults — and `Stack::install`
/// is what puts a `FontDefinitions` set and a provider list into the context. Two things follow, and
/// both are asserted by `tests::fonts_install_...`:
///
/// * the shell gets a **symbol face egui's defaults do not have**. `egui_face("Hack",
///   &[Proportional])` is the framework's documented use of the hook, and it is the case this shell
///   has: `Ubuntu-Light` + `egui-icons` (egui's whole proportional chain) carry no `▸`/`▾`, which is
///   what a tree, an expanded group and a sort arrow draw with. Without the stack those glyphs are
///   boxes; with it they are Hack's.
/// * the **system fallback is real**, and it is eframe's: the binary enables eframe's `system_fonts`
///   feature (root `Cargo.toml`), which installs `egui_system_font_provider` into the context, so a
///   CJK or Arabic file name draws from the installed fonts. `UiFonts` deliberately installs *no*
///   provider of its own here: with craft-ui's `system_fonts` feature off it would have none to
///   offer, and with it on it would *replace* eframe's provider with a second, equivalent index.
///
/// Outfit itself still arrives only through `storytold/craft-fonts` + `craft_fonts_build` faces
/// (R9); `app_face` is then the one call to add, and the snapshot baselines move with it (step 15).
pub mod fonts {
    use egui::FontFamily;

    /// The UI language the stack is ordered for. zen-man's catalogue is English passthrough today
    /// (plan §10 D5/R5); this is the value a translation build passes on.
    const UI_LANGUAGE: &str = "en";

    /// The stack, without installing it — the shape the tests assert against.
    #[must_use]
    pub fn stack() -> craft_ui::fonts::Stack {
        craft_ui::fonts::UiFonts::new(&craft_ui::fonts::I18nScripts)
            .languages(&[UI_LANGUAGE])
            .ui_language(UI_LANGUAGE)
            .egui_face("Hack", &[FontFamily::Proportional])
            .stack()
    }

    /// Install the font stack in `ctx` (idempotent: it sets definitions and providers, it does not
    /// accumulate). Call before [`apply`](super::apply) so the token text styles are set last.
    pub fn install(ctx: &egui::Context) -> craft_ui::fonts::Stack {
        let stack = stack();
        stack.install(ctx);
        stack
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_tokens_quote_the_css_table() {
        let t = Tokens::dark();
        // §2.3 spot checks, from `frontend/css/base.css:2-18`.
        assert_eq!(t.bg_main, Color32::from_rgb(0x0f, 0x11, 0x17));
        assert_eq!(t.accent, Color32::from_rgb(0x58, 0x65, 0xf2));
        assert_eq!(t.row_height, 40.0);
        assert_eq!(t.tab_bar_height, 34.0);
        assert_eq!(t.toolbar_height, 38.0);
        assert_eq!(t.status_height, 30.0);
        assert_eq!(t.left_drawer_width, 250.0);
        assert_eq!(t.right_drawer_width, 300.0);
        // `sidebar.css:43-76`: the row's text column is the nav padding, its own padding, the 18 px
        // icon box and the icon's 10 px margin — 50 px from the panel's edge, and the row is one
        // line tall (`padding: 10px 12px` around a 0.95rem line = `row_height`).
        assert_eq!(
            t.drawer_pad_x + t.drawer_row_pad_x + t.drawer_icon_box + t.row_icon_gap,
            50.0
        );
        assert_eq!(t.drawer_row_corner, 6);
    }

    /// `Color32` is not `Ord`, so the palette's distinctness is a pairwise walk.
    fn distinct_group_colours(t: &Tokens) -> bool {
        let paint: Vec<Color32> = GroupColor::ALL.iter().map(|c| t.group_color(*c)).collect();
        paint
            .iter()
            .enumerate()
            .all(|(ix, colour)| paint[ix + 1..].iter().all(|other| other != colour))
    }

    #[test]
    fn group_colours_are_the_nine_palette_values() {
        let t = Tokens::dark();
        assert_eq!(
            t.group_color(GroupColor::A),
            Color32::from_rgb(0xff, 0x4a, 0x4a)
        );
        assert_eq!(
            t.group_color(GroupColor::B),
            Color32::from_rgb(0x3b, 0x82, 0xf6)
        );
        assert_eq!(
            t.group_color(GroupColor::C),
            Color32::from_rgb(0x10, 0xb9, 0x81)
        );
        assert_eq!(
            t.group_color(GroupColor::D),
            Color32::from_rgb(0xfb, 0xbf, 0x24)
        );
        assert_eq!(
            t.group_color(GroupColor::E),
            Color32::from_rgb(0x8b, 0x5c, 0xf6)
        );
        // The four letters the port added: no Go value exists, so the palette *is* the contract.
        assert_eq!(
            t.group_color(GroupColor::F),
            Color32::from_rgb(0xf9, 0x73, 0x16)
        );
        assert_eq!(
            t.group_color(GroupColor::G),
            Color32::from_rgb(0x06, 0xb6, 0xd4)
        );
        assert_eq!(
            t.group_color(GroupColor::H),
            Color32::from_rgb(0xec, 0x48, 0x99)
        );
        assert_eq!(
            t.group_color(GroupColor::I),
            Color32::from_rgb(0x14, 0xb8, 0xa6)
        );
        assert!(distinct_group_colours(&t), "nine distinct colours");
        let light = Tokens::light();
        assert_ne!(
            light.group_color(GroupColor::A),
            t.group_color(GroupColor::A),
            "and a light-mode ramp of its own"
        );
        assert!(
            distinct_group_colours(&light),
            "nine distinct light colours"
        );
    }

    #[test]
    fn apply_round_trips_through_tokens() {
        let ctx = egui::Context::default();
        apply(&ctx, ThemeMode::Dark);
        let style = ctx.global_style();
        assert!(style.visuals.dark_mode);
        assert_eq!(tokens(&style), Tokens::dark());
        apply(&ctx, ThemeMode::Light);
        let style = ctx.global_style();
        assert!(!style.visuals.dark_mode);
        assert_eq!(tokens(&style), Tokens::light());
    }

    #[test]
    fn selected_row_tint_is_the_accent_at_fifteen_percent() {
        let t = Tokens::dark();
        // rgba(88,101,242,0.15): 0.15 * 255 = 38.25 -> 38.
        assert_eq!(
            t.selected_row_bg,
            Color32::from_rgba_unmultiplied(88, 101, 242, 38)
        );
    }

    /// The glyph the font decision turns on: a tree/group arrow, drawn by step 8's tab bar and file
    /// rows, that egui's own proportional chain has no face for.
    const SYMBOL: char = '\u{25b8}';

    /// Every non-ASCII glyph the shell paints, in one list (plan §9.2 step-8 row).
    ///
    /// This fork of egui **panics** on a missing glyph (`epaint/src/text/fonts.rs:825-851`), so a
    /// character the stack cannot draw is a crash in the listing, not a cosmetic defect. Two
    /// candidates step 8 removed for exactly that reason: `⟳` U+27F3 (the toolbar now paints the
    /// `refresh` SVG) and `⚠` U+26A0 (the banner and the error row use a warning-coloured `!`).
    /// Both are asserted *absent* below, so this list cannot silently grow back to include them.
    const SHELL_GLYPHS: [char; 14] = [
        '▸', '▾', '▴', '▲', '▼', '▶', '×', '·', '—', '…', '●', '←', '↑', '→',
    ];

    /// …and the two the stack provably lacks.
    const UNDRAWABLE_GLYPHS: [char; 2] = ['\u{27f3}', '\u{26a0}'];

    #[test]
    fn every_glyph_the_shell_paints_has_a_face_in_the_installed_stack() {
        let font = Tokens::dark().font();
        let mut fonts = fonts::stack().fonts(false);
        for glyph in SHELL_GLYPHS {
            assert!(
                fonts.has_glyph(&font, glyph),
                "the installed stack cannot draw {glyph} (U+{:04X}), which in this fork is a panic \
                 in the first frame that shows it",
                u32::from(glyph)
            );
        }
        for glyph in UNDRAWABLE_GLYPHS {
            assert!(
                !fonts.has_glyph(&font, glyph),
                "U+{:04X} became drawable — the shell may stop working around it",
                u32::from(glyph)
            );
        }
    }

    #[test]
    fn fonts_install_adds_the_symbol_face_eguis_defaults_lack() {
        let font = Tokens::dark().font();
        // 1. the untouched egui default cannot draw it — this is the state step 7 shipped in.
        let mut defaults = egui::epaint::text::Fonts::new(
            egui::epaint::text::TextOptions::default(),
            egui::FontDefinitions::default(),
        );
        assert!(
            !defaults.has_glyph(&font, SYMBOL),
            "egui's own proportional faces must be the failing baseline, or the test proves nothing"
        );
        // 2. the stack does: Hack follows the token face in the proportional chain.
        let stack = fonts::stack();
        assert!(
            stack.fonts(false).has_glyph(&font, SYMBOL),
            "the installed stack is what draws the shell's arrows"
        );
        let chain = stack.chain(&egui::FontFamily::Proportional);
        assert_eq!(
            chain.first().map(String::as_str),
            Some("Ubuntu-Light"),
            "the stack keeps egui's proportional face first (R9: no bundled face)"
        );
        assert_eq!(
            chain.last().map(String::as_str),
            Some("Hack"),
            "the symbol face is the last configured face, ahead of the system fallback"
        );
        // 3. and `install` really moves the *context*, not just the value it returns: after one
        //    pass the context's own definitions are the stack's chains.
        let ctx = egui::Context::default();
        let installed = fonts::install(&ctx);
        let mut output = ctx.run_ui(egui::RawInput::default(), |_| {});
        // The pass may leave a font-texture delta with no backend to apply it: egui asserts on drop.
        output.textures_delta.clear();
        let chain = installed.chain(&egui::FontFamily::Proportional).to_vec();
        let definitions = ctx.fonts(|f| f.definitions().clone());
        assert_eq!(
            definitions.families.get(&egui::FontFamily::Proportional),
            Some(&chain),
            "`install` left the context on egui's own definitions"
        );
        assert!(
            definitions.font_data.contains_key("Hack"),
            "the symbol face's data never reached the context"
        );
        assert!(
            ctx.global_style()
                .text_styles
                .contains_key(&egui::TextStyle::Body)
        );
    }
}
