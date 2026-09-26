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

## SRV-011 - Presentation state is global, not per client

- **Claim:** AGENTS.md "Hot paths multiply".
- The per-wake and per-event costs were cut (dirty flags for PTY-source and input-mode syncs, allocation-free terminal id lookup, focus-only sync on agent events, one session snapshot per render). What remains is the hunter's structural point: the event loop mixes per-client presentation state (`foreground_client_id`, `effective_size`, global `app.state.active`, `outer_terminal_focus`, the diagnostic copies) with session state. Make each client's view the only source of presentation truth and drop the global "foreground client" projection into `AppState`. This is behind APP-010's app half.
- `terminal_id_by_string` is still a linear scan; `TerminalId` (`src/terminal/`) has no `Borrow<str>` to key a map by `&str`.
- `app.session_snapshot()` always builds `layouts`, which the shell projection never reads.

## SRV-016 - Direct-attach clients cannot be told about dropped input or oversized frames

- Shell clients get a `ClientShellError` when pane input is dropped under backpressure, a paste is rejected, or the screen is too large to send in one frame. Direct terminal-attach clients have no message for it; the server only logs. Needs a protocol message if it matters.

## SRV-017 - Broadcast clones the frame per client

- `send_to_all_clients` (`src/server/headless.rs`) clones the whole frame once per client. An `Arc<[u8]>` queue item would share one buffer.
