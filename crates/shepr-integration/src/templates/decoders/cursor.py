event = first_text("hook_event_name", "hookEventName")
if event is not None and event not in EVENTS:
    raise SystemExit(0)

session_id = first_text("session_id", "sessionId", "conversation_id", "conversationId")
if session_id is None:
    raise SystemExit(0)
report_session(session_id)
