#!/usr/bin/env python3
"""Prepare the repository-local build environment; never install into user directories."""
from __future__ import annotations

import sys

sys.dont_write_bytecode = True

import argparse
import contextlib
import hashlib
import os
from pathlib import Path, PurePosixPath, PureWindowsPath
import platform
import re
import shlex
import shutil
import stat
import subprocess
import tempfile
import urllib.request
import uuid
import zipfile

if __package__:
    from . import setup_zig
else:
    import setup_zig
try:
    import tomllib
except ImportError:
    tomllib = None

ROOT = Path(__file__).resolve().parents[1]
WINDOWS = os.name == "nt"
PINS = {
    "just": ("1.58.0", "https://github.com/casey/just/releases/download/1.58.0/just-1.58.0-x86_64-pc-windows-msvc.zip", "759f16fb7aa17c5c8b9594b6d4a8c1a6630dfd042cf2b3ff84841454d3d188dc", "just.exe"),
    "cargo-nextest": ("0.9.144", "https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-0.9.144/cargo-nextest-0.9.144-x86_64-pc-windows-msvc.zip", "edc18a0a36912d8add2a4856714aad8c128303c1de471f14716c2453318e719d", "cargo-nextest.exe"),
    "node": ("24.19.0", "https://nodejs.org/dist/v24.19.0/node-v24.19.0-win-x64.zip", "57f71ab3652e797d84acddc79c81cc9ff1c6ddb2a1974cdb83f00fee9bff4c73", "node-v24.19.0-win-x64/node.exe"),
    "bun": ("1.3.10", "https://github.com/oven-sh/bun/releases/download/bun-v1.3.10/bun-windows-x64.zip", "7a77b3e245e2e26965c93089a4a1332e8a326d3364c89fae1d1fd99cdd3cd73d", "bun-windows-x64/bun.exe"),
}
CACHE_PATHS = {
    "CARGO_HOME": ".local/cargo-home",
    "CARGO_TARGET_DIR": "target",
    "CARGO_BUILD_BUILD_DIR": "target/build",
    "ZIG_GLOBAL_CACHE_DIR": ".local/zig-cache/global",
    "ZIG_LOCAL_CACHE_DIR": ".local/zig-cache/local",
    "PIP_CACHE_DIR": ".local/cache/pip",
    "npm_config_cache": ".local/cache/npm",
    "BUN_INSTALL_CACHE_DIR": ".local/cache/bun",
    "UV_CACHE_DIR": ".local/cache/uv",
    "PYTHONPYCACHEPREFIX": ".local/cache/python",
    "XDG_CACHE_HOME": ".local/cache/xdg",
    "SCCACHE_DIR": ".local/cache/sccache",
    "CCACHE_DIR": ".local/cache/ccache",
    "RUSTUP_DOWNLOAD_DIR": ".local/cache/rustup/downloads",
    "RUSTUP_TMP_DIR": ".local/cache/rustup/tmp",
    "BUN_INSTALL": ".local/tools/bun-install",
    "UV_TOOL_DIR": ".local/tools/uv",
    "UV_TOOL_BIN_DIR": ".local/tools/bin",
    "UV_PYTHON_INSTALL_DIR": ".local/toolchains/python",
    "npm_config_prefix": ".local/tools/npm",
}
HERDR_OVERRIDES = (
    "HERDR_HOME", "HERDR_CONFIG_PATH", "HERDR_SOCKET_PATH", "HERDR_CLIENT_SOCKET_PATH",
    "HERDR_SESSION", "HERDR_SESSION_NAME", "HERDR_WORKSPACE", "HERDR_PANE_ID",
    "HERDR_CONFIG_DIR", "HERDR_STATE_DIR", "HERDR_RUNTIME_DIR", "HERDR_DATA_DIR",
    "HERDR_STARTUP_CWD", "HERDR_ENV", "HERDR_BIN_PATH", "HERDR_WORKSPACE_ID", "HERDR_TAB_ID",
    "HERDR_CODEX_SHIM_DIR", "HERDR_CODEX_LAUNCH_ACTIVE", "CLAUDE_CONFIG_DIR", "PI_CODING_AGENT_DIR",
)
BASH_GUARD = '''if [ -z "${HERDR_SETUP_TMPDIR:-}" ] || [ ! -d "$HERDR_SETUP_TMPDIR" ]; then
    printf '%s\\n' '[setup-env] project temporary directory is unavailable' >&2
    exit 1
fi
export TMPDIR="$HERDR_SETUP_TMPDIR" TMP="$HERDR_SETUP_TMPDIR" TEMP="$HERDR_SETUP_TMPDIR"
export BASH_ENV="$HERDR_SETUP_BASH_ENV"
'''


