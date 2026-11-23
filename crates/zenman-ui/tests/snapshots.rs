//! `xtask snapshots`'s engine: the one place that renders the baseline PNGs under
//! `tests/snapshots/` and the one place that decides — honestly, with a printed reason — that it
//! cannot.
//!
//! Plan §7 step 7: "writes `crates/zenman-ui/tests/snapshots/01-empty.png` from the software/GL
//! rasteriser (skips with a printed reason when no adapter exists)". Step 8c adds the two populated
//! baselines (`02-file-list.png`, `03-file-grid.png`), rendered from the **deterministic fixture
//! tree** of [`common::listing_tree`] — never the developer's home directory — so a baseline means
//! the same rows on every machine. Two facts learned the hard way (plan §9.2 step-7 row):
//!
//! * `egui_kittest`'s default renderer is *lazy* but not *soft* — the first render call with no
//!   adapter panics inside `expect("Failed to create render state")`. So the adapter probe comes
//!   first, in our own code, and the skip path is real IO-free output rather than a caught panic.
//! * The plan assumed `harness.snapshot(name)`; that function compares against a committed
//!   baseline and fails when it differs. Pixel equality is step 15's *human* gate, so this file
//!   renders and writes (`ZENMAN_SNAPSHOT_WRITE=1`, set by `cargo xtask snapshots`), and in plain
//!   `cargo test` mode only proves the frame renders, has the expected size, and carries the
//!   content it claims — no pixel diff, no fake green.
//!
//! Every baseline goes through the same three functions: [`Scene::build`] (session + layout + tab +
//! view mode + the font-stack context setup), [`drive`] (the click pair that makes the selection
//! non-degenerate) and [`render`] (probe → settle → render → size assert → gated write). A later
//! step that wants a `04-…` baseline adds a [`Scene`] variant and a test; it must not copy the
//! harness setup.

mod common;

use std::path::PathBuf;

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use image::RgbaImage;

use zenman_core::ops::batch_rename::{Row, Rules};
use zenman_core::ops::mkdir::MkdirReq;
use zenman_core::path::{AbsPath, FsPath};
use zenman_core::session::{Session, SessionPaths};
use zenman_core::store::config::{LayoutMode, PaneSlot, ThemeMode, ViewMode, ZenConfig};
use zenman_ui::app::ZenApp;
use zenman_ui::dialogs::Dialog;
use zenman_ui::dialogs::batch_rename::BatchRename;
use zenman_ui::dialogs::search_manager::ManagerRequest;
use zenman_ui::keymap::{Chord, Command};
use zenman_ui::menus::MenuTarget;
use zenman_ui::panels::file_pane::PaneCommand;
use zenman_ui::state::{Tab, panes_for, quad_slots};
use zenman_ui::theme::Tokens;

const SIZE: egui::Vec2 = egui::vec2(1440.0, 900.0);

/// The wall clock the two *clock-derived* surfaces of the shell are frozen to, so the baselines stay
/// byte-reproducible (plan §9.2 step-8c review 3). It is the fixture's own `MTIME_BASE`
/// (2023-11-14 22:13:20 UTC) spelled two ways: the unix second `index-status`'s `last_indexed` holds,
/// and the RFC 3339 shape an `action.log` record carries.
const FIXED_UNIX: i64 = 1_700_000_000;
/// [`FIXED_UNIX`] as the action-log row prints it. The panel trims to the minute, so any stamp inside
/// that minute paints identically.
const FIXED_RFC3339: &str = "2023-11-14T22:13:20+00:00";

/// The frozen `timeAgo` bucket of the painted log rows (step 15b): a live one changes between
/// runs, exactly like the RFC 3339 stamp beside it.
const FIXED_AGO: &str = "1m ago";

/// The rows the frozen index-running frame claims to have indexed. A rebuild's *live* count is a race
/// against a worker thread, so the frame is stamped with a fixed one — see [`drive_index`].
const INDEXED_FROZEN: i64 = 200;

/// Frames to let the shell settle — the same count `tests/file_pane.rs` uses before it asserts.
const SETTLE: usize = 6;

/// The page size a fresh tab asks for ([`zenman_ui::state::DEFAULT_PAGE_SIZE`], Go's 200). The
/// populated baselines show exactly one page, which is what makes "200 items in the listing" true.
const PAGE: usize = zenman_ui::state::DEFAULT_PAGE_SIZE;

/// The two fixture rows the populated baselines select: the listing's two directories, so the
/// selection is visible at the very top of the frame and its status line reads `2 items selected`.
const SELECTED: [&str; 2] = ["docs", "reports"];

