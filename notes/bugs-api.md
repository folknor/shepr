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

## API-008 - Hot-path traps on the server main loop from API reads

- `pane_info` (`src/app/creation.rs`) calls `foreground_cwd`, which does `io.foreground_process_group_id()`: a round-trip to the PTY actor thread with a 1 s `recv_timeout` (`src/pty/actor.rs`), plus /proc reads, per pane, on the main thread.
- It is reached from every `pane.get` (which subscription pollers send at 10 Hz each), `pane.list` and `session.snapshot` (once per pane), and every `PaneUpdated` emission, including stripped-title changes (`terminal_titles.rs`).
- Each `pane.output_matched` subscription runs a full recent-text `PaneRead` on the main thread 10 times a second.

## API-017 - Structural recommendation from the API hunter

- Replace the thread-per-connection plus 100 ms polling design with one event-driven connection loop, and make the event hub emit a complete, sequenced model diff.
- Streams now carry the hub sequence (`SubscriptionStream`, `src/api/subscriptions.rs`), but sampled subscriptions (output match, scroll, agent-status fallback) still poll the app 10 times a second (API-008).

## API-021 - A prompt abandoned at its `--timeout` is still typed later

- `agent.prompt --wait --timeout` answers `timeout` at the deadline (`await_prompt_submission`, `src/app/api/agents.rs`), but the submission stays queued in the PTY actor and may still be written to the agent afterwards. Cancelling it needs a cancel path in `src/pty/`.
- A plain `agent.prompt` (no `--wait`) is deliberately unbounded on both client and server for the same reason; a stalled main loop hangs it (commented in `prompt_agent`).

## API-023 - API handlers index workspaces directly after parsing an id

- Many handlers in `src/app/api/panes.rs` do `self.state.workspaces[ws_idx]` after `parse_*_id`. That panics only if the parsed index is stale, but it is the same class as the production `expect()`s just removed. Use `get`/`get_mut` and return the not-found error.
