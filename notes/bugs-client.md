# Defects: client, TUI shell and terminal input

Filed from the defect hunt over `crates/shepr-client` (endpoint, transport,
handshake, loop, input, and the `shell/` presentation) and `crates/shepr-termio`.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## CLIENT-003 - Termio structural notes

Scope: termio-root (filed by the hunter as structural notes, not defects in
themselves).

- **`blit.rs` trusts a `FrameData` invariant it cannot see.** `write_all_cells`,
  `write_changed_cells` and `blit_patch_to` index `frame.cells[idx]` directly and
  panic if `cells.len() != width * height`. The client-side composers check this
  before building frames (`wire_cells.rs`, `compose_pane_surface.rs`), so nothing
  is known to break, but the invariant lives in several callers instead of the
  type. A validated frame type (cells length proven at construction) removes the
  panics and the duplicated checks.
- **Two "text char of a key" helpers disagree.** `copy_mode_command_char` maps an
  unshifted char with Shift through `shifted_ascii_char` (`/` + Shift to `?`);
  `keybind_help_text_char` returns the unshifted char. For the same key (no
  alternate codepoint), copy mode and the help filter see different characters.
- **Misleading module doc.** `crates/shepr-termio/src/host_term/title.rs` opens
  with a module doc about clipboard bytes; the module is named for titles and
  holds both.

## CLIENT-006 - Endpoint health measures client-loop latency, not transport liveness

Scope: client-endpoint.

Where: `src/endpoint/health.rs`, `registry.rs` (`received`, `tick_health`), and
`lib.rs` `ClientLoop::run` (the `biased` select with the timer first).

**Claim broken.** `crosses_ssh` says machine connections get "heartbeats and a
silence deadline", and `HEARTBEAT_TIMEOUT` is documented as tolerating "missed
scheduling and transport jitter before marking the endpoint offline". In
practice the endpoint is marked offline for the client's own stall.

`received(now)` is stamped when the loop processes a message, not when it
arrives. When the loop wakes after a stall, the overdue timer wins the `biased`
select, so `handle_timer` calls `tick_health` first, which sees an outstanding
`ping_sent_at` older than `HEARTBEAT_TIMEOUT` (or a missing first snapshot past
`connected_at + HEARTBEAT_TIMEOUT`) and returns `Expired`, while the `HealthPong`
and snapshot frames sit unread in `event_rx`.

What stalls the loop: everything it does synchronously. `HostTerminalWriter`
writes stdout blocking, so a host terminal that stops reading (XOFF or scroll
lock, a slow forwarded tty, a suspended multiplexer above shepr) blocks the loop.
`forward_clipboard` and `ClipboardWrite` run the native clipboard helper inline,
up to `CLIPBOARD_HELPER_TIMEOUT`. Stalls longer than about 10 s disconnect every
machine at once, each with "endpoint health check timed out".

**Fix direction.** Stamp liveness on the reader thread (an atomic per connection,
updated as each frame arrives) and have `tick_health` read it; or drain
`event_rx` before judging health on a timer wake.

A fixer confirmed the defect and found both routes cross files: the reader stamp
needs a shared control added through `endpoint/writer.rs` and `transport.rs`, and
draining first needs the loop in `lib.rs`. Give one fixer `lib.rs`,
`transport.rs`, `endpoint/writer.rs`, `endpoint/health.rs` and
`endpoint/registry.rs` together.

## CLIENT-010 - A failed first Local handshake with machines configured throws away its diagnostic

Scope: client-endpoint.

Where: `src/lib.rs`, `run_launched_client` (the `initial` match) and
`run_client_loop` (`local_unavailable` sets `Connecting`).

**Claim broken.** AGENTS.md: "A config that fails to decode fails that handshake
and shows as that endpoint's Attention diagnostic"; likewise a refusal or
malformed welcome on the launch attempt.

The error goes to `warn!` only. Local shows as `Connecting`, and
`add_local(.., None, ..)` schedules a fresh attempt right away; the Attention
shows only if the second attempt fails the same way. If the server has meanwhile
stopped (it answered the first hello with `ServerShutdown`, or crashed decoding),
the user sees "Local is unavailable; start its server to reconnect" and never the
first failure. It also costs a redundant connection.

