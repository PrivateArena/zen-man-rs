//! The pre-flight checks every mutating operation runs before it touches a byte (plan §5.3
//! `ops/guard.rs`).
//!
//! Go spread these across three places with different strictness: `validateDest` was a **substring**
//! test for `..` (so `name..txt` was refused — `archive-extract-dotdot-falsepositive.json`, deviation
//! ledger #14), the containment check was inline in the paste loop (`api/ops.go:309`) and *silent*, and
//! the archive read-only rule was a per-handler `if`. Here they are functions with names, and each has
//! one answer:
//!
//! * [`vfs_write_guard`] — a write targeting an archive store is [`CoreError::VfsReadOnly`];
//! * [`dest_text_guard`] / [`dest_guard`] — a destination is component-wise legal (a `..` **component** is
//!   refused, `foo..bar` is not, a glob is never a destination);
//! * [`ensure_writable`] — the OS agrees this process may write here *now* (`access(2)`/`W_OK`, not the
//!   mode bits, which a caller would have to re-derive for ACLs and read-only mounts);
//! * [`guard_sources`] — the containment table: which sources may land where, and which are refused.

use crate::error::{CoreError, PathReject};
use crate::path::{AbsPath, FsPath, contains_dot_component, validate_dest_name};
use crate::platform::Env;

/// Why [`guard_sources`] refused one source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceReject {
    /// The destination is the source itself (`cp dir dir`, `api/ops.go:309`'s `src == dst` half).
    SamePath,
    /// The destination lands inside the source's own subtree (`api/ops.go:309`'s `isSubdir` half).
    DestInsideSource,
}

/// One source that survived the guard, with the destination the paste would give it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowedSource {
    /// What is being copied or moved.
    pub source: AbsPath,
    /// `dest.join(base(source))` — the path Go built at `api/ops.go:307`, here once instead of per
    /// operation so the guard and the executor cannot disagree about it.
    pub target: AbsPath,
}

/// The outcome of a containment check over a whole selection: what may proceed, and what was refused
/// and why. Go kept neither list — it `continue`d, and a selection that was entirely refused looked like
/// a successful no-op (deviation, plan §6.2/§9.2).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceGuard {
    /// The sources that may be written, in the caller's order.
    pub allowed: Vec<AllowedSource>,
    /// `(source, why)` for every refused source, in the caller's order.
    pub rejected: Vec<(AbsPath, SourceReject)>,
}

impl SourceGuard {
    /// `true` when nothing may proceed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.allowed.is_empty()
    }

    /// The allowed targets, in order — what `Env::copy_paths`/`paste_link` will be handed.
    #[must_use]
    pub fn targets(&self) -> Vec<AbsPath> {
        self.allowed.iter().map(|a| a.target.clone()).collect()
    }
}

/// Check a whole selection against one destination directory.
///
/// The rule is Go's (`api/ops.go:309`): with `target = dest.join(base(source))`, refuse when
/// `source == target` ([`SourceReject::SamePath`]) or when the target is inside the source's subtree
/// ([`SourceReject::DestInsideSource`]). Two deliberate differences:
///
/// * the comparison is [`AbsPath::is_ancestor_of`], i.e. component-wise on already-cleaned paths, which
///   is what Go's `isSubdir` string compare managed only *because* both sides were `Clean`ed;
/// * no `stat` is taken. Go gated the subtree test on `isSourceDir(source)`; this guard has no filesystem,
///   and needs none — a *file* source cannot have a directory inside it, so a paste that would have been
///   skipped by Go's `stat` cannot arise here (the destination is an existing directory by contract).
///
/// The `Err` is not about containment: it is a source with no name at all (`/`), for which no
/// destination can be built. An empty `sources` list is [`CoreError::Empty`] — Go's `"no source folder
/// specified"` (`api/ops.go:190`) and `"No files selected"` (`api/ops.go:146`).
pub fn guard_sources(sources: &[AbsPath], dest: &AbsPath) -> Result<SourceGuard, CoreError> {
    if sources.is_empty() {
        return Err(CoreError::Empty { what: "selection" });
    }
    let mut guard = SourceGuard::default();
    for source in sources {
        let name = source.file_name().ok_or_else(|| CoreError::InvalidPath {
            raw: source.to_string_lossy(),
            reason: PathReject::Empty,
        })?;
        let target = dest.join(&name.to_string_lossy())?;
        if target == *source {
            guard
                .rejected
                .push((source.clone(), SourceReject::SamePath));
        } else if source.is_ancestor_of(&target) {
            guard
                .rejected
                .push((source.clone(), SourceReject::DestInsideSource));
        } else {
            guard.allowed.push(AllowedSource {
                source: source.clone(),
                target,
            });
        }
    }
    Ok(guard)
}

