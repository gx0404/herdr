from __future__ import annotations

import json
import sys
import tempfile
import threading
import unittest
from pathlib import Path
from unittest import mock

from scripts import run_test_suite

PROJECT_ROOT = Path(__file__).resolve().parent.parent
JUSTFILE = PROJECT_ROOT / "justfile"


class PhaseDefinitionTests(unittest.TestCase):
    def test_phases_cover_all_suite_stages(self) -> None:
        self.assertEqual(
            run_test_suite.phase_names(),
            [
                "nextest",
                "maintenance",
                "ui-hot-path",
                "integration-assets",
                "docs-contract",
            ],
        )

    def test_phase_recipes_exist_in_justfile(self) -> None:
        justfile = JUSTFILE.read_text(encoding="utf-8")
        for _, recipe in run_test_suite.PHASES:
            self.assertIn(f"\n{recipe}:", justfile, f"justfile 缺少 recipe {recipe}")

    def test_justfile_test_recipe_delegates_to_orchestrator(self) -> None:
        justfile = JUSTFILE.read_text(encoding="utf-8")
        self.assertIn("scripts/run_test_suite.py", justfile)

    def test_maintenance_manifest_registers_orchestrator_tests(self) -> None:
        justfile = JUSTFILE.read_text(encoding="utf-8")
        self.assertIn("scripts.test_run_test_suite", justfile)
        self.assertIn("scripts.test_run_nextest", justfile)

    def test_nextest_recipes_have_an_explicit_overridable_thread_limit(self) -> None:
        justfile = JUSTFILE.read_text(encoding="utf-8")
        self.assertIn('env_var_or_default("HERDR_NEXTEST_JOBS", "4")', justfile)
        self.assertEqual(justfile.count("--test-threads {{nextest_jobs}}"), 3)
        self.assertIn("nextest-all:\n    {{python}} scripts/run_nextest.py --test-threads {{nextest_jobs}}", justfile)
        self.assertIn('test-one filter:\n    cargo nextest run --locked --test-threads {{nextest_jobs}} "{{filter}}" --status-level leak --final-status-level fail --failure-output final --success-output never', justfile)
        self.assertIn('ci-tests filter=\'all()\':\n    cargo nextest run --locked --test-threads {{nextest_jobs}} -E "{{filter}}" --status-level leak --final-status-level slow --failure-output final --success-output never', justfile)

    def test_default_log_root_is_explicit_and_each_run_uses_a_child_directory(self) -> None:
        self.assertEqual(PROJECT_ROOT / "target" / "test-suite-logs", run_test_suite.LOG_DIR)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)

            def runner(name: str, recipe: str, log_path: Path) -> tuple[int, float]:
                log_path.write_text(f"{name}:{recipe}\n", encoding="utf-8")
                return 0, 0.01

            with mock.patch.object(run_test_suite, "LOG_DIR", root):
                run_test_suite.run_all(
                    phase_jobs=1,
                    run_id="run-a",
                    phase_runner=runner,
                )
                run_test_suite.run_all(
                    phase_jobs=1,
                    run_id="run-b",
                    phase_runner=runner,
                )
            self.assertTrue((root / "run-a" / "manifest.json").is_file())
            self.assertTrue((root / "run-b" / "manifest.json").is_file())
            self.assertTrue((root / "run-a" / "nextest.log").is_file())
            self.assertTrue((root / "run-b" / "nextest.log").is_file())


