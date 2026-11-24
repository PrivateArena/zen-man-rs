# Attribution

Every non-code asset shipped in this repository, with its licence. `cargo xtask licenses` gates the
Rust dependency graph; **`cargo xtask assets`** — run by `cargo xtask ci` — gates *this* file, and it
is the gate that house rule 3 (an asset and its attribution land in the same commit) rests on:

* every file under `assets/` appears below by **literal path**, so a new icon with no row fails the
  gate — and a glob or a brace form (the way this file once listed its four chevron icons on one row)
  does not count as a row;
* no row may name a file that does not exist, so a rename cannot leave a phantom attribution behind;
* every row's `Licence` cell must be satisfiable from `deny.toml`'s `[licenses] allow` list.

## `assets/icons/` — the zen-man icon set

45 line icons, authored **originally for this project** (2026) by the zen-man-rs contributors, drawn
from scratch as simple geometric `viewBox="0 0 24 24"` stroke glyphs (`stroke="currentColor"`,
`stroke-width="2"`, the conventions `craft_ui::icons::SvgStyle` rewrites at load time). They are not
derived from any third-party icon set: no path data was copied from Feather/Lucide/Material or any
other catalogue.

Each is dual-licensed **MIT OR Apache-2.0**, the same dual licence as the workspace code
(`Cargo.toml [workspace.package] license`), so an icon may be used under either, with no attribution
required beyond this file.

| File | Licence | Glyph | Used for |
|---|---|---|---|
| `assets/icons/archive.svg` | MIT OR Apache-2.0 | box with strap | zip/7z/rar rows |
| `assets/icons/arrow-up.svg` | MIT OR Apache-2.0 | up arrow | breadcrumb "up one level" |
| `assets/icons/audio.svg` | MIT OR Apache-2.0 | note | audio rows |
| `assets/icons/chevron-down.svg` | MIT OR Apache-2.0 | down arrow | collapse, scroll affordances |
| `assets/icons/chevron-left.svg` | MIT OR Apache-2.0 | left arrow | collapse, history back |
| `assets/icons/chevron-right.svg` | MIT OR Apache-2.0 | right arrow | expand, history forward |
| `assets/icons/chevron-up.svg` | MIT OR Apache-2.0 | up arrow | collapse affordances |
| `assets/icons/close.svg` | MIT OR Apache-2.0 | cross | dialog and tab close |
| `assets/icons/code.svg` | MIT OR Apache-2.0 | braces | source file rows |
| `assets/icons/copy.svg` | MIT OR Apache-2.0 | two pages | copy verb |
| `assets/icons/cut.svg` | MIT OR Apache-2.0 | scissors | cut verb |
| `assets/icons/database.svg` | MIT OR Apache-2.0 | cylinders | database rows |
| `assets/icons/disk.svg` | MIT OR Apache-2.0 | disc | disk drawer rows |
| `assets/icons/drive.svg` | MIT OR Apache-2.0 | drive box | volume drawer rows |
| `assets/icons/eject.svg` | MIT OR Apache-2.0 | eject triangle | unmount action |
| `assets/icons/external-link.svg` | MIT OR Apache-2.0 | box with arrow | custom actions that leave the app |
| `assets/icons/file-text.svg` | MIT OR Apache-2.0 | page with lines | text file rows |
| `assets/icons/file.svg` | MIT OR Apache-2.0 | page | generic file rows (the icon set's fallback) |
| `assets/icons/folder-open.svg` | MIT OR Apache-2.0 | open folder | expanded directory rows |
| `assets/icons/folder.svg` | MIT OR Apache-2.0 | closed folder | directory rows, pane headers, bookmarks |
| `assets/icons/font.svg` | MIT OR Apache-2.0 | letter A | font file rows |
| `assets/icons/grid.svg` | MIT OR Apache-2.0 | four tiles | grid view mode |
| `assets/icons/history.svg` | MIT OR Apache-2.0 | clock with returning arrow | action-history toggle |
| `assets/icons/home.svg` | MIT OR Apache-2.0 | house | home breadcrumb |
| `assets/icons/image.svg` | MIT OR Apache-2.0 | picture | image rows |
| `assets/icons/info.svg` | MIT OR Apache-2.0 | circled i | about and props dialogs |
| `assets/icons/link.svg` | MIT OR Apache-2.0 | chain | symlink rows |
| `assets/icons/list.svg` | MIT OR Apache-2.0 | three lines | list view mode |
| `assets/icons/menu.svg` | MIT OR Apache-2.0 | three dots | pane overflow menu |
| `assets/icons/paste.svg` | MIT OR Apache-2.0 | clipboard | paste verb |
| `assets/icons/pencil.svg` | MIT OR Apache-2.0 | pencil | rename verb (registered as `edit`) |
| `assets/icons/plus.svg` | MIT OR Apache-2.0 | plus | new folder / new tab |
| `assets/icons/quad.svg` | MIT OR Apache-2.0 | four panes | quad-grid layout button |
| `assets/icons/refresh.svg` | MIT OR Apache-2.0 | circular arrow | refresh verb |
| `assets/icons/search.svg` | MIT OR Apache-2.0 | magnifier | header search, quick-find |
| `assets/icons/select-all.svg` | MIT OR Apache-2.0 | box with check | select-everything button |
| `assets/icons/select-none.svg` | MIT OR Apache-2.0 | box with cross | clear-selection button |
| `assets/icons/settings.svg` | MIT OR Apache-2.0 | gear | settings and custom-action roles |
| `assets/icons/split.svg` | MIT OR Apache-2.0 | split pane | two-pane layout button |
| `assets/icons/star.svg` | MIT OR Apache-2.0 | star | bookmark verb |
| `assets/icons/terminal.svg` | MIT OR Apache-2.0 | prompt | open-terminal custom action |
| `assets/icons/trash.svg` | MIT OR Apache-2.0 | waste bin | delete verb |
| `assets/icons/tree.svg` | MIT OR Apache-2.0 | indented lines | recursive-listing (flat) mode button |
| `assets/icons/video.svg` | MIT OR Apache-2.0 | camera | video rows |
| `assets/icons/workspace.svg` | MIT OR Apache-2.0 | stacked layers | workspace switcher button |

## Fonts

None are vendored (plan §10 decision R9): the shell installs the `craft_ui::fonts::UiFonts` stack
built on egui's own faces plus the system-font fallback `eframe`'s `system_fonts` feature installs, so
there is nothing here to licence. If `Outfit` (OFL-1.1, already on the `deny.toml` allowlist) is ever
bundled, it lands in `storytold/craft-fonts` first and gets a section here — and a row of this table's
shape, because `cargo xtask assets` reads literal paths, not prose.
