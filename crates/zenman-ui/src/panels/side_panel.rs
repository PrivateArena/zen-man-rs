//! The side panels (plan §5.8 `panels/side_panel.rs`, §7 row 15; defects D8/D9 of §9.2 step-15a).
//!
//! **Left** — Go's `aside.sidebar` (`index.html:19-34`, `frontend/js/sidebar.js`): three sections, in
//! Go's order — *Places* (Home, Root, Downloads, Documents, Desktop), *Bookmarks* (this port's
//! **favourites**, with the `+` of `index.html:29` and the hover-revealed `×` of `sidebar.js:78-92`),
//! and *Mounted Drives*. The rows come from `Session::places` (`services/places.rs`), so the list is
//! the platform's answer and the persisted config, never a UI-side guess.
//!
//! **Right** — Go's `aside.right-sidebar` (`index.html:425-487`): the **Custom Actions** manager.
//! That is what Go shows there, which is why the disks left this panel: they are Go's *left* drawer
//! content (`index.html:33`), and the header keeps the meter every reference screenshot shows.
//!
//! # Geometry (D9)
//!
//! Every row is one allocated rect (the only click target) with the icon, the label and the
//! right-hand column painted inside it, at Go's own columns (`sidebar.css:43-77`): nav padding,
//! the row's `10px 12px` padding, an 18 px icon box with a 10 px margin, then the text. The label is
//! **pixel-measured and ellipsised to the width that is left** after the right-hand column, and the
//! whole row lives inside its rect, so a long volume name can never reach the window edge. A mounted
//! drive's `X free of Y` is therefore painted as a bar with the numbers in the row's tooltip — the
//! 250 px panel has no single line that holds both. The `×` is a second, later interaction rect
//! overlapping the row's right end (last-allocated-wins, plan §9.2 step 8), so it answers instead of
//! navigating.
//!
//! # Navigation
//!
//! A row answers `PaneCommand::Navigate` for the **active** pane (the same seam the breadcrumb uses);
//! a place that is not reachable still renders, still navigates (the pane then shows core's own typed
//! directory error), and says why in its own line — a typed state, not a dead row.

use egui::{
    Align2, CornerRadius, FontId, Layout, Rect, RichText, ScrollArea, Sense, Ui, Vec2, vec2,
};

use zenman_core::metrics::ByteSize;
use zenman_core::path::FsPath;
use zenman_core::services::places::{Disk, Place, PlaceKind, Places};
use zenman_core::store::config::{CustomAction, PaneSlot};

use crate::app::ZenApp;
use crate::icons;
use crate::panels::file_pane::PaneCommand;
use crate::theme::Tokens;
use crate::widgets::{disk_meter, toast};

/// What a click in either drawer means. The shell answers it; the panel never touches the engine.
#[derive(Clone, Debug, PartialEq)]
pub enum SidePanelCommand {
    /// List this place in the active pane.
    Navigate(FsPath),
    /// Right-click on a place / favourite / drive row: the folder menu for **that** row's path.
    ///
    /// Go attaches one handler to every `.sidebar-item` — places (`sidebar.js:32-37`), bookmarks
    /// (`:90-95`), mounts (`:129-134`) — and all three call the same `showFolderContextMenu(e, path,
    /// pane, 'sidebar')`, so the drawer's menu is the breadcrumb chip's menu and not the listing's.
    FolderContext {
        /// The row's path.
        path: FsPath,
        /// Where the pointer was.
        at: egui::Pos2,
    },
    /// Favourite the active pane's directory (Go's `addCurrentToBookmarks`, `sidebar.js:135-150`).
    AddCurrent,
    /// Drop one favourite (Go's `{"action":"remove"}`, `api/places.go:176-186`).
    RemoveFavourite(String),
    /// The `+` in the right drawer's header (Go's `#btn-add-action`).
    NewAction,
    /// Edit one custom action (Go's per-row edit).
    EditAction(CustomAction),
}

