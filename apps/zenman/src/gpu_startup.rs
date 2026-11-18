//! GPU-safe boot (plan §5.1/§5.9 `apps/zenman/src/gpu_startup.rs` row, failure mode **F18**, open
//! question **OQ3**).
//!
//! zen-man has no rendering to lose (plan §5.9), so the ladder is deliberately boring and it is
//! resolved *before* a window or a `wgpu::Device` exists:
//!
//! 1. **sentinel** — a pending marker (`starting.toml`) is written before the driver is touched and
//!    cleared only after a run finished cleanly, so a driver that dies mid-`run_native` is detected
//!    by the *next* start (an in-process retry cannot catch a segfault),
//! 2. **probe** — [`probe_first_available`] asks `wgpu` which adapters it can actually see for each
//!    profile's backend mask, and picks the first profile with one,
//! 3. **bless** — the profile that survived a whole run is recorded in `data_dir/gpu/last-good.toml`
//!    and tried first next time,
//! 4. then (and only then) `craft`-style `eframe::run_native` (see `main.rs`).
//!
//! `--print-adapters` ([`probe_print_and_exit`]) is the diagnostic that answers OQ3 on any machine:
//! it prints `backend | device_type | name` for every adapter `wgpu` sees and exits without opening
//! a window or creating a device.
//!
//! Framework APIs this file relies on, each confirmed against the pinned sources:
//! * `wgpu::Instance::enabled_backend_features()` — `wgpu-30.0.2/src/api/instance.rs:108`
//! * `wgpu::Instance::new(InstanceDescriptor)` — `wgpu-30.0.2/src/api/instance.rs:63`
//! * `Instance::enumerate_adapters(Backends) -> impl Future<Output = Vec<Adapter>>` — same file:145
//!   (hence the tiny [`block_on`], the same thread-parking shape photocraft uses at
//!   `apps/photocraft/src/services.rs:336`)
//! * `wgpu::Adapter::get_info() -> AdapterInfo{name, device_type, backend, ..}` —
//!   `wgpu-30.0.2/src/api/adapter.rs:169`, `wgpu-types-30.0.2/src/adapter.rs:145`
//! * `egui_wgpu::WgpuSetup::CreateNew(WgpuSetupCreateNew { instance_descriptor, power_preference, .. })`
//!   — `egui-wgpu/src/setup.rs:43,58,172` (reached through `eframe`'s re-export, as photocraft does at
//!   `apps/photocraft/src/gpu_startup.rs:23`)

use std::future::Future;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::task::Wake;

use eframe::{egui_wgpu, wgpu};

/// Name of the blessed-profile record inside `data_dir/gpu/` (plan §5.9 `Sentinel` row).
pub const LAST_GOOD_FILE: &str = "last-good.toml";
/// Name of the pending marker inside `data_dir/gpu/`: present at start, removed by [`Sentinel::bless`].
pub const MARKER_FILE: &str = "starting.toml";
/// Environment override that restricts the ladder to one backend. A diagnostic and the
/// `tests/gpu_fallback.rs` case-(b) hook; `WGPU_BACKEND` still overrides `wgpu` itself and wins.
pub const ENV_BACKEND: &str = "ZENMAN_GPU_BACKEND";

/// How a profile is expected to render. Recorded, never used to decide anything on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuKind {
    /// A hardware backend (Vulkan / Metal / DX12) or a hardware GL driver.
    Gpu,
    /// Last resort: whatever `wgpu` picks, including a software rasteriser (llvmpipe, WARP, swiftshader).
    Software,
    /// The adapter type could not be classified (an unknown backend, or no adapter at all).
    Unknown,
}

impl GpuKind {
    /// Stable short name for reports and for `last-good.toml`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gpu => "gpu",
            Self::Software => "software",
            Self::Unknown => "unknown",
        }
    }
}

/// One candidate `wgpu` configuration. Field names follow plan §5.1's `WgpuSetup` row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgpuSetup {
    /// Which `wgpu` backends the instance may use; the adapter picker chooses among them.
    pub backend_mask: wgpu::Backends,
    /// Passed to `wgpu` when it picks the adapter (ignored when a selector is installed).
    pub power_preference: wgpu::PowerPreference,
    /// True when this profile deliberately targets legacy/software GL (the `--gl` and software rungs).
    pub gl_for_legacy: bool,
}

impl WgpuSetup {
    /// A hardware-first setup: the native backends `wgpu` was built for, minus GL.
    pub fn hardware() -> Self {
        Self {
            backend_mask: wgpu::Backends::VULKAN | wgpu::Backends::METAL | wgpu::Backends::DX12,
            power_preference: wgpu::PowerPreference::HighPerformance,
            gl_for_legacy: false,
        }
    }

