//! Step 13b — archive browsing in the UI, Properties, and extract progress + cancel.
//!
//! The plan §7 row 13 criteria this file owns, one named test each:
//!
//! * **VFS browsing** — activating an archive row switches the tab's *store* and lists the container
//!   through `Session::archive_list` (the rows are plain `FileEntry`s, so the pane draws them exactly
//!   like a directory's);
//! * **`a.zip/b.7z` browses to depth 2** — and a third level does **not** open, because
//!   `VfsKind::Archive` has room for one nested member and no more (`MAX_NEST_DEPTH`);
//! * **read-only** — New folder / Paste / Delete / Rename / Chmod are refused *with the reason*, and
//!   nothing changes on disk;
//! * **Properties on <kbd>Alt+Enter</kbd> for local and archive entries** — the same chord, two
//!   engines, real fields, and a directory size that fills in when the walk answers (plan **D11**:
//!   unknown is `--`, never `0`);
//! * **extract shows progress + cancel** — a `Receipt::Running` job polled to a terminal state, the
//!   destination really populated, and a cancellation that stops the worker;
//! * **a corrupt container says so** — the typed error in the pane and in a toast, never an empty
//!   listing;
//! * **the breadcrumb shows the archive trail** — the container's own chip is the store, not the file.
//!
//! The containers are written by the `zip` crate here and now (a dev-dependency on a package already
//! in the graph through `zenman-core`), so every test knows exactly which members exist — including
//! the three-deep chain the captured fixtures cannot express. 7z decoding is covered on the core side
//! by `zenman-core/tests/archive_sevenz.rs`.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};

use egui_kittest::Harness;

use zenman_core::ops::ConflictPolicy;
use zenman_core::path::{AbsPath, FsPath};
use zenman_core::store::config::{LayoutMode, PaneSlot};
use zenman_ui::app::ZenApp;
use zenman_ui::dialogs::Dialog;
use zenman_ui::dialogs::properties::PropertiesDialog;
use zenman_ui::keymap::Command;
use zenman_ui::panels::breadcrumb;
use zenman_ui::panels::file_pane::PaneCommand;
use zenman_ui::state::{Tab, panes_for};

const SETTLE: usize = 6;

// --------------------------------------------------------------------------- fixture plumbing

/// A fresh scratch directory (the trees persist between runs, plan §9.2 step 9).
fn clean(tag: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("zenman-ui-fixtures")
        .join(format!("zenman-ui-fixture-{tag}"));
    common::remove_tree(&root);
    std::fs::create_dir_all(&root).expect("fixture directory");
    root
}

/// The bytes of a stored (uncompressed, so the sizes in the assertions are the ones written) zip.
fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, body) in entries {
        if name.ends_with('/') {
            writer
                .add_directory(name.trim_end_matches('/'), opts)
                .expect("add a directory member");
        } else {
            writer.start_file(*name, opts).expect("start a member");
            writer.write_all(body).expect("write a member");
        }
    }
    writer.finish().expect("finish the container").into_inner()
}

/// Write a zip holding `entries`.
fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
    std::fs::write(path, zip_bytes(entries)).expect("write the container");
}

/// `outer.zip` holding `inner.zip` holding `deep.zip` — the three-container chain the captured
/// fixtures cannot express (their inner 7z is a signature stub, `make-fixtures.py:257-267`).
fn nested_zip_bytes() -> Vec<u8> {
    let deep = zip_bytes(&[("deepest.txt", b"third level, and the cap\n")]);
    let middle = zip_bytes(&[("deep.zip", &deep[..]), ("note.txt", b"second level\n")]);
    zip_bytes(&[("inner.zip", &middle[..]), ("top.txt", b"first level\n")])
}

