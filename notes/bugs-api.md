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

## API-017 - Structural recommendation from the API hunter

- Replace the thread-per-connection plus 100 ms polling design with one event-driven connection loop, and make the event hub emit a complete, sequenced model diff.
- Streams carry the hub sequence (`SubscriptionStream`, `src/api/subscriptions.rs`), but sampled subscriptions (output match, scroll, agent-status fallback) still poll the app 10 times a second; each `pane.output_matched` subscription runs a full recent-text `PaneRead` on the main thread each time.

## API-024 - Text points are 32-bit screen rows on the wire

- `PaneTextPoint.row` is `u32` and names a screen row, which drifts once scrollback is full. The terminal now exposes absolute `u64` rows (`search_text_window_absolute`, `word_motion_target_absolute`, `paragraph_motion_target_absolute`, `extract_selection_absolute` on `PaneTerminal`); widen the wire type and call those readers. Part of TERM-015.
