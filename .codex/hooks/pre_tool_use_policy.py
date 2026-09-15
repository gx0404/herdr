#!/usr/bin/env python3
"""Codex PreToolUse 适配器：复用 .claude/hooks 的策略真源与判定逻辑。

协议分叉只做适配，不复制策略；探针见 scripts/test_ai_tool_hooks.py。
"""

from __future__ import annotations

import importlib.util
import subprocess
import sys
from pathlib import Path


def _locate_gate() -> Path | None:
    result = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=False
    )
    root = result.stdout.strip() if result.returncode == 0 else ""
    if not root:
        return None
    gate = Path(root) / ".claude" / "hooks" / "pre_tool_use_gate.py"
    return gate if gate.is_file() else None


def main() -> int:
    gate_path = _locate_gate()
    if gate_path is None:
        return 0
    spec = importlib.util.spec_from_file_location("herdr_pre_tool_use_gate", gate_path)
    if spec is None or spec.loader is None:
        return 0
    gate = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(gate)
    return gate.main(["--protocol", "codex"])


if __name__ == "__main__":
    raise SystemExit(main())
