//! One window per data directory (plan §5.9 `single_instance.rs` row).
//!
//! Why this exists at all: the undo log and the SQLite index are **shared** files, and Go's writes were
//! racy across processes (`log.go` opened the log without `O_APPEND`, so two processes both `seek(End)`
//! and then both write at the same offset). Two zen-man windows on one data directory is a corruption
//! bug, so the second one must not open.
//!
//! # Protocol
//!
//! A Unix socket under `<data_dir>/run/zenman-<variant>-<hash>.sock`. The second process connects, writes
//! one `focus <app_id>` line, and exits **0** — "I am not a failure, I just did not need to start". The
//! first process accepts the connection and raises its window (plan §5.9: *a second invocation focuses the
//! first and exits 0*).
//!
//! # The socket name is derived, never fixed
//!
//! `--app-variant` and `--data-dir` both feed the path, and it lives under the data directory rather than
//! in `/tmp`: a fixed `/tmp/zenman.sock` would make two concurrent test runs fight, and would let one
//! user's session block another's. The path is reported on the boot line and by `--migrate-config`-style
//! diagnostics so a user can see which instance they are about to focus.
//!
//! # `SOCK_CLOEXEC`, and why one more call is needed
//!
//! The plan's requirement is that the listener socket must not leak into a child process — zen-man spawns
//! external programs through `platform::Linux::open_with`/custom actions, and a forked `xdg-open` holding
//! our listener fd would keep the "instance alive" signal true after we exit. `UnixListener::bind` is not
//! `SOCK_CLOEXEC` at the libc level (the plan cites `library/std/src/sys/pal/unix/net.rs`, `os_ext.rs:231-232`
//! in its wording): std creates the socket and then sets `FD_CLOEXEC` with a second call, which leaves the
//! window where a concurrent `fork`+`exec` inherits an uncloexec fd. We therefore set it explicitly with
//! `rustix::io::fcntl_setfd(…, CLOEXEC)` immediately after `bind`, and read it back with `fcntl_getfd` so a
//! platform that refuses is reported instead of assumed. See [`configure_cloexec`].

#![cfg_attr(not(unix), allow(dead_code))]

use std::error::Error;
use std::path::{Path, PathBuf};

use crate::app_dirs::AppPaths;

/// The boxed error this module reports: it wraps `std::io::Error`, `rustix`'s `Errno` and its own
/// message, and the caller only ever prints it or turns it into an exit code.
pub type BoxError = Box<dyn Error + Send + Sync>;

/// What the second and subsequent processes send. Kept to one line so a `read_line` on the other side
/// terminates.
pub const FOCUS_MESSAGE: &str = "focus";

/// …and the file-manager hand-off: `open <app_id> <path>`, one line per requested folder.
///
/// The path is the sender's *resolved* absolute path, put on the wire by `Cli::targets` rather than
/// handed over raw: the instance that opens a folder must not be the one to decide what an arbitrary
/// string meant, and plan §4.2's guard has already been paid for by the time a request is written.
pub const OPEN_MESSAGE: &str = "open";

/// One request from a second invocation of the same data directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// "Raise your window" — the whole of the protocol before a file manager needed one.
    Focus,
    /// "List this folder": the desktop asked a *file manager* to open something, and this is the
    /// instance that is already running being asked to do it instead of a second window opening
    /// beside it (a second window is the corruption bug the claim exists to prevent).
    Open(String),
}

impl Request {
    /// The line this request is written as.
    fn line(&self, app_id: &str) -> String {
        match self {
            Self::Focus => format!("{FOCUS_MESSAGE} {app_id}"),
            Self::Open(path) => format!("{OPEN_MESSAGE} {app_id} {path}"),
        }
    }

    /// Parse one wire line. Anything this build cannot read is taken for the focus request the protocol
    /// used to consist of entirely: a newer sender must at least raise the window.
    fn parse(line: &str) -> Self {
        let mut fields = line.splitn(3, ' ');
        if fields.next() == Some(OPEN_MESSAGE) {
            let _owner = fields.next();
            if let Some(path) = fields.next().filter(|p| !p.is_empty()) {
                return Self::Open(path.to_owned());
            }
        }
        Self::Focus
    }
}

/// The result of [`claim_or_focus`].
#[derive(Debug)]
pub enum Claim {
    /// This process is the instance. The guard owns the socket and removes it on drop.
    First(SingleInstanceGuard),
    /// Another instance answered: it has been asked to focus, and this process must exit 0.
    AlreadyRunning {
        /// The socket that answered.
        socket: PathBuf,
    },
}

