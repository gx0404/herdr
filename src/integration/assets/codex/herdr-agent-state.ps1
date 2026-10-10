# installed by herdr
# managed by herdr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# HERDR_INTEGRATION_ID=codex
# HERDR_INTEGRATION_VERSION=10

param([string]$Action = "")

# A failing report must never break Codex.
trap { exit 0 }

if ($Action -notin @("session", "working", "idle", "activity", "attention")) { exit 0 }
if ($env:HERDR_ENV -ne "1") { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:HERDR_PANE_ID)) { exit 0 }

# Read the hook input as UTF-8 bytes; the console input code page may differ.
try {
    $buffer = New-Object IO.MemoryStream
    [Console]::OpenStandardInput().CopyTo($buffer)
    $inputText = [Text.Encoding]::UTF8.GetString($buffer.ToArray()).TrimStart([char]0xFEFF)
    $payload = if ([string]::IsNullOrWhiteSpace($inputText)) { $null } else { $inputText | ConvertFrom-Json }
} catch {
    exit 0
}

$hookEvent = if ($payload.hook_event_name -is [string]) { $payload.hook_event_name } else { "" }
$seq = [DateTimeOffset]::UtcNow.Ticks
$herdr = if ([string]::IsNullOrWhiteSpace($env:HERDR_BIN_PATH)) { "herdr" } else { $env:HERDR_BIN_PATH }

function Invoke-Herdr([string[]]$Arguments) {
    try {
        & $herdr @Arguments 2>$null | Out-Null
    } catch {
    }
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

function Get-FirstLine([string]$Text) {
    return @($Text -split "\r?\n" | Where-Object { $_.Trim() })[0]
}

if ($Action -eq "attention") {
    # PermissionRequest only adds the needs-input detail: Codex's blocked state
    # still comes from Herdr's screen detection, which shows the detail only while
    # the pane is blocked. The hook never answers the approval for Codex. A nested
    # codex process inherits CODEX_THREAD_ID from its parent; its approvals are not
    # this pane's, unless the payload names a sub-agent of this session.
    if ($hookEvent -cne "PermissionRequest") { exit 0 }
    if (-not [string]::IsNullOrWhiteSpace($env:CODEX_THREAD_ID) -and $env:CODEX_THREAD_ID -ne $payload.session_id -and [string]::IsNullOrWhiteSpace($payload.agent_id)) { exit 0 }
    # Herdr rejects a title over 200 and a detail over 2000 characters.
    $toolName = ConvertTo-HerdrText $payload.tool_name 200
    if (-not $toolName) { exit 0 }
    $toolInput = $payload.tool_input
    $command = $toolInput.command
    if ($command -is [array]) { $command = (@($command | Where-Object { $_ -is [string] })) -join " " }
    $summaryKey = $null
    $summary = $null
    if ($command -is [string] -and $command.TrimStart().StartsWith("*** Begin Patch")) {
        $files = @()
        foreach ($line in ($command -split "\r?\n")) {
            if ($line -match '^\*\*\* (Add|Update|Delete) File: (.+)$' -and $Matches[2].Trim()) { $files += $Matches[2].Trim() }
        }
        $more = if ($files.Count -gt 3) { " +$($files.Count - 3)" } else { "" }
        $summaryKey = "command"
        $summary = ConvertTo-HerdrText ((@($files | Select-Object -First 3) -join ", ") + $more) 200
    } elseif ($command -is [string] -and -not [string]::IsNullOrWhiteSpace($command)) {
        $summaryKey = "command"
        $summary = ConvertTo-HerdrText (Get-FirstLine $command) 200
    } else {
        foreach ($key in @("file_path", "path", "url", "query", "pattern", "description", "prompt")) {
            $value = $toolInput.$key
            if ($value -is [string] -and -not [string]::IsNullOrWhiteSpace($value)) {
                $summaryKey = $key
                $summary = ConvertTo-HerdrText (Get-FirstLine $value) 200
                break
            }
        }
    }
    $detail = if ($summary) { "$($toolName): $summary" } else { $toolName }
    $reportArgs = @("pane", "report-agent-attention", $env:HERDR_PANE_ID, "--source", "herdr:codex", "--agent", "codex", "--kind", "needs_input", "--detail", $detail, "--seq", "$seq")
    # `description` is the approval reason Codex gives with some requests.
    if ($summaryKey -ne "description") {
        $title = ConvertTo-HerdrText $toolInput.description 200
        if ($title) { $reportArgs += @("--title", $title) }
    }
    Invoke-Herdr $reportArgs
    exit 0
}

if ($Action -eq "activity") {
    # Activity hooks only tell herdr that this pane's sub-agent threads changed;
    # the server rebuilds the tree from Codex's own session files. The spawned
    # thread id (agent_id) is the activity node id.
    if (@("SubagentStart", "SubagentStop") -cnotcontains $hookEvent) { exit 0 }
    $reportArgs = @("pane", "report-agent-activity", $env:HERDR_PANE_ID, "--source", "herdr:codex", "--agent", "codex", "--hint", $hookEvent, "--seq", "$seq")
    $nodeId = ConvertTo-HerdrText $payload.agent_id 256
    if ($nodeId) { $reportArgs += @("--node-id", $nodeId) }
    Invoke-Herdr $reportArgs
    exit 0
}

$expectedEvents = @{ session = @("SessionStart"); working = @("UserPromptSubmit"); idle = @("Stop", "Interrupt") }[$Action]
if ($payload.hook_event_name -and $payload.hook_event_name -notin $expectedEvents) { exit 0 }

$sessionId = $payload.session_id
if ([string]::IsNullOrWhiteSpace($sessionId)) { exit 0 }
if ($Action -eq "session" -and [string]::IsNullOrWhiteSpace($payload.transcript_path)) { exit 0 }
if (-not [string]::IsNullOrWhiteSpace($env:CODEX_THREAD_ID) -and $env:CODEX_THREAD_ID -ne $sessionId) { exit 0 }

$reportArgs = @(
    "pane",
    $(if ($Action -eq "session") { "report-agent-session" } else { "report-agent" }),
    $env:HERDR_PANE_ID,
    "--source",
    "herdr:codex",
    "--agent",
    "codex",
    "--seq",
    "$seq",
    "--agent-session-id",
    "$sessionId",
    "--agent-session-path",
    "$($payload.transcript_path)"
)
if ($Action -ne "session") { $reportArgs += @("--state", $Action) }
if ($payload.hook_event_name -eq "SessionStart" -and $payload.source -is [string] -and -not [string]::IsNullOrWhiteSpace($payload.source)) {
    $reportArgs += @("--session-start-source", "$($payload.source)")
}
Invoke-Herdr $reportArgs
