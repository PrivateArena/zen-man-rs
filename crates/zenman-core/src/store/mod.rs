//! Persistent state (plan §5.4): the settings file and the one-time import of the Go app's data.
//!
//! `store/{config,legacy}` are step 4; `store/sql` (the shared index database) is step 5.
//!
//! # The filesystem seam, and why it is here
//!
//! Plan §5.1's house rule is "core has no `std::process`/`std::fs` outside `platform/`". `platform/`
//! owns *capability* questions (can we symlink? is this path trashable? what does `statvfs` say); this
//! module owns *our own files*, which is a different concern and one the plan §5.4 rows specify
//! concretely (`load(path)`, atomic `save`, `config.json.bak`). So `store/*` is the second, narrow
//! filesystem seam in the crate: it touches only paths the caller handed it, never discovers a
//! directory of its own (that is `apps/zenman::app_dirs`, the authority — see
//! [`crate::platform::system`] for the same rule stated for `SystemInfo`), and spawns nothing.

pub mod config;
pub mod legacy;
pub mod sql;
