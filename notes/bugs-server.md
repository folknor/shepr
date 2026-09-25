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

## SRV-001 - Restored agents may never resume while any pane keeps producing output

- **Where:** `headless.rs:350` calls `handle_scheduled_tasks_headless(now, needs_render)`. The parameter is named `geometry_dirty`, but what it receives is "any render pending".
- **What happens:** when it is true, `pending_agent_resume_deadline = None` (headless.rs:2166-2167). On the next quiet iteration, `sync_pending_agent_resume_deadline` (app/agent_resume.rs:38) sees `None` and calls `get_or_insert(now + 750ms)`, so the theme wait starts over.
- **Effect:** any pane (visible or hidden) that produces output at least every 750 ms keeps pushing back the first resume, which is the one gated by the theme wait. This holds until `next_agent_resume_at` exists, so it can go on indefinitely. Hidden-only PTY work also sets `needs_render`.
- **Claim broken:** "Session restore … and agent resume on restore", and the parameter's own name.
- **Test gap:** `headless_scheduled_tasks_start_pending_agent_resume_without_foreground_client` (headless/tests/mod.rs:4433) only ever passes `false`.

## SRV-002 - Shutdown drops API requests instead of answering `server_unavailable`

- **Doc claim** (headless.rs:1981): "During shutdown, remaining requests get a server_unavailable error." Four paths break it:
  - In the post-select shutdown branch (headless.rs:455-477), a `LoopEvent::Api(msg)` falls into `_ => {}` and is dropped. It was already dequeued, so `reject_queued_api_requests_for_shutdown` never sees it.
  - `pending_alt_screen_reads` and `deferred_alt_screen_reads` are never answered or aborted in `complete_shutdown` (lifecycle.rs). Their senders are just dropped. An in-flight traversal also leaves the agent's alternate-screen viewport scrolled up.
  - Requests the API thread queues after `reject_queued_api_requests_for_shutdown` are dropped. That thread lives until `HeadlessServer` drops, which is after `save_session_on_shutdown`.
  - Small race: the client socket is removed (lifecycle.rs:52) before the session save and before the API socket goes away. A `shepr` launched in that window spawns a daemon that exits with AddrInUse, and the user waits out the 15 s timeout.

## SRV-003 - SIGTERM/SIGHUP quit can save a session with panes missing

- **Where:** headless.rs:298-302. The quit path drains up to `APP_EVENT_CHANNEL_CAPACITY` internal events, including `PaneDied`, before `initiate_shutdown`. The save then happens after the loop.
- On logout or `kill`, where panes get signalled at the same time, dead panes can be removed from the layout before it is saved.
- The host-shutdown path deliberately avoids this ("Do not drain pane deaths here"), but the signal path doesn't.
- Hunter confidence: medium; the hunter did not trace what `PaneDied` does to the layout.

## SRV-004 - Alternate-screen read leaves side effects when it fails

- **Where:** `alt_screen_read.rs`. The `complete_fallback` paths (5 s restore expiry at :140-145, runtime gone, `send_wheel` error) answer the caller but never scroll the agent back down.
- The user's agent TUI is left scrolled into history. The type exists to capture history "by scrolling while idle" and restore the viewport.

## SRV-005 - Code paths for writer-less clients that production never creates

- Every production `ClientConnection` gets `Some(writer)`, and `ClientDetach` removes the client.
- So the `writer.is_none()` branches are test-only, and the comment at headless.rs:987-989 ("A detached client keeps its entry with no writer") is false.
- `send_to_client` returns `true` when the writer is `None`, contrary to its doc ("Returns false if the client was not found or the send failed").

## SRV-006 - Dead config-diagnostic and keybinding machinery on the server

- `config_diagnostic_deadline` is never set to `Some` in production (only initialised to `None` at app/mod.rs:286), so the expiry branch at headless.rs:2133-2141 is dead.
- `sync_visible_server_config_diagnostic` is only ever called with `false`, so the "without keybindings" variant is never chosen for app state. Shell snapshots set `config_diagnostic` from the static startup fields anyway (render.rs:490).
- `server_keybindings` plus `apply_keybindings` re-clone and reapply an immutable keymap on every foreground sync. Config is never reloaded, and there is no non-server keybinding mode left.
- See also UI-006 for the client side of the config diagnostic.

## SRV-007 - Other server code that outlived the stripping

- The non-shell branch of `resize_shared_runtime_to_effective_size_with_pending_agent_resumes` (headless.rs:540-556) is unreachable: the foreground is always a shell client.
- The `_client_local` parameter of `handle_api_request_with_shutdown_check_inner` is unused.
- (`windows_record` in held-input tracking is filed under PLAT-003.)

## SRV-008 - Production `expect`/`unreachable!` in the server

- **Claim:** "No unwrap in production".
- `shell_render.expect(..)` (render.rs:559), `.expect("checked client")` (surface_interest.rs:51) and `unreachable!()` (alt_screen_read.rs:157). None of them is reachable today.

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
