#!/usr/bin/env python3
"""Stop 钩子：写 <root>/.claude/review/READY.json 供跨工具复审闭环消费。

信号文件包含当前变更文件清单；幂等，失败静默（Stop 不得阻塞会话结束）。
"""

from __future__ import annotations

import datetime
import json
import subprocess
import sys
from pathlib import Path

# 组件根按本文件位置（<组件>/.claude/hooks/）推导：单仓内 git 顶层是单仓根，不是本组件。
COMPONENT_ROOT = Path(__file__).resolve().parents[2]


def _component_prefix(root: Path) -> str | None:
    """组件在 Git 工作树内的路径前缀（单仓内为 `herdr/`，独立仓为空）；不在工作树内返回 None。"""
    result = subprocess.run(
        ["git", "-C", str(root), "rev-parse", "--show-prefix"],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        return None
    return result.stdout.strip()


def _changed_files(root: Path, prefix: str) -> list[str]:
    # porcelain 路径恒相对 git 顶层：限定到组件目录并去掉组件前缀，清单保持组件相对。
    result = subprocess.run(
        ["git", "-C", str(root), "status", "--porcelain", "--", "."],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        return []
    changed: list[str] = []
    for line in result.stdout.splitlines():
        entry = line[3:].strip()
        if not entry:
            continue
        changed.append(" -> ".join(part.strip('"').removeprefix(prefix) for part in entry.split(" -> ")))
    return sorted(set(changed))


def main() -> int:
    # Windows cp1252 控制台打印中文决策理由会崩溃，安全门必须稳定输出。
    for _stream in (sys.stdout, sys.stderr):
        if hasattr(_stream, "reconfigure"):
            _stream.reconfigure(encoding="utf-8", errors="replace")
    root = COMPONENT_ROOT
    prefix = _component_prefix(root)
    if prefix is None:
        return 0
    try:
        review_dir = root / ".claude" / "review"
        review_dir.mkdir(parents=True, exist_ok=True)
        payload = {
            "ready": True,
            "written_at": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
            "changed": _changed_files(root, prefix),
        }
        (review_dir / "READY.json").write_text(
            json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
    except OSError:
        return 0
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