/// One drawer row's content: Go's `.sidebar-item` — an icon, a label, and a right-hand column.
///
/// The column is either short text (a custom action's role) or a capacity meter (a mounted drive).
/// The meter is what the 250 px panel can actually afford: Go's own row prints only the volume name
/// (`sidebar.js:114-117`), and the port's numbers — `369.8 GiB free of 915.8 GiB` — are twice the
/// width of the longest name, so they become a bar plus the row's tooltip instead of a second line.
struct RowSpec {
    icon: &'static str,
    label: String,
    trailing: Option<String>,
    /// The used fraction of a volume, painted as a bar on the row's right.
    meter: Option<f32>,
    note: Option<String>,
    /// The tooltip: the numbers the meter stands for, or why a place is not there.
    hover: Option<String>,
    muted: bool,
    activate: Option<SidePanelCommand>,
    remove: Option<SidePanelCommand>,
    /// The path a right-click on this row opens the folder menu for — `Some` for a row that **is** a
    /// folder (place, favourite, mount), `None` for the right drawer's custom-action rows.
    folder: Option<FsPath>,
}

impl RowSpec {
    fn new(icon: &'static str, label: impl Into<String>) -> Self {
        Self {
            icon,
            label: label.into(),
            trailing: None,
            meter: None,
            note: None,
            hover: None,
            muted: false,
            activate: None,
            remove: None,
            folder: None,
        }
    }

    /// Ask for the folder menu on right-click, on the same path the row navigates to. A path that will
    /// not parse gets no menu, which is also what `activate` means for the row (`navigate` refuses it).
    fn for_folder(mut self, path: &str) -> Self {
        self.folder = SidePanelCommand::folder(path);
        self
    }
}

/// Draw the left drawer: places, favourites, mounted drives.
pub fn show_left(app: &mut ZenApp, ui: &mut Ui) {
    let tokens = app.tokens;
    let places = app.session.places();
    let mut out: Vec<SidePanelCommand> = Vec::new();
    if let Some(command) = drawer(ui, &tokens, "Zen-Man", None, |ui| {
        section(ui, &tokens, "Places", None, &mut out);
        for place in &places.user {
            show_row(ui, &tokens, &place_spec(place), &mut out);
        }
        section(
            ui,
            &tokens,
            "Bookmarks",
            Some(SidePanelCommand::AddCurrent),
            &mut out,
        );
        if places.favourites.is_empty() {
            empty_note(ui, &tokens, "No bookmarks");
        }
        for place in &places.favourites {
            show_row(ui, &tokens, &favourite_spec(place), &mut out);
        }
        section(ui, &tokens, "Mounted Drives", None, &mut out);
        if places.disks.is_empty() {
            empty_note(ui, &tokens, "No mounted drives");
        }
        for disk in &places.disks {
            show_row(ui, &tokens, &disk_spec(disk), &mut out);
        }
    }) {
        out.push(command);
    }
    run(app, out);
}

/// Draw the right drawer: Go's Custom Actions manager.
pub fn show_right(app: &mut ZenApp, ui: &mut Ui) {
    let tokens = app.tokens;
    let actions = app.session.config().custom_actions;
    let mut out: Vec<SidePanelCommand> = Vec::new();
    // Go's right drawer has no section title at all: its `.sidebar-header` *is* the title, with the
    // `+` (`#btn-add-action`) beside it (`index.html:426-429`).
    if let Some(command) = drawer(
        ui,
        &tokens,
        "Custom Actions",
        Some((SidePanelCommand::NewAction, "Add New Action")),
        |ui| {
            if actions.is_empty() {
                empty_note(ui, &tokens, "No custom actions");
                empty_note(
                    ui,
                    &tokens,
                    "add one to run a program on the selected files",
                );
            }
            for action in &actions {
                let mut spec = RowSpec::new(action_icon(action), &action.name);
                spec.trailing = Some(
                    match action.role.as_str() {
                        "files" => "files",
                        "dirs" => "folders",
                        "background" => "background",
                        "" => "any",
                        other => other,
                    }
                    .to_string(),
                );
                spec.note = (!action.patterns.trim().is_empty()).then(|| action.patterns.clone());
                spec.activate = Some(SidePanelCommand::EditAction(action.clone()));
                show_row(ui, &tokens, &spec, &mut out);
            }
        },
    ) {
        out.push(command);
    }
    run(app, out);
}

