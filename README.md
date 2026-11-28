# zen-man-rs

A Rust/[egui](https://github.com/emilk/egui) port of the `zen-man` file manager
(originally Go + browser UI). There is **no web server and no REST layer**: the domain logic is a
headless library (`zenman-core`), the UI is an egui shell (`zenman-ui`), and `apps/zenman` is a thin
binary. The undo log (`ZMLG` `action.log`) and the SQLite index are byte-compatible with the Go app,
so an existing data directory keeps its history across the port.

## Build, run, test

```sh
./scripts/build.sh                                 # preflight + fetch --locked + release build
./scripts/build.sh --check                         # "can this machine build it?", nothing compiled
./scripts/build.sh --smoke                         # ... and prove it boots (--safe-gpu --smoke 1)
./scripts/build.sh --install                       # -> $PREFIX/bin/zenman (default ~/.local/bin)
./scripts/build.sh --dist                          # -> target/dist/zenman-<ver>-<triple>.tar.gz
```

The script is the short path; the underlying commands are all ordinary cargo:

```sh
cargo build --release -p zenman                    # -> target/release/zenman (~31 MB, single file)
cargo build -p zenman
cargo run  -p zenman                     # opens the window on a real GPU; falls back per gpu_startup
```

What a build machine needs (the script checks each of these and says which one failed): a Rust
toolchain at the declared MSRV with `rustfmt`/`clippy`, and a **C compiler** — `rusqlite` is `bundled`
so the SQLite amalgamation (FTS5 + trigrams) compiles in, which is why the binary needs no distro
`libsqlite3`. Nothing else: every `-sys` crate in the graph is `dlopen`-based (`x11-dl`, `xkbcommon-dl`,
`wayland-sys`, `ash`), so the X11/Wayland/GPU libraries are **runtime** requirements, not build ones.
The fonts and icons ship embedded (`include_bytes!`), so the executable is self-contained; the
`ATTRIBUTION.md` that goes with it ships in the `--dist` tarball.

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

## The icon on all three desktops

`zenman` embeds its own icon, so the window, the taskbar button, alt-tab and (on macOS 11+) the Dock
tile are right from a bare `cargo run` — no `.desktop` file, no installer, no bundle:

| | what carries it |
|---|---|
| Windows | the embedded PNG (`WM_SETICON`) + `assets/app-icon/zenman.ico` compiled into the `.exe` |
| macOS | the embedded PNG (`setApplicationIconImage`), plus `zenman.icns` inside `ZenMan.app` (`scripts/make-app-bundle.sh`) |
| Linux / X11 | the embedded PNG via `_NET_WM_ICON` |
| Linux / Wayland | needs a `.desktop` file — which the compositor, not the process, reads. Opt in: `zenman --install-desktop-entry` writes the entry *and* the hicolor icons into `$XDG_DATA_HOME`, idempotently, and never overwrites a file it did not generate |

The art in `assets/app-icon/` is generated from `assets/icons/prototype-icon-2.png` by
`scripts/make-app-icon.py`; `cargo xtask app-icon` re-derives it and fails if a committed artefact has
fallen behind. Details: [`docs/app-icon.md`](docs/app-icon.md).

## Your tabs come back, and portable mode

**Session restore.** Every tab strip, the selected tab, the grid (one-, two- or four-pane), the divider
positions, and each tab's group colour and view mode are autosaved to `workspaces.default` in
`config.json` — the same key the Go app used, so a `config.json` written by `zen-man` restarts this port
with the tabs `zen-man` left open. The write is debounced 1.5 s (Go's own interval) and flushed again on
a clean exit, so closing the window immediately after opening a tab does not lose it; an idle window
writes nothing. Launch with **no argument** and that session is your window. Launch `zenman <folder>`
and the folder you named is: the session is not merged into that window, and the run then autosaves the
window it opened, which is what the next no-argument launch brings back.

```none
boot note: session: none saved yet                                  ← first run on this tree
boot note: session: restored 6 tab(s) in 2 pane(s)                 ← the next one
boot note: session: restored 5 tab(s) in 2 pane(s), 1 tab(s) with an unreadable path
```

A stored tab whose path this build cannot read (a relative or `~`-prefixed string — the shape Go wrote
when it saved an archive's internal path) is skipped and counted in that line, never opened at whatever
directory the process happened to start in. A tab whose directory is simply gone is **kept**: it lists
an error, which is where you left it and what Go did.

**A named window.** `zenman --session <FILE>` opens the session in `FILE`, which is one entry of
`config.json`'s `workspaces` object — copy `workspaces.default` out of the settings file to make one.
The file is read, never written, and it outranks both of the above; the autosave still writes
`workspaces.default` where it always did.

**Portable mode.** Create an empty marker beside the executable:

```sh
touch target/release/portable.dat
```

Everything **this app** writes then lives in `target/release/data/` — `config.json` (bookmarks, actions,
the session), `config.toml` (window/dock settings), `ui.ron`, `action.log`, `zen-man.db`, `gpu/`,
`run/`, `cache/` — and nothing of it under `~/.config` or `~/.local/share`. (`--migrate-config` still
reads the Go app's `~/.config/zen-man/config.json`, because reading that file is its whole job, and
`zenman.portable` is accepted as a synonym for the marker.)

| in order of precedence | trigger | effect |
|---|---|---|
| override | `--data-dir`/`--config`, `ZENMAN_DATA_DIR`/`ZENMAN_CONFIG_DIR` | the paths you named; outranks the marker |
| portable | `portable.dat` beside the binary | `<exe>/data`, including the cache (beside the `.app` on macOS) |
| platform | default | `io.storytold.zenman` app dirs, `XDG_*` honoured (Go's hard-coded `~/.config` was a real bug) |

A marker whose `data/` folder cannot be written (a read-only USB stick) is a warning on stderr and a
fallback to the platform layout, never a refusal to start. The boot report prints which layout won —
`layout: portable (everything beside the binary)` — next to the three paths in use, on stdout and in the
window.

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
