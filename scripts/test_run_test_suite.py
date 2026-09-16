from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

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


if __name__ == "__main__":
    unittest.main()