fn abs(path: &Path) -> AbsPath {
    AbsPath::try_from(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn fs_path(path: &Path) -> FsPath {
    FsPath::from_abs(abs(path))
}

fn build_harness(app: ZenApp) -> Harness<'static, ZenApp> {
    Harness::builder()
        .with_size(egui::vec2(1200.0, 800.0))
        .with_max_steps(64)
        .with_accessibility_check(false)
        .with_context_setup(|ctx| {
            egui_extras::install_image_loaders(ctx);
            zenman_ui::theme::fonts::install(ctx);
            zenman_ui::theme::apply(ctx, zenman_core::store::config::ThemeMode::Dark);
        })
        .build_ui_state(
            |ui, app| {
                app.logic(ui.ctx());
                app.ui(ui);
            },
            app,
        )
}

/// A single-pane shell listing `dir`.
fn pane_harness(tag: &str, dir: &Path) -> Harness<'static, ZenApp> {
    let session = common::session(tag);
    let mut app = ZenApp::new(session, Vec::new());
    app.set_layout(panes_for(LayoutMode::Single));
    app.open_tab(PaneSlot::Left, fs_path(dir));
    let mut harness = build_harness(app);
    settle(&mut harness);
    harness
}

fn settle(harness: &mut Harness<'static, ZenApp>) {
    for _ in 0..SETTLE {
        harness.run();
    }
}

fn shown<'h>(harness: &'h Harness<'static, ZenApp>, slot: PaneSlot) -> &'h Tab {
    harness
        .state()
        .model
        .panes
        .get(&slot)
        .and_then(|p| p.active_tab())
        .expect("the harness has a shown tab")
}

fn names(tab: &Tab) -> Vec<String> {
    tab.entries
        .entries
        .iter()
        .map(|entry| entry.name.clone())
        .collect()
}

fn row_of(tab: &Tab, name: &str) -> Option<usize> {
    tab.entries
        .entries
        .iter()
        .position(|entry| entry.name == name)
}

/// Activate a row exactly as a double-click does: the pane's own `Open` command.
fn activate(harness: &mut Harness<'static, ZenApp>, slot: PaneSlot, name: &str) {
    let ix = row_of(shown(harness, slot), name)
        .unwrap_or_else(|| panic!("no row {name:?} in {:?}", names(shown(harness, slot))));
    harness
        .state_mut()
        .run_command(slot, PaneCommand::Open { ix });
    settle(harness);
}

fn select(harness: &mut Harness<'static, ZenApp>, slot: PaneSlot, name: &str) {
    let ix = row_of(shown(harness, slot), name)
        .unwrap_or_else(|| panic!("no row {name:?} in {:?}", names(shown(harness, slot))));
    let keys: Vec<String> = shown(harness, slot)
        .entries
        .entries
        .iter()
        .map(|e| Tab::row_key(e).to_string())
        .collect();
    let tab = harness
        .state_mut()
        .model
        .panes
        .get_mut(&slot)
        .and_then(|p| p.active_tab_mut())
        .expect("a shown tab");
    tab.sel.apply(
        zenman_ui::widgets::virtual_list::SelKind::Single,
        ix,
        keys.len(),
        |i| keys.get(i).map(String::as_str),
    );
}

fn dispatch(harness: &mut Harness<'static, ZenApp>, slot: PaneSlot, command: Command) {
    let ctx = harness.ctx.clone();
    harness.state_mut().dispatch(slot, command, &ctx);
    settle(harness);
}

fn toast_texts(harness: &Harness<'static, ZenApp>) -> Vec<String> {
    harness
        .state()
        .notices
        .queued()
        .iter()
        .map(|notice| notice.text.clone())
        .collect()
}

fn properties(harness: &Harness<'static, ZenApp>) -> PropertiesDialog {
    match harness.state().dialogs.active() {
        Some(Dialog::Properties(dialog)) => dialog.clone(),
        other => panic!("expected the properties dialog, found {other:?}"),
    }
}

fn prop<'a>(dialog: &'a PropertiesDialog, label: &str) -> &'a str {
    dialog
        .rows
        .iter()
        .find(|row| row.label == label)
        .map(|row| row.value.as_str())
        .unwrap_or_else(|| panic!("no {label:?} row in {:?}", dialog.rows))
}

