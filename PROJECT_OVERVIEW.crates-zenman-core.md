<!-- codegraph-file-count: 68, last-commit: 84b9074d690d9956d22f923d0898d9af73eb2948 -->

# zenman-core (crates/zenman-core) — Rust 2024, headless file-manager engine

Scope file for `crates/zenman-core/` (68 files). See `PROJECT_OVERVIEW.md` for the workspace, the egui shell and the binary.

## Purpose

`zenman-core` is the UI-free engine of the zen-man egui port: directory listing, file operations (copy/move/paste/delete/rename/batch-rename/chmod/open), archive-as-filesystem browsing and extraction, a SQLite FTS5 filename index with a live directory watcher, a byte-compatible `ZMLG` undo log with revert, background jobs, and a platform abstraction over Linux/Windows. Its primary output is the `Session` struct that `zenman-ui` drives one frame at a time. On-disk formats (config JSON, `action.log`, index DB) are deliberately compatible with the original Go application. No egui/eframe dependency is allowed in this crate — `cargo xtask deps` enforces the boundary.

## Architecture

```
apps/zenman -> Session::open(paths, config, env, limits)
  Session (facade, single owner of engine state)
    DirService / SearchService / ArchiveService / WorkspaceService  (services/)
    ops::{copy,move,paste,delete,rename,batch_rename,chmod,open}    + guard::SourceGuard
    index::{schema,walker,updater,watcher} -> rusqlite    history::{codec,store} -> action.log
    jobs::JobQueue (threads) --events--> Session::poll_at(now) -> Vec<EngineEvent>
      all FS/OS effects go through platform::Env (linux | windows | stub)
```

## File Tree

```
crates/zenman-core/
  Cargo.toml                    features: watch (inotify), rar (no-op, licence gate)
  src/
    lib.rs                      module surface, #![forbid(unsafe_code)]
    session.rs                  Session facade — the crate's only public entry point
    path.rs entry.rs mode.rs limits.rs cursor.rs metrics.rs error.rs events.rs jobs.rs
    services/                   dirs search_live search_index actions action_log archive
                                places workspaces name_match sort
    ops/                        mod copy move paste delete rename batch_rename clipboard
                                chmod mkdir open guard
    index/                      schema walker updater rebuild watcher intent
    history/                    codec store revert
    archive/                    mod reader vfs extract
    store/                      config legacy sql
    platform/                   mod (Env trait) system linux windows stub env_probe
  tests/                        contract*.rs (7) + archive_sevenz.rs + perf_batch_rename.rs
```

## Component Roles

