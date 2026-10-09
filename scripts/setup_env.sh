#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
if command -v cygpath >/dev/null 2>&1; then ROOT="$(cygpath -m "$ROOT")"; fi
try_python() {
    local python="${1//\\//}"
    shift
    case "$python" in */[Ww][Ii][Nn][Dd][Oo][Ww][Ss][Aa][Pp][Pp][Ss]/*) return 1 ;; esac
    [[ -f "$python" && -x "$python" ]] || return 1
    "$python" -I -B -c 'import sys; sys.exit(sys.version_info < (3, 11))' >/dev/null 2>&1 || return 1
    exec "$python" -B "$ROOT/scripts/setup_env.py" "$@"
}
if [[ ${HERDR_SETUP_PYTHON+x} ]]; then
    try_python "$HERDR_SETUP_PYTHON" "$@" || {
        printf '%s\n' '[setup-env] HERDR_SETUP_PYTHON must name an existing native Python >=3.11, not a WindowsApps alias.' >&2
        exit 1
    }
fi
for name in python3 python; do
    python="$(type -P "$name" || true)"
    try_python "$python" "$@" || true
done
local_roots=("${LOCALAPPDATA:-}")
if [[ -n "${USERPROFILE:-}" ]]; then local_roots+=("$USERPROFILE/AppData/Local"); fi
for local_root in "${local_roots[@]}"; do
    [[ -n "$local_root" ]] || continue
    local_root="${local_root//\\//}"
    for python in "$local_root"/Python/pythoncore-*/python.exe "$local_root"/Programs/Python/Python*/python.exe; do
        try_python "$python" "$@" || true
    done
done
printf '%s\n' '[setup-env] Existing native Python >=3.11 is required; WindowsApps aliases were not invoked and no installation was attempted.' >&2
exit 1
