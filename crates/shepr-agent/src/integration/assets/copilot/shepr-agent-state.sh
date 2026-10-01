#!/bin/sh
# installed by shepr
# managed by shepr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# SHEPR_INTEGRATION_ID=copilot
# SHEPR_INTEGRATION_VERSION=5

set -eu

hook_input_file="$(mktemp "${TMPDIR:-/tmp}/shepr-copilot-hook.XXXXXX")" || {
  cat >/dev/null 2>/dev/null || true
  exit 0
}
trap 'rm -f "$hook_input_file"' 0
trap 'exit 0' HUP INT TERM
cat >"$hook_input_file" 2>/dev/null || true

# Shared agent configs contain release hooks only. Dev panes use detection.
[ "${SHEPR_BUILD_PROFILE:-}" = "release" ] || exit 0
[ "${SHEPR_ENV:-}" = "1" ] || exit 0
[ -n "${SHEPR_SOCKET_PATH:-}" ] || exit 0
[ -n "${SHEPR_PANE_ID:-}" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0

# A python failure must not fail the hook: under `set -eu` it would exit
# non-zero with a traceback on stderr, which the agent may show to the user.
SHEPR_HOOK_INPUT_FILE="$hook_input_file" python3 - 2>/dev/null <<'PY' || true
import json
import os
import socket
import time

source = "shepr:copilot"
pane_id = os.environ.get("SHEPR_PANE_ID")
socket_path = os.environ.get("SHEPR_SOCKET_PATH")
hook_input_file = os.environ.get("SHEPR_HOOK_INPUT_FILE")

if not pane_id or not socket_path:
    raise SystemExit(0)

hook_input = {}
if hook_input_file:
    try:
        with open(hook_input_file, encoding="utf-8") as handle:
            content = handle.read()
        if content.strip():
            parsed = json.loads(content)
            if isinstance(parsed, dict):
                hook_input = parsed
    except Exception:
        hook_input = {}

def first_text(*keys):
    for key in keys:
        value = hook_input.get(key)
        if isinstance(value, str) and value:
            return value
    return None

def normalize_event(event):
    return event.replace("_", "").replace("-", "").lower()

event = first_text("hook_event_name", "hookEventName")
if event:
    if normalize_event(event) != "sessionstart":
        raise SystemExit(0)
elif "prompt" in hook_input or first_text("tool_name", "toolName", "notification_type", "notificationType", "stop_reason", "stopReason", "reason"):
    raise SystemExit(0)

session_id = hook_input.get("session_id")
if not isinstance(session_id, str) or not session_id:
    session_id = hook_input.get("sessionId")
if not isinstance(session_id, str) or not session_id:
    raise SystemExit(0)

report_seq = time.time_ns()
request = {
    "id": f"{source}:{report_seq}",
    "method": "pane.report_agent_session",
    "params": {
        "pane_id": pane_id,
        "source": source,
        "agent": "copilot",
        "agent_session_id": session_id,
        "seq": report_seq,
    },
}

try:
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.settimeout(0.5)
    client.connect(socket_path)
    client.sendall((json.dumps(request) + "\n").encode("utf-8"))
    try:
        client.recv(4096)
    except Exception:
        pass
    client.close()
except Exception:
    pass
PY
