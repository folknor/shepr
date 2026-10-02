#!/bin/sh
# installed by shepr
# managed by shepr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# SHEPR_INTEGRATION_ID=claude
# SHEPR_INTEGRATION_VERSION=5

set -eu

action="${1:-}"
hook_input_file="$(mktemp "${TMPDIR:-/tmp}/shepr-claude-hook.XXXXXX")" || {
  cat >/dev/null 2>/dev/null || true
  exit 0
}
trap 'rm -f "$hook_input_file"' 0
trap 'exit 0' HUP INT TERM
cat >"$hook_input_file" 2>/dev/null || true

case "$action" in
  session) ;;
  *) exit 0 ;;
esac

# Shared agent configs contain release hooks only. Dev panes use detection.
[ "${SHEPR_BUILD_PROFILE:-}" = "release" ] || exit 0
[ "${SHEPR_ENV:-}" = "1" ] || exit 0
[ -n "${SHEPR_SOCKET_PATH:-}" ] || exit 0
[ -n "${SHEPR_PANE_ID:-}" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0

# A python failure must not fail the hook: under `set -eu` it would exit
# non-zero with a traceback on stderr, which Claude Code shows to the user.
SHEPR_ACTION="$action" SHEPR_HOOK_INPUT_FILE="$hook_input_file" python3 - 2>/dev/null <<'PY' || true
import json
import os
import socket
import time

source = "shepr:claude"
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
# fields we can read; treat it as empty.
if not isinstance(hook_input, dict):
    hook_input = {}

if "CURSOR_VERSION" in os.environ or "cursor_version" in hook_input:
    raise SystemExit(0)
hook_event_name = str(hook_input.get("hook_event_name") or "")
if hook_event_name != "SessionStart":
    raise SystemExit(0)
is_subagent = bool(hook_input.get("agent_id"))
if is_subagent:
    raise SystemExit(0)
report_seq = time.time_ns()
request_id = f"{source}:{report_seq}"
session_id = hook_input.get("session_id")
agent_session_id = session_id if isinstance(session_id, str) and session_id else None
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
    if session_start_source:
        params["session_start_source"] = session_start_source
    request = {
        "id": request_id,
        "method": "pane.report_agent_session",
        "params": params,
    }
else:
    raise SystemExit(0)

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
