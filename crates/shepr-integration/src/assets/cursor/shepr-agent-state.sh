#!/bin/sh
# managed by shepr; reinstalling the integration replaces this file.
# SHEPR_INTEGRATION_ID=cursor
# SHEPR_INTEGRATION_VERSION=4

hook_input="$(cat 2>/dev/null || true)"

[ "${1:-}" = "session" ] || exit 0
# Shared agent configs contain release hooks only. Dev panes use detection.
[ "${SHEPR_BUILD_PROFILE:-}" = "release" ] || exit 0
[ "${SHEPR_ENV:-}" = "1" ] || exit 0
[ -n "${SHEPR_SOCKET_PATH:-}" ] || exit 0
[ -n "${SHEPR_PANE_ID:-}" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0

printf '%s' "$hook_input" | python3 -c '
import json
import os
import socket
import sys
import time

try:
    payload = json.load(sys.stdin)
except Exception:
    raise SystemExit(0)

# A valid JSON body that is not an object (a list, a string, null) carries no
# session id; do not rely on stderr suppression to hide the AttributeError.
if not isinstance(payload, dict):
    raise SystemExit(0)

def first_text(*names):
    for name in names:
        value = payload.get(name)
        if isinstance(value, str) and value:
            return value
    return None

event = first_text("hook_event_name", "hookEventName")
if event not in (None, "sessionStart"):
    raise SystemExit(0)

session_id = first_text("session_id", "sessionId", "conversation_id", "conversationId")
if session_id is None:
    raise SystemExit(0)

seq = time.time_ns()
request = json.dumps({
    "id": f"shepr:cursor:{seq}",
    "method": "pane.report_agent_session",
    "params": {
        "pane_id": os.environ["SHEPR_PANE_ID"],
        "source": "shepr:cursor",
        "agent": "cursor",
        "seq": seq,
        "agent_session_id": session_id,
    },
})
try:
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(0.5)
        client.connect(os.environ["SHEPR_SOCKET_PATH"])
        client.sendall((request + "\n").encode())
        client.recv(4096)
except Exception:
    pass
' 2>/dev/null || true
