#!/bin/sh
# installed by shepr
# managed by shepr; every release shepr server launch on this host rewrites this file.
# add custom hooks beside this file instead of editing it.

set -eu

# Every exit path of the hook ends here, so the agent always sees a clean exit.
finish() {
  cat >/dev/null 2>/dev/null || true
  exit 0
}

action="${1:-}"
trap 'finish' HUP INT TERM

case "$action" in
  session) ;;
  *) finish ;;
esac
# Shared agent configs contain release hooks only. Dev panes use detection.
[ "${SHEPR_BUILD_PROFILE:-}" = "release" ] || finish
[ "${SHEPR_ENV:-}" = "1" ] || finish
[ -n "${SHEPR_SOCKET_PATH:-}" ] || finish
[ -n "${SHEPR_PANE_ID:-}" ] || finish
command -v python3 >/dev/null 2>&1 || finish

# A python failure must not fail the hook: under `set -eu` it would exit
# non-zero with a traceback on stderr, which the agent may show to the user.
SHEPR_ACTION="$action" SHEPR_HOOK_SEQ="${hook_seq:-}" python3 -c '
import json
import sys
from types import SimpleNamespace
import os
import socket
import time

SOURCE = "shepr:cursor"
METHOD_SESSION = "pane.report_agent_session"
METHOD_STATE = "pane.report_agent"
START = SimpleNamespace(**{"startup": "startup", "resume": "resume", "select": "select"})
ACTION_SESSION = "session"
SOCKET_WAIT_SECONDS = 0.5
# The hook events this integration is registered for, per action and in all.
EVENTS_BY_ACTION = {"session": ("sessionStart",)}
EVENTS = ("sessionStart",)

action = os.environ.get("SHEPR_ACTION", "")
pane_id = os.environ.get("SHEPR_PANE_ID")
socket_path = os.environ.get("SHEPR_SOCKET_PATH")

if not pane_id or not socket_path:
    raise SystemExit(0)

# Some hooks are stamped by the shell the moment they start, so interpreter
# startup jitter cannot reorder near-simultaneous events. `date` without %N
# support prints a literal N; fall back to our own clock then, and for every
# hook the shell does not stamp.
raw_seq = os.environ.get("SHEPR_HOOK_SEQ", "")
report_seq = int(raw_seq) if raw_seq.isdigit() else time.time_ns()


def read_hook_input():
    try:
        # Read bytes: json.loads detects the payload encoding, not the locale.
        content = sys.stdin.buffer.read()
        if not content.strip():
            return {}
        parsed = json.loads(content)
    except Exception:
        return {}
    # A valid JSON body that is not an object (a list, a string, null) carries
    # no fields we can read; treat it as empty.
    return parsed if isinstance(parsed, dict) else {}


hook_input = read_hook_input()


def first_text(*keys):
    for key in keys:
        value = hook_input.get(key)
        if isinstance(value, str) and value:
            return value
    return None


def send(method, params):
    request = {
        "id": f"{SOURCE}:{report_seq}",
        "method": method,
        "params": {
            "pane_id": pane_id,
            "source": SOURCE,
            "seq": report_seq,
            **params,
        },
    }
    try:
        client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        client.settimeout(SOCKET_WAIT_SECONDS)
        client.connect(socket_path)
        client.sendall((json.dumps(request) + "\n").encode())
        try:
            client.recv(4096)
        except Exception:
            pass
        client.close()
    except Exception:
        pass


def report_session(session_id, session_start_source=None):
    params = {"agent_session_id": session_id}
    if session_start_source:
        params["session_start_source"] = session_start_source
    send(METHOD_SESSION, params)


def report_state(state, session_id):
    send(METHOD_STATE, {"state": state, "agent_session_id": session_id})


event = first_text("hook_event_name", "hookEventName")
if event is not None and event not in EVENTS:
    raise SystemExit(0)

session_id = first_text("session_id", "sessionId", "conversation_id", "conversationId")
if session_id is None:
    raise SystemExit(0)
report_session(session_id)
' 2>/dev/null || true

finish
