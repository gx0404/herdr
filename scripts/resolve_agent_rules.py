#!/usr/bin/env python3
"""按文件 scope 和任务类型解析本轮必读的 AI 领域规则。

契约与 xyz 框架一致：docs/AGENT_RULES/routes.toml 是唯一机器真源；
本脚本无状态，每次传入完整 scope 重新求并集；未知路径/任务 fail-fast；
--check 校验仓库闭集（每个 Git 可见文件恰属一个领域或 root_only）。
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from functools import lru_cache
from pathlib import Path, PurePosixPath

try:
    import tomllib
except ModuleNotFoundError:  # Python 3.10 开发机兼容；CI 运行时为 3.11+
    import tomli as tomllib  # type: ignore[no-redef]

REPO_ROOT = Path(__file__).resolve().parents[1]
ROUTES_PATH = PurePosixPath("docs/AGENT_RULES/routes.toml")
ROOT_AGENTS_MAX_BYTES = 16 * 1024
DOMAIN_RULE_MAX_BYTES = 16 * 1024
ROUTES_VERSION = 2
# vendored 上游源码树自带其权威 AGENTS.md/AI_POLICY.md，属上游资产，不参与嵌套权威检查。
NESTED_AGENTS_ALLOWED_PREFIXES = ("vendor/",)


class RuleManifestError(ValueError):
    """规则路由缺失、含糊或不满足闭集约束。"""


@dataclass(frozen=True)
class RuleRoute:
    id: str
    doc: str
    paths: tuple[str, ...]
    tasks: tuple[str, ...]


@dataclass(frozen=True)
class RuleRoutes:
    version: int
    root_max_bytes: int
    root_only: tuple[str, ...]
    rules: tuple[RuleRoute, ...]


def _string_tuple(
    value: object,
    *,
    label: str,
    normalizer: Callable[[str], str],
    case_sensitive: bool = False,
) -> tuple[str, ...]:
    if not isinstance(value, list) or any(not isinstance(item, str) for item in value):
        raise RuleManifestError(f"{label} 必须是字符串数组")
    normalized = tuple(normalizer(item) for item in value)
    folded = normalized if case_sensitive else tuple(item.casefold() for item in normalized)
    if len(set(folded)) != len(folded):
        raise RuleManifestError(f"{label} 包含重复项")
    return normalized


def _normalize_route_pattern(value: str) -> str:
    if not value or "\\" in value or value.startswith("/"):
        raise RuleManifestError(f"路由 path 无效：{value!r}")
    path = PurePosixPath(value)
    if any(part in {"", ".", ".."} for part in path.parts):
        raise RuleManifestError(f"路由 path 必须位于仓库内：{value!r}")
    return path.as_posix()


def _normalize_doc(value: str) -> str:
    doc = _normalize_route_pattern(value)
    path = PurePosixPath(doc)
    if path.parent != ROUTES_PATH.parent or path.suffix.casefold() != ".md":
        raise RuleManifestError("领域规则 doc 必须指向 docs/AGENT_RULES/*.md")
    return doc


def _normalize_task(value: str) -> str:
    if re.fullmatch(r"[a-z][a-z0-9-]*", value) is None:
        raise RuleManifestError(f"任务名必须是小写 kebab-case：{value!r}")
    return value


def load_rule_routes(root: Path) -> RuleRoutes:
    path = root.joinpath(*ROUTES_PATH.parts)
    if path.is_symlink() or not path.is_file():
        raise RuleManifestError(f"缺少普通文件：{ROUTES_PATH.as_posix()}")
    try:
        payload = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError) as exc:
        raise RuleManifestError("领域规则路由无法按 UTF-8 TOML 读取") from exc
    expected = {"version", "root_max_bytes", "root_only", "rules"}
    if set(payload) != expected:
        raise RuleManifestError("routes.toml 顶层必须恰好包含 version/root_max_bytes/root_only/rules")
    if type(payload["version"]) is not int or payload["version"] != ROUTES_VERSION:
        raise RuleManifestError(f"不支持的路由版本：{payload['version']!r}")
    if type(payload["root_max_bytes"]) is not int or payload["root_max_bytes"] != ROOT_AGENTS_MAX_BYTES:
        raise RuleManifestError(f"root_max_bytes 必须固定为 {ROOT_AGENTS_MAX_BYTES}")
    root_only = _string_tuple(
        payload["root_only"], label="root_only", normalizer=_normalize_route_pattern, case_sensitive=True
    )
    raw_rules = payload["rules"]
    if not isinstance(raw_rules, list) or not raw_rules:
        raise RuleManifestError("routes.toml 的 rules 必须是非空数组")

    rules: list[RuleRoute] = []
    seen_ids: set[str] = set()
    seen_docs: set[str] = set()
    for index, item in enumerate(raw_rules):
        if not isinstance(item, dict) or set(item) != {"id", "doc", "paths", "tasks"}:
            raise RuleManifestError(f"rules[{index}] 必须恰好包含 id/doc/paths/tasks")
        route_id = item["id"]
        if not isinstance(route_id, str) or re.fullmatch(r"[a-z][a-z0-9-]*", route_id) is None:
            raise RuleManifestError(f"rules[{index}].id 必须是小写 kebab-case")
        if route_id.casefold() in seen_ids:
            raise RuleManifestError(f"领域规则 id 重复：{route_id}")
        seen_ids.add(route_id.casefold())
        if not isinstance(item["doc"], str):
            raise RuleManifestError(f"rules[{index}].doc 必须是字符串")
        doc = _normalize_doc(item["doc"])
        if doc.casefold() in seen_docs:
            raise RuleManifestError(f"领域规则文档重复：{doc}")
        seen_docs.add(doc.casefold())
        doc_path = root.joinpath(*PurePosixPath(doc).parts)
        if doc_path.is_symlink() or not doc_path.is_file():
            raise RuleManifestError(f"领域规则文档必须是普通文件：{doc}")
        size = doc_path.stat().st_size
        if size == 0 or size > DOMAIN_RULE_MAX_BYTES:
            raise RuleManifestError(f"领域规则文档大小无效：{doc} ({size} bytes)")
        paths = _string_tuple(
            item["paths"], label=f"rules[{index}].paths", normalizer=_normalize_route_pattern, case_sensitive=True
        )
        tasks = _string_tuple(item["tasks"], label=f"rules[{index}].tasks", normalizer=_normalize_task)
        if not paths and not tasks:
            raise RuleManifestError(f"规则 {route_id} 必须声明 paths 或 tasks")
        rules.append(RuleRoute(route_id, doc, paths, tasks))
    return RuleRoutes(ROUTES_VERSION, ROOT_AGENTS_MAX_BYTES, root_only, tuple(rules))


def resolve_rules(root: Path, paths: Sequence[str], tasks: Sequence[str] = ()) -> tuple[RuleRoute, ...]:
    if not paths:
        raise RuleManifestError("至少提供一个 scope path")
    resolved_root = root.resolve()
    routes = load_rule_routes(resolved_root)
    normalized_paths = tuple(_normalize_scope_path(path, resolved_root) for path in paths)
    expanded_paths = _expand_scope_paths(resolved_root, normalized_paths)
    normalized_tasks = tuple(_normalize_task(task) for task in tasks)
    allowed_tasks = {task for route in routes.rules for task in route.tasks}
    unknown_tasks = sorted(set(normalized_tasks) - allowed_tasks)
    if unknown_tasks:
        raise RuleManifestError(f"未声明的任务类型：{unknown_tasks}; allowed={sorted(allowed_tasks)}")
    matched_ids: set[str] = set()
    for path in expanded_paths:
        path_routes = tuple(
            route for route in routes.rules if any(_route_matches(pattern, path) for pattern in route.paths)
        )
        root_only = tuple(pattern for pattern in routes.root_only if _route_matches(pattern, path))
        if path_routes and root_only:
            raise RuleManifestError(
                f"scope 同时命中领域路由与 root_only：{path}; rules={[route.id for route in path_routes]}"
            )
        if not path_routes and not root_only:
            raise RuleManifestError(f"scope 路径未声明领域路由或 root_only：{path}")
        matched_ids.update(route.id for route in path_routes)
    matched_ids.update(route.id for route in routes.rules if any(task in route.tasks for task in normalized_tasks))
    return tuple(sorted((route for route in routes.rules if route.id in matched_ids), key=lambda route: route.id))


def validate_repository(root: Path) -> RuleRoutes:
    resolved_root = root.resolve()
    authority = resolved_root / "AGENTS.md"
    if authority.is_symlink() or not authority.is_file():
        raise RuleManifestError("仓库根 AGENTS.md 必须是普通文件")
    size = authority.stat().st_size
    if size > ROOT_AGENTS_MAX_BYTES:
        raise RuleManifestError(f"AGENTS.md 为 {size} bytes，超过 {ROOT_AGENTS_MAX_BYTES} bytes")
    try:
        if not authority.read_text(encoding="utf-8").strip():
            raise RuleManifestError("AGENTS.md 不得为空")
    except UnicodeDecodeError as exc:
        raise RuleManifestError("AGENTS.md 必须是 UTF-8") from exc

    routes = load_rule_routes(resolved_root)
    if [route.id for route in routes.rules] != sorted(route.id for route in routes.rules):
        raise RuleManifestError("routes.toml 的 rules 必须按 id 稳定排序")
    for route in routes.rules:
        if list(route.paths) != sorted(route.paths):
            raise RuleManifestError(f"路由 {route.id} 的 paths 必须排序")
        if list(route.tasks) != sorted(route.tasks):
            raise RuleManifestError(f"路由 {route.id} 的 tasks 必须排序")
    if list(routes.root_only) != sorted(routes.root_only):
        raise RuleManifestError("root_only 必须排序")

    rules_dir = resolved_root / "docs" / "AGENT_RULES"
    declared_docs = {route.doc for route in routes.rules}
    actual_docs = {
        path.relative_to(resolved_root).as_posix()
        for path in rules_dir.glob("*.md")
        if path.name != "README.md" and path.is_file()
    }
    if actual_docs != declared_docs:
        raise RuleManifestError(
            f"领域文档闭集不一致：missing={sorted(declared_docs - actual_docs)}, "
            f"unregistered={sorted(actual_docs - declared_docs)}"
        )

    repo_files = _repository_files(resolved_root)
    nested = sorted(
        path
        for path in repo_files
        if path != "AGENTS.md"
        and path.endswith("/AGENTS.md")
        and not path.startswith(NESTED_AGENTS_ALLOWED_PREFIXES)
    )
    if nested:
        raise RuleManifestError(f"禁止子目录 AGENTS.md（vendor 除外）：{nested}")
    for route in routes.rules:
        for pattern in route.paths:
            _validate_static_root(resolved_root, pattern)
            if not any(_route_matches(pattern, path) for path in repo_files):
                raise RuleManifestError(f"路由 {route.id} 未命中仓库文件：{pattern}")
    for pattern in routes.root_only:
        _validate_static_root(resolved_root, pattern)
        if not any(_route_matches(pattern, path) for path in repo_files):
            raise RuleManifestError(f"root_only 未命中仓库文件：{pattern}")

    uncovered: list[str] = []
    overlapping: list[str] = []
    for path in repo_files:
        domain = any(_route_matches(pattern, path) for route in routes.rules for pattern in route.paths)
        root_only = any(_route_matches(pattern, path) for pattern in routes.root_only)
        if domain and root_only:
            overlapping.append(path)
        elif not domain and not root_only:
            uncovered.append(path)
    if overlapping:
        raise RuleManifestError(f"领域路由与 root_only 重叠：{_summarize_paths(overlapping)}")
    if uncovered:
        raise RuleManifestError(f"仓库文件未声明路由：{_summarize_paths(uncovered)}")
    _validate_domain_prose(resolved_root, routes)
    return routes


def _validate_domain_prose(root: Path, routes: RuleRoutes) -> None:
    seen: dict[str, str] = {}
    for route in routes.rules:
        text = (root / route.doc).read_text(encoding="utf-8")
        for paragraph in re.split(r"\n\s*\n", text):
            normalized = " ".join(line.strip() for line in paragraph.splitlines()).strip()
            if len(normalized) < 160 or normalized.startswith(("#", "```", "~~~", "|")):
                continue
            previous = seen.get(normalized)
            if previous is not None:
                raise RuleManifestError(f"领域规则存在重复正文：{previous} 与 {route.doc}")
            seen[normalized] = route.doc


def _normalize_scope_path(raw: str, root: Path) -> str:
    if not raw or "\\" in raw:
        raise RuleManifestError(f"scope 路径无效：{raw!r}")
    candidate = Path(raw)
    if candidate.is_absolute():
        try:
            value = candidate.resolve(strict=False).relative_to(root).as_posix()
        except ValueError as exc:
            raise RuleManifestError(f"scope 路径越出仓库：{raw}") from exc
    else:
        value = PurePosixPath(raw).as_posix()
        while value.startswith("./"):
            value = value[2:]
    if value in {"", "."}:
        return "."
    path = PurePosixPath(value)
    if path.is_absolute() or any(part in {"", ".", ".."} for part in path.parts):
        raise RuleManifestError(f"scope 路径必须位于仓库内：{raw}")
    return path.as_posix()


def _expand_scope_paths(root: Path, paths: Sequence[str]) -> tuple[str, ...]:
    repo_files: tuple[str, ...] | None = None
    expanded: set[str] = set()
    for path in paths:
        candidate = root if path == "." else root.joinpath(*PurePosixPath(path).parts)
        if not candidate.is_dir():
            expanded.add(path)
            continue
        if repo_files is None:
            repo_files = _repository_files(root)
        prefix = "" if path == "." else f"{path}/"
        descendants = tuple(item for item in repo_files if not prefix or item.startswith(prefix))
        if not descendants:
            raise RuleManifestError(f"scope 目录不含 Git 可见文件：{path}")
        expanded.update(descendants)
    return tuple(sorted(expanded))


def _summarize_paths(paths: Sequence[str], limit: int = 20) -> str:
    shown = list(paths[:limit])
    if len(paths) > limit:
        shown.append(f"... 共 {len(paths)} 个")
    return repr(shown)


def _route_matches(pattern: str, path: str) -> bool:
    if pattern.endswith("/**") and path == pattern[:-3]:
        return True
    return _compile_route_pattern(pattern).fullmatch(path) is not None


@lru_cache(maxsize=512)
def _compile_route_pattern(pattern: str) -> re.Pattern[str]:
    chunks = ["^"]
    index = 0
    while index < len(pattern):
        if pattern.startswith("**/", index):
            chunks.append("(?:.*/)?")
            index += 3
        elif pattern.startswith("**", index):
            chunks.append(".*")
            index += 2
        elif pattern[index] == "*":
            chunks.append("[^/]*")
            index += 1
        elif pattern[index] == "?":
            chunks.append("[^/]")
            index += 1
        else:
            chunks.append(re.escape(pattern[index]))
            index += 1
    chunks.append("$")
    return re.compile("".join(chunks))


def _validate_static_root(root: Path, pattern: str) -> None:
    wildcard_positions = [pos for token in ("*", "?") if (pos := pattern.find(token)) >= 0]
    if not wildcard_positions:
        if not (root / pattern).exists():
            raise RuleManifestError(f"路由 path 不存在：{pattern}")
        return
    prefix = pattern[: min(wildcard_positions)]
    static_root = PurePosixPath(prefix) if prefix.endswith("/") else PurePosixPath(prefix).parent
    if not root.joinpath(*static_root.parts).exists():
        raise RuleManifestError(f"路由 path 的静态根不存在：{pattern}")


def _repository_files(root: Path) -> tuple[str, ...]:
    result = subprocess.run(
        ["git", "-C", str(root), "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
        check=False,
        capture_output=True,
    )
    if result.returncode == 0:
        return tuple(sorted(item.decode("utf-8") for item in result.stdout.split(b"\0") if item))
    return tuple(
        sorted(
            path.relative_to(root).as_posix()
            for path in root.rglob("*")
            if path.is_file() and ".git" not in path.relative_to(root).parts
        )
    )


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", nargs="*", help="本轮 scope 内的仓库相对路径（可多个）")
    parser.add_argument("--task", action="append", default=[], help="任务类型，如 review（可重复）")
    parser.add_argument("--json", action="store_true", dest="as_json", help="输出机器可读 JSON")
    parser.add_argument("--check", action="store_true", help="校验路由闭集与规则体量")
    parser.add_argument("--root", type=Path, default=REPO_ROOT, help=argparse.SUPPRESS)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    parser = _parser()
    args = parser.parse_args(argv)
    try:
        if args.check:
            if args.paths or args.task or args.as_json:
                parser.error("--check 不与 paths/--task/--json 同用")
            routes = validate_repository(args.root)
            print(f"OK: {len(routes.rules)} 份领域规则，AGENTS.md <= {routes.root_max_bytes} bytes")
            return 0
        if not args.paths:
            parser.error("至少提供一个 path")
        matched = resolve_rules(args.root, args.paths, args.task)
        if args.as_json:
            payload = {
                "version": 1,
                "scope": list(args.paths),
                "tasks": sorted(set(args.task)),
                "rules": [{"id": route.id, "doc": route.doc} for route in matched],
            }
            print(json.dumps(payload, ensure_ascii=False, sort_keys=True, separators=(",", ":")))
        else:
            for route in matched:
                print(route.doc)
        return 0
    except RuleManifestError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
