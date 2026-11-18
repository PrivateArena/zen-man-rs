//! The SVG icon registry (plan §5.7 `icons.rs` row).
//!
//! The bytes are the repository's own `assets/icons/*.svg` — authored for this project, MIT OR
//! Apache-2.0, recorded in `ATTRIBUTION.md` in the same commit as the files (house rule 3). They
//! reach egui through [`craft_ui::icons::SvgIconSet`], which rewrites `currentColor`/`stroke-width`
//! at load time; `apps/zenman` installs `egui_extras`' SVG loader before the first frame (it has
//! since step 1).

use std::sync::OnceLock;

use egui::{Color32, Image, ImageSource, Rect, Ui, Vec2};

use zenman_core::entry::FileEntry;

/// Every shipped icon, name → bytes. The names are the plan row's registry; a test asserts every
/// name used anywhere in code exists in the set (`set().contains`).
static ICONS: &[(&str, &[u8])] = &[
    (
        "archive",
        include_bytes!("../../../assets/icons/archive.svg"),
    ),
    (
        "arrow-up",
        include_bytes!("../../../assets/icons/arrow-up.svg"),
    ),
    ("audio", include_bytes!("../../../assets/icons/audio.svg")),
    (
        "chevron-down",
        include_bytes!("../../../assets/icons/chevron-down.svg"),
    ),
    (
        "chevron-left",
        include_bytes!("../../../assets/icons/chevron-left.svg"),
    ),
    (
        "chevron-right",
        include_bytes!("../../../assets/icons/chevron-right.svg"),
    ),
    (
        "chevron-up",
        include_bytes!("../../../assets/icons/chevron-up.svg"),
    ),
    ("close", include_bytes!("../../../assets/icons/close.svg")),
    ("code", include_bytes!("../../../assets/icons/code.svg")),
    ("copy", include_bytes!("../../../assets/icons/copy.svg")),
    ("cut", include_bytes!("../../../assets/icons/cut.svg")),
    (
        "database",
        include_bytes!("../../../assets/icons/database.svg"),
    ),
    ("disk", include_bytes!("../../../assets/icons/disk.svg")),
    (
        "document",
        include_bytes!("../../../assets/icons/file-text.svg"),
    ),
    ("drive", include_bytes!("../../../assets/icons/drive.svg")),
    ("edit", include_bytes!("../../../assets/icons/pencil.svg")),
    ("eject", include_bytes!("../../../assets/icons/eject.svg")),
    (
        "external-link",
        include_bytes!("../../../assets/icons/external-link.svg"),
    ),
    ("file", include_bytes!("../../../assets/icons/file.svg")),
    ("folder", include_bytes!("../../../assets/icons/folder.svg")),
    (
        "folder-open",
        include_bytes!("../../../assets/icons/folder-open.svg"),
    ),
    ("font", include_bytes!("../../../assets/icons/font.svg")),
    ("grid", include_bytes!("../../../assets/icons/grid.svg")),
    ("home", include_bytes!("../../../assets/icons/home.svg")),
    ("image", include_bytes!("../../../assets/icons/image.svg")),
    ("info", include_bytes!("../../../assets/icons/info.svg")),
    ("link", include_bytes!("../../../assets/icons/link.svg")),
    ("list", include_bytes!("../../../assets/icons/list.svg")),
    ("menu", include_bytes!("../../../assets/icons/menu.svg")),
    ("paste", include_bytes!("../../../assets/icons/paste.svg")),
    ("plus", include_bytes!("../../../assets/icons/plus.svg")),
    ("quad", include_bytes!("../../../assets/icons/quad.svg")),
    (
        "refresh",
        include_bytes!("../../../assets/icons/refresh.svg"),
    ),
    ("search", include_bytes!("../../../assets/icons/search.svg")),
    (
        "settings",
        include_bytes!("../../../assets/icons/settings.svg"),
    ),
    ("split", include_bytes!("../../../assets/icons/split.svg")),
    ("star", include_bytes!("../../../assets/icons/star.svg")),
    (
        "terminal",
        include_bytes!("../../../assets/icons/terminal.svg"),
    ),
    ("trash", include_bytes!("../../../assets/icons/trash.svg")),
    ("video", include_bytes!("../../../assets/icons/video.svg")),
];

/// Extension → glyph, the plan row's `ICON_BY_EXT`. Lower-case extension, no dot. The fallback for
/// an unknown extension is `"file"`; step 8's `entry_row` is the consumer, and the row tests live
/// there — this table is the single source.
pub const ICON_BY_EXT: &[(&str, &str)] = &[
    ("rs", "code"),
    ("py", "code"),
    ("js", "code"),
    ("ts", "code"),
    ("go", "code"),
    ("c", "code"),
    ("h", "code"),
    ("cpp", "code"),
    ("sh", "terminal"),
    ("bash", "terminal"),
    ("json", "code"),
    ("toml", "code"),
    ("ron", "code"),
    ("yaml", "code"),
    ("yml", "code"),
    ("md", "document"),
    ("txt", "document"),
    ("log", "document"),
    ("pdf", "document"),
    ("png", "image"),
    ("jpg", "image"),
    ("jpeg", "image"),
    ("gif", "image"),
    ("webp", "image"),
    ("svg", "image"),
    ("mp3", "audio"),
    ("flac", "audio"),
    ("wav", "audio"),
    ("ogg", "audio"),
    ("mp4", "video"),
    ("mkv", "video"),
    ("webm", "video"),
    ("zip", "archive"),
    ("7z", "archive"),
    ("rar", "archive"),
    ("tar", "archive"),
    ("gz", "archive"),
    ("xz", "archive"),
    ("ttf", "font"),
    ("otf", "font"),
    ("woff", "font"),
    ("woff2", "font"),
    ("db", "database"),
    ("sqlite", "database"),
];

