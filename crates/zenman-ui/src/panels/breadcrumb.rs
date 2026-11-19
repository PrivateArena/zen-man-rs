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
                        // The trailing button is subtracted from the trail's budget *first*, which
                        // is what guarantees the collapse can never push it off screen.
                        let side = tokens.toolbar_height - tokens.header_button_inset;
                        let budget =
                            (ui.available_width() - side - tokens.crumb_separator_gap).max(0.0);
                        let response = path_chips(ui, m.path, budget, tokens, out);
                        // Go's address bar: clicking the free space to the right of the path opens
                        // the editor.
                        if response.clicked() {
                            *m.editing = true;
                        }
                        if edit_button(ui, tokens).clicked() {
                            *m.editing = true;
                        }
                    }
                },
            );
        });
}

/// The volume / store root, named the way the reference names it (`navigation.js:190`,
/// `<span class="breadcrumb-segment" data-path="/">Root</span>`).
const ROOT_LABEL: &str = "Root";

/// What stands in for the segments a narrow trail had to hide. `…` is in the glyph-guard set
/// (`theme.rs::every_glyph_the_shell_paints_has_a_face_in_the_installed_stack`).
const COLLAPSE_LABEL: &str = "…";

/// The reference's separator between two chips (`navigation.js`'s `.breadcrumb-separator`).
const SEPARATOR_LABEL: &str = "/";

/// One crumb: the label the reference would print, and the path clicking it lists.
pub struct Crumb {
    /// The text of the chip.
    pub label: String,
    /// The path a click on the chip lists.
    pub target: FsPath,
}

/// The trail of one path: the store root first, then one crumb per [`FsPath::components`] segment.
///
/// The store root is `path.store`'s own root, not the filesystem's: inside an archive (step 13)
/// `Root` is the archive, the host segments that lead to it stay clickable as *filesystem* paths,
/// and the container's own chip — plus a nested member's — is the *store* it opens, which is what
/// Go's `data-path` + archive query triple did (`navigation.js:190`, step 13b).
#[must_use]
pub fn trail(path: &FsPath) -> Vec<Crumb> {
    let mut out = vec![Crumb {
        label: ROOT_LABEL.to_owned(),
        target: path.store.root_path(),
    }];
    // How many leading components belong to the archive's *host* (its own path), and how many to the
    // nested member: those accumulate on the filesystem, the ones after them on the archive store.
    let host_len = path
        .store
        .host()
        .map_or(0, |host| host.components_clean().count());
    let nested_len = path.store.nested().map_or(0, |nested| {
        nested.split('/').filter(|s| !s.is_empty()).count()
    });
    let boundary = host_len + nested_len;
    let mut current = FsPath::ROOT;
    let mut crossed = false;
    for (ix, segment) in path.components().into_iter().skip(1).enumerate() {
        if !crossed && ix >= boundary {
            current = path.store.root_path();
            crossed = true;
        }
        // A segment `FsPath::join` refuses cannot be in a trail this function was handed: `path`
        // itself is built of them, and `components` never emits `.` or `..`.
        if let Ok(joined) = current.join(&segment) {
            current = joined;
        }
        // Go's chip trail made the container itself clickable *as a store* (`navigation.js`'s
        // `data-path` plus the archive query triple), so the two boundary chips target the stores
        // they open and not the bytes on disk: the host chip opens the container, the nested
        // member's chip the inner store (depth 2, the cap). Step 13b.
        let store_target = if host_len > 0 && ix == host_len - 1 {
            path.store
                .host()
                .and_then(|host| FsPath::from_abs(host.clone()).as_store().ok())
        } else if nested_len > 0 && ix == boundary - 1 {
            Some(path.store.root_path())
        } else {
            None
        };
        out.push(Crumb {
            label: segment,
            target: store_target.unwrap_or_else(|| current.clone()),
        });
    }
    out
}

/// One box of the laid-out trail: a chip, or the `/` between two chips.
struct Slot {
    label: String,
    /// `None` for a separator, which is painted and never interactive.
    target: Option<FsPath>,
    width: f32,
    /// The chip the trail is *at* — the current segment, styled apart from its ancestors.
    current: bool,
}

/// The chip's box: its text (capped by [`Tokens::crumb_max_width`]) plus the reference's own
/// `padding: 2px 6px`.
fn chip_width(ui: &Ui, text: &str, tokens: &Tokens) -> f32 {
    measure(ui, text, tokens).min(tokens.crumb_max_width) + tokens.crumb_chip_padding * 2.0
}

