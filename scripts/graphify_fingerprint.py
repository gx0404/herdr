#!/usr/bin/env python3
"""图谱产物指纹：登记源码与管线哈希，校验报告/镜像/标签一致性。

write  : 重建后写入 graphify-out/source-fingerprint.json（tracked）。
check  : 只读校验——源码指纹未过期、管线脚本未变、tracked 产物与镜像一致；
         graph.json 为本机产物，存在则校验，缺失时降级为提示（查询不可用）。
"""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
GRAPH_DIR = REPO_ROOT / "graphify-out"
FINGERPRINT_PATH = GRAPH_DIR / "source-fingerprint.json"
MIRROR_REPORT = REPO_ROOT / "docs" / "graphify" / "GRAPH_REPORT.md"
SCHEMA_VERSION = 1
INDEX_ROOT = "src"
# 与图谱输入一致：排除项与 src/.graphifyignore 对应。
EXCLUDED_SOURCES = {"src/ghostty/bindings.rs"}
PIPELINE_INPUTS = ["scripts/graphify.sh", "scripts/graphify_fingerprint.py", "src/.graphifyignore"]
TRACKED_ARTIFACTS = ["GRAPH_REPORT.md", ".graphify_labels.json", ".graphify_labels.json.sig"]
LOCAL_ARTIFACTS = ["graph.json"]


class FingerprintError(ValueError):
    """指纹缺失、过期或不一致。"""


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 16), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _graph_sources() -> list[str]:
    result = subprocess.run(
        ["git", "-C", str(REPO_ROOT), "ls-files", "--cached", "--others", "--exclude-standard", "--", "src"],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise FingerprintError("git ls-files 失败")
    files = [line.strip() for line in result.stdout.splitlines() if line.strip()]
    return sorted(
        path for path in files if path.endswith(".rs") and path not in EXCLUDED_SOURCES
    )


def _source_digest(sources: list[str]) -> str:
    digest = hashlib.sha256()
    for path in sources:
        digest.update(path.encode("utf-8"))
        digest.update(b"\0")
        digest.update(_sha256_file(REPO_ROOT / path).encode("ascii"))
        digest.update(b"\0")
    return digest.hexdigest()


def _build_payload() -> dict:
    sources = _graph_sources()
    payload = {
        "schema_version": SCHEMA_VERSION,
        "index_root": INDEX_ROOT,
        "source": {
            "file_count": len(sources),
            "excluded": sorted(EXCLUDED_SOURCES),
            "sha256": _source_digest(sources),
        },
        "pipeline": {
            name: _sha256_file(REPO_ROOT / name) for name in sorted(PIPELINE_INPUTS)
        },
        "artifacts": {},
    }
    for name in TRACKED_ARTIFACTS + LOCAL_ARTIFACTS:
        artifact = GRAPH_DIR / name
        if artifact.is_file():
            payload["artifacts"][name] = _sha256_file(artifact)
    return payload


def write_fingerprint() -> None:
    GRAPH_DIR.mkdir(parents=True, exist_ok=True)
    payload = _build_payload()
    FINGERPRINT_PATH.write_text(
        json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    print(f"fingerprint written: {FINGERPRINT_PATH.relative_to(REPO_ROOT)}")


def check_fingerprint() -> None:
    if not FINGERPRINT_PATH.is_file():
        raise FingerprintError(
            f"缺少 {FINGERPRINT_PATH.relative_to(REPO_ROOT)}；请运行 just graph 重建"
        )
    try:
        stored = json.loads(FINGERPRINT_PATH.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise FingerprintError(f"指纹文件不可读：{exc}") from exc
    if stored.get("schema_version") != SCHEMA_VERSION:
        raise FingerprintError("指纹 schema 版本不匹配；请运行 just graph 重建")

    current = _build_payload()
    if stored.get("source", {}).get("sha256") != current["source"]["sha256"]:
        raise FingerprintError(
            "src/ 源码指纹已变化：图谱过期，请运行 just graph 重建并提交新报告/指纹"
        )
    if stored.get("source", {}).get("file_count") != current["source"]["file_count"]:
        raise FingerprintError("src/ 源文件数已变化：图谱过期，请运行 just graph 重建")
    for name, digest in stored.get("pipeline", {}).items():
        if current["pipeline"].get(name) != digest:
            raise FingerprintError(f"管线脚本已变化：{name}；请运行 just graph 重建")

    for name in TRACKED_ARTIFACTS:
        if name in stored.get("artifacts", {}):
            artifact = GRAPH_DIR / name
            if not artifact.is_file():
                raise FingerprintError(f"tracked 图谱产物缺失：graphify-out/{name}")
            if _sha256_file(artifact) != stored["artifacts"][name]:
                raise FingerprintError(f"图谱产物与指纹不一致：graphify-out/{name}")
    report = GRAPH_DIR / "GRAPH_REPORT.md"
    if report.is_file() and MIRROR_REPORT.is_file():
        if report.read_bytes() != MIRROR_REPORT.read_bytes():
            raise FingerprintError(
                "docs/graphify/GRAPH_REPORT.md 与 graphify-out/GRAPH_REPORT.md 不一致；重跑 just graph"
            )
    if "graph.json" in stored.get("artifacts", {}):
        graph_json = GRAPH_DIR / "graph.json"
        if graph_json.is_file():
            if _sha256_file(graph_json) != stored["artifacts"]["graph.json"]:
                raise FingerprintError("graph.json 与指纹不一致：请运行 just graph 重建")
            print("graph.json: 本机图谱与指纹一致")
        else:
            print("graph.json: 本机未构建（query/path 不可用）；其余产物已验证")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("write", "check"))
    args = parser.parse_args(argv)
    try:
        if args.mode == "write":
            write_fingerprint()
        else:
            check_fingerprint()
    except FingerprintError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
