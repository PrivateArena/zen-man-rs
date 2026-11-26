//! Properties (plan §5.8 `dialogs/properties.rs`, step 13b).
//!
//! Go shipped **no** properties dialog at all: `/api/props` was a route name that appeared in the
//! plan's dead-endpoint list, and the frontend never asked. So the content here is the plan row's
//! field list, filled from the two engines the port does have — the listing's own `FileEntry` for a
//! filesystem row, and [`Session::archive_props`](zenman_core::session::Session::archive_props) for a
//! member — and nothing is invented.
//!
//! Two rules decide what a row says:
//!
//! * **D11 (unknown is not zero).** A field the engine has not answered prints `--`. A directory's
//!   size is the live case: the walk is a job, so the dialog opens with `--` and rewrites the row when
//!   the cached [`DirSizeStat`](zenman_core::services::dirs::DirSizeStat) appears. It never guesses `0`.
//! * **No size arithmetic in the UI.** Every number here came from `Session`; the dialog only formats
//!   bytes with the one formatter the shell has ([`format_size`](crate::widgets::disk_meter::format_size)).
//!
//! # Archive entries
//!
//! §7 row 13 names this as the criterion: <kbd>Alt+Enter</kbd> opens Properties for local **and**
//! archive entries. The subject is therefore an [`FsPath`], not an `AbsPath`: inside a store the
//! "size" of the container is the host file's, the "uncompressed total" is the manifest's, and the
//! encrypted count is the warning the reader would otherwise only produce by failing.

use egui::{Align2, Area, Key, Order, RichText, Ui, Vec2};

use zenman_core::entry::FileEntry;
use zenman_core::jobs::JobId;
use zenman_core::path::{AbsPath, FsPath, VfsKind};
use zenman_core::services::dirs::DirSizeStat;
use zenman_core::session::{Origin, Receipt};
use zenman_core::store::config::PaneSlot;

use super::prompt::consume_key;
use super::{Answer, Choice, Dialog, DialogKind};
use crate::app::ZenApp;
use crate::theme::Tokens;
use crate::widgets::disk_meter::format_size;

/// What a `--` means in the as-yet unanswered fields (plan D11, spelled once).
const UNKNOWN: &str = "--";

/// One label/value line.
#[derive(Clone, Debug)]
pub struct PropRow {
    /// Left column.
    pub label: String,
    /// Right column; [`UNKNOWN`] until the engine answers.
    pub value: String,
}

/// What the dialog is about — and, with it, which engine answers.
#[derive(Clone, Debug)]
pub enum Subject {
    /// A filesystem path (the pane's directory, or one of its rows).
    Local {
        /// The absolute path the rows describe.
        abs: AbsPath,
        /// The listing's row, when the subject is one (it carries mode/mtime/size).
        entry: Option<FileEntry>,
    },
    /// A path inside an archive store (the store root, or one of its members).
    Archive {
        /// The store-relative path.
        path: FsPath,
        /// The member's row, when the subject is one.
        entry: Option<FileEntry>,
    },
}

impl Subject {
    /// Is this an archive subject?
    #[must_use]
    pub fn is_archive(&self) -> bool {
        matches!(self, Self::Archive { .. })
    }

    /// The subject's display text (an archive path prints its slash form, host first).
    #[must_use]
    pub fn display(&self) -> String {
        match self {
            Self::Local { abs, .. } => abs.to_string_lossy(),
            Self::Archive { path, .. } => path.to_string(),
        }
    }
}

/// The dialog's state (plan §5.8's `PropertiesDialog`).
#[derive(Clone, Debug)]
pub struct PropertiesDialog {
    /// The pane this dialog was opened from (the subject travels with the dialog, plan §4.1).
    pub slot: PaneSlot,
    /// What it is about.
    pub subject: Subject,
    /// The rows, in order.
    pub rows: Vec<PropRow>,
    /// Why the rows are missing (a listing/props failure), shown inline — never an empty dialog.
    pub status: Option<String>,
    /// The directory whose size is still walking, and the row its answer rewrites.
    pub warming: Option<(AbsPath, usize)>,
    /// The warming job, for the shell's job strip.
    #[allow(dead_code)]
    pub job: Option<JobId>,
    /// The origin the size job answers to (`None` until one is running).
    pub origin: Option<Origin>,
}

impl PropertiesDialog {
    /// A dialog over a filesystem path.
    #[must_use]
    pub fn local(slot: PaneSlot, abs: AbsPath, entry: Option<FileEntry>) -> Self {
        let is_dir = entry.as_ref().is_some_and(|e| e.is_dir);
        let mut rows = base_rows(&Subject::Local {
            abs: abs.clone(),
            entry: entry.clone(),
        });
        let size_row = rows.len();
        push_size_rows(&mut rows, entry.as_ref(), is_dir);
        let warming = is_dir.then(|| (abs.clone(), size_row));
        Self {
            slot,
            subject: Subject::Local { abs, entry },
            rows,
            status: None,
            // A directory's size is a walk: unknown now, and the row says so (D11).
            warming,
            job: None,
            origin: None,
        }
    }