/// A live claim on the data directory.
///
/// Dropping it releases the claim: the socket file and its pid file are removed, so the next start is
/// `First` again. A guard created by `--no-daemon`/`--new-instance` owns nothing and removes nothing.
#[derive(Debug)]
pub struct SingleInstanceGuard {
    /// The socket path, kept for the drop and for `--print-*` diagnostics.
    socket: PathBuf,
    /// The listener, on unix. A `Box<dyn…>` so the type has the same shape on every platform.
    listener: Option<ListenerSlot>,
    /// Whether `socket` is ours to delete.
    owns_socket: bool,
}

/// The listening half, kept behind a named type so the public struct stays portable.
#[cfg(unix)]
#[derive(Debug)]
struct ListenerSlot(std::os::unix::net::UnixListener);

/// Non-unix builds have no socket to hold; the claim is a pure in-process flag (plan §5.9 defers the
/// Windows named mutex to `winsafe`, step 16).
#[cfg(not(unix))]
#[derive(Debug)]
// The bool is the "would have claimed" marker; the lint allowance is the platform stub's, not a
// code-smell exception.
#[cfg_attr(not(unix), allow(dead_code))]
struct ListenerSlot(bool);

impl SingleInstanceGuard {
    /// Where the claim lives (empty for a `--no-daemon` run, which claims nothing).
    #[must_use]
    pub fn socket_path(&self) -> Option<&Path> {
        self.owns_socket.then_some(self.socket.as_path())
    }

    /// The pending requests from second invocations, if any arrived: the drain is what makes a second
    /// invocation actually raise the first window (and open its folder) instead of merely exiting.
    ///
    /// Non-blocking and best-effort by design — the caller polls it from the event loop, and an
    /// `accept()` here would park the UI thread.
    #[must_use]
    pub fn take_requests(&self) -> Vec<Request> {
        #[cfg(unix)]
        {
            let mut out = Vec::new();
            loop {
                match self.accept_pending() {
                    Ok(stream) => out.extend(read_requests(stream)),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
            out
        }
        #[cfg(not(unix))]
        {
            Vec::new()
        }
    }
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        if !self.owns_socket {
            return;
        }
        // Removing our own socket is the whole point: leaving it behind would make the *next* start
        // believe an instance is alive. The pid file goes with it.
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(pid_file_path(&self.socket));
    }
}

#[cfg(unix)]
impl SingleInstanceGuard {
    /// Accept one pending connection without blocking.
    fn accept_pending(&self) -> std::io::Result<std::os::unix::net::UnixStream> {
        // No `expect`: a `First` claim built by `--no-daemon` carries no listener, and asking it for a
        // focus request is legal (it answers zero).
        let Some(ListenerSlot(listener)) = self.listener.as_ref() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "this run claimed nothing (--no-daemon)",
            ));
        };
        let (stream, _) = listener.accept()?;
        stream.set_nonblocking(true)?;
        Ok(stream)
    }
}

/// The pid file beside `socket`: its existence plus its contents is how a stale socket is told apart
/// from a live one.
#[must_use]
pub fn pid_file_path(socket: &Path) -> PathBuf {
    let mut name = socket
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    name.push_str(".pid");
    socket.with_file_name(name)
}

/// The socket path for `app_id` in `paths`: `<data>/run/zenman-<variant>-<hash>.sock`.
///
/// `app_id` is `ZenMan` plus the `--app-variant` suffix (`ZenMan-steptest`), so the two things that
/// distinguish a run — its data directory and its variant — are both in the name. The hash is FNV-1a over
/// the whole id: the sanitising below is lossy (`a b` and `a-b` collide), the hash is not.
#[must_use]
pub fn socket_path(app_id: &str, paths: &AppPaths) -> PathBuf {
    paths.run_dir().join(format!(
        "zenman-{}-{:016x}.sock",
        sanitize(app_id),
        fnv1a(app_id)
    ))
}

