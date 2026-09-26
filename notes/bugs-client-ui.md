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

## UI-006 - Config diagnostics and endpoint notices never go away, and they cost performance

- `config_diagnostic` is set once and never cleared. It permanently disables the surface-patch fast path (`surface_patch.rs`), so every pane update becomes a full compose.
- `visible_endpoint_notice` can only be dismissed by clicking the toast (`mouse.rs`). With `ui.mouse_capture = false` there is no way to dismiss it, and it also blocks the fast path.

## UI-007 - A blocking clipboard read can freeze the whole client

Surfaced in two scopes: client UI, platform.

- Ctrl+V in modal inputs calls `platform::read_clipboard_text` synchronously from key routing (`src/client/shell/input.rs`).
- That spawns `wl-paste`/`xclip`/`xsel` with no timeout (`platform/linux.rs`). A hung clipboard owner (e.g. `xclip -out` against an unresponsive selection owner) stalls rendering and input for every pane, indefinitely.

## UI-008 - The screen freezes when an overlay does not fit

- `compose` uses `?` on the overlay renderers (`composition.rs`).
- If a popup does not fit, the whole frame is `None` and nothing, including pane output, is presented until the overlay closes. The navigator needs at least 11 rows; help needs at least 10 rows and 26 columns.

## UI-009 - Surface invalidation flashes the wrong screen

- Resize, sidebar toggle and sidebar drag fall back to `compose_unavailable`.
- With a collapsed sidebar or several endpoints, that paints the machine list plus "Local: online. Select a connected machine." into the pane area.
- Sidebar dragging also sends one `ClientShellResize` per column, and each one resizes every PTY (`mouse.rs`).

## UI-011 - The copy-mode cursor can hide under the mode bar

- With the default top tab bar, the mode bar covers the pane's bottom row.
- Motions call `reveal_copy_cursor(.., false)`, so the cursor and selection can sit under the bar. Search results reserve that row; motions do not.

## UI-012 - Hit maps are emptied before the matching surface arrives

- This happens on every snapshot until its surface lands (`state.rs`), and when a rev+1 surface is parked.
- Clicks in that window are dropped, entering copy mode silently fails, and a click inside Help or the navigator closes it (the popup rect is empty).

## UI-014 - Small sidebar and banner gaps

- The collapsed single-endpoint sidebar never scrolls, so workspaces past its height are invisible and can't be clicked, yet Navigate mode can still select them.
- The multi-endpoint sidebar ignores `workspace_drop_indicator_row`, although drag-reordering works there.
- `render_config_diagnostic_buffer` sizes the banner by byte length rather than display width.

## UI-015 - Dead client code left behind by the stripping

- The server never emits `confirmation_required`, so that client path is dead and the `chrome_context.rs` test fakes it. If it were ever wired up, accepting it would send `WorkspaceClose{close_group:true}` for a Tab/PaneClose.
- The `ClientShellKeybindingSource::Local` branch of `apply_snapshot_keybindings` and `local_keys` are unreachable.
- Workspace grouping (`indented`, `last_child`, `suppress_git_details`) is always false.
- The host parser never produces `RawInputEvent::Text`.

## UI-016 - A mismatched pane surface is still presented, and the other renderers are unaudited for direct indexing

- `compose` deliberately leaves pane hits unclipped; the selection, copy-search and copy-cursor draws go through `cell_mut`. But a surface larger than the layout (in-flight surface after a resize or sidebar toggle, or the `hide_tab_bar_when_single_tab` 1→2 tab switch; see EP-006) is still accepted by `set_pane_surface`/`install_pane_surface` with no geometry check. For that frame, mouse hit-testing and copy-mode cursor placement use rows that aren't on screen.
- A selection survives `install_pane_surface` when there is no previous surface to compare against, and can highlight stale coordinates for a frame.
- The non-pane renderers `compose` calls (sidebar, tab bar, overlays, notices, config-diagnostic banner) take layout-derived rects and were not audited for direct `Buffer[(x,y)]` indexing.
- `restore_mode_bar` and the `mode_bar_cells` slice index `frame.cells` directly; safe only while `render_mode_bar` returns a rect inside the frame.

## UI-017 - Queued copy keys are dropped when the copy pane disappears

- When a snapshot removes the copy-mode pane, `apply_active_snapshot` drops any queued copy-mode keys silently. Queued keys after a mode-exiting key are now replayed in the new mode, but this path never replays them. Possibly intended (they were copy motions); nobody has decided.