/// Turn a [`SourceGuard`] into a go/no-go answer for the operation as a whole.
///
/// `Ok(())` when at least one source may proceed — Go then silently did the partial paste. When *every*
/// source was refused, the operation is a no-op the user did not ask for, so it fails with the first
/// refusal ([`CoreError::DestInsideSource`], naming the target that would have landed inside it); the
/// full table stays available on the guard for a caller that wants to report the partial case.
pub fn ensure_guarded(guard: &SourceGuard, dest: &AbsPath) -> Result<(), CoreError> {
    if !guard.allowed.is_empty() {
        return Ok(());
    }
    match guard.rejected.first() {
        Some((source, _)) => Err(CoreError::DestInsideSource {
            src: source.to_string_lossy(),
            // Re-derived the same way `guard_sources` built it, so the message cannot name a path the
            // guard did not consider.
            dest: {
                let name = source
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                dest.join(&name).map_or_else(
                    |_| dest.to_string_lossy(),
                    |target| target.to_string_lossy(),
                )
            },
        }),
        // `guard_sources` never produces an empty-on-both-sides guard; a hand-built one is "nothing to do".
        None => Err(CoreError::Empty { what: "selection" }),
    }
}

/// Reject a write whose target lives inside an archive (`api/ops.go:73,78` → `vfs_readonly`).
///
/// Reads are of course fine — this is the *write* guard the plan §5.3 row names, and the reason it takes
/// the store-typed [`FsPath`] rather than an [`AbsPath`]: only an `FsPath` knows which store it is in.
pub fn vfs_write_guard(paths: &[&FsPath]) -> Result<(), CoreError> {
    for path in paths {
        if path.store.is_archive() {
            return Err(CoreError::VfsReadOnly {
                path: path.to_string(),
            });
        }
    }
    Ok(())
}

/// Read a **destination** as the store its text addresses, with the one answer [`FsPath::resolve`]
/// cannot give from the name alone: an archive-named component that is a *directory* on disk is not a
/// container.
///
/// The parse split is name-based (plan §5.2's case 6), so `/a/b.zip/inner` reads as an archive path
/// whether `b.zip` is a container or the real directory of `docs/contracts.md` ledger #13. For a *read*
/// that ambiguity is harmless — the reader opens the file and fails honestly — but a **write** guard
/// must not refuse a folder for the shape of its name, which is the exact Go defect that ledger records.
/// So the question goes to the filesystem: an existing host that is **not** a directory is a store, and
/// [`vfs_write_guard`] refuses it; anything else — a directory, or nothing there yet — stays the plain
/// path, so a missing destination is reported as missing ([`ensure_writable`] and the write itself) and
/// not as a read-only archive.
#[must_use]
pub fn dest_store(source: &dyn crate::services::dirs::FsSource, dest: &AbsPath) -> FsPath {
    let resolved = FsPath::resolve(&dest.to_string_lossy(), std::path::Path::new("/"), None)
        .unwrap_or_else(|_| FsPath::from_abs(dest.clone()));
    if !resolved.store.is_archive() {
        return resolved;
    }
    let host_is_dir = resolved
        .store
        .host()
        .and_then(|host| source.lstat(host.as_path()).ok())
        .is_some_and(|info| info.kind.is_navigable_dir());
    if host_is_dir {
        FsPath::from_abs(dest.clone())
    } else {
        resolved
    }
}

