# Attribution

Every non-code asset shipped in this repository, with its licence. `cargo xtask licenses` gates the
Rust dependency graph; this file is the equivalent gate for assets (plan §5.1 house rule 3: an asset
and its attribution land in the **same commit**).

## `assets/icons/*.svg` — the zen-man icon set

40 line icons, authored **originally for this project** (2026) by the zen-man-rs contributors, drawn
from scratch as simple geometric `viewBox="0 0 24 24"` stroke glyphs (`stroke="currentColor"`,
`stroke-width="2"`, the conventions `craft_ui::icons::SvgStyle` rewrites at load time). They are not
derived from any third-party icon set: no path data was copied from Feather/Lucide/Material or any
other catalogue.

Licence: **MIT OR Apache-2.0**, the same dual licence as the workspace code (`Cargo.toml
[workspace.package] license`), so the icons can be used under either, with no attribution required
beyond this file.

| File | Glyph | Used for |
|---|---|---|
| `folder.svg`, `folder-open.svg` | closed / open folder | directory rows, pane headers |
| `file.svg`, `file-text.svg` | page / page with lines | generic and text file rows |
| `link.svg` | chain | symlink rows |
| `archive.svg` | box with strap | zip/7z/rar rows |
| `image.svg`, `audio.svg`, `video.svg` | picture / note / camera | media rows |
| `code.svg`, `font.svg`, `database.svg` | braces / A / cylinders | source, font, db rows |
| `search.svg` | magnifier | header search, quick-find |
| `home.svg`, `arrow-up.svg`, `eject.svg`, `disk.svg`, `drive.svg` | places | breadcrumb / drawer actions |
| `settings.svg`, `info.svg`, `star.svg`, `terminal.svg`, `external-link.svg` | gear / i / star / prompt / box-arrow | actions and custom-action roles |
| `grid.svg`, `list.svg`, `split.svg`, `quad.svg` | view modes | layout toolbar |
| `refresh.svg`, `plus.svg`, `pencil.svg`, `trash.svg`, `copy.svg`, `cut.svg`, `paste.svg`, `close.svg` | edit verbs | toolbar / context menus |
| `chevron-{left,right,up,down}.svg` | arrows | collapse, scroll affordances |
| `menu.svg` | three dots | pane overflow menu |

## Fonts

None are vendored (plan §10 decision R9): the shell installs egui's built-in fonts plus a
system-font fallback, so there is nothing here to licence. If `Outfit` (OFL-1.1, already on the
`deny.toml` allowlist) is ever bundled, it lands in `storytold/craft-fonts` first and gets a section
here.

## Third-party crates

Covered by `cargo xtask licenses` over `cargo metadata` with `deny.toml` as its data file; no crate
licence needs an asset-style entry here.
