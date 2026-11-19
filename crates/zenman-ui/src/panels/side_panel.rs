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
//! Every row is one flat `Button` (paint-only icon, `Sense::hover` allocations beside it — rule 10),
//! sized to the panel, with its label **ellipsised to the width the panel actually has** and its
//! free-space text painted right-aligned inside the same rect. A long volume name therefore can never
//! reach the window edge, and the trailing text is deliberately not part of the accessible name, so a
//! test can find the row at any width.
//!
//! # Navigation
//!
//! A row answers `PaneCommand::Navigate` for the **active** pane (the same seam the breadcrumb uses);
//! a place that is not reachable still renders, still navigates (the pane then shows core's own typed
//! directory error), and says why in its own line — a typed state, not a dead row.

use egui::{Align2, FontId, Layout, RichText, ScrollArea, Sense, Stroke, Ui, vec2};

use zenman_core::metrics::ByteSize;
use zenman_core::path::FsPath;
use zenman_core::services::places::{Disk, Place, PlaceKind, Places};
use zenman_core::store::config::{CustomAction, PaneSlot};

use crate::app::ZenApp;
use crate::icons;
use crate::panels::file_pane::PaneCommand;
use crate::theme::Tokens;
use crate::widgets::toast;

/// What a click in either drawer means. The shell answers it; the panel never touches the engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidePanelCommand {
    /// List this place in the active pane.
    Navigate(FsPath),
    /// Favourite the active pane's directory (Go's `addCurrentToBookmarks`, `sidebar.js:135-150`).
    AddCurrent,
    /// Drop one favourite (Go's `{"action":"remove"}`, `api/places.go:176-186`).
    RemoveFavourite(String),
    /// The `+` in the right drawer's header (Go's `#btn-add-action`).
    NewAction,
    /// Edit one custom action (Go's per-row edit).
    EditAction(CustomAction),
}

/// One drawer row's content: Go's `.sidebar-item` — an icon, a label, muted trailing text.
struct RowSpec {
    icon: &'static str,
    label: String,
    trailing: Option<String>,
    note: Option<String>,
    muted: bool,
    activate: Option<SidePanelCommand>,
    remove: Option<SidePanelCommand>,
}

impl RowSpec {
    fn new(icon: &'static str, label: impl Into<String>) -> Self {
        Self {
            icon,
            label: label.into(),
            trailing: None,
            note: None,
            muted: false,
            activate: None,
            remove: None,
        }
    }
}

/// Draw the left drawer: places, favourites, mounted drives.
pub fn show_left(app: &mut ZenApp, ui: &mut Ui) {
    let tokens = app.tokens;
    let places = app.session.places();
    let mut out: Vec<SidePanelCommand> = Vec::new();
    drawer(ui, &tokens, |ui| {
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
    });
    run(app, out);
}

/// Draw the right drawer: Go's Custom Actions manager.
pub fn show_right(app: &mut ZenApp, ui: &mut Ui) {
    let tokens = app.tokens;
    let actions = app.session.config().custom_actions;
    let mut out: Vec<SidePanelCommand> = Vec::new();
    drawer(ui, &tokens, |ui| {
        section(
            ui,
            &tokens,
            "Custom Actions",
            Some(SidePanelCommand::NewAction),
            &mut out,
        );
        if actions.is_empty() {
            empty_note(ui, &tokens, "No custom actions");
            ui.label(
                RichText::new("add one to run a program on the selected files")
                    .color(tokens.text_muted)
                    .size(tokens.font_size - 1.0),
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
    });
    run(app, out);
}

fn drawer(ui: &mut Ui, tokens: &Tokens, body: impl FnOnce(&mut Ui)) {
    let heading = tokens.font_size;
    ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.label(
                RichText::new("zenman")
                    .color(tokens.text_active)
                    .size(heading)
                    .strong(),
            );
            body(ui);
        });
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
    ui.add_space(tokens.drawer_section_gap);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(title.to_uppercase())
                .color(tokens.text_muted)
                .size(tokens.font_size - 1.0)
                .strong(),
        );
        if let Some(command) = add {
            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("+").clicked() {
                    out.push(command);
                }
            });
        }
    });
}

