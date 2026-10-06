from __future__ import annotations

import builtins
import importlib.util
import json
import os
import signal
import subprocess
import sys
import tempfile
import textwrap
import threading
import time
import unittest
from pathlib import Path
from unittest import mock

from scripts import run_nextest


def listing(names: list[str], selected: list[str] | None = None) -> dict:
    selected = names if selected is None else selected
    return {
        "test-count": len(names),
        "rust-suites": {"package::bin/test": {"testcases": {
            name: {"ignored": False, "filter-match": {"status": "matches" if name in selected else "mismatch"}}
            for name in names
        }}},
    }


class SimulatedRun(run_nextest.NextestRun):
    def __init__(self, root: Path, shards=2, names=None, parts=None):
        super().__init__(8, shards, "test(probe)", root=root,
                         environment={"NEXTEST_USER_CONFIG_FILE": "none", "CUSTOM_TEST_ENV": "kept"})
        self.names = ["a", "b"] if names is None else names
        self.parts = [["a"], ["b"]] if parts is None else parts
        self.calls = []
        self.codes = {}
        self.observations = []

    def command(self, name, command, *, output=None, record=None):
        self.calls.append((name, command, record.get("invocation_id") if record is not None else None))
        if output is not None:
            payload = listing(self.names, self.parts[int(name.split("-")[1]) - 1]) if name.endswith("-list") else listing(self.names)
            output.write_text(json.dumps(payload), encoding="utf-8")
        if record is not None:
            self.observations.append(json.loads(self.manifest_path.read_text(encoding="utf-8")))
            with self.lock:
                record.update(status="complete", exit_code=self.codes.get(name, 0))
        return self.codes.get(name, 0)


