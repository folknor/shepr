# Defects: client shell

Filed from the defect hunt over `crates/shepr-client/src/shell.rs` and
`crates/shepr-client/src/shell/`.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

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

## CSHELL-018 - Structural: a request ledger instead of per-feature in-flight bookkeeping

In-flight state is spread across `pending_requests`, `pane_scroll_in_flight`,
`pane_scroll_queued`, `pane_scroll_targets`, `copy_operation_in_flight`,
`copy_operation_queue`, `copy_input_queue`, `ClientWordSelection::pending_row`
and `PendingWorkspaceHighlight::request_id`. Cancellation now runs a rollback
owned by each pending request kind, but ordinary completion still unwinds each
feature's own maps and queues by hand (CSHELL-023 is a path that forgets). One
ledger whose entries own both completion and rollback would make dropped
follow-up work impossible by construction. A related leak: leaving copy mode
behind a full queue (`abandon_copy_operation`) makes the abandoned request's
completion and rollback inert but leaves its `pending_requests` entry, so a
request that never answers holds one entry for the connection's life.

## CSHELL-027 - A cursor-only change on the copy pane may take the patch fast path

Lateral, `shell/presentation/surface_patch.rs`. The copy-mode and selection
blockers on the patch fast path key on `patch.panes`, the panes whose metadata
changed. A cursor-only change on the copy pane (DECSCUSR, or a cursor move with
no content revision change) would take the fast path and apply the terminal
cursor while copy mode is drawn there. Check whether a cursor change always
moves `content_revision`; if not, the blocker should also fire when
`patch.cursor` differs and the focused pane is the copy pane.

## CSHELL-028 - The `SessionRestoreIncomplete` notice arm is reachable only from tests

Lateral. The restore notice now travels in the client shell snapshot, so no
server path sends `NoticeKind::SessionRestoreIncomplete` as a
`ClientShellError` any more, yet `receive_server_notice` still formats it.
Remove the arm, and the variant if nothing else uses it.

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
