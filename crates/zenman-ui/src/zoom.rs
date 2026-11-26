//! The shell's UI zoom: the parent menu's `− | 100% | +` row, egui's `zoom_factor`, and
//! `config.json`'s `zoomPercent`.
//!
//! # One level, two front ends
//!
//! The row ([`crate::panels::app_menu`]) and egui's own <kbd>Ctrl</kbd>+<kbd>+</kbd> /
//! <kbd>Ctrl</kbd>+<kbd>-</kbd> / <kbd>Ctrl</kbd>+<kbd>0</kbd> chords (`egui::gui_zoom`, on by
//! default) both end in `egui::Context::set_zoom_factor`, so the level is *read back* from the
//! context every frame ([`Zoom::tick`]) rather than only counted from clicks. That is what lets the
//! printed number stay true while the keyboard is being used, and it is why the bounds here are
//! egui's own ([`MIN_ZOOM_PERCENT`]..=[`MAX_ZOOM_PERCENT`] rather than a browser's narrower 30–300):
//! a level the chord can reach but the config refuses to save is a bug report waiting for a restart.
//!
//! The step is 10% and the rounding is egui's (`egui::gui_zoom::zoom_in`), so a click and a chord
//! from the same level arrive at the same level — and the round trip through per cent is exact on
//! that grid, which is what stops the read-back from re-triggering itself.
//!
//! # Who owns the value at boot
//!
//! eframe persists `egui::Memory` — `zoom_factor` included — in `ui.ron` next to `config.json`, and
//! restores it when it builds the context (`winit_integration::create_egui_context`). Both files
//! therefore claim the same setting, and `config.json` wins: `apps/zenman` sets the factor in the
//! `run_native` creation hook, which runs after that restore and before the first frame, so nothing
//! is ever drawn at the stale scale. [`Zoom::from_config`] then asks for the same level on the first
//! tick as well — a no-op against egui's own "already at this factor" guard, and the reason a shell
//! built without that hook (`ZenApp::new`, the tests) still boots at the configured level. `ui.ron`
//! is rewritten to the same number on the way out, so the two converge rather than fight.

use std::time::Instant;

use egui::Context;
use zenman_core::services::workspaces::AUTOSAVE_DEBOUNCE;
use zenman_core::store::config::{
    DEFAULT_ZOOM_PERCENT, MAX_ZOOM_PERCENT, MIN_ZOOM_PERCENT, ZOOM_STEP_PERCENT,
};

/// The denominator of a per cent. Named, because the geometry gate (`xtask/src/source_guards.rs`)
/// counts a bare `100.0` in this crate as an unnamed measurement.
const PER_CENT: f32 = 100.0;

/// The scale factor egui multiplies the native pixels-per-point by to draw at a per-cent level — the
/// one place the two units meet, so the shell, the row and `apps/zenman`'s boot hook cannot disagree.
#[must_use]
pub fn factor_of(percent: u32) -> f32 {
    percent as f32 / PER_CENT
}

/// The shell's zoom state, and the only place that decides what the UI is scaled by.
#[derive(Clone, Debug)]
pub struct Zoom {
    /// The level the shell is at — or, inside the frame of a click, is on its way to.
    percent: u32,
    /// The level `config.json` carried, applied on the first tick; see the module's boot note.
    boot: Option<u32>,
    /// The level a click asked for, not yet handed to egui.
    wanted: Option<u32>,
    /// `percent` is not what `config.json` holds.
    dirty: bool,
    /// When the debounced write is due — `None` until the first tick after a change, which is what
    /// starts the clock (and keeps a burst of clicks to one write).
    due: Option<Instant>,
}

impl Zoom {
    /// Start at the level the config holds, and ask for it once on the first tick.
    ///
    /// The clamp is a guard, not a policy: the binary's boot path validates the file first
    /// (`ZenConfig::validate` refuses what it cannot draw at), so this only ever fires for a caller
    /// that hands the shell a `ZenConfig` it built itself — which is what `ZenApp::new` and the
    /// tests are allowed to do.
    #[must_use]
    pub fn from_config(percent: u32) -> Self {
        let percent = percent.clamp(MIN_ZOOM_PERCENT, MAX_ZOOM_PERCENT);
        Self {
            percent,
            boot: Some(percent),
            wanted: None,
            dirty: false,
            due: None,
        }
    }

    /// The current level, in per cent.
    #[must_use]
    pub fn percent(&self) -> u32 {
        self.percent
    }

    /// What the menu's row paints.
    #[must_use]
    pub fn status(&self) -> ZoomStatus {
        ZoomStatus {
            percent: self.percent,
            at_min: self.percent <= MIN_ZOOM_PERCENT,
            at_max: self.percent >= MAX_ZOOM_PERCENT,
        }
    }

    /// One step up (`Ctrl`+`+`'s button half).
    pub fn zoom_in(&mut self) {
        let next = self.shifted(1);
        self.request(next);
    }

    /// One step down.
    pub fn zoom_out(&mut self) {
        let next = self.shifted(-1);
        self.request(next);
    }

