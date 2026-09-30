from __future__ import annotations

import contextlib
import io
import json
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import gx_package as package


SHA = "1234567890abcdef1234567890abcdef12345678"
VERSION = "0.9.2"
TOOLS = {name: Path(name) for name in ("rustup", "zig", "iscc", "dpkg-deb", "readelf", "nm", "musl-gcc")}


def pe(machine: int = 0x8664) -> bytes:
    data = bytearray(512)
    data[:2] = b"MZ"
    struct.pack_into("<I", data, 0x3C, 0x80)
    data[0x80:0x84] = b"PE\0\0"
    struct.pack_into("<H", data, 0x84, machine)
    struct.pack_into("<H", data, 0x94, 240)
    struct.pack_into("<H", data, 0x98, 0x20B)
    return bytes(data)


def elf(machine: int = 62, program_type: int = 1) -> bytes:
    data = bytearray(120)
    data[:7] = b"\x7fELF\x02\x01\x01"
    struct.pack_into("<HH", data, 16, 2, machine)
    struct.pack_into("<Q", data, 32, 64)
    struct.pack_into("<HH", data, 54, 56, 1)
    struct.pack_into("<I", data, 64, program_type)
    return bytes(data)


class GXPackageTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="herdr-gx-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        (self.root / "Cargo.toml").write_text('[package]\nname = "herdr"\nversion = "0.9.2"\n', encoding="utf-8")
        (self.root / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "1.96.1"\n', encoding="utf-8")
        (self.root / "LICENSE").write_text("Herdr license\n", encoding="utf-8")
        (self.root / "packaging/linux").mkdir(parents=True)
        shutil.copyfile(package.ROOT / "packaging/linux/control.in", self.root / "packaging/linux/control.in")
        self.binary = self.root / "herdr.exe"
        self.binary.write_bytes(pe())

    def test_cargo_and_toolchain_are_the_version_sources(self) -> None:
        self.assertEqual(package.cargo_version(self.root), VERSION)
        self.assertEqual(package.rust_toolchain(self.root), "1.96.1")
        self.assertRegex(package.cargo_version(), "^" + package.VERSION_PATTERN + "$")

    def test_invalid_cargo_versions_fail(self) -> None:
        for version in ("v0.9.1", "0.9", "0.9.1-preview", "01.9.1", "1.2.65536", "../../escape", "1.2.3+meta"):
            with self.subTest(version=version):
                (self.root / "Cargo.toml").write_text(f'[package]\nname="herdr"\nversion="{version}"\n', encoding="utf-8")
                with self.assertRaisesRegex(ValueError, "version X.Y.Z"):
                    package.cargo_version(self.root)

    def test_unpinned_toolchain_fails(self) -> None:
        (self.root / "rust-toolchain.toml").write_text('[toolchain]\nchannel="stable"\n', encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "exact Rust"):
            package.rust_toolchain(self.root)

    def test_source_identity_includes_untracked_and_uses_full_sha(self) -> None:
        with mock.patch.object(package, "output", side_effect=[SHA, "?? new-source.rs"]) as output:
            self.assertEqual(package.source_info(self.root), (SHA, True))
        status = output.call_args_list[-1].args[0]
        self.assertIn("--untracked-files=all", status)
        self.assertNotIn("--ignored", status)
        self.assertIn("--no-optional-locks", status)
        for invalid in (SHA[:8], SHA.upper(), "not-a-sha"):
            with mock.patch.object(package, "output", return_value=invalid):
                with self.assertRaisesRegex(ValueError, "40-character"):
                    package.source_info(self.root)

    def test_dirty_requires_explicit_local_opt_in(self) -> None:
        package.require_clean(False, False)
        package.require_clean(True, True)
        with self.assertRaisesRegex(ValueError, "--allow-dirty"):
            package.require_clean(True, False)

    def test_iscc_override_and_standard_install_discovery(self) -> None:
        compiler = self.root / "Inno Setup 6/ISCC.exe"
        compiler.parent.mkdir()
        compiler.write_bytes(b"compiler")
        with mock.patch.dict(os.environ, {"ISCC": str(compiler)}, clear=True):
            self.assertEqual(package.tool("iscc"), compiler.resolve())
        with mock.patch.dict(os.environ, {"ISCC": str(self.root / "missing")}, clear=True):
            with mock.patch.object(package.shutil, "which", return_value=None):
                with self.assertRaisesRegex(ValueError, "ISCC points to a missing"):
                    package.tool("iscc")
        with mock.patch.dict(os.environ, {"ProgramFiles(x86)": str(self.root)}, clear=True):
            with mock.patch.object(package.shutil, "which", return_value=None):
                self.assertEqual(package.tool("iscc"), compiler)

    def test_build_always_invokes_pinned_cargo_with_isolated_target_and_identity(self) -> None:
        inherited = {"RUSTFLAGS": "-C target-feature=-crt-static", "HERDR_BUILD_CHANNEL": "preview", "HERDR_BUILD_ID": "old"}
        for kind in ("windows", "linux"):
            with self.subTest(platform=kind), mock.patch.dict(os.environ, inherited):
                with mock.patch.object(package, "run") as run:
                    result = package.build_binary(kind, SHA, TOOLS, self.root)
                argv = run.call_args.args[0]
                self.assertEqual(argv[:6], [TOOLS["rustup"], "run", "1.96.1", "cargo", "build", "--locked"])
                self.assertIn(package.TARGETS[kind], argv)
                env = run.call_args.kwargs["env"]
                self.assertEqual(env["HERDR_BUILD_COMMIT"], SHA)
                self.assertEqual(env["HERDR_PACKAGE_MANAGER"], package.MANAGERS[kind])
                self.assertEqual(env["RUSTUP_AUTO_INSTALL"], "0")
                self.assertEqual(env["CARGO_ENCODED_RUSTFLAGS"], "-C\x1ftarget-feature=+crt-static")
                self.assertNotIn("RUSTFLAGS", env)
                self.assertNotIn("HERDR_BUILD_CHANNEL", env)
                self.assertNotIn("HERDR_BUILD_ID", env)
                self.assertTrue(result.is_relative_to(self.root / "target/gx" / kind))
                if kind == "linux":
                    self.assertEqual(env["CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER"], "musl-gcc")

    def test_correct_windows_binary_identity_passes(self) -> None:
        version = f"herdr {VERSION}-gx.windows-installer.{SHA}"
        with mock.patch.object(package, "output", return_value=version):
            result = package.verify_binary(self.binary, "windows", VERSION, SHA, TOOLS)
        self.assertEqual(result, {"sha256": package.digest(self.binary), "version_output": version})

    def test_wrong_binary_identity_fails(self) -> None:
        for version in (
            f"herdr {VERSION}", f"herdr {VERSION}-gx.deb.{SHA}",
            f"herdr {VERSION}-gx.windows-installer.{SHA[:8]}",
            f"herdr {VERSION}-gx.windows-installer.{'f' * 40}",
            f"herdr 0.9.1-gx.windows-installer.{SHA}",
        ):
            with self.subTest(version=version), mock.patch.object(package, "output", return_value=version):
                with self.assertRaisesRegex(ValueError, "identity mismatch"):
                    package.verify_binary(self.binary, "windows", VERSION, SHA, TOOLS)

    def test_wrong_pe_architecture_and_dynamic_crt_fail_before_execution(self) -> None:
        self.binary.write_bytes(pe(0xAA64))
        with mock.patch.object(package, "output") as output:
            with self.assertRaisesRegex(ValueError, "x86_64 PE"):
                package.verify_binary(self.binary, "windows", VERSION, SHA, TOOLS)
            output.assert_not_called()
        self.binary.write_bytes(pe())
        with mock.patch.object(package.conpty, "validate_static_msvc_runtime", side_effect=ValueError("dynamic CRT")):
            with self.assertRaisesRegex(ValueError, "dynamic CRT"):
                package.verify_binary(self.binary, "windows", VERSION, SHA, TOOLS)

    def test_linux_binary_static_identity_passes(self) -> None:
        self.binary.write_bytes(elf())
        version = f"herdr {VERSION}-gx.deb.{SHA}"
        with mock.patch.object(package, "output", side_effect=["There is no dynamic section", "", version]):
            self.assertEqual(package.verify_binary(self.binary, "linux", VERSION, SHA, TOOLS)["version_output"], version)

    def test_elf_rejects_wrong_architecture_interpreter_and_truncation(self) -> None:
        for data, message in ((elf(183), "x86_64"), (elf(program_type=3), "dynamic interpreter"), (elf()[:64], "program header"), (b"bad", "x86_64")):
            with self.subTest(message=message):
                self.binary.write_bytes(data)
                with self.assertRaisesRegex(ValueError, message):
                    package.validate_elf(self.binary, TOOLS)

    def test_elf_rejects_dynamic_dependencies_and_unresolved_cpp(self) -> None:
        self.binary.write_bytes(elf())
        for results, message in ((["0x1 (NEEDED) Shared library: [libc.so.6]"], "dynamic library"), (["", "U __cxa_throw"], "C\\+\\+"), (["", "U _ZSt4cout"], "C\\+\\+")):
            with self.subTest(results=results), mock.patch.object(package, "output", side_effect=results):
                with self.assertRaisesRegex(ValueError, message):
                    package.validate_elf(self.binary, TOOLS)

    def test_windows_validates_strict_bundle_before_adding_license_and_uses_template_contract(self) -> None:
        events = []

        def stage_bundle(metadata, arch, nupkg, binary, bundle):
            events.append("stage")
            bundle.mkdir()
            shutil.copyfile(binary, bundle / "herdr.exe")
            (bundle / "conpty").mkdir()
            (bundle / "conpty/herdr-conpty.json").write_text("{}", encoding="utf-8")

        def validate_stage(metadata, arch, bundle):
            events.append("validate")
            self.assertFalse((bundle / "LICENSE").exists())
            self.assertEqual(arch, "x86_64")

        def compile_installer(argv, **kwargs):
            events.append("compile")
            stage = Path(next(arg.split("=", 1)[1] for arg in argv if str(arg).startswith("/DStageDir=")))
            self.assertEqual(stage.name, "payload")
            self.assertTrue((stage / "herdr.exe").is_file())
            self.assertTrue((stage / "LICENSE").is_file())
            self.assertFalse((stage / "app").exists())
            self.assertIn(f"/DPackageVersion={VERSION}", argv)
            self.assertIn(f"/DRepoDir={self.root}", argv)
            self.assertIn(f"/O{self.root}", argv)
            self.assertEqual(argv[-1], self.root / "packaging/windows/herdr-gx.iss")
            (self.root / package.artifact_name("windows", VERSION)).write_bytes(b"installer")

        with mock.patch.object(package.conpty, "stage_bundle", side_effect=stage_bundle), mock.patch.object(package.conpty, "validate_stage", side_effect=validate_stage), mock.patch.object(package, "run", side_effect=compile_installer):
            artifact, files = package.package_windows(self.root, self.root, VERSION, self.binary, TOOLS, self.root)
        self.assertEqual(events, ["stage", "validate", "compile"])
        self.assertEqual(artifact.name, "herdr-gx-0.9.2-windows-x86_64-setup.exe")
        self.assertEqual(files["herdr.exe"], package.digest(self.binary))
        self.assertIn("conpty/herdr-conpty.json", files)
        self.assertIn("LICENSE", files)

    def test_inno_template_matches_producer_contract(self) -> None:
        template = (package.ROOT / "packaging/windows/herdr-gx.iss").read_text(encoding="utf-8")
        for define in ("PackageVersion", "StageDir", "RepoDir"):
            self.assertIn(f"#ifndef {define}", template)
        self.assertIn("OutputBaseFilename=herdr-gx-{#PackageVersion}-windows-x86_64-setup", template)
        self.assertIn('Source: "{#StageDir}\\*"; DestDir: "{app}"', template)
        self.assertNotIn('{#StageDir}\\app', template)

    def test_conpty_failure_never_calls_inno(self) -> None:
        with mock.patch.object(package.conpty, "stage_bundle", side_effect=ValueError("ConPTY package hash mismatch")), mock.patch.object(package, "run") as run:
            with self.assertRaisesRegex(ValueError, "hash mismatch"):
                package.package_windows(self.root, self.root, VERSION, self.binary, TOOLS, self.root)
            run.assert_not_called()

    def test_deb_stages_minimal_payload_and_root_owned_archive(self) -> None:
        with mock.patch.object(package, "run") as run, mock.patch.object(package, "verify_deb") as verify:
            artifact, files = package.package_deb(self.root, self.root, VERSION, self.binary, TOOLS, self.root)
        self.assertEqual(artifact.name, "herdr-gx_0.9.2_amd64.deb")
        self.assertEqual(set(files), {"usr/bin/herdr", "usr/share/doc/herdr-gx/copyright"})
        argv = run.call_args.args[0]
        self.assertIn("--root-owner-group", argv)
        self.assertIn("--build", argv)
        self.assertNotIn("--force-overwrite", argv)
        text = (self.root / "payload/DEBIAN/control").read_text(encoding="utf-8")
        self.assertIn("Version: 0.9.2\n", text)
        self.assertIn("Provides: herdr\nConflicts: herdr\n", text)
        self.assertNotIn("Replaces:", text)
        self.assertNotIn("Depends:", text)
        self.assertNotIn("@", text.split("Installed-Size:", 1)[1])
        self.assertEqual({p.name for p in (self.root / "payload/DEBIAN").iterdir()}, {"control"})
        verify.assert_called_once()
        if os.name != "nt":
            self.assertEqual((self.root / "payload/usr/bin/herdr").stat().st_mode & 0o777, 0o755)

    def test_bad_deb_template_fails_before_build(self) -> None:
        (self.root / "packaging/linux/control.in").write_text("Package: herdr-gx\n", encoding="utf-8")
        with mock.patch.object(package, "run") as run:
            with self.assertRaisesRegex(ValueError, "placeholder"):
                package.package_deb(self.root, self.root, VERSION, self.binary, TOOLS, self.root)
            run.assert_not_called()

    def test_deb_verification_checks_fields_permissions_and_hashes(self) -> None:
        extracted = self.root / "extracted"
        fields = ["herdr-gx", VERSION, "amd64", "herdr", "herdr"]
        listing = "drwxr-xr-x root/root 0 2026-09-27 00:00 ./\n-rwxr-xr-x root/root 512 2026-09-27 00:00 ./usr/bin/herdr"

        def extract(argv):
            (extracted / "usr/bin").mkdir(parents=True)
            (extracted / "DEBIAN").mkdir()
            (extracted / "DEBIAN/control").write_text("Package: herdr-gx\n", encoding="utf-8")
            shutil.copyfile(self.binary, extracted / "usr/bin/herdr")

        with mock.patch.object(package, "output", side_effect=[*fields, listing]), mock.patch.object(package, "run", side_effect=extract):
            package.verify_deb(self.binary, VERSION, {"usr/bin/herdr": package.digest(self.binary)}, extracted, TOOLS)
        with mock.patch.object(package, "output", side_effect=[*fields, listing.replace("root/root", "user/user")]):
            with self.assertRaisesRegex(ValueError, "root/root"):
                package.verify_deb(self.binary, VERSION, {}, extracted, TOOLS)
        with mock.patch.object(package, "output", side_effect=[*fields, listing.replace("-rwxr-xr-x", "-rw-r--r--")]):
            with self.assertRaisesRegex(ValueError, "permissions"):
                package.verify_deb(self.binary, VERSION, {}, extracted, TOOLS)
        with mock.patch.object(package, "output", side_effect=[*fields, listing]), mock.patch.object(package, "run"):
            with self.assertRaisesRegex(ValueError, "hashes"):
                package.verify_deb(self.binary, VERSION, {"usr/bin/herdr": "0" * 64}, extracted, TOOLS)

    def test_manifest_schema_and_checksums_cover_package_and_manifest(self) -> None:
        for kind in ("windows", "linux"):
            with self.subTest(platform=kind):
                artifact = self.root / package.artifact_name(kind, VERSION)
                artifact.write_bytes(b"archive")
                binary = {"sha256": package.digest(self.binary), "version_output": f"herdr {VERSION}-gx.{package.MANAGERS[kind]}.{SHA}"}
                files = {"herdr.exe" if kind == "windows" else "usr/bin/herdr": binary["sha256"]}
                metadata = package.write_manifest(artifact, kind, VERSION, SHA, True, binary, files)
                manifest = json.loads(metadata.read_text(encoding="utf-8"))
                self.assertEqual(set(manifest), {"schema_version", "version", "source_commit", "source_dirty", "platform", "architecture", "target", "package_manager", "binary", "artifact", "files"})
                self.assertEqual(manifest["schema_version"], 1)
                self.assertEqual(manifest["source_commit"], SHA)
                self.assertTrue(manifest["source_dirty"])
                self.assertEqual(manifest["architecture"], "x86_64")
                self.assertEqual(manifest["binary"], binary)
                self.assertEqual(manifest["files"], files)
                self.assertEqual(manifest["artifact"], {"name": artifact.name, "size": 7, "sha256": package.digest(artifact)})
                self.assertEqual(metadata.read_bytes(), (json.dumps(manifest, indent=2, sort_keys=True) + "\n").encode("utf-8"))
                checksums = artifact.with_name(artifact.name + ".sha256").read_bytes()
                self.assertEqual(checksums, (
                    f"{package.digest(artifact)}  {artifact.name}\n"
                    f"{package.digest(metadata)}  {metadata.name}\n"
                ).encode("ascii"))
                self.assertNotIn(b"\r", metadata.read_bytes())
                self.assertNotIn(b"\r", checksums)

    def test_manifest_rejects_swapped_binary_or_missing_artifact(self) -> None:
        artifact = self.root / "missing.exe"
        binary = {"sha256": package.digest(self.binary), "version_output": "herdr"}
        with self.assertRaisesRegex(ValueError, "binary hash"):
            package.write_manifest(artifact, "windows", VERSION, SHA, False, binary, {"herdr.exe": "0" * 64})
        with self.assertRaisesRegex(ValueError, "nonempty artifact"):
            package.write_manifest(artifact, "windows", VERSION, SHA, False, binary, {"herdr.exe": binary["sha256"]})

    def test_check_is_read_only_and_does_not_build_or_stage(self) -> None:
        dest = self.root / "not-created"
        with mock.patch.object(package, "source_info", return_value=(SHA, True)), mock.patch.object(package, "preflight", return_value=TOOLS), mock.patch.object(package, "build_binary") as build, mock.patch.object(package.tempfile, "TemporaryDirectory") as staging, contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(package.main(["--platform", "windows", "--output-dir", str(dest), "--allow-dirty", "--check"]), 0)
            build.assert_not_called()
            staging.assert_not_called()
        self.assertFalse(dest.exists())

    def test_cli_dirty_default_refuses_before_any_build(self) -> None:
        with mock.patch.object(package, "source_info", return_value=(SHA, True)), mock.patch.object(package, "preflight") as preflight, contextlib.redirect_stderr(io.StringIO()) as stderr:
            self.assertEqual(package.main(["--platform", "windows", "--check"]), 1)
            preflight.assert_not_called()
        self.assertIn("--allow-dirty", stderr.getvalue())

    def test_cli_builds_and_exports_verified_sidecars(self) -> None:
        dest = self.root / "packages"
        binary = {"sha256": package.digest(self.binary), "version_output": f"herdr {VERSION}-gx.windows-installer.{SHA}"}

        def packager(stage, output_dir, version, binary_path, tools):
            artifact = output_dir / package.artifact_name("windows", version)
            artifact.write_bytes(b"installer")
            return artifact, {"herdr.exe": binary["sha256"]}

        with mock.patch.object(package, "source_info", return_value=(SHA, True)), mock.patch.object(package, "preflight", return_value=TOOLS), mock.patch.object(package, "build_binary", return_value=self.binary) as build, mock.patch.object(package, "verify_binary", return_value=binary), mock.patch.object(package, "package_windows", side_effect=packager), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(package.main(["--platform", "windows", "--output-dir", str(dest), "--allow-dirty"]), 0)
            build.assert_called_once_with("windows", SHA, TOOLS)
        self.assertEqual(len(list(dest.iterdir())), 3)
        metadata = next(dest.glob("*.manifest.json"))
        self.assertTrue(json.loads(metadata.read_text(encoding="utf-8"))["source_dirty"])

    def test_cli_does_not_reuse_existing_installer(self) -> None:
        artifact = self.root / package.artifact_name("windows", VERSION)
        artifact.write_bytes(b"old installer")
        with mock.patch.object(package, "source_info", return_value=(SHA, False)), mock.patch.object(package, "preflight", return_value=TOOLS), mock.patch.object(package, "build_binary") as build, contextlib.redirect_stderr(io.StringIO()) as stderr:
            self.assertEqual(package.main(["--platform", "windows", "--output-dir", str(self.root)]), 1)
            build.assert_not_called()
        self.assertIn("refusing to overwrite", stderr.getvalue())
        self.assertEqual(artifact.read_bytes(), b"old installer")

    def test_source_change_during_build_prevents_export(self) -> None:
        dest = self.root / "packages"
        binary = {"sha256": package.digest(self.binary), "version_output": "herdr"}
        with mock.patch.object(package, "source_info", side_effect=[(SHA, False), (SHA, True)]), mock.patch.object(package, "preflight", return_value=TOOLS), mock.patch.object(package, "build_binary", return_value=self.binary), mock.patch.object(package, "verify_binary", return_value=binary), mock.patch.object(package, "package_windows", return_value=(self.binary, {})), contextlib.redirect_stderr(io.StringIO()) as stderr:
            self.assertEqual(package.main(["--platform", "windows", "--output-dir", str(dest)]), 1)
        self.assertIn("changed during packaging", stderr.getvalue())
        self.assertFalse(dest.exists())

    def native_package_kind(self) -> str:
        # preflight 只接受原生主机：GX 只出 Windows EXE 与 Linux deb（release-channels.md）。
        if os.name == "nt":
            return "windows"
        if sys.platform == "linux":
            return "linux"
        self.skipTest("GX packaging runs only on native Windows or Linux hosts")

    def test_preflight_checks_installed_toolchain_without_installing(self) -> None:
        kind = self.native_package_kind()
        (self.root / "packaging/windows").mkdir()
        (self.root / "packaging/windows/herdr-gx.iss").write_text("[Setup]\n", encoding="utf-8")
        (self.root / "packaging/windows/conpty.json").write_text("{}", encoding="utf-8")
        results = ["rustc 1.96.1 (hash date)", "cargo 1.96.1 (hash date)", package.TARGETS[kind], "0.16.0"]
        if kind == "linux":
            results.append("Debian package archive backend version 1.22.6")
        with mock.patch.object(package.platform, "machine", return_value="x86_64"), mock.patch.object(package, "tool", side_effect=lambda name, root: Path(name)), mock.patch.object(package, "output", side_effect=results) as output, mock.patch.object(package.conpty, "load_metadata", return_value={"notices": []}), mock.patch.object(package.conpty, "acquire_package") as acquire, contextlib.redirect_stdout(io.StringIO()):
            tools = package.preflight(kind, self.root)
        self.assertIn("rustup", tools)
        acquire.assert_not_called()
        calls = output.call_args_list
        for call in calls[:3]:
            self.assertEqual(call.kwargs["env"]["RUSTUP_AUTO_INSTALL"], "0")
            self.assertNotIn("install", call.args[0])
        self.assertIn("--installed", calls[2].args[0])

    def test_preflight_missing_target_fails_without_installing(self) -> None:
        kind = self.native_package_kind()
        results = ["rustc 1.96.1 (hash date)", "cargo 1.96.1 (hash date)", "unrelated-target"]
        with mock.patch.object(package.platform, "machine", return_value="x86_64"), mock.patch.object(package, "tool", side_effect=lambda name, root: Path(name)), mock.patch.object(package, "output", side_effect=results), contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaisesRegex(ValueError, "not installed"):
                package.preflight(kind, self.root)

    def test_subprocess_failures_are_reported(self) -> None:
        error = subprocess.CalledProcessError(1, ["rustup"], stderr="toolchain is not installed")
        with mock.patch.object(package, "source_info", return_value=(SHA, False)), mock.patch.object(package, "preflight", side_effect=error), contextlib.redirect_stderr(io.StringIO()) as stderr:
            self.assertEqual(package.main(["--platform", "windows", "--check"]), 1)
        self.assertIn("toolchain is not installed", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