    /// The legacy-GL rung: `llvmpipe`/`swrast`/old drivers, and the profile `--gl` selects.
    pub fn legacy_gl() -> Self {
        Self {
            backend_mask: wgpu::Backends::GL,
            power_preference: wgpu::PowerPreference::LowPower,
            gl_for_legacy: true,
        }
    }

    /// The last-resort rung: every backend `wgpu` was compiled with, low power preference, so a
    /// software adapter is acceptable when nothing else exists.
    pub fn last_resort() -> Self {
        Self {
            backend_mask: wgpu::Backends::all(),
            power_preference: wgpu::PowerPreference::LowPower,
            gl_for_legacy: true,
        }
    }

    /// A rung restricted to one named backend (see [`ENV_BACKEND`]); `None` for an unknown name.
    pub fn single_backend(name: &str) -> Option<Self> {
        let mask = match name.trim().to_ascii_lowercase().as_str() {
            "vulkan" => wgpu::Backends::VULKAN,
            "gl" | "opengl" | "glx" | "egl" => wgpu::Backends::GL,
            "dx12" | "d3d12" => wgpu::Backends::DX12,
            "metal" => wgpu::Backends::METAL,
            "noop" => wgpu::Backends::NOOP,
            other => {
                let _ = other;
                return None;
            }
        };
        Some(Self {
            backend_mask: mask,
            power_preference: wgpu::PowerPreference::LowPower,
            gl_for_legacy: mask == wgpu::Backends::GL,
        })
    }

    /// Copy this setup into `egui_wgpu`'s own configuration, which is what `eframe` uses to create
    /// the instance and pick the adapter. An `Existing` setup is left alone: a caller that injected
    /// its own device outranks our ladder, and overwriting it would be a lie.
    pub fn apply(&self, setup: &mut egui_wgpu::WgpuSetup) {
        if let egui_wgpu::WgpuSetup::CreateNew(create) = setup {
            create.instance_descriptor.backends = self.backend_mask;
            create.power_preference = self.power_preference;
        }
    }
}

/// A named rung of the fallback ladder.
#[derive(Clone, Debug)]
pub struct GpuProfile {
    /// Stable identifier, recorded in `last-good.toml` and printed in the boot report.
    pub name: &'static str,
    /// What this rung is expected to render on.
    pub kind: GpuKind,
    /// The `wgpu` configuration it selects.
    pub setup: WgpuSetup,
}

/// The bare ladder, plan §5.9: `Vulkan/Metal/DX12 → GL → software`.
pub fn fallback_profiles() -> [WgpuSetup; 3] {
    [
        WgpuSetup::hardware(),
        WgpuSetup::legacy_gl(),
        WgpuSetup::last_resort(),
    ]
}

/// The bare ladder with names and kinds.
pub fn fallback_ladder() -> Vec<GpuProfile> {
    let names = [
        ("gpu", GpuKind::Gpu),
        ("gl", GpuKind::Gpu),
        ("software", GpuKind::Software),
    ];
    fallback_profiles()
        .into_iter()
        .zip(names)
        .map(|(setup, (name, kind))| GpuProfile { name, kind, setup })
        .collect()
}

/// The ladder this start will try, from the CLI flags and [`ENV_BACKEND`].
///
/// `--safe-gpu` starts one rung down (the plan's "the next start drops a backend"); `--gl` and
/// `--disable-gpu` collapse the ladder to a single rung, which is what makes a failure reproducible;
/// `ZENMAN_GPU_BACKEND=<backend>` narrows every rung to that backend.
pub fn select_ladder(safe_gpu: bool, gl_only: bool, disable_gpu: bool) -> Vec<GpuProfile> {
    let mut ladder = if let Some(name) = std::env::var(ENV_BACKEND)
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        match WgpuSetup::single_backend(&name) {
            Some(setup) => vec![GpuProfile {
                name: "forced",
                kind: GpuKind::Unknown,
                setup,
            }],
            None => {
                eprintln!(
                    "zenman: ignoring unknown {ENV_BACKEND} value (expected vulkan|gl|dx12|metal|noop)"
                );
                fallback_ladder()
            }
        }
    } else if gl_only {
        vec![GpuProfile {
            name: "gl",
            kind: GpuKind::Gpu,
            setup: WgpuSetup::legacy_gl(),
        }]
    } else if disable_gpu {
        vec![GpuProfile {
            name: "software",
            kind: GpuKind::Software,
            setup: WgpuSetup::last_resort(),
        }]
    } else {
        fallback_ladder()
    };
    if safe_gpu && ladder.len() > 1 {
        ladder.remove(0);
    }
    ladder
}

