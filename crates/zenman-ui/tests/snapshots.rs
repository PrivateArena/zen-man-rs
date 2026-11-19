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

use zenman_core::path::{AbsPath, FsPath};
use zenman_core::store::config::{LayoutMode, PaneSlot, ThemeMode, ViewMode};
use zenman_ui::app::ZenApp;
use zenman_ui::panels::file_pane::PaneCommand;
use zenman_ui::state::{Tab, panes_for};
use zenman_ui::theme::Tokens;

const SIZE: egui::Vec2 = egui::vec2(1440.0, 900.0);

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
        }
    }

    /// The session tag this scene needs its own temp directory under.
    const fn tag(&self) -> &'static str {
        match self {
            Self::Empty => "snap-empty",
            Self::Listing { tag, .. } => tag,
        }
    }

    /// Build the shell for this scene: a real `Session` over a private temp data directory, the
    /// layout, and — for the populated scenes — a real tab over the fixture listing at the wanted
    /// view mode. The context setup (image loaders *then* the font stack *then* the theme) is the
    /// requirement of rule 11: this egui fork panics on a missing glyph, and `egui_extras` image
    /// painting panics without the loaders.
    fn build(&self) -> Harness<'static, ZenApp> {
        let session = common::session(self.tag());
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
        }
        Harness::builder()
            // The header's workspace `ComboBox` (step 14b) surfaces `current_text_value` but no
            // accessible name in this fork — the same omission the fork's own `TextEdit` has.
            .with_accessibility_check(false)
            .with_size(SIZE)
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
        println!("wrote {}", out.display());
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
