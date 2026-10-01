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

## CSHELL-016 - A parked copy mode makes every pane's output recompose the whole frame

Hot path. `fast_path_blocker` sends every pane patch through a full `compose`
while `copy_mode.is_some()` or `selection.is_some()`, whatever pane they belong
to. Copy mode stays parked on a pane that lost focus, so leaving a pane in copy
mode and moving on makes every byte of output in every other pane recompose the
whole frame indefinitely. Scope the blocker to patches that touch the copy or
selection pane.

## CSHELL-018 - Structural: a request ledger instead of per-feature in-flight bookkeeping

In-flight state is spread across `pending_requests`, `pane_scroll_in_flight`,
`pane_scroll_queued`, `pane_scroll_targets`, `copy_operation_in_flight`,
`copy_operation_queue`, `copy_input_queue`, `ClientWordSelection::pending_row`
and `PendingWorkspaceHighlight::request_id`. Cancellation now runs a rollback
owned by each pending request kind, but ordinary completion still unwinds each
feature's own maps and queues by hand (CSHELL-023 is a path that forgets). One
ledger whose entries own both completion and rollback would make dropped
follow-up work impossible by construction.

## CSHELL-023 - A result for a mismatched boot drops its pending entry without rollback

Lateral. `apply_endpoint_result` removes the pending request entry before it
returns early for a result whose boot no longer matches, and it does not run the
request kind's rollback on that path, so the in-flight state that request set
(copy operation, pane scroll, word-selection row) can stay set with nothing
behind it. Cancellation runs the rollback; this path should too.

## CSHELL-024 - The placeholder layer lets the banner and notice card overlap other chrome

Lateral, `shell/presentation/composition.rs`. With the single composition
pipeline the lifecycle banner also draws over the placeholder: with no sidebar
it shares row 0 with the left-aligned status message and can cover its tail on
a narrow terminal. With the active endpoint Online but no surface yet, the
notice card now draws at offset 0 where the old unavailable path used 1, so it
can overlap the status line or the sidebar header.

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

## CSHELL-022 - Clicking the displayed endpoint's machine row no longer cancels a pending switch

`handle_endpoint_machine_click` on the active endpoint's machine row now only
toggles collapse and pushes no `ActivateEndpoint`, which removed a redundant
re-activation. It also removed the one machine-row gesture that cancelled a
pending switch: with a switch to a remote machine in flight and Local still
displayed, clicking Local's machine row used to activate Local and so cancel
the switch. The test
`clicking_local_can_cancel_a_remote_switch_while_local_is_still_displayed` now
expects no action for the machine row; only the workspace row still cancels.
Decide whether a machine-row click on the displayed endpoint should activate it
when a switch away from it is pending.