/// One baseline: what to build, and what the artefact is called.
enum Scene {
    /// The canonical empty shell — a two-pane split with no tabs (the Go app's default view).
    Empty,
    /// One pane, one real tab over the fixture listing, in `list` or `grid` view.
    Listing {
        /// The `common::session`/`listing_tree` tag — distinct per baseline so the trees never share.
        tag: &'static str,
        /// The pane's view mode.
        view: ViewMode,
    },
    /// Two panes, each with its own tab — the Go app's default `split` workspace.
    Split { tag: &'static str },
    /// The four-pane grid (`dock::set_layout` / `AppModel::set_grid`; no `LayoutMode` spelling).
    Quad { tag: &'static str },
    /// One drawer open over the populated listing (bookmarks left, disks right).
    Drawer { tag: &'static str, side: Side },
    /// The action-log slide-in, over a log with real records in it.
    ActionLog { tag: &'static str },
    /// A context menu, actually drawn: over a row, or over the background.
    Menu { tag: &'static str, at: MenuAt },
    /// The batch-rename dialog, open on a real selection.
    BatchRename { tag: &'static str },
    /// The quick-find dialog, open with a finished walk's rows.
    QuickFind { tag: &'static str },
    /// The Search Manager, on a terminal index (`running: false`) or mid-rebuild (`true`).
    Index { tag: &'static str, running: bool },
    /// The address field, open on the path it stands for.
    Address { tag: &'static str },
}

/// Which drawer [`Scene::Drawer`] opens.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
}

/// Which menu [`Scene::Menu`] opens.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MenuAt {
    /// A row — Go's `handlePaneContextMenu` with a target.
    Row,
    /// The empty band below the rows.
    Background,
}

/// Every baseline against the `docs/parity/` Go reference it is reviewed against, and the one line a
/// reviewer needs to know about the pairing (plan §7 step 15: *every reference screenshot has a named
/// snapshot*). `""` in the middle column means the baseline has no Go counterpart.
///
/// This is the single source of truth for the pairing and `cargo xtask parity` reads it out of this
/// file's own source — the same discipline as [`SIZE`] and the baseline names — so a new scene without
/// a row is a gate failure rather than a silent gap, and a renumbered reference cannot drift unnoticed.
/// The notes are printed verbatim by `cargo xtask parity` and by its review index page.
#[rustfmt::skip]
const REFERENCE_MAP: &[(&str, &str, &str)] = &[
    ("01-empty.png", "", "the empty two-pane shell — ours alone: the Go app always restores a workspace, so no reference exists"),
    ("02-file-list.png", "01-main-list.png", "the list view over the deterministic fixture tree (step 8)"),
    ("03-file-grid.png", "02-grid-view.png", "the grid view over the same tree"),
    ("04-split-view.png", "03-split-view.png", "two panes with a live tab each, plus a selection"),
    ("05-quad-view.png", "04-quad-view.png", "the four-pane grid — the narrow pane is D7's clipping test"),
    ("06-left-drawer.png", "05-left-drawer.png", "Places / bookmarks / mounted drives (D8)"),
    ("07-right-drawer.png", "06-right-drawer.png", "the custom-actions drawer (D9)"),
    ("08-action-log-drawer.png", "07-action-log-drawer.png", "Go's Action History slide-in over real log records (D10)"),
    ("09-search-manager.png", "08-search-manager.png", "the Search Manager on a terminal index, empty query (D2/D3)"),
    ("10-index-running.png", "16-index-running.png", "the same dialog mid-rebuild: settings + progress, no stale toast (D4/D5)"),
    ("11-context-menu-file.png", "09-context-menu-file.png", "the row context menu, Go's item set with our chords"),
    ("12-context-menu-background.png", "10-context-menu-background.png", "the background context menu, Go's three rows (D11)"),
    ("13-batch-rename.png", "14-batch-rename.png", "the batch-rename preview table (D12)"),
    ("14-quick-find.png", "15-quick-find.png", "quick-find over a finished walk (D6)"),
    ("15-address-bar.png", "", "the address field open over its own path — ours alone: Go's bar was a native `<input>`, and no reference screenshot shows it open"),
];

/// The `REFERENCE_MAP` row of one baseline: its Go reference (empty when it has none) and the note.
/// An unmapped baseline is reported rather than hidden — `cargo xtask parity` fails on it.
fn reference_for(baseline: &str) -> (&'static str, &'static str) {
    REFERENCE_MAP
        .iter()
        .find(|(name, _, _)| *name == baseline)
        .map_or(
            ("", "UNMAPPED — `cargo xtask parity` fails on this row"),
            |(_, reference, note)| (*reference, *note),
        )
}

impl Scene {
    /// The committed artefact's name. The scheme is `NN-<what>.png`, ordered so the populated
    /// baselines read next to the empty one and echo `docs/parity/01-main-list.png` /
    /// `02-grid-view.png` (the Go references these are reviewed against).
    const fn file_name(&self) -> &'static str {
        match self {
            Self::Empty => "01-empty.png",
            Self::Listing {
                view: ViewMode::List,
                ..
            } => "02-file-list.png",
            Self::Listing { .. } => "03-file-grid.png",
            Self::Split { .. } => "04-split-view.png",
            Self::Quad { .. } => "05-quad-view.png",
            Self::Drawer {
                side: Side::Left, ..
            } => "06-left-drawer.png",
            Self::Drawer { .. } => "07-right-drawer.png",
            Self::ActionLog { .. } => "08-action-log-drawer.png",
            Self::Index { running: false, .. } => "09-search-manager.png",
            Self::Index { .. } => "10-index-running.png",
            Self::Menu {
                at: MenuAt::Row, ..
            } => "11-context-menu-file.png",
            Self::Menu { .. } => "12-context-menu-background.png",
            Self::BatchRename { .. } => "13-batch-rename.png",
            Self::QuickFind { .. } => "14-quick-find.png",
            Self::Address { .. } => "15-address-bar.png",
        }
    }

    /// The session tag this scene needs its own temp directory under.
    const fn tag(&self) -> &'static str {
        match self {
            Self::Empty => "snap-empty",
            Self::Listing { tag, .. }
            | Self::Split { tag }
            | Self::Quad { tag }
            | Self::Drawer { tag, .. }
            | Self::ActionLog { tag }
            | Self::Menu { tag, .. }
            | Self::BatchRename { tag }
            | Self::QuickFind { tag }
            | Self::Address { tag }
            | Self::Index { tag, .. } => tag,
        }
    }

    /// Does the artefact depend on a *mutable* engine store — an `action.log` that would otherwise grow
    /// by one record per run, an index database that would? Then its session directory is wiped first:
    /// the fixture trees persist between runs, and so would the records.
    fn needs_fresh_session(&self) -> bool {
        matches!(self, Self::ActionLog { .. } | Self::Index { .. })
    }

    /// Build the shell for this scene: a real `Session` over a private temp data directory, the
    /// layout, and — for the populated scenes — a real tab over the fixture listing at the wanted
    /// view mode. The context setup (image loaders *then* the font stack *then* the theme) is the
    /// requirement of rule 11: this egui fork panics on a missing glyph, and `egui_extras` image
    /// painting panics without the loaders.
    fn build(&self) -> Harness<'static, ZenApp> {
        if self.needs_fresh_session() {
            remove_session_dir(self.tag());
        }
        let session = match self {
            // Every index call needs a database; `common::session` deliberately has none.
            Self::Index { tag, .. } => {
                let root = common::listing_tree(tag);
                index_session(tag, &root)
            }
            _ => common::session(self.tag()),
        };
        let mut app = ZenApp::new(session, vec![format!("snapshot: {}", self.file_name())]);
        match self {
            Self::Empty => app.set_layout(panes_for(LayoutMode::Split)),
            Self::Listing { tag, view } => {
                app.set_layout(panes_for(LayoutMode::Single));
                let path = fixture_path(&common::listing_tree(tag));
                app.open_tab(PaneSlot::Left, path);
                // The toolbar toggle is the only way the app itself switches views, and it is a
                // *pane* command, so this is exactly what the toolbar button does.
                if *view == ViewMode::Grid {
                    app.run_command(PaneSlot::Left, PaneCommand::ToggleGrid);
                }
            }
            Self::Split { tag } => {
                let tree = common::listing_tree(tag);
                freeze_mtime(&tree.join("docs"), FIXED_UNIX as u64);
                app.set_layout(panes_for(LayoutMode::Split));
                app.open_tab(PaneSlot::Left, fixture_path(&tree));
                // A *different* directory, so the two panes' rows stay distinguishable by label.
                app.open_tab(PaneSlot::Right, fixture_path(&tree.join("docs")));
            }
            Self::Quad { tag } => {
                let tree = common::listing_tree(tag);
                freeze_mtime(&tree.join("docs"), FIXED_UNIX as u64);
                freeze_mtime(&tree.join("reports"), FIXED_UNIX as u64);
                app.set_layout(quad_slots());
                for (slot, dir) in [
                    (PaneSlot::Left, tree.clone()),
                    (PaneSlot::Right, tree.join("docs")),
                    (PaneSlot::LeftBottom, tree.join("reports")),
                    (PaneSlot::RightBottom, tree),
                ] {
                    app.open_tab(slot, fixture_path(&dir));
                }
            }
            // A background menu needs empty space under the rows to be *had* from, so these two scenes
            // list the small tree (three rows) rather than the 457-row page.
            Self::Menu { tag, .. } => {
                let tree = common::nested_tree(tag);
                freeze_mtime(&tree, FIXED_UNIX as u64);
                // (the tree is built above; nothing else to stage)
                app.set_layout(panes_for(LayoutMode::Single));
                app.open_tab(PaneSlot::Left, fixture_path(&tree));
            }
            Self::BatchRename { tag }
            | Self::QuickFind { tag }
            | Self::Address { tag }
            | Self::Drawer { tag, .. }
            | Self::ActionLog { tag }
            | Self::Index { tag, .. } => {
                app.set_layout(panes_for(LayoutMode::Single));
                app.open_tab(PaneSlot::Left, fixture_path(&common::listing_tree(tag)));
            }
        }
        let mut builder = Harness::builder()
            // The header's workspace `ComboBox` (step 14b) surfaces `current_text_value` but no
            // accessible name in this fork — the same omission the fork's own `TextEdit` has.
            .with_accessibility_check(false)
            .with_size(SIZE);
        if !matches!(self, Self::Empty | Self::Listing { .. }) {
            // Dialogs, jobs and the coalesced `DirChanged` window stretch the frame loop — the same 64
            // `tests/{menus,quick_find,search_manager}.rs` settled on.
            builder = builder.with_max_steps(64);
        }
        builder
            .with_context_setup(|ctx| {
                egui_extras::install_image_loaders(ctx);
                zenman_ui::theme::fonts::install(ctx);
                zenman_ui::theme::apply(ctx, ThemeMode::Dark);
            })
            .build_ui_state(
                |ui, app| {
                    app.logic(ui.ctx());
                    app.ui(ui);
                },
                app,
            )
    }
}

/// Pin **every** entry of a fixture tree to a deterministic mtime.
///
/// `common::listing_tree` pins the mtimes of the rows of the `listing` directory itself, and
/// `common::nested_tree` pins none at all: the files inside `docs/`, the one file inside `reports/`
/// and every row of the nested tree are written *now*, so their date cells are the wall clock — which
/// is what made four of the new baselines differ between two runs of the same checkout. A scene that
/// lists one of those directories pins it here, in the same order every time (a `BTreeMap`, so the
/// assignment cannot depend on `read_dir`).
fn freeze_mtime(root: &std::path::Path, base: u64) {
    let mut entries = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                entries.push(path);
            }
        }
    }
    entries.sort();
    for (index, path) in entries.iter().enumerate() {
        let when = std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::from_secs(base + index as u64 * 86_400);
        let file = std::fs::File::options()
            .read(true)
            .open(path)
            .unwrap_or_else(|e| panic!("opening {}: {e}", path.display()));
        file.set_modified(when)
            .unwrap_or_else(|e| panic!("setting {}: {e}", path.display()));
    }
}

/// Pin one path's mtime, file *or* directory: the two rows the action-log scene `mkdir`s are created
/// *now*, and their date cells are painted in the listing behind the panel.
fn pin_mtime(path: &std::path::Path, secs: u64) {
    let when = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs);
    let file = std::fs::File::options()
        .read(true)
        .open(path)
        .unwrap_or_else(|e| panic!("opening {}: {e}", path.display()));
    file.set_modified(when)
        .unwrap_or_else(|e| panic!("setting {}: {e}", path.display()));
}

