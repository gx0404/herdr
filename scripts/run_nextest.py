"""Run nextest unchanged by default, or with an explicit shared-budget hash partition."""

from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import json
import os
import signal
import subprocess
import sys
import threading
import time
import uuid
from pathlib import Path, PureWindowsPath
from typing import Mapping

try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib

if __package__:
    from .run_test_suite import _atomic_write_json, new_run_id
else:
    from run_test_suite import _atomic_write_json, new_run_id

PROJECT_ROOT = Path(__file__).resolve().parent.parent
LOG_DIR = PROJECT_ROOT / "target" / "test-suite-logs"
REPORT_ARGS = [
    "--status-level", "leak", "--final-status-level", "fail",
    "--failure-output", "final", "--success-output", "never",
]


class NoTestsError(ValueError):
    pass


def allocation(jobs: int | str, shards: int | str) -> list[int]:
    values = []
    for name, raw in (("nextest jobs", jobs), ("HERDR_NEXTEST_SHARDS", shards)):
        if isinstance(raw, bool) or not str(raw).isascii() or not str(raw).isdigit() or int(raw) < 1:
            raise ValueError(f"{name} must be a positive integer: {raw!r}")
        values.append(int(raw))
    budget, count = values
    if count > budget:
        raise ValueError("HERDR_NEXTEST_SHARDS must not exceed the total nextest jobs budget")
    base, extra = divmod(budget, count)
    return [base + (index < extra) for index in range(count)]


def single_command(jobs: int, filterset: str | None) -> list[str]:
    command = ["cargo", "nextest", "run", "--locked", "--test-threads", str(jobs), *REPORT_ARGS]
    return command + (["--filterset", filterset] if filterset is not None else [])


def has_recording_config(value: object) -> bool:
    if isinstance(value, dict):
        return any("record" in key.lower() or has_recording_config(item) for key, item in value.items())
    if isinstance(value, list):
        return any(has_recording_config(item) for item in value)
    return False


def check_shard_config(root: Path, environment: Mapping[str, str]) -> None:
    def refuse(reason: str) -> None:
        raise ValueError(f"Cannot shard nextest: {reason}; use HERDR_NEXTEST_SHARDS=1")

    if environment.get("NEXTEST_PROFILE", "default") != "default":
        refuse("only the default profile supports sharding")
    for key in environment:
        if key == "NEXTEST_RUN_ID" or (key.startswith("NEXTEST_") and "RECORD" in key):
            refuse(f"inherited {key} cannot be isolated safely")
    path = root / ".config" / "nextest.toml"
    config = tomllib.loads(path.read_text(encoding="utf-8")) if path.exists() else {}
    if set(config) - {"profile", "nextest-version"}:
        refuse("repository store, groups, scripts or other global settings require one runner")
    profile = config.get("profile", {}).get("default", {})
    safe = {
        "default-filter", "retries", "slow-timeout", "leak-timeout", "fail-fast",
        "status-level", "final-status-level", "failure-output", "success-output",
        "junit", "flaky-result",
    }
    if set(profile) - safe:
        refuse("default profile overrides, thread constraints or unknown settings require one runner")
    junit = profile.get("junit", {}).get("path")
    if junit is not None:
        path_value = PureWindowsPath(junit)
        if path_value.drive or path_value.root or ".." in path_value.parts:
            refuse("JUnit path must remain relative to each runner's store")

    configured_user = environment.get("NEXTEST_USER_CONFIG_FILE")
    if configured_user == "none":
        return
    if configured_user:
        user_paths = [root / configured_user]
    else:
        home = Path(environment.get("HOME") or environment.get("USERPROFILE") or Path.home())
        user_paths = [Path(environment.get("XDG_CONFIG_HOME", str(home / ".config"))) / "nextest" / "config.toml"]
        if os.name == "nt" and environment.get("APPDATA"):
            user_paths.append(Path(environment["APPDATA"]) / "nextest" / "config.toml")
        if sys.platform == "darwin":
            user_paths.append(home / "Library" / "Application Support" / "nextest" / "config.toml")
    for user_path in user_paths:
        if user_path.is_file():
            user = tomllib.loads(user_path.read_text(encoding="utf-8"))
            if has_recording_config(user):
                refuse("user recording configuration requires one runner")


