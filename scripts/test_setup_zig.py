#!/usr/bin/env python3
"""Pinned Zig resolution, read-only checks, safe archives and transactional installation."""

from __future__ import annotations

import contextlib
import hashlib
import io
import os
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path
from unittest import mock

from scripts import setup_zig

PROJECT_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = PROJECT_ROOT / "scripts" / "setup_zig.py"


class LocalTestCase(unittest.TestCase):
    def setUp(self) -> None:
        temporary_root = PROJECT_ROOT / "target" / "tmp"
        temporary_root.mkdir(parents=True, exist_ok=True)
        self._tmp = tempfile.TemporaryDirectory(prefix="setup-zig-test-", dir=temporary_root)
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)
        self.enterContext(mock.patch.object(setup_zig, "REPO_ROOT", self.root))
        self.enterContext(mock.patch.dict(os.environ, {
            "HERDR_ZIG_HOME": str(self.root / "zig-home"),
            "TMP": str(self.root), "TEMP": str(self.root), "TMPDIR": str(self.root),
        }))
        os.environ.pop("ZIG", None)
        self.output = io.StringIO()
        self.enterContext(contextlib.redirect_stdout(self.output))
        self.enterContext(contextlib.redirect_stderr(self.output))

    def binary(self, directory: Path, content: bytes = b"fake zig") -> Path:
        directory.mkdir(parents=True, exist_ok=True)
        binary = directory / ("zig.exe" if os.name == "nt" else "zig")
        binary.write_bytes(content)
        return binary


class PinTableTests(unittest.TestCase):
    def test_pins_cover_all_platforms_with_valid_shas(self) -> None:
        self.assertEqual({"x86_64-linux", "aarch64-linux", "x86_64-macos",
                          "aarch64-macos", "x86_64-windows"}, set(setup_zig.PINS))
        for key, pin in setup_zig.PINS.items():
            self.assertEqual(len(pin["sha256"]), 64, key)
            int(pin["sha256"], 16)
            suffix = ".zip" if key.endswith("windows") else ".tar.xz"
            self.assertEqual(pin["tarball"], f"zig-{key}-{setup_zig.ZIG_VERSION}{suffix}")

    def test_download_bases_order_respects_custom_mirror(self) -> None:
        with mock.patch.dict(os.environ, {"HERDR_ZIG_MIRROR": "https://example.cn/zig"}):
            self.assertEqual(["https://example.cn/zig/", "https://pkg.machengine.org/zig/",
                              setup_zig.DOWNLOAD_BASE], setup_zig.download_bases())
        self.assertEqual("https://ziglang.org/download/0.16.0/", setup_zig.DOWNLOAD_BASE)