/// One adapter, reduced to the three columns `--print-adapters` prints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterReport {
    /// `wgpu::Backend` name, e.g. `Vulkan`, `Gl`.
    pub backend: String,
    /// `wgpu::DeviceType` name, e.g. `DiscreteGpu`, `Cpu`.
    pub device_type: String,
    /// The driver's own device description.
    pub name: String,
}

/// Ask `wgpu` which adapters exist for `mask`. Creates an `Instance` only — no surface, no device,
/// so a driver that dies while creating a device is not provoked here.
pub fn enumerate(mask: wgpu::Backends) -> Vec<AdapterReport> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: mask,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    block_on(instance.enumerate_adapters(mask))
        .iter()
        .map(|adapter| {
            let info = adapter.get_info();
            AdapterReport {
                backend: format!("{:?}", info.backend),
                device_type: format!("{:?}", info.device_type),
                name: info.name,
            }
        })
        .collect()
}

/// The result of walking the ladder before the window exists.
#[derive(Clone, Debug)]
pub enum Probe {
    /// Rung `index` of the probed ladder has at least one adapter; `adapters` are its candidates.
    Found {
        index: usize,
        adapters: Vec<AdapterReport>,
    },
    /// No rung had a single adapter. The per-rung counts are kept so the message is actionable.
    Empty { tried: Vec<(&'static str, usize)> },
}

/// First rung with a visible adapter, in ladder order (plan §5.1 "probe").
pub fn probe_first_available(ladder: &[GpuProfile]) -> Probe {
    let mut tried = Vec::with_capacity(ladder.len());
    for (index, profile) in ladder.iter().enumerate() {
        let adapters = enumerate(profile.setup.backend_mask);
        if !adapters.is_empty() {
            return Probe::Found { index, adapters };
        }
        tried.push((profile.name, adapters.len()));
    }
    Probe::Empty { tried }
}

/// `--print-adapters`: enumerate everything this build of `wgpu` can see and exit. Answers OQ3.
pub fn probe_print_and_exit() -> ExitCode {
    let mask = wgpu::Instance::enabled_backend_features();
    println!("wgpu enabled backends for this build: {mask:?}");
    println!("backend | device_type | name");
    let adapters = enumerate(mask);
    for adapter in &adapters {
        println!(
            "{} | {} | {}",
            adapter.backend, adapter.device_type, adapter.name
        );
    }
    println!("{} adapter(s)", adapters.len());
    if adapters.is_empty() {
        println!(
            "no adapter: this build has no usable rasteriser here (OQ3) — snapshot tests must skip"
        );
    }
    ExitCode::SUCCESS
}

/// `data_dir/gpu/{last-good.toml,starting.toml}` (plan §5.9 `Sentinel` row).
#[derive(Clone, Debug)]
pub struct Sentinel {
    /// The blessed record, `data_dir/gpu/last-good.toml`.
    path: PathBuf,
    /// The pending marker written before the driver is touched.
    marker: PathBuf,
}

/// What `last-good.toml` recorded about the last run that finished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blessed {
    /// Profile name, e.g. `gpu`.
    pub profile: String,
    /// `wgpu::Backend` name of the adapter that worked, e.g. `Vulkan`.
    pub backend: String,
    /// Adapter name, for the human reading the file.
    pub adapter: String,
}

impl Sentinel {
    /// Sentinels for `dir` (= [`super::app_dirs::AppPaths::gpu_dir`]).
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join(LAST_GOOD_FILE),
            marker: dir.join(MARKER_FILE),
        }
    }

    /// Path of the blessed record (plan §5.1: `data_dir/gpu/last-good.toml`).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Path of the pending marker.
    pub fn marker_path(&self) -> &Path {
        &self.marker
    }

    /// Write the pending marker *before* the driver is touched, so a crash inside it is visible next
    /// time. A failure to write is reported by the caller as a warning, never a reason not to boot.
    pub fn create(&self, profile: &GpuProfile, adapter: &AdapterReport) -> std::io::Result<()> {
        write_atomically(
            &self.marker,
            &record(
                "pending",
                profile.name,
                adapter,
                "this file is removed once a run completes cleanly",
            ),
        )
    }

    /// Record `profile` as the one that survived a whole run and clear the marker.
    pub fn bless(&self, profile: &GpuProfile, adapter: &AdapterReport) -> std::io::Result<()> {
        write_atomically(
            &self.path,
            &record(
                "blessed",
                profile.name,
                adapter,
                "delete this file to forget the saved GPU choice",
            ),
        )?;
        match std::fs::remove_file(&self.marker) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// True when a previous start left its marker behind: that run never blessed itself, so this
    /// start drops the top rung of the ladder (plan §5.9: "keep the marker so the next start drops
    /// a backend").
    pub fn exists(&self) -> bool {
        self.marker.exists()
    }

    /// Read the blessed record, tolerating a file this version does not understand ([`None`]).
    pub fn read_blessed(&self) -> Option<Blessed> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        let mut profile = String::new();
        let mut backend = String::new();
        let mut adapter = String::new();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim().trim_matches('"').to_string();
            match key.trim() {
                "profile" => profile = value,
                "backend" => backend = value,
                "adapter" => adapter = value,
                _ => {}
            }
        }
        (!profile.is_empty()).then_some(Blessed {
            profile,
            backend,
            adapter,
        })
    }
}

