//! The toast stack (plan §7 step 7: "earlier than any feature that can fail").
//!
//! Mirrors photocraft's `notices.rs` pattern the plan names: an `egui::Area` anchored
//! right-bottom, newest last, each notice expiring on its own clock. The Go app had no toasts at
//! all — errors were `alert()`s that froze the page — so this is a §6.2-class improvement and the
//! delivery path for every `StartupNote` and job failure.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use egui::{Align2, Area, Color32, CornerRadius, Frame, Id, Margin, Stroke, Ui, Vec2};

use crate::theme::Tokens;

/// How loud a notice is. Deliberately parallel to core's `NoticeLevel` (mapped in `jobs_ui::route`)
/// but independent of it — the widget does not know the engine exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// FYI.
    Info,
    /// Worked, but not how it was asked.
    Success,
    /// Degraded or partially done.
    Warning,
    /// Did not happen.
    Error,
}

/// One posted notice.
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    /// Loudness.
    pub level: Level,
    /// The text.
    pub text: String,
    /// When it was posted (expiry is measured from here).
    pub posted: Instant,
}

/// How long a notice stays before it fades.
pub const NOTICE_TTL: Duration = Duration::from_secs(6);

/// The most the stack will show; older ones are dropped at `post` time, not at draw time (a flood
/// of warnings from a partial paste must not grow the queue without bound).
pub const NOTICE_CAP: usize = 8;

/// How far the stack sits inside the window's right-bottom corner.
const ANCHOR_INSET: f32 = 12.0;
/// Gap between two stacked notices.
const STACK_GAP: f32 = 6.0;
/// A card's own padding (egui takes `Margin` in integer points).
const CARD_SIDE_MARGIN: i8 = 12;
/// …and its smaller vertical padding, so the stack reads as one block.
const CARD_TOP_MARGIN: i8 = 8;

/// The toast queue, owned by `ZenApp` (plan §4.3: notices are app-level, not pane-level).
#[derive(Clone, Debug, Default)]
pub struct State {
    queue: VecDeque<Notice>,
}

impl State {
    /// Post a notice (photocraft's `notices::post`, plan §2.2 "absent from craft_ui" list).
    pub fn post(&mut self, level: Level, text: impl Into<String>) {
        self.post_at(Instant::now(), level, text);
    }

    /// The testable form of [`Self::post`].
    pub fn post_at(&mut self, now: Instant, level: Level, text: impl Into<String>) {
        self.queue.push_back(Notice {
            level,
            text: text.into(),
            posted: now,
        });
        while self.queue.len() > NOTICE_CAP {
            self.queue.pop_front();
        }
    }

    /// Drop expired notices; returns how many went.
    pub fn expire(&mut self, now: Instant) -> usize {
        let before = self.queue.len();
        self.queue
            .retain(|notice| now.duration_since(notice.posted) < NOTICE_TTL);
        before - self.queue.len()
    }

    /// The queue, oldest first (render order top-to-bottom, newest nearest the corner).
    #[must_use]
    pub fn queued(&self) -> &VecDeque<Notice> {
        &self.queue
    }

    /// The accent for a level (the dot; the text colour is the theme's).
    #[must_use]
    pub fn color(level: Level, tokens: &Tokens) -> Color32 {
        match level {
            Level::Info => tokens.text_muted,
            Level::Success => tokens.success,
            Level::Warning => tokens.warning,
            Level::Error => tokens.danger,
        }
    }

    /// Draw the stack anchored to the window's right-bottom corner (empty queue draws nothing, so
    /// this is cheap to call every frame).
    pub fn show(&mut self, ctx: &egui::Context, tokens: &Tokens) {
        self.expire(Instant::now());
        if self.queue.is_empty() {
            return;
        }
        let queue = self.queue.clone();
        Area::new(Id::unique("zenman-notices"))
            .anchor(Align2::RIGHT_BOTTOM, Vec2::splat(-ANCHOR_INSET))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(0.0, STACK_GAP);
                    for notice in &queue {
                        card(ui, notice, tokens);
                    }
                });
            });
    }
}

/// One toast card.
fn card(ui: &mut Ui, notice: &Notice, tokens: &Tokens) {
    let accent = State::color(notice.level, tokens);
    Frame::new()
        .fill(tokens.bg_card)
        .stroke(Stroke::new(1.0, tokens.border))
        .corner_radius(CornerRadius::same(tokens.corner_radius))
        .inner_margin(Margin::symmetric(CARD_SIDE_MARGIN, CARD_TOP_MARGIN))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("●").color(accent).strong());
                ui.label(&notice.text);
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posts_cap_and_expire() {
        let mut state = State::default();
        let t0 = Instant::now();
        for i in 0..NOTICE_CAP + 4 {
            state.post_at(t0, Level::Info, format!("n{i}"));
        }
        assert_eq!(
            state.queued().len(),
            NOTICE_CAP,
            "the cap is enforced at post time"
        );
        assert_eq!(state.queued().back().map(|n| n.text.as_str()), Some("n11"));
        assert_eq!(state.queued().front().map(|n| n.text.as_str()), Some("n4"));
        assert_eq!(state.expire(t0 + NOTICE_TTL / 2), 0);
        assert_eq!(state.expire(t0 + NOTICE_TTL * 2), NOTICE_CAP);
        assert!(state.queued().is_empty());
    }

    #[test]
    fn levels_map_to_distinct_token_colours() {
        let tokens = Tokens::dark();
        let colours = [
            State::color(Level::Info, &tokens),
            State::color(Level::Success, &tokens),
            State::color(Level::Warning, &tokens),
            State::color(Level::Error, &tokens),
        ];
        let unique: std::collections::HashSet<_> = colours.iter().collect();
        assert_eq!(unique.len(), 4, "every level must be visually distinct");
    }
}