def _unique_object(pairs: list[tuple[str, object]]) -> dict:
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate nextest JSON key: {key}")
        result[key] = value
    return result


def selected_tests(path: Path) -> set[tuple[str, str]]:
    data = json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=_unique_object)
    suites = data.get("rust-suites")
    if not isinstance(suites, dict) or not isinstance(data.get("test-count"), int):
        raise ValueError(f"unsupported nextest list format: {path}")
    selected = set()
    total = 0
    for suite_id, suite in suites.items():
        cases = suite.get("testcases")
        if not isinstance(cases, dict):
            raise ValueError(f"missing nextest testcases: {suite_id}")
        total += len(cases)
        for name, case in cases.items():
            status = case.get("filter-match", {}).get("status")
            if status not in {"matches", "mismatch"} or not isinstance(case.get("ignored"), bool):
                raise ValueError(f"unsupported nextest filter-match: {suite_id}:{name}")
            if status == "matches":
                if case["ignored"]:
                    raise ValueError(f"unexpected ignored test selected: {suite_id}:{name}")
                selected.add((suite_id, name))
    if total != data["test-count"]:
        raise ValueError(f"nextest list test-count mismatch: {path}")
    return selected


def set_summary(tests: set[tuple[str, str]]) -> dict:
    encoded = json.dumps(sorted(tests), ensure_ascii=True, separators=(",", ":")).encode()
    return {"count": len(tests), "sha256": hashlib.sha256(encoded).hexdigest()}


def verify_coverage(baseline: set, partitions: list[set]) -> None:
    seen = set()
    for tests in partitions:
        if seen & tests:
            raise ValueError("nextest partitions overlap")
        seen.update(tests)
    if seen != baseline:
        raise ValueError(f"nextest partition coverage mismatch: missing={len(baseline - seen)}, extra={len(seen - baseline)}")


