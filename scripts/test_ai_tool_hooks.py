#!/usr/bin/env python3
"""AI 工具面安全门探针：无副作用输入断言允许/拒绝，防止门空转或误杀。"""

from __future__ import annotations

import json
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
    ]
    ASK_COMMANDS = [
        "git push origin feature/x --force-with-lease",
        "git add -A",
        "git add --all",
        "pkill -9 herdr",
    ]
    ALLOW_COMMANDS = [
        "cargo nextest run --locked",
        "just check",
        "python3 scripts/resolve_agent_rules.py --check",
        "git push origin feature/gx_herdr",
        "git commit -m 'fix: pane focus'",
        "python3 -m unittest scripts.test_changelog",
        "rg 'fn main' src/",
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
        stdin = json.dumps({"tool_name": "Bash", "tool_input": {"command": "gh pr merge 1"}})
        result = subprocess.run(
            ["bash", str(GATE_SH)], input=stdin.encode("utf-8"), capture_output=True, check=False
        )
        self.assertEqual(0, result.returncode)
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
        self.assertEqual("deny", payload["permissionDecision"])
        self.assertIn("reason", payload)

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
        self.assertEqual("deny", payload["permissionDecision"])

    def test_malformed_input_does_not_crash(self) -> None:
        result = subprocess.run(
            [sys.executable, str(GATE)], input=b"not json", capture_output=True, check=False
        )
        self.assertEqual(0, result.returncode)


class SignalHookTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        subprocess.run(["git", "init", "-q", str(self.root)], check=True, capture_output=True)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def test_record_edit_appends_log(self) -> None:
        stdin = json.dumps({"tool_name": "Write", "tool_input": {"file_path": "src/app/state.rs"}})
        result = subprocess.run(
            [sys.executable, str(RECORD_EDIT)],
            input=stdin.encode("utf-8"),
            capture_output=True,
            check=False,
            cwd=self.root,
        )
        self.assertEqual(0, result.returncode)
        log = (self.root / ".claude" / "live-edits.log").read_text(encoding="utf-8")
        self.assertIn("src/app/state.rs", log)
        self.assertIn("Write", log)

    def test_notify_review_writes_ready_json(self) -> None:
        (self.root / "src").mkdir()
        (self.root / "src" / "main.rs").write_text("fn main() {}\n", encoding="utf-8")
        subprocess.run(
            ["git", "-C", str(self.root), "add", "src/main.rs"], check=True, capture_output=True
        )
        result = subprocess.run(
            [sys.executable, str(NOTIFY_REVIEW)],
            input=b"{}",
            capture_output=True,
            check=False,
            cwd=self.root,
        )
        self.assertEqual(0, result.returncode)
        payload = json.loads((self.root / ".claude" / "review" / "READY.json").read_text(encoding="utf-8"))
        self.assertTrue(payload["ready"])
        self.assertIn("src/main.rs", payload["changed"])

    def test_notify_review_outside_repo_is_silent(self) -> None:
        with tempfile.TemporaryDirectory() as outside:
            result = subprocess.run(
                [sys.executable, str(NOTIFY_REVIEW)],
                input=b"{}",
                capture_output=True,
                check=False,
                cwd=outside,
            )
            self.assertEqual(0, result.returncode)
            self.assertEqual(b"", result.stdout)


if __name__ == "__main__":
    unittest.main()
