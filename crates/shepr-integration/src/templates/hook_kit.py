import json
from types import SimpleNamespace
import os
import socket
import time

SOURCE = "@SOURCE@"
METHOD_SESSION = "@METHOD_SESSION@"
METHOD_STATE = "@METHOD_STATE@"
START = SimpleNamespace(**@START_PY@)
ACTION_SESSION = "@ACTION_SESSION@"
SOCKET_WAIT_SECONDS = @SOCKET_WAIT_SECONDS@
# The hook events this integration is registered for, per action and in all.
EVENTS_BY_ACTION = @EVENTS_BY_ACTION@
EVENTS = @EVENTS@

action = os.environ.get("SHEPR_ACTION", "")
pane_id = os.environ.get("@ENV_PANE@")
socket_path = os.environ.get("@ENV_SOCKET@")
hook_input_file = os.environ.get("SHEPR_HOOK_INPUT_FILE")

if not pane_id or not socket_path:
    raise SystemExit(0)

# Some hooks are stamped by the shell the moment they start, so interpreter
# startup jitter cannot reorder near-simultaneous events. `date` without %N
# support prints a literal N; fall back to our own clock then, and for every
# hook the shell does not stamp.
raw_seq = os.environ.get("SHEPR_HOOK_SEQ", "")
report_seq = int(raw_seq) if raw_seq.isdigit() else time.time_ns()


def read_hook_input():
    if not hook_input_file:
        return {}
    try:
        with open(hook_input_file, encoding="utf-8") as handle:
            content = handle.read()
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


