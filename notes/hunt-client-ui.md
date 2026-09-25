I read the whole client UI scope and followed values into the server snapshot/surface paths, `raw_input`, `render_ansi` and `ghostty/mod.rs`. I did not edit, build or run anything. The findings below are ordered by severity; each one names the claim it breaks.

## High

**1. Keys replayed after a copy-mode request are silently thrown away.**
- Claim: copy mode queues keys while a motion or search request is in flight and "replays" them.
- In copy mode, `handle_key` queues every key while `copy_operation_in_flight` is set (`src/client/shell/input.rs:209`).
- When the response arrives, `complete_copy_operation` → `dispatch_queued_copy_input` → `handle_key(key, &mut outcome)` runs on a local `ClientShellInput` (`src/client/shell/actions.rs:473-499`, `510-558`).
- `handle_endpoint_result` returns only `(repaint, outcome.actions)`. So `outcome.requests` (pane input), `detach`, `resize` and the host queries are dropped.
- Both callers discard the rest: `src/client/mod.rs:1078` and the timer/expiry path at `:1381`.
- User-visible effect: press `w` (or `/foo⏎`, `n`, `$`), then `q`, then type `ls⏎` before the reply lands. `q` is replayed and exits copy mode, but the keystrokes routed to the pane are lost.
- A queued `prefix+b` toggles the sidebar and invalidates the surface but never sends the resize.
- Fix: return the full `ClientShellInput` from `handle_endpoint_result` and route it through `finish_client_shell_input`.

**2. The client can panic when a pane surface is larger than the current layout.**
- `render_selection_highlight` (`src/ui/panes.rs:691`) and `render_client_copy_search_highlights` (`src/client/shell/composition.rs:599`) index `Buffer[(x,y)]` directly. In ratatui-core 0.1.2 that panics when out of bounds.
- The index is `hit.inner_rect` (surface pane rect plus layout offset), which `compose` never clips to the buffer (`composition.rs:232-269`). Only `blit_pane_surface` clips.
- Two ways a mismatched surface gets presented:
  - **Resize race:** Resize, sidebar toggle and sidebar drag call `invalidate_pane_surface`. A surface already in flight at the old size is then accepted by `set_pane_surface`/`install_pane_surface` with no geometry check (`state.rs:1065-1206`).
  - **`hide_tab_bar_when_single_tab`:** going from 1 tab to 2 changes `layout()` from the snapshot alone, with no invalidation (`install_client_shell_snapshot`, `shell_runtime.rs:572-597`). The surface for that revision is one row too tall.
- The selection also survives: `install_pane_surface` only invalidates it when there is a previous surface to compare against.
- Result: any visible selection or copy-search match on the pane's bottom row crashes the client.
- Fix: clip every hit to `layout.pane_surface`, or reject surfaces whose geometry does not match the requested surface size.

**3. Selection and copy-mode coordinates drift once scrollback is full.**
- Rows are "0 = oldest retained line" (`src/ghostty/mod.rs:1083`). Once history hits its cap, every new line evicts the oldest; alacritty bumps `display_offset` (`research/.../grid/mod.rs:267`), and every absolute row shifts by one.
- `state.rs:1136` claims "Ordinary selections are live buffer ranges". Those selections are never adjusted, and neither is the copy-mode cursor or its anchor.
- Live copies (`request_selection_copy(..., live=true)`) then read different text than what was highlighted. This hits exactly the long-running, busy agent panes the tool is for.
- Fix: a monotonic line index from the server, e.g. total lines ever scrolled plus the viewport row.

## Medium

**4. `ui.redraw_on_focus_gained` does nothing in shell mode.**
- The config docs promise to "Force a full host-terminal redraw".
- The shell path only sets `outcome.repaint` (`shell/input.rs:170`), which runs a diffing `encode(frame, repaint_pending=false)` (`protocol/render_ansi.rs:82`).
- `state.request_repaint()` is only called on the direct-attach path (`client/mod.rs:623`).

**5. The shell client never re-queries the host theme after a dark/light switch.**
- `ClientShellInput.query_host_theme` is never set anywhere. Direct attach does this re-query (`raw_input.rs:527`).
- The framer still arms itself to wait for replies that never come (`raw_input.rs:460-464`).
- Effect: default and palette colours stay stale, and so does `host_background`, which drives the selection highlight colour.