/// A drawer: Go's `.sidebar-header` (`sidebar.css:29-39` — 20 px padding, an 1.2rem title, a bottom
/// border, and on the right the section's own action button) above the scrolling body.
fn drawer(
    ui: &mut Ui,
    tokens: &Tokens,
    title: &str,
    add: Option<(SidePanelCommand, &str)>,
    body: impl FnOnce(&mut Ui),
) -> Option<SidePanelCommand> {
    let mut clicked = None;
    ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let (rect, head_response) = ui.allocate_exact_size(
                vec2(ui.available_width(), tokens.drawer_header_height),
                Sense::hover(),
            );
            // The title is paint, but it is still the panel's name in the accessibility tree (Go's
            // `<h2>` is a heading, not a decoration).
            head_response.widget_info(|| {
                let mut info = egui::WidgetInfo::new(egui::Role::Heading);
                info.label = Some(title.to_owned());
                info
            });
            let head = Rect::from_min_max(
                egui::pos2(rect.min.x + tokens.drawer_head_pad_x, rect.min.y),
                egui::pos2(rect.max.x - tokens.drawer_head_pad_x, rect.max.y),
            );
            ui.painter().text(
                egui::pos2(head.min.x, rect.center().y),
                Align2::LEFT_CENTER,
                title,
                FontId::proportional(tokens.drawer_title_size),
                tokens.text_active,
            );
            let side = tokens.drawer_title_size;
            if let Some((command, hint)) = add {
                // Go's `.btn-sidebar-action` is a 1.1rem glyph with no box of its own; this is the
                // port's, one comfortable touch target (`row_height`) wide.
                let hit = tokens.row_height;
                let box_rect = Rect::from_center_size(
                    egui::pos2(head.max.x - hit / 2.0, rect.center().y),
                    Vec2::splat(hit),
                );
                let add_response = ui
                    .interact(
                        box_rect,
                        ui.scope_id().with("zenman-drawer-add"),
                        Sense::click(),
                    )
                    .on_hover_text(hint);
                add_response.widget_info(|| {
                    let mut info = egui::WidgetInfo::new(egui::Role::Button);
                    info.label = Some(hint.to_owned());
                    info
                });
                if add_response.clicked() {
                    clicked = Some(command);
                }
                let tint = if add_response.hovered() {
                    tokens.text_active
                } else {
                    tokens.text_muted
                };
                ui.painter().text(
                    egui::pos2(head.max.x - side / 2.0, rect.center().y),
                    Align2::CENTER_CENTER,
                    "+",
                    FontId::proportional(tokens.drawer_title_size),
                    tint,
                );
            }
            ui.painter()
                .hline(rect.min.x..=rect.max.x, rect.max.y, tokens.hairline());
            ui.add_space(tokens.drawer_section_gap);
            // `.sidebar-nav { gap: 5px }` — the rows of a section sit 5 points apart.
            ui.spacing_mut().item_spacing.y = tokens.drawer_row_gap;
            body(ui);
        });
    clicked
}

fn run(app: &mut ZenApp, commands: Vec<SidePanelCommand>) {
    for command in commands {
        app.run_side_panel_command(command);
    }
}

/// A section title — Go's `.sidebar-section-title` (uppercase, muted, `+` on the right).
fn section(
    ui: &mut Ui,
    tokens: &Tokens,
    title: &str,
    add: Option<SidePanelCommand>,
    out: &mut Vec<SidePanelCommand>,
) {
    ui.add_space(tokens.drawer_section_top);
    ui.horizontal(|ui| {
        ui.add_space(tokens.drawer_section_pad_x);
        ui.label(
            RichText::new(title.to_uppercase())
                .color(tokens.text_muted)
                .size(tokens.font_size - 1.0)
                .strong(),
        );
        if let Some(command) = add {
            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                // Go's `#btn-add-bookmark` (`index.html:29`), whose `title` is this sentence.
                if ui
                    .button(RichText::new("+").color(tokens.text_muted))
                    .on_hover_text("Add Current to Bookmarks")
                    .clicked()
                {
                    out.push(command);
                }
            });
        }
    });
}

fn empty_note(ui: &mut Ui, tokens: &Tokens, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(tokens.drawer_pad_x + tokens.drawer_row_pad_x - tokens.row_icon_gap);
        ui.label(
            RichText::new(text)
                .color(tokens.text_muted)
                .size(tokens.font_size - 1.0),
        );
    });
}

