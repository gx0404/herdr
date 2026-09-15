#!/usr/bin/env python3
"""scripts/resolve_agent_rules.py 的行为测试：并集语义与闭集守门。"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from scripts import resolve_agent_rules as resolver

PROJECT_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = PROJECT_ROOT / "scripts" / "resolve_agent_rules.py"

LONG_PROSE = (
    "这一段是用于重复正文检测的长段落，规范化后的长度必须超过一百六十个字符，"
    "它出现在两份不同的领域规则文档里时必须被闭集校验拒绝，"
    "因为复制粘贴的规则迟早会各自漂移，真源只能有一个，"
    "其余位置必须引用而不是复制正文，重复是维护事故的起点；"
    "闭集校验会把它规范化成单个空格连接的长行并记录首次出现的位置，"
    "第二次出现即判定为重复正文并直接失败，防止两份规则文档各说各话。"
)

BASE_ROUTES = """\
version = 2
root_max_bytes = 16384
root_only = []

[[rules]]
id = "ai-tooling"
doc = "docs/AGENT_RULES/ai-tooling.md"
paths = [
  "AGENTS.md",
  "CLAUDE.md",
  "docs/AGENT_RULES/**",
]
tasks = []

[[rules]]
id = "alpha"
doc = "docs/AGENT_RULES/alpha.md"
paths = [
  "src/alpha.rs",
  "src/alpha/**",
]
tasks = []

[[rules]]
id = "code-review"
doc = "docs/AGENT_RULES/code-review.md"
paths = []
tasks = ["review"]

