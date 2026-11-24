# The application icon

One mark, five delivery mechanisms, one generator. How `zenman`'s picture gets onto Linux, macOS and
Windows — and what each of them will and will not do for you.

```
assets/icons/prototype-icon.png        the artwork as drawn: 1024×579, flat white field
        │  scripts/make-app-icon.py    matte the field → crop → square canvas → size ladder
        ▼
assets/app-icon/zenman-master.png      1024² RGBA canonical art (committed, like everything below)
   ├── zenman.png              256²     embedded by `include_bytes!` → window · taskbar · Dock
   ├── zenman.ico            16…256     Windows resource table → Explorer · shortcuts · pinned tiles
   ├── zenman.icns          16…1024     macOS bundle resource  → Finder · Launchpad · ⌘-Tab
   └── linux/<s>x<s>.png     16…256     hicolor icon theme     → GNOME/KDE app grid
```

## What is shown where

| | window + taskbar / Dock | the file on disk |
|---|---|---|
| **Windows** | `WM_SETICON` from the embedded PNG (`ICON_BIG` + `ICON_SMALL`): title bar, taskbar button, alt-tab | the `.ico` **in the `.exe`'s resource table** (compiled by `apps/zenman/build.rs`) |
| **macOS** | `setApplicationIconImage` from the same bytes: the Dock tile and app menu, bundle or not | `CFBundleIconFile` in `ZenMan.app`, built by `scripts/make-app-bundle.sh` |
| **Linux / X11** | `_NET_WM_ICON`, set by winit from the same PNG — **no desktop file needed** | `Icon=` of a `.desktop` file, resolved through the icon theme |
| **Linux / Wayland** | nothing exists to ask: `xdg-shell` has no icon request | the compositor matches the `app_id` against `~/.local/share/applications/ZenMan.desktop`, then `Icon=` in `hicolor` |

That middle column is why the icon is right on a bare `cargo run` — X11 carries it with the window.
Wayland is the one case that cannot be answered from inside a process, so it is an opt-in:

```sh
zenman --install-desktop-entry     # writes the entry + the hicolor PNGs to $XDG_DATA_HOME, then exits
```

It writes only into `$XDG_DATA_HOME`, is idempotent, and leaves any `.desktop` file it did not
generate alone. The entry it writes is also the file-manager registration (`MimeType=inode/directory`),
whose other half — accepting a `file://` URI as an argument — the CLI already had.

The two names in the entry are not free choices, and both were read off a live X11 window:
`app_id` is `ZenMan`, which is what the Wayland compositor looks up in `applications/`; and
egui-winit passes the same id to winit's `with_name(app_id, "")`, so the X11 window answers to
`WM_CLASS = ("", "ZenMan")` and the entry says `StartupWMClass=ZenMan`. (`xprop -id <win> _NET_WM_ICON`
prints `(not shown)` for the pixel data — pass `-len 4000000` to see `Icon (256 x 256)`.)

## Rebuilding the art

```sh
python3 scripts/make-app-icon.py                 # rewrite every artefact from the prototype
python3 scripts/make-app-icon.py --preview /tmp  # … and a size ladder on light and dark chrome
python3 scripts/make-app-icon.py --check         # what CI runs (or: cargo xtask app-icon)
```

The master, the embedded PNG, the `.ico`, the `.icns` and the hicolor files are all **derived** and all
committed; `cargo xtask app-icon` (in `cargo xtask ci`) re-derives them and fails if any of them has
fallen behind the artwork, if one is not a valid container at the size its name claims, or if the code
stopped embedding one of them. `cargo xtask assets` is the separate, existing question: is it licensed.

Full mechanics — why the artwork's own drop shadow is deleted rather than matted, why the build
dependency is not target-gated, what a `*-windows-gnu` host needs — are in the module docs beside the
code they describe: `apps/zenman/src/app_icon.rs`, `apps/zenman/src/desktop_entry.rs`,
`apps/zenman/build.rs`, `xtask/src/app_icon.rs`, and the header of `scripts/make-app-icon.py`.
