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

## UI-001 - Keys replayed after a copy-mode request are silently thrown away

- **Claim:** copy mode queues keys while a motion or search request is in flight and "replays" them.
- In copy mode, `handle_key` queues every key while `copy_operation_in_flight` is set (`src/client/shell/input.rs:209`).
- When the response arrives, `complete_copy_operation` → `dispatch_queued_copy_input` → `handle_key(key, &mut outcome)` runs on a local `ClientShellInput` (`src/client/shell/actions.rs:473-499`, `510-558`).
- `handle_endpoint_result` returns only `(repaint, outcome.actions)`. So `outcome.requests` (pane input), `detach`, `resize` and the host queries are dropped.
- Both callers discard the rest: `src/client/mod.rs:1078` and the timer/expiry path at `:1381`.
- **User-visible effect:** press `w` (or `/foo⏎`, `n`, `$`), then `q`, then type `ls⏎` before the reply lands. `q` is replayed and exits copy mode, but the keystrokes routed to the pane are lost.
- A queued `prefix+b` toggles the sidebar and invalidates the surface but never sends the resize.
- **Suggested fix:** return the full `ClientShellInput` from `handle_endpoint_result` and route it through `finish_client_shell_input`.

## UI-002 - The client can panic when a pane surface is larger than the current layout

- `render_selection_highlight` (`src/ui/panes.rs:691`) and `render_client_copy_search_highlights` (`src/client/shell/composition.rs:599`) index `Buffer[(x,y)]` directly. In ratatui-core 0.1.2 that panics when out of bounds.
- The index is `hit.inner_rect` (surface pane rect plus layout offset), which `compose` never clips to the buffer (`composition.rs:232-269`). Only `blit_pane_surface` clips.
- Two ways a mismatched surface gets presented:
  - **Resize race:** Resize, sidebar toggle and sidebar drag call `invalidate_pane_surface`. A surface already in flight at the old size is then accepted by `set_pane_surface`/`install_pane_surface` with no geometry check (`state.rs:1065-1206`).
  - **`hide_tab_bar_when_single_tab`:** going from 1 tab to 2 changes `layout()` from the snapshot alone, with no invalidation (`install_client_shell_snapshot`, `shell_runtime.rs:572-597`). The surface for that revision is one row too tall. (Related: EP-006.)
- The selection also survives: `install_pane_surface` only invalidates it when there is a previous surface to compare against.
- **Result:** any visible selection or copy-search match on the pane's bottom row crashes the client.
- **Suggested fix:** clip every hit to `layout.pane_surface`, or reject surfaces whose geometry does not match the requested surface size.

## UI-003 - `ui.redraw_on_focus_gained` does nothing in shell mode

- The config docs promise to "Force a full host-terminal redraw".
- The shell path only sets `outcome.repaint` (`shell/input.rs:170`), which runs a diffing `encode(frame, repaint_pending=false)` (`protocol/render_ansi.rs:82`).
- `state.request_repaint()` is only called on the direct-attach path (`client/mod.rs:623`).

## UI-004 - The shell client never re-queries the host theme after a dark/light switch

- `ClientShellInput.query_host_theme` is never set anywhere. Direct attach does this re-query (`raw_input.rs:527`).
- The framer still arms itself to wait for replies that never come (`raw_input.rs:460-464`).
- **Effect:** default and palette colours stay stale, and so does `host_background`, which drives the selection highlight colour.

## UI-005 - Drag-selecting in an unfocused pane gets cancelled by unrelated snapshots

- `apply_active_snapshot` clears the selection whenever `focused_pane_id != selection.pane_id` (`state.rs:964-970`).
- The click's `PaneFocus` goes through the serialized command lane. Any snapshot in between (agent title spinners churn these constantly) kills the drag.
- Word gestures have a `focus_confirmed` guard against exactly this; plain selections do not.

## UI-006 - Config diagnostics and endpoint notices never go away, and they cost performance

- `config_diagnostic` is set once and never cleared. It permanently disables the surface-patch fast path (`surface_patch.rs:67`), so every pane update becomes a full compose.
- `visible_endpoint_notice` can only be dismissed by clicking the toast (`mouse.rs:658`). With `ui.mouse_capture = false` there is no way to dismiss it, and it also blocks the fast path.
- See also SRV-006 for the server side.

## UI-007 - A blocking clipboard read can freeze the whole client

Surfaced in two scopes: client UI, platform.

- Ctrl+V in modal inputs calls `platform::read_clipboard_text` synchronously from key routing (`src/client/shell/input.rs:383`).
- That spawns `wl-paste`/`xclip`/`xsel` with no timeout (`platform/linux.rs:838`). A hung clipboard owner (e.g. `xclip -out` against an unresponsive selection owner) stalls rendering and input for every pane, indefinitely.

## UI-008 - The screen freezes when an overlay does not fit

- `compose` uses `?` on the overlay renderers (`composition.rs:485-507`).
- If a popup does not fit, the whole frame is `None` and nothing, including pane output, is presented until the overlay closes. The navigator needs at least 11 rows; help needs at least 10 rows and 26 columns.

## UI-009 - Surface invalidation flashes the wrong screen

- Resize, sidebar toggle and sidebar drag fall back to `compose_unavailable`.
- With a collapsed sidebar or several endpoints, that paints the machine list plus "Local: online. Select a connected machine." into the pane area.
- Sidebar dragging also sends one `ClientShellResize` per column, and each one resizes every PTY (`mouse.rs:8-22`).

## UI-010 - Esc on the close-confirmation dialog lands you in Navigate mode

- It sets `mode = Navigate` unconditionally (`overlay_input.rs:593-599`), whatever the dialog was opened from.

## UI-011 - The copy-mode cursor can hide under the mode bar

- With the default top tab bar, the mode bar covers the pane's bottom row.
- Motions call `reveal_copy_cursor(.., false)`, so the cursor and selection can sit under the bar. Search results reserve that row; motions do not.

## UI-012 - Hit maps are emptied before the matching surface arrives

- This happens on every snapshot until its surface lands (`state.rs:883-890`), and when a rev+1 surface is parked.
- Clicks in that window are dropped, entering copy mode silently fails, and a click inside Help or the navigator closes it (the popup rect is empty).

## UI-013 - Window title ignores "Empty leaves the title alone"

- `clear_endpoint_host_effects` writes "shepr" unconditionally (`shell_runtime.rs:112`).
- A `WindowTitle{None}` from the server also becomes "shepr" (`terminal_effects.rs:4`).

## UI-014 - Small sidebar and banner gaps

- The collapsed single-endpoint sidebar never scrolls, so workspaces past its height are invisible and can't be clicked, yet Navigate mode can still select them.
- The multi-endpoint sidebar ignores `workspace_drop_indicator_row`, although drag-reordering works there.
- `render_config_diagnostic_buffer` sizes the banner by byte length rather than display width.

## UI-015 - Dead client code left behind by the stripping

- The server never emits `confirmation_required`, so that client path is dead and the `chrome_context.rs:502` test fakes it. If it were ever wired up, accepting it would send `WorkspaceClose{close_group:true}` for a Tab/PaneClose.
- The `ClientShellKeybindingSource::Local` branch of `apply_snapshot_keybindings` and `local_keys` are unreachable.
- Workspace grouping (`indented`, `last_child`, `suppress_git_details`) is always false.
- The host parser never produces `RawInputEvent::Text`.
