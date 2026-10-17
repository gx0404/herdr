from __future__ import annotations

import tempfile
import unittest
from unittest import mock
import io
from pathlib import Path

from scripts import run_test_catalog as catalog
from scripts.run_test_suite import read_counts
from scripts import run_test_suite, run_parallel_unittest


class SelectionTests(unittest.TestCase):
    def test_new_tests_default_to_inclusion_and_full_restores_extended(self):
        names = ["m.T.fast", "m.T.slow", "m.T.new"]
        entries = [{"selector": "m.T.slow", "reason": "real process qualification"}]
        self.assertEqual(catalog.partition(names, entries, "default"), ([names[0], names[2]], [names[1]]))
        self.assertEqual(catalog.partition(names, entries, "full"), (names, []))

    def test_stale_and_empty_selections_fail_in_both_suites(self):
        for suite in ("default", "full"):
            with self.assertRaisesRegex(ValueError, "stale"):
                catalog.partition(["a"], [{"selector": "missing"}], suite)
            with self.assertRaisesRegex(ValueError, "empty"):
                catalog.partition([], [], suite)
        with self.assertRaisesRegex(ValueError, "empty"):
            catalog.partition(["a"], [{"selector": "a"}], "default")

    def test_module_prefix_has_a_boundary(self):
        selected, _ = catalog.partition(["m.A.test", "m.Again.test"], [{"selector": "m.A"}], "default", prefix=True)
        self.assertEqual(selected, ["m.Again.test"])

    def test_native_substring_skip_cannot_hide_new_sibling_tests(self):
        self.assertTrue(catalog.native_skip_is_exact(["m::slow", "m::fast"], ["m::slow"]))
        self.assertFalse(catalog.native_skip_is_exact(["m::slow", "m::slow_new"], ["m::slow"]))

    def test_catalog_rejects_missing_reasons_duplicates_and_unknown_schema(self):
        for content in ('version = 2', 'version = 1\nother = []',
                        'version = 1\n[[python_extended]]\nselector = "a"',
                        'version = 1\n' + '[[python_extended]]\nselector = "a"\nreason = "slow"\n' * 2):
            with tempfile.TemporaryDirectory() as temporary:
                path = Path(temporary) / "catalog.toml"
                path.write_text(content, encoding="utf-8")
                with self.assertRaises(ValueError):
                    catalog.load_catalog(path)

    def test_empty_bun_execution_cannot_pass(self):
        process = mock.Mock(stdout=io.StringIO("No tests found\n"))
        process.wait.return_value = 0
        with mock.patch.object(catalog, "bun_files", return_value=["fixture.test.ts"]), \
             mock.patch.object(catalog.subprocess, "Popen", return_value=process):
            with self.assertRaisesRegex(ValueError, "empty"):
                catalog.run_bun("docs-contract", {}, "default")

    def test_empty_python_execution_cannot_pass(self):
        completed = mock.Mock(returncode=0, stdout="Ran 0 tests in 0.000s\nOK\n")
        with mock.patch.object(run_parallel_unittest.subprocess, "run", return_value=completed):
            result = run_parallel_unittest.run_unit(run_parallel_unittest.Unit("empty", ("empty",), ()))
        self.assertEqual(result.returncode, 1)

    def test_missing_phase_log_cannot_pass(self):
        with tempfile.TemporaryDirectory() as temporary:
            result = run_test_suite.run_all(log_root=Path(temporary), phase_runner=lambda *args: (0, 0.01))
        self.assertEqual(run_test_suite.summarize(result)[0], 1)

    def test_interrupt_is_recorded_as_cancelled_not_passed(self):
        def interrupted(*args):
            raise KeyboardInterrupt("test cancellation")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with self.assertRaises(KeyboardInterrupt):
                run_test_suite.run_all(log_root=root, run_id="cancel", phase_jobs=1, phase_runner=interrupted)
            import json
            report = json.loads((root / "cancel/manifest.json").read_text(encoding="utf-8"))
            self.assertEqual(report["status"], "cancelled")
            self.assertIsNone(report["target_met"])


class CountsTests(unittest.TestCase):
    def test_counts_do_not_treat_filtered_tests_as_executed(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "run.log"
            path.write_text("[single] Summary [ 2.1s] 10 tests run: 10 passed, 99 skipped\n", encoding="utf-8")
            self.assertEqual(read_counts(path), {"executed": 10, "skipped": 99})
            path.write_text("Ran 20 tests in 3 units (4 workers) in 1.0s (skipped=2)\n 3 pass\n 1 skip\n", encoding="utf-8")
            self.assertEqual(read_counts(path), {"executed": 21, "skipped": 3})
            self.assertIsNone(read_counts(path.with_name("missing")))

    def test_authoritative_counts_do_not_double_count_isolated_filters(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "run.log"
            path.write_text('Summary [1s] 2 tests run: 2 passed, 5000 skipped\n'
                            'HERDR_TEST_COUNTS {"executed": 5000, "skipped": 27}\n', encoding="utf-8")
            self.assertEqual(read_counts(path), {"executed": 5000, "skipped": 27})


if __name__ == "__main__":
    unittest.main()
