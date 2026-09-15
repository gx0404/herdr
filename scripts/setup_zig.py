#!/usr/bin/env python3
"""钉版 Zig 工具链管理（vendored libghostty-vt 构建需要 Zig 0.16.0）。

安装位置在**仓库内部**：`<repo>/.local/toolchains/zig/zig-0.16.0/`（gitignored），
不写任何用户全局状态；build.rs 会自动探测该目录（优先级：$ZIG > 项目内钉版 >
PATH）。多 worktree 想共享一份时设 HERDR_ZIG_HOME 指向公共目录。

模式：
  默认/--check : 只读诊断，报告钉版安装状态与 PATH 中可见的 zig。
  --install    : 下载 sha256 钉死的官方 tarball，校验后原子落位。
  --force      : 允许覆盖已存在的钉版目录。

HERDR_ZIG_HOME 可覆盖安装根（测试与 worktree 共享用）。
"""

from __future__ import annotations

import argparse
import hashlib
import os
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
from pathlib import Path

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
    system = {"linux": "linux", "darwin": "macos", "macos": "macos"}.get(platform.system().lower(), "")
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


def zig_binary() -> Path:
    return install_dir() / "zig"


def _run_zig_version(binary: Path | str) -> str | None:
    try:
        result = subprocess.run(
            [str(binary), "version"], capture_output=True, text=True, check=False, timeout=30
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    return result.stdout.strip() if result.returncode == 0 else None


def check() -> int:
    key = platform_key()
    pinned = zig_binary()
    if pinned.is_file():
        version = _run_zig_version(pinned)
        status = f"INSTALLED {pinned} (zig version: {version})"
        if version != ZIG_VERSION:
            status += f"  [警告: 期望 {ZIG_VERSION}]"
        else:
            status += "；build.rs 将自动使用（$ZIG 仍可覆盖）"
    else:
        status = f"MISSING {pinned}；运行 just setup-zig --install（或 scripts/setup_env.sh）"
    print(f"[setup-zig] {status}")
    on_path = shutil.which("zig")
    if on_path:
        print(f"[setup-zig] PATH zig: {on_path} (zig version: {_run_zig_version(on_path)})")
    else:
        print("[setup-zig] PATH zig: 无；未安装钉版时 cargo build 会失败")
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


def _extract(archive: Path, dest: Path) -> None:
    tar = shutil.which("tar")
    if tar:
        result = subprocess.run(
            [tar, "-xJf", str(archive), "-C", str(dest)],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            raise SetupZigError(f"tar 解压失败：{result.stderr.strip()}")
        return
    # 后备：部分 python 构建缺少 lzma 模块或 filter 参数（3.10 无 filter=）。
    with tarfile.open(archive, "r:xz") as tf:
        try:
            tf.extractall(dest, filter="data")
        except TypeError:
            tf.extractall(dest)  # 内容已由 sha256 钉版校验


def install(force: bool) -> int:
    key = platform_key()
    pin = PINS[key]
    target_dir = install_dir()
    if target_dir.exists() and not force:
        # 幂等：已装且 zig version 匹配则跳过；只有损坏的安装才要求 --force。
        if zig_binary().is_file():
            version = _run_zig_version(zig_binary())
            if version == ZIG_VERSION:
                print(f"[setup-zig] 已安装且有效，跳过：{zig_binary()} (zig version: {version})；覆盖请加 --force")
                return 0
            raise SetupZigError(
                f"已存在 {target_dir} 但无效（zig version: {version!r}）；覆盖请加 --force"
            )
        raise SetupZigError(f"已存在 {target_dir} 但缺少 zig 可执行文件；覆盖请加 --force")
    target_dir.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=str(target_dir.parent)) as tmp_name:
        tmp_root = Path(tmp_name)
        archive = tmp_root / pin["tarball"]
        _download(pin["tarball"], archive)
        digest = _sha256(archive)
        if digest != pin["sha256"]:
            raise SetupZigError(f"sha256 校验失败：{digest} != {pin['sha256']}")
        # 解压优先走系统 tar（部分 python 构建缺 lzma）；sha256 已校验完整性。
        _extract(archive, tmp_root)
        extracted = tmp_root / pin["tarball"].removesuffix(".tar.xz")
        zig = extracted / "zig"
        if not zig.is_file():
            raise SetupZigError(f"tarball 结构异常：缺少 {zig}")
        staged = tmp_root / INSTALL_DIR_NAME
        extracted.rename(staged)
        if target_dir.exists():
            shutil.rmtree(target_dir)
        staged.rename(target_dir)
    version = _run_zig_version(zig_binary())
    if version != ZIG_VERSION:
        raise SetupZigError(f"安装后 zig version 输出异常：{version!r}")
    print(f"[setup-zig] 已安装 {zig_binary()} (zig version: {version})")
    print("[setup-zig] 位于仓库内的 gitignored 目录；cargo build / just 直接可用（无需 source）")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--install", action="store_true", help="下载并安装钉版工具链（默认只检查）")
    parser.add_argument("--check", action="store_true", help="只读诊断（默认行为，显式给出亦可）")
    parser.add_argument("--force", action="store_true", help="覆盖已存在的钉版目录")
    args = parser.parse_args(argv)
    try:
        if args.install:
            return install(args.force)
        return check()
    except SetupZigError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
