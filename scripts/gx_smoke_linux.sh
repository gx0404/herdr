#!/usr/bin/env bash
set -euo pipefail

if [[ "${HERDR_GX_DISPOSABLE:-}" != 1 || "$EUID" != 0 ]] ||
   [[ ! -f /.dockerenv && ! -f /run/.containerenv ]]; then
  printf '%s\n' 'Refusing installation smoke: requires HERDR_GX_DISPOSABLE=1 and root inside a disposable container, never a workstation/WSL host.' >&2
  exit 2
fi
if [[ $# -lt 1 || $# -gt 2 ]]; then
  printf 'Usage: %s DEB [PREVIOUS_DEB]\n' "$0" >&2
  exit 2
fi
source /etc/os-release
if [[ "$ID" != ubuntu || ( "$VERSION_ID" != 22.04 && "$VERSION_ID" != 24.04 ) || "$(dpkg --print-architecture)" != amd64 ]]; then
  printf '%s\n' 'Smoke supports Ubuntu 22.04/24.04 amd64 only.' >&2
  exit 2
fi
for tool in apt-get dpkg dpkg-deb runuser useradd userdel python3 timeout sha256sum; do
  command -v "$tool" >/dev/null || { printf 'Missing required tool: %s\n' "$tool" >&2; exit 2; }
done
if [[ -e /usr/bin/herdr ]] || dpkg-query -W -f='${Status}' herdr-gx 2>/dev/null | grep -q ' installed$'; then
  printf '%s\n' 'Refusing to touch an existing Herdr installation.' >&2
  exit 2
fi

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
deb="$(realpath -- "$1")"
previous="${2:-}"
work="$(mktemp -d /var/tmp/herdr-gx-lifecycle.XXXXXXXX)"
user="gxsmoke$(id -u)${RANDOM}"
created_user=0
installed=0
conflict_installed=0
cleanup() {
  local result=$?
  trap - EXIT
  set +e
  if [[ "$installed" == 1 ]]; then timeout 120 apt-get remove --yes herdr-gx || result=1; fi
  if [[ "$conflict_installed" == 1 ]]; then timeout 120 dpkg --remove herdr-gx-smoke-conflict || result=1; fi
  if [[ "$created_user" == 1 ]]; then userdel "$user" || result=1; fi
  rm -rf -- "$work" || result=1
  exit "$result"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
chmod 755 "$work"
useradd --user-group --no-create-home --home-dir "$work/home" --shell /bin/sh "$user"
created_user=1
mkdir -p "$work/home" "$work/run" "$work/cwd"
chown -R "$user:$user" "$work/home" "$work/run" "$work/cwd"
cp -- "$script_dir/gx_smoke_runtime.py" "$work/runtime.py"

validate_manifest() {
  python3 - "$1" <<'PY'
import hashlib
import json
from pathlib import Path
import sys
path = Path(sys.argv[1])
m = json.loads(Path(str(path) + '.manifest.json').read_text())
assert m['schema_version'] == 1 and m['platform'] == 'linux'
assert m['package_manager'] == 'deb' and m['architecture'] == 'x86_64'
assert m['target'] == 'x86_64-unknown-linux-musl'
assert hashlib.sha256(path.read_bytes()).hexdigest() == m['artifact']['sha256']
print(m['version'])
PY
}
install_deb() {
  cp -- "$1" "$work/candidate.deb"
  chmod 644 "$work/candidate.deb"
  timeout 180 env DEBIAN_FRONTEND=noninteractive apt-get install --yes --reinstall "$work/candidate.deb"
  installed=1
}
assert_installed() {
  local source_deb="$1"
  python3 - "$source_deb" <<'PY'
import hashlib
import json
from pathlib import Path
import stat
import sys
m = json.loads(Path(sys.argv[1] + '.manifest.json').read_text())
for relative, expected in m['files'].items():
    path = Path('/') / relative
    assert path.is_file(), path
    assert hashlib.sha256(path.read_bytes()).hexdigest() == expected, path
    assert path.stat().st_uid == 0 and path.stat().st_gid == 0, path
binary = Path('/usr/bin/herdr')
assert binary.stat().st_mode & stat.S_IXUSR
assert not binary.stat().st_mode & (stat.S_IWGRP | stat.S_IWOTH)
PY
  local expected
  expected="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["binary"]["version_output"])' "$source_deb.manifest.json")"
  runuser -u "$user" -- env -i PATH=/usr/bin:/bin HOME="$work/home" \
    EXPECTED_VERSION="$expected" PROBE_CWD="$work/cwd" /bin/sh -c '
      cd "$PROBE_CWD"
      test "$(command -v herdr)" = /usr/bin/herdr
      test "$(herdr --version)" = "$EXPECTED_VERSION"
      herdr --help >/dev/null
    '
}
version="$(validate_manifest "$deb")"
[[ "$(dpkg-deb -f "$deb" Package)" == herdr-gx ]]
[[ "$(dpkg-deb -f "$deb" Version)" == "$version" ]]
if [[ -n "$previous" ]]; then
  previous="$(realpath -- "$previous")"
  previous_version="$(validate_manifest "$previous")"
  dpkg --compare-versions "$previous_version" lt "$version" || {
    printf '%s\n' 'Upgrade requires a genuinely older Cargo version, not a same-version reinstall.' >&2; exit 2;
  }
  install_deb "$previous"
  assert_installed "$previous"
  install_deb "$deb"
  printf 'PASS upgrade %s -> %s\n' "$previous_version" "$version"
else
  install_deb "$deb"
  printf '%s\n' 'N/A previous-version upgrade: first release/no previous deb supplied. Same-version reinstall is tested separately.'
fi
assert_installed "$deb"
install_deb "$deb"
assert_installed "$deb"
expected="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["binary"]["version_output"])' "$deb.manifest.json")"
runuser -u "$user" -- env -i PATH=/usr/bin:/bin HOME="$work/home" \
  python3 "$work/runtime.py" --binary herdr --expected-version "$expected"
runuser -u "$user" -- /bin/sh -c 'mkdir -p "$1/.config/herdr"; printf keep-user-data > "$1/.config/herdr/gx-smoke-sentinel"' sh "$work/home"
timeout 120 env DEBIAN_FRONTEND=noninteractive apt-get remove --yes herdr-gx
installed=0
[[ ! -e /usr/bin/herdr ]]
[[ "$(<"$work/home/.config/herdr/gx-smoke-sentinel")" == keep-user-data ]]
runuser -u "$user" -- env -i PATH=/usr/bin:/bin HOME="$work/home" /bin/sh -c '! command -v herdr'

mkdir -p "$work/conflict/DEBIAN" "$work/conflict/usr/bin"
cat > "$work/conflict/DEBIAN/control" <<'CONTROL'
Package: herdr-gx-smoke-conflict
Version: 1.0
Architecture: amd64
Maintainer: Herdr GX smoke <noreply@example.invalid>
Description: Disposable conflicting-file fixture
CONTROL
printf '#!/bin/sh\nprintf other-herdr\\n\n' > "$work/conflict/usr/bin/herdr"
chmod 755 "$work/conflict/usr/bin/herdr"
dpkg-deb --build --root-owner-group "$work/conflict" "$work/conflict.deb"
timeout 120 dpkg --install "$work/conflict.deb"
conflict_installed=1
before="$(sha256sum /usr/bin/herdr)"
if timeout 120 env DEBIAN_FRONTEND=noninteractive apt-get install --yes "$work/candidate.deb"; then
  installed=1
  printf '%s\n' 'Package unexpectedly replaced a conflicting installation.' >&2
  exit 1
fi
[[ "$(sha256sum /usr/bin/herdr)" == "$before" ]]
[[ "$(dpkg-query -W -f='${Status}' herdr-gx-smoke-conflict)" == 'install ok installed' ]]
timeout 120 dpkg --remove herdr-gx-smoke-conflict
conflict_installed=0
printf '%s\n' 'PASS Ubuntu install/reinstall, installed command/payload/runtime, uninstall/data preservation and conflicting-package rejection.'