/// The row itself: one rect, one click target, and everything else painted inside it.
///
/// Go's `.sidebar-item` is a single flex line — 10/12 px padding, an 18 px icon box with a 10 px
/// margin, the label, then the right-hand column (`sidebar.css:55-77`). That is why this is one
/// allocation plus paint rather than an `egui::Button`: a button *centres* what it is given, and a
/// centred label beside a left-aligned icon is exactly the dead space this row must not have.
fn show_row(ui: &mut Ui, tokens: &Tokens, spec: &RowSpec, out: &mut Vec<SidePanelCommand>) {
    let (rect, response) = ui.allocate_exact_size(
        vec2(ui.available_width(), tokens.row_height),
        Sense::click(),
    );
    let hovered = response.hovered();
    let columns = RowColumns::plan(ui, rect, spec, tokens);
    if ui.is_rect_visible(rect) {
        paint_row(ui, &columns, spec, hovered, tokens);
    }

    // The `×`, allocated *after* the row: in this egui fork the last-allocated overlapping rect is
    // the one that answers (`remove` therefore steals the click from `activate`, which is the only
    // order that can work — plan §9.2 step 8).
    let mut remove_hovered = false;
    if let Some(command) = &spec.remove {
        let hit = columns.remove;
        let remove = ui
            .interact(hit, response.id.with("zenman-row-remove"), Sense::click())
            .on_hover_text("Remove Bookmark");
        remove_hovered = remove.hovered();
        remove.widget_info(|| {
            let mut info = egui::WidgetInfo::new(egui::Role::Button);
            info.label = Some(format!("remove {}", spec.label));
            info
        });
        if remove.clicked() {
            out.push(command.clone());
        }
        // Go's delete button has no `contextmenu` handler of its own, so the event bubbles to the row
        // and the folder menu opens from over the `×` too.
        if remove.secondary_clicked()
            && let Some(command) =
                folder_menu(spec, remove.interact_pointer_pos().unwrap_or(hit.min))
        {
            out.push(command);
        }
    }

    let label = spec.label.clone();
    let response = match &spec.hover {
        Some(hint) => response.on_hover_text(hint.clone()),
        None => response,
    };
    response.widget_info(|| {
        let mut info = egui::WidgetInfo::new(egui::Role::Button);
        info.label = Some(label.clone());
        info
    });
    if response.clicked()
        && let Some(command) = &spec.activate
    {
        out.push(command.clone());
    }
    // Right-click is the folder menu (`sidebar.js:32-37`). It is the row's own gesture, not a variant of
    // `activate`: the selection and the listing are left exactly as they were, and the menu is about the
    // row's path — which is not necessarily the pane's directory.
    if response.secondary_clicked()
        && let Some(command) =
            folder_menu(spec, response.interact_pointer_pos().unwrap_or(rect.min))
    {
        out.push(command);
    }

    // Go reveals the `×` on hover with `opacity: 0 → 1` (`sidebar.js:93-100`); the slot is always
    // there, so the row never changes width between frames.
    if spec.remove.is_some() && (hovered || remove_hovered) && ui.is_rect_visible(columns.remove) {
        ui.painter().text(
            columns.remove.center(),
            Align2::CENTER_CENTER,
            "×",
            FontId::proportional(tokens.font_size),
            if spec.muted {
                tokens.disabled_text
            } else {
                tokens.text_main
            },
        );
    }

    if let Some(note) = &spec.note {
        // D8's one-line "why" — under the row, at its text column, in the muted size.
        let (note_rect, _) = ui.allocate_exact_size(
            vec2(rect.width(), tokens.font_size + tokens.drawer_row_pad_x),
            Sense::hover(),
        );
        if ui.is_rect_visible(note_rect) {
            ui.painter().text(
                egui::pos2(columns.text_x, note_rect.center().y),
                Align2::LEFT_CENTER,
                note.clone(),
                FontId::proportional(tokens.font_size - 1.0),
                tokens.text_muted,
            );
        }
    }
}

