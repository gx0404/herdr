#!/usr/bin/env python3
"""PostToolUse(Edit|Write)：把编辑事件追加到 <root>/.claude/live-edits.log。

只做记录，不拦截、不修改源码；任何失败都静默退出（PostToolUse 不得阻塞）。
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


def main() -> int:
    try:
        payload = json.load(sys.stdin)
    except json.JSONDecodeError:
        return 0
    if not isinstance(payload, dict):
        return 0
    root = _repo_root()
    if root is None:
        return 0
    tool = str(payload.get("tool_name", ""))
    tool_input = payload.get("tool_input")
    target = ""
    if isinstance(tool_input, dict):
        for key in ("file_path", "notebook_path"):
            if isinstance(tool_input.get(key), str):
                target = tool_input[key]
                break
    if not target:
        return 0
    try:
        log_dir = root / ".claude"
        log_dir.mkdir(parents=True, exist_ok=True)
        stamp = datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds")
        with (log_dir / "live-edits.log").open("a", encoding="utf-8") as handle:
            handle.write(f"{stamp}\t{tool}\t{target}\n")
    except OSError:
        return 0
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
