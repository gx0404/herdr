#!/bin/sh
# managed by herdr; reinstalling the integration replaces this file.
# HERDR_INTEGRATION_ID=kimi
# HERDR_INTEGRATION_VERSION=8

action="${1:-}"
case "$action" in
  session|working|blocked|idle|activity) ;;
  *) exit 0 ;;
esac

[ "${HERDR_ENV:-}" = "1" ] || exit 0
[ -n "${HERDR_SOCKET_PATH:-}" ] || exit 0
[ -n "${HERDR_PANE_ID:-}" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0

python3 -c '
import json
import os
import re
import socket
import sys
import time

action = sys.argv[1]
try:
    payload = json.load(sys.stdin)
except Exception:
    payload = {}
if not isinstance(payload, dict):
    payload = {}

session_id = payload.get("session_id")
if not isinstance(session_id, str) or not session_id:
    session_id = None

seq = time.time_ns()
params = {
    "pane_id": os.environ["HERDR_PANE_ID"],
    "source": "herdr:kimi",
    "agent": "kimi",
    "seq": seq,
}
if action == "activity":
    # Activity hints only tell herdr to re-read the Kimi session files. The
    # hook event name is the hint; background task ids map to "task:<id>"
    # nodes. Agent-kind task ids carry no subagent id, so they send no node.
    method = "pane.report_agent_activity"
    hint = payload.get("hook_event_name")
    if isinstance(hint, str) and re.fullmatch(r"[A-Za-z0-9_.:-]{1,64}", hint):
        params["hint"] = hint
    task_id = payload.get("task_id")
    if task_id is None and payload.get("source_kind") == "background_task":
        task_id = payload.get("source_id")
    if (
        isinstance(task_id, str)
        and re.fullmatch(r"[A-Za-z0-9_-]{1,128}", task_id)
        and not task_id.startswith("agent-")
    ):
        params["node_id"] = "task:" + task_id
elif action == "session":
    if session_id is None:
        raise SystemExit(0)
    method = "pane.report_agent_session"
    params["session_start_source"] = "startup"
    params["agent_session_id"] = session_id
else:
    method = "pane.report_agent"
    params["state"] = action
    if session_id is not None:
        params["agent_session_id"] = session_id

request = json.dumps({"id": f"herdr:kimi:{seq}", "method": method, "params": params})
try:
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(0.5)
        client.connect(os.environ["HERDR_SOCKET_PATH"])
        client.sendall((request + "\n").encode())
        client.recv(4096)
except Exception:
    pass
' "$action" 2>/dev/null || true
