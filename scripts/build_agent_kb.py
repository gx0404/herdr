#!/usr/bin/env python3
"""构建 herdr agent 知识库（docs/kb/chunks.json）。

语料三层（确定性、无时间戳、无绝对路径）：
1. 文档：AGENTS.md、根 README*、CONTRIBUTING.md、docs/*.md、docs/AGENT_RULES/*.md、
   docs/next/website 英文 mdx、根 CHANGELOG.md 近端；
2. 配置契约：src/detect/manifests/*.toml 摘要、config-reference.json 与
   API schema 的顶层结构摘要；
3. 代码结构：src/**/*.rs（排除生成物 bindings.rs）的模块文档与公开项签名。

默认 check 模式：内存重建并与现有产物 diff，不一致退出 1；--confirm 才写入。
语料闭集 = Git 可见文件；新增语料文件需先 git add（或调整本脚本清单）。
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
OUTPUT_PATH = REPO_ROOT / "docs" / "kb" / "chunks.json"
SCHEMA_VERSION = 1
MAX_CHUNK_CHARS = 1500
MAX_CODE_CHUNK_CHARS = 4000
CHANGELOG_HEAD_CHARS = 6000
EXCLUDED_CODE = {"src/ghostty/bindings.rs"}

DOC_FILES = [
    "AGENTS.md",
    "CLAUDE.md",
    "CONTRIBUTING.md",
    "README.md",
    "README.zh-CN.md",
    "CHANGELOG.md",
]
DOC_GLOBS = [
    "docs/*.md",
    "docs/AGENT_RULES/*.md",
    "docs/next/website/src/content/docs/*.mdx",
]
CONFIG_CONTRACTS = [
    "docs/next/website/src/data/config-reference.json",
    "docs/next/api/herdr-api.schema.json",
]


def _git_files() -> set[str]:
    result = subprocess.run(
        ["git", "-C", str(REPO_ROOT), "ls-files", "--cached", "--others", "--exclude-standard"],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise SystemError("git ls-files 失败")
    return {line.strip() for line in result.stdout.splitlines() if line.strip()}


def _sha256_text(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _glob_relative(pattern: str, tracked: set[str]) -> list[str]:
    base = REPO_ROOT / Path(pattern).parent
    suffix = Path(pattern).name
    if suffix == "*.md":
        names = sorted(p.name for p in base.glob("*.md"))
    elif suffix == "*.mdx":
        names = sorted(p.name for p in base.glob("*.mdx"))
    elif suffix == "*.toml":
        names = sorted(p.name for p in base.glob("*.toml"))
    else:
        names = []
    return [f"{Path(pattern).parent.as_posix()}/{name}" for name in names if f"{Path(pattern).parent.as_posix()}/{name}" in tracked]


def _split_heading_chunks(text: str) -> list[tuple[str, str]]:
    """按 markdown 标题切分；返回 (标题路径, 正文) 列表。"""
    lines = text.splitlines()
    sections: list[tuple[str, str]] = []
    current_title: list[str] = []
    buffer: list[str] = []
    heading_stack: list[str] = []

    def flush() -> None:
        body = "\n".join(buffer).strip("\n")
        if body.strip():
            sections.append((" > ".join(current_title), body))

    for line in lines:
        match = re.match(r"^(#{1,4})\s+(.*)$", line)
        if match:
            flush()
            buffer = []
            level = len(match.group(1))
            title = match.group(2).strip()
            while len(heading_stack) >= level:
                heading_stack.pop()
            heading_stack.append(title)
            current_title = list(heading_stack)
            buffer.append(line)
        else:
            buffer.append(line)
    flush()
    if not sections:
        sections = [("", text.strip("\n"))]
    return sections


def _emit_chunks(chunks: list[dict], doc: str, anchor: str, text: str, max_chars: int) -> None:
    """把一节正文按 max_chars 分片成带续编 id 的 chunk。"""
    clean = text.strip("\n")
    if not clean.strip():
        return
    parts: list[str] = []
    current: list[str] = []
    size = 0

    def flush() -> None:
        # 切片点落在空行上时，新片可能只剩空白行（例如空行后紧跟一行超过
        # max_chars 的文字，空行独占一片后立刻又被切走）。空白片没有检索价值，
        # 还会让新鲜度锁的非空断言变红，所以直接丢弃，续编号只数有内容的片。
        part = "\n".join(current)
        if part.strip():
            parts.append(part)

    for line in clean.splitlines():
        if size + len(line) + 1 > max_chars and current:
            flush()
            current, size = [], 0
        current.append(line)
        size += len(line) + 1
    if current:
        flush()
    # 保留 CJK：slug 只替换分隔类字符，否则中文标题会全部坍缩成同一 id。
    slug = re.sub(r"[^a-z0-9\u3040-\u30ff\u3400-\u4dbf\u4e00-\u9fff\uf900-\ufaff]+", "-", f"{doc}#{anchor}".lower()).strip("-")
    base_id = slug[:160].strip("-")
    for index, part in enumerate(parts, start=1):
        chunk_id = base_id if len(parts) == 1 else f"{base_id}-part{index}"
        chunks.append(
            {
                "id": chunk_id,
                "doc": doc,
                "anchor": anchor,
                "text": part,
                "source_sha256": _sha256_text(clean),
            }
        )


def _doc_chunks(path: str, chunks: list[dict]) -> None:
    text = (REPO_ROOT / path).read_text(encoding="utf-8")
    if path == "CHANGELOG.md":
        _emit_chunks(chunks, path, "recent", text[:CHANGELOG_HEAD_CHARS], MAX_CHUNK_CHARS)
        return
    for title, body in _split_heading_chunks(text):
        _emit_chunks(chunks, path, title or "_top", body, MAX_CHUNK_CHARS)


def _json_summary_chunks(path: str, chunks: list[dict]) -> None:
    payload = json.loads((REPO_ROOT / path).read_text(encoding="utf-8"))
    if isinstance(payload, dict):
        keys = sorted(str(key) for key in payload)
        summary = ["顶层键：" + ", ".join(keys[:80])]
        for key in ("title", "version", "$id", "type"):
            if key in payload:
                summary.append(f"{key}: {payload[key]}")
        defs = payload.get("definitions") or payload.get("$defs") or {}
        if isinstance(defs, dict):
            names = sorted(str(name) for name in defs)[:200]
            summary.append("定义（截断）：" + ", ".join(names))
        if isinstance(payload.get("sections"), dict):
            summary.append("sections：" + ", ".join(sorted(payload["sections"])[:80]))
        _emit_chunks(chunks, path, "summary", "\n".join(summary), MAX_CHUNK_CHARS)
    else:
        _emit_chunks(chunks, path, "summary", json.dumps(payload, ensure_ascii=False)[:MAX_CHUNK_CHARS], MAX_CHUNK_CHARS)


def _manifest_chunks(path: str, chunks: list[dict]) -> None:
    text = (REPO_ROOT / path).read_text(encoding="utf-8")
    lines: list[str] = [f"agent 检测 manifest：{path}"]
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith(("id =", "version =", "min_engine_version =", "[[rules]]", "state =", "priority =")):
            lines.append(stripped)
    _emit_chunks(chunks, path, "summary", "\n".join(lines), MAX_CHUNK_CHARS)


_PUB_ITEM = re.compile(r"^\s*(pub(?:\([^)]*\))?\s+(?:async\s+)?(?:unsafe\s+)?(?:const|static|fn|struct|enum|trait|type|mod)\b.*)$")
_IMPL_ITEM = re.compile(r"^\s*(impl\b.*\{?)$")
_MODULE_DOC = re.compile(r"^\s*//!\s?(.*)$")


def _code_chunks(path: str, chunks: list[dict]) -> None:
    lines = (REPO_ROOT / path).read_text(encoding="utf-8").splitlines()
    module_docs: list[str] = []
    for line in lines:
        match = _MODULE_DOC.match(line)
        if match:
            module_docs.append(match.group(1))
        elif module_docs:
            break
    body: list[str] = []
    if module_docs:
        body.append("模块文档：" + " ".join(module_docs))
    body.append("公开项：")
    pending_doc: list[str] = []
    for line in lines:
        stripped = line.strip()
        if stripped.startswith("///"):
            pending_doc.append(stripped.lstrip("/ ").strip())
            continue
        if _PUB_ITEM.match(line) or _IMPL_ITEM.match(line):
            entry = line.strip()
            if pending_doc:
                entry += " — " + " ".join(pending_doc[:3])
            body.append(entry)
        pending_doc = []
    _emit_chunks(chunks, path, "structure", "\n".join(body), MAX_CODE_CHUNK_CHARS)


def build_payload() -> dict:
    tracked = _git_files()
    chunks: list[dict] = []
    for path in DOC_FILES:
        if path in tracked:
            _doc_chunks(path, chunks)
    for pattern in DOC_GLOBS:
        for path in _glob_relative(pattern, tracked):
            _doc_chunks(path, chunks)
    for path in CONFIG_CONTRACTS:
        if path in tracked:
            _json_summary_chunks(path, chunks)
    for path in _glob_relative("src/detect/manifests/*.toml", tracked):
        _manifest_chunks(path, chunks)
    for path in sorted(item for item in tracked if item.startswith("src/") and item.endswith(".rs") and item not in EXCLUDED_CODE):
        _code_chunks(path, chunks)
    chunks.sort(key=lambda chunk: chunk["id"])
    seen: dict[str, int] = {}
    for chunk in chunks:
        base = chunk["id"]
        if base in seen:
            seen[base] += 1
            chunk["id"] = f"{base}-x{seen[base]}"
        else:
            seen[base] = 0
    return {"schema_version": SCHEMA_VERSION, "chunk_count": len(chunks), "chunks": chunks}


def render(payload: dict) -> str:
    return json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--confirm", action="store_true", help="写入产物（默认只检查）")
    args = parser.parse_args(argv)
    payload = build_payload()
    content = render(payload)
    if args.confirm:
        OUTPUT_PATH.parent.mkdir(parents=True, exist_ok=True)
        OUTPUT_PATH.write_text(content, encoding="utf-8")
        print(f"written: {OUTPUT_PATH.relative_to(REPO_ROOT)} ({payload['chunk_count']} chunks)")
        return 0
    if not OUTPUT_PATH.is_file():
        print("error: docs/kb/chunks.json 不存在；确认语料有意更新后用 --confirm 生成", file=sys.stderr)
        return 1
    if OUTPUT_PATH.read_text(encoding="utf-8") != content:
        print(
            "error: docs/kb/chunks.json 与当前语料不一致；语料有意变化时运行 "
            "python3 scripts/build_agent_kb.py --confirm 并审 diff",
            file=sys.stderr,
        )
        return 1
    print(f"OK: docs/kb/chunks.json 与语料一致（{payload['chunk_count']} chunks）")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
