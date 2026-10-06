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
        self.assertIn('temporary_root="$run_dir/tmp"', self.smoke)
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
        self.assertIn('state="$temporary_root/$name"', self.case)
        self.assertIn('raw="$out/cpu-raw.txt"', self.case)
        self.assertIn('printf \'%s\\n\' "$status" > "$out/exit-code.txt"', self.case)
        self.assertIn('rm -rf "$state"', self.case)
        self.assertIn('TMUX_TMPDIR="$tmux_root"', self.case)
        self.assertNotIn("/var/tmp", self.case)

    def test_case_passes_isolated_user_state_to_herdr(self):
        for definition in (
            'home="$state/home"',
            'xdg="$state/xdg"',
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

    def _run_contract(self, script: Path, status: int, remove_config_guard: bool = False):
        bash = shutil.which("bash")
        self.assertIsNotNone(bash, "bash is required for performance environment contracts")
        source = script.read_text(encoding="utf-8")
        if script == SMOKE:
            start = source.index('temporary_root="$run_dir/tmp"\n')
            end = source.index('exec > >(tee -a "$run_dir/run.log") 2>&1\n', start)
            source = source[start:end]
            phases = ("smoke-launch", "smoke-cleanup")
        else:
            source = source[:source.index('\npanes_json=\n')]
            phases = ("launch", "control", "stop", "delete")
        if remove_config_guard:
            source = source.replace("unset HERDR_CONFIG_PATH\n", "").replace("-u HERDR_CONFIG_PATH", "")

        with tempfile.TemporaryDirectory(prefix="herdr-perf-environment-") as temporary:
            root = Path(temporary) / "private state"
            for directory in ("bin", "probes", "inherited", "run", "output", "tmp"):
                (root / directory).mkdir(parents=True)
            sentinel = root / "inherited/config-sentinel.toml"
            sentinel.write_bytes(b"do not read or modify this fake user config\n")
            baseline = root / "baseline"
            baseline.write_bytes(b"fake baseline override; never executed\n")
            environment = {
                key: os.environ[key] for key in ("PATH", "SYSTEMROOT", "WINDIR")
                if key in os.environ
            }
            environment.update(HOME=str(root / "inherited"), USERPROFILE=str(root / "inherited"))
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
                HERDR_SESSION="inherited-session",
                TMUX="inherited-tmux",
                CONTRACT_ROOT=shell_root,
                PROBE_DIR=f"{shell_root}/probes",
                CONTRACT_STATUS=str(status),
            )
            stubs = {
                "capture": '''#!/usr/bin/env bash
set -euo pipefail
env -0 > "$PROBE_DIR/$1.env"
''',
                "herdr": '''#!/usr/bin/env bash
set -euo pipefail
case "$1 ${2:-}" in
  '--session '*) phase=launch ;;
  'pane list') phase=control ;;
  'session stop') phase=stop ;;
  'session delete') phase=delete ;;
  *) exit 91 ;;
esac
exec "$CONTRACT_ROOT/bin/capture" "$phase"
''',
                "tmux": '''#!/usr/bin/env bash
set -euo pipefail
case "$1" in
  new-session)
    "$CONTRACT_ROOT/bin/capture" tmux-launch
    exec "$BASH" --noprofile --norc -c "${@: -1}"
    ;;
  kill-session) exec "$CONTRACT_ROOT/bin/capture" tmux-cleanup ;;
  *) exit 92 ;;
esac
''',
            }
            for name, contents in stubs.items():
                path = root / "bin" / name
                path.write_text(contents, encoding="utf-8", newline="\n")
                path.chmod(0o755)
            harness = '''set -euo pipefail
export PATH="$CONTRACT_ROOT/bin:/usr/bin:/bin"
"$CONTRACT_ROOT/bin/capture" before
rm() {
  [[ $# -eq 2 && $1 == -rf ]] || return 93
  case "$2" in
    "$CONTRACT_ROOT/run/tmp") "$CONTRACT_ROOT/bin/capture" smoke-cleanup ;;
    "$CONTRACT_ROOT/tmp/"*) ;;
    *) return 94 ;;
  esac
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
                evidence = root / "run"
                state = root / "run/tmp"
            else:
                harness += '''set -- "$CONTRACT_ROOT/bin/herdr" candidate visible30 1 1 0 \
  "$CONTRACT_ROOT/output" "$CONTRACT_ROOT/tmp" linux
'''
                harness += source
                harness += '\n"${control_env[@]}" "$bin" pane list\n'
                evidence = root / "output/candidate/visible30/r1"
                state = root / "tmp"
            harness += 'exit "$CONTRACT_STATUS"\n'
            harness_path = root / "contract.sh"
            harness_path.write_text(harness, encoding="utf-8", newline="\n")
            result = subprocess.run(
                [bash, "--noprofile", "--norc", f"{shell_root}/contract.sh"],
                cwd=root, env=environment, capture_output=True, text=True, timeout=20, check=False,
            )
            self.assertEqual(result.returncode, status, result.stdout + result.stderr)
            self.assertEqual((evidence / "exit-code.txt").read_text().strip(), str(status))
            self.assertEqual(sentinel.read_bytes(), b"do not read or modify this fake user config\n")
            self.assertEqual(baseline.read_bytes(), b"fake baseline override; never executed\n")
            if script == SMOKE:
                self.assertFalse(state.exists())
                self.assertTrue((evidence / "metadata.txt").is_file())
                self.assertIn(f"source={shell_root}/baseline", (evidence / "baseline-command.txt").read_text())
            else:
                self.assertEqual([path.name for path in state.iterdir()], ["tmux"])
                self.assertTrue((evidence / "case-metadata.txt").is_file())
                self.assertTrue((evidence / "launch-command.txt").is_file())
            observations = {
                path.stem: dict(entry.split("=", 1) for entry in path.read_text().split("\0") if entry)
                for path in (root / "probes").glob("*.env")
            }
            self.assertEqual(
                observations["before"]["HERDR_CONFIG_PATH"],
                f"{shell_root}/inherited/config-sentinel.toml",
            )
            for phase in phases:
                self.assertIn(phase, observations)
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
                self._assert_isolated(observations, shell_root, phases, f"{shell_root}/run/tmp")

    def test_standalone_case_drops_config_path_for_launch_control_and_cleanup(self):
        for status in (0, 23):
            with self.subTest(exit_status=status):
                observations, shell_root, phases = self._run_contract(CASE, status)
                self._assert_isolated(observations, shell_root, phases, f"{shell_root}/tmp")
                self.assertNotIn("HERDR_SESSION", observations["launch"])
                session = observations["control"]["HERDR_SESSION"]
                self.assertTrue(session.startswith("rpslcv3r1x"), session)
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


if __name__ == "__main__":
    unittest.main()
