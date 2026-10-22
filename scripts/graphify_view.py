#!/usr/bin/env python3
"""Graphify 0.9.20 全量图的离线视图适配器；check 只读且不修复。"""

from __future__ import annotations

import argparse
import hashlib
import html
import json
import re
import sys
from pathlib import Path
from urllib.parse import quote

if __package__ in (None, ""):
    import graphify_fingerprint as fingerprint
else:
    from . import graphify_fingerprint as fingerprint

REPO_ROOT = Path(__file__).resolve().parents[1]
SCHEMA_VERSION = 1
MANIFEST = "view-manifest.json"
ASSET_ROOT = "scripts/graphify_view_assets"
REQUIRED_ASSETS = {"index.html", "viewer.js", "viewer.css", "vendor/three.bundle.js"}


class ViewError(ValueError):
    """输入、来源或产物不满足视图契约。"""


def _sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _json(value: object) -> bytes:
    return (json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"),
                       allow_nan=False) + "\n").encode("utf-8")


def _relative(name: str) -> list[str]:
    if not isinstance(name, str) or not name or "\\" in name or ":" in name:
        raise ViewError(f"不安全的相对路径：{name!r}")
    parts = name.split("/")
    if any(part in {"", ".", ".."} or "\x00" in part for part in parts):
        raise ViewError(f"不安全的相对路径：{name!r}")
    return parts


def _safe_path(root: Path, name: str) -> Path:
    """逐级拒绝 symlink/junction，包括尚未存在输出的父目录。"""
    path = root
    for part in _relative(name):
        path = path / part
        if path.is_symlink() or getattr(path, "is_junction", lambda: False)():
            raise ViewError(f"拒绝链接路径：{name}")
    if not path.resolve().is_relative_to(root.resolve()):
        raise ViewError(f"路径越界：{name}")
    return path


def _read(root: Path, name: str) -> bytes:
    path = _safe_path(root, name)
    if not path.is_file():
        raise ViewError(f"缺少文件：{name}")
    return path.read_bytes()


def _load_json(data: bytes, name: str) -> dict:
    try:
        value = json.loads(data)
    except (ValueError, UnicodeError) as exc:
        raise ViewError(f"JSON 无效：{name}") from exc
    if not isinstance(value, dict):
        raise ViewError(f"JSON 顶层必须为对象：{name}")
    return value


def _tree(root: Path, directory: str) -> list[str]:
    base = _safe_path(root, directory)
    if not base.exists():
        return []
    if not base.is_dir():
        raise ViewError(f"应为目录：{directory}")
    result = []
    for path in sorted(base.iterdir()):
        name = path.relative_to(root).as_posix()
        _safe_path(root, name)
        if path.is_dir():
            result.extend(_tree(root, name))
        elif path.is_file():
            result.append(name)
    return result


def _source_lines(raw: bytes, path: str) -> list[str]:
    """AST 行号以 LF 为界；源码里的 Unicode 分隔符不是新的源码行。"""
    try:
        source = raw.decode("utf-8")
    except UnicodeError as exc:
        raise ViewError(f"源码不是 UTF-8：{path}") from exc
    if "\x00" in source:
        raise ViewError(f"源码包含二进制 NUL：{path}")
    lines = source.split("\n")
    if len(lines) > 1 and not lines[-1]:
        lines.pop()
    return [line.removesuffix("\r") for line in lines]


def _snapshot(path: str, raw: bytes) -> bytes:
    rows = "\n".join(
        f'<span class="line" id="L{i}"><a href="#L{i}" aria-label="第 {i} 行">{i}</a> '
        f'{html.escape(line)}</span>'
        for i, line in enumerate(_source_lines(raw, path), 1)
    )
    title = html.escape(path)
    return (f'<!doctype html><html lang="zh-CN"><meta charset="utf-8">'
            f'<meta name="viewport" content="width=device-width,initial-scale=1">'
            f'<title>{title} · 来源快照</title><style>'
            'body{background:#10141e;color:#e5e9f2;font:14px/1.6 monospace;padding:24px}'
            'a{color:#89cfff}pre{white-space:pre;overflow:auto}.line{display:block}'
            '.line:target{background:#594b1c}.line>a{display:inline-block;min-width:4em}'
            '</style><nav><a href="../GRAPH_TREE.html">返回图谱</a> · '
            f'<a href="../../{quote(path, safe="/")}">打开原文件</a></nav>'
            f'<h1>{title}</h1><p>SHA-256: {_sha(raw)}</p><pre>{rows}</pre></html>\n'
            ).encode("utf-8")


