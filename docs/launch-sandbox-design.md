# Execute + Execute (sandbox): running a file instead of viewing it

Status: design (this file) → core (`zenman-core`) → shell (`zenman-ui`).
Owner docs: `PROJECT_OVERVIEW.md` §Read Triggers; menu rules in `crates/zenman-ui/src/menus/mod.rs` module docs.

## 1. Why

Two real cases from the desktop:

| File | `file --mime-type` | Today | Wanted |
|---|---|---|---|
| `rustdesk/AppRun` (`0775`, `#!/bin/bash`) | `text/x-shellscript` | `Open` → the text editor (the desktop handler for a shell script) | keep `Open`, add a row that **runs** it |
| `zen-dragon` (`0775`, ELF) | `application/x-executable` | `Open` → `command failed: xdg-open` (xdg-open exits 4: no handler registered) | `Open` should just work, and say something useful when it cannot |

So: a **run** verb next to the **view** verb, plus a sandboxed variant of the run verb.

## 2. Menu shape

The popup has no horizontal layout today: `Vec<Vec<MenuSpec>>` is *groups* of vertically stacked rows
(`menus::file_menu`, `crates/zenman-ui/src/menus/mod.rs:421`). To keep the two run rows from costing two
lines, one group of exactly two specs that asks for pair layout is drawn **side by side**:

```
Open
Execute            Execute (sandbox)
Cut      Copy      Copy Path   Copy Name
...
```

* `Execute` is present only when the right-clicked row is runnable (§3) — the same mechanism as the
  directory-only block, i.e. a new `MenuTarget::Row { on_dir, on_run }` field, not one more
  `MenuContext` boolean (`bits()` is the cross-product the tests walk exhaustively, and as of step C
  it is eight booleans wide, `can_sandbox` being the one that *is* a capability rather than a fact
  about the click).
* The pair is measured as **two half-rows**, each with its own icon column, its own label + hint
  budget and its own [`Tokens::menu_item_pad_x`] on both of its edges — i.e. twice the wider half, not
  the sum of the two labels. One pad per half (the obvious first sketch) leaves the label
  `menu_item_pad_x` short of what the row painter then budgets for it, which wraps
  `Execute (sandbox)` onto a second line inside a 26 pt row.
* Both rows are enabled by `selection` (Go's own guard) and act on the whole selection, like `Open`.
* `Execute (sandbox)` is present only when `on_run && ctx.can_sandbox` (§5.4). Absent, never greyed:
  an unshare-less kernel has no greyed way to succeed.

## 3. What counts as runnable

`zenman_core::ops::launch::classify(path) -> RunKind`, no shell-out, one `stat` + one 8-byte read:

| `RunKind` | rule |
|---|---|
| `Script { shebang: true }` | not a directory, any of `0o111` exec bits set, first two bytes `#!` |
| `Script { shebang: false }` | exec bit set, name matches the script extension set, and no `#!` was found |
| `Binary` | exec bit set, first bytes are an ELF / Mach-O / PE magic (`\x7fELF`, Mach-O `0xfeedface`/`0xfeedfacf` in **both** byte orders — four byte patterns, `MZ`) |
| `NotRunnable` | anything else: no exec bit, an executable data file, a directory, an unreadable path |

The extension set is the interpreter-name list (`.sh .bash .zsh .dash .ksh .fish .py .pl .pm .rb .js .mjs
.ts .lua .php .tcl .awk .ps1 .bat .cmd`) — a `.txt` with the exec bit set and no `#!` stays
`NotRunnable`, so the row never appears over a plain text file. Non-unix platforms have no exec bits:
`classify` answers from the magic/extension only there, and `Session::can_sandbox` is `false`.

A shebang-less script runs through `/bin/sh` (the interpreter choice lives in `launch::build`; there is
no separate `interpreter_for` function), never through the shell string
API: the request is always an **argv vector** (`ExecRequest::argv`), so a path with a space or a quote in
it cannot become code.

## 4. Launch (unsandboxed)

`launch::request(env, path, args, sandbox)` — the pure argv builder `launch::build` plus the `setsid`
PATH probe — produces the `ExecRequest` that `Env::exec_program` runs; `launch::launch(env, paths,
sandbox, cancel)` is the batch verb that loops it into a `LaunchReport`:

1. `program` = the target (`Script{shebang:true}` / `Binary`) or `/bin/sh` with the target as `argv[1]`.
2. `cwd` = the target's parent (a portable AppImage/AppRun resolves its own directory with
   `dirname "$0"`, so the cwd must be the app's folder, not zenman's).
