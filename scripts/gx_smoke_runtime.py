#!/usr/bin/env python3
from __future__ import annotations

import argparse
from contextlib import contextmanager
import ctypes
import getpass
import hashlib
import json
import os
from pathlib import Path
import re
import select
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import uuid


ROOT_ENV = "HERDR_GX_SMOKE_ROOT"
DEADLINE_SECONDS = 180
CONFIG = '''onboarding = false
[experimental]
allow_nested = true
[update]
version_check = true
manifest_check = false
'''
INHERITED_KEYS = {
    "HERDR_SOCKET_PATH", "HERDR_CLIENT_SOCKET_PATH", "HERDR_SESSION",
    "HERDR_WORKSPACE_ID", "HERDR_TAB_ID", "HERDR_PANE_ID", "HERDR_STARTUP_CWD",
    "HERDR_CONFIG_PATH", "HERDR_HOME",
}


def isolated_env(root: Path, session: str, source: dict[str, str]) -> dict[str, str]:
    env = {key: value for key, value in source.items() if key not in INHERITED_KEYS}
    for key, name in {
        "HOME": "home", "USERPROFILE": "home", "LOCALAPPDATA": "local",
        "APPDATA": "roaming", "XDG_CONFIG_HOME": "config", "XDG_STATE_HOME": "state",
        "XDG_DATA_HOME": "data", "XDG_CACHE_HOME": "cache", "XDG_RUNTIME_DIR": "run",
        "HERDR_HOME": "herdr-home",
    }.items():
        env[key] = str(root / name)
    env.update({
        ROOT_ENV: str(root), "HERDR_SESSION": session,
        "HERDR_CONFIG_PATH": str(root / "config.toml"), "HERDR_LANG": "en",
        "HERDR_FAKE_UPDATE_VERSION": "9999.0.0", "TERM": "xterm-256color",
    })
    if os.name != "nt":
        env["SHELL"] = "/bin/sh"
    else:
        system_root = next((value for key, value in source.items() if key.upper() == "SYSTEMROOT"), None)
        if not system_root:
            raise RuntimeError("Windows runtime probe requires SystemRoot")
        env["SHELL"] = str(Path(system_root) / "System32/WindowsPowerShell/v1.0/powershell.exe")
    return env


def process_identity(pid: int) -> str | None:
    if os.name == "nt":
        from ctypes import wintypes
        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel.OpenProcess.restype = wintypes.HANDLE
        kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
        kernel.GetProcessTimes.argtypes = [wintypes.HANDLE] + [ctypes.POINTER(wintypes.FILETIME)] * 4
        kernel.WaitForSingleObject.argtypes = [wintypes.HANDLE, wintypes.DWORD]
        kernel.CloseHandle.argtypes = [wintypes.HANDLE]
        handle = kernel.OpenProcess(0x100000 | 0x1000, False, pid)
        if not handle:
            return None
        try:
            if kernel.WaitForSingleObject(handle, 0) != 258:
                return None
            times = [wintypes.FILETIME() for _ in range(4)]
            if not kernel.GetProcessTimes(handle, *(ctypes.byref(item) for item in times)):
                raise ctypes.WinError(ctypes.get_last_error())
            return f"{pid}:{times[0].dwHighDateTime}:{times[0].dwLowDateTime}"
        finally:
            kernel.CloseHandle(handle)
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        if fields[0] == "Z":
            return None
        return f"{pid}:{fields[19]}"
    except (FileNotFoundError, ProcessLookupError):
        return None


def same_process(identity: str | None) -> bool:
    return bool(identity and process_identity(int(identity.split(":", 1)[0])) == identity)


def require_pidfd() -> None:
    if not callable(getattr(os, "pidfd_open", None)) or not callable(getattr(signal, "pidfd_send_signal", None)):
        raise RuntimeError("Linux smoke cleanup requires pidfd_open and pidfd_send_signal; PID-based fallback is prohibited")


def pidfd_running(pidfd: int) -> bool:
    poll = select.poll()
    poll.register(pidfd, select.POLLIN | select.POLLHUP | select.POLLERR)
    return not poll.poll(0)


def pidfd_belongs_to_root(pidfd: int, proc: Path, root: Path) -> bool:
    if not pidfd_running(pidfd):
        return False
    pid = int(proc.name)
    identity = process_identity(pid)
    if identity is None or proc.stat().st_uid != os.getuid():
        return False
    marker = f"{ROOT_ENV}={root}".encode()
    if marker not in (proc / "environ").read_bytes().split(b"\0"):
        return False
    return process_identity(pid) == identity and pidfd_running(pidfd)