/// The raw-text half of the destination rule — what Go's `validateDest` was applied to, the JSON string
/// before any parsing (`archive_extract.go:57-66`).
///
/// Component-wise, so `foo..bar` and `a..b.txt` are allowed and `/tmp/../etc/passwd` is refused
/// (plan F19, `archive-extract-dotdot-falsepositive.json`): the check runs on the *text* because that is
/// the only place a `..` component still exists — both [`AbsPath`] and [`FsPath`] have already cleaned or
/// rejected it by the time they are built, so a guard that only saw the parsed form would be a tautology.
pub fn dest_text_guard(raw: &str) -> Result<(), CoreError> {
    if contains_dot_component(raw) {
        return Err(CoreError::InvalidPath {
            raw: raw.to_string(),
            reason: PathReject::EscapesRoot,
        });
    }
    Ok(())
}

/// Check a parsed destination: the store may not be an archive, and every component of its relative
/// path must be a legal *destination* name (no `..`, no `.`, no glob metacharacters).
///
/// The archive half is the extract-into-a-`.zip` case; the component half is [`dest_text_guard`]'s rule
/// applied to what survived parsing, plus [`validate_dest_name`]'s rejection of `*?[`, which is never a
/// literal destination (plan F19).
pub fn dest_guard(dest: &FsPath) -> Result<(), CoreError> {
    if dest.store.is_archive() {
        return Err(CoreError::VfsReadOnly {
            path: dest.to_string(),
        });
    }
    dest_text_guard(&dest.relative)?;
    for component in dest.relative.split('/') {
        if component.is_empty() {
            continue;
        }
        validate_dest_name(component)?;
    }
    Ok(())
}