/// Where a row's pieces go. Planned before anything is painted so the label is ellipsised to the
/// width that is genuinely left, and nothing can reach the panel's edge (defect D9).
struct RowColumns {
    /// The row's rect, shrunk by Go's `padding: 10px 12px` (and by the nav's own 10 px inset).
    content: Rect,
    /// The 18 px icon box, centred on the row's line like Go's `text-align: center`.
    icon: Rect,
    /// The label's left edge.
    text_x: f32,
    /// The right-hand column's right edge — the label may not pass it.
    trailing_right: f32,
    /// The `×` slot, `Some` only for a removable row.
    remove: Rect,
    /// The meter slot, `Some` only for a volume with a capacity.
    meter: Option<Rect>,
}

impl RowColumns {
    fn plan(ui: &Ui, rect: Rect, spec: &RowSpec, tokens: &Tokens) -> Self {
        let edge = tokens.drawer_pad_x + tokens.drawer_row_pad_x;
        let content = Rect::from_min_max(
            egui::pos2(rect.min.x + edge, rect.min.y),
            egui::pos2(rect.max.x - edge, rect.max.y),
        );
        let icon = Rect::from_min_size(
            egui::pos2(
                content.min.x,
                rect.center().y - tokens.drawer_icon_box / 2.0,
            ),
            Vec2::splat(tokens.drawer_icon_box),
        );
        let text_x = content.min.x + tokens.drawer_icon_box + tokens.row_icon_gap;
        let side = (tokens.row_height - tokens.drawer_row_pad_x).max(tokens.drawer_icon_box);
        let remove = Rect::from_min_max(
            egui::pos2(content.max.x - side, rect.min.y),
            egui::pos2(content.max.x, rect.max.y),
        );
        let mut right = if spec.remove.is_some() {
            content.max.x - side - tokens.drawer_trailing_gap
        } else {
            content.max.x
        };
        let trailing_width = spec.trailing.as_deref().map_or(0.0, |text| {
            measure_text(ui, text, tokens.font_size - 1.0, tokens)
        });
        if trailing_width > 0.0 {
            right -= trailing_width + tokens.drawer_trailing_gap;
        }
        let meter = spec.meter.map(|_| {
            let bar = Rect::from_min_size(
                egui::pos2(
                    right - tokens.drawer_meter_width,
                    rect.center().y - tokens.drawer_meter_height / 2.0,
                ),
                vec2(tokens.drawer_meter_width, tokens.drawer_meter_height),
            );
            right -= tokens.drawer_meter_width + tokens.drawer_trailing_gap;
            bar
        });
        Self {
            content,
            icon,
            text_x,
            trailing_right: right,
            remove,
            meter,
        }
    }

    /// How much room the label has, never less than one glyph's worth.
    fn label_width(&self, tokens: &Tokens) -> f32 {
        (self.trailing_right - self.text_x - tokens.drawer_trailing_gap).max(1.0)
    }
}

/// The unbounded width of a string in this theme's font — measured as a galley, never estimated.
/// The colour is a token's because the measure does not depend on it and the colour guard does.
fn measure_text(ui: &Ui, text: &str, size: f32, tokens: &Tokens) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    ui.painter()
        .layout_no_wrap(
            text.to_owned(),
            FontId::proportional(size),
            tokens.text_muted,
        )
        .size()
        .x
}