@contextmanager
def owned_linux_processes(root: Path):
    require_pidfd()
    owned = []
    try:
        for proc in Path("/proc").iterdir():
            if not proc.name.isdigit() or int(proc.name) == os.getpid():
                continue
            pidfd = None
            try:
                pidfd = os.pidfd_open(int(proc.name), 0)
                if pidfd_belongs_to_root(pidfd, proc, root):
                    owned.append(pidfd)
                    pidfd = None
            except (FileNotFoundError, ProcessLookupError, PermissionError):
                continue
            finally:
                if pidfd is not None:
                    os.close(pidfd)
        yield owned
    finally:
        for pidfd in owned:
            os.close(pidfd)


def linux_processes_remaining(root: Path) -> bool:
    with owned_linux_processes(root) as processes:
        return bool(processes)


def reap_linux(root: Path) -> None:
    for sig in (signal.SIGTERM, signal.SIGKILL):
        with owned_linux_processes(root) as processes:
            for pidfd in processes:
                try:
                    signal.pidfd_send_signal(pidfd, sig)
                except ProcessLookupError:
                    pass
        deadline = time.monotonic() + 3
        while linux_processes_remaining(root) and time.monotonic() < deadline:
            time.sleep(0.05)
    if linux_processes_remaining(root):
        raise RuntimeError(f"owned processes survived cleanup: {root}")


class WindowsJob:
    def __init__(self) -> None:
        from ctypes import wintypes

        class Basic(ctypes.Structure):
            _fields_ = [
                ("process_time", ctypes.c_int64), ("job_time", ctypes.c_int64),
                ("flags", wintypes.DWORD), ("min_working", ctypes.c_size_t),
                ("max_working", ctypes.c_size_t), ("process_limit", wintypes.DWORD),
                ("affinity", ctypes.c_size_t), ("priority", wintypes.DWORD),
                ("scheduling", wintypes.DWORD),
            ]

        class Extended(ctypes.Structure):
            _fields_ = [("basic", Basic), ("io", ctypes.c_uint64 * 6),
                        ("memory", ctypes.c_size_t * 4)]

        self.kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        self.kernel.CreateJobObjectW.restype = wintypes.HANDLE
        self.kernel.CreateJobObjectW.argtypes = [ctypes.c_void_p, wintypes.LPCWSTR]
        self.kernel.SetInformationJobObject.argtypes = [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD]
        self.kernel.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
        self.kernel.TerminateJobObject.argtypes = [wintypes.HANDLE, wintypes.UINT]
        self.kernel.CloseHandle.argtypes = [wintypes.HANDLE]
        self.handle = self.kernel.CreateJobObjectW(None, None)
        if not self.handle:
            raise ctypes.WinError(ctypes.get_last_error())
        info = Extended()
        info.basic.flags = 0x2000
        if not self.kernel.SetInformationJobObject(self.handle, 9, ctypes.byref(info), ctypes.sizeof(info)):
            self.close()
            raise ctypes.WinError(ctypes.get_last_error())

    def assign(self, process: subprocess.Popen) -> None:
        if not self.kernel.AssignProcessToJobObject(self.handle, int(process._handle)):
            raise ctypes.WinError(ctypes.get_last_error())

    def close(self) -> None:
        if self.handle:
            self.kernel.TerminateJobObject(self.handle, 1)
            self.kernel.CloseHandle(self.handle)
            self.handle = None


def sandbox_base() -> Path:
    name = str(os.getuid()) if os.name != "nt" else re.sub(r"[^a-zA-Z0-9_-]", "_", getpass.getuser())
    parent = Path(tempfile.gettempdir()) if os.name == "nt" else Path("/var/tmp")
    return parent / f"herdr-gx-smoke-{name}"


def sweep_stale(base: Path) -> None:
    for root in base.glob("run-*"):
        if root.is_symlink() or not root.is_dir():
            continue
        marker = root / "owner.json"
        if not marker.is_file():
            continue
        info = json.loads(marker.read_text(encoding="utf-8"))
        if info.get("schema") != 1 or info.get("root") != str(root):
            continue
        if same_process(info.get("owner")) or same_process(info.get("guard")):
            continue
        if os.name != "nt":
            reap_linux(root)
        shutil.rmtree(root)


def owned_root(path: Path) -> tuple[Path, dict]:
    root = path.resolve()
    if root.parent != sandbox_base().resolve() or not root.name.startswith("run-"):
        raise RuntimeError("refusing cleanup outside the smoke sandbox")
    info = json.loads((root / "owner.json").read_text(encoding="utf-8"))
    if info.get("schema") != 1 or info.get("root") != str(root):
        raise RuntimeError("smoke sandbox ownership marker mismatch")
    return root, info


