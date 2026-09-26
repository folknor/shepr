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

## SRV-006 - Dead config-diagnostic and keybinding machinery on the server

- `config_diagnostic_deadline` is never set to `Some` in production (only initialised to `None` at app/mod.rs:286), so its expiry branch in `handle_scheduled_tasks_headless` is dead.
- `sync_visible_server_config_diagnostic` is only ever called with `false`, so the "without keybindings" variant is never chosen for app state. Shell snapshots set `config_diagnostic` from the static startup fields anyway (render.rs:490).
- `server_keybindings` plus `apply_keybindings` re-clone and reapply an immutable keymap on every foreground sync. Config is never reloaded, and there is no non-server keybinding mode left.
- See also UI-006 for the client side of the config diagnostic.

## SRV-007 - Other server code that outlived the stripping

- The non-shell branch of `resize_shared_runtime_to_effective_size_with_pending_agent_resumes` (headless.rs) is unreachable: the foreground is always a shell client.
- The `_client_local` parameter of `handle_api_request_with_shutdown_check_inner` is unused.
- (`windows_record` in held-input tracking is filed under PLAT-003.)

## SRV-009 - `shell_surface_active` defaults to `true` for terminal-attach clients

- `ClientConnection::new_with_mode` sets it for `TerminalPending`/`TerminalAttach` too (clients.rs:217).
- `promote_client_to_foreground` and `claim_*_shell_tab_geometry` check only that flag, not the mode. Today every caller guards the mode first, so this is a latent trap rather than a live bug.

## SRV-010 - Pane input is dropped under backpressure

- All pane input goes through `try_send_bytes` (pane_input.rs). When the PTY write queue is full, keystrokes and pastes are dropped with only a log line.
- In a batch, the first error aborts the remaining events (`?`), including releases.

## SRV-011 - Server hot-path costs

- **Claim:** AGENTS.md "Hot paths multiply".
- On every loop wake, which means every PTY render notify:
  - `sync_immediate_pty_sources`, which allocates HashSets and walks all panes for direct attaches.
  - `stream_host_mouse_capture_mode` and `stream_direct_terminal_keyboard_mode`.
- Per event and per request:
  - `terminal_id_by_string` does a linear scan with `to_string()` per terminal. It is called for every direct-attach keystroke, every mouse event and every render. `TerminalId::as_str` already exists.
  - `sync_foreground_client_state` runs `compute_view_without_resizing_panes` plus two keymap clones on every `StateChanged`/`HookStateReported` event and every API request.
  - Each full render rebuilds `app.session_snapshot()` for each shell client (render.rs:483) just to diff it.
- The hunter's structural suggestion: the event loop mixes per-client presentation state (`foreground_client_id`, `effective_size`, global `app.state.active`, `outer_terminal_focus`, the keybinding and diagnostic copies) with session state; make each client's view the only source of presentation truth and drop the global "foreground client" projection into `AppState`. This is behind APP-010, SRV-006 and most of this entry.

## SRV-012 - Client shell labels are matched to app state by position

- `client_shell.rs:47-105` zips the snapshot's workspaces and tabs against `app.state` by position. If `session_snapshot` ever filters or reorders, labels are silently misattributed.

## SRV-013 - Pending-resume candidates are recomputed several times per call

- `sync_pending_agent_resume_deadline` and `start_pending_agent_resumes` (`src/app/agent_resume.rs`) can each compute `pending_agent_resume_candidates()` more than once per call. Only costs anything while resumes are pending.

## SRV-014 - `frame_server_message_with_max` duplicates the frame-size check

- `write_message` now refuses any payload over `MAX_FRAME_SIZE` itself. The explicit size check in `HeadlessServer::frame_server_message_with_max` (`src/server/headless.rs`) is redundant except for callers passing a smaller cap.
