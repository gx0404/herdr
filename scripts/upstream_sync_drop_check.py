#!/usr/bin/env python3
"""检查 fork 已删除的集成路径没有被上游同步带回。

本 fork 的官方集成只保留六家；同步上游时，六家以外集成商的新增与修改一律不
合入。口径见 docs/AGENT_RULES/README.md「fork 已删除的集成」。

清单 scripts/upstream_sync_drop_paths.txt 每行一个 glob（整行 # 为注释），语义
与 docs/AGENT_RULES/routes.toml 相同：* 不跨目录，** 跨目录，`dir/**` 额外匹配
目录自身。检查对象是 Git 可见文件（索引中的 + 未被忽略的未跟踪文件）：被忽略
的残留（如 __pycache__/）不算「被带回」；根目录不是 Git 工作树顶层时退回文件
系统遍历。

退出码：0 无命中（--list 只列不判，同为 0）；1 仍有命中路径；2 清单本身无效
（--list 下同样报 2）。
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
from collections.abc import Iterable, Sequence
from functools import lru_cache
from pathlib import Path, PurePosixPath

REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_PATHS_FILE = REPO_ROOT / "scripts" / "upstream_sync_drop_paths.txt"


class CheckError(Exception):
    pass


def load_drop_patterns(paths_file: Path) -> tuple[str, ...]:
    try:
        text = paths_file.read_text(encoding="utf-8")
    except OSError as exc:
        raise CheckError(f"无法读取清单 {paths_file}: {exc}") from exc
    except UnicodeDecodeError as exc:
        raise CheckError(f"清单 {paths_file} 必须是 UTF-8") from exc

    patterns: list[str] = []
    for number, raw in enumerate(text.splitlines(), start=1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        _validate_pattern(line, f"{paths_file}:{number}")
        if line in patterns:
            raise CheckError(f"{paths_file}:{number}: 重复的 glob：{line}")
        patterns.append(line)
    # 空清单会让检查恒为绿，属于假安全，直接拒绝。
    if not patterns:
        raise CheckError(f"清单 {paths_file} 不含任何 glob")
    return tuple(patterns)


def _validate_pattern(pattern: str, where: str) -> None:
    # 不支持行尾注释：带空白或 # 的行永远匹配不到文件，必须 fail-fast。
    if "#" in pattern or any(char.isspace() for char in pattern):
        raise CheckError(f"{where}: glob 不得含空白或行尾注释：{pattern!r}")
    if "\\" in pattern:
        raise CheckError(f"{where}: glob 必须用 / 分隔：{pattern!r}")
    if pattern.endswith("/"):
        raise CheckError(f"{where}: 目录请写成 `dir/**`：{pattern!r}")
    path = PurePosixPath(pattern)
    if path.is_absolute() or any(part in {".", ".."} for part in pattern.split("/")):
        raise CheckError(f"{where}: glob 必须是仓库内相对路径：{pattern!r}")
    if any(part == "" for part in pattern.split("/")):
        raise CheckError(f"{where}: glob 含空路径段：{pattern!r}")


def pattern_matches(pattern: str, path: str) -> bool:
    if pattern.endswith("/**") and path == pattern[:-3]:
        return True
    return _compile_pattern(pattern).fullmatch(path) is not None


@lru_cache(maxsize=512)
def _compile_pattern(pattern: str) -> re.Pattern[str]:
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


def find_surviving_paths(patterns: Sequence[str], files: Iterable[str]) -> tuple[str, ...]:
    return tuple(
        sorted({path for path in files if any(pattern_matches(pattern, path) for pattern in patterns)})
    )


def repository_files(root: Path) -> tuple[str, ...]:
    resolved_root = root.resolve()
    if _is_git_toplevel(resolved_root):
        result = subprocess.run(
            ["git", "-C", str(resolved_root), "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
            check=False,
            capture_output=True,
        )
        if result.returncode == 0:
            # 冲突中的路径（如 deleted by us）在索引里有多个 stage，用集合去重。
            return tuple(sorted({item.decode("utf-8") for item in result.stdout.split(b"\0") if item}))
    return _walk_files(resolved_root)


def _is_git_toplevel(root: Path) -> bool:
    # 只有 root 自身是工作树顶层才用 Git 视图；否则（如位于别的仓库内的临时
    # 目录）git 会列出外层仓库的文件，结果不可信。
    try:
        result = subprocess.run(
            ["git", "-C", str(root), "rev-parse", "--show-toplevel"],
            check=False,
            capture_output=True,
            text=True,
        )
    except OSError:
        return False
    if result.returncode != 0:
        return False
    toplevel = result.stdout.strip()
    return bool(toplevel) and Path(toplevel).resolve() == root


def _walk_files(root: Path) -> tuple[str, ...]:
    found: list[str] = []
    for current, dirnames, filenames in os.walk(root, followlinks=False):
        dirnames[:] = [name for name in dirnames if name != ".git"]
        base = Path(current)
        entries = list(filenames)
        # 指向目录的符号链接不会被遍历，按一个路径条目计入。
        entries.extend(name for name in dirnames if (base / name).is_symlink())
        found.extend((base / name).relative_to(root).as_posix() for name in entries)
    return tuple(sorted(found))


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument(
        "--list",
        action="store_true",
        dest="list_only",
        help="只列出仍存在的命中路径，不判定（清单有效时退出码恒为 0）",
    )
    parser.add_argument(
        "--paths-file",
        type=Path,
        default=DEFAULT_PATHS_FILE,
        help="丢弃路径清单（默认 scripts/upstream_sync_drop_paths.txt）",
    )
    parser.add_argument("--root", type=Path, default=REPO_ROOT, help=argparse.SUPPRESS)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        patterns = load_drop_patterns(args.paths_file)
        surviving = find_surviving_paths(patterns, repository_files(args.root))
    except CheckError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    for path in surviving:
        print(path)
    if args.list_only:
        return 0
    if surviving:
        print(
            f"error: {len(surviving)} 个应丢弃的路径仍是 Git 可见文件；"
            "口径见 docs/AGENT_RULES/README.md「fork 已删除的集成」",
            file=sys.stderr,
        )
        return 1
    print(f"OK: {len(patterns)} 条丢弃 glob 均无命中")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