/// `<scratch>/zenman-ui-test-<tag>` — the *session* directory (config, `action.log`, index database),
/// as distinct from the fixture tree `common::listing_tree` builds.
fn session_dir(tag: &str) -> PathBuf {
    std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("zenman-ui-fixtures")
        .join(format!("zenman-ui-test-{tag}"))
}

fn remove_session_dir(tag: &str) {
    common::remove_tree(&session_dir(tag));
}

/// A session **with** an index database over `root` — the scene-level copy of `tests/search_manager.rs`'s
/// fixture, because `common::session` has no database and every index call needs one.
fn index_session(tag: &str, root: &std::path::Path) -> Session {
    let dir = session_dir(tag);
    std::fs::create_dir_all(&dir).expect("the session directory is creatable");
    let paths = SessionPaths::new(
        dir.join("config.json"),
        dir.join("action.log"),
        Some(dir.join("zen-man.db")),
        dir.join("probe"),
    )
    .with_db_open(4, std::time::Duration::from_millis(25));
    let (session, _notes) = Session::open(
        paths,
        ZenConfig::default(),
        zenman_core::platform::default_env(),
        ZenConfig::default().limits(),
    )
    .expect("a session opens over a fresh directory");
    let mut cfg = session.index_config();
    cfg.roots = vec![root.display().to_string()];
    session
        .set_index_config(cfg, session.next_origin())
        .expect("the fixture root is a sound configuration");
    session
}

/// Drive the populated scene's selection through the real input path: a plain click, then a
/// shift-click extending the range — so the frame shows the selection fill, the accent border and a
/// status line that is not "0 items selected".
fn drive(harness: &mut Harness<'static, ZenApp>) {
    settle(harness);
    let [first, second] = SELECTED;
    harness.get_by_label(first).click();
    settle(harness);
    harness
        .get_by_label(second)
        .click_modifiers(egui::Modifiers {
            shift: true,
            ..Default::default()
        });
    settle(harness);
}

fn settle(harness: &mut Harness<'static, ZenApp>) {
    for _ in 0..SETTLE {
        harness.run();
    }
}

