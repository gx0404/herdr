param(
    [Parameter(Mandatory = $true)]
    [string] $ExePath,

    [string] $Session = 'ci-windows',
    [ValidateSet('auto', 'system')][string]$ConptyMode = 'auto',
    [switch]$PassThru
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'windows_input/windows_smoke_helpers.ps1')
$exe = (Resolve-Path -LiteralPath $ExePath).Path
$shellPath = (Get-Command powershell -ErrorAction Stop).Source
$context = New-WindowsSmokeContext -Name $Session
$context.Exe = $exe
$result = [ordered]@{ runtime = 'PENDING'; runtime_stage = 'preflight'; cleanup = 'PENDING' }
$runtimeError = $null
try {
    Set-SmokeConptyMode -Context $context -Mode $ConptyMode
    Set-SmokeConfig $context $shellPath
    $fakeDir = Join-Path $context.Root 'fake-conpty'
    [IO.Directory]::CreateDirectory($fakeDir) | Out-Null
    $fakeSource = Join-Path $fakeDir 'fake_conpty.rs'
    $fakeDll = Join-Path $fakeDir 'conpty.dll'
    @'
#![allow(non_snake_case)]

use std::ffi::c_void;

#[repr(C)]
pub struct COORD {
    pub X: i16,
    pub Y: i16,
}

type HANDLE = *mut c_void;
type HRESULT = i32;

#[no_mangle]
pub extern "system" fn CreatePseudoConsole(
    _size: COORD,
    _h_input: HANDLE,
    _h_output: HANDLE,
    _flags: u32,
    _hpc: *mut HANDLE,
) -> HRESULT {
    -2147467259
}

#[no_mangle]
pub extern "system" fn ResizePseudoConsole(_hpc: HANDLE, _size: COORD) -> HRESULT {
    -2147467259
}

#[no_mangle]
pub extern "system" fn ClosePseudoConsole(_hpc: HANDLE) {}
'@ | Set-Content -NoNewline -Encoding utf8 $fakeSource
    Invoke-SmokeRustc -Context $context -Source $fakeSource -Output $fakeDll -CrateType cdylib
    $env:PATH = "$fakeDir;$env:PATH"
    Invoke-SmokeHerdr -Context $context -Arguments @('--version') | Out-Null
    Invoke-SmokeHerdr -Context $context -Arguments @('--default-config') | Out-Null
    Invoke-SmokeHerdr -Context $context -Arguments @('config', 'check') | Out-Null
    $context.Server = Start-SmokeProcess -Context $context -Command $exe -Arguments @('--session', $context.Session, 'server')
    $context.ServerStarted = $true
    $ready = $false
    $deadline = [DateTime]::UtcNow.AddSeconds(20)
    do {
        $status = Invoke-SmokeHerdr -Context $context -Arguments @('pane', 'list') -AllowFailure -TimeoutMilliseconds 5000
        if ($status.ExitCode -eq 0) { $ready = $true; break }
        if ($context.Server.Wait(0)) { throw "test server exited ($($context.Server.ExitCode))" }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    if (-not $ready) { throw 'server did not become ready' }
    $created = Invoke-SmokeHerdr -Context $context -Arguments @('workspace', 'create', '--cwd', $context.Root) | ConvertFrom-Json
    $paneId = $created.result.root_pane.pane_id
    if ([string]::IsNullOrWhiteSpace($paneId)) { throw 'workspace create did not return a root pane id' }
    $marker = 'HERDR_CONPTY_' + [guid]::NewGuid().ToString('N')
    $first = $marker.Substring(0, 13)
    $last = $marker.Substring(13)
    Invoke-SmokeHerdr -Context $context -Arguments @('pane', 'run', $paneId, "[Console]::WriteLine(('{0}{1}' -f '$first','$last'))") | Out-Null
    $matched = $false
    $deadline = [DateTime]::UtcNow.AddSeconds(20)
    do {
        $text = Invoke-SmokeHerdr -Context $context -Arguments @('pane', 'read', $paneId, '--source', 'recent-unwrapped', '--lines', '40', '--format', 'text')
        $lines = $text -split "`r?`n" | ForEach-Object { $_.Trim() }
        if ($lines -contains $marker) { $matched = $true; break }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    if (-not $matched) { throw "pane read did not include the smoke marker: $text" }
    [IO.File]::WriteAllText((Join-Path $context.Root 'pane.txt'), $text)
    $result.runtime = 'PASS'
} catch {
    $runtimeError = $_
} finally {
    $exitCode = Complete-WindowsSmoke -Context $context -Result $result -RuntimeError $runtimeError
}
if ($PassThru) { [pscustomobject]$result }
exit $exitCode
