# Defects: client shell

Filed from the defect hunt over `crates/shepr-client/src/shell.rs` and
`crates/shepr-client/src/shell/`. The restore-notice findings from this hunt are
merged into CEND-001 in `notes/bugs-client-endpoints.md`.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## CSHELL-001 - Overlays are never drawn when there is no presentable surface, but still own input

Where: `ClientShellState::compose_unavailable`
(`shell/presentation/composition.rs`).

`compose` returns `compose_unavailable` whenever `snapshot` or `pane_surface` is
`None`. That path draws the sidebar, a status line, the mode bar and the notice
card, and never looks at `self.overlay`. Every overlay keeps routing keys
(`route_key_press` sends everything to `route_overlay_key` while
`self.overlay.is_some()`) and mouse events (`handle_mouse_with_accounting`).

Claims broken:

- `compose`'s own contract for an overlay that cannot be drawn: "a one-line hint
  says why the overlay is missing ... Overlay hit rects stay empty". On this path
  there is no hint; the overlay is simply skipped.
- AGENTS.md: "With machines configured, losing the local server does not end the
  client either: it keeps serving the remote machines". With Local active and
  down, the session navigator (prefix+g) is the keyboard route to a remote
  machine, and it opens invisible. Typing edits an unseen query, and Enter
  activates whatever row happens to be selected.
- The expanded sidebar still draws the `menu` launcher and sets
  `hits.global_launcher` on this path, so clicking it opens a `GlobalMenu` that
  is never painted; the next click anywhere closes it, because
  `hits.global_menu_rows` is empty.
- `compose` says the mode bar is drawn "only when no overlay is open". The
  unavailable path draws it with an overlay open.

Fix direction: one composition pipeline; see CSHELL-017.

## CSHELL-002 - Cancellation drops follow-up work but keeps the state that work set: copy mode and pane scrolling can wedge

Where: `cancel_endpoint_request_with_notice` (`shell/navigation/actions.rs`),
`complete_copy_operation` and `dispatch_queued_copy_input`
(`shell/input/copy_mode.rs`). Callers: `cancel_unsent_endpoint_request` (from
`dispatch_client_shell_actions` in `shell_runtime.rs` when the active endpoint
does not own presentation, and from `cancel_endpoint_commands`) and
`cancel_endpoint_request` (from `lib.rs` for completions on another generation
or a non-active endpoint, and from `mark_endpoint_disconnected`).

