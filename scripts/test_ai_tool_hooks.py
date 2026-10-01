#!/usr/bin/env python3
"""AI 工具面安全门探针：无副作用输入断言允许/拒绝，防止门空转或误杀。"""

from __future__ import annotations

import json
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[1]
GATE = PROJECT_ROOT / ".claude" / "hooks" / "pre_tool_use_gate.py"
GATE_SH = PROJECT_ROOT / ".claude" / "hooks" / "block_dangerous.sh"
CODEX_ADAPTER = PROJECT_ROOT / ".codex" / "hooks" / "pre_tool_use_policy.py"
RECORD_EDIT = PROJECT_ROOT / ".claude" / "hooks" / "record_edit.py"
NOTIFY_REVIEW = PROJECT_ROOT / ".claude" / "hooks" / "notify_review.py"


def _run_gate(tool: str, payload: dict, *extra: str) -> subprocess.CompletedProcess[bytes]:
    stdin = json.dumps({"tool_name": tool, "tool_input": payload})
    return subprocess.run(
        [sys.executable, str(GATE), *extra],
        input=stdin.encode("utf-8"),
        capture_output=True,
        check=False,
    )


def _decision(result: subprocess.CompletedProcess[bytes]) -> tuple[str, str]:
    body = result.stdout.decode("utf-8").strip()
    if not body:
        return "allow", ""
    payload = json.loads(body)
    hook_output = payload.get("hookSpecificOutput", {})
    return hook_output.get("permissionDecision", ""), hook_output.get("permissionDecisionReason", "")


class DangerousPatternConfTests(unittest.TestCase):
    def test_conf_parses_fully(self) -> None:
        import importlib.util

        spec = importlib.util.spec_from_file_location("herdr_gate", GATE)
        gate = importlib.util.module_from_spec(spec)
        assert spec.loader is not None
        spec.loader.exec_module(gate)
        patterns = gate.load_patterns()
        self.assertGreaterEqual(len(patterns), 10)
        for _section, pattern, reason, level in patterns:
            self.assertTrue(reason.strip())
            self.assertIn(level, {"deny", "ask"})