class SetupError(RuntimeError):
    pass


def local_path(root: Path, value: str, variable: str) -> Path:
    if not value.strip():
        raise SetupError(f"{variable} is empty; unset it or use a repository-local path")
    path = Path(value)
    path = (root / path).resolve() if not path.is_absolute() else path.resolve()
    if not path.is_relative_to(root.resolve()):
        raise SetupError(f"{variable} points outside repository: {path}; unset it or choose a path inside {root}")
    return path


def environment(root: Path, inherited: dict[str, str]) -> dict[str, str]:
    env = dict(inherited)
    for name, relative in CACHE_PATHS.items():
        aliases = [key for key in env if key.upper() == name.upper()]
        values = [local_path(root, env[key], key) for key in aliases]
        if len(set(values)) > 1:
            raise SetupError(f"Conflicting case variants of {name}")
        for key in aliases:
            del env[key]
        if name == "CARGO_BUILD_BUILD_DIR" and not values:
            continue
        env[name] = str(values[0] if values else local_path(root, relative, name))
    if Path(env["CARGO_HOME"]) != (root / ".local/cargo-home").resolve():
        raise SetupError("CARGO_HOME must be <repo>/.local/cargo-home (credentials/config are not migrated)")
    for name in ("HERDR_ZIG_HOME", "HERDR_WINDOWS_CROSS_ROOT", "LIBGHOSTTY_VT_WINDOWS_LIBC",
                 "CARGO_INSTALL_ROOT", "CARGO_BUILD_TARGET_DIR", "BUN_INSTALL_GLOBAL_DIR", "BUN_INSTALL_BIN"):
        if name in env:
            env[name] = str(local_path(root, env[name], name))
    for name in ("TEMP", "TMP", "TMPDIR"):
        env[name] = str(local_path(root, "target/tmp", name))
    env.update(RUSTUP_AUTO_INSTALL="0", RUSTUP_NO_UPDATE_CHECK="1", PYTHONDONTWRITEBYTECODE="1")
    prefixes = [root / ".local/tool-shims"]
    prefixes += [root / ".local/tools" / name / Path(pin[3]).parent for name, pin in PINS.items()]
    prefixes.append(root / ".local/tools/bin")
    prepend_path(env, prefixes)
    shell_environment(root, env, writable=False)
    return env


def shell_environment(root: Path, env: dict[str, str], *, writable: bool, directory: Path | None = None) -> None:
    owned = {"BASH_ENV", "PSMODULEANALYSISCACHEPATH", "HERDR_SETUP_TMPDIR", "HERDR_SETUP_BASH_ENV", "HERDR_SETUP_PYTHON"}
    for key in list(env):
        if key.upper() in owned:
            del env[key]
    env["BASH_ENV"] = "/dev/null"
    env["PSModuleAnalysisCachePath"] = os.devnull
    env["HERDR_SETUP_PYTHON"] = str(Path(sys.executable).resolve())
    if not writable:
        return
    base = directory if directory is not None else root / ".local"
    guard = local_path(root, str(base / "bash-env.sh"), "BASH_ENV")
    cache = local_path(root, str(base / "cache/powershell/ModuleAnalysisCache"), "PSModuleAnalysisCachePath")
    temporary = local_path(root, env["TMPDIR"], "TMPDIR")
    if not temporary.is_dir():
        raise SetupError(f"Project temporary directory is unavailable: {temporary}")
    guard.parent.mkdir(parents=True, exist_ok=True)
    cache.parent.mkdir(parents=True, exist_ok=True)
    if not guard.is_file() or guard.read_text(encoding="utf-8") != BASH_GUARD:
        staging = guard.with_name(guard.name + "." + uuid.uuid4().hex + ".tmp")
        try:
            staging.write_text(BASH_GUARD, encoding="utf-8", newline="\n")
            staging.replace(guard)
        finally:
            staging.unlink(missing_ok=True)
    env["BASH_ENV"] = env["HERDR_SETUP_BASH_ENV"] = guard.as_posix()
    env["HERDR_SETUP_TMPDIR"] = temporary.as_posix()
    env["PSModuleAnalysisCachePath"] = str(cache)
    shims = base / "tool-shims"
    write_python_shims(root, shims)
    prepend_path(env, [shims])