**Fix direction.** Route the launch attempt's error through
`handshake_error(.., Some(mismatch_guidance))` and seed the Local status and
diagnostic with it (Attention when `needs_attention()`), as a supervisor `Status`
event would.

## CLIENT-012 - Smaller client endpoint defects, smells and stale docs

Scope: client-endpoint.

- **`ClientMessageSink` is a dead abstraction with an infallible `io::Result`**
  (`transport.rs`). Nothing uses the `LocalStream` impl or
  `write_to_local_server`. The registry impl always returns `Ok(())`, so both
  `write_to_server(..).map_err(ClientError::ConnectionLost)?` calls in
  `finish_client_shell_input` are dead error paths that look like they could end
  the client. Delete the trait and call `endpoints.send`.
- **`ClientError::ConnectionFailed` wraps non-connect failures with a misleading
  hint.** Its Display adds "Is the shepr server running? Running `shepr` starts
  one." It also wraps `set_nonblocking` and recv-timeout failures in
  `do_handshake` and thread-spawn failures in `spawn_endpoint_reader` /
  `start_endpoint_transport`. The launch path (`lib.rs`) flattens it to
  `io::Error::other(..to_string())`, losing the kind.
- **Stale "saved endpoint" wording.** `ClientError::EndpointSetup`'s doc and
  Display say "saved SSH endpoints"; machines are `[[machines]]` config entries,
  and nothing is saved.
- **`lib.rs` module doc**: "Forwards OSC 52 clipboard writes from server to its
  own stdout". `forward_clipboard` prefers the host's native clipboard tool and
  writes OSC 52 only as a fallback, or when `prefers_osc52_clipboard`.
- **`CLIENT_EVENT_QUEUE_CAPACITY` doc** says the queue is shared by "the resize
  and server-reader threads". The stdin thread uses it too, and that sharing is
  what triggers CLIENT-004.
- **Handshake timeout chosen by `surface_active`.** `do_handshake` picks
  `LOCAL_HANDSHAKE_READ_TIMEOUT` vs `REMOTE_HANDSHAKE_READ_TIMEOUT` from the
  `surface_active` flag. A supervised Local reconnect passes `false` and gets the
  60 s "fresh SSH connection" timeout, capped only by `ATTEMPT_BUDGET`, while the
  `limits.rs` docs describe the remote timeout as the machines'. Pass the link
  kind instead of reusing the hello flag.
