#!/usr/bin/env python3
"""Build independent Herdr GX installers without installing or publishing them."""
from __future__ import annotations

import argparse
import json
import os
import platform
import re
import shutil
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib  # type: ignore[no-redef]

if __package__:
    from . import package_windows_conpty as conpty
else:
    import package_windows_conpty as conpty

ROOT = Path(__file__).resolve().parents[1]
TARGETS = {
    "windows": "x86_64-pc-windows-msvc",
    "linux": "x86_64-unknown-linux-musl",
}
MANAGERS = {"windows": "windows-installer", "linux": "deb"}
ZIG_VERSION = "0.16.0"
VERSION_PATTERN = r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"


def run(argv, **kwargs):
    return subprocess.run([str(arg) for arg in argv], check=True, **kwargs)


def output(argv, **kwargs) -> str:
    return run(argv, capture_output=True, text=True, encoding="utf-8", **kwargs).stdout.strip()


def digest(path: Path) -> str:
    return conpty.sha256_file(path)


def cargo_version(root: Path = ROOT) -> str:
    package = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))["package"]
    version = package.get("version")
    if package.get("name") != "herdr" or not isinstance(version, str):
        raise ValueError("Cargo.toml must define the herdr package version")
    if not re.fullmatch(VERSION_PATTERN, version) or any(int(n) > 65535 for n in version.split(".")):
        raise ValueError("GX packages require Cargo.toml version X.Y.Z (each component <= 65535)")
    return version


def rust_toolchain(root: Path = ROOT) -> str:
    channel = tomllib.loads((root / "rust-toolchain.toml").read_text(encoding="utf-8"))["toolchain"]["channel"]
    if not isinstance(channel, str) or not re.fullmatch(VERSION_PATTERN, channel):
        raise ValueError("rust-toolchain.toml must pin an exact Rust X.Y.Z version")
    return channel


def source_info(root: Path = ROOT) -> tuple[str, bool]:
    toplevel = output(["git", "--no-optional-locks", "-C", root, "rev-parse", "--show-toplevel"])
    if Path(toplevel).resolve() != root.resolve():
        raise ValueError(
            "package source must be a standalone herdr checkout; "
            "source archives require an external builder with a verified revision and archive checksum"
        )
    sha = output(["git", "--no-optional-locks", "-C", root, "rev-parse", "HEAD"])
    if not re.fullmatch(r"[0-9a-f]{40}", sha):
        raise ValueError("HEAD must resolve to a full 40-character lowercase Git SHA")
    dirty = bool(output([
        "git", "--no-optional-locks", "-C", root, "status", "--porcelain=v1", "--untracked-files=all",
    ]))
    return sha, dirty


def require_clean(dirty: bool, allow_dirty: bool) -> None:
    if dirty and not allow_dirty:
        raise ValueError("source tree is dirty; commit the intended source first, or use --allow-dirty for local testing only")


def tool(name: str, root: Path = ROOT) -> Path:
    override = os.environ.get({"iscc": "ISCC", "zig": "ZIG"}.get(name, ""))
    if override:
        candidate = Path(override)
        if candidate.is_file():
            return candidate.resolve()
        found = shutil.which(override)
        if found:
            return Path(found).resolve()
        raise ValueError(f"{name.upper()} points to a missing executable: {override}")
    if name == "zig":
        for filename in ("zig.exe", "zig") if os.name == "nt" else ("zig",):
            candidate = root / ".local/toolchains/zig" / f"zig-{ZIG_VERSION}" / filename
            if candidate.is_file():
                return candidate
    found = shutil.which(name)
    if found:
        return Path(found).resolve()
    if name == "iscc":
        for env_name in ("ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"):
            base = os.environ.get(env_name)
            if not base:
                continue
            for version in ("6", "7"):
                for prefix in (Path(base), Path(base) / "Programs"):
                    candidate = prefix / f"Inno Setup {version}" / "ISCC.exe"
                    if candidate.is_file():
                        return candidate
    raise ValueError(f"required tool not found: {name}; provision it explicitly before packaging (nothing was installed)")


