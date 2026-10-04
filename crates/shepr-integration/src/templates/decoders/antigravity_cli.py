# Session-only: this hook reports the Antigravity conversation so Shepr can
# resume the pane. Lifecycle state comes from Shepr's screen detection.
session_id = first_text("conversationId")
if session_id is None:
    raise SystemExit(0)
report_session(session_id)
