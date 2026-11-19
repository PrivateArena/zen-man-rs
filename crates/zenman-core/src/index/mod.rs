//! The search index (plan §5.5): the SQLite objects the Go indexer created, and nothing else.
//!
//! Step 5 owned [`schema`] — the DDL and its verification. Step 6 owns [`intent`], the seam that lets
//! `ops/*` and `services/*` report a filesystem change **without holding a database connection** (plan
//! §5.3's rename row, §5.5's updater queue). The rest of the family the plan lists (`store`, `live`,
//! `node`, `walker`, `rebuild`, `updater`, `watcher`) arrives with §5.5's own step (12), and
//! `services/search_index.rs` (plan §5.4) is the API layer over it.
//!
//! The schema boundary the plan §5.5 row for `index/store.rs` mentions is deliberately narrow: every
//! other module in this family takes a `&Connection` handed to it and never opens a database of its own
//! — that is [`crate::store::sql`]’s single job, so there is exactly one place that knows about
//! `journal_mode`, `busy_timeout` and the retry loop.

pub mod intent;
pub mod rebuild;
pub mod schema;
pub mod updater;
pub mod walker;
/// The filesystem watcher (plan §5.5 `index/watcher.rs`), feature-gated: `Watcher`/`start` are the
/// inotify backend with `--features watch` on Linux and a typed-error stub everywhere else, while
/// `DirChanged`/`Coalescer` are always compiled — that split is what lets the no-feature build keep the
/// event type the `Session` pump consumes without pulling in a backend it cannot build.
pub mod watcher;