/// Draw one row inside its planned columns. Paint-only: [`show_row`] owns the two interaction rects.
fn paint_row(ui: &mut Ui, c: &RowColumns, spec: &RowSpec, hovered: bool, tokens: &Tokens) {
    if hovered {
        ui.painter().rect_filled(
            c.content,
            CornerRadius::same(tokens.drawer_row_corner),
            tokens.bg_hover,
        );
    }
    icons::paint(
        ui,
        c.icon,
        spec.icon,
        tokens.drawer_icon_size,
        if spec.muted {
            tokens.disabled_text
        } else {
            tokens.text_muted
        },
    );
    // Go's `.sidebar-item:hover` swaps the *text* to `--text-active` too, which is what makes a
    // hovered row read as a target rather than as a highlight behind dead text.
    let colour = if spec.muted {
        tokens.disabled_text
    } else if hovered {
        tokens.text_active
    } else {
        tokens.text_main
    };
    let label = ellipsize_to_width(ui, &spec.label, c.label_width(tokens), tokens);
    ui.painter().text(
        egui::pos2(c.text_x, c.content.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(tokens.font_size),
        colour,
    );
    if let Some(trailing) = &spec.trailing {
        ui.painter().text(
            egui::pos2(
                c.trailing_right + tokens.drawer_trailing_gap,
                c.content.center().y,
            ),
            Align2::LEFT_CENTER,
            trailing.clone(),
            FontId::proportional(tokens.font_size - 1.0),
            tokens.text_muted,
        );
    }
    if let (Some(bar), Some(frac)) = (c.meter, spec.meter) {
        crate::widgets::disk_meter::paint_bar(ui.painter(), bar, tokens, frac);
    }
}

/// Trim `text` to what `width` points of this theme's font can actually hold, with the ellipsis the
/// file rows use (`entry_row`'s `max_rows: 1` + `overflow_character`). Pixel-measured, not estimated:
/// an estimate of one point per character cut `Midgard Survivors` to eight characters in a column
/// that easily holds sixteen, which is what made the drawer rows look empty on their left half.
#[must_use]
pub fn ellipsize_to_width(ui: &Ui, text: &str, width: f32, tokens: &Tokens) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    let cut = |keep: usize| -> String {
        let mut s: String = chars.iter().take(keep).collect();
        s.push('\u{2026}');
        s
    };
    if measure_text(ui, text, tokens.font_size, tokens) <= width {
        return text.to_owned();
    }
    // First guess: the share of the string the share of its width that is left. Then step to the
    // widest cut that fits — two or three measures for a volume name, not one per character.
    let full = measure_text(ui, text, tokens.font_size, tokens).max(1.0);
    let mut keep = ((chars.len() as f32) * (width / full)).floor() as usize;
    while keep > 0 && measure_text(ui, &cut(keep), tokens.font_size, tokens) > width {
        keep -= 1;
    }
    while keep < chars.len() && measure_text(ui, &cut(keep + 1), tokens.font_size, tokens) <= width
    {
        keep += 1;
    }
    cut(keep)
}

fn place_spec(place: &Place) -> RowSpec {
    let mut spec = RowSpec::new(place_icon(place.kind), &place.label);
    spec.note = place_note(place);
    spec.muted = !place.exists;
    spec.hover = Some(place.path.clone());
    spec = spec.for_folder(&place.path);
    spec.activate = SidePanelCommand::navigate(&place.path);
    spec
}

fn favourite_spec(place: &Place) -> RowSpec {
    let mut spec = place_spec(place);
    spec.icon = "star";
    spec.remove = Some(SidePanelCommand::RemoveFavourite(place.path.clone()));
    spec
}

fn disk_spec(disk: &Disk) -> RowSpec {
    let mut spec = RowSpec::new(if disk.removable { "eject" } else { "drive" }, &disk.label);
    match &disk.unavailable {
        // D8: the reason is short enough to be the row's own text, and it is the one thing the
        // bar cannot say.
        Some(why) => spec.trailing = Some(why.clone()),
        None => {
            // The numbers go to the tooltip; the bar is what fits Go's single line.
            spec.hover = Some(format!(
                "{} free of {}",
                ByteSize(disk.free).humanize(),
                ByteSize(disk.total).humanize()
            ));
            spec.meter = Some(disk_meter::used_fraction(
                disk.total.saturating_sub(disk.free),
                disk.total,
            ));
        }
    }
    spec.muted = disk.unavailable.is_some();
    // An unreadable volume still answers a right-click: `Copy Path` is the one thing that is useful
    // when the drive cannot be entered, and Go's row carries its path regardless.
    spec = spec.for_folder(&disk.mount_point);
    spec.activate = SidePanelCommand::navigate(&disk.mount_point);
    spec
}

/// The typed state of an unreachable place, in one line (D8). `None` when the place is there.
#[must_use]
pub fn place_note(place: &Place) -> Option<String> {
    place
        .unavailable
        .as_ref()
        .map(|why| format!("unreachable — {why}"))
}

fn place_icon(kind: PlaceKind) -> &'static str {
    match kind {
        PlaceKind::Home => "home",
        PlaceKind::Root => "drive",
        PlaceKind::UserDir => "folder",
        PlaceKind::Favourite => "star",
        PlaceKind::Mount => "drive",
    }
}