    /// A dialog over an archive path, filled from `Session::archive_props` (the caller runs the read
    /// so the error text lands in [`Self::status`] rather than in an empty dialog).
    #[must_use]
    pub fn archive(
        slot: PaneSlot,
        path: FsPath,
        entry: Option<FileEntry>,
        props: Result<zenman_core::services::archive::ArchiveProps, zenman_core::error::CoreError>,
    ) -> Self {
        let subject = Subject::Archive { path, entry };
        let mut rows = base_rows(&subject);
        match props {
            Ok(props) => {
                rows.push(row("Container", subject.display()));
                rows.push(row("Format", format!("{:?}", props.format).to_lowercase()));
                rows.push(row("Store depth", props.depth.to_string()));
                rows.push(row(
                    "Compressed size (container)",
                    format_size(props.host_bytes),
                ));
                rows.push(row("Uncompressed total", format_size(props.total_bytes)));
                rows.push(row(
                    "Members",
                    format!(
                        "{} ({} files, {} directories)",
                        props.entries, props.files, props.dirs
                    ),
                ));
                rows.push(row("Encrypted members", props.encrypted.to_string()));
                rows.push(row("Nested archives", props.nested.to_string()));
            }
            Err(e) => {
                // The typed message, inline (encrypted / corrupt / modified all read differently).
                rows.push(row("Container", subject.display()));
                return Self {
                    slot,
                    subject,
                    rows,
                    status: Some(e.user_message()),
                    warming: None,
                    job: None,
                    origin: None,
                };
            }
        }
        Self {
            slot,
            subject,
            rows,
            status: None,
            warming: None,
            job: None,
            origin: None,
        }
    }

    /// Ask the engine for a directory's size, and take the answer from the cache afterwards
    /// (`Session::dir_size` warms the very cache `cached_dir_sizes` reads).
    pub fn warm_size(&mut self, app: &mut ZenApp) {
        let Some((path, _)) = self.warming.clone() else {
            return;
        };
        if self.origin.is_none() {
            let origin = app.session.next_origin();
            match app.session.dir_size(&path, origin) {
                Receipt::Done(stat) => self.apply_size(&stat),
                Receipt::Running { job, origin: o } => {
                    if let Some(info) = app.session.job_info(job) {
                        app.jobs.track(info, Some(self.slot), None);
                    }
                    self.job = Some(job);
                    self.origin = Some(o);
                }
            }
        }
        // A finished walk (this frame's `Done`, or one that completed while the dialog was open)
        // answers from the cache; `None` keeps the honest `--`.
        if self.warming.is_some() {
            let stat = app.session.cached_dir_sizes(&[path]).pop().flatten();
            if let Some(stat) = stat {
                self.apply_size(&stat);
            }
        }
    }

    /// Rewrite the size row from a real answer.
    fn apply_size(&mut self, stat: &DirSizeStat) {
        if let Some((_, ix)) = self.warming {
            if let Some(row) = self.rows.get_mut(ix) {
                row.value = format_size(stat.bytes);
            }
            if let Some(row) = self.rows.get_mut(ix + 1) {
                row.value = stat.files_count.to_string();
            }
        }
        self.warming = None;
    }

    /// Draw it; `Some(answer)` once closed. The size answer arrives through [`tick`], which is the
    /// only place this dialog touches the engine (a draw never does — plan §4.1).
    pub fn show(&mut self, ui: &mut Ui, tokens: &Tokens) -> Option<Answer> {
        let escape = consume_key(ui, Key::Escape);
        let mut close = escape;
        let width = tokens.action_log_width;
        let rows = self.rows.clone();
        let status = self.status.clone();
        let kind = DialogKind::Properties {
            slot: self.slot,
            path: self.subject.display(),
        };
        Area::new(ui.scope_id().with("zenman-properties"))
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .order(Order::Foreground)
            .constrain(true)
            .show(ui.ctx(), |ui| {
                super::confirm::modal_frame(tokens, ui, width, |ui| {
                    ui.label(RichText::new("Properties").color(tokens.text_main).strong());
                    ui.separator();
                    for row in &rows {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&row.label).color(tokens.text_muted).strong());
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(RichText::new(&row.value).color(tokens.text_main));
                                },
                            );
                        });
                    }
                    if let Some(status) = &status {
                        ui.label(RichText::new(status).color(tokens.danger));
                    }
                    ui.separator();
                    if ui
                        .button(RichText::new("Close").color(tokens.text_main))
                        .clicked()
                    {
                        close = true;
                    }
                });
            });
        if close {
            return Some(Answer::Button {
                kind,
                choice: Choice::Cancel,
            });
        }
        None
    }
}

/// One row, tersely.
fn row(label: &str, value: impl Into<String>) -> PropRow {
    PropRow {
        label: label.to_string(),
        value: value.into(),
    }
}