fn empty_note(ui: &mut Ui, tokens: &Tokens, text: &str) {
    ui.label(
        RichText::new(text)
            .color(tokens.text_muted)
            .size(tokens.font_size - 1.0),
    );
}

/// The row itself: one flat button (the only click target), a paint-only icon, and paint-only
/// trailing text inside the same rect.
fn show_row(ui: &mut Ui, tokens: &Tokens, spec: &RowSpec, out: &mut Vec<SidePanelCommand>) {
    let row_width = ui.available_width();
    let remove_width = if spec.remove.is_some() {
        tokens.row_height
    } else {
        0.0
    };
    let icon_width = tokens.drawer_icon_size + tokens.row_icon_gap;
    let text_width = (row_width - remove_width - icon_width - tokens.row_pad_x * 2.0).max(1.0);
    let label = ellipsize_to_width(&spec.label, text_width, tokens);
    let colour = if spec.muted {
        tokens.disabled_text
    } else {
        tokens.text_main
    };
    let mut hovered = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let (icon_rect, _) =
            ui.allocate_exact_size(vec2(icon_width, tokens.row_height), Sense::hover());
        icons::paint(
            ui,
            icon_rect,
            spec.icon,
            tokens.drawer_icon_size,
            if spec.muted {
                tokens.disabled_text
            } else {
                tokens.text_muted
            },
        );
        let response = ui.add_sized(
            vec2(row_width - remove_width - icon_width, tokens.row_height),
            egui::Button::new(RichText::new(label).color(colour).size(tokens.font_size))
                .fill(tokens.transparent)
                .stroke(Stroke::NONE),
        );
        hovered = response.hovered();
        if let Some(trailing) = &spec.trailing {
            let right = response.rect.max - vec2(tokens.row_pad_x, 0.0);
            ui.painter().text(
                right,
                Align2::RIGHT_CENTER,
                trailing.clone(),
                FontId::proportional(tokens.font_size - 1.0),
                tokens.text_muted,
            );
        }
        if response.clicked()
            && let Some(command) = &spec.activate
        {
            out.push(command.clone());
        }
        if let Some(command) = &spec.remove {
            // Allocated always, revealed on hover (Go's `opacity: 0` → `1`, `sidebar.js:93-100`), so
            // the row never changes width between frames.
            let remove = ui.add_sized(
                vec2(remove_width, tokens.row_height),
                egui::Button::new(
                    RichText::new("×")
                        .color(if hovered && !spec.muted {
                            tokens.text_main
                        } else {
                            tokens.transparent
                        })
                        .size(tokens.font_size),
                )
                .fill(tokens.transparent)
                .stroke(Stroke::NONE),
            );
            if remove.clicked() {
                out.push(command.clone());
            }
        }
    });
    if let Some(note) = &spec.note {
        ui.label(
            RichText::new(note)
                .color(tokens.text_muted)
                .size(tokens.font_size - 1.0),
        );
    }
}

/// Trim `text` to the glyphs `width` points of this theme's font can hold, with the ellipsis the
/// file rows use. The public form is what the tests check a drawer row against.
#[must_use]
pub fn ellipsize_to_width(text: &str, width: f32, tokens: &Tokens) -> String {
    let budget = (width / tokens.font_size).floor().max(0.0) as usize;
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= budget {
        return text.to_string();
    }
    let cut: String = chars.iter().take(budget.saturating_sub(1)).collect();
    format!("{cut}…")
}

fn place_spec(place: &Place) -> RowSpec {
    let mut spec = RowSpec::new(place_icon(place.kind), &place.label);
    spec.note = place_note(place);
    spec.muted = !place.exists;
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
    spec.trailing = Some(match &disk.unavailable {
        Some(why) => why.clone(),
        None => format!(
            "{} free of {}",
            ByteSize(disk.free).humanize(),
            ByteSize(disk.total).humanize()
        ),
    });
    spec.muted = disk.unavailable.is_some();
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
        zenman_core::path::AbsPath::from_path(path)
            .map(|abs| Self::Navigate(FsPath::from_abs(abs)))
            .ok()
    }
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