def _source_fields(item: dict, source_lines: dict[str, int]) -> dict:
    raw = item.get("source_file")
    if raw in (None, ""):
        return {"source_path": None, "source_url": None, "line": None}
    _relative(raw)
    # Graphify 0.9.20 在固定 src 抽取根下导出相对路径，不从 id/label 猜测。
    path = "src/" + raw
    if path not in source_lines:
        raise ViewError(f"来源不在固定 src 索引清单中：{raw}")
    location = item.get("source_location")
    match = re.fullmatch(r"L([1-9][0-9]*)", location) if isinstance(location, str) else None
    line = int(match[1]) if match else None
    if line is not None and line > source_lines[path]:
        raise ViewError(f"来源行号越界：{path}:{location}")
    url = f"view-sources/{_sha(path.encode('utf-8'))}.html"
    return {"source_path": path, "source_url": url + (f"#L{line}" if line else ""), "line": line}


def adapt_graph(graph: dict, sources: dict[str, bytes], graph_sha: str) -> dict:
    """保留全部原字段及边，不按显示预算删节点或推断符号类型。"""
    if not isinstance(graph.get("nodes"), list) or not isinstance(graph.get("links"), list):
        raise ViewError("图谱必须包含 nodes/links 数组")
    if not graph["nodes"]:
        raise ViewError("图谱为空")
    source_lines = {path: len(_source_lines(raw, path)) for path, raw in sources.items()}
    nodes, links, ids, communities = [], [], set(), {}
    for original in graph["nodes"]:
        if not isinstance(original, dict):
            raise ViewError("节点必须为对象")
        node_id = original.get("id")
        if not isinstance(node_id, str) or not node_id or node_id in ids:
            raise ViewError(f"节点 ID 无效或重复：{node_id!r}")
        ids.add(node_id)
        fields = _source_fields(original, source_lines)
        path = fields["source_path"]
        module = path.split("/")[1] if path and path.count("/") > 1 else "src" if path else "未定位引用"
        node = {**original, **fields, "module": module, "kind": "unknown"}
        nodes.append(node)
        community = original.get("community")
        if community is not None and (type(community) not in (int, str)):
            raise ViewError("community 必须为整数、字符串或 null")
        key = json.dumps(community)
        if key not in communities:
            communities[key] = {"id": community, "label": original.get("community_name") or
                                (f"Community {community}" if community is not None else "未分组"),
                                "node_count": 0}
        communities[key]["node_count"] += 1
    for original in graph["links"]:
        if not isinstance(original, dict):
            raise ViewError("边必须为对象")
        if any(not isinstance(original.get(key), str) or original[key] not in ids
               for key in ("source", "target")):
            raise ViewError("边包含悬空端点")
        links.append({**original, **_source_fields(original, source_lines)})
    unlocated = sum(node["source_url"] is None for node in nodes)
    return {"schema_version": SCHEMA_VERSION,
            "meta": {"index_root": "src", "graph_sha256": graph_sha,
                     "node_count": len(nodes), "edge_count": len(links),
                     "source_file_count": len(sources), "community_count": len(communities),
                     "located_node_count": len(nodes) - unlocated, "unlocated_node_count": unlocated},
            "nodes": nodes, "links": links, "communities": list(communities.values()),
            "graph_metadata": {key: value for key, value in graph.items() if key not in ("nodes", "links")}}


def build_view(root: Path, source_paths: list[str] | None = None) -> tuple[dict[str, bytes], dict]:
    """确定性生成内存产物；不创建目录、不写文件。"""
    if source_paths is None:
        if root.resolve() != fingerprint.REPO_ROOT.resolve():
            raise ViewError("非项目根测试必须显式提供来源清单")
        source_paths = fingerprint._graph_sources()
    if not source_paths or len(set(source_paths)) != len(source_paths):
        raise ViewError("源码清单为空或包含重复路径")
    sources = {}
    for name in sorted(source_paths):
        if _relative(name)[0] != "src" or not name.endswith(fingerprint.INDEX_SUFFIXES):
            raise ViewError(f"源码不在固定索引范围：{name}")
        sources[name] = _read(root, name)
    graph_raw = _read(root, "graphify-out/graph.json")
    graph = _load_json(graph_raw, "graph.json")
    assets = {name[len(ASSET_ROOT) + 1:]: _read(root, name) for name in _tree(root, ASSET_ROOT)}
    if missing := REQUIRED_ASSETS - assets.keys():
        raise ViewError(f"缺少视图资源：{', '.join(sorted(missing))}")
    outputs = {"GRAPH_TREE.html": assets["index.html"]}
    outputs.update({"view-assets/" + name: data for name, data in assets.items()})
    for path, raw in sources.items():
        outputs[f"view-sources/{_sha(path.encode('utf-8'))}.html"] = _snapshot(path, raw)
    data = adapt_graph(graph, sources, _sha(graph_raw))
    # 独立脚本支持 file://；转义脚本敏感字符，夹具恶意字符串不会生成 HTML。
    serialized = _json(data).decode("utf-8").replace("<", "\\u003c").replace(
        ">", "\\u003e").replace("&", "\\u0026").replace("\u2028", "\\u2028").replace("\u2029", "\\u2029")
    outputs["view-data.js"] = ("window.GRAPHIFY_VIEW_DATA=" + serialized.rstrip() + ";\n").encode("utf-8")
    adapter_paths = ("scripts/graphify_view.py", "scripts/graphify_fingerprint.py", "scripts/graphify.sh")
    manifest = {"schema_version": SCHEMA_VERSION, "index_root": "src", "graph_sha256": _sha(graph_raw),
                "adapter": {name: _sha(_read(root, name)) for name in adapter_paths},
                "assets": {name: _sha(raw) for name, raw in assets.items()},
                "sources": {name: _sha(raw) for name, raw in sources.items()},
                "counts": data["meta"], "outputs": {name: _sha(raw) for name, raw in outputs.items()}}
    return outputs, manifest


