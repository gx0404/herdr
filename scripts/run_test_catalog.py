"""Execute a fixed, reviewed suite; new tests default to inclusion, never cached success.

The TOML catalog contains exclusions with reasons, not an allowlist. Python discovery
uses test_*.py, Bun discovery uses *.test.ts. Full mode restores every extended item.
"""
from __future__ import annotations

import argparse
import fnmatch
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
import unittest

try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib

ROOT = Path(__file__).resolve().parents[1]
CATALOG = ROOT / ".config/test-suites.toml"
sys.path.insert(0, str(ROOT))


def load_catalog(path: Path = CATALOG) -> dict:
    data = tomllib.loads(path.read_text(encoding="utf-8"))
    allowed = {"version", "python_extended", "rust_extended", "rust_isolated", "bun_extended"}
    if data.get("version") != 1 or set(data) - allowed:
        raise ValueError("unsupported test catalog schema")
    for section in allowed - {"version"}:
        seen = set()
        for entry in data.get(section, []):
            if set(entry) - {"selector", "reason", "platform"} or not all(
                isinstance(entry.get(key), str) and entry[key].strip() for key in ("selector", "reason")
            ):
                raise ValueError(f"invalid {section} entry: selector and reason required")
            if "platform" in entry and (section not in {"rust_extended", "rust_isolated"}
                                        or entry["platform"] not in {"windows", "unix"}):
                raise ValueError("invalid test platform")
            if entry["selector"] in seen:
                raise ValueError(f"duplicate selector: {entry['selector']}")
            seen.add(entry["selector"])
    return data


def applies(entry: dict) -> bool:
    return "platform" not in entry or entry["platform"] == ("windows" if os.name == "nt" else "unix")


def native_skip_is_exact(names: list[str], skipped: list[str]) -> bool:
    """libtest --skip is a substring match; never silently exclude a new sibling."""
    intended = set(skipped)
    return all(name in intended or not any(skip in name for skip in skipped) for name in names)


def partition(names: list[str], entries: list[dict], suite: str, *, prefix: bool = False) -> tuple[list[str], list[str]]:
    """Validate each exclusion even in full mode; stale selectors are errors."""
    excluded = set()
    for entry in entries:
        selector = entry["selector"]
        matches = {name for name in names if fnmatch.fnmatchcase(name, selector)
                   or (prefix and name.startswith(selector + "."))}
        if not matches:
            raise ValueError(f"stale test selector: {selector}")
        excluded.update(matches)
    selected = names if suite == "full" else [name for name in names if name not in excluded]
    if not selected:
        raise ValueError("empty test selection")
    return selected, sorted(excluded) if suite != "full" else []


def python_tests(data: dict, suite: str) -> tuple[list[str], list[str]]:
    from scripts.run_parallel_unittest import iter_test_cases, _is_load_failure
    names = []
    for path in sorted((ROOT / "scripts").glob("test_*.py")):
        # This phase is independently scheduled by the outer orchestrator.
        if path.stem == "test_ui_hot_path_architecture":
            continue
        loaded = list(iter_test_cases(unittest.defaultTestLoader.loadTestsFromName("scripts." + path.stem)))
        if not loaded or any(_is_load_failure(case) for case in loaded):
            raise ValueError(f"cannot collect tests from {path.name}")
        names.extend(case.id() for case in loaded)
    return partition(names, data.get("python_extended", []), suite, prefix=True)


def bun_files(phase: str) -> list[str]:
    roots = [ROOT / "src/integration/assets"] if phase == "integration-assets" else [ROOT / "scripts/docs"]
    if phase == "maintenance-bun":
        return [path.relative_to(ROOT).as_posix() for path in sorted((ROOT / "scripts").glob("*.test.ts"))]
    return [path.relative_to(ROOT).as_posix() for root in roots for path in sorted(root.rglob("*.test.ts"))]


