#!/usr/bin/env python3
"""知识库行为测试：golden 检索回归、确定性、闭集与新鲜度锁。"""

from __future__ import annotations

import json
import unittest
from pathlib import Path

from scripts import agent_kb
from scripts import build_agent_kb

PROJECT_ROOT = Path(__file__).resolve().parents[1]
KB_PATH = PROJECT_ROOT / "docs" / "kb" / "chunks.json"

# (查询, 期望命中：任一可接受文档出现在 top-8)
GOLDEN_QUERIES = [
    ("稳定端点 codec 冻结 fixture", ["docs/AGENT_RULES/protocol-api.md"]),
    ("渲染热路径 性能 pane 循环", ["docs/AGENT_RULES/app-render.md"]),
    ("Windows 交叉编译 SDK xwin", ["docs/AGENT_RULES/platform.md"]),
    ("libghostty-vt 补丁 vendor", ["docs/AGENT_RULES/vendored-libghostty-vt.md"]),
    ("CHANGELOG 什么时候编辑 发布", ["docs/AGENT_RULES/docs-pipeline.md", "docs/AGENT_RULES/release-channels.md"]),
    ("preview 渠道 发布资产", ["docs/AGENT_RULES/release-channels.md"]),
    ("hook 危险模式 拒绝", ["docs/AGENT_RULES/ai-tooling.md", "docs/AI_TOOLS.md"]),
    ("AppState test_new 测试", ["docs/AGENT_RULES/testing.md", "docs/AGENT_RULES/persistence-session.md"]),
    ("检测 manifest 热重载", ["docs/AGENT_RULES/detection.md"]),
    ("恢复 身份 快照 adversarial", ["docs/AGENT_RULES/persistence-session.md"]),
    ("领域规则路由 resolver", ["docs/AGENT_RULES/README.md", "docs/AGENT_RULES/ai-tooling.md", "AGENTS.md"]),
    ("提交规范 conventional refs", ["AGENTS.md", "docs/AGENT_RULES/governance.md"]),
]


class KbCorpusTests(unittest.TestCase):
    def test_chunks_file_exists_and_fresh(self) -> None:
        """新鲜度锁：产物必须与当前语料一致（等价 build_agent_kb.py 默认 check）。"""
        self.assertTrue(KB_PATH.is_file(), "docs/kb/chunks.json 缺失")
        rendered = build_agent_kb.render(build_agent_kb.build_payload())
        self.assertEqual(KB_PATH.read_text(encoding="utf-8"), rendered)

    def test_build_is_deterministic(self) -> None:
        first = build_agent_kb.render(build_agent_kb.build_payload())
        second = build_agent_kb.render(build_agent_kb.build_payload())
        self.assertEqual(first, second)

    def test_corpus_closure_rules(self) -> None:
        payload = json.loads(KB_PATH.read_text(encoding="utf-8"))
        docs = {chunk["doc"] for chunk in payload["chunks"]}
        ids = [chunk["id"] for chunk in payload["chunks"]]
        self.assertEqual(len(ids), len(set(ids)), "chunk id 必须唯一")
        # 图谱报告是机器生成物，不进检索语料；生成代码不进代码结构层。
        for doc in docs:
            self.assertFalse(doc.startswith("graphify-out"), doc)
            self.assertNotEqual(doc, "src/ghostty/bindings.rs")
        # 核心语料必须在库：领域规则全集 + 框架文档 + manifest 摘要 + 代码结构。
        rules_dir = PROJECT_ROOT / "docs" / "AGENT_RULES"
        for rule_doc in rules_dir.glob("*.md"):
            self.assertIn(f"docs/AGENT_RULES/{rule_doc.name}", docs, rule_doc.name)
        self.assertTrue(any(doc.startswith("src/detect/manifests/") for doc in docs))
        self.assertTrue(any(doc.endswith(".rs") for doc in docs))
        self.assertGreater(payload["chunk_count"], 700)

    def test_every_chunk_has_source_hash_and_anchor(self) -> None:
        payload = json.loads(KB_PATH.read_text(encoding="utf-8"))
        for chunk in payload["chunks"]:
            self.assertEqual(len(chunk["source_sha256"]), 64, chunk["id"])
            self.assertIn("anchor", chunk)
            self.assertTrue(chunk["text"].strip(), chunk["id"])


class KbRetrievalTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        payload = json.loads(KB_PATH.read_text(encoding="utf-8"))
        cls.index = agent_kb.KBIndex(payload["chunks"])

    def test_golden_queries_hit_expected_docs(self) -> None:
        for query, expected in GOLDEN_QUERIES:
            with self.subTest(query=query):
                results = self.index.search(query, top=8)
                self.assertTrue(results, f"无命中：{query}")
                docs = [item["doc"] for item in results]
                self.assertTrue(
                    any(candidate in docs for candidate in expected),
                    f"{query} -> top8={docs}，期望之一 {expected}",
                )

    def test_no_result_for_garbage_query(self) -> None:
        self.assertEqual(self.index.search("zzqqxxw qqzzwwk", top=8), [])

    def test_tokenizer_handles_cjk_and_code(self) -> None:
        tokens = agent_kb.tokenize("PROTOCOL_VERSION 与 协议版本")
        self.assertIn("protocol_version", tokens)
        self.assertTrue(any("协议" in token for token in tokens))


if __name__ == "__main__":
    unittest.main()
