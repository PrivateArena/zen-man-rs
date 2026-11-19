# Crash reports (step 1)

Written by `apps/zenman/src/crash_guard.rs` + `apps/zenman/src/gpu_startup.rs`; mirrors plan §5.9's
`crash_guard` row and failure mode **F18**. Everything here exists because the workspace rule is
*return errors, never abort*: a zen-man that dies in a driver takes the user's undo log with it, so a
failure has to leave (a) a file a human can read and (b) a machine-readable exit code.

## Where the files are

| Path | Written by | Meaning |
|---|---|---|
| `<data_dir>/crashes/panic-<unix-seconds>-<pid>.txt` | `crash_guard::install_hook` | one file per panic: message, `file:line`, pid, version |
| `<data_dir>/gpu/starting.toml` | `gpu_startup::Sentinel::create` | pending marker, written **before** the driver is touched; removed by `bless` |
| `<data_dir>/gpu/last-good.toml` | `gpu_startup::Sentinel::bless` | the profile that completed a run; tried first next boot |

`<data_dir>` is what `zenman --help` resolves: `--data-dir`, else `ZENMAN_DATA_DIR`, else the platform
`ProjectDirs` data directory for `io.storytold.zenman` (see `apps/zenman/src/app_dirs.rs`). `crashes/`
is created lazily — a run that never panics never creates it.

## Reading a report

```
zen-man crash report
version: zenman 0.1.0
panic: index out of bounds: the len is 0 but the index is 0
location: apps/zenman/src/app.rs:41
pid: 41234
```

`location:` is the `file:line` of the `panic!`/indexing that fired. Attach this file **and**
`<data_dir>/gpu/starting.toml` (if present) to a bug report: the marker says which GPU profile was
being attempted, which is the difference between "a widget panicked" and "this machine cannot render".

## The process does not abort

`crash_guard::guard("startup", …)` wraps the boot (and, from step 6 on, every command) in
`catch_unwind`. A panic inside becomes an `Err` that `main` prints, so:

* the undo log and SQLite database are left as they were — nothing is written after a panic, because
  the code that writes is behind the returned `Err`,
* `panic = "unwind"` is kept in every profile (root `Cargo.toml`); switching to `abort` would turn a
  reportable error into an unrecoverable exit and is forbidden by the house rules,
* the GPU sentinel is **not** blessed, so the *next* start drops one rung of the fallback ladder
  (plan §5.9). That is deliberate: a segfault inside a vendor driver cannot be caught in-process, so
  the marker is the detection mechanism.

## Exit codes

| code | meaning | what to do |
|---|---|---|
| 0 | ran normally, or a parity stub printed its landing step | — |
| 1 | the window/event loop failed for a non-graphics reason | check the message; usually a display problem |
| 2 | the layout could not be resolved (`--data-dir`/`--config` unusable, bad `--smoke`) | fix the flag or `ZENMAN_DATA_DIR` |
| 3 | no GPU profile had an adapter | `zenman --print-adapters`; on a headless box install lavapipe/`llvmpipe` (plan OQ3) |
| 4 | the chosen profile created a device and `run_native` still failed | the marker is kept: the next start drops this rung, or run `zenman --safe-gpu` now |
| 5 | a panic escaped into `crash_guard` | read `<data_dir>/crashes/` |

## Recovering a stuck GPU choice

```sh
zenman --print-adapters                 # what wgpu sees, no window, exit 0
zenman --safe-gpu                       # skip Vulkan/Metal/DX12, start from GL
zenman --gl                             # legacy GL only
zenman --disable-gpu                    # last-resort (software-capable) rung only
rm <data_dir>/gpu/last-good.toml        # forget the saved profile
rm <data_dir>/gpu/starting.toml         # forget a crash marker after you have read it
```

`ZENMAN_GPU_BACKEND=vulkan|gl|dx12|metal|noop` narrows every rung to one backend; it is the hook
`apps/zenman/tests/gpu_fallback.rs` uses to reproduce "every rung rejected" (exit 3) on a machine that
does have a GPU. `WGPU_BACKEND` (wgpu's own variable) still overrides everything.

## The three panic boundaries (step 16b)

| Boundary | Code | What the user sees | Test |
|---|---|---|---|
| boot | `main.rs` wraps `run_with_context` in `crash_guard::guard("startup", …)` | exit 5 + report file | `crash_guard::tests::guard_turns_a_panic_into_an_error_without_aborting` |
| job worker | `zenman-core::jobs::spawn` catches panics inside the closure and sends a typed `JobEvent::Failed` naming the job | the job fails as a toast, the session stays alive | `jobs::tests::a_panicking_job_becomes_a_typed_failure_not_a_lost_thread` (a real panic through the public `spawn` seam) |
| frame | `apps/zenman/src/app.rs::ShellApp` wraps `logic` and `ui` in `crash_guard::guard` | one `Error` toast ("internal error in the UI logic …"), the window survives; the report file is written by the hook | the guard + report mechanics are the two `crash_guard` unit tests; the *wiring* of a shell panic to a toast has no headless test because no public API injects a panic into `ZenApp::logic` — this is stated rather than faked |

A panic inside an *op* is covered by whichever boundary runs it: synchronous ops run on the frame
thread (frame boundary), job ops on the worker (job boundary). The panic hook always writes the
report first, so every path leaves a file under `<data_dir>/crashes/`.

## Not yet here

Job-state dumping (the hook records no `JobInfo` list yet — the report names the panic, not the
running jobs), and any Windows/macOS-specific report path — the layout above is the one `app_dirs`
resolves on each platform, so the directory differs but the files do not.
