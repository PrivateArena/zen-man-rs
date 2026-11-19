# zen-man-rs

A Rust/[egui](https://github.com/emilk/egui) port of the `zen-man` file manager
(originally Go + browser UI). There is **no web server and no REST layer**: the domain logic is a
headless library (`zenman-core`), the UI is an egui shell (`zenman-ui`), and `apps/zenman` is a thin
binary. The undo log (`ZMLG` `action.log`) and the SQLite index are byte-compatible with the Go app,
so an existing data directory keeps its history across the port.

## Build, run, test

```sh
cargo build -p zenman
cargo run  -p zenman                     # opens the window on a real GPU; falls back per gpu_startup
```

Headless / CI:

```sh
cargo xtask ci                                       # the gate: fmt-check, clippy -D warnings, tests,
                                                     # deps, licenses, artefact + IO guards, cross rung
xvfb-run -a cargo run -p zenman -- --safe-gpu --smoke 1   # the only sanctioned window test
cargo test -p zenman-core --features watch           # the watcher lane (xtask ci gates this too)
cargo xtask licenses                                 # licence/attribution gate (deny.toml as data)
cargo xtask parity                                   # reference/baseline pairing gate + review index
```

Cross-target: `xtask ci` runs a `x86_64-pc-windows-msvc` rung. On a Linux host without an MSVC
toolchain it reports the documented **PARTIAL** (bundled SQLite needs `lib.exe`/`cl.exe`; that is a
toolchain block, not a code failure — see plan §9.2 step-16a). The `#[cfg(not(unix))]` arms of
`platform/linux.rs` are compile-proven only; run the hard rung on a Windows host once.

MSRV: `rust-version = "1.95"` is **declared, not build-verified on the dev box** — the pinned
channel is absent there (`rust-toolchain.toml` explains why the channel pin is `stable` instead).

## Feature status (plan §7, all 16 steps executed)

| Step | Deliverable | Acceptance evidence |
|---|---|---|
| 1 | scaffold, gates, GPU-safe boot | `xtask ci`/`licenses`; `--safe-gpu` smoke; `gpu_startup::Sentinel` ladder |
| 2 | golden fixtures frozen | `tests/golden/**` (134 files), `${FIXTURE_ROOT}` normalisation + CI machine-path guard |
| 3 | value types | `path::tests` 20-case VFS table; `mode::Display` vs Go `fs.FileMode.String()` |
| 4 | boot plumbing | Go `config.json` import leaves `.bak`; second instance exits 0 after focus |
| 5 | persistence | `contract_history.rs` byte-identical `action.log` re-encode; Go `zen-man.db` opens |
| 6 | jobs/session skeleton | `Session::open` survives corrupt log / locked db / garbage config (3 notes) |
| 7 | shell skeleton + assets | icons + `ATTRIBUTION.md` same commit; `01-empty.png`; colour-literal guard |
| 8 | file list | `contract_dirs` goldens; `perf_frame.rs` (list 190 µs / grid 303 µs medians) |
| 9 | write path | `contract_ops.rs`; every mutation logs Go-parity + emits `DirChanged{origin}` |
| 10 | menus, dialogs, keys | every `MenuContext` + chord asserted; F2 in-cell rename |
| 11 | live search + quick-find | `contract_search::live_walk_limits`; ≤1 walk per 250 ms; inline regex errors |
| 12 | index (+ `watch` feature) | interrupted rebuild keeps the old index; incremental upsert/prune; watcher gated |
| 13 | archives | `contract_archive`; zip-slip refused; extract progress/cancel; Properties everywhere |
| 14 | workspaces, actions, batch rename, action log | contract tests; autosave survives `kill -9` |
| 15 | parity harness | `xtask parity`: 13 matched, 0 missing/orphan/unmatched, 3 mislabelled refs noted |
| 16 | platform + hardening + release gate | `contract.rs` §6.2 full sweep (14 tests); frame/job panic boundaries; Windows platform layer + cross rung |

## Deliberate deviations you will notice

* **Trash-first deletes** — `Del` goes to the bin; a volume without a native bin (measured per path)
  *refuses* rather than silently destroying. `Shift+Delete` / the preference opts into permanent.
* **`--` for unknown sizes/counts** — never a fake `0` (Go's `(undefined files)` bug, decision D11).
* **`Saved` without a `✓`** — the pinned font stack has no U+2713 and this egui fork panics on
  missing glyphs; the chip text is deliberately glyph-free.
* **`KiB/MiB/GiB`** — binary units labelled honestly (Go divided by 1000³ and called it GB).
* Sort is clickable, listing sort is stable, arrow/Home/End/PageUp/Esc navigation added, `Del`/menus
  everywhere — the full list with its guard tests is plan §6.2.

## Known gaps (all verified real at this commit)

| Gap | Where the fix belongs |
|---|---|
| D7: quad-pane body overflows its dock rect by ~35 px | dock/content width arithmetic (fork `docking` + `PaneDock::body`), not `file_pane` |
| `file_pane::grouped_walks…` intermittent flake (2 s `settle_until` deadline) | `zenman-ui/tests/common.rs` deadline/pattern |
| quirk 12: the >100-entry `reload_required` side has no test (golden-only evidence) | `crates/zenman-core/tests/contract_ops.rs` |
| unterminated `"` in a search filter — decision OPEN, do not silently "fix" either side | plan §6.2 open row (`services/search_index.rs` pins the port side) |
| Windows `disk_space`, `.lnk` shortcuts, `GetDriveTypeW` removable-ness are typed refusals | needs a licence-cleared owner of the `unsafe` FFI (workspace forbids `unsafe`) or stable `Metadata::file_system` |
| Windows filesystem watcher unwritten (`CoreError::Watcher` there) | `index/watcher` Windows backend (`ReadDirectoryChangesW`) |
| snapshot artefacts `06`/`07` are machine-specific (real `statvfs`, real `$HOME`) | keep out of cross-machine pixel comparison; `xtask snapshots/parity` |
| no `Session::paste_preflight` — cold-destination conflict prompt falls back to `SkipExisting` | core session (collision half of `ops::paste::plan`) + `dialogs/confirm` |

## Licence & attribution

Licences are gated, not promised: `cargo xtask licenses` enforces the Craft allowlist over
`cargo metadata`, and every font/icon/asset has an entry in `ATTRIBUTION.md` in the same commit that
shipped it (house rule; the gate checks it). The port keeps the Go app's undo-log and database
formats byte-compatible — see `docs/contracts.md` for the wire contract and its quirk verdicts.