    /// Back to the native scale — <kbd>Ctrl</kbd>+<kbd>0</kbd>'s button half, and the middle of the
    /// row when the row is not already there.
    pub fn reset(&mut self) {
        self.request(DEFAULT_ZOOM_PERCENT);
    }

    /// The frame tick, run before anything is drawn at a scale (`app.rs::logic`).
    ///
    /// Hands egui the level the row asked for, adopts the level the chord asked for, and returns the
    /// level to write to `config.json` when the debounce has elapsed — `None` on every other frame,
    /// so the caller's IO is one line and this owns the arithmetic and the clock.
    pub fn tick(&mut self, ctx: &Context, now: Instant) -> Option<u32> {
        if let Some(percent) = self.boot.take() {
            self.wanted = Some(percent);
        }
        // `set_zoom_factor` only lands at the start of the *next* pass, so what `zoom_factor()`
        // answers here is this frame's truth: either the level we asked for last tick (equal, and
        // nothing to do) or a level this shell never asked for — the <kbd>Ctrl</kbd> chord — which is
        // adopted, printed and persisted exactly as a click would have been. A click and a chord in
        // the *same* frame is the one case where the two front ends meet: the chord runs at the end
        // of the pass and wins, and the row prints its level from the next frame on.
        if self.wanted.is_none() {
            let seen = Self::percent_of(ctx.zoom_factor());
            if seen != self.percent {
                self.request(seen);
            }
        }
        if let Some(percent) = self.wanted.take() {
            ctx.set_zoom_factor(factor_of(percent));
        }
        if self.dirty {
            // A shell with nothing else to draw sleeps, and the debounce would then wait for the next
            // keystroke instead of its own deadline.
            ctx.request_repaint_after(AUTOSAVE_DEBOUNCE);
        }
        self.write_due(now)
    }

    /// The debounce clock: `Some` once the level has sat unwritten for [`AUTOSAVE_DEBOUNCE`], and
    /// then only once. Split from [`Zoom::tick`] so the clock is testable without a frame or a wait:
    /// [`Zoom::request`] restarts it, so a burst of clicks is one write of its last level.
    fn write_due(&mut self, now: Instant) -> Option<u32> {
        if !self.dirty {
            return None;
        }
        match self.due {
            Some(at) if now >= at => {
                self.dirty = false;
                self.due = None;
                Some(self.percent)
            }
            Some(_) => None,
            None => {
                self.due = Some(now + AUTOSAVE_DEBOUNCE);
                None
            }
        }
    }

    /// The level `config.json` still does not have, **debounce ignored** (window close, and the
    /// smoke run's self-close — the same reason `flush_session` exists for the workspace).
    pub fn pending(&mut self) -> Option<u32> {
        if !self.dirty {
            return None;
        }
        self.dirty = false;
        self.due = None;
        Some(self.percent)
    }

    /// `steps` steps of [`ZOOM_STEP_PERCENT`] away from the current level, snapped to the step grid
    /// first: a level that arrived from egui's continuous <kbd>Ctrl</kbd>+wheel still lands on a round
    /// number after one click, and the arithmetic matches `egui::gui_zoom`'s own rounding.
    fn shifted(&self, steps: i32) -> u32 {
        let snapped =
            (self.percent + ZOOM_STEP_PERCENT / 2) / ZOOM_STEP_PERCENT * ZOOM_STEP_PERCENT;
        let next = i64::from(snapped) + i64::from(steps) * i64::from(ZOOM_STEP_PERCENT);
        next.clamp(i64::from(MIN_ZOOM_PERCENT), i64::from(MAX_ZOOM_PERCENT)) as u32
    }

    /// Want `percent`, and remember that the file disagrees.
    fn request(&mut self, percent: u32) {
        if percent == self.percent {
            return;
        }
        self.percent = percent;
        self.wanted = Some(percent);
        self.dirty = true;
        self.due = None;
    }

    /// The inverse of the factor handed to egui, in per cent and on the step grid. A non-finite
    /// factor — a hand-edited `ui.ron` is RON, and `NaN as u32` is `0`, which egui would happily
    /// multiply the screen by — is the native scale rather than an invisible zero.
    fn percent_of(factor: f32) -> u32 {
        if !factor.is_finite() {
            return DEFAULT_ZOOM_PERCENT;
        }
        let percent = (factor * PER_CENT).round();
        percent.clamp(MIN_ZOOM_PERCENT as f32, MAX_ZOOM_PERCENT as f32) as u32
    }
}

/// The zoom row's view of [`Zoom`], copied into the frame's model so the panel that paints the row
/// does not have to borrow the shell it is drawn by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZoomStatus {
    /// The level, in per cent: the number between the two buttons.
    pub percent: u32,
    /// `−` has nothing left to give.
    pub at_min: bool,
    /// `+` has nothing left to give.
    pub at_max: bool,
}

impl ZoomStatus {
    /// The middle cell's caption: `100%`.
    #[must_use]
    pub fn caption(self) -> String {
        format!("{}%", self.percent)
    }
}

impl Default for ZoomStatus {
    /// The native scale with both buttons live: what a row paints when no config is behind it.
    fn default() -> Self {
        Self {
            percent: DEFAULT_ZOOM_PERCENT,
            at_min: false,
            at_max: false,
        }
    }
}

impl Default for Zoom {
    /// The native scale, with no config behind it — what a shell built without a session gets.
    fn default() -> Self {
        Self::from_config(DEFAULT_ZOOM_PERCENT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_steps_are_ten_per_cent_and_eguis_own_rounding() {
        let zoom = Zoom::from_config(100);
        assert_eq!(zoom.shifted(1), 110);
        assert_eq!(zoom.shifted(-1), 90);
        // A level the chord left behind is snapped before it is stepped, so one click is still one
        // readable step (`egui::gui_zoom::zoom_in` from 1.05 goes to 1.2 for the same reason).
        assert_eq!(Zoom::from_config(105).shifted(1), 120);
        assert_eq!(Zoom::from_config(105).shifted(-1), 100);
        assert_eq!(Zoom::from_config(104).shifted(1), 110);
    }

    #[test]
    fn the_bounds_are_eguis_and_both_buttons_stop_there() {
        let mut zoom = Zoom::from_config(MIN_ZOOM_PERCENT);
        assert!(zoom.status().at_min);
        zoom.zoom_out();
        assert_eq!(zoom.percent(), MIN_ZOOM_PERCENT, "no past the floor");
        let mut zoom = Zoom::from_config(MAX_ZOOM_PERCENT);
        assert!(zoom.status().at_max);
        zoom.zoom_in();
        assert_eq!(zoom.percent(), MAX_ZOOM_PERCENT, "no past the ceiling");
        // egui's chord can reach them; a config that then refused to save would lose the level.
        assert_eq!(Zoom::from_config(20).percent(), MIN_ZOOM_PERCENT);
        assert_eq!(Zoom::from_config(500).percent(), MAX_ZOOM_PERCENT);
    }

    #[test]
    fn a_config_outside_the_range_is_clamped_not_believed() {
        assert_eq!(Zoom::from_config(5000).percent(), MAX_ZOOM_PERCENT);
        assert_eq!(Zoom::from_config(1).percent(), MIN_ZOOM_PERCENT);
        assert_eq!(
            Zoom::from_config(95).percent(),
            95,
            "in range is left alone"
        );
    }

    #[test]
    fn the_caption_is_the_level_and_a_percent() {
        assert_eq!(Zoom::from_config(100).status().caption(), "100%");
        assert_eq!(Zoom::from_config(90).status().caption(), "90%");
    }

    #[test]
    fn a_pending_level_is_offered_once_and_then_the_file_is_current() {
        let mut zoom = Zoom::from_config(100);
        assert_eq!(
            zoom.pending(),
            None,
            "booting at the configured level is not a change to write back"
        );
        zoom.zoom_in();
        assert_eq!(zoom.pending(), Some(110));
        assert_eq!(zoom.pending(), None, "and a second flush writes nothing");
    }

    #[test]
    fn a_burst_of_clicks_is_one_write_of_its_last_level() {
        let mut zoom = Zoom::from_config(100);
        let t0 = Instant::now();
        assert_eq!(
            zoom.write_due(t0),
            None,
            "a level the file already has starts no clock"
        );
        zoom.zoom_in();
        assert_eq!(zoom.write_due(t0), None, "the clock starts at the change");
        zoom.zoom_in();
        assert_eq!(
            zoom.write_due(t0 + AUTOSAVE_DEBOUNCE),
            None,
            "and the second click restarts it"
        );
        assert_eq!(
            zoom.write_due(t0 + AUTOSAVE_DEBOUNCE * 2),
            Some(120),
            "one write, of the level the burst ended on"
        );
        assert_eq!(zoom.write_due(t0 + AUTOSAVE_DEBOUNCE * 3), None);
    }

    #[test]
    fn a_factor_that_is_not_a_number_is_the_native_scale() {
        // `ui.ron` is RON, and `NaN as u32` is 0 — which egui would multiply the screen by. Both
        // non-finite shapes go to the native scale rather than to the nearest bound.
        assert_eq!(Zoom::percent_of(f32::NAN), DEFAULT_ZOOM_PERCENT);
        assert_eq!(Zoom::percent_of(f32::INFINITY), DEFAULT_ZOOM_PERCENT);
        assert_eq!(Zoom::percent_of(0.0), MIN_ZOOM_PERCENT);
        assert_eq!(Zoom::percent_of(9.9), MAX_ZOOM_PERCENT);
        // The two units round-trip on the step grid, which is what keeps the frame's read-back from
        // seeing a change where there was none.
        for percent in (MIN_ZOOM_PERCENT..=MAX_ZOOM_PERCENT).step_by(ZOOM_STEP_PERCENT as usize) {
            assert_eq!(Zoom::percent_of(factor_of(percent)), percent, "{percent}%");
        }
    }
}
