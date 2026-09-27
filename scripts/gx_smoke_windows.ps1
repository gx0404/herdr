param(
    [Parameter(Mandatory = $true)] [string] $InstallerPath,
    [Parameter(Mandatory = $true)] [string] $ExpectedVersion,
    [string] $PreviousInstallerPath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($env:HERDR_GX_DISPOSABLE -ne '1' -or $env:GITHUB_ACTIONS -ne 'true' -or
    $env:RUNNER_ENVIRONMENT -ne 'github-hosted') {
    throw 'Installation smoke requires HERDR_GX_DISPOSABLE=1 on a disposable GitHub-hosted Windows runner. Never run this on your workstation.'
}
if (-not [Environment]::Is64BitOperatingSystem) { throw 'Windows x64 is required.' }

$installDir = Join-Path $env:LOCALAPPDATA 'Programs\Herdr GX'
$ownerKey = 'HKCU:\Software\Herdr GX\Installer'
if ((Test-Path $installDir) -or (Test-Path $ownerKey)) {
    throw 'Refusing to touch an existing Herdr GX installation.'
}
$installer = (Resolve-Path -LiteralPath $InstallerPath).Path
$python = (Get-Command python -CommandType Application).Source
$runtimeScript = Join-Path $PSScriptRoot 'gx_smoke_runtime.py'
$work = Join-Path ([IO.Path]::GetTempPath()) ('herdr-gx-lifecycle-' + [guid]::NewGuid().ToString('N'))
$registry = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
$originalRaw = $registry.GetValue('Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
$originalKind = if ($null -eq $originalRaw) { [Microsoft.Win32.RegistryValueKind]::ExpandString } else { $registry.GetValueKind('Path') }
$oldProcessPath = $env:PATH
$logs = 0
$installed = $false

function Read-RawPath { return [string]$registry.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames) }
function Set-RawPath([string] $Raw, [Microsoft.Win32.RegistryValueKind] $Kind) { $registry.SetValue('Path', $Raw, $Kind) }
function Assert-True([bool] $Condition, [string] $Message) { if (-not $Condition) { throw $Message } }
function Normalize-PathEntry([string] $Entry) { return [Environment]::ExpandEnvironmentVariables($Entry.Trim().Trim('"')).Replace('/', '\').TrimEnd('\').ToLowerInvariant() }
function Assert-PathCount([int] $Count) {
    $matches = @((Read-RawPath).Split(';') | Where-Object { (Normalize-PathEntry $_) -eq (Normalize-PathEntry $installDir) })
    Assert-True ($matches.Count -eq $Count) "Expected $Count Herdr GX PATH entries, got $($matches.Count)."
}
function Invoke-Bounded([string] $File, [string[]] $Arguments, [bool] $MustFail = $false) {
    $quoted = @($Arguments | ForEach-Object { '"' + $_.Replace('"', '\"') + '"' })
    $process = Start-Process -FilePath $File -ArgumentList $quoted -PassThru -WorkingDirectory $work
    if (-not $process.WaitForExit(120000)) {
        $process.Kill()
        throw "Owned command timed out: $File"
    }
    $process.Refresh()
    if ($MustFail) { Assert-True ($process.ExitCode -ne 0) "Expected occupied-file rejection: $File" }
    else { Assert-True ($process.ExitCode -eq 0) "Command failed ($($process.ExitCode)): $File" }
}
function Install-Package([string] $Path, [bool] $MustFail = $false) {
    $script:logs++
    Invoke-Bounded $Path @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-', "/LOG=$work\install-$script:logs.log") $MustFail
    if (-not $MustFail) { $script:installed = $true }
}
function Remove-Package([bool] $MustFail = $false) {
    Invoke-Bounded (Join-Path $installDir 'unins000.exe') @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', "/LOG=$work\uninstall.log") $MustFail
    if (-not $MustFail) { $script:installed = $false }
}
function Load-Manifest([string] $Path) {
    $manifest = Get-Content -LiteralPath "$Path.manifest.json" -Raw | ConvertFrom-Json
    Assert-True ($manifest.schema_version -eq 1 -and $manifest.platform -eq 'windows' -and
        $manifest.package_manager -eq 'windows-installer' -and $manifest.architecture -eq 'x86_64') 'Invalid installer manifest identity.'
    Assert-True ((Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() -eq $manifest.artifact.sha256) 'Installer checksum mismatch.'
    return $manifest
}
function Assert-Payload($Manifest) {
    foreach ($property in $Manifest.files.PSObject.Properties) {
        $path = Join-Path $installDir $property.Name
        Assert-True ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -eq $property.Value) "Payload checksum mismatch: $path"
    }
    Assert-True (Test-Path (Join-Path $installDir 'conpty\herdr-conpty.json')) 'ConPTY marker is missing.'
    Assert-True (Test-Path (Join-Path $installDir 'conpty\conpty.dll')) 'ConPTY DLL is missing.'
    Assert-True (Test-Path (Join-Path $installDir 'conpty\x64\OpenConsole.exe')) 'ConPTY host is missing.'
    Assert-True (Test-Path (Join-Path $installDir 'LICENSE')) 'Herdr license is missing.'
    Assert-True (Test-Path (Join-Path $installDir 'THIRD-PARTY-NOTICES')) 'Third-party notices are missing.'
}
function Refresh-ProcessPath {
    $machine = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $env:PATH = [Environment]::ExpandEnvironmentVariables($machine + ';' + (Read-RawPath))
}
function Assert-NewShell($Manifest) {
    Refresh-ProcessPath
    $env:GX_EXPECTED_EXE = Join-Path $installDir 'herdr.exe'
    $env:GX_EXPECTED_VERSION = $Manifest.binary.version_output
    $probe = @'
$ErrorActionPreference = 'Stop'
$resolved = (Get-Command herdr -CommandType Application).Source
if ($resolved -ne $env:GX_EXPECTED_EXE) { throw "Get-Command resolves to $resolved" }
$where = @(& where.exe herdr)
if ($LASTEXITCODE -ne 0 -or $where[0] -ne $env:GX_EXPECTED_EXE) { throw "where.exe resolves to $where" }
$version = (& herdr --version | Out-String).Trim()
if ($LASTEXITCODE -ne 0 -or $version -ne $env:GX_EXPECTED_VERSION) { throw "Version mismatch: $version" }
& herdr --help | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'herdr --help failed' }
'@
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($probe))
    Invoke-Bounded (Get-Process -Id $PID).Path @('-NoProfile', '-NonInteractive', '-EncodedCommand', $encoded)
}

New-Item -ItemType Directory -Path $work | Out-Null
$dataRoot = Join-Path $env:APPDATA 'herdr'
$dataSentinel = Join-Path $dataRoot ('gx-smoke-' + [guid]::NewGuid().ToString('N'))
try {
    $manifest = Load-Manifest $installer
    Assert-True ($manifest.version -eq $ExpectedVersion) 'ExpectedVersion does not match the manifest.'
    $baseline = '%SystemRoot%\System32;' + [string]$originalRaw
    Set-RawPath $baseline ([Microsoft.Win32.RegistryValueKind]::ExpandString)
    New-Item -ItemType Directory -Force -Path $dataRoot | Out-Null
    [IO.File]::WriteAllText($dataSentinel, 'keep-user-data')
    if ($PreviousInstallerPath) {
        $previous = (Resolve-Path -LiteralPath $PreviousInstallerPath).Path
        $previousManifest = Load-Manifest $previous
        Assert-True ([version]$previousManifest.version -lt [version]$ExpectedVersion) 'Upgrade requires a genuinely older Cargo version, not the same installer.'
        Install-Package $previous
        Assert-Payload $previousManifest
        Assert-NewShell $previousManifest
        Install-Package $installer
        Write-Host "PASS upgrade $($previousManifest.version) -> $ExpectedVersion"
    } else {
        Install-Package $installer
        Write-Host 'N/A previous-version upgrade: first release/no previous installer supplied. Same-version reinstall is tested separately.'
    }
    Assert-Payload $manifest
    Assert-PathCount 1
    $firstPath = Read-RawPath
    Assert-True ($registry.GetValueKind('Path') -eq [Microsoft.Win32.RegistryValueKind]::ExpandString) 'PATH value type changed.'
    Assert-True ($firstPath.StartsWith($baseline, [StringComparison]::Ordinal)) 'Raw PATH was expanded or rewritten.'
    Install-Package $installer
    Assert-True ((Read-RawPath) -ceq $firstPath) 'Repeated install changed PATH ownership or duplicated PATH.'
    Assert-NewShell $manifest
    $holdDir = Join-Path $work 'runtime-hold'
    New-Item -ItemType Directory -Path $holdDir | Out-Null
    $runtimeArgs = @($runtimeScript, '--binary', 'herdr', '--expected-version', $manifest.binary.version_output, '--hold-dir', $holdDir)
    $runtimeQuoted = @($runtimeArgs | ForEach-Object { '"' + $_.Replace('"', '\"') + '"' })
    $runtime = Start-Process -FilePath $python -ArgumentList $runtimeQuoted -PassThru -WorkingDirectory $work
    try {
        $deadline = (Get-Date).AddSeconds(80)
        while (-not (Test-Path (Join-Path $holdDir 'ready'))) {
            $runtime.Refresh()
            if ($runtime.HasExited -or (Get-Date) -ge $deadline) { throw 'Installed runtime did not become ready.' }
            Start-Sleep -Milliseconds 100
        }
        Install-Package $installer $true
        Remove-Package $true
        Assert-Payload $manifest
        Assert-True ((Read-RawPath) -ceq $firstPath) 'Occupied-runtime operation changed PATH.'
    } finally {
        [IO.File]::WriteAllText((Join-Path $holdDir 'release'), 'release')
        if (-not $runtime.WaitForExit(30000)) { $runtime.Kill(); throw 'Runtime cleanup timed out; independent watchdog will reclaim its sandbox.' }
    }
    Assert-True ($runtime.ExitCode -eq 0) 'Installed runtime smoke failed.'
    foreach ($relative in @('herdr.exe', 'conpty\conpty.dll', 'conpty\x64\OpenConsole.exe')) {
        $locked = [IO.File]::Open((Join-Path $installDir $relative), [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
        try {
            Install-Package $installer $true
            Remove-Package $true
            Assert-Payload $manifest
            Assert-True ((Read-RawPath) -ceq $firstPath) 'Failed occupied-file operation changed PATH.'
        } finally { $locked.Dispose() }
    }
    $laterEntry = Join-Path $work 'later-user-path'
    Set-RawPath ($firstPath + ';' + $laterEntry) ([Microsoft.Win32.RegistryValueKind]::ExpandString)
    Remove-Package
    Assert-True ((Read-RawPath) -ceq ($baseline + ';' + $laterEntry)) 'Uninstall did not remove only its owned PATH entry.'
    Assert-True ((Get-Content -LiteralPath $dataSentinel -Raw) -eq 'keep-user-data') 'Uninstall removed user data.'
    Assert-True (-not (Test-Path (Join-Path $installDir 'herdr.exe'))) 'Uninstall left the installed binary.'
    Assert-True (-not (Test-Path $ownerKey)) 'Uninstall left PATH ownership metadata.'

    $shadow = Join-Path $work 'shadow'
    New-Item -ItemType Directory -Path $shadow | Out-Null
    [IO.File]::WriteAllText((Join-Path $shadow 'herdr.cmd'), '@echo shadow')
    $preexisting = $baseline + ';' + $shadow + ';' + $installDir.ToUpperInvariant() + '\'
    Set-RawPath $preexisting ([Microsoft.Win32.RegistryValueKind]::String)
    Install-Package $installer
    Assert-PathCount 1
    Assert-True ((Read-RawPath) -ceq $preexisting) 'Install changed a preexisting equivalent PATH entry.'
    Assert-True ($registry.GetValueKind('Path') -eq [Microsoft.Win32.RegistryValueKind]::String) 'REG_SZ PATH type changed.'
    Assert-True (-not ((Get-Item $ownerKey).GetValueNames() -contains 'AddedPath')) 'Installer claimed a preexisting PATH entry.'
    $installLog = Get-Content -LiteralPath "$work\install-$logs.log" -Raw
    Assert-True ($installLog.Contains('PATH conflict:')) 'Installer did not report PATH shadowing.'
    Refresh-ProcessPath
    Push-Location $work
    try {
        $hits = @(& where.exe herdr)
        Assert-True ($hits[0] -eq (Join-Path $shadow 'herdr.cmd')) 'Shadow fixture did not actually precede GX.'
    } finally { Pop-Location }
    Remove-Package
    Assert-True ((Read-RawPath) -ceq $preexisting) 'Uninstall removed a preexisting PATH entry.'
    Assert-True (Test-Path (Join-Path $shadow 'herdr.cmd')) 'Uninstall touched another Herdr installation.'
    Write-Host 'PASS Windows install/reinstall, PATH ownership/types/shadowing, payload, runtime, locks, uninstall and user-data preservation.'
} finally {
    if ($installed -and (Test-Path (Join-Path $installDir 'unins000.exe'))) {
        try { Remove-Package } catch { Write-Warning "Cleanup could not uninstall the owned test installation: $_" }
    }
    if ($null -eq $originalRaw) { $registry.DeleteValue('Path', $false) }
    else { $registry.SetValue('Path', $originalRaw, $originalKind) }
    $registry.Dispose()
    $env:PATH = $oldProcessPath
    Remove-Item Env:GX_EXPECTED_EXE, Env:GX_EXPECTED_VERSION -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $dataSentinel -Force -ErrorAction SilentlyContinue
    if (-not $installed) { Remove-Item -LiteralPath $work -Recurse -Force }
    else { Write-Warning "Logs retained at $work" }
}
