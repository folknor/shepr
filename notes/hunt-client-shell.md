# Defect hunt: client shell

Scope: `crates/shepr-client/src/shell.rs` and `crates/shepr-client/src/shell/`.
Findings are ordered by severity. Each names the claim it breaks. Line numbers
are avoided; functions are named instead.

## 1. Overlays are never drawn when there is no presentable surface, but still own input

Where: `ClientShellState::compose_unavailable` (`shell/presentation/composition.rs`).

`compose` returns `compose_unavailable` whenever `snapshot` or `pane_surface` is
`None`. That path draws the sidebar, a status line, the mode bar and the notice
card, and never looks at `self.overlay`. Meanwhile every overlay keeps routing
keys (`route_key_press` sends everything to `route_overlay_key` while
`self.overlay.is_some()`) and mouse events (`handle_mouse_with_accounting`).

Broken claims:

- `compose`'s own contract for an overlay that cannot be drawn: "a one-line hint
  says why the overlay is missing ... Overlay hit rects stay empty". On this path
  there is no hint, and the overlay is not too big; it is simply skipped.
- AGENTS.md: "With machines configured, losing the local server does not end the
  client either: it keeps serving the remote machines". With Local active and
  down, the session navigator (prefix+g) is the keyboard route to a remote
  machine, and it opens invisible. Typing then edits an unseen query, and Enter
  activates whatever row happens to be selected.
- The expanded sidebar still draws the `menu` launcher and sets
  `hits.global_launcher` on this path, so clicking it opens a `GlobalMenu` that
  is never painted. The next click anywhere closes it, because
  `hits.global_menu_rows` is empty.
- `compose` says the mode bar is drawn "only when no overlay is open". The
  unavailable path draws it with an overlay open.

Fix direction: one composition pipeline. Make "no surface yet" a placeholder
layer in the pane area, and run overlays, notices and the mode bar through the
same stages as in `compose`. See structural note A.

## 2. Cancellation drops follow-up work but keeps the state that work set: copy mode and pane scrolling can wedge

Where: `cancel_endpoint_request_with_notice` (`shell/navigation/actions.rs`),
`complete_copy_operation` and `dispatch_queued_copy_input`
(`shell/input/copy_mode.rs`). Callers are `cancel_unsent_endpoint_request`
(from `dispatch_client_shell_actions` in `shell_runtime.rs`, when the active
endpoint does not own presentation, and from `cancel_endpoint_commands`) and
`cancel_endpoint_request` (from `lib.rs` for completions on another generation
or a non-active endpoint, and from `mark_endpoint_disconnected`).

