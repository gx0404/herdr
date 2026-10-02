"""并行编排 `just test` 的五个测试阶段。

各阶段的命令真源在 justfile recipe（nextest-all / maintenance-test /
ui-hot-path-architecture-test / integration-assets-test / docs-contract-test），
本脚本只负责有界并发调度、实时前缀转发输出、按 run-id 隔离日志、原子结果
manifest 与失败聚合；任一阶段失败即整体非零退出，工具链缺失记为该阶段失败，
不整族跳过。
"""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Mapping

PROJECT_ROOT = Path(__file__).resolve().parent.parent
LOG_DIR = PROJECT_ROOT / "target" / "test-suite-logs"
FAILURE_RECAP_LINES = 40
PHASE_JOBS_ENV = "HERDR_TEST_PHASE_JOBS"
TEST_BUDGET_ENV = "HERDR_TEST_BUDGET"
MAINTENANCE_JOBS_ENV = "HERDR_MAINTENANCE_JOBS"
NEXTEST_JOBS_ENV = "HERDR_NEXTEST_JOBS"
MANIFEST_NAME = "manifest.json"

# (阶段名, 对应 just recipe 名)；顺序仅决定汇总打印顺序，执行受 phase jobs 限制。
PHASES: tuple[tuple[str, str], ...] = (
    ("nextest", "nextest-all"),
    ("maintenance", "maintenance-test"),
    ("ui-hot-path", "ui-hot-path-architecture-test"),
    ("integration-assets", "integration-assets-test"),
    ("docs-contract", "docs-contract-test"),
)
MAX_DEFAULT_TEST_BUDGET = 8
DEFAULT_TEST_BUDGET = min(MAX_DEFAULT_TEST_BUDGET, max(1, os.cpu_count() or 1))
DEFAULT_PHASE_JOBS = min(2, len(PHASES), DEFAULT_TEST_BUDGET)
DEFAULT_MAINTENANCE_JOBS = min(4, DEFAULT_TEST_BUDGET)
DEFAULT_NEXTEST_JOBS = max(1, DEFAULT_TEST_BUDGET - DEFAULT_MAINTENANCE_JOBS)

LAST_RUN_DIR: Path | None = None


def phase_names() -> list[str]:
    return [name for name, _ in PHASES]


def _positive_int(value: int | str, label: str) -> int:
    try:
        parsed = int(value)
    except (TypeError, ValueError) as error:
        raise ValueError(f"{label} must be a positive integer: {value!r}") from error
    if parsed < 1:
        raise ValueError(f"{label} must be a positive integer: {parsed}")
    return parsed


def resolve_phase_jobs(value: int | None = None, environ: Mapping[str, str] | None = None) -> int:
    """Resolve the phase limit, with the command-line value taking precedence over the environment."""
    if value is not None:
        return _positive_int(value, "phase jobs")
    environment = os.environ if environ is None else environ
    raw = environment.get(PHASE_JOBS_ENV)
    if raw is not None:
        return _positive_int(raw, PHASE_JOBS_ENV)
    return DEFAULT_PHASE_JOBS


def resolve_test_budget(value: int | None = None, environ: Mapping[str, str] | None = None) -> int:
    """Resolve the default worker budget used by the combined test orchestrator."""
    if value is not None:
        return _positive_int(value, "test budget")
    environment = os.environ if environ is None else environ
    raw = environment.get(TEST_BUDGET_ENV)
    if raw is not None:
        return _positive_int(raw, TEST_BUDGET_ENV)
    return DEFAULT_TEST_BUDGET


def resolve_maintenance_jobs(
    value: int | None = None,
    environ: Mapping[str, str] | None = None,
    test_budget: int | None = None,
) -> int:
    """Resolve maintenance workers, keeping the combined default within its budget."""
    if value is not None:
        return _positive_int(value, "maintenance jobs")
    environment = os.environ if environ is None else environ
    raw = environment.get(MAINTENANCE_JOBS_ENV)
    if raw is not None:
        return _positive_int(raw, MAINTENANCE_JOBS_ENV)
    budget = resolve_test_budget(test_budget, environment)
    return min(DEFAULT_MAINTENANCE_JOBS, budget)


