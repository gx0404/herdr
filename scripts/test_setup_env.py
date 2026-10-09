from __future__ import annotations

import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
import zipfile

from scripts import setup_env as setup


class SetupEnvTests(unittest.TestCase):
    def setUp(self):
        temporary = setup.ROOT / "target/tmp"
        temporary.mkdir(parents=True, exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(prefix="setup env 中文 ", dir=temporary)
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "1.96.1"\n', encoding="utf-8")
        cache_keys = {key.upper() for key in setup.CACHE_PATHS}
        self.inherited = {key: value for key, value in os.environ.items()
                          if key.upper() not in cache_keys and not key.startswith("HERDR_")
                          and key.upper() not in ("ZIG", "BASH_ENV", "PSMODULEANALYSISCACHEPATH")}
        self.inherited.update(BASH_ENV="/dev/null", PSModuleAnalysisCachePath=os.devnull)
        self.env = setup.environment(self.root, self.inherited)

    def test_environment_is_read_only_local_and_idempotent(self):
        before = list(self.root.rglob("*"))
        again = setup.environment(self.root, self.env)
        self.assertEqual(self.env, again)
        self.assertEqual(before, list(self.root.rglob("*")))
        self.assertNotIn("CARGO_BUILD_BUILD_DIR", self.env)
        for key in (*setup.CACHE_PATHS, "TEMP", "TMP", "TMPDIR"):
            if key in self.env:
                self.assertTrue(Path(self.env[key]).is_relative_to(self.root), key)
        self.assertEqual(self.env["RUSTUP_AUTO_INSTALL"], "0")
        self.assertEqual(self.env.get("HOME"), self.inherited.get("HOME"))
        self.assertEqual(self.env["BASH_ENV"], "/dev/null")
        self.assertEqual(self.env["PSModuleAnalysisCachePath"], os.devnull)

    def test_shell_startup_overrides_and_guard_materialization(self):
        env = setup.environment(self.root, {**self.inherited, "BASH_ENV": str(self.root / "caller.sh"),
                                           "PSMODULEANALYSISCACHEPATH": str(self.root / "caller-cache")})
        self.assertEqual(env["BASH_ENV"], "/dev/null")
        self.assertEqual(env["PSModuleAnalysisCachePath"], os.devnull)
        self.assertNotIn("PSMODULEANALYSISCACHEPATH", env)
        before = list(self.root.rglob("*"))
        setup.shell_environment(self.root, env, writable=False)
        self.assertEqual(before, list(self.root.rglob("*")))
        with self.assertRaisesRegex(setup.SetupError, "temporary directory is unavailable"):
            setup.shell_environment(self.root, env, writable=True)
        self.assertEqual(before, list(self.root.rglob("*")))
        setup.prepare_directories(self.root, env)
        guard = Path(env["BASH_ENV"])
        self.assertEqual(guard.read_text(encoding="utf-8"), setup.BASH_GUARD)
        self.assertEqual(guard, self.root / ".local/bash-env.sh")
        self.assertEqual(Path(env["PSModuleAnalysisCachePath"]), self.root / ".local/cache/powershell/ModuleAnalysisCache")
        timestamp = guard.stat().st_mtime_ns
        setup.shell_environment(self.root, env, writable=True)
        self.assertEqual(guard.stat().st_mtime_ns, timestamp)
        with self.assertRaises(setup.SetupError):
            setup.shell_environment(self.root, env, writable=True, directory=self.root.parent)

    def test_read_only_probes_disable_inherited_startup_and_module_cache(self):
        env = {**self.env, "BASH_ENV": "caller.sh", "PSModuleAnalysisCachePath": "caller-cache"}
        with mock.patch.object(setup.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "ok", "")) as run:
            setup.probe(["powershell", "--version"], env, self.root)
        child = run.call_args.kwargs["env"]
        self.assertEqual(child["BASH_ENV"], "/dev/null")
        self.assertEqual(child["PSModuleAnalysisCachePath"], os.devnull)
        self.assertEqual(env["BASH_ENV"], "caller.sh")
        self.assertFalse((self.root / ".local").exists())

    def test_external_empty_and_case_variant_cache_overrides_rejected(self):
        for key in (*setup.CACHE_PATHS, "HERDR_ZIG_HOME", "HERDR_WINDOWS_CROSS_ROOT",
                    "CARGO_INSTALL_ROOT", "CARGO_BUILD_TARGET_DIR", "BUN_INSTALL_GLOBAL_DIR", "BUN_INSTALL_BIN"):
            for value in (str(self.root.parent), "", "../outside"):
                with self.subTest(key=key, value=value), self.assertRaises(setup.SetupError):
                    setup.environment(self.root, {key: value})
        with self.assertRaises(setup.SetupError):
            setup.environment(self.root, {"NPM_CONFIG_CACHE": str(self.root.parent)})
        with self.assertRaises(setup.SetupError):
            setup.environment(self.root, {"CARGO_HOME": "some-other-local-home"})
        env = setup.environment(self.root, {"CARGO_TARGET_DIR": "target/custom", "TEMP": "C:/system-temp"})
        self.assertEqual(env["CARGO_TARGET_DIR"], str(self.root / "target/custom"))
        self.assertEqual(env["TEMP"], str(self.root / "target/tmp"))

    def test_explicit_zig_never_falls_back(self):
        for value in ("", "missing/zig.exe"):
            env = {**self.env, "ZIG": value}
            if value:
                self.assertEqual(setup.zig_path(self.root, env), str(self.root / value))
            else:
                with self.assertRaises(setup.SetupError):
                    setup.zig_path(self.root, env)
        with mock.patch.object(setup, "probe", side_effect=setup.SetupError("bad zig")), \
                mock.patch.object(setup, "prepare_directories") as mkdir:
            with self.assertRaises(setup.SetupError):
                setup.install(self.root, {**self.env, "ZIG": "missing/zig"}, False)
            mkdir.assert_not_called()

    def test_probe_runs_real_program_and_checks_exit(self):
        self.assertEqual(setup.probe([sys.executable, "-B", "-c", "print('ok')"], self.env, self.root), "ok")
        for code in ("raise SystemExit(7)", "pass"):
            with self.assertRaises(setup.SetupError):
                setup.probe([sys.executable, "-B", "-c", code], self.env, self.root)
        with self.assertRaises(setup.SetupError):
            setup.probe([str(self.root / "missing")], self.env, self.root)

    def test_missing_required_tools_and_broken_probes_fail(self):
        for missing in ("node", "bun", "cargo-nextest", "rustfmt", "cargo-clippy", "just", "git"):
            def locate(name, env):
                if name == missing:
                    raise setup.SetupError("missing " + name)
                return name
            with self.subTest(missing=missing), mock.patch.object(setup, "locate", side_effect=locate), \
                    mock.patch.object(setup, "probe", return_value="0.16.0"), \
                    mock.patch.object(setup, "windows_sdk", return_value="SDK"):
                _, errors = setup.inspect(self.root, {**self.env, "ZIG": "zig"}, "", [])
                self.assertTrue(any(missing in error for error in errors))
        with mock.patch.object(setup, "locate", return_value="fake"), \
                mock.patch.object(setup, "probe", side_effect=setup.SetupError("broken")), \
                mock.patch.object(setup, "windows_sdk", return_value="SDK"):
            _, errors = setup.inspect(self.root, self.env, "1.96.1", [])
            self.assertGreater(len(errors), 8)

    def test_rustup_resolves_direct_toolchain_without_auto_install(self):
        rustc = self.root / "rustup/toolchains/1.96.1-host/bin/rustc.exe"
        rustc.parent.mkdir(parents=True)
        rustc.touch()
        with mock.patch.object(setup, "locate", return_value="rustup"), \
                mock.patch.object(setup, "probe", return_value=str(rustc)) as probe:
            with self.assertRaisesRegex(setup.SetupError, "rustfmt"):
                setup.rust_environment(self.root, self.env)
            for name in ("cargo", "rustfmt", "cargo-clippy"):
                (rustc.parent / (name + ".exe")).touch()
            self.assertEqual(setup.rust_environment(self.root, self.env), "1.96.1")
        self.assertEqual(probe.call_args.args[0], ["rustup", "which", "--toolchain", "1.96.1", "rustc"])
        self.assertEqual(self.env["RUSTUP_AUTO_INSTALL"], "0")
        self.assertEqual(self.env["RUSTUP_HOME"], str(self.root / "rustup"))
        self.assertEqual(self.env["PATH"].split(os.pathsep)[0], str(rustc.parent))

    def test_check_no_writes_and_no_installs(self):
        before = list(self.root.rglob("*"))
        with mock.patch.object(setup, "ROOT", self.root), mock.patch.dict(os.environ, self.inherited, clear=True), \
                mock.patch.object(setup, "configure", return_value=("1.96.1", [])), \
                mock.patch.object(setup, "inspect", return_value=(["OK fake"], [])), \
                mock.patch.object(setup, "install") as install, \
                mock.patch.object(setup, "prepare_directories") as mkdir, \
                mock.patch.object(setup, "write_activators") as write, \
                mock.patch.object(setup.urllib.request, "urlopen") as download, contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(setup.main(["--check"]), 0)
        for action in (install, mkdir, write, download):
            action.assert_not_called()
        self.assertEqual(before, list(self.root.rglob("*")))

    def test_invalid_cli_rejected_before_install(self):
        for args in (["--bogus"], ["--check", "--force"], ["--check", "--install"], ["--run"], ["--force", "--run", "foo"]):
            with self.subTest(args=args), mock.patch.object(setup, "install") as install, \
                    contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as error:
                setup.main(args)
            self.assertEqual(error.exception.code, 2)
            install.assert_not_called()

    def test_run_has_zero_install_and_preserves_exit(self):
        with mock.patch.object(setup, "ROOT", self.root), mock.patch.dict(os.environ, self.inherited, clear=True), \
                mock.patch.object(setup, "configure", return_value=("1.96.1", [])), \
                mock.patch.object(setup, "inspect", return_value=([], [])), \
                mock.patch.object(setup, "install") as install, \
                mock.patch.object(setup, "run_command", return_value=23) as run:
            self.assertEqual(setup.main(["--run", "program", "a b", "x&y", "--flag"]), 23)
        install.assert_not_called()
        self.assertEqual(run.call_args.args[2], ["program", "a b", "x&y", "--flag"])

    def test_actual_child_argv_temp_home_cache_and_exit(self):
        output = self.root / "child.json"
        arguments = ["a b", "中文", "&|;$(no)", 'a"b', ""]
        script = ("import json,os,sys,tempfile; from pathlib import Path; "
                  "Path(sys.argv[1]).write_text(json.dumps({'argv':sys.argv[2:],'env':"
                  "{k:os.environ.get(k) for k in " + repr(list(setup.CACHE_PATHS) + list(setup.HERDR_OVERRIDES) +
                  ["HOME", "USERPROFILE", "XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_DATA_HOME", "XDG_RUNTIME_DIR", "APPDATA", "LOCALAPPDATA", "TEMP", "TMP", "TMPDIR", "BASH_ENV", "PSModuleAnalysisCachePath", "HERDR_SETUP_TMPDIR"]) +
                  "},'temp':tempfile.gettempdir(),'cwd':os.getcwd()}),encoding='utf-8'); sys.exit(23)")
        env = {**self.env, **{key: "inherited-not-local" for key in setup.HERDR_OVERRIDES}}
        self.assertEqual(setup.run_command(self.root, env, ["python3", "-B", "-c", script, str(output), *arguments]), 23)
        data = json.loads(output.read_text(encoding="utf-8"))
        self.assertEqual(data["argv"], arguments)
        self.assertEqual(Path(data["cwd"]), self.root)
        self.assertTrue(Path(data["temp"]).is_relative_to(self.root / "target/tmp"))
        for key, value in data["env"].items():
            if (key in setup.HERDR_OVERRIDES and key != "HERDR_HOME") or key == "CARGO_BUILD_BUILD_DIR":
                self.assertIsNone(value, key)
            else:
                self.assertTrue(Path(value).is_relative_to(self.root), key)
        self.assertFalse(Path(data["temp"]).exists())

    def test_real_bash_nested_python_and_powershell_temporary_files(self):
        bash = str(setup.native_bash(self.env)) if setup.WINDOWS else setup.locate("bash", self.env)
        caller = self.root / "caller.sh"
        marker = self.root / "caller-was-sourced"
        caller.write_text('printf sourced > "$TEST_MARKER"\n', encoding="utf-8")
        python_probe = self.root / "temp probe.py"
        python_probe.write_text(
            "import json,os,sys,tempfile\nfrom pathlib import Path\n"
            "assert Path(sys.executable) == Path(os.environ['TEST_PYTHON'])\n"
            "with tempfile.NamedTemporaryFile() as f:\n"
            "    result={'python':f.name,'mktemp':os.environ['TEST_MKTEMP'],"
            "'cache':os.environ['PSModuleAnalysisCachePath'],'guard':os.environ['BASH_ENV']}\n"
            "    Path(sys.argv[1]).write_text(json.dumps(result),encoding='utf-8')\n", encoding="utf-8")
        shell_probe = self.root / "temp probe.sh"
        shell_probe.write_text(
            'set -eu\nfile="$(mktemp)"\nexport TEST_MKTEMP="$file"\n'
            'python -B "$TEST_PROBE" "$1"\nrm -- "$file"\n', encoding="utf-8")
        env = {**self.env, "BASH_ENV": caller.as_posix(), "PSModuleAnalysisCachePath": str(self.root / "caller-cache"),
               "TEST_MARKER": marker.as_posix(), "TEST_PYTHON": Path(sys.executable).as_posix(),
               "TEST_PROBE": python_probe.as_posix(), "TEST_SHELL": shell_probe.as_posix(),
               "TEST_BASH": Path(bash).as_posix()}
        outer = self.root / "outer.json"
        nested = self.root / "nested.json"
        command = '. "$TEST_SHELL" "$1"; "$TEST_BASH" --noprofile --norc "$TEST_SHELL" "$2"'
        if setup.WINDOWS:
            ps_probe = self.root / "temp probe.ps1"
            ps_probe.write_text(
                'param([string]$Output)\n$ErrorActionPreference="Stop"\n'
                '$p=[IO.Path]::GetTempFileName()\ntry {\n'
                '    @{temp=$p;cache=$env:PSModuleAnalysisCachePath;guard=$env:BASH_ENV} | '
                'ConvertTo-Json -Compress | Set-Content -LiteralPath $Output -Encoding UTF8\n'
                '} finally { [IO.File]::Delete($p) }\n', encoding="utf-8")
            env["TEST_POWERSHELL"] = Path(setup.locate("powershell", self.env)).as_posix()
            env["TEST_PS_PROBE"] = ps_probe.as_posix()
            command += '; "$TEST_POWERSHELL" -NoProfile -NonInteractive -File "$TEST_PS_PROBE" "$3"'
        powershell_output = self.root / "powershell.json"
        argv = [bash, "--noprofile", "--norc", "-c", command, "test", outer.as_posix(), nested.as_posix(), powershell_output.as_posix()]
        self.assertEqual(setup.run_command(self.root, env, argv), 0)
        self.assertFalse(marker.exists())
        self.assertFalse((self.root / "caller-cache").exists())
        for output in (outer, nested):
            for key, value in json.loads(output.read_text(encoding="utf-8")).items():
                self.assertTrue(Path(value).is_relative_to(self.root / "target/tmp"), (key, value))
        if setup.WINDOWS:
            for key, value in json.loads(powershell_output.read_text(encoding="utf-8-sig")).items():
                self.assertTrue(Path(value).is_relative_to(self.root / "target/tmp"), (key, value))
            direct = self.root / "powershell-direct.json"
            self.assertEqual(setup.run_command(self.root, env, [env["TEST_POWERSHELL"], "-NoProfile", "-NonInteractive",
                                                              "-File", str(ps_probe), str(direct)]), 0)
            for key, value in json.loads(direct.read_text(encoding="utf-8-sig")).items():
                self.assertTrue(Path(value).is_relative_to(self.root / "target/tmp"), (key, value))

    def test_run_scratch_directories_do_not_discover_outer_repository(self):
        git = setup.locate("git", self.env)
        subprocess.run([git, "init", "--quiet", str(self.root)], env=self.env, check=True)
        script = (
            "import pathlib,subprocess,sys,tempfile\n"
            "git=sys.argv[1]\n"
            "def root(cwd):\n"
            "    return subprocess.run([git,'rev-parse','--show-toplevel'],cwd=cwd,capture_output=True,text=True,encoding='utf-8')\n"
            "assert pathlib.Path(root(pathlib.Path.cwd()).stdout.strip()).resolve()==pathlib.Path.cwd().resolve()\n"
            "with tempfile.TemporaryDirectory() as tmp:\n"
            "    assert root(tmp).returncode!=0, 'scratch directory inherited the outer repository'\n"
            "    subprocess.run([git,'init','--quiet',tmp],check=True)\n"
            "    assert pathlib.Path(root(tmp).stdout.strip()).resolve()==pathlib.Path(tmp).resolve()\n"
        )
        self.assertEqual(setup.run_command(self.root, self.env, [sys.executable, "-B", "-c", script, git]), 0)

    @unittest.skipUnless(setup.WINDOWS, "CP936 CMD forwarding is Windows-specific")
    def test_cmd_shim_uses_unicode_native_python_under_cp936(self):
        native = self.root / "解释器 中文/python.exe"
        native.parent.mkdir()
        shutil.copy2(sys.executable, native)
        for pattern in ("python*.dll", "vcruntime*.dll"):
            for dll in Path(sys.executable).parent.glob(pattern):
                shutil.copy2(dll, native.parent / dll.name)
        with mock.patch.object(setup.sys, "executable", str(native)):
            env = setup.environment(self.root, self.inherited)
            setup.prepare_directories(self.root, env)
        env["PYTHONHOME"] = sys.base_prefix
        shim = self.root / ".local/tool-shims/python3.cmd"
        self.assertTrue(shim.read_bytes().isascii())
        output = self.root / "shim result.json"
        script = self.root / "shim probe.py"
        script.write_text("import json,sys\nfrom pathlib import Path\nPath(sys.argv[1]).write_text(json.dumps([sys.executable,sys.argv[2:]]),encoding='utf-8')\n", encoding="utf-8")
        driver = self.root / "cp936.cmd"
        driver.write_text('@chcp 936 >nul\r\n@call "%TEST_SHIM%" "%TEST_SCRIPT%" "%TEST_OUTPUT%" "space arg" "%TEST_ARG%"\r\n', encoding="ascii", newline="")
        env.update(TEST_SHIM=str(shim), TEST_SCRIPT=str(script), TEST_OUTPUT=str(output), TEST_ARG="中文参数")
        command = [setup.locate("cmd", env), "/d", "/c", str(driver)]
        result = subprocess.run(command, env=env, cwd=self.root, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        executable, arguments = json.loads(output.read_text(encoding="utf-8"))
        self.assertEqual(Path(executable), native)
        self.assertEqual(arguments, ["space arg", "中文参数"])
        del env["HERDR_SETUP_PYTHON"]
        result = subprocess.run(command, env=env, cwd=self.root, capture_output=True)
        self.assertEqual(result.returncode, 1)

    def test_inspect_rejects_corrupt_canonical_and_legacy_archives(self):
        for directory in (".local/downloads", ".local/tools/downloads"):
            archive = self.root / directory / setup.PINS["just"][1].rsplit("/", 1)[1]
            archive.parent.mkdir(parents=True)
            archive.write_bytes(b"corrupt cached archive")
            with mock.patch.object(setup, "locate", side_effect=lambda name, env: str(self.root / name)), \
                    mock.patch.object(setup, "probe", return_value=setup.setup_zig.ZIG_VERSION), \
                    mock.patch.object(setup, "windows_sdk", return_value="SDK"):
                _, errors = setup.inspect(self.root, {**self.env, "ZIG": "zig"}, "", [])
            self.assertTrue(any("SHA256 mismatch" in error and str(archive) in error for error in errors), errors)
            archive.unlink()

    def test_windows_sdk_honors_nondefault_case_insensitive_program_files(self):
        base = self.root / "custom SDK root"
        installation = self.root / "custom VS root"
        vswhere = base / "Microsoft Visual Studio/Installer/vswhere.exe"
        linker = installation / "VC/Tools/MSVC/1/bin/Hostx64/x64/link.exe"
        for path in (vswhere, linker, base / "Windows Kits/10/Lib/1/um/x64/kernel32.lib",
                     base / "Windows Kits/10/Lib/1/ucrt/x64/ucrt.lib"):
            path.parent.mkdir(parents=True, exist_ok=True)
            path.touch()
        for key in ("PROGRAMFILES(X86)", "ProgramFiles(x86)"):
            with mock.patch.object(setup, "probe", side_effect=[str(installation), "headers"]) as probe:
                report = setup.windows_sdk(self.root, {key: str(base)})
            self.assertIn(str(linker), report)
            self.assertEqual(probe.call_args_list[0].args[0][0], str(vswhere))

    def test_bootstrap_skips_aliases_and_discovers_native_python_without_installing(self):
        scripts = self.root / "scripts"
        scripts.mkdir()
        wrapper = scripts / "setup_env.sh"
        shutil.copy2(setup.ROOT / "scripts/setup_env.sh", wrapper)
        aliases = self.root / "WindowsApps"
        aliases.mkdir()
        marker = self.root / "alias-called"
        for name in ("python", "python3"):
            path = aliases / name
            path.write_text('#!/usr/bin/env bash\n: > "$TEST_ALIAS_CALLED"\nexit 99\n', encoding="utf-8")
            path.chmod(0o755)
        bash = Path(setup.native_bash(self.env)) if setup.WINDOWS else Path(setup.locate("bash", self.env))
        tools = bash.parent.parent / "usr/bin" if setup.WINDOWS else bash.parent
        local = self.root / "local app data"
        output = self.root / "native-arguments"
        env = {**self.inherited, "PATH": os.pathsep.join((str(aliases), str(tools))),
               "LOCALAPPDATA": str(local), "USERPROFILE": str(self.root / "profile"),
               "TEST_ALIAS_CALLED": marker.as_posix(), "TEST_NATIVE_OUTPUT": output.as_posix()}
        env.pop("HERDR_SETUP_PYTHON", None)
        command = [str(bash), "--noprofile", "--norc", str(wrapper), "--check", "arg 中文"]
        result = subprocess.run(command, env=env, cwd=self.root, capture_output=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertFalse(marker.exists())
        native = local / "Python/pythoncore-test/python.exe"
        native.parent.mkdir(parents=True)
        native.write_text('#!/usr/bin/env bash\nif [[ "$*" == *sys.version_info* ]]; then exit 0; fi\nprintf "%s\\n" "$@" > "$TEST_NATIVE_OUTPUT"\nexit 23\n', encoding="utf-8")
        native.chmod(0o755)
        result = subprocess.run(command, env=env, cwd=self.root, capture_output=True)
        self.assertEqual(result.returncode, 23, result.stderr)
        self.assertFalse(marker.exists())
        self.assertEqual(output.read_text(encoding="utf-8").splitlines()[-2:], ["--check", "arg 中文"])
        env["HERDR_SETUP_PYTHON"] = str(aliases / "python")
        result = subprocess.run(command, env=env, cwd=self.root, capture_output=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertFalse(marker.exists())
        env["HERDR_SETUP_PYTHON"] = str(native)
        result = subprocess.run(command, env=env, cwd=self.root, capture_output=True)
        self.assertEqual(result.returncode, 23, result.stderr)

    def test_bash_bootstrap_avoids_bash4_only_expansions(self):
        source = (setup.ROOT / "scripts/setup_env.sh").read_text(encoding="utf-8")
        self.assertNotRegex(source, r"\$\{[^}]*,,[^}]*\}")
        self.assertNotRegex(source, r"\[\[\s+-v\b")
        self.assertIn("${HERDR_SETUP_PYTHON+x}", source)

    def powershell_wrapper(self):
        scripts = self.root / "scripts"
        scripts.mkdir()
        wrapper = scripts / "setup_env.ps1"
        shutil.copy2(setup.ROOT / "scripts/setup_env.ps1", wrapper)
        (scripts / "setup_env.py").write_text(
            "import json,os,sys\nwith open(os.environ['TEST_NATIVE_OUTPUT'],'w',encoding='utf-8') as f: json.dump(sys.argv[1:],f)\nraise SystemExit(23)\n", encoding="utf-8")
        env = {**self.inherited, "TEMP": str(self.root), "TMP": str(self.root), "TMPDIR": str(self.root),
               "TEST_NATIVE_OUTPUT": str(self.root / "wrapper-argv.json")}
        return wrapper, env

    @unittest.skipUnless(setup.WINDOWS, "Native PowerShell 5.1 wrapper is Windows-specific")
    def test_powershell_wrapper_preserves_array_arguments_and_exit(self):
        import base64
        wrapper, env = self.powershell_wrapper()
        env["HERDR_SETUP_PYTHON"] = sys.executable
        arguments = ["--check", "space 中文", 'quote"value', "trailing\\", "", "&|;$(no)"]
        quote = lambda value: "'" + value.replace("'", "''") + "'"
        source = "$forwarded=@(" + ",".join(quote(value) for value in arguments) + "); & " + quote(str(wrapper)) + " @forwarded; exit $LASTEXITCODE"
        command = [setup.locate("powershell", env), "-NoProfile", "-NonInteractive", "-EncodedCommand",
                   base64.b64encode(source.encode("utf-16-le")).decode("ascii")]
        result = subprocess.run(command, env=env, cwd=self.root, capture_output=True)
        self.assertEqual(result.returncode, 23, result.stderr)
        self.assertEqual(json.loads(Path(env["TEST_NATIVE_OUTPUT"]).read_text(encoding="utf-8")), arguments)

    @unittest.skipUnless(setup.WINDOWS, "Native PowerShell 5.1 wrapper is Windows-specific")
    def test_powershell_wrapper_skips_aliases_and_probes_minimum_native_version(self):
        wrapper, env = self.powershell_wrapper()
        powershell = setup.locate("powershell", env)
        aliases = self.root / "WindowsApps"
        local = self.root / "local app data"
        marker = self.root / "candidate-probed"
        env.update(PATH=str(aliases), LOCALAPPDATA=str(local), USERPROFILE=str(self.root / "profile"),
                   TEST_PROBE_MARKER=str(marker))
        env.pop("HERDR_SETUP_PYTHON", None)

        def interpreter(directory, version):
            directory.mkdir(parents=True, exist_ok=True)
            binary = directory / "python.exe"
            shutil.copy2(sys.executable, binary)
            for pattern in ("python*.dll", "vcruntime*.dll"):
                for dll in Path(sys.executable).parent.glob(pattern):
                    shutil.copy2(dll, directory / dll.name)
            paths = [str(Path(sys.base_prefix) / "Lib"), str(Path(sys.base_prefix) / "DLLs"), ".", "import site"]
            (directory / f"python{sys.version_info.major}{sys.version_info.minor}._pth").write_text("\n".join(paths), encoding="utf-8")
            (directory / "sitecustomize.py").write_text(
                "import os,sys\nwith open(os.environ['TEST_PROBE_MARKER'],'w') as f: f.write('probed')\n"
                f"if '-c' in sys.argv: sys.version_info={version!r}\n", encoding="utf-8")
            return binary

        alias = interpreter(aliases, (3, 14, 3))
        command = [powershell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", str(wrapper), "--check"]
        result = subprocess.run(command, env=env, cwd=self.root, capture_output=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertFalse(marker.exists())
        directory = local / "Python/pythoncore-fixture"
        interpreter(directory, (3, 10, 0))
        result = subprocess.run(command, env=env, cwd=self.root, capture_output=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertTrue(marker.exists())
        marker.unlink()
        interpreter(directory, (3, 11, 0))
        result = subprocess.run(command, env=env, cwd=self.root, capture_output=True)
        self.assertEqual(result.returncode, 23, result.stderr)
        self.assertTrue(marker.exists())
        marker.unlink()
        env["HERDR_SETUP_PYTHON"] = str(alias)
        result = subprocess.run(command, env=env, cwd=self.root, capture_output=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertFalse(marker.exists())

    def archive(self, entries):
        buffer = io.BytesIO()
        with zipfile.ZipFile(buffer, "w") as archive:
            for name, content in entries:
                archive.writestr(name, content)
        return buffer.getvalue()

    def test_zip_rejects_paths_and_links_before_extracting(self):
        for name in ("../escape", "/absolute", "C:/escape", "a\\..\\escape", "file:stream"):
            with self.subTest(name=name):
                archive = self.root / "unsafe.zip"
                archive.write_bytes(self.archive([("safe", "data"), (name, "bad")]))
                target = self.root / "extracted"
                with self.assertRaises(setup.SetupError):
                    setup.extract_zip(archive, target)
                self.assertFalse(target.exists())
        info = zipfile.ZipInfo("link")
        info.external_attr = (0o120777 << 16)
        archive.write_bytes(self.archive([(info, "../outside")]))
        with self.assertRaises(setup.SetupError):
            setup.extract_zip(archive, target)

    def test_download_checksum_staging_reuse_and_failed_force_preserve_old(self):
        data = self.archive([("just.exe", b"fake binary")])
        pin = ("1.58.0", "https://example.invalid/just.zip", hashlib.sha256(data).hexdigest(), "just.exe")
        with mock.patch.object(setup, "WINDOWS", True), mock.patch.object(setup.platform, "machine", return_value="AMD64"), \
                mock.patch.dict(setup.PINS, {"just": pin}), \
                mock.patch.object(setup.urllib.request, "urlopen", return_value=io.BytesIO(data)) as download, \
                mock.patch.object(setup, "probe", return_value="just 1.58.0"):
            setup.install_portable("just", self.root, self.env)
            setup.install_portable("just", self.root, self.env)
            self.assertEqual(download.call_count, 1)
        binary = self.root / ".local/tools/just/just.exe"
        self.assertEqual(binary.read_bytes(), b"fake binary")
        with mock.patch.object(setup, "WINDOWS", True), mock.patch.object(setup.platform, "machine", return_value="AMD64"), \
                mock.patch.dict(setup.PINS, {"just": pin}), mock.patch.object(setup, "probe", side_effect=setup.SetupError("broken")):
            with self.assertRaises(setup.SetupError):
                setup.install_portable("just", self.root, self.env)
        self.assertEqual(binary.read_bytes(), b"fake binary")
        pin = (*pin[:2], "0" * 64, pin[3])
        with mock.patch.object(setup, "WINDOWS", True), mock.patch.object(setup.platform, "machine", return_value="AMD64"), \
                mock.patch.dict(setup.PINS, {"just": pin}), mock.patch.object(setup.urllib.request, "urlopen", return_value=io.BytesIO(data)):
            with self.assertRaisesRegex(setup.SetupError, "SHA256"):
                setup.install_portable("just", self.root, self.env)
        self.assertEqual(binary.read_bytes(), b"fake binary")

    def test_failed_swap_and_failed_rollback_keep_original_backup(self):
        data = self.archive([("just.exe", b"new")])
        pin = ("1.58.0", "https://example.invalid/just.zip", hashlib.sha256(data).hexdigest(), "just.exe")
        binary = self.root / ".local/tools/just/just.exe"
        binary.parent.mkdir(parents=True)
        binary.write_bytes(b"original")
        original_rename = Path.rename

        def rename(path, destination):
            if path.name == "new" or path.name.startswith(".just-backup-"):
                raise OSError("injected rename failure")
            return original_rename(path, destination)

        with mock.patch.object(setup, "WINDOWS", True), mock.patch.object(setup.platform, "machine", return_value="AMD64"), \
                mock.patch.dict(setup.PINS, {"just": pin}), \
                mock.patch.object(setup.urllib.request, "urlopen", return_value=io.BytesIO(data)), \
                mock.patch.object(setup, "probe", return_value="just 1.58.0"), mock.patch.object(Path, "rename", rename):
            with self.assertRaisesRegex(setup.SetupError, "original preserved"):
                setup.install_portable("just", self.root, self.env)
        backups = list(binary.parent.parent.glob(".just-backup-*/just.exe"))
        self.assertEqual(len(backups), 1)
        self.assertEqual(backups[0].read_bytes(), b"original")

    def test_default_install_reuses_external_tools_but_installs_local_zig(self):
        from scripts import setup_zig
        with mock.patch.object(setup.shutil, "which", return_value=str(self.root.parent / "external.exe")), \
                mock.patch.object(setup, "probe", return_value="1.0"), \
                mock.patch.object(setup, "install_portable") as portable, \
                mock.patch.object(setup_zig, "install", return_value=0) as zig:
            setup.install(self.root, self.env, True)
        portable.assert_not_called()
        zig.assert_called_once_with(True)

    def test_activators_relocate_quote_and_are_idempotent(self):
        scripts = self.root / "scripts"
        scripts.mkdir()
        (scripts / "setup_env.py").write_text(
            "import os,sys\nfrom pathlib import Path\nfrom scripts import setup_env as s\n"
            "sys.stdout.reconfigure(encoding='utf-8')\n"
            "root=Path(__file__).resolve().parents[1]\n"
            "env=s.environment(root,dict(os.environ))\n"
            "s.prepare_directories(root,env)\n"
            "print(s.exports(env,dict(os.environ),sys.argv[-1]))\n", encoding="utf-8")
        setup.write_activators(self.root)
        moved = self.root / "moved ' 中文 root"
        moved.mkdir()
        shutil.move(str(self.root / ".local"), moved)
        shutil.move(str(scripts), moved)
        env = dict(self.inherited)
        env["PYTHONPATH"] = str(setup.ROOT)
        env["PYTHONDONTWRITEBYTECODE"] = "1"
        env["TEMP"] = env["TMP"] = env["TMPDIR"] = str(self.root)
        bash = str(setup.native_bash(env)) if setup.WINDOWS else setup.locate("bash", env)
        activation = setup.bash_path(str(moved / ".local/activate.sh"))
        script = 'source "$1" || exit; first="$PATH"; source "$1" || exit; [[ "$first" == "$PATH" ]] || exit 7; [[ "$CARGO_HOME" == "$2" ]] || exit 8'
        result = subprocess.run([bash, "--noprofile", "--norc", "-c", script, "test", activation, str(moved / ".local/cargo-home")],
                                env=env, cwd=self.root, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        if setup.WINDOWS:
            ps = setup.locate("powershell", env)
            activation = str(moved / ".local/activate.ps1").replace("'", "''")
            expected = str(moved / ".local/cargo-home").replace("'", "''")
            command = f". '{activation}'; $first=$env:PATH; . '{activation}'; if($first -cne $env:PATH){{exit 7}}; if($env:CARGO_HOME -cne '{expected}'){{exit 8}}"
            result = subprocess.run([ps, "-NoProfile", "-NonInteractive", "-Command", command], env=env, cwd=self.root, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
