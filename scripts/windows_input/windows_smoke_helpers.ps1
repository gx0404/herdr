function Initialize-SmokeNative {
    if ('HerdrSmoke.SmokeJob' -as [type]) { return }
    Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Text;
using Microsoft.Win32.SafeHandles;

namespace HerdrSmoke {
    internal static class Native {
        internal const uint CREATE_SUSPENDED = 4, CREATE_NO_WINDOW = 0x08000000;
        internal const uint KILL_ON_JOB_CLOSE = 0x2000;
        [StructLayout(LayoutKind.Sequential)] internal struct BasicLimit {
            public long ProcessTime, JobTime;
            public uint Flags;
            public UIntPtr MinWorking, MaxWorking;
            public uint ProcessLimit;
            public UIntPtr Affinity;
            public uint Priority, Scheduling;
        }
        [StructLayout(LayoutKind.Sequential)] internal struct ExtendedLimit {
            public BasicLimit Basic;
            public ulong ReadOps, WriteOps, OtherOps, ReadBytes, WriteBytes, OtherBytes;
            public UIntPtr ProcessMemory, JobMemory, PeakProcessMemory, PeakJobMemory;
        }
        [StructLayout(LayoutKind.Sequential)] internal struct Accounting {
            public long UserTime, KernelTime, PeriodUserTime, PeriodKernelTime;
            public uint PageFaults, TotalProcesses, ActiveProcesses, TerminatedProcesses;
        }
        [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)] internal struct Startup {
            public uint Size;
            public string Reserved, Desktop, Title;
            public uint X, Y, Width, Height, XChars, YChars, Fill, Flags;
            public ushort Show, ReservedSize;
            public IntPtr ReservedPointer, Input, Output, Error;
        }
        [StructLayout(LayoutKind.Sequential)] internal struct ProcessInfo {
            public IntPtr Process, Thread;
            public uint ProcessId, ThreadId;
        }
        [StructLayout(LayoutKind.Sequential)] internal struct Security {
            public int Length;
            public IntPtr Descriptor;
            [MarshalAs(UnmanagedType.Bool)] public bool Inherit;
        }
        [StructLayout(LayoutKind.Sequential)] internal struct SystemInfo {
            public ushort Architecture, Reserved;
            public uint PageSize;
            public IntPtr MinimumAddress, MaximumAddress;
            public UIntPtr ActiveMask;
            public uint ProcessorCount, ProcessorType, Granularity;
            public ushort Level, Revision;
        }
        [DllImport("kernel32.dll")]
        internal static extern void GetNativeSystemInfo(out SystemInfo info);
        [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
        internal static extern IntPtr CreateJobObjectW(IntPtr attributes, string name);
        [DllImport("kernel32.dll", SetLastError = true)]
        internal static extern bool SetInformationJobObject(SafeFileHandle job, int kind, ref ExtendedLimit info, int size);
        [DllImport("kernel32.dll", SetLastError = true)]
        internal static extern bool QueryInformationJobObject(SafeFileHandle job, int kind, out Accounting info, int size, IntPtr returned);
        [DllImport("kernel32.dll", SetLastError = true)]
        internal static extern bool AssignProcessToJobObject(SafeFileHandle job, IntPtr process);
        [DllImport("kernel32.dll", SetLastError = true)]
        internal static extern bool TerminateJobObject(SafeFileHandle job, uint exitCode);
        [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
        internal static extern bool CreateProcessW(string application, StringBuilder command, IntPtr processAttributes,
            IntPtr threadAttributes, bool inherit, uint flags, IntPtr environment, string directory,
            ref Startup startup, out ProcessInfo process);
        [DllImport("kernel32.dll", SetLastError = true)]
        internal static extern uint ResumeThread(IntPtr thread);
        [DllImport("kernel32.dll", SetLastError = true)]
        internal static extern bool TerminateProcess(IntPtr process, uint exitCode);
        [DllImport("kernel32.dll", SetLastError = true)]
        internal static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);
        [DllImport("kernel32.dll", SetLastError = true)]
        internal static extern uint WaitForSingleObject(SafeFileHandle handle, uint milliseconds);
        [DllImport("kernel32.dll", SetLastError = true)]
        internal static extern bool GetExitCodeProcess(SafeFileHandle process, out uint code);
        [DllImport("kernel32.dll", SetLastError = true)]
        internal static extern bool CloseHandle(IntPtr handle);
        [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
        internal static extern SafeFileHandle CreateFileW(string name, uint access, uint share, ref Security attributes,
            uint disposition, uint flags, IntPtr template);
        internal static Exception Error(string operation) {
            return new Win32Exception(Marshal.GetLastWin32Error(), operation);
        }
        internal static string Quote(string value) {
            var result = new StringBuilder("\"");
            int slashes = 0;
            foreach (char c in value) {
                if (c == '\\') { slashes++; continue; }
                result.Append('\\', c == '"' ? slashes * 2 + 1 : slashes);
                result.Append(c);
                slashes = 0;
            }
            return result.Append('\\', slashes * 2).Append('"').ToString();
        }
    }
    public sealed class SmokeProcess : IDisposable {
        private readonly SafeFileHandle handle;
        public string LogPrefix { get; private set; }
        internal SmokeProcess(IntPtr process, string logPrefix) {
            handle = new SafeFileHandle(process, true);
            LogPrefix = logPrefix;
        }
        public bool Wait(int milliseconds) {
            uint status = Native.WaitForSingleObject(handle, unchecked((uint)milliseconds));
            if (status == 0) return true;
            if (status == 258) return false;
            throw Native.Error("WaitForSingleObject");
        }
        public int ExitCode {
            get {
                uint code;
                if (!Wait(0)) throw new InvalidOperationException("owned process has not exited");
                if (!Native.GetExitCodeProcess(handle, out code)) throw Native.Error("GetExitCodeProcess");
                return unchecked((int)code);
            }
        }
        public void Dispose() { handle.Dispose(); }
    }
    public sealed class SmokeJob : IDisposable {
        private readonly SafeFileHandle handle;
        private readonly List<SmokeProcess> processes = new List<SmokeProcess>();
        public static bool IsNativeX64 {
            get { Native.SystemInfo info; Native.GetNativeSystemInfo(out info); return info.Architecture == 9; }
        }
        public SmokeJob() {
            handle = new SafeFileHandle(Native.CreateJobObjectW(IntPtr.Zero, null), true);
            if (handle.IsInvalid) throw Native.Error("CreateJobObjectW");
            var limit = new Native.ExtendedLimit();
            limit.Basic.Flags = Native.KILL_ON_JOB_CLOSE;
            if (!Native.SetInformationJobObject(handle, 9, ref limit, Marshal.SizeOf(limit))) {
                var error = Native.Error("SetInformationJobObject");
                handle.Dispose();
                throw error;
            }
        }
        public uint ActiveProcesses {
            get {
                Native.Accounting info;
                if (!Native.QueryInformationJobObject(handle, 1, out info, Marshal.SizeOf(typeof(Native.Accounting)), IntPtr.Zero))
                    throw Native.Error("QueryInformationJobObject");
                return info.ActiveProcesses;
            }
        }
        public bool WaitEmpty(int milliseconds) {
            var watch = Stopwatch.StartNew();
            do {
                if (ActiveProcesses == 0) return true;
                System.Threading.Thread.Sleep(25);
            } while (watch.ElapsedMilliseconds < milliseconds);
            return ActiveProcesses == 0;
        }
        public void Terminate() {
            if (!Native.TerminateJobObject(handle, 1)) throw Native.Error("TerminateJobObject");
        }
        public SmokeProcess Start(string executable, string[] arguments, string directory, string logPrefix, bool interactive) {
            var command = new StringBuilder(Native.Quote(executable));
            foreach (string argument in arguments) command.Append(' ').Append(Native.Quote(argument));
            var startup = new Native.Startup();
            startup.Size = (uint)Marshal.SizeOf(startup);
            var security = new Native.Security();
            security.Length = Marshal.SizeOf(security);
            security.Inherit = true;
            SafeFileHandle input = null, output = null, error = null;
            var process = new Native.ProcessInfo();
            try {
                if (!interactive) {
                    input = Native.CreateFileW("NUL", 0x80000000, 3, ref security, 3, 0, IntPtr.Zero);
                    output = Native.CreateFileW(logPrefix + ".stdout", 0x40000000, 7, ref security, 2, 0, IntPtr.Zero);
                    error = Native.CreateFileW(logPrefix + ".stderr", 0x40000000, 7, ref security, 2, 0, IntPtr.Zero);
                    if (input.IsInvalid || output.IsInvalid || error.IsInvalid) throw Native.Error("CreateFileW");
                    startup.Flags = 0x100;
                    startup.Input = input.DangerousGetHandle();
                    startup.Output = output.DangerousGetHandle();
                    startup.Error = error.DangerousGetHandle();
                }
                uint flags = Native.CREATE_SUSPENDED | (interactive ? 0 : Native.CREATE_NO_WINDOW);
                if (!Native.CreateProcessW(executable, command, IntPtr.Zero, IntPtr.Zero, !interactive, flags,
                    IntPtr.Zero, directory, ref startup, out process)) throw Native.Error("CreateProcessW");
                if (!Native.AssignProcessToJobObject(handle, process.Process)) throw Native.Error("AssignProcessToJobObject");
                if (Native.ResumeThread(process.Thread) == uint.MaxValue) throw Native.Error("ResumeThread");
                var owned = new SmokeProcess(process.Process, logPrefix);
                process.Process = IntPtr.Zero;
                processes.Add(owned);
                return owned;
            } catch (Exception failure) {
                // An unassigned child is still suspended; only its original creation handle can stop it.
                if (process.Process != IntPtr.Zero) {
                    bool terminated = Native.TerminateProcess(process.Process, 1);
                    var cleanupError = terminated ? new Exception("created process did not exit") : Native.Error("TerminateProcess");
                    if (Native.WaitForSingleObject(process.Process, 5000) != 0)
                        throw new AggregateException("created process cleanup failed", failure, cleanupError);
                }
                throw;
            } finally {
                if (process.Process != IntPtr.Zero) Native.CloseHandle(process.Process);
                if (process.Thread != IntPtr.Zero) Native.CloseHandle(process.Thread);
                if (input != null) input.Dispose();
                if (output != null) output.Dispose();
                if (error != null) error.Dispose();
            }
        }
        public void Dispose() {
            handle.Dispose();
            foreach (var process in processes) process.Dispose();
            processes.Clear();
        }
    }
}
'@
}

function Clear-SmokeEnvironmentKey([string]$Key) {
    if (Test-Path -LiteralPath "Env:$Key") { Remove-Item -LiteralPath "Env:$Key" }
}

function Restore-SmokeEnvironment($Context) {
    foreach ($key in $Context.SavedEnvironment.Keys) {
        if ($null -eq $Context.SavedEnvironment[$key]) { Clear-SmokeEnvironmentKey $key }
        else { [Environment]::SetEnvironmentVariable($key, $Context.SavedEnvironment[$key], 'Process') }
    }
}

function New-WindowsSmokeContext {
    param([string]$Name, [string]$Root = '')
    $prefix = $Name -creplace '[^a-zA-Z0-9_-]', '-'
    $prefix = $prefix.Substring(0, [Math]::Min(31, $prefix.Length))
    $session = $prefix + '-' + [guid]::NewGuid().ToString('N')
    $repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
    if (-not $Root) { $Root = Join-Path $repo "target/tmp/windows-smoke/$session" }
    $Root = [IO.Path]::GetFullPath($Root)
    $allowed = @((Join-Path $repo 'target/tmp'), (Join-Path $repo '.local'))
    if (-not ($allowed | Where-Object { $Root.StartsWith(([IO.Path]::GetFullPath($_) + [IO.Path]::DirectorySeparatorChar), [StringComparison]::OrdinalIgnoreCase) })) {
        throw 'smoke root must be private and project-local under target/tmp or .local'
    }
    [IO.Directory]::CreateDirectory($Root) | Out-Null
    $paths = @{
        HOME = 'home'; USERPROFILE = 'home'; APPDATA = 'roaming'; LOCALAPPDATA = 'local';
        XDG_CONFIG_HOME = 'config'; XDG_STATE_HOME = 'state'; XDG_DATA_HOME = 'data';
        XDG_CACHE_HOME = 'cache'; XDG_RUNTIME_DIR = 'run'; HERDR_HOME = 'herdr-home';
        CODEX_HOME = 'codex'; KIMI_CODE_HOME = 'kimi'; CLAUDE_CONFIG_DIR = 'claude';
        TEMP = 'tmp'; TMP = 'tmp'; TMPDIR = 'tmp'; TMUX_TMPDIR = 'tmp'
    }
    $keys = @($paths.Keys) + @('HOMEDRIVE', 'HOMEPATH', 'HERDR_SESSION', 'HERDR_CONFIG_PATH', 'HERDR_LANG', 'HERDR_WINDOWS_CONPTY', 'PATH', 'SHELL', 'PSModuleAnalysisCachePath')
    $keys += @(Get-ChildItem Env: | Where-Object { $_.Name -like 'HERDR_*' } | ForEach-Object { $_.Name })
    $saved = @{}
    foreach ($key in $keys | Select-Object -Unique) { $saved[$key] = [Environment]::GetEnvironmentVariable($key) }
    $rustupHome = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $env:USERPROFILE '.rustup' }
    $ctx = [pscustomobject]@{
        Root = $Root; Session = $session; Exe = ''; Job = $null; Server = $null; ServerStarted = $false;
        SavedEnvironment = $saved; CommandNumber = 0; CleanupTimeoutMilliseconds = 10000;
        RustupHome = $rustupHome; Toolchain = $null; ConptyMode = 'auto'; RuntimeStage = 'preflight'
    }
    try {
        foreach ($key in $saved.Keys) {
            if ($key -like 'HERDR_*') { Clear-SmokeEnvironmentKey $key }
        }
        foreach ($key in $paths.Keys) {
            $path = Join-Path $Root $paths[$key]
            [IO.Directory]::CreateDirectory($path) | Out-Null
            [Environment]::SetEnvironmentVariable($key, $path, 'Process')
        }
        $env:HOMEDRIVE = [IO.Path]::GetPathRoot($env:USERPROFILE).TrimEnd('\')
        $env:HOMEPATH = $env:USERPROFILE.Substring($env:HOMEDRIVE.Length)
        $env:HERDR_SESSION = $session
        $env:HERDR_CONFIG_PATH = Join-Path $Root 'config.toml'
        $env:HERDR_LANG = 'en'
        $env:PSModuleAnalysisCachePath = Join-Path $Root 'cache/ModuleAnalysisCache'
        Initialize-SmokeNative
        $ctx.Job = New-Object HerdrSmoke.SmokeJob
        return $ctx
    } catch {
        if ($null -ne $ctx.Job) { $ctx.Job.Dispose() }
        Restore-SmokeEnvironment $ctx
        throw
    }
}

function Set-SmokeConptyMode {
    param($Context, [ValidateSet('auto', 'system')][string]$Mode = 'auto')
    Clear-SmokeEnvironmentKey 'HERDR_WINDOWS_CONPTY'
    if ($Mode -eq 'system') { $env:HERDR_WINDOWS_CONPTY = 'system' }
    $Context.ConptyMode = $Mode
}

function Assert-SmokeX64Image([string]$Path) {
    $stream = [IO.File]::OpenRead($Path)
    $reader = New-Object IO.BinaryReader($stream)
    try {
        if ($stream.Length -lt 64 -or $reader.ReadUInt16() -ne 0x5a4d) { throw "not a Windows executable: $Path" }
        $stream.Position = 60
        $offset = $reader.ReadUInt32()
        if ($offset -lt 64 -or $offset -gt ($stream.Length - 24)) { throw "invalid PE header: $Path" }
        $stream.Position = $offset
        if ($reader.ReadUInt32() -ne 0x4550 -or $reader.ReadUInt16() -ne 0x8664) { throw "smoke supports only x64 Windows executables: $Path" }
        $stream.Position = $offset + 22
        $flags = $reader.ReadUInt16()
        if (($flags -band 0x2000) -ne 0 -or ($flags -band 2) -eq 0) { throw "not an executable image: $Path" }
    } finally { $reader.Dispose(); $stream.Dispose() }
}

function Get-SmokeToolchain($Context) {
    if ($null -ne $Context.Toolchain) { return $Context.Toolchain }
    if (-not [HerdrSmoke.SmokeJob]::IsNativeX64) { throw 'Windows smoke supports only native x64 hosts' }
    if ($Context.Exe) { Assert-SmokeX64Image $Context.Exe }
    $repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
    $manifest = [IO.File]::ReadAllText((Join-Path $repo 'rust-toolchain.toml'))
    $channels = [regex]::Matches($manifest, '(?m)^\s*channel\s*=\s*"(\d+\.\d+\.\d+)"\s*$')
    if ($channels.Count -ne 1) { throw 'rust-toolchain.toml must declare one pinned Rust channel' }
    $target = 'x86_64-pc-windows-msvc'
    $toolchain = $channels[0].Groups[1].Value + '-' + $target
    $sysroot = Join-Path $Context.RustupHome "toolchains/$toolchain"
    $rustc = Join-Path $sysroot 'bin/rustc.exe'
    $linker = Join-Path $sysroot "lib/rustlib/$target/bin/rust-lld.exe"
    foreach ($path in @($rustc, $linker)) {
        if (-not [IO.File]::Exists($path)) { throw "required existing toolchain component not found (no automatic install): $path" }
        Assert-SmokeX64Image $path
    }
    foreach ($key in @('RUSTUP_TOOLCHAIN', 'CARGO_BUILD_JOBS')) {
        $Context.SavedEnvironment[$key] = [Environment]::GetEnvironmentVariable($key)
    }
    $env:RUSTUP_TOOLCHAIN = $toolchain
    $env:CARGO_BUILD_JOBS = '4'
    $Context.Toolchain = [pscustomobject]@{ Name = $toolchain; Target = $target; Rustc = $rustc; Linker = $linker }
    return $Context.Toolchain
}

function Invoke-SmokeRustc {
    param($Context, [string]$Source, [string]$Output, [ValidateSet('bin', 'cdylib')][string]$CrateType = 'bin')
    $Context.RuntimeStage = if ($CrateType -eq 'cdylib') { 'fake_dll_build' } else { 'shell_launcher' }
    foreach ($path in @($Source, $Output)) {
        if (-not [IO.Path]::GetFullPath($path).StartsWith(($Context.Root + [IO.Path]::DirectorySeparatorChar), [StringComparison]::OrdinalIgnoreCase)) {
            throw 'smoke builds must remain within their private root'
        }
    }
    $toolchain = Get-SmokeToolchain $Context
    $build = Invoke-SmokeCommand -Context $Context -Command $toolchain.Rustc -Arguments @('--crate-type', $CrateType, '--edition', '2021', '--target', $toolchain.Target, '-C', "linker=$($toolchain.Linker)", $Source, '-o', $Output) -TimeoutMilliseconds 60000
    if ($build.ExitCode -ne 0) {
        $failure = New-Object Exception("smoke build failed ($($build.ExitCode)): $($build.Output)$($build.Error)")
        $failure.Data['ExitCode'] = $build.ExitCode
        throw $failure
    }
}

function New-SmokeShellLauncher($Context, [string]$ShellPath) {
    $shell = (Resolve-Path -LiteralPath $ShellPath).Path
    $directory = Join-Path $Context.Root 'shell'
    [IO.Directory]::CreateDirectory($directory) | Out-Null
    $source = Join-Path $directory 'launcher.rs'
    $launcher = Join-Path $directory ([IO.Path]::GetFileName($shell))
    [IO.File]::WriteAllText((Join-Path $directory 'shell-path.txt'), $shell, (New-Object Text.UTF8Encoding($false)))
    @'
use std::process::{Command, Stdio};

fn run() -> Result<i32, Box<dyn std::error::Error>> {
    let executable = std::env::current_exe()?;
    let directory = executable.parent().ok_or("launcher has no parent")?;
    let shell = std::fs::read_to_string(directory.join("shell-path.txt"))?;
    let status = Command::new(shell)
        .args(["-NoLogo", "-NoProfile"])
        .args(std::env::args_os().skip(1))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    status.code().ok_or_else(|| "shell exited without a status".into())
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(error) => { eprintln!("isolated shell launcher: {error}"); std::process::exit(1); }
    }
}
'@ | Set-Content -NoNewline -Encoding utf8 $source
    Invoke-SmokeRustc -Context $Context -Source $source -Output $launcher
    return $launcher
}

