//! `launch`: run the file instead of viewing it — plain and sandboxed (design
//! `docs/launch-sandbox-design.md` §3-§5).
//!
//! Go had no run verb: `handleOpen` (`api/ops.go:122-143`) was the only activation, which is why an
//! executable with no registered desktop handler answered `command failed: xdg-open` instead of running
//! (design §1). This module is the missing verb. It stays deliberately small about *decisions* —
//! [`classify`] is one `stat` plus one ≤8-byte read and never a spawn (design §3's budget), and
//! [`build`] is a pure argv builder that neither probes PATH nor touches the desktop — so the menu layer
//! can ask "would running this be possible" at hover speed.
//!
//! # The argv, never a command line
//!
//! Every request this module produces is an [`ExecRequest::argv`] vector; [`ExecRequest::shell`] is never
//! used, not even for the shebang-less script that runs through `/bin/sh`. A path with a space, a quote or
//! a `$` in it is therefore *data* at every hop (design §3's deviation from the intuitive `sh -c`
//! spelling, which would have made the file name shell source text). The one place text crosses a shell
//! anyway — the sandbox wrapper — receives paths exclusively through environment variables that the
//! wrapper quotes, and [`build`] refuses any target, argument or mask containing a newline before the
//! spawn, because the newline-separated environment is the transport and a newline would turn one path
//! into two argv entries (design §5.2).
//!
//! # Two program names are hardcoded on purpose
//!
//! `/bin/sh` (the interpreter for a shebang-less script, and the reader of [`SANDBOX_SCRIPT`]) and the
//! `setsid` / `unshare` pair (the session detacher and the namespace tool) are named as *program names*,
//! resolved through the platform's `exec` lookup — design §4.4 and §5.1's "no third party" rule. Nothing
//! else here hardcodes a path; the sandbox mask roots come from `$HOME`/`$XDG_*` ([`sandbox_masks`]). The
//! launcher's *name* and its flag list are not duplicated here either: [`crate::platform::PROGRAM_UNSHARE`]
//! and [`crate::platform::SANDBOX_UNSHARE_FLAGS`] are the platform's constants, which is what lets the
//! capability probe measure the exact command line [`build`] produces (§5.4).
//!
//! # Failure is named where it can be, probed where it cannot
//!
//! [`classify`] answers [`RunKind::NotRunnable`] for anything it cannot read, and [`launch`] reports a
//! per-path [`CoreError`]. What it cannot report is a *refused sandbox*: the launcher runs inside
//! `setsid --fork`, and that wrapper exits `0` the moment it has forked, whatever `unshare` answers
//! afterwards (measured: `setsid --fork sh -c 'exit 7'; echo $?` prints `0`). A kernel that denies a user
//! namespace therefore produces a zero status, a report full of "launched" and no app — which is why the
//! capability is **probed** rather than trusted ([`can_sandbox`], design §5.3/§5.4) and the row is absent
//! rather than dead.

use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use crate::error::{CoreError, PathReject};
use crate::jobs::CancelToken;
use crate::path::AbsPath;
use crate::platform::{Env, ExecRequest, PROGRAM_UNSHARE, SANDBOX_UNSHARE_FLAGS};

/// What double-clicking / `Execute` could do with one path (design §3's table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunKind {
    /// Not executable in the sense this verb means: no exec bit, a directory, an unreadable path, or an
    /// executable *data* file (exec bit set, no shebang, no magic, no known script extension).
    NotRunnable,
    /// Exec bit set and a text script: `shebang: true` runs directly, `shebang: false` through
    /// `/bin/sh` (design §3's last paragraph).
    Script {
        /// `true` when the first two bytes were `#!`, i.e. the kernel can feed it to an interpreter.
        shebang: bool,
    },
    /// Exec bit set and a binary magic (`ELF`, Mach-O, `MZ`) — run it directly.
    Binary,
}

impl RunKind {
    /// `true` when the `Execute` row may appear (design §2: the row is present only for these).
    #[must_use]
    pub const fn is_runnable(&self) -> bool {
        !matches!(self, Self::NotRunnable)
    }
}

/// The interpreter-name list of design §3 — a `.txt` with the exec bit and no `#!` stays
/// [`RunKind::NotRunnable`], so the row never appears over a plain text file.
const SCRIPT_EXTENSIONS: [&str; 20] = [
    "sh", "bash", "zsh", "dash", "ksh", "fish", "py", "pl", "pm", "rb", "js", "mjs", "ts", "lua",
    "php", "tcl", "awk", "ps1", "bat", "cmd",
];