def write_python_shims(root: Path, directory: Path) -> None:
    if not Path(sys.executable).is_file():
        raise SetupError(f"Python interpreter is unavailable: {sys.executable}")
    for name in ("python", "python3"):
        for suffix in ("", ".cmd"):
            local_path(root, str(directory / (name + suffix)), "Python shim")
    directory.mkdir(parents=True, exist_ok=True)
    python = shlex.quote(bash_path(sys.executable))
    for name in ("python", "python3"):
        (directory / name).write_text(f'#!/usr/bin/env bash\nexec {python} -B "$@"\n', encoding="utf-8", newline="\n")
        (directory / name).chmod(0o755)
        (directory / (name + ".cmd")).write_text(
            '@if not defined HERDR_SETUP_PYTHON exit /b 1\r\n'
            '@if not exist "%HERDR_SETUP_PYTHON%" exit /b 1\r\n'
            '@"%HERDR_SETUP_PYTHON%" -B %*\r\n', encoding="ascii", newline="")


def prepend_path(env: dict[str, str], paths: list[Path]) -> None:
    entries = [str(p) for p in paths] + env.get("PATH", "").split(os.pathsep)
    seen = set()
    unique = []
    for item in entries:
        key = os.path.normcase(os.path.normpath(item))
        if item and key not in seen:
            unique.append(item)
            seen.add(key)
    env["PATH"] = os.pathsep.join(unique)


def probe(argv: list[str], env: dict[str, str], root: Path) -> str:
    child = dict(env)
    shell_environment(root, child, writable=False)
    try:
        result = subprocess.run(argv, cwd=root, env=child, capture_output=True, text=True,
                                encoding="utf-8", errors="replace", timeout=30, check=False)
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise SetupError(f"{argv[0]}: {exc}") from exc
    output = (result.stdout or result.stderr).strip()
    if result.returncode or not output:
        raise SetupError(f"{argv[0]} probe failed (exit {result.returncode}): {output[:500]}")
    return output


def locate(name: str, env: dict[str, str]) -> str:
    found = shutil.which(name, path=env.get("PATH", ""))
    if not found:
        raise SetupError(f"Missing required tool: {name}")
    return found


def rust_environment(root: Path, env: dict[str, str]) -> str:
    if tomllib is None:
        raise SetupError("Python >=3.11 with stdlib tomllib is required")
    channel = tomllib.loads((root / "rust-toolchain.toml").read_text(encoding="utf-8"))["toolchain"]["channel"]
    rustup = locate("rustup", env)
    rustc = Path(probe([rustup, "which", "--toolchain", channel, "rustc"], env, root))
    missing = [name for name in ("rustc", "cargo", "rustfmt", "cargo-clippy")
               if not (rustc.parent / (name + rustc.suffix)).is_file()]
    if missing:
        raise SetupError(f"Installed Rust {channel} lacks required binaries/components: {', '.join(missing)}; no rustup installation was attempted")
    env["RUSTUP_TOOLCHAIN"] = channel
    env["RUSTUP_HOME"] = str(rustc.parents[3])
    prepend_path(env, [rustc.parent])
    return channel


