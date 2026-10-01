# Defect hunt: server loop and transport

Scope: `crates/shepr-server/src/server/` (headless loop and submodules, client
accept, transport and writer threads, client shell, pane input, render stream),
`crates/shepr-server/src/ui/`, and `crates/shepr-daemon/`. Findings are ordered
by how much they matter. Each names the claim it breaks.

## 1. A large endpoint reply disconnects the client instead of being refused

Claim broken: the doc comment on `response_message`
(`crates/shepr-server/src/server/client_commands.rs`): "only one past
`MAX_MESSAGE_SIZE`, which the client would refuse, is answered with
`EndpointError::ResponseTooLarge`, naming its size, rather than failing to send
and leaving the client to wait out its command timeout."

What happens: `response_message` measures against `shepr_protocol::MAX_MESSAGE_SIZE`
(1 GiB). The reply then goes onto the client's control lane through
`send_to_client` -> `ClientControlWriter::send` -> `ClientWriterQueue::send_control`
(`client_transport.rs`), which refuses any item larger than the free part of
`CLIENT_CONTROL_QUEUE_MAX_BYTES` (16 MiB, `limits.rs`) and, on refusal, calls
`close_connection()`. `send_to_client` then sees the error and runs
`remove_client_and_resize_if_needed`.

So a `pane.selection.read` whose text is between 16 MiB and 1 GiB (scrollback
is capped at 1,000,000 lines in `shepr-vt`, so a select-all over a long
history gets there) does not get `ResponseTooLarge`: the whole client
connection is shut down and the TUI loses the server. The same applies to any
reply that lands while earlier control items already occupy part of the 16 MiB.

Direction: the server has two size limits for one lane and they disagree. Pick
one bound for what a single control item may be (it must be at most the queue
byte cap) and use it in `response_within`; or let a single item exceed the
queue cap when the queue is otherwise empty (the cap is meant to bound a slow
reader's backlog, not one message). The former is the smaller change, the
latter is what the queue's own doc ("Bound control memory per client even
when a peer reads slowly") actually wants.

## 2. Navigating away leaves the old workspace sized for a client that no longer views it

Claim broken: the PTY size rule doc on `workspace_geometry_source`
(`headless/client_views.rs`): "When the controller stops viewing the
workspace, a remaining viewer takes it over
(`reapply_controlled_shell_workspace_geometry`)."

What happens: `handle_client_shell_command` (`headless/endpoint_requests.rs`)
only calls `reapply_controlled_shell_workspace_geometry` when the command's
`traits.changes_topology` is set. `workspace.focus` (and every other plain
navigation) has `changes_topology = false`, so it takes the other branch,
`claim_shell_workspace_geometry(client_id)`, which claims the workspace the
client navigated *to*. The workspace it left keeps that client as its
controller.

Scenario: clients A (200x60) and B (100x30) both view W1, A controls it. A
runs `workspace.focus` to W2. `workspace_geometry_source(W1)` still finds two
presenting clients and returns `Client(A)` because A is still active, so W1's
PTYs stay at 200x60 while only B looks at them. Nothing corrects it until B
sends input, gains outer focus or runs a geometry-claiming command.

Direction: the controller map is edge-triggered state that every path must
remember to repair, and this path forgets. The level-based approach
`sync_pane_focus` already uses would fix the whole class: derive "controller
of W" from the views each time the views change (keep the last claimant only
as a tie-break among current viewers), rather than storing a controller that
outlives the viewing. At the very least, call
`reapply_controlled_shell_workspace_geometry` whenever `navigate_shell_client`
moved the client.

## 3. Replies answered before the stop go out after the shutdown notice

Claim broken: the comment in the `Stopping` branch of `HeadlessServer::run`
(`headless.rs`): "Commands answered before the stop (a command that arrived
while stopping is answered with the refusal) still reach their clients, ahead
of the shutdown notice."

What happens: every path into the stop calls `initiate_shutdown` first
(the top-of-loop check, the post-select check, the API and endpoint
handlers). `initiate_shutdown` (`headless/lifecycle.rs`) immediately queues
`ServerShutdown` on every client's control lane via `send_to_all_clients` and
places the shutdown flush barrier. Held endpoint replies are only flushed on
the next loop pass (`resolve_pending_endpoint_replies_for_shutdown` and
`flush_endpoint_replies` in the `Stopping` branch), or inside
`reject_endpoint_request_for_shutdown`, both after that. The control lane is
FIFO, so the replies reach the socket behind `ServerShutdown` and behind the
flush barrier `await_shutdown_flushes` waits for. A client that tears down on
`ServerShutdown` never reads them. No test covers the order relative to
`ServerShutdown`
(`pending_endpoint_replies_leave_with_their_client_and_resolve_at_shutdown`
only checks the replies among themselves).