def guard(args: argparse.Namespace) -> int:
    root, info = owned_root(args.root)
    info["guard"] = process_identity(os.getpid())
    temporary = root / "owner.json.tmp"
    temporary.write_text(json.dumps(info), encoding="utf-8")
    temporary.replace(root / "owner.json")
    if os.name != "nt":
        require_pidfd()
    job = WindowsJob() if os.name == "nt" else None
    worker = None
    try:
        worker = subprocess.Popen(
            [sys.executable, str(Path(__file__).resolve()), "--worker", "--root", str(root),
             "--binary", args.binary, "--expected-version", args.expected_version,
             "--hold-dir", args.hold_dir,
             *(["--reaper-probe"] if args.reaper_probe else [])],
            stdin=subprocess.PIPE,
            env={**os.environ, ROOT_ENV: str(root)},
        )
        if job:
            job.assign(worker)
        worker.stdin.write(b"GO\n")
        worker.stdin.close()
        deadline = time.monotonic() + DEADLINE_SECONDS
        while worker.poll() is None:
            if not same_process(info["owner"]) or time.monotonic() >= deadline:
                raise RuntimeError("smoke owner exited or watchdog deadline expired")
            time.sleep(0.1)
        return worker.returncode
    finally:
        if job:
            job.close()
        else:
            reap_linux(root)
        if worker:
            if job and worker.poll() is None:
                worker.kill()
            worker.wait(timeout=10)
        shutil.rmtree(root)


def command(binary: str, env: dict[str, str], root: Path, *args: str,
            check: bool = True) -> subprocess.CompletedProcess:
    result = subprocess.run([binary, "--session", env["HERDR_SESSION"], *args],
                            env=env, cwd=root, capture_output=True, text=True,
                            encoding="utf-8", errors="replace", timeout=20)
    if check and result.returncode:
        raise RuntimeError(f"herdr {' '.join(args)} failed ({result.returncode}): {result.stdout}{result.stderr}")
    return result


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def require_package_version(version: str, manager: str) -> None:
    match = re.fullmatch(r"herdr \S+-gx\.(windows-installer|deb)\.[0-9a-f]{40}", version)
    if not match or match.group(1) != manager:
        raise RuntimeError(f"refusing update probes against a non-package/wrong-platform binary: {version!r}")