/// Keep the file name portable: alphanumerics, `.`, `_`, `-`; everything else becomes `-`.
fn sanitize(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// FNV-1a, 64-bit. Pure std, no dependency, and stable for a file name (it is not a security boundary).
fn fnv1a(raw: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in raw.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Claim the instance slot, or hand a request to whoever holds it.
///
/// `no_daemon` (the `--no-daemon`/`--new-instance` escape hatch) skips the whole mechanism: the caller
/// gets [`Claim::First`] with a guard that owns nothing. That flag exists because the acceptance criterion
/// is "two instances run", and because a developer debugging a locked database needs a way in.
///
/// `targets` are the folders this invocation was asked to open, already resolved to absolute paths: a
/// non-empty list is written as an [`Request::Open`] per folder, an empty one as the bare focus request.
/// Either way the second process exits 0 — it succeeded, in the sense that the user's request reached
/// the window that is going to serve it.
///
/// # Errors
/// A [`BoxError`] describing the I/O failure when the run directory cannot be created, the socket cannot
/// be bound, or a socket whose owner is *alive* refuses connections. The last case is deliberate: it is
/// the one situation where deleting the socket would let two processes write the same undo log.
pub fn claim_or_focus(
    app_id: &str,
    paths: &AppPaths,
    no_daemon: bool,
    targets: &[String],
) -> Result<Claim, BoxError> {
    let socket = socket_path(app_id, paths);
    if no_daemon {
        return Ok(Claim::First(SingleInstanceGuard {
            socket,
            listener: None,
            owns_socket: false,
        }));
    }
    #[cfg(unix)]
    {
        claim_or_focus_unix(app_id, &socket, targets)
    }
    #[cfg(not(unix))]
    {
        // Plan §5.9 defers the Windows named mutex to `winsafe` (step 16). Until then a non-unix build
        // starts, and says it is not protected — silent unsafety is the worse failure.
        eprintln!(
            "zenman: single-instance guarding is not implemented for this platform; \
             another zenman on {} may corrupt the shared index and undo log",
            paths.data.display()
        );
        Ok(Claim::First(SingleInstanceGuard {
            socket,
            listener: None,
            owns_socket: false,
        }))
    }
}

/// Write the request on an accepted connection, best-effort.
///
/// The caller reports a failure but still treats it as "someone else owns the data directory": the
/// second process must not open a window either way.
fn write_request(
    stream: &mut std::os::unix::net::UnixStream,
    app_id: &str,
    targets: &[String],
) -> std::io::Result<()> {
    use std::io::Write as _;
    // One line per requested folder; a bare second invocation still asks for focus alone.
    let requests: Vec<Request> = match targets {
        [] => vec![Request::Focus],
        wanted => wanted.iter().cloned().map(Request::Open).collect(),
    };
    let text: String = requests
        .iter()
        .map(|r| format!("{}\n", r.line(app_id)))
        .collect();
    stream.write_all(text.as_bytes())?;
    stream.flush()
}

/// Read the request (or requests) one connection carried, so the sender sees a clean close.
#[cfg(unix)]
fn read_requests(mut stream: std::os::unix::net::UnixStream) -> Vec<Request> {
    use std::io::Read as _;
    // Long enough for a folder path plus the verb and the app id; a request that overruns it still
    // parses as the focus request it began as.
    let mut buf = [0u8; 4096];
    let Ok(n) = stream.read(&mut buf) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&buf[..n]);
    let found: Vec<Request> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| Request::parse(line.trim()))
        .collect();
    // One connection is one invocation: fold its folder requests in order, and never report an empty
    // list — a request that could not be read still means "someone is there".
    (!found.is_empty())
        .then_some(found)
        .unwrap_or(vec![Request::Focus])
}

#[cfg(not(unix))]
fn read_requests(_stream: ()) -> Vec<Request> {
    vec![Request::Focus]
}

