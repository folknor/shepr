session_id = first_text("session_id")
if not session_id:
    raise SystemExit(0)
report_session(session_id)
