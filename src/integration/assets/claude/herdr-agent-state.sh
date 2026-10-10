#!/bin/sh
# installed by herdr
# managed by herdr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# HERDR_INTEGRATION_ID=claude
# HERDR_INTEGRATION_VERSION=12

set -eu

action="${1:-}"
hook_input_file="$(mktemp "${TMPDIR:-/tmp}/herdr-claude-hook.XXXXXX")" || exit 0
trap 'rm -f "$hook_input_file"' EXIT HUP INT TERM
cat >"$hook_input_file" 2>/dev/null || true

case "$action" in
  session|activity|attention) ;;
  *) exit 0 ;;
esac

[ "${HERDR_ENV:-}" = "1" ] || exit 0
[ -n "${HERDR_SOCKET_PATH:-}" ] || exit 0
[ -n "${HERDR_PANE_ID:-}" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0

HERDR_ACTION="$action" HERDR_HOOK_INPUT_FILE="$hook_input_file" python3 - <<'PY' || true
import json
import os
import random
import socket
import time

source = "herdr:claude"
action = os.environ.get("HERDR_ACTION", "")
pane_id = os.environ.get("HERDR_PANE_ID")
socket_path = os.environ.get("HERDR_SOCKET_PATH")
hook_input_file = os.environ.get("HERDR_HOOK_INPUT_FILE")

if not pane_id or not socket_path:
    raise SystemExit(0)

# Claude Code background sessions (`claude --bg`, or any session the Claude Code
# daemon hosts) inherit HERDR_PANE_ID and HERDR_SOCKET_PATH from the pane that
# started the daemon, so reporting from them would pass them off as that pane's
# agent. The daemon marks them with CLAUDE_CODE_SESSION_KIND=bg and
# CLAUDE_JOB_DIR; Claude Code strips the former from the environment it gives
# hooks (seen with Claude Code 2.1.281) and keeps the latter, so either one
# means a background session. HERDR_REPORT_BG_SESSIONS=1 reports them anyway.
if os.environ.get("HERDR_REPORT_BG_SESSIONS") != "1" and (
    os.environ.get("CLAUDE_CODE_SESSION_KIND") == "bg" or os.environ.get("CLAUDE_JOB_DIR")
):
    raise SystemExit(0)

hook_input = {}
if hook_input_file:
    try:
        with open(hook_input_file, encoding="utf-8") as handle:
            content = handle.read()
        if content.strip():
            hook_input = json.loads(content)
    except Exception:
        hook_input = {}
if not isinstance(hook_input, dict):
    hook_input = {}

if "CURSOR_VERSION" in os.environ or "cursor_version" in hook_input:
    raise SystemExit(0)
hook_event_name = str(hook_input.get("hook_event_name") or "")
request_id = f"{source}:{int(time.time() * 1000)}:{random.randrange(1_000_000):06d}"
report_seq = time.time_ns()

# Herdr rejects attention text over these lengths (characters).
TITLE_MAX_CHARS = 200
DETAIL_MAX_CHARS = 2000
OPTION_MAX_CHARS = 120
MAX_OPTIONS = 8
SUMMARY_MAX_CHARS = 200
# Tool input fields that name what a permission request is about, best first.
SUMMARY_KEYS = (
    "command",
    "file_path",
    "notebook_path",
    "path",
    "url",
    "query",
    "pattern",
    "description",
    "plan",
    "prompt",
)
# Claude also announces a permission dialog with Notification(permission_prompt),
# but only after 6 seconds without input, and that message only names the tool.
# Every other needs-input report is numbered this far ahead of the clock, so the
# notification cannot replace the PermissionRequest detail of the same dialog:
# Herdr ignores a report whose seq is not newer than the last one from this
# source. Without a detailed report in the last ten minutes it still lands.
DETAIL_SEQ_LEAD_NS = 600 * 1_000_000_000


def text_field(name):
    value = hook_input.get(name)
    return value if isinstance(value, str) and value else None


def clip(value, limit):
    if not isinstance(value, str):
        return None
    text = " ".join(value.split())
    if not text:
        return None
    return text if len(text) <= limit else text[: limit - 1] + "\u2026"


def first_line(text):
    for line in text.splitlines():
        if line.strip():
            return line
    return text


def permission_report():
    tool_name = clip(hook_input.get("tool_name"), TITLE_MAX_CHARS)
    if not tool_name:
        return None
    tool_input = hook_input.get("tool_input")
    if not isinstance(tool_input, dict):
        tool_input = {}
    questions = tool_input.get("questions")
    if tool_name == "AskUserQuestion" and isinstance(questions, list) and questions:
        question = questions[0] if isinstance(questions[0], dict) else {}
        choices = question.get("options")
        options = []
        for option in choices if isinstance(choices, list) else []:
            label = clip(option.get("label"), OPTION_MAX_CHARS) if isinstance(option, dict) else None
            if label and len(options) < MAX_OPTIONS:
                options.append(label)
        detail = clip(question.get("question"), DETAIL_MAX_CHARS)
        if detail:
            return {"title": clip(question.get("header"), TITLE_MAX_CHARS), "detail": detail, "options": options}
    summary_key = next(
        (key for key in SUMMARY_KEYS if isinstance(tool_input.get(key), str) and tool_input[key].strip()),
        None,
    )
    summary = clip(first_line(tool_input[summary_key]), SUMMARY_MAX_CHARS) if summary_key else None
    title = clip(tool_input.get("description"), TITLE_MAX_CHARS) if summary_key != "description" else None
    return {"title": title, "detail": f"{tool_name}: {summary}" if summary else tool_name}


def notification_report():
    if text_field("notification_type") not in ("permission_prompt", "elicitation_dialog"):
        return None
    report = {
        "title": clip(hook_input.get("title"), TITLE_MAX_CHARS),
        "detail": clip(hook_input.get("message"), DETAIL_MAX_CHARS),
    }
    return report if report["title"] or report["detail"] else None


def send(request):
    try:
        client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        client.settimeout(0.5)
        client.connect(socket_path)
        client.sendall((json.dumps(request) + "\n").encode())
        try:
            client.recv(4096)
        except Exception:
            pass
        client.close()
    except Exception:
        pass


if action == "attention":
    # Details only: Claude's state still comes from Herdr's screen detection, and
    # Herdr shows these details only while the pane is blocked, dropping them when
    # it leaves that state. The hook never answers the prompt for Claude.
    if hook_event_name == "PermissionRequest":
        report = permission_report()
        seq = report_seq + DETAIL_SEQ_LEAD_NS
    elif hook_event_name == "Notification":
        report = notification_report()
        lead = 0 if text_field("notification_type") == "permission_prompt" else DETAIL_SEQ_LEAD_NS
        seq = report_seq + lead
    else:
        raise SystemExit(0)
    if not report:
        raise SystemExit(0)
    params = {
        "pane_id": pane_id,
        "source": source,
        "agent": "claude",
        "kind": "needs_input",
        "seq": seq,
    }
    for name in ("title", "detail", "options"):
        if report.get(name):
            params[name] = report[name]
    send({
        "id": request_id,
        "method": "pane.report_agent_attention",
        "params": params,
    })
    raise SystemExit(0)

if action == "activity":
    # Activity hooks only tell herdr that the sub-agent/task tree changed; the
    # server rebuilds the tree from transcripts. SubagentStart/SubagentStop
    # always carry the spawned agent's agent_id, and Task* events carry the
    # caller's agent_id when fired inside a sub-agent, so the SessionStart
    # "agent_id means sub-agent, drop it" rule must not apply here.
    if hook_event_name in ("SubagentStart", "SubagentStop"):
        node_id = text_field("agent_id")
    elif hook_event_name in ("TaskCreated", "TaskCompleted"):
        task_id = text_field("task_id")
        node_id = f"task:{task_id}" if task_id else None
    else:
        raise SystemExit(0)
    params = {
        "pane_id": pane_id,
        "source": source,
        "agent": "claude",
        "hint": hook_event_name,
        "seq": report_seq,
    }
    if node_id:
        params["node_id"] = node_id
    send({
        "id": request_id,
        "method": "pane.report_agent_activity",
        "params": params,
    })
    raise SystemExit(0)

if hook_event_name != "SessionStart":
    raise SystemExit(0)
is_subagent = bool(hook_input.get("agent_id"))
if is_subagent:
    raise SystemExit(0)
session_id = hook_input.get("session_id")
agent_session_id = session_id if isinstance(session_id, str) and session_id else None
transcript_path = hook_input.get("transcript_path")
agent_session_path = transcript_path if isinstance(transcript_path, str) and transcript_path else None
session_start_source = hook_input.get("source") if hook_event_name == "SessionStart" else None
if not isinstance(session_start_source, str) or not session_start_source:
    session_start_source = None
if agent_session_id:
    params = {
        "pane_id": pane_id,
        "source": source,
        "agent": "claude",
        "seq": report_seq,
        "agent_session_id": agent_session_id,
    }
    if agent_session_path:
        params["agent_session_path"] = agent_session_path
    if session_start_source:
        params["session_start_source"] = session_start_source
    request = {
        "id": request_id,
        "method": "pane.report_agent_session",
        "params": params,
    }
else:
    raise SystemExit(0)

send(request)
PY