class CheckModeTests(LocalTestCase):
    def test_check_missing_is_readonly_and_nonzero(self) -> None:
        before = list(self.root.rglob("*"))
        with mock.patch.object(setup_zig.shutil, "which", return_value=None), \
                mock.patch.object(setup_zig, "_download") as download, \
                mock.patch.object(Path, "mkdir", side_effect=AssertionError("check wrote")), \
                mock.patch.object(Path, "open", side_effect=AssertionError("check opened file")):
            self.assertEqual(2, setup_zig.check())
        self.assertEqual(before, list(self.root.rglob("*")))
        download.assert_not_called()
        self.assertIn("MISSING", self.output.getvalue())

    def test_explicit_override_wins_even_when_wrong_or_missing(self) -> None:
        self.binary(setup_zig.build_auto_root() / setup_zig.INSTALL_DIR_NAME)
        for version in (None, "0.15.2", setup_zig.ZIG_VERSION):
            with self.subTest(version=version), mock.patch.dict(os.environ, {"ZIG": "chosen-zig"}), \
                    mock.patch.object(setup_zig, "_run_zig_version", return_value=version) as probe, \
                    mock.patch.object(setup_zig.shutil, "which") as which:
                self.assertEqual(("chosen-zig", "ZIG"), setup_zig.resolve_effective_zig())
                self.assertEqual(0 if version == setup_zig.ZIG_VERSION else 2, setup_zig.check())
                probe.assert_called_once_with("chosen-zig")
                which.assert_not_called()

    def test_empty_explicit_override_does_not_fall_back(self) -> None:
        for value in ("", "  "):
            with self.subTest(value=value), mock.patch.dict(os.environ, {"ZIG": value}), \
                    mock.patch.object(setup_zig, "_run_zig_version") as probe:
                self.assertEqual(2, setup_zig.check())
                probe.assert_not_called()

    def test_project_precedes_path_including_exe_on_unix(self) -> None:
        directory = setup_zig.build_auto_root() / setup_zig.INSTALL_DIR_NAME
        self.binary(directory)
        expected = directory / "zig.exe"
        expected.write_bytes(b"exe")
        with mock.patch.object(setup_zig.shutil, "which") as which:
            self.assertEqual((str(expected), "project"), setup_zig.resolve_effective_zig())
        which.assert_not_called()

    def test_custom_home_is_not_build_auto_root(self) -> None:
        self.binary(setup_zig.install_dir())
        with mock.patch.object(setup_zig.shutil, "which", return_value="path-zig"), \
                mock.patch.object(setup_zig, "_run_zig_version", return_value=setup_zig.ZIG_VERSION) as probe:
            self.assertEqual(0, setup_zig.check())
        probe.assert_called_once_with("path-zig")
        self.assertIn(f"install_root: {setup_zig.install_root()}", self.output.getvalue())
        self.assertIn(f"build auto root: {setup_zig.build_auto_root()}", self.output.getvalue())

    def test_invalid_project_does_not_fall_back(self) -> None:
        self.binary(setup_zig.build_auto_root() / setup_zig.INSTALL_DIR_NAME)
        with mock.patch.object(setup_zig, "_run_zig_version", return_value="0.15.2"), \
                mock.patch.object(setup_zig.shutil, "which") as which:
            self.assertEqual(2, setup_zig.check())
        which.assert_not_called()

    def test_default_install_root_is_project_local(self) -> None:
        os.environ.pop("HERDR_ZIG_HOME")
        self.assertEqual(setup_zig.build_auto_root(), setup_zig.install_root())

    def test_version_probe_checks_exit_timeout_and_local_caches_without_writes(self) -> None:
        with mock.patch.dict(os.environ, {"ZIG_GLOBAL_CACHE_DIR": "outside", "ZIG_LOCAL_CACHE_DIR": ""}), \
                mock.patch.object(setup_zig.subprocess, "run", return_value=subprocess.CompletedProcess(
                    [], 0, "0.16.0\n", "")) as run:
            self.assertEqual("0.16.0", setup_zig._run_zig_version("zig"))
            self.assertEqual(30, run.call_args.kwargs["timeout"])
            env = run.call_args.kwargs["env"]
            for name in ("ZIG_GLOBAL_CACHE_DIR", "ZIG_LOCAL_CACHE_DIR"):
                self.assertTrue(Path(env[name]).is_relative_to(self.root))
        self.assertEqual([], list(self.root.iterdir()))
        for outcome in (OSError("missing"), subprocess.TimeoutExpired("zig", 30),
                        subprocess.CompletedProcess([], 1, "0.16.0", "failure")):
            with self.subTest(outcome=outcome), mock.patch.object(setup_zig.subprocess, "run") as run:
                if isinstance(outcome, Exception):
                    run.side_effect = outcome
                else:
                    run.return_value = outcome
                self.assertIsNone(setup_zig._run_zig_version("zig"))

    def test_cli_rejects_conflicting_modes_and_force_without_install(self) -> None:
        for args in (["--check", "--install"], ["--force"], ["--check", "--force"]):
            with self.subTest(args=args), mock.patch.object(setup_zig, "install") as install:
                with self.assertRaises(SystemExit) as raised:
                    setup_zig.main(args)
                self.assertEqual(2, raised.exception.code)
                install.assert_not_called()

    def test_real_cli_default_missing_override_fails_without_creating_home(self) -> None:
        env = dict(os.environ, ZIG=str(self.root / "missing-zig"))
        result = subprocess.run([sys.executable, "-B", str(SCRIPT_PATH)], env=env,
                                capture_output=True, timeout=30, check=False)
        self.assertEqual(2, result.returncode, result.stderr)
        self.assertFalse(setup_zig.install_root().exists())