The comment in `cancel_endpoint_request_with_notice` says: "A cancelled
copy-mode request does not continue its key queue (`continue_queue` is false on
every error), so nothing but a repaint can come out of it." That is false.
`complete_copy_operation` with `continue_queue == false` still calls
`dispatch_queued_copy_input` whenever `copy_mode_owns_input()` holds ("Failed
copy requests still release the buffered input"). Replayed keys can:

- issue a new copy motion or search. `dispatch_next_copy_operation` sets
  `copy_operation_in_flight = true`, inserts a `pending_requests` entry and
  pushes a `ClientShellAction::Endpoint`;
- exit copy mode (`y`, Enter, `q` from the queue). `exit_copy_mode` then pushes
  `PaneSelectionRead` and `PaneScroll`. `dispatch_pane_scroll_offset` inserts
  `pane_scroll_in_flight[pane]`;
- after that exit, route the keys behind it to the pane as
  `ClientShellPaneInput` requests (the "put them back" logic in
  `dispatch_queued_copy_input`).

The cancel wrapper keeps only `outcome.repaint` and logs the rest as dropped.
On the handoff path (`cancel_unsent_endpoint_request` while the endpoint is
still online), `push_endpoint_command_with_kind` succeeds, so the dropped work
leaves behind:

- `copy_operation_in_flight == true`, with a pending request that will never
  complete or expire (it never reached the endpoint command queue, so nothing
  times it out). Every later copy-mode key is queued in `handle_key` until
  `MAX_COPY_INPUT_QUEUE`, then refused with "copy-mode input queue is full".
  Copy mode is wedged until an interrupt key (Esc, `q`, prefix) exits it.
- `pane_scroll_in_flight[pane]` with no request behind it. From then on,
  `push_pane_scroll_offset` for that pane only writes `pane_scroll_queued`.
  Copy-mode paging, scrollbar drags and selection autoscroll stop scrolling
  that pane until a reboot, a disconnect or an endpoint switch clears the maps.
- typed keystrokes silently lost.

The test-only `handle_endpoint_result` says it plainly: "The caller must route
the whole outcome ... or the replayed keystrokes are lost." The cancel path
breaks that rule.

Fix direction: make cancellation return a full `ClientShellInput` and route it
through `finish_client_shell_input` like any result. Alternatively, make a
cancelled copy operation discard the queue instead of replaying it. The general
fix is in structural note B.

## 3. Surface patches only ever target the visible surface; a pending or dropped baseline fails the connection

Where: `apply_pane_surface_patch` (`shell/presentation/surface_patch.rs`),
`set_pane_surface` and `install_pane_surface` (`shell/state.rs`). The caller in
`lib.rs` fails the endpoint connection on `Rejected`.

Two ways the shell's surface falls out of step with the reader's accepted
baseline:

- `set_pane_surface` parks a surface whose `projection_revision` is
  `snapshot.revision + 1` in `pending_pane_surface`, so `pane_surface` stays at
  N. A patch built on that pending N+1 surface (projection N+1, base = the
  pending surface revision) is checked only against `pane_surface` and
  rejected. The state field comment allows this ordering: "A future projection
  surface waits here until its matching snapshot arrives."
- In the slow path (`fast_path_blocker` is `Some`), the patched copy goes
  through `set_pane_surface(next)`. When the snapshot has moved past the
  visible surface (the `projection_gap` case the blocker names), `next` has
  `projection_revision < snapshot.revision`. `set_pane_surface` silently
  returns, and `apply_pane_surface_patch` still reports `Applied(None)`. The
  shell's `surface_revision` has not advanced, so the next patch on that
  baseline is `Rejected` and the connection is torn down. `Applied` is a false
  report here.

Whether either sequence occurs depends on server ordering of snapshots, surfaces
and patches. The client code explicitly allows these orders, so its handling
must not fail the connection. Fix direction: keep the server baseline (the last
accepted full surface plus patches) separate from what is presentable, and
apply patches to the baseline. See structural note C.

## 4. A pane-split drag released during a projection gap drops the final ratio

Where: the `ClientChromeDrag::PaneSplit` arms in `handle_mouse_with_accounting`
(`shell/input/mouse.rs`).

During the drag, every admitted `LayoutSetSplitRatio` bumps the projection
revision. Until the matching surface arrives, `pane_split_target_is_current`
returns `None`. Drag events in that window are ignored, and the release sends
its ratio only when the check returns `Some(true)`. The send throttle
(`MOUSE_DRAG_SEND_INTERVAL`) means the last motion before release is often not
the last ratio sent. A release that lands in the gap, which is likely right
after a send, leaves the split at the last throttled ratio rather than where the
pointer was released. The release path exists precisely to send that final
ratio.

Fix: on release, send the final ratio when the topology signature still
matches the hit's, even during a revision gap. Alternatively, defer it until
the surface catches up.

## 5. The navigate-mode bar hardcodes keys that are configurable

Where: `render_mode_bar`, `ClientShellMode::Navigate` arm
(`shell/presentation/render.rs`).

The bar prints `esc back`, `↑/↓ workspace` and `tab pane`. Those actions are
`navigate_back`, `navigate_workspace_up`/`navigate_workspace_down` and
`navigate_cycle_pane_next` in `keybinding_table.rs`, all rebindable. The Prefix
bar next to it is config-driven (`prefix_rhs`). The keybinding help overlay is
config-driven too. Broken claim (AGENTS.md): "The client applies its own config
to everything it draws and interprets: keys, ...". After a rebind, the bar
advertises dead keys.

## 6. "Cycle pane" picks its next pane from two different orders

Where: `cycle_pane` (`shell/input/input.rs`, used by navigate mode) and the
`CyclePaneNext`/`CyclePanePrevious` arm of `endpoint_command_for_action`
(`shell/navigation/actions.rs`, used by the prefix binding).

Navigate mode cycles over `pane_surface.panes`: the panes on the presented
surface, in surface order, which may be a zoomed or retained-future layout. The
prefix binding cycles over `snapshot.panes` filtered by focused workspace, in
snapshot order. Both are labelled "cycle pane" in the binding table. The same
key action can land on different panes depending on the mode, and a zoomed
workspace can cycle differently in each. Pick one source (the snapshot) for
both.

## 7. Copy-mode replay drops queued keys when the reply arrives in Prefix mode

Where: `complete_copy_operation` (`shell/input/copy_mode.rs`).

The prefix key is a copy-mode interrupt key, so it is not queued. It switches
`mode` to Prefix. If the in-flight reply arrives before the prefix sequence
finishes, `copy_mode_owns_input()` is false only because `mode != Copy`. The
`else` branch then clears `copy_input_queue`, with the comment "These keys
belonged to the copy pane. Do not send them into a pane that gained focus while
the request was outstanding". No pane gained focus, and the keys are silently
lost. Interrupt keys (Esc, prefix) also run ahead of keys typed before them that
are still queued. For example, `w v Esc`: Esc runs first, then `v` starts a
selection the user meant to cancel.

## 8. Restore notices: a single slot, keyed to the wrong boot, wiped by unrelated resets

Where: `receive_server_notice` and `push_endpoint_notice`
(`shell/navigation/actions.rs`), and `reset_endpoint_projection`
(`shell/state.rs`).

- The server sends `SessionRestoreIncomplete` "After the snapshot, so the client
  keys the notice to this boot" (`server/headless.rs`). The client keys it with
  `self.snapshot`'s boot, which is the active endpoint's boot even when the
  notice came from another machine.
- There is one `visible_endpoint_notice` slot. At startup with several
  partially restored machines, each notice replaces the previous one, and all
  but the last are never seen. The commit that added it claims every client is
  told.
- `reset_endpoint_projection` (every endpoint switch and every boot change)
  clears `visible_endpoint_notice`, so switching to the machine the notice is
  about erases it.

## 9. Lost-release drags are only half recovered

Where: `MouseEventKind::Down(MouseButton::Left)` in
`handle_mouse_with_accounting`.

A `SidebarWidth` drag whose release never arrived gets its owed resize on the
next press ("still owes the endpoint its resize"), but not its
`persist_chrome_preferences`. A lost `SidebarSection` release is not persisted
either. A lost `PaneSplit` release never sends its final throttled ratio.
`ClientChromePreferences` claims to remember manual chrome changes across
launches, and these changes are not remembered.

## 10. The "Action interrupted" notice fires for internal reads

Where: `apply_endpoint_result`, the `Cancelled` arm.

`mark_endpoint_disconnected` cancels every pending request with
`show_cancelled_notice = true`. Pane scroll, word-selection row reads, copy
motions and copy searches are reads with no server-side effect. The notice text
"This server action was interrupted. Check its state before retrying." describes
a user action with an unknown outcome. That fits close or rename, not these.

## 11. Two width models in one sidebar row (smell, can misalign)

The width rules disagree:

- `sidebar_tokens.rs` budgets and truncates with `unicode_width`
  (`UnicodeWidthStr`, `UnicodeWidthChar`).
- `agent_sidebar.rs::put_text` and `display_width` use
  `shepr_vt::unicode_display_units` / `unicode_text_width`. Its tests assert
  `display_width("\u{263a}\u{fe0f}") == 1` "matches terminal".
- `render.rs::put_text` uses ratatui `set_stringn`, which is `unicode_width`.
- `wire_cells.rs` says "Both use the one width rule output uses
  (`shepr_termio::blit::symbol_width`)". Its glyph repair runs over chrome that
  ratatui laid out under a different rule.

Agent titles routinely contain emoji and VS16 sequences. A token budgeted at
one width and drawn or repaired at another truncates wrongly, or leaves a blank
continuation cell. Use one width function everywhere chrome text is measured.

## 12. Index- and path-addressed layout commands race other clients

Where: `workspace_move_command` (`insert_index` from the client's snapshot
order) and the split drag (`LayoutSetSplitRatio { path }`).

Presentation is per client and several TUI clients can share a server. A
workspace created, closed or moved by another client between this client's
snapshot and its command makes `insert_index` name a different slot. The server
applies it without complaint. The `topology_signature` check guards only
against this client's own stale surface. Addressing by identity would close
this: "before workspace X", and a split named by its child pane ids.

## 13. Copy-mode cursor and selection anchor are not clamped when history evicts their rows

Where: `install_pane_surface` (`shell/state.rs`).

When `history_origin` advances, `prune_evicted_search_matches` drops evicted
search matches, but `copy_mode.cursor` and `copy_mode.selection`'s anchor keep
rows below the origin. The cursor is then drawn nowhere
(`client_copy_cursor_cell` returns `None`), and the next motion sends an origin
row the server no longer has. The module comment ("output that evicts history
does not move the line the cursor ... names") assumes the row still exists.
Clamp the cursor to `retained_row` the way `move_copy_cursor` already does.

## 14. Smaller items

- `receive_endpoint_error(message)` hardcodes code `paste_rejected` and title
  "Paste rejected". The generic name hides that it serves exactly one purpose,
  and its only caller is `push_focused_paste`.
- The collapsed sidebar labels machines by endpoint index (`L`, then `2`, `3`,
  ...), so the first configured machine reads `2`.
- `compose` stamps `last_composed_at` and clears `selection_repaint_deadline`
  even when it returns `None` (pending surface, generation or revision
  mismatch). `request_selection_drag_repaint` then throttles against a frame
  that was never presented.
- `render_global_menu` anchors to `hits.global_launcher`. If the sidebar
  collapses while the menu is open (keybind), the launcher rect is empty and
  the menu jumps to the top-left corner.
- `handle_endpoint_machine_click` on the active endpoint's row both toggles its
  collapse and pushes `ActivateEndpoint` for the endpoint that is already
  active.

## Performance note (hot path)

`fast_path_blocker` sends every pane patch through a full `compose` while
`copy_mode.is_some()` or `selection.is_some()`, whatever pane they belong to.
Copy mode stays parked (`copy_mode` is `Some`) on a pane that lost focus, so
leaving a pane in copy mode and moving on makes every byte of output in every
other pane recompose the whole frame indefinitely. AGENTS.md ("Hot paths
multiply") asks for narrow work there. Scope the blocker to patches that touch
the copy or selection pane.

## Structural notes

A. **One composition pipeline.** `compose` and `compose_unavailable` already
disagree on overlays (finding 1), the mode bar rule and hit-map handling. A
single pipeline with the pane area as a layer (surface or placeholder) removes
the drift. Overlays, banners, notices and the mode bar would be applied once,
in one order.

B. **A request ledger instead of per-feature in-flight bookkeeping.** In-flight
state is spread across `pending_requests`, `pane_scroll_in_flight`,
`pane_scroll_queued`, `pane_scroll_targets`, `copy_operation_in_flight`,
`copy_operation_queue`, `copy_input_queue`,
`ClientWordSelection::pending_row` and
`PendingWorkspaceHighlight::request_id`. Each completion path must remember to
unwind its own maps, and the cancel path (finding 2) does not. Better: one
ledger whose entries own their completion and rollback, with cancellation being
a completion with `Err` whose whole outcome is routed. That makes "dropped
follow-up work" impossible by construction.

C. **Surface baseline versus presentable surface.** The shell keeps
`pane_surface` and `pending_pane_surface` with revision rules that live in
three places (`set_pane_surface`, `install_pane_surface`,
`apply_pane_surface_patch`), and the reader in `lib.rs` keeps its own baseline.
Model them explicitly: a server baseline that patches always apply to (mirroring
the reader), and a separately chosen presentable pair (snapshot plus surface at
the same revision). Finding 3 then cannot happen, and `Applied`/`Rejected`
regain their meaning.

D. **Config-derived labels for every drawn key hint.** The mode bars, footers
in the navigator and help overlays, and the copy-mode bar each hand-write key
names. Navigate (finding 5) is the one that is configurable today. Deriving
labels the way the Prefix bar does prevents the next drift.
