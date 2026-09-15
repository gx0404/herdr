#!/usr/bin/env python3
"""PreToolUse 安全门：消费 dangerous_patterns.conf，判定后按协议输出决策。

策略真源只有 conf 一份；本脚本与 .codex 适配器都不自带模式。
输入（stdin JSON）：{"tool_name": "...", "tool_input": {...}}。
输出：无命中时不输出（默认放行）；命中 deny/ask 时输出对应协议的决策 JSON。
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

CONF_PATH = Path(__file__).resolve().parent / "dangerous_patterns.conf"
VALID_SECTIONS = {"SHELL", "FILE"}
VALID_LEVELS = {"deny", "ask"}
FILE_TOOL_KEYS = ("file_path", "notebook_path")
WRITE_TOOLS = {"Edit", "Write", "NotebookEdit"}


def _repo_root() -> Path | None:
    """定位仓库根，用于把工具传入的绝对 file_path 相对化后再匹配。"""
    import subprocess

    result = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=False
    )
    if result.returncode == 0 and result.stdout.strip():
        return Path(result.stdout.strip())
    return None


class PatternError(ValueError):
    """dangerous_patterns.conf 结构非法。"""


def load_patterns(conf_path: Path = CONF_PATH) -> list[tuple[str, re.Pattern[str], str, str]]:
    patterns: list[tuple[str, re.Pattern[str], str, str]] = []
    for line_no, raw in enumerate(conf_path.read_text(encoding="utf-8").splitlines(), start=1):
        line = raw.rstrip("\n")
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        parts = line.split("\t")
        if len(parts) < 3:
            raise PatternError(f"conf 第 {line_no} 行列数不足：{line!r}")
        section, regex, reason = parts[0].strip().upper(), parts[1], parts[2].strip()
        level = parts[3].strip().lower() if len(parts) > 3 and parts[3].strip() else "deny"
        if section not in VALID_SECTIONS:
            raise PatternError(f"conf 第 {line_no} 行 SECTION 非法：{section}")
        if level not in VALID_LEVELS:
            raise PatternError(f"conf 第 {line_no} 行级别非法：{level}")
        try:
            compiled = re.compile(regex)
        except re.error as exc:
            raise PatternError(f"conf 第 {line_no} 行正则非法：{regex}") from exc
        patterns.append((section, compiled, reason, level))
    if not patterns:
        raise PatternError("conf 未声明任何模式")
    return patterns


def evaluate(
    tool_name: str, tool_input: dict, patterns, repo_root: Path | None = None
) -> tuple[str | None, str | None]:
    """返回 (级别, 理由)；无命中返回 (None, None)。"""
    command = ""
    files: list[str] = []
    if tool_name == "Bash":
        if isinstance(tool_input.get("command"), str):
            command = tool_input["command"]
    elif tool_name in WRITE_TOOLS:
        # FILE 模式只约束写入面；Read/Grep 等只读工具不受限。
        for key in FILE_TOOL_KEYS:
            if isinstance(tool_input.get(key), str):
                files.append(tool_input[key])
    # 真实工具常传绝对路径；FILE 模式以仓库相对路径锚定，先相对化再匹配，
    # 否则整段 FILE 门对绝对路径失效。仓库外路径按原样匹配（不会命中 ^ 锚定模式）。
    normalized_files: list[str] = []
    for subject in files:
        candidate = Path(subject)
        if candidate.is_absolute():
            try:
                if repo_root is not None:
                    subject = candidate.resolve().relative_to(repo_root).as_posix()
            except ValueError:
                pass
        normalized_files.append(subject)
    for section, pattern, reason, level in patterns:
        if section == "SHELL":
            if command and pattern.search(command):
                return level, f"{reason}（命中命令片段）"
        else:
            for subject in normalized_files:
                if pattern.search(subject):
                    return level, f"{reason}（命中文件：{subject}）"
    return None, None


def _decision_payload(level: str, reason: str, protocol: str) -> dict:
    decision = "ask" if level == "ask" else "deny"
    if protocol == "codex":
        return {"permissionDecision": decision, "reason": reason}
    return {
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": decision,
            "permissionDecisionReason": reason,
        }
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--protocol", choices=("claude", "codex"), default="claude")
    args = parser.parse_args(argv)
    try:
        payload = json.load(sys.stdin)
    except json.JSONDecodeError:
        payload = {}
    tool_name = payload.get("tool_name") if isinstance(payload, dict) else ""
    tool_input = payload.get("tool_input") if isinstance(payload, dict) else None
    if not isinstance(tool_input, dict):
        tool_input = {}
    try:
        patterns = load_patterns()
    except (OSError, PatternError) as exc:
        # 安全门自身损坏时对写面 fail-closed，对其余工具不拦截。
        print(f"error: dangerous_patterns.conf: {exc}", file=sys.stderr)
        return 2 if tool_name in {"Bash", "Edit", "Write"} else 0
    level, reason = evaluate(str(tool_name), tool_input, patterns, repo_root=_repo_root())
    if level is not None:
        print(json.dumps(_decision_payload(level, reason or "命中危险模式", args.protocol), ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