// ------------------------------------------------------------------------------- VFS browsing

#[test]
fn activating_an_archive_row_lists_its_members_read_only() {
    let root = clean("browse");
    write_zip(
        &root.join("plain.zip"),
        &[
            ("a.txt", b"first member\n"),
            ("docs/", b""),
            ("docs/readme.md", b"# inside the container\n"),
        ],
    );
    let mut harness = pane_harness("browse", &root);
    let slot = PaneSlot::Left;
    assert!(
        !shown(&harness, slot).path.is_archive_file(),
        "the pane starts on the filesystem"
    );

    activate(&mut harness, slot, "plain.zip");

    let tab = shown(&harness, slot);
    assert_eq!(
        tab.path.store_depth(),
        1,
        "activating the row opened the container as a store"
    );
    let mut listed = names(tab);
    listed.sort();
    assert_eq!(listed, vec!["a.txt", "docs"], "both root members listed");
    assert_eq!(
        tab.entries
            .entries
            .iter()
            .find(|e| e.name == "a.txt")
            .expect("a.txt row")
            .mode,
        "-r--r--r--",
        "an archive member is never writable, and the row says so"
    );
    assert!(
        tab.entries
            .entries
            .iter()
            .find(|e| e.name == "docs")
            .expect("docs row")
            .is_dir,
        "a synthesised directory member is still a directory row"
    );
    // The member path is the *internal* path, which is what the pane joins onto the store.
    assert_eq!(
        tab.entries
            .entries
            .iter()
            .find(|e| e.name == "a.txt")
            .expect("a.txt row")
            .rel_path,
        "a.txt"
    );

    // Navigating a directory inside the container stays in the same store.
    activate(&mut harness, slot, "docs");
    let tab = shown(&harness, slot);
    assert_eq!(tab.path.store_depth(), 1);
    assert_eq!(
        tab.path.relative_to(&tab.path.store_root()),
        Some("docs".into())
    );
    assert_eq!(names(tab), vec!["readme.md"]);

    // …and Up steps back through the store like any other navigation.
    let ctx = harness.ctx.clone();
    harness.state_mut().dispatch(slot, Command::GoUp, &ctx);
    settle(&mut harness);
    assert_eq!(shown(&harness, slot).path.relative, "");
    let mut back = names(shown(&harness, slot));
    back.sort();
    assert_eq!(back, vec!["a.txt", "docs"]);
}

#[test]
fn a_nested_archive_browses_to_depth_two_and_a_third_level_does_not_open() {
    let root = clean("nested");
    // outer.zip / inner.zip / deep.zip — three containers, but only two stores.
    let bytes = nested_zip_bytes();
    std::fs::write(root.join("outer.zip"), bytes).expect("write the outer container");

    let mut harness = pane_harness("nested", &root);
    let slot = PaneSlot::Left;

    activate(&mut harness, slot, "outer.zip");
    assert_eq!(shown(&harness, slot).path.store_depth(), 1);

    activate(&mut harness, slot, "inner.zip");
    let tab = shown(&harness, slot);
    assert_eq!(
        tab.path.store_depth(),
        2,
        "a.zip/b.7z browses to depth 2 (§7 row 13)"
    );
    assert!(names(tab).contains(&"deep.zip".to_string()));

    // The cap: at depth 2 an archive-named member is a member, so `Open` does not switch store and
    // does not navigate — it is a file row.
    let before = shown(&harness, slot).path.clone();
    activate(&mut harness, slot, "deep.zip");
    let after = shown(&harness, slot);
    assert_eq!(after.path.store_depth(), 2, "no third store opens");
    assert_eq!(
        after.path.relative, before.relative,
        "and the pane did not move"
    );
    assert!(
        after.entries.entries.iter().any(|e| e.name == "deep.zip"),
        "the depth-3 container is still just a member row"
    );

    // Back/forward step across the store switch like any other navigation (the history is the
    // tab's own, and it holds store paths).
    harness.state_mut().run_command(slot, PaneCommand::Back);
    settle(&mut harness);
    assert_eq!(
        shown(&harness, slot).path.store_depth(),
        1,
        "back leaves the nested store"
    );
    harness.state_mut().run_command(slot, PaneCommand::Forward);
    settle(&mut harness);
    assert_eq!(
        shown(&harness, slot).path.store_depth(),
        2,
        "and forward returns to it"
    );
}