def preflight(kind: str, root: Path = ROOT) -> dict[str, Path]:
    if platform.machine().lower() not in {"amd64", "x86_64"}:
        raise ValueError("GX packaging requires an x86_64 build host")
    if (kind == "windows" and os.name != "nt") or (kind == "linux" and sys.platform != "linux"):
        raise ValueError(f"{kind} packaging requires a native {kind} host; Linux may run in WSL or a disposable container")
    local_config = root / ".cargo" / "config.local.toml"
    if local_config.exists():
        # 本机加速配置会改链接器、开不稳定选项（RUSTC_BOOTSTRAP），不能带进安装包。
        raise ValueError(
            f"{local_config} holds local-only build settings; run `just local-build-config --disable` before packaging"
        )
    channel = rust_toolchain(root)
    names = ["git", "rustup", "zig"]
    names += ["iscc"] if kind == "windows" else ["dpkg-deb", "readelf", "nm", "musl-gcc", "cc", "ar"]
    tools, failures = {}, []
    for name in names:
        try:
            tools[name] = tool(name, root)
            print(f"FOUND {name}: {tools[name]}")
        except ValueError as error:
            failures.append(str(error))
    if failures:
        raise ValueError("preflight failed:\n" + "\n".join(failures))
    env = os.environ.copy()
    env["RUSTUP_AUTO_INSTALL"] = "0"
    for name in ("rustc", "cargo"):
        version = output([tools["rustup"], "run", channel, name, "--version"], env=env, cwd=root)
        if not version.startswith(f"{name} {channel} "):
            raise ValueError(f"expected {name} {channel}, got {version!r}")
    targets = output([
        tools["rustup"], "target", "list", "--installed", "--toolchain", channel,
    ], env=env, cwd=root).splitlines()
    if TARGETS[kind] not in targets:
        raise ValueError(f"Rust target {TARGETS[kind]} is not installed for {channel}; preflight never installs targets")
    zig_version = output([tools["zig"], "version"], cwd=root)
    if zig_version != ZIG_VERSION:
        raise ValueError(f"expected Zig {ZIG_VERSION}, got {zig_version!r}")
    required = [root / "LICENSE"]
    if kind == "windows":
        required += [root / "packaging/windows/herdr-gx.iss", root / "packaging/windows/conpty.json"]
        metadata = conpty.load_metadata(required[-1])
        for notice in metadata["notices"]:
            path = root / notice["source"]
            if digest(path) != notice["sha256"]:
                raise ValueError(f"ConPTY notice hash mismatch: {path}")
    else:
        required += [root / "packaging/linux/control.in"]
        output([tools["dpkg-deb"], "--version"])
    for path in required:
        if not path.is_file():
            raise ValueError(f"missing packaging input: {path}")
    return tools


def build_binary(kind: str, sha: str, tools: dict[str, Path], root: Path = ROOT) -> Path:
    target = TARGETS[kind]
    target_dir = root / "target/gx" / kind
    env = os.environ.copy()
    env.update({
        "HERDR_PACKAGE_MANAGER": MANAGERS[kind],
        "HERDR_BUILD_COMMIT": sha,
        "RUSTUP_AUTO_INSTALL": "0",
        "CARGO_TARGET_DIR": str(target_dir),
        "CARGO_ENCODED_RUSTFLAGS": "-C\x1ftarget-feature=+crt-static",
        "CARGO_PROFILE_RELEASE_STRIP": "none",
        "ZIG": str(tools["zig"]),
    })
    for name in ("HERDR_BUILD_CHANNEL", "HERDR_BUILD_ID", "RUSTFLAGS"):
        env.pop(name, None)
    if kind == "linux":
        env["CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER"] = str(tools["musl-gcc"])
        env["CC_x86_64_unknown_linux_musl"] = str(tools["musl-gcc"])
    run([
        tools["rustup"], "run", rust_toolchain(root), "cargo", "build", "--locked", "--release",
        "--bin", "herdr", "--target", target,
    ], env=env, cwd=root)
    return target_dir / target / "release" / ("herdr.exe" if kind == "windows" else "herdr")


def validate_elf(path: Path, tools: dict[str, Path]) -> None:
    with path.open("rb") as binary:
        header = binary.read(64)
        if (len(header) != 64 or header[:7] != b"\x7fELF\x02\x01\x01"
                or struct.unpack_from("<H", header, 18)[0] != 62
                or struct.unpack_from("<H", header, 16)[0] not in {2, 3}):
            raise ValueError(f"not an x86_64 little-endian ELF executable: {path}")
        offset = struct.unpack_from("<Q", header, 32)[0]
        entry_size, count = struct.unpack_from("<HH", header, 54)
        if entry_size < 56 or not count or count == 65535 or offset + entry_size * count > path.stat().st_size:
            raise ValueError(f"invalid ELF program header table: {path}")
        for index in range(count):
            binary.seek(offset + index * entry_size)
            if struct.unpack("<I", binary.read(4))[0] == 3:
                raise ValueError(f"ELF has a dynamic interpreter; a static musl build is required: {path}")
    dynamic = output([tools["readelf"], "--dynamic", "--wide", path], env={**os.environ, "LC_ALL": "C"})
    if re.search(r"\(NEEDED\)|\bGLIBC_[0-9]", dynamic):
        raise ValueError(f"ELF has dynamic library dependencies; a static musl build is required: {path}")
    undefined = output([tools["nm"], "--undefined-only", path], env={**os.environ, "LC_ALL": "C"})
    if re.search(r"__cxa|__gxx|GLIBCXX|CXXABI|_ZSt|_ZNSt", undefined):
        raise ValueError(f"ELF has unresolved C++ runtime symbols: {path}")


