hook_event_name = str(hook_input.get("hook_event_name") or "")
if hook_event_name and hook_event_name not in EVENTS_BY_ACTION.get(action, ()):
    raise SystemExit(0)

session_id = first_text("session_id")
if not session_id:
    raise SystemExit(0)
inherited_session_id = os.environ.get("CODEX_THREAD_ID")
if inherited_session_id and inherited_session_id != session_id:
    raise SystemExit(0)
if action == ACTION_SESSION:
    session_start_source = None
    if hook_event_name in EVENTS_BY_ACTION[ACTION_SESSION]:
        session_start_source = first_text("source")
    report_session(session_id, session_start_source)
else:
    report_state(action, session_id)
