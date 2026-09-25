# installed by herdr
# managed by herdr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# HERDR_INTEGRATION_ID=kimi
# HERDR_INTEGRATION_VERSION=8

param([string]$Action = "")

if (@("session", "working", "blocked", "idle", "activity") -notcontains $Action) { exit 0 }
if ($env:HERDR_ENV -ne "1") { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:HERDR_PANE_ID)) { exit 0 }

$inputText = [Console]::In.ReadToEnd()
try {
    $payload = if ([string]::IsNullOrWhiteSpace($inputText)) { $null } else { $inputText | ConvertFrom-Json }
} catch {
    $payload = $null
}

$seq = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
$sessionId = if ($null -ne $payload -and -not [string]::IsNullOrWhiteSpace($payload.session_id)) { $payload.session_id } else { $null }
$herdr = if ([string]::IsNullOrWhiteSpace($env:HERDR_BIN_PATH)) { "herdr" } else { $env:HERDR_BIN_PATH }

# The hook inherits Kimi CLI environment. Relative, empty and literal-tilde
# overrides have no unambiguous absolute path; do not substitute the server home.
function Test-KimiAbsolutePath([string]$Value) {
    # Windows PowerShell 5.1 lacks Path.IsPathFullyQualified. Exclude drive-
    # relative C:foo and root-relative \foo, accepting drive paths and UNC shares.
    return $Value -match '^(?:[A-Za-z]:[\\/]|\\\\[^\\]+\\[^\\]+(?:\\|$))'
}
function Get-KimiSessionDirectory {
    if ($sessionId -isnot [string] -or $sessionId -notmatch '^[A-Za-z0-9_-]{1,128}$') { return $null }
    $name = if ($sessionId.StartsWith("session_")) { $sessionId } else { "session_$sessionId" }
    if ($name -eq "session_") { return $null }
    $root = [Environment]::GetEnvironmentVariable("KIMI_CODE_HOME")
    if ($null -eq $root) {
        if ([string]::IsNullOrEmpty($env:USERPROFILE) -or -not (Test-KimiAbsolutePath $env:USERPROFILE)) { return $null }
        $root = Join-Path $env:USERPROFILE ".kimi-code"
    }
    if ([string]::IsNullOrEmpty($root) -or -not (Test-KimiAbsolutePath $root)) { return $null }
    try {
        $sessions = [IO.Path]::GetFullPath((Join-Path $root "sessions"))
        $found = $null
        $count = 0
        # Enumerate lazily: never read all workspaces before applying the bound.
        foreach ($bucket in [IO.Directory]::EnumerateFileSystemEntries($sessions)) {
            $count++
            if ($count -gt 256) { return $null }
            $attributes = [IO.File]::GetAttributes($bucket)
            if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or ($attributes -band [IO.FileAttributes]::Directory) -eq 0) { continue }
            $candidate = Join-Path $bucket $name
            if (-not [IO.Directory]::Exists($candidate)) { continue }
            if ($null -ne $found -or ([IO.File]::GetAttributes($candidate) -band [IO.FileAttributes]::ReparsePoint) -ne 0) { return $null }
            $stateFile = Join-Path $candidate "state.json"
            if (([IO.File]::GetAttributes($stateFile) -band [IO.FileAttributes]::ReparsePoint) -ne 0) { return $null }
            $stream = [IO.File]::OpenRead($stateFile)
            try {
                $buffer = New-Object byte[] (8 * 1024 * 1024 + 1)
                $length = 0
                while ($length -lt $buffer.Length) {
                    $read = $stream.Read($buffer, $length, $buffer.Length - $length)
                    if ($read -eq 0) { break }
                    $length += $read
                }
                if ($length -gt 8 * 1024 * 1024) { return $null }
                $metadata = [Text.Encoding]::UTF8.GetString($buffer, 0, $length) | ConvertFrom-Json
            } finally { $stream.Dispose() }
            if ($null -eq $metadata -or $metadata -isnot [pscustomobject]) { return $null }
            if ($metadata.PSObject.Properties.Name -contains "id") {
                if ($metadata.id -isnot [string] -or $metadata.id -cne $name) { return $null }
            } elseif ($metadata.PSObject.Properties.Name -contains "version" -or $metadata.workDir -isnot [string]) {
                return $null
            }
            $found = [IO.Path]::GetFullPath($candidate)
        }
        return $found
    } catch { return $null }
}

$sessionPath = Get-KimiSessionDirectory
try {
    if ($Action -eq "session" -or $null -ne $sessionPath) {
        if ([string]::IsNullOrWhiteSpace($sessionId)) { exit 0 }
        $sessionArgs = @("pane", "report-agent-session", $env:HERDR_PANE_ID, "--source", "herdr:kimi", "--agent", "kimi", "--agent-session-id", $sessionId, "--seq", $seq)
        if ($Action -eq "session") { $sessionArgs += @("--session-start-source", "startup") }
        if ($null -ne $sessionPath) { $sessionArgs += @("--agent-session-path", $sessionPath) }
        & $herdr @sessionArgs 2>$null | Out-Null
        $seq++
    }
    # Activity has no CLI subcommand; the path supplement lets normal polling
    # find the pane-specific session even if SessionStart preceded its creation.
    if ($Action -eq "session" -or $Action -eq "activity") { exit 0 }
    if ([string]::IsNullOrWhiteSpace($sessionId)) {
        & $herdr pane report-agent $env:HERDR_PANE_ID --source herdr:kimi --agent kimi --state $Action --seq $seq 2>$null | Out-Null
    } else {
        & $herdr pane report-agent $env:HERDR_PANE_ID --source herdr:kimi --agent kimi --state $Action --agent-session-id $sessionId --seq $seq 2>$null | Out-Null
    }
} catch {
}