3. wrapped in `setsid --fork` when `env.has_program("setsid")` — a new session, **reparented to init**, so
   `exec_program`'s `child.wait()` returns in milliseconds instead of parking the pane until the app exits.
   Measured: the outer command returns in 1 ms while the sandboxed child lives on.
4. `detached = true` (no pipes), through `Env::exec_program` — the crate's single spawn site
   (`clippy.toml` bans `Command::output`, `platform/linux.rs:283` is the only spawn).

No new `Env` method for the launch itself: everything above is expressible as one `ExecRequest`, so the
ten `Env` impls (three platform, seven test) stay untouched. (§5.4's probe does add one trait method,
`Env::supports_sandbox`, and it has a **default** body, which is why the same ten impls still compile
unchanged.)

## 5. Launch (sandbox)

### 5.1 Kernel features, no third party

util-linux `unshare(1)` only (`/usr/bin/unshare`, part of `util-linux`, on every distro that ships
systemd). No `bwrap`, no `firejail`, no `flatpak`, no seccomp, no `unsafe` (the workspace `forbid`s
`unsafe_code`, so `clone(2)`/`unshare(2)` are out of scope — the tool is shelled out to).

```
setsid --fork \
  unshare --user --map-root-user --mount --pid --fork --propagation private --mount-proc -- \
  /bin/sh -c <SANDBOX_SCRIPT>
```

| flag | gives |
|---|---|
| `--user --map-root-user` | new **user namespace**, uid 0 inside, real uid outside; can be escaped never by privilege, and no `sudo` on the host |
| `--mount` + `--propagation private` | private mount table; the tmpfs masks in §5.2 never reach the host's mounts |
| `--pid --fork --mount-proc` | the sandbox sees its own process tree (4 procs, `ppid=1`); it cannot `kill(2)` or `ptrace` anything on the host because no host pid is visible |
| `--mount-proc` | `/proc` is the sandbox's, so `/proc/<host pid>` tricks are gone with it |

**Network is deliberately *not* namespaced.** `--net` without a `veth`/bridge setup means "no
connectivity", which breaks the two uses that matter (a remote-desktop AppRun, anything that phones
home once). A sandboxed-but-useless launch is not the ask; §6 states what is and is not isolated.

### 5.2 The filesystem mask

`SANDBOX_SCRIPT` is a **constant** `&str` baked into the crate. It receives everything through
environment variables and quotes every expansion. No path ever reaches the shell as *source text*, so a
path containing `'`, `$` or `` ` `` is data, and a newline in a path is refused in Rust before the spawn
(`build` → `CoreError::InvalidPath`/`PathReject::Newline`, because the transport below is
newline-separated):

| variable | holds |
|---|---|
| `ZENMAN_SANDBOX_TARGET` | the file being run |
| `ZENMAN_SANDBOX_KEEP` | its directory — *the bundle*: `dirname "$0"`, the `resources/`, the `.so`s |
| `ZENMAN_SANDBOX_ARGV` | the inner argv, one entry per line, program first |
| `ZENMAN_SANDBOX_MASKS` | the data directories whose contents must disappear, one per line |

The *survivor* is the bundle, except when the target sits **directly** inside a mask root (`~/fix.sh`),
where there is no folder of its own to protect and only the file itself survives. Then, per mask root:

* it does not contain the survivor → `mount -t tmpfs tmpfs D`. Flat, immediate, and the case the rustdesk
  AppRun lands in.
* it **is** the outermost root that contains the survivor → `mount --bind` the survivor into a fresh
  directory under `/dev/shm` (falling back to `/tmp`), mask the root, `mkdir -p` the survivor's parents
  inside the new tmpfs, `mount --bind` it back at its own path, then drop the stash's mount and `rmdir`
  the parking place. Masking a root **cannot** therefore hide the app from itself, cannot leave a
  `zenman-sandbox.*` behind, and — because the mask is the root itself rather than its children —
  `~/.netrc`, `~/.bashrc` and a top-level *write* also disappear into the tmpfs. The first draft of this
  script masked child-by-child down the path instead, which left the target's own directory gutted (an
  AppRun without its `resources/` is a syntax error waiting to happen) and left `$HOME`'s loose files
  readable and writable.
* it is a root inside an already-masked root (`$HOME/.config` after `$HOME`) → its path no longer exists,
  `[ -d ]` is false, the iteration is a no-op.

If the stash cannot be arranged (`mktemp` and `mkdir` both fail) the outer root is the one root **not**
masked: isolation degrades, the launch still happens, and the row is not a security promise (§6).

Mask roots come from `launch::sandbox_masks()`: `$HOME`, `$XDG_CONFIG_HOME`, `$XDG_DATA_HOME`,
`$XDG_CACHE_HOME`, `$XDG_STATE_HOME`, de-duplicated and restricted to directories that exist. **Not**
`/tmp` and not `$XDG_RUNTIME_DIR`: those hold the X11 socket (`/tmp/.X11-unix`) and the Wayland socket, so
masking them produces a silent "the app did not appear" instead of a sandbox. The bundle stays
readable **and** writable — a portable app that unpacks next to its binary would otherwise fail, and the
mask's job is to keep personal data out of sight, not to jail writes into the app's own folder.

### 5.5 Measured on the dev box (util-linux 2.42.4, `unprivileged_userns_clone=1`)

Driven through `ops::launch::launch` with the real `platform::default_env()`, probe scripts under
`$HOME/zenman-e2e/app/` and a neighbour `$HOME/zenman-e2e/leak/canary`:

| probe line | sandboxed | unsandboxed |
|---|---|---|
| `id -u` | `0` (mapped) | `1000` |
| `ls /proc \| grep -c '[0-9]*$'` | **4** | 507 |
| `ls -A $HOME` | `zenman-e2e,` (the recreated chain only) | 35 entries |
| `cat $HOME/.ssh`, `head $HOME/.bashrc` | unreadable | readable |
| `cat $HOME/zenman-e2e/leak/canary` | `No such file or directory` | `SECRET` |
| `cat $HOME/zenman-e2e/app/canary`, `app/resources/r.txt` | `VISIBLE`, `RES` | same |
| `echo RAN > $HOME/ZENMAN-SANDBOX-LEAK` | gone when the sandbox exits (never on the host) | on the host |
| target outside every mask (`/tmp/zenman-e2e-outside/app/probe.sh`) | `ls -A $HOME` = empty | — |
| outer command | returns in ~1 ms, child reparented to `ppid=1` | same |

This harness is deliberately **not** a committed test (`cargo xtask ci` may not rewrite a developer's
`$HOME`, and this repo bans `#[ignore]`d tests): §5.5 is the record, and re-deriving it is a
`zenman-core/tests/` file written for one `cargo test -- --nocapture` run and deleted afterwards.