class OwnedProcess:
    STOP_GRACE_SECONDS = 15
    STOP_KILL_SECONDS = 10

    def __init__(self, command: list[str], **kwargs):
        self.job = None
        self.process = None
        self._stopped = False
        self._stop_error = None
        if os.name == "nt":
            import ctypes
            from ctypes import wintypes

            class BasicLimits(ctypes.Structure):
                _fields_ = [
                    ("process_time", ctypes.c_int64), ("job_time", ctypes.c_int64),
                    ("flags", wintypes.DWORD), ("min_ws", ctypes.c_size_t),
                    ("max_ws", ctypes.c_size_t), ("active", wintypes.DWORD),
                    ("affinity", ctypes.c_size_t), ("priority", wintypes.DWORD),
                    ("scheduling", wintypes.DWORD),
                ]

            class ExtendedLimits(ctypes.Structure):
                _fields_ = [
                    ("basic", BasicLimits), ("io", ctypes.c_uint64 * 6),
                    ("process_memory", ctypes.c_size_t), ("job_memory", ctypes.c_size_t),
                    ("peak_process", ctypes.c_size_t), ("peak_job", ctypes.c_size_t),
                ]

            self.api = ctypes.WinDLL("kernel32", use_last_error=True)
            signatures = {
                "CreateJobObjectW": ([ctypes.c_void_p, wintypes.LPCWSTR], wintypes.HANDLE),
                "SetInformationJobObject": ([wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD], wintypes.BOOL),
                "AssignProcessToJobObject": ([wintypes.HANDLE, wintypes.HANDLE], wintypes.BOOL),
                "TerminateJobObject": ([wintypes.HANDLE, wintypes.UINT], wintypes.BOOL),
                "CloseHandle": ([wintypes.HANDLE], wintypes.BOOL),
            }
            for name, (arguments, result) in signatures.items():
                function = getattr(self.api, name)
                function.argtypes, function.restype = arguments, result
            self.job = self.api.CreateJobObjectW(None, None)
            if not self.job:
                raise ctypes.WinError(ctypes.get_last_error())
            try:
                limits = ExtendedLimits()
                limits.basic.flags = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                if not self.api.SetInformationJobObject(self.job, 9, ctypes.byref(limits), ctypes.sizeof(limits)):
                    raise ctypes.WinError(ctypes.get_last_error())
                self.process = subprocess.Popen(command, creationflags=0x4, **kwargs)
                if not self.api.AssignProcessToJobObject(self.job, int(self.process._handle)):
                    raise ctypes.WinError(ctypes.get_last_error())
                resume = ctypes.WinDLL("ntdll").NtResumeProcess
                resume.argtypes, resume.restype = [wintypes.HANDLE], ctypes.c_long
                if resume(int(self.process._handle)) < 0:
                    raise OSError("cannot resume owned nextest process")
            except BaseException:
                if self.process is not None:
                    self.process.kill()
                    self.process.wait()
                self.close()
                raise
        else:
            self.process = subprocess.Popen(command, start_new_session=True, **kwargs)

    def stop(self) -> None:
        if self._stopped:
            if self._stop_error is not None:
                raise OSError(self._stop_error)
            return
        try:
            if self.job is not None:
                if not self.api.TerminateJobObject(self.job, 130):
                    raise OSError("cannot terminate owned nextest job")
                self.process.wait(timeout=self.STOP_KILL_SECONDS)
            else:
                self._stop_posix()
        except BaseException as error:
            self._stop_error = f"cleanup incomplete: {type(error).__name__}: {error}"
            self._stopped = self.process.poll() is not None
            raise
        else:
            self._stopped = True
            self._stop_error = None

    def _signal_group(self, signum: int) -> bool:
        if self.process.poll() is not None:
            return False
        try:
            os.killpg(self.process.pid, signum)
        except ProcessLookupError:
            return False
        return True

    def _stop_posix(self) -> None:
        if not self._signal_group(signal.SIGTERM):
            raise OSError("runner exited before cancellation; descendant cleanup unverified")
        try:
            code = self.process.wait(timeout=self.STOP_GRACE_SECONDS)
        except (subprocess.TimeoutExpired, KeyboardInterrupt):
            self._signal_group(signal.SIGTERM)
            try:
                self.process.wait(timeout=self.STOP_KILL_SECONDS)
            except (subprocess.TimeoutExpired, KeyboardInterrupt):
                self._signal_group(signal.SIGKILL)
                self.process.wait(timeout=self.STOP_KILL_SECONDS)
                raise OSError("runner force-killed; separate test groups may remain, cleanup unverified")
            raise OSError("nextest required a second cancellation signal (forced cleanup)")
        if code is None or code < 0:
            raise OSError("runner did not finish its cancellation handler; descendant cleanup unverified")

    def close(self) -> None:
        try:
            if self.process is not None:
                for stream in (self.process.stdout, self.process.stderr):
                    if stream is not None:
                        stream.close()
        finally:
            if self.job is not None:
                if not self.api.CloseHandle(self.job):
                    raise OSError("cannot close owned nextest job")
                self.job = None


