# Defect hunt: client presentation shell

Scope: `crates/shepr-client/src/shell/`, `shell.rs`, `shell_runtime.rs`. The findings are
ordered roughly by impact. Each one names the claim it breaks. Where I followed a value
into `lib.rs` or the server, I say so.

## 1. An in-flight copy-mode request captures every key; a failed one throws them away

`input/input.rs` `handle_key` starts with:

```rust
if self.copy_operation_in_flight {
    self.copy_input_queue.push_back(key);
    return;
}
```

While a `PaneCopyMotion` or `PaneCopySearch` request is outstanding, every key event goes
into `copy_input_queue`: press, repeat and release, in any mode, overlay or not. That
includes the prefix, Esc, `q` and the Detach binding. The queue is replayed only when the
response is `Ok` and matches (`complete_copy_operation(.., continue_queue = true, ..)`).
On any error, including `Timeout` (`ENDPOINT_COMMAND_TIMEOUT` = 60 s) and `Cancelled`
(disconnect, lane retirement at a handoff, a dispatch refused because the endpoint does not
own the presentation), `complete_copy_operation` runs `copy_input_queue.clear()`.
`release_input_leases` (outer focus lost) clears it as well.

Consequences:
- A slow or stuck server, or a copy request queued behind a slow command in the same
  serialized endpoint lane, freezes the TUI keyboard for up to 60 s. Detach does not work
  during that time, which breaks the Detach keybinding.
- Mouse input is not queued. The user can click another pane, and the snapshot then drops
  the mode to `Terminal` (`apply_active_snapshot` does this when the copy pane loses focus).
  But `copy_operation_in_flight` is still set, so text typed into the new pane is queued
  behind an unrelated copy request. On failure it is discarded with no notice: user
  keystrokes bound for a terminal pane are lost.
