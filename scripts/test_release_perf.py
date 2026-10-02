from __future__ import annotations

import shutil
import subprocess
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


if __name__ == "__main__":
    unittest.main()