def verify_binary(path: Path, kind: str, version: str, sha: str, tools: dict[str, Path]) -> dict:
    if not path.is_file():
        raise ValueError(f"build did not produce the expected binary: {path}")
    if kind == "windows":
        data = path.read_bytes()
        if conpty.pe_machine(data) != 0x8664:
            raise ValueError(f"not an x86_64 PE executable: {path}")
        conpty.validate_static_msvc_runtime(data, path.name)
    else:
        validate_elf(path, tools)
    version_output = output([path, "--version"], timeout=30)
    expected = f"herdr {version}-gx.{MANAGERS[kind]}.{sha}"
    if version_output != expected:
        raise ValueError(f"binary identity mismatch: expected {expected!r}, got {version_output!r}")
    return {"sha256": digest(path), "version_output": version_output}


def payload_hashes(payload: Path) -> dict[str, str]:
    files = {}
    for path in sorted(payload.rglob("*")):
        if path.is_symlink():
            raise ValueError(f"payload must not contain symlinks: {path}")
        if path.is_file():
            files[path.relative_to(payload).as_posix()] = digest(path)
    return files


def artifact_name(kind: str, version: str) -> str:
    if kind == "windows":
        return f"herdr-gx-{version}-windows-x86_64-setup.exe"
    return f"herdr-gx_{version}_amd64.deb"


def package_windows(stage: Path, dest: Path, version: str, binary: Path,
                    tools: dict[str, Path], root: Path = ROOT) -> tuple[Path, dict[str, str]]:
    bundle = stage / "conpty-bundle"
    metadata = root / "packaging/windows/conpty.json"
    conpty.stage_bundle(metadata, "x86_64", stage / "conpty.nupkg", binary, bundle)
    conpty.validate_stage(metadata, "x86_64", bundle)
    payload = stage / "payload"
    shutil.copytree(bundle, payload)
    shutil.copyfile(root / "LICENSE", payload / "LICENSE")
    files = payload_hashes(payload)
    run([
        tools["iscc"], f"/DPackageVersion={version}", f"/DStageDir={payload}",
        f"/DRepoDir={root}", f"/O{dest}", root / "packaging/windows/herdr-gx.iss",
    ], cwd=root)
    if payload_hashes(payload) != files:
        raise ValueError("Windows payload changed while packaging")
    return dest / artifact_name("windows", version), files


def copy_file(source: Path, destination: Path, executable: bool = False) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)
    destination.chmod(0o755 if executable else 0o644)


