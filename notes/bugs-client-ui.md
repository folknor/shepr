# Client UI defects

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

## UI-017 - Queued copy keys are dropped when the copy pane disappears

- When a snapshot removes the copy-mode pane, `apply_active_snapshot` drops any queued copy-mode keys silently. Queued keys after a mode-exiting key are replayed in the new mode, but this path never replays them. Possibly intended (they were copy motions); nobody has decided.

## UI-020 - Client selections and copy mode still use screen rows

- The claim "Ordinary selections are live buffer ranges" (`src/client/shell/state.rs`) stays untrue until the client stores selections and the copy-mode cursor/anchor with the absolute-row `_at` methods in `src/selection.rs`, using viewport top = `history_origin + max_offset - offset` from the new scroll metrics. Part of TERM-015.

## UI-021 - Two client handlers still ignore the unavailable-handoff freeze

- The window-resize handler and the `ClientShellError` handler in `src/client/mod.rs` still use `present_frame`; while presentation is frozen after an unavailable handoff, a resize leaves a wrongly sized frame until the freeze ends. They probably belong on `ClientState::present_chrome`.
- A disconnect during a handoff builds `"endpoint connection was lost while activating {notice}"`, which reads badly with the existing notices ("...; reconnecting", "is no longer an enabled saved machine").
