# installed by herdr
# managed by herdr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# HERDR_INTEGRATION_ID=claude
# HERDR_INTEGRATION_VERSION=12

param([string]$Action = "")

# A failing report must never break Claude Code.
trap { exit 0 }

if (@("session", "activity", "attention") -notcontains $Action) { exit 0 }
if ($env:HERDR_ENV -ne "1") { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:HERDR_PANE_ID)) { exit 0 }
# Claude Code background sessions (`claude --bg`, or any session the Claude Code
# daemon hosts) inherit HERDR_PANE_ID from the pane that started the daemon, so
# reporting from them would pass them off as that pane's agent. The daemon marks
# them with CLAUDE_CODE_SESSION_KIND=bg and CLAUDE_JOB_DIR; Claude Code strips the
# former from the environment it gives hooks and keeps the latter, so either one
# means a background session. HERDR_REPORT_BG_SESSIONS=1 reports them anyway.
if ($env:HERDR_REPORT_BG_SESSIONS -ne "1" -and ($env:CLAUDE_CODE_SESSION_KIND -ceq "bg" -or -not [string]::IsNullOrEmpty($env:CLAUDE_JOB_DIR))) { exit 0 }

# Read the hook input as UTF-8 bytes; the console input code page may differ.
try {
    $buffer = New-Object IO.MemoryStream
    [Console]::OpenStandardInput().CopyTo($buffer)
    $inputText = [Text.Encoding]::UTF8.GetString($buffer.ToArray()).TrimStart([char]0xFEFF)
    $payload = if ([string]::IsNullOrWhiteSpace($inputText)) { $null } else { $inputText | ConvertFrom-Json }
} catch {
    exit 0
}

$propertyNames = @($payload.PSObject.Properties.Name)
if ((Test-Path Env:CURSOR_VERSION) -or $propertyNames -ccontains "cursor_version") { exit 0 }
$hookEvent = if ($propertyNames -ccontains "hook_event_name" -and $payload.hook_event_name -is [string]) { $payload.hook_event_name } else { "" }
$seq = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
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

# Herdr rejects attention text over 200 (title), 2000 (detail) and 120 (option)
# characters, and more than 8 options.
function Get-PermissionReport {
    $toolName = ConvertTo-HerdrText $payload.tool_name 200
    if (-not $toolName) { return $null }
    $toolInput = $payload.tool_input
    $questions = $toolInput.questions
    if ($toolName -ceq "AskUserQuestion" -and $questions -is [array] -and $questions.Count -gt 0) {
        $question = $questions[0]
        $detail = ConvertTo-HerdrText $question.question 2000
        if ($detail) {
            $options = @()
            if ($question.options -is [array]) {
                foreach ($option in $question.options) {
                    $label = ConvertTo-HerdrText $option.label 120
                    if ($label -and $options.Count -lt 8) { $options += $label }
                }
            }
            return @{ title = (ConvertTo-HerdrText $question.header 200); detail = $detail; options = $options }
        }
    }
    $summaryKey = $null
    $summary = $null
    foreach ($key in @("command", "file_path", "notebook_path", "path", "url", "query", "pattern", "description", "plan", "prompt")) {
        $value = $toolInput.$key
        if ($value -is [string] -and -not [string]::IsNullOrWhiteSpace($value)) {
            $summaryKey = $key
            $summary = ConvertTo-HerdrText (@($value -split "\r?\n" | Where-Object { $_.Trim() })[0]) 200
            break
        }
    }
    $title = if ($summaryKey -ne "description") { ConvertTo-HerdrText $toolInput.description 200 } else { $null }
    $detail = if ($summary) { "$($toolName): $summary" } else { $toolName }
    return @{ title = $title; detail = $detail; options = @() }
}

function Get-NotificationReport {
    if (@("permission_prompt", "elicitation_dialog") -cnotcontains $payload.notification_type) { return $null }
    $title = ConvertTo-HerdrText $payload.title 200
    $detail = ConvertTo-HerdrText $payload.message 2000
    if (-not $title -and -not $detail) { return $null }
    return @{ title = $title; detail = $detail; options = @() }
}

if ($Action -eq "attention") {
    # Details only: Claude's state still comes from Herdr's screen detection, and
    # Herdr shows these details only while the pane is blocked. The hook never
    # answers the prompt for Claude.
    #
    # Claude also announces a permission dialog with Notification(permission_prompt),
    # but only after 6 seconds without input, and that message only names the tool.
    # Every other report is numbered ten minutes ahead of the clock, so the
    # notification cannot replace the PermissionRequest detail of the same dialog:
    # Herdr ignores a report whose seq is not newer than the last one from this source.
    $lead = 600000
    if ($hookEvent -ceq "PermissionRequest") {
        $report = Get-PermissionReport
    } elseif ($hookEvent -ceq "Notification") {
        $report = Get-NotificationReport
        if ($payload.notification_type -ceq "permission_prompt") { $lead = 0 }
    } else {
        exit 0
    }
    if ($null -eq $report) { exit 0 }
    $reportArgs = @("pane", "report-agent-attention", $env:HERDR_PANE_ID, "--source", "herdr:claude", "--agent", "claude", "--kind", "needs_input", "--seq", [string]($seq + $lead))
    if ($report.title) { $reportArgs += @("--title", $report.title) }
    if ($report.detail) { $reportArgs += @("--detail", $report.detail) }
    foreach ($option in $report.options) { $reportArgs += @("--option", $option) }
    Invoke-Herdr $reportArgs
    exit 0
}

if ($Action -eq "activity") {
    # Activity hooks only tell herdr that the sub-agent/task tree changed; the
    # server rebuilds the tree from transcripts.
    if (@("SubagentStart", "SubagentStop") -ccontains $hookEvent) {
        $nodeId = ConvertTo-HerdrText $payload.agent_id 256
    } elseif (@("TaskCreated", "TaskCompleted") -ccontains $hookEvent) {
        $taskId = ConvertTo-HerdrText $payload.task_id 256
        $nodeId = if ($taskId) { "task:$taskId" } else { $null }
    } else {
        exit 0
    }
    $reportArgs = @("pane", "report-agent-activity", $env:HERDR_PANE_ID, "--source", "herdr:claude", "--agent", "claude", "--hint", $hookEvent, "--seq", "$seq")
    if ($nodeId) { $reportArgs += @("--node-id", $nodeId) }
    Invoke-Herdr $reportArgs
    exit 0
}

if ($hookEvent -cne "SessionStart") { exit 0 }
if (-not [string]::IsNullOrWhiteSpace($payload.agent_id)) { exit 0 }

$sessionId = $payload.session_id
if ([string]::IsNullOrWhiteSpace($sessionId)) { exit 0 }

$reportArgs = @(
    "pane",
    "report-agent-session",
    $env:HERDR_PANE_ID,
    "--source",
    "herdr:claude",
    "--agent",
    "claude",
    "--seq",
    "$seq",
    "--agent-session-id",
    "$sessionId"
)
if ($payload.transcript_path -is [string] -and -not [string]::IsNullOrWhiteSpace($payload.transcript_path)) {
    $reportArgs += @("--agent-session-path", "$($payload.transcript_path)")
}
if ($payload.source -is [string] -and -not [string]::IsNullOrWhiteSpace($payload.source)) {
    $reportArgs += @("--session-start-source", "$($payload.source)")
}
Invoke-Herdr $reportArgs
