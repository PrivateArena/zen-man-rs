//! Places, favourites and disks — the data behind the left drawer (plan §5.3 `services/places.rs`,
//! §5.8 `panels/side_panel.rs`; defects D8/D9 of §9.2 step-15a).
//!
//! # Where the list comes from (read out of the Go sources, not invented)
//!
//! * **Places** — `frontend/js/sidebar.js:9-15`: a *hardcoded* frontend list, `Home (~)`, `Root (/)`,
//!   `~/Downloads`, `~/Documents`, `~/Desktop`. Go never read `~/.config/user-dirs.dirs`, so there is
//!   no XDG-user-dirs port to make — that hypothesis is retired here.
//! * **Bookmarks** (this port's *favourites*) — `api/places.go:120-197`: `config.json`
//!   `bookmarks: ["/path", …]`, POST `{"action":"add"|"remove","path":…}`, deduplicated on add,
//!   display label = the last path component (`sidebar.js:66`).
//! * **Mounted Drives** — `api/places.go:236-291`: the directories under `/media` and
//!   `/run/media/<user>`. Go scanned those two trees by hand; the port asks the platform for its
//!   mount table (`Env::mount_entries`) and keeps Go's *visibility* rule, which is the honest
//!   superset (it also covers a mount systemd put somewhere else).
//!
//! `services/` does not discover paths (`services/mod.rs`), so the home directory arrives as an
//! argument: `platform::home_dir` answers it, `Env` answers the mounts and the free space, and this
//! module only folds them into rows.

use std::path::Path;

use crate::error::CoreError;
use crate::path::AbsPath;
use crate::platform::Env;
use crate::store::config::Bookmark;

/// Go's fixed Places list below `$HOME` (`frontend/js/sidebar.js:12-14`), in Go's order.
pub const GO_USER_DIRS: [&str; 3] = ["Downloads", "Documents", "Desktop"];

/// The two roots Go scans for mounted drives (`api/places.go:245,268`).
pub const GO_MOUNT_ROOTS: [&str; 2] = ["/media", "/run/media"];

/// The root filesystem. Go shows it in its header meter; the drawer shows it as a disk row so the
/// panel always has at least one honest entry (defect D9).
pub const ROOT_MOUNT: &str = "/";

/// Which drawer section a row belongs to (which decides its icon).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaceKind {
    /// `$HOME` (Go's `~`).
    Home,
    /// The filesystem root (Go's `/`).
    Root,
    /// One of Go's three user directories.
    UserDir,
    /// A favourite (Go's bookmark).
    Favourite,
    /// A mounted drive.
    Mount,
}

/// One navigable row of the left drawer.
///
/// `unavailable` is the *typed* state of a place this process cannot reach (defect D8: the row is
/// never silently dead — it says why).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    /// What the row prints (Go: the last path component).
    pub label: String,
    /// The absolute path the row navigates to.
    pub path: String,
    /// Which section this row belongs to.
    pub kind: PlaceKind,
    /// `true` when the path exists and is a directory.
    pub exists: bool,
    /// Why it does not, when it does not.
    pub unavailable: Option<String>,
}

impl Place {
    /// A row probed against the real filesystem.
    #[must_use]
    pub fn probe(label: impl Into<String>, path: impl Into<String>, kind: PlaceKind) -> Self {
        let label = label.into();
        let path = path.into();
        let (exists, unavailable) = match std::fs::metadata(&path) {
            Ok(meta) => (meta.is_dir(), None),
            Err(e) => (false, Some(e.to_string())),
        };
        Self {
            label,
            path,
            kind,
            exists,
            unavailable,
        }
    }

    /// A row that cannot be probed at all (no home directory to resolve it against).
    #[must_use]
    pub fn missing(
        label: impl Into<String>,
        path: impl Into<String>,
        kind: PlaceKind,
        why: impl Into<String>,
    ) -> Self {
        Self {
            label: label.into(),
            path: path.into(),
            kind,
            exists: false,
            unavailable: Some(why.into()),
        }
    }
}

