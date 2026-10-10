#!/bin/sh
# installed by herdr
# managed by herdr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# HERDR_INTEGRATION_ID=codex
# HERDR_INTEGRATION_VERSION=10

set -eu

action="${1:-}"
hook_input_file="$(mktemp "${TMPDIR:-/tmp}/herdr-codex-hook.XXXXXX")" || exit 0
trap 'rm -f "$hook_input_file"' EXIT HUP INT TERM
cat >"$hook_input_file" 2>/dev/null || true

case "$action" in
  session|activity|attention|working|idle) ;;
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

source = "herdr:codex"
action = os.environ.get("HERDR_ACTION", "")
pane_id = os.environ.get("HERDR_PANE_ID")
socket_path = os.environ.get("HERDR_SOCKET_PATH")
hook_input_file = os.environ.get("HERDR_HOOK_INPUT_FILE")

if not pane_id or not socket_path:
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

hook_event_name = str(hook_input.get("hook_event_name") or "")
request_id = f"{source}:{int(time.time() * 1000)}:{random.randrange(1_000_000):06d}"
report_seq = time.time_ns()


def text_field(name):
    value = hook_input.get(name)
    return value if isinstance(value, str) and value else None


# Herdr rejects attention text over these lengths (characters).
TITLE_MAX_CHARS = 200
SUMMARY_MAX_CHARS = 200
# Tool input fields that name what an approval request is about when it has no
# command (Bash and apply_patch use `command`), best first.
SUMMARY_KEYS = ("file_path", "path", "url", "query", "pattern", "description", "prompt")
PATCH_FILE_MARKERS = ("*** Add File: ", "*** Update File: ", "*** Delete File: ")


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


def approval_summary(tool_input):
    """The key the summary came from, and a one-line summary of the request."""
    command = tool_input.get("command")
    if isinstance(command, list):
        command = " ".join(part for part in command if isinstance(part, str))
    if isinstance(command, str) and command.lstrip().startswith("*** Begin Patch"):
        files = [
            line[len(marker):].strip()
            for line in command.splitlines()
            for marker in PATCH_FILE_MARKERS
            if line.startswith(marker) and line[len(marker):].strip()
        ]
        more = f" +{len(files) - 3}" if len(files) > 3 else ""
        return "command", clip(", ".join(files[:3]) + more, SUMMARY_MAX_CHARS)
    if isinstance(command, str) and command.strip():
        return "command", clip(first_line(command), SUMMARY_MAX_CHARS)
    for key in SUMMARY_KEYS:
        value = tool_input.get(key)
        if isinstance(value, str) and value.strip():
            return key, clip(first_line(value), SUMMARY_MAX_CHARS)
    return None, None


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
    # PermissionRequest only adds the needs-input detail: Codex's blocked state
    # still comes from Herdr's screen detection, which shows the detail only while
    # the pane is blocked. The hook never answers the approval for Codex. A nested
    # codex process inherits CODEX_THREAD_ID from its parent; its approvals are
    # not this pane's, unless the payload names a sub-agent of this session.
    if hook_event_name != "PermissionRequest":
        raise SystemExit(0)
    inherited_session_id = os.environ.get("CODEX_THREAD_ID")
    if inherited_session_id and inherited_session_id != text_field("session_id") and not text_field("agent_id"):
        raise SystemExit(0)
    tool_name = clip(hook_input.get("tool_name"), TITLE_MAX_CHARS)
    if not tool_name:
        raise SystemExit(0)
    tool_input = hook_input.get("tool_input")
    if not isinstance(tool_input, dict):
        tool_input = {}
    summary_key, summary = approval_summary(tool_input)
    params = {
        "pane_id": pane_id,
        "source": source,
        "agent": "codex",
        "kind": "needs_input",
        "detail": f"{tool_name}: {summary}" if summary else tool_name,
        "seq": report_seq,
    }
    # `description` is the approval reason Codex gives with some requests.
    title = clip(tool_input.get("description"), TITLE_MAX_CHARS) if summary_key != "description" else None
    if title:
        params["title"] = title
    send({
        "id": request_id,
        "method": "pane.report_agent_attention",
        "params": params,
    })
    raise SystemExit(0)

if action == "activity":
    # Activity hooks only tell herdr that this pane's sub-agent threads changed;
    # the server rebuilds the tree from Codex's own session files. SubagentStart
    # and SubagentStop carry the spawned thread id as agent_id (codex-cli 0.155.1
    # hook schema), which is the activity node id. A nested codex process can
    # only cause one extra refresh of the same tree, so the CODEX_THREAD_ID guard
    # used for session identity does not apply here.
    if hook_event_name not in ("SubagentStart", "SubagentStop"):
        raise SystemExit(0)
    params = {
        "pane_id": pane_id,
        "source": source,
        "agent": "codex",
        "hint": hook_event_name,
        "seq": report_seq,
    }
    node_id = text_field("agent_id")
    if node_id:
        params["node_id"] = node_id
    send({
        "id": request_id,
        "method": "pane.report_agent_activity",
        "params": params,
    })
    raise SystemExit(0)

expected_events = {"session": ("SessionStart",), "working": ("UserPromptSubmit",), "idle": ("Stop", "Interrupt")}[action]
if hook_event_name and hook_event_name not in expected_events:
    raise SystemExit(0)

session_id = hook_input.get("session_id")
agent_session_id = session_id if isinstance(session_id, str) and session_id else None
if action == "session":
    transcript_path = hook_input.get("transcript_path")
    if not isinstance(transcript_path, str) or not transcript_path.strip():
        raise SystemExit(0)
inherited_session_id = os.environ.get("CODEX_THREAD_ID")
if inherited_session_id and inherited_session_id != agent_session_id:
    raise SystemExit(0)
session_start_source = hook_input.get("source") if action == "session" else None
if not isinstance(session_start_source, str) or not session_start_source:
    session_start_source = None
if agent_session_id:
    params = {
        "pane_id": pane_id,
        "source": source,
        "agent": "codex",
        "seq": report_seq,
        "agent_session_id": agent_session_id,
    }
    if action == "session":
        # fork：会话上报连同 transcript 路径，CODEX_HOME 只设在 pane 里时活动发现
        # 仍能跟上；working/idle 分支不定义 transcript_path，不能带进 params。
        params["agent_session_path"] = transcript_path
        if session_start_source:
            params["session_start_source"] = session_start_source
        method = "pane.report_agent_session"
    else:
        params["state"] = action
        method = "pane.report_agent"
    request = {
        "id": request_id,
        "method": method,
        "params": params,
    }
else:
    raise SystemExit(0)

send(request)
PY
