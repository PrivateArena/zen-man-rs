//! The address strip: back / forward / up, the path as clickable segments, and the edit field
//! (plan §5.8 `breadcrumb.rs` row).
//!
//! Go's was a row of spans whose empty area turned into a text input (the address bar in
//! `main.js`). The same, with the two history steps and `..` made reachable by mouse — plan §2.3
//! lists all three among the keys the JS app never had.
//!
//! A typed path resolves through [`FsPath::resolve`] (plan §4.2), which is what rejects `..` before
//! anything is listed: the address bar is the one place a traversal string could enter the app, and
//! in the port it enters a *type* that cannot hold one.

use egui::{Ui, vec2};

use zenman_core::path::FsPath;

use crate::theme::Tokens;

use super::file_pane::PaneCommand;

/// The strip's input (plan row's `BreadcrumbModel`).
pub struct BreadcrumbModel<'a> {
    /// The listed path.
    pub path: &'a FsPath,
    /// There is somewhere to go back to (`Tab::can_go_back`).
    pub can_back: bool,
    /// There is somewhere to go forward to (`Tab::can_go_forward`).
    pub can_forward: bool,
    /// The address field is open (the pane owns the flag, so a click can open it).
    pub editing: &'a mut bool,
    /// Its buffer, owned by the pane (plan row's `buffer: &'a mut String`).
    pub buffer: &'a mut String,
}

/// Draw the strip and collect its commands.
pub fn show(ui: &mut Ui, m: &mut BreadcrumbModel<'_>, tokens: &Tokens, out: &mut Vec<PaneCommand>) {
    let height = tokens.toolbar_height;
    egui::Frame::new()
        .fill(tokens.bg_card)
        .inner_margin(egui::Margin::symmetric(tokens.cell_padding as i8, 0))
        .show(ui, |ui| {
            ui.allocate_ui_with_layout(
                vec2(ui.available_width(), height),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    let back = icon_button(ui, "←", "Back", m.can_back, tokens);
                    if back.clicked() && m.can_back {
                        out.push(PaneCommand::Back);
                    }
                    let forward = icon_button(ui, "→", "Forward", m.can_forward, tokens);
                    if forward.clicked() && m.can_forward {
                        out.push(PaneCommand::Forward);
                    }
                    let up = icon_button(ui, "↑", "Parent directory", true, tokens);
                    if up.clicked() {
                        out.push(PaneCommand::Up);
                    }
                    if *m.editing {
                        edit_field(ui, m, tokens, out);
                    } else {
                        let response = path_labels(ui, m.path, tokens, out);
                        // Go's address bar: clicking the free space to the right of the path opens
                        // the editor.
                        if response.clicked() {
                            *m.editing = true;
                        }
                    }
                },
            );
        });
}

/// The path, segment by segment, each one a navigation.
fn path_labels(
    ui: &mut Ui,
    path: &FsPath,
    tokens: &Tokens,
    out: &mut Vec<PaneCommand>,
) -> egui::Response {
    let mut current = FsPath::ROOT;
    let segments = path.components();
    let mut last_response = ui.available_rect_before_wrap();
    for (ix, segment) in segments.iter().enumerate() {
        let is_last = ix + 1 == segments.len();
        if let Ok(joined) = current.join(segment) {
            current = joined;
        }
        let target = current.clone();
        let label = format!("{segment}/");
        let button = egui::Button::new(
            egui::RichText::new(label)
                .color(if is_last {
                    tokens.text_active
                } else {
                    tokens.text_muted
                })
                .size(tokens.font_size),
        )
        .frame(false);
        let response = ui.add(button).on_hover_text(target.to_string());
        if response.clicked() && !is_last {
            out.push(PaneCommand::Navigate(target));
        }
        last_response = response.rect;
    }
    if segments.is_empty() {
        last_response = ui
            .add(
                egui::Button::new(
                    egui::RichText::new("/")
                        .color(tokens.text_active)
                        .size(tokens.font_size),
                )
                .frame(false),
            )
            .rect;
    }
    // The free space after the last segment.
    let rest = ui.available_rect_before_wrap();
    let space = ui
        .interact(
            egui::Rect::from_min_max(egui::pos2(last_response.max.x, rest.min.y), rest.max),
            egui::Id::unique("zenman-crumb-space"),
            egui::Sense::click(),
        )
        .on_hover_text("Edit the path");
    ui.advance_cursor_after_rect(space.rect);
    space
}

/// The address field: Enter resolves, `Esc` cancels (plan §4.2's guard is what makes the resolve
/// safe).
fn edit_field(
    ui: &mut Ui,
    m: &mut BreadcrumbModel<'_>,
    tokens: &Tokens,
    out: &mut Vec<PaneCommand>,
) {
    let field = egui::TextEdit::singleline(m.buffer)
        .font(egui::TextStyle::Monospace)
        .desired_width(ui.available_width() - tokens.row_icon_size * 2.0);
    let response = ui.add(field).on_hover_text("Type a path, Enter to list it");
    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
    let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
    if (enter && response.has_focus()) || escape {
        if enter {
            // Plan §4.2: the address bar is the one entry point for a user-typed path, and
            // `FsPath::resolve` is the guard — a `..` in it is rejected here, never listed.
            let raw = m.buffer.clone();
            match resolve_address(&raw, m.path) {
                Ok(path) => out.push(PaneCommand::Navigate(path)),
                Err(e) => out.push(PaneCommand::BadAddress(e.user_message())),
            }
        }
        m.buffer.clear();
        *m.editing = false;
    }
}

/// Resolve a typed address against the tab's own path (relative input means "under here",
/// absolute means "there").
fn resolve_address(raw: &str, cwd: &FsPath) -> Result<FsPath, zenman_core::error::CoreError> {
    let cwd = cwd.to_string();
    FsPath::resolve(raw, std::path::Path::new(&cwd), None)
}

/// A small square history button.
fn icon_button(
    ui: &mut Ui,
    glyph: &str,
    tooltip: &str,
    enabled: bool,
    tokens: &Tokens,
) -> egui::Response {
    let side = tokens.toolbar_height - tokens.header_button_inset;
    let (rect, response) = ui.allocate_exact_size(vec2(side, side), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        if response.hovered() && enabled {
            ui.painter().rect_filled(
                rect,
                egui::CornerRadius::same(tokens.corner_radius),
                tokens.bg_hover,
            );
        }
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            glyph,
            tokens.font(),
            if !enabled {
                tokens.disabled_text
            } else if response.hovered() {
                tokens.text_active
            } else {
                tokens.text_main
            },
        );
    }
    response.on_hover_text(if enabled {
        tooltip.to_string()
    } else {
        format!("{tooltip}: nowhere to go")
    })
}