/// One mounted filesystem, with the free space the row prints (plan §5.8's disk meter, in text).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disk {
    /// The volume name (Go: the directory name under `/media`).
    pub label: String,
    /// Where it is mounted.
    pub mount_point: String,
    /// Filesystem type (`ext4`, `vfat`, …; empty for the synthetic root row of a stub platform).
    pub fstype: String,
    /// Total bytes (`0` when `unavailable`).
    pub total: u64,
    /// Free bytes (`0` when `unavailable`).
    pub free: u64,
    /// Backed by removable media (`/sys/block/…/removable`).
    pub removable: bool,
    /// Why the platform could not answer.
    pub unavailable: Option<String>,
}

/// Everything the left drawer shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Places {
    /// Go's Places list: Home, Root, then the three user directories.
    pub user: Vec<Place>,
    /// The favourites (Go's bookmarks), in the order they were added.
    pub favourites: Vec<Place>,
    /// The mounted drives, root filesystem first.
    pub disks: Vec<Disk>,
}

impl Places {
    /// Fold the platform answers into rows. `home` is `platform::home_dir()`, `favourites` the
    /// persisted `config.json` bookmarks.
    #[must_use]
    pub fn build(env: &dyn Env, home: Option<&Path>, favourites: &[Bookmark]) -> Self {
        Self {
            user: user_places(home),
            favourites: favourite_places(favourites),
            disks: disks(env),
        }
    }
}

/// Go's Places list against `home` (`sidebar.js:9-15`), in Go's order.
#[must_use]
pub fn user_places(home: Option<&Path>) -> Vec<Place> {
    let mut rows = Vec::new();
    match home {
        Some(home) => {
            rows.push(Place::probe(
                "Home",
                home.display().to_string(),
                PlaceKind::Home,
            ));
            rows.push(Place::probe("Root", ROOT_MOUNT, PlaceKind::Root));
            for name in GO_USER_DIRS {
                rows.push(Place::probe(
                    name,
                    home.join(name).display().to_string(),
                    PlaceKind::UserDir,
                ));
            }
        }
        None => {
            rows.push(Place::missing(
                "Home",
                "",
                PlaceKind::Home,
                "this process has no home directory",
            ));
            rows.push(Place::probe("Root", ROOT_MOUNT, PlaceKind::Root));
        }
    }
    rows
}

/// The favourites as rows: Go's label rule (`sidebar.js:66`), with an explicit label winning when
/// the port has one.
#[must_use]
pub fn favourite_places(favourites: &[Bookmark]) -> Vec<Place> {
    favourites
        .iter()
        .map(|bookmark| {
            let label = bookmark
                .label
                .clone()
                .unwrap_or_else(|| bookmark_label(&bookmark.path));
            Place::probe(label, bookmark.path.clone(), PlaceKind::Favourite)
        })
        .collect()
}

/// Go's label for a bookmark: `b.split('/').pop() || b` (`sidebar.js:66`).
#[must_use]
pub fn bookmark_label(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
        .to_string()
}

/// Go's `normalize_bookmark` (plan §5.3): absolute, no trailing slash, no duplicate spelling. The
/// absoluteness check is core's own [`AbsPath`] rule, so a relative favourite is refused exactly the
/// way the address bar refuses one.
pub fn normalize(path: &str) -> Result<String, CoreError> {
    let trimmed = path.trim();
    AbsPath::from_path(trimmed).map_err(|_| CoreError::Config {
        msg: format!("favourite {path:?} is not an absolute path"),
    })?;
    let mut text = trimmed.to_string();
    while text.len() > 1 && text.ends_with('/') {
        text.pop();
    }
    Ok(text)
}

/// The mounts the drawer lists: Go's two scan roots, plus the root filesystem itself.
#[must_use]
pub fn visible_mount(mount_point: &str) -> bool {
    mount_point == ROOT_MOUNT
        || GO_MOUNT_ROOTS
            .iter()
            .any(|root| mount_point != *root && Path::new(mount_point).starts_with(root))
}

fn disks(env: &dyn Env) -> Vec<Disk> {
    let mut seen: Vec<String> = Vec::new();
    let mut rows = Vec::new();
    for entry in env
        .mount_entries()
        .map(|table| table.entries)
        .unwrap_or_default()
    {
        if !visible_mount(&entry.mount_point) || seen.contains(&entry.mount_point) {
            continue;
        }
        seen.push(entry.mount_point.clone());
        rows.push(probe_disk(
            env,
            &entry.mount_point,
            &entry.fstype,
            entry.removable,
        ));
    }
    if !seen.iter().any(|point| point == ROOT_MOUNT) {
        rows.push(probe_disk(env, ROOT_MOUNT, "", false));
    }
    rows
}