def package_deb(stage: Path, dest: Path, version: str, binary: Path,
                tools: dict[str, Path], root: Path = ROOT) -> tuple[Path, dict[str, str]]:
    payload = stage / "payload"
    copy_file(binary, payload / "usr/bin/herdr", executable=True)
    copy_file(root / "LICENSE", payload / "usr/share/doc/herdr-gx/copyright")
    files = payload_hashes(payload)
    installed_size = sum((path.stat().st_size + 1023) // 1024 for path in payload.rglob("*") if path.is_file())
    control = payload / "DEBIAN/control"
    control.parent.mkdir()
    template = (root / "packaging/linux/control.in").read_text(encoding="utf-8")
    if template.count("@VERSION@") != 1 or template.count("@INSTALLED_SIZE@") != 1:
        raise ValueError("Debian control template must contain one VERSION and INSTALLED_SIZE placeholder")
    rendered = template.replace("@VERSION@", version).replace("@INSTALLED_SIZE@", str(installed_size))
    if re.search(r"@[A-Z_]+@", rendered):
        raise ValueError("unresolved Debian control template placeholder")
    control.write_bytes(rendered.replace("\r\n", "\n").encode("utf-8"))
    control.chmod(0o644)
    payload.chmod(0o755)
    for directory in payload.rglob("*"):
        if directory.is_dir():
            directory.chmod(0o755)
    artifact = dest / artifact_name("linux", version)
    run([tools["dpkg-deb"], "--root-owner-group", "-Zxz", "--build", payload, artifact])
    verify_deb(artifact, version, files, stage / "deb-verification", tools)
    return artifact, files


def verify_deb(artifact: Path, version: str, files: dict[str, str], stage: Path,
               tools: dict[str, Path]) -> None:
    for field, expected in {
        "Package": "herdr-gx", "Version": version, "Architecture": "amd64",
        "Provides": "herdr", "Conflicts": "herdr",
    }.items():
        actual = output([tools["dpkg-deb"], "--field", artifact, field])
        if actual != expected:
            raise ValueError(f"Debian {field} mismatch: expected {expected!r}, got {actual!r}")
    contents = output([tools["dpkg-deb"], "--contents", artifact], env={**os.environ, "LC_ALL": "C"})
    for line in contents.splitlines():
        fields = line.split()
        if len(fields) < 6 or fields[1] != "root/root":
            raise ValueError("Debian archive entries must belong to root/root")
        expected_mode = "drwxr-xr-x" if fields[-1].endswith("/") else (
            "-rwxr-xr-x" if fields[-1] == "./usr/bin/herdr" else "-rw-r--r--"
        )
        if fields[0] != expected_mode:
            raise ValueError(f"unexpected Debian entry permissions: {line}")
    run([tools["dpkg-deb"], "--raw-extract", artifact, stage])
    if {p.name for p in (stage / "DEBIAN").iterdir()} != {"control"}:
        raise ValueError("Debian package must not contain maintainer scripts or other control files")
    actual_files = payload_hashes(stage)
    actual_files.pop("DEBIAN/control", None)
    if actual_files != files:
        raise ValueError("Debian payload hashes do not match the staged files")


def write_manifest(artifact: Path, kind: str, version: str, sha: str, dirty: bool,
                   binary: dict, files: dict[str, str]) -> Path:
    binary_path = "herdr.exe" if kind == "windows" else "usr/bin/herdr"
    if files.get(binary_path) != binary["sha256"]:
        raise ValueError("packaged binary hash does not match the verified build")
    if not artifact.is_file() or artifact.stat().st_size == 0:
        raise ValueError(f"packager did not produce a nonempty artifact: {artifact}")
    manifest = {
        "schema_version": 1, "version": version, "source_commit": sha, "source_dirty": dirty,
        "platform": kind, "architecture": "x86_64", "target": TARGETS[kind],
        "package_manager": MANAGERS[kind], "binary": binary,
        "artifact": {"name": artifact.name, "size": artifact.stat().st_size, "sha256": digest(artifact)},
        "files": files,
    }
    metadata = artifact.with_name(artifact.name + ".manifest.json")
    metadata.write_bytes((json.dumps(manifest, indent=2, sort_keys=True) + "\n").encode("utf-8"))
    artifact.with_name(artifact.name + ".sha256").write_bytes(
        f"{digest(artifact)}  {artifact.name}\n{digest(metadata)}  {metadata.name}\n".encode("ascii")
    )
    return metadata


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--platform", choices=tuple(TARGETS), required=True)
    parser.add_argument("--output-dir", type=Path, default=ROOT / "target/packages")
    parser.add_argument("--check", action="store_true", help="read-only preflight; never builds, downloads or installs")
    parser.add_argument("--allow-dirty", action="store_true", help="allow explicitly marked local-test packages; never publish these")
    args = parser.parse_args(argv)
    try:
        version = cargo_version()
        sha, dirty = source_info()
        require_clean(dirty, args.allow_dirty)
        tools = preflight(args.platform)
        if args.check:
            print(f"PASS preflight: {args.platform}, {version}, {sha}, source_dirty={dirty}; no build or install performed")
            return 0
        dest = args.output_dir.resolve()
        name = artifact_name(args.platform, version)
        for suffix in ("", ".manifest.json", ".sha256"):
            if (dest / (name + suffix)).exists():
                raise ValueError(f"refusing to overwrite existing package output: {dest / (name + suffix)}")
        binary_path = build_binary(args.platform, sha, tools)
        binary = verify_binary(binary_path, args.platform, version, sha, tools)
        with tempfile.TemporaryDirectory(prefix="herdr-gx-package-") as temporary:
            stage = Path(temporary)
            output_dir = stage / "output"
            output_dir.mkdir()
            packager = package_windows if args.platform == "windows" else package_deb
            artifact, files = packager(stage, output_dir, version, binary_path, tools)
            if source_info() != (sha, dirty):
                raise ValueError("source commit/dirty status changed during packaging; retry from a stable source tree")
            write_manifest(artifact, args.platform, version, sha, dirty, binary, files)
            dest.mkdir(parents=True, exist_ok=True)
            for suffix in ("", ".manifest.json", ".sha256"):
                source = artifact.with_name(artifact.name + suffix)
                with (dest / source.name).open("xb") as destination, source.open("rb") as data:
                    shutil.copyfileobj(data, destination)
        print(f"BUILT {dest / name}; source_dirty={dirty}")
        return 0
    except (ValueError, OSError, KeyError, struct.error, subprocess.SubprocessError) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        if isinstance(error, subprocess.CalledProcessError) and error.stderr:
            print(error.stderr, file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
