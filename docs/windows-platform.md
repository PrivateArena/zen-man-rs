# The Windows platform layer — step 16a log

What `zenman-core` answers on Windows, how it was measured, and what is knowingly left to step 16b.
The plan's authority for this is §5.2's `platform/windows.rs` row and §10 item 4 (**Windows is
first-class**; the fallback is `todo!()`-free typed refusals, which is what the two refusals below are).

## 1. The measured cross-build state

| Command (at HEAD `23c0611`, before this step) | Result |
|---|---|
| `cargo check -p zenman-core --target x86_64-pc-windows-msvc` | **never reached `rustc`** — `libsqlite3-sys 0.40.0` build script: `cc-rs` needs the MSVC toolchain (`lib.exe`, `cl.exe`), warns `GNU compiler is not supported for this target`, dies on `failed to find tool "lib.exe"`. No mingw SDK either, so the bundled SQLite amalgamation cannot be cross-compiled from this box at all. |
| `cargo check -p trash --target x86_64-pc-windows-msvc` | clean |
| `cargo check -p open --target x86_64-pc-windows-msvc` | clean |
| `cargo check -p zip --target x86_64-pc-windows-msvc` | clean |
| `cargo check -p sevenz-rust --target x86_64-pc-windows-msvc` | clean |
| `cargo check -p rustix@1.1.6 --target x86_64-pc-windows-msvc` | clean, but to an almost empty module set — the trap, hence the target gate (§2) |

So: **one upstream block** (`rusqlite`'s bundled C, a toolchain problem, not a code problem) and **no
rustc-level failure list**, because `rustc` was never invoked on our crate. That asymmetry is why the
Windows code was written to be target-neutral `std` (compiles and is tested on the host) instead of a
wall of `#[cfg(windows)]` arms that nothing here could compile.

## 2. Gating fixed, not worked around

* `crates/zenman-core/Cargo.toml`: `rustix` moved to `[target.'cfg(unix)'.dependencies]`. It is absent on
  Windows rather than present-but-empty, so an accidental `rustix::fs::…` outside a `#[cfg(unix)]` arm
  fails on the host build, where someone will see it.
* `ops/guard.rs`: `an_existing_symlink_intermediate_is_refused` is `#[cfg(unix)]` (its subject is
  `std::os::unix::fs::symlink`), which is what a `--all-targets` cross check needs.
* Already correct from step 3, verified by grep rather than changed: `path.rs`, `entry.rs`,
  `index/walker.rs`, `ops/{copy,mkdir}`, `platform/env_probe.rs`, `index/watcher.rs` — every unix-only
  call sits inside a `#[cfg(unix)]`/`#[cfg(target_os = "linux")]` arm whose twin returns a typed answer.
* Linux behaviour is unchanged: the incoming test suite is the regression net (`cargo xtask ci` green).

## 3. The dependency question

**No dependency was added, and one could not help.** `unsafe_code` is `forbid`ded at workspace level, so
every candidate FFI (`GetDiskFreeSpaceExW`, `GetLogicalDrives`, `GetDriveTypeW`, `GetVolumeInformationW`,
`IShellLink`) is barred at the call site even with `windows-sys` present. The safe `std` wrapper for the
two `GetDiskFreeSpaceExW` numbers, `Metadata::file_system`, is **still unstable**
(`rust-lang/rust#67087`). Consequences, all typed, all tested:

| Question | Windows answer today | Why not more |
|---|---|---|
| `disk_space` | `CoreError::Unsupported` naming `GetDiskFreeSpaceExW` | unstable `std` API / `unsafe` FFI; `wmic` is gone from Windows 11 and a PowerShell spawn per poll is not a disk meter |
| `create_shortcut` (`.lnk`) | `CoreError::Unsupported` | `.lnk` is a COM (`IShellLink`) binary; the safe `[InternetShortcut]` `.url` is a different extension — 16b's decision |
| `chmod` | `CoreError::Unsupported` | the read-only attribute is not an `st_mode`; mapping would silently discard bits |
| `mount_entries` (drives) | **implemented** — 26 roots probed by `stat` | `GetLogicalDrives` would be cheaper; `removable` stays `false` (no `GetDriveTypeW`) |
| `can_trash` | **implemented** — volume root / UNC / missing refused | same-volume proof needs `f_fsid`/volume serial; `recycle_bin_exists` is the rule a future probe feeds |
| `exec_program` | **implemented** — `cmd /C` argv vector, `""`-doubled quoting | — |
| watcher (`--features watch`) | **compiles, answers `CoreError::Watcher`** | `ReadDirectoryChangesW` needs an owned handle + `unsafe`; requirements are documented in `platform/windows.rs`'s module doc |

## 4. Cross-target CI

`cargo xtask ci` runs `xtask/src/cross_target.rs`: probe the target (skip with a printed reason when
`rustup` or the target is absent) → hard-check the platform-layer dependencies for the target → try the
whole crate and print the documented PARTIAL while the only failure is the bundled-SQLite build script.
Any `error[E…]` or `zenman-core` compile failure for that target is a hard gate failure, so the rung
promotes itself the moment an MSVC toolchain (or a Windows runner) is available.

## 5. Deferred to step 16b

`contract.rs` full sweep · `crash_guard` polish · `README.md` · MSRV check · running rung 3 once **on
Windows** (the `#[cfg(not(unix))]` arms of `platform/linux.rs` are compile-proven only there) · the
`disk_space` and `.lnk` decisions above · `GetDriveTypeW`-backed `removable` · `services/places.rs`
growing a Windows branch over `platform::windows::mount_entries` · the `ReadDirectoryChangesW` backend.