class PreToolUseGateTests(unittest.TestCase):
    DENY_COMMANDS = [
        "git push origin master --force",
        "git push -f origin master",
        "git commit -m x --no-verify",
        "git commit -n -m x",
        "git add -f .claude/secrets",
        "gh pr merge 12",
        "gh release create v9.9.9",
        "cargo publish --dry-run",
        "echo '{}' > distribution/latest.json",
        "sed -i s/x/y/ distribution/preview.json",
        "cat ~/.ssh/id_rsa",
        # 命令位置的各种真执行形态都必须拦住
        "just check && git push origin master --force",
        "git -C /tmp/x push -f origin master",
        "bash -c 'gh pr merge 12'",
        "GH_TOKEN=x gh release create v9.9.9",
        "bash <<'EOF'\ncargo publish\nEOF",
        "echo '{}' | sudo tee distribution/latest.json",
        "if true; then git commit -m x --no-verify; fi",
    ]
    ASK_COMMANDS = [
        "git push origin feature/x --force-with-lease",
        "pkill -9 herdr",
        "cargo build && pkill herdr",
        "sudo killall herdr",
        "pgrep herdr | xargs -r pkill -f",
        "bash <<'EOF'\npkill herdr\nEOF",
        "bash -c 'pkill herdr'",
        "HERDR_SESSION=x pkill herdr",
        "if true; then git push origin x --force-with-lease; fi",
    ]
    ALLOW_COMMANDS = [
        "cargo nextest run --locked",
        "just check",
        "python3 scripts/resolve_agent_rules.py --check",
        "git push origin feature/gx_herdr",
        "git commit -m 'fix: pane focus'",
        # 常规暂存已按 2026-09 用户基线放行（原 ask 规则已删）。
        "git add -A",
        "git add --all",
        "cd /tmp && git add -A",
        "python3 -m unittest scripts.test_changelog",
        "rg 'fn main' src/",
        # 文本提及不是执行：ask 级模式只在命令位置命中。
        "rg pkill docs/",
        "python3 - <<'PYEOF'\ntext = '禁 pkill/猜 PID'\nprint(len(text))\nPYEOF",
        "python3 - <<'PYEOF'\ntext = '只精确暂存，禁 git add -A'\nPYEOF",
        "python3 - <<'PYEOF'\ntext = '需人工确认：git push --force-with-lease'\nPYEOF",
        "git commit -m 'docs: 说明为何不用 git add --all'",
        "rg 'gh pr merge' docs/",
        "python3 - <<'PYEOF'\ntext = '禁 git push --force、gh pr merge 与 cargo publish'\nPYEOF",
        "python3 - <<'PYEOF'\ntext = '勿用 sed 改 distribution/latest.json，勿 cat ~/.ssh/id_rsa'\nPYEOF",
        "git commit -m 'docs: 解释为何禁止 git push --force 与 gh release create'",
    ]
    DENY_FILES = [
        "distribution/latest.json",
        "distribution/preview.json",
        "docs/versions/0.9.0/website/src/content/docs/index.mdx",
        "docs/preview/website/src/content/docs/index.mdx",
        "docs/next/CHANGELOG.md",
        "CHANGELOG.md",
        "vendor/libghostty-vt/src/main.zig",
        ".github/MAINTAINERS",
        ".env",
        "config/.env.local",
    ]
    ALLOW_FILES = [
        "src/app/state.rs",
        "docs/AGENT_RULES/routes.toml",
        "docs/next/website/src/content/docs/index.mdx",
        "distribution/agent-detection/index.toml",
    ]

    def test_deny_commands_blocked(self) -> None:
        for command in self.DENY_COMMANDS:
            with self.subTest(command=command):
                decision, reason = _decision(_run_gate("Bash", {"command": command}))
                self.assertEqual("deny", decision, f"{command}: {reason}")

    def test_ask_commands_escalate(self) -> None:
        for command in self.ASK_COMMANDS:
            with self.subTest(command=command):
                decision, _ = _decision(_run_gate("Bash", {"command": command}))
                self.assertEqual("ask", decision)

    def test_allow_commands_pass(self) -> None:
        for command in self.ALLOW_COMMANDS:
            with self.subTest(command=command):
                decision, _ = _decision(_run_gate("Bash", {"command": command}))
                self.assertEqual("allow", decision)

    def test_deny_files_blocked_for_edit_and_write(self) -> None:
        for file_path in self.DENY_FILES:
            for tool in ("Edit", "Write"):
                with self.subTest(tool=tool, file_path=file_path):
                    decision, _ = _decision(_run_gate(tool, {"file_path": file_path}))
                    self.assertEqual("deny", decision)

    def test_absolute_paths_are_relativized_before_matching(self) -> None:
        # 真实工具按其契约传绝对路径；未相对化会让整段 FILE 门失效。
        for relative in self.DENY_FILES:
            with self.subTest(file_path=relative):
                decision, _ = _decision(
                    _run_gate("Edit", {"file_path": str(PROJECT_ROOT / relative)})
                )
                self.assertEqual("deny", decision)
        for relative in self.ALLOW_FILES:
            with self.subTest(allowed=relative):
                decision, _ = _decision(
                    _run_gate("Edit", {"file_path": str(PROJECT_ROOT / relative)})
                )
                self.assertEqual("allow", decision)

    def test_outside_repo_absolute_path_passes_without_crash(self) -> None:
        decision, _ = _decision(_run_gate("Edit", {"file_path": "/tmp/notes/CHANGELOG.md"}))
        self.assertEqual("allow", decision)

    def test_allow_files_pass(self) -> None:
        for file_path in self.ALLOW_FILES:
            with self.subTest(file_path=file_path):
                decision, _ = _decision(_run_gate("Edit", {"file_path": file_path}))
                self.assertEqual("allow", decision)

    def test_unknown_tool_passes(self) -> None:
        decision, _ = _decision(_run_gate("Read", {"file_path": "CHANGELOG.md"}))
        self.assertEqual("allow", decision)

    def test_bash_wrapper_matches_python_gate(self) -> None:
        # 按 PATH 解析 bash：Windows 上裸 "bash" 经 CreateProcess 先命中
        # System32\bash.exe（WSL 启动器，CI runner 无发行版），而不是 Git Bash。
        bash = shutil.which("bash")
        self.assertIsNotNone(bash, "bash is required to exercise the hook wrapper")
        stdin = json.dumps({"tool_name": "Bash", "tool_input": {"command": "gh pr merge 1"}})
        result = subprocess.run(
            [bash, str(GATE_SH)], input=stdin.encode("utf-8"), capture_output=True, check=False
        )
        self.assertEqual(0, result.returncode, result.stderr.decode("utf-8", "replace"))
        payload = json.loads(result.stdout.decode("utf-8"))
        self.assertEqual(
            "deny", payload["hookSpecificOutput"]["permissionDecision"]
        )

    def test_codex_protocol_shape(self) -> None:
        stdin = json.dumps({"tool_name": "Bash", "tool_input": {"command": "gh pr merge 1"}})
        result = subprocess.run(
            [sys.executable, str(GATE), "--protocol", "codex"],
            input=stdin.encode("utf-8"),
            capture_output=True,
            check=False,
        )
        payload = json.loads(result.stdout.decode("utf-8"))
        self.assertEqual("deny", payload["hookSpecificOutput"]["permissionDecision"])
        self.assertIn("permissionDecisionReason", payload["hookSpecificOutput"])

    def test_codex_adapter_blocks_in_repo(self) -> None:
        stdin = json.dumps({"tool_name": "Bash", "tool_input": {"command": "git push --force"}})
        result = subprocess.run(
            [sys.executable, str(CODEX_ADAPTER)],
            input=stdin.encode("utf-8"),
            capture_output=True,
            check=False,
            cwd=PROJECT_ROOT,
        )
        self.assertEqual(0, result.returncode)
        payload = json.loads(result.stdout.decode("utf-8"))
        self.assertEqual("deny", payload["hookSpecificOutput"]["permissionDecision"])

    def test_codex_never_emits_unsupported_ask_decision(self) -> None:
        stdin = json.dumps({"tool_name": "Bash", "tool_input": {"command": "git push --force-with-lease"}})
        result = subprocess.run([sys.executable, str(GATE), "--protocol", "codex"], input=stdin.encode(), capture_output=True, check=False)
        self.assertEqual(0, result.returncode)
        decision = json.loads(result.stdout)["hookSpecificOutput"]
        self.assertEqual("deny", decision["permissionDecision"])
        self.assertIn("不支持 ask", decision["permissionDecisionReason"])

    def test_codex_regular_validation_does_not_trigger_hook_approval(self) -> None:
        for command in ["mkdir -p /var/tmp/herdr-repro", "cargo nextest run --locked", "python3 .local/repro/validate.py capture monitor"]:
            stdin = json.dumps({"tool_name": "Bash", "tool_input": {"command": command}})
            result = subprocess.run([sys.executable, str(GATE), "--protocol", "codex"], input=stdin.encode(), capture_output=True, check=False)
            self.assertEqual(0, result.returncode)
            self.assertEqual(b"", result.stdout)

    def test_malformed_input_does_not_crash(self) -> None:
        result = subprocess.run(
            [sys.executable, str(GATE)], input=b"not json", capture_output=True, check=False
        )
        self.assertEqual(0, result.returncode)


