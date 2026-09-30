from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from scripts import local_build_config as config

try:
    import tomllib
except ModuleNotFoundError:  # Python 3.10
    tomllib = None

PROJECT_ROOT = Path(__file__).resolve().parent.parent
WINDOWS = "x86_64-pc-windows-msvc"
LINUX = "x86_64-unknown-linux-gnu"


def parse(text: str) -> dict:
    if tomllib is None:
        raise unittest.SkipTest("需要 Python 3.11+ 的 tomllib 解析生成的 TOML")
    return tomllib.loads(text)


class RenderTests(unittest.TestCase):
    def test_windows_default_links_with_rust_lld_and_drops_dependency_debuginfo(self) -> None:
        text = config.render(WINDOWS)
        self.assertTrue(text.startswith(config.MARKER))
        data = parse(text)
        self.assertEqual(data["target"][WINDOWS], {"linker": "rust-lld"})
        self.assertEqual(data["profile"]["dev"]["package"]["*"], {"debug": False})
        self.assertNotIn("env", data)
        self.assertNotIn("Zthreads", text)

    def test_non_windows_hosts_keep_their_default_linker(self) -> None:
        data = parse(config.render(LINUX))
        self.assertNotIn("target", data)
        self.assertEqual(data["profile"]["dev"]["package"]["*"], {"debug": False})

    def test_parallel_frontend_is_opt_in_and_scoped_to_the_host_target(self) -> None:
        windows = parse(config.render(WINDOWS, 8))
        self.assertEqual(windows["target"][WINDOWS], {"linker": "rust-lld", "rustflags": ["-Zthreads=8"]})
        self.assertEqual(windows["env"], {"RUSTC_BOOTSTRAP": "1"})
        linux = parse(config.render(LINUX, 4))
        self.assertEqual(linux["target"][LINUX], {"rustflags": ["-Zthreads=4"]})
        self.assertEqual(linux["env"], {"RUSTC_BOOTSTRAP": "1"})


class FileLifecycleTests(unittest.TestCase):
    def setUp(self) -> None:
        self._temp = tempfile.TemporaryDirectory()
        self.path = Path(self._temp.name) / ".cargo" / "config.local.toml"

    def tearDown(self) -> None:
        self._temp.cleanup()

    def test_enable_then_disable_round_trips(self) -> None:
        config.enable(self.path, config.render(WINDOWS))
        self.assertTrue(config.is_generated(self.path))
        config.enable(self.path, config.render(WINDOWS, 8))
        self.assertIn("-Zthreads=8", self.path.read_text(encoding="utf-8"))
        self.assertTrue(config.disable(self.path))
        self.assertFalse(self.path.exists())
        self.assertFalse(config.disable(self.path))

    def test_hand_written_files_are_never_overwritten_or_removed_without_force(self) -> None:
        self.path.parent.mkdir(parents=True)
        self.path.write_text("[build]\njobs = 4\n", encoding="utf-8")
        with self.assertRaisesRegex(config.ConfigError, "--force"):
            config.enable(self.path, config.render(WINDOWS))
        with self.assertRaisesRegex(config.ConfigError, "--force"):
            config.disable(self.path)
        self.assertEqual(self.path.read_text(encoding="utf-8"), "[build]\njobs = 4\n")
        config.enable(self.path, config.render(WINDOWS), force=True)
        self.assertTrue(config.is_generated(self.path))

    def test_parallel_frontend_requires_enable_and_two_threads(self) -> None:
        with self.assertRaises(SystemExit):
            config.main(["--status", "--parallel-frontend"])
        with self.assertRaises(SystemExit):
            config.main(["--enable", "--parallel-frontend", "1"])


class RepositoryWiringTests(unittest.TestCase):
    def test_repo_config_optionally_includes_the_local_file(self) -> None:
        self.assertTrue(config.repo_includes_local_config())
        if tomllib is not None:
            data = tomllib.loads((PROJECT_ROOT / ".cargo" / "config.toml").read_text(encoding="utf-8"))
            self.assertIn({"path": "config.local.toml", "optional": True}, data["include"])

    def test_local_file_is_gitignored(self) -> None:
        ignored = (PROJECT_ROOT / ".gitignore").read_text(encoding="utf-8").splitlines()
        self.assertIn("/.cargo/config.local.toml", ignored)

    def test_recipe_and_maintenance_manifest_are_registered(self) -> None:
        justfile = (PROJECT_ROOT / "justfile").read_text(encoding="utf-8")
        self.assertIn("\nlocal-build-config *args:\n", justfile)
        self.assertIn("scripts/local_build_config.py", justfile)
        self.assertIn("scripts.test_local_build_config", justfile)


if __name__ == "__main__":
    unittest.main()