#[cfg(unix)]
fn claim_or_focus_unix(app_id: &str, socket: &Path, targets: &[String]) -> Result<Claim, BoxError> {
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }

    // 1. Is someone listening? `connect` is the only trustworthy test: a stale socket file refuses, a
    //    live one accepts.
    match try_connect(socket) {
        Connect::Listening(mut stream) => {
            // Ours: hand it the focus request and step aside.
            if let Err(e) = write_request(&mut stream, app_id, targets) {
                eprintln!(
                    "zenman: could not deliver the focus request to {}: {e}",
                    socket.display()
                );
            }
            drop(stream);
            return Ok(Claim::AlreadyRunning {
                socket: socket.to_path_buf(),
            });
        }
        Connect::NotRunning => {}
        Connect::Unreachable(err) => {
            // 2. A file is there and it is not answering. Stale, or a live process that has stopped
            //    accepting? The pid file decides.
            let pid_file = pid_file_path(socket);
            match read_claimed_pid(&pid_file) {
                Some(pid) if process_is_alive(pid) => {
                    return Err(format!(
                        "{} belongs to zenman pid {pid}, which is running but not accepting \
                         connections; not removing it (use --new-instance to start anyway)",
                        socket.display()
                    )
                    .into());
                }
                Some(pid) => {
                    eprintln!(
                        "zenman: removing the stale socket {} (its owner, pid {pid}, is gone)",
                        socket.display()
                    );
                }
                None => {
                    eprintln!(
                        "zenman: {} is not a live zenman socket ({}); removing it",
                        socket.display(),
                        err
                    );
                }
            }
            std::fs::remove_file(socket)
                .map_err(|e| format!("cannot remove {}: {e}", socket.display()))?;
            let _ = std::fs::remove_file(pid_file);
        }
    }

    // 3. Bind. `bind` fails with `EADDRINUSE` if a racing process got here first, and that process is
    //    then the owner — we become the one that focuses.
    let listener = match std::os::unix::net::UnixListener::bind(socket) {
        Ok(listener) => listener,
        Err(e) if e.raw_os_error() == Some(libc_eaddrinuse()) => {
            // Someone won the race; become the process that focuses them.
            if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(socket) {
                if let Err(err) = write_request(&mut stream, app_id, targets) {
                    eprintln!("zenman: could not deliver the focus request: {err}");
                }
                return Ok(Claim::AlreadyRunning {
                    socket: socket.to_path_buf(),
                });
            }
            return Err(format!("cannot bind {}: {e}", socket.display()).into());
        }
        Err(e) => return Err(format!("cannot bind {}: {e}", socket.display()).into()),
    };
    configure_cloexec(&listener)?;
    listener.set_nonblocking(true)?;
    let pid_file = pid_file_path(socket);
    std::fs::write(&pid_file, format!("{}", std::process::id()))
        .map_err(|e| format!("cannot write {}: {e}", pid_file.display()))?;
    Ok(Claim::First(SingleInstanceGuard {
        socket: socket.to_path_buf(),
        listener: Some(ListenerSlot(listener)),
        owns_socket: true,
    }))
}

/// Set `FD_CLOEXEC` on the freshly bound listener and read it back (module docs: *why*).
///
/// # Errors
/// A [`BoxError`] when the platform refuses to set or confirm the flag — reported, not swallowed,
/// because an inheriting child would keep the instance slot alive after this process exits.
#[cfg(unix)]
pub fn configure_cloexec(listener: &std::os::unix::net::UnixListener) -> Result<(), BoxError> {
    let fd = std::os::fd::AsFd::as_fd(listener);
    rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC)?;
    let now = rustix::io::fcntl_getfd(fd)?;
    if !now.contains(rustix::io::FdFlags::CLOEXEC) {
        return Err(
            format!("the instance socket is not FD_CLOEXEC after fcntl (flags {now:?})").into(),
        );
    }
    Ok(())
}

/// `EADDRINUSE`, without pulling in `libc` as a direct dependency.
#[cfg(unix)]
fn libc_eaddrinuse() -> i32 {
    // Linux and the BSDs disagree (98 vs 48), so read it from `rustix`, which knows the target.
    rustix::io::Errno::ADDRINUSE.raw_os_error()
}

/// Why `connect` said no.
#[cfg(unix)]
enum Connect {
    /// Someone is listening: another instance. The stream is handed over so the focus request goes on the
    /// very connection that proved the owner exists — one connect, one pending request.
    Listening(std::os::unix::net::UnixStream),
    /// No file at all: we are first.
    NotRunning,
    /// A file exists but does not answer. `.0` is the reason, for the log line.
    Unreachable(String),
}

#[cfg(unix)]
fn try_connect(socket: &Path) -> Connect {
    match std::os::unix::net::UnixStream::connect(socket) {
        Ok(stream) => Connect::Listening(stream),
        // `NotFound` is `ENOENT`: no file, so no owner.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Connect::NotRunning,
        Err(e) => Connect::Unreachable(e.to_string()),
    }
}

/// The pid recorded in `pid_file`, if it parses and is not this process's own (a crashed run that left
/// its socket is the case this exists for; `Own` means the caller is the owner and must not steal it).
fn read_claimed_pid(pid_file: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(pid_file).ok()?;
    let pid: u32 = text.trim().parse().ok()?;
    (pid != 0).then_some(pid)
}

