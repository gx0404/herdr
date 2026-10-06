#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
VENDORED_DIR=${VENDORED_GHOSTTY_DIR:-"$ROOT_DIR/vendor/libghostty-vt"}
OPTIMIZE=${LIBGHOSTTY_VT_OPTIMIZE:-ReleaseFast}

if [[ ! -f "$VENDORED_DIR/build.zig" ]]; then
  echo "error: vendored libghostty-vt source not found at $VENDORED_DIR" >&2
  exit 1
fi

cd "$VENDORED_DIR"
ZIG_GLOBAL_CACHE_DIR=${ZIG_GLOBAL_CACHE_DIR-"$ROOT_DIR/.local/zig-cache/global"} \
ZIG_LOCAL_CACHE_DIR=${ZIG_LOCAL_CACHE_DIR-"$ROOT_DIR/.local/zig-cache/local"} \
  zig build -Demit-lib-vt -Doptimize="$OPTIMIZE" "$@"

echo
printf 'built libghostty-vt in %s/zig-out\n' "$VENDORED_DIR"
