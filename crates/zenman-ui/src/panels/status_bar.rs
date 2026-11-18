//! The 30 px status bar (plan §5.8 `status_bar.rs` row).
//!
//! Go's said `N items selected` plus a hover tooltip of the first ten names in *insertion* order
//! (`file-list.js:569`, `Array.from(set).slice(0, 10)`); here the clamp is in **display order**,
//! which is the plan's one-line fix, and the plural is a real test rather than a hope.

use egui::Ui;

use crate::jobs_ui::JobSlot;
use crate::state::Selection;
use crate::theme::Tokens;

/// How many selected names the tooltip lists before it says "and N more" (`file-list.js:578`).
pub const DISPLAY_CAP: usize = 10;

/// Percent per unit fraction: the job line reports a whole-number percentage of an
/// `f32` progress fraction, and the geometry guard wants that number named.
const PERCENT_PER_FRACTION: f32 = 100.0;

/// What the bar's left cell is saying.
#[derive(Clone, Debug, PartialEq)]
pub enum StatusInfo {
    /// `N items` — the listing's own count.
    Count(usize),
    /// The listing failed.
    Error(String),
    /// A job is moving bytes **and has said how many**: the two numbers come from a
    /// `JobProgress`, never from a placeholder. Step 9's paste/copy is what fills this.
    Copying {
        /// Bytes done.
        done: u64,
        /// Bytes total, `None` while unknown.
        total: Option<u64>,
    },
    /// A job is running and its progress is a *fraction* (a listing read, a size walk): the
    /// shell has a label and a 0..=1 fraction, not byte counts, and says exactly that rather than
    /// inventing `0 of 0`.
    Running {
        /// The job's own label, already worded.
        label: String,
        /// Last reported fraction; `None` while the job is a spinner.
        frac: Option<f32>,
    },
    /// The last thing that finished.
    Done(String),
}

/// The bar's input (plan row's `StatusModel`).
pub struct StatusModel<'a> {
    /// The active tab's selection.
    pub selection: &'a Selection,
    /// The selected rows' names, **in display order** — the pane's visual order, not the set's.
    pub names: Vec<String>,
    /// The listing/job state.
    pub info: StatusInfo,
    /// The live jobs, for the right-hand cell.
    pub jobs: &'a [JobSlot],
    /// How many panes the grid shows.
    pub panes: usize,
}

/// Draw the bar.
pub fn show(ui: &mut Ui, m: &StatusModel<'_>, tokens: &Tokens) {
    let rect = ui.available_rect_before_wrap();
    let bar = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), tokens.status_height));
    if ui.is_rect_visible(bar) {
        ui.painter().rect_filled(bar, 0.0, tokens.bg_sidebar);
        ui.painter()
            .hline(bar.x_range(), bar.min.y, tokens.hairline());
    }
    let count = m.selection.len();
    let left = if count == 0 {
        "0 items selected".to_string()
    } else {
        format!("{} item{} selected", count, plural(count))
    };
    let text = match &m.info {
        StatusInfo::Count(rows) => format!("{left} · {rows} item{} in the listing", plural(*rows)),
        StatusInfo::Error(text) => format!("{left} · {text}"),
        StatusInfo::Copying { done, total } => match total {
            Some(total) if *total > 0 => format!(
                "{left} · copying {} of {}",
                crate::widgets::disk_meter::format_size(*done),
                crate::widgets::disk_meter::format_size(*total)
            ),
            _ => format!("{left} · copying"),
        },
        StatusInfo::Running { label, frac } => match frac {
            Some(frac) => format!(
                "{left} · {label} {}%",
                (frac.clamp(0.0, 1.0) * PERCENT_PER_FRACTION).round()
            ),
            None => format!("{left} · {label}"),
        },
        StatusInfo::Done(text) => format!("{left} · {text}"),
    };
    let label = ui.put(
        egui::Rect::from_min_size(
            egui::pos2(
                bar.min.x + tokens.cell_padding,
                bar.center().y - tokens.font_size,
            ),
            egui::vec2(bar.width() * 0.6, tokens.font_size * 2.0),
        ),
        egui::Label::new(
            egui::RichText::new(&text)
                .color(tokens.text_muted)
                .size(tokens.font_size - 1.0),
        )
        .truncate(),
    );
    if count > 0 {
        let _ = label.on_hover_text(selection_tooltip(m));
    }
    let live: Vec<&JobSlot> = m.jobs.iter().filter(|job| job.live).collect();
    let right = if live.is_empty() {
        format!("{} pane{}", m.panes, plural(m.panes))
    } else {
        format!(
            "{} · {} job{} running",
            live[0].label,
            live.len(),
            plural(live.len())
        )
    };
    ui.put(
        egui::Rect::from_min_size(
            egui::pos2(
                bar.max.x - bar.width() * 0.35,
                bar.center().y - tokens.font_size,
            ),
            egui::vec2(
                bar.width() * 0.35 - tokens.cell_padding,
                tokens.font_size * 2.0,
            ),
        ),
        egui::Label::new(
            egui::RichText::new(right)
                .color(tokens.text_muted)
                .size(tokens.font_size - 1.0),
        )
        .truncate(),
    );
    ui.advance_cursor_after_rect(bar);
}

/// The tooltip body: the first [`DISPLAY_CAP`] names in display order, then the remainder
/// (`file-list.js:571-592`, with the order fixed).
#[must_use]
pub fn selection_tooltip(m: &StatusModel<'_>) -> String {
    let shown = m.names.len().min(DISPLAY_CAP);
    let mut lines: Vec<String> = m.names.iter().take(shown).cloned().collect();
    let rest = m.names.len().saturating_sub(shown);
    if rest > 0 {
        lines.push(format!("and {rest} more item{}", plural(rest)));
    }
    lines.join("\n")
}

/// English plural `s`.
#[must_use]
pub fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}
