# Backstop: only report on session-start payloads. Grok emits
# "session_start" and accepts the Claude/Cursor spellings, so tolerate all
# three; a missing field is allowed for forward compatibility.
hook_event_name = first_text("hook_event_name", "hookEventName")
if hook_event_name is not None and hook_event_name not in ("session_start", "sessionStart") + EVENTS:
    raise SystemExit(0)

# Grok injects GROK_SESSION_ID into every hook process; prefer it and fall
# back to the event payload's session id fields.
session_id = os.environ.get("GROK_SESSION_ID") or first_text("session_id", "sessionId")
if not session_id:
    raise SystemExit(0)
report_session(session_id, first_text("source"))