def _owned(name: str) -> bool:
    _relative(name)
    return name in {"GRAPH_TREE.html", "view-data.js"} or name.startswith(("view-assets/", "view-sources/"))


def _existing(root: Path) -> set[str]:
    names = set()
    for name in ("GRAPH_TREE.html", "view-data.js"):
        path = _safe_path(root, "graphify-out/" + name)
        if path.exists():
            if not path.is_file():
                raise ViewError(f"应为文件：{name}")
            names.add(name)
    for directory in ("view-assets", "view-sources"):
        names.update(name[len("graphify-out/"):] for name in _tree(root, "graphify-out/" + directory))
    return names


def _stored(root: Path) -> dict | None:
    path = _safe_path(root, "graphify-out/" + MANIFEST)
    if not path.exists():
        return None
    stored = _load_json(_read(root, "graphify-out/" + MANIFEST), MANIFEST)
    if stored.get("schema_version") != SCHEMA_VERSION or not isinstance(stored.get("outputs"), dict):
        raise ViewError("视图清单 schema 无效")
    if not stored["outputs"] or any(not _owned(name) or not isinstance(digest, str) or
                                   re.fullmatch(r"[0-9a-f]{64}", digest) is None
                                   for name, digest in stored["outputs"].items()):
        raise ViewError("视图清单包含非法产物")
    return stored


def export_view(root: Path, source_paths: list[str] | None = None) -> dict:
    outputs, manifest = build_view(root, source_paths)
    stored = _stored(root)
    existing = _existing(root)
    old_outputs = stored["outputs"] if stored else {}
    if unknown := existing - old_outputs.keys():
        raise ViewError(f"拒绝覆盖未登记视图文件：{sorted(unknown)[0]}")
    for name in existing:
        if _sha(_read(root, "graphify-out/" + name)) != old_outputs[name]:
            raise ViewError(f"拒绝覆盖已修改视图文件：{name}")
    for name, raw in outputs.items():
        path = _safe_path(root, "graphify-out/" + name)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(raw)
    for name in existing - outputs.keys():
        _safe_path(root, "graphify-out/" + name).unlink()
    # 清单最后写入；中途失败由 check 检出，不声称多文件原子事务。
    _safe_path(root, "graphify-out/" + MANIFEST).write_bytes(_json(manifest))
    return manifest


def check_view(root: Path, source_paths: list[str] | None = None) -> dict:
    stored = _stored(root)
    if stored is None:
        raise ViewError("缺少 view-manifest.json；请运行 scripts/graphify.sh export html")
    outputs, expected = build_view(root, source_paths)
    if stored != expected:
        raise ViewError("视图输入或清单已过期；请运行 scripts/graphify.sh export html")
    if _existing(root) != outputs.keys():
        raise ViewError("视图产物缺失或存在未登记文件")
    for name, digest in expected["outputs"].items():
        if _sha(_read(root, "graphify-out/" + name)) != digest:
            raise ViewError(f"视图产物摘要不一致：{name}")
    return expected


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("export", "check"))
    args = parser.parse_args(argv)
    try:
        # 视图不能给过期 AST 配上新源码；同一源指纹门用于导出和只读检查。
        fingerprint.check_fingerprint()
        manifest = (export_view if args.mode == "export" else check_view)(REPO_ROOT)
    except (ViewError, fingerprint.FingerprintError, OSError, UnicodeError, ValueError) as exc:
        print(f"[graphify-view] ERROR: {exc}", file=sys.stderr)
        return 2
    counts = manifest["counts"]
    print(f"[graphify-view] {args.mode}: {counts['node_count']} nodes, {counts['edge_count']} links, "
          f"{counts['source_file_count']} sources, {counts['unlocated_node_count']} unlocated")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
