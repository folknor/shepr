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
