# zen-man wire and storage contracts

The reference an implementer reads while writing `crates/zenman-core`. Everything here is
transcribed from bytes a disposable Go `zen-man` instance actually emitted — not from handler
intent. Cross-references: the plan is
[`ARCHITECTURE_PLAN_zen-man-rs-egui-port.md`](../ARCHITECTURE_PLAN_zen-man-rs-egui-port.md);
its **§2.4** table is the authoritative measured-deltas list, §2.3 the behaviour contract,
§5.2–§5.6 the `zenman-core` blueprint rows this document is meant to unambiguous-ise.

## Quirk verdicts — plan §2.3's fourteen behaviours, one line each

Step 15d's sweep. This table is the **verdict list** row 15 asks for; the *evidence* (which golden
proves the Go side) is the [Deviation ledger](#deviation-ledger) further down, and the port-side bug
fixes beyond these fourteen are plan §6.2. Verdicts are `kept` (we behave like Go), `fixed` (we do not,
because Go is wrong), `deviated (reason)`, `not built`.

All fourteen are built and all but two are pinned by a named test; where the claim is a test, the test
name is real and greppable. Two of them changed shape since steps 5-13 wrote the prose below, and those
rows now say so.

| # | Quirk | Verdict | Pinned by |
|---|---|---|---|
| 1 | Dirs first, regardless of `order` | **kept** | `services/dirs.rs::listing_is_sorted_dirs_first_by_lowercased_name`, `descending_keeps_dirs_first_and_pages_advance` |
| 2 | Name sort = byte compare of **lowercased** names (`a10 < a9`) | **kept** | same test (its name half) + `contract_dirs::dir_listing_matches_go` against `dir-sortcase-asc/desc` |
| 3 | Cursor = decimal index into a list re-read and re-sorted per request | **kept**, documented as a flaw (O(n) per page; concurrent edits shift the index) | `services/dirs.rs::the_second_page_hit_reads_the_directory_once`, `a_flat_page_is_never_served_to_a_plain_request` |
| 4 | Flat walk caps at 10 000 | **kept** | `services/dirs.rs::the_flat_cap_counts_kept_rows_and_truncates_lexically`, `the_walk_cap_reports_instead_of_undercounting` |
| 5 | Symlink-to-dir reports `is_dir:false` | **fixed** — `is_dir:true` **and** `is_symlink:true`; `open` still follows, the walker still does not descend | `entry.rs::symlink_to_a_directory_is_a_directory_and_a_link` (plan §6.2 cited `ops/list.rs`, a file this port never grew — corrected) |
| 6 | Hidden files never filtered by the backend | **kept** at the engine boundary (hiding is a UI toggle) | `services/search_live.rs::hidden_and_scope_filters`, `live_search_hidden_file_policy` |
| 7 | `chmod` is bare octal | **kept as measured, not as the plan said**: Go takes `^[0-7]{3,4}$` (`"0644"` is 200), the port validates exactly that; symbolic modes (`u+x`) are a port addition, not parity | `mode.rs::parse_octal_follows_the_measured_chmod_rule`, `session::chmod_round_trips_octal_to_symbolic_and_logs_the_text_as_typed` |
| 8 | Paste conflict is dir-into-dir only, HTTP 200 + magic string | **fixed** — `PasteOutcome::Conflict` (+ `ConflictPolicy::{SkipExisting, Overwrite, Rename}`); the HTTP envelope does not exist here | `contract_ops.rs::merge_dir_onto_dir_merges_and_names_the_conflict`, `error.rs::conflict_is_go_http_200_with_names` |
| 9 | `copyFile` silently skips an existing destination | **fixed** — every collision is reported per file (`PasteReport::outcomes`), and Go's skip is reachable only as `ConflictPolicy::SkipExisting` | `contract_ops.rs::each_collision_policy_answers_a_taken_name` |
| 10 | `moveItem` errors on collision | **kept** (it is the correct behaviour), typed instead of HTTP 500 text | `error.rs::already_exists_is_go_http_500` + the `Overwrite`/`Rename` halves of the test above |
| 11 | Batch rename smuggled as a JSON string in `name` | **fixed** on the wire (`RenameTarget::Batch`); the undo log still stores Go's name-array text, byte-identically | `contract_ops.rs::a_batch_rename_renames_a_parent_and_its_children_and_logs_the_name_array` |
| 12 | `track_entries` only when ≤ 100, else `reload_required` | **kept** — `ops/paste.rs::TRACK_ENTRIES_CAP = 100` | the small-paste half is asserted (`contract_ops.rs:406`, `!report.reload_required`); the > 100 side has **no test** — the `op-paste-bulk.json` golden (104 sources) is the only evidence. Honest gap, step 16 |
| 13 | VFS write guard is a `.zip/` **substring** test | **fixed** — component-wise, so a real `fake.zip/` directory stays writable | `ops/guard.rs::archive_source_is_vfs_readonly`, `dest_text_guard_is_component_wise` |
| 14 | `validateDest` rejects any `..` **substring** | **fixed** — normalise, then test containment (so `dotdot..name` is allowed and `../escape` is not) | `path.rs::dotdot_substring_is_fine`, `resolve_rejects_escaping_and_accepts_dotdot_inside`, `archive/extract.rs::plan_resolves_a_dot_dot_inside_a_name_the_way_go_s_false_positive_did_not` |

Two open items sit *beside* the fourteen and are not verdicts on them: the unterminated-`"` filter
question (plan §6.2's last row, still undecided, pinned on the port side by
`services/search_index.rs::an_unterminated_quote_keeps_the_path_clean_the_port_side_of_a_known_divergence`),
and the `xtask parity` defect list (plan §9.2 step-15a/15c), which is about rendering, not behaviour.

## How these goldens were produced

* `scripts/capture-golden.sh` builds a fixture tree with `scripts/make-fixtures.py`, starts a
  *disposable* Go instance (`HOME=${HOME_ROOT}`, `-port 7891 -dev=false`, stripped `PATH` so the
  system clipboard is unreachable), then drives 122 HTTP captures plus 16 derived artefacts into
  `tests/golden/`. The fixture tree is wiped and rebuilt on every run, so the mutating `/api/op`
  sequence is reproducible.
* `tests/golden/PROVENANCE.json` pins the source of truth:
  `go_commit d5cd1a542167c56f06f2f585f0fd0cab26d719fc`, `go_binary_sha256 c4b68d7a…e9e44e94`,
  `platform "Linux 7.3.3-1-cachyos"`, `captured_at_utc 2026-11-18T08:52:22Z`.
* `tests/golden/INDEX.json` is the manifest: one row per capture with `name`, `http_status`,
  `content_type`, the request URL and the file. **Every status code quoted below comes from that
  row**, not from reading the Go source. `artifacts[]` explains each non-HTTP artefact, including
  five rows recording captures that were later deleted as byte-identical duplicates.

### Placeholders

`scripts/normalise-golden.py` replaces the two capture-machine roots in every artefact:

| Placeholder | Was | Appears in |
|---|---|---|
| `${FIXTURE_ROOT}` | `/tmp/zm-fix` — fixture tree, archives, `/ops` scratch dir | every `/api/dir`, `/api/search`, `/api/archive/*`, `/api/op`, `/api/log` capture |
| `${HOME_ROOT}` | `/tmp/zm-go-home` — the disposable `HOME` | `PROVENANCE.json`, `legacy-config-real.json` |
| `${CAPTURE_MEDIA}` | `/media/jang` — real mounts + the developer's real config paths | `places*.json` (`mounts[]`), `legacy-config-real.json`, `PROVENANCE.go_repo` |

Two artefacts are **deliberately not placeholdered**: `action.log` (its CRC-32 seals the original
bytes; the original strings are preserved in `LOG-PATHS.json` so a decoder test can reproduce them)
and `zen-man.db` (kept as the raw Go file — its `files.path` column literally contains
`/tmp/zm-fix/tree/…`, confirmed by a direct row read).

> **The golden bytes win over any prose here.** If this file and a golden disagree, the golden is
> right and this document has a bug; fix the document, never the golden. Two prose-level caveats:
> the JSON goldens were **re-indented with two spaces** by `normalise-golden.py`, so they are not
> byte-identical to Go's output (which is compact `json.NewEncoder` output); and one golden
> (`err-chmod-octaltext.json`) is not valid JSON at all, so it survives re-indenting untouched.
> Compare parsed JSON (or per-shape key order), never whole-file bytes.

## Conventions

### The error envelope

Every error is produced by Go's `http.Error(w, `+"`"+`{"error": …}`+"`"+`, code)`, which forces
`Content-Type: text/plain; charset=utf-8`. The goldens confirm it verbatim — all 24 error captures
carry that header (`INDEX.json`), while all 98 success captures carry `application/json`.

Consequences for `crates/zenman-core/src/error.rs`:

* The body is **hand-written JSON text, not `json.Marshal`**. Handlers interpolate `%v`, so error
  text is *unescaped*: `err-perm-denied-dir.json` embeds Go's `open …: permission denied`, and
  `err-archive-notfound.json` embeds a raw filesystem error.
* One case is **not parseable JSON**: `err-chmod-octaltext.json` — `{"error": "Invalid permission
  mode: strconv.ParseUint: parsing "0o755": invalid syntax"}` — because Go's `%v` printed the
  inner quoted operand (`api/ops.go:931-933`). This golden is a *negative* fixture: our typed error
  must not copy that shape.
* Two shapes of "envelope" exist. Most errors are a single key: `{"error": "…"}`. Two carry more:
  `err-vfs-readonly.json` (`{"error":"vfs_readonly","message":"Write operations not supported inside
  archives"}`, `api/ops.go:73,78`) and every *success* body uses its own key set. Codes with
  machine-readable sentinel values in `error` and **no** separate field: `"vfs_readonly"`,
  `"archive_modified"` (`api/archive_handlers.go:66`), `"zip slip detected: <name>"` (inside
  `ExtractJob.error`, `api/archive_extract.go:68-78`).
* `http.Error` appends exactly one trailing `\n`. That newline is part of the contract.

### Key order

* **Struct** responses keep Go field-declaration order: `DirResponse`
  (`api/dir.go:29-37`) → `path, entries, cursor, has_more, total_count, loaded_so_far`;
  `FileEntry` (`api/dir.go:17-27`) → `name, is_dir, is_symlink, size, mod_time, mode, rel_path,
  depth, files_count`; `ActionRecord` (`api/log.go:53-64`) → `id, timestamp, action, action_str,
  status, status_str, sources, dest, name, reversible`. All confirmed in `dir-root.json`,
  `log.json`.
* **`map[string]interface{}`** responses are alphabetised by `encoding/json`: `dir-size-subdir.json`
  (`files_count, path, size, status`), `places.json` (`bookmarks, mounts`),
  `search-index-*.json` (`capped, entries, index_dirty, last_indexed_unix, total_matched`),
  `archive-list-*.json` (`archive, entries, nested, path`), `props-*.json` (`platform,
  symlink_supported` — Go's source order is the other way round, `main.go:141-147`).
* Rust serde emits declaration order, so the map-shaped families need either a `BTreeMap` or
  `#[serde(rename_all)]`-sorted structs to match. Byte-comparing *both* families under one rule is
  wrong in either direction (plan §2.4 "Response key order").

### `[]` versus `null`

Go distinguishes a nil slice from an empty one and the goldens keep the difference:

| Field | Value when empty | Evidence |
|---|---|---|
| `/api/dir` `entries` | `[]` (`make(…, 0, …)`) | `dir-empty.json` |
| `/api/search` `entries` | **`null`** | `search-live-recursive.json`, `search-live-empty-q` (non-empty), plan §2.4 |
| `/api/index/search` `entries` | **`null`** | `search-index-empty.json` |
| `/api/log` `records` | `[]` (explicit `if records == nil`, `api/log.go:652-653`) | `log.json` |
| log record `sources` | `[]`, never `null` | `log.json` id 1 (a `mkdir`); hexdump `numSrc = 0x0000` |
| `/api/places` `bookmarks` | `[]` | `places-initial.json` |
| `/api/workspaces` `right_tabs` | `[]` (came from the request) | `config.json`, `workspaces-restore.json` |
| `/api/actions` body | `[]` (`if cfg.CustomActions == nil`, `api/actions.go:40-41`) | `actions-delete.json` |

`Vec<T>` in serde always serialises as `[]`, which is the deliberate improvement (plan §6.2). Keep
the distinction alive in tests: `null` here is an *observed* value, not an absence.

### Trailing newline

Every golden ends in `\n`, but for three different reasons, and only two of them are Go:

* `json.NewEncoder(w).Encode(...)` → compact JSON + `\n`. This is the overwhelming majority.
* `http.Error(...)` → the message + `\n`.
* `w.Write([]byte(`+"`"+`{"status": "success"}`+"`"+`))` (`api/ops.go:141,388,952`;
  `api/places.go:247,263`) → **no** trailing newline in Go. The captured `op-chmod.json`,
  `op-delete-file.json` and `workspaces-save.json` nevertheless end in `\n` because
  `normalise-golden.py` re-serialised them. So "does this endpoint end with a newline" is
  **not** recoverable from the goldens for the bare-status bodies — treat all of them as
  newline-terminated and assert it once, globally (see `xtask contracts`).

## Endpoints

Registered at `main.go:99-121`. `status counts` below come from `tests/golden/INDEX.json` (122 HTTP captures: 97×200, 9×404, 7×400, 4×500, 2×409, 1×405, 1×422, 1×403).

### `/api/dir` — list a directory

`api/dir.go`; handler head `:72-100`. Only `OPTIONS` is special-cased, so **any other method, including POST, returns the listing** (`dir-post-returns-listing.json`, byte-equal to `dir-root.json`).

Query (all optional):

| name | Go type | meaning | present when |
|---|---|---|---|
| `path` | string | directory to list; `ResolvePath` = `filepath.Abs` (`:58`) then `filepath.Clean` (`:63`) | required in practice |
| `limit` | int | page size, default **200** (`:106`) | always applied |
| `cursor` | string | decimal index into the fully re-read, re-sorted list (`:114`) | first page omits it |
| `sort` | string | `name` \| `size` \| `date` | `name` default |
| `order` | string | `asc` \| `desc` | `asc` default |
| `flat` | bool | recursive walk under `path` | off |
| `no_folders` | bool | with `flat`, omit directories | off |

Response: `entries[]`, `path`, `name`, `cursor` (next index, `""` when `has_more:false`, `:276`), `has_more`, `loaded_so_far` (`== endIndex`), `total_count` (pre-pagination), and `files_count` **only** when a `folder_sizes` cache row exists (`:250-257`) — the root cause of the frontend's `(undefined files)`.

`entries[]` fields: `name`, `path`, `is_dir`, `size`, `mod_time` (unix seconds), `mode` (see *fileMode strings*), `is_symlink` absent — symlinks are identified by the `L` mode glyph and report `is_dir:false` even for symlink-to-dir.

Status codes: 200, 400 invalid path (`:89`), 404 (`:96`), 400 not-a-directory (`:100`), 500 flat-walk failure (`:182`), **500 for permission denied** (`:189`).

Goldens: `dir-root`, `dir-subdir`, `dir-empty`, `dir-deep`, `dir-sortcase-asc|desc`, `dir-sort-size|date|date-desc`, `dir-many-p1|p2|last|smallpage`, `dir-flat-subdir|deep|nofolders|cap`, `dir-perm-noexec`, `dir-post-returns-listing`, `dir-search-term`, `err-nonexistent`, `err-not-a-directory`, `err-perm-denied-dir`.

```json
{"entries":[{"name":"a10.txt","path":"${FIXTURE_ROOT}/tree/a10.txt","is_dir":false,"size":3,
 "mod_time":1794991404,"mode":"-rw-r--r--"}, ...],"path":"${FIXTURE_ROOT}/tree","name":"tree",
 "cursor":"","has_more":false,"loaded_so_far":1214,"total_count":1214}
```

### `/api/dir/size` — folder sizes

GET only (`:325`; 405 at `:336`). Query `path`. Map response, therefore **alphabetised**: `files_count`, `path`, `size`, `status`. Side effect: it writes `folder_sizes` rows for every subfolder, which is why a later `/api/dir` on the same tree suddenly gains `files_count`. Golden `dir-size-subdir.json`.

### `/api/search` — live (filesystem) search

GET. Query `q`, `path`, `recursive`, `regex`, `limit` (default **500**; `limit <= 0` becomes **1 000 000**, `api/search.go:67-74`). Response struct: `entries`, `total_matched`, `capped` (set at `:220`). `entries` is **`null`** when empty and is **not** dirs-first (`search-live-empty-q.json` lists `a.txt` before `b`). `SearchEntry` here has no `is_symlink`. Errors: 400 missing `q` (`:46`), 404 (`:53`), 400 bad regex (`:57`, `:85`).

### `/api/op` — every mutation

POST only (405 at `:49-51`). Body `OpRequest{op, sources, dest, name, merge}` (`:19-24`). A path containing `.zip/`, `.rar/` or `.7z/` as a **substring** is refused **405** by the VFS guard (`:60-79`, `err-vfs-readonly.json`) — and because it is a substring test, a real directory named `fake.zip/` is also refused. Dispatch table `:83-116`, unknown op 400 (`:118`).

| `op` | handler | response | notes and evidence |
|---|---|---|---|
| `open` | `:83` | **no golden** | unexercised; the port must still define it |
| `copy` | `:89` | `{status, items:[{path, name, size, isDir}]}` | **camelCase `isDir`** in a snake_case API (`:171-182`); stat errors are swallowed (`:166-173`), so a vanished source reports `size:0` (`op-copy-link.json`) |
| `cut` | same helper | as `copy` | `op-cut.json` |
| `copy_inside` | `:96` | **no golden** | navigates into a folder's contents as the copy source |
| `cut_inside` | `:96` | **no golden** | idem |
| `clear` | — | `{status:"success"}` | empties the clipboard, `op-clear.json` |
| `paste` | `:256-280` | `{status, entries?, reload_required}` | conflict only for dir-into-dir and it is **HTTP 200** `{"status":"conflict","conflicts":["mergedir"]}` (`paste-conflict.json`); `merge:true` bypasses (`paste-conflict-merge.json`); `trackEntries = len(paths) <= 100` (`:303`) so `reload_required = !trackEntries` (`:363`) — `op-paste-bulk.json` (104 sources) has `entries:null, reload_required:true`; **`paste_link` has no `reload_required` key at all** (`:917-920`); `copyFile` **silently skips an existing destination** (`:649-651`) proven by `paste-skip-existing.json` returning size 14 for a 12-byte source; a `move` collision errors **500** `file already exists at destination` (`:710-721`, `err-paste-move-collision.json`) |
| `delete` | `:367-388` | `{status}` | `os.RemoveAll`: succeeds on an **empty list** and on a **nonexistent path** (`op-delete-empty-sources.json`, `op-delete-dir.json`) |
| `rename` | `:410`, `:423-438` | single: `{status, entry}`; batch: `{status}` | batch is smuggled as a **JSON array string in `name`** (`:431-438`); with exactly one source that JSON string becomes the **literal filename** (`op-single-rename-json-name.json` → `entry.name = ["a","b"]`); the partial-failure path `:457-467` has **no golden** |
| `mkdir` | `:501` | `{status, entry}` | logs `Append(ActionMkdir, []string{}, dest, name)` — hence `sources: []` |
| `paste_link` | `:917-920` | `{status}` | no `reload_required`; symlink support is platform-gated by `/api/props` |
| `chmod` | `:931` | `{status}` | `strconv.ParseUint(modeStr, 8, 32)`: `"600"`→200, **`"0644"`→200 (leading zero accepted)**, `"0o755"`→400 with an **invalid-JSON** body |

### `/api/places` — bookmarks

GET returns the bookmark list; POST `action:"add"|"remove"` (`api/places.go:124-190`). A POST with any other `action` is a **silent 200 no-op** returning the unchanged list (`:166-190`) — no golden for it, and no 405 golden either.

### `/api/workspaces`

GET list (`:213-222`). POST `action:"save"|"restore"|"delete"` (`:224-266`): missing `name` → 400 (`:236`); `restore` of a missing workspace → **404** (`:252`); any other action falls through to **405 `"Method not allowed"`** (`:268`). `action:"delete"` has **no golden**.

### `/api/actions` — custom actions

GET returns a **bare array**. POST `action:"create"|"update"|"delete"|"reorder"|"default"` → `{status, custom_actions}` (`:134`). Unknown `action` → **400 `"Unknown action"`** (`:124-126`) — note the asymmetry with `/api/workspaces`, which 405s. Unknown id on `update`/`delete` → 404 (`:78`, `:93`); `update`, `reorder` and the 404 cases have **no goldens**.

### `/api/action/exec`

POST only (`:151-152`). Body `{id, paths, dir}`; unknown id 404 (`:182`). Placeholders substituted: `{dir} {target_dir} {file} {files} {name} {names} {parent}` (`:229-235`). Runs `sh -c` with `cmd.Dir = req.Dir` (`:237-243`). With `show_output:true` → `{status:"success", output}` (`:255-258`), and on non-zero exit **HTTP 200** with `{"status":"error", "error":…, "output":…}` (`:248-253`); otherwise `{status:"success"}` (`:265-267`). The `show_output:false` and error-body cases have **no goldens**.

### `/api/props` — capability probe (misnamed)

`main.go:141-147`. Returns `{"platform":"linux","symlink_supported":true}` and **ignores `path` completely** — `props-file`, `props-dir`, `props-symlink`, `props-crlf-name` are byte-identical. It performs **no method check at all**, so PUT/DELETE are answered too. The Properties dialog therefore cannot be built from this endpoint; it needs `page_dir` + `dir_size` +
entry stat — which is exactly what step 13 built (`dialogs/properties.rs`), and why no `/api/props`
counterpart exists in the port: Go's own frontend never called this route at all (plan §9.2 step-13).

### `/api/log`, `/api/log/revert`

`GET /api/log`: GET only (405 `:620-621`); `limit` default **50**, clamped to **1…500** (`:625-630`); `path` filter (400 on a bad path `:637-640`); response `{records, count}` (`:656-659`). 405 and the `path`-400 cases have **no goldens**.
`POST /api/log/revert`: `{id}` → 404 (`:689`), **409** `already reverted` (`:693-694`), **422** `action 'copy' cannot be reverted` (`:699`), 500 (`:704`), 200 `{status:"reverted", record_id, action}` (`:713-716`). Only `copy` is captured for the 422.

### `/api/diskspace`

GET only (`api/diskspace.go:25`); `{free, total}` (key order verified in the golden; whether it is struct-declaration order or map order is **not determinable from the capture**). 400 on a bad path (`:32`) — but the captured bad-path probe returned **500** (`err-diskspace-badpath.json`), and there is no 400 golden.

### `/api/index/*`

| route | method | response | citation |
|---|---|---|---|
| `/api/index/search` | GET `q,limit,offset` | map: rows + `capped = totalMatched > limit+offset` | `api/search_index_api.go:57`; 500 at `:50`. `rel_path` holds the **absolute** path and `mode` is always `""` (`search-index-*.json`) |
| `/api/index/rebuild` | POST only (`:76-77`) | `{started:true}` | progress then via `status` |
| `/api/index/status` | no method check beyond `OPTIONS` (`:94-99`) | map `config, is_dirty, is_indexing, last_indexed, progress{indexed}, total_files` | `index-status.json` |
| `/api/index/config` | POST only (`:142-143`) | `{saved:true}` | decodes **only** `roots, excludes, auto_index, worker_count, follow_symlinks` (`:147-154`); `schema_version`, `max_depth`, `index_on_startup`, `index_state` are never read, so **omitted fields are zeroed on save** — the mechanism behind plan §5.3's note |
| `/api/index/cancel` | POST only (`:192-193`) | `{cancelled:true}` | |

Query forms **not** captured: `size:`, `mtime:`, `-negation`, quoted values, and `type:file`. Step 12
corrects the framing: Go's filter regex is `(\bext|\bin|\btype):(?:"([^"]+)"|(\S+))`
(`api/search_index.go:44`), so **`size:` and `mtime:` were never operators on either side** — the
capture list above is about what the harness did not send, not about a grammar the port skipped. The
port implements the same three operators (`ext:`, `in:`, `type:`, `type:file` included) plus quoted
values, and the only real divergence is the unterminated-quote case in plan §6.2's last row.

### `/api/archive/*`

`GET /api/archive/dir` — query `archive`, `nested`, `path`. **No pagination whatsoever**: no `cursor`, `has_more`, or `total_count` (`api/archive_handlers.go:110-117`). Entries come from `VirtualNode.ToFileEntry()` with a **hardcoded** `mode:"-r--r--r--"` and `rel_path = InternalPath` (`api/archive_vfs.go:23-32`). Sorting here *is* dirs-first + lowercase-name (unlike `/api/search`). Status codes: 400 (`:34`), **500 when the archive file is missing** (manifest read failure, `:54` — `err-archive-notfound.json` is 500, not 404), **409** `archive_modified` (`:66`), 404 dir-not-in-archive (`:87`), 400 not-a-dir-inside (`:92`, not captured). A zip-slip member surfaces as a **directory literally named `..`** (`archive-list-zipslip.json`, `api/archive_parser.go:110-152`); zip **symlink members come back as regular files whose size is the target path's length** (`archive-list-symlink.json`).

`GET /api/archive/stream` — raw `application/octet-stream`; golden `archive-stream-plain.txt` (15 B, no added newline). 400 (`:137`), 404 (`:163`, `:340`) — neither captured.

`POST /api/archive/extract` — body `{archive, nested, sources, dest}` (`:96-101`). `validateDest` (`:57-66`) rejects any **`..` substring** with 403 (`:118-121`) — so `name..txt` is refused (`archive-extract-dotdot-falsepositive.json`) — while a **relative** destination passes because `ResolvePath` has already made it absolute (`archive-extract-relative-dest.json` → 200 + `job_id`). `safeDestPath` does the real zip-slip containment (`:68-78`, `archive-extract-zipslip.json`). Response `{job_id}` (`:209`). 400 `No matching files to extract` and 405 not captured; the concurrent-extract cap of 3 and the 1 h job GC are **unexercised**.

`GET /api/archive/extract/status` — query `job_id`; 400 (`:221`), 404 (`:227`). Struct `ExtractJob`: `id, done, bytes_done, bytes_total, error(omitempty)`, with `CompletedAt json:"-"` (`:25-30`). Goldens `archive-extract-one-status.json`, `archive-extract-zipslip-status.json`.



## Cursor and pagination

`cursor` is a **decimal string index into a list that is fully re-read and re-sorted on every request** (`api/dir.go:111-115`, `:274`) — not an opaque token, not a keyset. The response's `cursor` is the index of the *next* item and is `""` when `has_more:false`; `loaded_so_far == endIndex`; `total_count` is computed before pagination (`dir-many-p1.json` cursor `"200"`, `dir-many-last.json` cursor `""`). Consequences the port must reproduce: concurrent edits between page fetches shift the index and silently skip or duplicate entries, and paging a large directory is O(n) per page.

Flat (`flat=true`) listings cap at **10 000** entries (`:132`, `:176`; proven by `dir-flat-cap.json`: `total_count:10000` from a 10 005-file tree).

Endpoints with **no** pagination: `/api/archive/dir`. `/api/log` takes `limit` only. `/api/index/search` uses `limit`+`offset` rather than a cursor.

## fileMode strings

Every distinct value observed in the goldens, all produced by `info.Mode().String()` at `api/dir.go:235-247`:

| mode | meaning | golden |
|---|---|---|
| `-rw-r--r--` | regular file | most files |
| `-rwxr-xr-x` | executable | `executable.sh` |
| `----------` | mode `000` file | `noperm.bin` |
| `drwxr-xr-x` | directory | `dir-root.json` |
| `dr--r--r--` | directory without execute (unreadable entries) | `dir-perm-noexec.json` |
| `d---------` | directory with no permissions | `noread-dir` |
| `Lrwxrwxrwx` | symlink — **capital `L`, not `ls`-style `l`** | `dir-root.json` (`link-to-dir`) |
| `prw-r--r--` | FIFO | `fifo` |
| `-r--r--r--` | **hardcoded** for archive members | every `archive-list-*.json` |
| `""` | always, for index rows | every `search-index-*.json` |
| `unknown` | `entry.Info()` failed | `dir-perm-noexec.json` |

The `unknown` path also leaves `size:0` and `mod_time:-62135596800` (Go's zero `time.Time` in seconds). So a degraded listing is **HTTP 200 with per-entry `mode:"unknown"`**, not an error — the port needs an explicit `Mode::Unknown` and an `Option<DateTime>` mtime. Setuid/setgid/sticky glyphs (`S`, `T`) never appear in the goldens because no such file was created on purpose, so `ModeBits`' high-bit handling is untested against Go.

## ZMLG undo log

Layout (`api/log.go:18-83`, cross-checked against `LOG-HEXDUMP.txt` and `LOG-RECORDS.json`):

```
"ZMLG" | i64be id | i64be unix_nanoseconds | u8 actionType | u8 status | u32be recLen
body: [u16be len][bytes] sources… | 0x0000 | [u16be len][bytes] dest | [u16be len][bytes] name
trailer: u32be CRC32/IEEE over everything before it
recLen = magic+header+body (CRC excluded); total_len = recLen + 4

*Corrected by step 5 against the bytes (this table previously labelled `recLen` as `CRC32` in the header row). Also measured there: no file header, no record count, **no end sentinel**; an empty log is a 0-byte file (`api/log.go:143`), not `[0xA5,0x5A]`; `/api/log` wraps as `{"count":N,"records":[…]}` (never `total`/`entries`, and `records` is `[]` when empty — `api/log.go:685-687`); a revert is an **in-place patch** of the status byte + re-sealed CRC (`api/log.go:355-386`), never a rewrite into a revert directory, and the file size does not change; the frame stores **nanoseconds** with no zone, so `log.json`’s `+07:00` is the capture host’s local zone and is not reproducible text; Go decodes *any* `actionType`/`status` byte (`api/log.go:486`, `""` for unknown action, `"done"` for any status but 1), so both are newtypes over `u8` rather than `#[repr(u8)]` enums if byte-identical re-encode is required. The golden `action.log` holds **19 records, 1 revert marker** (7 661 B, last frame ends at EOF).
```

Big-endian throughout. `recLen` excludes the CRC field; `verifyCRC` (`:591-597`) covers every byte before it, **so the revert patch must recompute the CRC** — and demonstrably does: in the captured post-revert log, record 1 has `status:1` with `crc_ok:true`. The file is opened `O_RDWR|O_CREATE` **without `O_APPEND`**, appending via `Seek(0, io.SeekEnd)` (`:142`, `:245`). Path resolution: `$XDG_DATA_HOME|~/.local/share/zen-man/action.log`, falling back to `/tmp/zen-man` (`:122-131`). `isReversible = {PasteCopy, PasteMove, Rename, Mkdir, PasteLink}` (`:600-602`).

Observed record offsets (all 19, from `LOG-RECORDS.json`; file size 7661 = 7567 + 94): `0, 59, 132, 197, 282, 350, 438, 503, 574, 643, 713, 799, 884, 949, 1082, 1165, 4319, 7493, 7567`. Record 1 (id 1, `mkdir`, `numSrc=0`) is the reverted one — the hexdump shows `00 00 | 00 0f …/tmp/zm-fix/ops | 00 08 made-dir`. Batch rename is stored as `name = "[\"renamed-one.txt\",\"renamed-two.txt\"]"` (id 14, `rec_len 129`); the revert-side sniff is `api/log_revert.go:110`.

**Codec test gaps** (the frozen log cannot cover these, so `contract_history.rs` in step 5 must synthesise them): a bad-CRC record, a truncated tail, a magic-resync log, a `u16::MAX`-length (65 535-byte) path field, non-UTF-8 path bytes, and reverts of `paste-copy` / `paste-move` / `rename` / `paste-link` — the capture contains exactly one reverted record and it is a `mkdir`.

## SQLite storage

Dumped with `sqlite3 tests/golden/zen-man.db` (schema as in plan §2.3, confirmed verbatim). Row counts: `files` 1260, `files_fts` 1260, `files_fts_content` 1260, `files_fts_docsize` 1260, `files_fts_data` 19, `files_fts_idx` 17, `files_fts_config` 1, `folder_sizes` 31, `schema_meta` 1 (`search_schema_version = '1'`).

Evidence of the shadow→live swap: **no `files_shadow` / `files_fts_shadow` tables remain**, yet all five indexes are still named `idx_files_shadow_{path,name,ext,parent,mtime}` and are attached to `"files"` in `sqlite_master` — the rename keeps index names (`api/search_index_walker.go:227-241`). FTS5 shadow tables present: `files_fts_{content,data,docsize,idx,config}`.

Pragmas on the captured file: `journal_mode = delete`, `page_size = 4096`, `user_version = 0`. Go sets **only** `SetMaxOpenConns(1)` (`api/db.go:49`) and issues `PRAGMA wal_checkpoint(PASSIVE)` (`api/search_index_walker.go:250`), which is a **no-op because WAL was never enabled**. A repo-wide grep for `PRAGMA|busy_timeout|synchronous` finds nothing else. `[UNCERTAIN]` `api/db.go` past `:60` was not read, so "no `busy_timeout` anywhere" rests on that grep rather than a full read. Driver name is `"sqlite"` (`api/db.go:44`), i.e. the modernc pure-Go driver — `rusqlite` uses SQLite C, which is a **behavioural change** (locking granularity, FTS5 availability) that step 5's "open the Go db unchanged" test is the gate for.

Column conventions: `name` is lowercased, `name_orig` keeps the original, `ext` is lowercased without the dot, `mtime` is **seconds**, `parent` is an absolute path.

## Config and session files

`tests/golden/config.json` (written by the disposable instance) contains exactly the three Go fields:

```json
{"bookmarks":[], "workspaces":{"fixture":{"left_tabs":[{"id":"t1","path":"…","name":"Fixtures",
 "group":"A","color":"#ff4a4a"}],"left_active":"t1","right_tabs":[],"right_active":""}},"custom_actions":[]}
```

`tests/golden/legacy-config-real.json` (a read-only copy of the real app's file) additionally proves `""` — not absent — for `group` and `color`, and multi-tab lists with **pre-placeholder absolute paths**.

| Go (`config.json`) | Rust `ZenConfig` (plan §5.4) | migration note |
|---|---|---|
| `bookmarks[]` | `bookmarks` | 1:1 |
| `workspaces{name:{left_tabs,left_active,right_tabs,right_active,split}}` | `workspaces{name:{panes:BTreeMap<PaneSlot,PaneSession>, active, layout, dock}}` | `split:true` → `LayoutMode::Split`, absent → `Single`; `left_*`/`right_*` → `PaneSlot::{Left,Right}`; `PaneSlot::LeftBottom/RightBottom` have **no Go counterpart** and start empty |
| `TabState{id,path,name,group,color}` | `TabSession{id,path,name,group:Option<GroupColor>,view}` | `group:""` → `None`; `color` is **dropped** (single source of truth becomes `GroupColor::color()`, §2.3) — a real data loss to record in the migration report; `view` has **no Go counterpart** and defaults to `List` |
| absent | `schema_version`, `indexer` | defaulted; `indexer` seeds from `search-config.json`, not from `config.json` |
| `custom_actions[]` | same | 1:1 |

`tests/golden/search-config.json` has **10** fields including `index_state{last_indexed_unix,total_files,is_dirty}`, but the `config` embedded in `/api/index/status` is a **5-field subset** — so `ZenConfig.indexer` cannot round-trip through `/api/index/status`; read the file, not the status payload.

### Session save and restore

The session is **not a file of its own**: it is `workspaces.default` from the table above — the key the
Go app autosaved to (`api/places.go:26`, `frontend/js/workspace.js:103-133`) and the key the shell
writes through `WorkspaceService::save`, so an existing `config.json` restarts the Rust port with the
tabs the Go app left behind.

| event | behaviour | difference from Go |
|---|---|---|
| tab / pane / layout change | `workspaces::Autosave` notes the state; 1.5 s of quiet writes `config.json` atomically (tmp+rename) | same debounce, no timer thread (core may not depend on `craft-settings`) |
| clean exit | `eframe::App::save` → `ZenApp::flush_session` + `Persisted::flush`, **debounce ignored** | Go had no shutdown path at all, so a quit inside the window lost the last tab opened |
| boot, no path argument | `workspaces.default` is applied: pane set, tab strips, selected tab, per-tab group/view, and each pane's tab groups with their labels and folds; the session's own divider tree when it has one, and otherwise the shell settings' tree stands **while the pane set is unchanged** (a Go-migrated session has no tree, and a different grid shape rebuilds it) | Go restored `left/right` tabs + `split` only; a quad session was beyond its config, and it had no group beyond a letter |
| boot, with a path argument | the argument wins and no session is applied | n/a — Go accepted no paths |

A stored tab is `TabSession{id, path, name, group, groupId, view}`, where `path` is the **store string** —
`FsPath::Display`, which round-trips an archive member (`/a/b.zip/inner`) through `FsPath::resolve`;
the host path alone would silently delete every archive tab at the next restart. On restore, a path
that is not absolute is **skipped and reported** (Go resolved one against its own server cwd,
`api/dir.go:39-44`, which this port has no honest equivalent for); a path that is absolute but gone
keeps its tab and lists an error, which is what Go did. Only the selected tab is read during the
restore; the rest of the strip is queued for the next frame — Go's own `isLazy` rule
(`workspace.js:32-35`) — which is also what keeps an archive on an unresponsive mount from delaying
the window.

#### Tab groups (`groups`, `groupId`)

A pane's tab groups ride the same object: `PaneSession.groups` is a list of
`TabGroupSession{id, name, group, collapsed}` (`docs/tab-groups-design.md`), and `TabSession.groupId`
names the row its tab belongs to.

| key | who wrote it | what it means | what Go does with it |
|---|---|---|---|
| `group` | Go (`a`…`e`), and the port writes it on | the group's **colour**, which is what the port paints and Go paints | reads and writes it; `F`…`I` (the port's four extra palette slots) are not its letters, so it reads those as no group |
| `groupId` | the port only | the group's **identity** — `g<n>`, opaque like `id` | ignored on read, **dropped on write**: a Go re-save loses the labels and folds, and `group` is what survives so the grouping itself comes back |
| `groups[]` | the port only | one row per group: label (`""` = name it after the colour), colour, fold | same: dropped |

A group id is stored as the number the shell minted (`g41`), and a restore **keeps** an id it can parse
and moves its allocator past it. That is not tidiness: `Autosave` compares the session by value, so a
restore that renamed its own groups would be dirty the moment it started and would rewrite the file.

Restore repairs rather than refuses, because the file may have been written by something else:
clashing ids keep the first row; a tab naming a missing row falls back to its `group` letter rather than
losing membership; a letter-only session (any Go one) becomes **one unnamed, expanded group per
contiguous run of that letter**, since Go had no group object to name; a row with no members is dropped;
a row with no colour takes the first colour no other row of that pane holds, and past nine of them colours
repeat — colour is an attribute, identity is `groupId`.


`--session <FILE>` reads a **session file**: one entry of the `workspaces` object, so copying
`workspaces.default` out of a `config.json` makes one (`WorkspaceService::read_session`). It is read,
never written, and it outranks the autosave and the path arguments; the autosave keeps writing
`workspaces.default`.

One, two and four panes are the dock trees `dock::layout_for_slots` can build. A session naming any
other set restores as the grid its `layout` names and **counts** the tabs that had nowhere to go, in
the report line and in a notice, rather than painting a leaf that has no pane behind it.

### Where the files live

| layout | trigger | paths |
|---|---|---|
| override | `--data-dir`/`--config`, `ZENMAN_DATA_DIR`/`ZENMAN_CONFIG_DIR` | as given; `--data-dir` takes the cache with it, `--config` alone does not |
| **portable** | `portable.dat` (or `zenman.portable`) beside the executable, contents ignored | `<exe>/data` for settings **and** data, `<exe>/data/cache` for scratch — none of this app's files under `$HOME`. On macOS the marker is read beside the `.app` and the tree goes there too: writing inside a signed bundle breaks the signature (the rule craft-libs uses, `crates/settings/src/paths.rs:141-158`) |
| platform | otherwise | `directories::ProjectDirs` for `io.storytold.zenman`; `XDG_*` honoured (Go hard-coded `~/.config`, plan §5.1) |

An explicit override outranks the marker, and a marker whose folder is unwritable is a warning on
stderr plus a fallback to the platform layout — never a refusal to start (`app_dirs::Layout`, reported
by the window's `layout:` boot line). Portable mode moves this app's own files; the read-only Go legacy
preview (`config_boot::legacy_preview_note`) still looks at `~/.config/zen-man/config.json`, because
reading that file is the whole of its job.

## Deviation ledger

Plan §2.3's fourteen behaviours, each against its golden:

| # | Quirk | Verdict | Evidence | Port decision |
|---|---|---|---|---|
| 1 | Dirs always first, regardless of order | CONFIRMED | `dir-root.json` (8 dirs first); `dir-sort-date-desc`, `dir-sort-size` | keep |
| 2 | Name sort = byte compare of lowercased names | CONFIRMED | `dir-root.json` `a10.txt` before `a9.txt`; `dir-sortcase-asc/desc` | keep |
| 3 | Cursor = decimal index into a re-read, re-sorted list | CONFIRMED | `dir-many-p1/p2/last/smallpage` | keep (documented as a known flaw) |
| 4 | Flat walk caps at 10 000 | CONFIRMED | `dir-flat-cap.json` `total_count:10000` from 10 005 files | keep |
| 5 | Symlinks report `is_dir:false` | CONFIRMED | `dir-root.json` `link-to-dir` `Lrwxrwxrwx` | **fix** (real `file_type`) |
| 6 | Hidden files never filtered | CONFIRMED | `.hidden` in `dir-root.json`, `dir-post-returns-listing.json` | keep |
| 7 | chmod accepts bare octal only | CONFIRMED, **plan imprecise** | `op-chmod.json` `"600"`→200, `op-chmod-leading-zero.json` `"0644"`→**200**, `err-chmod-octaltext.json` `"0o755"`→400 | validate `^[0-7]{3,4}$`; symbolic modes are our addition |
| 8 | Paste conflict only dir-into-dir, HTTP 200 | CONFIRMED | `paste-conflict.json`, `paste-conflict-merge.json` | **fix** (typed `PasteOutcome::Conflict`) |
| 9 | `copyFile` silently skips an existing destination | CONFIRMED | `paste-skip-existing.json` size 14 (pre-existing) vs 12 (source) | **fix** (reported per file) |
| 10 | `moveItem` errors on collision | CONFIRMED | `err-paste-move-collision.json` 500 | keep behaviour, type the error |
| 11 | Batch rename smuggled as a JSON string in `name` | CONFIRMED + sharp edge | `op-batch-rename.json` (log rec 14); `op-single-rename-json-name.json` | **fix** (`RenameTarget::Batch`) |
| 12 | `track_entries` only ≤100, else `reload_required` | CONFIRMED | `op-paste-bulk.json` (104) vs `op-paste-copy.json` | keep |
| 13 | VFS guard is a `.zip/` substring test | CONFIRMED | `err-vfs-readonly.json` 405 for the **real** directory `tree/fake.zip/inner.txt` (`dir-root.json` has `fake.zip`, `is_dir:true`) | **fix** (component-wise, `ops/guard.rs`) |
| 14 | `validateDest` rejects any `..` substring | CONFIRMED, weaker than the plan stated | `archive-extract-dotdot-falsepositive.json` 403 for `dotdot..name`; `archive-extract-relative-dest.json` 200 | **fix** (normalise then test containment, F19) |

## Fixtures we cannot generate

`7z`, `7za`, `7zr`, `rar`, `unar` are **absent** from this machine; `unrar` and `bsdtar` are present but `unrar` is read-only, so **no `.rar` can be created at all** and there is no `.rar` golden — the RAR decode path has zero coverage, which leaves plan R-D7 (the `rar` feature flag and its `deny.toml` pairing) unverifiable on this host. The only `.7z` is a **26-byte signature stub** (`inner.7z` inside `nested.zip`, `archive-list-nested.json` `size:26`), so the real 7z decode path has no positive golden either. No setuid binary was created on purpose (no root-owned artefacts), so the `S`/`T` mode glyphs are untested. No encrypted archive was created, so `ArchiveEncrypted`/422 (`api/archive_handlers.go:51`) is unevidenced.

## Test gaps to close in later steps

| Gap | Step | Owner file |
|---|---|---|
| Synthesised bad-CRC, truncated-tail, magic-resync, 65 535-byte-path and non-UTF-8 log records | 5 | `tests/contract_history.rs` |
| Reverts of `paste-copy`, `paste-move`, `rename`, `paste-link` (only `mkdir` is frozen) and the 422 for `delete`/`chmod` | 5, 9 | `contract_history.rs`, `contract_ops.rs` |
| `busy_timeout` / WAL behaviour when opening **Go's** db | 5 | `store/sql.rs`, `index/schema.rs` |
| `Mode::Unknown`, zero-mtime → `Option`, setuid glyph handling | 3 | `mode.rs`, `entry.rs` |
| Real `.7z` and `.rar` decoding; encrypted-archive refusal; archive symlink mode bits | 13 | `archive/reader.rs` (needs the tools installed, or committed fixtures with an attribution/licence review) |
| Archive listing pagination (an improvement with **no Go counterpart**, so nothing to compare) | 13 | `services/archive.rs` |
| `/api/op` `open`, `copy_inside`, `cut_inside`; batch rename partial-failure; `action/exec` `show_output:false` and non-zero exit; `/api/workspaces` `delete`; `/api/actions` `update`/`reorder`/404 | 9, 14 | `contract_ops.rs`, `services/actions.rs` |
| Index query grammar: `size:`, `mtime:`, `-negation`, quoted values, `type:file` | 12 | `services/search_index.rs` — **closed by step 12 for what Go actually has**: `ext:`/`in:`/`type:` + quoted values are implemented and tested; `size:`/`mtime:`/`-negation` are not operators in Go's regex either, so they are absent by contract, not by omission |

## Addenda beyond plan §2.4

Measured here and not recorded in the plan's table; the plan has been updated to reference this section.

1. `/api/actions` unknown `action` → **400**, while `/api/workspaces` unknown `action` → **405** (`api/actions.go:124-126` vs `api/places.go:268`). The port maps both to one `CoreError::BadRequest`, so this asymmetry is a Go-only artefact — but any contract test asserting a status code must know which side it is on.
2. `/api/props` performs **no method check whatsoever** (`main.go:141-147`), which is stronger than "it is only a capability probe".
3. `/api/places` with an unrecognised `action` is a **silent 200 no-op** (`api/places.go:166-190`).
4. `/api/index/config` decodes only 5 of the 10 stored fields, so an omitted field is **zeroed on save** (`api/search_index_api.go:147-154`).
5. Result caps: `/api/log` `limit` default 50, clamped 1…500 (`api/log.go:625-630`); `/api/search` `limit` default 500 and `<= 0` becomes 1 000 000 (`api/search.go:67-74`); `/api/index/search` caps on `limit + offset`.
6. Archive listings **are** dirs-first + lowercase-name sorted (`api/archive_handlers.go:102-108`) — unlike `/api/search`. So `services/archive.rs` can share the comparator in `ops/list.rs`.
7. `/api/archive/dir` on a **missing archive file returns 500**, not 404 (manifest read failure).
8. Counting correction: `tests/golden/` holds **134 files** (122 captures + 12 artefacts on disk; `INDEX.json` lists 138 records because 5 of them are `superseded-by:` tombstones for deleted files). Plan §2.4's "139 artefacts" is wrong.

## Known citation drift in plan §2.4

`ResolvePath`'s Abs+Clean is at `api/dir.go:58` and `:63` (the plan cites `57-62`); `copyFile`'s skip is at `api/ops.go:649-651` (the plan cites `648-651`). `api/search_index.go:381-386`, `:16-32`, `:141-192` and `api/search_index_walker.go:227-241` were **not re-read** here — the facts they support are independently proven by the goldens and by the db dump.

