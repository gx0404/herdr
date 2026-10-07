import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from scripts import windows_cross


class WindowsCrossTests(unittest.TestCase):
    def test_libc_configuration_matches_xwin_layout(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for relative in (
                "sdk/include/ucrt/stdlib.h", "crt/include/vcruntime.h",
                "sdk/lib/ucrt/x86_64/ucrt.lib", "crt/lib/x86_64/vcruntime.lib",
                "sdk/lib/um/x86_64/kernel32.lib",
            ):
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.touch()
            contents = windows_cross.libc_contents(root)
            self.assertIn(f"include_dir={root / 'sdk/include/ucrt'}\n", contents)
            self.assertIn(f"msvc_lib_dir={root / 'crt/lib/x86_64'}\n", contents)
            self.assertTrue(contents.endswith("gcc_dir=\n"))
            (root / "crt/include/vcruntime.h").unlink()
            with self.assertRaisesRegex(ValueError, "missing .*vcruntime.h"):
                windows_cross.libc_contents(root)

    def test_persistent_configuration_and_explicit_override(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            default = root / "libc.txt"
            override = root / "custom.txt"
            default.touch()
            override.touch()
            with patch.object(windows_cross, "SDK_ROOT", root), patch.dict(os.environ, {}, clear=True):
                self.assertEqual(windows_cross.libc_path(), default.resolve())
                with patch.dict(os.environ, {windows_cross.LIBC_ENV: str(override)}):
                    self.assertEqual(windows_cross.libc_path(), override.resolve())
                    override.unlink()
                    with self.assertRaisesRegex(ValueError, "just setup-windows-cross"):
                        windows_cross.libc_path()

    def test_sdk_root_defaults_inside_repo(self):
        # 构建本地性（fork 硬规则）：无显式覆盖时 SDK 根必须落在仓库 .local/ 内。
        if os.environ.get("HERDR_WINDOWS_CROSS_ROOT"):
            self.skipTest("HERDR_WINDOWS_CROSS_ROOT 显式覆盖时默认值不生效")
        self.assertEqual(
            windows_cross.SDK_ROOT,
            windows_cross._REPO_ROOT / ".local" / "windows-cross",
        )

    def test_sdk_root_env_override_leaves_repo(self):
        import importlib

        try:
            with patch.dict(os.environ, {"HERDR_WINDOWS_CROSS_ROOT": "D:/shared/sdk"}, clear=True):
                importlib.reload(windows_cross)
                self.assertEqual(windows_cross.SDK_ROOT, Path("D:/shared/sdk"))
        finally:
            with patch.dict(os.environ, dict(os.environ), clear=True):
                importlib.reload(windows_cross)

    def test_missing_setup_does_not_run_build_or_download(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(windows_cross, "SDK_ROOT", Path(directory)), \
                    patch.dict(os.environ, {}, clear=True), \
                    patch.object(windows_cross.subprocess, "run") as run:
                with self.assertRaisesRegex(ValueError, "just setup-windows-cross"):
                    windows_cross.lint()
                run.assert_not_called()

    def test_lint_passes_sdk_to_cargo_without_changing_parent_environment(self):
        with patch.object(windows_cross, "libc_path", return_value=Path("/sdk/libc.txt")), \
                patch.dict(os.environ, {"KEEP_ME": "yes"}, clear=True), \
                patch.object(windows_cross.subprocess, "run") as run:
            windows_cross.lint()
            self.assertEqual(run.call_count, 2)
            cargo = run.call_args
            # 与 CI `windows_check.ps1 -Mode lint` 同口径：连测试目标一起查（I2b）。
            self.assertEqual(
                cargo.args[0],
                [
                    "cargo", "clippy", "--all-targets", "--locked",
                    "--target", windows_cross.TARGET, "--", "-D", "warnings",
                ],
            )
            self.assertEqual(cargo.kwargs["env"][windows_cross.LIBC_ENV], str(Path("/sdk/libc.txt")))
            self.assertEqual(cargo.kwargs["env"]["KEEP_ME"], "yes")
            self.assertNotIn(windows_cross.LIBC_ENV, os.environ)

    def test_license_acceptance_is_only_forwarded_when_explicit(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(windows_cross, "SDK_ROOT", Path(directory)), \
                    patch.object(windows_cross.shutil, "which", return_value="tool"), \
                    patch.object(windows_cross, "libc_contents", return_value="configuration"), \
                    patch.object(windows_cross.subprocess, "run") as run:
                for accepted in (False, True):
                    run.reset_mock()
                    windows_cross.setup(accepted)
                    command = run.call_args_list[0].args[0]
                    self.assertEqual("--accept-license" in command, accepted)
                    self.assertEqual(command[0], "xwin")
                    self.assertIn("--copy", command)


ROOT = Path(__file__).resolve().parent.parent


class WindowsCheckTests(unittest.TestCase):
    def setUp(self):
        self.shell = shutil.which("pwsh") or shutil.which("powershell")
        if not self.shell and os.name != "nt":
            self.skipTest("PowerShell harness is exercised by the Windows job")
        self.assertIsNotNone(self.shell, "PowerShell is required on Windows")
        temporary_root = ROOT / "target" / "tmp"
        temporary_root.mkdir(parents=True, exist_ok=True)
        directory = tempfile.TemporaryDirectory(prefix="windows-check-", dir=temporary_root)
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.sentinels = []
        for cache in (".zig-cache", "vendor/libghostty-vt/.zig-cache", "vendor/libghostty-vt/zig-out",
                      ".local/zig-cache/global", ".local/zig-cache/local"):
            sentinel = self.root / cache / "sentinel"
            sentinel.parent.mkdir(parents=True)
            sentinel.write_bytes(b"must remain unchanged\x00\xff")
            self.sentinels.append(sentinel)
        self.log = self.root / "commands.log"
        self.harness = self.root / "harness.ps1"
        self.harness.write_text(
            'function cargo {\n'
            '    [System.IO.File]::AppendAllText($env:FIXTURE_LOG, "cargo $($args -join \' \')`n")\n'
            '    if ($args[0] -eq $env:FIXTURE_FAIL) {\n'
            '        [Console]::Error.WriteLine($env:FIXTURE_DIAGNOSTIC)\n'
            '        $global:LASTEXITCODE = [int]$env:FIXTURE_CODE\n'
            '    } else { $global:LASTEXITCODE = 0 }\n'
            '}\n'
            'function just {\n'
            '    [System.IO.File]::AppendAllText($env:FIXTURE_LOG, "just $($args -join \' \')`n")\n'
            '    $global:LASTEXITCODE = 0\n'
            '}\n'
            '& $env:FIXTURE_SCRIPT -Mode $env:FIXTURE_MODE\n',
            encoding="utf-8",
        )

    def run_check(self, mode="check", failure="", code=0):
        parent = dict(os.environ)
        env = {**parent, "FIXTURE_LOG": str(self.log), "FIXTURE_FAIL": failure,
               "FIXTURE_CODE": str(code), "FIXTURE_DIAGNOSTIC": "HERDR_UNIQUE_CARGO_DIAGNOSTIC",
               "FIXTURE_SCRIPT": str(ROOT / "scripts/windows_check.ps1"), "FIXTURE_MODE": mode}
        result = subprocess.run([self.shell, "-NoProfile", "-NonInteractive", "-File", str(self.harness)],
                                cwd=self.root, env=env, capture_output=True, timeout=30)
        self.assertEqual(dict(os.environ), parent)
        commands = self.log.read_text(encoding="utf-8").splitlines()
        output = (result.stdout + result.stderr).decode("utf-8", errors="replace")
        return result, commands, output

    def assert_caches_unchanged(self):
        for sentinel in self.sentinels:
            with self.subTest(cache=sentinel.parent.relative_to(self.root)):
                self.assertTrue(sentinel.is_file())
                self.assertEqual(sentinel.read_bytes(), b"must remain unchanged\x00\xff")

    def test_clippy_failure_is_not_retried_and_preserves_diagnostic(self):
        result, commands, output = self.run_check(failure="clippy", code=101)
        self.assertEqual(result.returncode, 1, output)
        self.assertIn("command failed with exit code 101", output)
        self.assertEqual(output.count("HERDR_UNIQUE_CARGO_DIAGNOSTIC"), 1, output)
        self.assertEqual(commands, ["cargo fmt --check", "cargo clippy --all-targets --locked -- -D warnings"])
        self.assert_caches_unchanged()

    def test_clippy_failure_does_not_remove_any_cache(self):
        result, _, output = self.run_check(failure="clippy", code=101)
        self.assertEqual(result.returncode, 1, output)
        self.assert_caches_unchanged()

    def test_fmt_failure_stops_before_clippy(self):
        result, commands, output = self.run_check(failure="fmt", code=2)
        self.assertEqual(result.returncode, 1, output)
        self.assertIn("command failed with exit code 2", output)
        self.assertEqual(output.count("HERDR_UNIQUE_CARGO_DIAGNOSTIC"), 1, output)
        self.assertEqual(commands, ["cargo fmt --check"])
        self.assert_caches_unchanged()

    def test_lint_success_stops_after_clippy(self):
        result, commands, output = self.run_check(mode="lint")
        self.assertEqual(result.returncode, 0, output)
        self.assertEqual(commands, ["cargo fmt --check", "cargo clippy --all-targets --locked -- -D warnings"])
        self.assert_caches_unchanged()

    def test_check_success_runs_all_steps_in_order(self):
        result, commands, output = self.run_check()
        self.assertEqual(result.returncode, 0, output)
        self.assertEqual(commands, ["cargo fmt --check", "cargo clippy --all-targets --locked -- -D warnings",
                                    "just test", "cargo build --locked"])
        self.assert_caches_unchanged()


if __name__ == "__main__":
    unittest.main()
