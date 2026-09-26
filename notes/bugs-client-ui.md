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

## UI-009 - Surface invalidation flashes the wrong screen

- Resize, sidebar toggle and sidebar drag fall back to `compose_unavailable`.
- With a collapsed sidebar or several endpoints, that paints the machine list plus "Local: online. Select a connected machine." into the pane area.
- Sidebar dragging also sends one `ClientShellResize` per column, and each one resizes every PTY (`mouse.rs`).

## UI-015 - Dead client code left behind by the stripping

- The server never emits `confirmation_required`, so that client path is dead and the `chrome_context.rs` test fakes it.
- The `ClientShellKeybindingSource::Local` branch of `apply_snapshot_keybindings` and `local_keys` are unreachable.
- Workspace grouping (`indented`, `last_child`, `suppress_git_details`) is always false.
- The host parser never produces `RawInputEvent::Text`.

## UI-016 - A mismatched pane surface is still presented, and some renderers index the buffer directly

- `compose` deliberately leaves pane hits unclipped; the selection, copy-search and copy-cursor draws go through `cell_mut`. But a surface larger than the layout (in-flight surface after a resize or sidebar toggle, or the `hide_tab_bar_when_single_tab` 1→2 tab switch; see EP-006) is still accepted by `set_pane_surface`/`install_pane_surface` with no geometry check. For that frame, mouse hit-testing and copy-mode cursor placement use rows that aren't on screen.
- A selection survives `install_pane_surface` when there is no previous surface to compare against, and can highlight stale coordinates for a frame.
- Still reading `buffer[(x, y)]` directly: `render_mode_bar` (bar fill and search-prompt cursor), `overlays.rs` `panel` and the help scrollbar, `sidebar.rs` `render_workspace_rows`.
- `restore_mode_bar` and the `mode_bar_cells` slice index `frame.cells` directly; safe only while `render_mode_bar` returns a rect inside the frame.

## UI-017 - Queued copy keys are dropped when the copy pane disappears

- When a snapshot removes the copy-mode pane, `apply_active_snapshot` drops any queued copy-mode keys silently. Queued keys after a mode-exiting key are replayed in the new mode, but this path never replays them. Possibly intended (they were copy motions); nobody has decided.