### 5.3 A refused sandbox is measured before the click, never reported after it

The plain launcher is wrapped in `setsid --fork`, and that wrapper reports **its own** status, not the
child's. Measured on the dev box:

```console
$ setsid --fork false;              echo $?   -> 0
$ setsid --fork sh -c 'exit 7';     echo $?   -> 0
```

so `exec_program`'s wait sees `setsid` exit `0` and never sees `unshare` refuse a userns (`ENOSPC`,
`kernel.unprivileged_userns_clone=0`, an old util-linux that does not know `--propagation`). The first
draft of this design assumed the opposite and gated the row on a PATH scan alone — the result on such a
kernel was a row that, when clicked, did **nothing at all**: no app, no report entry, no toast.

The refusal is therefore caught *before the row is drawn*: the platform runs the real recipe once
(§5.4) and the answer decides the row's existence — absent where the kernel says no, never greyed, never
silently dead. What the report can still show is a failure of the wrapper itself (`setsid` not on PATH,
`fork` failing): that status does reach `LaunchReport::failed` as a `CoreError::Shell`. Nothing that
happens *inside* the new session does, which is the whole reason §5.4 exists.

### 5.4 Capability probe

`can_sandbox(env)` (`ops/launch.rs`) = unix (`Os::is_unix`) **and** `env.supports_sandbox()`, and that
platform answer is **measured**, in two steps:

1. a PATH scan for `unshare` (`platform::PROGRAM_UNSHARE`) — a machine without util-linux never spawns;
2. one spawn of `unshare <SANDBOX_UNSHARE_FLAGS> -- /bin/sh -c true`, whose exit status *is* the answer.
   A `spawn` error (no binary, `EPERM` on `exec`) is `Ok(false)` as well, and the *reason* is dropped on
   purpose: the row is a boolean question, and the sentence explaining one refusal would mislead about
   the next.

The verdict is cached for the session inside `Linux` (`sandbox_cache`, the twin of the symlink
`ProbeCache`), so the spawn happens **at most once**; the menu-open path that asks on every frame pays a
PATH scan plus a lock read. Measured on the dev box (util-linux 2.42.4, `unprivileged_userns_clone=1`,
`/bin/sh -c true` inside the recipe): the measurement costs **~0.85–1.3 ms** (`supports_sandbox()`'s first
call 1.30 ms; five fresh `Linux` instances 0.85–1.02 ms each), every later `supports_sandbox()` call
**~7 µs**, and the cache read alone **~30–50 ns**. The row is therefore offered on this box — and on a
kernel that refuses it, `supports_sandbox()` answers `false` and `Execute (sandbox)` is not drawn.

