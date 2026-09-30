from __future__ import annotations

import sys
import tempfile
import textwrap
import unittest
import uuid
from pathlib import Path

from scripts import run_parallel_unittest as runner

PROJECT_ROOT = Path(__file__).resolve().parent.parent
JUSTFILE = PROJECT_ROOT / "justfile"


class TempModuleMixin:
    """在临时目录里写可导入的测试模块，测试结束后从 sys.path/sys.modules 移除。"""

    def make_module(self, source: str) -> tuple[str, Path]:
        directory = Path(self._temp.name)
        name = f"parallel_unittest_probe_{uuid.uuid4().hex[:8]}"
        (directory / f"{name}.py").write_text(textwrap.dedent(source), encoding="utf-8")
        return name, directory

    def setUp(self) -> None:
        self._temp = tempfile.TemporaryDirectory()
        sys.path.insert(0, self._temp.name)
        self._modules_before = set(sys.modules)

    def tearDown(self) -> None:
        sys.path.remove(self._temp.name)
        for name in set(sys.modules) - self._modules_before:
            if name.startswith("parallel_unittest_probe_"):
                del sys.modules[name]
        self._temp.cleanup()


class PlanTests(TempModuleMixin, unittest.TestCase):
    BIG_AND_SMALL = """
        import unittest

        class Big(unittest.TestCase):
            def test_a(self): pass
            def test_b(self): pass
            def test_c(self): pass
            def test_d(self): pass
            def test_e(self): pass

        class Small(unittest.TestCase):
            def test_only(self): pass
        """

    def test_without_history_each_class_is_one_unit(self) -> None:
        module, _ = self.make_module(self.BIG_AND_SMALL)
        units = runner.plan_units([module])
        self.assertEqual([unit.label for unit in units], [f"{module}.Big", f"{module}.Small"])
        self.assertEqual(len(units[0].targets), 5)

    def test_history_splits_slow_classes_into_bounded_chunks_in_loader_order(self) -> None:
        module, _ = self.make_module(self.BIG_AND_SMALL)
        durations = {f"{module}.Big.test_{name}": 1.0 for name in "abcde"}
        durations[f"{module}.Small.test_only"] = 0.01
        units = runner.plan_units([module], durations, target_seconds=2.5)
        self.assertEqual(
            [unit.label for unit in units],
            [f"{module}.Big[1/3]", f"{module}.Big[2/3]", f"{module}.Big[3/3]", f"{module}.Small"],
        )
        self.assertEqual(units[0].targets, (f"{module}.Big.test_a", f"{module}.Big.test_b"))
        self.assertEqual(units[2].targets, (f"{module}.Big.test_e",))
        self.assertEqual(sum(len(unit.test_ids) for unit in units), 6)

    def test_a_single_test_over_budget_gets_its_own_unit(self) -> None:
        chunks = runner.split_by_duration(["a", "b", "c"], {"a": 0.1, "b": 9.0, "c": 0.1}, target_seconds=1.0)
        self.assertEqual(chunks, [["a"], ["b"], ["c"]])

    def test_modules_that_fail_to_load_run_whole_so_the_error_is_reported(self) -> None:
        broken, _ = self.make_module("raise RuntimeError('import boom')\n")
        missing = "parallel_unittest_probe_does_not_exist"
        units = runner.plan_units([broken, missing])
        self.assertEqual([unit.targets for unit in units], [(broken,), (missing,)])
        self.assertTrue(all(not unit.test_ids for unit in units))


