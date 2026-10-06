from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
SMOKE = ROOT / "scripts/release_perf_smoke.sh"
CASE = ROOT / "scripts/release_perf_case.sh"


class ReleasePerfScriptContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.smoke = SMOKE.read_text(encoding="utf-8")
        cls.case = CASE.read_text(encoding="utf-8")

    def test_smoke_records_a_project_local_unique_run_and_preserves_results(self):
        self.assertIn('result_root="$repo_root/.local/perf-baseline"', self.smoke)
        self.assertIn('run_dir=$(mktemp -d "$result_root/run-', self.smoke)
        self.assertIn('printf \'%s\\n\' "$run_id" > "$run_dir/run-id.txt"', self.smoke)
        self.assertIn('printf \'%s\\n\' "$status" > "$run_dir/exit-code.txt"', self.smoke)
        self.assertIn('summary="$run_dir/summary.txt"', self.smoke)
        self.assertIn('exec > >(tee -a "$run_dir/run.log") 2>&1', self.smoke)
        self.assertIn('rm -rf "$temporary_root"', self.smoke)
        self.assertNotIn('rm -rf "$run_dir"', self.smoke)
        self.assertNotIn("/var/tmp", self.smoke)

    def test_smoke_isolates_user_state_inside_run_temp(self):
        self.assertIn('temporary_root=$(mktemp -d "$repo_root/.local/p-XXXXXX")', self.smoke)
        self.assertIn('runtime-owner.txt', self.smoke)
        for fragment in (
            'export HOME="$isolated_home"',
            'export USERPROFILE="$isolated_home"',
            'export XDG_CONFIG_HOME="$isolated_config"',
            'export XDG_STATE_HOME="$isolated_state"',
            'export XDG_RUNTIME_DIR="$isolated_runtime"',
            'export XDG_DATA_HOME="$isolated_data"',
            'export XDG_CACHE_HOME="$isolated_cache"',
            'export APPDATA="$isolated_appdata"',
            'export LOCALAPPDATA="$isolated_localappdata"',
            'export HERDR_HOME="$isolated_herdr_home"',
            'export CODEX_HOME="$isolated_codex_home"',
            'export KIMI_CODE_HOME="$isolated_kimi_home"',
            'export TMPDIR="$isolated_tmp"',
        ):
            self.assertIn(fragment, self.smoke)
        self.assertIn('chmod 700 "$temporary_root"', self.smoke)

    def test_smoke_records_commands_and_default_serial_execution(self):
        for path in ("candidate-command.txt", "baseline-command.txt"):
            self.assertIn(path, self.smoke)
        for field in ("rounds=2", "scenarios=hidden50,visible30", "execution=serial"):
            self.assertIn(field, self.smoke)
        self.assertIn('"$results" "$temporary_root" "$platform"', self.smoke)

    def test_case_keeps_raw_samples_and_cleans_only_the_temporary_state(self):
        self.assertIn("usage: $0 <binary> <variant> <scenario> <round> <seconds> <warmup> <output-root> <temporary-root> <platform>", self.case)
        self.assertIn('state=$(mktemp -d "$temporary_root/c-XXXXXX")', self.case)
        self.assertIn('raw="$out/cpu-raw.txt"', self.case)
        self.assertIn('printf \'%s\\n\' "$status" > "$out/exit-code.txt"', self.case)
        self.assertIn('rm -rf "$state"', self.case)
        self.assertIn('TMUX_TMPDIR="$tmux_root"', self.case)
        self.assertNotIn("/var/tmp", self.case)

    def test_case_passes_isolated_user_state_to_herdr(self):
        for definition in (
            'home="$state/home"',
            'xdg="$state/c"',
            'xdg_state="$state/xdg-state"',
            'runtime="$state/run"',
            'xdg_data="$state/xdg-data"',
            'xdg_cache="$state/xdg-cache"',
            'appdata="$state/appdata"',
            'localappdata="$state/localappdata"',
            'herdr_home="$state/herdr-home"',
            'codex_home="$state/codex-home"',
            'kimi_home="$state/kimi-home"',
            'tmp="$state/tmp"',
        ):
            self.assertIn(definition, self.case)
        for fragment in (
            'HOME="$home"',
            'USERPROFILE="$home"',
            'XDG_CONFIG_HOME="$xdg"',
            'XDG_STATE_HOME="$xdg_state"',
            'XDG_RUNTIME_DIR="$runtime"',
            'XDG_DATA_HOME="$xdg_data"',
            'XDG_CACHE_HOME="$xdg_cache"',
            'APPDATA="$appdata"',
            'LOCALAPPDATA="$localappdata"',
            'HERDR_HOME="$herdr_home"',
            'CODEX_HOME="$codex_home"',
            'KIMI_CODE_HOME="$kimi_home"',
            'TMPDIR="$tmp"',
        ):
            self.assertGreaterEqual(self.case.count(fragment), 2, fragment)

    def test_windows_remains_explicitly_not_supported(self):
        self.assertIn("release performance smoke supports Linux and macOS", self.smoke)
        self.assertIn('case "$platform" in linux) platform_tag=l ;; macos) platform_tag=m ;; *) exit 2 ;; esac', self.case)
        self.assertNotIn("powershell", self.smoke.lower())
        self.assertNotIn("pwsh", self.smoke.lower())

    def test_shell_scripts_are_syntactically_valid_when_bash_is_available(self):
        bash = shutil.which("bash")
        if bash is None:
            self.skipTest("bash is required for Unix performance script syntax validation")
        for script in (SMOKE, CASE):
            with self.subTest(script=script.name):
                result = subprocess.run(
                    [bash, "-n", str(script)],
                    cwd=ROOT,
                    capture_output=True,
                    text=True,
                    timeout=20,
                    check=False,
                )
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_maintenance_manifest_registers_this_contract(self):
        justfile = (ROOT / "justfile").read_text(encoding="utf-8")
        self.assertIn("scripts.test_release_perf", justfile)


