//! The tab strip (plan §5.8 `tab_bar.rs` row): the coloured 38 px strip a pane owns because its
//! dock group is stripless.
//!
//! Widths come from [`craft_ui::tabs::fit`], which is the framework's answer to Go's `scrollWidth`
//! arithmetic (`tabs.js` measured the strip in DOM units and could push the `+` button outside the
//! pane). The plan's Done-when — "40 tabs: `fit` puts the overflow list and the `+` button in the
//! strip at all widths, `+` stays inside the pane bounds" — is [`tests::forty_tabs_never_lose_the_new_tab_button`].

//!
//! Group *runs* used to be computed here (`runs`/`GroupRun`); they were never called, never tested,
//! and keyed on the per-tab colour, which is no longer identity. `items()` of the tab-group step is what
//! walks the strip, and it is tested.

use craft_ui::tabs::TabFit;
use egui::{Id, Rect, Ui};

use zenman_core::store::config::GroupColor;

use crate::state::{Tab, TabGroup};
use crate::theme::Tokens;

/// What the strip asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabCommand {
    /// Show this tab.
    Select(usize),
    /// Close it (step 9's write path is what makes a tab dirty; the button is here now).
    Close(usize),
    /// The tab's own menu, opened over the tab `ix` names — Go's `showTabContextMenu`
    /// (`tabs.js:351-386`). `at` is the pointer, because the menu opens under the cursor and not under
    /// the tab (`positionElementSmartly`, `utils.js:21-44`).
    Context { ix: usize, at: egui::Pos2 },
    /// A new tab in this pane.
    NewTab,
    /// Open / shut the overflow list.
    ToggleOverflow,
}

/// The strip's input (plan row's `TabBarModel`).
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
}

impl TabBarModel<'_> {
    /// The colour a tab's accent bar takes: its group's, or none.
    #[must_use]
    pub fn tab_color(&self, tab: &Tab) -> Option<GroupColor> {
        tab.group
            .and_then(|id| self.groups.iter().find(|group| group.id == id))
            .map(|group| group.color)
    }
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
    // Widths: the label's measured galley plus the chrome, capped by the token maximum.
    let natural: Vec<f32> = m
        .tabs
        .iter()
        .map(|tab| {
            let text =
                ui.painter()
                    .layout_no_wrap(tab.title.clone(), tokens.font(), tokens.text_main);
            (text.size().x + tokens.tab_fixed_chrome).min(tokens.tab_max_width)
        })
        .collect();
    let available = (strip.width() - tokens.tab_new_button - tokens.tab_gap).max(0.0);
    let TabFit {
        shown,
        overflow,
        overflow_width,
    } = craft_ui::tabs::fit(
        &natural,
        m.active,
        available,
        tokens.tab_min_width,
        tokens.tab_overflow_width,
    );
    let mut x = strip.min.x + tokens.tab_gap;
    let bottom = strip.max.y - tokens.tab_vertical_inset;
    let height = strip.height() - 2.0 * tokens.tab_vertical_inset;
    for (ix, width) in shown {
        let tab_rect =
            Rect::from_min_size(egui::pos2(x, bottom - height), egui::vec2(width, height));
        x += width + tokens.tab_gap;
        if !ui.is_rect_visible(tab_rect) {
            continue;
        }
        let Some(tab) = m.tabs.get(ix) else { continue };
        let selected = ix == m.active;
        let hovered = ui
            .interact(tab_rect, id.with(("tab", ix)), egui::Sense::click())
            .hovered();
        let fill = if selected {
            tokens.bg_card
        } else if hovered {
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
        ui.painter().rect_stroke(
            tab_rect,
            radius,
            tokens.hairline(),
            egui::StrokeKind::Inside,
        );
        if let Some(color) = m.tab_color(tab) {
            ui.painter().rect_filled(
                Rect::from_min_size(
                    tab_rect.min,
                    egui::vec2(tokens.accent_bar_width, tab_rect.height()),
                ),
                0.0,
                tokens.group_color(color),
            );
        }
        let close = Rect::from_min_size(
            egui::pos2(
                tab_rect.max.x - tokens.tab_close_size - 2.0,
                tab_rect.center().y - tokens.tab_close_size / 2.0,
            ),
            egui::vec2(tokens.tab_close_size, tokens.tab_close_size),
        );
        let label = Rect::from_min_size(
            egui::pos2(
                tab_rect.min.x + tokens.tab_label_indent,
                tab_rect.center().y - tokens.font_size,
            ),
            egui::vec2(
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
            .interact(close, id.with(("tab-close", ix)), egui::Sense::click())
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
            out.push(TabCommand::Close(ix));
        } else {
            // One response for both buttons: `Sense::click()` answers *either*, and a right-click used
            // to *select* the tab here. The menu owns the tab under the cursor instead — Go's
            // `showTabContextMenu` never called `switchTab` — so the secondary press must not switch to
            // it, and the primary press still does.
            let tab = ui.interact(tab_rect, id.with(("tab", ix)), egui::Sense::click());
            if tab.secondary_clicked() {
                out.push(TabCommand::Context {
                    ix,
                    at: tab.interact_pointer_pos().unwrap_or(tab_rect.min),
                });
            } else if tab.clicked() {
                out.push(TabCommand::Select(ix));
            }
        }
    }
    // The overflow list, then the `+` button — both *after* the tabs, inside the strip's own rect.
    let mut right_max = strip.max.x - tokens.tab_gap;
    if !overflow.is_empty() {
        right_max -= overflow_width;
        let button = Rect::from_min_size(
            egui::pos2(right_max, bottom - height),
            egui::vec2(overflow_width, height),
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
                        out.push(TabCommand::Select(*ix));
                    }
                }
            });
        }
    }
    let new = Rect::from_min_size(
        egui::pos2(right_max - tokens.tab_new_button, bottom - height),
        egui::vec2(tokens.tab_new_button, height),
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
}