class CodexConfigShapeTests(unittest.TestCase):
    """锁定真实 Codex CLI 的 config.toml 形状（v0.154 实测）。

    事件键必须是数组表（[[hooks.PreToolUse]]），命令嵌套在其 .hooks 数组；
    写成 [hooks.PreToolUse] 单表会让 Codex 启动即报
    `invalid type: map, expected a sequence` 并闪退。
    """

    def _load(self) -> dict:
        # Python 3.11+ 自带 tomllib；旧解释器与未装 tomli 的环境（CI 容器）
        # 都能解析：优先标准库，再退 tomli，都没有时跳过并明示原因。
        try:
            import tomllib as _toml  # type: ignore[no-redef]
        except ModuleNotFoundError:
            import tomli as _toml  # type: ignore[no-redef]

        payload = _toml.loads((PROJECT_ROOT / ".codex" / "config.toml").read_text(encoding="utf-8"))
        return payload

    def test_hook_events_are_arrays_with_nested_command_arrays(self) -> None:
        payload = self._load()
        hooks = payload["hooks"]
        for event in ("PreToolUse", "PostToolUse", "Stop"):
            self.assertIsInstance(hooks[event], list, event)
            for entry in hooks[event]:
                if event != "Stop":
                    # Stop 是会话结束事件，无工具可过滤，不需要 matcher。
                    self.assertIn("matcher", entry)
                self.assertIsInstance(entry["hooks"], list)
                command = entry["hooks"][0]
                self.assertEqual("command", command["type"])
                self.assertIn("command", command)
                self.assertIn("timeout", command)

    def test_agents_registration_uses_config_file(self) -> None:
        payload = self._load()
        reviewer = payload["agents"]["herdr_reviewer"]
        self.assertEqual("agents/herdr_reviewer.toml", reviewer["config_file"])
        self.assertIn("description", reviewer)
        agent_path = PROJECT_ROOT / ".codex" / "agents" / "herdr_reviewer.toml"
        self.assertTrue(agent_path.is_file())