/// Ask the platform whether this process may write `path` right now, and turn the answer into the error
/// the UI already knows how to show ([`CoreError::PermissionDenied`], the Go 500 of
/// `docs/contracts.md` §2.4 *Permission errors*).
///
/// A platform that cannot answer ([`CoreError::Unsupported`]) gets the benefit of the doubt: this is a
/// pre-flight check, and inventing a rejection the write itself would not have produced is worse than
/// letting the real `io::Error` surface from the operation.
pub fn ensure_writable(env: &dyn Env, path: &AbsPath) -> Result<(), CoreError> {
    match env.is_writable(path) {
        Ok(true) => Ok(()),
        Ok(false) => Err(CoreError::PermissionDenied {
            path: path.to_string_lossy(),
        }),
        Err(CoreError::Unsupported { .. }) => Ok(()),
        Err(other) => Err(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::VfsKind;

    fn abs(text: &str) -> AbsPath {
        AbsPath::from_path(text).unwrap_or_else(|_| panic!("`{text}` is an absolute path"))
    }

    /// The `Env` of a platform that can answer `is_writable`: the trait's default body would answer
    /// `Unsupported` for everything, which is the other branch this module tests.
    struct Writable {
        answer: bool,
    }

    impl Env for Writable {
        fn os(&self) -> crate::platform::Os {
            crate::platform::Os::Linux
        }
        fn caps(&self) -> crate::platform::PlatformCaps {
            crate::platform::PlatformCaps::empty()
        }
        fn open_with(
            &self,
            _: &AbsPath,
        ) -> crate::platform::PlatformFuture<'_, Result<(), CoreError>> {
            crate::platform::ready(Err(CoreError::Cancelled))
        }
        fn copy_paths(
            &self,
            _: &[crate::platform::CopyItem],
        ) -> crate::platform::PlatformFuture<'_, Result<crate::platform::CopyReport, CoreError>>
        {
            crate::platform::ready(Err(CoreError::Cancelled))
        }
        fn paste_link(
            &self,
            _: &[crate::platform::LinkItem],
        ) -> crate::platform::PlatformFuture<'_, Result<(), CoreError>> {
            crate::platform::ready(Err(CoreError::Cancelled))
        }
        fn supports_symlink(&self) -> Result<bool, CoreError> {
            Ok(false)
        }
        fn chmod(
            &self,
            _: &[AbsPath],
            _: crate::mode::ModeBits,
        ) -> crate::platform::PlatformFuture<'_, Result<(), CoreError>> {
            crate::platform::ready(Err(CoreError::Cancelled))
        }
        fn can_trash(&self, _: &AbsPath) -> bool {
            false
        }
        fn trash(&self, _: &AbsPath) -> Result<bool, CoreError> {
            Err(CoreError::Cancelled)
        }
        fn create_shortcut(&self, _: &crate::platform::Shortcut) -> Result<(), CoreError> {
            Err(CoreError::Cancelled)
        }
        fn disk_space(&self, _: &AbsPath) -> Result<crate::platform::DiskSpace, CoreError> {
            Ok(crate::platform::DiskSpace::default())
        }
        fn mount_entries(&self) -> Result<crate::platform::MountTable, CoreError> {
            Ok(crate::platform::MountTable::empty())
        }
        fn exec_program(
            &self,
            _: crate::platform::ExecRequest,
        ) -> crate::platform::PlatformFuture<'_, Result<crate::platform::ExecOutput, CoreError>>
        {
            crate::platform::ready(Err(CoreError::Cancelled))
        }
        fn is_writable(&self, _: &AbsPath) -> Result<bool, CoreError> {
            Ok(self.answer)
        }
    }

    #[test]
    fn is_subdir_table_matches_go() {
        // `TestIsSubdir` (`api/ops_inside_test.go:110-127`), row for row, through `is_ancestor_of`.
        // The two `..` rows arrive already cleaned, which is what `AbsPath` guarantees; Go cleaned them
        // itself inside `isSubdir` — same input, same answer.
        let rows: [(&str, &str, bool); 5] = [
            ("/a/b", "/a/b", true),
            ("/a/b", "/a/b/c", true),
            ("/a/b", "/a/bc", false),
            ("/a/b", "/a/b/../b", true),
            ("/a/b", "/a/b/../b/c", true),
        ];
        for (parent, child, want) in rows {
            let parent = abs(parent);
            // `/a/b/../b` parses to `/a/b`: the ancestor test is inclusive, so the equal row is `true`
            // exactly as Go's `parent == child` shortcut returned it.
            let got = parent.is_ancestor_of(&abs(child));
            assert_eq!(got, want, "is_ancestor_of({parent:?}, {child:?})");
        }
    }

    #[test]
    fn guard_into_own_subtree_is_rejected() {
        // `cp /tree/a /tree/a/b` — Go skipped it in silence (`api/ops.go:309-311`).
        let guard = guard_sources(&[abs("/tree/a")], &abs("/tree/a/b")).expect("a nameable source");
        assert!(guard.allowed.is_empty(), "{guard:?}");
        assert_eq!(
            guard.rejected,
            vec![(abs("/tree/a"), SourceReject::DestInsideSource)]
        );
        let err = ensure_guarded(&guard, &abs("/tree/a/b"))
            .expect_err("an all-refused selection is an error");
        assert_eq!(err.kind(), "dest_inside_source");
        assert_eq!(err.status_code(), 400);
        assert!(err.is_expected());
        assert_eq!(
            err.to_string(),
            "cannot place /tree/a/b/a inside its own source /tree/a",
            "the message names the target the guard computed"
        );
    }

    #[test]
    fn sibling_prefix_is_allowed() {
        // The `isSubdir` string-prefix trap: `/tree/ab` is not inside `/tree/a`.
        let guard = guard_sources(&[abs("/tree/a")], &abs("/tree/ab")).expect("guard");
        assert_eq!(
            guard.allowed,
            vec![AllowedSource {
                source: abs("/tree/a"),
                target: abs("/tree/ab/a"),
            }]
        );
        assert!(guard.rejected.is_empty());
        assert_eq!(guard.targets(), vec![abs("/tree/ab/a")]);
        assert!(ensure_guarded(&guard, &abs("/tree/ab")).is_ok());
    }

    #[test]
    fn paste_onto_itself_is_same_path() {
        // `cp /tree/a /tree` — `dst = Join(dest, base(src)) == src`, Go's `src == dst` half.
        let guard = guard_sources(&[abs("/tree/a")], &abs("/tree")).expect("guard");
        assert_eq!(
            guard.rejected,
            vec![(abs("/tree/a"), SourceReject::SamePath)]
        );
        assert!(guard.allowed.is_empty());
    }

    #[test]
    fn guard_keeps_the_order_and_mixes_both_lists() {
        let guard = guard_sources(
            &[abs("/tree/a"), abs("/tree/b"), abs("/tree/a/keep")],
            &abs("/tree/a"),
        )
        .expect("guard");
        assert_eq!(guard.targets(), vec![abs("/tree/a/b")]);
        assert_eq!(
            guard.rejected.iter().map(|r| &r.1).collect::<Vec<_>>(),
            vec![&SourceReject::DestInsideSource, &SourceReject::SamePath]
        );
        // A partial paste is allowed to proceed; the refusals stay reportable (plan §6.2's `skipped`).
        assert!(ensure_guarded(&guard, &abs("/tree/a")).is_ok());
    }

    #[test]
    fn empty_source_list_is_rejected() {
        let err =
            guard_sources(&[], &abs("/tree")).expect_err("Go: \"no source folder specified\"");
        assert_eq!(err.kind(), "empty");
        assert_eq!(err.status_code(), 400);
        assert!(err.is_expected());
        // A source with no name has no destination to compute (`api/ops.go:306` would have joined `""`).
        let rootless = guard_sources(&[abs("/")], &abs("/tree")).expect_err("`/` has no base name");
        assert_eq!(rootless.kind(), "invalid_path");
    }

    #[test]
    fn archive_source_is_vfs_readonly() {
        // `api/ops.go:73` — the sentinel, not the prose, is the contract.
        let host = abs("/tree/fake.zip");
        let inner = FsPath::archive(host.clone(), None, "sub").expect("an archive path");
        let plain = FsPath::fs("tree/plain").expect("a filesystem path");
        let err =
            vfs_write_guard(&[&plain, &inner]).expect_err("writing into an archive is refused");
        assert_eq!(err.kind(), "vfs_readonly");
        assert_eq!(err.status_code(), 405);
        assert!(!err.is_expected());
        assert!(vfs_write_guard(&[&plain]).is_ok());
        assert!(
            vfs_write_guard(&[]).is_ok(),
            "nothing to guard is not a failure"
        );

        // The parsed destination carries the same rule; the raw text carries the `..` one.
        let dest_err = dest_guard(&inner).expect_err("extract into an archive");
        assert_eq!(dest_err.kind(), "vfs_readonly");
        assert!(dest_guard(&plain).is_ok());
        assert!(dest_guard(&FsPath::fs("a/b..c").expect("a legal name")).is_ok());
        assert!(
            dest_guard(&FsPath {
                store: VfsKind::Fs,
                relative: "a/../../etc/passwd".to_string(),
            })
            .is_err(),
            "a hand-built `..` must not slip through the parsed form either"
        );
    }

    #[test]
    fn dest_text_guard_is_component_wise() {
        // Plan §5.3's done-when row: `foo..bar` allowed, `/tmp/../etc/passwd` rejected.
        assert!(dest_text_guard("foo..bar").is_ok());
        assert!(dest_text_guard("a..b.txt").is_ok());
        assert!(dest_text_guard("/tmp/etc/passwd").is_ok());
        let err = dest_text_guard("/tmp/../etc/passwd").expect_err("a `..` component escapes");
        assert_eq!(err.kind(), "invalid_path");
        assert_eq!(dest_text_guard("a/..").unwrap_err().kind(), "invalid_path");
    }

    #[test]
    fn dest_guard_rejects_a_glob_destination() {
        let glob = FsPath::fs("out/*.txt").expect("a parseable relative path");
        let err = dest_guard(&glob).expect_err("a glob is never a literal destination");
        assert_eq!(err.kind(), "invalid_path");
    }

    #[test]
    fn ensure_writable_uses_the_platform_answer() {
        let path = abs("/tree/a");
        assert!(ensure_writable(&Writable { answer: true }, &path).is_ok());
        let denied = ensure_writable(&Writable { answer: false }, &path)
            .expect_err("a refusal is a permission error the UI can show");
        assert_eq!(denied.kind(), "permission_denied");
        assert_eq!(
            denied.status_code(),
            500,
            "Go measured 500 for a denied directory"
        );
        // A platform that cannot answer defers to the write itself (`stub::Unsupported`).
        assert!(
            ensure_writable(&crate::platform::stub::Unsupported::new(), &path).is_ok(),
            "an unknown answer must not become a false rejection"
        );
    }
}