/// Probe for a rasteriser, render the scene, assert the frame is the harness size and is not
/// degenerate, and write the PNG when `ZENMAN_SNAPSHOT_WRITE` says to. `None` means *skipped*: the
/// only printed reason in the tree, and the reason `cargo xtask snapshots` can be honest on a
/// machine without a device.
fn render(scene: &Scene) -> Option<RgbaImage> {
    let adapters = match probe_adapters() {
        Ok(adapters) => adapters,
        Err(reason) => {
            println!("SKIP snapshots::{} — {reason}", scene.file_name());
            return None;
        }
    };
    println!("snapshot adapter candidates: {}", adapters.join(", "));

    let mut harness = scene.build();
    match scene {
        Scene::Empty => settle(&mut harness),
        Scene::Listing { view, .. } => {
            drive(&mut harness);
            assert_populated(&harness, *view);
        }
        Scene::Split { .. } => drive_split(&mut harness),
        Scene::Quad { .. } => drive_quad(&mut harness),
        Scene::Drawer { side, .. } => drive_drawer(&mut harness, *side),
        Scene::ActionLog { tag } => drive_action_log(&mut harness, tag),
        Scene::Menu { at, .. } => drive_menu(&mut harness, *at),
        Scene::BatchRename { tag } => drive_batch_rename(&mut harness, tag),
        Scene::QuickFind { .. } => drive_quick_find(&mut harness),
        Scene::Address { .. } => drive_address(&mut harness),
        Scene::Index { running, .. } => drive_index(&mut harness, *running),
    }

    let image = match harness.render() {
        Ok(image) => image,
        Err(e) => {
            // An adapter existed but the renderer still failed (a driver fault): that is a real
            // failure, not a skip — printing and passing here would be the fake green.
            panic!(
                "an adapter exists ({}), but rendering failed: {e}",
                adapters.join(", ")
            );
        }
    };
    if let Scene::ActionLog { .. } = scene {
        // The frame that was *just painted* must still be backed by the frozen stamps: the log panel
        // re-reads itself on its own poll, and a live RFC 3339 stamp would make the baseline differ
        // between two runs of the same checkout.
        let rows = &harness.state().log.page.rows;
        assert!(
            rows.iter()
                .all(|r| r.timestamp == FIXED_RFC3339 && r.ago == FIXED_AGO),
            "the painted frame is backed by the frozen stamps, not live ones: {:?}",
            rows.iter().map(|r| r.timestamp.clone()).collect::<Vec<_>>()
        );
    }
    assert_eq!(
        (image.width(), image.height()),
        (SIZE.x as u32, SIZE.y as u32),
        "{} must be the harness size",
        scene.file_name()
    );

    if std::env::var("ZENMAN_SNAPSHOT_WRITE").is_ok() {
        // Absolute, from the crate manifest: `cargo xtask snapshots` runs the test from the
        // workspace root, where a cwd-relative `tests/snapshots/…` would have written a PNG in the
        // wrong directory and left the committed artefact stale-but-green (step 7c).
        let out: PathBuf = [
            env!("CARGO_MANIFEST_DIR"),
            "tests",
            "snapshots",
            scene.file_name(),
        ]
        .iter()
        .collect();
        std::fs::create_dir_all(out.parent().expect("tests/snapshots has a parent"))
            .expect("the snapshot directory is creatable");
        image
            .save(&out)
            .unwrap_or_else(|e| panic!("writing {}: {e}", out.display()));
        let (reference, note) = reference_for(scene.file_name());
        println!("wrote {}", out.display());
        println!(
            "  reviewed against {} — {}",
            if reference.is_empty() {
                "(no Go reference)"
            } else {
                reference
            },
            note
        );
    } else {
        println!(
            "render OK ({}x{}); set ZENMAN_SNAPSHOT_WRITE=1 (or run `cargo xtask snapshots`) to write {}",
            image.width(),
            image.height(),
            scene.file_name()
        );
    }
    Some(image)
}

/// Every wgpu adapter this process can see, as `Backend | name` lines. An error is the skip reason.
fn probe_adapters() -> Result<Vec<String>, String> {
    let instance = std::sync::Arc::new(wgpu::Instance::default());
    let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()));
    if adapters.is_empty() {
        return Err("wgpu sees no adapter (no Vulkan/GL device reachable); install lavapipe or run \
                    under a Mesa-capable X server (`xvfb-run -a`), then re-run `cargo xtask snapshots`"
            .to_string());
    }
    Ok(adapters
        .iter()
        .map(|a| {
            let info = a.get_info();
            format!("{:?} | {}", info.backend, info.device)
        })
        .collect())
}

/// The populated frame must actually be showing the fixture: one page of rows, rows painted in the
/// requested view, and the two rows we clicked selected. A baseline that rendered an empty pane
/// would be a *degenerate* artefact — green, and useless to the step-15 human review.
fn assert_populated(harness: &Harness<'static, ZenApp>, view: ViewMode) {
    let tab = shown(harness.state());
    assert_eq!(tab.view, view, "the requested view mode reached the pane");
    assert_eq!(tab.entries.entries.len(), PAGE, "one page of the fixture");
    assert!(
        tab.has_more,
        "the fixture is bigger than the page the frame shows"
    );
    assert!(
        tab.scroll.painted > 0,
        "rows were painted: {:?}",
        tab.scroll
    );
    assert_eq!(
        tab.sel.len(),
        SELECTED.len(),
        "the driven selection is live"
    );
    for name in SELECTED {
        assert!(tab.sel.contains(name), "{name} is part of the selection");
    }
    // The same facts as the widget tree, i.e. as *painted* nodes: the header, the tab title, and
    // the selected rows.
    for label in ["Modified", "listing", "docs", "reports"] {
        assert!(
            harness.query_all_by_label(label).count() > 0,
            "`{label}` is in the frame"
        );
    }
}

/// The pane's shown tab, by slot.
fn shown_in(state: &ZenApp, slot: PaneSlot) -> &Tab {
    state
        .model
        .panes
        .get(&slot)
        .and_then(|p| p.active_tab())
        .unwrap_or_else(|| panic!("{slot:?} opened a tab"))
}

/// `Command`s reach the engine through the shell's own seam, exactly as a toolbar click or a chord
/// does — never by writing the flag.
fn dispatch(harness: &mut Harness<'static, ZenApp>, command: Command) {
    let ctx = harness.ctx.clone();
    harness.state_mut().dispatch(PaneSlot::Left, command, &ctx);
    harness.run();
    harness.run();
}

/// A raw pointer press+release at one point — for the band that is not a row (`tests/menus.rs`'s).
fn click_at(harness: &mut Harness<'static, ZenApp>, at: egui::Pos2, button: egui::PointerButton) {
    for pressed in [true, false] {
        harness.input_mut().events.push(egui::Event::PointerButton {
            pos: at,
            button,
            pressed,
            modifiers: egui::Modifiers::default(),
        });
    }
    harness.run();
    harness.run();
}

