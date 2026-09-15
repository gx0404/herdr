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


def _repo_root() -> Path | None:
    result = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=False
    )
    if result.returncode == 0 and result.stdout.strip():
        return Path(result.stdout.strip())
    return None


def _changed_files(root: Path) -> list[str]:
    result = subprocess.run(
        ["git", "-C", str(root), "status", "--porcelain"],
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
        changed.append(entry.strip('"'))
    return sorted(set(changed))


def main() -> int:
    root = _repo_root()
    if root is None:
        return 0
    try:
        review_dir = root / ".claude" / "review"
        review_dir.mkdir(parents=True, exist_ok=True)
        payload = {
            "ready": True,
            "written_at": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
            "changed": _changed_files(root),
        }
        (review_dir / "READY.json").write_text(
            json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
    except OSError:
        return 0
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
