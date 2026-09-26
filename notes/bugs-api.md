# JSON API defects

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

## API-004 - `pane read --raw` is identical to `--ansi`

- `strip_ansi` now works server-side, but there is no raw PTY history to return, so the CLI's `pane read --raw` (sets `strip_ansi=false`) produces exactly what `--ansi` does. Drop the flag or document it as an alias.

## API-005 - Advertised event types that never fire

- `EventData::WorkspaceUpdated` is never built anywhere, so a `workspace.updated` subscription is accepted and never fires.
- `EventKind::PaneOutputChanged` and `EventData::PaneOutputChanged` are never emitted.
- `EventMatch` accepts 16 variants, but `events.wait` supports exactly one (`wait.rs:765-785`); the rest parse fine and then fail with `unsupported_event_wait_match`.

## API-006 - A stream with several subscriptions delivers events out of order

- `stream_subscriptions` (`server.rs:619-651`) drains each subscription's history in turn. With `[pane.closed, pane.created]`, a create-then-close in one poll window arrives as closed, then created.
- Events carry no sequence number on the wire, so clients cannot reorder them. The hub has a global sequence; the stream discards it.

## API-008 - Hot-path traps on the server main loop from API reads

- `pane_info` (`src/app/creation.rs:307`) calls `foreground_cwd`, which does `io.foreground_process_group_id()`: a round-trip to the PTY actor thread with a 1 s `recv_timeout` (`src/pty/actor.rs:209-216`), plus /proc reads, per pane, on the main thread.
- It is reached from every `pane.get` (which subscription pollers send at 10 Hz each), `pane.list` and `session.snapshot` (once per pane), and every `PaneUpdated` emission, including stripped-title changes (`terminal_titles.rs:72`).
- Each `pane.output_matched` subscription runs a full recent-text `PaneRead` on the main thread 10 times a second.

## API-009 - Latency on every API request from byte-at-a-time reads

Surfaced in two scopes: JSON API, wire protocol.

- `read_initial_request_line` (`server.rs:529-570`) reads one byte per syscall, and sleeps 100 ms on the first `Pending`.
- A client that writes right after connecting often loses that race, so each CLI and hook call can pay about +100 ms. A 1 MiB request costs about 1M read syscalls.

## API-010 - No timeout on ordinary requests

- `handle_request` → `dispatch_to_app(..., None, None)` blocks forever; `APP_RESPONSE_TIMEOUT` only applies to internal pollers.
- `ApiClient::request` / `request_value` set no timeout either. A stalled main loop hangs every CLI call and every agent hook that shells out to it.

## API-013 - `read_runtime_status_at` misreports a stalled server

- `read_runtime_status_at` (`src/api/status.rs:34-43`) only maps `TimedOut` to "not running". A timed-out receive on this socket reports `WouldBlock` (the client test at `client.rs:278` already expects either), so a stalled server shows up as an opaque error.

## API-014 - `CLIENT_SHELL_METHODS` advertises methods that don't exist

- `CLIENT_SHELL_METHODS` (`src/server/client_commands.rs:25-26`) advertises `pane.link.activate` / `pane.link.resolve`, which don't exist in `Method`: leftovers from a stripped feature.

## API-015 - Unreachable arms in the app-side dispatch

- In `src/app/api.rs`: `ServerStop` (production always passes `server_stop`), `ServerSshAgentRegister`, `ClientWindowTitle*`, `AgentWait`, `AgentPrompt`.

## API-016 - Production `expect()` in the API layer

- **Claim:** no `unwrap` in production code.
- `responses.rs:5,19`; `app/api.rs:565`; `panes.rs:132`; `tabs.rs:125,139,173`; `workspaces.rs:80`; `wait.rs:796,807,827`. Line numbers predate a round of edits in these files; re-locate before fixing.

## API-017 - Structural recommendation from the API hunter

- Replace the thread-per-connection plus 100 ms polling design with one event-driven connection loop, and make the event hub emit a complete, sequenced model diff, with the sequence on the wire.
- The hunter's claim: that fixes API-005 and API-006 in one move and removes the 10 Hz `PaneGet`/`PaneRead` fan-out (API-008). Close events for removed children are now emitted per handler (`tab_close_events` / `workspace_close_events` in `src/app/api.rs`); a single emission point would subsume those.

## API-018 - The API listener thread outlives its `ServerHandle`

- Dropping `ServerHandle` (`src/api/server.rs`) clears `running` and removes the socket file, but the listener thread stays blocked in `accept` with the fd open until the process exits. The listener only checks `running` after an accept returns.

## API-019 - `WorkspaceCloseParams.close_group` is dead

- The CLI no longer offers `--group` and always sends `close_group: false`; `handle_workspace_close` never reads it. Drop the field from `src/api/schema/workspaces.rs`.

## API-020 - Closing tabs or workspaces from the UI may emit no API events (unverified)

- API handlers now emit child-first close events, but closes driven by keybindings (`src/app/actions`, input) go through different paths. Not checked whether they emit `PaneClosed`/`TabClosed`/`WorkspaceClosed`.

## API-021 - A prompt abandoned at its `--timeout` is still typed later

- `agent.prompt --wait --timeout` now answers `timeout` at the deadline (`await_prompt_submission`, `src/app/api/agents.rs`), but the submission stays queued in the PTY actor and may still be written to the agent afterwards. Cancelling it needs a cancel path in `src/pty/`.