def native_bash(env: dict[str, str]) -> Path:
    git = Path(locate("git", env))
    for parent in git.parents:
        for relative in ("bin/bash.exe", "usr/bin/bash.exe"):
            candidate = parent / relative
            if candidate.is_file():
                return candidate
    raise SetupError("Native Git Bash missing; WSL bash is not a supported Windows build shell")


def zig_path(root: Path, env: dict[str, str]) -> str:
    if "ZIG" in env:
        value = env["ZIG"]
        if not value.strip():
            raise SetupError("ZIG is empty; unset it or provide a valid executable")
        if Path(value).is_absolute() or "/" in value or "\\" in value:
            return str((root / value).resolve())
        return locate(value, env)
    local = Path(env.get("HERDR_ZIG_HOME", str(root / ".local/toolchains/zig"))) / setup_zig.INSTALL_DIR_NAME
    candidate = local / ("zig.exe" if WINDOWS else "zig")
    return str(candidate) if candidate.is_file() else locate("zig", env)


def configure(root: Path, env: dict[str, str]) -> tuple[str, list[str]]:
    errors = []
    channel = ""
    try:
        channel = rust_environment(root, env)
    except (SetupError, OSError, KeyError, ValueError) as exc:
        errors.append(str(exc))
    if WINDOWS:
        try:
            bash = native_bash(env)
            prepend_path(env, [bash.parent])
        except SetupError as exc:
            errors.append(str(exc))
    try:
        env["ZIG"] = zig_path(root, env)
    except SetupError as exc:
        errors.append(str(exc))
    return channel, errors


def windows_sdk(root: Path, env: dict[str, str]) -> str:
    base = Path({key.upper(): value for key, value in env.items()}.get("PROGRAMFILES(X86)", "C:/Program Files (x86)"))
    vswhere = base / "Microsoft Visual Studio/Installer/vswhere.exe"
    if not vswhere.is_file():
        raise SetupError("Visual Studio C++ Build Tools missing (vswhere.exe not found); system installation requires authorization")
    installation = Path(probe([str(vswhere), "-latest", "-products", "*", "-requires",
                               "Microsoft.VisualStudio.Component.VC.Tools.x86.x64", "-property", "installationPath"], env, root))
    linkers = sorted(installation.glob("VC/Tools/MSVC/*/bin/Hostx64/x64/link.exe"))
    sdk = base / "Windows Kits/10/Lib"
    versions = sorted(p for p in sdk.glob("*") if (p / "um/x64/kernel32.lib").is_file()
                      and (p / "ucrt/x64/ucrt.lib").is_file())
    if not linkers or not versions:
        raise SetupError("MSVC x64 linker or Windows SDK x64 libraries missing; install C++ Build Tools with Windows SDK after authorization")
    probe([str(linkers[-1]), "/dump", "/headers", sys.executable], env, root)
    return f"{linkers[-1]}; SDK {versions[-1]}"


