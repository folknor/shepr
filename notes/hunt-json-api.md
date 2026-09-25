JSON API hunt: findings

Scope covered: all of `src/api/` and `src/app/api.rs` plus `src/app/api/*`. I followed requests into `src/server/headless.rs`, `src/pty/actor.rs`, `src/ipc.rs`, `src/pane.rs` and `src/app/{ids,creation}.rs`. I read code only and ran nothing. Findings are in rough order of severity.

**1. One failed `accept()` kills the API server for good** (`src/api/server.rs:108-137`)
- `for stream in listener.incoming()` does `error!(...); break;` on any accept error. EMFILE (easy to hit: one thread and fd per subscription, plus PTYs) or ECONNABORTED ends the listener thread, and nothing restarts it.
- `std::thread::spawn` inside the loop panics on thread exhaustion, which has the same effect.
- The listener never checks `running`.
- Across the boundary it gets worse. `autodetect::is_server_listening` probes the *client* socket, which is still alive. So `validate_running_server_compatibility` then gets `Ok(None)` from the dead API socket and refuses to attach ("status API is unavailable"). The CLI and agent hooks also fail. The server is unusable until someone kills it by hand.
- This breaks the claim "every CLI subcommand goes through" the API.

**2. `agent.wait` and `agent.prompt --wait` never finish when the pane is removed along with its tab or workspace**
- `wait_for_resolved_agent` (`src/api/wait.rs:381-508`) only probes on `PaneClosed`, `PaneExited`, `PaneMoved`, `PaneAgentDetected` or `PaneUpdated` for that pane.
- None of these emit `PaneClosed` for the panes they remove:
  - `tab.close` (`src/app/api/tabs.rs:220-285`)
  - `workspace.close` (`src/app/api/workspaces.rs:315-341`)
  - `layout.apply` replacing a tab (`src/app/api/layouts.rs:153-183`)
- The later `PaneDied` is gated on `self.find_pane(...)` (`src/app/api.rs:110-121`), so `PaneExited` never comes either.
- Result: with no timeout the wait loops forever; with a timeout it reports `timeout` instead of `agent_not_running`.
- Same gap the other way round: `pane.close` of the last pane in a tab that has sibling tabs removes the tab (`src/workspace.rs:989-1000`) but emits only `PaneClosed`, no `TabClosed` (`src/app/api/panes.rs:1892-1906`).
- Anyone subscribed to `pane.closed` / `tab.closed` gets an incomplete view of the model.

**3. `agent.prompt --wait --timeout` can hang past its timeout**
- `wait.rs:216-219` sets `AgentPromptWaitOptions.submission_deadline` (`#[serde(skip)]`, `src/api/schema/agents.rs:41`). Nothing reads it: `queue_agent_prompt` ignores `params.wait`.
- The prompt is dispatched with no timeout (`wait.rs:226`).
- The deferred thread blocks on `completion.recv()` with no bound (`src/app/api/agents.rs:72-82`).
- The PTY actor never produces `TimedOut`, so the `timeout` branch in `handle_deferred_agent_api_request` is dead code.
- If the agent stops reading stdin, the submission sits in `WritingText` forever and the caller's timeout is ignored.

**4. Parameters and fields that are parsed but have no effect**
- `strip_ansi` does nothing. It appears in `PaneReadParams`, `AgentReadParams`, `PaneWaitForOutputParams` and the `pane.output_matched` subscription. `read_terminal_snapshot` (`src/app/api_helpers.rs:109`) never receives it.
  - As a result the CLI's `pane read --raw` (sets `strip_ansi=false`, `src/cli/pane.rs:507-510`) behaves exactly like `--ansi`.
- `PaneReadResult.revision` is always 0: hard-coded in `panes.rs:1528` and `agents.rs:219`.
  - So `OutputMatched.revision` (`wait.rs:100`) is always 0 too, while `PaneInfo.revision` is real.
- `format: ansi` with `source: detection` silently returns plain text (`api_helpers.rs:137-139`).
- `PaneProcessInfo.tty` is always `None` (`panes.rs:545`).
- `layout.apply` ignores the `pane_id` it accepts on each `LayoutPane`.

**5. Advertised event types that never fire**
- `EventData::WorkspaceUpdated` is never built anywhere, so a `workspace.updated` subscription is accepted and never fires.
- `EventKind::PaneOutputChanged` and `EventData::PaneOutputChanged` are never emitted.
- `EventMatch` accepts 16 variants, but `events.wait` supports exactly one (`wait.rs:765-785`); the rest parse fine and then fail with `unsupported_event_wait_match`.