def resolve_nextest_jobs(
    value: int | None = None,
    environ: Mapping[str, str] | None = None,
    maintenance_jobs: int | None = None,
    test_budget: int | None = None,
) -> int:
    """Resolve nextest threads, leaving the remaining default budget to nextest."""
    if value is not None:
        return _positive_int(value, "nextest jobs")
    environment = os.environ if environ is None else environ
    raw = environment.get(NEXTEST_JOBS_ENV)
    if raw is not None:
        return _positive_int(raw, NEXTEST_JOBS_ENV)
    budget = resolve_test_budget(test_budget, environment)
    maintenance = (
        maintenance_jobs
        if maintenance_jobs is not None
        else resolve_maintenance_jobs(environ=environment, test_budget=budget)
    )
    return max(1, budget - maintenance)


def new_run_id() -> str:
    timestamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    return f"{timestamp}-{uuid.uuid4().hex[:8]}"


def _relative_or_absolute(path: Path) -> str:
    try:
        return path.relative_to(PROJECT_ROOT).as_posix()
    except ValueError:
        return str(path)


def _atomic_write_json(path: Path, payload: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary_path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w",
            encoding="utf-8",
            dir=path.parent,
            prefix=f".{path.name}.",
            suffix=".tmp",
            delete=False,
        ) as temporary:
            temporary_path = Path(temporary.name)
            json.dump(payload, temporary, indent=2, sort_keys=True)
            temporary.write("\n")
            temporary.flush()
            os.fsync(temporary.fileno())
        os.replace(temporary_path, path)
    except OSError:
        if temporary_path is not None:
            try:
                temporary_path.unlink()
            except OSError:
                pass
        raise


def _manifest_payload(
    run_id: str,
    run_dir: Path,
    results: dict[str, tuple[int | None, float]],
    status: str,
    test_budget: int,
    phase_jobs: int,
    maintenance_jobs: int,
    nextest_jobs: int,
) -> dict[str, object]:
    phases: dict[str, object] = {}
    for name, recipe in PHASES:
        if name in results:
            code, seconds = results[name]
            phase_status = "passed" if code == 0 else "failed"
        else:
            code, seconds = None, 0.0
            phase_status = "pending" if status == "running" else "missing"
        phases[name] = {
            "recipe": recipe,
            "status": phase_status,
            "exit_code": code,
            "seconds": round(seconds, 3),
            "log": _relative_or_absolute(run_dir / f"{name}.log"),
        }
    failures = [name for name, (code, _) in results.items() if code != 0]
    return {
        "run_id": run_id,
        "status": status,
        "manifest": _relative_or_absolute(run_dir / MANIFEST_NAME),
        "test_budget": test_budget,
        "phase_jobs": phase_jobs,
        "maintenance_jobs": maintenance_jobs,
        "nextest_jobs": nextest_jobs,
        "phases": phases,
        "failures": failures,
    }


def _write_manifest(
    manifest_path: Path,
    run_id: str,
    run_dir: Path,
    results: dict[str, tuple[int | None, float]],
    status: str,
    test_budget: int,
    phase_jobs: int,
    maintenance_jobs: int,
    nextest_jobs: int,
) -> None:
    _atomic_write_json(
        manifest_path,
        _manifest_payload(
            run_id,
            run_dir,
            results,
            status,
            test_budget,
            phase_jobs,
            maintenance_jobs,
            nextest_jobs,
        ),
    )


