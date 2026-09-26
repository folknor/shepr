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

## APP-026 - `workspace_info` indexes workspaces directly

- `App::workspace_info` (`src/app/creation.rs`) indexes `workspaces[index]`. It has many callers; returning `Option` touches all of them.

## APP-027 - The API layout of a zoomed tab doesn't match the screen

- `pane_layout_snapshot` reports the tiled rects for a zoomed tab, with only a `zoomed` flag. `PaneGeometry::tab_panes(layout, zoomed)` now holds the zoom rule; use it so the API layout matches what is on screen. Other UI files (`src/ui/tab_surface.rs`, mouse hit-testing) were not checked for their own zoom geometry.
