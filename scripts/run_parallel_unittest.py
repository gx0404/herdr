"""并行运行 unittest 模块清单（`just maintenance-test` 的执行器）。

父进程用 unittest 的 loader 枚举清单模块里的测试，默认按「模块 + 类」切成工作单元；
每个单元在独立子进程里跑 `python -m unittest`，线程池按 CPU 数并发。清单里的模块已
核实可跨进程并发：不写仓库工作树、git 写操作只发生在各自唯一的临时仓库、没有固定
端口/管道/锁文件。

上一轮各测试的耗时记在 `target/test-suite-logs/unittest-durations.json`（gitignored）。
有记录时，慢的类按估计耗时拆成不超过 `TARGET_UNIT_SECONDS` 的块，并从长到短派发，
缩短长尾；没有记录时每类一个单元、按清单顺序派发（子进程启动也有成本，不盲目细拆）。
任一单元失败（含模块导入失败、子进程异常退出）即整体非零退出，失败单元的完整输出
会回放。
"""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
import re
import subprocess
import sys
import time
import unittest
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Iterator

PROJECT_ROOT = Path(__file__).resolve().parent.parent
DURATIONS_PATH = PROJECT_ROOT / "target" / "test-suite-logs" / "unittest-durations.json"
TARGET_UNIT_SECONDS = 3.0
UNKNOWN_TEST_SECONDS = 0.05
SLOWEST_REPORTED = 5

_RAN_RE = re.compile(r"^Ran (\d+) tests? in ", re.MULTILINE)
_SKIPPED_RE = re.compile(r"skipped=(\d+)")


@dataclass(frozen=True)
class Unit:
    """一个子进程要跑的测试：`targets` 是 unittest 名称（测试 id 或整个模块）。"""

    label: str
    targets: tuple[str, ...]
    test_ids: tuple[str, ...]


@dataclass(frozen=True)
class UnitResult:
    unit: Unit
    returncode: int | None
    output: str
    seconds: float
    ran: int
    skipped: int


def iter_test_cases(suite: unittest.TestSuite | unittest.TestCase) -> Iterator[unittest.TestCase]:
    if isinstance(suite, unittest.TestSuite):
        for item in suite:
            yield from iter_test_cases(item)
    else:
        yield suite


def _is_load_failure(test: unittest.TestCase) -> bool:
    # 导入或加载失败时 loader 返回合成的 _FailedTest，它的错误只能在子进程里重现。
    return type(test).__module__ == "unittest.loader"


def split_by_duration(
    test_ids: list[str], durations: dict[str, float], target_seconds: float = TARGET_UNIT_SECONDS
) -> list[list[str]]:
    """按历史耗时顺序装箱：每块估计不超过 target_seconds（单个超时的测试独占一块）。"""
    if not durations:
        return [test_ids]
    chunks: list[list[str]] = []
    current: list[str] = []
    current_seconds = 0.0
    for test_id in test_ids:
        seconds = durations.get(test_id, UNKNOWN_TEST_SECONDS)
        if current and current_seconds + seconds > target_seconds:
            chunks.append(current)
            current, current_seconds = [], 0.0
        current.append(test_id)
        current_seconds += seconds
    if current:
        chunks.append(current)
    return chunks


def plan_units(
    modules: Iterable[str],
    durations: dict[str, float] | None = None,
    target_seconds: float = TARGET_UNIT_SECONDS,
) -> list[Unit]:
    """把模块清单切成工作单元；加载失败的模块整体作为一个单元，交给子进程报错。"""
    durations = durations or {}
    loader = unittest.TestLoader()
    units: list[Unit] = []
    for module in modules:
        try:
            cases = list(iter_test_cases(loader.loadTestsFromName(module)))
        except Exception:
            units.append(Unit(module, (module,), ()))
            continue
        if not cases or any(_is_load_failure(case) for case in cases):
            units.append(Unit(module, (module,), ()))
            continue
        by_class: dict[str, list[str]] = {}
        for case in cases:
            test_id = case.id()
            by_class.setdefault(test_id.rsplit(".", 1)[0], []).append(test_id)
        for class_name, test_ids in by_class.items():
            chunks = split_by_duration(test_ids, durations, target_seconds)
            for index, chunk in enumerate(chunks, start=1):
                label = class_name if len(chunks) == 1 else f"{class_name}[{index}/{len(chunks)}]"
                units.append(Unit(label, tuple(chunk), tuple(chunk)))
    return units