class PhaseConfigurationTests(unittest.TestCase):
    def test_environment_phase_limit_is_used_and_cli_value_wins(self) -> None:
        self.assertEqual(
            run_test_suite.resolve_phase_jobs(environ={run_test_suite.PHASE_JOBS_ENV: "3"}),
            3,
        )
        self.assertEqual(
            run_test_suite.resolve_phase_jobs(1, {run_test_suite.PHASE_JOBS_ENV: "3"}),
            1,
        )

    def test_invalid_phase_limit_is_not_silently_accepted(self) -> None:
        with self.assertRaises(ValueError):
            run_test_suite.resolve_phase_jobs(environ={run_test_suite.PHASE_JOBS_ENV: "0"})

    def test_budget_defaults_leave_capacity_for_nextest(self) -> None:
        environment = {run_test_suite.TEST_BUDGET_ENV: "8"}
        maintenance = run_test_suite.resolve_maintenance_jobs(environ=environment)
        self.assertEqual(maintenance, 4)
        self.assertEqual(
            run_test_suite.resolve_nextest_jobs(environ=environment, maintenance_jobs=maintenance),
            4,
        )

    def test_budget_cli_values_override_environment(self) -> None:
        environment = {
            run_test_suite.TEST_BUDGET_ENV: "12",
            run_test_suite.MAINTENANCE_JOBS_ENV: "3",
            run_test_suite.NEXTEST_JOBS_ENV: "5",
        }
        self.assertEqual(run_test_suite.resolve_test_budget(7, environment), 7)
        self.assertEqual(run_test_suite.resolve_maintenance_jobs(2, environment, test_budget=7), 2)
        self.assertEqual(
            run_test_suite.resolve_nextest_jobs(1, environment, maintenance_jobs=2, test_budget=7),
            1,
        )

    def test_combined_runner_passes_worker_limits_to_child_environment(self) -> None:
        seen: dict[str, dict[str, str]] = {}

        def runner(
            name: str,
            recipe: str,
            log_path: Path,
            *,
            environment: dict[str, str],
        ) -> tuple[int, float]:
            seen[name] = environment
            log_path.write_text("ok\n", encoding="utf-8")
            return 0, 0.01

        with tempfile.TemporaryDirectory() as temporary:
            with mock.patch.object(run_test_suite, "run_phase", side_effect=runner):
                run_test_suite.run_all(
                    phase_jobs=1,
                    test_budget=9,
                    maintenance_jobs=3,
                    nextest_jobs=6,
                    log_root=Path(temporary),
                    run_id="environment",
                )
        self.assertEqual(len(seen), len(run_test_suite.PHASES))
        self.assertTrue(
            all(
                environment[run_test_suite.MAINTENANCE_JOBS_ENV] == "3"
                and environment[run_test_suite.NEXTEST_JOBS_ENV] == "6"
                for environment in seen.values()
            )
        )

    def test_phase_limit_bounds_active_workers(self) -> None:
        active = 0
        maximum = 0
        entered = 0
        lock = threading.Lock()
        first_workers = threading.Barrier(2, timeout=30)

        def runner(name: str, recipe: str, log_path: Path) -> tuple[int, float]:
            nonlocal active, maximum, entered
            log_path.write_text("ok\n", encoding="utf-8")
            with lock:
                active += 1
                maximum = max(maximum, active)
                entered += 1
                synchronize = entered <= 2
            try:
                if synchronize:
                    first_workers.wait()
                return 0, 0.02
            finally:
                with lock:
                    active -= 1

        with tempfile.TemporaryDirectory() as temporary:
            results = run_test_suite.run_all(
                phase_jobs=2,
                log_root=Path(temporary),
                run_id="bounded",
                phase_runner=runner,
            )
        self.assertEqual(len(results), len(run_test_suite.PHASES))
        self.assertEqual(active, 0)
        for name, (code, _) in results.items():
            self.assertEqual(code, 0, name)
        self.assertEqual(maximum, 2)


class SummarizeTests(unittest.TestCase):
    def test_all_pass_yields_zero(self) -> None:
        results = {name: (0, 1.0) for name in run_test_suite.phase_names()}
        self.assertEqual(run_test_suite.summarize(results), (0, []))

    def test_any_failure_yields_one_with_names(self) -> None:
        results = {name: (0, 1.0) for name in run_test_suite.phase_names()}
        results["maintenance"] = (1, 2.0)
        self.assertEqual(run_test_suite.summarize(results), (1, ["maintenance"]))

    def test_startup_failure_counts_as_failure(self) -> None:
        results = {name: (0, 1.0) for name in run_test_suite.phase_names()}
        results["nextest"] = (None, 0.1)
        self.assertEqual(run_test_suite.summarize(results), (1, ["nextest"]))

    def test_missing_phase_counts_as_failure(self) -> None:
        results = {"nextest": (0, 1.0)}
        code, failures = run_test_suite.summarize(results)
        self.assertEqual(code, 1)
        self.assertEqual(failures, [name for name, _ in run_test_suite.PHASES if name != "nextest"])


