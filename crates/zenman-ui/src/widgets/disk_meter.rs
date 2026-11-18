//! The disk-usage meter (plan §5.7 step-7 widget list).
//!
//! Go rendered it in `frontend/js/diskspace.js` as a labelled bar fed by `/api/diskspace`; here it
//! is a pure paint given the numbers — the *numbers* come from `zenman_core::platform::Env`
//! (`disk_space`, step 3) and the polling belongs to the right drawer (step 14), so the widget
//! never touches IO and stays testable with fixed inputs (rule 7).

use egui::{Align2, Pos2, Rect, Response, Sense, Ui, Vec2};

use crate::theme::Tokens;

/// Bar height inside the meter row.
pub const BAR_HEIGHT: f32 = 6.0;
/// Row height the meter asks for: the bar plus the text that sits beside it.
const ROW_HEIGHT_ALLOWANCE: f32 = 16.0;
/// The bar stops growing here; the drawer is narrower than a wide window.
const BAR_MAX_WIDTH: f32 = 240.0;
/// Gap between the bar's right edge and its `used of total` label.
const LABEL_GAP: f32 = 8.0;
/// Corner radius of the two bars (egui takes it as an integer point value).
const BAR_CORNER: u8 = 3;
/// One binary step, and the point where the two-decimal form stops being worth its width —
/// formatting constants, not layout, and the geometry guard's `const` clause is what lets them be
/// named here instead of in the token table.
const KIB: f64 = 1024.0;
/// Above this a scaled value is printed without decimals (Go's `formatSize` did the same).
const WHOLE_NUMBER_LIMIT: f64 = 100.0;

/// Format a byte count the way the Go status bar did (`formatSize`, duplicated twice there — here
/// once, plan §2.3's bug list).
#[must_use]
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= KIB && unit + 1 < UNITS.len() {
        value /= KIB;
        unit += 1;
    }
    if value >= WHOLE_NUMBER_LIMIT {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// The used fraction of a volume, `0.0` for an unknown/empty total (never a division by zero).
#[must_use]
pub fn used_fraction(used: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        ((used as f64) / (total as f64)).clamp(0.0, 1.0) as f32
    }
}

/// Paint the meter into `ui` as an `alloc`-free row: label, bar, `used of total`.
///
/// Returns the row response so step 14 can attach the refresh click.
pub fn show(ui: &mut Ui, tokens: &Tokens, label: &str, used: u64, total: u64) -> Response {
    let frac = used_fraction(used, total);
    let desired = egui::vec2(ui.available_width(), BAR_HEIGHT + ROW_HEIGHT_ALLOWANCE);
    let (rect, response) = ui.allocate_at_least(desired, Sense::hover());
    if ui.is_rect_visible(rect) {
        let bar = Rect::from_min_size(
            Pos2::new(rect.min.x, rect.center().y - BAR_HEIGHT / 2.0),
            Vec2::new(rect.width().min(BAR_MAX_WIDTH), BAR_HEIGHT),
        );
        ui.painter()
            .rect_filled(bar, egui::CornerRadius::same(BAR_CORNER), tokens.bg_hover);
        let fill = Rect::from_min_size(bar.min, Vec2::new(bar.width() * frac, bar.height()));
        let colour = if frac > 0.9 {
            tokens.danger
        } else if frac > 0.75 {
            tokens.warning
        } else {
            tokens.accent
        };
        ui.painter()
            .rect_filled(fill, egui::CornerRadius::same(BAR_CORNER), colour);
        let text = format!(
            "{label}: {} of {} used",
            format_size(used),
            format_size(total)
        );
        ui.painter().text(
            Pos2::new(bar.max.x + LABEL_GAP, rect.center().y),
            Align2::LEFT_CENTER,
            text,
            tokens.font(),
            tokens.text_muted,
        );
    }
    response
}

/// The same meter as a single painted bar in a known rect (the drawer header uses this).
pub fn paint_bar(painter: &egui::Painter, rect: Rect, tokens: &Tokens, frac: f32) {
    let frac = frac.clamp(0.0, 1.0);
    painter.rect_filled(rect, egui::CornerRadius::same(BAR_CORNER), tokens.bg_hover);
    painter.rect_filled(
        Rect::from_min_size(rect.min, Vec2::new(rect.width() * frac, rect.height())),
        egui::CornerRadius::same(BAR_CORNER),
        tokens.accent,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_size_matches_go_tiers() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(5 * 1024 * 1024 * 1024), "5.0 GiB");
        assert_eq!(format_size(120 * 1024 * 1024 * 1024), "120 GiB");
    }

    #[test]
    fn fraction_is_clamped_and_zero_total_is_safe() {
        assert_eq!(used_fraction(0, 0), 0.0);
        assert_eq!(used_fraction(500, 1000), 0.5);
        assert_eq!(used_fraction(u64::MAX, 1), 1.0);
    }

    #[test]
    fn bar_geometry_is_finite_for_every_input() {
        for (used, total) in [(0, 0), (1, u64::MAX), (u64::MAX, u64::MAX), (7, 7)] {
            let f = used_fraction(used, total);
            assert!(
                f.is_finite() && (0.0..=1.0).contains(&f),
                "{used}/{total} -> {f}"
            );
        }
    }
}