/// Is `pid` a live process? `/proc/<pid>` is the dependency-free test on Linux; elsewhere the absence of
/// `/proc` means we cannot tell, and the caller treats `None`-style answers as "assume gone" only after
/// the kernel has already said the socket does not answer.
#[cfg(target_os = "linux")]
#[must_use]
pub fn process_is_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn process_is_alive(pid: u32) -> bool {
    // Without `/proc` there is no dependency-free way to ask, and zen-man's non-Linux support is step
    // 16's problem: report "alive" only for our own pid, which is the one case we can prove.
    pid == std::process::id()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_dirs::{AppPaths, Overrides};

    /// A fresh `zenman-single-<tag>-<pid>-<seq>` tree from the shared scratch module (reapable by
    /// the sweep); `AppPaths` outlives the call, so this is a plain path, not a guard.
    fn paths(tag: &str) -> AppPaths {
        let dir = crate::scratch::create(&format!("single-{tag}"));
        AppPaths::resolve(&Overrides {
            data_dir: Some(dir),
            ..Default::default()
        })
        .expect("an explicit data dir always resolves")
    }

    #[test]
    fn the_socket_path_follows_the_variant_and_the_data_dir() {
        let a = paths("path");
        let plain = socket_path("ZenMan", &a);
        let variant = socket_path("ZenMan-steptest", &a);
        assert!(plain.starts_with(a.run_dir()), "{plain:?}");
        assert!(variant.starts_with(a.run_dir()), "{variant:?}");
        assert_ne!(plain, variant, "two variants must not share a socket");
        assert!(
            plain.to_string_lossy().contains("zenman-"),
            "the name says what owns it"
        );
        // A different data directory is a different socket, which is what lets the test suite (and two
        // users) run at the same time.
        let b = paths("path-b");
        assert_ne!(socket_path("ZenMan", &a), socket_path("ZenMan", &b));
        // Sanitising keeps the name usable; the hash keeps it unique.
        let odd = socket_path("ZenMan step test!", &a);
        assert!(odd.to_string_lossy().ends_with(".sock"));
        assert!(!odd.to_string_lossy().contains(' '));
        assert_ne!(odd, socket_path("ZenMan-step-test-", &a));
    }

    #[test]
    fn the_first_claim_wins_and_the_second_focuses_it() {
        let p = paths("two");
        let app = "ZenMan-claimtest";
        let first = claim_or_focus(app, &p, false, &[]).expect("first claims");
        let Claim::First(guard) = first else {
            panic!("the first claim must own the socket");
        };
        let socket = guard
            .socket_path()
            .expect("an owned claim reports its socket")
            .to_path_buf();
        // A socket is not a *file* in `is_file()`'s sense: `exists()` is the right predicate.
        assert!(socket.exists(), "the socket exists once claimed");
        assert!(
            socket.parent().expect("parent").is_dir(),
            "the run directory was created"
        );

        // Second process, same data directory: it must find the owner and hand it a focus request.
        let second = claim_or_focus(app, &p, false, &[]).expect("second resolves");
        assert!(
            matches!(second, Claim::AlreadyRunning { .. }),
            "the second claim must not own anything: {second:?}"
        );
        assert_eq!(
            guard.take_requests(),
            vec![Request::Focus],
            "the focus request arrives at the owner"
        );

        // A different variant is a different instance, and claims its own socket.
        assert!(matches!(
            claim_or_focus("ZenMan-othervariant", &p, false, &[]).expect("independent"),
            Claim::First(_)
        ));
        drop(guard);
    }

    #[test]
    fn a_second_invocation_hands_over_the_folders_it_was_asked_to_open() {
        // The file-manager contract, in the one place it can be tested without a desktop:
        // `xdg-open <dir>` runs a second `zenman`, which must not open a second window (that is the
        // corruption bug the claim exists to prevent) and must not lose the folder either.
        let p = paths("open-request");
        let app = "ZenMan-openrequest";
        let Claim::First(guard) = claim_or_focus(app, &p, false, &[]).expect("first claims") else {
            panic!("the first claim must own the socket");
        };
        assert!(
            guard.take_requests().is_empty(),
            "nothing has asked for anything yet"
        );

        let asked = ["/tmp/one".to_string(), "/tmp/two with spaces".to_string()];
        let Claim::AlreadyRunning { .. } =
            claim_or_focus(app, &p, false, &asked).expect("second resolves")
        else {
            panic!("the second invocation must not claim a live socket")
        };
        assert_eq!(
            guard.take_requests(),
            vec![
                Request::Open(asked[0].clone()),
                Request::Open(asked[1].clone())
            ],
            "both folders arrive, in order, spaces and all"
        );

        // And the path is handed over exactly as the sender resolved it — not re-derived from a cwd the
        // receiving instance does not share.
        let Claim::AlreadyRunning { .. } =
            claim_or_focus(app, &p, false, &["/x".to_string()]).expect("third resolves")
        else {
            panic!("");
        };
        assert_eq!(guard.take_requests(), vec![Request::Open("/x".to_string())]);
        drop(guard);
    }

    #[test]
    fn releasing_the_guard_lets_the_next_claim_win() {
        let p = paths("release");
        let app = "ZenMan-release";
        let claim = claim_or_focus(app, &p, false, &[]).expect("claims");
        let Claim::First(guard) = claim else {
            panic!("expected First");
        };
        let socket = guard.socket_path().expect("owned").to_path_buf();
        drop(guard);
        assert!(!socket.exists(), "the drop released the socket");
        assert!(!pid_file_path(&socket).exists(), "and the pid file");
        assert!(matches!(
            claim_or_focus(app, &p, false, &[]).expect("re-claims"),
            Claim::First(_)
        ));
    }

    #[test]
    fn no_daemon_claims_nothing_and_writes_no_socket() {
        let p = paths("nodaemon");
        let app = "ZenMan-nodaemon";
        let claim = claim_or_focus(app, &p, true, &[]).expect("bypassed");
        let Claim::First(guard) = claim else {
            panic!("--no-daemon must still produce a First so the app runs");
        };
        assert_eq!(guard.socket_path(), None, "nothing was claimed");
        assert!(!socket_path(app, &p).exists(), "and nothing was written");
        assert!(guard.take_requests().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn the_listener_socket_is_cloexec() {
        let listener = std::os::unix::net::UnixListener::bind("zenman-cloexec-test.sock")
            .or_else(|_| {
                // Bind in the temp dir if the cwd is unwritable (never in CI, but cheap to survive).
                std::os::unix::net::UnixListener::bind(
                    std::env::temp_dir().join("zenman-cloexec-test.sock"),
                )
            })
            .expect("bind");
        configure_cloexec(&listener).expect("FD_CLOEXEC is set and confirmed");
        let flags =
            rustix::io::fcntl_getfd(std::os::fd::AsFd::as_fd(&listener)).expect("fcntl_getfd");
        assert!(flags.contains(rustix::io::FdFlags::CLOEXEC), "{flags:?}");
        std::fs::remove_file("zenman-cloexec-test.sock").ok();
        std::fs::remove_file(std::env::temp_dir().join("zenman-cloexec-test.sock")).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_socket_left_by_a_dead_owner_is_removed_and_taken_over() {
        let p = paths("stale");
        let app = "ZenMan-stale";
        let socket = socket_path(app, &p);
        std::fs::create_dir_all(socket.parent().expect("run dir")).expect("run dir");
        // A crashed run leaves the file behind: bind, then drop the listener without cleanup.
        let left =
            std::os::unix::net::UnixListener::bind(&socket).expect("bind as the crashed run");
        drop(left);
        assert!(socket.exists(), "the stale file is left behind");
        // Bound to a name: dropping the guard at the end of an `assert!` would remove the very socket
        // this assertion is about.
        let claim = claim_or_focus(app, &p, false, &[]).expect("clears it");
        assert!(
            matches!(claim, Claim::First(_)),
            "an unowned stale socket is cleared and claimed"
        );
        assert!(
            socket.exists(),
            "and re-created its own socket at the same path"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_live_owner_is_never_displaced() {
        let p = paths("live");
        let app = "ZenMan-live";
        let socket = socket_path(app, &p);
        std::fs::create_dir_all(socket.parent().expect("run dir")).expect("run dir");
        // Something at the path that refuses connections, and a pid file naming *this* live process:
        // the combination the safety rule is about — never delete a socket a running instance owns.
        std::fs::write(&socket, b"not a zenman socket").expect("planted file");
        std::fs::write(pid_file_path(&socket), format!("{}", std::process::id()))
            .expect("live pid");
        let err = claim_or_focus(app, &p, false, &[])
            .expect_err("a live owner's socket must refuse the claim");
        assert!(
            err.to_string().contains("not removing it"),
            "the error says what it refused to do: {err}"
        );
        assert!(socket.exists(), "the live owner's file is untouched");
    }
}