| File / Module | Role | LOC | Key Exports |
|---|---|---|---|
| src/lib.rs | Crate root: public module list + crate-wide `forbid(unsafe_code)`; re-exports the session API | ~74 | `pub mod archive..store` (17 modules), `pub use session::{...}` |
| src/session.rs | Engine facade and single owner of config, DB handles, job queue, watcher, caches, event queue | ~3635 | `Session::open(paths, config, env, limits) -> Result<(Session, Vec<StartupNote>), CoreError>`; `poll_at(now) -> Vec<EngineEvent>`; `page_dir(&ListDir, Origin) -> Result<Receipt<Arc<DirPage>>, CoreError>`; `clipboard() -> Option<Clipboard>` and `clipboard_with(read)` — the frame loop takes the second, which looks at the queue without copying every path and name in it |
| src/path.rs | Path value types + classification: absolute host paths, VFS archive paths, file-URI parsing, Windows forms | ~2092 | `AbsPath::{resolve(raw, cwd, home), join, join_rel, relative_to}`; `FsPath::new(store, relative)`; `enum VfsKind`; `decode_file_uri(raw) -> Option<String>` |
| src/entry.rs | The one entry record shared by FS, index and archive listings | ~1303 | `struct FileEntry`, `enum EntryKind`, `trait EntryLike`, `struct EntryStat`, `struct VirtualNode`, `unix_to_datetime(secs)` |
| src/error.rs | Single typed error enum for the whole engine (path rejects, IO, conflicts, DB, archive, unsupported) | ~749 | `enum CoreError` (~60 variants), `enum PathReject` |
| src/events.rs | The only thing the UI receives per frame: job/dir/config notices | ~376 | `enum EngineEvent`, `enum ChangeCause`, `enum JobPayload`, `struct IndexSummary`, `enum NoticeLevel` |
| src/jobs.rs | Threaded job queue: submit, cancel, progress, completion events | ~1483 | `JobId`, `enum JobKind/JobStatus`, `struct CancelToken`, `JobSpec`, `JobHandle`, `struct JobProgress` |
| src/limits.rs | Every engine budget in one value, so tests can shrink the world | ~329 | `struct Limits` (page caps, cache tiers, budgets), `enum DirCacheTier` |
| src/metrics.rs | Go-compatible size/time formatting (Go zero-time, `mod_time` text forms) | ~282 | `format_mod_time(unix: Option<i64>) -> String`, `format_mtime`, `format_relative_time(then, now)` |
| src/mode.rs | POSIX mode bits <-> Go `finfo` strings, Windows attributes, octal parse | ~755 | `parse_octal(raw) -> Result<ModeBits, CoreError>`, `finfo_string(mode) -> String`, `mode_to_attributes(bits, is_dir)` |
| src/cursor.rs | Opaque page cursor for directory paging | ~210 | `page<T>(all: &[T], cursor, limit) -> (&[T], Cursor, bool)` |
| src/services/dirs.rs | Directory listing service: paged reads, bounded page cache, dir-size walks, flat/tree modes | ~2072 | `DirService::new(source: Arc<dyn FsSource>, limits)`, `ListDir::first(path)`, `struct DirPage`, `enum FlatMode`, `trait FsSource`, `struct DirSizeStat` |
| src/services/search_live.rs | Budgeted live filesystem search (walk + throttle + admission) | ~1222 | `run(req: &SearchQuery, source, limits, cancel, sink) -> Result<SearchStatus, CoreError>`; `SearchService`; `SearchThrottle`; `enum SearchAdmission` |
| src/services/search_index.rs | Query parsing + FTS5/trigram SQL against the index DB | ~743 | `parse_query(q) -> ParsedQuery`, `search(conn, q, limit, offset, dirty, last_indexed_unix) -> Result<IndexSearchResults, _>` |
| src/services/actions.rs | Custom external commands: role/pattern matching, `%token` substitution, exec | ~647 | `matches(&CustomAction, &ActionContext) -> bool`, `substitute_for(os, template, ctx) -> String`, `build_request(...) -> ExecRequest`, `exec(env, action, ctx, cap)` |
| src/services/action_log.rs | View/query model over the undo log | ~295 | `struct LogQuery`, `LogRow`, `LogPage`, `label(&ActionRecord) -> String` |
| src/services/archive.rs | Archive properties for the Properties dialog | ~189 | `ArchiveService`, `struct ArchiveProps` |
| src/services/places.rs | Left-panel places: user dirs, bookmarks, visible mounts | ~382 | `Places`, `user_places(home)`, `favourite_places(&[Bookmark])`, `struct Disk`, `visible_mount(mount_point)` |
| src/services/workspaces.rs | Named workspace (session) store, the `--session` session file, and the autosave scheduler | ~645 | `WorkspaceService::{new, reload, list, restore, save, delete, rename, read_session}`, `WorkspaceMeta`, `RESERVED`, `struct Autosave` (`note`/`due`/`take_due`/`take_pending`/`discard`/`mark_saved`/`show_saved`), `Pending`, `normalise_name`, `blank_session(layout, path) -> Workspace` |
| src/services/name_match.rs | Hand-written matcher used by Quick Find / filters: glob, word, and a mini regex VM | ~1337 | `compile_name_match(query, mode) -> Result<NameMatcher, CoreError>`, `tokenize(query)`, `enum NameMode` |
| src/services/sort.rs | Single sort spec shared by panes and the index | ~267 | `trait SortRow`, `sort_by<T: SortRow>(rows, spec)`, `sort_entries(&mut [FileEntry], spec)` |
| src/ops/mod.rs | Shared op vocabulary: conflict policy, progress sink, shared error cell | ~197 | `enum ConflictPolicy/ConflictDecision`, `unique_target(&AbsPath)`, `trait ProgressSink`, `shared(CoreError) -> SharedError` |
| src/ops/guard.rs | Safety pre-flight: source/dest guards, VFS write refusal, traversal-safe joins | ~576 | `guard_sources(&[AbsPath], dest) -> Result<SourceGuard, _>`, `safe_join(dest, member)`, `dest_guard(&FsPath)`, `ensure_writable(env, path)` |
| src/ops/copy.rs | Recursive copy with mode/ symlink handling and cancellation | ~389 | `copy_file(src, dst, cancel, progress) -> Result<u64, _>`, `copy_tree(src, dst, policy, symlinks, cancel, progress)` |
| src/ops/move.rs | Rename/move incl. cross-device copy+delete fallback | ~380 | `trait MoveFs`, `move_item(src, target, policy, fs, cancel, progress) -> Result<MoveStats, _>`, `is_cross_device(&CoreError)` |
| src/ops/paste.rs | Clipboard paste orchestration (cut vs copy, conflict loop, log record) | ~463 | `paste(env, fs, req: &PasteReq, guard, cancel, progress) -> Result<PasteReport, CoreError>`, `enum PasteMode/PasteOutcome` |
| src/ops/delete.rs | Delete via platform trash-or-permanent with per-item outcome | ~438 | `delete(env, req: &DeleteReq, cancel, progress) -> Result<DeleteReport, CoreError>`, `enum DeleteMode/DeleteOutcome` |
| src/ops/rename.rs | Batch rename of many sources to one name | ~408 | `rename(fs, req: &RenameReq, cancel, progress) -> Result<RenameReport, CoreError>`, `validate_rename_name(name)` |
| src/ops/batch_rename.rs | Pure preview/apply rule engine (find/replace/counter/case/regex) | ~1363 | `preview(rows, rules, existing) -> PreviewSet`, `struct Rules`, `Row`, `PreviewRow`, `enum RowStatus` |
| src/ops/clipboard.rs | zen-man clipboard format + system `file://` URI list round trip; one `lstat` per queued path answers both its size and Go's `isDir` (`api/ops.go:163-176`), which is what lets the shell's queue list print a folder as the listing row does | ~960 | `store(source, req: &ClipboardStore) -> Result<Clipboard, _>`, `struct ClipItem` (`path`/`display`/`is_dir`/`size`), `uri_list(&[AbsPath])`, `parse_uri_list(text)`, `enum ClipOp` |
| src/ops/chmod.rs | Mode change request/report | ~223 | `chmod(env, req: &ChmodReq, cancel) -> Result<ChmodReport, CoreError>`, `octal_text(ModeBits)` |
| src/ops/mkdir.rs | Directory creation with explicit mode | ~123 | `mkdir(env, req: &MkdirReq) -> Result<FileEntry, CoreError>`, `stat_entry(path)` |
| src/ops/open.rs | Open/reveal routing to the platform opener | ~169 | `open_paths(env, paths, cancel) -> Result<OpenReport, _>`, `reveal(env, path)`, `route(path) -> OpenRoute` |
| src/index/schema.rs | Idempotent DDL, version stamping, schema verification | ~678 | `apply(conn) -> Result<SchemaReport, CoreError>`, `verify(conn) -> Result<Vec<SchemaIssue>, _>`, `version_of(conn)` |
| src/index/walker.rs | Exclude-pattern compiler + cancelable directory walk producing rows | ~1313 | `walk(cfg: &WalkCfg, cancel, sink: &mut dyn FnMut(&[IndexedRow])) -> Result<WalkStats, _>`, `compile_excludes(&[String]) -> GlobMatcher` |
| src/index/updater.rs | Applies one engine mutation to the index without a re-walk | ~871 | `apply_op(conn, &IndexOp)`, `apply_ops(conn, ops)`, `coalesce(ops) -> Vec<IndexOp>` |
| src/index/rebuild.rs | Full index rebuild with staged progress and batching | ~601 | `run(conn, cfg, cancel) -> Result<RebuildSummary, CoreError>`, `run_with_hook(..., hook: &dyn Fn(&RebuildProgress))` |
| src/index/watcher.rs | inotify watcher + change coalescer (feature `watch`) | ~1213 | `enum DirChanged`, `struct Coalescer`, `struct WatchConfig` |
| src/index/intent.rs | The seam ops use to tell the index what happened | ~344 | `enum IndexOp`, `trait IndexIntent`, `type SharedIntent = Arc<dyn IndexIntent>`, `noop_intent()`, `recording_intent()` |
| src/history/codec.rs | `ZMLG` action-log frame codec: CRC32, Go nanotime, field caps | ~1009 | `encode(out, &ActionRecord) -> Result<usize, _>`, `encode_vec`, `verify_crc(frame) -> bool`, `struct ActionRecord/ActionLog/ActionEntry` |
| src/history/store.rs | Append-only log file management: open/scan/resync/prune/atomic rewrite | ~1371 | `ActionLogStore`, `LogOpenOptions`, `struct ResyncReport`, `enum PruneLimit`, `PruneReport` |
| src/history/revert.rs | Undo of a logged action via the normal op layer | ~363 | `revert(env, fs, &ActionRecord, cancel, progress) -> Result<RevertReport, CoreError>`, `ensure_reversible(record)` |
| src/archive/mod.rs | Format detection by name and magic; archive error type | ~216 | `detect(path)`, `detect_name(name)`, `sniff(prefix) -> Option<ArchiveFormat>`, `enum ArchiveError` |
| src/archive/reader.rs | Reader trait + zip / 7z backends with decompression caps | ~654 | `open(fmt, file, size, name) -> Result<Box<dyn ArchiveReader>, CoreError>`, `open_bytes(...)`, `trait ArchiveReader`, `struct RawEntry` |
| src/archive/vfs.rs | Archive contents as a browsable node tree + manifest cache keyed on host mtime | ~738 | `build_manifest(r: &mut R, limits) -> Result<Manifest, CoreError>`, `ManifestCache`, `ArchiveStore`, `member_path(host, nested, inner) -> FsPath` |
| src/archive/extract.rs | Extraction job: plan, conflict policy, progress, cancel | ~782 | `plan(&Manifest, dest, sources) -> Result<Vec<PlanItem>, _>`, `extract_all(reader, items, dest, policy, cancel, progress)` |
| src/store/config.rs | The whole config model (theme, columns, layout, tabs, index config, bookmarks) | ~1046 | `enum ThemeMode/LayoutMode/ViewMode/SortColumn/PaneSlot`, `struct SortSpec`, `ColumnWidths`, `TabSession`, `type ConfigVersion` |
| src/store/legacy.rs | Reads the Go app's `config.json` / workspaces and plans the migration | ~1014 | `load_legacy_config(path) -> Result<ZenConfig, CoreError>`, `plan_migration(&LegacySource)`, `migrate_from_go(source, dry_run)` |
| src/store/sql.rs | SQLite open policy: retries, pragmas, integrity + schema gates, typed open errors | ~496 | `connect_with_retry(path, mode, attempts, backoff)`, `open_with_retry(...) -> Result<Connection, DbOpenError>`, `enum DbOpenError` |
| src/platform/mod.rs | The `Env` port every effect goes through + platform capability flags | ~749 | `trait Env { caps, open_with, trash, exec_program, ... }`, `Platform::detect()`, `default_env() -> Arc<dyn Env>`, `struct ExecRequest/ExecOutput`, `Os` |
| src/platform/system.rs | `SystemEnv`: OS-backed `Env` glue (env vars, cwd, locale tag, system info) | ~332 | `struct SystemEnv`, `SystemInfo`, `locale_tag(lc_all, lang) -> String` |
| src/platform/linux.rs | Linux implementation: rustix statvfs/accessat, inotify glue, xdg-open, trash | ~1272 | `struct Linux`, `symlink(link, target)`, `chmod(targets, mode)`, `exec_program(&ExecRequest)`, `is_writable(path)` |
| src/platform/windows.rs | Windows implementation: Win32 junctions/shortcuts/attributes, no rustix | ~1250 | `struct Windows` (same `Env` surface), shortcut + `attributes_to_mode` paths |
| src/platform/stub.rs | Named "unsupported" bodies for targets with no platform backend | ~290 | `struct Unsupported`, `Unsupported::new()`, `labelled(what)`, `no_mounts_for(path) -> MountTable` |
| src/platform/env_probe.rs | Capability probing in a scratch dir: overlayfs, symlink support, userns | ~388 | `probe(env) -> Result<ProbeReport, CoreError>`, `overlayfs_works(scratch_root)`, `probe_symlink_support(root, flags)` |
| src/scratch.rs | **test-only** (`#[cfg(test)]`, `#[path]`-included by the other crates): scratch-tree naming, one mode-relaxing recursive delete, and the reaper for trees whose owner process exited | ~250 | `create(name) -> PathBuf`, `struct Scratch` (guard), `remove_tree(&Path)`, `sweep_once()`, `sweep_stale() -> usize`, `survivors()`, `reapable(name, &Path)` |
| tests/contract.rs | Golden contracts for the generic engine surface (paths, errors, entries) | ~554 | no exports |
| tests/contract_dirs.rs | Directory listing/paging/cache contracts | ~1292 | no exports |
| tests/contract_ops.rs | Largest contract suite: copy/move/paste/delete/rename/chmod incl. cancellation | ~2692 | no exports |
| tests/contract_archive.rs | Archive browse/extract contracts (zip + 7z fixtures) | ~708 | no exports |
| tests/contract_search.rs | Live search and FTS index contracts | ~1530 | no exports |
| tests/contract_history.rs | Action-log codec/store/revert contracts | ~959 | no exports |
| tests/contract_workspaces.rs | Workspace save/load contracts | ~1233 | no exports |
| tests/archive_sevenz.rs | Real 7z round trip using `sevenz-rust` writer (dev-dep) | ~124 | no exports |
| tests/perf_batch_rename.rs | Batch-rename throughput guard | ~148 | no exports |

