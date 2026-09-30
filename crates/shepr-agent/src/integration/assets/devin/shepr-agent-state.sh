#!/bin/sh
# installed by shepr
# managed by shepr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# SHEPR_INTEGRATION_ID=devin
# SHEPR_INTEGRATION_VERSION=3

set -eu

action="${1:-}"
hook_input_file="$(mktemp "${TMPDIR:-/tmp}/shepr-devin-hook.XXXXXX")" || exit 0
trap 'rm -f "$hook_input_file"' 0
trap 'exit 0' HUP INT TERM
cat >"$hook_input_file" 2>/dev/null || true

case "$action" in
  session) ;;
  *) exit 0 ;;
esac

[ "${SHEPR_ENV:-}" = "1" ] || exit 0
[ -n "${SHEPR_SOCKET_PATH:-}" ] || exit 0
[ -n "${SHEPR_PANE_ID:-}" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0

# A python failure must not fail the hook: under `set -eu` it would exit
# non-zero with a traceback on stderr, which the agent may show to the user.
SHEPR_HOOK_INPUT_FILE="$hook_input_file" python3 - 2>/dev/null <<'PY' || true
from __future__ import annotations

import json
import os
import socket
import time

SOURCE = "shepr:devin"
AGENT = "devin"


def load_hook_input(path: str | None) -> dict:
    if not path:
        return {}
    try:
        with open(path, encoding="utf-8") as handle:
            content = handle.read()
        if not content.strip():
            return {}
        parsed = json.loads(content)
        return parsed if isinstance(parsed, dict) else {}
    except Exception:
        return {}


def hook_session_id(hook_input: dict) -> str | None:
    for key in ("session_id", "sessionId"):
        value = hook_input.get(key)
        if isinstance(value, str) and value:
            return value
    return None


def hook_event_name(hook_input: dict) -> str:
    value = hook_input.get("hook_event_name")
    return value if isinstance(value, str) else ""


pane_id = os.environ.get("SHEPR_PANE_ID")
socket_path = os.environ.get("SHEPR_SOCKET_PATH")
hook_input = load_hook_input(os.environ.get("SHEPR_HOOK_INPUT_FILE"))

if not pane_id or not socket_path:
    raise SystemExit(0)

# Tool hooks can repeat the same identity for every call. Devin's installed
# hook also receives those events, but session identity only changes at these
# lifecycle boundaries, so ignore the others without querying global sessions.
if hook_event_name(hook_input) not in ("SessionStart", "UserPromptSubmit"):
    raise SystemExit(0)

report_seq = time.time_ns()
request_id = f"{SOURCE}:{report_seq}"

session_id = hook_session_id(hook_input)
if not session_id:
    raise SystemExit(0)
request = {
    "id": request_id,
    "method": "pane.report_agent_session",
    "params": {
        "pane_id": pane_id,
        "source": SOURCE,
        "agent": AGENT,
        "agent_session_id": session_id,
        "seq": report_seq,
    },
}

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
