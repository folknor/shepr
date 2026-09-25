I read everything in scope (src/server/headless.rs and all of headless/*, plus clients.rs, client_shell.rs, client_commands.rs, client_endpoint_control.rs, pane_input.rs, keybindings.rs, alt_screen_read.rs, autodetect.rs, socket_paths.rs and mod.rs). I also followed values into app/agent_resume.rs, app/runtime.rs, app/actions.rs, pane.rs, pane/terminal.rs, protocol/wire.rs, client_accept.rs and ui/tab_surface.rs. Nothing was built or run. I made one read-only `grep` of Cargo.toml, to confirm that ctrlc has the `termination` feature (it does, so SIGTERM and SIGHUP reach the quit path).

Findings are ordered by severity. Each one names the claim it breaks.

## High

**1. Restored agents may never resume while any pane keeps producing output.**
- Where: `headless.rs:350` calls `handle_scheduled_tasks_headless(now, needs_render)`. The parameter is named `geometry_dirty`, but what it receives is "any render pending".
- What happens: when it is true, `pending_agent_resume_deadline = None` (headless.rs:2166-2167). On the next quiet iteration, `sync_pending_agent_resume_deadline` (app/agent_resume.rs:38) sees `None` and calls `get_or_insert(now + 750ms)`, so the theme wait starts over.
- Effect: any pane (visible or hidden) that produces output at least every 750 ms keeps pushing back the first resume, which is the one gated by the theme wait. This holds until `next_agent_resume_at` exists, so it can go on indefinitely. Hidden-only PTY work also sets `needs_render`.
- Claim broken: "Session restore … and agent resume on restore", and the parameter's own name.
- Test gap: `headless_scheduled_tasks_start_pending_agent_resume_without_foreground_client` (headless/tests/mod.rs:4433) only ever passes `false`.

**2. `remove_var` runs while other threads exist.**
- Where: `bootstrap.rs:100`, `take_startup_cwd`, calls `unsafe { std::env::remove_var(..) }` from inside `rt.block_on`.
- By that point the multi-thread tokio workers, the API server thread (started at bootstrap.rs:13) and whatever `App::new` spawned during session restore are all running.
- That is exactly the precondition `remove_var`'s safety contract forbids, so this is UB under concurrency.
- Fix: read the value in `main` before any thread starts, or don't mutate the environment at all.

**3. Protocol-version check cannot catch a stale build.**
- Where: `autodetect.rs:92` and `protocol/wire.rs:6,21`.
- The wire docs say `PROTOCOL_VERSION` "turns an accidental mismatch into a clear handshake error". But it is a hand-bumped constant (`1`), and the codec is positional and not self-describing.
- Any reinstall where a wire type changed but the constant didn't gives silent misdecoding instead of an error. This is likely with hand-copied binaries on SSH hosts.
- `saved_federation` skips `validate_running_server_compatibility` entirely (autodetect.rs:232).
- Fix: put a build identity (commit hash or binary hash) in the handshake and the status ping, and drop the manual constant.

## Medium

**4. Shutdown drops API requests instead of answering `server_unavailable`.**
- Doc claim (headless.rs:1981): "During shutdown, remaining requests get a server_unavailable error." Four paths break it:
  - In the post-select shutdown branch (headless.rs:455-477), a `LoopEvent::Api(msg)` falls into `_ => {}` and is dropped. It was already dequeued, so `reject_queued_api_requests_for_shutdown` never sees it.
  - `pending_alt_screen_reads` and `deferred_alt_screen_reads` are never answered or aborted in `complete_shutdown` (lifecycle.rs). Their senders are just dropped. An in-flight traversal also leaves the agent's alternate-screen viewport scrolled up.
  - Requests the API thread queues after `reject_queued_api_requests_for_shutdown` are dropped. That thread lives until `HeadlessServer` drops, which is after `save_session_on_shutdown`.
  - Small race: the client socket is removed (lifecycle.rs:52) before the session save and before the API socket goes away. A `shepr` launched in that window spawns a daemon that exits with AddrInUse, and the user waits out the 15 s timeout.

**5. SIGTERM/SIGHUP quit can save a session with panes missing.**
- Where: headless.rs:298-302. The quit path drains up to `APP_EVENT_CHANNEL_CAPACITY` internal events, including `PaneDied`, before `initiate_shutdown`. The save then happens after the loop.
- On logout or `kill`, where panes get signalled at the same time, dead panes can be removed from the layout before it is saved.
- The host-shutdown path deliberately avoids this ("Do not drain pane deaths here"), but the signal path doesn't. Confidence is medium: I did not trace what `PaneDied` does to the layout.

**6. Alternate-screen read leaves side effects when it fails.**
- Where: `alt_screen_read.rs`. The `complete_fallback` paths (5 s restore expiry at :140-145, runtime gone, `send_wheel` error) answer the caller but never scroll the agent back down.
- The user's agent TUI is left scrolled into history. The type exists to capture history "by scrolling while idle" and restore the viewport.

**7. The seen-marking can hit a tab the foreground client isn't viewing.**
- Where: `sync_foreground_client_state` (headless.rs:634-636) calls `mark_active_tab_seen()`. That marks the global `app.state.active` tab, not the foreground client's `shell_location` tab.
- It runs on every `StateChanged`/`HookStateReported` event and every API request.
- With several clients, endpoint requests from client A move `app.state.active` (`set_default_shell_target_from_client`) while B is the focused foreground. B's focus then clears "done" state on A's tab.

## Low / contract drift

**8. Oversized frames freeze a client silently.**
- Where: render.rs:630-636. The frame is skipped with only a warn: no deferred render and no message to the client.
- If a surface always exceeds 2 MB (very large grid with many hyperlinks or long graphemes), that client never gets another frame.

**9. Code paths for writer-less clients that production never creates.**
- Every production `ClientConnection` gets `Some(writer)`, and `ClientDetach` removes the client.
- So the `writer.is_none()` branches are test-only, and the comment at headless.rs:987-989 ("A detached client keeps its entry with no writer") is false.
- `send_to_client` returns `true` when the writer is `None`, contrary to its doc ("Returns false if the client was not found or the send failed").

**10. Dead config-diagnostic and keybinding machinery.**
- `config_diagnostic_deadline` is never set to `Some` in production (only initialised to `None` at app/mod.rs:286), so the expiry branch at headless.rs:2133-2141 is dead.
- `sync_visible_server_config_diagnostic` is only ever called with `false`, so the "without keybindings" variant is never chosen for app state. Shell snapshots set `config_diagnostic` from the static startup fields anyway (render.rs:490).
- `server_keybindings` plus `apply_keybindings` re-clone and reapply an immutable keymap on every foreground sync. Config is never reloaded, and there is no non-server keybinding mode left.

**11. Other code that outlived the stripping.**
- The non-shell branch of `resize_shared_runtime_to_effective_size_with_pending_agent_resumes` (headless.rs:540-556) is unreachable: the foreground is always a shell client.
- The `_client_local` parameter of `handle_api_request_with_shutdown_check_inner` is unused.
- `windows_record` / `WindowsKeyRecord` in held-input tracking (clients.rs:251,267) is Windows console data carried on the wire of a Linux-only fork.

**12. Rule violations.** "No unwrap in production" is broken by `shell_render.expect(..)` (render.rs:559), `.expect("checked client")` (surface_interest.rs:51) and `unreachable!()` (alt_screen_read.rs:157). None of them is reachable today.

**13. `shell_surface_active` defaults to `true` for terminal-attach clients.**
- `ClientConnection::new_with_mode` sets it for `TerminalPending`/`TerminalAttach` too (clients.rs:217).
- `promote_client_to_foreground` and `claim_*_shell_tab_geometry` check only that flag, not the mode. Today every caller guards the mode first, so this is a latent trap rather than a live bug.

**14. Input is dropped under backpressure.**
- All pane input goes through `try_send_bytes` (pane_input.rs). When the PTY write queue is full, keystrokes and pastes are dropped with only a log line.
- In a batch, the first error aborts the remaining events (`?`), including releases.

## Hot-path costs (AGENTS.md "Hot paths multiply")

These run on every loop wake, which means every PTY render notify:
- `sync_immediate_pty_sources`, which allocates HashSets and walks all panes for direct attaches.
- `stream_host_mouse_capture_mode` and `stream_direct_terminal_keyboard_mode`.

Per-event and per-request costs:
- `terminal_id_by_string` does a linear scan with `to_string()` per terminal. It is called for every direct-attach keystroke, every mouse event and every render. `TerminalId::as_str` already exists.
- `sync_foreground_client_state` runs `compute_view_without_resizing_panes` plus two keymap clones on every `StateChanged`/`HookStateReported` event and every API request.
- Each full render rebuilds `app.session_snapshot()` for each shell client (render.rs:483) just to diff it.

`client_shell.rs:47-105` zips the snapshot's workspaces and tabs against `app.state` by position. If `session_snapshot` ever filters or reorders, labels are silently misattributed.

## Structural suggestion

The event loop mixes per-client presentation state (`foreground_client_id`, `effective_size`, global `app.state.active`, `outer_terminal_focus`, the keybinding and diagnostic copies) with session state. That mix is behind findings 7 and 10 and most of the hot-path work.

The larger fix I'd suggest: make each client's view (location, geometry, focus, diagnostics) the only source of presentation truth, and drop the global "foreground client" projection into `AppState`.