**6. Drag-selecting in an unfocused pane gets cancelled by unrelated snapshots.**
- `apply_active_snapshot` clears the selection whenever `focused_pane_id != selection.pane_id` (`state.rs:964-970`).
- The click's `PaneFocus` goes through the serialized command lane. Any snapshot in between (agent title spinners churn these constantly) kills the drag.
- Word gestures have a `focus_confirmed` guard against exactly this; plain selections do not.

**7. Config diagnostics and endpoint notices never go away, and they cost performance.**
- `config_diagnostic` is set once and never cleared. It permanently disables the surface-patch fast path (`surface_patch.rs:67`), so every pane update becomes a full compose.
- `visible_endpoint_notice` can only be dismissed by clicking the toast (`mouse.rs:658`). With `ui.mouse_capture = false` there is no way to dismiss it, and it also blocks the fast path.

**8. A blocking clipboard read can freeze the whole client.**
- Ctrl+V in modal inputs calls `platform::read_clipboard_text` synchronously (`input.rs:383`).
- That spawns `wl-paste`/`xclip`/`xsel` with no timeout (`platform/linux.rs:838`). A hung clipboard owner stalls rendering and input for every pane.

## Low / UX

**9. The screen freezes when an overlay does not fit.**
- `compose` uses `?` on the overlay renderers (`composition.rs:485-507`).
- If a popup does not fit, the whole frame is `None` and nothing, including pane output, is presented until the overlay closes. The navigator needs at least 11 rows; help needs at least 10 rows and 26 columns.

**10. Surface invalidation flashes the wrong screen.**
- Resize, sidebar toggle and sidebar drag fall back to `compose_unavailable`.
- With a collapsed sidebar or several endpoints, that paints the machine list plus "Local: online. Select a connected machine." into the pane area.
- Sidebar dragging also sends one `ClientShellResize` per column, and each one resizes every PTY (`mouse.rs:8-22`).

**11. Esc on the close-confirmation dialog lands you in Navigate mode.**
- It sets `mode = Navigate` unconditionally (`overlay_input.rs:593-599`), whatever the dialog was opened from.

**12. The copy-mode cursor can hide under the mode bar.**
- With the default top tab bar, the mode bar covers the pane's bottom row.
- Motions call `reveal_copy_cursor(.., false)`, so the cursor and selection can sit under the bar. Search results reserve that row; motions do not.

**13. Hit maps are emptied before the matching surface arrives.**
- This happens on every snapshot until its surface lands (`state.rs:883-890`), and when a rev+1 surface is parked.
- Clicks in that window are dropped, entering copy mode silently fails, and a click inside Help or the navigator closes it (the popup rect is empty).

**14. Window title ignores "Empty leaves the title alone".**
- `clear_endpoint_host_effects` writes "shepr" unconditionally (`shell_runtime.rs:112`).
- A `WindowTitle{None}` from the server also becomes "shepr" (`terminal_effects.rs:4`).

**15. Small sidebar and banner gaps.**
- The collapsed single-endpoint sidebar never scrolls, so workspaces past its height are invisible and can't be clicked, yet Navigate mode can still select them.
- The multi-endpoint sidebar ignores `workspace_drop_indicator_row`, although drag-reordering works there.
- `render_config_diagnostic_buffer` sizes the banner by byte length rather than display width.

**16. Dead code left behind by the stripping.**
- The server never emits `confirmation_required`, so that client path is dead and the `chrome_context.rs:502` test fakes it.
- If it were ever wired up, accepting it would send `WorkspaceClose{close_group:true}` for a Tab/PaneClose.
- The `ClientShellKeybindingSource::Local` branch of `apply_snapshot_keybindings` and `local_keys` are unreachable.
- Workspace grouping (`indented`, `last_child`, `suppress_git_details`) is always false.
- The host parser never produces `RawInputEvent::Text`.

## For the endpoint/activation hunter
- `install_client_shell_snapshot` calls `present_frame` (which respects the freeze) when `projection_pending`, and otherwise `present_frozen_chrome` (which bypasses it) (`shell_runtime.rs:605-611`).
- `finish_client_shell_input` bypasses the freeze whenever no activation is pending, including after `present_handoff_unavailable` froze presentation with `pending = None`. That lets full frames with the stale pane surface through.
- This looks inverted relative to the comments.