class ManifestTests(unittest.TestCase):
    def test_manifest_is_atomic_and_records_final_phase_results(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)

            def runner(name: str, recipe: str, log_path: Path) -> tuple[int, float]:
                log_path.write_text("ok\n", encoding="utf-8")
                return 0, 0.25

            run_test_suite.run_all(
                phase_jobs=3,
                test_budget=9,
                maintenance_jobs=3,
                nextest_jobs=6,
                log_root=root,
                run_id="manifest-run",
                phase_runner=runner,
            )
            run_dir = root / "manifest-run"
            manifest = json.loads((run_dir / "manifest.json").read_text(encoding="utf-8"))
            self.assertEqual(manifest["status"], "passed")
            self.assertEqual(manifest["run_id"], "manifest-run")
            self.assertEqual(
                {key: manifest[key] for key in ("test_budget", "phase_jobs", "maintenance_jobs", "nextest_jobs")},
                {"test_budget": 9, "phase_jobs": 3, "maintenance_jobs": 3, "nextest_jobs": 6},
            )
            self.assertTrue(all(item["status"] == "passed" for item in manifest["phases"].values()))
            self.assertEqual(list(run_dir.glob(".manifest.json.*.tmp")), [])

    def test_failed_phase_is_recorded_as_failed_not_green(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)

            def runner(name: str, recipe: str, log_path: Path) -> tuple[int | None, float]:
                log_path.write_text("failure\n", encoding="utf-8")
                if name == "maintenance":
                    return 3, 0.1
                if name == "docs-contract":
                    return None, 0.1
                return 0, 0.1

            results = run_test_suite.run_all(
                phase_jobs=2,
                log_root=root,
                run_id="failed-run",
                phase_runner=runner,
            )
            manifest = json.loads((root / "failed-run" / "manifest.json").read_text(encoding="utf-8"))
            self.assertEqual(run_test_suite.summarize(results)[0], 1)
            self.assertEqual(manifest["status"], "failed")
            self.assertEqual(manifest["phases"]["maintenance"]["exit_code"], 3)
            self.assertEqual(manifest["phases"]["maintenance"]["status"], "failed")
            self.assertIsNone(manifest["phases"]["docs-contract"]["exit_code"])
            self.assertEqual(manifest["phases"]["docs-contract"]["status"], "failed")