def run_bun(phase: str, data: dict, suite: str) -> int:
    files = bun_files(phase)
    if not files:
        raise ValueError(f"empty Bun phase: {phase}")
    entries = data.get("bun_extended", [])
    # Selectors use file::exact test title. Negative title matching retains new tests.
    for entry in entries:
        path, separator, title = entry["selector"].partition("::")
        if not separator or not (ROOT / path).is_file() or title not in (ROOT / path).read_text(encoding="utf-8"):
            raise ValueError(f"stale Bun selector: {entry['selector']}")
    for path in files:
        excluded = [entry["selector"].split("::", 1)[1] for entry in entries
                    if entry["selector"].startswith(path + "::") and suite != "full"]
        command = ["bun", "test", "./" + path]
        if excluded:
            expression = "^(?!(?:" + "|".join(re.escape(title) for title in excluded) + ")$)"
            command += ["--test-name-pattern", expression]
            print(f"extended tests deferred in {path}: {len(excluded)}", flush=True)
        process = subprocess.Popen(command, cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                   encoding="utf-8", errors="replace")
        output = []
        assert process.stdout is not None
        with process.stdout:
            for line in process.stdout:
                output.append(line)
                print(line, end="", flush=True)
        code = process.wait()
        if code:
            return code
        if not re.search(r"Ran [1-9][0-9]* tests? across", "".join(output)):
            raise ValueError(f"empty or unreported Bun execution: {path}")
    return 0