## Cross-References

| File | Called by / calls | Why it's central |
|---|---|---|
| src/session.rs | Called by `apps/zenman/src/main.rs` (`run_with_context`), `apps/zenman/src/app.rs` (`ShellApp::new`), `apps/zenman/src/config_boot.rs`, `apps/zenman/src/single_instance.rs`; calls `DirService`, `SearchService`, `ArchiveService`, `jobs`, `index::{updater,watcher,rebuild}`, `history::store`, `ops::*` | 1.7k refs, 169 symbols — every feature enters and leaves through `Session` |
| src/services/dirs.rs | Called by `session.rs`, `apps/zenman/src/app.rs`, `archive/extract.rs`; implements `trait FsSource` consumed by `search_live::run`, `clipboard::store`, `index/walker` | Every listing, size and search runs on `FsSource`/`DirService` |
| src/path.rs | Called by nearly all crate files plus `apps/zenman` and `zenman-ui`; produces `AbsPath` (host) and `FsPath` (host or archive member) | 1.7k refs — the two path types are the crate's common currency |
| src/entry.rs | Called by `archive/reader.rs`, `archive/mod.rs`, `index/{updater,walker}`, `history/{store,revert}`, `ops/mkdir` | `FileEntry` is the single row shape for FS, index and archive views |
| src/archive/vfs.rs | Called by `session.rs`, `archive/extract.rs`, `apps/zenman/src/main.rs`; calls `reader::open`, `entry::VirtualNode`, `path::VfsKind` | Turns an archive into a directory tree, so browsing and extraction share one model |
| src/index/intent.rs | Implemented/consumed by `session.rs` (`SharedIntent`, `note_index`), `index/updater.rs`, `index/rebuild.rs` | The one-way seam that keeps the DB in step without a re-walk |