/// The unbounded width of one label, measured the way the rows measure: a galley, never an estimate.
fn measure(ui: &Ui, text: &str, tokens: &Tokens) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let job = egui::text::LayoutJob::simple(
        text.to_owned(),
        tokens.font(),
        tokens.text_main,
        f32::INFINITY,
    );
    ui.painter().layout_job(job).size().x
}

/// Lay the trail out for `budget` points: every chip when it fits, and when it does not, the root,
/// a collapse marker and as many of the *deepest* segments as fit — the tail is what the user is
/// navigating, so it survives; the middle is what a path has most of, so it goes.
fn plan(ui: &Ui, crumbs: &[Crumb], budget: f32, tokens: &Tokens) -> Vec<Slot> {
    let sep = measure(ui, SEPARATOR_LABEL, tokens) + tokens.crumb_separator_gap * 2.0;
    let chips: Vec<f32> = crumbs
        .iter()
        .map(|crumb| chip_width(ui, &crumb.label, tokens))
        .collect();
    // The width of a run of chips, separators between them included.
    let total = |widths: &[f32]| -> f32 {
        widths.iter().sum::<f32>() + (widths.len().saturating_sub(1)) as f32 * sep
    };
    let push =
        |slots: &mut Vec<Slot>, label: &str, target: Option<FsPath>, width: f32, current: bool| {
            if !slots.is_empty() {
                slots.push(Slot {
                    label: SEPARATOR_LABEL.to_owned(),
                    target: None,
                    width: sep,
                    current: false,
                });
            }
            slots.push(Slot {
                label: label.to_owned(),
                target,
                width,
                current,
            });
        };

    if total(&chips) <= budget {
        let mut slots = Vec::new();
        for (ix, crumb) in crumbs.iter().enumerate() {
            push(
                &mut slots,
                &crumb.label,
                Some(crumb.target.clone()),
                chips[ix],
                ix + 1 == crumbs.len(),
            );
        }
        return slots;
    }

    // Collapsed: the root, the marker, and the deepest `keep` crumbs. The marker navigates to the
    // *deepest hidden* segment, i.e. it walks out one step of the trail it hid.
    let collapse = chip_width(ui, COLLAPSE_LABEL, tokens);
    let mut keep = crumbs.len() - 1;
    while keep > 1 {
        let widths: Vec<f32> = chips[crumbs.len() - keep..].to_vec();
        if total(&widths) + chips[0] + collapse + sep * 2.0 <= budget {
            break;
        }
        keep -= 1;
    }
    let split = crumbs.len() - keep;
    // `split > 1` is what makes the marker honest: there has to be at least one hidden segment
    // between the root and the tail, or it would be a `…` that hides nothing.
    let hiding = split > 1;
    let mut slots = Vec::new();
    push(
        &mut slots,
        &crumbs[0].label,
        Some(crumbs[0].target.clone()),
        chips[0],
        false,
    );
    if hiding {
        push(
            &mut slots,
            COLLAPSE_LABEL,
            Some(crumbs[split - 1].target.clone()),
            collapse,
            false,
        );
    }
    for (ix, crumb) in crumbs[split..].iter().enumerate() {
        push(
            &mut slots,
            &crumb.label,
            Some(crumb.target.clone()),
            chips[split + ix],
            split + ix + 1 == crumbs.len(),
        );
    }

    // Even `Root / … / current` can be too wide for a pane squeezed to its minimum. Nothing may
    // reach past the budget, because the address field's button is laid out after it: drop the root
    // chip first, then squeeze the current one, which is the last thing that can give.
    let mut used = slots.iter().map(|slot| slot.width).sum::<f32>();
    if used > budget && slots.len() > 2 {
        // `slots[0]` is the root chip and `slots[1]` the separator that followed it.
        slots.drain(0..2);
        used = slots.iter().map(|slot| slot.width).sum::<f32>();
    }
    if used > budget {
        let rest: f32 = slots[..slots.len() - 1].iter().map(|slot| slot.width).sum();
        let last = slots.len() - 1;
        slots[last].width = (budget - rest).max(0.0);
    }
    slots
}

