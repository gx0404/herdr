#!/usr/bin/env python3
"""scripts/setup_zig.py 行为测试：钉版表完整性、check 只读语义与拒绝边界。"""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from scripts import setup_zig

PROJECT_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = PROJECT_ROOT / "scripts" / "setup_zig.py"


class PinTableTests(unittest.TestCase):
    def test_pins_cover_four_platforms_with_valid_shas(self) -> None:
        self.assertEqual(
            {"x86_64-linux", "aarch64-linux", "x86_64-macos", "aarch64-macos"},
            set(setup_zig.PINS),
        )
        for key, pin in setup_zig.PINS.items():
            self.assertEqual(len(pin["sha256"]), 64, key)
            int(pin["sha256"], 16)  # 必须是十六进制
            self.assertEqual(pin["tarball"], f"zig-{key}-{setup_zig.ZIG_VERSION}.tar.xz")

    def test_download_url_points_at_pinned_release(self) -> None:
        self.assertTrue(setup_zig.DOWNLOAD_BASE.startswith("https://ziglang.org/download/0.16.0/"))

    def test_download_bases_order_respects_custom_mirror(self) -> None:
        import os as _os

        saved = _os.environ.pop("HERDR_ZIG_MIRROR", None)
        try:
            default = setup_zig.download_bases()
            self.assertEqual(default[0], setup_zig.DOWNLOAD_BASE)
            self.assertIn("https://pkg.machengine.org/zig/", default)
            _os.environ["HERDR_ZIG_MIRROR"] = "https://example.cn/zig"
            custom = setup_zig.download_bases()
            self.assertEqual(
                ["https://example.cn/zig/", setup_zig.DOWNLOAD_BASE, "https://pkg.machengine.org/zig/"],
                custom,
            )
        finally:
            if saved is not None:
                _os.environ["HERDR_ZIG_MIRROR"] = saved
            else:
                _os.environ.pop("HERDR_ZIG_MIRROR", None)


class CheckModeTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def _run(self, *args: str, extra_env: dict[str, str] | None = None) -> subprocess.CompletedProcess[bytes]:
        env = dict(os.environ)
        env["HERDR_ZIG_HOME"] = str(self.root / "zig-home")
        env["HERDR_ZIG_LOCAL_BIN"] = str(self.root / "bin")
        env.pop("ZIG", None)
        if extra_env:
            env.update(extra_env)
        return subprocess.run(
            [sys.executable, str(SCRIPT_PATH), *args],
            capture_output=True,
            check=False,
            env=env,
        )

    def test_check_missing_is_readonly_exit_zero(self) -> None:
        result = self._run()
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn(b"MISSING", result.stdout)
        self.assertFalse((self.root / "zig-home").exists(), "check 不得创建目录")

    def test_check_reports_installed_fake_binary(self) -> None:
        zig_dir = self.root / "zig-home" / setup_zig.INSTALL_DIR_NAME
        zig_dir.mkdir(parents=True)
        (zig_dir / "zig").write_text("#!/bin/sh\necho 0.16.0\n")
        (zig_dir / "zig").chmod(0o755)
        result = self._run()
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn(b"INSTALLED", result.stdout)

    def test_install_refuses_existing_dir_without_force(self) -> None:
        zig_dir = self.root / "zig-home" / setup_zig.INSTALL_DIR_NAME
        zig_dir.mkdir(parents=True)
        # 目录存在但无有效 zig（损坏安装）→ 必须报错并提示 --force。
        result = self._run("--install")
        self.assertEqual(2, result.returncode)
        self.assertIn(b"--force", result.stderr)

    def test_install_is_idempotent_when_valid(self) -> None:
        zig_dir = self.root / "zig-home" / setup_zig.INSTALL_DIR_NAME
        zig_dir.mkdir(parents=True)
        (zig_dir / "zig").write_text("#!/bin/sh\necho 0.16.0\n")
        (zig_dir / "zig").chmod(0o755)
        result = self._run("--install")
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("跳过", result.stdout.decode("utf-8"))

    def test_platform_key_maps_machine_aliases(self) -> None:
        self.assertEqual(setup_zig.PINS.get(setup_zig.platform_key()) is not None, True)

    def test_default_install_root_is_project_local(self) -> None:
        # 默认必须自包含在仓库内（gitignored）；HERDR_ZIG_HOME 仅作显式覆盖。
        saved = os.environ.pop("HERDR_ZIG_HOME", None)
        try:
            self.assertEqual(
                setup_zig.install_root(),
                setup_zig.REPO_ROOT / ".local" / "toolchains" / "zig",
            )
        finally:
            if saved is not None:
                os.environ["HERDR_ZIG_HOME"] = saved


if __name__ == "__main__":
    unittest.main()
