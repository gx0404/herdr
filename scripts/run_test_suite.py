"""并行编排 `just test` 的五个测试阶段。

各阶段的命令真源在 justfile recipe（nextest-all / maintenance-test /
ui-hot-path-architecture-test / integration-assets-test / docs-contract-test），
本脚本只负责并发调度、实时前缀转发输出、日志落盘与失败聚合；任一阶段失败
即整体非零退出，工具链缺失记为该阶段失败，不整族跳过。
"""

from __future__ import annotations

import subprocess
import sys
import threading
import time
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parent.parent
LOG_DIR = PROJECT_ROOT / "target" / "test-suite-logs"
FAILURE_RECAP_LINES = 40

# (阶段名, 对应 just recipe 名)；顺序仅决定汇总打印顺序，执行是并发的。
PHASES: tuple[tuple[str, str], ...] = (
    ("nextest", "nextest-all"),
    ("maintenance", "maintenance-test"),
    ("ui-hot-path", "ui-hot-path-architecture-test"),
    ("integration-assets", "integration-assets-test"),
    ("docs-contract", "docs-contract-test"),
)


def phase_names() -> list[str]:
    return [name for name, _ in PHASES]


def run_phase(
    name: str,
    recipe: str,
    log_path: Path,
    command: list[str] | None = None,
) -> tuple[int | None, float]:
    """运行单个阶段，输出实时带前缀转发并写入日志，返回 (退出码, 耗时秒)。

    默认执行 `just <recipe>`；command 参数供离线测试注入。启动失败（命令缺失
    等）返回 None，按失败处理。
    """
    if command is None:
        command = ["just", recipe]
    started = time.monotonic()
    prefix = f"[{name}] "
    try:
        process = subprocess.Popen(
            command,
            cwd=PROJECT_ROOT,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
    except OSError as error:
        print(f"{prefix}阶段启动失败: {error}", flush=True)
        return None, time.monotonic() - started

    with log_path.open("w", encoding="utf-8") as log:
        stream = process.stdout
        if stream is not None:
            for line in stream:
                sys.stdout.write(prefix + line)
                sys.stdout.flush()
                log.write(line)
        code = process.wait()
    return code, time.monotonic() - started


def run_all() -> dict[str, tuple[int | None, float]]:
    LOG_DIR.mkdir(parents=True, exist_ok=True)
    results: dict[str, tuple[int | None, float]] = {}
    threads: list[threading.Thread] = []
    lock = threading.Lock()

    def worker(name: str, recipe: str) -> None:
        result = run_phase(name, recipe, LOG_DIR / f"{name}.log")
        with lock:
            results[name] = result

    for name, recipe in PHASES:
        thread = threading.Thread(target=worker, args=(name, recipe))
        thread.start()
        threads.append(thread)
    for thread in threads:
        thread.join()
    return results


def summarize(
    results: dict[str, tuple[int | None, float]],
) -> tuple[int, list[str]]:
    """聚合各阶段结果：任一失败返回 (1, 失败阶段名列表)，否则 (0, [])。"""
    failures = [
        name for name, (code, _) in results.items() if code != 0
    ]
    return (1 if failures else 0), failures


def print_recap(name: str) -> None:
    log_path = LOG_DIR / f"{name}.log"
    print(f"\n----- [{name}] 失败输出尾部（完整日志 {log_path}）-----")
    lines = log_path.read_text(encoding="utf-8", errors="replace").splitlines()
    for line in lines[-FAILURE_RECAP_LINES:]:
        print(line)


def main() -> int:
    print(
        f"并行运行 {len(PHASES)} 个测试阶段，日志目录: {LOG_DIR}",
        flush=True,
    )
    results = run_all()
    print("\n===== 测试阶段汇总 =====")
    for name, _ in PHASES:
        code, seconds = results[name]
        status = "OK" if code == 0 else f"FAIL(exit={code})"
        print(f"{name:<20} {seconds:6.1f}s  {status}")
    exit_code, failures = summarize(results)
    for name in failures:
        print_recap(name)
    if failures:
        print(f"\n失败阶段: {', '.join(failures)}")
    else:
        print("\n全部阶段通过。")
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