class RunPhaseTests(unittest.TestCase):
    def test_run_phase_streams_output_and_writes_log(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "phase.log"
            code, seconds = run_test_suite.run_phase(
                "probe",
                "nonexistent-recipe-for-probe",
                log_path,
                command=[
                    "python3",
                    "-c",
                    "import sys; print('suite-probe'); sys.exit(3)",
                ],
            )
            self.assertEqual(code, 3)
            self.assertGreaterEqual(seconds, 0.0)
            self.assertIn("suite-probe", log_path.read_text(encoding="utf-8"))

    def test_run_phase_forwards_utf8_output_regardless_of_locale(self) -> None:
        # “吐”的 UTF-8 编码含 0x90，cp1252 下未定义：按 locale 解码会让转发崩溃。
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "phase.log"
            code, _ = run_test_suite.run_phase(
                "probe",
                "nonexistent-recipe-for-probe",
                log_path,
                command=[
                    sys.executable,
                    "-c",
                    "import sys; sys.stdout.buffer.write('吐 suite-probe\\n'.encode('utf-8'))",
                ],
            )
            self.assertEqual(code, 0)
            self.assertIn("吐 suite-probe", log_path.read_text(encoding="utf-8"))

    def test_run_phase_reports_missing_command_as_none(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "phase.log"
            code, _ = run_test_suite.run_phase(
                "probe",
                "nonexistent-recipe-for-probe",
                log_path,
                command=["definitely-missing-program-xyz"],
            )
        self.assertIsNone(code)


class BudgetAndProgressTests(unittest.TestCase):
    def test_small_budgets_reserve_capacity_for_concurrent_phases(self) -> None:
        for budget in range(1, 9):
            with self.subTest(budget=budget):
                environment = {run_test_suite.TEST_BUDGET_ENV: str(budget)}
                phases = run_test_suite.resolve_phase_jobs(environ=environment)
                maintenance = run_test_suite.resolve_maintenance_jobs(environ=environment)
                nextest = run_test_suite.resolve_nextest_jobs(
                    environ=environment, maintenance_jobs=maintenance
                )
                self.assertEqual(phases, min(2, budget))
                self.assertEqual(maintenance, min(4, max(1, budget - 1)))
                self.assertEqual(nextest, max(1, budget - maintenance))
                workers = max(maintenance, nextest) if phases == 1 else maintenance + nextest
                self.assertLessEqual(workers, budget)

    def test_serial_phases_can_each_use_the_selected_budget(self) -> None:
        environment = {
            run_test_suite.TEST_BUDGET_ENV: "8",
            run_test_suite.PHASE_JOBS_ENV: "1",
        }
        self.assertEqual(run_test_suite.resolve_maintenance_jobs(environ=environment), 4)
        self.assertEqual(run_test_suite.resolve_nextest_jobs(environ=environment), 8)

    def test_explicit_worker_overrides_are_not_capped_by_the_budget(self) -> None:
        environment = {
            run_test_suite.TEST_BUDGET_ENV: "1",
            run_test_suite.PHASE_JOBS_ENV: "3",
            run_test_suite.MAINTENANCE_JOBS_ENV: "8",
            run_test_suite.NEXTEST_JOBS_ENV: "10",
        }
        self.assertEqual(run_test_suite.resolve_phase_jobs(environ=environment), 3)
        self.assertEqual(run_test_suite.resolve_maintenance_jobs(environ=environment), 8)
        self.assertEqual(run_test_suite.resolve_nextest_jobs(environ=environment), 10)

    def test_cli_budget_drives_default_phase_and_worker_limits(self) -> None:
        results = {name: (0, 0.01) for name in run_test_suite.phase_names()}
        environment = {run_test_suite.TEST_BUDGET_ENV: "12"}
        with mock.patch.dict(run_test_suite.os.environ, environment, clear=True):
            with mock.patch.object(run_test_suite, "run_all", return_value=results) as runner:
                self.assertEqual(run_test_suite.main(["--test-budget", "1"]), 0)
        self.assertEqual(
            runner.call_args.kwargs,
            {"phase_jobs": 1, "test_budget": 1, "maintenance_jobs": 1, "nextest_jobs": 1},
        )

    def test_running_phase_is_distinct_from_pending_and_finished_phases(self) -> None:
        observations: list[tuple[str, dict[str, object]]] = []
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)

            def runner(name: str, recipe: str, log_path: Path) -> tuple[int, float]:
                manifest = json.loads((log_path.parent / "manifest.json").read_text(encoding="utf-8"))
                observations.append((name, manifest))
                log_path.write_text("ok\n", encoding="utf-8")
                return 0, 0.01

            with mock.patch.dict(run_test_suite.os.environ, {}, clear=True):
                run_test_suite.run_all(
                    test_budget=1,
                    log_root=root,
                    run_id="live-status",
                    phase_runner=runner,
                )
            final = json.loads((root / "live-status" / "manifest.json").read_text(encoding="utf-8"))
        self.assertEqual(final["phase_jobs"], 1)
        self.assertEqual(final["status"], "passed")
        names = run_test_suite.phase_names()
        self.assertEqual([name for name, _ in observations], names)
        for index, (name, manifest) in enumerate(observations):
            self.assertEqual(manifest["status"], "running")
            self.assertEqual(manifest["phases"][name]["status"], "running")
            self.assertIsNone(manifest["phases"][name]["exit_code"])
            for previous in names[:index]:
                self.assertEqual(manifest["phases"][previous]["status"], "passed")
            for pending in names[index + 1 :]:
                self.assertEqual(manifest["phases"][pending]["status"], "pending")

    def test_phase_log_is_visible_before_the_process_exits(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "phase.log"
            observed: list[str] = []

            def lines():
                yield "suite-live-probe\n"
                observed.append(log_path.read_text(encoding="utf-8"))

            stream = mock.Mock(wraps=lines())
            stream.__iter__ = lambda self: self._mock_wraps
            process = mock.Mock(stdout=stream)
            process.wait.return_value = 0
            with mock.patch.object(run_test_suite.subprocess, "Popen", return_value=process):
                code, _ = run_test_suite.run_phase("probe", "probe", log_path)
            self.assertEqual(code, 0)
            self.assertEqual(observed, ["suite-live-probe\n"])


if __name__ == "__main__":
    unittest.main()
