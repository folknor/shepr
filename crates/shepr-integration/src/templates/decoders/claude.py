if "CURSOR_VERSION" in os.environ or "cursor_version" in hook_input:
    raise SystemExit(0)
hook_event_name = str(hook_input.get("hook_event_name") or "")
if hook_event_name not in EVENTS:
    raise SystemExit(0)
if hook_input.get("agent_id"):
    raise SystemExit(0)
session_id = first_text("session_id")
if not session_id:
    raise SystemExit(0)
report_session(session_id, first_text("source"))