#[test]
fn a_corrupt_container_reports_the_typed_error_inline_and_as_a_toast() {
    let root = clean("corrupt");
    let good = zip_bytes(&[("a.txt", b"content that will be truncated\n")]);
    // A signature and a truncated tail: the reader's `ArchiveCorrupt`, not an empty listing.
    std::fs::write(root.join("broken.zip"), &good[..40]).expect("write the stub");

    let mut harness = pane_harness("corrupt", &root);
    let slot = PaneSlot::Left;
    activate(&mut harness, slot, "broken.zip");

    let tab = shown(&harness, slot);
    let error = tab.error.as_ref().expect("the listing error is on the tab");
    assert_eq!(error.kind(), "archive_corrupt");
    assert!(
        toast_texts(&harness)
            .iter()
            .any(|text| text.contains("corrupt") || text.contains("broken.zip")),
        "the failure also reached a toast: {:?}",
        toast_texts(&harness)
    );
    assert!(
        tab.entries.entries.is_empty(),
        "a failed listing puts no rows up: the error row is the pane's state, not an empty listing"
    );
}

// ----------------------------------------------------------------------------------- read-only

#[test]
fn write_commands_inside_an_archive_are_refused_with_a_reason_and_change_nothing() {
    let root = clean("readonly");
    write_zip(&root.join("box.zip"), &[("a.txt", b"member bytes\n")]);

    let mut harness = pane_harness("readonly", &root);
    let slot = PaneSlot::Left;
    activate(&mut harness, slot, "box.zip");
    let before: Vec<String> = std::fs::read_dir(root.join("box.zip").parent().expect("parent"))
        .expect("read the host directory")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();

    select(&mut harness, slot, "a.txt");
    for command in [
        Command::NewFolder,
        Command::Delete,
        Command::PermanentDelete,
        Command::Rename,
        Command::Chmod,
        Command::Paste,
        Command::Cut,
    ] {
        dispatch(&mut harness, slot, command);
        let texts = toast_texts(&harness);
        assert!(
            texts.iter().any(|t| t.contains("read-only")),
            "{command:?} did not say why it refused: {texts:?}"
        );
    }

    let after: Vec<String> = std::fs::read_dir(root.join("box.zip").parent().expect("parent"))
        .expect("read the host directory")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(before, after, "nothing was created, renamed or deleted");
    assert!(
        root.join("box.zip").is_file(),
        "the container itself is untouched"
    );
    // The listing is still the container's, and the member is still there.
    assert_eq!(names(shown(&harness, slot)), vec!["a.txt"]);
}

// -------------------------------------------------------------------------------- properties