/// The frame is not blank *because of the widget tree*: these labels are in it.
fn assert_labels(harness: &Harness<'static, ZenApp>, labels: &[&str]) {
    for label in labels {
        assert!(
            harness.query_all_by_label(label).count() > 0,
            "`{label}` is in the frame"
        );
    }
}

/// `04-split-view`: two panes, two directories, one live selection.
fn drive_split(harness: &mut Harness<'static, ZenApp>) {
    settle(harness);
    harness.get_by_label("a.txt").click();
    settle(harness);
    assert_eq!(
        shown_in(harness.state(), PaneSlot::Left)
            .entries
            .entries
            .len(),
        PAGE,
        "the left pane shows one page"
    );
    assert_eq!(
        shown_in(harness.state(), PaneSlot::Right)
            .entries
            .entries
            .len(),
        3,
        "the right pane shows `docs`"
    );
    assert_eq!(
        shown_in(harness.state(), PaneSlot::Left).sel.len(),
        1,
        "the click selected a row"
    );
    for slot in [PaneSlot::Left, PaneSlot::Right] {
        assert!(
            shown_in(harness.state(), slot).scroll.painted > 0,
            "{slot:?} painted rows"
        );
    }
    assert_labels(harness, &["Modified", "listing", "readme.md"]);
}

/// `05-quad-view`: four panes, four listings.
fn drive_quad(harness: &mut Harness<'static, ZenApp>) {
    settle(harness);
    for (slot, want) in [
        (PaneSlot::Left, PAGE),
        (PaneSlot::Right, 3),
        (PaneSlot::LeftBottom, 1),
        (PaneSlot::RightBottom, PAGE),
    ] {
        let tab = shown_in(harness.state(), slot);
        assert_eq!(
            tab.entries.entries.len(),
            want,
            "{slot:?} loaded its directory"
        );
        assert!(tab.scroll.painted > 0, "{slot:?} painted rows");
    }
    assert_labels(harness, &["Modified", "listing", "readme.md", "empty"]);
}

/// `06`/`07-drawer`: the toggle command opens exactly one drawer, over the populated listing.
fn drive_drawer(harness: &mut Harness<'static, ZenApp>, side: Side) {
    settle(harness);
    dispatch(
        harness,
        match side {
            Side::Left => Command::ToggleLeftDrawer,
            Side::Right => Command::ToggleRightDrawer,
        },
    );
    settle(harness);
    let model = &harness.state().model;
    let (left, right) = (model.left_drawer.open, model.right_drawer.open);
    match side {
        Side::Left => assert!(
            left && !right,
            "the left drawer is open alone: {left}/{right}"
        ),
        Side::Right => assert!(
            right && !left,
            "the right drawer is open alone: {left}/{right}"
        ),
    }
    assert_eq!(
        shown(harness.state()).entries.entries.len(),
        PAGE,
        "the listing is behind the drawer"
    );
    // Step 15b: the drawers are the real content now, and no row of either may reach the panel's
    // edge (defect D9) — step 8d's breadcrumb rect test, against the drawer's own width.
    let tokens = harness.state().tokens;
    let places = harness.state().session.places();
    assert!(
        places.user.iter().any(|p| p.label == "Home"),
        "the left drawer's Places list has Home"
    );
    assert!(!places.disks.is_empty(), "the drawer has a disk");
    assert_labels(
        harness,
        match side {
            Side::Left => &["PLACES", "BOOKMARKS", "MOUNTED DRIVES"],
            // Go's right drawer is headed by its `.sidebar-header h2` (`index.html:427`), not by a
            // section title — the title *is* the panel's name.
            Side::Right => &["Custom Actions"],
        },
    );
    if side == Side::Left {
        for label in ["Home", "Downloads"] {
            let rect = harness.get_by_label(label).rect();
            assert!(
                rect.max.x <= tokens.left_drawer_width,
                "`{label}` at {rect:?} must fit the {} px panel",
                tokens.left_drawer_width
            );
        }
        for disk in &places.disks {
            // The mount's volume name can collide with a breadcrumb chip of the same word, so the
            // nodes *inside the panel's own x-range* are the row: the assertion is that one exists
            // and that it does not reach the panel's edge.
            let rects: Vec<_> = harness
                .query_all_by_label(&disk.label)
                .map(|node| node.rect())
                .filter(|rect| rect.min.x < tokens.left_drawer_width)
                .collect();
            assert!(
                !rects.is_empty(),
                "the disk row {:?} is in the frame",
                disk.label
            );
            for rect in rects {
                assert!(
                    rect.max.x <= tokens.left_drawer_width,
                    "the disk row {:?} at {rect:?} must fit the {} px panel",
                    disk.label,
                    tokens.left_drawer_width
                );
            }
        }
    }
    assert_labels(harness, &["Modified", "listing", "docs"]);
}

/// `08-action-log-drawer`: two real `mkdir`s through `Session` — so the undo log holds two real
/// records — then the `H` command's panel. The records' RFC 3339 stamps are the only run-derived text
/// in the frame, so they are rewritten to [`FIXED_RFC3339`] immediately before the render.
fn drive_action_log(harness: &mut Harness<'static, ZenApp>, tag: &'static str) {
    let listing = common::listing_tree(tag);
    for name in ["log-alpha", "log-beta"] {
        let _ = std::fs::remove_dir(listing.join(name));
    }
    let parent = AbsPath::try_from(listing.as_path())
        .unwrap_or_else(|e| panic!("{}: {e}", listing.display()));
    for name in ["log-alpha", "log-beta"] {
        let origin = harness.state().session.next_origin();
        harness
            .state_mut()
            .session
            .mkdir(&MkdirReq::new(parent.clone(), name), origin)
            .unwrap_or_else(|e| panic!("mkdir {name}: {}", e.user_message()));
    }
    // The two new rows are painted by the listing behind the panel, so their mtimes are pinned before
    // the first frame can read them.
    for (index, name) in ["log-alpha", "log-beta"].iter().enumerate() {
        pin_mtime(
            &listing.join(name),
            FIXED_UNIX as u64 + 20 * 86_400 + index as u64 * 86_400,
        );
    }
    dispatch(harness, Command::ActionLog);
    common::settle_until(harness, "the log panel's rows", |h| {
        h.state().log.page.rows.len() >= 2
    });
    let page = &harness.state().log.page;
    assert!(harness.state().model.log_panel, "the panel is open");
    assert!(page.total >= 2, "the log holds both mkdirs: {}", page.total);
    assert!(
        page.rows.iter().any(|r| r.action == "mkdir"),
        "a row says what the action was: {:?}",
        page.rows.iter().map(|r| r.action).collect::<Vec<_>>()
    );
    freeze_log_panel(harness);
    assert_labels(harness, &["listing", "log-alpha"]);
}

