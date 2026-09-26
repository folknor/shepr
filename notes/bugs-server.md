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
- The per-wake and per-event costs were cut. What remains is the hunter's structural point: the event loop mixes per-client presentation state (`foreground_client_id`, `effective_size`, global `app.state.active`, `outer_terminal_focus`, the diagnostic copies) with session state. Make each client's view the only source of presentation truth and drop the global "foreground client" projection into `AppState`.
- `terminal_id_by_string` is still a linear scan; `TerminalId` (`src/terminal/`) has no `Borrow<str>` to key a map by `&str`.
- `app.session_snapshot()` always builds `layouts`, which the shell projection never reads.
