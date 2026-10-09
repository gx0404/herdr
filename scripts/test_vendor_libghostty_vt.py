from __future__ import annotations

import io
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts.vendor_libghostty_vt import (
    ensure_dist_archive,
    parse_archive_root,
    require_clean_checkout,
)


class VendorLibghosttyVtTests(unittest.TestCase):
    def test_parse_archive_root_returns_single_top_level_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            archive = Path(temp_dir) / "libghostty-vt.tar.gz"
            with tarfile.open(archive, "w:gz") as tar:
                data = b"hello"
                info = tarfile.TarInfo("libghostty-vt-1.0.0/README.md")
                info.size = len(data)
                tar.addfile(info, io.BytesIO(data))

            self.assertEqual(parse_archive_root(archive), "libghostty-vt-1.0.0")

    def test_ensure_dist_archive_refuses_stale_archives_without_head_match(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo = Path(temp_dir)
            dist = repo / "zig-out" / "dist"
            dist.mkdir(parents=True)
            (dist / "libghostty-vt-1.3.2-main-+deadbeef0.tar.gz").write_bytes(b"stale")

            def git_output(command: list[str], **_kwargs: object) -> str:
                if command[1] == "status":
                    return ""
                return "0123456789abcdef\n"

            with (
                mock.patch("scripts.vendor_libghostty_vt.subprocess.run"),
                mock.patch(
                    "scripts.vendor_libghostty_vt.subprocess.check_output",
                    side_effect=git_output,
                ),
            ):
                with self.assertRaisesRegex(FileNotFoundError, "HEAD 012345678"):
                    ensure_dist_archive(repo)

    def test_require_clean_checkout_rejects_tracked_and_untracked_changes(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo = Path(temp_dir)
            with mock.patch(
                "scripts.vendor_libghostty_vt.subprocess.check_output",
                return_value=" M src/terminal.zig\n?? local.patch\n",
            ):
                with self.assertRaisesRegex(ValueError, "refusing to vendor from dirty checkout"):
                    require_clean_checkout(repo)

    def test_ensure_dist_archive_rejects_checkout_dirtied_by_build(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo = Path(temp_dir)
            with (
                mock.patch("scripts.vendor_libghostty_vt.subprocess.run"),
                mock.patch(
                    "scripts.vendor_libghostty_vt.subprocess.check_output",
                    side_effect=["", "0123456789abcdef\n", " M generated.txt\n"],
                ),
            ):
                with self.assertRaisesRegex(ValueError, "refusing to vendor from dirty checkout"):
                    ensure_dist_archive(repo)

    def test_vendored_tree_contains_required_upstream_files(self) -> None:
        root = Path(__file__).resolve().parent.parent / "vendor" / "libghostty-vt"
        required = [
            root / "build.zig",
            root / "build.zig.zon",
            root / "CMakeLists.txt",
            root / "dist" / "cmake" / "ghostty-vt-config.cmake.in",
            root / "include" / "ghostty" / "vt.h",
            root / "include" / "ghostty" / "vt" / "render.h",
            root / "src" / "lib_vt.zig",
        ]

        missing = [str(path.relative_to(root)) for path in required if not path.exists()]
        self.assertEqual(missing, [])

    def test_vendor_metadata_exists_and_points_at_vendored_tree(self) -> None:
        project_root = Path(__file__).resolve().parent.parent
        metadata = project_root / "vendor" / "libghostty-vt.vendor.json"
        self.assertTrue(metadata.exists())
        text = metadata.read_text(encoding="utf-8")
        self.assertIn('"source_commit"', text)
        self.assertIn('"dist_archive"', text)
        self.assertIn('"extracted_dir"', text)

    def test_local_vendor_patches_are_listed_in_patch_index(self) -> None:
        project_root = Path(__file__).resolve().parent.parent
        index = project_root / "vendor" / "libghostty-vt.patches.md"
        patch_dir = project_root / "vendor" / "patches" / "libghostty-vt"
        patches = sorted(patch_dir.glob("*.patch"))

        if not patches:
            return

        self.assertTrue(index.exists())
        text = index.read_text(encoding="utf-8")
        missing = [
            path.relative_to(project_root).as_posix()
            for path in patches
            if path.relative_to(project_root).as_posix() not in text
        ]
        self.assertEqual(missing, [])

    def test_local_vendor_patches_are_applied_to_vendored_tree(self) -> None:
        project_root = Path(__file__).resolve().parent.parent
        patch_dir = project_root / "vendor" / "patches" / "libghostty-vt"

        for patch in sorted(patch_dir.glob("*.patch")):
            result = subprocess.run(
                ["git", "apply", "--check", "--reverse", str(patch.relative_to(project_root))],
                cwd=project_root,
                text=True,
                capture_output=True,
            )
            self.assertEqual(
                result.returncode,
                0,
                f"{patch.relative_to(project_root)} is not applied cleanly:\n"
                f"stdout:\n{result.stdout}\n"
                f"stderr:\n{result.stderr}",
            )

    def test_embedded_libghostty_logging_is_silenced(self) -> None:
        root = Path(__file__).resolve().parent.parent / "vendor" / "libghostty-vt"
        lib_vt = root / "src" / "lib_vt.zig"
        sys_zig = root / "src" / "terminal" / "c" / "sys.zig"
        lib_text = lib_vt.read_text(encoding="utf-8")
        sys_text = sys_zig.read_text(encoding="utf-8")
        self.assertIn('.logFn = @import("terminal/c/sys.zig").logFn', lib_text)
        self.assertIn("if (global.log == null) return;", sys_text)


ROOT = Path(__file__).resolve().parent.parent
CACHE_ENV = ("ZIG_GLOBAL_CACHE_DIR", "ZIG_LOCAL_CACHE_DIR")


class ZigCacheContract:
    def setUp(self):
        temporary_root = ROOT / "target" / "tmp"
        temporary_root.mkdir(parents=True, exist_ok=True)
        directory = tempfile.TemporaryDirectory(prefix="zig-cache-", dir=temporary_root)
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name) / "repo space 缓存"
        self.manifest = self.root / "crates" / "ghostty-vt"
        self.vendor = self.root / "vendor" / "libghostty-vt"
        self.output = self.root / "target" / "build output"
        for path in (self.manifest, self.vendor, self.output):
            path.mkdir(parents=True)
        (self.vendor / "VERSION").write_text("1.2.3\n", encoding="utf-8")
        (self.vendor / "build.zig").touch()
        (self.vendor / "build").write_text(
            "import json, os, sys\n"
            "from pathlib import Path\n"
            "Path(os.environ['ZIG_RECORD']).write_text(json.dumps({"
            "'cwd': os.getcwd(), 'argv': sys.argv[1:], "
            "'cache': {key: os.environ.get(key) for key in "
            "('ZIG_GLOBAL_CACHE_DIR', 'ZIG_LOCAL_CACHE_DIR')}}), encoding='utf-8')\n",
            encoding="utf-8",
        )

    def check_cache(self, overrides):
        parent = dict(os.environ)
        env = dict(parent)
        for key in (*CACHE_ENV, "LIBGHOSTTY_VT_OPTIMIZE", "LIBGHOSTTY_VT_SIMD",
                    "LIBGHOSTTY_VT_ZIG_SYSTEM_DIR", "LIBGHOSTTY_VT_WINDOWS_LIBC"):
            env.pop(key, None)
        env.update(overrides)
        record = self.root / "record.json"
        env["ZIG_RECORD"] = str(record)
        result = self.run_zig(env)
        self.assertEqual(dict(os.environ), parent)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        data = json.loads(record.read_text(encoding="utf-8"))
        self.assertEqual(Path(data["cwd"]), self.vendor)
        self.assertFalse((self.root / ".local").exists())
        for key, directory in zip(CACHE_ENV, ("global", "local")):
            with self.subTest(variable=key):
                if key in overrides:
                    self.assertEqual(data["cache"][key], overrides[key])
                else:
                    value = data["cache"][key]
                    self.assertIsNotNone(value)
                    if os.name == "nt" and value.startswith("/") and value[2:3] == "/":
                        value = value[1] + ":" + value[2:]
                    self.assertEqual(Path(value), self.root / ".local" / "zig-cache" / directory)
        self.check_invocation(data, result)

    def test_cache_defaults(self):
        self.check_cache({})

    def test_global_override_keeps_local_default(self):
        self.check_cache({CACHE_ENV[0]: str(self.root / "shared global 缓存")})

    def test_local_override_keeps_global_default(self):
        self.check_cache({CACHE_ENV[1]: str(self.root / "shared local 缓存")})

    def test_both_overrides_preserve_relative_spaces_and_unicode(self):
        self.check_cache({CACHE_ENV[0]: "../relative global 缓存", CACHE_ENV[1]: "relative local 缓存"})

    def test_empty_overrides_are_not_defaults(self):
        for overrides in ({CACHE_ENV[0]: ""}, {CACHE_ENV[1]: ""}, dict.fromkeys(CACHE_ENV, "")):
            with self.subTest(overrides=overrides):
                self.check_cache(overrides)

    @unittest.skipUnless(os.name == "posix", "non-Unicode environment bytes require Unix")
    def test_non_unicode_overrides_keep_bytes(self):
        self.check_cache({key: os.fsdecode(b"relative cache-\xff") for key in CACHE_ENV})


class BuildScriptZigCacheTests(ZigCacheContract, unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        rustc = shutil.which("rustc")
        if not rustc:
            raise RuntimeError("rustc is required for the build-script cache contract")
        temporary_root = ROOT / "target" / "tmp"
        temporary_root.mkdir(parents=True, exist_ok=True)
        directory = tempfile.TemporaryDirectory(prefix="zig-build-script-", dir=temporary_root)
        cls.addClassCleanup(directory.cleanup)
        root = Path(directory.name)
        harness = root / "cache_probe.rs"
        source = json.dumps((ROOT / "crates/ghostty-vt/build.rs").as_posix(), ensure_ascii=False)
        harness.write_text(
            "mod build_script { include!(" + source + "); pub fn run() { main(); } }\n"
            "fn main() {\n"
            "    let keys = [\"ZIG_GLOBAL_CACHE_DIR\", \"ZIG_LOCAL_CACHE_DIR\"];\n"
            "    let before = keys.map(std::env::var_os);\n"
            "    build_script::run();\n"
            "    assert_eq!(before, keys.map(std::env::var_os));\n"
            "    println!(\"parent-cache-environment-unchanged\");\n"
            "}\n",
            encoding="utf-8",
        )
        cls.binary = root / ("cache_probe.exe" if os.name == "nt" else "cache_probe")
        result = subprocess.run(
            [rustc, "--edition=2021", str(harness), "-o", str(cls.binary)],
            cwd=root, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=120,
        )
        if result.returncode:
            raise RuntimeError(f"build-script compile failed:\n{result.stdout}{result.stderr}")

    def run_zig(self, env):
        env.update({"ZIG": sys.executable, "CARGO_MANIFEST_DIR": str(self.manifest),
                    "OUT_DIR": str(self.output), "TARGET": "x86_64-pc-windows-msvc"})
        return subprocess.run([str(self.binary)], cwd=self.root, env=env, capture_output=True,
                              text=True, encoding="utf-8", errors="replace", timeout=30)

    def check_invocation(self, data, result):
        self.assertEqual(data["argv"], [
            "--prefix", str(self.output / "zig-out"), "-Demit-lib-vt", "-Doptimize=ReleaseFast",
            "-Dsimd=true", "-Dtarget=x86_64-windows-msvc", "-Dversion-string=1.2.3",
            "-Demit-xcframework=false",
        ])
        self.assertIn("parent-cache-environment-unchanged", result.stdout)
        for key in CACHE_ENV:
            self.assertIn(f"cargo:rerun-if-env-changed={key}\n", result.stdout)
        self.assertIn(f"cargo:rustc-link-search=native={self.output / 'zig-out' / 'lib'}\n", result.stdout)
        self.assertIn("cargo:rustc-link-lib=static=ghostty-vt-static\n", result.stdout)


class ShellZigCacheTests(ZigCacheContract, unittest.TestCase):
    def run_zig(self, env):
        bash = shutil.which("bash")
        self.assertIsNotNone(bash, "bash is required for the direct Zig build entrypoint")
        scripts = self.root / "scripts"
        scripts.mkdir(exist_ok=True)
        script = scripts / "build_vendored_libghostty_vt.sh"
        shutil.copyfile(ROOT / "scripts" / script.name, script)
        binary_dir = self.root / "bin"
        binary_dir.mkdir(exist_ok=True)
        zig = binary_dir / "zig"
        zig.write_text('#!/usr/bin/env bash\nexec "$FIXTURE_PYTHON" build "$@"\n', encoding="utf-8")
        zig.chmod(0o755)
        env.update({"FIXTURE_PYTHON": Path(sys.executable).as_posix(),
                    "PATH": str(binary_dir) + os.pathsep + env.get("PATH", ""),
                    "VENDORED_GHOSTTY_DIR": self.vendor.as_posix()})
        return subprocess.run([bash, str(script), "--prefix", "custom output"], cwd=self.output,
                              env=env, capture_output=True, text=True, encoding="utf-8",
                              errors="replace", timeout=30)

    def check_invocation(self, data, result):
        self.assertEqual(data["argv"], ["build", "-Demit-lib-vt", "-Doptimize=ReleaseFast",
                                       "--prefix", "custom output"])
        self.assertIn("built libghostty-vt", result.stdout)


if __name__ == "__main__":
    unittest.main()