fn action_icon(action: &CustomAction) -> &'static str {
    match action.role.as_str() {
        "background" => "terminal",
        "dirs" => "folder",
        _ => "file",
    }
}

impl SidePanelCommand {
    /// The command a row sends: `FsPath` is built once here, and a path the parser refuses simply
    /// does not produce a row command at all.
    #[must_use]
    pub fn navigate(path: &str) -> Option<Self> {
        Self::folder(path).map(Self::Navigate)
    }

    /// A path from a settings file or a mount table, typed or refused (plan §4.2's boundary rule: the
    /// string becomes an `FsPath` here, and a row whose path will not parse simply has no menu).
    fn folder(path: &str) -> Option<FsPath> {
        zenman_core::path::AbsPath::from_path(path)
            .map(FsPath::from_abs)
            .ok()
    }
}

/// The command a right-click on `spec`'s row raises, or `None` for a row that is not a folder.
fn folder_menu(spec: &RowSpec, at: egui::Pos2) -> Option<SidePanelCommand> {
    spec.folder
        .clone()
        .map(|path| SidePanelCommand::FolderContext { path, at })
}

impl ZenApp {
    /// Answer one drawer command. Kept on the shell so the panel stays a view.
    pub fn run_side_panel_command(&mut self, command: SidePanelCommand) {
        match command {
            SidePanelCommand::Navigate(path) => {
                let slot = self.model.active_pane;
                self.run_command(slot, PaneCommand::Navigate(path));
            }
            SidePanelCommand::AddCurrent => {
                let slot = self.model.active_pane;
                self.add_current_to_favourites(slot);
            }
            SidePanelCommand::RemoveFavourite(path) => self.remove_favourite_path(&path),
            SidePanelCommand::FolderContext { path, at } => {
                // Recorded exactly as a breadcrumb chip records it, so one folder menu
                // (`open_requested_menu` → `menus::menu_for`) answers both entries.
                let slot = self.model.active_pane;
                self.context_request = Some(crate::app::MenuRequest {
                    slot,
                    subject: crate::app::MenuSubject::Folder(path),
                    at,
                });
            }
            SidePanelCommand::NewAction => {
                let slot = self.model.active_pane;
                self.open_action_editor(slot, None);
            }
            SidePanelCommand::EditAction(action) => {
                let slot = self.model.active_pane;
                self.open_action_editor(slot, Some(action));
            }
        }
    }

    /// Go's `addCurrentToBookmarks` (`sidebar.js:135-150`), the target `Command::AddToFavorites`
    /// has been waiting for since step 7.
    pub fn add_current_to_favourites(&mut self, slot: PaneSlot) {
        match self.shown_dir_abs(slot) {
            Some(dir) => {
                let path = dir.to_string();
                self.add_favourite_path(&path);
            }
            None => self.notices.post(
                toast::Level::Warning,
                "this pane has no directory to favourite",
            ),
        }
    }

    /// `POST /api/places` `{"action":"add"}` (`api/places.go:147-166`): deduplicated, persisted.
    pub fn add_favourite_path(&mut self, path: &str) {
        match self.session.add_favourite(path) {
            Ok(list) => self.notices.post(
                toast::Level::Success,
                format!("favourite added — {} bookmark(s)", list.len()),
            ),
            Err(e) => self.notices.post(
                toast::Level::Error,
                format!("favourite: {}", e.user_message()),
            ),
        }
    }

    /// `POST /api/places` `{"action":"remove"}` (`api/places.go:167-186`).
    pub fn remove_favourite_path(&mut self, path: &str) {
        match self.session.remove_favourite(path) {
            Ok(list) => self.notices.post(
                toast::Level::Success,
                format!("favourite removed — {} bookmark(s)", list.len()),
            ),
            Err(e) => self.notices.post(
                toast::Level::Error,
                format!("favourite: {}", e.user_message()),
            ),
        }
    }
}

/// `Places` is named in the module docs as the panel's model; this is the seam that keeps it a
/// type the shell can hand over (plan §5.8's `SidePanelModel`).
#[must_use]
pub fn places_have_disks(places: &Places) -> bool {
    !places.disks.is_empty()
}