def run_phase(
    name: str,
    recipe: str,
    log_path: Path,
    command: list[str] | None = None,
    environment: Mapping[str, str] | None = None,
) -> tuple[int | None, float]:
    """运行单个阶段，输出实时带前缀转发并写入日志，返回 (退出码, 耗时秒)。

    默认执行 `just <recipe>`；command 参数供离线测试注入。启动失败（命令缺失
    等）返回 None，按失败处理；日志无法创建也返回 None，避免把不可审计的阶段当作成功。
    """
    if command is None:
        command = ["just", recipe]
    started = time.monotonic()
    prefix = f"[{name}] "
    try:
        log_path.parent.mkdir(parents=True, exist_ok=True)
        log = log_path.open("w", encoding="utf-8")
    except OSError as error:
        print(f"{prefix}日志启动失败: {error}", flush=True)
        return None, time.monotonic() - started

    with log:
        try:
            # 子进程（cargo、bun、git、已重配 UTF-8 的脚本）输出 UTF-8；按 locale 解码在
            # Windows CI（cp1252）遇到 0x90 等未定义字节会让转发线程崩溃、阶段结果丢失。
            process = subprocess.Popen(
                command,
                cwd=PROJECT_ROOT,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                encoding="utf-8",
                errors="replace",
                env=None if environment is None else dict(environment),
            )
        except OSError as error:
            message = f"阶段启动失败: {error}"
            print(prefix + message, flush=True)
            log.write(message + "\n")
            return None, time.monotonic() - started

        stream = process.stdout
        try:
            if stream is not None:
                for line in stream:
                    sys.stdout.write(prefix + line)
                    sys.stdout.flush()
                    log.write(line)
            code = process.wait()
        except OSError as error:
            message = f"阶段输出失败: {error}"
            print(prefix + message, flush=True)
            log.write(message + "\n")
            return None, time.monotonic() - started
        finally:
            if stream is not None:
                stream.close()
    return code, time.monotonic() - started


def run_all(
    phase_jobs: int | None = None,
    log_root: Path | None = None,
    run_id: str | None = None,
    phase_runner: Callable[[str, str, Path], tuple[int | None, float]] | None = None,
    test_budget: int | None = None,
    maintenance_jobs: int | None = None,
    nextest_jobs: int | None = None,
) -> dict[str, tuple[int | None, float]]:
    """Run all phases with a bounded number of concurrent workers.

    `phase_runner` keeps command injection available to script unit tests. `log_root` is
    the parent directory; each invocation gets its own `<run-id>` child directory.
    """
    global LAST_RUN_DIR

    phase_limit = resolve_phase_jobs(phase_jobs)
    budget = resolve_test_budget(test_budget)
    maintenance_limit = resolve_maintenance_jobs(maintenance_jobs, test_budget=budget)
    nextest_limit = resolve_nextest_jobs(
        nextest_jobs,
        maintenance_jobs=maintenance_limit,
        test_budget=budget,
    )
    phase_environment = os.environ.copy()
    phase_environment[MAINTENANCE_JOBS_ENV] = str(maintenance_limit)
    phase_environment[NEXTEST_JOBS_ENV] = str(nextest_limit)
    selected_run_id = run_id or new_run_id()
    run_dir = (log_root or LOG_DIR) / selected_run_id
    run_dir.mkdir(parents=True, exist_ok=False)
    LAST_RUN_DIR = run_dir
    manifest_path = run_dir / MANIFEST_NAME
    results: dict[str, tuple[int | None, float]] = {}
    manifest_lock = threading.Lock()
    if phase_runner is None:

        def runner(name: str, recipe: str, log_path: Path) -> tuple[int | None, float]:
            return run_phase(name, recipe, log_path, environment=phase_environment)

    else:
        runner = phase_runner
    _write_manifest(
        manifest_path,
        selected_run_id,
        run_dir,
        results,
        "running",
        budget,
        phase_limit,
        maintenance_limit,
        nextest_limit,
    )

    def worker(name: str, recipe: str) -> tuple[int | None, float]:
        return runner(name, recipe, run_dir / f"{name}.log")

    with concurrent.futures.ThreadPoolExecutor(max_workers=phase_limit) as pool:
        futures = {pool.submit(worker, name, recipe): name for name, recipe in PHASES}
        for future in concurrent.futures.as_completed(futures):
            name = futures[future]
            try:
                result = future.result()
            except Exception as error:  # noqa: BLE001 - a worker crash is a failed phase.
                print(f"[{name}] 阶段线程异常: {error}", flush=True)
                result = (None, 0.0)
            with manifest_lock:
                results[name] = result
                _write_manifest(
                    manifest_path,
                    selected_run_id,
                    run_dir,
                    results,
                    "running",
                    budget,
                    phase_limit,
                    maintenance_limit,
                    nextest_limit,
                )

    exit_code, _ = summarize(results)
    _write_manifest(
        manifest_path,
        selected_run_id,
        run_dir,
        results,
        "passed" if exit_code == 0 else "failed",
        budget,
        phase_limit,
        maintenance_limit,
        nextest_limit,
    )
    return results


