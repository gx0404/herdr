# installed by herdr
# managed by herdr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# HERDR_INTEGRATION_ID=kimi
# HERDR_INTEGRATION_VERSION=9

param([string]$Action = "")

# A failing report must never break Kimi.
trap { exit 0 }

if (@("session", "working", "blocked", "idle", "activity") -notcontains $Action) { exit 0 }
if ($env:HERDR_ENV -ne "1") { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:HERDR_PANE_ID)) { exit 0 }

# Read the hook input as UTF-8 bytes; the console input code page may differ.
try {
    $buffer = New-Object IO.MemoryStream
    [Console]::OpenStandardInput().CopyTo($buffer)
    $inputText = [Text.Encoding]::UTF8.GetString($buffer.ToArray()).TrimStart([char]0xFEFF)
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

# One line of at most $Max characters that survives Windows PowerShell's native
# argument passing: it does not escape embedded double quotes, so they become
# single quotes, and a trailing backslash would escape the closing quote.
function ConvertTo-HerdrText($Value, [int]$Max) {
    if ($Value -isnot [string]) { return $null }
    $text = (($Value -replace '\s+', ' ').Trim()) -replace '"', "'"
    if ($text.Length -eq 0) { return $null }
    if ($text.Length -gt $Max) {
        $cut = $Max - 1
        if ([char]::IsHighSurrogate($text[$cut - 1])) { $cut-- }
        $text = $text.Substring(0, $cut) + [char]0x2026
    }
    if ($text.EndsWith('\')) { $text += ' ' }
    return $text
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
    if ($Action -eq "session") { exit 0 }
    if ($Action -eq "activity") {
        # Activity hints only tell herdr to re-read the Kimi session files. The
        # hook event name is the hint; background task ids map to "task:<id>"
        # nodes. Agent-kind task ids carry no subagent id, so they send no node.
        $activityArgs = @("pane", "report-agent-activity", $env:HERDR_PANE_ID, "--source", "herdr:kimi", "--agent", "kimi", "--seq", $seq)
        $hint = $payload.hook_event_name
        if ($hint -is [string] -and $hint -cmatch '^[A-Za-z0-9_.:-]{1,64}\z') { $activityArgs += @("--hint", $hint) }
        $taskId = $payload.task_id
        if ($null -eq $taskId -and $payload.source_kind -eq "background_task") { $taskId = $payload.source_id }
        if ($taskId -is [string] -and $taskId -cmatch '^[A-Za-z0-9_-]{1,128}\z' -and -not $taskId.StartsWith("agent-")) {
            $activityArgs += @("--node-id", "task:$taskId")
        }
        & $herdr @activityArgs 2>$null | Out-Null
        exit 0
    }
    # A turn that fails (provider or authentication errors) ends with StopFailure.
    # It reports idle like the other turn endings, and before that the failure
    # with its error type and message, so Herdr announces a failed turn rather
    # than a finished one. Attention reports keep their own sequence per source.
    if ($Action -eq "idle" -and $payload.hook_event_name -ceq "StopFailure") {
        $failureArgs = @("pane", "report-agent-attention", $env:HERDR_PANE_ID, "--source", "herdr:kimi", "--agent", "kimi", "--kind", "failed", "--seq", $seq)
        $errorType = ConvertTo-HerdrText $(if ($payload.error_type -is [string]) { $payload.error_type } else { $payload.errorType }) 200
        $errorMessage = ConvertTo-HerdrText $(if ($payload.error_message -is [string]) { $payload.error_message } else { $payload.errorMessage }) 2000
        if ($errorType) { $failureArgs += @("--title", $errorType) }
        if ($errorMessage) { $failureArgs += @("--detail", $errorMessage) }
        # The lifecycle report below must not depend on this one.
        try {
            & $herdr @failureArgs 2>$null | Out-Null
        } catch {
        }
    }
    if ([string]::IsNullOrWhiteSpace($sessionId)) {
        & $herdr pane report-agent $env:HERDR_PANE_ID --source herdr:kimi --agent kimi --state $Action --seq $seq 2>$null | Out-Null
    } else {
        & $herdr pane report-agent $env:HERDR_PANE_ID --source herdr:kimi --agent kimi --state $Action --agent-session-id $sessionId --seq $seq 2>$null | Out-Null
    }
} catch {
}