## Key Architectural Patterns

1. **Single facade, no god-object globals**: `Session` owns all engine state; `zenman-ui` holds exactly one `Session` and only ever calls `Session::*` methods. Nothing else in the crate is stateful across calls.
2. **Origin-tagged causality**: every mutating call takes an `Origin` (allocated by `OriginAlloc`) and returns a `Receipt<T>` (Done / Running(job) / with origin), so the UI can tell which pane caused which event after the fact.
3. **Event queue instead of callbacks**: background work never calls the UI. Jobs push `EngineEvent`s; the frame loop drains them with `Session::poll_at(now)`, and `wants_repaint()` returns the next wake-up interval.
4. **`platform::Env` port**: all FS/OS effects (trash, open, exec, statvfs, symlink, probe) are trait methods implemented by `linux.rs` / `windows.rs` / `stub.rs`; tests inject fakes, so `ops/*` never touch `std::fs` directly (enforced by `cargo xtask ci` source guards).
5. **Index intent pattern**: `IndexIntent`/`IndexOp` record mutations during ops; `index/updater::coalesce` folds them into minimal SQL so the DB catches up incrementally, with `rebuild::run` as the cold path.
6. **Guard-before-mutate**: `ops::guard::guard_sources` / `dest_guard` / `vfs_write_guard` / `safe_join` run before any write, refusing archive-target writes and traversal escapes, and the `SourceGuard` protects cut-paste from destroying its own sources.
7. **Go byte-compatibility as a contract**: `history/codec` (`ZMLG` + CRC32 + Go nanotime), `metrics` time formats, `mode` finfo strings and `store/legacy` config import exist to keep an existing zen-man data directory usable; `tests/contract*` pin that behaviour.
8. **Budgets as data**: `Limits` feeds `SessionOptions`, `DirCacheTier`, `SearchBudget`, page cache caps and archive decompression caps, so resource behaviour is testable rather than ambient.

