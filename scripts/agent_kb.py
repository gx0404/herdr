#!/usr/bin/env python3
"""BM25-lite 检索 herdr agent 知识库（docs/kb/chunks.json）。

用法：python3 scripts/agent_kb.py "<查询>" [--top 8] [--json]
分词：拉丁词 + CJK 二元组；命中带 doc/anchor，便于回源阅读。
"""

from __future__ import annotations

import argparse
import json
import math
import re
import sys
from collections import Counter
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
KB_PATH = REPO_ROOT / "docs" / "kb" / "chunks.json"

_CJK_RANGE = (
    "\u3000-\u303f"  # CJK 标点
    "\u3040-\u30ff"  # 假名
    "\u3400-\u4dbf"
    "\u4e00-\u9fff"
    "\uf900-\ufaff"
)
_LATIN_TOKEN = re.compile(r"[a-z0-9_./:-]{2,}")
_CJK_TOKEN = re.compile(f"[{_CJK_RANGE}]")


def tokenize(text: str) -> list[str]:
    lowered = text.lower()
    tokens = _LATIN_TOKEN.findall(lowered)
    cjk_chars = "".join(_CJK_TOKEN.findall(lowered))
    tokens.extend(f"{cjk_chars[i]}{cjk_chars[i + 1]}" for i in range(len(cjk_chars) - 1))
    return tokens


class KBIndex:
    def __init__(self, chunks: list[dict]) -> None:
        self.chunks = chunks
        self.doc_ids = [f"{chunk['doc']}#{chunk['id']}" for chunk in chunks]
        tokenized = [tokenize(chunk.get("text", "")) for chunk in chunks]
        self.term_freqs = [Counter(tokens) for tokens in tokenized]
        doc_freq: Counter = Counter()
        for tokens in tokenized:
            doc_freq.update(set(tokens))
        total = max(len(chunks), 1)
        self.idf = {term: math.log(1 + (total - df + 0.5) / (df + 0.5)) for term, df in doc_freq.items()}
        self.lengths = [len(tokens) for tokens in tokenized]
        self.avg_length = (sum(self.lengths) / total) if total else 1.0

    def search(self, query: str, top: int = 8) -> list[dict]:
        query_tokens = tokenize(query)
        scores: list[float] = []
        for index in range(len(self.chunks)):
            freq = self.term_freqs[index]
            length = max(self.lengths[index], 1)
            score = 0.0
            for term in query_tokens:
                tf = freq.get(term, 0)
                if not tf:
                    continue
                idf = self.idf.get(term, 0.0)
                score += idf * (tf * 2.2) / (tf + 1.2 * (0.25 + 0.75 * length / self.avg_length))
            scores.append(score)
        ranked = sorted(range(len(scores)), key=lambda i: (-scores[i], self.doc_ids[i]))
        results = []
        for index in ranked[:top]:
            if scores[index] <= 0:
                break
            chunk = self.chunks[index]
            results.append(
                {
                    "doc": chunk["doc"],
                    "anchor": chunk.get("anchor", ""),
                    "score": round(scores[index], 4),
                    "excerpt": chunk.get("text", "")[:160].replace("\n", " "),
                }
            )
        return results


def load_index() -> KBIndex:
    payload = json.loads(KB_PATH.read_text(encoding="utf-8"))
    return KBIndex(payload["chunks"])


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("query")
    parser.add_argument("--top", type=int, default=8)
    parser.add_argument("--json", action="store_true", dest="as_json")
    args = parser.parse_args(argv)
    if not KB_PATH.is_file():
        print("error: 缺少 docs/kb/chunks.json；先运行 python3 scripts/build_agent_kb.py --confirm", file=sys.stderr)
        return 2
    results = load_index().search(args.query, args.top)
    if args.as_json:
        print(json.dumps(results, ensure_ascii=False, indent=2))
        return 0
    if not results:
        print("(无命中)")
        return 0
    for item in results:
        print(f"{item['score']:>8.4f}  {item['doc']}  [{item['anchor']}]")
        print(f"          {item['excerpt']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