function Set-SmokeConfig($Context, [string]$ShellPath) {
    $launcher = New-SmokeShellLauncher -Context $Context -ShellPath $ShellPath
    $env:SHELL = $launcher
    $shellToml = $launcher.Replace('\', '\\').Replace('"', '\"')
    $config = @"
onboarding = false
[experimental]
allow_nested = true
[terminal]
default_shell = "$shellToml"
[update]
version_check = false
manifest_check = false
"@
    [IO.File]::WriteAllText((Join-Path $Context.Root 'config.toml'), $config, (New-Object Text.UTF8Encoding($false)))
}

function Start-SmokeProcess {
    param($Context, [string]$Command, [string[]]$Arguments = @(), [switch]$Interactive)
    $Context.CommandNumber++
    $prefix = Join-Path $Context.Root ('command-{0:d3}' -f $Context.CommandNumber)
    [IO.File]::WriteAllText(($prefix + '.json'), (@{command = $Command; arguments = $Arguments; session = $Context.Session} | ConvertTo-Json))
    return $Context.Job.Start($Command, $Arguments, $Context.Root, $prefix, $Interactive.IsPresent)
}

function Invoke-SmokeCommand {
    param($Context, [string]$Command, [string[]]$Arguments = @(), [int]$TimeoutMilliseconds = 20000, [switch]$Interactive)
    $process = Start-SmokeProcess -Context $Context -Command $Command -Arguments $Arguments -Interactive:$Interactive
    if (-not $process.Wait($TimeoutMilliseconds)) { throw "owned command timed out: $Command $($Arguments -join ' ')" }
    $output = ''; $errorText = ''
    if (-not $Interactive) {
        $output = [IO.File]::ReadAllText($process.LogPrefix + '.stdout')
        $errorText = [IO.File]::ReadAllText($process.LogPrefix + '.stderr')
    }
    $exitCode = $process.ExitCode
    [IO.File]::WriteAllText(($process.LogPrefix + '.exit'), [string]$exitCode)
    return [pscustomobject]@{ ExitCode = $exitCode; Output = $output; Error = $errorText }
}

function Invoke-SmokeHerdr {
    param($Context, [string[]]$Arguments, [switch]$AllowFailure, [int]$TimeoutMilliseconds = 20000, [switch]$Interactive)
    $Context.RuntimeStage = if ($Interactive) { 'interactive' } else { ($Arguments | Select-Object -First 2) -join '_' }
    $call = Invoke-SmokeCommand -Context $Context -Command $Context.Exe -Arguments (@('--session', $Context.Session) + $Arguments) -TimeoutMilliseconds $TimeoutMilliseconds -Interactive:$Interactive
    if (-not $AllowFailure -and $call.ExitCode -ne 0) {
        $errorValue = New-Object Exception("Herdr failed ($($call.ExitCode)): $($Arguments -join ' ')`n$($call.Output)$($call.Error)")
        $errorValue.Data['ExitCode'] = $call.ExitCode
        throw $errorValue
    }
    if ($AllowFailure) { return $call }
    return $call.Output
}

function Wait-SmokeCleanup($Context, $Result, $Errors, [int]$TimeoutMilliseconds) {
    $Result.active_processes = $null
    try {
        if (-not $Context.Job.WaitEmpty($TimeoutMilliseconds)) { throw 'owned Job did not become empty after graceful shutdown' }
        $Result.active_processes = $Context.Job.ActiveProcesses
    } catch {
        $Errors.Add($_.ToString())
        $Result.cleanup_mode = 'forced'
        try {
            $Context.Job.Terminate()
            if (-not $Context.Job.WaitEmpty(5000)) { throw 'owned Job still active after forced cleanup' }
            $Result.active_processes = $Context.Job.ActiveProcesses
        } catch { $Errors.Add($_.ToString()) }
    }
}

function Complete-WindowsSmoke {
    param($Context, $Result, $RuntimeError = $null, [scriptblock]$Stop, [scriptblock]$Delete)
    $errors = New-Object 'Collections.Generic.List[string]'
    $Result.root = $Context.Root
    $Result.session = $Context.Session
    $Result.runtime_stage = $Context.RuntimeStage
    $Result.cleanup_mode = 'graceful'
    $Result.active_processes = $null
    $Result.runtime_error = $null
    $exitCode = 0
    if ($null -ne $RuntimeError) {
        $Result.runtime = 'FAIL'
        $Result.runtime_error = $RuntimeError.ToString()
        $exitCode = 1
        if ($RuntimeError.Exception.Data.Contains('ExitCode')) { $exitCode = [int]$RuntimeError.Exception.Data['ExitCode'] }
        if ($exitCode -eq 0) { $exitCode = 1 }
    } elseif ($Result.runtime -ne 'PASS') {
        $Result.runtime = 'FAIL'
        $Result.runtime_error = 'runtime did not complete'
        $exitCode = 1
    }
    if ($Result.runtime_error) { [Console]::Error.WriteLine($Result.runtime_error) }
    $Result.runtime_exit_code = $exitCode
    $Result.server_exit_code = $null
    $Result.shutdown = if ($Context.ServerStarted) { 'requested' } else { 'not_started' }
    try {
        try {
            $serverExited = $false
            if ($Context.Server -is [HerdrSmoke.SmokeProcess]) {
                $settle = if ($null -ne $RuntimeError) { 1000 } else { 0 }
                $serverExited = $Context.Server.Wait($settle)
                if ($serverExited) { $Result.shutdown = 'already_exited' }
            }
            if ($null -ne $Stop) { & $Stop | Out-Null }
            elseif ($Context.ServerStarted -and -not $serverExited) {
                Invoke-SmokeHerdr -Context $Context -Arguments @('session', 'stop', $Context.Session, '--json') -TimeoutMilliseconds 5000 | Out-Null
            }
        } catch { $errors.Add($_.ToString()) }
        Wait-SmokeCleanup $Context $Result $errors $Context.CleanupTimeoutMilliseconds
        if ($Result.active_processes -eq 0 -and $null -ne $Result.active_processes) {
            try {
                if ($null -ne $Delete) { & $Delete | Out-Null }
                elseif ($Context.ServerStarted) {
                    Invoke-SmokeHerdr -Context $Context -Arguments @('session', 'delete', $Context.Session, '--json') -TimeoutMilliseconds 5000 | Out-Null
                }
            } catch { $errors.Add($_.ToString()) }
            Wait-SmokeCleanup $Context $Result $errors 5000
        }
    } finally {
        $Result.server_stderr = ''
        if ($Context.Server -is [HerdrSmoke.SmokeProcess]) {
            try {
                $Result.server_stderr = [IO.File]::ReadAllText($Context.Server.LogPrefix + '.stderr')
                if ($Context.Server.Wait(0)) {
                    $Result.server_exit_code = $Context.Server.ExitCode
                    if ($exitCode -eq 0 -and $Result.cleanup_mode -eq 'graceful' -and $Result.server_exit_code -ne 0) {
                        $exitCode = $Result.server_exit_code
                        $Result.runtime = 'FAIL'
                        $Result.runtime_exit_code = $exitCode
                        $Result.runtime_error = "owned server exited abnormally ($exitCode)"
                        [Console]::Error.WriteLine($Result.runtime_error)
                    }
                }
            } catch { $errors.Add($_.ToString()) }
        }
        try { $Context.Job.Dispose() } catch { $errors.Add($_.ToString()) }
        try { Restore-SmokeEnvironment $Context } catch { $errors.Add($_.ToString()) }
    }
    $Result.conpty_mode = $Context.ConptyMode
    $Result.toolchain = if ($null -ne $Context.Toolchain) { $Context.Toolchain.Name } else { $null }
    $Result.cleanup = if ($errors.Count -eq 0) { 'PASS' } else { 'FAIL' }
    $Result.cleanup_errors = @($errors.ToArray())
    if ($exitCode -eq 0 -and $errors.Count -gt 0) { $exitCode = 1 }
    $Result.exit_code = $exitCode
    try { $Result | ConvertTo-Json -Depth 8 | Set-Content -Encoding utf8 (Join-Path $Context.Root 'result.json') }
    catch { [Console]::Error.WriteLine($_.ToString()); if ($exitCode -eq 0) { $exitCode = 1 }; $Result.exit_code = $exitCode }
    foreach ($message in $errors) { [Console]::Error.WriteLine($message) }
    Write-Host "replay report: $($Context.Root)"
    return $exitCode
}