def summarize(
    results: dict[str, tuple[int | None, float]],
) -> tuple[int, list[str]]:
    """聚合各阶段结果：任一失败或缺失返回 (1, 失败阶段名列表)。"""
    failures = [name for name, _ in PHASES if results.get(name, (None, 0.0))[0] != 0]
    return (1 if failures else 0), failures


def print_recap(name: str, log_dir: Path | None = None) -> None:
    selected_log_dir = log_dir or LAST_RUN_DIR or LOG_DIR
    log_path = selected_log_dir / f"{name}.log"
    print(f"\n----- [{name}] 失败输出尾部（完整日志 {log_path}）-----")
    try:
        lines = log_path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError as error:
        print(f"日志不可读: {error}")
        return
    for line in lines[-FAILURE_RECAP_LINES:]:
        print(line)


def _force_utf8_streams() -> None:
    # Windows CI 的 Python 默认 stdout 是 cp1252，打印中文会 UnicodeEncodeError；
    # 模块级生效：run_phase 等入口也会被测试与编排方直接调用。
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8", errors="replace")


_force_utf8_streams()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument(
        "--phase-jobs",
        "--jobs",
        dest="phase_jobs",
        type=int,
        help=f"并发测试阶段数（默认 {DEFAULT_PHASE_JOBS}；也可用 {PHASE_JOBS_ENV}）",
    )
    parser.add_argument(
        "--test-budget",
        type=int,
        help=f"combined test worker budget (default {DEFAULT_TEST_BUDGET}; also {TEST_BUDGET_ENV})",
    )
    parser.add_argument(
        "--maintenance-jobs",
        type=int,
        help=f"maintenance workers (also {MAINTENANCE_JOBS_ENV})",
    )
    parser.add_argument(
        "--nextest-jobs",
        type=int,
        help=f"nextest test threads (also {NEXTEST_JOBS_ENV})",
    )
    args = parser.parse_args(argv)
    _force_utf8_streams()
    try:
        phase_jobs = resolve_phase_jobs(args.phase_jobs)
        test_budget = resolve_test_budget(args.test_budget)
        maintenance_jobs = resolve_maintenance_jobs(args.maintenance_jobs, test_budget=test_budget)
        nextest_jobs = resolve_nextest_jobs(
            args.nextest_jobs,
            maintenance_jobs=maintenance_jobs,
            test_budget=test_budget,
        )
        results = run_all(
            phase_jobs=phase_jobs,
            test_budget=test_budget,
            maintenance_jobs=maintenance_jobs,
            nextest_jobs=nextest_jobs,
        )
    except (OSError, ValueError) as error:
        print(f"测试编排失败: {error}", file=sys.stderr, flush=True)
        return 2

    run_dir = LAST_RUN_DIR or LOG_DIR
    print(
        f"并行运行 {len(PHASES)} 个测试阶段（phase={phase_jobs}, maintenance={maintenance_jobs}, "
        f"nextest={nextest_jobs}, budget={test_budget}），日志目录: {run_dir}",
        flush=True,
    )
    print("\n===== 测试阶段汇总 =====")
    for name, _ in PHASES:
        code, seconds = results.get(name, (None, 0.0))
        status = "OK" if code == 0 else f"FAIL(exit={code})"
        print(f"{name:<20} {seconds:6.1f}s  {status}")
    exit_code, failures = summarize(results)
    for name in failures:
        print_recap(name, run_dir)
    if failures:
        print(f"\n失败阶段: {', '.join(failures)}")
    else:
        print("\n全部阶段通过。")
    print(f"结果 manifest: {run_dir / MANIFEST_NAME}")
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