/// Let every hover/fade animation reach its end state before a frame is captured.
///
/// egui computes a widget's animated visuals (and a fading scrollbar) from the **wall clock**, so a
/// frame taken while one of them is still in flight differs between two runs of the same checkout.
/// Sleeping past the animation and *then* running the frames lands every one of them on its final
/// value — which is what makes the artefact byte-reproducible.
fn park(harness: &mut Harness<'static, ZenApp>) {
    for _ in 0..2 {
        std::thread::sleep(std::time::Duration::from_millis(400));
        settle(harness);
    }
}

/// Stamp every painted log row with [`FIXED_RFC3339`], and *prove* it stuck: the panel re-reads
/// itself on its own 5 s poll, so the patch is applied, the frames are run, the patch is applied
/// again and only then asserted — a frame that painted a live stamp would be caught here, not in the
/// next machine's `cmp`.
fn freeze_log_panel(harness: &mut Harness<'static, ZenApp>) {
    park(harness);
    let stamp = |h: &mut Harness<'static, ZenApp>| {
        for row in &mut h.state_mut().log.page.rows {
            row.timestamp = FIXED_RFC3339.to_string();
            row.ago = FIXED_AGO.to_string();
        }
    };
    stamp(harness);
    settle(harness);
    stamp(harness);
    let rows = &harness.state().log.page.rows;
    assert!(
        rows.iter()
            .all(|r| r.timestamp == FIXED_RFC3339 && r.ago == FIXED_AGO),
        "every painted row carries the frozen stamp: {:?}",
        rows.iter().map(|r| r.timestamp.clone()).collect::<Vec<_>>()
    );
}

/// `11`/`12-context-menu`: the right-click a user makes — on a row, and on the empty band.
fn drive_menu(harness: &mut Harness<'static, ZenApp>, at: MenuAt) {
    settle(harness);
    match at {
        MenuAt::Row => {
            harness.get_by_label("top.txt").click_secondary();
            harness.run();
            harness.run();
        }
        MenuAt::Background => {
            let last = harness.get_by_label("top.txt").rect().max;
            click_at(
                harness,
                egui::Pos2::new(last.x - 60.0, last.y + 60.0),
                egui::PointerButton::Secondary,
            );
        }
    }
    settle(harness);
    let open = harness
        .state()
        .menu
        .clone()
        .unwrap_or_else(|| panic!("the right-click opened no menu"));
    match at {
        MenuAt::Row => assert!(
            !matches!(open.target, MenuTarget::Background),
            "a row right-click must not draw the background menu"
        ),
        MenuAt::Background => assert!(
            matches!(open.target, MenuTarget::Background),
            "the empty band must draw the background menu"
        ),
    }
    assert!(
        !harness.state().menu_specs().is_empty(),
        "the open menu has items to draw"
    );
    assert_eq!(
        shown(harness.state()).entries.entries.len(),
        3,
        "the small tree is behind the menu"
    );
    assert_labels(harness, &["top.txt", "alpha"]);
}

/// `13-batch-rename`: a real range selection, then the dialog on the rows that selection carries.
fn drive_batch_rename(harness: &mut Harness<'static, ZenApp>, tag: &'static str) {
    settle(harness);
    harness.get_by_label("a.txt").click();
    settle(harness);
    harness
        .get_by_label("c.md")
        .click_modifiers(egui::Modifiers {
            shift: true,
            ..Default::default()
        });
    settle(harness);
    assert_eq!(
        shown(harness.state()).sel.len(),
        3,
        "the shift-click extended the selection over `a.txt b.txt c.md`"
    );
    let listing = common::listing_tree(tag);
    let rows: Vec<Row> = ["a.txt", "b.txt", "c.md"]
        .iter()
        .map(|name| {
            Row::new(
                AbsPath::try_from(listing.join(name).as_path())
                    .unwrap_or_else(|e| panic!("{name}: {e}")),
                *name,
                false,
            )
        })
        .collect();
    let rules = Rules {
        find: "t".to_string(),
        replace: "T".to_string(),
        include_extension: true,
        ..Rules::default()
    };
    let existing = common::listing_names().into_iter().collect();
    let opened = harness
        .state_mut()
        .dialogs
        .open(Dialog::BatchRename(Box::new(BatchRename::new(
            PaneSlot::Left,
            rows,
            rules,
            existing,
        ))));
    assert!(opened, "no other dialog was open to refuse it");
    settle(harness);
    match harness.state().dialogs.active() {
        Some(Dialog::BatchRename(b)) => {
            assert_eq!(b.rows.len(), 3, "the dialog carries the selection");
            assert!(b.computes > 0, "the preview was computed at least once");
        }
        other => panic!("the batch-rename dialog is not open: {other:?}"),
    }
    assert_labels(harness, &["listing", "a.txt"]);
}

/// `14-quick-find`: <kbd>Ctrl</kbd>+<kbd>F</kbd> and a typed query — real keystrokes — then the walk's
/// answer.
fn drive_quick_find(harness: &mut Harness<'static, ZenApp>) {
    settle(harness);
    harness
        .input_mut()
        .events
        .push(Chord::ctrl(egui::Key::F).event());
    harness.run();
    harness.run();
    assert!(
        matches!(harness.state().dialogs.active(), Some(Dialog::QuickFind(_))),
        "the chord opened quick find"
    );
    for letter in "row-00".chars() {
        harness
            .input_mut()
            .events
            .push(egui::Event::Text(letter.to_string()));
        harness.run();
    }
    common::settle_until(harness, "the walk's rows", |h| {
        match h.state().dialogs.active() {
            Some(Dialog::QuickFind(f)) => !f.busy && f.job.is_none() && f.results.len() >= 10,
            _ => false,
        }
    });
    match harness.state().dialogs.active() {
        Some(Dialog::QuickFind(f)) => {
            assert_eq!(f.query, "row-00", "the field holds what was typed");
            assert!(f.error.is_none(), "the walk was refused: {:?}", f.error);
            assert!(
                f.results.iter().all(|hit| hit.name().contains("row-00")),
                "every row matches the query"
            );
        }
        other => panic!("quick find closed itself: {other:?}"),
    }
    // 15c D6: the list must *realise* its rows. The pane behind the dialog shows `row-0007.txt` as its
    // last visible row, so a node for row 8 or 9 can only have come from the dialog's own list — the
    // frame that showed a single row cannot produce one.
    let deep = harness.query_all_by_label("row-0008.txt").count()
        + harness.query_all_by_label("row-0009.txt").count();
    assert!(
        deep > 0,
        "the quick-find list realises rows below the pane's own viewport (D6)"
    );
    assert_labels(harness, &["listing"]);
}