## Read Triggers

| If you need to... | Open these files |
|---|---|
| Add a new file operation (op + event + undo) | `src/ops/mod.rs` (ConflictPolicy/ProgressSink), a sibling op e.g. `src/ops/delete.rs`, `src/session.rs` (`delete`/`revert` methods), `src/events.rs`, `crates/zenman-core/tests/contract_ops.rs` |
| Change directory listing / paging / caching | `src/services/dirs.rs` (`DirService`, `ListDir`, `BoundedCache`), `src/session.rs` (`page_dir`, `invalidate`), `tests/contract_dirs.rs` |
| Add an archive format | `src/path.rs` (`ArchiveFormat`), `src/archive/mod.rs` (`sniff`), `src/archive/reader.rs` (new `ArchiveReader` impl), `src/archive/vfs.rs`, `tests/contract_archive.rs` |
| Change the index schema or rebuild | `src/index/schema.rs`, `src/index/updater.rs`, `src/index/rebuild.rs`, `crates/zenman-core/tests/contract_search.rs`, `docs/contracts.md` |
| Change live search behaviour or budgets | `src/services/search_live.rs` (`SearchQuery`, `SearchThrottle`, `SearchAdmission`), `src/limits.rs`, `src/services/name_match.rs` |
| Touch config, layout or tab persistence | `src/store/config.rs`, `src/store/legacy.rs` (Go import), `src/session.rs` (`config`, `write_bookmarks`), `tests/contract_workspaces.rs` |
| Fix undo / action-log corruption handling | `src/history/codec.rs` (frame format), `src/history/store.rs` (resync/prune), `src/history/revert.rs` |
| Add a platform capability or fix a Linux/Windows gap | `src/platform/mod.rs` (`trait Env`, `PlatformCaps`), `src/platform/linux.rs` or `windows.rs`, `src/platform/stub.rs`, `src/platform/env_probe.rs` |
| Debug a cancel/stalled job | `src/jobs.rs` (`CancelToken`, `Progress`, `JobEvent`), `src/session.rs` (`poll_at`, `cancel_origin`), `crates/zenman-ui/src/jobs_ui.rs` |
| Change path parsing, file URIs or VFS addressing | `src/path.rs` (`AbsPath`, `FsPath`, `VfsKind`, `decode_file_uri`), `apps/zenman/src/main.rs` (`uri_scheme`, `targets`) |

