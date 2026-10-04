# Tool hooks can repeat the same identity for every call. Devin's installed
# hook also receives those events, but session identity only changes at these
# lifecycle boundaries, so ignore the others without querying global sessions.
if first_text("hook_event_name") not in EVENTS:
    raise SystemExit(0)

session_id = first_text("session_id", "sessionId")
if not session_id:
    raise SystemExit(0)
report_session(session_id)