[[rules]]
id = "omega"
doc = "docs/AGENT_RULES/omega.md"
paths = [
  "docs/omega/**",
  "src/omega/**",
]
tasks = []
"""


def _write(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


class ResolverTestCase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        _write(self.root / "AGENTS.md", "# root\n\n启动协议见 resolver。\n")
        _write(self.root / "CLAUDE.md", "@AGENTS.md\n")
        _write(self.root / "docs/AGENT_RULES/routes.toml", BASE_ROUTES)
        _write(self.root / "docs/AGENT_RULES/ai-tooling.md", "# ai-tooling\n\n工具面规则。\n")
        _write(self.root / "docs/AGENT_RULES/alpha.md", "# alpha\n\nalpha 领域规则。\n")
        _write(self.root / "docs/AGENT_RULES/code-review.md", "# review\n\n审核输出格式。\n")
        _write(self.root / "docs/AGENT_RULES/omega.md", "# omega\n\nomega 领域规则。\n")
        _write(self.root / "src/alpha.rs", "// alpha\n")
        _write(self.root / "src/alpha/mod.rs", "// alpha mod\n")
        _write(self.root / "src/omega/thing.rs", "// omega\n")
        _write(self.root / "docs/omega/info.md", "# omega info\n")

    def tearDown(self) -> None:
        self._tmp.cleanup()

    # ---- 解析语义 ----

    def test_resolve_directory_expands_to_union(self) -> None:
        matched = resolver.resolve_rules(self.root, ["src"])
        self.assertEqual(["alpha", "omega"], [route.id for route in matched])

    def test_resolve_single_file_matches_domain(self) -> None:
        matched = resolver.resolve_rules(self.root, ["src/alpha.rs"])
        self.assertEqual(["alpha"], [route.id for route in matched])

    def test_resolve_accepts_not_yet_created_routed_file(self) -> None:
        matched = resolver.resolve_rules(self.root, ["src/omega/future.rs"])
        self.assertEqual(["omega"], [route.id for route in matched])

    def test_resolve_accepts_absolute_path_inside_root(self) -> None:
        matched = resolver.resolve_rules(self.root, [str(self.root / "src" / "alpha.rs")])
        self.assertEqual(["alpha"], [route.id for route in matched])

    def test_unknown_path_rejected(self) -> None:
        with self.assertRaises(resolver.RuleManifestError):
            resolver.resolve_rules(self.root, ["unknown/file.rs"])

    def test_unknown_task_rejected(self) -> None:
        with self.assertRaises(resolver.RuleManifestError):
            resolver.resolve_rules(self.root, ["src"], tasks=["deploy"])

    def test_review_task_adds_code_review(self) -> None:
        matched = resolver.resolve_rules(self.root, ["src/alpha.rs"], tasks=["review"])
        self.assertEqual(["alpha", "code-review"], [route.id for route in matched])

    def test_scope_escape_rejected(self) -> None:
        with self.assertRaises(resolver.RuleManifestError):
            resolver.resolve_rules(self.root, ["../outside.rs"])

    # ---- CLI ----

    def _run_cli(self, *args: str) -> subprocess.CompletedProcess[bytes]:
        return subprocess.run(
            [sys.executable, str(SCRIPT_PATH), "--root", str(self.root), *args],
            capture_output=True,
            check=False,
        )

    def test_cli_prints_doc_paths(self) -> None:
        result = self._run_cli("src")
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertEqual(
            ["docs/AGENT_RULES/alpha.md", "docs/AGENT_RULES/omega.md"],
            result.stdout.decode("utf-8").splitlines(),
        )

    def test_cli_json_payload(self) -> None:
        result = self._run_cli("--json", "--task", "review", "src/omega/thing.rs")
        self.assertEqual(0, result.returncode, result.stderr)
        payload = json.loads(result.stdout.decode("utf-8"))
        self.assertEqual(["code-review", "omega"], [rule["id"] for rule in payload["rules"]])

    def test_cli_error_exit_code(self) -> None:
        result = self._run_cli("missing.rs")
        self.assertEqual(2, result.returncode)
        self.assertIn("error:", result.stderr.decode("utf-8"))

    def test_cli_check_ok(self) -> None:
        result = self._run_cli("--check")
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("OK: 4 份领域规则", result.stdout.decode("utf-8"))

    # ---- 闭集守门（每个拒绝项独立注入证明）----

    def test_check_rejects_oversized_root(self) -> None:
        _write(self.root / "AGENTS.md", "# root\n" + ("x" * 17000) + "\n")
        with self.assertRaises(resolver.RuleManifestError) as ctx:
            resolver.validate_repository(self.root)
        self.assertIn("超过", str(ctx.exception))

    def test_check_rejects_unregistered_doc(self) -> None:
        _write(self.root / "docs/AGENT_RULES/stray.md", "# stray\n")
        with self.assertRaises(resolver.RuleManifestError) as ctx:
            resolver.validate_repository(self.root)
        self.assertIn("闭集不一致", str(ctx.exception))

    def test_check_rejects_uncovered_file(self) -> None:
        _write(self.root / "stray.txt", "unrouted\n")
        with self.assertRaises(resolver.RuleManifestError) as ctx:
            resolver.validate_repository(self.root)
        self.assertIn("未声明路由", str(ctx.exception))

    def test_check_rejects_zero_hit_glob(self) -> None:
        routes = BASE_ROUTES.replace('  "src/alpha/**",\n', '  "src/alpha/**",\n  "src/alpha/*.zz",\n')
        routes = routes.replace('id = "alpha"', 'id = "beta"').replace('alpha.md', 'beta.md')
        _write(self.root / "docs/AGENT_RULES/routes.toml", routes)
        _write(self.root / "docs/AGENT_RULES/beta.md", "# beta\n\nbeta 领域规则。\n")
        (self.root / "docs/AGENT_RULES/alpha.md").unlink()
        with self.assertRaises(resolver.RuleManifestError) as ctx:
            resolver.validate_repository(self.root)
        self.assertIn("未命中仓库文件", str(ctx.exception))

    def test_check_rejects_duplicate_prose(self) -> None:
        _write(self.root / "docs/AGENT_RULES/alpha.md", f"# alpha\n\n{LONG_PROSE}\n")
        _write(self.root / "docs/AGENT_RULES/omega.md", f"# omega\n\n{LONG_PROSE}\n")
        with self.assertRaises(resolver.RuleManifestError) as ctx:
            resolver.validate_repository(self.root)
        self.assertIn("重复正文", str(ctx.exception))

    def test_check_rejects_nested_agents_outside_vendor(self) -> None:
        _write(self.root / "sub/AGENTS.md", "# nested\n")
        with self.assertRaises(resolver.RuleManifestError) as ctx:
            resolver.validate_repository(self.root)
        self.assertIn("禁止子目录 AGENTS.md", str(ctx.exception))

    def test_check_allows_nested_agents_under_vendor(self) -> None:
        routes = BASE_ROUTES.replace("root_only = []", 'root_only = ["vendor/**"]')
        _write(self.root / "docs/AGENT_RULES/routes.toml", routes)
        _write(self.root / "vendor/libghostty-vt/AGENTS.md", "# vendored upstream\n")
        resolver.validate_repository(self.root)

    def test_check_rejects_root_only_overlap(self) -> None:
        routes = BASE_ROUTES.replace("root_only = []", 'root_only = ["src/alpha.rs"]')
        _write(self.root / "docs/AGENT_RULES/routes.toml", routes)
        with self.assertRaises(resolver.RuleManifestError) as ctx:
            resolver.validate_repository(self.root)
        self.assertIn("重叠", str(ctx.exception))

    def test_check_rejects_unsorted_rules(self) -> None:
        blocks = BASE_ROUTES.split("[[rules]]")
        reordered = blocks[0] + "[[rules]]" + blocks[2] + "[[rules]]" + blocks[1] + "[[rules]]" + blocks[3]
        _write(self.root / "docs/AGENT_RULES/routes.toml", reordered)
        with self.assertRaises(resolver.RuleManifestError) as ctx:
            resolver.validate_repository(self.root)
        self.assertIn("稳定排序", str(ctx.exception))

    def test_toml_backend_available(self) -> None:
        # 无论 tomllib（3.11+）还是 tomli 回退（3.10），模块都必须可用。
        self.assertTrue(hasattr(resolver.tomllib, "loads"))


if __name__ == "__main__":
    unittest.main()