Direction: flush (and resolve) held replies inside `initiate_shutdown`, before
`send_to_all_clients`, or make the comment say what happens.

## 4. A failed health pong leaves a ghost client registered

Claim broken: the transport's own pattern that every reader exit tells the
loop (`send_client_disconnected` on every other early `break` in
`client_read_loop_with_endpoint_controls`), and the comment on `HealthPing`
that a client removed from the registry cannot be kept alive by pongs (the
converse, a dead connection staying in the registry, is what happens here).

What happens: on `ClientMessage::HealthPing` the reader sends the pong through
its own control-writer clone. If that send fails because the queue is over its
item or byte cap, `send_control` calls `close_connection()` (socket shut down,
`writer_alive = false`, so the writer thread exits without a write error and
sends nothing), and the reader does `break` without
`send_client_disconnected`. Neither thread reports the client gone.

The `ClientConnection` stays in the registry until the next server-side send
to it fails. Until then it still counts as presenting a surface: it keeps
geometry control (PTYs sized for a dead terminal), may stay the foreground
client (host theme, clipboard writes from unviewed panes go to it), keeps
panes it viewed in focus (no focus-out report), and counts in
`app_client_count` (git refresh cadence). On an idle session the next send
can be a long way off.

Direction: send `ClientDisconnected` on that `break` (and on the encode
failure `break` above it). Structurally, have the writer queue's
`close_connection` itself be the one place that reports the disconnect, so no
path that closes the queue can forget.

## 5. A slow client turns every drain of its render slot into a full render for everyone

Claim broken: the closing comment of `render_and_stream`
(`headless/render.rs`): "Full-frame recovery is tracked per connection. A slow
client must not keep responsive peers on the global full-render path while it
waits for its render slot to drain." Also the "hot paths multiply" principle in
AGENTS.md.

What happens: when a client's one-slot render lane is full,
`render_and_stream` and `render_retained_pane_surface_and_stream` call
`defer_full_render()` on that client. When its writer thread drains the slot
it posts `ClientWriterDrained`; `handle_server_event_with_render_impact` returns
the client's deferred `RenderDemand::Full`, and the loop joins it into the one
global `render_demand`. The next render is `render_and_stream` for every
client: every responsive peer's surface is recomputed in full
(`render_pane_surface`, frame build, diff) and the retained path is skipped
for all of them. A client behind a slow link (an SSH bridge, a paused
terminal) whose slot is usually full therefore puts every peer on the full
path at its drain rate, which is exactly what the comment says must not
happen. The per-connection tracking exists only for the slow client's own
baseline.

Direction: the loop has one global `RenderDemand` while baselines, deferrals
and render targets are per client. Make demand per client: a drained client
should be rendered alone (full for it, nothing for the others), and a PTY
change should go retained to every client that can take it. This is the
natural shape for "presentation is per client"; the global demand is a
leftover of the single-client design.

## 6. The retained scrollbar patch can paint over the right border of a narrow pane

Claim broken: the retained path's contract
(`render_retained_pane_surface_and_stream`, `headless/retained_surface.rs`):
"Applies terminal dirty rows to the committed origin-relative pane surface.
Any presentation or geometry uncertainty falls back to the complete renderer."
The patch must reproduce what the complete renderer would draw.

What happens: the complete renderer decides the scrollbar gutter through
`terminal_content_rect` (`shepr-mux` `workspace/geometry.rs`): a pane whose
inner width is 4 or less gets no gutter and `stable_scrollbar_gutter`
(`ui/panes.rs`) returns `scrollbar_rect = None`. `retained_scrollbar_patch`
re-derives the rect on its own as `inner_rect.x + inner_rect.width`, checked
only against `pane.rect`. With no gutter that column is the pane's own right
border when the pane has one (outer borders on, `pane_borders = "always"`,
or `pane_gaps`), which lies inside `pane.rect`, so the check passes. Once such
a pane gains scrollback, a retained update paints scrollbar cells over its
right border and sets `scrollbar_rect` on the wire pane (the client then
hit-tests a scrollbar in the border). The next full render puts the border
back, so the frame flickers between the two.

Direction: do not re-derive layout in the retained path. Carry the gutter
decision (or the full `PaneInfo`) beside the committed baseline, the way
`surface_pane_identities` already carries pane identity, and patch only the
rect the full renderer reserved.

## 7. Stale or untrue documentation in scope

Each states something the code does not do.

- `HeadlessServer::host_shutdown_monitor` (`headless.rs`): "`None` before
  `run` and while the server has dropped it to release its delay lock (see
  `freeze_for_host_shutdown`)." Nothing drops it; `freeze_for_host_shutdown`
  calls `release_delay_lock(generation)` and the monitor lives until the
  server does.
