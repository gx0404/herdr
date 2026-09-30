#!/usr/bin/env python3
"""Codex PreToolUse 适配器：复用 .claude/hooks 的策略真源与判定逻辑。

协议分叉只做适配，不复制策略；探针见 scripts/test_ai_tool_hooks.py。
"""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path


def _locate_gate() -> Path | None:
    # 按本文件位置（<组件>/.codex/hooks/）定位组件根：单仓内 git 顶层是单仓根，不是本组件。
    gate = Path(__file__).resolve().parents[2] / ".claude" / "hooks" / "pre_tool_use_gate.py"
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
