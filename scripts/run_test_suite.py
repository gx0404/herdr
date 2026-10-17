"""并行编排 `just test` 的五个测试阶段。

各阶段的命令真源在 justfile recipe（rust-test / maintenance-test /
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
import re
import signal
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
sys.path.insert(0, str(PROJECT_ROOT))
LOG_DIR = PROJECT_ROOT / "target" / "test-suite-logs"
FAILURE_RECAP_LINES = 40
PHASE_JOBS_ENV = "HERDR_TEST_PHASE_JOBS"
TEST_BUDGET_ENV = "HERDR_TEST_BUDGET"
MAINTENANCE_JOBS_ENV = "HERDR_MAINTENANCE_JOBS"
NEXTEST_JOBS_ENV = "HERDR_NEXTEST_JOBS"
MANIFEST_NAME = "manifest.json"

# (阶段名, 对应 just recipe 名)；顺序仅决定汇总打印顺序，执行受 phase jobs 限制。
PHASES: tuple[tuple[str, str], ...] = (
    ("nextest", "rust-test"),
    ("maintenance", "maintenance-test"),
    ("ui-hot-path", "ui-hot-path-architecture-test"),
    ("integration-assets", "integration-assets-test"),
    ("docs-contract", "docs-contract-test"),
)
MAX_DEFAULT_TEST_BUDGET = 8
MAX_DEFAULT_MAINTENANCE_JOBS = 4
DEFAULT_TEST_BUDGET = min(MAX_DEFAULT_TEST_BUDGET, max(1, os.cpu_count() or 1))

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


def resolve_phase_jobs(
    value: int | None = None,
    environ: Mapping[str, str] | None = None,
    test_budget: int | None = None,
) -> int:
    """Resolve the phase limit, with the command-line value taking precedence over the environment."""
    if value is not None:
        return _positive_int(value, "phase jobs")
    environment = os.environ if environ is None else environ
    raw = environment.get(PHASE_JOBS_ENV)
    if raw is not None:
        return _positive_int(raw, PHASE_JOBS_ENV)
    return min(3, len(PHASES), resolve_test_budget(test_budget, environment))


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
    phase_jobs: int | None = None,
) -> int:
    """Resolve maintenance workers, reserving a worker for nextest when phases overlap."""
    if value is not None:
        return _positive_int(value, "maintenance jobs")
    environment = os.environ if environ is None else environ
    raw = environment.get(MAINTENANCE_JOBS_ENV)
    if raw is not None:
        return _positive_int(raw, MAINTENANCE_JOBS_ENV)
    budget = resolve_test_budget(test_budget, environment)
    phases = resolve_phase_jobs(phase_jobs, environment, test_budget=budget)
    available = budget if phases == 1 else max(1, budget - 1)
    return min(MAX_DEFAULT_MAINTENANCE_JOBS, available)


def resolve_nextest_jobs(
    value: int | None = None,
    environ: Mapping[str, str] | None = None,
    maintenance_jobs: int | None = None,
    test_budget: int | None = None,
    phase_jobs: int | None = None,
) -> int:
    """Resolve nextest workers, sharing the default budget only when phases overlap."""
    if value is not None:
        return _positive_int(value, "nextest jobs")
    environment = os.environ if environ is None else environ
    raw = environment.get(NEXTEST_JOBS_ENV)
    if raw is not None:
        return _positive_int(raw, NEXTEST_JOBS_ENV)
    budget = resolve_test_budget(test_budget, environment)
    phases = resolve_phase_jobs(phase_jobs, environment, test_budget=budget)
    if phases == 1:
        return budget
    maintenance = (
        _positive_int(maintenance_jobs, "maintenance jobs")
        if maintenance_jobs is not None
        else resolve_maintenance_jobs(environ=environment, test_budget=budget, phase_jobs=phases)
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
    phase_started: Mapping[str, float],
) -> dict[str, object]:
    phases: dict[str, object] = {}
    for name, recipe in PHASES:
        if name in results:
            code, seconds = results[name]
            phase_status = "passed" if code == 0 else "failed"
        elif status == "running" and name in phase_started:
            code, seconds = None, time.monotonic() - phase_started[name]
            phase_status = "running"
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
    phase_started: Mapping[str, float],
    suite: str = "default",
    started: float | None = None,
) -> None:
    payload = _manifest_payload(
            run_id,
            run_dir,
            results,
            status,
            test_budget,
            phase_jobs,
            maintenance_jobs,
            nextest_jobs,
            phase_started,
        )
    payload["suite"] = suite
    payload["total_seconds"] = round(time.monotonic() - started, 3) if started is not None else None
    payload["target_seconds"] = 60 if suite == "default" else None
    payload["target_met"] = (payload["total_seconds"] <= 60) if status == "passed" and suite == "default" and started is not None else None
    payload["extended_tests"] = "included" if suite == "full" else "not_run"
    for name, (code, _) in results.items():
        payload["phases"][name]["counts"] = read_counts(run_dir / f"{name}.log") if code == 0 else None
    _atomic_write_json(manifest_path, payload)


def read_counts(path: Path) -> dict[str, int] | None:
    """Parse executed counts, keeping filtered/ignored tests separate from passes."""
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return None
    recorded = re.findall(r"HERDR_TEST_COUNTS (\{[^\n]+\})", text)
    if recorded:
        return json.loads(recorded[-1])
    nextest = re.findall(r"Summary .*? (\d+) tests run:.*?(\d+) passed(?:, (\d+) skipped)?", text)
    native = re.findall(r"test result: ok\. (\d+) passed; 0 failed; (\d+) ignored; \d+ measured; (\d+) filtered out", text)
    if nextest or native:
        return {"executed": sum(int(row[0]) for row in nextest + native),
                "skipped": sum(int(row[2] or 0) for row in nextest) + sum(int(row[1]) + int(row[2]) for row in native)}
    python = re.findall(r"^Ran (\d+) tests in .*?(?:\(skipped=(\d+)\))?$", text, re.MULTILINE)
    bun_pass = sum(map(int, re.findall(r"^\s*(\d+) pass$", text, re.MULTILINE)))
    bun_skip = sum(map(int, re.findall(r"^\s*(\d+) skip$", text, re.MULTILINE)))
    if not python and not re.search(r"^\s*\d+ pass$", text, re.MULTILINE):
        return None
    skipped = sum(int(skip or 0) for _, skip in python)
    return {"executed": sum(int(count) for count, _ in python) - skipped + bun_pass,
            "skipped": skipped + bun_skip}


def run_phase(
    name: str,
    recipe: str,
    log_path: Path,
    command: list[str] | None = None,
    environment: Mapping[str, str] | None = None,
    ownership: Callable | None = None,
    release_ownership: Callable | None = None,
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
        log = log_path.open("w", encoding="utf-8", buffering=1)
    except OSError as error:
        print(f"{prefix}日志启动失败: {error}", flush=True)
        return None, time.monotonic() - started

    with log:
        try:
            # 子进程（cargo、bun、git、已重配 UTF-8 的脚本）输出 UTF-8；按 locale 解码在
            # Windows CI（cp1252）遇到 0x90 等未定义字节会让转发线程崩溃、阶段结果丢失。
            arguments = dict(
                cwd=PROJECT_ROOT,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                encoding="utf-8",
                errors="replace",
                env=None if environment is None else dict(environment),
            )
            owner = ownership(command, arguments) if ownership is not None else None
            process = owner.process if owner is not None else subprocess.Popen(command, **arguments)
        except OSError as error:
            message = f"阶段启动失败: {error}"
            log.write(message + "\n")
            print(prefix + message, flush=True)
            return None, time.monotonic() - started

        stream = process.stdout
        try:
            if stream is not None:
                for line in stream:
                    log.write(line)
                    log.flush()
                    sys.stdout.write(prefix + line)
                    sys.stdout.flush()
            code = process.wait()
        except BaseException as error:
            message = f"阶段输出失败: {error}"
            if owner is not None:
                owner.stop()
            elif process.poll() is None:
                process.terminate()
                process.wait(timeout=15)
            try:
                print(prefix + message, flush=True)
                log.write(message + "\n")
            except OSError:
                pass  # Original I/O failure still makes this phase fail.
            if not isinstance(error, Exception):
                raise
            return None, time.monotonic() - started
        finally:
            if stream is not None:
                stream.close()
            if owner is not None:
                if release_ownership is not None:
                    release_ownership(owner)
                else:
                    owner.close()
    return code, time.monotonic() - started


def run_all(
    phase_jobs: int | None = None,
    log_root: Path | None = None,
    run_id: str | None = None,
    phase_runner: Callable[[str, str, Path], tuple[int | None, float]] | None = None,
    test_budget: int | None = None,
    maintenance_jobs: int | None = None,
    nextest_jobs: int | None = None,
    suite: str = "default",
    filterset: str = "all()",
) -> dict[str, tuple[int | None, float]]:
    """Run all phases with a bounded number of concurrent workers.

    `phase_runner` keeps command injection available to script unit tests. `log_root` is
    the parent directory; each invocation gets its own `<run-id>` child directory.
    """
    global LAST_RUN_DIR
    if suite not in {"default", "full"}:
        raise ValueError("unknown test suite")
    started_at = time.monotonic()

    budget = resolve_test_budget(test_budget)
    phase_limit = resolve_phase_jobs(phase_jobs, test_budget=budget)
    maintenance_limit = resolve_maintenance_jobs(
        maintenance_jobs, test_budget=budget, phase_jobs=phase_limit
    )
    nextest_limit = resolve_nextest_jobs(
        nextest_jobs,
        maintenance_jobs=maintenance_limit,
        test_budget=budget,
        phase_jobs=phase_limit,
    )
    phase_environment = os.environ.copy()
    phase_environment[MAINTENANCE_JOBS_ENV] = str(maintenance_limit)
    phase_environment[NEXTEST_JOBS_ENV] = str(nextest_limit)
    phase_environment["HERDR_TEST_SUITE"] = suite
    phase_environment["HERDR_TEST_FILTER"] = filterset
    selected_run_id = run_id or new_run_id()
    run_dir = (log_root or LOG_DIR) / selected_run_id
    run_dir.mkdir(parents=True, exist_ok=False)
    LAST_RUN_DIR = run_dir
    manifest_path = run_dir / MANIFEST_NAME
    results: dict[str, tuple[int | None, float]] = {}
    phase_started: dict[str, float] = {}
    manifest_lock = threading.Lock()
    owner_lock = threading.Lock()
    owners = []
    cancelled = threading.Event()

    def ownership(command, arguments):
        from scripts.run_nextest import OwnedProcess
        with owner_lock:
            if cancelled.is_set():
                raise OSError("suite cancelled before phase admission")
            owner = OwnedProcess(command, **arguments)
            owners.append(owner)
            return owner

    def release_ownership(owner):
        # Serialize close with cancellation: a closed Windows Job must never be
        # mistaken for a POSIX process group by the concurrent cancellation path.
        with owner_lock:
            owners.remove(owner)
            owner.close()
    if phase_runner is None:

        def runner(name: str, recipe: str, log_path: Path) -> tuple[int | None, float]:
            return run_phase(name, recipe, log_path, environment=phase_environment,
                             ownership=ownership, release_ownership=release_ownership)

    else:
        runner = phase_runner

    def write_manifest(status: str = "running") -> None:
        _write_manifest(
            manifest_path,
            selected_run_id,
            run_dir,
            results,
            status,
            budget,
            phase_limit,
            maintenance_limit,
            nextest_limit,
            phase_started,
            suite,
            started_at,
        )

    write_manifest()
    print(
        f"测试编排启动（phase={phase_limit}, maintenance={maintenance_limit}, "
        f"nextest={nextest_limit}, budget={budget}），结果 manifest: {manifest_path}",
        flush=True,
    )

    def worker(name: str, recipe: str) -> None:
        started = time.monotonic()
        with manifest_lock:
            phase_started[name] = started
            write_manifest()
        try:
            result = runner(name, recipe, run_dir / f"{name}.log")
            if result[0] == 0:
                # A zero exit without its evidence file is not an auditable pass.
                with (run_dir / f"{name}.log").open("rb") as evidence:
                    evidence.read(1)
        except Exception as error:  # noqa: BLE001 - a worker crash is a failed phase.
            print(f"[{name}] 阶段线程异常: {error}", flush=True)
            result = (None, time.monotonic() - started)
        with manifest_lock:
            results[name] = result
            write_manifest()

    pool = concurrent.futures.ThreadPoolExecutor(max_workers=phase_limit)
    try:
        futures = [pool.submit(worker, name, recipe) for name, recipe in PHASES]
        for future in concurrent.futures.as_completed(futures):
            future.result()
    except BaseException:
        cancelled.set()
        with owner_lock:
            for owner in owners:
                if owner.process.poll() is None:
                    try:
                        owner.stop()
                    except OSError as error:
                        print(f"phase cleanup failed: {error}", flush=True)
        pool.shutdown(wait=True, cancel_futures=True)
        write_manifest("cancelled")
        raise
    else:
        pool.shutdown(wait=True)

    exit_code, _ = summarize(results)
    write_manifest("passed" if exit_code == 0 else "failed")
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
    parser.add_argument("--suite", choices=("default", "full"), default=os.environ.get("HERDR_TEST_SUITE", "default"))
    parser.add_argument("--filterset", default="all()", help="additional platform selection (CI)")
    parser.add_argument(
        "--phase-jobs",
        "--jobs",
        dest="phase_jobs",
        type=int,
        help=f"并发测试阶段数（默认 min(3, {len(PHASES)}, test_budget)；也可用 {PHASE_JOBS_ENV}）",
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
        test_budget = resolve_test_budget(args.test_budget)
        phase_jobs = resolve_phase_jobs(args.phase_jobs, test_budget=test_budget)
        maintenance_jobs = resolve_maintenance_jobs(
            args.maintenance_jobs, test_budget=test_budget, phase_jobs=phase_jobs
        )
        nextest_jobs = resolve_nextest_jobs(
            args.nextest_jobs,
            maintenance_jobs=maintenance_jobs,
            test_budget=test_budget,
            phase_jobs=phase_jobs,
        )
        results = run_all(
            phase_jobs=phase_jobs,
            test_budget=test_budget,
            maintenance_jobs=maintenance_jobs,
            nextest_jobs=nextest_jobs,
            suite=args.suite,
            filterset=args.filterset,
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
        print("\n完整回归通过。" if args.suite == "full" else "\n默认回归通过；扩展测试未执行（just test-full）。")
    print(f"结果 manifest: {run_dir / MANIFEST_NAME}")
    return exit_code


if __name__ == "__main__":
    def interrupted(signum, frame):
        raise KeyboardInterrupt(f"received signal {signum}")
    signal.signal(signal.SIGTERM, interrupted)
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        print("test suite cancelled; incomplete phases are not passes", file=sys.stderr)
        sys.exit(130)
