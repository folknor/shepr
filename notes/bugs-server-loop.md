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

## SLOOP-001 - A large endpoint reply disconnects the client instead of being refused

Claim broken: the doc on `response_message` (`server/client_commands.rs`):
"only one past `MAX_MESSAGE_SIZE`, which the client would refuse, is answered
with `EndpointError::ResponseTooLarge`, naming its size, rather than failing to
send and leaving the client to wait out its command timeout."

`response_message` measures against `shepr_protocol::MAX_MESSAGE_SIZE` (1 GiB).
The reply then goes onto the client's control lane through `send_to_client` ->
`ClientControlWriter::send` -> `ClientWriterQueue::send_control`
(`client_transport.rs`), which refuses any item larger than the free part of
`CLIENT_CONTROL_QUEUE_MAX_BYTES` (16 MiB) and on refusal calls
`close_connection()`. `send_to_client` then runs
`remove_client_and_resize_if_needed`.

A `pane.selection.read` whose text is between 16 MiB and 1 GiB (scrollback is
capped at 1,000,000 lines, so a select-all over a long history gets there)
shuts the whole client connection instead of returning `ResponseTooLarge`. The
same applies to any reply that lands while earlier control items occupy part of
the 16 MiB.

Direction: pick one bound for a single control item (at most the queue byte
cap) and use it in `response_within`; or let a single item exceed the queue cap
when the queue is otherwise empty (the cap is meant to bound a slow reader's
backlog, not one message), which is what the queue's own doc ("Bound control
memory per client even when a peer reads slowly") wants.

## SLOOP-002 - Navigating away leaves the old workspace sized for a client that no longer views it

Claim broken: the PTY size rule doc on `workspace_geometry_source`
(`headless/client_views.rs`): "When the controller stops viewing the workspace,
a remaining viewer takes it over (`reapply_controlled_shell_workspace_geometry`)."

`handle_client_shell_command` (`headless/endpoint_requests.rs`) only calls
`reapply_controlled_shell_workspace_geometry` when the command's
`traits.changes_topology` is set. `workspace.focus` and other plain navigation
have it false, so they take `claim_shell_workspace_geometry(client_id)`, which
claims the workspace the client navigated to. The workspace it left keeps that
client as its controller.

Scenario: clients A (200x60) and B (100x30) view W1, A controls it. A runs
`workspace.focus` to W2. `workspace_geometry_source(W1)` still finds two
presenting clients and returns `Client(A)` because A is still active, so W1's
PTYs stay at 200x60 while only B looks at them, until B sends input, gains outer
focus or runs a geometry-claiming command.

Direction: derive "controller of W" level-based from the views each time they
change, as `sync_pane_focus` does (keep the last claimant only as a tie-break
among current viewers). At minimum, call
`reapply_controlled_shell_workspace_geometry` whenever `navigate_shell_client`
moved the client. See SLOOP-018.

## SLOOP-003 - Replies answered before the stop go out after the shutdown notice

Claim broken: the comment in the `Stopping` branch of `HeadlessServer::run`:
"Commands answered before the stop (a command that arrived while stopping is
answered with the refusal) still reach their clients, ahead of the shutdown
notice."

Every path into the stop calls `initiate_shutdown` first, which immediately
queues `ServerShutdown` on every client's control lane via `send_to_all_clients`
and places the shutdown flush barrier. Held endpoint replies are flushed only on
the next loop pass (`resolve_pending_endpoint_replies_for_shutdown` and
`flush_endpoint_replies`) or in `reject_endpoint_request_for_shutdown`, both
after that. The control lane is FIFO, so the replies reach the socket behind
`ServerShutdown` and behind the flush barrier `await_shutdown_flushes` waits for.
A client that tears down on `ServerShutdown` never reads them. No test covers
the order relative to `ServerShutdown`
(`pending_endpoint_replies_leave_with_their_client_and_resolve_at_shutdown` only
checks the replies among themselves).

Direction: flush (and resolve) held replies inside `initiate_shutdown`, before
`send_to_all_clients`, or make the comment say what happens.

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

## SLOOP-014 - A `ClientDisconnected` for an already-removed client re-runs removal

Every removal shuts the socket down, so the reader always reports EOF afterwards,
and it is not short-circuited: `remove_client_and_resize_if_needed` runs again,
re-applies every workspace's geometry, requests a recompute on every client,
attempts pending agent resumes, and the event returns a full render. Same for
`ClientDetach` followed by EOF.

## SLOOP-015 - Geometry application reports "applied", not "changed"

`apply_shell_geometry` / `apply_all_workspace_geometry` return "the rule applied
to some workspace", not "something changed size". With one client that is always
true, so every topology command, client removal and resize of the sole client
requests a recompute of every client. `PaneRuntime::resize` is a no-op for an
unchanged size, so wasted work rather than wrong output.

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
SLOOP-003 and the ghost-client finding in `notes/bugs-rejected-candidates.md`
are each a place where two of these disagree. One per-client outbox type owning
ordering, size policy, flush barriers and disconnect reporting would remove the
class.

## SLOOP-018 - Structural: geometry control and render demand are stored edge-triggered state

Geometry control (SLOOP-002) and render demand (SLOOP-004) are stored,
edge-triggered state that paths must remember to update, while pane focus
(`sync_pane_focus`) is derived level-based from the views and has none of these
bugs. Deriving the controller and per-client demand the same way is the rewrite
the hunter says pays.

## SLOOP-019 - A mouse release over another pane leaves the press held

Lateral. Held presses (`clients.rs`) are keyed by (target pane, press id), so a
mouse Up delivered to a different target than its Down (a release over another
pane) does not clear the Down entry. Abrupt teardown then sends a stray release
to the original pane. Minor, and new with the per-target key.
