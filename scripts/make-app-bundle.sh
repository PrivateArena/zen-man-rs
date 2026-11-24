#!/usr/bin/env bash
# Build the macOS application bundle: the thing a .icns is only ever read from.
#
# Why a bundle and not just the binary: macOS looks the application icon up in
#   ZenMan.app/Contents/Resources/<CFBundleIconFile>
# and reads the name, the identifier and the version from Info.plist. Run from the bundle, zenman
# also gets a real application menu, a Dock tile with the right name, and a `ZenMan.app` that drags
# onto /Applications. Loose in ~/Downloads the icon is the generic app and the menu is the binary's
# file name — which is also why `zenman --install-desktop-entry` has no macOS counterpart.
#
# The runtime icon (`apps/zenman/src/app_icon.rs`) is still worth having on macOS: eframe feeds it to
# `setApplicationIconImage`, which is what changes the Dock tile on macOS 11+ when the bundle is
# absent. With the bundle present the two agree, because both come out of the same master art.
#
# Usage (on macOS):
#   scripts/make-app-bundle.sh                 # debug binary, target/darwin/ZenMan.app
#   scripts/make-app-bundle.sh --release       # release binary
#   BIN=target/release/zenman OUT=/tmp/BUNDLE scripts/make-app-bundle.sh
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
BUNDLE_ID="io.storytold.zenman"
APP_NAME="ZenMan"
OUT_DIR="${OUT:-$ROOT/target/dist}"
ICNS="$ROOT/assets/app-icon/zenman.icns"

usage() {
  sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

MODE=debug
for arg in "$@"; do
  case "$arg" in
    --release) MODE=release ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $arg (try --help)" >&2; exit 2 ;;
  esac
done

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "this script builds a .app bundle, so it runs on macOS; uname says $(uname -s)" >&2
  exit 1
fi

if [[ ! -f "$ICNS" ]]; then
  echo "$ICNS is missing: run scripts/make-app-icon.py first" >&2
  exit 1
fi

BIN="${BIN:-$ROOT/target/$MODE/zenman}"
if [[ ! -x "$BIN" ]]; then
  CARGO_ARGS=(-p zenman)
  [[ "$MODE" == release ]] && CARGO_ARGS+=(--release)
  echo "==> cargo build ${CARGO_ARGS[*]}"
  (cd "$ROOT" && cargo build "${CARGO_ARGS[@]}")
fi
[[ -x "$BIN" ]] || { echo "$BIN still does not exist" >&2; exit 1; }

VERSION=$(sed -n '/^\[workspace.package\]/,/^\[/p' "$ROOT/Cargo.toml" \
  | sed -n 's/^version = "\([^"]*\)".*/\1/p' | head -1)
[[ -n "$VERSION" ]] || VERSION=0

APP="$OUT_DIR/$APP_NAME.app"
APP_ID="$BUNDLE_ID"
if [[ -n "${ZENMAN_BUNDLE_ID:-}" ]]; then
  APP_ID="$ZENMAN_BUNDLE_ID"
fi

echo "==> $APP ($APP_ID, version $VERSION)"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/$APP_NAME"
# `PkgInfo` is the classic bundle marker; Finder wants it, and it costs eight bytes.
printf 'APPL????' > "$APP/Contents/PkgInfo"

# The icon file name is what `CFBundleIconFile` says, extension included or omitted — the spec
# accepts both; spelled with the extension so a renamed resource cannot quietly break the tile.
cp "$ICNS" "$APP/Contents/Resources/zenman.icns"
ICON_FILE=zenman.icns

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$APP_NAME</string>
  <key>CFBundleDisplayName</key><string>Zen-Man</string>
  <key>CFBundleExecutable</key><string>$APP_NAME</string>
  <key>CFBundleIdentifier</key><string>$APP_ID</string>
  <key>CFBundleIconFile</key><string>$ICON_FILE</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <!-- The document types are the macOS half of the "open a folder" contract that `--install-desktop-entry`
       writes on Linux: `zenman <path>` and drag-onto-icon are how a mac user opens a folder anyway. -->
  <key>CFBundleDocumentTypes</key>
  <array>
    <dict>
      <key>CFBundleTypeName</key><string>Folder</string>
      <key>CFBundleTypeRole</key><string>Viewer</string>
      <key>LSItemContentTypes</key>
      <array><string>public.folder</string></array>
    </dict>
  </array>
  <!-- Retina: without this the system renders the app into a backing store it then upscales, which
       blurs both the window and the Dock tile. -->
  <key>NSHighResolutionCapable</key><true/>
  <key>NSPrincipalClass</key><string>NSApplication</string>
</dict>
</plist>
PLIST

# A bundle whose icon changed is only re-read after Launch Services is told about it; `lsregister` is
# the supported way, and it is safe to run repeatedly.
REGISTER=/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister
if [[ -x "$REGISTER" ]]; then
  "$REGISTER" -f "$APP" || echo "lsregister failed; the icon may need a logout to appear" >&2
fi

echo "==> built $APP"
echo "    open it with:  open '$APP'"
echo "    the Finder icon comes from Contents/Resources/zenman.icns; the window and Dock tile are"
echo "    already correct without the bundle, from the PNG embedded in the binary."