def inspect(root: Path, env: dict[str, str], channel: str, initial: list[str]) -> tuple[list[str], list[str]]:
    reports, errors = [], list(initial)
    commands = {"git": ["--version"], "just": ["--version"], "cargo-nextest": ["nextest", "--version"],
                "node": ["--version"], "bun": ["--version"], "rustc": ["--version"],
                "cargo": ["--version"], "rustfmt": ["--version"], "cargo-clippy": ["clippy", "--version"]}
    if WINDOWS:
        commands.update(bash=["--version"], powershell=["-NoProfile", "-NonInteractive", "-Command", "$PSVersionTable.PSVersion.ToString()"])
    for name, args in commands.items():
        try:
            binary = locate(name, env)
            output = probe([binary, *args], env, root)
            if name in ("rustc", "cargo") and channel and not re.search(r"\b" + re.escape(channel) + r"\b", output):
                raise SetupError(f"{name} must match rust-toolchain.toml ({channel}), got {output}")
            reports.append(f"OK {name}: {binary} ({output.splitlines()[0]})")
            if name in PINS:
                pin = PINS[name]
                for directory in (".local/downloads", ".local/tools/downloads"):
                    archive = root / directory / pin[1].rsplit("/", 1)[1]
                    if archive.is_file():
                        if sha256(archive) != pin[2]:
                            raise SetupError(f"Cached official {name} archive SHA256 mismatch: {archive}")
                        reports.append(f"OK {name} cached archive SHA256: {archive}")
        except SetupError as exc:
            errors.append(str(exc))
    try:
        output = probe([sys.executable, "-B", "-c", "import sys,tomllib; print(sys.version.split()[0])"], env, root)
        reports.append(f"OK python: {sys.executable} ({output})")
    except SetupError as exc:
        errors.append(str(exc))
    try:
        binary = zig_path(root, env)
        output = probe([binary, "version"], env, root)
        if output != setup_zig.ZIG_VERSION:
            raise SetupError(f"ZIG must be {setup_zig.ZIG_VERSION}, got {output!r} from {binary}")
        reports.append(f"OK zig: {binary} ({output})")
    except SetupError as exc:
        errors.append(str(exc))
    if WINDOWS:
        try:
            reports.append(f"OK MSVC/SDK: {windows_sdk(root, env)}")
        except SetupError as exc:
            errors.append(str(exc))
    graph_python = root / ".local/tools/graphify" / ("Scripts/python.exe" if WINDOWS else "bin/python")
    try:
        version = probe([str(graph_python), "-B", "-c", "import importlib.metadata; print(importlib.metadata.version('graphifyy'))"], env, root)
        reports.append(f"OPTIONAL graphify: {version} ({graph_python}); graph checks not run")
    except SetupError:
        reports.append("OPTIONAL graphify: unavailable; not required for build/test (framework-check needs it)")
    return reports, errors


def sha256(path: Path) -> str:
    with path.open("rb") as handle:
        return hashlib.file_digest(handle, "sha256").hexdigest()


def extract_zip(archive: Path, destination: Path) -> None:
    with zipfile.ZipFile(archive) as source:
        for entry in source.infolist():
            name = entry.filename
            path = PurePosixPath(name)
            if (path.is_absolute() or ".." in path.parts or "\\" in name
                    or PureWindowsPath(name).drive or ":" in name
                    or stat.S_ISLNK(entry.external_attr >> 16)):
                raise SetupError(f"Unsafe archive member: {name}")
            local_path(destination, name, "archive member")
        source.extractall(destination)


def install_portable(name: str, root: Path, env: dict[str, str]) -> None:
    if not WINDOWS or platform.machine().lower() not in ("amd64", "x86_64"):
        raise SetupError(f"Automatic {name} installation supports Windows x64 only; provide an existing executable on PATH")
    version, url, digest, member = PINS[name]
    downloads = local_path(root, ".local/downloads", "tool downloads")
    tools = local_path(root, ".local/tools", "tool installation")
    local_path(root, str(tools / name), "tool installation")
    downloads.mkdir(parents=True, exist_ok=True)
    archive = downloads / url.rsplit("/", 1)[1]
    if not archive.is_file() or sha256(archive) != digest:
        partial = archive.with_name(archive.name + "." + uuid.uuid4().hex + ".part")
        try:
            with urllib.request.urlopen(url, timeout=60) as response, partial.open("wb") as handle:
                shutil.copyfileobj(response, handle)
            if sha256(partial) != digest:
                raise SetupError(f"SHA256 mismatch for {name}")
            partial.replace(archive)
        finally:
            partial.unlink(missing_ok=True)
    tools.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="setup-", dir=tools) as temp:
        staging = Path(temp) / "new"
        staging.mkdir()
        extract_zip(archive, staging)
        binary = staging / member
        args = ["nextest", "--version"] if name == "cargo-nextest" else ["--version"]
        output = probe([str(binary), *args], env, root)
        if not re.search(r"\b" + re.escape(version) + r"\b", output):
            raise SetupError(f"Unexpected staged {name} version: {output}")
        target = tools / name
        backup = tools / f".{name}-backup-{uuid.uuid4().hex}"
        if target.exists():
            target.rename(backup)
        try:
            staging.rename(target)
        except OSError as exc:
            if backup.exists():
                try:
                    backup.rename(target)
                except OSError as rollback:
                    raise SetupError(f"Installation failed ({exc}); restore failed ({rollback}); original preserved at {backup}") from rollback
            raise
        if backup.exists():
            shutil.rmtree(backup)