/// Draw the chips inside `budget`, then return the free space after them (whose click opens the
/// address field, exactly where Go's bar did it).
fn path_chips(
    ui: &mut Ui,
    path: &FsPath,
    budget: f32,
    tokens: &Tokens,
    out: &mut Vec<PaneCommand>,
) -> egui::Response {
    let slots = plan(ui, &trail(path), budget, tokens);
    let chip_height = (tokens.toolbar_height - tokens.crumb_chip_inset * 2.0).max(0.0);
    for slot in slots {
        let width = slot.width;
        if width <= 0.0 {
            continue;
        }
        let (rect, response) = ui.allocate_exact_size(vec2(width, chip_height), slot_sense(&slot));
        if !ui.is_rect_visible(rect) {
            continue;
        }
        let Some(target) = slot.target else {
            // The separator: paint-only, in the reference's border colour (`.breadcrumb-separator`).
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                SEPARATOR_LABEL,
                tokens.font(),
                tokens.border,
            );
            continue;
        };
        let hovered = response.hovered();
        let fill = if slot.current || hovered {
            tokens.bg_hover
        } else {
            tokens.transparent
        };
        let radius = egui::CornerRadius::same(tokens.crumb_corner_radius);
        ui.painter().rect_filled(rect, radius, fill);
        if slot.current {
            // The distinguishable current-segment style: the same hairline every panel border uses.
            ui.painter()
                .rect_stroke(rect, radius, tokens.hairline(), egui::StrokeKind::Inside);
        }
        let colour = if slot.current {
            tokens.text_active
        } else if hovered {
            tokens.text_main
        } else {
            tokens.text_muted
        };
        paint_chip_label(ui, rect, &slot.label, colour, tokens);
        let label = slot.label.clone();
        // One interaction rect per chip, disjoint by construction (sequential layout): the
        // last-allocated-wins rule of plan §9.2 step 8 cannot bite between two chips.
        response.widget_info(|| {
            let mut info = egui::WidgetInfo::new(egui::Role::Button);
            info.label = Some(label.clone());
            info
        });
        if response.clicked() && target != *path {
            // The pane's own navigate command — the same one the address field and the history
            // buttons use; there is no second mechanism and there must not be one.
            out.push(PaneCommand::Navigate(target));
        }
    }
    // The free space after the trail, up to the room reserved for the trailing button.
    let rest = ui.available_rect_before_wrap();
    let side = tokens.toolbar_height - tokens.header_button_inset;
    let edge = egui::Rect::from_min_max(
        rest.min,
        egui::pos2(
            (rest.max.x - side - tokens.crumb_separator_gap).max(rest.min.x),
            rest.max.y,
        ),
    );
    let space = {
        // Scoped to this pane's `Ui` (`scope_id`), not `Id::unique` (which is the *same* id everywhere — two
        // panes' trails then collide on one id at two rects and egui paints its id-clash flame,
        // which this font stack panics on, rule 11).
        let space_id = ui.scope_id().with("zenman-crumb-space");
        ui.interact(edge, space_id, egui::Sense::click())
            .on_hover_text("Edit the path")
    };
    ui.advance_cursor_after_rect(space.rect);
    space
}

/// Chips take clicks; the separators between them take nothing (they are paint, and an interaction
/// rect over a neighbour's is the bug the row rule exists to prevent).
fn slot_sense(slot: &Slot) -> egui::Sense {
    if slot.target.is_some() {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    }
}

/// Paint one chip's label, ellipsised to the space the chip has. Paint-only: the chip's own
/// allocation is its single interaction rect.
fn paint_chip_label(
    ui: &mut Ui,
    rect: egui::Rect,
    text: &str,
    colour: egui::Color32,
    tokens: &Tokens,
) {
    let width = (rect.width() - tokens.crumb_chip_padding * 2.0).max(0.0);
    if width <= 0.0 || text.is_empty() {
        return;
    }
    let mut job = egui::text::LayoutJob::simple(text.to_owned(), tokens.font(), colour, width);
    job.wrap = egui::text::TextWrapping {
        max_width: width,
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some(COLLAPSE_LABEL.chars().next().unwrap_or('.')),
    };
    let galley = ui.painter().layout_job(job);
    let x = rect.min.x + tokens.crumb_chip_padding;
    let y = rect.center().y - galley.size().y / 2.0;
    ui.painter().galley(egui::pos2(x, y), galley, colour);
}

/// The trailing address-field button: the one affordance the trail can never squeeze out of the
/// strip, because its width is subtracted from the trail's budget before the chips are counted.
fn edit_button(ui: &mut Ui, tokens: &Tokens) -> egui::Response {
    let side = tokens.toolbar_height - tokens.header_button_inset;
    let (rect, response) = ui.allocate_exact_size(vec2(side, side), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        if response.hovered() {
            ui.painter().rect_filled(
                rect,
                egui::CornerRadius::same(tokens.crumb_corner_radius),
                tokens.bg_hover,
            );
        }
        let tint = if response.hovered() {
            tokens.text_active
        } else {
            tokens.text_muted
        };
        crate::icons::paint(ui, rect, "edit", tokens.row_icon_size, tint);
    }
    let response = response.on_hover_text("Edit the path");
    response.widget_info(|| {
        let mut info = egui::WidgetInfo::new(egui::Role::Button);
        info.label = Some("Edit the path".into());
        info
    });
    response
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