def load_durations(path: Path = DURATIONS_PATH) -> dict[str, float]:
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    if not isinstance(data, dict):
        return {}
    return {key: float(value) for key, value in data.items() if isinstance(value, (int, float))}


def estimated_seconds(unit: Unit, durations: dict[str, float]) -> float:
    if not unit.test_ids:
        return durations.get(unit.label, UNKNOWN_TEST_SECONDS)
    return sum(durations.get(test_id, UNKNOWN_TEST_SECONDS) for test_id in unit.test_ids)


def order_units(units: list[Unit], durations: dict[str, float]) -> list[Unit]:
    """有历史耗时时从长到短派发（最长处理时间优先）；没有时保持清单顺序。"""
    if not durations:
        return list(units)
    return sorted(units, key=lambda unit: estimated_seconds(unit, durations), reverse=True)


def record_durations(results: Iterable[UnitResult], path: Path = DURATIONS_PATH) -> None:
    durations = load_durations(path)
    for result in results:
        if result.returncode != 0:
            continue
        if result.unit.test_ids:
            share = result.seconds / len(result.unit.test_ids)
            for test_id in result.unit.test_ids:
                durations[test_id] = round(share, 3)
        else:
            durations[result.unit.label] = round(result.seconds, 3)
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(durations, indent=0, sort_keys=True), encoding="utf-8")
    except OSError:
        pass


def parse_counts(output: str) -> tuple[int, int]:
    ran = sum(int(match) for match in _RAN_RE.findall(output))
    skipped = sum(int(match) for match in _SKIPPED_RE.findall(output))
    return ran, skipped


def run_unit(unit: Unit, python: str = sys.executable, cwd: Path = PROJECT_ROOT) -> UnitResult:
    started = time.monotonic()
    try:
        completed = subprocess.run(
            [python, "-m", "unittest", "-q", *unit.targets],
            cwd=cwd,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            encoding="utf-8",
            errors="replace",
            check=False,
        )
    except OSError as error:
        return UnitResult(unit, None, f"failed to start unittest: {error}", time.monotonic() - started, 0, 0)
    ran, skipped = parse_counts(completed.stdout)
    return UnitResult(unit, completed.returncode, completed.stdout, time.monotonic() - started, ran, skipped)


def run_units(units: list[Unit], jobs: int, runner=run_unit) -> list[UnitResult]:
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, jobs)) as pool:
        return list(pool.map(runner, units))


def summarize(results: list[UnitResult], wall_seconds: float, jobs: int) -> tuple[int, list[str]]:
    """返回 (退出码, 输出行)：任一单元非零退出即失败；失败单元回放完整输出。"""
    lines: list[str] = []
    failures = [result for result in results if result.returncode != 0]
    for result in failures:
        lines.append(f"===== FAIL {result.unit.label} (exit={result.returncode}, {result.seconds:.1f}s)")
        lines.append(f"python -m unittest {' '.join(result.unit.targets)}")
        lines.append(result.output.rstrip())
    ran = sum(result.ran for result in results)
    skipped = sum(result.skipped for result in results)
    slowest = sorted(results, key=lambda result: result.seconds, reverse=True)[:SLOWEST_REPORTED]
    lines.append(
        f"Ran {ran} tests in {len(results)} units ({jobs} workers) in {wall_seconds:.1f}s"
        + (f" (skipped={skipped})" if skipped else "")
    )
    lines.append("slowest units: " + ", ".join(f"{result.unit.label} {result.seconds:.1f}s" for result in slowest))
    if failures:
        lines.append(f"FAILED ({len(failures)} of {len(results)} units)")
        return 1, lines
    lines.append("OK")
    return 0, lines


def _force_utf8_streams() -> None:
    # Windows CI 的 stdout 是 cp1252；失败回放可能带中文。
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8", errors="replace")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("modules", nargs="+", help="unittest 模块名，例如 scripts.test_release")
    parser.add_argument("--jobs", type=int, default=os.cpu_count() or 4, help="并发子进程数（默认 CPU 数）")
    args = parser.parse_args(argv)
    _force_utf8_streams()
    # 与子进程的 `python -m unittest`（cwd=仓库根）同样从仓库根导入 `scripts.*`。
    sys.path.insert(0, str(PROJECT_ROOT))
    started = time.monotonic()
    durations = load_durations()
    units = order_units(plan_units(args.modules, durations), durations)
    results = run_units(units, args.jobs)
    code, lines = summarize(results, time.monotonic() - started, args.jobs)
    record_durations(results)
    print("\n".join(lines), flush=True)
    return code


if __name__ == "__main__":
    sys.exit(main())