// --------------------------------------------------------------------------- zip-slip

/// Resolve one archive-internal path *under* `dest`, or refuse it.
///
/// The plan (F9) names this function as the guard against zip-slip, and the two things that make it a
/// guard rather than a string test are:
///
/// 1. the containment check runs on the **resolved** path — an [`AbsPath`] cannot hold `.` or `..`, so
///    `is_ancestor_of` is a component-wise compare of normalised forms, not the prefix test Go used
///    (`api/archive_extract.go:68-78`, which also flagged the legal name `a..b.txt`:
///    `tests/golden/archive-extract-dotdot-falsepositive.json`); and
/// 2. every **existing intermediate** is `symlink_metadata`-checked, so an earlier member that was a
///    symlink (or a pre-existing link in the destination) cannot be walked through by a later one.
///
/// A literal `..` component is refused before resolution: it has no meaning inside a store, and refusing
/// it is what makes "extracts to nothing" true for a hostile container.
///
/// # Errors
/// [`CoreError::InvalidPath`] with [`PathReject::EscapesRoot`] for a member that escapes `dest` or resolves
/// through a symlink; whatever [`AbsPath::join`] reports for an illegal name.
pub fn safe_join(dest: &AbsPath, member: &str) -> Result<AbsPath, CoreError> {
    let raw = member.to_string();
    let mut target = dest.clone();
    let mut intermediates: Vec<AbsPath> = Vec::new();
    for comp in member.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." {
            return Err(CoreError::InvalidPath {
                raw,
                reason: PathReject::EscapesRoot,
            });
        }
        target = target.join(comp)?;
        intermediates.push(target.clone());
    }
    // The belt to the type's braces: prove containment after normalisation.
    if !dest.is_ancestor_of(&target) || dest == &target {
        return Err(CoreError::InvalidPath {
            raw,
            reason: PathReject::EscapesRoot,
        });
    }
    for path in intermediates
        .iter()
        .take(intermediates.len().saturating_sub(1))
    {
        if let Ok(md) = std::fs::symlink_metadata(path.as_path())
            && md.file_type().is_symlink()
        {
            return Err(CoreError::InvalidPath {
                raw,
                reason: PathReject::EscapesRoot,
            });
        }
    }
    Ok(target)
}

