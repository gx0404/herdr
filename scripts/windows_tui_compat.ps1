# Isolated replay under PowerShell 5.1/7; GUI mode only opens the named session created by this script.
param(
    [Parameter(Mandatory = $true)][string]$ExePath,
    [ValidateSet('powershell', 'pwsh')][string]$Shell = 'powershell',
    [switch]$Interactive
)
$ErrorActionPreference = 'Stop'
$exe = (Resolve-Path -LiteralPath $ExePath).Path
$shellPath = (Get-Command $Shell -ErrorAction Stop).Source
$session = 'tui-compat-' + [guid]::NewGuid().ToString('N').Substring(0, 12)
$root = Join-Path ([IO.Path]::GetTempPath()) $session
[IO.Directory]::CreateDirectory($root) | Out-Null
$keys = @('HERDR_SESSION', 'HERDR_SOCKET_PATH', 'HERDR_CLIENT_SOCKET_PATH', 'HERDR_CONFIG_PATH',
    'HERDR_WORKSPACE_ID', 'HERDR_TAB_ID', 'HERDR_PANE_ID', 'XDG_CONFIG_HOME', 'XDG_STATE_HOME')
$saved = @{}
foreach ($key in $keys) {
    $saved[$key] = [Environment]::GetEnvironmentVariable($key)
    if (Test-Path "Env:$key") { Remove-Item "Env:$key" }
}
$env:HERDR_SESSION = $session
$env:XDG_CONFIG_HOME = Join-Path $root 'config'
$env:XDG_STATE_HOME = Join-Path $root 'state'
$env:HERDR_CONFIG_PATH = Join-Path $root 'config.toml'
$shellToml = $shellPath.Replace('\', '\\').Replace('"', '\"')
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
[IO.File]::WriteAllText($env:HERDR_CONFIG_PATH, $config, (New-Object Text.UTF8Encoding($false)))
function Invoke-Herdr([string[]]$Argv) {
    $output = & $exe @Argv
    if ($LASTEXITCODE -ne 0) { throw "Herdr failed ($LASTEXITCODE): $($Argv -join ' ')" }
    return $output
}
$server = $null
$result = [ordered]@{ shell = $Shell; session = $session; runtime = 'PENDING'; gui = 'PENDING'; cleanup = 'PENDING' }
try {
    Invoke-Herdr @('config', 'check') | Out-Null
    if ($Interactive) {
        Write-Host 'In the test session, check: Ctrl+B Space main menu; Ctrl+B / search; the settings page; dragging the window border.'
        Write-Host 'During continuous output, hold the left button to select; output should continue in the background, and releasing copies what you saw. Shift drag-select is handled by WezTerm.'
        Write-Host 'Resize to 60x16, 80x24, 120x40, and 160x50 in turn; check CJK text, IME, scrolling, and the right-click menu.'
        Write-Host 'Detach with Ctrl+B d to return; this script then cleans up only its own session.'
        & $exe --session $session
        if ($LASTEXITCODE -ne 0) { throw 'test TUI exited abnormally' }
        $result.runtime = 'PASS'
        $result.gui = 'PENDING: attach manual observations to this report'
    } else {
        $server = Start-Process -FilePath $exe -ArgumentList 'server' -PassThru -WindowStyle Hidden
        $ready = $false
        for ($attempt = 0; $attempt -lt 40; $attempt++) {
            try { $null = & $exe pane list 2>$null } catch { }
            if ($LASTEXITCODE -eq 0) { $ready = $true; break }
            Start-Sleep -Milliseconds 250
        }
        if (-not $ready) { throw 'test server not ready' }
        $created = (Invoke-Herdr @('workspace', 'create', '--cwd', $root) | Out-String | ConvertFrom-Json)
        $pane = $created.result.root_pane.pane_id
        if (-not $pane) { throw 'no pane_id returned' }
        $marker = 'HERDR_TUI_' + [guid]::NewGuid().ToString('N')
        $first = $marker.Substring(0, 10)
        $last = $marker.Substring(10)
        Invoke-Herdr @('pane', 'run', $pane, "[Console]::WriteLine(('{0}{1}' -f '$first','$last')); [Console]::WriteLine(('PS_MAJOR=' + `$PSVersionTable.PSVersion.Major))") | Out-Null
        $matched = $false
        for ($attempt = 0; $attempt -lt 40; $attempt++) {
            $text = Invoke-Herdr @('pane', 'read', $pane, '--source', 'recent-unwrapped', '--lines', '40', '--format', 'text') | Out-String
            $expectedMajor = if ($Shell -eq 'pwsh') { '7' } else { '5' }
            $lines = $text -split "`r?`n" | ForEach-Object { $_.Trim() }
            if (($lines -contains $marker) -and ($lines -contains "PS_MAJOR=$expectedMajor")) { $matched = $true; break }
            Start-Sleep -Milliseconds 250
        }
        if (-not $matched) { throw 'PowerShell ConPTY did not return the marker' }
        [IO.File]::WriteAllText((Join-Path $root 'pane.txt'), $text)
        $result.runtime = 'PASS'
    }
} finally {
    try {
        $null = & $exe session stop $session --json
        if ($null -ne $server) {
            if (-not $server.WaitForExit(10000)) { Stop-Process -Id $server.Id -Force }
        }
        $null = & $exe session delete $session
        $result.cleanup = if ($LASTEXITCODE -eq 0) { 'PASS' } else { 'FAIL' }
    } catch { $result.cleanup = 'FAIL: ' + $_.Exception.Message }
    $result | ConvertTo-Json | Set-Content -Encoding utf8 (Join-Path $root 'result.json')
    foreach ($key in $keys) {
        if ($null -eq $saved[$key]) {
            if (Test-Path "Env:$key") { Remove-Item "Env:$key" }
        } else {
            Set-Item "Env:$key" $saved[$key]
        }
    }
    Write-Host "replay report: $root"
}