## Dependencies

### Engine core (internal, `crates/zenman-core/Cargo.toml`)
| Package / Module | Role | Version |
|---|---|---|
| (none internal) | `zenman-core` depends on no workspace crate — the headless boundary is a manifest fact | — |

### Storage
| rusqlite (`bundled`) | Index + settings DB with FTS5 and trigram tokenizer; C SQLite compiled in, no distro lib | 0.43 |
| serde / serde_json | Config, workspaces, legacy Go config, contracts | 1 / 1 |

### Filesystem & OS
| rustix (`fs`, unix-only target) | statvfs, accessat, inotify backend for the watcher | 1.1.6 |
| trash | Recycle-bin delete path on both platforms | 5.4.0 |
| open | `xdg-open` / ShellExecute seam | 5.4.5 |

### Formats & support
| zip / sevenz-rust | Archive readers; `sevenz-rust` gets `compress` in dev-deps only (7z writing is test-only) | 9.7 / 0.6.1 |
| thiserror / bitflags | `CoreError` derivation; `ModeBits`, `PlatformCaps`, `ProbeFlags` | 2 / 2 |
| chrono (`std`, `clock`) | Go-compatible timestamps and formatting | 0.4 |

## Build & Run

| Command | Purpose |
|---|---|
| `cargo test -p zenman-core` | Engine contract suites (`tests/contract*.rs`) |
| `cargo test -p zenman-core --test contract_ops` | One contract area (file operations) |
| `cargo check -p zenman-core --features watch,rar` | Both optional features compile on and off |
| `cargo xtask deps` | Proves `zenman-core` stays free of egui/eframe/craft-ui |
| `cargo xtask contracts` | Regenerates/validates `tests/golden` fixtures against `docs/contracts.md` |
