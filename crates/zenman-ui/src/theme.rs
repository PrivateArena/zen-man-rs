//! The zen-man design tokens (plan §5.7 `theme.rs` row).
//!
//! **This module owns every colour literal in the crate** — the `xtask ci` colour guard greps the
//! rest of `crates/zenman-ui/src` for `Color32::` constructions and fails on any hit. The values are
//! the §2.3 visual-spec table, quoted verbatim from the Go app's `frontend/css/base.css:2-18`
//! (and re-measured against `docs/parity/*.png` by step 15's human gate, never by pixel diff).
//!
//! Geometry is here too: the 40 px row / 38 px tab / 50 px toolbar / 48 px header / 30 px status bar
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
    /// Menu shadow wash, the paint of `0 10px 25px rgba(0,0,0,.5)`; egui has no shadow blur, so the
    /// shadow is a darkened panel fill (deviation recorded in the step-7 §9.2 row).
    pub menu_shadow: Color32,
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
    // ---- geometry (§2.3 px table) ---------------------------------------------------------------
    /// File row height: 40.
    pub row_height: f32,
    /// Tab bar height: 38.
    pub tab_bar_height: f32,
    /// Pane toolbar height: 50.
    pub toolbar_height: f32,
    /// Global header height: 48.
    pub header_height: f32,
    /// Status bar height: 30.
    pub status_height: f32,
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
    /// Quick-find dialog width fraction: 0.8 (of the viewport, capped by [`Self::dialog_max_width`]).
    pub quick_find_fraction: f32,
    /// Search-manager dialog area fraction: 0.85.
    pub search_manager_fraction: f32,
    /// Dialog max width: 1200 (quick-find) — the search manager caps at 1100.
    pub dialog_max_width: f32,
    /// Search-manager max width: 1100.
    pub search_manager_max_width: f32,
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
            transparent: Color32::TRANSPARENT,
            group_red: Color32::from_rgb(0xff, 0x4a, 0x4a),
            group_blue: Color32::from_rgb(0x3b, 0x82, 0xf6),
            group_green: Color32::from_rgb(0x10, 0xb9, 0x81),
            group_yellow: Color32::from_rgb(0xfb, 0xbf, 0x24),
            group_purple: Color32::from_rgb(0x8b, 0x5c, 0xf6),
            row_height: 40.0,
            tab_bar_height: 38.0,
            toolbar_height: 50.0,
            header_height: 48.0,
            status_height: 30.0,
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
            transparent: Color32::TRANSPARENT,
            group_red: Color32::from_rgb(0xd6, 0x32, 0x32),
            group_blue: Color32::from_rgb(0x25, 0x63, 0xd0),
            group_green: Color32::from_rgb(0x0d, 0x94, 0x67),
            group_yellow: Color32::from_rgb(0xd6, 0x9c, 0x12),
            group_purple: Color32::from_rgb(0x6f, 0x44, 0xd3),
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
        }
    }

    /// The proportional font at the token size (the `DockStyle` font, table rows, tab labels).
    #[must_use]
    pub fn font(&self) -> FontId {
        FontId::proportional(self.font_size)
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

/// Font installation (plan §5.7 `theme::fonts` row; §10 decision R9: **no bundled Outfit**).
///
/// The plan's first choice was `craft_ui::fonts::UiFonts`, which needs a `LanguageScripts` provider
/// and a `craft_i18n` catalogue to resolve families; egui's built-in proportional+emoji fonts are
/// what the scaffold window has been drawing with since step 1, they need no vendored files, and
/// R9 already records the Outfit compromise. So this installs the defaults and is the single place
/// a later step swaps in `UiFonts`/`craft_fonts_build` faces.
pub mod fonts {
    /// Install the font stack. Idempotent: egui's defaults are already resident, so this is a hook,
    /// not work — its job is to be the one call site the binary makes.
    pub fn install(ctx: &egui::Context) {
        // No faces to add today; touching the context keeps the API honest (and a test can prove
        // the context still resolves its default families after the call).
        let _ = ctx;
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
        assert_eq!(t.tab_bar_height, 38.0);
        assert_eq!(t.toolbar_height, 50.0);
        assert_eq!(t.header_height, 48.0);
        assert_eq!(t.status_height, 30.0);
        assert_eq!(t.left_drawer_width, 250.0);
        assert_eq!(t.right_drawer_width, 300.0);
    }

    #[test]
    fn group_colours_are_the_five_hex_values() {
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

    #[test]
    fn fonts_install_keeps_the_default_families_resolvable() {
        let ctx = egui::Context::default();
        fonts::install(&ctx);
        let style = ctx.global_style();
        assert!(style.text_styles.contains_key(&egui::TextStyle::Body));
    }
}