- `ServerEvent::QuitSignal` (`client_transport.rs`): "Ctrl+C or external
  shutdown signal received." Its only producer is the logind monitor's wake
  closure in `start_host_shutdown_monitor`; Ctrl+C goes through the stop
  latch. The handler's comment in `apply_server_event` ("the next iteration
  will initiate shutdown") is also wrong: a host shutdown warning freezes
  saves and does not stop the server. The name itself misleads; it is a
  host-shutdown wake.
- `apply_server_event` has a `ClientWriterDrained` arm that cannot run:
  `handle_server_event_with_render_impact`, the only caller, intercepts that
  event first. Two copies of the same rule, one dead.
- `render_and_stream` (`headless/render.rs`): "Rendered above for every
  active shell client ... so there is always a surface here". The surface is
  `None` whenever rendering was deferred (synchronized output, a poisoned
  core, a changed epoch); the `continue` is the normal path there, not an
  impossibility.
- Lateral (protocol scope): `EndpointCommandTraits::mutates_ui`
  (`shepr-protocol/src/command.rs`) says "so the server renders after it".
  The server never reads it; render demand comes from the app's outcome.
- The PTY size rule doc lists "surface activation" among the claims, but
  `set_client_shell_surface_active` skips the claim when another client with
  outer focus already views the workspace. The exception is reasonable; the
  rule doc does not mention it.

## 8. Lower-confidence defects and edge cases

- Held-press tracking is keyed by key code or mouse button only
  (`ClientShellPressId`, `clients.rs`). A press of key K forwarded to pane A,
  then a press of K to pane B before any release, overwrites A's entry, so on
  abrupt teardown only B gets a release and A keeps K held, against "Presses
  forwarded by this shell that need release on abrupt teardown". Key the map
  by (target, press id).
- `drain_server_events` and `drain_api_requests_with_shutdown_check` drain
  until empty, while internal events are capped at `APP_EVENT_DRAIN_LIMIT`
  "so clients still get service". Producers run on other threads and refill
  while the loop drains, so a pane-input flood from several clients, or an
  agent hook storm on the API, can postpone rendering and the scheduled tasks
  for as long as it lasts. Bound both drains like the internal one.
- `ClientDisconnected` for a client already removed (every removal shuts the
  socket down, so the reader always reports EOF afterwards) is not
  short-circuited: `remove_client_and_resize_if_needed` runs again, re-applies
  every workspace's geometry, requests a recompute on every client, attempts
  pending agent resumes, and the event returns a full render. Same for
  `ClientDetach` followed by EOF.
- `apply_shell_geometry` / `apply_all_workspace_geometry` return "the rule
  applied to some workspace", not "something changed size". With one client
  that is always true, so every topology command, every client removal and
  every resize of the sole client requests a recompute of every client.
  `PaneRuntime::resize` is a no-op for an unchanged size, so this is wasted
  work rather than wrong output.
- During shutdown, a failed send in `send_to_all_clients`,
  `reject_endpoint_request_for_shutdown` or `flush_endpoint_replies` goes
  through `remove_client_and_resize_if_needed`, which re-applies geometry and
  may resize PTYs of a server that is stopping. Harmless, but a stopping
  server should not resize anything; a plain registry removal would do.
- `HeadlessServer::run`'s select treats a closed `api_rx` as `Timer`. A
  closed channel resolves at once on every poll, so the loop would spin. It
  never closes in production only because `run_server` keeps its original
  `api_tx` alive in a local for the whole `block_on`. Nothing documents that
  the local is load-bearing.

## 9. Structural observations

- Bootstrap order (`headless/bootstrap.rs`): lease, then the API socket, then
  `App::with_paths` (restore: spawns a fresh shell for every saved pane), and
  only then `HeadlessServer::new` binds the client socket. If the client socket
  is held by another server (the case `startup_error(ServerSocket::Client, ..)`
  exists for), a full session's shells were spawned, ran their rc files and are
  killed again. Binding both sockets before restoring would make the refusal
  free. The API socket also accepts during the restore with nobody answering;
  a `ping` there waits out the restore.
- Outbound messages to a client take three routes with different ordering
  rules: the control lane (FIFO, byte-capped, overflow closes the connection),
  the one-slot render lane (drained after control), and the endpoint-reply
  outbox in the loop (held until after the render). Findings 1, 3 and 4 are
  each a place where two of these disagree. One per-client outbox type that
  owns ordering, size policy, flush barriers and disconnect reporting would
  remove that class.
- Geometry control (finding 2) and render demand (finding 5) are both stored,
  edge-triggered state that paths must remember to update, while pane focus
  (`sync_pane_focus`) is derived level-based from the views and has none of
  these bugs. Deriving the controller and per-client demand the same way is
  the rewrite that pays.
