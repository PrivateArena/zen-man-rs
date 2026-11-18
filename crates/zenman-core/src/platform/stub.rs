//! The platform nobody has written yet — plan §5.2 `platform/mod.rs` row's `Env` trait made honest for a
//! host with no implementation.
//!
//! This is the `#[cfg(not(target_os = "linux"))]` path in the sense that matters: it is what
//! [`crate::platform::default_env`] hands out on Windows until step 16 lands `platform/windows.rs`, and it
//! compiles on every target (nothing here may name `std::os::unix`, `rustix` or `/proc`).
//!
//! The rule is *typed refusal, never a panic and never a silent success*: an action answers
//! [`CoreError::Unsupported`] naming the capability, so a UI built on top of this still shows the user a
//! real reason instead of doing nothing. The two infallible questions (`can_trash`) and the two read-only
//! enumerations (`disk_space`, `mount_entries`) answer empty-but-valid, because a caller that has to handle
//! "no disk meter" and "no volumes" is a caller that does not panic on an empty table — plan §5.2's
//! `mounts()` row: "`vec![]` … (no error)".

use std::path::Path;

use super::{
    CopyItem, CopyReport, DiskSpace, Env, ExecOutput, ExecRequest, LinkItem, MountEntry,
    MountTable, Os, PlatformCaps, PlatformFuture, Shortcut, ready,
};
use crate::error::CoreError;
use crate::mode::ModeBits;
use crate::path::AbsPath;

/// An `Env` with no platform behind it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Unsupported {
    /// What to name in the errors, so a stray use of the stub in a test says which lane it came from.
    what: &'static str,
}

impl Unsupported {
    /// The stub `default_env` returns on an unimplemented OS.
    #[must_use]
    pub fn new() -> Self {
        Self {
            what: "this platform (platform/windows.rs lands in plan step 16)",
        }
    }

    /// A stub that blames a named component — used by tests that must prove an error came from the stub
    /// and not from a real platform refusing one specific thing.
    #[must_use]
    pub fn labelled(what: &'static str) -> Self {
        Self { what }
    }

    /// The label this stub puts in [`CoreError::Unsupported::what`].
    #[must_use]
    pub fn label(&self) -> &'static str {
        self.what
    }

    fn unsupported(&self) -> CoreError {
        CoreError::Unsupported { what: self.what }
    }
}

impl Env for Unsupported {
    fn os(&self) -> Os {
        // The honest answer is still the OS we are on; it is the *implementation* that is missing.
        Os::current()
    }

    fn caps(&self) -> PlatformCaps {
        // Nothing is known to work, so nothing may be shown: the UI's `MenuContext::can_symlink` and the
        // disk meter both switch off from this.
        PlatformCaps::empty()
    }

    fn open_with(&self, _path: &AbsPath) -> PlatformFuture<'_, Result<(), CoreError>> {
        ready(Err(self.unsupported()))
    }

    fn copy_paths(&self, _items: &[CopyItem]) -> PlatformFuture<'_, Result<CopyReport, CoreError>> {
        ready(Err(self.unsupported()))
    }

    fn paste_link(&self, _items: &[LinkItem]) -> PlatformFuture<'_, Result<(), CoreError>> {
        ready(Err(CoreError::SymlinkUnsupported))
    }

    fn supports_symlink(&self) -> Result<bool, CoreError> {
        // There is no probe to run and no cache to read: the platform cannot say more than "no links".
        Err(CoreError::SymlinkUnsupported)
    }

    fn chmod(
        &self,
        _targets: &[AbsPath],
        _mode: ModeBits,
    ) -> PlatformFuture<'_, Result<(), CoreError>> {
        ready(Err(self.unsupported()))
    }

    fn can_trash(&self, _path: &AbsPath) -> bool {
        false
    }

    fn trash(&self, _path: &AbsPath) -> Result<bool, CoreError> {
        Err(self.unsupported())
    }

    fn create_shortcut(&self, _shortcut: &Shortcut) -> Result<(), CoreError> {
        Err(self.unsupported())
    }

    fn disk_space(&self, _path: &AbsPath) -> Result<DiskSpace, CoreError> {
        // `0/0` is the "no meter" answer: `widgets/disk_meter.rs` renders it as hidden, and every caller
        // that would have had to `?` a platform error now cannot fail.
        Ok(DiskSpace::default())
    }

    fn mount_entries(&self) -> Result<MountTable, CoreError> {
        Ok(MountTable {
            entries: Vec::<MountEntry>::new(),
        })
    }

    fn exec_program(&self, req: ExecRequest) -> PlatformFuture<'_, Result<ExecOutput, CoreError>> {
        // Spawning is not a platform *capability* question the way `chmod` is: a host with no
        // implementation cannot run the user's custom action, and saying so beats an empty success.
        let _ = req;
        ready(Err(self.unsupported()))
    }

    fn has_program(&self, _name: &str) -> Result<bool, CoreError> {
        // The trait's default body already refuses; this override only makes the refusal *this* stub's,
        // which is what `every_trait_method_is_answered_with_a_typed_error` checks the label of.
        Err(self.unsupported())
    }

    fn is_writable(&self, _path: &AbsPath) -> Result<bool, CoreError> {
        Err(self.unsupported())
    }
}