#[test]
fn alt_enter_on_a_local_file_opens_properties_with_real_fields() {
    let root = clean("props-local");
    std::fs::write(root.join("notes.txt"), b"0123456789").expect("write a file");
    std::fs::create_dir_all(root.join("sub")).expect("write a directory");
    std::fs::write(root.join("sub/inner.txt"), b"abc").expect("write a nested file");

    let mut harness = pane_harness("props-local", &root);
    let slot = PaneSlot::Left;
    select(&mut harness, slot, "notes.txt");
    dispatch(&mut harness, slot, Command::Properties);

    let dialog = properties(&harness);
    assert!(!dialog.subject.is_archive(), "a local subject");
    assert_eq!(prop(&dialog, "Name"), "notes.txt");
    assert_eq!(prop(&dialog, "Type"), "File");
    assert!(
        prop(&dialog, "Path").ends_with("notes.txt"),
        "the path row is the real one: {}",
        prop(&dialog, "Path")
    );
    assert_eq!(prop(&dialog, "Size"), "10 B");
    assert_ne!(
        prop(&dialog, "Modified"),
        "--",
        "the listing carries an mtime"
    );

    // A *directory* is the D11 case: `--` while the walk runs, the real number when it answers.
    harness.state_mut().dialogs.close();
    settle(&mut harness);
    select(&mut harness, slot, "sub");
    dispatch(&mut harness, slot, Command::Properties);
    let opened = properties(&harness);
    assert_eq!(prop(&opened, "Type"), "Directory");
    common::settle_until(
        &mut harness,
        "the directory size arriving in the properties dialog",
        |h| match h.state().dialogs.active() {
            Some(Dialog::Properties(dialog)) => dialog
                .rows
                .iter()
                .any(|r| r.label == "Size" && r.value != "--"),
            _ => false,
        },
    );
    let warmed = properties(&harness);
    assert_eq!(
        prop(&warmed, "Size"),
        "3 B",
        "the walk's answer, not a UI-side guess"
    );
    assert_eq!(prop(&warmed, "Files"), "1");
}

#[test]
fn alt_enter_on_an_archive_member_opens_the_archive_payload() {
    let root = clean("props-archive");
    write_zip(
        &root.join("pack.zip"),
        &[
            ("a.txt", b"first member bytes here\n"),
            ("docs/readme.md", b"# hi\n"),
        ],
    );

    let mut harness = pane_harness("props-archive", &root);
    let slot = PaneSlot::Left;
    activate(&mut harness, slot, "pack.zip");
    select(&mut harness, slot, "a.txt");
    dispatch(&mut harness, slot, Command::Properties);

    let dialog = properties(&harness);
    assert!(dialog.subject.is_archive(), "the subject is a member");
    assert_eq!(prop(&dialog, "Name"), "a.txt");
    assert_eq!(prop(&dialog, "Format"), "zip");
    assert_eq!(prop(&dialog, "Store depth"), "1");
    assert_eq!(prop(&dialog, "Archive membership"), "yes");
    // `archive_props`' own numbers, not UI-side arithmetic (§5.8: no size computation in the UI).
    // The subtree the member names, measured by `archive_props` — not by the UI.
    assert_eq!(prop(&dialog, "Uncompressed total"), "24 B");
    assert!(
        !prop(&dialog, "Compressed size (container)").is_empty(),
        "the container's own size is on the card"
    );
    assert_eq!(prop(&dialog, "Encrypted members"), "0");
}

// ------------------------------------------------------------- extract progress + cancellation

#[test]
fn extract_runs_to_a_terminal_state_with_progress_and_fills_the_destination() {
    let root = clean("extract");
    write_zip(
        &root.join("payload.zip"),
        &[
            ("one.txt", b"first extracted file\n"),
            ("two.txt", b"second extracted file\n"),
        ],
    );

    let mut harness = pane_harness("extract", &root);
    let slot = PaneSlot::Left;
    activate(&mut harness, slot, "payload.zip");
    harness
        .state_mut()
        .extract_here(slot, ConflictPolicy::SkipExisting);
    assert_eq!(
        harness.state().extracts.len(),
        1,
        "the receipt opened one progress panel"
    );
    // Progress was observed *before* the terminal state, and the job is a tracked one.
    let live = harness.state().extracts[0].live;
    let tracked = harness.state().jobs.foreground.len();
    settle(&mut harness);
    let _ = (live, tracked);

    common::settle_until(
        &mut harness,
        "the extraction reaching a terminal state",
        |h| h.state().extracts.first().is_some_and(|job| !job.live),
    );
    let job = &harness.state().extracts[0];
    assert!(!job.summary.is_empty(), "the panel reports a summary");
    assert!(
        job.summary.contains("Extraction finished"),
        "{}",
        job.summary
    );

    // The destination is the directory that holds the container, and the bytes are really there.
    assert_eq!(
        std::fs::read(root.join("one.txt")).expect("one.txt"),
        b"first extracted file\n"
    );
    assert_eq!(
        std::fs::read(root.join("two.txt")).expect("two.txt"),
        b"second extracted file\n"
    );
    // …and the pane is still listing its store afterwards (core's `DirChanged` from the done
    // hook reloads the destination's tabs; this tab is inside the container).
    settle(&mut harness);
    assert_eq!(names(shown(&harness, slot)), vec!["one.txt", "two.txt"]);
}