class ArchiveTests(LocalTestCase):
    def test_zip_does_not_invoke_available_gnu_tar(self) -> None:
        archive = self.root / "zig.zip"
        with zipfile.ZipFile(archive, "w") as zf:
            zf.writestr("package/zig.exe", b"binary")
        with mock.patch.object(setup_zig.shutil, "which", return_value="/usr/bin/tar"), \
                mock.patch.object(setup_zig.subprocess, "run", side_effect=AssertionError("GNU tar used")):
            setup_zig._extract(archive, self.root / "out")
        self.assertEqual(b"binary", (self.root / "out/package/zig.exe").read_bytes())

    def test_tar_xz_extracts_regular_files_and_executable_mode(self) -> None:
        archive = self.root / "zig.tar.xz"
        with tarfile.open(archive, "w:xz") as tf:
            member = tarfile.TarInfo("package/zig")
            member.size, member.mode = 6, 0o755
            tf.addfile(member, io.BytesIO(b"binary"))
        setup_zig._extract(archive, self.root / "out")
        binary = self.root / "out/package/zig"
        self.assertEqual(b"binary", binary.read_bytes())
        if os.name != "nt":
            self.assertEqual(0o755, stat.S_IMODE(binary.stat().st_mode))

    def test_zip_and_tar_reject_traversal_and_links(self) -> None:
        for kind in ("zip", "tar"):
            for name in ("../escape", "/absolute", "C:/drive", "..\\escape", "file:stream",
                         ".. /escape", "NUL", "link"):
                with self.subTest(kind=kind, name=name):
                    archive = self.root / ("bad.zip" if kind == "zip" else "bad.tar.xz")
                    if kind == "zip":
                        with zipfile.ZipFile(archive, "w") as zf:
                            info = zipfile.ZipInfo(name)
                            if name == "link":
                                info.external_attr = (stat.S_IFLNK | 0o777) << 16
                            zf.writestr(info, b"../escape")
                    else:
                        with tarfile.open(archive, "w:xz") as tf:
                            info = tarfile.TarInfo(name)
                            if name == "link":
                                info.type, info.linkname = tarfile.SYMTYPE, "../escape"
                            tf.addfile(info)
                    with self.assertRaises(setup_zig.SetupZigError):
                        setup_zig._extract(archive, self.root / "out")
                    self.assertFalse((self.root / "out").exists())
                    self.assertFalse((self.root / "escape").exists())

    def test_tar_rejects_hardlinks_and_special_files(self) -> None:
        for kind in (tarfile.LNKTYPE, tarfile.FIFOTYPE, tarfile.CHRTYPE):
            with self.subTest(kind=kind):
                archive = self.root / "bad.tar.xz"
                with tarfile.open(archive, "w:xz") as tf:
                    info = tarfile.TarInfo("unsafe")
                    info.type, info.linkname = kind, "outside"
                    tf.addfile(info)
                with self.assertRaises(setup_zig.SetupZigError):
                    setup_zig._extract(archive, self.root / "out")


