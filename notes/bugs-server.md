# Headless server defects

```
1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
```

## SRV-011 - Server hot-path costs

- **Claim:** AGENTS.md "Hot paths multiply".
- On every loop wake, which means every PTY render notify:
  - `sync_immediate_pty_sources`, which allocates HashSets and walks all panes for direct attaches.
  - `stream_host_mouse_capture_mode` and `stream_direct_terminal_keyboard_mode`.
- Per event and per request:
  - `terminal_id_by_string` does a linear scan with `to_string()` per terminal. It is called for every direct-attach keystroke, every mouse event and every render. `TerminalId::as_str` already exists.
  - `sync_foreground_client_state` runs `compute_view_without_resizing_panes` on every `StateChanged`/`HookStateReported` event and every API request.
  - Each full render rebuilds `app.session_snapshot()` for each shell client (`render.rs`) just to diff it. Each shell snapshot is also built with `config_diagnostic: None` and then overwritten.
- The hunter's structural suggestion: the event loop mixes per-client presentation state (`foreground_client_id`, `effective_size`, global `app.state.active`, `outer_terminal_focus`, the diagnostic copies) with session state; make each client's view the only source of presentation truth and drop the global "foreground client" projection into `AppState`. This is behind APP-010 and most of this entry.

## SRV-013 - Pending-resume candidates are recomputed several times per call

- `sync_pending_agent_resume_deadline` and `start_pending_agent_resumes` (`src/app/agent_resume.rs`) can each compute `pending_agent_resume_candidates()` more than once per call. Only costs anything while resumes are pending.

## SRV-015 - Every frame is copied twice on the way out

- `frame_server_message` makes `write_message` build the frame in one `Vec` and then copies it into another; a full copy of every frame for every client. A `protocol::encode_frame(msg) -> Result<Vec<u8>>` would remove it.

## SRV-016 - Direct-attach clients cannot be told about dropped input

- Shell clients now get a `ClientShellError` when pane input is dropped under backpressure or a paste is rejected. Direct terminal-attach clients have no message for it; the server only logs. Needs a protocol message if it matters.
