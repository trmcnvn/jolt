#!/usr/bin/env bash
# Linux packaging: build the release binaries and produce
#   target/package/jolt-<version>-linux-<arch>.tar.gz
# containing the CLI/engine, desktop app, .desktop entry, icons, and install.sh
# that drops them into ~/.local (XDG) paths.
#
# Usage: scripts/package-linux.sh
# Env:   PROFILE=debug for a fast unoptimized package (CI smoke); default release.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
PROFILE="${PROFILE:-release}"
ARCH="$(uname -m)"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
OUT_DIR="$ROOT/target/package"
STAGE="$OUT_DIR/jolt-$VERSION-linux-$ARCH"
TARBALL="$STAGE.tar.gz"

# Cached CI target directories may contain artifacts from an earlier release.
rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"

rm -rf "$STAGE" "$TARBALL"
mkdir -p "$STAGE"

cd "$ROOT"
if [[ "$PROFILE" == "release" ]]; then
  cargo build --release -p jolt --bin Jolt
  install -m 755 "$ROOT/target/release/Jolt" "$STAGE/jolt-desktop"
  cargo build --release -p jolt --no-default-features --features cli --bin jolt-cli
  install -m 755 "$ROOT/target/release/jolt-cli" "$STAGE/jolt"
else
  cargo build -p jolt --bin Jolt
  install -m 755 "$ROOT/target/debug/Jolt" "$STAGE/jolt-desktop"
  cargo build -p jolt --no-default-features --features cli --bin jolt-cli
  install -m 755 "$ROOT/target/debug/jolt-cli" "$STAGE/jolt"
fi
install -m 644 "$ROOT/dist/jolt.desktop" "$STAGE/jolt.desktop"
install -m 644 "$ROOT/dist/jolt-512.png" "$STAGE/jolt-512.png"
install -m 644 "$ROOT/dist/jolt.png" "$STAGE/jolt.png"

cat >"$STAGE/install.sh" <<'INSTALL'
#!/usr/bin/env bash
# Install Jolt into ~/.local (no root needed).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
install -Dm755 "$HERE/jolt" "$HOME/.local/bin/jolt"
install -Dm755 "$HERE/jolt-desktop" "$HOME/.local/bin/jolt-desktop"
install -Dm644 "$HERE/jolt.desktop" "$HOME/.local/share/applications/jolt.desktop"
install -Dm644 "$HERE/jolt-512.png" "$HOME/.local/share/icons/hicolor/512x512/apps/jolt.png"
install -Dm644 "$HERE/jolt.png" "$HOME/.local/share/icons/hicolor/1024x1024/apps/jolt.png"
command -v update-desktop-database >/dev/null 2>&1 \
  && update-desktop-database "$HOME/.local/share/applications" || true
echo "Installed. Make sure ~/.local/bin is on your PATH."
INSTALL
chmod 755 "$STAGE/install.sh"

tar -czf "$TARBALL" -C "$OUT_DIR" "$(basename "$STAGE")"
rm -rf "$STAGE"
echo "packaged: $TARBALL"
tar -tzf "$TARBALL"