class InstallTests(LocalTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.archive = self.root / "fixture.zip"
        self.stem = "zig-test-0.16.0"
        name = "zig.exe" if os.name == "nt" else "zig"
        with zipfile.ZipFile(self.archive, "w") as zf:
            zf.writestr(f"{self.stem}/{name}", b"new")
        pin = {"tarball": f"{self.stem}.zip", "sha256": hashlib.sha256(self.archive.read_bytes()).hexdigest()}
        self.enterContext(mock.patch.object(setup_zig, "platform_key", return_value="test"))
        self.enterContext(mock.patch.dict(setup_zig.PINS, {"test": pin}))
        self.download = self.enterContext(mock.patch.object(setup_zig, "_download", side_effect=
            lambda _name, dest: shutil.copyfile(self.archive, dest)))
        self.probe = self.enterContext(mock.patch.object(setup_zig, "_run_zig_version",
                                                        return_value=setup_zig.ZIG_VERSION))

    def test_install_then_idempotent_skip(self) -> None:
        self.assertEqual(0, setup_zig.install(False))
        self.assertEqual(b"new", setup_zig.zig_binary().read_bytes())
        self.assertEqual(0, setup_zig.install(False))
        self.download.assert_called_once()
        self.assertEqual([setup_zig.install_dir()], list(setup_zig.install_root().iterdir()))

    def test_force_validates_while_old_is_still_present_then_replaces(self) -> None:
        old = self.binary(setup_zig.install_dir(), b"old")

        def probe(staged: Path) -> str:
            self.assertEqual(b"old", old.read_bytes())
            self.assertEqual(b"new", staged.read_bytes())
            return setup_zig.ZIG_VERSION

        self.probe.side_effect = probe
        self.assertEqual(0, setup_zig.install(True))
        self.assertEqual(b"new", old.read_bytes())
        self.assertEqual([setup_zig.install_dir()], list(setup_zig.install_root().iterdir()))

    def test_failed_download_cleans_staging_and_preserves_old(self) -> None:
        old = self.binary(setup_zig.install_dir(), b"old")

        def fail(_name: str, destination: Path) -> None:
            destination.write_bytes(b"partial download")
            raise setup_zig.SetupZigError("network failure")

        self.download.side_effect = fail
        with self.assertRaisesRegex(setup_zig.SetupZigError, "network failure"):
            setup_zig.install(True)
        self.assertEqual(b"old", old.read_bytes())
        self.assertEqual([setup_zig.install_dir()], list(setup_zig.install_root().iterdir()))
        self.probe.assert_not_called()

    def test_corrupt_install_requires_force(self) -> None:
        for has_binary in (False, True):
            with self.subTest(has_binary=has_binary):
                setup_zig.install_dir().mkdir(parents=True, exist_ok=True)
                if has_binary:
                    self.binary(setup_zig.install_dir(), b"corrupt")
                self.probe.return_value = "0.15.2"
                with self.assertRaisesRegex(setup_zig.SetupZigError, "--force"):
                    setup_zig.install(False)
        self.download.assert_not_called()

    def test_staged_version_is_validated_before_replacing_old(self) -> None:
        old = self.binary(setup_zig.install_dir(), b"old")
        self.probe.return_value = "wrong"
        with self.assertRaisesRegex(setup_zig.SetupZigError, "version"):
            setup_zig.install(True)
        self.assertEqual(b"old", old.read_bytes())
        self.assertIn(".zig-stage-", str(self.probe.call_args.args[0]))

    def test_hash_mismatch_preserves_old_without_extracting_or_probing(self) -> None:
        old = self.binary(setup_zig.install_dir(), b"old")
        with mock.patch.object(setup_zig, "_sha256", return_value="wrong"), \
                mock.patch.object(setup_zig, "_extract") as extract:
            with self.assertRaisesRegex(setup_zig.SetupZigError, "sha256"):
                setup_zig.install(True)
        self.assertEqual(b"old", old.read_bytes())
        extract.assert_not_called()
        self.probe.assert_not_called()

    def test_replacement_failure_restores_old(self) -> None:
        old = self.binary(setup_zig.install_dir(), b"old")
        rename = Path.rename

        def fail_staged(path: Path, target: Path) -> Path:
            if path.name == self.stem:
                raise PermissionError("locked target")
            return rename(path, target)

        with mock.patch.object(Path, "rename", fail_staged):
            with self.assertRaisesRegex(setup_zig.SetupZigError, "locked target"):
                setup_zig.install(True)
        self.assertEqual(b"old", old.read_bytes())
        self.assertEqual([setup_zig.install_dir()], list(setup_zig.install_root().iterdir()))

    def test_backup_rename_failure_keeps_old(self) -> None:
        old = self.binary(setup_zig.install_dir(), b"old")
        with mock.patch.object(Path, "rename", side_effect=PermissionError("old locked")):
            self.assertEqual(2, setup_zig.main(["--install", "--force"]))
        self.assertEqual(b"old", old.read_bytes())

    def test_failed_rollback_keeps_recoverable_backup(self) -> None:
        self.binary(setup_zig.install_dir(), b"old")
        rename = Path.rename

        def fail_new_and_restore(path: Path, target: Path) -> Path:
            if path == setup_zig.install_dir():
                return rename(path, target)
            raise PermissionError("locked")

        with mock.patch.object(Path, "rename", fail_new_and_restore):
            with self.assertRaisesRegex(setup_zig.SetupZigError, "旧安装保留"):
                setup_zig.install(True)
        backups = list(setup_zig.install_root().glob(".zig-backup-*"))
        self.assertEqual(1, len(backups))
        self.assertEqual(b"old", setup_zig.zig_binary(backups[0]).read_bytes())

    def test_existing_lock_is_not_ignored_or_removed(self) -> None:
        lock = setup_zig.install_root() / ".install-lock"
        lock.mkdir(parents=True)
        with self.assertRaisesRegex(setup_zig.SetupZigError, "安装锁"):
            setup_zig.install(True)
        self.assertTrue(lock.is_dir())
        self.download.assert_not_called()

    def test_symlink_install_is_refused_even_with_force(self) -> None:
        with mock.patch.object(Path, "is_symlink", return_value=True):
            with self.assertRaisesRegex(setup_zig.SetupZigError, "链接目录"):
                setup_zig.install(True)
        self.download.assert_not_called()


if __name__ == "__main__":
    unittest.main()