class OrderingAndDurationTests(unittest.TestCase):
    def units(self) -> list[runner.Unit]:
        return [
            runner.Unit("m.Fast", ("m.Fast.test_a",), ("m.Fast.test_a",)),
            runner.Unit("m.Slow", ("m.Slow.test_a", "m.Slow.test_b"), ("m.Slow.test_a", "m.Slow.test_b")),
            runner.Unit("broken", ("broken",), ()),
        ]

    def test_without_history_the_manifest_order_is_kept(self) -> None:
        units = self.units()
        self.assertEqual(runner.order_units(units, {}), units)

    def test_history_dispatches_the_longest_units_first(self) -> None:
        durations = {"m.Fast.test_a": 0.1, "m.Slow.test_a": 3.0, "m.Slow.test_b": 2.0, "broken": 1.0}
        ordered = runner.order_units(self.units(), durations)
        self.assertEqual([unit.label for unit in ordered], ["m.Slow", "broken", "m.Fast"])

    def test_durations_round_trip_and_skip_failed_units(self) -> None:
        units = self.units()
        results = [
            runner.UnitResult(units[0], 0, "", 0.4, 1, 0),
            runner.UnitResult(units[1], 1, "", 9.0, 2, 0),
            runner.UnitResult(units[2], 0, "", 1.5, 0, 0),
        ]
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "nested" / "durations.json"
            runner.record_durations(results, path)
            self.assertEqual(runner.load_durations(path), {"m.Fast.test_a": 0.4, "broken": 1.5})

    def test_unreadable_history_is_ignored(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "durations.json"
            path.write_text("[not, a, mapping", encoding="utf-8")
            self.assertEqual(runner.load_durations(path), {})


class SummaryTests(unittest.TestCase):
    def test_counts_are_parsed_from_unittest_output(self) -> None:
        output = "....\n----\nRan 4 tests in 0.010s\n\nOK (skipped=2)\n"
        self.assertEqual(runner.parse_counts(output), (4, 2))

    def test_any_failed_unit_fails_the_run_and_replays_its_output(self) -> None:
        passing = runner.Unit("m.A", ("m.A.test_a",), ("m.A.test_a",))
        failing = runner.Unit("m.B", ("m.B.test_b",), ("m.B.test_b",))
        results = [
            runner.UnitResult(passing, 0, "Ran 1 test in 0.1s\nOK\n", 0.2, 1, 0),
            runner.UnitResult(failing, 1, "FAIL: test_b\nAssertionError: 故障\n", 0.3, 1, 0),
        ]
        code, lines = runner.summarize(results, 1.0, 4)
        text = "\n".join(lines)
        self.assertEqual(code, 1)
        self.assertIn("===== FAIL m.B (exit=1", text)
        self.assertIn("AssertionError: 故障", text)
        self.assertNotIn("===== FAIL m.A", text)
        self.assertTrue(lines[-1].startswith("FAILED (1 of 2 units)"))

    def test_start_failures_count_as_failures(self) -> None:
        unit = runner.Unit("m.A", ("m.A.test_a",), ("m.A.test_a",))
        code, lines = runner.summarize([runner.UnitResult(unit, None, "failed to start", 0.0, 0, 0)], 0.1, 1)
        self.assertEqual(code, 1)

    def test_all_passing_units_report_ok(self) -> None:
        unit = runner.Unit("m.A", ("m.A.test_a",), ("m.A.test_a",))
        code, lines = runner.summarize([runner.UnitResult(unit, 0, "", 0.1, 3, 1)], 0.5, 2)
        self.assertEqual(code, 0)
        self.assertIn("Ran 3 tests in 1 units (2 workers)", lines[0])
        self.assertIn("(skipped=1)", lines[0])
        self.assertEqual(lines[-1], "OK")

    def test_results_keep_dispatch_order(self) -> None:
        units = [runner.Unit(f"m.T{index}", (f"m.T{index}.test",), (f"m.T{index}.test",)) for index in range(6)]
        results = runner.run_units(units, 3, runner=lambda unit: runner.UnitResult(unit, 0, "", 0.0, 1, 0))
        self.assertEqual([result.unit for result in results], units)


class SubprocessTests(TempModuleMixin, unittest.TestCase):
    def test_unit_runs_in_a_child_unittest_process(self) -> None:
        module, directory = self.make_module(
            """
            import unittest

            class Probe(unittest.TestCase):
                def test_pass(self): pass

                @unittest.skip("probe")
                def test_skip(self): pass

                def test_fail(self):
                    self.assertEqual(1, 2)
            """
        )
        passing = runner.Unit("p", (f"{module}.Probe.test_pass", f"{module}.Probe.test_skip"), ())
        failing = runner.Unit("f", (f"{module}.Probe.test_fail",), ())
        ok = runner.run_unit(passing, cwd=directory)
        bad = runner.run_unit(failing, cwd=directory)
        self.assertEqual((ok.returncode, ok.ran, ok.skipped), (0, 2, 1))
        self.assertEqual((bad.returncode, bad.ran), (1, 1))
        self.assertIn("AssertionError", bad.output)


class RegistrationTests(unittest.TestCase):
    def test_maintenance_recipe_runs_the_manifest_through_the_parallel_runner(self) -> None:
        justfile = JUSTFILE.read_text(encoding="utf-8")
        recipe = justfile.split("\nmaintenance-test:", 1)[1].split("\n\n", 1)[0]
        self.assertIn("scripts/run_parallel_unittest.py", recipe)
        self.assertIn("scripts.test_run_parallel_unittest", recipe)
        self.assertNotIn("-m unittest", recipe)


if __name__ == "__main__":
    unittest.main()