def run_rust(data: dict, suite: str) -> int:
    from scripts import run_nextest
    jobs = int(os.environ.get("HERDR_NEXTEST_JOBS", "4"))
    filterset = os.environ.get("HERDR_TEST_FILTER", "all()")
    entries = data.get("rust_extended", [])
    binary_names = {path.stem for path in (ROOT / "tests").glob("*.rs")}
    deferred_binaries = set()
    deferred_tests = []
    for entry in entries:
        kind, separator, name = entry["selector"].partition(":")
        if not separator or kind not in {"binary", "test"}:
            raise ValueError(f"invalid Rust selector: {entry['selector']}")
        if kind == "binary":
            if name not in binary_names:
                raise ValueError(f"stale Rust binary selector: {name}")
            deferred_binaries.add(name)
        else:
            deferred_tests.append(name)
    isolated = [entry["selector"] for entry in data.get("rust_isolated", []) if applies(entry)]
    if set(isolated) & set(deferred_tests):
        raise ValueError("isolated default test cannot also be extended")
    source = "\n".join(path.read_text(encoding="utf-8") for path in (ROOT / "src").rglob("*.rs"))
    for name in [*deferred_tests, *isolated]:
        if not re.fullmatch(r"[A-Za-z_][A-Za-z_0-9]*(?:::[A-Za-z_][A-Za-z_0-9]*)+", name):
            raise ValueError(f"invalid qualified Rust test selector: {name}")
        if not re.search(r"\bfn\s+" + re.escape(name.rsplit("::", 1)[-1]) + r"\s*\(", source):
            raise ValueError(f"stale Rust test selector: {name}")
    run = run_nextest.NextestRun(jobs, 1)
    try:
        run.manifest["suite"] = suite
        run.manifest["status"] = "running"
        listing = run.directory / "unit-tests.txt"
        # Cargo's native harness avoids thousands of process launches. Process-wide
        # resource/identity assertions are explicitly rerun with nextest below.
        common = ["cargo", "test", "--locked", "--workspace", "--lib", "--bins"]
        if run.command("unit-discovery", [*common, "--", "--list"], output=listing):
            raise RuntimeError("Rust unit discovery failed")
        names = [line[:-6] for line in listing.read_text(encoding="utf-8").splitlines() if line.endswith(": test")]
        if not names:
            raise ValueError("empty Rust unit discovery")
        # Some qualifications are target-gated. Validate their symbol against source
        # as well, so an unavailable target is distinct from a misspelled selector.
        applicable_tests = [entry["selector"][5:] for entry in entries
                            if entry["selector"].startswith("test:") and applies(entry)]
        for name in [*applicable_tests, *isolated]:
            if name not in names:
                raise ValueError(f"stale Rust test selector: {name}")
        if suite == "full":
            code = run_nextest.main(["--filterset", filterset])
            run.manifest["status"] = "passed" if code == 0 else "failed"
            run.save()
            return code
        run.manifest["extended_binaries"] = sorted(deferred_binaries)
        run.manifest["extended_tests"] = [name for name in deferred_tests if name in names]
        run.manifest["isolated_tests"] = [name for name in isolated if name in names]
        run.save()
        if (filterset not in {"all()", "not binary(live_handoff)"}
                or not native_skip_is_exact(names, [*deferred_tests, *isolated])):
            print("Using exact nextest selection for this filter/test inventory", flush=True)
            exclusions = [f"binary(={name})" for name in sorted(deferred_binaries)]
            exclusions += [f"test(={name})" for name in deferred_tests]
            code = run_nextest.main(["--filterset", f"({filterset}) & not (" + " | ".join(exclusions) + ")"])
            run.manifest["status"] = "passed" if code == 0 else "failed"
            run.save()
            return code
        command = [*common, "--quiet", "--", "--test-threads", str(jobs)]
        for name in [*deferred_tests, *isolated]:
            command += ["--skip", name]
        code = run.command("unit-tests", command)
        if code:
            run.fail("unit regression failed")
            return code
        selected_isolated = [name for name in isolated if name in names]
        if selected_isolated:
            expression = " | ".join(f"test(={name})" for name in selected_isolated)
            code = run.command("isolated-tests", ["cargo", "nextest", "run", "--locked", "--lib", "--bins",
                "--test-threads", str(jobs), "-E", expression, *run_nextest.REPORT_ARGS, "--no-tests", "fail"])
            if code:
                run.fail("isolated regression failed")
                return code
        # Known real-process integration binaries are deferred; new binaries run by default.
        new_binaries = sorted(binary_names - deferred_binaries)
        if new_binaries:
            command = ["cargo", "nextest", "run", "--locked", "--test-threads", str(jobs)]
            for name in new_binaries:
                command += ["--test", name]
            code = run.command("integration-tests", [*command, "-E", filterset, *run_nextest.REPORT_ARGS, "--no-tests", "fail"])
            if code:
                run.fail("integration regression failed")
                return code
        run.manifest["status"] = "passed"
        native_output = (run.directory / "unit-tests.log").read_text(encoding="utf-8")
        native_counts = re.findall(r"test result: ok\. (\d+) passed; 0 failed; (\d+) ignored", native_output)
        if not native_counts or sum(int(row[0]) for row in native_counts) == 0:
            raise ValueError("missing Rust execution counts")
        executed = sum(int(row[0]) for row in native_counts) + len(selected_isolated)
        ignored = sum(int(row[1]) for row in native_counts)
        if new_binaries:
            extra = (run.directory / "integration-tests.log").read_text(encoding="utf-8")
            match = re.search(r"Summary .*? (\d+) tests run:", extra)
            if not match:
                raise ValueError("missing integration execution counts")
            executed += int(match[1])
        run.manifest["counts"] = {"executed": executed, "skipped": ignored,
                                  "extended_unit_tests": len(run.manifest["extended_tests"]),
                                  "extended_binaries": len(deferred_binaries)}
        run.save()
        print("HERDR_TEST_COUNTS " + json.dumps(run.manifest["counts"]), flush=True)
        print(f"Rust evidence: {run.manifest_path}", flush=True)
        return 0
    except BaseException as error:
        run.fail(f"{type(error).__name__}: {error}")
        run.cancel()
        raise


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("phase", choices=("maintenance", "integration-assets", "docs-contract", "rust"))
    parser.add_argument("--suite", choices=("default", "full"), default=os.environ.get("HERDR_TEST_SUITE", "default"))
    args = parser.parse_args(argv)
    try:
        data = load_catalog()
        if args.phase == "maintenance":
            from scripts import run_parallel_unittest as runner
            names, deferred = python_tests(data, args.suite)
            print(f"Python suite={args.suite}: selected={len(names)}, extended_deferred={len(deferred)}", flush=True)
            # Group by class, retaining isolation while avoiding one interpreter per test.
            by_class: dict[str, list[str]] = {}
            for name in names:
                by_class.setdefault(name.rsplit(".", 1)[0], []).append(name)
            durations = runner.load_durations()
            units = []
            for label, items in by_class.items():
                chunks = runner.split_by_duration(items, durations) if args.suite == "full" else [items]
                for index, chunk in enumerate(chunks):
                    units.append(runner.Unit(f"{label}[{index + 1}/{len(chunks)}]", tuple(chunk), tuple(chunk)))
            jobs = runner.resolve_jobs()
            started = time.monotonic()
            results = runner.run_units(runner.order_units(units, durations), jobs)
            code, lines = runner.summarize(results, time.monotonic() - started, jobs)
            runner.record_durations(results)
            print("\n".join(lines), flush=True)
            if code:
                return code
            code = run_bun("maintenance-bun", data, args.suite)
            return code or subprocess.call([sys.executable, "-B", "scripts/upstream_sync_drop_check.py"], cwd=ROOT)
        if args.phase == "rust":
            return run_rust(data, args.suite)
        return run_bun(args.phase, data, args.suite)
    except (OSError, ValueError, RuntimeError) as error:
        print(f"test catalog failed: {error}", file=sys.stderr, flush=True)
        return 2


if __name__ == "__main__":
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8", errors="replace")
    raise SystemExit(main())