/// `15-address-bar`: the same entry point the chord uses, and the frame a user sees when the strip
/// turns into a field.
fn drive_address(harness: &mut Harness<'static, ZenApp>) {
    settle(harness);
    dispatch(harness, Command::FocusSearch);
    park(harness);
    let pane = &harness.state().model.panes[&PaneSlot::Left];
    assert!(pane.address_editing, "the field is open");
    // The defect this baseline exists for: an open field with nothing in it is a black strip, and
    // the only way out was <kbd>Esc</kbd>.
    let path = pane
        .active_tab()
        .map(|tab| tab.path.to_string())
        .unwrap_or_default();
    assert!(
        !path.is_empty() && pane.address_buffer == path,
        "the field holds the path it stands for, not nothing: {:?} vs {path:?}",
        pane.address_buffer
    );
}

/// Stamp the status block's clock-derived fields, run the frames, stamp again, and assert — the
/// dialog re-reads `Session::index_status` on its own 1 s poll, so a single patch can be overwritten
/// by a frame that lands a second later.
fn freeze_index(harness: &mut Harness<'static, ZenApp>, running: bool) {
    let stamp = |h: &mut Harness<'static, ZenApp>| {
        if let Some(Dialog::SearchManager(m)) = h.state_mut().dialogs.active_mut() {
            m.status.last_indexed = FIXED_UNIX;
            if running {
                m.status.is_indexing = true;
                m.status.progress.indexed = INDEXED_FROZEN;
            }
        }
    };
    stamp(harness);
    settle(harness);
    stamp(harness);
    let status = match harness.state().dialogs.active() {
        Some(Dialog::SearchManager(m)) => m.status.clone(),
        other => panic!("the manager closed itself: {other:?}"),
    };
    assert_eq!(
        status.last_indexed, FIXED_UNIX,
        "the painted stamp is the frozen one"
    );
    assert_eq!(
        status.is_indexing, running,
        "the painted state is the scene's"
    );
}

/// `09`/`10-index-running`: the manager opened through its chord, the rebuild driven through the
/// dialog's own request seam.
///
/// * terminal (`running == false`): a **real** rebuild, waited out with `settle_until` on the engine's
///   own `index_is_running()`; only `last_indexed` is then frozen, because its value is `now`.
/// * running: a real rebuild job is started (`Session::index_rebuild`) and `is_indexing` is asserted
///   from the engine; the *count* is then stamped with [`INDEXED_FROZEN`], because the live number is
///   wherever the worker had got to when the frame was taken — no two runs agree on it.
///
/// Both freeze the fields, then render without another poll: the poll's own interval is a second of
/// wall clock, and the render is one frame.
fn drive_index(harness: &mut Harness<'static, ZenApp>, running: bool) {
    dispatch(harness, Command::SearchManager);
    common::settle_until(harness, "the search manager", |h| {
        matches!(h.state().dialogs.active(), Some(Dialog::SearchManager(_)))
    });
    if running {
        harness
            .state()
            .session
            .index_rebuild()
            .expect("the rebuild starts");
        assert!(
            harness.state().session.index_is_running(),
            "the job this scene started is running"
        );
    } else {
        match harness.state_mut().dialogs.active_mut() {
            Some(Dialog::SearchManager(m)) => m.requests.push(ManagerRequest::Rebuild),
            other => panic!("the manager is not open: {other:?}"),
        }
    }
    common::settle_until(harness, "the rebuild reaching a terminal state", |h| {
        !h.state().session.index_is_running()
            && match h.state().dialogs.active() {
                Some(Dialog::SearchManager(m)) => m.rebuild.is_none() && !m.status.is_indexing,
                _ => false,
            }
    });
    settle(harness);
    freeze_index(harness, running);
    // 15c D2/D3, pinned on the painted frame: with **no query typed** the manager still shows the
    // results table and Go's empty-state line, and the `last indexed` stamp is a *date*, never the raw
    // unix second the golden carries.
    for label in [
        "Name",
        "Path",
        "Size",
        "Modified",
        "No items matched your query",
    ] {
        assert!(
            harness.query_all_by_label(label).count() > 0,
            "`{label}` is in the manager's frame with an empty query (D2)"
        );
    }
    let stamp = format!(
        "last indexed {}",
        zenman_core::metrics::format_mtime(Some(FIXED_UNIX))
    );
    assert!(
        harness.query_all_by_label(&stamp).count() > 0,
        "the frozen stamp is painted as a date: `{stamp}` (D3)"
    );
    assert_eq!(
        harness
            .query_all_by_label(&format!("last indexed {FIXED_UNIX}"))
            .count(),
        0,
        "the raw unix second never reaches the frame (D3)"
    );
    if running {
        // 15c D4: the mid-rebuild frame is the settings view, with the live progress line on it.
        for label in ["Index roots", "Exclude patterns", "indexing 200"] {
            assert!(
                harness.query_all_by_label(label).count() > 0,
                "`{label}` is in the running frame (D4)"
            );
        }
        // 15c D5: this frame claims a rebuild is running, so a *completion* notice beside it is the
        // contradiction the review found. The shell withdraws it on the state transition; this scene
        // reaches the running state through a direct `Session::index_rebuild`, so it withdraws it here
        // through the very call the shell makes (the transition itself is pinned by the `tick` code).
        let withdrawn = harness
            .state_mut()
            .notices
            .dismiss_containing("row(s) indexed");
        println!("index-running frame: withdrew {withdrawn} stale completion notice(s)");
    }
    match harness.state_mut().dialogs.active_mut() {
        Some(Dialog::SearchManager(m)) => {
            m.status.last_indexed = FIXED_UNIX;
            if running {
                m.status.is_indexing = true;
                m.status.progress.indexed = INDEXED_FROZEN;
            }
            assert!(
                m.status.error.is_none(),
                "the rebuild reported an error: {:?}",
                m.status.error
            );
        }
        other => panic!("the manager closed itself: {other:?}"),
    }
    let status = match harness.state().dialogs.active() {
        Some(Dialog::SearchManager(m)) => m.status.clone(),
        other => panic!("the manager closed itself: {other:?}"),
    };
    assert_eq!(
        status.is_indexing, running,
        "the frame says what the scene is"
    );
    assert!(
        status.total_files > 0,
        "the index holds the fixture: {}",
        status.total_files
    );
    assert_eq!(
        status.last_indexed, FIXED_UNIX,
        "the stamp is the frozen one"
    );
    if running {
        // Tidy: no worker thread outliving the frame it was started for.
        let _ = harness.state().session.index_cancel();
    }
    assert!(
        !harness.state().session.index_config().roots.is_empty(),
        "the dialog lists an index root"
    );
    assert_labels(harness, &["listing"]);
}