class ConfigurationTests(unittest.TestCase):
    def test_allocations_preserve_total_budget(self):
        for shards, expected in ((1, [8]), (2, [4, 4]), (4, [2, 2, 2, 2])):
            self.assertEqual(run_nextest.allocation(8, shards), expected)
        self.assertEqual(run_nextest.allocation("7", "4"), [2, 2, 2, 1])

    def test_invalid_budget_and_shard_values_fail(self):
        for value in ("", "0", "-1", "1.5", "num-cpus", " 4", "+2", "４", True, None):
            for args in ((value, 1), (8, value)):
                with self.subTest(args=args), self.assertRaises(ValueError):
                    run_nextest.allocation(*args)
        with self.assertRaises(ValueError):
            run_nextest.allocation(2, 4)

    def test_default_command_is_the_original_recipe(self):
        self.assertEqual(run_nextest.single_command(4, None), [
            "cargo", "nextest", "run", "--locked", "--test-threads", "4",
            "--status-level", "leak", "--final-status-level", "fail",
            "--failure-output", "final", "--success-output", "never",
        ])
        self.assertEqual(run_nextest.single_command(4, "test(probe)")[-2:], ["--filterset", "test(probe)"])

    def test_main_defaults_to_one_shard_and_cli_budget_wins(self):
        with mock.patch.dict(os.environ, {"HERDR_NEXTEST_JOBS": "8"}, clear=True), mock.patch.object(run_nextest, "NextestRun") as runner:
            runner.return_value.run.return_value = 0
            self.assertEqual(run_nextest.main([]), 0)
            runner.assert_called_with(8, 1, None)
            self.assertEqual(run_nextest.main(["--test-threads", "4"]), 0)
            runner.assert_called_with(4, 1, None)

    def test_main_does_not_accept_unsafe_passthrough_arguments(self):
        with self.assertRaises(SystemExit):
            run_nextest.main(["--partition", "hash:1/4"])

    def test_shard_config_rejects_cross_runner_constraints(self):
        configs = [
            "[store]\ndir='somewhere'", "[test-groups.serial]\nmax-threads=1",
            "[script.setup]\ncommand='setup'", "experimental=['setup-scripts']",
            "[profile.default]\nthreads-required=2", "[profile.default]\ntest-threads=4",
            "[[profile.default.overrides]]\nfilter='all()'\ntest-group='serial'",
        ]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / ".config").mkdir()
            path = root / ".config/nextest.toml"
            for content in configs:
                path.write_text(content, encoding="utf-8")
                with self.subTest(content=content), self.assertRaisesRegex(ValueError, "SHARDS=1"):
                    run_nextest.check_shard_config(root, {"NEXTEST_USER_CONFIG_FILE": "none"})

    def test_profile_run_id_recording_and_junit_guards(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for environment in ({"NEXTEST_PROFILE": "timing"}, {"NEXTEST_RUN_ID": "fixed"},
                                {"NEXTEST_EXPERIMENTAL_RECORD": "1"}, {"NEXTEST_RECORD": "1"}):
                with self.subTest(environment=environment), self.assertRaises(ValueError):
                    run_nextest.check_shard_config(root, environment)
            (root / ".config").mkdir()
            path = root / ".config/nextest.toml"
            for junit in ("/absolute.xml", "C:/absolute.xml", "../outside.xml", "C:relative.xml"):
                path.write_text("[profile.default.junit]\npath=" + json.dumps(junit), encoding="utf-8")
                with self.subTest(junit=junit), self.assertRaises(ValueError):
                    run_nextest.check_shard_config(root, {"NEXTEST_USER_CONFIG_FILE": "none"})
            path.write_text("[profile.default.junit]\npath='junit.xml'\n[profile.timing]\ntest-threads=1", encoding="utf-8")
            run_nextest.check_shard_config(root, {"NEXTEST_USER_CONFIG_FILE": "none"})

    def test_user_display_config_is_not_dropped_but_recording_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            user = root / "user.toml"
            environment = {"NEXTEST_USER_CONFIG_FILE": str(user), "CUSTOM_TEST_ENV": "kept"}
            user.write_text("[ui]\nshow-progress='none'", encoding="utf-8")
            run_nextest.check_shard_config(root, environment)
            run = run_nextest.NextestRun(4, 2, root=root, environment=environment)
            self.assertEqual(run.environment, environment)
            user.write_text("[record]\nenabled=true", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "recording"):
                run_nextest.check_shard_config(root, environment)


class CoverageTests(unittest.TestCase):
    def test_lists_distinguish_suite_identity_and_ignored_tests(self):
        data = listing(["same"])
        data["rust-suites"]["other"] = {"testcases": {"same": {"ignored": False, "filter-match": {"status": "matches"}},
                                                    "ignored": {"ignored": True, "filter-match": {"status": "mismatch"}}}}
        data["test-count"] = 3
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "list.json"
            path.write_text(json.dumps(data), encoding="utf-8")
            self.assertEqual(run_nextest.selected_tests(path), {("package::bin/test", "same"), ("other", "same")})

    def test_malformed_unknown_duplicate_and_truncated_lists_fail(self):
        invalid = ["{}", '{"rust-suites": {}, "rust-suites": {}, "test-count": 0}', '{"test-count":']
        data = listing(["a"])
        data["test-count"] = 2
        invalid.append(json.dumps(data))
        data = listing(["a"])
        data["rust-suites"]["package::bin/test"]["testcases"]["a"]["filter-match"]["status"] = "unknown"
        invalid.append(json.dumps(data))
        data = listing(["a"])
        data["rust-suites"]["package::bin/test"]["testcases"]["a"]["ignored"] = True
        invalid.append(json.dumps(data))
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "list.json"
            for content in invalid:
                path.write_text(content, encoding="utf-8")
                with self.subTest(content=content), self.assertRaises(ValueError):
                    run_nextest.selected_tests(path)

    def test_coverage_requires_disjoint_exact_union(self):
        run_nextest.verify_coverage({"a", "b"}, [{"a"}, {"b"}, set()])
        for partitions in ([{"a"}, {"a", "b"}], [{"a"}, set()], [{"a", "b"}, {"c"}]):
            with self.subTest(partitions=partitions), self.assertRaises(ValueError):
                run_nextest.verify_coverage({"a", "b"}, partitions)


class OrchestrationTests(unittest.TestCase):
    def test_compile_once_reuse_has_no_build_options_and_stores_are_independent(self):
        with tempfile.TemporaryDirectory() as temporary:
            run = SimulatedRun(Path(temporary))
            self.assertEqual(run.run(), 0)
            calls = dict((name, args) for name, args, _ in run.calls)
            self.assertEqual(calls["cargo-metadata"], ["cargo", "metadata", "--locked", "--offline", "--format-version", "1"])
            self.assertEqual(calls["binaries-metadata"], ["cargo", "nextest", "list", "--locked", "--list-type", "binaries-only", "--message-format", "json"])
            for name, args in calls.items():
                if name not in {"cargo-metadata", "binaries-metadata"}:
                    self.assertIn("--cargo-metadata", args)
                    self.assertIn("--binaries-metadata", args)
                    self.assertNotIn("--locked", args)
                    self.assertNotIn("--bin", args)
                    self.assertNotIn("--user-config-file", args)
                    self.assertNotIn("--message-format", args if name in {"shard-1", "shard-2"} else [])
            records = run.manifest["runners"]
            self.assertEqual(sum(record["workers"] for record in records), 8)
            self.assertEqual(len({record["invocation_id"] for record in records}), 2)
            for record in records:
                self.assertNotIn("run_id", record)
                self.assertNotIn("nextest_run_id", record)
            self.assertEqual(len({record["store"] for record in records}), 2)
            self.assertTrue(all(Path(record["tool_config"]).is_file() for record in records))
            self.assertEqual(run.manifest["expected_counts"], [1, 1])
            self.assertEqual(run.manifest["status"], "passed")
            self.assertEqual(run.manifest["coverage"]["count"], 2)
            self.assertFalse(list(run.directory.glob(".*.tmp")))

    def test_shard_invocation_id_is_not_injected_into_nextest_environment(self):
        class EnvironmentRun(SimulatedRun):
            def command(self, name, command, **kwargs):
                if name in {"shard-1", "shard-2"}:
                    probe = "import os; assert 'NEXTEST_RUN_ID' not in os.environ; print(os.environ['CUSTOM_TEST_ENV'])"
                    return run_nextest.NextestRun.command(self, name, [sys.executable, "-c", probe], **kwargs)
                return super().command(name, command, **kwargs)

        with tempfile.TemporaryDirectory() as temporary:
            run = EnvironmentRun(Path(temporary))
            with mock.patch.object(run_nextest, "OwnedProcess", wraps=run_nextest.OwnedProcess) as owner:
                self.assertEqual(run.run(), 0)
            self.assertEqual(owner.call_count, 2)
            for call in owner.call_args_list:
                self.assertEqual(call.kwargs["env"], run.environment)
            manifest = json.loads(run.manifest_path.read_text(encoding="utf-8"))
            for record in manifest["runners"]:
                self.assertIn("invocation_id", record)
                self.assertNotIn("run_id", record)
                self.assertNotIn("nextest_run_id", record)
                self.assertEqual(Path(record["log"]).read_text(encoding="utf-8").strip(), "kept")

    def test_empty_shard_is_idle_and_never_launched(self):
        with tempfile.TemporaryDirectory() as temporary:
            run = SimulatedRun(Path(temporary), parts=[["a", "b"], []])
            self.assertEqual(run.run(), 0)
            self.assertEqual(run.manifest["runners"][1]["status"], "idle")
            self.assertIsNone(run.manifest["runners"][1]["exit_code"])
            self.assertNotIn("shard-2", [name for name, _, _ in run.calls])

    def test_fully_empty_selection_is_not_success(self):
        with tempfile.TemporaryDirectory() as temporary:
            run = SimulatedRun(Path(temporary), names=[], parts=[[], []])
            with self.assertRaisesRegex(ValueError, "no tests"):
                run.run()
            self.assertEqual(run.manifest["status"], "failed")
            self.assertNotIn("shard-1", [name for name, _, _ in run.calls])

    def test_missing_failed_and_unstarted_runners_cannot_pass(self):
        with tempfile.TemporaryDirectory() as temporary:
            for outcome in (None, 7, "missing"):
                run = SimulatedRun(Path(temporary))
                if outcome == "missing":
                    original = run.command
                    def omit(name, command, **kwargs):
                        return 0 if name == "shard-2" else original(name, command, **kwargs)
                    run.command = omit
                else:
                    run.codes["shard-2"] = outcome
                self.assertNotEqual(run.run(), 0)
                self.assertEqual(run.manifest["status"], "failed")

    def test_preparation_failure_is_manifested_before_any_runner(self):
        with tempfile.TemporaryDirectory() as temporary:
            run = SimulatedRun(Path(temporary), parts=[["a"], ["a"]])
            with self.assertRaises(ValueError):
                run.run()
            self.assertEqual(run.manifest["status"], "failed")
            self.assertNotIn("shard-1", [name for name, _, _ in run.calls])

    def test_single_runner_does_not_validate_or_prepare_sharding(self):
        with tempfile.TemporaryDirectory() as temporary:
            run = SimulatedRun(Path(temporary), shards=1)
            run.environment.update(NEXTEST_PROFILE="custom", NEXTEST_RUN_ID="inherited")
            with mock.patch.object(run_nextest, "check_shard_config", side_effect=AssertionError("must not run")):
                self.assertEqual(run.run(), 0)
            self.assertEqual([name for name, _, _ in run.calls], ["single"])
            self.assertEqual(run.calls[0][1], run_nextest.single_command(8, "test(probe)"))

    def test_atomic_failure_cannot_report_success(self):
        with tempfile.TemporaryDirectory() as temporary:
            run = SimulatedRun(Path(temporary))
            with mock.patch.object(run_nextest, "_atomic_write_json", side_effect=OSError("disk full")):
                with self.assertRaises(OSError):
                    run.run()
            self.assertEqual(run.manifest["status"], "failed")


class ProcessTests(unittest.TestCase):
    def make_run(self, root):
        return run_nextest.NextestRun(4, 1, root=root)

    def test_actual_child_output_exit_and_environment(self):
        with tempfile.TemporaryDirectory() as temporary:
            run = self.make_run(Path(temporary))
            run.environment["CUSTOM_TEST_ENV"] = "kept"
            code = run.command("probe", [sys.executable, "-c", "import os,sys; print(os.environ['CUSTOM_TEST_ENV']); sys.exit(7)"])
            self.assertEqual(code, 7)
            self.assertIn("kept", (run.directory / "probe.log").read_text(encoding="utf-8"))
            self.assertEqual(run.manifest["status"], "failed")
            self.assertFalse(run.processes)

    def test_start_failure_has_no_fake_exit_code(self):
        with tempfile.TemporaryDirectory() as temporary:
            run = self.make_run(Path(temporary))
            with self.assertRaises(OSError):
                run.command("missing", ["definitely-missing-nextest-program"])
            record = run.manifest["commands"]["missing"]
            self.assertEqual(record["status"], "failed")
            self.assertIsNone(record["exit_code"])

    def test_log_is_visible_while_owned_process_is_running(self):
        with tempfile.TemporaryDirectory() as temporary:
            run = self.make_run(Path(temporary))
            release = Path(temporary) / "release"
            command = [sys.executable, "-c", "import pathlib,time; print('live-output', flush=True); p=pathlib.Path(" + repr(str(release)) + ");\nwhile not p.exists(): time.sleep(.02)"]
            results, errors = [], []
            def launch():
                try:
                    results.append(run.command("live", command))
                except BaseException as error:
                    errors.append(error)
            worker = threading.Thread(target=launch)
            worker.start()
            try:
                deadline = time.monotonic() + 10
                path = run.directory / "live.log"
                while time.monotonic() < deadline:
                    if path.exists() and "live-output" in path.read_text(encoding="utf-8"):
                        break
                    time.sleep(.02)
                self.assertTrue(worker.is_alive())
                manifest = json.loads(run.manifest_path.read_text(encoding="utf-8"))
                self.assertEqual(manifest["commands"]["live"]["status"], "running")
                self.assertIn("live-output", path.read_text(encoding="utf-8"))
            finally:
                release.touch()
                worker.join(15)
                if worker.is_alive():
                    run.cancel()
                    worker.join(10)
            self.assertFalse(errors)
            self.assertEqual(results, [0])

    def test_output_failure_and_keyboard_cancel_clean_owned_children(self):
        for error in (OSError("output broken"), KeyboardInterrupt()):
            with self.subTest(error=type(error).__name__), tempfile.TemporaryDirectory() as temporary:
                run = self.make_run(Path(temporary))
                output = mock.Mock()
                output.write.side_effect = error
                with mock.patch.object(run_nextest.sys, "stdout", output):
                    with self.assertRaises(type(error)):
                        run.command("broken", [sys.executable, "-c", "import signal,sys,time; signal.signal(signal.SIGTERM, lambda *_: sys.exit(130)); print('ready', flush=True); time.sleep(60)"])
                manifest = json.loads(run.manifest_path.read_text(encoding="utf-8"))
                self.assertEqual(manifest["status"], "failed")
                self.assertEqual(manifest["commands"]["broken"]["status"], "failed")
                self.assertFalse(run.processes)
                self.assertTrue(run.cancelled.is_set())

    def test_log_open_failure_is_fatal(self):
        with tempfile.TemporaryDirectory() as temporary:
            run = self.make_run(Path(temporary))
            (run.directory / "unwritable.log").mkdir()
            with self.assertRaises(OSError):
                run.command("unwritable", [sys.executable, "-c", "pass"])
            self.assertEqual(run.manifest["status"], "failed")
            self.assertFalse(run.processes)

    def test_each_invocation_uses_a_separate_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            first = self.make_run(Path(temporary))
            second = self.make_run(Path(temporary))
            self.assertNotEqual(first.directory, second.directory)
            self.assertTrue(first.directory.name.startswith("nextest-"))
            self.assertTrue(second.directory.name.startswith("nextest-"))


class FailureContractTests(unittest.TestCase):
    def test_main_preserves_empty_selection_failure_code(self):
        with mock.patch.dict(os.environ, {}, clear=True), mock.patch.object(run_nextest, "NextestRun") as runner:
            runner.return_value.run.side_effect = run_nextest.NoTestsError("no tests")
            self.assertEqual(run_nextest.main([]), 4)

    def test_missing_single_result_is_not_success(self):
        with tempfile.TemporaryDirectory() as temporary:
            run = SimulatedRun(Path(temporary), shards=1)
            run.codes["single"] = None
            self.assertNotEqual(run.run(), 0)
            self.assertEqual(run.manifest["status"], "failed")

    def test_io_error_after_spawn_marks_failure_before_cleanup(self):
        with tempfile.TemporaryDirectory() as temporary:
            run = run_nextest.NextestRun(4, 1, root=Path(temporary))
            original_save, original_stop = run.save, run_nextest.OwnedProcess.stop
            failed_once, cleanup_states = [], []
            def save():
                if not failed_once and run.manifest["commands"].get("probe", {}).get("status") == "running":
                    failed_once.append(True)
                    raise OSError("manifest write failed after spawn")
                original_save()
            def stop(owner):
                cleanup_states.append(json.loads(run.manifest_path.read_text(encoding="utf-8"))["status"])
                original_stop(owner)
            with mock.patch.object(run, "save", side_effect=save), mock.patch.object(run_nextest.OwnedProcess, "stop", stop):
                with self.assertRaises(OSError):
                    run.command("probe", [sys.executable, "-c", "import time; time.sleep(60)"])
            self.assertEqual(cleanup_states, ["failed"])
            self.assertIn("manifest write", " ".join(run.manifest["failures"]))
            self.assertFalse(run.processes)

    def test_cancellation_stops_all_parallel_runners(self):
        class ChildRun(SimulatedRun):
            def command(self, name, command, **kwargs):
                if name in {"shard-1", "shard-2"}:
                    probe = "import signal,sys,time; signal.signal(signal.SIGTERM, lambda *_: sys.exit(130)); print('ready', flush=True); time.sleep(60)"
                    return run_nextest.NextestRun.command(self, name, [sys.executable, "-c", probe], **kwargs)
                return super().command(name, command, **kwargs)
        with tempfile.TemporaryDirectory() as temporary:
            run = ChildRun(Path(temporary))
            def interrupt(futures):
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline:
                    with run.lock:
                        logs = [run.directory / f"shard-{index}.log" for index in (1, 2)]
                        if len(run.processes) == 2 and all(path.is_file() and "ready" in path.read_text(encoding="utf-8") for path in logs):
                            break
                    time.sleep(.01)
                else:
                    self.fail("parallel runners did not start")
                raise KeyboardInterrupt()
            with mock.patch.object(run_nextest.concurrent.futures, "as_completed", side_effect=interrupt):
                with self.assertRaises(KeyboardInterrupt):
                    run.run()
            self.assertEqual(run.manifest["status"], "failed")
            self.assertFalse(run.processes)
            self.assertTrue(run.cancelled.is_set())

    def test_output_failure_reclaims_owned_grandchild(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            heartbeat = root / "heartbeat"
            child = "import pathlib,time; p=pathlib.Path(" + repr(str(heartbeat)) + ");\nwhile True:\n p.write_text(str(time.time())); time.sleep(.02)"
            parent = (
                "import pathlib,subprocess,sys,time; subprocess.Popen([sys.executable,'-c'," + repr(child) + "]); "
                "p=pathlib.Path(" + repr(str(heartbeat)) + "); deadline=time.monotonic()+8\n"
                "while not p.exists() and time.monotonic()<deadline: time.sleep(.02)\n"
                "print('child-started', flush=True); time.sleep(60)"
            )
            run = run_nextest.NextestRun(4, 1, root=root)
            output = mock.Mock()
            output.write.side_effect = OSError("output broken")
            with mock.patch.object(run_nextest.sys, "stdout", output):
                with self.assertRaises(OSError):
                    run.command("tree", [sys.executable, "-c", parent])
            self.assertTrue(heartbeat.is_file())
            before = heartbeat.read_text(encoding="utf-8")
            time.sleep(.2)
            self.assertEqual(heartbeat.read_text(encoding="utf-8"), before)
            self.assertEqual(run.manifest["status"], "failed")


class CompatibilityAndRecordingTests(unittest.TestCase):
    def test_nested_user_recording_is_rejected_including_platform_overrides(self):
        configurations = [
            "[experimental]\nrecord=true",
            "[[overrides]]\nplatform='cfg(windows)'\nrecord.enabled=true",
            "[[overrides]]\nplatform='cfg(unix)'\nrecord.enabled=true",
            "[record]\nenabled=false",
            "[experimental]\nrecord=false",
            "[nested.deeper.record]\nenabled=true",
        ]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            user = root / "user.toml"
            environment = {"NEXTEST_USER_CONFIG_FILE": str(user)}
            for content in configurations:
                with self.subTest(content=content):
                    user.write_text(content, encoding="utf-8")
                    with self.assertRaisesRegex(ValueError, "recording.*SHARDS=1"):
                        run_nextest.check_shard_config(root, environment)
            user.write_text("[ui]\nshow-progress='none'\n[[overrides]]\nplatform='cfg(windows)'\nui.show-progress='bar'", encoding="utf-8")
            run_nextest.check_shard_config(root, environment)

    def test_tomli_fallback_when_tomllib_is_unavailable(self):
        original_import = builtins.__import__
        fallback = mock.Mock()
        fallback.loads.side_effect = run_nextest.tomllib.loads
        imports = []
        def import_module(name, *args, **kwargs):
            if name == "tomllib":
                raise ModuleNotFoundError("simulated Python 3.10", name="tomllib")
            if name == "tomli":
                imports.append(name)
                return fallback
            return original_import(name, *args, **kwargs)
        spec = importlib.util.spec_from_file_location("scripts._run_nextest_tomli_probe", run_nextest.__file__)
        module = importlib.util.module_from_spec(spec)
        with mock.patch.object(builtins, "__import__", side_effect=import_module):
            spec.loader.exec_module(module)
        self.assertEqual(imports, ["tomli"])
        self.assertIs(module.tomllib, fallback)
        self.assertEqual(module.single_command(4, None), run_nextest.single_command(4, None))
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            user = root / "user.toml"
            user.write_text("[ui]\nshow-progress='none'", encoding="utf-8")
            module.check_shard_config(root, {"NEXTEST_USER_CONFIG_FILE": str(user)})
        fallback.loads.assert_called_once()


class PosixShutdownContractTests(unittest.TestCase):
    def owner(self, results, events):
        owner = run_nextest.OwnedProcess.__new__(run_nextest.OwnedProcess)
        owner.job = None
        owner._stopped = False
        owner._stop_error = None
        owner.process = mock.Mock(pid=4242)
        owner.process.poll.return_value = None
        pending = iter(results)
        def wait(*, timeout):
            events.append(("wait", timeout))
            result = next(pending)
            if isinstance(result, BaseException):
                raise result
            owner.process.poll.return_value = result
            return result
        owner.process.wait.side_effect = wait
        return owner

    def test_mocked_posix_graceful_signal_precedes_wait_and_stop_is_idempotent(self):
        events = []
        owner = self.owner([130], events)
        with mock.patch.object(run_nextest.os, "killpg", create=True, side_effect=lambda pid, sig: events.append(("signal", sig))):
            owner.stop()
            owner.stop()
        self.assertEqual(events, [("signal", signal.SIGTERM), ("wait", 15)])

    def test_mocked_posix_second_signal_forces_tests_before_killing_runner(self):
        events = []
        owner = self.owner([subprocess.TimeoutExpired("runner", 15), 130], events)
        with mock.patch.object(run_nextest.os, "killpg", create=True, side_effect=lambda pid, sig: events.append(("signal", sig))):
            with self.assertRaisesRegex(OSError, "second cancellation.*forced"):
                owner.stop()
            with self.assertRaisesRegex(OSError, "cleanup incomplete"):
                owner.stop()
        self.assertEqual(events, [("signal", signal.SIGTERM), ("wait", 15), ("signal", signal.SIGTERM), ("wait", 10)])

    def test_mocked_posix_last_resort_kill_never_claims_descendant_cleanup(self):
        events = []
        timeout = subprocess.TimeoutExpired("runner", 10)
        owner = self.owner([timeout, timeout, -9], events)
        with mock.patch.object(run_nextest.os, "killpg", create=True, side_effect=lambda pid, sig: events.append(("signal", sig))), mock.patch.object(run_nextest.signal, "SIGKILL", 9, create=True):
            with self.assertRaisesRegex(OSError, "separate test groups may remain.*unverified"):
                owner.stop()
        self.assertEqual(events, [("signal", signal.SIGTERM), ("wait", 15), ("signal", signal.SIGTERM), ("wait", 10), ("signal", 9), ("wait", 10)])

    def test_mocked_posix_missing_runner_and_signal_exit_are_unverified(self):
        owner = self.owner([], [])
        owner.process.poll.return_value = 130
        with mock.patch.object(run_nextest.os, "killpg", create=True) as send:
            with self.assertRaisesRegex(OSError, "unverified"):
                owner.stop()
            send.assert_not_called()
        owner = self.owner([-signal.SIGTERM], [])
        with mock.patch.object(run_nextest.os, "killpg", create=True):
            with self.assertRaisesRegex(OSError, "cancellation handler.*unverified"):
                owner.stop()

    def test_cleanup_failure_is_persisted_not_only_raised(self):
        with tempfile.TemporaryDirectory() as temporary:
            run = run_nextest.NextestRun(4, 1, root=Path(temporary))
            owner = mock.Mock()
            owner.stop.side_effect = OSError("forced cleanup unverified")
            run.processes.append(owner)
            with self.assertRaisesRegex(OSError, "forced cleanup unverified"):
                run.cancel()
            manifest = json.loads(run.manifest_path.read_text(encoding="utf-8"))
            self.assertEqual(manifest["status"], "failed")
            self.assertIn("forced cleanup unverified", manifest["failures"][0])

    def test_interrupted_grace_wait_escalates_before_caching_stopped(self):
        events = []
        owner = self.owner([KeyboardInterrupt("second interrupt"), 130], events)
        with mock.patch.object(run_nextest.os, "killpg", create=True, side_effect=lambda pid, sig: events.append(("signal", sig))):
            with self.assertRaisesRegex(OSError, "second cancellation.*forced"):
                owner.stop()
        self.assertEqual(events, [("signal", signal.SIGTERM), ("wait", 15), ("signal", signal.SIGTERM), ("wait", 10)])
        self.assertTrue(owner._stopped)

    def test_interrupted_escalation_wait_still_reaps_runner(self):
        events = []
        owner = self.owner([KeyboardInterrupt(), KeyboardInterrupt(), -9], events)
        with mock.patch.object(run_nextest.os, "killpg", create=True, side_effect=lambda pid, sig: events.append(("signal", sig))), mock.patch.object(run_nextest.signal, "SIGKILL", 9, create=True):
            with self.assertRaisesRegex(OSError, "force-killed.*unverified"):
                owner.stop()
        self.assertEqual(events, [("signal", signal.SIGTERM), ("wait", 15), ("signal", signal.SIGTERM), ("wait", 10), ("signal", 9), ("wait", 10)])

    def test_interrupted_signal_does_not_cache_an_unfinished_stop(self):
        owner = self.owner([130], [])
        with mock.patch.object(run_nextest.os, "killpg", create=True, side_effect=[KeyboardInterrupt(), None]) as send:
            with self.assertRaises(KeyboardInterrupt):
                owner.stop()
            self.assertFalse(owner._stopped)
            owner.stop()
            owner.stop()
        self.assertEqual(send.call_count, 2)
        self.assertIsNone(owner._stop_error)

    def test_cancel_retries_interrupted_owner_and_continues_other_owners(self):
        with tempfile.TemporaryDirectory() as temporary:
            run = run_nextest.NextestRun(4, 2, root=Path(temporary))
            first, second = mock.Mock(), mock.Mock()
            first.stop.side_effect = [KeyboardInterrupt("again"), None]
            run.processes.extend([first, second])
            with self.assertRaisesRegex(OSError, "interrupted"):
                run.cancel()
            self.assertEqual(first.stop.call_count, 2)
            second.stop.assert_called_once()
            manifest = json.loads(run.manifest_path.read_text(encoding="utf-8"))
            self.assertEqual(manifest["status"], "failed")

    def test_main_defers_repeated_signals_until_cleanup_finishes(self):
        previous = {sig: signal.getsignal(sig) for sig in (signal.SIGINT, signal.SIGTERM)}
        run = mock.Mock(cancelled=threading.Event())
        completed = []
        def execute():
            interrupt = signal.getsignal(signal.SIGINT)
            terminate = signal.getsignal(signal.SIGTERM)
            with self.assertRaises(KeyboardInterrupt):
                interrupt(signal.SIGINT, None)
            run.cancelled.set()
            interrupt(signal.SIGINT, None)
            terminate(signal.SIGTERM, None)
            completed.append(True)
            raise KeyboardInterrupt("original cancellation")
        run.run.side_effect = execute
        with mock.patch.dict(os.environ, {}, clear=True), mock.patch.object(run_nextest, "NextestRun", return_value=run):
            self.assertEqual(run_nextest.main([]), 130)
        self.assertEqual(completed, [True])
        for sig, handler in previous.items():
            self.assertIs(signal.getsignal(sig), handler)

    @unittest.skipUnless(os.name == "posix", "requires a real POSIX host and independent process groups")
    def test_real_posix_runner_forwards_shutdown_to_independent_child_group(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            child_code = textwrap.dedent(f"""
                import os, pathlib, signal, sys, time
                root = pathlib.Path({str(root)!r})
                parent = os.getppid()
                def stop(signum, frame):
                    (root / 'child-signal').write_text(str(signum))
                    sys.exit(0)
                signal.signal(signal.SIGTERM, stop)
                (root / 'child-ready').touch()
                deadline = time.monotonic() + 10
                try:
                    while os.getppid() == parent and time.monotonic() < deadline:
                        time.sleep(.01)
                finally:
                    (root / 'child-done').touch()
            """)
            runner_code = textwrap.dedent(f"""
                import json, os, pathlib, signal, subprocess, sys, time
                root = pathlib.Path({str(root)!r})
                child = subprocess.Popen([sys.executable, '-c', {child_code!r}], start_new_session=True)
                def stop(signum, frame):
                    os.killpg(child.pid, signum)
                    child.wait(timeout=5)
                    (root / 'runner-reaped').touch()
                    sys.exit(130)
                signal.signal(signal.SIGTERM, stop)
                try:
                    deadline = time.monotonic() + 5
                    while not (root / 'child-ready').exists() and time.monotonic() < deadline:
                        time.sleep(.01)
                    (root / 'groups.json').write_text(json.dumps([os.getpgrp(), os.getpgid(child.pid)]))
                    (root / 'runner-ready').touch()
                    deadline = time.monotonic() + 15
                    while time.monotonic() < deadline:
                        time.sleep(.01)
                finally:
                    if child.poll() is None:
                        child.kill()
                    child.wait(timeout=5)
            """)
            owner = run_nextest.OwnedProcess([sys.executable, "-c", runner_code], cwd=root)
            try:
                deadline = time.monotonic() + 10
                while not (root / "runner-ready").exists() and time.monotonic() < deadline:
                    time.sleep(.01)
                groups = json.loads((root / "groups.json").read_text(encoding="utf-8"))
                self.assertNotEqual(groups[0], groups[1])
                owner.stop()
                self.assertEqual(owner.process.returncode, 130)
                self.assertEqual((root / "child-signal").read_text(encoding="utf-8"), str(signal.SIGTERM))
                self.assertTrue((root / "runner-reaped").is_file())
                self.assertTrue((root / "child-done").is_file())
            finally:
                try:
                    if owner.process.poll() is None:
                        owner.stop()
                finally:
                    owner.close()


if __name__ == "__main__":
    unittest.main()
