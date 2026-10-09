#!/usr/bin/env python3
"""钉版 Zig 工具链管理（vendored libghostty-vt 构建需要 Zig 0.16.0）。

安装位置在**仓库内部**：`<repo>/.local/toolchains/zig/zig-0.16.0/`（gitignored），
不写任何用户全局状态；build.rs 会自动探测该目录（优先级：$ZIG > 项目内钉版 >
PATH）。HERDR_ZIG_HOME 只改变安装根；自定义安装需要显式设置 ZIG 才能被构建使用。

模式：
  默认/--check : 只读诊断实际生效的 Zig；缺失、失败或版本不匹配均非零退出。
  --install    : 下载 sha256 钉死的归档，暂存验证后落位；替换失败时恢复旧安装。
  --force      : 与 --install 同用，允许替换已存在的钉版目录。

临时下载、解压与备份均放在安装根内；版本探测缓存固定在仓库内。
"""

from __future__ import annotations

import argparse
import hashlib
import os
import platform
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
import zipfile
from pathlib import Path, PurePosixPath, PureWindowsPath


# Windows cp1252 控制台打印中文状态会 UnicodeEncodeError；check/install 输出
# 含中文提示，模块级重配保证 CLI 与被测调用都稳定。
for _stream in (sys.stdout, sys.stderr):
    if hasattr(_stream, "reconfigure"):
        _stream.reconfigure(encoding="utf-8", errors="replace")

REPO_ROOT = Path(__file__).resolve().parents[1]
ZIG_VERSION = "0.16.0"
INSTALL_DIR_NAME = f"zig-{ZIG_VERSION}"
# 官方 index.json 的 sha256 钉版（2026-09 获取）。
PINS: dict[str, dict[str, str]] = {
    "x86_64-linux": {
        "tarball": f"zig-x86_64-linux-{ZIG_VERSION}.tar.xz",
        "sha256": "70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00",
    },
    "aarch64-linux": {
        "tarball": f"zig-aarch64-linux-{ZIG_VERSION}.tar.xz",
        "sha256": "ea4b09bfb22ec6f6c6ceac57ab63efb6b46e17ab08d21f69f3a48b38e1534f17",
    },
    "x86_64-macos": {
        "tarball": f"zig-x86_64-macos-{ZIG_VERSION}.tar.xz",
        "sha256": "0387557ed1877bc6a2e1802c8391953baddba76081876301c522f52977b52ba7",
    },
    "aarch64-macos": {
        "tarball": f"zig-aarch64-macos-{ZIG_VERSION}.tar.xz",
        "sha256": "b23d70deaa879b5c2d486ed3316f7eaa53e84acf6fc9cc747de152450d401489",
    },
    "x86_64-windows": {
        # 官方 Windows 包是 zip；sha256 取自 ziglang.org download 页钉版。
        "tarball": f"zig-x86_64-windows-{ZIG_VERSION}.zip",
        "sha256": "68659eb5f1e4eb1437a722f1dd889c5a322c9954607f5edcf337bc3684a75a7e",
    },
}
DOWNLOAD_BASE = f"https://ziglang.org/download/{ZIG_VERSION}/"
# 下载源顺序：国内教育网/阿里镜像站均未收录 zig（2026-09 核实，TUNA /zig/ 404），
# 实测可达的快源是 machengine（Cloudflare CDN）；官方源在部分网络被限速到 <20KB/s，
# 故默认镜像优先、官方后备。所有来源都经下方 sha256 钉版校验，来源可信度由哈希保证；
# 未来出现更近的镜像时设 HERDR_ZIG_MIRROR=<base> 前置即可。
MIRROR_BASES = ["https://pkg.machengine.org/zig/"]
# 初始探测窗（8s 内需 ≥512KB，即 ≥64KB/s）；此后要求持续平均速率 ≥100KB/s，
# 低于即切换下一个源。不设绝对时限：53MB 在 100KB/s 下 ~9 分钟也允许完成。
SPEED_FLOOR_BYTES = 512 * 1024
SPEED_PROBE_SECONDS = 8
MIN_SUSTAINED_RATE_BYTES_PER_SEC = 100 * 1024
PROGRESS_INTERVAL_SECONDS = 3


class SetupZigError(RuntimeError):
    """安装前置不满足或校验失败。"""


