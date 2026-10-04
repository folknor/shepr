session_id = first_text("session_id")
# A state event without its session identity cannot safely claim pane state.
if session_id is None:
    raise SystemExit(0)
if action == ACTION_SESSION:
    # Preserve the source Kimi reports for server validation. A bare
    # SessionStart still identifies the start of a fresh root session.
    report_session(session_id, first_text("source") or "startup")
else:
    report_state(action, session_id)