/// The process-wide icon set (an immutable `Craft` catalog kept in a `OnceLock`, per
/// `craft_ui::icons`' own instructions).
#[must_use]
pub fn set() -> &'static craft_ui::icons::SvgIconSet {
    static SET: OnceLock<SvgIconSetHolder> = OnceLock::new();
    &SET.get_or_init(|| {
        SvgIconSetHolder(craft_ui::icons::SvgIconSet::new(
            ICONS,
            craft_ui::icons::SvgStyle {
                current_color: "#ffffff",
                stroke_width: Some("1.75"),
            },
            "bytes://zenman-icons/",
            "file",
        ))
    })
    .0
}

/// Newtype so the `OnceLock` can hold the set without leaking the constructor.
struct SvgIconSetHolder(craft_ui::icons::SvgIconSet);

/// The icon as an `egui::Image` with the set's shared bytes (no re-decode, `SvgIconSet::source`).
#[must_use = "an `Image` draws nothing until it is added to a `Ui`"]
pub fn image(name: &str, size: f32, tint: Color32) -> Image<'static> {
    let source: ImageSource<'static> = set().source(name);
    Image::new(source)
        .max_size(Vec2::splat(size))
        .tint(tint)
        .show_loading_spinner(false)
}

/// Paint an icon centred in `rect`, tinted `tint` — **paint-only** (plan §9.2 step 8: `ui.put`
/// allocates a rect, and the last-allocated overlapping rect wins hover/click in this egui fork,
/// so decoration over a row's interaction rect would steal its clicks). Interactivity comes from
/// the caller's own `ui.interact`/button, never from this call.
pub fn paint(ui: &mut Ui, rect: Rect, name: &str, size: f32, tint: Color32) {
    let side = size.min(rect.width()).min(rect.height());
    let centred = Rect::from_center_size(rect.center(), Vec2::splat(side));
    image(name, side, tint).paint_at(ui, centred);
}

/// The glyph for one directory row (plan row: this **replaces the `📁/📄/🔗` emoji scheme**).
///
/// Symlinks win over their target's kind: Go's row showed the link glyph because its `is_dir` is
/// the lstat answer, and the port keeps the row identical while `open` follows (plan §6.2 row 2).
#[must_use]
pub fn icon_for(entry: &FileEntry) -> &'static str {
    if entry.is_symlink {
        return "link";
    }
    if entry.is_dir {
        return "folder";
    }
    let ext = entry
        .name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    ICON_BY_EXT
        .iter()
        .find(|(needle, _)| *needle == ext.as_str())
        .map_or("file", |(_, icon)| *icon)
}

/// Every name this crate and the binary may pass to [`image`]/[`paint`]; the test below asserts
/// each is in the set (plan §5.7 `icons.rs` "Done when").
pub const KNOWN_ICONS: &[&str] = &[
    "archive",
    "arrow-up",
    "audio",
    "chevron-down",
    "chevron-left",
    "chevron-right",
    "chevron-up",
    "close",
    "code",
    "copy",
    "cut",
    "database",
    "disk",
    "document",
    "drive",
    "edit",
    "eject",
    "external-link",
    "file",
    "folder",
    "folder-open",
    "font",
    "grid",
    "home",
    "image",
    "info",
    "link",
    "list",
    "menu",
    "paste",
    "plus",
    "quad",
    "refresh",
    "search",
    "settings",
    "split",
    "star",
    "terminal",
    "trash",
    "video",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_known_icon_resolves() {
        let set = set();
        for name in KNOWN_ICONS {
            assert!(
                set.contains(name),
                "`{name}` is in KNOWN_ICONS but not the set"
            );
        }
    }

    #[test]
    fn the_fallback_exists() {
        assert!(set().contains("file"));
        // An unknown name still yields a source (the fallback's bytes under the requested URI).
        let src = set().source("does-not-exist");
        match src {
            ImageSource::Bytes { uri, bytes } => {
                assert!(uri.starts_with("bytes://zenman-icons/"));
                assert!(
                    matches!(bytes, egui::load::Bytes::Shared(_)),
                    "the fallback must carry shared bytes"
                );
            }
            _ => panic!("SvgIconSet must produce shared bytes"),
        }
    }

    #[test]
    fn icon_for_covers_dirs_links_and_extensions() {
        let dir = FileEntry {
            name: "photos".into(),
            is_dir: true,
            is_symlink: false,
            size: 0,
            mod_time: None,
            mode: String::new(),
            rel_path: String::new(),
            depth: 0,
            files_count: None,
        };
        assert_eq!(icon_for(&dir), "folder");
        let mut link = dir.clone();
        link.is_symlink = true;
        assert_eq!(icon_for(&link), "link");
        let mut archive = dir.clone();
        archive.is_dir = false;
        archive.name = "a.zip".into();
        assert_eq!(icon_for(&archive), "archive");
        let mut upper = archive.clone();
        upper.name = "A.ZIP".into();
        assert_eq!(
            icon_for(&upper),
            "archive",
            "extension match is case-folded"
        );
        let mut plain = archive.clone();
        plain.name = "README".into();
        assert_eq!(icon_for(&plain), "file");
        let mut dotted = archive.clone();
        dotted.name = ".dotfile".into();
        assert_eq!(icon_for(&dotted), "file");
    }

    #[test]
    fn every_extension_target_is_a_known_icon() {
        for (_, icon) in ICON_BY_EXT {
            assert!(
                set().contains(icon),
                "ICON_BY_EXT points at missing icon `{icon}`"
            );
        }
    }
}