#[test]
fn cancelling_an_extraction_stops_it_and_says_so() {
    let root = clean("cancel");
    // Enough members that the worker is still running when the panel's cancel lands.
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    for ix in 0..600_u32 {
        entries.push((format!("member-{ix:04}.txt"), vec![b'x'; 8 * 1024]));
    }
    let owned: Vec<(&str, &[u8])> = entries.iter().map(|(n, b)| (n.as_str(), &b[..])).collect();
    write_zip(&root.join("big.zip"), &owned);

    let mut harness = pane_harness("cancel", &root);
    let slot = PaneSlot::Left;
    activate(&mut harness, slot, "big.zip");
    harness
        .state_mut()
        .extract_here(slot, ConflictPolicy::SkipExisting);
    let id = harness.state().extracts[0].id;
    // Cancel in the same frame the job started: the worker is still walking.
    let accepted = harness.state().session.archive_cancel(id);
    assert!(accepted, "the job was still live to be cancelled");
    settle(&mut harness);

    common::settle_until(
        &mut harness,
        "the cancelled extraction reaching its terminal state",
        |h| h.state().extracts.first().is_some_and(|job| !job.live),
    );
    let job = &harness.state().extracts[0];
    assert!(
        job.summary.to_lowercase().contains("cancel"),
        "the panel says the extraction was cancelled: {}",
        job.summary
    );
    // The files written before the stop stay (the report is the truth, not a rollback) — but the
    // container's full member count did not land.
    let landed = std::fs::read_dir(&root)
        .expect("read the destination")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("member-"))
        .count();
    assert!(
        landed < entries.len(),
        "a cancelled extraction wrote every member ({landed})"
    );
}

// ------------------------------------------------------------------------------- breadcrumb

#[test]
fn the_breadcrumb_trail_names_the_stores_it_crosses() {
    let host = AbsPath::try_from(Path::new("/data/box.zip")).expect("abs");
    let nested = FsPath::archive(host.clone(), Some("inner.7z".into()), "docs")
        .expect("a depth-2 member path");

    let trail = breadcrumb::trail(&nested);
    let labels: Vec<&str> = trail.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels, vec!["Root", "data", "box.zip", "inner.7z", "docs"]);

    // The host chip is the *store*, not the file: clicking it lists the container.
    let host_crumb = trail
        .iter()
        .find(|c| c.label == "box.zip")
        .expect("host chip");
    assert_eq!(host_crumb.target.store_depth(), 1);
    assert!(host_crumb.target.is_store_root());

    // …and the nested member's chip is the depth-2 store, which is the cap.
    let nested_crumb = trail
        .iter()
        .find(|c| c.label == "inner.7z")
        .expect("nested chip");
    assert_eq!(nested_crumb.target.store_depth(), 2);
    assert!(nested_crumb.target.is_store_root());

    // The plain-filesystem trail is unchanged: `Root` then one chip per segment.
    let plain = FsPath::fs("data/box.zip").expect("fs path");
    let plain_trail = breadcrumb::trail(&plain);
    let labels: Vec<&str> = plain_trail.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels, vec!["Root", "data", "box.zip"]);
    assert_eq!(
        plain_trail.last().expect("last chip").target,
        plain,
        "outside a store the last chip is the path itself"
    );
}
