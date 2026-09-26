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

## API-006 - A stream with several subscriptions delivers events out of order

- `stream_subscriptions` (`src/api/server.rs`) drains each subscription's history in turn. With `[pane.closed, pane.created]`, a create-then-close in one poll window arrives as closed, then created.
- Events carry no sequence number on the wire, so clients cannot reorder them. The hub has a global sequence; the stream discards it.

## API-008 - Hot-path traps on the server main loop from API reads

- `pane_info` (`src/app/creation.rs`) calls `foreground_cwd`, which does `io.foreground_process_group_id()`: a round-trip to the PTY actor thread with a 1 s `recv_timeout` (`src/pty/actor.rs`), plus /proc reads, per pane, on the main thread.
- It is reached from every `pane.get` (which subscription pollers send at 10 Hz each), `pane.list` and `session.snapshot` (once per pane), and every `PaneUpdated` emission, including stripped-title changes (`terminal_titles.rs`).
- Each `pane.output_matched` subscription runs a full recent-text `PaneRead` on the main thread 10 times a second.

## API-015 - Unreachable arms in the app-side dispatch

- In `src/app/api.rs`: `ServerStop` (production always passes `server_stop`), `ServerSshAgentRegister`, `ClientWindowTitle*`, `AgentWait`, `AgentPrompt`.

## API-016 - Production `expect()` in the API layer

- **Claim:** no `unwrap` in production code.
- `responses.rs`; `app/api.rs`; `app/api/panes.rs`; `app/api/tabs.rs`; `app/api/workspaces.rs`; `wait.rs` (`wait_matched_response`, three calls). Several sites were already converted in passing; re-locate each before fixing.

## API-017 - Structural recommendation from the API hunter

- Replace the thread-per-connection plus 100 ms polling design with one event-driven connection loop, and make the event hub emit a complete, sequenced model diff, with the sequence on the wire.
- The hunter's claim: that fixes API-006 in one move and removes the 10 Hz `PaneGet`/`PaneRead` fan-out (API-008). Close events for removed children are now emitted per handler (`tab_close_events` / `workspace_close_events` in `src/app/api.rs`); a single emission point would subsume those.

## API-020 - Closing tabs or workspaces from the UI may emit no API events (unverified)

- API handlers emit child-first close events, but closes driven by keybindings (`src/app/actions`, input) go through different paths. Not checked whether they emit `PaneClosed`/`TabClosed`/`WorkspaceClosed`.

## API-021 - A prompt abandoned at its `--timeout` is still typed later

- `agent.prompt --wait --timeout` answers `timeout` at the deadline (`await_prompt_submission`, `src/app/api/agents.rs`), but the submission stays queued in the PTY actor and may still be written to the agent afterwards. Cancelling it needs a cancel path in `src/pty/`.
- A plain `agent.prompt` (no `--wait`) is deliberately unbounded on both client and server for the same reason; a stalled main loop hangs it (commented in `prompt_agent`).

## API-022 - The API client has its own polling `DeadlineReader`

- `src/api/client.rs` defines a `DeadlineReader` that polls every 2 ms, duplicating `crate::ipc::DeadlineReader` (which resets `SO_RCVTIMEO` to the time left before each read). Use the ipc one.