`platform::SANDBOX_UNSHARE_FLAGS` is the **one** list both sides use — `ops::launch::unshare_command()`
builds it and `Linux::supports_sandbox()` measures it — so the probe can never have tested a command line
other than the one the row runs. `--propagation` needs util-linux ≥ 2.25; where any of the flags is
refused the probe says no and the row is not offered. Every platform that is not `Linux` keeps the trait
default `Ok(false)`, which is the correct answer on Windows and the honest one everywhere else.

Folded into `MenuContext::can_sandbox` next to `can_symlink` (`crates/zenman-ui/src/app.rs:1674`).

## 6. What this sandbox is, and what it is not

Isolated: process tree, private mount table, personal data under the mask roots (except the bundle),
`/proc`. Shared: the filesystem outside the mask roots (read+write), the network, the display server, the
kernel, and everything inside the app's own folder.

* **Not** a boundary against a program that wants to read `/etc` or `/usr`, nor against a kernel
  vulnerability: the kernel is shared. Data that lives outside `$HOME` and the XDG dirs — a separate
  `/media/<user>` partition, for instance — is not masked either.
* **Not** a resource limit: no cgroup, no rlimit, no `--max-rss`. A fork bomb inside its own pid ns still
  eats RAM.
* **Is** enough for the ask: an untrusted-looking script cannot read the mail profile, the ssh keys or
  the shell history, cannot signal or inspect the user's processes, and its writes under `$HOME` —
  anywhere but inside its own bundle — land in a tmpfs that disappears when it exits.
* **One failure stays invisible even with the probe**: a namespace that is created *successfully* and
  whose app then dies — a missing `resources/`, a bad `exec`, a signal. The launcher (`setsid`, then
  `unshare`, then the wrapper `sh`) has exited long before, so its status is already spent and
  `LaunchReport` can only say "the wrapper accepted the job" (§5.3). Nothing in this design turns that
  into a toast; the user sees an app that never appeared.
`docs/` should say so again in the user-facing text; the module docs are the authority.

## 7. `Open` improvements (the same change set)

1. `open_paths`' `Launch` arm: on failure, if `classify` says `Binary` **and** the file has an exec bit,
   retry as §4's `execute` — an executable with no registered handler is the file the user meant to run.
   Scripts keep the editor (the ask was a *new row* for them, not a surprise exec) and keep their error.
2. The toast text gains the launcher's own words. `CoreError::Shell`'s `Display` is
   `command failed: xdg-open` (`error.rs:229`); add `CoreError::detail()` (Display, plus `stderr`/`code`
   when non-empty) and use it where `run_open` reports a failure (`app.rs:3906`), so the user reads
   `no method available for opening zen-dragon` instead of a bare program name.

## 8. Work split

| Step | Files | Gate |
|---|---|---|
| A. `ops::launch` | new `crates/zenman-core/src/ops/launch.rs`, `ops/mod.rs`, `session.rs` (`run_kind`, `can_sandbox`, `launch`; the ops-level names are `classify`/`build`/`request`/`launch` — no `execute`/`execute_sandboxed` pair, sandboxing is a `bool` on one verb) | unit tests in-module + `crates/zenman-core/tests/contract_ops.rs` |
| B. `Open` | `ops/open.rs`, `error.rs` (`detail`), `apps`/`app.rs` toast (core half done: `detail()` exists and `open_paths` falls back; the toast call site belongs to the UI step) | `contract_ops`, `tests/file_pane.rs` |
| C. menu + pair layout | `keymap.rs`, `menus/mod.rs` (`MenuTarget::Row`, `can_sandbox`, geometry/`row`/`hover_corners`), `app.rs` (`open_requested_menu`, `menu_context`, `dispatch`) | `menus/mod.rs` in-crate cross-product + `crates/zenman-ui/tests/{menus,file_pane,keymap}.rs` |
| D. labels | `keymap.rs::label`, `apps/zenman/i18n/en.json` (source-of-truth only; nothing reads it back yet) | `cargo xtask ci` |

Verification loop: `cargo fmt --all && cargo clippy -p zenman-core -p zenman-ui --all-targets -- -D
warnings && cargo test -p zenman-core -p zenman-ui`, then `cargo xtask ci`.