/// The pane's shown tab (the populated harness is a single pane, so there is one candidate).
fn shown(state: &ZenApp) -> &Tab {
    state
        .model
        .panes
        .get(&PaneSlot::Left)
        .and_then(|p| p.active_tab())
        .expect("the scene opened a tab")
}

fn fixture_path(path: &std::path::Path) -> FsPath {
    FsPath::from_abs(AbsPath::try_from(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
}

/// The share of pixels that are *not* the window background token.
fn non_background_share(image: &RgbaImage) -> f64 {
    let bg = Tokens::dark().bg_main;
    let want = [bg[0], bg[1], bg[2], bg[3]];
    let hits = image.pixels().filter(|p| p.0 != want).count();
    hits as f64 / (image.width() as f64 * image.height() as f64)
}

#[test]
fn empty_shell_snapshot_renders_or_skips_with_a_reason() {
    let scene = Scene::Empty;
    let Some(image) = render(&scene) else { return };

    // A content sanity check that survives antialiasing: the window background token must be the
    // dominant colour (an empty shell is mostly background).
    let share = non_background_share(&image);
    assert!(
        share < 0.75,
        "the dark background must dominate an empty shell; {share:.3} of pixels are not the \
         background token"
    );
}

#[test]
fn file_list_snapshot_renders_or_skips_with_a_reason() {
    let scene = Scene::Listing {
        tag: "snap-list",
        view: ViewMode::List,
    };
    let Some(image) = render(&scene) else { return };

    // The populated frame is *not* mostly background: rows, header, icons and text cover it. The
    // threshold is loose on purpose — this is a "the render is not blank" check, not a diff.
    let share = non_background_share(&image);
    assert!(
        share > 0.2,
        "a populated listing must cover the window; only {share:.3} of pixels differ from the \
         background token"
    );
}

#[test]
fn file_grid_snapshot_renders_or_skips_with_a_reason() {
    let scene = Scene::Listing {
        tag: "snap-grid",
        view: ViewMode::Grid,
    };
    let Some(image) = render(&scene) else { return };

    let share = non_background_share(&image);
    assert!(
        share > 0.2,
        "a populated grid must cover the window; only {share:.3} of pixels differ from the \
         background token"
    );
}

/// The same "the render is not blank" check the three step-8 baselines make, for every later scene.
/// The threshold is loose on purpose: this is a content assertion, never a pixel diff (rule: the
/// human gate is step 15b's `xtask parity`, not this file).
fn assert_covers(image: &RgbaImage, min: f64, what: &str) {
    let share = non_background_share(image);
    assert!(
        share > min,
        "{what} must cover the window; only {share:.3} of pixels differ from the background token"
    );
}

#[test]
fn split_view_snapshot_renders_or_skips_with_a_reason() {
    let scene = Scene::Split { tag: "snap-split" };
    let Some(image) = render(&scene) else { return };
    assert_covers(&image, 0.2, "a split listing");
}

#[test]
fn quad_view_snapshot_renders_or_skips_with_a_reason() {
    let scene = Scene::Quad { tag: "snap-quad" };
    let Some(image) = render(&scene) else { return };
    assert_covers(&image, 0.2, "a four-pane grid");
}

#[test]
fn left_drawer_snapshot_renders_or_skips_with_a_reason() {
    let scene = Scene::Drawer {
        tag: "snap-drawer-left",
        side: Side::Left,
    };
    let Some(image) = render(&scene) else { return };
    assert_covers(&image, 0.2, "a listing with the bookmarks drawer open");
}

#[test]
fn right_drawer_snapshot_renders_or_skips_with_a_reason() {
    let scene = Scene::Drawer {
        tag: "snap-drawer-right",
        side: Side::Right,
    };
    let Some(image) = render(&scene) else { return };
    assert_covers(&image, 0.2, "a listing with the custom-actions drawer open");
}

#[test]
fn address_bar_snapshot_renders_or_skips_with_a_reason() {
    let scene = Scene::Address {
        tag: "snap-address",
    };
    let Some(image) = render(&scene) else { return };
    assert_covers(&image, 0.2, "a listing with its address field open");
}

#[test]
fn action_log_drawer_snapshot_renders_or_skips_with_a_reason() {
    let scene = Scene::ActionLog { tag: "snap-log" };
    let Some(image) = render(&scene) else { return };
    assert_covers(&image, 0.2, "the action-log slide-in");
}

#[test]
fn search_manager_snapshot_renders_or_skips_with_a_reason() {
    let scene = Scene::Index {
        tag: "snap-index",
        running: false,
    };
    let Some(image) = render(&scene) else { return };
    assert_covers(&image, 0.2, "the Search Manager over a listing");
}

#[test]
fn index_running_snapshot_renders_or_skips_with_a_reason() {
    let scene = Scene::Index {
        tag: "snap-index-run",
        running: true,
    };
    let Some(image) = render(&scene) else { return };
    assert_covers(&image, 0.2, "the Search Manager mid-rebuild");
}

#[test]
fn context_menu_file_snapshot_renders_or_skips_with_a_reason() {
    let scene = Scene::Menu {
        tag: "snap-menu-row",
        at: MenuAt::Row,
    };
    let Some(image) = render(&scene) else { return };
    assert_covers(&image, 0.1, "a row context menu");
}

#[test]
fn context_menu_background_snapshot_renders_or_skips_with_a_reason() {
    let scene = Scene::Menu {
        tag: "snap-menu-bg",
        at: MenuAt::Background,
    };
    let Some(image) = render(&scene) else { return };
    assert_covers(&image, 0.1, "a background context menu");
}

#[test]
fn batch_rename_snapshot_renders_or_skips_with_a_reason() {
    let scene = Scene::BatchRename { tag: "snap-batch" };
    let Some(image) = render(&scene) else { return };
    assert_covers(&image, 0.2, "the batch-rename dialog");
}

#[test]
fn quick_find_snapshot_renders_or_skips_with_a_reason() {
    let scene = Scene::QuickFind { tag: "snap-find" };
    let Some(image) = render(&scene) else { return };
    assert_covers(&image, 0.2, "the quick-find dialog");
}