**6. A stream with several subscriptions delivers events out of order**
- `stream_subscriptions` (`server.rs:619-651`) drains each subscription's history in turn. With `[pane.closed, pane.created]`, a create-then-close in one poll window arrives as closed, then created.
- Events carry no sequence number on the wire, so clients cannot reorder them. The hub has a global sequence; the stream discards it.

**7. Subscription and wait errors are silently swallowed after setup**
- `poll_batch` / `poll` turn `pane_not_found` into "no events" for agent-status, scroll and output subscriptions (`subscriptions.rs:255-262, 312-318, 375, 552`).
  - When the pane closes, or moves between workspaces (its public ID changes), the subscription goes silent for good. It keeps sending a `PaneGet`/`PaneRead` to the app every 100 ms until the client disconnects.
- `wait_for_event` drops every error except `pane_not_found` (`wait.rs:745`), including `events_lost` and `server_unavailable`, and spins until timeout or forever.
- `wait_for_resolved_agent` and `poll_result` use the unchecked `events_after`, so history loss (512-event cap) goes undetected even though `events_after_checked` exists for exactly this.

**8. Hot-path traps on the server main loop**
- `pane_info` (`src/app/creation.rs:307`) calls `foreground_cwd`, which does `io.foreground_process_group_id()`: a round-trip to the PTY actor thread with a **1 s** `recv_timeout` (`src/pty/actor.rs:209-216`), plus /proc reads, per pane, on the main thread.
- It is reached from:
  - every `pane.get`, which subscription pollers send at 10 Hz each;
  - `pane.list` and `session.snapshot` (once per pane);
  - every `PaneUpdated` emission, including stripped-title changes (`terminal_titles.rs:72`).
- Each `pane.output_matched` subscription runs a full recent-text `PaneRead` on the main thread 10 times a second.

**9. Latency on every API request** (`read_initial_request_line`, `server.rs:529-570`)
- It reads one byte per syscall, and sleeps 100 ms on the first `Pending`.
- A client that writes right after connecting often loses that race, so each CLI and hook call can pay about +100 ms. A 1 MiB request costs about 1M read syscalls.

**10. No timeout on ordinary requests**
- `handle_request` → `dispatch_to_app(..., None, None)` blocks forever; `APP_RESPONSE_TIMEOUT` only applies to internal pollers.
- `ApiClient::request` / `request_value` set no timeout either. A stalled main loop hangs every CLI call and every agent hook that shells out to it.

**11. Smaller defects**
- `pane.rename` changes `PaneInfo.label` but emits no `PaneUpdated` (`panes.rs:1462-1488`); `agent.rename`, metadata and title changes do emit it.
- `pane.send_keys` writes each key separately (`panes.rs:1926-1930`), so backpressure can leave a partial key sequence. `agent.send_keys` sends one combined write.
- `probe_stream_closed` treats any extra client byte as "closed" and consumes it (`src/ipc.rs:125-127`). A client that sends a trailing blank line gets its wait or subscription dropped silently, with no response.
- `read_runtime_status_at` (`src/api/status.rs:34-43`) only maps `TimedOut` to "not running". A timed-out receive on this socket reports `WouldBlock` (the client test at `client.rs:278` already expects either), so a stalled server shows up as an opaque error.
- `CLIENT_SHELL_METHODS` (`src/server/client_commands.rs:25-26`) advertises `pane.link.activate` / `pane.link.resolve`, which don't exist in `Method`: leftovers from a stripped feature.
- `src/app/ids.rs` still accepts herdr-era ID forms (`p_…`, `t_…`, `w_N`, bare `N`, `ws-N`). The bare and `w_N` forms are *positional*, so a stale or index-style ID can resolve to a different workspace. This contradicts the "stable public identity, independent of display order" comment on `Workspace.id` and the no-upstream-compat stance.
- Unreachable arms in the app-side dispatch (`src/app/api.rs`): `ServerStop` (production always passes `server_stop`), `ServerSshAgentRegister`, `ClientWindowTitle*`, `AgentWait`, `AgentPrompt`.
- `expect()` in production paths, against the no-`unwrap` rule:
  - `responses.rs:5,19`
  - `app/api.rs:565`
  - `panes.rs:132,1485`
  - `tabs.rs:125,139,173`
  - `workspaces.rs:80`
  - `wait.rs:796,807,827`

**Structural recommendation:** replace the thread-per-connection plus 100 ms polling design with one event-driven connection loop, and make the event hub emit a complete, sequenced model diff. Every pane, tab or workspace removal should emit child-first close events from one place, and the sequence should go on the wire. That fixes findings 2, 5, 6 and 7 in one move and removes the 10 Hz `PaneGet`/`PaneRead` fan-out.
