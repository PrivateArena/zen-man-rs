//! The service layer (plan §5.3): one module per user-facing facility, each owning its caches and its
//! own narrow seam to the outside world.
//!
//! Step 6 of plan §7 owns [`dirs`]; `archive`, `ops`, `actions`, `search_live`, `search_index` and
//! `watch` arrive in the steps that own their behaviour (plan §5.6's file list), and every one of them
//! reaches the filesystem through a trait like [`dirs::FsSource`] and the index through
//! [`crate::index::intent::IndexIntent`] — never through a database connection (plan §5.3's rename row).
//!
//! Two rules bind every module here:
//!
//! * **caller-named paths only.** Nothing in `services/` discovers a directory, resolves `~`, or reads an
//!   environment variable; a path arrives already absolute (plan §5.2's `SystemInfo` note: `AppPaths` in
//!   `apps/zenman` stays the authority).
//! * **the OS answers OS questions.** Anything `platform::Env` can answer — chmod, trash, statvfs, spawn,
//!   open — goes through `Env`; [`FsSource`] is the *listing* seam because `Env` has no such method, and
//!   it is the only place under `services/` where a filesystem call happens.

pub mod archive;
pub mod dirs;
pub mod name_match;
pub mod search_index;
pub mod search_live;
pub mod sort;
