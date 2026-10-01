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

## SLOOP-016 - The client socket reservation is released before the real bind

Bootstrap now checks the client socket lock and liveness after the lease and
API bind but before restore, so a held client socket is refused before any
shell spawns, while the listener stays unpublished for the API-first startup
transition. (The claim that a `ping` waits out the restore was wrong: the API
answers it on its connection thread.) Residue: the reservation in
`headless/bootstrap.rs` is released before the platform binder reacquires its
lock, so a listener can race into that gap. Closing it needs a shepr-platform
IPC helper that hands the held lock to the binder.

## SLOOP-024 - Other unbounded server queues and per-request threads

Lateral. The headless worker completion channel (`headless/worker.rs`) is
unbounded, and checkout-root handling (`headless/endpoint_requests.rs`) starts a
thread per request with no evident in-flight limit. Check whether either can
grow without bound under a flood of client requests, and bound them if so.

## SLOOP-025 - The API queue capacity matches the connection cap only by value

Lateral. `API_REQUEST_CHANNEL_CAPACITY` (shepr-server `limits.rs`) equals
`MAX_ACTIVE_CONNECTIONS` (shepr-api, `pub(crate)`), and its doc relies on that:
each API connection carries one request, so a responsive loop never fills the
queue. Nothing ties the two; export the connection cap and derive the capacity
from it. Smaller hygiene from the same wave: the test-only `App::new` still takes
an unbounded API receiver it ignores (about 40 test call sites pass one), and
the client socket reservation tests in `headless/bootstrap.rs` sit in a module
named `startup_cwd_tests`.

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
class.

## SLOOP-018 - Structural: render demand is stored edge-triggered state

Outside the resolution loop: the owner resolves this directly. Do not assign it
or related bugs to fixers.

Geometry control is now resolved from the current viewers (a remembered
controller wins only while it views the workspace). Render demand (SLOOP-004)
is still stored, edge-triggered state that paths must remember to update, while
pane focus (`sync_pane_focus`) is derived level-based from the views. Deriving
per-client demand the same way is the remaining rewrite.
