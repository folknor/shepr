# Defects: server loop and transport

Filed from the defect hunt over `crates/shepr-server/src/server/`,
`crates/shepr-server/src/ui/` and `crates/shepr-daemon/`.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## SLOOP-001 - A reply that fits the control queue alone can still close a busy client

`response_message` (`server/client_commands.rs`) now answers
`EndpointError::ResponseTooLarge` for a reply that cannot fit the client control
queue cap (`CLIENT_CONTROL_QUEUE_MAX_BYTES`) by itself. Residue: a reply that
fits the cap alone but lands while earlier control items occupy part of it is
still refused by `ClientWriterQueue::send_control` (`client_transport.rs`),
which closes the connection under the slow-reader policy. Letting a single item
exceed the cap when the queue is otherwise empty, or answering such a reply
with an error instead of a disconnect, would close it.

## SLOOP-003 - Requests still buffered at the stop are refused after the shutdown notice

Held endpoint replies are now resolved and queued before `ServerShutdown` is
broadcast, and a request already dequeued when the stop arrives gets its
refusal first. Residue: requests still buffered in the server event receiver
are refused during shutdown cleanup, after the notice, so a client that tears
down on `ServerShutdown` may never read those refusals. The comment in
`lifecycle.rs` states this ordering.

## SLOOP-004 - A slow client turns every drain of its render slot into a full render for everyone

Claims broken: the closing comment of `render_and_stream`
(`headless/render.rs`): "Full-frame recovery is tracked per connection. A slow
client must not keep responsive peers on the global full-render path while it
waits for its render slot to drain." Also AGENTS.md "Hot paths multiply".

When a client's one-slot render lane is full, `render_and_stream` and
`render_retained_pane_surface_and_stream` call `defer_full_render()` on it. When
its writer drains the slot it posts `ClientWriterDrained`;
`handle_server_event_with_render_impact` returns the client's deferred
`RenderDemand::Full`, and the loop joins it into the one global `render_demand`.
The next render is `render_and_stream` for every client: every responsive peer's
surface is recomputed in full and the retained path is skipped for all. A client
behind a slow link (an SSH bridge, a paused terminal) whose slot is usually full
puts every peer on the full path at its drain rate.

Direction: make demand per client. A drained client should be rendered alone
(full for it, nothing for the others), and a PTY change should go retained to
every client that can take it.

## SLOOP-005 - The retained scrollbar patch can paint over the right border of a narrow pane

Claim broken: the retained path's contract
(`render_retained_pane_surface_and_stream`, `headless/retained_surface.rs`):
"Applies terminal dirty rows to the committed origin-relative pane surface. Any
presentation or geometry uncertainty falls back to the complete renderer."

The complete renderer decides the scrollbar gutter through
`terminal_content_rect` (shepr-mux `workspace/geometry.rs`): a pane whose inner
width is 4 or less gets no gutter and `stable_scrollbar_gutter` (`ui/panes.rs`)
returns `scrollbar_rect = None`. `retained_scrollbar_patch` re-derives the rect
as `inner_rect.x + inner_rect.width`, checked only against `pane.rect`. With no
gutter that column is the pane's own right border when it has one (outer borders
on, `pane_borders = "always"`, or `pane_gaps`), inside `pane.rect`, so the check
passes. Once such a pane gains scrollback, a retained update paints scrollbar
cells over its right border and sets `scrollbar_rect` on the wire pane (the
client then hit-tests a scrollbar in the border). The next full render puts the
border back, so the frame flickers.

Direction: carry the gutter decision (or the full `PaneInfo`) beside the
committed baseline, as `surface_pane_identities` already carries pane identity,
and patch only the rect the full renderer reserved.

## SLOOP-006 - Doc: `host_shutdown_monitor` is never dropped

`HeadlessServer::host_shutdown_monitor` (`headless.rs`): "`None` before `run`
and while the server has dropped it to release its delay lock (see
`freeze_for_host_shutdown`)." Nothing drops it; `freeze_for_host_shutdown` calls
`release_delay_lock(generation)` and the monitor lives until the server does.

## SLOOP-007 - Doc: `ServerEvent::QuitSignal` is a host-shutdown wake

`client_transport.rs`: "Ctrl+C or external shutdown signal received." Its only
producer is the logind monitor's wake closure in `start_host_shutdown_monitor`;
Ctrl+C goes through the stop latch. The handler's comment in
`apply_server_event` ("the next iteration will initiate shutdown") is also
wrong: a host shutdown warning freezes saves and does not stop the server. The
name misleads.

## SLOOP-008 - A dead `ClientWriterDrained` arm in `apply_server_event`

`handle_server_event_with_render_impact`, the only caller, intercepts that event
first. Two copies of the same rule, one dead.

## SLOOP-013 - Server-event and API drains are unbounded

`drain_server_events` and `drain_api_requests_with_shutdown_check` drain until
empty, while internal events are capped at `APP_EVENT_DRAIN_LIMIT` "so clients
still get service". Producers refill from other threads while the loop drains,
so a pane-input flood from several clients, or an agent hook storm on the API,
can postpone rendering and the scheduled tasks for as long as it lasts. Bound
both like the internal one.

## SLOOP-016 - Structural: bootstrap restores before binding the client socket

`headless/bootstrap.rs`: lease, then the API socket, then `App::with_paths`
(restore spawns a fresh shell for every saved pane), and only then
`HeadlessServer::new` binds the client socket. If the client socket is held by
another server (the case `startup_error(ServerSocket::Client, ..)` exists for),
a full session's shells were spawned, ran their rc files and are killed again.
Binding both sockets before restoring would make the refusal free. The API
socket also accepts during restore with nobody answering; a `ping` there waits
out the restore.

## SLOOP-017 - Structural: three outbound routes with different ordering rules

Outbound messages to a client take the control lane (FIFO, byte-capped, overflow
closes the connection), the one-slot render lane (drained after control), and
the endpoint-reply outbox in the loop (held until after the render). SLOOP-001,
SLOOP-003 (both residues) and the ghost-client finding in `notes/bugs-rejected-candidates.md`
are each a place where two of these disagree. One per-client outbox type owning
ordering, size policy, flush barriers and disconnect reporting would remove the
class.

## SLOOP-018 - Structural: render demand is stored edge-triggered state

Geometry control is now resolved from the current viewers (a remembered
controller wins only while it views the workspace). Render demand (SLOOP-004)
is still stored, edge-triggered state that paths must remember to update, while
pane focus (`sync_pane_focus`) is derived level-based from the views. Deriving
per-client demand the same way is the remaining rewrite.

## SLOOP-020 - The geometry fallback ignores outer focus

Lateral. `workspace_geometry_source` (`headless/client_views.rs`) falls back to
the lowest-id viewer when the remembered controller is not viewing, ignoring
outer focus. If a stale controller survives to a surface activation,
`resize_shell_workspaces_sized_for` can size the workspace for the activating
client although a focused viewer is present, bypassing the rule that surface
activation does not claim a workspace another focused active shell already
views. Settlement after navigation makes this hard to reach. Preferring a
focused viewer before the lowest id, in both the fallback and
`reapply_controlled_shell_workspace_geometry`, would close it.

## SLOOP-019 - A mouse release over another pane leaves the press held

Lateral. Held presses (`clients.rs`) are keyed by (target pane, press id), so a
mouse Up delivered to a different target than its Down (a release over another
pane) does not clear the Down entry. Abrupt teardown then sends a stray release
to the original pane. Minor, and new with the per-target key.