def download_bases() -> list[str]:
    """下载源顺序：HERDR_ZIG_MIRROR 自定义源 → 内置镜像 → 官方。"""
    bases: list[str] = []
    custom = os.environ.get("HERDR_ZIG_MIRROR", "").rstrip("/")
    if custom:
        if not custom.endswith("/"):
            custom += "/"
        bases.append(custom)
    bases.extend(MIRROR_BASES)
    bases.append(DOWNLOAD_BASE)
    return bases


def platform_key() -> str:
    machine = {"x86_64": "x86_64", "amd64": "x86_64", "arm64": "aarch64", "aarch64": "aarch64"}.get(
        platform.machine().lower(), ""
    )
    system = {"linux": "linux", "darwin": "macos", "macos": "macos", "windows": "windows"}.get(
        platform.system().lower(), ""
    )
    key = f"{machine}-{system}"
    if key not in PINS:
        raise SetupZigError(
            f"本平台 {key} 没有钉版条目；请手动安装 Zig {ZIG_VERSION} 并设置 ZIG 环境变量"
        )
    return key


def install_root() -> Path:
    # 默认装进仓库内（自包含、可移植）；HERDR_ZIG_HOME 供测试/多 worktree 共享覆盖。
    override = os.environ.get("HERDR_ZIG_HOME")
    if override:
        return Path(override)
    return REPO_ROOT / ".local" / "toolchains" / "zig"


def install_dir() -> Path:
    return install_root() / INSTALL_DIR_NAME


def build_auto_root() -> Path:
    return REPO_ROOT / ".local" / "toolchains" / "zig"


def zig_binary(directory: Path | None = None) -> Path:
    directory = install_dir() if directory is None else directory
    for name in ("zig.exe", "zig"):
        candidate = directory / name
        if candidate.is_file():
            return candidate
    return directory / ("zig.exe" if os.name == "nt" else "zig")


def resolve_effective_zig() -> tuple[str, str]:
    """Return (command, source) using build.rs precedence, without running or writing."""
    if "ZIG" in os.environ:
        explicit = os.environ["ZIG"]
        if not explicit.strip():
            raise SetupZigError("ZIG 显式设置为空；不会回退到项目内安装或 PATH")
        return explicit, "ZIG"
    pinned = zig_binary(build_auto_root() / INSTALL_DIR_NAME)
    if pinned.is_file():
        return str(pinned), "project"
    on_path = shutil.which("zig")
    if on_path:
        return on_path, "PATH"
    raise SetupZigError("MISSING effective Zig；运行 just setup-zig --install 或显式设置 ZIG")


def _run_zig_version(binary: Path | str) -> str | None:
    env = dict(os.environ)
    for name, suffix in (("ZIG_GLOBAL_CACHE_DIR", "global"), ("ZIG_LOCAL_CACHE_DIR", "local")):
        env[name] = str(REPO_ROOT / ".local" / "zig-cache" / suffix)
    try:
        result = subprocess.run(
            [str(binary), "version"], capture_output=True, text=True, check=False,
            timeout=30, env=env,
        )
    except (OSError, subprocess.TimeoutExpired, UnicodeError):
        return None
    return result.stdout.strip() if result.returncode == 0 else None


def check() -> int:
    print(f"[setup-zig] install_root: {install_root()}")
    print(f"[setup-zig] build auto root: {build_auto_root()}（HERDR_ZIG_HOME 不改变 build.rs 解析）")
    pinned = zig_binary()
    print(f"[setup-zig] local install: {'PRESENT' if pinned.is_file() else 'MISSING'} {pinned}")
    try:
        binary, source = resolve_effective_zig()
        version = _run_zig_version(binary)
        print(f"[setup-zig] effective Zig ({source}): {binary} (zig version: {version!r})")
        if version != ZIG_VERSION:
            raise SetupZigError(f"有效 Zig 必须为 {ZIG_VERSION}；当前版本不匹配或 version 命令失败")
    except SetupZigError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    return 0


