# App core defects

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

## APP-029 - Directional pane API calls ignore zoom (decision)

- `pane.edges`, `directional_pane_target` (neighbor, focus-direction, swap) and `pane.resize` in `src/app/api/panes.rs` use tiled geometry even when the tab is zoomed, consistent with TUI navigation (`AppState::navigate_pane`). So `pane.edges` on a zoomed pane reports tiled edges while on screen it touches every edge. Needs a decision on intent.