fn record(kind: &str, profile: &str, adapter: &AdapterReport, comment: &str) -> String {
    format!(
        "# zenman GPU {kind} record — written by apps/zenman/src/gpu_startup.rs.\n# {comment}\nprofile = \"{profile}\"\nbackend = \"{}\"\nname = \"{}\"\n",
        adapter.backend, adapter.name
    )
}

/// Write `text` through a sibling temp file, so a crash cannot leave a half-written sentinel.
/// (`craft-atomic::atomic_write` is the plan's tool for this and is a step-4 dependency; until it is
/// wired up, this local equivalent keeps the same guarantee and is deleted when it lands.)
fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(text.as_bytes())?;
    file.flush()?;
    std::fs::rename(&tmp, path)
}

/// Run one `wgpu` future on this thread: `enumerate_adapters` is `async` but completes without I/O on
/// the native backends, so parking the calling thread is enough (same shape as photocraft's
/// `services::block_on`, `apps/photocraft/src/services.rs:336`).
fn block_on<F: Future>(future: F) -> F::Output {
    struct Unpark(std::thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = std::task::Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = std::task::Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let std::task::Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ladder_goes_hardware_then_gl_then_software() {
        let ladder = fallback_ladder();
        assert_eq!(ladder.len(), 3);
        assert_eq!(ladder[0].name, "gpu");
        assert!(!ladder[0].setup.gl_for_legacy);
        assert!(ladder[1].setup.gl_for_legacy);
        assert_eq!(ladder[2].setup.backend_mask, wgpu::Backends::all());
    }

    #[test]
    fn the_flags_narrow_the_ladder() {
        assert_eq!(select_ladder(false, false, false).len(), 3);
        // --safe-gpu drops the top rung; a one-rung ladder stays one rung.
        assert_eq!(select_ladder(true, false, false).len(), 2);
        assert_eq!(select_ladder(true, true, false).len(), 1);
        assert_eq!(select_ladder(false, true, false).len(), 1);
        assert_eq!(select_ladder(false, false, true).len(), 1);
    }

    #[test]
    fn an_unknown_forced_backend_is_not_a_ladder() {
        assert!(WgpuSetup::single_backend("3dfx").is_none());
        assert!(WgpuSetup::single_backend(" metal ").is_some());
    }

    #[test]
    fn a_corrupt_or_absent_sentinel_reads_as_none() {
        let dir = std::env::temp_dir().join(format!("zenman-sentinel-{}", std::process::id()));
        let sentinel = Sentinel::new(&dir);
        assert_eq!(sentinel.path(), dir.join(LAST_GOOD_FILE).as_path());
        assert_eq!(sentinel.read_blessed().map(|b| b.profile), None);
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(sentinel.path(), "not a record at all\n");
        assert_eq!(sentinel.read_blessed().map(|b| b.profile), None);
        let _ = std::fs::remove_file(sentinel.path());
    }

    #[test]
    fn create_then_bless_clears_the_marker() {
        let dir =
            std::env::temp_dir().join(format!("zenman-sentinel-cycle-{}", std::process::id()));
        let _ = std::fs::remove_dir(dir.join(MARKER_FILE));
        let sentinel = Sentinel::new(&dir);
        let profile = GpuProfile {
            name: "gpu",
            kind: GpuKind::Gpu,
            setup: WgpuSetup::hardware(),
        };
        let adapter = AdapterReport {
            backend: "Vulkan".into(),
            device_type: "Cpu".into(),
            name: "test".into(),
        };
        sentinel
            .create(&profile, &adapter)
            .expect("temp dir is writable");
        assert!(sentinel.exists());
        sentinel
            .bless(&profile, &adapter)
            .expect("temp dir is writable");
        assert!(!sentinel.exists(), "blessing must clear the pending marker");
        assert_eq!(
            sentinel.read_blessed().map(|b| (b.profile, b.backend)),
            Some(("gpu".to_string(), "Vulkan".to_string()))
        );
    }
}
