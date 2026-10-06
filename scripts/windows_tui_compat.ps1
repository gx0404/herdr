# Isolated replay under PowerShell 5.1/7; GUI mode only opens this script's owned session.
param(
    [Parameter(Mandatory = $true)][string]$ExePath,
    [ValidateSet('powershell', 'pwsh')][string]$Shell = 'powershell',
    [ValidateSet('auto', 'system')][string]$ConptyMode = 'auto',
    [switch]$Interactive,
    [switch]$PassThru
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'windows_input/windows_smoke_helpers.ps1')
$exe = (Resolve-Path -LiteralPath $ExePath).Path
$shellPath = (Get-Command $Shell -ErrorAction Stop).Source
$context = New-WindowsSmokeContext -Name 'tui-compat'
$context.Exe = $exe
$result = [ordered]@{ shell = $Shell; runtime = 'PENDING'; runtime_stage = 'preflight'; gui = 'PENDING'; cleanup = 'PENDING' }
$runtimeError = $null
try {
    Set-SmokeConptyMode -Context $context -Mode $ConptyMode
    Set-SmokeConfig $context $shellPath
    Invoke-SmokeHerdr -Context $context -Arguments @('config', 'check') | Out-Null
    if ($Interactive) {
        Write-Host 'In the test session, check: Ctrl+B Space main menu; Ctrl+B / search; the settings page; dragging the window border.'
        Write-Host 'During continuous output, hold the left button to select; output should continue in the background, and releasing copies what you saw. Shift drag-select is handled by WezTerm.'
        Write-Host 'Resize to 60x16, 80x24, 120x40, and 160x50 in turn; check CJK text, IME, scrolling, and the right-click menu.'
        Write-Host 'Detach with Ctrl+B d to return; this script then cleans up only its own session.'
        $context.ServerStarted = $true
        Invoke-SmokeHerdr -Context $context -Arguments @() -Interactive -TimeoutMilliseconds -1 | Out-Null
        $result.runtime = 'PASS'
        $result.gui = 'PENDING: attach manual observations to this report'
    } else {
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
        if (-not $ready) { throw 'test server not ready' }
        $created = Invoke-SmokeHerdr -Context $context -Arguments @('workspace', 'create', '--cwd', $context.Root) | ConvertFrom-Json
        $pane = $created.result.root_pane.pane_id
        if (-not $pane) { throw 'no pane_id returned' }
        $marker = 'HERDR_TUI_' + [guid]::NewGuid().ToString('N')
        $first = $marker.Substring(0, 10)
        $last = $marker.Substring(10)
        Invoke-SmokeHerdr -Context $context -Arguments @('pane', 'run', $pane, "[Console]::WriteLine(('{0}{1}' -f '$first','$last')); [Console]::WriteLine(('PS_MAJOR=' + `$PSVersionTable.PSVersion.Major))") | Out-Null
        $matched = $false
        $deadline = [DateTime]::UtcNow.AddSeconds(20)
        do {
            $text = Invoke-SmokeHerdr -Context $context -Arguments @('pane', 'read', $pane, '--source', 'recent-unwrapped', '--lines', '40', '--format', 'text')
            $expectedMajor = if ($Shell -eq 'pwsh') { '7' } else { '5' }
            $lines = $text -split "`r?`n" | ForEach-Object { $_.Trim() }
            if (($lines -contains $marker) -and ($lines -contains "PS_MAJOR=$expectedMajor")) { $matched = $true; break }
            Start-Sleep -Milliseconds 250
        } while ([DateTime]::UtcNow -lt $deadline)
        if (-not $matched) { throw 'PowerShell ConPTY did not return the marker' }
        [IO.File]::WriteAllText((Join-Path $context.Root 'pane.txt'), $text)
        $result.runtime = 'PASS'
        $result.gui = 'N/A: headless replay; interactive qualification not run'
    }
} catch {
    $runtimeError = $_
} finally {
    $exitCode = Complete-WindowsSmoke -Context $context -Result $result -RuntimeError $runtimeError
}
if ($PassThru) { [pscustomobject]$result }
exit $exitCode
