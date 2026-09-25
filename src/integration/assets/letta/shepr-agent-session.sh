#!/bin/sh
# managed by shepr; reinstalling the integration replaces this file.
# SHEPR_INTEGRATION_ID=letta
# SHEPR_INTEGRATION_VERSION=1

[ "${1:-}" = "session" ] || exit 0
[ "${SHEPR_ENV:-}" = "1" ] || exit 0
[ -n "${SHEPR_PANE_ID:-}" ] || exit 0
[ -n "${SHEPR_SOCKET_PATH:-}" ] || exit 0
if [ -n "${SHEPR_BIN_PATH:-}" ]; then
    [ -x "$SHEPR_BIN_PATH" ] || exit 0
else
    command -v shepr >/dev/null 2>&1 || exit 0
fi
command -v python3 >/dev/null 2>&1 || exit 0

# SessionStart stdout is injected into Letta's next user message. Keep every
# path silent, including successful reports and malformed hook input.
python3 -c '
import json
import os
import subprocess
import sys
import time

try:
    payload = json.load(sys.stdin)
    conversation_id = payload.get("conversation_id")
    agent_id = payload.get("agent_id")
    if not isinstance(conversation_id, str) or not conversation_id:
        raise ValueError
    if conversation_id == "default":
        if not isinstance(agent_id, str) or not agent_id:
            raise ValueError
        session_id = "default:" + agent_id
    else:
        session_id = conversation_id

    command = os.environ.get("SHEPR_BIN_PATH") or "shepr"
    args = [
        command, "pane", "report-agent-session", os.environ["SHEPR_PANE_ID"],
        "--source", "shepr:letta", "--agent", "letta",
        "--agent-session-id", session_id, "--seq", str(time.time_ns()),
        "--session-start-source", "new" if payload.get("is_new_session") else "resume",
    ]
    subprocess.run(
        args,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=1,
        check=False,
    )
except Exception:
    pass
' 2>/dev/null || true
