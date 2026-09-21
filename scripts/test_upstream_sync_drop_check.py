#!/usr/bin/env python3
"""scripts/upstream_sync_drop_check.py 的行为测试：清单解析、glob 语义与判定。

全部用临时目录构造，不依赖真实仓库此刻是否还留着非六家集成。
"""

from __future__ import annotations

import contextlib
import io
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import upstream_sync_drop_check as check

DROP_LIST = """\
# 注释与空行会被跳过

src/integration/assets/copilot/**
src/detect/manifests/amp.toml
"""


def _write(path: Path, content: str = "x\n") -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def _run(*argv: str) -> tuple[int, str, str]:
    stdout, stderr = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
        code = check.main(list(argv))
    return code, stdout.getvalue(), stderr.getvalue()


class DropListParsingTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.paths_file = Path(self._tmp.name) / "drop.txt"

    def test_skips_comments_and_blank_lines_and_keeps_order(self):
        self.paths_file.write_text(DROP_LIST, encoding="utf-8")
        self.assertEqual(
            check.load_drop_patterns(self.paths_file),
            ("src/integration/assets/copilot/**", "src/detect/manifests/amp.toml"),
        )

    def test_rejects_invalid_lists(self):
        cases = {
            "缺失文件": None,
            "空清单": "# 只有注释\n\n",
            "重复": "a/b.toml\na/b.toml\n",
            "绝对路径": "/etc/passwd\n",
            "越界": "src/../../outside\n",
            "当前目录段": "./src/a.toml\n",
            "空路径段": "src//a.toml\n",
            "反斜杠": "src\\a.toml\n",
            "目录尾斜杠": "src/integration/assets/copilot/\n",
            "行尾注释": "src/a.toml # 说明\n",
        }
        for name, content in cases.items():
            with self.subTest(name):
                if content is None:
                    self.paths_file.unlink(missing_ok=True)
                else:
                    self.paths_file.write_text(content, encoding="utf-8")
                with self.assertRaises(check.CheckError):
                    check.load_drop_patterns(self.paths_file)


class GlobSemanticsTests(unittest.TestCase):
    def test_directory_glob_matches_nested_files_and_the_directory_itself(self):
        pattern = "src/integration/assets/copilot/**"
        self.assertTrue(check.pattern_matches(pattern, "src/integration/assets/copilot/plugin.json"))
        self.assertTrue(check.pattern_matches(pattern, "src/integration/assets/copilot/hooks/a/b.sh"))
        self.assertTrue(check.pattern_matches(pattern, "src/integration/assets/copilot"))

    def test_directory_glob_does_not_match_sibling_with_shared_prefix(self):
        pattern = "src/integration/assets/pi/**"
        self.assertFalse(check.pattern_matches(pattern, "src/integration/assets/pilot/plugin.json"))

    def test_single_star_does_not_cross_directories(self):
        self.assertTrue(check.pattern_matches("src/detect/manifests/*.toml", "src/detect/manifests/amp.toml"))
        self.assertFalse(check.pattern_matches("src/detect/manifests/*.toml", "src/detect/manifests/sub/amp.toml"))

    def test_exact_path_matches_only_itself(self):
        self.assertTrue(check.pattern_matches("src/detect/manifests/amp.toml", "src/detect/manifests/amp.toml"))
        self.assertFalse(check.pattern_matches("src/detect/manifests/amp.toml", "src/detect/manifests/amp.toml.bak"))
        self.assertFalse(
            check.pattern_matches("src/detect/manifests/amp.toml", "distribution/agent-detection/amp.toml")
        )

    def test_find_surviving_paths_is_sorted_and_deduplicated(self):
        surviving = check.find_surviving_paths(
            ("b/**", "a.toml", "b/x.toml"),
            ["keep.toml", "b/x.toml", "a.toml", "b/x.toml", "b/sub/y.toml"],
        )
        self.assertEqual(surviving, ("a.toml", "b/sub/y.toml", "b/x.toml"))


class MainTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name) / "repo"
        self.root.mkdir()
        self.paths_file = Path(self._tmp.name) / "drop.txt"
        self.paths_file.write_text(DROP_LIST, encoding="utf-8")
        # 六家保留路径：任何模式下都不得被报告。
        _write(self.root / "src/integration/assets/claude/hook.sh")
        _write(self.root / "src/detect/manifests/claude.toml")

    def _args(self, *extra: str) -> tuple[str, ...]:
        return (*extra, "--root", str(self.root), "--paths-file", str(self.paths_file))

    def test_passes_when_no_dropped_path_exists(self):
        code, stdout, stderr = _run(*self._args())
        self.assertEqual(code, 0)
        self.assertIn("OK", stdout)
        self.assertEqual(stderr, "")

    def test_fails_and_reports_only_dropped_paths(self):
        _write(self.root / "src/integration/assets/copilot/hooks/state.sh")
        _write(self.root / "src/detect/manifests/amp.toml")
        code, stdout, stderr = _run(*self._args())
        self.assertEqual(code, 1)
        self.assertEqual(
            stdout.splitlines(),
            ["src/detect/manifests/amp.toml", "src/integration/assets/copilot/hooks/state.sh"],
        )
        self.assertIn("2 个应丢弃的路径", stderr)

    def test_list_mode_reports_without_failing(self):
        _write(self.root / "src/detect/manifests/amp.toml")
        code, stdout, stderr = _run(*self._args("--list"))
        self.assertEqual(code, 0)
        self.assertEqual(stdout.splitlines(), ["src/detect/manifests/amp.toml"])
        self.assertEqual(stderr, "")

    def test_list_mode_prints_nothing_for_a_clean_tree(self):
        code, stdout, _ = _run(*self._args("--list"))
        self.assertEqual(code, 0)
        self.assertEqual(stdout, "")

    def test_invalid_list_exits_with_usage_error_even_in_list_mode(self):
        self.paths_file.write_text("# 空\n", encoding="utf-8")
        for extra in ((), ("--list",)):
            with self.subTest(extra):
                code, stdout, stderr = _run(*self._args(*extra))
                self.assertEqual(code, 2)
                self.assertEqual(stdout, "")
                self.assertIn("error:", stderr)

    def test_walk_fallback_counts_directory_symlinks_and_skips_dot_git(self):
        _write(self.root / ".git/src/detect/manifests/amp.toml")
        target = Path(self._tmp.name) / "elsewhere"
        target.mkdir()
        (self.root / "src/integration/assets/copilot").symlink_to(target, target_is_directory=True)
        files = check.repository_files(self.root)
        self.assertIn("src/integration/assets/copilot", files)
        self.assertFalse(any(path.startswith(".git/") for path in files))
        code, stdout, _ = _run(*self._args())
        self.assertEqual(code, 1)
        self.assertEqual(stdout.splitlines(), ["src/integration/assets/copilot"])

    def test_git_view_ignores_ignored_leftovers_but_sees_index_and_untracked(self):
        if shutil.which("git") is None:
            self.skipTest("需要 git")
        # 继承的 GIT_* 会把 git 重定向到别的仓库，测试内清掉。
        scrubbed = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
        with mock.patch.dict(os.environ, scrubbed, clear=True):
            subprocess.run(["git", "init", "-q", str(self.root)], check=True, capture_output=True)
            _write(self.root / ".gitignore", "__pycache__/\n")
            # 删除后常见的被忽略残留：不算「被带回」。
            _write(self.root / "src/integration/assets/copilot/__pycache__/mod.pyc")
            # 已进索引的与未忽略的未跟踪文件：都要报告。
            _write(self.root / "src/detect/manifests/amp.toml")
            subprocess.run(
                ["git", "-C", str(self.root), "add", "src/detect/manifests/amp.toml"],
                check=True,
                capture_output=True,
            )
            _write(self.root / "src/integration/assets/copilot/plugin.json")
            code, stdout, _ = _run(*self._args())
        self.assertEqual(code, 1)
        self.assertEqual(
            stdout.splitlines(),
            ["src/detect/manifests/amp.toml", "src/integration/assets/copilot/plugin.json"],
        )


class BundledDropListTests(unittest.TestCase):
    """入库清单的静态不变量：只校验清单自身，不看仓库里这些路径是否还在。"""

    KEPT = ("claude", "codex", "kimi", "zcode", "pi", "opencode")

    def test_bundled_list_is_valid(self):
        self.assertTrue(check.load_drop_patterns(check.DEFAULT_PATHS_FILE))

    def test_bundled_list_never_drops_a_kept_integration(self):
        patterns = check.load_drop_patterns(check.DEFAULT_PATHS_FILE)
        for name in self.KEPT:
            for path in (
                f"src/integration/assets/{name}/hook.sh",
                f"src/detect/manifests/{name}.toml",
                f"distribution/agent-detection/{name}.toml",
            ):
                with self.subTest(path):
                    self.assertEqual(check.find_surviving_paths(patterns, [path]), ())
        shared = ["distribution/agent-detection/index.toml", "src/integration/assets/herdr-agent-state.test.ts"]
        self.assertEqual(check.find_surviving_paths(patterns, shared), ())


if __name__ == "__main__":
    unittest.main()