fn probe_disk(env: &dyn Env, mount_point: &str, fstype: &str, removable: bool) -> Disk {
    let (total, free, unavailable) = match AbsPath::from_path(mount_point) {
        Ok(path) => match env.disk_space(&path) {
            Ok(space) => (space.total, space.free, None),
            Err(e) => (0, 0, Some(e.user_message())),
        },
        Err(e) => (0, 0, Some(e.user_message())),
    };
    Disk {
        label: bookmark_label(mount_point),
        mount_point: mount_point.to_string(),
        fstype: fstype.to_string(),
        total,
        free,
        removable,
        unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A fresh, uniquely-named scratch tree (`zenman-core-places-<name>-<pid>-<seq>`); naming and
    /// stale-tree reaping come from `crate::scratch`.
    fn scratch(name: &str) -> std::path::PathBuf {
        crate::scratch::create(&format!("core-places-{name}"))
    }

    fn bookmark(path: &str) -> Bookmark {
        Bookmark {
            path: path.to_string(),
            label: None,
            added_at: None,
        }
    }

    #[test]
    fn user_places_follow_gos_sidebar_list_and_order() {
        let home = scratch("home");
        fs::create_dir_all(home.join("Desktop")).expect("Desktop exists");
        let rows = user_places(Some(home.as_path()));
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            ["Home", "Root", "Downloads", "Documents", "Desktop"],
            "Go's list, in Go's order (`sidebar.js:9-15`)"
        );
        assert!(
            rows[0].exists && rows[1].exists,
            "home and root are reachable here"
        );
        let desktop = rows
            .iter()
            .find(|r| r.label == "Desktop")
            .expect("Desktop is listed");
        assert!(desktop.exists, "the fixture made it");
        let downloads = rows
            .iter()
            .find(|r| r.label == "Downloads")
            .expect("Downloads is listed");
        assert!(!downloads.exists, "and did not make this one");
        assert_eq!(
            rows.iter().filter(|r| r.kind == PlaceKind::UserDir).count(),
            GO_USER_DIRS.len(),
            "exactly Go's three user directories, no invented ones"
        );
    }

    #[test]
    fn an_unreachable_place_is_a_typed_state_not_a_dead_row() {
        let dir = scratch("favourites");
        let gone = dir.join("no-such-place");
        let rows = favourite_places(&[bookmark(&gone.display().to_string())]);
        assert_eq!(rows.len(), 1, "the row is still there");
        assert!(!rows[0].exists, "and it knows it is not reachable");
        assert!(
            rows[0].unavailable.is_some(),
            "with a reason the drawer can print: {:?}",
            rows[0].unavailable
        );
    }

    #[test]
    fn favourite_paths_are_normalised_and_relative_ones_refused() {
        assert_eq!(normalize("/data/").expect("absolute"), "/data");
        assert_eq!(
            normalize("/").expect("root"),
            "/",
            "the root keeps its slash"
        );
        assert_eq!(
            normalize("  /a/b  ").expect("trimmed"),
            "/a/b",
            "surrounding space is not content"
        );
        assert!(
            matches!(normalize("relative/way"), Err(CoreError::Config { .. })),
            "a relative favourite is refused, not stored and then never found"
        );
    }

    #[test]
    fn only_gos_mount_roots_and_the_root_are_listed() {
        assert!(visible_mount("/"), "the root filesystem is always listed");
        assert!(
            visible_mount("/media/user/DATA"),
            "Go lists a volume under /media/<user>"
        );
        assert!(
            visible_mount("/run/media/user/USB"),
            "…and under /run/media/<user>"
        );
        assert!(!visible_mount("/media"), "not the scan root itself");
        assert!(
            !visible_mount("/proc/filesystems"),
            "and nothing outside Go's two roots"
        );
        assert_eq!(bookmark_label("/media/user/DATA"), "DATA", "Go's row name");
        assert_eq!(bookmark_label("/"), "/", "the root row is the root");
    }
}
