#!/usr/bin/env bash
# Pack the release binary into a real macOS app and install it into
# /Applications, so the copy that gets launched is never the one sitting inside
# the project tree.
#
#   ./scripts/make_app_bundle.sh              # bundle, install, relaunch
#   ./scripts/make_app_bundle.sh --no-install # leave it in dist/ only
#   ./scripts/make_app_bundle.sh --no-launch  # install but do not run it
#
# Why install rather than run from `dist/`: an app launched from inside a working
# directory is an app whose identity moves with that directory, and a folder like
# Documents or Desktop costs a permission prompt every time it is launched from.
# /Applications is the place macOS expects an app to live.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release/artifeeds-app"
ICNS="$ROOT/assets/AppIcon.icns"
STAGE="$ROOT/dist/Artifeeds.app"
TARGET="/Applications/Artifeeds.app"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | cut -d'"' -f2)"
APP_NAME="Artifeeds"

INSTALL=1
LAUNCH=1
for arg in "$@"; do
  case "$arg" in
    --no-install) INSTALL=0 ;;
    --no-launch)  LAUNCH=0 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

if [[ ! -x "$BIN" ]]; then
  echo "missing $BIN — run: cargo build --release" >&2
  exit 1
fi
if [[ ! -f "$ICNS" ]]; then
  echo "generating icon…"
  python3 "$ROOT/scripts/make_icon.py" >/dev/null
fi

# ---- the bundle ------------------------------------------------------------
rm -rf "$STAGE"
mkdir -p "$STAGE/Contents/MacOS" "$STAGE/Contents/Resources"
cp "$BIN" "$STAGE/Contents/MacOS/$APP_NAME"
cp "$ICNS" "$STAGE/Contents/Resources/AppIcon.icns"

cat > "$STAGE/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>              <string>${APP_NAME}</string>
  <key>CFBundleDisplayName</key>       <string>${APP_NAME}</string>
  <key>CFBundleExecutable</key>        <string>${APP_NAME}</string>
  <key>CFBundleIdentifier</key>        <string>local.artifeeds</string>
  <key>CFBundleIconFile</key>          <string>AppIcon</string>
  <key>CFBundlePackageType</key>       <string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION:-0.1.0}</string>
  <key>CFBundleVersion</key>           <string>${VERSION:-0.1.0}</string>
  <key>LSMinimumSystemVersion</key>    <string>11.0</string>
  <key>NSHighResolutionCapable</key>   <true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key> <true/>
</dict>
</plist>
PLIST

codesign --force --deep --sign - "$STAGE" >/dev/null 2>&1 || true
echo "built   $STAGE"

# ---- install ---------------------------------------------------------------
if [[ $INSTALL -eq 1 ]]; then
  if [[ ! -w /Applications ]]; then
    echo "cannot write /Applications — the app is in dist/" >&2
    exit 0
  fi

  # A running copy keeps the old code until it restarts, and replacing a bundle
  # under a live process is a good way to leave it half-updated.
  if pgrep -f "$TARGET/Contents/MacOS/$APP_NAME" >/dev/null 2>&1; then
    osascript -e "quit app \"$APP_NAME\"" >/dev/null 2>&1 || true
    for _ in $(seq 1 20); do
      pgrep -f "$TARGET/Contents/MacOS/$APP_NAME" >/dev/null 2>&1 || break
      sleep 0.2
    done
    pkill -f "$TARGET/Contents/MacOS/$APP_NAME" >/dev/null 2>&1 || true
  fi

  rm -rf "$TARGET"
  # `ditto` rather than `cp -R`: it preserves the code signature, extended
  # attributes and symlinks exactly as built.
  ditto "$STAGE" "$TARGET"
  # Nothing here came from the network, but a quarantine flag inherited from
  # somewhere else is what produces "downloaded from the internet" prompts.
  xattr -dr com.apple.quarantine "$TARGET" 2>/dev/null || true
  codesign --force --deep --sign - "$TARGET" >/dev/null 2>&1 || true
  echo "install $TARGET  (${VERSION:-0.1.0})"

  if [[ $LAUNCH -eq 1 ]]; then
    open "$TARGET"
    echo "launch  $APP_NAME"
  fi
fi
