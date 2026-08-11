#!/bin/sh
# Jolt headless installer.
#
#   curl -fsSL https://jolt.trmcnvn.dev/install.sh | sh
#
# Installs the UI-free CLI/engine and desktop app under Jolt's XDG data root,
# puts `jolt` on PATH, adds a desktop launcher, and — once you've signed in —
# runs it as a systemd user service that survives reboots. Re-running upgrades
# in place preserves application state.
#
# The binary ships with production endpoints baked in: no JOLT_EDGE_URL or
# client-id configuration needed. Overrides (if any) go in the data root's
# `env` file.
set -eu

BASE="${JOLT_BASE_URL:-https://jolt.trmcnvn.dev}"

# --- platform ---------------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux) plat=linux ;;
  Darwin)
    echo "jolt install: on macOS, download the desktop app instead:" >&2
    echo "  $BASE/releases/latest.txt → $BASE/releases/jolt-<version>-macos-arm64.dmg" >&2
    exit 1
    ;;
  *)
    echo "jolt install: unsupported OS '$os' — only Linux for now." >&2
    exit 1
    ;;
esac
case "$arch" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *)
    echo "jolt install: unsupported architecture '$arch'." >&2
    exit 1
    ;;
esac

# --- download ----------------------------------------------------------------
ver="$(curl -fsSL "$BASE/releases/latest.txt" | tr -d '[:space:]')"
[ -n "$ver" ] || { echo "jolt install: could not resolve latest version" >&2; exit 1; }
file="jolt-$ver-$plat-$arch.tar.gz"
if [ -n "${JOLT_DATA_DIR:-}" ]; then
  data_root="$JOLT_DATA_DIR"