class ReleasePerfEnvironmentContractTests(unittest.TestCase):
    ISOLATED_DIRS = (
        "HOME", "USERPROFILE", "XDG_CONFIG_HOME", "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR", "XDG_DATA_HOME", "XDG_CACHE_HOME", "APPDATA",
        "LOCALAPPDATA", "HERDR_HOME", "CODEX_HOME", "KIMI_CODE_HOME", "TMPDIR",
    )
    CASE_UNSET = (
        "HERDR_BIN_PATH", "HERDR_ENV", "HERDR_SOCKET_PATH", "HERDR_CLIENT_SOCKET_PATH",
        "HERDR_STARTUP_CWD", "HERDR_WORKSPACE_ID", "HERDR_TAB_ID", "HERDR_PANE_ID",
    )

    def _run_contract(self, script: Path, status: int, remove_config_guard: bool = False,
                      fault: str = "", full_case: bool = False, platform: str = "linux",
                      readiness_stderr: str = "last failure", rewrite_tmp: bool = False):
        bash = shutil.which("bash")
        self.assertIsNotNone(bash, "bash is required for performance environment contracts")
        source = script.read_text(encoding="utf-8")
        if script == SMOKE:
            start = source.index('umask 077\n')
            end = source.index('exec > >(tee -a "$run_dir/run.log") 2>&1\n', start)
            source = source[start:end]
            phases = ("smoke-launch", "smoke-cleanup") if not fault else ("smoke-launch",)
        else:
            if not full_case:
                source = source[:source.index('\npanes_json=\n')]
            phases = ("launch", "control", "stop", "delete")
        if remove_config_guard:
            source = source.replace("unset HERDR_CONFIG_PATH\n", "").replace("-u HERDR_CONFIG_PATH", "")

        local = ROOT / ".local"
        local.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="t-", dir=local) as temporary:
            root = Path(temporary)
            for directory in ("bin", "probes", "inherited", "run", "output", "tmp", ".local"):
                (root / directory).mkdir(parents=True)
            sentinel = root / "inherited/config-sentinel.toml"
            sentinel.write_bytes(b"do not read or modify this fake user config\n")
            baseline = root / "baseline"
            baseline.write_bytes(b"fake baseline override; never executed\n")
            outside_files = {}
            if fault.startswith("symlink-"):
                for relative in ("herdr-server.log", "p/herdr-server.log",
                                 "sessions/p/herdr-server.log", "herdr/sessions/p/herdr-server.log"):
                    external = root / "outside" / relative
                    external.parent.mkdir(parents=True, exist_ok=True)
                    external.write_bytes(b"external sentinel: must not read or change\n")
                    outside_files[external] = external.read_bytes()
                link = root / "middle-link"
                if os.name == "nt":
                    linked = subprocess.run(
                        ["cmd.exe", "/c", "mklink", "/J", str(link), str(root / "outside")],
                        capture_output=True, text=True, timeout=20, check=False,
                    )
                    self.assertEqual(linked.returncode, 0, linked.stdout + linked.stderr)
                else:
                    link.symlink_to(root / "outside", target_is_directory=True)
            environment = {
                key: os.environ[key] for key in ("PATH", "SYSTEMROOT", "WINDIR")
                if key in os.environ
            }
            process_tmp = root / "process tmp"
            process_tmp.mkdir()
            environment.update(
                HOME=str(root / "inherited"), USERPROFILE=str(root / "inherited"),
                TMP=process_tmp.as_posix(), TEMP=process_tmp.as_posix(),
                TMPDIR=process_tmp.as_posix(),
            )
            for key in self.ISOLATED_DIRS:
                (root / "inherited" / key).mkdir()
            shell_root = root.as_posix()
            if os.name == "nt":
                converted = subprocess.run(
                    [bash, "--noprofile", "--norc", "-c", 'cygpath -u "$1"', "contract", str(root)],
                    cwd=root, env=environment, capture_output=True, text=True, timeout=20, check=False,
                )
                self.assertEqual(converted.returncode, 0, converted.stderr)
                shell_root = converted.stdout.strip()
                self.assertTrue(shell_root.startswith("/"), shell_root)
            environment.update({key: f"{shell_root}/inherited/{key}" for key in self.ISOLATED_DIRS})
            environment.update({key: f"{shell_root}/inherited/{key}" for key in self.CASE_UNSET})
            environment.update(
                HERDR_CONFIG_PATH=f"{shell_root}/inherited/config-sentinel.toml",
                HERDR_PERF_BASELINE_BIN=f"{shell_root}/baseline",
                HERDR_SESSION="inherited-session", TMUX="inherited-tmux",
                CONTRACT_ROOT=shell_root, PROBE_DIR=f"{shell_root}/probes",
                CONTRACT_STATUS=str(status), CONTRACT_FAULT=fault,
                CONTRACT_READINESS_STDERR=readiness_stderr,
                CONTRACT_PROCESS_TMP=f"{shell_root}/process tmp",
                BASH_ENV=f"{shell_root}/private-bash-env.sh",
            )
            (root / "private-bash-env.sh").write_text(
                'export TMP="$CONTRACT_PROCESS_TMP" TEMP="$CONTRACT_PROCESS_TMP"\n',
                encoding="utf-8", newline="\n",
            )
            stubs = {
                "capture": '''#!/usr/bin/env bash
set -euo pipefail
[[ $TMP == "$CONTRACT_PROCESS_TMP" && $TEMP == "$CONTRACT_PROCESS_TMP" ]] || exit 97
[[ -d $TMP && -d $TEMP && -d $TMPDIR ]] || exit 97
printf 'private temp write\\n' > "$TMP/capture-$1"
env -0 > "$PROBE_DIR/$1.env"
''',
                "herdr": '''#!/usr/bin/env bash
set -euo pipefail
case "$1 ${2:-}" in
  '--session '*)
    phase=launch
    printf '%s' "$2" > "$PROBE_DIR/launch-name.txt"
    mkdir -p "$XDG_CONFIG_HOME/herdr/sessions/$2"
    printf 'synthetic server failure\\n' > "$XDG_CONFIG_HOME/herdr/sessions/$2/herdr-server.log"
    printf 'not allowlisted\\n' > "$XDG_CONFIG_HOME/herdr/sessions/$2/private.txt"
    ;;
  'pane list') phase=control ;;
  'session stop') phase=stop ;;
  'session delete') phase=delete ;;
  'session list') phase=list ;;
  'workspace list') printf '{"result":{"workspaces":[{"workspace_id":"w"}]}}'; exit 0 ;;
  'pane run') exit 0 ;;
  'pane read') printf 'bench-output-'; exit 0 ;;
  *) exit 91 ;;
esac
[[ -f "$PROBE_DIR/$phase.env" ]] || env -0 > "$PROBE_DIR/$phase.env"
printf '%s\\n' "$phase" >> "$PROBE_DIR/calls.txt"
[[ $CONTRACT_FAULT != "$phase" ]] || exit 41
case "$phase" in
  control)
    if [[ $CONTRACT_FAULT == readiness ]]; then printf '%s' "$CONTRACT_READINESS_STDERR" >&2; printf 'partial'; exit 37; fi
    printf '{"result":{"panes":[{"pane_id":"p"}]}}'
    ;;
  list) printf '{"sessions":[]}' ;;
esac
''',
                "tmux": '''#!/usr/bin/env bash
set -euo pipefail
[[ $1 == -S && $3 == -f ]] || exit 90
printf '%s\\n' "$1 $2 $3 $4" >> "$PROBE_DIR/tmux-args.txt"
socket_path=$2
shift 4
case "$1" in
  new-session)
    : > "$socket_path"
    "$CONTRACT_ROOT/bin/capture" tmux-launch
    exec "$BASH" --noprofile --norc -c "${@: -1}"
    ;;
  display-message) printf '303\\n' ;;
  kill-server)
    "$CONTRACT_ROOT/bin/capture" tmux-cleanup
    if [[ $CONTRACT_FAULT == tmux-alive-no-socket ]]; then command rm "$socket_path"; fi
    [[ $CONTRACT_FAULT != tmux ]]
    ;;
  list-panes)
    if [[ ${@: -1} == '#{pane_pid}' ]]; then printf '202'; else printf '202 1 37\\n'; fi
    ;;
  capture-pane)
    [[ $CONTRACT_FAULT != capture-failure ]] || exit 43
    printf 'synthetic dead pane\\n'
    ;;
  *) exit 92 ;;
esac
''',
                "jq": '''#!/usr/bin/env bash
set -euo pipefail
case "$*" in
  *'.sessions |'*) [[ $(<"${@: -1}") == '{"sessions":[]}' ]] ;;
  *pane_id*) cat >/dev/null; printf 'p' ;;
  *workspace_id*) cat >/dev/null; printf 'w' ;;
  *) exit 96 ;;
esac
''',
                "lsof": '#!/usr/bin/env bash\nprintf "101\\n"\n',
                "pidstat": '#!/usr/bin/env bash\nprintf "0 0 101 1.0 0 0\\n0 0 202 2.0 0 0\\n"\n',
                "tail": '''#!/usr/bin/env bash
printf '%s\\n' "${@: -1}" >> "$PROBE_DIR/log-reads.txt"
[[ $CONTRACT_FAULT != tail-failure ]] || exit 44
exec /usr/bin/tail "$@"
''',
            }
            real_perl = shutil.which("perl")
            if os.name != "nt":
                self.assertIsNotNone(real_perl)
            environment.update(CONTRACT_PERL=Path(real_perl).as_posix() if real_perl else "",
                               CONTRACT_WINDOWS=str(int(os.name == "nt")))
            stubs["perl"] = '''#!/usr/bin/env bash
set -euo pipefail
[[ $1 == -e ]] || exit 98
case "$2" in
  *lstat*0700*)
    [[ -d $3 ]] || exit 98
    if [[ $CONTRACT_WINDOWS == 1 ]]; then
      [[ $CONTRACT_FAULT != public-root ]]
    else
      exec "$CONTRACT_PERL" "$@"
    fi
    ;;
  *'use Errno qw(ESRCH)'*)
    [[ $3 == 303 && -f "$PROBE_DIR/tmux-cleanup.env" ]] || exit 98
    count=0
    [[ ! -f "$PROBE_DIR/tmux-probes.txt" ]] || count=$(<"$PROBE_DIR/tmux-probes.txt")
    count=$((count + 1))
    printf '%s\\n' "$count" > "$PROBE_DIR/tmux-probes.txt"
    case "$CONTRACT_FAULT" in
      tmux-alive|tmux-alive-no-socket) exit 0 ;;
      tmux-unknown) exit 4 ;;
      tmux-delay) [[ $count -gt 2 ]] || exit 0 ;;
    esac
    exit 3
    ;;
  *) exit 98 ;;
esac
'''
            if rewrite_tmp:
                stubs["bash"] = '''#!/bin/sh
export TMP=/tmp TEMP=/tmp
exec "$CONTRACT_BASH" "$@"
'''
            for name, contents in stubs.items():
                path = root / "bin" / name
                path.write_text(contents, encoding="utf-8", newline="\n")
                path.chmod(0o755)
            harness = '''set -euo pipefail
export PATH="$CONTRACT_ROOT/bin:/usr/bin:/bin"
export CONTRACT_BASH="$BASH"
sleep() { :; }
"$CONTRACT_ROOT/bin/capture" before
rm() {
  [[ $# -eq 2 && $1 == -rf ]] || return 93
  case "$2" in
    "$CONTRACT_ROOT/.local/p-"*) "$CONTRACT_ROOT/bin/capture" smoke-cleanup ;;
    "$CONTRACT_ROOT/tmp/"*) ;;
    *) return 94 ;;
  esac
  [[ $CONTRACT_FAULT != remove ]] || return 95
  command rm "$@"
}
'''
            if script == SMOKE:
                harness += '''run_dir="$CONTRACT_ROOT/run"
run_id=contract-run
repo_root="$CONTRACT_ROOT"
set -- "$CONTRACT_ROOT/bin/herdr"
'''
                harness += source
                harness += '\n"$CONTRACT_ROOT/bin/capture" smoke-launch\n'
                if fault == "owner":
                    harness += 'printf "wrong owner\\n" > "$temporary_root/owner.txt"\n'
                evidence = root / "run"
            else:
                harness += '''printf 'uid=%s\\nruntime=%s\\n' "$(id -u)" "$CONTRACT_ROOT/tmp" > "$CONTRACT_ROOT/tmp/owner.txt"
cp "$CONTRACT_ROOT/tmp/owner.txt" "$CONTRACT_ROOT/runtime-owner.txt"
'''
                if fault == "owner":
                    harness += 'rm -f "$CONTRACT_ROOT/tmp/owner.txt"\n'.replace('rm -f', 'command rm -f')
                output = "output/" + "long-evidence-" * 12 if fault == "long-evidence" else "output"
                (root / output).mkdir(parents=True, exist_ok=True)
                if fault == "long-evidence":
                    harness += f'cp "$CONTRACT_ROOT/runtime-owner.txt" "$CONTRACT_ROOT/{output}/../runtime-owner.txt"\n'
                runtime = "tmp"
                if fault in ("long-runtime", "unicode-runtime"):
                    runtime += "/" + ("x" * 110 if fault == "long-runtime" else "界" * 37)
                    harness += f'mkdir -p "$CONTRACT_ROOT/{runtime}"\n'
                    harness += f'printf "uid=%s\\nruntime=%s\\n" "$(id -u)" "$CONTRACT_ROOT/{runtime}" > "$CONTRACT_ROOT/{runtime}/owner.txt"\n'
                    harness += f'cp "$CONTRACT_ROOT/{runtime}/owner.txt" "$CONTRACT_ROOT/runtime-owner.txt"\n'
                mode = "755" if fault == "public-root" else "700"
                harness += f'chmod {mode} "$CONTRACT_ROOT/{runtime}"\n'
                harness += f'''set -- "$CONTRACT_ROOT/bin/herdr" candidate visible30 1 1 0 \\
  "$CONTRACT_ROOT/{output}" "$CONTRACT_ROOT/{runtime}" {platform}
'''
                harness += source
                if not full_case:
                    harness += '\n"${control_env[@]}" "$bin" pane list\n'
                    if fault == "case-owner":
                        harness += 'printf "wrong owner\\n" > "$state/owner.txt"\n'
                if fault.startswith("symlink-"):
                    relative = {"symlink-c": "c", "symlink-herdr": "c/herdr",
                                "symlink-sessions": "c/herdr/sessions",
                                "symlink-session": "c/herdr/sessions/p"}[fault]
                    harness += f'command rm -rf "$state/{relative}"\n'
                    harness += f'mv "$CONTRACT_ROOT/middle-link" "$state/{relative}"\n'
                    harness += f'[[ -L "$state/{relative}" ]] || exit 99\n'
                elif fault == "redirect-failure":
                    harness += 'mkdir "$out/tmux-pane.txt"\n'
                elif fault == "missing-log":
                    harness += 'command rm "$xdg/herdr/sessions/$name/herdr-server.log"\n'
                evidence = root / output / "candidate/visible30/r1"
            harness += 'exit "$CONTRACT_STATUS"\n'
            harness_path = root / "contract.sh"
            harness_path.write_text(harness, encoding="utf-8", newline="\n")
            result = subprocess.run(
                [bash, "--noprofile", "--norc", f"{shell_root}/contract.sh"],
                cwd=root, env=environment, capture_output=True, text=True, timeout=60, check=False,
            )
            expected = status
            if script == CASE and fault in ("owner", "public-root", "long-runtime", "unicode-runtime"):
                expected = 2
            elif fault and fault not in ("long-evidence", "missing-log", "tmux-delay") and not status:
                expected = 1
            self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
            self.assertEqual(sentinel.read_bytes(), b"do not read or modify this fake user config\n")
            self.assertEqual(baseline.read_bytes(), b"fake baseline override; never executed\n")
            if script == CASE and fault in ("owner", "public-root"):
                self.assertIn("ownership receipt", result.stderr)
                self.assertFalse((root / "probes/launch.env").exists())
                return
            self.assertEqual((evidence / "exit-code.txt").read_text().strip(), str(expected))
            if script == SMOKE:
                receipt = (evidence / "runtime-owner.txt").read_text()
                runtime_path = next(line.split("=", 1)[1] for line in receipt.splitlines() if line.startswith("runtime="))
                self.assertIn("/.local/p-", runtime_path)
                self.assertTrue((evidence / "metadata.txt").is_file())
                self.assertIn(f"source={shell_root}/baseline", (evidence / "baseline-command.txt").read_text())
                self.assertEqual(len(list((root / ".local").glob("p-*"))), 1 if fault else 0)
            else:
                self.assertTrue((evidence / "case-metadata.txt").is_file())
                if fault in ("long-runtime", "unicode-runtime"):
                    self.assertIn("Unix socket path too long", result.stderr)
                    self.assertEqual((evidence / "cleanup.txt").read_text(), "state=not-started\n")
                    self.assertFalse((root / "probes/launch.env").exists())
                    return
                self.assertTrue((evidence / "launch-command.txt").is_file())
                diagnostics_failed = fault in ("capture-failure", "tail-failure", "redirect-failure")
                unsafe_path = fault.startswith("symlink-")
                if fault in ("stop", "delete", "list", "tmux", "remove", "case-owner", "tmux-alive", "tmux-alive-no-socket", "tmux-unknown") or diagnostics_failed or unsafe_path:
                    self.assertTrue((root / "tmp/retain").exists())
                    self.assertTrue(list((root / "tmp").glob("c-*")))
                else:
                    self.assertFalse(list((root / "tmp").glob("c-*")))
                if unsafe_path:
                    self.assertIn("ownership=refused", (evidence / "cleanup.txt").read_text())
                    self.assertFalse((root / "probes/log-reads.txt").exists())
                    for phase in ("stop", "delete", "list", "tmux-cleanup"):
                        self.assertFalse((root / f"probes/{phase}.env").exists())
                    self.assertFalse((evidence / "herdr-server.log.txt").exists())
                    for external, content in outside_files.items():
                        self.assertEqual(external.read_bytes(), content)
                elif diagnostics_failed:
                    self.assertIn("diagnostics=failed", (evidence / "cleanup.txt").read_text())
                    self.assertTrue((root / "probes/stop.env").exists())
                    self.assertFalse((root / "probes/delete.env").exists())
                    self.assertFalse((root / "probes/tmux-cleanup.env").exists())
                    originals = list((root / "tmp").glob("c-*/c/herdr/sessions/p/herdr-server.log"))
                    self.assertEqual(len(originals), 1)
                    self.assertIn("synthetic server failure", originals[0].read_text())
                elif fault != "case-owner":
                    self.assertIn("synthetic dead pane", (evidence / "tmux-pane.txt").read_text())
                    self.assertIn("202 1 37", (evidence / "tmux-status.txt").read_text())
                    if fault == "missing-log":
                        self.assertIn("herdr-server.log=absent", (evidence / "diagnostics-status.txt").read_text())
                    else:
                        self.assertIn("synthetic server failure", (evidence / "herdr-server.log.txt").read_text())
                    self.assertFalse((evidence / "private.txt").exists())
                calls = (root / "probes/calls.txt").read_text().splitlines()
                if fault == "delete":
                    self.assertEqual(calls.count("delete"), 50)
                    self.assertEqual((evidence / "cleanup.txt").read_text().count("delete=41\n"), 50)
                if fault == "readiness":
                    self.assertEqual(calls.count("control"), 150)
                    self.assertEqual((evidence / "readiness-exit-code.txt").read_text().strip(), "37")
                    self.assertEqual((evidence / "readiness-stderr.txt").read_text(), readiness_stderr)
                    self.assertEqual((evidence / "readiness-stdout.txt").read_text(), "partial")
                elif full_case:
                    self.assertEqual(float((evidence / "total-cpu.txt").read_text()), 3.0)
                if not unsafe_path and not diagnostics_failed and fault != "case-owner":
                    self.assertEqual((evidence / "tmux-server-pid.txt").read_text().strip(), "303")
                    probes = (evidence / "tmux-process-probe.txt").read_text().splitlines()
                    if fault in ("tmux-alive", "tmux-alive-no-socket", "tmux-unknown"):
                        self.assertIn("tmux_process=unknown-or-running", (evidence / "cleanup.txt").read_text())
                        self.assertEqual(len(probes), 1 if fault == "tmux-unknown" else 50)
                        self.assertEqual(bool(list((root / "tmp").glob("c-*/t"))), fault != "tmux-alive-no-socket")
                    else:
                        self.assertIn("tmux_process=exited\ntmux_socket=stale-owned", (evidence / "cleanup.txt").read_text())
                        self.assertEqual(probes[-1], "pid=303 probe=3")
                        self.assertEqual(len(probes), 3 if fault == "tmux-delay" else 1)
                tmux_args = (root / "probes/tmux-args.txt").read_text().splitlines()
                self.assertEqual(len(set(tmux_args)), 1)
            observations = {
                path.stem: dict(entry.split("=", 1) for entry in path.read_text().split("\0") if entry)
                for path in (root / "probes").glob("*.env")
            }
            self.assertEqual(observations["before"]["HERDR_CONFIG_PATH"], f"{shell_root}/inherited/config-sentinel.toml")
            for observed in observations.values():
                for key in ("TMP", "TEMP"):
                    self.assertIn(observed[key], (process_tmp.as_posix(), f"{shell_root}/process tmp"))
            self.assertEqual((process_tmp / "capture-before").read_text(), "private temp write\n")
            self.assertNotIn("could not find /tmp", result.stderr)
            if not fault:
                for phase in phases:
                    self.assertIn(phase, observations)
            if script == CASE:
                self.assertEqual((root / "probes/launch-name.txt").read_text(), observations["control"]["HERDR_SESSION"])
            return observations, shell_root, phases

    def _assert_isolated(self, observations, shell_root, phases, temporary_root):
        for phase in phases:
            with self.subTest(phase=phase):
                environment = observations[phase]
                self.assertNotIn("HERDR_CONFIG_PATH", environment)
                for key in self.ISOLATED_DIRS:
                    self.assertTrue(environment[key].startswith(temporary_root + "/"), key)
                self.assertEqual(environment["HERDR_PERF_BASELINE_BIN"], f"{shell_root}/baseline")

    def test_smoke_drops_config_path_for_children_and_cleanup(self):
        for status in (0, 23):
            with self.subTest(exit_status=status):
                observations, shell_root, phases = self._run_contract(SMOKE, status)
                runtime = observations["smoke-launch"]["HOME"].rsplit("/", 1)[0]
                self.assertTrue(runtime.startswith(f"{shell_root}/.local/p-"))
                self._assert_isolated(observations, shell_root, phases, runtime)

    def test_standalone_case_drops_config_path_for_launch_control_and_cleanup(self):
        for status in (0, 23):
            with self.subTest(exit_status=status):
                observations, shell_root, phases = self._run_contract(CASE, status)
                self._assert_isolated(observations, shell_root, phases, f"{shell_root}/tmp")
                self.assertNotIn("HERDR_SESSION", observations["launch"])
                session = observations["control"]["HERDR_SESSION"]
                self.assertEqual(session, "p")
                for phase in ("control", "stop", "delete"):
                    self.assertEqual(observations[phase]["HERDR_SESSION"], session)
                for phase in phases:
                    for key in self.CASE_UNSET:
                        self.assertNotIn(key, observations[phase])
                for phase in ("tmux-launch", "tmux-cleanup"):
                    self.assertNotIn("TMUX", observations[phase])
                    self.assertEqual(observations[phase]["TMUX_TMPDIR"], f"{shell_root}/tmp/tmux")

    def test_negative_control_detects_config_path_when_guards_are_removed(self):
        for script in (SMOKE, CASE):
            with self.subTest(script=script.name):
                observations, shell_root, phases = self._run_contract(script, 23, remove_config_guard=True)
                for phase in phases:
                    self.assertEqual(
                        observations[phase]["HERDR_CONFIG_PATH"],
                        f"{shell_root}/inherited/config-sentinel.toml",
                    )

    def test_long_evidence_does_not_lengthen_runtime_sockets(self):
        self._run_contract(CASE, 0, fault="long-evidence", full_case=True)

    def test_socket_preflight_fails_closed_on_both_unix_platforms(self):
        for platform in ("linux", "macos"):
            with self.subTest(platform=platform):
                self._run_contract(CASE, 0, fault="long-runtime", platform=platform)

    def test_successful_samples_fail_when_cleanup_is_uncertain(self):
        for fault in ("stop", "delete", "list", "tmux", "remove"):
            with self.subTest(fault=fault):
                self._run_contract(CASE, 0, fault=fault, full_case=True)

    def test_cleanup_failure_preserves_first_failure(self):
        self._run_contract(CASE, 23, fault="stop")
        self._run_contract(SMOKE, 23, fault="remove")

    def test_missing_or_changed_ownership_refuses_cleanup(self):
        self._run_contract(CASE, 0, fault="owner")
        self._run_contract(CASE, 0, fault="case-owner")
        self._run_contract(SMOKE, 0, fault="owner")

    def test_readiness_failure_retains_diagnostics_before_cleanup(self):
        self._run_contract(CASE, 0, fault="readiness", full_case=True)

    def test_socket_limit_counts_utf8_bytes_not_characters(self):
        self._run_contract(CASE, 0, fault="unicode-runtime", platform="macos")

    def test_world_readable_runtime_is_rejected_before_launch(self):
        self._run_contract(CASE, 0, fault="public-root")

    def test_smoke_cleanup_failure_cannot_report_success(self):
        self._run_contract(SMOKE, 0, fault="remove")

    def test_middle_symlinks_refuse_reads_and_cleanup_commands(self):
        for component in ("c", "herdr", "sessions", "session"):
            with self.subTest(component=component):
                self._run_contract(CASE, 0, fault=f"symlink-{component}")

    def test_diagnostic_failures_retain_originals_and_do_not_delete(self):
        for fault in ("capture-failure", "tail-failure", "redirect-failure"):
            with self.subTest(fault=fault):
                self._run_contract(CASE, 0, fault=fault, full_case=True)

    def test_diagnostic_failure_preserves_first_failure(self):
        self._run_contract(CASE, 23, fault="tail-failure")

    def test_absent_optional_logs_do_not_fail_cleanup(self):
        self._run_contract(CASE, 0, fault="missing-log")

    def test_readiness_preserves_multiline_stderr_without_filtering(self):
        stderr = "bash.exe: warning: could not find /tmp, please create!\n" * 2 + "last failure"
        self._run_contract(CASE, 0, fault="readiness", full_case=True, readiness_stderr=stderr)

    def test_bash_startup_tmp_rewrite_still_uses_private_temp(self):
        self._run_contract(CASE, 0, rewrite_tmp=True)
        self._run_contract(SMOKE, 0, rewrite_tmp=True)

    def test_tmux_delayed_exit_with_stale_socket_is_confirmed(self):
        self._run_contract(CASE, 0, fault="tmux-delay", full_case=True)

    def test_tmux_alive_or_unknown_retains_runtime_and_fails(self):
        self._run_contract(CASE, 0, fault="tmux-alive", full_case=True)
        self._run_contract(CASE, 0, fault="tmux-alive-no-socket", full_case=True)
        self._run_contract(CASE, 0, fault="tmux-unknown", full_case=True)
        self._run_contract(CASE, 23, fault="tmux-alive")


if __name__ == "__main__":
    unittest.main()
