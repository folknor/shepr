#!/bin/sh
# installed by shepr
# managed by shepr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# SHEPR_INTEGRATION_ID=mastracode
# SHEPR_INTEGRATION_VERSION=4

set -eu

# Stamp the report the moment the hook starts. Every event runs this script in
# a fresh process, and shepr drops a report whose seq is older than the last
# one it accepted, so taking the timestamp after python3 has started would let
# interpreter startup jitter reorder near-simultaneous events (a PreToolUse
# followed at once by a PermissionRequest).
hook_seq="$(date +%s%N 2>/dev/null || true)"

action="${1:-}"
hook_input_file="$(mktemp "${TMPDIR:-/tmp}/shepr-mastracode-hook.XXXXXX")" || exit 0
trap 'rm -f "$hook_input_file"' EXIT HUP INT TERM
cat >"$hook_input_file" 2>/dev/null || true

case "$action" in
  session|working|idle|blocked) ;;
  *) exit 0 ;;
esac

[ "${SHEPR_ENV:-}" = "1" ] || exit 0
[ -n "${SHEPR_SOCKET_PATH:-}" ] || exit 0
[ -n "${SHEPR_PANE_ID:-}" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0

# A python failure must not fail the hook: under `set -eu` it would exit
# non-zero with a traceback on stderr, which the agent may show to the user.
SHEPR_ACTION="$action" SHEPR_HOOK_INPUT_FILE="$hook_input_file" SHEPR_HOOK_SEQ="$hook_seq" python3 - 2>/dev/null <<'PY' || true
import json
import os
import random
import socket
import time

source = "shepr:mastracode"
action = os.environ.get("SHEPR_ACTION", "")
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
            hook_input = json.loads(content)
    except Exception:
        hook_input = {}
# A valid JSON body that is not an object (a list, a string, null) carries no
# fields we can read; treat it as empty so state reports still go through.
if not isinstance(hook_input, dict):
    hook_input = {}

request_id = f"{source}:{int(time.time() * 1000)}:{random.randrange(1_000_000):06d}"
raw_seq = os.environ.get("SHEPR_HOOK_SEQ", "")
# `date` without %N support prints a literal N; fall back to our own clock.
report_seq = int(raw_seq) if raw_seq.isdigit() else time.time_ns()
session_id = hook_input.get("session_id")
if isinstance(session_id, str) and session_id:
    agent_session_id = session_id
else:
    agent_session_id = None
if action == "session":
    if not agent_session_id:
        raise SystemExit(0)
    # Pass MastraCode's own start source through when it sends one (shepr
    # ignores values it does not know); a bare SessionStart is a fresh start.
    session_start_source = hook_input.get("source")
    if not isinstance(session_start_source, str) or not session_start_source:
        session_start_source = "startup"
    request = {
        "id": request_id,
        "method": "pane.report_agent_session",
        "params": {
            "pane_id": pane_id,
            "source": source,
            "agent": "mastracode",
            "agent_session_id": agent_session_id,
            "session_start_source": session_start_source,
            "seq": report_seq,
        },
    }
else:
    request = {
        "id": request_id,
        "method": "pane.report_agent",
        "params": {
            "pane_id": pane_id,
            "source": source,
            "agent": "mastracode",
            "state": action,
            "seq": report_seq,
        },
    }
    if agent_session_id:
        request["params"]["agent_session_id"] = agent_session_id

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
PY