else
  case "${XDG_DATA_HOME:-}" in
    /*) platform_data_root="$XDG_DATA_HOME/jolt" ;;
    *) platform_data_root="$HOME/.local/share/jolt" ;;
  esac
  legacy_data_root="$HOME/.jolt"
  # Do not move a live legacy engine out from under itself. Install this release
  # there; its first lock-free startup performs the migration and leaves a
  # compatibility symlink for this installer and existing service units.
  if [ -e "$platform_data_root" ] && [ -d "$legacy_data_root" ] && [ ! -L "$legacy_data_root" ]; then
    echo "jolt install: both '$legacy_data_root' and '$platform_data_root' exist; move or remove one first." >&2
    exit 1
  elif [ ! -e "$platform_data_root" ] && [ -d "$legacy_data_root" ] && [ ! -L "$legacy_data_root" ]; then
    data_root="$legacy_data_root"
  else
    data_root="$platform_data_root"
  fi
fi
app_root="$data_root/app"
dest="$app_root/$ver"
tmp=""
desktop_tmp=""
trap '[ -z "$tmp" ] || rm -rf "$tmp"; [ -z "$desktop_tmp" ] || rm -f "$desktop_tmp"' EXIT

if [ -x "$dest/jolt" ]; then
  echo "jolt $ver already downloaded — relinking."
else
  tmp="$(mktemp -d)"
  echo "downloading jolt $ver ($plat-$arch)…"
  curl -fSL --progress-bar "$BASE/releases/$file" -o "$tmp/$file"
  mkdir -p "$dest"
  tar -xzf "$tmp/$file" -C "$dest" --strip-components=1
fi

ln -sfn "$dest" "$app_root/current"
mkdir -p "$HOME/.local/bin"
ln -sf "$app_root/current/jolt" "$HOME/.local/bin/jolt"

# --- desktop integration -----------------------------------------------------
case "${XDG_DATA_HOME:-}" in
  /*) data_home="$XDG_DATA_HOME" ;;
  *) data_home="$HOME/.local/share" ;;
esac
applications_dir="$data_home/applications"
icon_dir_512="$data_home/icons/hicolor/512x512/apps"
icon_dir_1024="$data_home/icons/hicolor/1024x1024/apps"
mkdir -p "$applications_dir" "$icon_dir_512" "$icon_dir_1024"
icon_512="$dest/jolt-512.png"
[ -f "$icon_512" ] || icon_512="$dest/jolt.png"
install -m 644 "$icon_512" "$icon_dir_512/jolt.png"
install -m 644 "$dest/jolt.png" "$icon_dir_1024/jolt.png"

# Desktop launchers do not reliably inherit ~/.local/bin in PATH. Point the
# entry at the managed `current` symlink so upgrades remain atomic.
desktop_tmp="$applications_dir/.jolt.desktop.$$"
JOLT_DESKTOP_EXEC="$app_root/current/jolt-desktop" awk '
function quote_exec(value, out, i, ch) {
  out = "\""
  for (i = 1; i <= length(value); i++) {
    ch = substr(value, i, 1)
    if (ch == "%") {
      out = out "%%"
      continue
    }
    if (ch == "\\" || ch == "\"" || ch == "`" || ch == "$") out = out "\\"
    out = out ch
  }
  return out "\""
}
/^Exec=/ { print "Exec=" quote_exec(ENVIRON["JOLT_DESKTOP_EXEC"]); next }
/^TryExec=/ { print "TryExec=" ENVIRON["JOLT_DESKTOP_EXEC"]; next }
{ print }
' "$dest/jolt.desktop" >"$desktop_tmp"
chmod 644 "$desktop_tmp"
mv -f "$desktop_tmp" "$applications_dir/jolt.desktop"
desktop_tmp=""
command -v update-desktop-database >/dev/null 2>&1 \
  && update-desktop-database "$applications_dir" || true

# --- service -----------------------------------------------------------------
# Auth is decoupled from the daemon: `jolt login` persists the session and a
# service-managed `jolt headless` loads it (exiting with "run jolt login
# first" otherwise) — so the service starts only after first sign-in.
signed_in=no
[ -f "$data_root/session.json" ] && signed_in=yes

service=manual
if command -v systemctl >/dev/null 2>&1 && [ -n "${XDG_RUNTIME_DIR:-}" ]; then
  mkdir -p "$HOME/.config/systemd/user"
  service_exec="$(printf '%s' "$app_root/current/jolt" | sed 's/\\/\\\\/g; s/"/\\"/g; s/%/%%/g')"
  service_env="$(printf '%s' "$data_root/env" | sed 's/\\/\\\\/g; s/"/\\"/g; s/%/%%/g')"
  service_data_environment=""
  if [ -n "${JOLT_DATA_DIR:-}" ]; then
    service_data_value="$(printf '%s' "$JOLT_DATA_DIR" | sed 's/\\/\\\\/g; s/"/\\"/g')"
    service_data_environment="Environment=\"JOLT_DATA_DIR=$service_data_value\""
  elif [ -n "${XDG_DATA_HOME:-}" ]; then
    service_data_value="$(printf '%s' "$XDG_DATA_HOME" | sed 's/\\/\\\\/g; s/"/\\"/g')"
    service_data_environment="Environment=\"XDG_DATA_HOME=$service_data_value\""
  fi
  cat >"$HOME/.config/systemd/user/jolt.service" <<UNIT
[Unit]
Description=Jolt headless engine
After=network-online.target
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
$service_data_environment
ExecStart="$service_exec" headless
Restart=on-failure
RestartSec=5
EnvironmentFile=-"$service_env"

[Install]
WantedBy=default.target
UNIT
  systemctl --user daemon-reload
  systemctl --user enable jolt >/dev/null 2>&1 || true
  if [ "$signed_in" = yes ]; then
    systemctl --user restart jolt
    service=running
  else
    service=ready
  fi
  # Keep the user manager (and the engine) running without an active login.
  loginctl enable-linger "$USER" 2>/dev/null \
    || sudo -n loginctl enable-linger "$USER" 2>/dev/null \
    || echo "warn: could not enable linger — the engine stops when you log out (run: sudo loginctl enable-linger $USER)"
else
  echo "warn: systemd user session not available — run the engine manually with: jolt headless"
fi

# --- agent CLIs ---------------------------------------------------------------
command -v claude >/dev/null 2>&1 || \
  echo "note: Claude Code CLI not found — install it with: curl -fsSL https://claude.ai/install.sh | bash"

case ":$PATH:" in
  *":$HOME/.local/bin:"*) path_hint="" ;;
  *) path_hint=' (add ~/.local/bin to your PATH)' ;;
esac

echo ""
echo "✓ jolt $ver installed$path_hint"
echo ""
case "$service" in
  running)
    echo "the engine restarted with the new version."
    echo "  systemctl --user status jolt    check the service"
    ;;
  ready)
    echo "next steps:"
    echo "  jolt login                               sign in (paste-code) and exit"
    echo "  systemctl --user start jolt              then start the engine"
    ;;
  manual)
    echo "next: \`jolt login\` to sign in, then run the engine with \`jolt headless\`."
    ;;
esac
