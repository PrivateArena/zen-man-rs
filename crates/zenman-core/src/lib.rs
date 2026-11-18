//! `zenman-core` — the headless engine of the zen-man port: no egui, no wgpu, no window (plan §8 D1).
//!
//! Step 1 of plan §7 is a scaffold: this crate deliberately contains **no** modules yet. Every
//! module the plan names is listed below with the plan section that specifies it, so the roadmap is
//! reviewable from the crate root, and each lands in the step that owns it.
//!
//! ```text
//! error, metrics, path, cursor, entry, mode, limits, platform/{mod,posix,windows}   §5.2  step 3
//! ops/{mod,list,clipboard,paste,copy,move,delete,rename,mkdir,chmod,open,guard}     §5.3  step 9
//! services/{dirs,places,workspaces,actions,search_live,search_index,archive}        §5.3  steps 6, 11-13
//! store/{config,legacy,sql}, history/{codec,store,revert}, index/*                  §5.4  steps 4, 5, 12
//! archive/{mod,reader,vfs,extract}                                                  §5.5  step 13
//! jobs, session, prefs, contract                                                    §5.6  step 6
//! ```
//!
//! Two invariants the rest of the plan depends on this crate keeping:
//!
//! * no `unsafe` (plan §2.1 house rules; enforced by the crate attribute below), and
//! * no `egui`/`eframe`/`craft_ui` dependency in any form — `cargo xtask deps` fails the build if one
//!   appears (plan §5.1, `xtask` row: "`xtask deps` fails if `zenman-core` depends on
//!   egui/eframe/craft-ui").

// Plan §5.1 `crates/zenman-core/src/lib.rs` row. `forbid` (not `deny`) is deliberate: the core has no
// named unsafe exception, unlike `apps/zenman`/`zenman-ui` (see the workspace lint note in
// Cargo.toml).
#![forbid(unsafe_code)]
// The `unwrap_used`-class denials apply to shipping code only; the `#[cfg(test)]` modules below are
// full of `unwrap()`, which photocraft's precedent (`crates/*/src/lib.rs`) allows in tests.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unimplemented,
        clippy::todo,
        clippy::unreachable
    )
)]
#![deny(clippy::wildcard_imports)]

pub mod cursor;
pub mod entry;
pub mod error;
pub mod events;
pub mod history;
pub mod index;
pub mod jobs;
pub mod limits;
pub mod metrics;
pub mod mode;
pub mod path;
pub mod platform;
pub mod services;
pub mod session;
pub mod store;

/// Plan §5.1's `lib.rs` row: the façade re-export the UI is allowed to reach for.
///
/// `SessionConfig` was renamed to [`session::SessionOptions`] + [`session::SessionPaths`] in plan
/// §7 step 6's brief (the config the binary loads is `store::config::ZenConfig`, and keeping both under
/// one name would have been a trap); the re-export therefore names what exists.
pub use session::{
    Origin, OriginAlloc, Receipt, Session, SessionOptions, SessionPaths, StartupNote,
};

/// The plan step this crate is implemented up to (plan §5.1 `lib.rs` row; `apps/zenman` prints the
/// three crates' values in `--smoke` so a boot log says what is real and what is scaffold).
///
/// Step 6 of plan §7: the value types, the platform seam, the persistence layer, and now the job table,
/// the `DirService` listing/size path and the `Session` façade.
pub const PLAN_STEP: u32 = 6;
