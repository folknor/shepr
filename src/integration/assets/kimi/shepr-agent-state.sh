#!/bin/sh
# managed by shepr; reinstalling the integration replaces this file.
# SHEPR_INTEGRATION_ID=kimi
# SHEPR_INTEGRATION_VERSION=3

# Stamp the report the moment the hook starts. Every event runs this script in
# a fresh process, and shepr drops a report whose seq is older than the last
# one it accepted, so taking the timestamp after python3 has started would let
# interpreter startup jitter reorder near-simultaneous events (a PreToolUse
# followed at once by a PermissionRequest).
hook_seq="$(date +%s%N 2>/dev/null || true)"

action="${1:-}"
case "$action" in
  session|working|blocked|idle) ;;
  *) exit 0 ;;
esac

[ "${SHEPR_ENV:-}" = "1" ] || exit 0
[ -n "${SHEPR_SOCKET_PATH:-}" ] || exit 0
[ -n "${SHEPR_PANE_ID:-}" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0

python3 -c '
import json
import os
import socket
import sys
import time

action = sys.argv[1]
try:
    payload = json.load(sys.stdin)
except Exception:
    payload = {}
# A valid JSON body that is not an object (a list, a string, null) carries no
# fields we can read; treat it as empty so state reports still go through.
if not isinstance(payload, dict):
    payload = {}

session_id = payload.get("session_id")
if not isinstance(session_id, str) or not session_id:
    session_id = None

raw_seq = sys.argv[2] if len(sys.argv) > 2 else ""
# `date` without %N support prints a literal N; fall back to our own clock.
seq = int(raw_seq) if raw_seq.isdigit() else time.time_ns()
params = {
    "pane_id": os.environ["SHEPR_PANE_ID"],
    "source": "shepr:kimi",
    "agent": "kimi",
    "seq": seq,
}
if action == "session":
    if session_id is None:
        raise SystemExit(0)
    method = "pane.report_agent_session"
    # Pass the start source Kimi reports through when there is one (shepr
    # ignores values it does not know); a bare SessionStart is a fresh start.
    start_source = payload.get("source")
    if not isinstance(start_source, str) or not start_source:
        start_source = "startup"
    params["session_start_source"] = start_source
else:
    method = "pane.report_agent"
    params["state"] = action
if session_id is not None:
    params["agent_session_id"] = session_id

request = json.dumps({"id": f"shepr:kimi:{seq}", "method": method, "params": params})
try:
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(0.5)
        client.connect(os.environ["SHEPR_SOCKET_PATH"])
        client.sendall((request + "\n").encode())
        client.recv(4096)
except Exception:
    pass
' "$action" "$hook_seq" 2>/dev/null || true
