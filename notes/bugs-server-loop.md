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

## SLOOP-028 - The connection-limit refusal can be lost, and its counter is process-wide

Lateral, `server/client_accept.rs`. Client connections are capped and an
over-cap client gets `HandshakeRefusal::ConnectionLimit`. But
`reject_busy_client` writes the preamble and refusal and drops the stream
without reading the client's preamble and hello, so a client that writes its
hello after the close can get EPIPE and report a lost connection instead of the
limit (it retries either way). A bounded read of the hello, or `shutdown(Write)`
and a short drain, makes the message reliable. Separately, the admission counter
`ACTIVE_CLIENT_CONNECTIONS` is a process-wide static shared by every
`HeadlessServer` in the process, in-process test servers included; hold it on
the server. Also: `HeadlessServer::new` binds the client socket at the path the
startup reservation carries; a `debug_assert_eq!` that this equals
`client_socket_path(&app.paths)` was removed (debug asserts are disallowed), so
pin that equality with a bootstrap test instead.

## SLOOP-029 - An Unchanged plan's metadata skips the size check

Lateral, `server/render_stream.rs`. When `recompute_pending` is set, an
Unchanged plan for an invalid grid or oversized metadata is still sent as an
empty `SurfaceUpdate`. That is intended, but its metadata comes from
`baseline.update` without the `metadata_fits` check the other paths apply.
Confirm the metadata can never exceed the wire limits on that path, or check it.

## SLOOP-004 - A slow client turns every drain of its render slot into a full render for everyone

Outside the resolution loop: the owner resolves this directly. Do not assign it
or related bugs to fixers.

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

## SLOOP-017 - Structural: three outbound routes with different ordering rules

Outside the resolution loop: the owner resolves this directly. Do not assign it
or related bugs to fixers.

Outbound messages to a client take the control lane (FIFO, byte-capped, overflow
closes the connection), the one-slot render lane (drained after control), and
the endpoint-reply outbox in the loop (held until after the render). SLOOP-001,
SLOOP-003 (both residues) and the ghost-client finding in `notes/bugs-rejected-candidates.md`
are each a place where two of these disagree. One per-client outbox type owning
ordering, size policy, flush barriers and disconnect reporting would remove the
class. A related case: endpoint replies in the outbox (`headless.rs`) can pile
up behind one unresolved worker reply while a raw local client keeps sending
commands, with no bound on the held replies.

## SLOOP-018 - Structural: render demand is stored edge-triggered state

Outside the resolution loop: the owner resolves this directly. Do not assign it
or related bugs to fixers.

Geometry control is now resolved from the current viewers (a remembered
controller wins only while it views the workspace). Render demand (SLOOP-004)
is still stored, edge-triggered state that paths must remember to update, while
pane focus (`sync_pane_focus`) is derived level-based from the views. Deriving
per-client demand the same way is the remaining rewrite.
