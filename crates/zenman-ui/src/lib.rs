//! `zenman-ui` — the egui shell: the `craft_ui::docking` surface, panes, tabs, widgets, dialogs and
//! menus of plan §5.7-§5.8. Headless-friendly by rule: this crate never touches `std::fs` (plan §5.1
//! "Done when": `grep -rn "std::fs::" crates/zenman-ui/src` is empty); persistence goes through
//! `zenman-core` and `craft-settings`, which is step 7's job.
//!
//! Step 1 of plan §7 is a scaffold: **no UI code yet**. The module map, with the plan row that owns
//! each file:
//!
//! ```text
//! theme, icons, dock, dock_content, state, app, jobs_ui, persist          §5.7  step 7
//! keymap                                                                  §5.8  step 10
//! widgets/{virtual_list,entry_row,modals,toast,disk_meter}                §5.8  steps 7, 8, 10
//! panels/{header,breadcrumb,tab_bar,file_pane,side_panel,action_log,status_bar}  §5.8  steps 7, 8, 14
//! dialogs/{mod,quick_find,search_manager,batch_rename,properties,action_editor}  §5.8  steps 10-13
//! menus, drag_drop                                                        §5.8  steps 10, 13
//! ```
//!
//! The dependency boundary that makes that split real: this crate may use `zenman-core`, `egui` and
//! `craft-ui`; `zenman-core` may never use any of them (checked by `cargo xtask deps`).

// Plan §5.1: `unsafe_code` is `deny` here (the crate-local exception mechanism documented in
// Cargo.toml), with the same per-crate clippy header as `zenman-core`.
#![deny(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]
#![deny(clippy::wildcard_imports)]

// The framework this crate is written against is part of its public surface: step 7's shell is built
// out of `craft_ui` primitives, and re-exporting it here means callers get the *same* rev the crate
// was compiled against (and the manifest dependency is real today, not wishful). Re-added here rather
// than dropped, so the `cargo tree -i egui` / `xtask deps` audit in step 1 sees the true graph.
pub use craft_ui;

/// The step-1 placeholder; `pub mod app, state, theme, …` (plan §5.1) arrives in step 7 with the
/// modules above. Kept as a value so the crate is not empty enough to look accidental.
pub const PLAN_STEP: u32 = 1;

/// Proves at compile time that the fork linkage plan §2.1 pins is the one this crate sees: a
/// reference to the `craft_ui` re-exported geometry type forces `craft-ui` (and through it the
/// patched `egui`) into this crate's metadata.
pub const FRAMEWORK_LINKAGE: Option<egui::Style> = None;
