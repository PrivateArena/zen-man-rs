//! The undo log (plan §5.4): the byte-exact `ZMLG` frame [`codec`] and the append-only [`store`]
//! wrapper around one file.
//!
//! There is no `HistoryError`: plan §5.4's codec row names `error.rs` as its only dependency, and
//! `CoreError::LogCorrupt { offset, reason }` already carries the two facts a corrupt log needs (where,
//! and why the reader gave up) plus [`CoreError::kind`] = `"log_corrupt"` for the UI. Every fallible
//! function in this module returns `Result<_, CoreError>` for that reason, and plan §6.1 F4's
//! "never panics on a truncated log" is satisfied without a second error type.
//!
//! Step 9c added [`revert`] — the filesystem half of `/api/log/revert`, which drives the `ops/*` write
//! path backwards and ends in [`codec::patch_reverted`]'s in-place status patch (the capture's revert
//! appends no record; see the module docs there).
//!
//! [`codec::patch_reverted`]: codec::patch_reverted

pub mod codec;
pub mod revert;
pub mod store;

pub use codec::{
    ActionEntry, ActionLog, ActionRecord, ActionType, Decoded, LogStatus, encode, encode_vec,
    patch_reverted,
};
pub use revert::{RevertReport, ensure_reversible, revert};
pub use store::{
    ActionLogStore, LogOpenOptions, LogRecordRange, PruneLimit, PruneReport, ResyncReport,
};

/// Plan §5.4's spelling of the record type; [`codec::ActionRecord`] is the same item.
pub use codec::ActionRecord as HistoryEntry;