/// The fields both subjects answer: name, type, path, and (for a member) where it lives.
fn base_rows(subject: &Subject) -> Vec<PropRow> {
    let entry = match subject {
        Subject::Local { entry, .. } | Subject::Archive { entry, .. } => entry.as_ref(),
    };
    let name = entry
        .map(|e| e.name.clone())
        .or_else(|| {
            subject
                .display()
                .rsplit('/')
                .next()
                .map(|s: &str| s.to_string())
        })
        .unwrap_or_default();
    let kind = match entry {
        Some(e) if e.is_dir => "Directory",
        Some(e) if e.is_symlink => "Symbolic link",
        Some(_) => "File",
        None if subject.is_archive() => "Archive",
        None => "Directory",
    };
    let mut rows = vec![
        row("Name", if name.is_empty() { "/" } else { &name }),
        row("Type", kind),
        row("Path", subject.display()),
    ];
    if let Some(e) = entry {
        rows.push(row("Permissions", e.mode.as_str()));
        if let Some(mtime) = &e.mod_time {
            rows.push(row("Modified", mtime.format("%Y %b %e, %H:%M").to_string()));
        } else {
            rows.push(row("Modified", UNKNOWN));
        }
    }
    // The fields the wire never carried (plan §2.4: Go's `FileEntry` has no uid/gid, no atime, and no
    // symlink target) — printed as unknown, because an empty value would read as "none".
    rows.push(row("Accessed", UNKNOWN));
    rows.push(row("Owner / group", UNKNOWN));
    if subject.is_archive() {
        rows.push(row("Archive membership", "yes"));
    }
    rows
}

/// The size block: a file's own size, or a directory's (unknown until the walk answers).
fn push_size_rows(rows: &mut Vec<PropRow>, entry: Option<&FileEntry>, is_dir: bool) {
    if is_dir {
        // Both cells are the walk's answer, which does not exist yet: `--`, never `0` (D11).
        rows.push(row("Size", UNKNOWN));
        rows.push(row("Files", UNKNOWN));
    } else {
        let size = entry.map_or_else(
            || UNKNOWN.to_string(),
            |e| format_size(e.size.max(0) as u64),
        );
        rows.push(row("Size", size));
        rows.push(row(
            "Files",
            entry
                .and_then(|e| e.files_count)
                .map_or_else(|| UNKNOWN.to_string(), |n| n.to_string()),
        ));
    }
}

/// Poll the open Properties dialog once per frame (the shell's `logic`, next to the job drain):
/// a directory's size is a job, and this is the half that turns the finished walk into a number on
/// the row — the drawing half never touches the engine.
pub fn tick(app: &mut ZenApp) {
    // Ask before lifting: `take` removes, and a lifted prompt the caller does not put back is a
    // dialog that vanished on a frame that only meant to look at it.
    if !matches!(app.dialogs.active(), Some(Dialog::Properties(_))) {
        return;
    }
    let Some(Dialog::Properties(mut dialog)) = app.dialogs.take() else {
        return;
    };
    dialog.warm_size(app);
    app.dialogs.put(Dialog::Properties(dialog));
}

/// Open Properties for the pane's selection, or for its directory when nothing is selected.
///
/// The archive branch is §7 row 13's criterion: the *same* chord, the *same* dialog, the archive
/// payload. The listing read that failed is reported in the dialog, not swallowed.
pub fn open(app: &mut ZenApp, slot: PaneSlot) {
    let Some(tab) = app.model.panes.get(&slot).and_then(|p| p.active_tab()) else {
        app.notices
            .post(crate::widgets::toast::Level::Warning, "no tab to inspect");
        return;
    };
    let in_archive = matches!(tab.path.store, VfsKind::Archive { .. });
    // The first selected row in listing order (Go's `selectedPaths.values().next()`); no selection
    // means the directory the pane is showing.
    let selected = tab
        .entries
        .entries
        .iter()
        .find(|e| tab.sel.keys().contains(crate::state::Tab::row_key(e)))
        .cloned();
    // The selected row's own path, store-correct in both stores (`Tab::row_fs`): the two cases used
    // to spell the same `join_rel` here, which is right for a host listing and wrong for a member.
    let path = match &selected {
        Some(entry) => tab
            .row_fs(entry)
            .or_else(|| tab.path.join(&entry.name).ok()),
        None => Some(tab.path.clone()),
    };
    let Some(path) = path else {
        app.notices
            .post(crate::widgets::toast::Level::Error, "cannot read this path");
        return;
    };
    if in_archive {
        let props = app.session.archive_props(&path);
        let dialog = PropertiesDialog::archive(slot, path, selected, props);
        // `open_dialog` does the "already open" toast itself; a second one would double up.
        let _ = app.open_dialog(super::Dialog::Properties(dialog));
        return;
    }
    let Some(abs) = path.host_abs() else {
        app.notices
            .post(crate::widgets::toast::Level::Error, "cannot read this path");
        return;
    };
    let dialog = PropertiesDialog::local(slot, abs, selected);
    let _ = app.open_dialog(super::Dialog::Properties(dialog));
}
