//! `zenman-ui` — the egui shell: the `craft_ui::docking` surface, panes, tabs, widgets, dialogs and
//! menus of plan §5.7-§5.8. Headless-friendly by rule: this crate never touches `std::fs` (rule 7,
//! enforced by a grep guard in `xtask ci`); all IO goes through `zenman-core` and `craft-settings`.
//!
//! Step 7 of plan §7 landed the shell skeleton. The module map, with the plan row that owns each:
//!
//! ```text
//! theme, icons, dock, dock_content, state, app, jobs_ui, persist    §5.7  step 7   (this step)
//! widgets/{toast,disk_meter}                                        §5.8  step 7   (this step)
//! widgets/{virtual_list,entry_row}                                  §5.8  step 8
//! panels/{header,breadcrumb,tab_bar,file_pane,side_panel,action_log,status_bar}  §5.8  steps 8, 14
//! dialogs/{mod,quick_find,search_manager,batch_rename,properties,action_editor}  §5.8  steps 10-13
//! keymap, menus, drag_drop                                          §5.8  steps 10, 13
//! ```
//!
//! The dependency boundary that makes that split real: this crate may use `zenman-core`, `egui` and
//! `craft-ui`; `zenman-core` may never use any of them (checked by `cargo xtask deps`).
//!
//! Two crate-wide invariants the gates enforce: colours live only in [`theme`] (colour guard), and
//! no module opens a file (IO guard).

// Plan §5.1: `unsafe_code` is `deny` here (the crate-local exception mechanism documented in
// Cargo.toml), with the same per-crate clippy header as `zenman-core`. The `cfg_attr(not(test))`
// shape is rule 3: the test bodies are full of `unwrap()`, which photocraft's precedent allows.
#![deny(unsafe_code)]
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

// The framework this crate is written against is part of its public surface: the shell is built
// out of `craft_ui` primitives, and re-exporting it means callers get the *same* rev the crate was
// compiled against.
pub use craft_ui;

pub mod app;
pub mod dock;
pub mod dock_content;
pub mod icons;
pub mod jobs_ui;
pub mod panels;
pub mod persist;
pub mod state;
pub mod theme;
pub mod widgets;

/// The plan step this crate is implemented up to (plan §5.1 `lib.rs` row; `apps/zenman` echoes it).
///
/// Step 7: the shell skeleton — theme tokens, icon registry, the stripless dock, the empty panes,
/// the job plumbing and the settings/dock persistence.
pub const PLAN_STEP: u32 = 7;

/// Proves at compile time that the fork linkage plan §2.1 pins is the one this crate sees: a
/// reference to the `craft_ui` re-exported geometry type forces `craft-ui` (and through it the
/// patched `egui`) into this crate's metadata.
pub const FRAMEWORK_LINKAGE: Option<egui::Style> = None;