/// Every magic prefix design §3 recognises: ELF, the four Mach-O single-slice magics (both word sizes,
/// both byte orders), and the PE/DOS header.
const BINARY_MAGICS: [&[u8]; 6] = [
    b"\x7fELF",
    b"\xfe\xed\xfa\xce",
    b"\xce\xfa\xed\xfe",
    b"\xcf\xfa\xed\xfe",
    b"\xfe\xed\xfa\xcf",
    b"MZ",
];

/// The two deliberate program names (module docs). `unshare` is the platform layer's constant because the
/// capability probe there measures the very binary this module builds an argv around.
const SHELL: &str = "/bin/sh";
const SETSID: &str = "setsid";

/// Classify one path the way design §3 defines it: one `stat` (following symlinks, like
/// [`crate::ops::open::route`]) plus one read of at most eight bytes, and **never** a spawn. On unix the
/// exec bit is mandatory for every runnable answer (an unreadable path or a missing file answers
/// [`RunKind::NotRunnable`], never an error); off unix there are no exec bits, so the answer comes from
/// the magic and the extension alone.
#[must_use]
pub fn classify(path: &AbsPath) -> RunKind {
    let Ok(meta) = std::fs::metadata(path.as_path()) else {
        return RunKind::NotRunnable;
    };
    if !meta.is_file() {
        return RunKind::NotRunnable;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        // Any of the three exec bits, the same test `platform::linux::is_executable` and Go's
        // `exec.LookPath` use.
        if meta.permissions().mode() & 0o111 == 0 {
            return RunKind::NotRunnable;
        }
    }
    let head = read_head(path);
    if head.starts_with(b"#!") {
        return RunKind::Script { shebang: true };
    }
    if BINARY_MAGICS.iter().any(|m| head.starts_with(m)) {
        return RunKind::Binary;
    }
    if has_script_extension(path) {
        return RunKind::Script { shebang: false };
    }
    RunKind::NotRunnable
}

/// The first ≤ 8 bytes of the file; empty when it cannot be opened or read (design §3: an unreadable
/// path is not runnable, and `classify` has no error channel because the answer *is* the decision).
fn read_head(path: &AbsPath) -> Vec<u8> {
    let Ok(mut file) = std::fs::File::open(path.as_path()) else {
        return Vec::new();
    };
    let mut buf = [0u8; 8];
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => filled += n,
        }
    }
    buf[..filled].to_vec()
}