def _registered_hook_commands() -> list[tuple[str, str, str]]:
    """三份工具配置登记的全部 hook 命令：(工具, 事件, 原样 command)。"""
    try:
        import tomllib
    except ModuleNotFoundError:  # Python 3.10 回退已装的 tomli
        import tomli as tomllib
    claude = json.loads((PROJECT_ROOT / ".claude" / "settings.json").read_text(encoding="utf-8"))
    zcode = json.loads((PROJECT_ROOT / ".zcode" / "config.json").read_text(encoding="utf-8"))
    codex = tomllib.loads((PROJECT_ROOT / ".codex" / "config.toml").read_text(encoding="utf-8"))
    commands: list[tuple[str, str, str]] = []
    for tool, events in (
        ("claude", claude["hooks"]),
        ("zcode", zcode["hooks"]["events"]),
        ("codex", codex["hooks"]),
    ):
        for event, blocks in events.items():
            for block in blocks:
                for hook in block["hooks"]:
                    commands.append((tool, event, hook["command"]))
    return commands


class RegisteredEntryProbeTests(unittest.TestCase):
    """按三份配置里的原样 command 经 `bash -c` 验证独立仓入口与安全决策。"""

    def test_registered_probe_uses_path_resolved_bash(self) -> None:
        from unittest.mock import patch

        with patch.object(subprocess, "run", wraps=subprocess.run) as run:
            result = self._run_registered("printf resolved-bash", {})
        self.assertEqual(shutil.which("bash"), run.call_args.args[0][0])
        self.assertEqual(0, result.returncode, result.stderr.decode("utf-8", "replace"))
        self.assertEqual(b"resolved-bash", result.stdout)

    def _run_registered(self, command: str, payload: dict) -> subprocess.CompletedProcess[bytes]:
        bash = shutil.which("bash")
        self.assertIsNotNone(bash, "bash is required to exercise the registered hooks")
        return subprocess.run(
            [bash, "-c", command],
            input=json.dumps(payload).encode("utf-8"),
            capture_output=True,
            check=False,
            cwd=PROJECT_ROOT,
        )

    def test_pre_tool_use_entries_gate_as_configured(self) -> None:
        # 危险字面量拆分构造：宿主会话可能挂着同一安全门，命令文本不得整串出现。
        probes = [
            ("Bash", {"command": "gh pr mer" + "ge 12"}, "deny"),
            ("Edit", {"file_path": str(PROJECT_ROOT / "CHANGELOG.md")}, "deny"),
            ("Edit", {"file_path": str(PROJECT_ROOT / "src" / "app" / "state.rs")}, "allow"),
        ]
        entries = [(tool, command) for tool, event, command in _registered_hook_commands() if event == "PreToolUse"]
        self.assertEqual({"claude", "zcode", "codex"}, {tool for tool, _ in entries})
        for tool, command in entries:
            for tool_name, tool_input, expected in probes:
                with self.subTest(tool=tool, probe=tool_name, expected=expected):
                    result = self._run_registered(command, {"tool_name": tool_name, "tool_input": tool_input})
                    self.assertEqual(0, result.returncode, result.stderr.decode("utf-8", "replace"))
                    self.assertEqual(expected, _decision(result)[0])

    def test_registered_scripts_resolve_inside_standalone_checkout(self) -> None:
        # 只把解释器换成 test -f：路径与 $(git rev-parse ...) 仍按原样经 shell 求值，
        # 但不真正执行会写本机状态的 PostToolUse/Stop 信号钩子。
        for tool, event, command in _registered_hook_commands():
            with self.subTest(tool=tool, event=event):
                probe, replaced = re.subn(r"^(?:bash|python3)\s+", "test -f ", command)
                self.assertEqual(1, replaced, f"无法识别的解释器：{command}")
                result = self._run_registered(probe, {})
                self.assertEqual(0, result.returncode, f"{tool} {event} 登记的脚本不存在：{command}")