def prepare_directories(root: Path, env: dict[str, str], *, shells: bool = True) -> None:
    for key in (*CACHE_PATHS, "TEMP", "TMP", "TMPDIR"):
        if key in env:
            local_path(root, env[key], key).mkdir(parents=True, exist_ok=True)
    if shells:
        shell_environment(root, env, writable=True)


@contextlib.contextmanager
def process_environment(env: dict[str, str]):
    original = dict(os.environ)
    os.environ.clear()
    os.environ.update(env)
    try:
        yield
    finally:
        os.environ.clear()
        os.environ.update(original)


def install(root: Path, env: dict[str, str], force: bool) -> None:
    if "ZIG" in env:
        output = probe([zig_path(root, env), "version"], env, root)
        if output != setup_zig.ZIG_VERSION:
            raise SetupError(f"Explicit ZIG must be {setup_zig.ZIG_VERSION}, got {output!r}")
    local_path(root, env.get("HERDR_ZIG_HOME", ".local/toolchains/zig"), "HERDR_ZIG_HOME")
    prepare_directories(root, env)
    for name in PINS:
        binary = shutil.which(name, path=env["PATH"])
        managed = binary and Path(binary).resolve().is_relative_to((root / ".local/tools").resolve())
        if binary and not (force and managed):
            args = ["nextest", "--version"] if name == "cargo-nextest" else ["--version"]
            probe([binary, *args], env, root)
            continue
        install_portable(name, root, env)
    with process_environment(env):
        result = setup_zig.install(force)
        if result:
            raise SetupError(f"Zig installer failed: {result}")


def bash_path(value: str) -> str:
    value = value.replace("\\", "/")
    if WINDOWS and re.match(r"^[A-Za-z]:/", value):
        value = "/" + value[0].lower() + value[2:]
    return value


def exports(env: dict[str, str], inherited: dict[str, str], shell: str) -> str:
    lines = []
    for name, value in env.items():
        if inherited.get(name) == value:
            continue
        if shell == "bash":
            if name == "PATH":
                value = ":".join(bash_path(part) for part in value.split(os.pathsep))
            lines.append(f"export {name}={shlex.quote(value)}")
        else:
            quoted = "'" + value.replace("'", "''") + "'"
            if not value.isascii():
                import base64
                encoded = base64.b64encode(value.encode("utf-8")).decode("ascii")
                quoted = f"[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{encoded}'))"
            lines.append("$env:" + name + " = " + quoted)
    return "\n".join(lines)


def write_activators(root: Path) -> None:
    local = local_path(root, ".local", "activators")
    for relative in ("activate.sh", "activate.ps1", "tool-shims/python", "tool-shims/python3",
                     "tool-shims/python.cmd", "tool-shims/python3.cmd"):
        local_path(root, str(local / relative), "activators")
    local.mkdir(parents=True, exist_ok=True)
    python = shlex.quote(bash_path(sys.executable))
    bash = f'''#!/usr/bin/env bash
_herdr_activate() {{
    local root output
    root="$(cd -- "$(dirname -- "${{BASH_SOURCE[0]}}")/.." && pwd)" || return
    if command -v cygpath >/dev/null 2>&1; then root="$(cygpath -m "$root")" || return; fi
    output="$({python} -B "$root/scripts/setup_env.py" --env bash)" || return
    eval "$output"
}}
_herdr_activate
_herdr_status=$?
unset -f _herdr_activate
return "$_herdr_status" 2>/dev/null || exit "$_herdr_status"
'''
    powershell = "'" + sys.executable.replace("'", "''") + "'"
    ps = f'''$herdrRoot = Split-Path -Parent $PSScriptRoot
$herdrOutput = & {powershell} -B (Join-Path $herdrRoot 'scripts/setup_env.py') --env powershell
if ($LASTEXITCODE -ne 0) {{ throw "herdr environment setup failed ($LASTEXITCODE)" }}
Invoke-Expression ($herdrOutput -join "`n")
Remove-Variable herdrRoot,herdrOutput
'''
    (local / "activate.sh").write_text(bash, encoding="utf-8", newline="\n")
    (local / "activate.ps1").write_text(ps, encoding="utf-8-sig", newline="\n")
    write_python_shims(root, local / "tool-shims")


