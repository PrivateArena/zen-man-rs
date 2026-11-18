//! The undo log (plan §5.4): the byte-exact `ZMLG` frame [`codec`] and the append-only [`store`]
//! wrapper around one file.
//!
//! There is no `HistoryError`: plan §5.4's codec row names `error.rs` as its only dependency, and
//! `CoreError::LogCorrupt { offset, reason }` already carries the two facts a corrupt log needs (where,
//! and why the reader gave up) plus [`CoreError::kind`] = `"log_corrupt"` for the UI. Every fallible
//! function in this module returns `Result<_, CoreError>` for that reason, and plan §6.1 F4's
//! "never panics on a truncated log" is satisfied without a second error type.
//!
//! `history/revert.rs` — the filesystem half of `/api/log/revert` — is **not** here: it needs
//! `ops/*`, which is step 9 (plan §7), and the codec side it will call ([`codec::patch_reverted`]) is
//! complete and tested now.
//!
//! [`codec::patch_reverted`]: codec::patch_reverted

pub mod codec;
pub mod store;

pub use codec::{
    ActionEntry, ActionLog, ActionRecord, ActionType, Decoded, LogStatus, encode, encode_vec,
    patch_reverted,
};
pub use store::{
    ActionLogStore, LogOpenOptions, LogRecordRange, PruneLimit, PruneReport, ResyncReport,
};

/// Plan §5.4's spelling of the record type; [`codec::ActionRecord`] is the same item.
pub use codec::ActionRecord as HistoryEntry;