The comment in `cancel_endpoint_request_with_notice` says "A cancelled copy-mode
request does not continue its key queue (`continue_queue` is false on every
error), so nothing but a repaint can come out of it." That is false.
`complete_copy_operation` with `continue_queue == false` still calls
`dispatch_queued_copy_input` whenever `copy_mode_owns_input()` holds ("Failed
copy requests still release the buffered input"). Replayed keys can issue a new
copy motion or search (`dispatch_next_copy_operation` sets
`copy_operation_in_flight = true`, inserts a `pending_requests` entry, pushes a
`ClientShellAction::Endpoint`); exit copy mode (`y`, Enter, `q`), so
`exit_copy_mode` pushes `PaneSelectionRead` and `PaneScroll` and
`dispatch_pane_scroll_offset` inserts `pane_scroll_in_flight[pane]`; and after
that exit, route the keys behind it to the pane as `ClientShellPaneInput`.

The cancel wrapper keeps only `outcome.repaint` and logs the rest as dropped. On
the handoff path (`cancel_unsent_endpoint_request` while the endpoint is still
online) `push_endpoint_command_with_kind` succeeds, so the dropped work leaves:

- `copy_operation_in_flight == true` with a pending request that never completes
  or expires (it never reached the endpoint command queue, so nothing times it
  out). Every later copy-mode key is queued in `handle_key` until
  `MAX_COPY_INPUT_QUEUE`, then refused with "copy-mode input queue is full". Copy
  mode is wedged until an interrupt key exits it.
- `pane_scroll_in_flight[pane]` with no request behind it. `push_pane_scroll_offset`
  for that pane then only writes `pane_scroll_queued`, so copy-mode paging,
  scrollbar drags and selection autoscroll stop scrolling that pane until a
  reboot, a disconnect or an endpoint switch clears the maps.
- typed keystrokes silently lost.

The test-only `handle_endpoint_result` says it: "The caller must route the whole
outcome ... or the replayed keystrokes are lost." The cancel path breaks that
rule.

Fix direction: make cancellation return a full `ClientShellInput` and route it
through `finish_client_shell_input` like any result, or make a cancelled copy
operation discard the queue instead of replaying it. General fix: CSHELL-018.

## CSHELL-003 - A pane-split drag released during a projection gap drops the final ratio

Where: the `ClientChromeDrag::PaneSplit` arms in `handle_mouse_with_accounting`
(`shell/input/mouse.rs`).

During the drag, every admitted `LayoutSetSplitRatio` bumps the projection
revision. Until the matching surface arrives, `pane_split_target_is_current`
returns `None`; drag events in that window are ignored, and the release sends its
ratio only when the check returns `Some(true)`. The send throttle
(`MOUSE_DRAG_SEND_INTERVAL`) means the last motion before release is often not
the last ratio sent. A release in the gap, likely right after a send, leaves the
split at the last throttled ratio rather than where the pointer was released.
The release path exists to send that final ratio.

Fix: on release, send the final ratio when the topology signature still matches
the hit's, even during a revision gap, or defer it until the surface catches up.

## CSHELL-004 - The navigate-mode bar hardcodes keys that are configurable

Where: `render_mode_bar`, `ClientShellMode::Navigate` arm
(`shell/presentation/render.rs`). The bar prints `esc back`, `↑/↓ workspace` and
`tab pane`. Those actions (`navigate_back`, `navigate_workspace_up`/`down`,
`navigate_cycle_pane_next` in `keybinding_table.rs`) are rebindable; the Prefix
bar next to it (`prefix_rhs`) and the keybinding help overlay are
config-driven. Claim broken (AGENTS.md): "The client applies its own config to
everything it draws and interprets: keys, ...". After a rebind the bar
advertises dead keys. See CSHELL-020.

## CSHELL-005 - "Cycle pane" picks its next pane from two different orders

Where: `cycle_pane` (`shell/input/input.rs`, navigate mode) and the
`CyclePaneNext`/`CyclePanePrevious` arm of `endpoint_command_for_action`
(`shell/navigation/actions.rs`, prefix binding). Navigate mode cycles over
`pane_surface.panes` (the presented surface, in surface order, possibly a zoomed
or retained-future layout); the prefix binding cycles over `snapshot.panes`
filtered by focused workspace, in snapshot order. Both are labelled "cycle pane"
in the binding table, so the same action can land on different panes by mode,
and a zoomed workspace can cycle differently in each. Pick one source (the
snapshot) for both.

## CSHELL-006 - Copy-mode replay drops queued keys when the reply arrives in Prefix mode

Where: `complete_copy_operation` (`shell/input/copy_mode.rs`). The prefix key is
a copy-mode interrupt key, so it is not queued; it switches `mode` to Prefix. If
the in-flight reply arrives before the prefix sequence finishes,
`copy_mode_owns_input()` is false only because `mode != Copy`, and the `else`
branch clears `copy_input_queue` with the comment "These keys belonged to the
copy pane. Do not send them into a pane that gained focus while the request was
outstanding". No pane gained focus; the keys are silently lost. Interrupt keys
(Esc, prefix) also run ahead of keys typed before them that are still queued:
`w v Esc` runs Esc first, then `v` starts a selection the user meant to cancel.

## CSHELL-007 - The "Action interrupted" notice fires for internal reads

Where: `apply_endpoint_result`, the `Cancelled` arm. `mark_endpoint_disconnected`
cancels every pending request with `show_cancelled_notice = true`. Pane scroll,
word-selection row reads, copy motions and copy searches are reads with no
server-side effect. The text "This server action was interrupted. Check its
state before retrying." describes a user action with an unknown outcome, which
fits close or rename, not these.

## CSHELL-008 - Two width models in one sidebar row

`sidebar_tokens.rs` budgets and truncates with `unicode_width`;
`agent_sidebar.rs::put_text` and `display_width` use
`shepr_vt::unicode_display_units` / `unicode_text_width` (its tests assert
`display_width("\u{263a}\u{fe0f}") == 1` "matches terminal");
`render.rs::put_text` uses ratatui `set_stringn` (`unicode_width`); and
`wire_cells.rs` says "Both use the one width rule output uses
(`shepr_termio::blit::symbol_width`)" while its glyph repair runs over chrome
ratatui laid out under a different rule. Agent titles routinely contain emoji and
VS16 sequences; a token budgeted at one width and drawn or repaired at another
truncates wrongly or leaves a blank continuation cell. Use one width function
everywhere chrome text is measured.

## CSHELL-009 - Index- and path-addressed layout commands race other clients

Where: `workspace_move_command` (`insert_index` from the client's snapshot
order) and the split drag (`LayoutSetSplitRatio { path }`). Several TUI clients
can share a server. A workspace created, closed or moved by another client
between this client's snapshot and its command makes `insert_index` name a
different slot, and the server applies it. The `topology_signature` check guards
only against this client's own stale surface. Addressing by identity ("before
workspace X", a split named by its child pane ids) would close it.

## CSHELL-010 - Copy-mode cursor and selection anchor are not clamped when history evicts their rows

Where: `install_pane_surface` (`shell/state.rs`). When `history_origin`
advances, `prune_evicted_search_matches` drops evicted search matches, but
`copy_mode.cursor` and `copy_mode.selection`'s anchor keep rows below the origin.
The cursor is then drawn nowhere (`client_copy_cursor_cell` returns `None`), and
the next motion sends an origin row the server no longer has. The module comment
("output that evicts history does not move the line the cursor ... names")
assumes the row still exists. Clamp the cursor to `retained_row` as
`move_copy_cursor` already does.

## CSHELL-011 - `receive_endpoint_error` hardcodes the paste notice

`receive_endpoint_error(message)` hardcodes code `paste_rejected` and title
"Paste rejected". The generic name hides that it serves one purpose; its only
caller is `push_focused_paste`.

## CSHELL-012 - The collapsed sidebar labels machines by endpoint index

Labels are `L`, then `2`, `3`, ..., so the first configured machine reads `2`.

## CSHELL-013 - `compose` stamps a frame time for frames it did not produce

`compose` stamps `last_composed_at` and clears `selection_repaint_deadline` even
when it returns `None` (pending surface, generation or revision mismatch).
`request_selection_drag_repaint` then throttles against a frame that was never
presented.

## CSHELL-014 - The global menu jumps when the sidebar collapses under it

`render_global_menu` anchors to `hits.global_launcher`. If the sidebar collapses
while the menu is open (keybind), the launcher rect is empty and the menu jumps
to the top-left corner.

## CSHELL-015 - Clicking the active endpoint's row also re-activates it

`handle_endpoint_machine_click` on the active endpoint's row both toggles its
collapse and pushes `ActivateEndpoint` for the endpoint that is already active.

## CSHELL-016 - A parked copy mode makes every pane's output recompose the whole frame

Hot path. `fast_path_blocker` sends every pane patch through a full `compose`
while `copy_mode.is_some()` or `selection.is_some()`, whatever pane they belong
to. Copy mode stays parked on a pane that lost focus, so leaving a pane in copy
mode and moving on makes every byte of output in every other pane recompose the
whole frame indefinitely. Scope the blocker to patches that touch the copy or
selection pane.

## CSHELL-017 - Structural: one composition pipeline

`compose` and `compose_unavailable` already disagree on overlays (CSHELL-001),
the mode bar rule and hit-map handling. A single pipeline with the pane area as a
layer (surface or placeholder) would apply overlays, banners, notices and the
mode bar once, in one order.

## CSHELL-018 - Structural: a request ledger instead of per-feature in-flight bookkeeping

In-flight state is spread across `pending_requests`, `pane_scroll_in_flight`,
`pane_scroll_queued`, `pane_scroll_targets`, `copy_operation_in_flight`,
`copy_operation_queue`, `copy_input_queue`, `ClientWordSelection::pending_row`
and `PendingWorkspaceHighlight::request_id`. Each completion path must unwind its
own maps, and the cancel path (CSHELL-002) does not. One ledger whose entries own
their completion and rollback, with cancellation a completion with `Err` whose
whole outcome is routed, would make dropped follow-up work impossible by
construction.

## CSHELL-019 - Structural: surface baseline versus presentable surface

The shell keeps `pane_surface` and `pending_pane_surface` with revision rules in
three places (`set_pane_surface`, `install_pane_surface`,
`apply_pane_surface_patch`), and the reader in `lib.rs` keeps its own baseline.
Model them explicitly: a server baseline that patches always apply to (mirroring
the reader), and a separately chosen presentable pair (snapshot plus surface at
the same revision). The patch-rejection finding filed in
`notes/bugs-rejected-candidates.md` then cannot happen, and `Applied`/`Rejected`
regain their meaning.

## CSHELL-020 - Structural: config-derived labels for every drawn key hint

The mode bars, the navigator and help footers, and the copy-mode bar each
hand-write key names. Deriving labels as the Prefix bar does prevents the next
drift after CSHELL-004.