#[cfg(test)]
mod safe_join_tests {
    use super::*;

    #[test]
    fn traversal_is_refused_and_legal_names_are_not() {
        let dest = AbsPath::from_path("/tmp/dest").expect("abs");
        for bad in ["../../evil", "a/../../evil", "../evil", "a/../../b/../../c"] {
            let e = safe_join(&dest, bad).expect_err("must refuse");
            assert_eq!(e.kind(), "invalid_path", "{bad}");
        }
        for ok in ["a..b.txt", "deep/nested/x.txt", "..name", "name.."] {
            let t = safe_join(&dest, ok).expect("legal");
            assert!(dest.is_ancestor_of(&t), "{ok} -> {t}");
        }
        // The member may not *be* the destination either (that would clobber the directory itself).
        assert!(safe_join(&dest, "").is_err());
    }

    /// Symlinks are the subject, so this is a unix test by nature: `#[cfg(unix)]` keeps a
    /// `--all-targets` cross check for Windows (step 16a) from tripping over `std::os::unix` here.
    #[cfg(unix)]
    #[test]
    fn an_existing_symlink_intermediate_is_refused() {
        let base = crate::scratch::create("safe-join");
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).expect("outside");
        let dest = base.join("dest");
        std::fs::create_dir_all(&dest).expect("dest");
        let abs_dest = AbsPath::from_path(&dest).expect("abs dest");
        let link = dest.join("link");
        std::os::unix::fs::symlink(&outside, &link).expect("symlink");
        let e = safe_join(&abs_dest, "link/escape.txt").expect_err("must not follow the link");
        assert_eq!(e.kind(), "invalid_path");
        let _ = crate::scratch::remove_tree(&base);
    }
}