/// The one place the "is this path mounted/removable" question is answered without a mount table: an empty
/// table says no to everything, which is the conservative reading.
#[must_use]
pub fn no_mounts_for(_path: &Path) -> MountTable {
    MountTable::empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};

    fn poll_once<T>(mut future: PlatformFuture<'_, T>) -> T {
        // A noop waker is enough: none of these futures ever returns `Pending` (`zenman-core` has no
        // executor — plan §9.1 — and `ready` is complete by construction).
        let mut cx = Context::from_waker(Waker::noop());
        let pinned: Pin<&mut dyn Future<Output = T>> = future.as_mut();
        match pinned.poll(&mut cx) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("the stub must answer immediately"),
        }
    }

    /// Every method, with its outcome reduced to the one thing the table asserts.
    fn outcomes(env: &Unsupported, path: &AbsPath) -> Vec<(&'static str, String)> {
        let mode = ModeBits::DEFAULT_FILE;
        let item = CopyItem {
            source: path.clone(),
            dest: path.clone(),
        };
        let link = LinkItem {
            link: path.clone(),
            target: path.clone(),
        };
        let shortcut = Shortcut {
            link: path.clone(),
            target: path.clone(),
            arguments: None,
            description: None,
            working_dir: None,
        };
        let req = ExecRequest::argv("true", Vec::<String>::new());

        let mut rows: Vec<(&'static str, Result<(), CoreError>)> = vec![
            ("open_with", poll_once(env.open_with(path))),
            ("copy_paths", poll_once(env.copy_paths(&[item])).map(|_| ())),
            ("paste_link", poll_once(env.paste_link(&[link]))),
            ("supports_symlink", env.supports_symlink().map(|_| ())),
            (
                "chmod",
                poll_once(env.chmod(std::slice::from_ref(path), mode)),
            ),
            ("trash", env.trash(path).map(|_| ())),
            ("create_shortcut", env.create_shortcut(&shortcut)),
            ("disk_space", env.disk_space(path).map(|_| ())),
            ("mount_entries", env.mount_entries().map(|_| ())),
            ("exec_program", poll_once(env.exec_program(req)).map(|_| ())),
            ("has_program", env.has_program("sh").map(|_| ())),
            ("is_writable", env.is_writable(path).map(|_| ())),
        ];

        let mut table: Vec<(&'static str, String)> = rows
            .drain(..)
            .map(|(name, result)| {
                let text = match result {
                    Ok(()) => "ok".to_string(),
                    Err(CoreError::Unsupported { what }) => format!("unsupported:{what}"),
                    Err(other) => format!("error:{}", other.kind()),
                };
                (name, text)
            })
            .collect();
        // The one `bool` answer has no error to report, so it is recorded as its value.
        table.push(("can_trash", format!("bool:{}", env.can_trash(path))));
        // `os`/`caps` are the two infallible reads and are covered by `the_stub_still_knows_what_it_is`.
        table
    }

    #[test]
    fn every_trait_method_is_answered_with_a_typed_error() {
        let env = Unsupported::labelled("stub-lane");
        let path = AbsPath::from_path(std::env::temp_dir()).expect("the temp dir is absolute");
        let rows = outcomes(&env, &path);

        // The trait has exactly these fourteen methods; a fifteenth would have to be added here to compile.
        let names: Vec<&str> = rows.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            names,
            vec![
                "open_with",
                "copy_paths",
                "paste_link",
                "supports_symlink",
                "chmod",
                "trash",
                "create_shortcut",
                "disk_space",
                "mount_entries",
                "exec_program",
                "has_program",
                "is_writable",
                "can_trash"
            ]
        );

        for (name, answer) in &rows {
            match *name {
                // The two read-only enumerations answer empty-but-valid so a caller cannot fail on them.
                "disk_space" | "mount_entries" => assert_eq!(answer, "ok", "{name} must not error"),
                // Symlinks have their own variant, because `paste_link` has a documented fallback.
                "paste_link" | "supports_symlink" => {
                    assert_eq!(answer, "error:symlink_unsupported", "{name}");
                }
                "can_trash" => assert_eq!(answer, "bool:false", "{name} must be a refusal"),
                _ => assert_eq!(
                    answer, "unsupported:stub-lane",
                    "{name} must name the missing platform"
                ),
            }
        }
    }

    #[test]
    fn the_stub_still_knows_what_it_is() {
        let env = Unsupported::new();
        assert_eq!(env.os(), Os::current());
        assert!(
            env.caps().is_empty(),
            "an unimplemented platform may not advertise a capability"
        );
        assert_eq!(
            env.label(),
            "this platform (platform/windows.rs lands in plan step 16)"
        );
        assert!(super::no_mounts_for(Path::new("/")).entries.is_empty());
    }

    #[test]
    fn the_empty_table_refuses_every_question() {
        let table = env_mount_table();
        assert_eq!(table, MountTable::empty());
        assert!(!table.is_mounted(Path::new("/")));
        assert!(!table.is_removable(Path::new("/")));
        assert!(table.mount_for(Path::new("/")).is_none());
    }

    fn env_mount_table() -> MountTable {
        Unsupported::new().mount_entries().expect("never an error")
    }
}