- The queue has no bound. On replay, `dispatch_queued_copy_input` runs every key through
  `handle_key` into one `ClientShellInput`, and `push_target_event` (`input/events.rs`)
  appends consecutive same-pane events to a single `ClientShellPaneInput` with no size or
  count cap. The server closes the whole client connection when one message expands to
  more than `MAX_INPUT_EVENT_BATCH` (4096) events
  (`shepr-server/src/server/client_transport.rs`, "oversized targeted pane input batch,
  closing"). A long stall followed by held keys or key repeat can therefore end in a
  disconnect.

Direction: gate the queue on copy mode actually owning the key (copy mode is active and
the copy pane is focused). Let prefix and Detach bindings pass. Bound the queue. When a copy
request fails, replay the queued non-copy keys instead of dropping them. Cap
`push_target_event` batches at the server's limits the way `push_focused_paste` already
does for text.

## 2. Every server `ClientShellError` is shown as "Paste rejected"

`lib.rs` handles `ServerMessage::ClientShellError { kind }` by calling
`shell.receive_endpoint_error(kind.to_string())`. `navigation/actions.rs` hard-codes that
notice:

```rust
pub(crate) fn receive_endpoint_error(&mut self, message: String) -> bool {
    self.push_endpoint_notice(ClientEndpointNoticeKind::Rejected, "paste_rejected", "Paste rejected", message)
}
```

The server sends three `NoticeKind`s through this message: `PasteRejected`,
`PaneInputDropped { pane_id, events }` (`shepr-server/src/server/headless.rs`) and
`OversizedSurface { claimed, max }` (`headless/render.rs`). Dropped keystrokes and an
oversized surface both appear under the title "Paste rejected" with the code
`paste_rejected`. The notice's title does not match what happened. The client should take
the typed `NoticeKind` and choose the title from it, not flatten it to a string first.

## 3. An "Unavailable" notice shows once per boot of the active endpoint, then never again

`push_endpoint_notice` (actions.rs) deduplicates every non-`Rejected` notice with
`endpoint_notice_seen.insert(key)`. The key is `(active snapshot boot_id, kind, code)`, and
the set is cleared only by `reset_endpoint_projection` (a boot or endpoint change). The
only other removal is a Timeout key, cleared after a later success of the same method. The
card expiring or being dismissed does not re-arm it. Affected paths:

- `receive_endpoint_unavailable(message)` uses the message itself as the code. The second
  time the same activation fails with the same text, the user sees nothing: they click the
  machine and nothing happens. Examples are `"{label}: {error}"` from a preflight failure,
  `"buildbox did not produce a coherent surface in time"` from rollback, `"{label} is not
  ready"` and `"Local is reconnecting; ..."`. `begin_endpoint_activation` (shell_runtime.rs)
  reports every preflight failure through this call, so that reporting is best-effort
  once.
- `ClientShellEndpointError::Cancelled` maps to kind `Unavailable`, code `"cancelled"`.
  Only the first interrupted action per boot is reported. The notice's own text says
  "Check its state before retrying", so later interruptions, such as a workspace close lost
  in flight, are exactly the ones the user needs to hear about.
- The key's `boot_id` is documented as "the server boot the notice is about", but it is
  always the active snapshot's boot. A failed switch to another machine is recorded against
  the boot of the machine the user is switching away from.

Related message problem: `dispatch_client_shell_actions` (shell_runtime.rs) cancels an
endpoint request that never left the client, because its endpoint is not active or does
not own the presentation (for example during a handoff). It still gets the same "This
server action was interrupted. Check its state before retrying." text, which suggests the
action may have been partly applied. `Cancelled` does not tell "never sent" apart from
"sent, outcome unknown".

## 4. The active remote endpoint cannot be picked explicitly; only Local can

The runtime documents that the user can re-prove the active endpoint:
- `begin_endpoint_activation`: "With no proven owner (`Unavailable`) a pick of the active
  endpoint re-proves ownership through a handoff."
- `automatic_activation`: a failed handoff "is not retried ... until a new connection or an
  explicit pick".

The shell never emits that pick for a remote endpoint that is already active.
`activate_endpoint` and `focus_or_activate` (`navigation/endpoint_navigation.rs`) push
`ActivateEndpoint` only when `endpoint_id != active`, or when the endpoint is Local
(`endpoint_id.is_local() && (multi_endpoint_active() || !online)`). For the active remote:
- Clicking its machine row in `handle_endpoint_machine_click` only toggles collapse. Local
  also activates on the same click.
- Picking it in the navigator (`Machine` target) closes the overlay and does nothing.
- Picking one of its workspaces or panes produces a plain `WorkspaceFocus` or `PaneFocus`
  command. `dispatch_client_shell_actions` cancels it because the presentation is not
  owned, which in turn triggers the misleading notice from finding 3.

So once the automatic re-proof on a connection has failed, a remote active endpoint left
`Unavailable` cannot be recovered by the promised explicit pick. The user must pick
another machine or wait for a reconnect. The Local special case is justified in the
comments as "Local can still be displayed while a remote activation is pending. Route
explicit selections through the runtime so they can cancel that handoff." The same
reasoning applies to a displayed remote while a handoff to a third machine is pending.
Clicking the displayed remote does not cancel that handoff; it produces a cancelled
command. The fix is to route the pick through `ActivateEndpoint` whenever the presentation
is not owned, whichever endpoint it is, instead of special-casing Local. The shell cannot
see ownership today, so either the runtime should decide or the shell should be told.

## 5. The navigator highlights one row but Enter acts on another (or on none)

`overlays/overlays.rs` `render_navigator_overlay` highlights
`navigator_selected_index(&rows, n).unwrap_or(0)`. `move_navigator_selection` also falls
back to 0. `accept_navigator_selection` uses `selected_navigator_target`, which returns
`None` when `navigator.selected` is `Some(target)` but that target is no longer in `rows`.

`selected` is reset on query or filter changes, but not when a snapshot removes its pane,
workspace or machine (the pane closes, or an endpoint's snapshot changes). After that,
row 0 is drawn as selected, and Enter or a click on the "selected" row does nothing.
Enter silently fails on the highlighted row.

The same overlay's footer advertises `a/b/w/i/d filter`, but `route_overlay_key` binds
only `a`, `b`, `w` and `i`. Plain `d` does nothing; only Ctrl+D is bound, and it moves the
selection by 8. The hint names a key that does not exist.

## 6. Two sort keys for the "same" agent list

In `Priority` mode:
- The sidebar rows come from `endpoint_agents::agent_rows`, which calls
  `aggregate_navigation::aggregate_agent_rows`. That orders by
  `(stale, status, Reverse(client recency))`, where recency is a client-side counter
  assigned in `cache_endpoint_snapshot_at_generation`.
- In single-endpoint mode, `FocusAgent(n)`, `NextAgent` and `PreviousAgent` resolve
  through `agent_sidebar::ordered_agent_pane_ids`, which orders by
  `(status, Reverse(state_change_seq))` (actions.rs `endpoint_command_for_action`). The
  collapsed single-endpoint sidebar prints `index + 1` beside each row
  (`endpoint_agents::render_collapsed`).

The two orders agree only while recency and `state_change_seq` stay monotone together. On
a reconnect to a restarted server, `state_change_seq` restarts. An agent whose new seq
happens to equal its old one is not seen as "changed" and keeps its old, lower recency,
while its seq may now be the highest. The numbers printed in the sidebar then differ from
what `FocusAgent(n)` focuses.

`indexed_navigation_target_exists` (input.rs) validates `FocusAgent` against a third
list, `online_agent_targets`, which drops stale endpoints. In multi-endpoint `Spaces` mode
a stale endpoint's rows sit in the middle of the displayed list but are not counted, so
indices shift. `ordered_agent_pane_ids` also counts agents whose workspace is missing from
the snapshot, which `AgentRowIndex::agent_row` drops from the display.

Direction: build one ordered agent list per compose, and use it for rendering, the hit
map, the indices and relative navigation.

## 7. `reveal_workspace` ignores the endpoint and uses the wrong scroll units

`state.rs` `reveal_workspace` returns early if any `hits.workspaces` entry has the same
`workspace_id`, whatever its `endpoint_id`. Workspace IDs are per server, so a same-ID
workspace on another machine suppresses the reveal. If it does not return early, it sets
`workspace_scroll = position among the active snapshot's entries`. In the multi-endpoint
expanded sidebar, the scroll index counts `Row::Endpoint` header rows and every other
endpoint's workspaces (`endpoint_sidebar::render_expanded`). In the collapsed sidebar it
counts rows the same way.

`endpoint_command_for_action` calls it for `SwitchWorkspace(n)` in multi-endpoint mode too
(`handle_endpoint_navigation` does not take `SwitchWorkspace`), so the list first jumps to
the wrong row. The `reveal_focused_workspace` pass on the next snapshot then corrects it.
The effect is a visible jump plus an early return that can skip the correction.

## 8. The help overlay's scroll range is computed with a different wrap than the renderer uses

`render_help_overlay` computes `total_rows` as `Σ ceil(chars / width)` from
`chars().count()`, and the group header uses `group.len()`, which is bytes. It then renders
with `Paragraph::wrap(Wrap { trim: false })`, which wraps at word boundaries and can
produce more rows than character division predicts. `help_max_scroll` is too small in that
case, and the last entries cannot be scrolled into view in a narrow terminal. The overlay's
own "scroll" contract is not met for its tail.

## 9. The machine diagnostic keeps newlines, but the card shows one line

`machine_diagnostics.rs` keeps up to `MAX_MACHINE_DIAGNOSTIC_CHARS` (4096) characters and
preserves `'\n'`, so the SSH diagnostic stays readable. `endpoint_notices.rs`
`render_notification_card` draws the body as a single `Line` in a card at most 4 rows
tall, with its width computed from the whole string. Newlines are dropped or joined, and
all but the first terminal-width's worth of text is cut. The diagnostic the badge click
exists to show (for example the "restart shepr to authenticate" details) is mostly
invisible.

## 10. Smaller issues and smells

- **Endpoint error cleared by non-user input.** `begin_input_batch` clears `endpoint_error`
  on any host input batch, including `Moved` mouse reports (any-motion tracking is on,
  since menus use hover), focus in and out, and host colour replies. `set_endpoint_error`
  promises a lifetime (`ENDPOINT_ERROR_TIMEOUT`), but a mouse twitch or a terminal reply
  ends it at once. That makes errors such as "failed to replace client shell state" (from
  `persist_chrome_preferences`) easy to miss.
- **The unavailable view ignores the collapsed sidebar.** `compose_unavailable` always calls
  `endpoint_sidebar::render_expanded`, even when the sidebar is collapsed in Compact mode
  (`layout.sidebar` is 4 columns wide). The expanded machine list is squeezed into 4
  columns, with hit rects to match. With a single endpoint and the sidebar collapsed, it
  also prints "Local: online. Select a connected machine." when there is no other machine
  to select.
- **A scrollbar click leaves copy mode without ending it.** `handle_mouse`'s pane-scrollbar
  branch sets `self.mode = ClientShellMode::Terminal` unconditionally, from Copy, Prefix,
  Navigate or Resize, and leaves `copy_mode` in place. The next snapshot flips the mode
  back to Copy (`apply_active_snapshot`), so keys typed in between go to the pane as
  terminal input while the copy highlight state persists.
- **Workspace drag does nothing when collapsed.** `workspace_drop_target_at` bounds rows by
  `hits.new_workspace.y`. That hit rect is set only by the expanded sidebar, so in the
  collapsed sidebar it is `Rect::default()` (y = 0) and every drop target is rejected. The
  drag silently does nothing, although the collapsed sidebar registers workspace hits and
  presses.
- **Dead resize code.** `install_client_shell_snapshot` (shell_runtime.rs) compares
  `surface_size` before and after installing a snapshot and sends a resize if it changed.
  `surface_size` depends only on sidebar state, which snapshot installation does not
  change (`apply_endpoint_config` swaps only keybinds), so this branch cannot fire. It
  suggests a coupling that no longer exists.
- **Endpoint config is applied only as a keymap.** `apply_endpoint_config`
  (`presentation/config.rs`) takes only `live_keybinds()` from the endpoint's config.
  Palette, sidebar tokens, `confirm_close`, `copy_on_select`, mouse settings and so on stay
  from the client's launch config. `ClientShellEndpoint::config`'s doc says a reconnect "to
  a server launched with another config shows that config", and AGENTS.md says the client
  "rebuilds its runtime values from it". What "shows" covers is not stated. If more than
  the keymap is meant, this is a gap; if only the keymap, the doc wording should say so.
- **Timer wakes every 100 ms.** `timer_delay` only knows the autoscroll and repaint
  deadlines. The notice, endpoint error, workspace highlight and selection-clear deadlines
  rely on `MAX_CLIENT_TIMER_DELAY` (100 ms) polling, so the client loop wakes 10 times a
  second forever while idle. Folding those deadlines into `timer_delay` would allow a long
  idle sleep.
- **Keyboard mode not re-synced on snapshot installs.** The keybinding-change mode reset in
  `apply_active_snapshot` (Prefix, Navigate or Resize back to Terminal) is not followed by
  `sync_client_shell_keyboard_report_all`. Snapshot installs do not go through
  `finish_client_shell_input`, so the host stays in report-all mode until the next input
  arrives. It is harmless today because the next batch re-syncs, but the invariant "host
  mode follows shell mode" is only restored lazily.
