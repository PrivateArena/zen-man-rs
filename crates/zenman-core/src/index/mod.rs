//! The search index (plan §5.5): the SQLite objects the Go indexer created, and nothing else.
//!
//! Step 5 owns [`schema`] — the DDL and its verification. The rest of the family the plan lists
//! (`store`, `live`, `node`, `walker`, `search`, `db`, `watch`) arrives with §5.5's own step (12), and
//! `services/search_index.rs` (plan §5.4) is the API layer over it.
//!
//! The schema boundary the plan §5.5 row for `index/store.rs` mentions is deliberately narrow: every
//! other module in this family takes a `&Connection` handed to it and never opens a database of its own
//! — that is [`crate::store::sql`]’s single job, so there is exactly one place that knows about
//! `journal_mode`, `busy_timeout` and the retry loop.

pub mod schema;