def run_command(root: Path, env: dict[str, str], command: list[str]) -> int:
    prepare_directories(root, env, shells=False)
    with tempfile.TemporaryDirectory(prefix="setup-run-", dir=root / "target/tmp") as temporary:
        sandbox = Path(temporary)
        child = dict(env)
        for key in list(child):
            if key.upper() in HERDR_OVERRIDES:
                del child[key]
        for key, directory in {
            "HOME": "home", "USERPROFILE": "home", "XDG_CONFIG_HOME": "config",
            "XDG_STATE_HOME": "state", "XDG_DATA_HOME": "data", "XDG_CACHE_HOME": "cache",
            "XDG_RUNTIME_DIR": "runtime", "APPDATA": "appdata", "LOCALAPPDATA": "localappdata",
            "TEMP": "tmp", "TMP": "tmp", "TMPDIR": "tmp", "CODEX_HOME": "codex", "KIMI_CODE_HOME": "kimi",
        }.items():
            path = sandbox / directory
            path.mkdir(mode=0o700, exist_ok=True)
            child[key] = str(path)
        child["HERDR_HOME"] = str(sandbox / "herdr")
        Path(child["HERDR_HOME"]).mkdir()
        child["GIT_CEILING_DIRECTORIES"] = os.pathsep.join(
            value for value in (str(sandbox), child.get("GIT_CEILING_DIRECTORIES", "")) if value
        )
        shell_environment(root, child, writable=True, directory=sandbox)
        argv = list(command)
        if argv[0] in ("python", "python3"):
            argv[0] = sys.executable
        else:
            argv[0] = shutil.which(argv[0], path=child["PATH"]) or argv[0]
        try:
            return subprocess.run(argv, cwd=root, env=child, check=False).returncode
        except OSError as exc:
            raise SetupError(f"Cannot execute {argv[0]}: {exc}") from exc


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument("--check", action="store_true", help="read-only real probes; never install")
    modes.add_argument("--install", action="store_true", help="install missing portable tools (default)")
    modes.add_argument("--run", nargs=argparse.REMAINDER, help="run exact argv in isolated project environment; no installation")
    modes.add_argument("--env", choices=("bash", "powershell"), help="emit activation assignments, without installing tools")
    parser.add_argument("--force", action="store_true", help="replace managed tools only (install mode)")
    args = parser.parse_args(argv)
    if args.force and (args.check or args.run is not None or args.env):
        parser.error("--force is only valid in install mode")
    if args.run == []:
        parser.error("--run requires a command")
    try:
        inherited = dict(os.environ)
        env = environment(ROOT, inherited)
        installing = not args.check and args.run is None and not args.env
        if installing:
            install(ROOT, env, args.force)
        channel, errors = configure(ROOT, env)
        reports, errors = inspect(ROOT, env, channel, errors)
        if errors:
            for message in [*reports, *("FAIL " + error for error in errors)]:
                print("[setup-env] " + message, file=sys.stderr)
            return 1
        if args.env:
            prepare_directories(ROOT, env)
            print(exports(env, inherited, args.env))
            return 0
        if args.run is not None:
            return run_command(ROOT, env, args.run)
        for message in reports:
            print("[setup-env] " + message)
        if installing:
            write_activators(ROOT)
            print("[setup-env] Ready. Source .local/activate.sh or .local/activate.ps1; or use --run COMMAND ...")
        else:
            print("[setup-env] PASS: project environment probes; parent shell PATH is unchanged")
        return 0
    except (SetupError, OSError, ValueError, RuntimeError) as exc:
        print(f"[setup-env] FAIL {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8", errors="replace")
    raise SystemExit(main())