def runtime_smoke(args: argparse.Namespace) -> int:
    if sys.stdin.buffer.readline() != b"GO\n":
        raise RuntimeError("runtime worker must be launched by its watchdog")
    root, _ = owned_root(args.root)
    if args.reaper_probe:
        child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(300)"],
                                 env={**os.environ, ROOT_ENV: str(root)})
        (root / "probe-ready").write_text(json.dumps([
            process_identity(os.getpid()), process_identity(child.pid),
        ]), encoding="utf-8")
        time.sleep(300)
        return 0
    session = "gx-smoke-" + uuid.uuid4().hex[:16]
    env = isolated_env(root, session, dict(os.environ))
    for name in ("home", "local", "roaming", "config", "state", "data", "cache", "run", "herdr-home"):
        (root / name).mkdir(mode=0o700)
    config_path = root / "config.toml"
    config_path.write_text(CONFIG, encoding="utf-8")
    binary = str(Path(args.binary).resolve())
    before_binary, before_config = digest(Path(binary)), digest(config_path)
    call = lambda *argv, **kwargs: command(binary, env, root, *argv, **kwargs)
    version = call("--version").stdout.strip()
    require_package_version(version, "windows-installer" if os.name == "nt" else "deb")
    if args.expected_version and version != args.expected_version:
        raise RuntimeError(f"version mismatch: {version!r} != {args.expected_version!r}")
    call("--help")
    for argv in (("server", "--help"), ("workspace", "create", "--help"),
                 ("pane", "run", "--help"), ("pane", "read", "--help"),
                 ("session", "stop", "--help"), ("session", "delete", "--help")):
        call(*argv)
    call("config", "check")
    server = None
    with (root / "server.log").open("wb") as log:
        try:
            server = subprocess.Popen([binary, "--session", session, "server"],
                                      env=env, cwd=root, stdin=subprocess.DEVNULL,
                                      stdout=log, stderr=log)
            deadline = time.monotonic() + 25
            while True:
                ready = call("pane", "list", check=False)
                if ready.returncode == 0:
                    break
                if server.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("server not ready: " + (root / "server.log").read_text(errors="replace"))
                time.sleep(0.15)
            created = json.loads(call("workspace", "create", "--no-focus", "--label", session,
                                      "--cwd", str(root)).stdout)
            pane = created["result"]["root_pane"]["pane_id"]
            if not isinstance(pane, str) or not pane:
                raise RuntimeError("workspace create returned no pane id")
            marker = "GX_SMOKE_" + uuid.uuid4().hex
            if os.name == "nt":
                script = "Write-Output ('" + marker[:9] + "' + '" + marker[9:] + "')"
            else:
                script = "printf '%s%s\\n' '" + marker[:9] + "' '" + marker[9:] + "'"
            call("pane", "run", pane, script)
            deadline = time.monotonic() + 25
            while True:
                read = call("pane", "read", pane, "--source", "recent-unwrapped", "--lines", "80", "--format", "text")
                if marker in read.stdout:
                    break
                if time.monotonic() >= deadline:
                    raise RuntimeError(f"pane did not produce the marker: {read.stdout}")
                time.sleep(0.15)
            for argv in (("update",), ("update", "--handoff"), ("channel", "set", "stable"),
                         ("channel", "set", "preview")):
                refused = call(*argv, check=False)
                if refused.returncode == 0:
                    raise RuntimeError(f"package build did not reject {' '.join(argv)}")
                if not re.search(r"install|package|deb", refused.stdout + refused.stderr, re.I):
                    raise RuntimeError(f"no package-management guidance: {refused.stdout}{refused.stderr}")
                if digest(Path(binary)) != before_binary or digest(config_path) != before_config:
                    raise RuntimeError("rejected update changed the binary or config")
                call("pane", "read", pane, "--format", "text")
            call("session", "list", "--json")
            if args.hold_dir:
                hold = Path(args.hold_dir).resolve()
                (hold / "ready").write_text(str(server.pid), encoding="utf-8")
                deadline = time.monotonic() + 60
                while not (hold / "release").exists():
                    if time.monotonic() >= deadline:
                        raise RuntimeError("occupied-runtime smoke did not release its hold")
                    if server.poll() is not None:
                        raise RuntimeError("an installer stopped the running smoke runtime")
                    time.sleep(0.1)
                call("pane", "read", pane, "--format", "text")
        finally:
            if server:
                try:
                    call("session", "stop", session, "--json")
                    server.wait(timeout=15)
                    call("session", "delete", session, "--json")
                    listed = call("session", "list", "--json").stdout
                    if session in listed:
                        raise RuntimeError("temporary session remains after delete")
                except Exception:
                    print((root / "server.log").read_text(errors="replace"), file=sys.stderr)
                    raise
    print(json.dumps({"status": "PASS", "binary": binary, "version": version,
                      "session": session, "pane": pane, "marker": marker,
                      "cleanup": "session stopped and deleted"}), flush=True)
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Run a bounded isolated runtime probe of an installed Herdr binary")
    parser.add_argument("--binary", default="herdr")
    parser.add_argument("--expected-version", default="")
    parser.add_argument("--hold-dir", default="", help="Signal ready here and wait up to 60s for a release file while the owned runtime remains running")
    parser.add_argument("--guard", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--worker", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--root", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--reaper-probe", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.worker:
        return runtime_smoke(args)
    if args.guard:
        return guard(args)
    if args.reaper_probe:
        binary = sys.executable
    else:
        binary = shutil.which(args.binary)
        if not binary:
            parser.error(f"installed binary not found: {args.binary}")
    base = sandbox_base()
    base.mkdir(mode=0o700, parents=True, exist_ok=True)
    sweep_stale(base)
    root = Path(tempfile.mkdtemp(prefix="run-", dir=base)).resolve()
    (root / "owner.json").write_text(json.dumps({
        "schema": 1, "root": str(root), "owner": process_identity(os.getpid()),
    }), encoding="utf-8")
    options = {"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP | subprocess.DETACHED_PROCESS} if os.name == "nt" else {"start_new_session": True}
    guard_process = subprocess.Popen(
        [sys.executable, str(Path(__file__).resolve()), "--guard", "--root", str(root),
         "--binary", str(Path(binary).resolve()), "--expected-version", args.expected_version,
         "--hold-dir", args.hold_dir,
         *(["--reaper-probe"] if args.reaper_probe else [])],
        stdin=subprocess.DEVNULL, stdout=sys.stdout, stderr=sys.stderr, **options)
    return guard_process.wait(timeout=DEADLINE_SECONDS + 45)


if __name__ == "__main__":
    raise SystemExit(main())