- **Double Detach.** On Ctrl+C (`ClientLoop::run`), on `outcome.detach` and on
  `TerminalUnavailable`, the active endpoint gets `Detach` and then another from
  `EndpointRegistry::drop` (the comments admit it). The server ignores it; one
  owner (the registry's Drop) would do.
- **Expired commands for a non-active endpoint leave the shell's pending request
  uncancelled** (`handle_timer`: `if !shell.endpoint_is_active(..) { continue;
  }`), while the response path cancels in the same situation
  (`cancel_endpoint_request`). Lane retirement at source-off makes this hard to
  reach, but the paths disagree.
- **`start_endpoint_transport(.., lifetime: impl Send + 'static, ..)`** is only
  ever passed `()`.
- **Launch-time Local failure after connect ends the client even with
  machines.** In `run_client_loop`, `start_endpoint_transport(..)?` (`try_clone`
  or thread spawn) returns an error even when
  `local_failure_policy.reconnects_local()`, breaking "With machines configured,
  losing the local server does not end the client" for this narrow resource
  failure.

## CLIENT-014 - An in-flight copy-mode request captures every key, and a failed one throws them away

Scope: client-shell. Related: WIRE-002 (the same unbounded `push_target_event`).

`input/input.rs` `handle_key` starts with:

```rust
if self.copy_operation_in_flight {
    self.copy_input_queue.push_back(key);
    return;
}
```

While a `PaneCopyMotion` or `PaneCopySearch` request is outstanding, every key
event goes into `copy_input_queue` (press, repeat and release, any mode, overlay
or not), including the prefix, Esc, `q` and the Detach binding. The queue is
replayed only when the response is `Ok` and matches
(`complete_copy_operation(.., continue_queue = true, ..)`). On any error,
including `Timeout` (`ENDPOINT_COMMAND_TIMEOUT` = 60 s) and `Cancelled`
(disconnect, lane retirement at a handoff, a dispatch refused because the
endpoint does not own the presentation), `complete_copy_operation` runs
`copy_input_queue.clear()`; `release_input_leases` (outer focus lost) clears it
too.

- A slow or stuck server, or a copy request queued behind a slow command in the
  same serialized lane, freezes the TUI keyboard for up to 60 s, Detach included,
  breaking the Detach keybinding.
- Mouse input is not queued. The user can click another pane; the snapshot drops
  the mode to `Terminal` (`apply_active_snapshot` does this when the copy pane
  loses focus), but `copy_operation_in_flight` stays set, so text typed into the
  new pane is queued behind an unrelated copy request and, on failure, discarded
  with no notice.
- The queue has no bound in memory. (The replayed input no longer risks a
  disconnect: the client batcher now splits every message at the shared payload
  and event limits.)

**Direction.** Gate the queue on copy mode actually owning the key (copy mode
active and the copy pane focused); let prefix and Detach bindings pass; bound the
queue; on failure replay queued non-copy keys instead of dropping them.

## CLIENT-019 - Two sort keys for the same agent list

Scope: client-shell.

In `Priority` mode:

- Sidebar rows come from `endpoint_agents::agent_rows` via
  `aggregate_navigation::aggregate_agent_rows`, ordered by
  `(stale, status, Reverse(client recency))`, where recency is a client-side
  counter assigned in `cache_endpoint_snapshot_at_generation`.
- In single-endpoint mode, `FocusAgent(n)`, `NextAgent` and `PreviousAgent`
  resolve through `agent_sidebar::ordered_agent_pane_ids`, ordered by
  `(status, Reverse(state_change_seq))` (actions.rs `endpoint_command_for_action`).
  The collapsed single-endpoint sidebar prints `index + 1` beside each row
  (`endpoint_agents::render_collapsed`).

They agree only while recency and `state_change_seq` stay monotone together. On a
reconnect to a restarted server, `state_change_seq` restarts; an agent whose new
seq happens to equal its old one is not seen as changed and keeps its old, lower
recency while its seq may now be the highest, so the printed numbers differ from
what `FocusAgent(n)` focuses.

`indexed_navigation_target_exists` (input.rs) validates `FocusAgent` against a
third list, `online_agent_targets`, which drops stale endpoints; in multi-endpoint
`Spaces` mode a stale endpoint's rows sit mid-list but are not counted, so indices
shift. `ordered_agent_pane_ids` also counts agents whose workspace is missing from
the snapshot, which `AgentRowIndex::agent_row` drops from the display.

**Direction.** One ordered agent list per compose, used for rendering, the hit
map, the indices and relative navigation. Related: AGENT-027 (the seq bumps on
Idle/Unknown flips).

## CLIENT-020 - `reveal_workspace` ignores the endpoint and uses the wrong scroll units

Scope: client-shell.

`state.rs` `reveal_workspace` returns early if any `hits.workspaces` entry has the
same `workspace_id`, whatever its `endpoint_id`; workspace IDs are per server, so a
same-ID workspace on another machine suppresses the reveal. Otherwise it sets
`workspace_scroll = position among the active snapshot's entries`, while in the
multi-endpoint expanded sidebar the scroll index counts `Row::Endpoint` header rows
and every other endpoint's workspaces (`endpoint_sidebar::render_expanded`), and
the collapsed sidebar counts rows the same way. `endpoint_command_for_action` calls
it for `SwitchWorkspace(n)` in multi-endpoint mode too (`handle_endpoint_navigation`
does not take `SwitchWorkspace`), so the list first jumps to the wrong row; the
`reveal_focused_workspace` pass on the next snapshot corrects it. A visible jump,
plus an early return that can skip the correction.

## CLIENT-023 - Smaller client shell issues

Scope: client-shell. (Its note on the endpoint config being applied only as a
keymap is filed under WIRE-011, and its dead-resize note under CLIENT-013.)

- **Endpoint error cleared by non-user input.** `begin_input_batch` clears
  `endpoint_error` on any host input batch, including `Moved` mouse reports
  (any-motion tracking is on, since menus use hover), focus in and out, and host
  colour replies. `set_endpoint_error` promises a lifetime
  (`ENDPOINT_ERROR_TIMEOUT`), but a mouse twitch or terminal reply ends it at
  once, so errors such as "failed to replace client shell state" (from
  `persist_chrome_preferences`) are easy to miss.
- **The unavailable view ignores the collapsed sidebar.** `compose_unavailable`
  always calls `endpoint_sidebar::render_expanded`, even when the sidebar is
  collapsed in Compact mode (`layout.sidebar` is 4 columns wide), squeezing the
  expanded machine list into 4 columns with hit rects to match. With a single
  endpoint and the sidebar collapsed it prints "Local: online. Select a connected
  machine." when there is no other machine.
- **A scrollbar click leaves copy mode without ending it.** `handle_mouse`'s
  pane-scrollbar branch sets `self.mode = ClientShellMode::Terminal`
  unconditionally (from Copy, Prefix, Navigate or Resize) and leaves `copy_mode` in
  place. The next snapshot flips the mode back to Copy (`apply_active_snapshot`),
  so keys typed in between go to the pane as terminal input while the copy
  highlight persists.
- **Workspace drag does nothing when collapsed.** `workspace_drop_target_at`
  bounds rows by `hits.new_workspace.y`, which only the expanded sidebar sets; in
  the collapsed sidebar it is `Rect::default()` (y = 0), so every drop target is
  rejected, although the collapsed sidebar registers workspace hits and presses.
- **Timer wakes every 100 ms.** `timer_delay` only knows the autoscroll and
  repaint deadlines; the notice, endpoint error, workspace highlight and
  selection-clear deadlines rely on `MAX_CLIENT_TIMER_DELAY` (100 ms) polling, so
  the client loop wakes 10 times a second forever while idle. Folding those
  deadlines into `timer_delay` would allow a long idle sleep.
- **Keyboard mode not re-synced on snapshot installs.** The keybinding-change mode
  reset in `apply_active_snapshot` (Prefix, Navigate or Resize back to Terminal)
  is not followed by `sync_client_shell_keyboard_report_all`, and snapshot
  installs do not go through `finish_client_shell_input`, so the host stays in
  report-all mode until the next input. Harmless today because the next batch
  re-syncs, but "host mode follows shell mode" is only restored lazily.

## CLIENT-025 - A request that never left the client is still presented as interrupted

Scope: client-endpoint (lateral).

Requests refused before they enter the send queue now complete silently, since
their outcome is known. `EndpointCommands::send_next` still reports two different
cases through one outcome: a request retired for a stale connection generation
before it was ever sent, and a transport send failure after it may have reached
the server. Its caller presents both as "This server action was interrupted.
Check its state before retrying", which suggests a partial application that
cannot have happened in the first case. Split the outcome in the endpoint command
lane so a never-sent request is reported as not sent (or silently dropped with
the others), and only a send failure keeps the interrupted wording.

## CLIENT-026 - Side effects of routing every explicit pick through the runtime

Scope: client-shell (lateral).

Every sidebar and navigator pick now goes through `ActivateEndpoint`, so the
runtime can re-prove an unowned endpoint or retarget a handoff.

- **An offline active machine shows a notice on every row click.** Clicking the
  active remote's row outside the collapse toggle toggles collapse and also sends
  a pick; when that remote is offline, the pick shows "X is not ready" each time.
  Only send the pick when it can do something (the presentation is not owned and
  the endpoint is online), or keep row clicks collapse-only for the active
  machine.
- **Picks wait one loop iteration.** The pick is scheduled for the next loop
  event, so pane input from the same stdin read is written before the focus
  change; before, a sidebar workspace click queued its focus command in the same
  dispatch. Unlikely to matter, but it is an ordering change worth keeping in
  mind.
