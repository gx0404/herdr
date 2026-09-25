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

# Hooks inherit the CLI environment. Only absolute roots have unambiguous
# semantics here; an explicitly empty override is not the default home.
def session_directory():
    if not session_id or not re.fullmatch(r"[A-Za-z0-9_-]{1,128}", session_id):
        return None
    name = session_id if session_id.startswith("session_") else "session_" + session_id
    if name == "session_":
        return None
    root = os.environ.get("KIMI_CODE_HOME")
    if root is None:
        home = os.environ.get("HOME", "")
        root = os.path.join(home, ".kimi-code") if os.path.isabs(home) else ""
    if not os.path.isabs(root):
        return None
    sessions = os.path.realpath(os.path.join(root, "sessions"))
    found = None
    try:
        # Workspace aliases are authoritative; do not recalculate cwd hashes.
        # Count every entry and reject an incomplete scan, even after a match.
        with os.scandir(sessions) as buckets:
            for index, bucket in enumerate(buckets):
                if index >= 256:
                    return None
                if not bucket.is_dir(follow_symlinks=False):
                    continue
                candidate = os.path.join(bucket.path, name)
                if not os.path.isdir(candidate):
                    continue
                if found is not None or os.path.islink(candidate):
                    return None
                canonical = os.path.realpath(candidate)
                if os.path.commonpath([sessions, canonical]) != sessions:
                    return None
                state_path = os.path.join(candidate, "state.json")
                if os.path.islink(state_path) or not os.path.isfile(state_path):
                    return None
                with open(state_path, "rb") as state_file:
                    data = state_file.read(8 * 1024 * 1024 + 1)
                if len(data) > 8 * 1024 * 1024:
                    return None
                state = json.loads(data)
                if not isinstance(state, dict):
                    return None
                # v2 persists an ID; the legacy schema used workDir and no ID.
                if "id" in state:
                    if state["id"] != name:
                        return None
                elif "version" in state or not isinstance(state.get("workDir"), str):
                    return None
                found = canonical
    except (OSError, ValueError, TypeError, RecursionError):
        return None
    return found

session_path = session_directory()

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
    if session_path is not None:
        params["agent_session_path"] = session_path
else:
    method = "pane.report_agent"
    params["state"] = action
    if session_id is not None:
        params["agent_session_id"] = session_id

def send_report(method, params):
    request = json.dumps({"id": "herdr:kimi:" + str(params["seq"]), "method": method, "params": params})
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.settimeout(0.5)
            client.connect(os.environ["HERDR_SOCKET_PATH"])
            client.sendall((request + "\n").encode())
            client.recv(4096)
    except Exception:
        pass

# SessionStart may run before state.json exists. Later events supplement only
# that ID, never a fabricated startup/replacement, and must not consume the
# lifecycle reports sequence number (both share the same source deduplicator).
if action != "session" and session_path is not None:
    send_report("pane.report_agent_session", {
        "pane_id": params["pane_id"], "source": "herdr:kimi", "agent": "kimi",
        "seq": seq, "agent_session_id": session_id, "agent_session_path": session_path,
    })
    params["seq"] = seq + 1
send_report(method, params)
' "$action" 2>/dev/null || true