class SignalHookTests(unittest.TestCase):
    """信号钩子按自身文件位置推导组件根：拷进临时「单仓/herdr」布局再跑，不写真实工作区。"""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.monorepo = Path(self._tmp.name)
        self.root = self.monorepo / "herdr"
        subprocess.run(["git", "init", "-q", str(self.monorepo)], check=True, capture_output=True)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def _install(self, root: Path, script: Path) -> Path:
        hooks = root / ".claude" / "hooks"
        hooks.mkdir(parents=True, exist_ok=True)
        target = hooks / script.name
        shutil.copy2(script, target)
        return target

    def test_record_edit_appends_log(self) -> None:
        hook = self._install(self.root, RECORD_EDIT)
        stdin = json.dumps({"tool_name": "Write", "tool_input": {"file_path": "src/app/state.rs"}})
        result = subprocess.run(
            [sys.executable, str(hook)],
            input=stdin.encode("utf-8"),
            capture_output=True,
            check=False,
            cwd=self.monorepo,
        )
        self.assertEqual(0, result.returncode)
        log = (self.root / ".claude" / "live-edits.log").read_text(encoding="utf-8")
        self.assertIn("src/app/state.rs", log)
        self.assertIn("Write", log)
        # cwd 的 git 顶层是单仓根：日志必须落在组件内而不是单仓根。
        self.assertFalse((self.monorepo / ".claude").exists())

    def test_notify_review_writes_ready_json(self) -> None:
        hook = self._install(self.root, NOTIFY_REVIEW)
        (self.root / "src").mkdir()
        (self.root / "src" / "main.rs").write_text("fn main() {}\n", encoding="utf-8")
        (self.monorepo / "other").mkdir()
        (self.monorepo / "other" / "notes.md").write_text("x\n", encoding="utf-8")
        subprocess.run(
            ["git", "-C", str(self.monorepo), "add", "herdr/src/main.rs", "other/notes.md"],
            check=True,
            capture_output=True,
        )
        result = subprocess.run(
            [sys.executable, str(hook)],
            input=b"{}",
            capture_output=True,
            check=False,
            cwd=self.monorepo,
        )
        self.assertEqual(0, result.returncode)
        payload = json.loads((self.root / ".claude" / "review" / "READY.json").read_text(encoding="utf-8"))
        self.assertTrue(payload["ready"])
        # 清单只含本组件改动且为组件相对路径，单仓其他目录的改动不混入。
        self.assertIn("src/main.rs", payload["changed"])
        self.assertNotIn("other/notes.md", payload["changed"])
        self.assertFalse((self.monorepo / ".claude").exists())

    def test_notify_review_outside_repo_is_silent(self) -> None:
        with tempfile.TemporaryDirectory() as outside:
            hook = self._install(Path(outside), NOTIFY_REVIEW)
            result = subprocess.run(
                [sys.executable, str(hook)],
                input=b"{}",
                capture_output=True,
                check=False,
                cwd=outside,
            )
            self.assertEqual(0, result.returncode)
            self.assertEqual(b"", result.stdout)
            self.assertFalse((Path(outside) / ".claude" / "review").exists())


if __name__ == "__main__":
    unittest.main()
