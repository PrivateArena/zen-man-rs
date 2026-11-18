//! `xtask snapshots`'s engine: the one place that renders `01-empty.png` and the one place that
//! decides — honestly, with a printed reason — that it cannot.
//!
//! Plan §7 step 7: "writes `crates/zenman-ui/tests/snapshots/01-empty.png` from the software/GL
//! rasteriser (skips with a printed reason when no adapter exists)". Two facts learned the hard
//! way (plan §9.2 step-7 row):
//!
//! * `egui_kittest`'s default renderer is *lazy* but not *soft* — the first render call with no
//!   adapter panics inside `expect("Failed to create render state")`. So the adapter probe comes
//!   first, in our own code, and the skip path is real IO-free output rather than a caught panic.
//! * The plan assumed `harness.snapshot(name)`; that function compares against a committed
//!   baseline and fails when it differs. Pixel equality is step 15's *human* gate, so this file
//!   renders and writes (`ZENMAN_SNAPSHOT_WRITE=1`, set by `cargo xtask snapshots`), and in plain
//!   `cargo test` mode only proves the frame renders, has the expected size, and contains the
//!   token background colour — no pixel diff, no fake green.

use std::path::PathBuf;

use egui_kittest::Harness;

use zenman_core::store::config::ThemeMode;
use zenman_ui::app::ZenApp;
use zenman_ui::theme::Tokens;

const SIZE: egui::Vec2 = egui::vec2(1440.0, 900.0);

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

fn shell_harness() -> Harness<'static, ZenApp> {
    let dir = std::env::temp_dir().join(format!("zenman-ui-snap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let paths = zenman_core::session::SessionPaths::new(
        dir.join("config.json"),
        dir.join("action.log"),
        None,
        dir.join("probe"),
    )
    .with_db_open(1, std::time::Duration::from_millis(1));
    let (session, _notes) = zenman_core::session::Session::open(
        paths,
        zenman_core::store::config::ZenConfig::default(),
        zenman_core::platform::default_env(),
        zenman_core::store::config::ZenConfig::default().limits(),
    )
    .expect("session over a fresh directory");
    // The two-pane Split grid: the shell's canonical empty state (the Go app's default view).
    let mut app = ZenApp::new(session, vec!["snapshot: two empty panes".into()]);
    app.set_layout(zenman_ui::state::panes_for(
        zenman_core::store::config::LayoutMode::Split,
    ));
    Harness::builder()
        .with_size(SIZE)
        .with_context_setup(|ctx| {
            egui_extras::install_image_loaders(ctx);
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

#[test]
fn empty_shell_snapshot_renders_or_skips_with_a_reason() {
    let adapters = match probe_adapters() {
        Ok(adapters) => adapters,
        Err(reason) => {
            println!("SKIP snapshots::01-empty — {reason}");
            return;
        }
    };
    println!("snapshot adapter candidates: {}", adapters.join(", "));

    let mut harness = shell_harness();
    harness.run();
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
        "the snapshot must be the harness size"
    );

    // A content sanity check that survives antialiasing: the window background token must be the
    // dominant colour (an empty shell is mostly background).
    let tokens = Tokens::dark();
    let bg = tokens.bg_main;
    let want = [bg[0], bg[1], bg[2], bg[3]];
    let hits = image.pixels().filter(|p| p.0 == want).count();
    let share = hits as f64 / (image.width() as f64 * image.height() as f64);
    assert!(
        share > 0.25,
        "the dark background must dominate an empty shell; only {share:.3} of pixels are {want:?}"
    );

    if std::env::var("ZENMAN_SNAPSHOT_WRITE").is_ok() {
        let out: PathBuf = ["tests", "snapshots", "01-empty.png"].iter().collect();
        std::fs::create_dir_all(out.parent().expect("tests/snapshots has a parent"))
            .expect("the snapshot directory is creatable");
        image
            .save(&out)
            .unwrap_or_else(|e| panic!("writing {}: {e}", out.display()));
        println!(
            "wrote {}",
            std::path::absolute(&out).unwrap_or(out).display()
        );
    } else {
        println!(
            "render OK ({}x{}); set ZENMAN_SNAPSHOT_WRITE=1 (or run `cargo xtask snapshots`) to write the PNG",
            image.width(),
            image.height()
        );
    }
}
