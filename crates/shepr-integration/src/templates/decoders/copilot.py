def normalize_event(event):
    return event.replace("_", "").replace("-", "").lower()


event = first_text("hook_event_name", "hookEventName")
if event:
    if normalize_event(event) not in [normalize_event(name) for name in EVENTS]:
        raise SystemExit(0)
elif "prompt" in hook_input or first_text("tool_name", "toolName", "notification_type", "notificationType", "stop_reason", "stopReason", "reason"):
    raise SystemExit(0)

session_id = first_text("session_id", "sessionId")
if not session_id:
    raise SystemExit(0)
report_session(session_id)