class NextestRun:
    def __init__(self, jobs: int, shards: int, filterset: str | None = None,
                 *, root: Path = PROJECT_ROOT, log_root: Path | None = None,
                 environment: Mapping[str, str] | None = None):
        self.workers = allocation(jobs, shards)
        self.root = root.resolve()
        self.environment = dict(os.environ if environment is None else environment)
        self.filter_args = ["--filterset", filterset] if filterset is not None else []
        self.filterset = filterset
        self.run_id = "nextest-" + new_run_id()
        self.directory = (log_root or self.root / "target" / "test-suite-logs") / self.run_id
        self.directory.mkdir(parents=True, exist_ok=False)
        self.manifest_path = self.directory / "manifest.json"
        self.lock = threading.RLock()
        self.output_lock = threading.Lock()
        self.processes: list[OwnedProcess] = []
        self.cancelled = threading.Event()
        self.started = time.monotonic()
        self.manifest = {
            "run_id": self.run_id, "status": "pending", "nextest_jobs": sum(self.workers),
            "shards": len(self.workers), "filterset": filterset, "manifest": str(self.manifest_path),
            "commands": {}, "runners": [], "failures": [], "seconds": 0.0,
            "expected_counts": None, "coverage": None,
        }

    def save(self) -> None:
        with self.lock:
            self.manifest["seconds"] = round(time.monotonic() - self.started, 3)
            _atomic_write_json(self.manifest_path, self.manifest)

    def fail(self, message: str) -> None:
        with self.lock:
            self.manifest["status"] = "failed"
            self.manifest["failures"].append(message)
            self.save()

    def cancel(self) -> None:
        with self.lock:
            self.cancelled.set()
            errors = []
            for owner in self.processes:
                try:
                    try:
                        owner.stop()
                    except KeyboardInterrupt:
                        errors.append("cleanup interrupted; retrying owned process")
                        owner.stop()
                except BaseException as error:
                    errors.append(f"{type(error).__name__}: {error}")
            if errors:
                message = "owned process cleanup failed: " + "; ".join(errors)
                self.fail(message)
                raise OSError(message)

    def command(self, name: str, command: list[str], *, output: Path | None = None,
                record: dict | None = None) -> int:
        record = record if record is not None else {}
        log_path = self.directory / (name + ".log")
        with self.lock:
            record.update(command=command, status="pending", exit_code=None, seconds=0.0, log=str(log_path))
            if output is not None:
                record["output"] = str(output)
            self.manifest["commands"][name] = record
            self.save()
        start = time.monotonic()
        owner = None
        try:
            with log_path.open("w", encoding="utf-8", buffering=1) as log:
                from contextlib import ExitStack
                with ExitStack() as stack:
                    target = stack.enter_context(output.open("wb")) if output is not None else subprocess.PIPE
                    environment = self.environment.copy()
                    with self.lock:
                        if self.cancelled.is_set():
                            raise InterruptedError("nextest run cancelled before launch")
                        owner = OwnedProcess(command, cwd=self.root, env=environment, stdout=target,
                                             stderr=subprocess.PIPE if output is not None else subprocess.STDOUT,
                                             encoding="utf-8", errors="replace")
                        self.processes.append(owner)
                        record["status"] = "running"
                        self.save()
                    stream = owner.process.stderr if output is not None else owner.process.stdout
                    if stream is None:
                        raise OSError("missing nextest output stream")
                    with stream:
                        for line in stream:
                            log.write(line)
                            log.flush()
                            with self.output_lock:
                                sys.stdout.write(f"[{name}] {line}")
                                sys.stdout.flush()
                    code = owner.process.wait()
                    if code is None:
                        raise OSError("missing nextest exit code")
            with self.lock:
                record.update(status="complete", exit_code=code, seconds=round(time.monotonic() - start, 3))
                if code != 0:
                    self.fail(f"{name}: exit {code}")
                self.save()
            return code
        except BaseException as error:
            with self.lock:
                record.update(status="failed", error=str(error) or type(error).__name__,
                              seconds=round(time.monotonic() - start, 3))
            try:
                self.fail(f"{name}: {type(error).__name__}: {error}")
            finally:
                self.cancel()
            raise
        finally:
            if owner is not None:
                with self.lock:
                    owner.close()
                    self.processes.remove(owner)

    def prepare(self) -> list[dict]:
        check_shard_config(self.root, self.environment)
        cargo = self.directory / "cargo-metadata.json"
        binaries = self.directory / "binaries-metadata.json"
        if self.command("cargo-metadata", ["cargo", "metadata", "--locked", "--offline", "--format-version", "1"], output=cargo):
            raise RuntimeError("cargo metadata failed")
        if self.command("binaries-metadata", ["cargo", "nextest", "list", "--locked", "--list-type", "binaries-only", "--message-format", "json"], output=binaries):
            raise RuntimeError("nextest binary compilation failed")
        reuse = ["--cargo-metadata", str(cargo), "--binaries-metadata", str(binaries)]
        baseline_path = self.directory / "baseline.json"
        if self.command("baseline", ["cargo", "nextest", "list", *reuse, *self.filter_args, "--message-format", "json"], output=baseline_path):
            raise RuntimeError("nextest baseline listing failed")
        baseline = selected_tests(baseline_path)
        records, partitions = [], []
        for index, workers in enumerate(self.workers, 1):
            name = f"shard-{index}"
            directory = self.directory / name
            directory.mkdir()
            config = directory / "tool.toml"
            store = (directory / "nextest").relative_to(self.root).as_posix()
            config.write_text("[store]\ndir = " + json.dumps(store) + "\n", encoding="utf-8")
            partition = f"hash:{index}/{len(self.workers)}"
            common = [*reuse, "--tool-config-file", "herdr:" + str(config), *self.filter_args, "--partition", partition]
            listing = directory / "tests.json"
            if self.command(name + "-list", ["cargo", "nextest", "list", *common, "--message-format", "json"], output=listing):
                raise RuntimeError(f"{name} listing failed")
            tests = selected_tests(listing)
            partitions.append(tests)
            record = {
                "name": name, "partition": partition, "workers": workers,
                "invocation_id": str(uuid.uuid4()), "tool_config": str(config), "store": store,
                "expected": set_summary(tests), "status": "pending", "exit_code": None,
                "seconds": 0.0, "log": str(self.directory / (name + ".log")),
                "command": ["cargo", "nextest", "run", *common, "--test-threads", str(workers), *REPORT_ARGS, "--no-tests", "fail"],
            }
            records.append(record)
            self.manifest["runners"] = records
            self.save()
        verify_coverage(baseline, partitions)
        self.manifest["coverage"] = {**set_summary(baseline), "disjoint": True, "complete": True}
        self.manifest["expected_counts"] = [len(tests) for tests in partitions]
        for record in records:
            if not record["expected"]["count"]:
                record["status"] = "idle"
        self.save()
        if not baseline:
            raise NoTestsError("no tests to run (nextest empty-selection failure)")
        return records

    def run(self) -> int:
        try:
            self.save()
            print(f"nextest manifest: {self.manifest_path}", flush=True)
            self.manifest["status"] = "running"
            self.save()
            if len(self.workers) == 1:
                record = {"name": "single", "workers": self.workers[0], "partition": None}
                self.manifest["runners"] = [record]
                code = self.command("single", single_command(self.workers[0], self.filterset), record=record)
            else:
                records = self.prepare()
                pool = concurrent.futures.ThreadPoolExecutor(max_workers=len(records))
                try:
                    futures = [pool.submit(self.command, record["name"], record["command"], record=record)
                               for record in records if record["status"] != "idle"]
                    for future in concurrent.futures.as_completed(futures):
                        future.result()
                except BaseException:
                    try:
                        self.fail("runner execution interrupted")
                    finally:
                        self.cancel()
                    raise
                finally:
                    pool.shutdown(wait=True, cancel_futures=True)
                code = 0 if all(
                    (record["status"] == "idle" and record["expected"]["count"] == 0)
                    or (record["status"] == "complete" and record["exit_code"] == 0)
                    for record in records
                ) else 1
            if code != 0 or self.manifest["failures"]:
                self.fail("nextest did not complete successfully")
                return code or 1
            self.manifest["status"] = "passed"
            self.save()
            return 0
        except BaseException as error:
            try:
                self.fail(f"{type(error).__name__}: {error}")
            finally:
                self.cancel()
            raise


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--test-threads", default=os.environ.get("HERDR_NEXTEST_JOBS", "4"))
    parser.add_argument("--filterset")
    args = parser.parse_args(argv)
    try:
        workers = allocation(args.test_threads, os.environ.get("HERDR_NEXTEST_SHARDS", "1"))
        run = NextestRun(sum(workers), len(workers), args.filterset)
        previous = {sig: signal.getsignal(sig) for sig in (signal.SIGINT, signal.SIGTERM)}
        def interrupted(signum, frame):
            if not run.cancelled.is_set():
                raise KeyboardInterrupt(f"received signal {signum}")
        for sig in previous:
            signal.signal(sig, interrupted)
        try:
            return run.run()
        finally:
            for sig, handler in previous.items():
                signal.signal(sig, handler)
    except KeyboardInterrupt:
        print("nextest cancelled", file=sys.stderr, flush=True)
        return 130
    except Exception as error:
        print(f"nextest failed: {error}", file=sys.stderr, flush=True)
        return 4 if isinstance(error, NoTestsError) else 2


if __name__ == "__main__":
    sys.exit(main())
