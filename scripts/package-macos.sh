#!/usr/bin/env bash
# macOS packaging: build the release binary for the host arch and produce
#   target/package/jolt-<version>-macos-<arch>.dmg          (user download)
#   target/package/jolt-<version>-macos-<arch>-app.tar.gz   (auto-updater)
# containing Jolt.app (unsigned unless CODESIGN_IDENTITY is set).
#
# Usage: scripts/package-macos.sh
# Env:   SKIP_BUILD=1 to package an existing target/release/Jolt binary.
#        CODESIGN_IDENTITY="Developer ID Application: …" to sign the bundle.
#        NOTARYTOOL_KEY_PATH, NOTARYTOOL_KEY_ID, and NOTARYTOOL_ISSUER_ID
#        to notarize and staple both the app and dmg.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
ARCH="$(uname -m)" # arm64 on Apple silicon runners
OUT_DIR="$ROOT/target/package"
APP="$OUT_DIR/Jolt.app"
DMG="$OUT_DIR/jolt-$VERSION-macos-$ARCH.dmg"
APP_TARBALL="$OUT_DIR/jolt-$VERSION-macos-$ARCH-app.tar.gz"
NOTARY_ARCHIVE="$OUT_DIR/Jolt-notary.zip"

NOTARIZE=false
if [[ -n "${NOTARYTOOL_KEY_PATH:-}" \
  || -n "${NOTARYTOOL_KEY_ID:-}" \
  || -n "${NOTARYTOOL_ISSUER_ID:-}" ]]; then
  if [[ -z "${CODESIGN_IDENTITY:-}" \
    || -z "${NOTARYTOOL_KEY_PATH:-}" \
    || -z "${NOTARYTOOL_KEY_ID:-}" \
    || -z "${NOTARYTOOL_ISSUER_ID:-}" ]]; then
    echo "notarization requires CODESIGN_IDENTITY and all NOTARYTOOL_* variables" >&2
    exit 1
  fi
  NOTARIZE=true
fi

notarize() {
  xcrun notarytool submit "$1" \
    --key "$NOTARYTOOL_KEY_PATH" \
    --key-id "$NOTARYTOOL_KEY_ID" \
    --issuer "$NOTARYTOOL_ISSUER_ID" \
    --wait
}

# Cached CI target directories may contain artifacts from an earlier release.
rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"

cd "$ROOT"
if [[ "${SKIP_BUILD:-}" != 1 ]]; then
  cargo build --release -p jolt
fi
if [[ ! -x "$ROOT/target/release/Jolt" ]]; then
  echo "target/release/Jolt does not exist; run a release build first" >&2
  exit 1
fi

rm -rf "$APP" "$DMG" "$APP_TARBALL" "$NOTARY_ARCHIVE"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
install -m 755 "$ROOT/target/release/Jolt" "$APP/Contents/MacOS/Jolt"
sed "s/__VERSION__/$VERSION/" "$ROOT/dist/macos/Info.plist" >"$APP/Contents/Info.plist"

# Icon: iconset generated from the Jolt distribution artwork.
ICONSET="$OUT_DIR/jolt.iconset"
rm -rf "$ICONSET" && mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$ROOT/dist/jolt.png" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  retina=$((size * 2))
  sips -z "$retina" "$retina" "$ROOT/dist/jolt.png" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/jolt.icns"
rm -rf "$ICONSET"

if [[ -n "${CODESIGN_IDENTITY:-}" ]]; then
  codesign --deep --force --options runtime --timestamp \
    --sign "$CODESIGN_IDENTITY" "$APP"
  codesign --verify --deep --strict --verbose=2 "$APP"
else
  # Ad-hoc signature so the app launches on Apple silicon (Gatekeeper still
  # requires right-click → Open on first launch without notarization).
  codesign --deep --force --sign - "$APP"
fi

if [[ "$NOTARIZE" == true ]]; then
  ditto -c -k --sequesterRsrc --keepParent "$APP" "$NOTARY_ARCHIVE"
  notarize "$NOTARY_ARCHIVE"
  xcrun stapler staple "$APP"
  xcrun stapler validate "$APP"
  rm -f "$NOTARY_ARCHIVE"
fi

# The auto-updater artifact: the signed and, when configured, stapled bundle as
# a plain tarball. The in-app updater downloads + extracts it, then swaps
# /Applications/Jolt.app —
# crates/update stage_mac_app/apply_mac_app).
tar -czf "$APP_TARBALL" -C "$OUT_DIR" Jolt.app
echo "packaged: $APP_TARBALL"

hdiutil create -volname Jolt -srcfolder "$APP" -ov -format UDZO "$DMG"
if [[ -n "${CODESIGN_IDENTITY:-}" ]]; then
  codesign --force --timestamp --sign "$CODESIGN_IDENTITY" "$DMG"
  codesign --verify --verbose=2 "$DMG"
fi
if [[ "$NOTARIZE" == true ]]; then
  notarize "$DMG"
  xcrun stapler staple "$DMG"
  xcrun stapler validate "$DMG"
fi
echo "packaged: $DMG"