def _download_one(url: str, destination: Path) -> None:
    import time

    print(f"[setup-zig] 下载 {url}")
    start = time.monotonic()
    last_report = start
    downloaded = 0
    total: int | None = None
    try:
        with urllib.request.urlopen(url, timeout=60) as response, destination.open("wb") as handle:
            length = response.headers.get("Content-Length")
            if length and length.isdigit():
                total = int(length)
            while True:
                chunk = response.read(1 << 14)
                if not chunk:
                    break
                handle.write(chunk)
                downloaded += len(chunk)
                now = time.monotonic()
                if now - last_report >= PROGRESS_INTERVAL_SECONDS:
                    rate = downloaded / 1024 / max(now - start, 0.001)
                    if total is not None:
                        print(
                            f"[setup-zig]   {downloaded / 1048576:.1f}/{total / 1048576:.1f}MB"
                            f"（{rate:.0f}KB/s）"
                        )
                    else:
                        print(f"[setup-zig]   {downloaded / 1048576:.1f}MB（{rate:.0f}KB/s）")
                    last_report = now
                elapsed = now - start
                if elapsed > SPEED_PROBE_SECONDS and (
                    downloaded < SPEED_FLOOR_BYTES
                    or downloaded < MIN_SUSTAINED_RATE_BYTES_PER_SEC * elapsed
                ):
                    rate = downloaded / 1024 / max(elapsed, 0.001)
                    raise SetupZigError(f"源速度过低（{rate:.0f}KB/s），切换下一个源")
    except urllib.error.HTTPError as exc:
        raise SetupZigError(f"HTTP {exc.code}") from exc
    except (urllib.error.URLError, OSError) as exc:
        raise SetupZigError(f"网络失败：{exc}") from exc


def _download(tarball: str, destination: Path) -> None:
    last_error: SetupZigError | None = None
    for base in download_bases():
        destination.unlink(missing_ok=True)
        try:
            _download_one(base + tarball, destination)
            return
        except SetupZigError as exc:
            print(f"[setup-zig] {exc}")
            last_error = exc
    raise SetupZigError(f"所有下载源失败：{last_error}")


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 16), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _archive_path(dest: Path, name: str) -> Path:
    parts = PurePosixPath(name)
    if (
        not name or parts.is_absolute() or PureWindowsPath(name).drive
        or ".." in parts.parts or "\\" in name or ":" in name or "\0" in name
        or any(part.endswith((" ", ".")) or PureWindowsPath(part).is_reserved() for part in parts.parts)
    ):
        raise SetupZigError(f"归档路径不安全：{name!r}")
    target = dest.joinpath(*parts.parts)
    if not target.resolve().is_relative_to(dest.resolve()):
        raise SetupZigError(f"归档路径越界：{name!r}")
    current = dest
    for part in parts.parts:
        current /= part
        if current.is_symlink():
            raise SetupZigError(f"归档路径包含符号链接：{name!r}")
    return target


def _extract(archive: Path, dest: Path) -> None:
    try:
        if archive.suffix == ".zip":
            with zipfile.ZipFile(archive) as zf:
                members = zf.infolist()
                for member in members:
                    _archive_path(dest, member.filename)
                    kind = stat.S_IFMT(member.external_attr >> 16)
                    if kind not in (0, stat.S_IFREG, stat.S_IFDIR):
                        raise SetupZigError(f"归档包含链接或特殊文件：{member.filename}")
                for member in members:
                    target = _archive_path(dest, member.filename)
                    if member.is_dir():
                        target.mkdir(parents=True, exist_ok=True)
                    else:
                        target.parent.mkdir(parents=True, exist_ok=True)
                        with zf.open(member) as source, target.open("xb") as output:
                            shutil.copyfileobj(source, output)
            return
        with tarfile.open(archive, "r:xz") as tf:
            members = tf.getmembers()
            for member in members:
                _archive_path(dest, member.name)
                if not (member.isdir() or member.isreg()):
                    raise SetupZigError(f"归档包含链接或特殊文件：{member.name}")
            for member in members:
                target = _archive_path(dest, member.name)
                if member.isdir():
                    target.mkdir(parents=True, exist_ok=True)
                else:
                    target.parent.mkdir(parents=True, exist_ok=True)
                    source = tf.extractfile(member)
                    if source is None:
                        raise SetupZigError(f"归档文件无法读取：{member.name}")
                    with source, target.open("xb") as output:
                        shutil.copyfileobj(source, output)
                    target.chmod(member.mode & 0o777)
    except (OSError, tarfile.TarError, zipfile.BadZipFile, EOFError) as exc:
        raise SetupZigError(f"归档解压失败：{exc}") from exc


