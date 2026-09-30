from __future__ import annotations

from contextlib import contextmanager, ExitStack, nullcontext
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import call, patch

from scripts import gx_smoke_runtime as smoke


ROOT = Path(__file__).resolve().parent.parent
# 运行时的进程身份与回收只实现了 Linux（/proc + pidfd）与 Windows（Win32 句柄）：
# GX 只为这两类主机出包（release-channels.md），其他主机（macOS）跳过依赖它的用例。
SMOKE_HOST = os.name == "nt" or sys.platform.startswith("linux")
SMOKE_HOST_REASON = "GX smoke runtime supports only Linux and Windows hosts"


class IsolationTests(unittest.TestCase):
    def test_environment_drops_parent_targets_and_keeps_nesting_guard(self):
        root = Path(tempfile.gettempdir()) / "gx-isolation-test"
        inherited = {key: "parent" for key in smoke.INHERITED_KEYS}
        inherited.update({"HERDR_ENV": "1", "SYSTEMROOT": r"C:\Windows", "PATH": "keep"})
        env = smoke.isolated_env(root, "gx-smoke-test", inherited)
        self.assertEqual(env["HERDR_ENV"], "1")
        self.assertEqual(env["PATH"], "keep")
        self.assertEqual(env["HERDR_SESSION"], "gx-smoke-test")
        for key in ("HERDR_SOCKET_PATH", "HERDR_CLIENT_SOCKET_PATH", "HERDR_PANE_ID",
                    "HERDR_WORKSPACE_ID", "HERDR_TAB_ID", "HERDR_STARTUP_CWD"):
            self.assertNotIn(key, env)
        for key in ("HOME", "USERPROFILE", "LOCALAPPDATA", "APPDATA", "XDG_CONFIG_HOME",
                    "XDG_STATE_HOME", "XDG_RUNTIME_DIR", "HERDR_HOME", "HERDR_CONFIG_PATH"):
            self.assertTrue(Path(env[key]).is_relative_to(root), key)
        self.assertIn("allow_nested = true", smoke.CONFIG)
        self.assertIn("version_check = true", smoke.CONFIG)

    @unittest.skipUnless(SMOKE_HOST, SMOKE_HOST_REASON)
    def test_current_process_identity_and_reused_pid_are_distinguished(self):
        identity = smoke.process_identity(os.getpid())
        self.assertIsNotNone(identity)
        self.assertTrue(smoke.same_process(identity))
        self.assertFalse(smoke.same_process(identity + ":different-birth"))
        self.assertFalse(smoke.same_process(None))

    @unittest.skipUnless(SMOKE_HOST, SMOKE_HOST_REASON)
    def test_stale_sweep_preserves_live_and_unmarked_directories(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            stale, live, unknown = (base / name for name in ("run-stale", "run-live", "run-unknown"))
            for path in (stale, live, unknown):
                path.mkdir()
            for path, owner in ((stale, "99999999:gone"), (live, smoke.process_identity(os.getpid()))):
                (path / "owner.json").write_text(json.dumps({"schema": 1, "root": str(path), "owner": owner}))
            with patch.object(smoke, "reap_linux"):
                smoke.sweep_stale(base)
            self.assertFalse(stale.exists())
            self.assertTrue(live.exists())
            self.assertTrue(unknown.exists())

    def test_watchdog_rejects_cleanup_outside_its_sandbox(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaisesRegex(RuntimeError, "outside the smoke sandbox"):
                smoke.owned_root(Path(tmp))
            self.assertTrue(Path(tmp).exists())

    def test_every_command_selects_the_named_session_and_has_timeout(self):
        with patch.object(subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "ok", "")) as run:
            smoke.command("herdr", {"HERDR_SESSION": "gx-test"}, ROOT, "pane", "list")
        self.assertEqual(run.call_args.args[0], ["herdr", "--session", "gx-test", "pane", "list"])
        self.assertEqual(run.call_args.kwargs["timeout"], 20)

    def test_command_failure_is_not_swallowed(self):
        with patch.object(subprocess, "run", return_value=subprocess.CompletedProcess([], 1, "", "error")):
            with self.assertRaisesRegex(RuntimeError, "error"):
                smoke.command("herdr", {"HERDR_SESSION": "gx-test"}, ROOT, "pane", "list")

    def test_update_probes_require_the_full_package_identity(self):
        for manager in ("deb", "windows-installer"):
            smoke.require_package_version("herdr 0.9.1-gx." + manager + "." + "a" * 40, manager)
        for version in ("herdr 0.9.1", "herdr 0.9.1-gx.deb.abc", "herdr 0.9.1-gx.deb." + "a" * 40):
            with self.assertRaisesRegex(RuntimeError, "refusing update probes"):
                smoke.require_package_version(version, "windows-installer")


class LifecycleContractTests(unittest.TestCase):
    def test_windows_installer_is_independent_user_install_with_no_force_close(self):
        text = (ROOT / "packaging/windows/herdr-gx.iss").read_text()
        for token in ("AppId={{532CEFC3-E286-41A4-B097-631BD705DB76}",
                      "DefaultDirName={localappdata}\\Programs\\Herdr GX", "PrivilegesRequired=lowest",
                      "CloseApplications=no", "RestartApplications=no", "ChangesEnvironment=yes",
                      'Source: "{#StageDir}\\*"', "InitializeUninstall", "OccupiedMessage",
                      "RegQueryValueExW", "RegWriteExpandStringValue", "SendMessageTimeoutW",
                      "VersionInfoVersion={#PackageVersion}", "VersionInfoTextVersion={#PackageVersion}",
                      "VersionInfoProductVersion={#PackageVersion}", "Handle = THandle(-1)"):
            self.assertIn(token, text)
        for token in ("restartreplace", "uninsrestartdelete", "PrivilegesRequiredOverridesAllowed",
                      "[UninstallDelete]", "taskkill", "TerminateProcess", "INVALID_HANDLE_VALUE"):
            self.assertNotIn(token, text)

    def test_installer_preserves_raw_type_and_conservative_entry_ownership(self):
        text = (ROOT / "packaging/windows/herdr-gx.iss").read_text()
        self.assertIn("if not HasPath(Raw, Directory) then begin", text)
        self.assertIn("(not Removed) and (Parts[I] = Owned)", text)
        self.assertIn("if Kind = RegSz then", text)
        self.assertIn("'AddedPath'", text)
        self.assertIn("'PathExisted'", text)
        self.assertIn("PATH conflict:", text)
        self.assertIn("Shell aliases/functions may take precedence", text)

    def test_linux_shell_syntax(self):
        bash = shutil.which("bash")
        self.assertIsNotNone(bash, "bash is required for lifecycle script syntax validation")
        result = subprocess.run([bash, "-n", "scripts/gx_smoke_linux.sh"], cwd=ROOT, capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_linux_guard_precedes_installation_and_refuses_this_host(self):
        text = (ROOT / "scripts/gx_smoke_linux.sh").read_text()
        self.assertLess(text.index("HERDR_GX_DISPOSABLE"), text.index("useradd --"))
        self.assertIn("/.dockerenv", text)
        self.assertNotIn("--force-overwrite", text)
        self.assertIn("N/A previous-version upgrade", text)
        env = {**os.environ, "HERDR_GX_DISPOSABLE": "0"}
        result = subprocess.run([shutil.which("bash"), "scripts/gx_smoke_linux.sh", "missing.deb"], cwd=ROOT, env=env,
                                capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("Refusing installation smoke", result.stderr)

    def test_windows_parser_and_guard(self):
        shell = shutil.which("pwsh") or shutil.which("powershell")
        if not shell and os.name != "nt":
            self.skipTest("Windows parser is exercised by the Windows job")
        self.assertIsNotNone(shell, "PowerShell is required on Windows")
        parser = "$e=$null; $t=$null; [System.Management.Automation.Language.Parser]::ParseFile((Join-Path (Get-Location) 'scripts/gx_smoke_windows.ps1'),[ref]$t,[ref]$e) | Out-Null; if ($e.Count) { $e | Out-String | Write-Error; exit 1 }"
        result = subprocess.run([shell, "-NoProfile", "-NonInteractive", "-Command", parser], cwd=ROOT,
                                capture_output=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)
        env = {**os.environ, "HERDR_GX_DISPOSABLE": "0"}
        result = subprocess.run([shell, "-NoProfile", "-NonInteractive", "-File", "scripts/gx_smoke_windows.ps1",
                                 "-InstallerPath", "missing.exe", "-ExpectedVersion", "0.0.0"], cwd=ROOT,
                                env=env, capture_output=True, timeout=30)
        self.assertNotEqual(result.returncode, 0)
        self.assertIsInstance(result.stdout, bytes)
        self.assertIsInstance(result.stderr, bytes)
        self.assertIn(b"disposable", result.stdout + result.stderr)


class LinuxPidfdTests(unittest.TestCase):
    root = Path("/var/tmp/herdr-gx-pidfd-test")
    pid = 12345
    pidfd = 77

    @contextmanager
    def snapshot(self, identities=("12345:old", "12345:old"), running=(True, True)):
        events = []
        births = iter(identities)

        def open_pidfd(pid, flags):
            events.append("open")
            return self.pidfd

        def identity(pid):
            events.append("identity")
            return next(births)

        def environment():
            events.append("environment")
            return f"{smoke.ROOT_ENV}={self.root}".encode() + b"\0"

        with ExitStack() as stack:
            mocks = {}
            for name, target, attribute, kwargs in (
                ("open", smoke.os, "pidfd_open", {"side_effect": open_pidfd, "create": True}),
                ("send", smoke.signal, "pidfd_send_signal", {"create": True}),
                ("close", smoke.os, "close", {}),
                ("kill", smoke.os, "kill", {}),
                ("pid", smoke.os, "getpid", {"return_value": 99999}),
                ("uid", smoke.os, "getuid", {"return_value": 42, "create": True}),
                ("list", Path, "iterdir", {"return_value": [Path(f"/proc/{self.pid}")]}),
                ("stat", Path, "stat", {"return_value": SimpleNamespace(st_uid=42)}),
                ("read", Path, "read_bytes", {"side_effect": environment}),
                ("identity", smoke, "process_identity", {"side_effect": identity}),
                ("running", smoke, "pidfd_running", {"side_effect": running}),
            ):
                mocks[name] = stack.enter_context(patch.object(target, attribute, **kwargs))
            yield mocks, events
            mocks["kill"].assert_not_called()

    def test_pidfd_is_opened_before_birth_and_ownership_reads_and_closed_after_use(self):
        with self.snapshot() as (mocks, events):
            with smoke.owned_linux_processes(self.root) as descriptors:
                self.assertEqual(descriptors, [self.pidfd])
                mocks["close"].assert_not_called()
            mocks["open"].assert_called_once_with(self.pid, 0)
            self.assertEqual(events, ["open", "identity", "environment", "identity"])
            mocks["close"].assert_called_once_with(self.pidfd)

    def test_pid_reuse_between_ownership_read_and_second_birth_read_is_rejected(self):
        with self.snapshot(identities=("12345:old", "12345:new")) as (mocks, _):
            with smoke.owned_linux_processes(self.root) as descriptors:
                self.assertEqual(descriptors, [])
            mocks["send"].assert_not_called()
            mocks["close"].assert_called_once_with(self.pidfd)

    def test_dead_original_pidfd_rejects_replacement_with_consistent_proc_identity(self):
        with self.snapshot(identities=("12345:new", "12345:new"), running=(True, False)) as (mocks, _):
            with smoke.owned_linux_processes(self.root) as descriptors:
                self.assertEqual(descriptors, [])
            mocks["send"].assert_not_called()
            mocks["close"].assert_called_once_with(self.pidfd)

    def test_exited_pidfd_does_not_read_replacement_ownership(self):
        with self.snapshot(running=(False,)) as (mocks, _):
            with smoke.owned_linux_processes(self.root) as descriptors:
                self.assertEqual(descriptors, [])
            mocks["identity"].assert_not_called()
            mocks["read"].assert_not_called()
            mocks["close"].assert_called_once_with(self.pidfd)

    def test_uncertain_ownership_never_returns_a_signal_target(self):
        for failure in (PermissionError(), FileNotFoundError()):
            with self.subTest(failure=type(failure).__name__), self.snapshot() as (mocks, _):
                mocks["read"].side_effect = failure
                with smoke.owned_linux_processes(self.root) as descriptors:
                    self.assertEqual(descriptors, [])
                mocks["send"].assert_not_called()
                mocks["close"].assert_called_once_with(self.pidfd)

    def test_unowned_environment_and_uid_are_rejected(self):
        for mismatch in ("environment", "uid"):
            with self.subTest(mismatch=mismatch), self.snapshot() as (mocks, _):
                if mismatch == "environment":
                    mocks["read"].side_effect = None
                    mocks["read"].return_value = b"HERDR_GX_SMOKE_ROOT=/unrelated\0"
                else:
                    mocks["stat"].return_value = SimpleNamespace(st_uid=43)
                with smoke.owned_linux_processes(self.root) as descriptors:
                    self.assertEqual(descriptors, [])
                mocks["send"].assert_not_called()
                mocks["close"].assert_called_once_with(self.pidfd)

    def test_reused_pid_at_signal_time_only_addresses_the_original_pidfd(self):
        with patch.object(smoke, "owned_linux_processes", side_effect=lambda root: nullcontext([self.pidfd])), \
             patch.object(smoke, "linux_processes_remaining", return_value=False), \
             patch.object(smoke.signal, "pidfd_send_signal", side_effect=ProcessLookupError, create=True) as send, \
             patch.object(smoke.signal, "SIGKILL", 9, create=True), \
             patch.object(smoke.os, "kill") as kill:
            smoke.reap_linux(self.root)
        self.assertEqual(send.call_args_list, [call(self.pidfd, signal.SIGTERM), call(self.pidfd, 9)])
        kill.assert_not_called()

    def test_missing_pidfd_support_fails_closed_without_pid_fallback(self):
        with patch.object(smoke.os, "pidfd_open", None, create=True), patch.object(smoke.os, "kill") as kill:
            with self.assertRaisesRegex(RuntimeError, "PID-based fallback is prohibited"):
                with smoke.owned_linux_processes(self.root):
                    self.fail("unsupported pidfd API must not enter the cleanup context")
        kill.assert_not_called()

    def test_owned_descriptors_close_when_cleanup_body_fails(self):
        with self.snapshot() as (mocks, _):
            with self.assertRaisesRegex(RuntimeError, "probe failure"):
                with smoke.owned_linux_processes(self.root):
                    raise RuntimeError("probe failure")
            mocks["close"].assert_called_once_with(self.pidfd)


@unittest.skipUnless(SMOKE_HOST, SMOKE_HOST_REASON)
class WatchdogTests(unittest.TestCase):
    def test_detached_guard_preserves_worker_failure_diagnostics(self):
        result = subprocess.run(
            [sys.executable, str(ROOT / "scripts/gx_smoke_runtime.py"), "--binary", sys.executable],
            stdin=subprocess.DEVNULL, capture_output=True, timeout=30,
        )
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn(b"herdr --version failed", result.stderr)

    def test_owner_killed_mid_probe_leaves_no_owned_processes(self):
        options = {} if os.name == "nt" else {"start_new_session": True}
        parent = subprocess.Popen([sys.executable, str(ROOT / "scripts/gx_smoke_runtime.py"), "--reaper-probe"],
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, **options)
        owner = smoke.process_identity(parent.pid)
        root = None
        identities = []
        try:
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline:
                for candidate in smoke.sandbox_base().glob("run-*"):
                    try:
                        info = json.loads((candidate / "owner.json").read_text())
                        if info.get("owner") == owner and (candidate / "probe-ready").exists():
                            root = candidate
                            identities = json.loads((candidate / "probe-ready").read_text())
                            break
                    except (FileNotFoundError, json.JSONDecodeError):
                        pass
                if root or parent.poll() is not None:
                    break
                time.sleep(0.05)
            self.assertIsNotNone(root, f"probe not ready, parent exit={parent.poll()}")
            self.assertTrue(all(smoke.same_process(item) for item in identities))
            if os.name == "nt":
                parent.kill()
            else:
                os.killpg(parent.pid, signal.SIGKILL)
            parent.wait(timeout=5)
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline and (root.exists() or any(smoke.same_process(item) for item in identities)):
                time.sleep(0.1)
            self.assertFalse(root.exists(), f"watchdog left sandbox {root}")
            self.assertFalse(any(smoke.same_process(item) for item in identities), "watchdog left owned Python processes")
        finally:
            if parent.poll() is None:
                parent.kill()
            parent.wait(timeout=5)


if __name__ == "__main__":
    unittest.main()