fn has_script_extension(path: &AbsPath) -> bool {
    path.as_path()
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| SCRIPT_EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// The mask roots of design §5.2: `$HOME` plus the four XDG dirs, **in that order**, de-duplicated,
/// restricted to absolute paths that currently exist as directories. Deliberately *not* `/tmp` and not
/// `$XDG_RUNTIME_DIR` — those hold the X11/Wayland sockets, and masking them produces a silent "the app
/// did not appear" instead of a sandbox (design §5.2's measured failure).
#[must_use]
pub fn sandbox_masks() -> Vec<String> {
    const KEYS: [&str; 5] = [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
    ];
    let mut out: Vec<String> = Vec::new();
    for key in KEYS {
        let Some(value) = std::env::var_os(key) else {
            continue;
        };
        // A non-UTF-8 override is not a path the sh wrapper could carry losslessly; skip it the same
        // way a missing variable is skipped.
        let Some(text) = value.to_str() else {
            continue;
        };
        if text.is_empty() || !Path::new(text).is_absolute() {
            continue;
        }
        if !std::fs::metadata(text).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        if !out.iter().any(|kept| kept == text) {
            out.push(text.to_string());
        }
    }
    out
}

/// The sandbox capability probe of design §5.4: unix **and** the platform's *measured* answer to "would
/// these flags produce a namespace here". Gates the *presence* of the `Execute (sandbox)` row, and
/// [`launch`] refuses up front on the same answer.
///
/// The measurement itself belongs to the platform ([`Env::supports_sandbox`], cached per session inside
/// [`crate::platform::linux::Linux`]): only there can the recipe be spawned the way the launcher would
/// spawn it — and it has to be spawned, because `setsid --fork` swallows a refused `unshare` (design
/// §5.3), so a PATH scan alone would offer a row that does nothing at all.
#[must_use]
pub fn can_sandbox(env: &dyn Env) -> bool {
    env.os().is_unix() && env.supports_sandbox().unwrap_or(false)
}

/// The POSIX-sh wrapper proven on the dev box (design §5.2). Four shapes were each run for real: a
/// bundle inside `$HOME`, a target sitting directly in `$HOME`, a target outside every data directory,
/// and a `KEEP` that names a mask root. Every input arrives through the environment and every
/// expansion is quoted, so no path ever reaches the shell as *source text*; the algorithm in the
/// script's own comments is the documentation for whoever debugs a launch at 23:00.
pub const SANDBOX_SCRIPT: &str = r#"# zenman sandbox wrapper. Every input arrives as data through the environment -- no path is ever
# part of this script's source text:
#   ZENMAN_SANDBOX_TARGET  the file to run
#   ZENMAN_SANDBOX_ARGV    its argv, one entry per line, program first
#   ZENMAN_SANDBOX_MASKS   the directories whose contents must disappear, one per line
#   ZENMAN_SANDBOX_KEEP    the directory that holds the target (its bundle: resources/, *.so, ...)
NL='
'
# What has to survive the masks. Normally the app's own folder. When the target sits *directly*
# inside one of the data directories there is no folder of its own to protect, so only the file
# itself is kept and its neighbours -- .ssh, .config, everything -- go.
survivor=$ZENMAN_SANDBOX_KEEP
for d in $ZENMAN_SANDBOX_MASKS; do
  [ "$d" = "$survivor" ] && survivor=$ZENMAN_SANDBOX_TARGET
done
# The outermost mask root that contains the survivor: that one needs the stash below. Any other root
# either does not contain it (masked flat) or lives inside the masked one (gone by its own turn).
outer=""
for d in $ZENMAN_SANDBOX_MASKS; do
  [ -d "$d" ] || continue
  case "$survivor" in
    "$d" | "$d"/*)
      if [ -z "$outer" ] || [ ${#d} -lt ${#outer} ]; then outer=$d; fi
      ;;
  esac
done
# A parking place for the survivor while its parent is hidden, in the private /dev/shm so nothing
# reaches the host. `$$` alone cannot name it: every sandbox is pid 1, and a stale directory would
# lock the next one out of its own mask, so take a unique name and fall back if mktemp is missing.
scratch=""
if [ -n "$outer" ]; then
  for s in /dev/shm /tmp; do
    [ -d "$s" ] || continue
    t=$(mktemp -d "$s/zenman-sandbox.XXXXXX" 2>/dev/null) || t=""
    if [ -n "$t" ]; then scratch=$t; break; fi
    if mkdir "$s/zenman-sandbox.$$" 2>/dev/null; then scratch="$s/zenman-sandbox.$$"; break; fi
  done
fi
# Save the bundle, mask, then put it back. `keepdir` is decided before the mask, because after it the
# path answers "no such file" and the kind of the thing we are protecting would be unknowable.
stashed=0
keepdir=0
[ -d "$survivor" ] && keepdir=1
if [ -n "$scratch" ]; then
  if [ "$keepdir" = 1 ]; then mkdir -p "$scratch/keep" 2>/dev/null; else : >"$scratch/keep" 2>/dev/null; fi
  mount --bind "$survivor" "$scratch/keep" 2>/dev/null && stashed=1
fi
for d in $ZENMAN_SANDBOX_MASKS; do
  [ -d "$d" ] || continue
  case "$survivor" in
    "$d" | "$d"/*) [ "$d" = "$outer" ] || continue ;;
  esac
  # With no stash there is no way to mask `outer` without hiding the app from itself, so the root
  # that holds it is the one root that stays. Every other data directory still goes.
  if [ "$d" = "$outer" ] && [ "$stashed" = 0 ]; then continue; fi
  # A refused mount is not fatal: tmpfs cannot be mounted on every filesystem, and a launch that is
  # less isolated beats a launch that does not happen. The row is not a security promise (design §6).
  mount -t tmpfs tmpfs "$d" 2>/dev/null
done
if [ "$stashed" = 1 ]; then
  # The mask deleted the survivor's parents, so recreate them in the fresh tmpfs and move the saved
  # tree back over its own path. The two mounts are independent: dropping the stash's mount does
  # not take the app away again, and the parking directory is removed so no run leaves a trace.
  mkdir -p "$(dirname "$survivor")" 2>/dev/null
  if [ "$keepdir" = 1 ]; then mkdir -p "$survivor" 2>/dev/null; else : >"$survivor" 2>/dev/null; fi
  mount --bind "$scratch/keep" "$survivor" 2>/dev/null
  umount "$scratch/keep" 2>/dev/null
fi
if [ -n "$scratch" ]; then
  rm -f "$scratch/keep" 2>/dev/null
  rmdir "$scratch/keep" "$scratch" 2>/dev/null
fi
OLDIFS=$IFS
IFS="$NL"
set -- $ZENMAN_SANDBOX_ARGV
IFS=$OLDIFS
prog=$1
shift
exec "$prog" "$@"
"#;

/// The inner program + argument vector [`build`] would spawn: the target itself for a shebang script or
/// a binary, `/bin/sh` with the target as `argv[1]` for a shebang-less script (design §4.1). `None` for a
/// [`RunKind::NotRunnable`] target — the caller turns that into the typed refusal.
fn inner_argv(kind: RunKind, path: &AbsPath, args: &[String]) -> Option<(String, Vec<String>)> {
    match kind {
        RunKind::NotRunnable => None,
        RunKind::Script { shebang: false } => {
            let mut argv = vec![path.to_string_lossy()];
            argv.extend_from_slice(args);
            Some((SHELL.to_string(), argv))
        }
        RunKind::Script { shebang: true } | RunKind::Binary => {
            Some((path.to_string_lossy(), args.to_vec()))
        }
    }
}

/// `unshare … -- /bin/sh -c SANDBOX_SCRIPT` as a vector (design §5.1's flag table: userns with a
/// root map, private mount table, own pid namespace with a fresh `/proc`; **no** `--net`, on purpose). The
/// flags come from [`SANDBOX_UNSHARE_FLAGS`], which is also the list the platform's capability probe
/// measures — one source of truth, so a probe can never have tested something other than what runs.
fn unshare_command() -> Vec<String> {
    let mut cmd: Vec<String> = Vec::new();
    cmd.push(PROGRAM_UNSHARE.to_string());
    cmd.extend(SANDBOX_UNSHARE_FLAGS.iter().map(|f| (*f).to_string()));
    cmd.push("--".to_string());
    cmd.extend([
        SHELL.to_string(),
        "-c".to_string(),
        SANDBOX_SCRIPT.to_string(),
    ]);
    cmd
}

/// Refuse anything the newline-separated transport could mis-split: one path with a `\n` would arrive
/// as two argv entries (or two masks), which is exactly the injection this whole shape exists to avoid.
fn ensure_single_line(value: &str) -> Result<(), CoreError> {
    if value.contains('\n') {
        return Err(CoreError::InvalidPath {
            raw: value.to_string(),
            reason: PathReject::Newline,
        });
    }
    Ok(())
}

/// Build the [`ExecRequest`] for one target — the pure decision half, with no PATH probe and no spawn
/// (the IO it performs is the single `classify` `stat`+read and, when `sandbox` is set, the mask
/// directory checks; the *availability* questions belong to [`request`] and [`can_sandbox`]).
///
/// The shape (design §4/§5): inner program + args, `cwd` = the target's parent (a portable
/// AppImage/AppRun resolves itself with `dirname "$0"`), `.detached()` (the app outlives the launcher),
/// wrapped in `setsid --fork` when `setsid`, and wrapped in the `unshare` pipeline + [`SANDBOX_SCRIPT`]
/// when `sandbox` — in which case `setsid` wraps the *launcher*, not the app. When both switches are
/// set, the sandbox wrap is the one that happens, with `setsid --fork` in front of `unshare`.
///
/// # Errors
/// [`CoreError::Unsupported`] for a [`RunKind::NotRunnable`] target (nothing here decides *why* the
/// desktop would not run it beyond design §3's table), and [`CoreError::InvalidPath`]
/// ([`PathReject::Newline`]) for a target, argument or mask root containing a newline.
// (No `#[must_use]`: `Result` is must-use by type, and clippy's `-D warnings` gate rejects the
// redundant attribute here — the brief's annotation applies to the non-Result fns only.)
pub fn build(
    path: &AbsPath,
    args: &[String],
    sandbox: bool,
    setsid: bool,
) -> Result<ExecRequest, CoreError> {
    let target = path.to_string_lossy();
    let (program, inner) =
        inner_argv(classify(path), path, args).ok_or(CoreError::Unsupported {
            what: "execute: not an executable script or binary",
        })?;
    ensure_single_line(&target)?;
    for arg in args {
        ensure_single_line(arg)?;
    }
    // The bundle the mask has to leave reachable (design §5.2): the directory that holds the target,
    // which is also the `cwd` below. A path with no parent is the filesystem root, whose own directory
    // is itself — and a root that is not a mask root can be masked flat without hiding the app.
    let keep = path
        .parent()
        .map_or_else(|| target.clone(), |p| p.to_string_lossy());
    ensure_single_line(&keep)?;
    // The wrapper's `ZENMAN_SANDBOX_ARGV`: the inner argv, one entry per line, program first.
    let mut inner_full: Vec<String> = Vec::with_capacity(inner.len() + 1);
    inner_full.push(program);
    inner_full.extend_from_slice(&inner);

    let mut request = if sandbox {
        let masks = sandbox_masks();
        for mask in &masks {
            ensure_single_line(mask)?;
        }
        let mut cmd = unshare_command();
        let mut request = if setsid {
            let mut argv = vec![String::from("--fork")];
            argv.append(&mut cmd);
            ExecRequest::argv(SETSID, argv)
        } else {
            let head = cmd.remove(0);
            ExecRequest::argv(head, cmd)
        };
        request.env = vec![
            ("ZENMAN_SANDBOX_TARGET".to_string(), target),
            ("ZENMAN_SANDBOX_KEEP".to_string(), keep),
            ("ZENMAN_SANDBOX_ARGV".to_string(), inner_full.join("\n")),
            ("ZENMAN_SANDBOX_MASKS".to_string(), masks.join("\n")),
        ];
        request
    } else if setsid {
        let mut argv = Vec::with_capacity(inner_full.len() + 1);
        argv.push(String::from("--fork"));
        argv.extend(inner_full);
        ExecRequest::argv(SETSID, argv)
    } else {
        let mut argv = inner_full;
        let head = argv.remove(0);
        ExecRequest::argv(head, argv)
    };
    request.cwd = path.parent();
    request.detached = true;
    Ok(request)
}

/// The [`build`] decision plus the one probe it refuses to make itself: is `setsid` on PATH *on this
/// machine* (design §4.3 — without it the child is not reparented and `exec_program`'s wait would park
/// the pane until the app exits). A failed probe answers "no wrapper", which is the shape that still
/// runs, just not detached from this session's lifecycle.
///
/// `sandbox == true` with `can_sandbox(env) == false` is the *caller's* problem, not this function's:
/// the request is still built (the builders stay pure), and [`launch`] is the layer that refuses up
/// front. The refusal has to happen there: a sandbox the kernel denies never reaches the launch report
/// at all, because `setsid --fork` reports its own zero status (design §5.3).
pub fn request(
    env: &dyn Env,
    path: &AbsPath,
    args: &[String],
    sandbox: bool,
) -> Result<ExecRequest, CoreError> {
    let setsid = env.has_program(SETSID).unwrap_or(false);
    build(path, args, sandbox, setsid)
}

/// What a batch `launch` did — the [`crate::ops::open::OpenReport`] shape without the navigation arm.
#[derive(Clone, Debug, Default)]
pub struct LaunchReport {
    /// Paths whose launcher exited zero. Not the app's exit, and not even a promise that the app exists:
    /// `setsid --fork` is long gone by then and reports its own status (design §4.3/§5.3), so this means
    /// "the wrapper accepted the job".
    pub launched: Vec<AbsPath>,
    /// Paths that could not be launched, each with its own error (Go's single `lastErr`, made
    /// per-path, exactly like [`crate::ops::open::OpenReport::failed`]).
    pub failed: Vec<(AbsPath, Arc<CoreError>)>,
}

impl LaunchReport {
    /// `true` when nothing happened at all — no launch, and no per-path failure either.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.launched.is_empty() && self.failed.is_empty()
    }

    /// The jobs/status summary line.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{} launched, {} failed",
            self.launched.len(),
            self.failed.len()
        )
    }
}

impl std::cmp::PartialEq for LaunchReport {
    /// Errors compare by [`CoreError::kind`], the same hand-written rule
    /// [`crate::ops::open::OpenReport`] uses: `CoreError` is not `Clone`/comparable, and a report test
    /// asserts *which* failure happened, not its prose.
    fn eq(&self, other: &Self) -> bool {
        self.launched == other.launched
            && self.failed.len() == other.failed.len()
            && self
                .failed
                .iter()
                .zip(other.failed.iter())
                .all(|(a, b)| a.0 == b.0 && a.1.kind() == b.1.kind())
    }
}

/// Launch every path (Go had no equivalent — see the module header), the same batch discipline
/// [`crate::ops::open::open_paths`] uses: per-path outcomes in [`LaunchReport`], the `Err` channel only
/// for the whole batch refusing to start.
///
/// What a non-zero exit means here is the *wrapper's* status and nothing more: `setsid --fork` detaches
/// the pipeline, so a launcher that is refused (`unshare` denying a userns, `sh` not finding the script)
/// is invisible to this call — measured: `setsid --fork sh -c 'exit 7'; echo $?` prints `0`. That is why
/// the sandbox is gated on the measured [`can_sandbox`] instead: the row is absent where the recipe would
/// be refused, rather than offered and silently doing nothing (design §5.3).
///
/// # Errors
/// [`CoreError::Empty`] for an empty selection and [`CoreError::Cancelled`]. A path that will not
/// launch is in [`LaunchReport::failed`], never in this `Err`.
pub fn launch(
    env: &dyn Env,
    paths: &[AbsPath],
    sandbox: bool,
    cancel: &CancelToken,
) -> Result<LaunchReport, CoreError> {
    if paths.is_empty() {
        return Err(CoreError::Empty { what: "selection" });
    }
    let sandbox_ok = !sandbox || can_sandbox(env);
    let mut report = LaunchReport::default();
    for path in paths {
        cancel.check()?;
        if !sandbox_ok {
            report.failed.push((
                path.clone(),
                Arc::new(CoreError::Unsupported {
                    what: "sandboxed launch (this system refuses the unshare sandbox)",
                }),
            ));
            continue;
        }
        let request = match request(env, path, &[], sandbox) {
            Ok(request) => request,
            Err(error) => {
                report.failed.push((path.clone(), Arc::new(error)));
                continue;
            }
        };
        let program = request.program.clone();
        match crate::jobs::block_on(env.exec_program(request)) {
            Ok(Ok(out)) if out.succeeded() => report.launched.push(path.clone()),
            Ok(Ok(out)) => report.failed.push((
                path.clone(),
                Arc::new(CoreError::Shell {
                    cmd: program,
                    code: out.code,
                    stderr: out.stderr,
                }),
            )),
            Ok(Err(error)) | Err(error) => report.failed.push((path.clone(), Arc::new(error))),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::path::PathBuf;

    /// The crate's scratch rule (see `src/scratch.rs`): a unique name, removed on drop, so a failing
    /// assert still cleans up after itself. Unix-only: it exists for the exec-bit tests, and an
    /// ungated item would be `dead_code` on a Windows build (`cfg(not(unix))`).
    #[cfg(unix)]
    struct Scratch(PathBuf);

    #[cfg(unix)]
    impl Scratch {
        fn new(tag: &str) -> Self {
            Self(crate::scratch::create(&format!("launch-{tag}")))
        }

        /// Write a file and (on unix) give it `mode`; returns the [`AbsPath`] form.
        fn file(&self, name: &str, bytes: &[u8], mode: u32) -> AbsPath {
            let path = self.0.join(name);
            std::fs::write(&path, bytes).expect("write");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                    .expect("chmod");
            }
            #[cfg(not(unix))]
            {
                let _ = mode;
            }
            AbsPath::from_path(&path).expect("scratch path is absolute")
        }

        fn dir(&self, name: &str) -> AbsPath {
            let path = self.0.join(name);
            std::fs::create_dir_all(&path).expect("mkdir");
            AbsPath::from_path(&path).expect("scratch dir is absolute")
        }
    }

    #[cfg(unix)]
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = crate::scratch::remove_tree(&self.0);
        }
    }

    #[cfg(unix)]
    const SHEBANG: &[u8] = b"#!/bin/sh\necho hi\n";
    #[cfg(unix)]
    const ELF: &[u8] = b"\x7fELF\x02\x01\x01\x00";
    #[cfg(unix)]
    const PLAIN: &[u8] = b"print('hi')\n";

    #[cfg(unix)]
    #[test]
    fn classify_sorts_scripts_binaries_and_the_rest() {
        let s = Scratch::new("classify");
        // §3's table, one row per assert: exec bit first, then shebang, magic, extension.
        let she = s.file("app.sh", SHEBANG, 0o755);
        assert_eq!(classify(&she), RunKind::Script { shebang: true });
        assert!(classify(&she).is_runnable());
        let she_quiet = s.file("quiet.sh", SHEBANG, 0o644);
        assert_eq!(
            classify(&she_quiet),
            RunKind::NotRunnable,
            "no exec bit, no row — the same file as above minus 0o111"
        );
        let py = s.file("tool.py", PLAIN, 0o755);
        assert_eq!(classify(&py), RunKind::Script { shebang: false });
        let txt = s.file("notes.txt", PLAIN, 0o755);
        assert_eq!(
            classify(&txt),
            RunKind::NotRunnable,
            "an executable data file stays a data file (§3's `.txt` rule)"
        );
        let elf = s.file("zen-dragon", ELF, 0o755);
        assert_eq!(classify(&elf), RunKind::Binary);
        let elf_quiet = s.file("zen-dragon-644", ELF, 0o644);
        assert_eq!(classify(&elf_quiet), RunKind::NotRunnable);
        let pe = s.file("thing.exe", b"MZ\x90\x00", 0o755);
        assert_eq!(classify(&pe), RunKind::Binary);
        let dir = s.dir("adir");
        assert_eq!(classify(&dir), RunKind::NotRunnable);
        let missing = AbsPath::from_path(s.0.join("nope")).expect("clean path");
        assert_eq!(classify(&missing), RunKind::NotRunnable);
    }

    #[cfg(unix)]
    #[test]
    fn build_shapes_the_four_switch_combinations() {
        let s = Scratch::new("build");
        let bin = s.file("app", ELF, 0o755);
        let py = s.file("tool.py", PLAIN, 0o755);
        let args: Vec<String> = vec!["one".into(), "two".into()];

        // plain: the target *is* the program; never a shell string, always detached, cwd = parent.
        let r = build(&bin, &args, false, false).expect("binary");
        assert_eq!(r.program, bin.to_string_lossy());
        assert_eq!(r.args, args);
        assert!(!r.shell);
        assert!(r.detached);
        assert_eq!(
            r.cwd,
            bin.parent(),
            "cwd is the app's folder, not zenman's (design §4.2)"
        );
        assert!(r.env.is_empty());

        // setsid: `setsid --fork <program> <args…>`.
        let r = build(&bin, &args, false, true).expect("setsid");
        assert_eq!(r.program, SETSID);
        let mut want = vec![String::from("--fork"), bin.to_string_lossy()];
        want.extend_from_slice(&args);
        assert_eq!(r.args, want);

        // sandbox: unshare pipeline feeding the wrapper, target/argv/masks through the environment.
        let r = build(&bin, &args, true, false).expect("sandbox");
        assert_eq!(r.program, PROGRAM_UNSHARE);
        // One source of truth, read at the builder itself: `unshare_command()`'s flag block *is*
        // `platform::SANDBOX_UNSHARE_FLAGS` — the list the capability probe spawns (design §5.4), so a
        // probe can never have measured a command line other than the one the row runs.
        let recipe = unshare_command();
        assert_eq!(recipe.first().map(String::as_str), Some(PROGRAM_UNSHARE));
        assert_eq!(
            &recipe[1..1 + SANDBOX_UNSHARE_FLAGS.len()],
            SANDBOX_UNSHARE_FLAGS,
            "the built sandbox command drifted from the probed one"
        );
        assert_eq!(recipe[1 + SANDBOX_UNSHARE_FLAGS.len()].as_str(), "--");
        // …and the request that reaches `exec_program` carries that same list (program name aside).
        assert_eq!(
            &r.args[..SANDBOX_UNSHARE_FLAGS.len()],
            SANDBOX_UNSHARE_FLAGS,
            "the built sandbox command drifted from the probed one"
        );
        assert_eq!(r.args[SANDBOX_UNSHARE_FLAGS.len()], "--");
        assert_eq!(
            r.args[r.args.len() - 3..],
            [
                SHELL.to_string(),
                "-c".to_string(),
                SANDBOX_SCRIPT.to_string()
            ]
        );
        assert_single_line_env(
            &r,
            &bin.to_string_lossy(),
            &format!("{}\none\ntwo", bin.to_string_lossy()),
        );

        // sandbox + setsid: setsid wraps the *launcher*, not the app.
        let r = build(&bin, &args, true, true).expect("both");
        assert_eq!(r.program, SETSID);
        assert_eq!(r.args.first().map(String::as_str), Some("--fork"));
        assert_eq!(r.args.get(1).map(String::as_str), Some(PROGRAM_UNSHARE));
        assert_eq!(
            &r.args[2..2 + SANDBOX_UNSHARE_FLAGS.len()],
            SANDBOX_UNSHARE_FLAGS,
            "`setsid` wrapped a recipe other than the probed one"
        );
        assert_eq!(
            r.args[r.args.len() - 3..],
            [
                SHELL.to_string(),
                "-c".to_string(),
                SANDBOX_SCRIPT.to_string()
            ]
        );

        // A shebang-less script runs through /bin/sh with the target as argv[1] — and the wrapper sees
        // that same inner argv, program first.
        let r = build(&py, &[], false, false).expect("py");
        assert_eq!(r.program, SHELL);
        assert_eq!(r.args, [py.to_string_lossy()]);
        let r = build(&py, &[], true, false).expect("py sandboxed");
        assert_single_line_env(
            &r,
            &py.to_string_lossy(),
            &format!("{SHELL}\n{}", py.to_string_lossy()),
        );

        // NotRunnable is a typed refusal, not a panic and not a silent Ok.
        let txt = s.file("notes.txt", PLAIN, 0o755);
        let err = build(&txt, &[], false, false).expect_err("executable data file");
        assert_eq!(err.kind(), "unsupported");
    }

    /// The sandbox environment contract: target, inner argv (one per line), masks (one per line),
    /// none of them containing a newline of their own beyond the separators.
    #[cfg(unix)]
    fn assert_single_line_env(request: &ExecRequest, target: &str, argv: &str) {
        let get = |key: &str| {
            request
                .env
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
                .unwrap_or_else(|| panic!("env is missing {key}: {:?}", request.env))
        };
        assert_eq!(get("ZENMAN_SANDBOX_TARGET"), target);
        assert_eq!(get("ZENMAN_SANDBOX_ARGV"), argv);
        // What the mask must leave reachable is the target's own directory (design §5.2).
        assert_eq!(
            get("ZENMAN_SANDBOX_KEEP"),
            Path::new(target)
                .parent()
                .and_then(|p| p.to_str())
                .expect("a target has a parent")
        );
        // Every mask line is an existing directory; the join is the only place a newline may appear.
        for line in get("ZENMAN_SANDBOX_MASKS").lines() {
            assert!(
                Path::new(line).is_dir(),
                "mask {line:?} should exist as a directory"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn build_refuses_anything_with_a_newline() {
        let s = Scratch::new("newline");
        let bin = s.file("app", ELF, 0o755);
        // An argument with a newline would arrive as two argv entries through the wrapper's `set --`.
        let err = build(&bin, &["two\nlines".into()], false, false).expect_err("split arg");
        assert_eq!(err.kind(), "invalid_path");
        // A *target* whose name contains a newline: legal on every unix filesystem, and refused here
        // for both the plain and the sandboxed transport (design §5.2: newlines are refused in Rust).
        let nasty = s.file("bad\nname", SHEBANG, 0o755);
        assert_eq!(
            classify(&nasty),
            RunKind::Script { shebang: true },
            "classify itself stays total — the refusal belongs to the transport"
        );
        let err = build(&nasty, &[], false, false).expect_err("newline target");
        assert_eq!(err.kind(), "invalid_path");
        assert_eq!(
            build(&nasty, &[], true, false)
                .expect_err("and sandboxed")
                .kind(),
            "invalid_path"
        );
    }

    #[test]
    fn sandbox_script_is_the_pure_validated_wrapper() {
        // Every input arrives through the environment: the script *reads* the four variables and never
        // assigns to (or interpolates) them — an assignment would mean the Rust side baked a path into
        // the script source, which is the injection this shape exists to prevent.
        for var in [
            "ZENMAN_SANDBOX_TARGET",
            "ZENMAN_SANDBOX_KEEP",
            "ZENMAN_SANDBOX_ARGV",
            "ZENMAN_SANDBOX_MASKS",
        ] {
            assert!(SANDBOX_SCRIPT.contains(var), "script must read {var}");
            assert!(
                !SANDBOX_SCRIPT.contains(&format!("{var}=")),
                "script must never assign {var} — paths are data, never source"
            );
        }
        assert!(SANDBOX_SCRIPT.starts_with("# zenman sandbox wrapper."));
        assert!(SANDBOX_SCRIPT.contains("mount -t tmpfs tmpfs"));
        assert!(SANDBOX_SCRIPT.ends_with("exec \"$prog\" \"$@\"\n"));
    }

    #[test]
    fn sandbox_masks_are_existing_dirs_in_order() {
        let masks = sandbox_masks();
        for m in &masks {
            assert!(Path::new(m).is_dir(), "{m} must exist as a directory");
            assert!(Path::new(m).is_absolute(), "{m} must be absolute");
            assert!(
                !m.contains(':'),
                "{m}: the list is one-per-line, no quoting games"
            );
        }
        // De-duplicated, and HOME first when it is present at all.
        let mut seen: Vec<&String> = Vec::new();
        for m in &masks {
            assert!(!seen.contains(&m), "{m} appears twice");
            seen.push(m);
        }
        if let Some(home) = std::env::var_os("HOME").filter(|h| Path::new(h).is_dir()) {
            let home = home.to_string_lossy().into_owned();
            assert_eq!(
                masks.first().map(String::as_str),
                Some(home.as_str()),
                "$HOME leads the mask list (design §5.2's order)"
            );
        }
    }
}