def _replace_install(staged: Path, target: Path, backup: Path) -> None:
    if target.is_symlink() or (hasattr(target, "is_junction") and target.is_junction()):
        raise SetupZigError(f"拒绝替换链接目录：{target}")
    had_old = target.exists()
    if had_old:
        target.rename(backup)
    try:
        staged.rename(target)
    except OSError as exc:
        if had_old:
            try:
                backup.rename(target)
            except OSError as restore_error:
                raise SetupZigError(
                    f"安装替换失败：{exc}；恢复失败：{restore_error}；旧安装保留在 {backup}"
                ) from exc
        raise SetupZigError(f"安装替换失败，旧安装未改变：{exc}") from exc
    if had_old:
        try:
            if backup.is_dir():
                shutil.rmtree(backup)
            else:
                backup.unlink()
        except OSError as exc:
            raise SetupZigError(f"新安装已落位，但旧备份清理失败：{backup}：{exc}") from exc


def _install_locked(target_dir: Path, pin: dict[str, str], force: bool) -> int:
    if target_dir.is_symlink() or (hasattr(target_dir, "is_junction") and target_dir.is_junction()):
        raise SetupZigError(f"拒绝替换链接目录：{target_dir}")
    if target_dir.exists() and not force:
        version = _run_zig_version(zig_binary()) if zig_binary().is_file() else None
        if version == ZIG_VERSION:
            print(f"[setup-zig] 已安装且有效，跳过：{zig_binary()}；覆盖请加 --force")
            return 0
        raise SetupZigError(f"已存在 {target_dir} 但无效（zig version: {version!r}）；覆盖请加 --force")
    with tempfile.TemporaryDirectory(prefix=".zig-stage-", dir=str(target_dir.parent)) as tmp_name:
        tmp_root = Path(tmp_name)
        archive = tmp_root / pin["tarball"]
        _download(pin["tarball"], archive)
        digest = _sha256(archive)
        if digest != pin["sha256"]:
            raise SetupZigError(f"sha256 校验失败：{digest} != {pin['sha256']}")
        payload = tmp_root / "payload"
        payload.mkdir()
        _extract(archive, payload)
        tarball = pin["tarball"]
        stem = tarball.removesuffix(".tar.xz") if tarball.endswith(".tar.xz") else tarball.removesuffix(".zip")
        staged = payload / stem
        zig = staged / ("zig.exe" if os.name == "nt" else "zig")
        if not zig.is_file():
            raise SetupZigError(f"tarball 结构异常：缺少 {zig}")
        version = _run_zig_version(zig)
        if version != ZIG_VERSION:
            raise SetupZigError(f"暂存 Zig version 输出异常：{version!r}；旧安装未改变")
        backup = target_dir.parent / f".zig-backup-{tmp_root.name}"
        _replace_install(staged, target_dir, backup)
    print(f"[setup-zig] 已安装 {zig_binary()} (zig version: {version})")
    if install_root().resolve() == build_auto_root().resolve():
        print("[setup-zig] cargo build / just 将自动使用项目内安装（ZIG 仍可覆盖）")
    else:
        print(f"[setup-zig] 自定义安装根不被 build.rs 自动探测；请显式设置 ZIG={zig_binary()}")
    return 0


def install(force: bool) -> int:
    pin = PINS[platform_key()]
    target_dir = install_dir()
    target_dir.parent.mkdir(parents=True, exist_ok=True)
    lock = target_dir.parent / ".install-lock"
    try:
        lock.mkdir()
    except FileExistsError as exc:
        raise SetupZigError(f"安装锁已存在：{lock}；确认无安装进程后再手动移除") from exc
    try:
        return _install_locked(target_dir, pin, force)
    finally:
        lock.rmdir()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--install", action="store_true", help="下载并安装钉版工具链（默认只检查）")
    mode.add_argument("--check", action="store_true", help="只读诊断（默认行为，显式给出亦可）")
    parser.add_argument("--force", action="store_true", help="覆盖已存在的钉版目录")
    args = parser.parse_args(argv)
    if args.force and not args.install:
        parser.error("--force requires --install")
    try:
        if args.install:
            return install(args.force)
        return check()
    except (SetupZigError, OSError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
