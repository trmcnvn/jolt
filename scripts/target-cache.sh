#!/usr/bin/env bash
# Inspect Jolt's Rust build cache and explicitly reclaim rebuildable artifacts.
# Reporting and dry runs are the default; deletion requires --yes.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_DIR="$ROOT/target"
DEBUG_DIR="$TARGET_DIR/debug"
INCREMENTAL_DIR="$DEBUG_DIR/incremental"
TARGET_WARN_GIB="${JOLT_TARGET_WARN_GIB:-20}"
INCREMENTAL_WARN_GIB="${JOLT_INCREMENTAL_WARN_GIB:-10}"

usage() {
  cat <<'EOF'
Usage:
  scripts/target-cache.sh status
  scripts/target-cache.sh warn
  scripts/target-cache.sh clean-incremental [--yes]
  scripts/target-cache.sh clean-debug [--yes]

Commands:
  status             Show the approximate size of each target area.
  warn               Stay quiet unless target or incremental exceeds its limit.
  clean-incremental  Report the incremental cache; pass --yes to remove it.
  clean-debug        Report all dev/test artifacts; pass --yes to remove them.

Environment:
  JOLT_TARGET_WARN_GIB       Whole-target warning threshold (default: 20).
  JOLT_INCREMENTAL_WARN_GIB  Incremental warning threshold (default: 10).
EOF
}

die() {
  echo "error: $*" >&2
  exit 1
}

validate_threshold() {
  local name="$1" value="$2"
  [[ "$value" =~ ^[0-9]+$ ]] || die "$name must be a non-negative integer"
}

dir_kib() {
  local path="$1" output
  if [[ -d "$path" ]]; then
    output="$(du -sk "$path" 2>/dev/null || true)"
    awk 'NR == 1 { print $1 }' <<<"${output:-0}"
  else
    echo 0
  fi
}

human_kib() {
  awk -v kib="$1" 'BEGIN {
    split("KiB MiB GiB TiB", units, " ")
    size = kib
    unit = 1
    while (size >= 1024 && unit < 4) {
      size /= 1024
      unit++
    }
    if (unit == 1) printf "%.0f %s", size, units[unit]
    else printf "%.1f %s", size, units[unit]
  }'
}

status() {
  local target_kib debug_kib deps_kib incremental_kib dbg_kib release_kib
  target_kib="$(dir_kib "$TARGET_DIR")"
  debug_kib="$(dir_kib "$DEBUG_DIR")"
  deps_kib="$(dir_kib "$DEBUG_DIR/deps")"
  incremental_kib="$(dir_kib "$INCREMENTAL_DIR")"
  dbg_kib="$(dir_kib "$TARGET_DIR/dbg")"
  release_kib="$(dir_kib "$TARGET_DIR/release")"

  printf '%-21s %10s\n' "target" "$(human_kib "$target_kib")"
  printf '%-21s %10s\n' "  debug (dev/test)" "$(human_kib "$debug_kib")"
  printf '%-21s %10s\n' "    deps" "$(human_kib "$deps_kib")"
  printf '%-21s %10s\n' "    incremental" "$(human_kib "$incremental_kib")"
  printf '%-21s %10s\n' "  dbg (full symbols)" "$(human_kib "$dbg_kib")"
  printf '%-21s %10s\n' "  release" "$(human_kib "$release_kib")"
  echo
  echo "Sizes are approximate allocated bytes reported by du."
}

warn_if_large() {
  local target_kib incremental_kib target_limit_kib incremental_limit_kib warned
  target_kib="$(dir_kib "$TARGET_DIR")"
  incremental_kib="$(dir_kib "$INCREMENTAL_DIR")"
  target_limit_kib=$((TARGET_WARN_GIB * 1024 * 1024))
  incremental_limit_kib=$((INCREMENTAL_WARN_GIB * 1024 * 1024))
  warned=0

  if ((target_kib > target_limit_kib)); then
    echo "warning: Jolt's target directory is $(human_kib "$target_kib") (limit: ${TARGET_WARN_GIB} GiB)." >&2
    warned=1
  fi
  if ((incremental_kib > incremental_limit_kib)); then
    echo "warning: Jolt's incremental cache is $(human_kib "$incremental_kib") (limit: ${INCREMENTAL_WARN_GIB} GiB)." >&2
    warned=1
  fi
  if ((warned)); then
    echo "Run scripts/target-cache.sh status, then a clean command for a dry run." >&2
  fi
}

build_is_running() {
  command -v pgrep >/dev/null 2>&1 || return 1
  pgrep -x cargo >/dev/null 2>&1 || pgrep -x rustc >/dev/null 2>&1
}

confirm_cleanup() {
  local confirmed="$1" size_kib="$2" label="$3" path="$4"
  if ((size_kib == 0)); then
    echo "No $label to clean."
    return 1
  fi

  echo "$label: $(human_kib "$size_kib") at $path"
  if [[ "$confirmed" != "yes" ]]; then
    echo "Dry run only. Re-run with --yes to remove these rebuildable artifacts."
    return 1
  fi

  build_is_running && die "cargo or rustc is running; wait for it to finish before cleaning"
  return 0
}

clean_incremental() {
  local confirmed="$1" incremental_kib
  incremental_kib="$(dir_kib "$INCREMENTAL_DIR")"
  confirm_cleanup "$confirmed" "$incremental_kib" "Incremental cache" "$INCREMENTAL_DIR" || return 0
  [[ "$INCREMENTAL_DIR" == "$ROOT/target/debug/incremental" ]] \
    || die "refusing to clean an unexpected path: $INCREMENTAL_DIR"

  rm -rf -- "$INCREMENTAL_DIR"
  echo "Removed the incremental cache. Cargo will rebuild it as needed."
}

clean_debug() {
  local confirmed="$1" debug_kib
  debug_kib="$(dir_kib "$DEBUG_DIR")"
  confirm_cleanup "$confirmed" "$debug_kib" "Dev/test artifacts" "$DEBUG_DIR" || return 0
  [[ "$DEBUG_DIR" == "$ROOT/target/debug" ]] \
    || die "refusing to clean an unexpected path: $DEBUG_DIR"

  cargo clean --manifest-path "$ROOT/Cargo.toml" --profile dev --target-dir "$TARGET_DIR"
}

parse_clean_confirmation() {
  case "${2:-}" in
    "") echo no ;;
    --yes)
      [[ $# -eq 2 ]] || die "unexpected arguments"
      echo yes
      ;;
    *) die "unknown option: $2" ;;
  esac
}

validate_threshold JOLT_TARGET_WARN_GIB "$TARGET_WARN_GIB"
validate_threshold JOLT_INCREMENTAL_WARN_GIB "$INCREMENTAL_WARN_GIB"

case "${1:-status}" in
  status)
    [[ $# -le 1 ]] || die "status takes no arguments"
    status
    ;;
  warn)
    [[ $# -eq 1 ]] || die "warn takes no arguments"
    warn_if_large
    ;;
  clean-incremental)
    clean_incremental "$(parse_clean_confirmation "$@")"
    ;;
  clean-debug)
    clean_debug "$(parse_clean_confirmation "$@")"
    ;;
  -h|--help|help)
    usage
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac
