# Technical implementation spec: per-client render demand and one outbox per client

Written against `reference/technical-implementation-spec.md` (the contract this
document must satisfy). Spawned from item 4 of `notes/work.md` ("Server loop:
per-client render demand and one outbox per client", formerly SLOOP-004,
SLOOP-017 and SLOOP-018). Revised after two reviews
(`notes/spec-server-loop-demand-outbox-r1.md`,
`notes/spec-server-loop-demand-outbox-r2.md`); section 8 records the findings
that were not taken and why.

The server loop renders for every client whenever any one client's render slot
drains, keeps render demand as a stored flag that each path must remember to
set, and sends messages to a client over three routes with three ordering
rules. This spec replaces both: what each client is owed is derived from its
own state each loop iteration (its location, its baseline, its slot, the panes
it shows, and a server-wide view epoch for changes that really are
session-wide), and every byte to a client goes through one `ClientOutbox`.

Nothing in the originating item is deferred. Things the item implies and the
survey turned up are bricks below: the dead `full_redraw_pending` flag, rejected
candidate REJ-022 (a stopping server re-applies geometry when a send fails),
which falls out of the single reap point, and the SLOOP-001 residue (section
2.6), which falls out of the loop-side reply queue.

## 1. Contracts inventoried

`docs/` does not exist in this repository. `reference/` holds only
`technical-implementation-spec.md`, which governs this document's shape and
changes nothing here. `AGENTS.md` is the written contract the work touches.

Statements in `AGENTS.md` this spec relies on, none contradicted:

- "Presentation is per client": each connection keeps its own surface size,
  outer focus, location and window title, and nothing projects one client's view
  into `AppState`. The work extends the principle to render demand.
- "Hot paths multiply": work reachable from view computation, rendering, client
  frame fanout runs per event times panes times clients, and "preserve the
  hidden-pane early exits", "keep terminal-core locks short". The work removes a
  path that multiplies a slow client's drain rate by every peer's render cost,
  stops one client's resize, scroll or presentation sync from rendering every
  peer, keeps the hidden-only exit and the `RenderSignal` coalescing untouched,
  and bounds the terminal-core locks the new plan takes (3.5).
- "Render is pure": `compute_surface_for()` reads `AppState` by shared
  reference. Untouched.
- "State is separated from runtime": `AppState` stays pure data. Everything
  here lives on `HeadlessServer`, `ClientConnection` and the transport, none of
  which is `AppState`.
- "No wire compatibility obligations" and "Wire encoding is shepr's own": the
  wire is not touched at all (section 6).

One contract is changed: the "Presentation is per client" bullet gains a
sentence stating that what a client is owed is derived per client (brick
L2.9). Nothing else in `AGENTS.md` asserts the old behavior. The comment at the
end of `render_and_stream` ("Full-frame recovery is tracked per connection")
and the module doc of `crates/shepr-server/src/server/headless.rs` become true
statements instead of aspirations; L2.8 rewrites both against the new code,
along with every other comment section 2.7 lists.

Lints that shape the bricks (`brokkr.toml`): `numeric-consts-live-in-limits`
(new constants go in `crates/shepr-server/src/limits.rs`),
`headless-loop-reads-the-app-clock` and `server-transport-clock-is-injected`
(no new clock reads), `durable-text-does-not-cite-notes` (no code comment cites
this document or `notes/`; each comment carries its own context),
`durable-text-has-no-plan-labels` (no `SLOOP` or `REJ` labels in code),
`no-shouting-rust`, and the gremlin ban (ASCII only).

## 2. Survey of the ground

All paths are under `crates/shepr-server/src/` unless stated.

### 2.1 Render demand today

- `app/mod.rs`: `RenderDemand { None, Partial, Full }` with `join` as a max.
  `Partial` is produced only by the loop; the app's own outcomes
  (`Outcome::render`, `handle_*_with_render_demand`) produce `None` or `Full`.
  Users: `app/mod.rs`, `app/events.rs`, `app/api.rs`, `server/clients.rs`,
  `server/headless.rs`, `headless/endpoint_requests.rs`,
  `headless/retained_surface.rs`, `headless/render.rs`,
  `headless/internal_events.rs`, `headless/tests/mod.rs`.
- `server/headless.rs` `run`: one local `render_demand`, joined at about
  fourteen sites (render signal pending joins `Partial`; internal events, API
  requests, server events, scheduled tasks, automatic workspace, the cwd timer,
  title changes, worker completions and select arms join `Full`). Step 6 renders
  when demand is not `None` and either the cadence is due or
  `has_pending_presentation_work` holds. `Full` or a failed retained pass calls
  `render_and_stream`, which renders every client; `Partial` with PTY damage
  first tries `render_retained_pane_surface_and_stream` (patches for every
  client).
- Every `true` from `handle_server_event` joins `Full`, and most of them are one
  client's event: `ClientShellResize` (already `request_repaint`s that client),
  `ClientShellFocus`, `ClientShellPresentationSync` (returns whether its
  `PresentationReady` send succeeded, so every presentation sync is a full
  render for every client), `ClientShellPaneInput` when it scrolled a pane,
  endpoint commands that only navigate the requester, and `ClientShellConnected`.
  Section 3.5 audits each one.
- `server/clients.rs`: `ClientConnection::render_pending: RenderDemand`, with
  `deferred_render`, `defer_full_render`, `clear_deferred_render`,
  `take_deferred_render`.
- `server/headless/render.rs` `render_and_stream` and
  `server/headless/retained_surface.rs`: when `writer.render.try_send` reports
  `Full`, call `client.defer_full_render()`.
- `server/client_transport.rs`: the writer thread, after writing a render frame,
  posts `ServerEvent::ClientWriterDrained`. `HeadlessServer::handle_server_event`
  and `handle_server_event_with_render_impact` return the drained client's
  `render_pending`, and the loop joins `Full` into the one global
  `render_demand`. That join is the defect: it is the only way a single
  client's debt becomes everyone's work, and it happens at the slow client's
  drain rate.
- `render_and_stream`, on `SurfaceRenderDeferred::Changed` (the viewed
  workspace vanished, or a pane's content epoch moved while the surface was
  being drawn), calls `render_dirty.request_generic()`, which the loop turns into
  a global `Full`.
- `app.full_redraw_pending` is written only by `render_and_stream` (to `false`)
  and by tests (to `true`). Nothing in production sets it. The retained pass
  reads it as an "unsafe_state" fallback that therefore never fires outside
  tests. It is dead and goes (brick L2.7).
- Level-based precedent the item cites: `sync_pane_focus`
  (`server/headless/client_views.rs`) recomputes the focused-pane set from the
  views and diffs it against `focused_panes`; `workspace_geometry_source` is a
  pure function of the registry. Level-based state in the render path already
  exists too: the full path's `needs_projection` predicate
  (`session_generation != shell_session_generation ||
  projected_location_generation != location.generation() || snapshot.is_none()`)
  decides whether a client's snapshot is due, and `request_repaint` /
  `request_recompute` mark one client's surface as owed.

### 2.2 The retained pass already half-knows per-client demand

`render_retained_pane_surface_and_stream` skips a client with a deferred
render, keeps per-client baselines, and plans every client's patch before
sending any. But every unsafe condition inside it is a `fallback!` that
abandons the whole pass and sends the caller to `render_and_stream` for all
clients, including conditions that concern one client's baseline
(`baseline_mismatch`, `hyperlink`, `invalid_patch`, `scrollbar_patch`,
`synchronized_visible`, `alternate_screen_geometry`). The late-fallback test
`late_retained_fallback_leaves_all_client_baselines_unchanged` pins that
nothing is committed when a later recipient fails. The new rule keeps that
atomicity per client (a baseline changes only on its own successful send) and
drops the cross-client coupling.

### 2.3 The three routes today

1. Control lane: `ClientControlWriter` over `ClientWriterQueue` in
   `server/client_transport.rs`. FIFO, items cap `CLIENT_CONTROL_QUEUE_MAX_ITEMS`
   and byte cap `CLIENT_CONTROL_QUEUE_MAX_BYTES` (both in `limits.rs`), counted
   while queued and in flight. Overflow calls `close_connection` (socket shut
   down, `writer_alive = false`) and returns `SendError`. The writer thread
   drains control before the render slot.
2. Render slot: `ClientRenderWriter`, one `Option<Vec<u8>>` in the same queue
   state. `try_send_render` returns `Full` while occupied.
3. Endpoint-reply outbox: `HeadlessServer::endpoint_replies:
   HashMap<ClientId, VecDeque<EndpointReplyEntry>>` with a global ticket
   counter, in `server/headless.rs` (`queue_endpoint_reply`,
   `reserve_endpoint_reply`, `complete_endpoint_reply`,
   `flush_endpoint_replies`, `resolve_pending_endpoint_replies_for_shutdown`).
   Each entry is a `ServerMessage` (not yet framed) with an optional shutdown
   refusal; `complete_endpoint_reply` fills the reply and drops the refusal. A
   reserved entry whose worker has not finished holds every later entry of
   that client. Flushed after each render pass, when no render is pending, at
   shutdown (`initiate_shutdown`, the `Stopping` branch of `run`) and by
   `reject_endpoint_request_for_shutdown` (`headless/endpoint_requests.rs`);
   each flushed reply goes through `send_to_client` onto the control lane. No
   bound on held entries or bytes.

The ordering the code gives today, which this spec keeps as the stated
contract: control messages leave in queue order; a reply follows the snapshot
the command changed (the snapshot is queued during the render pass, the reply
after it, both on the control FIFO); the render slot is a separate droppable
lane with no ordering relation to control, and the writer prefers control, so a
reply can reach the socket before the surface frame of the same pass. The
`flush_endpoint_replies` doc comment already says so.

### 2.4 Disconnect reporting today

- Reader thread (`client_read_loop_with_endpoint_controls`): every protocol or
  validation exit sends `ServerEvent::ClientDisconnected` or `ClientDetach` on
  the event channel (FIFO with that client's input events, so queued keystrokes
  are applied first). Four exits report nothing: the two `HealthPing` failures
  (the pong fails to encode, or the control send fails), the `should_quit` loop
  exit, and the "main loop gone" exit when the event channel is closed. The last
  two need nothing (the server is going away).
- Writer thread (`client_writer_loop`): a failed write sends
  `ClientDisconnected` with `blocking_send`.
- Queue overflow (`ClientWriterQueue::send_control`): calls `close_connection`
  and reports nothing.
- Reader `HealthPing`: `writer.send(pong)` through its own control clone; if the
  queue is over cap, `close_connection` runs (writer thread exits without a
  write error, so it reports nothing) and the reader `break`s with no event.
  This is REJ-021: the `ClientConnection` stays registered, still presenting a
  surface (geometry control, foreground, focus, `app_client_count`), until a
  later loop-side send happens to fail.
- Loop-side sends (`send_to_client`, `send_to_all_clients`, the mouse-capture
  and keyboard-mode streamers, `render_and_stream`, the retained pass) each
  collect `broken_clients` or call `remove_client_and_resize_if_needed`
  recursively mid-iteration. During shutdown that re-applies geometry on a
  stopping server (REJ-022).

### 2.5 Dependents the teardown must rewire

- `server/clients.rs`: `ClientConnection` fields `writer`, `render_pending`,
  `host_mouse_capture_active`, `host_sgr_pixels_active`, `sent_window_title`,
  and `ClientShellState::host_keyboard_report_all_active`; `app_client_count`,
  `remove_client` (closes the writer), `render_targets` (filters on
  `writer.is_some()`), `ClientRegistry::clear`; test constructors
  `ClientConnection::new` and `with_shell` taking `Option<ClientWriter>`.
- Readers of the told state outside the streamers: the `ClientShellPaneInput`
  arm of `apply_server_event` reads `client.host_sgr_pixels_active == Some(true)`
  to decide whether pixel mouse reports are eligible
  (`downgrade_ineligible_pixel_mouse`); tests read the four fields directly.
- `server/client_transport.rs`: the writer queue types, `ClientWriter`,
  `ServerEvent::ClientShellConnected { writer }`, the handshake, both thread
  loops, `send_shutdown_to_unregistered_client`, and about thirty tests of the
  queue and writer loop.
- `server/headless.rs`: the reply outbox and its ticket type
  (`EndpointReplyTicket`, imported by `headless/worker.rs`), `send_to_*`,
  `frame_server_message`, `shutdown_unregistered_clients: HashMap<ClientId,
  ClientWriter>`, `reject_late_client_connections`,
  `reject_unregistered_endpoint_request_for_shutdown`, the loop,
  `handle_server_event*`, `apply_server_event`, `take_drained_writer_render`.
- `server/headless/render.rs`: `render_and_stream`, the two streamers,
  `has_pending_presentation_work`, `sync_immediate_pty_sources` and
  `any_shell_surface_contains_pane` (filter on `writer`),
  `workspace_has_synchronized_pane`.
- `server/headless/retained_surface.rs`, `server/headless/surface_interest.rs`
  (`set_client_shell_surface_active`: its projection-exhaustion removal, the
  presentation reset, `clear_deferred_render`, `discard_pending_render`),
  `server/headless/client_views.rs` (`presents_surface`, `clipboard_viewers`,
  geometry filters on `writer`), `server/headless/endpoint_requests.rs`
  (`queue_endpoint_reply`, `reserve_endpoint_reply`,
  `reject_endpoint_request_for_shutdown` and its flush barrier, the `changed`
  composition of `handle_client_shell_command`),
  `server/headless/lifecycle.rs` (`queue_shutdown_flushes`, `initiate_shutdown`,
  `complete_shutdown`, `send_to_all_clients`),
  `server/headless/internal_events.rs` (clipboard sends),
  `server/headless/worker.rs`, `server/render_stream.rs` (`ClientRenderState`,
  `prepare_pane_surface`), `server/client_commands.rs` (`response_message` doc).
- Tests: `server/headless/tests/*.rs` (about eighty call sites of
  `render_and_stream`, `render_retained_pane_surface_and_stream`, `.writer`,
  `defer_full_render`, `deferred_render`, the `HeadlessServer { .. }` literal in
  `test_headless_server`; about thirty writer-less fixture constructions in
  `headless/tests/mod.rs` and `locations.rs`), `server/netside_tests.rs`,
  `server/clients.rs` tests, `headless/client_views.rs` tests.
- `limits.rs`: two new constants (section 3.3).

### 2.6 SLOOP-001 and SLOOP-003

`notes/work.md` names both IDs without their text. Their text is in the history
of the deleted `notes/bugs-server-loop.md` (`git show
9618aca^:notes/bugs-server-loop.md`):

- SLOOP-001, residue: "a reply that fits the cap alone but lands while earlier
  control items occupy part of it is still refused by
  `ClientWriterQueue::send_control`, which closes the connection under the
  slow-reader policy". Commit 9618aca removed the entry as adjudicated: the
  queue keeps closing, with a comment in `send_control` ("sending past the
  remaining budget would let a slow reader exceed the backlog bound"), a matching
  sentence in the `response_message` doc in `server/client_commands.rs`, and the
  test `client_control_queue_closes_when_endpoint_reply_exceeds_remaining_byte_budget`.
  The adjudication's reason is about admitting a reply onto the lane past its
  budget. It does not cover holding the reply loop-side until the lane has
  room, which costs no lane budget and is bounded by the held-reply bound this
  spec adds. The item lists SLOOP-001 as a case of two routes disagreeing, so it
  is in scope, and this spec closes it that way (3.3). The queue-level policy
  and its test stay: anything else that overflows the control lane still closes
  the connection.
- SLOOP-003, residue: "requests still buffered in the server event receiver are
  refused during shutdown cleanup, after the notice". Resolved in the current
  code: `complete_shutdown` runs `reject_late_client_connections` (which drains
  the server event receiver and refuses buffered endpoint requests, registered
  or not) before `send_to_all_clients` broadcasts `ServerShutdown`. Pinned by
  `an_endpoint_request_queued_at_shutdown_is_answered` (registered client),
  `a_queued_new_client_gets_its_endpoint_refusal_before_shutdown` and
  `a_dequeued_new_client_waits_for_queued_commands_before_shutdown`
  (unregistered). This spec keeps that order when the refusals move onto the
  outbox (3.2) and keeps those three tests, assertions unchanged.

### 2.7 Comments that the change makes false

Each is rewritten in the landing that makes it false (rule 5: a stale comment is
a defect):

- L1: `ClientConnection::writer`'s doc; `send_to_all_clients`' doc ("Broken
  connections are tracked and cleaned up"); `send_to_client`'s doc and its
  fixture comment; `handle_server_event`'s comment ("a client they remove on a
  failed send is settled by `remove_client`");
  `remove_client_and_resize_if_needed`'s comment; the `HealthPing` arm's
  comment; `ClientRegistry::clear`'s comment (transport handles);
  `initiate_shutdown`'s comment and the `Stopping` branch comment in `run`
  (they name `flush_endpoint_replies`); the `endpoint_requests.rs` doc on
  `handle_client_shell_endpoint_request` ("ready replies leave after any render
  the command needs (`flush_endpoint_replies`)");
  `reject_endpoint_request_for_shutdown`'s doc; `flush_endpoint_replies`' doc
  (now on `release_endpoint_replies`); the `send_control` comment and the
  `response_message` doc (SLOOP-001, 3.3).
- L2: the module doc of `headless.rs`; the closing comment of the full pass;
  the oversized-surface comment in the full path ("Renders only run on real
  damage, so this does not spin"); `handle_server_event`'s writer-drain comment
  ("Preserve deferred demand"); `client_writer_loop`'s comment on
  `ClientWriterDrained` ("dropping it could leave the server's deferred render
  unclaimed"); the `ClientRegistry` doc; the "Only the surface waits" comment in
  the `Err(reason)` arm of `render_and_stream`.

## 3. Target design

Two coherent landings, ordered so `brokkr check` is green at each boundary:
landing 1 builds the outbox and the single close path on top of today's render
demand; landing 2 replaces render demand. Landing 2 depends on the render-slot
API landing 1 introduces. Both are full rewrites of the area they cover, not
local patches.

### 3.1 `ClientOutbox`

New file `server/outbox.rs`, declared `pub(crate) mod outbox;` in
`server/mod.rs`. It absorbs the writer queue from `client_transport.rs`
(`ClientWriterQueue`, its state, `ClientWriteItem`, `ClientControlItem`, the
handles, the shutdown helpers, the test pair) and the reply queue from
`headless.rs`. `client_transport.rs` keeps the handshake, the reader loop, the
writer thread loop and the event enum.

```rust
/// What happened to a message offered to a client.
pub(crate) enum Delivery { Queued, Closed }

/// What happened to a surface frame offered to a client.
pub(crate) enum SurfaceOffer { Queued, Occupied, Closed }

/// Everything the server sends one client. Owned by `ClientConnection`;
/// deliberately not `Clone` (it owns the loop-side reply queue and told state).
pub(crate) struct ClientOutbox {
    queue: Arc<OutboxQueue>,   // shared with the writer thread and the reader
    attached: bool,            // false only for `detached()` fixtures
    replies: ReplyQueue,       // loop-side, never shared
    told: Told,                // loop-side, never shared
}

/// The reader thread's handle: control sends and close only.
#[derive(Clone)]
pub(crate) struct ControlSender { queue: Arc<OutboxQueue> }

/// A held endpoint reply's place in one client's reply queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReplySeq(u64);

/// A held endpoint reply's identity, built by the server from the client id
/// and the `ReplySeq` the client's outbox returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReplyTicket { pub(crate) client_id: ClientId, pub(crate) seq: ReplySeq }
```

`OutboxQueue` is the existing `ClientWriterQueue` state machine under a new
name: control FIFO (items and bytes bounds, queued plus in flight), the one
render slot, `senders` count, `writer_alive`, the shutdown stream, and now two
additions: `wake: Arc<tokio::sync::Notify>` (the server loop's outbox wake,
3.2) and a `room_wanted: bool` in the locked state (3.3). The sender count
works as now: `ClientOutbox` adds one sender when constructed and removes it on
`Drop`; `ControlSender` adds one per clone and removes it on `Drop`. These
replace the two handle types `ClientControlWriter` and `ClientRenderWriter`;
the writer thread still exits when senders reach zero or the writer dies.

Methods on `ClientOutbox` (all `&self` unless they touch loop-side state):

```rust
pub(crate) fn for_connection(shutdown_stream: LocalStream, wake: Arc<Notify>) -> Self;
pub(crate) fn detached() -> Self;            // never connected, for fixtures
pub(crate) fn queue_handle(&self) -> Arc<OutboxQueue>;   // for the writer thread
pub(crate) fn control_sender(&self) -> ControlSender;    // for the reader thread

pub(crate) fn send(&self, message: &ServerMessage) -> Delivery;
pub(crate) fn surface_slot_free(&self) -> bool;
pub(crate) fn offer_surface(&self, framed: Vec<u8>) -> SurfaceOffer;
pub(crate) fn discard_pending_surface(&self);
pub(crate) fn flush_barrier(&self) -> tokio::sync::oneshot::Receiver<()>;
pub(crate) fn is_attached(&self) -> bool;    // loop-owned, constant per outbox
pub(crate) fn is_closed(&self) -> bool;      // read by the reap only (3.2)
pub(crate) fn close(&self);
```

and on `ControlSender`:

```rust
pub(crate) fn send(&self, message: &ServerMessage) -> Delivery;
pub(crate) fn close(&self);
```

There is no `send_framed`: every loop-side message is a `ServerMessage`, and
held replies reach the FIFO through `release_replies` inside the type.

Rules the type owns, stated once:

- `send` encodes with `shepr_protocol::encode_message` (so a payload past one
  frame goes out as several frames in one buffer, as now) and queues on the
  control FIFO. An encode error (a payload past the protocol's message limit)
  is a `warn!`, then `close()`, then `Closed`: a message the client can never
  receive is a server-side protocol failure, and a client left waiting on it
  would only time out. This is a behavior change from `send_to_client`, which
  today warns and keeps the client; it is reachable only past the 1 GiB
  message limit, which every producer bounds far below (clipboard writes are
  capped at 192 KiB upstream, replies at the control lane cap).
- Size policy is the control lane's existing one (items and bytes, in flight
  included); overflow calls `close()` and returns `Closed`.
- A send to a closed or detached outbox returns `Closed` and queues nothing.
- `flush_barrier` appends a barrier to the control FIFO exactly as `send_flush`
  does; its receiver resolves when the writer has flushed the prefix, or is
  dropped if the writer exits.
- `offer_surface` is the only way a render frame enters the slot. It returns
  `Occupied` while the previous frame has not been taken by the writer. The
  full step checks `surface_slot_free` before it renders a surface (3.6), and
  the writer only ever empties the slot, so a pass that checked first meets
  `Occupied` only through a bug; the arm records the debt (`owe()`, 3.5) and
  drops the frame, never panics.
- `close()` is the one place a connection ends from the server side: it sets
  `writer_alive = false`, clears queued items and the slot, shuts both socket
  directions, and calls `wake.notify_one()`. It is idempotent: a second call
  shuts nothing again, logs nothing and stores no second wake.
- `detached()` builds an outbox with `attached == false` and a queue that is
  not alive: sends return `Closed`, `is_closed()` is false (it never connected,
  so the reap leaves it alone), and the presenting predicates exclude it
  through `is_attached()`. This is what writer-less fixtures meant.

### 3.2 One close path, derived disconnect reporting

Every route that ends a connection from the transport side calls
`OutboxQueue::close_connection` (what `close()` wraps): control overflow, a
failed socket write (the writer loop calls it instead of
`send_client_disconnected`), a failed health pong, an encode failure on the
pong. `close_connection` calls `wake.notify_one()`; `wake` is one
`Arc<Notify>` created in `HeadlessServer::new` as `outbox_wake`, held by the
server and cloned into `ClientTransportHandler`.

The loop does not wait for an event to learn a client is gone. Closure is a
state of the outbox, and the loop derives it in one place:

```rust
/// Removes every client whose outbox closed. Returns whether any was removed.
fn reap_closed_clients(&mut self) -> bool;
```

called at the top of each loop iteration (after the shutdown check) and by
`complete_shutdown` (first, before `reject_late_client_connections`, so a
closed client's buffered request finds no registered client and is dropped
with it, and the SLOOP-003 order of refusals before `ServerShutdown` is
unchanged). It collects ids with `is_closed()`, removes each through
`remove_client` (which already releases held inputs, syncs pane focus and, for
the foreground client, promotes the latest remaining client and syncs the host
theme), then re-applies controlled geometry once, but only when the lifecycle
is not `Stopping` (a stopping server removes plainly; REJ-022). What the
caller does with the result: in landing 1 a `true` joins `RenderDemand::Full`,
exactly as a reader-side disconnect does today; in landing 2 the reap itself
calls `mark_view_changed()` when it removed a client on a running server
(removal can hand geometry to another client, change the foreground client and
the host theme, and for the last client it is what triggers the headless
layout). The select gets a new arm `() = self.outbox_wake.notified() =>
LoopEvent::Timer`, so a close wakes an idle loop; `Notify` stores a permit when
nobody is waiting, so a close that lands between the reap and the select is not
lost.

The reap is the only reader of `is_closed()`. Every presenting predicate
(`presents_surface`, `app_client_count`, `render_targets`,
`sync_immediate_pty_sources`, `any_shell_surface_contains_pane`,
`clipboard_viewers`, renamed `pane_viewers` in L2) replaces `writer.is_some()`
with `outbox.is_attached()`, which is loop-owned and constant, so those
predicates stay stable within an iteration (the PTY size rule reads them
several times per pass and must get one answer). The removal is the latch. A
client whose transport closed keeps its registry entry, its geometry role and
its foreground role until the next iteration's reap, which the wake makes
immediate; a loop-side send to it in that window returns `Closed` and queues
nothing. In particular a pane-less clipboard write routed by
`send_to_foreground_client` to a foreground client whose outbox has just
closed is lost; it is not rerouted, because the write targets the terminal the
user was last active in, and another client's terminal is the wrong place for
it.

`ServerEvent::ClientDisconnected` and `ClientDetach` stay, for reader-side
exits. They must keep riding the event channel: they are FIFO with the same
client's input events, so a client that sends keys and closes has its keys
applied before it is removed. Their handlers keep the existing
`!contains_key -> false` guard, so the duplicate that follows a writer-side
close (the reader sees the shutdown socket and reports EOF) is a no-op.

A writer-side close is not ordered with input: input events of that client
still queued in the event channel when its writer fails are dropped (the reap
runs first and the input arms find no client). This is a behavior change and is
benign: the client is gone, and its held inputs are still released at removal.

The loop-side `broken_clients` vectors in `render_and_stream`, the two
streamers, `send_to_all_clients` and the retained pass disappear: a send either
queues or finds the outbox closed, and neither case mutates the registry.
`send_to_client` loses its `remove_client_and_resize_if_needed` call.

Close sites inside the render path that today push to `broken_clients` call
`outbox.close()` explicitly and return `ClientPassOutcome::Closed` (3.6):
projection revision exhaustion, snapshot framing failure, and a non-oversized
surface framing failure. `set_client_shell_surface_active`'s projection
exhaustion calls `outbox.close()` instead of removing the client, and returns
`None` as now.

### 3.3 Held endpoint replies

`ReplyQueue` (private to `outbox.rs`) replaces `HeadlessServer::endpoint_replies`
and `next_endpoint_reply_ticket`:

```rust
struct ReplyQueue { next_seq: u64, entries: VecDeque<HeldReply>, held_bytes: usize }
struct HeldReply { seq: ReplySeq, ready: Option<Vec<u8>>, refusal: Option<Vec<u8>> }
```

Entries hold framed bytes, not `ServerMessage`s, so their size is known when
they are held. Methods on `ClientOutbox`:

```rust
pub(crate) fn hold_reply(&mut self, message: &ServerMessage) -> Delivery;
pub(crate) fn reserve_reply(&mut self, refusal: &ServerMessage) -> Option<ReplySeq>;
pub(crate) fn complete_reply(&mut self, seq: ReplySeq, message: &ServerMessage) -> Delivery;
pub(crate) fn resolve_replies_for_shutdown(&mut self);
pub(crate) fn release_replies(&mut self, mode: ReleaseMode) -> Delivery;
pub(crate) fn held_reply_count(&self) -> usize;

pub(crate) enum ReleaseMode { WithinBudget, Shutdown }
```

The server builds `ReplyTicket { client_id, seq }` from the `ReplySeq`, so the
outbox never needs to know its own client id.

Behavior carried over from today: entries leave in command order; a reserved
entry whose reply is not ready holds every later entry of that client and no
other client's; `complete_reply` fills a still-pending entry, drops its refusal,
and ignores a completion for an entry already resolved; a completion never
leaves both buffers held; `resolve_replies_for_shutdown` moves every pending
entry's refusal into its reply; a client that leaves takes its queue with it,
and a completion for a departed client finds no outbox and is dropped (the
server looks the outbox up by `ticket.client_id` directly where
`complete_endpoint_reply` used to scan every client's queue).

Byte accounting: `held_bytes` is the sum of the lengths of every `ready` and
`refusal` buffer held. `hold_reply` adds the reply; `reserve_reply` adds the
refusal; `complete_reply` adds the reply and subtracts the dropped refusal;
`resolve_replies_for_shutdown` moves bytes from `refusal` to `ready`, leaving
the total unchanged; release subtracts each released reply. Both bounds are
checked after the addition, before the entry is kept.

The bound (new, in `limits.rs`):

```rust
/// Most endpoint replies held for one client, ready or pending.
pub(crate) const MAX_HELD_ENDPOINT_REPLIES: usize = 64;
/// Most framed reply and refusal bytes held for one client. A held reply
/// drains into the control lane, whose own byte budget is this size.
pub(crate) const MAX_HELD_ENDPOINT_REPLY_BYTES: usize = CLIENT_CONTROL_QUEUE_MAX_BYTES;
```

Policy on overflow is the control lane's: `close()` the outbox and log a
`warn`. The reasoning, so the number is not folklore: the same-build client
sends one endpoint command at a time per endpoint (`EndpointCommands::send_next`
returns while a command is in flight) and frees the lane only on the reply or
its 60 second command timeout, so a legitimate backlog is one or two entries.
Only a client that ignores that discipline (a raw local client) piles replies
up, behind one unresolved worker reply bounded by
`MAX_WORKER_COMPLETION_BACKLOG` workers; 64 is far past any legitimate backlog,
and anything past it is a client the server should drop rather than buffer
for. A reply is never silently dropped: a client waiting on it would only time
out. `reserve_reply` returns `None` when either bound is hit (the outbox has
closed itself); `hold_reply` and `complete_reply` return `Closed` on either
bound.

Release (closes SLOOP-001). `release_replies(WithinBudget)` moves ready prefix
entries onto the control FIFO in order, through a new
`OutboxQueue::try_send_control_within_budget(data) -> Result<(), Vec<u8>>`
which, under the queue lock, either queues the bytes (they fit the remaining
items and bytes budget) or hands them back and sets `room_wanted = true`
without closing anything. A reply handed back stays at the head of the held
queue and keeps everything behind it. The writer thread's
`finish_control_item` checks `room_wanted` under the same lock; when set, it
clears it and calls `wake.notify_one()`, so the loop runs another iteration
once the lane has drained an item, and releases again. No reply ever enters
the lane past its budget, so the backlog bound the 9618aca adjudication
protected still holds; the bytes waiting loop-side are bounded by
`MAX_HELD_ENDPOINT_REPLY_BYTES`. A reply can always eventually fit:
`response_message` bounds one reply to the lane's whole byte cap.
`release_replies(Shutdown)` admits ready replies with the ordinary `send`
policy (overflow closes): at shutdown the connection is ending, the
`ServerShutdown` notice must follow the replies on the FIFO, and the flush
barrier wait is bounded by `SHUTDOWN_FLUSH_TIMEOUT`.

The server-level `release_endpoint_replies(&mut self, mode)` calls
`release_replies` for every client; a `Closed` result is left for the reap.
Its callers: after each render pass and when no pass is owed
(`WithinBudget`), and `initiate_shutdown`, the `Stopping` branch of `run` and
`reject_endpoint_request_for_shutdown` (`Shutdown`).

### 3.4 `Told`: what the client has been told

The dedup caches that describe what the control lane last delivered move from
`ClientConnection` and `ClientShellState` into the outbox, so that sending and
remembering that it sent are one operation:

```rust
#[derive(Default)]
struct Told {
    mouse_capture: Option<(bool, bool)>,   // (enabled, sgr_pixels)
    keyboard_report_all: Option<bool>,
    window_title: Option<Option<String>>,  // Some(None) = told to use its default
}
pub(crate) fn tell_mouse_capture(&mut self, enabled: bool, sgr_pixels: bool) -> Delivery;
pub(crate) fn tell_keyboard_report_all(&mut self, enabled: bool) -> Delivery;
pub(crate) fn tell_window_title(&mut self, title: Option<String>) -> Delivery;
pub(crate) fn window_title_is_current(&self, title: &Option<String>) -> bool;
pub(crate) fn told_sgr_pixels(&self) -> bool;    // mouse_capture == Some((_, true))
pub(crate) fn forget_presentation(&mut self);    // all three back to None
```

A `tell_*` sends only when the value differs from what was last told, and
records it only on `Queued`; a `Closed` outcome records nothing. Today
`host_mouse_capture_active`, `host_sgr_pixels_active`,
`host_keyboard_report_all_active` and `sent_window_title` are reset to `None`
in two places that must agree (`set_client_shell_surface_active` when
activating, and the `ClientShellPresentationSync` arm); both call
`forget_presentation()`. The streamers `stream_host_mouse_capture_mode`,
`stream_shell_keyboard_mode` and `sync_window_title` shrink to computing the
desired value per client and calling the matching `tell_*`. The
`ClientShellPaneInput` arm reads `client.outbox.told_sgr_pixels()` where it
read `host_sgr_pixels_active == Some(true)`. Tests that read the old fields
read the outbox accessors (a `#[cfg(test)]` accessor for each `Told` field).

### 3.5 Level-based render demand

Stored: one server-wide version for session-wide changes, and per-client
settle points and surface debt. Derived: everything else.

```rust
/// Moves whenever something every client's projection or surface may depend
/// on changed (application state, theme, PTY sizes, the client set). A
/// version, compared against what each client last settled at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ViewEpoch(u64);
impl ViewEpoch {
    pub(crate) const ZERO: Self = Self(0);
    pub(crate) const INITIAL: Self = Self(1);
    pub(crate) fn advance(&mut self);   // saturating_add(1)
}
```

in `server/render_stream.rs`. `HeadlessServer` gains `view_epoch: ViewEpoch`
(starts `INITIAL`) and `headless_settled: ViewEpoch` (starts `ZERO`), and
`fn mark_view_changed(&mut self)` that advances `view_epoch`. The epoch is a
version, not an edge-triggered flag: each client records which version it last
settled at, so a client that could not be served when the epoch moved keeps
that debt until it is served, and bumping never costs more than one pass per
client. It does not remove the remember-to-set half for session-wide changes: a
path that changes shared state and does not call `mark_view_changed()` still
renders nobody. That is why the per-client half below is derived from levels
the client's own state already carries, and why the audit in this section
assigns every current `true` return to one side.

`ClientRenderState` (`server/render_stream.rs`) gains the per-client half:

```rust
/// What this client is owed beyond what its baseline says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SurfaceDebt {
    /// Owed only what the baseline implies (missing, or recompute pending).
    Clear,
    /// A surface this client should have was not delivered.
    Owed,
    /// The last attempt produced a surface the client can never receive
    /// (oversized). Suppresses the baseline-implied debt until a retry is due.
    Refused,
}

settled: ViewEpoch,   // starts ZERO; the epoch the last pass settled this client at
debt: SurfaceDebt,    // starts Clear

pub(crate) fn is_settled_at(&self, epoch: ViewEpoch) -> bool;
pub(crate) fn settle(&mut self, epoch: ViewEpoch);
pub(crate) fn invalidate(&mut self);        // settled = ZERO: this client alone is stale
pub(crate) fn owe(&mut self);               // debt = Owed
pub(crate) fn refuse(&mut self);            // debt = Refused
pub(crate) fn clear_debt(&mut self);        // debt = Clear
pub(crate) fn retry_refused(&mut self);     // Refused -> Clear; others unchanged
pub(crate) fn surface_debt(&self) -> bool {
    match self.debt {
        SurfaceDebt::Owed => true,
        SurfaceDebt::Refused => false,
        SurfaceDebt::Clear => self.last_surface.is_none() || self.recompute_pending,
    }
}
pub(crate) fn takes_patches(&self) -> bool {
    self.debt == SurfaceDebt::Clear && self.last_surface.is_some() && !self.recompute_pending
}
```

`request_repaint` also sets `debt = Clear`: a missing baseline is itself the
debt, and a new size is a new chance for a refused surface.
`ClientConnection::{render_pending, deferred_render, defer_full_render,
clear_deferred_render, take_deferred_render}` are deleted.

`prepare_pane_surface` returns a three-way result instead of `Option`:

```rust
pub(crate) enum PreparedSurface { Ready(PreparedRender), Unchanged, RevisionsExhausted }
```

so the pass can tell "nothing to send" from "can never send again".

The plan, a pure function of the registry, the epoch, the outbox slots and
whether the render signal is pending (`render_dirty.is_pending()`, already
level-based):

```rust
pub(super) struct RenderPlan {
    pub(super) full: Vec<ClientId>,   // projection or surface due; ascending id
    pub(super) patch: Vec<ClientId>,  // current baseline, PTY damage pending
    pub(super) headless_geometry: bool,
}
impl RenderPlan {
    pub(super) fn has_full(&self) -> bool;  // !full.is_empty() || headless_geometry
}
impl HeadlessServer {
    pub(super) fn render_plan(&self, render_signal_pending: bool) -> RenderPlan;
}
```

`render_plan` walks `render_targets(&self.clients)` (ascending id, outbox
attached) and for each client computes, cheapest first:

```
active          = client.is_active_shell_client()
stale           = !client.render_state.is_settled_at(self.view_epoch)
projection_due  = shell.projected_location_generation != shell.location.generation()
                  || shell.snapshot.is_none()
owed            = active && client.render_state.surface_debt()
                  && self.surface_deliverable(id, &mut held_memo)
full  if stale || projection_due || owed
patch if !full && render_signal_pending && active && client.render_state.takes_patches()
```

`projection_due` is the client-local half of the full path's `needs_projection`
predicate; its third clause (`session_generation`) is session-wide and moves
only with changes that also call `mark_view_changed()`. A navigation, a
reconcile that moves another client, or a surface activation (which clears
`snapshot`) therefore puts exactly the moved client in `full` with no epoch
bump.

`surface_deliverable(id, held_memo)` is `outbox.surface_slot_free()` and then,
only if the slot is free, the viewed workspace holds no synchronized or
poisoned pane. The second half is a new `workspace_surface_held(&WorkspaceId)
-> bool` beside the existing `workspace_has_synchronized_pane` (which keeps its
geometry meaning): true if any visible pane's `synchronized_output_state()` is
`None` (poisoned) or `Some((true, _))`. `held_memo: HashMap<WorkspaceId, bool>`
lives for one `render_plan` call, so each workspace's cores are locked at most
once per plan however many clients view it. The check runs only for a client
that already has a surface debt and a free slot, so an idle server, and a
server whose only debtors are slot-blocked, take no terminal-core lock to
derive their plan. `render_plan` runs once per iteration, plus once more inside
the render branch only when the epoch moved or a refusal was cleared since the
first call (3.8).

`headless_geometry` is `render_targets.is_empty() &&
self.headless_settled != self.view_epoch`: with no client attached, a
workspace the server has not laid out gets its PTY size from the PTY size
rule, once per epoch (today this is the empty-targets branch of
`render_and_stream`).

Audit of the sites that join `Full` today. Session-wide (call
`mark_view_changed()`): internal events, API requests, scheduled tasks
(`mark_shell_projection_dirty`), `create_automatic_workspace`, the cwd timer,
`sidebar_title_changed`, `render_request.generic`, worker completions, the reap
(3.2), and from server events:

- `ClientShellConnected`: the new client starts at `settled == ZERO`, so it is
  stale on its own. Bump only if the arm created the automatic workspace, the
  foreground change changed the host theme (`sync_host_theme_from_foreground`
  returned true), or `claim_unowned_shell_workspace_geometry` applied geometry.
- `ClientShellResize`: `request_repaint` already gives that client a surface
  debt; its snapshot does not depend on its surface size
  (`snapshot_from_session` takes the session, boot id, revision and location).
  Bump only if `resize_shell_workspaces_sized_for` resized a workspace (PTY
  sizes are shared by every viewer).
- `ClientShellHostTheme`: bump when the theme changed (it already
  `request_recompute`s every client).
- `ClientShellFocus`: `outer_terminal_focus` feeds geometry selection and pane
  focus reports, not any client's projection or surface. Bump only if the
  foreground client changed or the geometry claim applied geometry.
- `ClientShellPresentationSync`: nothing. The replayed modes and title are
  control messages, and the client completes its activation on
  `PresentationReady` alone (`receive_presentation_effects_ready` in
  `crates/shepr-client/src/endpoint/activation.rs` waits for no surface).
- `ClientShellPaneInput`: bump if the foreground client changed or the geometry
  claim applied geometry; if it only scrolled the pane, call
  `invalidate()` on each of `pane_viewers(pane_id)` (the renamed
  `clipboard_viewers`): a scroll moves that pane's viewport for its viewers
  only.
- `ClientShellEndpointRequest` (`handle_client_shell_command` and the checkout
  root path): bump for drained internal events, an app outcome that changed
  state, a foreground change, `create_automatic_workspace`, and any geometry
  application; a navigation or a reconcile alone needs nothing (the moved
  clients' location generations make them `projection_due`).
  `ClientShellSurfaceSet` bumps when it reports a change: it moves foreground,
  geometry controllers and pane focus, all shared.
- `ClientDetach`, `ClientDisconnected`: bump when a client was removed.

To make the audit hold structurally, `apply_server_event`,
`handle_client_shell_endpoint_request` and `handle_client_shell_command` stop
returning a render `bool`; each arm calls `mark_view_changed()` or
`invalidate()` itself at the point it knows which applies, and
`handle_server_event` returns `()`. The geometry helpers they call already
return whether they applied geometry.

The no-spin argument is part of the design, not an accident. A client that
cannot be served stays out of the plan for as long as it cannot be served, and
each terminal outcome leaves no debt the plan can see:

- Slot occupied: not deliverable, so only `stale` or `projection_due` can put
  it in `full`; the pass then projects it (control lane, independent of the
  slot) without rendering a surface for it, calls `owe()`, and settles it. It
  re-enters the plan only when the writer frees the slot, and the writer posts
  `ClientWriterDrained` after the write, which wakes the loop.
- Synchronized or poisoned pane: same shape. The plan re-evaluates
  `workspace_surface_held` every iteration, so a pane that enters synchronized
  output between the plan and the render takes the client out of the next plan.
  The wake that frees it is the pane's own PTY repaint signal (`render_dirty`),
  which the loop already treats as work; a poisoned pane's actor closes it
  shortly and the death event changes the epoch.
- `SurfaceRenderDeferred::Changed`: `owe()`, and no `request_generic()`. The
  content epoch moved because the PTY wrote, and that write raised its own
  render signal; a vanished workspace is a session change that already bumped
  the epoch. The client is deliverable, so the next pass serves it at the
  render cadence, and only it.
- Oversized surface (`FramingError::Oversized`): `refuse()` and settle. The
  client is told once (`oversized_surface_reported`, unchanged). `Refused`
  hides the baseline-implied debt, so a client with no baseline (an oversized
  first frame, or one after a resize or activation) or with a pending
  recompute stays out of the plan. A refused client is retried by: a new epoch
  (it is stale again), its own `request_repaint` (debt back to `Clear`), a
  location change (`projection_due`), or PTY damage on a pane its viewed
  workspace shows (the render branch calls `retry_refused()` for every
  `pane_viewers` of each PTY source before it plans, 3.8). That keeps today's
  behavior that real damage retries, so a surface that shrinks back under the
  limit is sent; between damage the client costs nothing.
- `PreparedSurface::Unchanged`: delivered-equivalent; `clear_debt()` and settle.
  With `debt == Clear`, a present baseline and no recompute pending (that is
  what `Unchanged` requires), `surface_debt()` is false.
- `PreparedSurface::RevisionsExhausted`: logged once, then `outbox.close()` and
  `Closed`, exactly like projection revision exhaustion: the client reconnects
  with a fresh counter. Holding the last frame instead would leave a debt
  nothing can clear.

### 3.6 Passes

```rust
pub(super) struct PassReport {
    pub(super) full: Vec<ClientId>,     // rendered full (projection plus surface decision)
    pub(super) patched: Vec<ClientId>,  // took a retained patch
    pub(super) owed: Vec<ClientId>,     // could not take a surface this pass
    pub(super) surface_renders: usize,  // render_client_shell_pane_surface calls made
}
pub(super) fn render_pass(
    &mut self,
    plan: &RenderPlan,
    pty_sources: &HashSet<PaneId>,   // empty when the sources are hidden-only or absent
) -> PassReport;
```

`render_pass` replaces `render_and_stream` and
`render_retained_pane_surface_and_stream`. It captures
`let epoch = self.view_epoch;` on entry and settles every client at `epoch`,
never at a re-read of `self.view_epoch`; nothing in the pass bumps the epoch
(the `Changed` deferral no longer requests a generic render), and a bump that a
future change adds inside the pass then leaves its clients stale instead of
being swallowed.

1. Patch step. Skipped when `pty_sources` is empty or
   `settings.reveal_hidden_cursor_for_cjk_ime` is set (the setting disables
   patches by design; its candidates are added to the full set instead). Runs
   `render_patches(&plan.patch, pty_sources) -> PatchOutcome { sent, promote,
   owed }`.
2. Full step. `render_full(ids)` for `plan.full` plus `promote`, ascending,
   deduplicated. Before drawing anything it computes, per id, whether the client
   is an active shell and `surface_deliverable` (one memo for the step), and
   builds `remaining_surface_renders` and `shared_surface_renders` over the
   deliverable ones only, keyed by the existing `PaneSurfaceRenderKey`: a client
   that cannot take a surface is not a sharer and costs no surface render.
   Per client, one extracted function
   `render_client_full(&mut self, id, deliverable: bool, shared: &mut SharedSurfaces)
   -> ClientPassOutcome` with
   `enum ClientPassOutcome { Delivered, Unchanged, Owed, Refused, Skipped, Closed }`.
   The body is today's per-target body: projection if due (`needs_projection`,
   `refresh_stale_shell_session_cache` once per step, `timer_projections`,
   projection revision exhaustion), then the surface decision. When
   `deliverable` is false it returns `Owed` after the projection without
   rendering a surface. The function borrows the client's fields apart
   (`let ClientConnection { outbox, render_state, shell, .. } = client;`) instead
   of cloning a writer handle, which `ClientOutbox` no longer allows. Its
   single exit maps the outcome to the debt: `Delivered` and `Unchanged` ->
   `clear_debt`; `Owed` -> `owe`; `Refused` -> `refuse`; `Skipped` (inactive
   shell) -> `clear_debt`; `Closed` -> nothing (the reap removes it). Every
   outcome but `Closed` then calls `settle(epoch)`. There is no `continue` that
   skips the settle: the early `continue`s of today's loop become returns of an
   outcome. Oversized notices are sent after the step, to clients whose outcome
   was `Refused` and whose `oversized_surface_reported` was newly set.
3. Headless step. `if plan.headless_geometry { apply_all_workspace_geometry when
   has_workspace_without_area; self.headless_settled = epoch }`.
4. Common tail: `timer_projections.clear()` (it is only an optimization; a
   client not in the pass recomputes its candidate later, correctness
   unaffected), and `debug!` of the report (`full`, `patched`, `owed` counts and
   `surface_renders`). `full_redraw_pending` is gone.

Fewer clients in a pass means fewer sharers, never a different result.

Geometry in the full step: today each target whose workspace it sources
resizes the workspace before any observer draws. In a subset pass the source of
a viewed workspace might not be in the subset. The geometry step therefore
iterates the distinct workspaces viewed by the pass's clients, looks up each
one's `workspace_geometry_source`, and applies the existing
alternate-screen-or-pane-identity "changed" test using the source client's
baseline whether or not that client is in the pass. Which clients draw is a
per-client question; whose size a workspace takes is already a per-workspace
one, and this keeps them separate. When the step does apply geometry to a
workspace, it calls `invalidate()` on every viewer of that workspace that is
not in this pass, so the resize reaches them on the next iteration instead of
waiting for their next PTY patch to fail its geometry check.

### 3.7 The patch step, per client

`render_patches` is the body of today's retained pass with its coupling
removed. Each candidate is checked in a fixed order: slot first, then its own
baseline and patch, so a candidate that would fail both is owed, not promoted.

- A candidate whose slot is occupied cannot take a patch. The damage is
  consumed for everyone, so that client's baseline is now behind the terminal:
  `owe()` it. (It can be a candidate because the plan checks only
  `takes_patches`, not the slot, for patch candidates.) It is not promoted;
  promotion would only try to render what the slot cannot take.
- A candidate failing a check about its own baseline or its own patch is
  promoted: `baseline_mismatch`, `synchronized_visible`, `hyperlink`,
  `invalid_patch`, `scrollbar_patch`, `alternate_screen_geometry`, a patch the
  planner refuses (`prepare_pane_surface_patch` returned `None`), a framing
  failure (which also `request_repaint`s), and a synchronized pane appearing
  between planning and send. Only that client is promoted; the others' patches
  are still sent.
- A failure collecting a source pane's dirty rows (`runtime_missing`,
  `terminal_snapshot`, `terminal_patch`) promotes every candidate whose
  baseline contains that pane, and leaves candidates that do not.

A client that took a patch is settled at the pass epoch (it was not stale, or
it would have been in `full`). `PatchOutcome::promote` becomes the full step's
extra ids (3.6, step 2). `retained_surface_fallback_reason` keeps its role: set
to the reason of the first promotion, reported by
`report_retained_surface_fallback` after the full step so the log still says
why a pass fell back. The invariant the late fallback test pins carries over per
client: nothing is committed to a client's baseline until its own send
succeeded; planning for all candidates finishes before any send, as now.

### 3.8 The loop

`run` keeps its structure; the local `render_demand` and every `join` go. The
top of each iteration, after the `Stopping` check, calls
`reap_closed_clients()`. Step 6 becomes:

```rust
let render_signal_pending = self.app.render_dirty.is_pending();
let plan = self.render_plan(render_signal_pending);
let render_cadence_due = self.app.can_render_now(now);
if (plan.has_full() || render_signal_pending)
    && (render_cadence_due
        || (self.app.can_present_now(now)
            && (plan.has_full() || self.app.render_dirty.has_immediate_work())))
{
    let planned_at = self.view_epoch;
    let render_request = self.app.render_dirty.take();
    let pty_dirty = !render_request.pty_sources.is_empty();
    if pty_dirty { self.host_input_modes_dirty = true; }
    if render_request.generic { self.mark_view_changed(); }
    let (sidebar_title_changed, outer_title_synced) =
        self.sync_terminal_title_sources(&render_request.terminal_title_sources);
    if sidebar_title_changed { self.mark_view_changed(); }
    let retried = pty_dirty && self.retry_refused_viewers(&render_request.pty_sources);
    let plan = if self.view_epoch != planned_at || retried || pty_dirty != render_signal_pending {
        self.render_plan(pty_dirty)
    } else {
        plan
    };
    if plan.has_full() && !outer_title_synced { self.sync_window_title(); }
    if !plan.has_full() && !pty_dirty { continue; } // title-only work: nothing to draw
    let hidden_only = pty_dirty && !plan.has_full()
        && !self.pty_sources_visible_to_any_render_target(&render_request.pty_sources);
    let sources = if hidden_only { HashSet::new() } else { render_request.pty_sources };
    let report = self.render_pass(&plan, &sources);
    self.report_retained_surface_fallback();
    self.app.record_render_attempt(now, !hidden_only);
    self.release_endpoint_replies(ReleaseMode::WithinBudget);
    continue;
}
if !plan.has_full() && !render_signal_pending {
    self.release_endpoint_replies(ReleaseMode::WithinBudget);
}
```

`retry_refused_viewers(sources) -> bool` calls `retry_refused()` on every
`pane_viewers` of each source and returns whether any client changed from
`Refused`. `pane_viewers` already restricts to attached active shells viewing
the pane, so a hidden-only source retries nobody.

The deadline call `next_headless_loop_deadline_with_git_refresh` takes
`plan.has_full() || render_signal_pending` where it took `render_demand !=
None`. Because blocked clients are out of the plan, a server whose only debtor
is slot-blocked or held by a synchronized pane schedules no cadence wake for it.
The select's `render_notify` arm yields `LoopEvent::Timer` (it only wakes the
loop; the pending flag is read from the signal), and the new `outbox_wake` arm
(3.2) yields `LoopEvent::Timer`. `ClientWriterDrained` is handled as a wake:
the event arm does nothing, and the next derivation finds that client's debt.
`handle_server_event_with_render_impact` and `take_drained_writer_render` are
deleted; `handle_server_event` stays as the one entry and returns `()` (3.5).
`LoopEvent::RenderRequested` is deleted with its arm folded into `Timer`.

Behavior mapping, so nothing is lost: a plain PTY change is
`render_signal_pending` with `full` empty and every active client that takes
patches in `patch`; a session-wide change is an epoch bump that puts every
presenting client in `full`; a drained slow client is the only member of `full`;
a resize, scroll, navigation or presentation sync puts only the clients it
moved in `full` (or nobody, for a presentation sync); a client that is blocked
is in neither and costs nothing until it is not. Replies are released after
each pass for every client (the pass projected every stale or projection-due
client, blocked or not, so a reply never overtakes the snapshot its command
changed), and when no pass is owed; a cadence-held pass keeps its replies, as
today. A reply that does not fit its client's control lane waits loop-side and
is released on the wake the writer raises when the lane drains (3.3).

### 3.9 The deleted and the kept

Deleted: `RenderDemand` entirely (the app's outcomes carry a
`view_changed: bool` instead, L2.6), the loop's `render_demand`,
`has_pending_presentation_work`, `ClientConnection::render_pending` and its
five methods, `take_drained_writer_render`,
`handle_server_event_with_render_impact`, the render `bool` returns of
`apply_server_event`, `handle_client_shell_endpoint_request` and
`handle_client_shell_command`, `LoopEvent::RenderRequested`,
`app.full_redraw_pending`, the `request_generic()` call on a `Changed`
deferral, every `broken_clients` vector, `HeadlessServer::endpoint_replies` and
its ticket counter, `EndpointReplyEntry`, `EndpointReplyTicket`,
`HeadlessServer::frame_server_message` (a function in `outbox.rs`),
`ClientWriter`, `ClientControlWriter`, `ClientRenderWriter`, the
`Option<ClientWriter>` of `ClientConnection`, `send_client_disconnected` from
the writer thread's failure path, and the global fallback of the retained pass.

Kept unchanged: `RenderSignal` (`shepr-mux`), the hidden-only exit and
`sync_immediate_pty_sources`, `ServerEvent::ClientWriterDrained`, the shell
projection cache and its generations, the PTY size rule and geometry
controllers, the delta planner inside `prepare_pane_surface`, the surface
protocol, retained patch computation (`changed_rows`, scrollbar patches,
hyperlink guard), shutdown ordering, the control lane's close-on-overflow
policy for `send`.

## 4. Obstacles resolved inline

1. The loop runs inside a tokio task, so a transport-side close cannot use
   `blocking_send` on the event channel (it panics in async context, and a full
   channel would drop the report). Closure is therefore state plus a `Notify`
   wake (3.2), not an event.
2. Reader-side exits must keep ordering with input, so they stay events
   (3.2). The duplicate after a writer-side close is already absorbed by the
   existing `!contains_key` guards. Writer-side closes drop still-queued input
   of the departed client, which is stated and benign (3.2).
3. Level-based debt could spin where a client cannot be served. Section 3.5
   gives each non-serviceable condition a state that takes the client out of
   the plan (`Owed` behind a blocked slot or held pane, `Refused` for an
   oversized surface, closure for exhausted revisions) and a named wake that
   brings it back.
4. A subset pass could draw an observer before the workspace's source client
   resized the PTYs. Geometry resolves per workspace from its source, not per
   pass member, and out-of-pass viewers of a resized workspace are invalidated
   (3.6).
5. Reply ordering relative to the snapshot must survive a pass that renders
   only some clients. Every stale or projection-due client is projected in the
   pass whether or not its surface can be delivered, so releasing every
   client's replies after a pass keeps "reply follows the snapshot it changed"
   (3.8).
6. Fixtures that built a client without a writer need a stand-in that the reap
   does not remove: `ClientOutbox::detached()` is never connected, refuses
   sends, is not closed, and is excluded from the presenting predicates by
   `is_attached()` (3.1).
7. Presenting predicates must not read cross-thread state: they read
   `is_attached()`; the reap alone reads `is_closed()` and latches it by
   removing the client (3.2).
8. A reply held for a client that leaves, or completed after it left, must not
   resurrect it: `ReplyTicket` carries the client id and the server resolves it
   through the registry; a missing client drops the completion (as
   `an_endpoint_reply_for_a_departed_client_is_dropped` pins).
9. A reply that fits the lane alone but not its remaining budget must neither
   close the client nor exceed the backlog bound: it waits loop-side, and the
   writer wakes the loop when the lane drains (3.3).
10. `numeric-consts-live-in-limits`: both new constants live in
    `crates/shepr-server/src/limits.rs`; no literals in the outbox.

## 5. Landings, bricks and gates

Order: L1 then L2. Each ends with `brokkr fmt` and a green `brokkr check`.
There are no by-hand gates. This spec commits nothing.

### Landing 1: the outbox and the single close path

Behavior after L1: the same frames and render demand as today, with one
outbox, one close path, bounded held replies, and no per-site removals. Three
behavior changes, each intended: a reply that does not fit its client's
remaining control budget waits for room instead of closing the client
(SLOOP-001); an encode failure on a loop-side send closes the client instead of
being skipped (3.1); input still queued from a client whose writer failed is
dropped instead of applied (3.2).

- L1.1 `server/outbox.rs` with `OutboxQueue`, `ClientOutbox`, `ControlSender`,
  `Delivery`, `SurfaceOffer`, `ReplySeq`, `ReplyTicket`, `ReplyQueue`,
  `ReleaseMode`, `Told`, per 3.1 to 3.4; move `ClientWriterQueue` and its tests,
  the test pair (`ClientWriter::test_pair` becomes `ClientOutbox::test_pair`,
  `RenderLaneReceiver` moves with it), and `frame_server_message`. Add `wake:
  Arc<Notify>` and `room_wanted` to `OutboxQueue`, `wake.notify_one()` in
  `close_connection`, `try_send_control_within_budget`, and the `room_wanted`
  wake in `finish_control_item`.
- L1.2 `limits.rs`: `MAX_HELD_ENDPOINT_REPLIES`,
  `MAX_HELD_ENDPOINT_REPLY_BYTES`.
- L1.3 `client_transport.rs`: handshake builds `ClientOutbox::for_connection`,
  the writer thread takes `queue_handle()`, the reader takes `control_sender()`;
  `ServerEvent::ClientShellConnected` carries `outbox: ClientOutbox`; the writer
  loop's failure path calls `close_connection` instead of
  `send_client_disconnected`; the `HealthPing` arm sends the pong with
  `ControlSender::send` and on `Closed` (overflow or encode failure, both of
  which close) `break`s; `send_shutdown_to_unregistered_client` uses the
  outbox. `ClientTransportHandler` gains `wake: Arc<Notify>`. Rewrite the
  `HealthPing` comment.
- L1.4 `clients.rs`: `ClientConnection` replaces `writer`,
  `host_mouse_capture_active`, `host_sgr_pixels_active`, `sent_window_title`
  and `ClientShellState::host_keyboard_report_all_active` with `outbox`;
  constructors take a `ClientOutbox`; `app_client_count` and `render_targets`
  use `is_attached()`, `remove_client` calls `close()`. Rewrite the
  `ClientRegistry::clear` comment. `render_pending` and its methods stay in this
  landing.
- L1.5 `headless.rs`: delete `endpoint_replies`, `EndpointReplyEntry`,
  `EndpointReplyTicket`, `next_endpoint_reply_ticket`; the reply methods
  become thin calls into the outbox (`queue_endpoint_reply` ->
  `hold_reply`, `reserve_endpoint_reply` -> `reserve_reply` plus the ticket,
  `complete_endpoint_reply` -> registry lookup by `ticket.client_id` then
  `complete_reply`, `flush_endpoint_replies` -> `release_endpoint_replies(mode)`,
  `resolve_pending_endpoint_replies_for_shutdown` -> per-outbox resolve);
  `send_to_client`, `send_to_all_clients`, `send_to_foreground_client` call
  `outbox.send` and do not remove clients; `shutdown_unregistered_clients`
  holds `ClientOutbox`; add `outbox_wake`, `reap_closed_clients`, the select
  arm, the top-of-iteration reap (a `true` joins `RenderDemand::Full`), and the
  reap at the start of `complete_shutdown`; `remove_client_and_resize_if_needed`
  keeps its reader-event role and skips geometry when stopping. The
  `ClientShellPaneInput` arm reads `told_sgr_pixels()`. `worker.rs` takes
  `ReplyTicket`. `endpoint_requests.rs` handles `None` from `reserve_reply` by
  returning without starting the worker, and
  `reject_endpoint_request_for_shutdown` releases with `ReleaseMode::Shutdown`
  and takes its barrier from `flush_barrier`. Rewrite the L1 comments listed in
  2.7.
- L1.6 `render.rs`, `retained_surface.rs`, `surface_interest.rs`,
  `client_views.rs`, `lifecycle.rs`, `internal_events.rs`: replace
  `writer.control.send` / `writer.render.try_send` with `outbox.send` /
  `offer_surface` (`Occupied` -> `defer_full_render`, as `Full` did), delete the
  `broken_clients` vectors and call `outbox.close()` at the render path's close
  sites (3.2), route the streamers through `tell_*`, call
  `forget_presentation()` in `set_client_shell_surface_active` and the
  presentation-sync arm, replace `writer.is_some()` filters with
  `is_attached()`. `initiate_shutdown` and the `Stopping` branch release with
  `ReleaseMode::Shutdown`.

Tests (new, in `server/outbox.rs` and `server/headless/tests/`):

- `control_overflow_closes_the_outbox_and_wakes_the_loop`
- `a_closed_outbox_refuses_every_later_send_and_closing_twice_is_a_no_op`
  (after `close()`, `send` and `offer_surface` return `Closed`; a second
  `close()` stores no second `Notify` permit)
- `a_detached_outbox_refuses_sends_and_is_never_reaped`
- `a_failed_health_pong_leaves_no_ghost_client` (drives the reader's
  `ControlSender` to overflow, then `reap_closed_clients`, and asserts the
  client, its geometry controller and its foreground role are gone)
- `closing_the_foreground_client_hands_foreground_over_at_the_reap` (two
  clients, the foreground one's outbox closes; before the reap the registry is
  unchanged, after it the other client is foreground and its host theme
  applies)
- `a_reaped_client_joins_full_render_demand` (L1 form of 3.2's reap result)
- `a_stopping_server_reaps_closed_clients_without_reapplying_geometry`
- `held_replies_leave_in_command_order_behind_a_pending_ticket`
- `held_reply_count_over_the_bound_closes_the_outbox`
- `held_reply_bytes_over_the_bound_closes_the_outbox` (reached once through
  ready replies and once through reserved refusals)
- `completing_a_reply_drops_its_refusal_from_the_held_bytes`
- `a_reply_that_fits_the_lane_waits_for_room_instead_of_closing` (lane partly
  occupied; release keeps the reply held and the outbox open; finishing a
  control item wakes the loop; the next release queues it)
- `shutdown_release_admits_replies_ahead_of_the_shutdown_notice`
- `shutdown_resolves_pending_tickets_with_their_refusal`
- `a_completion_for_a_departed_client_is_dropped`
- `told_values_send_only_changes_and_forget_on_presentation_reset`
- `the_surface_slot_holds_one_frame_and_frees_when_the_writer_takes_it`
- `an_unencodable_message_closes_the_outbox`

Existing tests adapted to the outbox API, assertions unchanged:
`pending_endpoint_replies_leave_with_their_client_and_resolve_at_shutdown`,
`an_endpoint_error_reply_is_held_until_the_flush`,
`an_endpoint_reply_for_a_departed_client_is_dropped`,
`immediate_endpoint_replies_stay_after_earlier_commands`,
`slow_checkout_root_worker_does_not_hold_other_clients`,
`an_endpoint_request_queued_at_shutdown_is_answered`,
`a_queued_new_client_gets_its_endpoint_refusal_before_shutdown`,
`a_dequeued_new_client_waits_for_queued_commands_before_shutdown`,
`client_control_queue_closes_when_endpoint_reply_exceeds_remaining_byte_budget`
(still true of the queue's `send` policy),
`a_client_without_a_writer_does_not_cache_the_window_title` (becomes a
detached-outbox case), the tests that read the told fields directly, the
writer-loop and queue tests moved from `client_transport.rs`, and every fixture
that built a client with `None` for its writer (now `ClientOutbox::detached()`).

Gate:

```
brokkr fmt
brokkr check
```

### Landing 2: per-client render demand

Behavior after L2: a drained slow client costs only its own render; a PTY
change reaches every client that can take it as a patch; one client's resize,
scroll, navigation or presentation sync renders only the clients it moved.

- L2.1 `render_stream.rs`: `ViewEpoch`, `SurfaceDebt`, `settled` and `debt`
  plus their methods on `ClientRenderState`; `request_repaint` clears the debt;
  `prepare_pane_surface` returns `PreparedSurface`.
- L2.2 `headless.rs`: `view_epoch`, `headless_settled`, `mark_view_changed`;
  replace every `render_demand.join` with `mark_view_changed` per the audit in
  3.5; `apply_server_event` and `handle_server_event` return `()` and mark per
  arm; the reap marks the view changed; delete `LoopEvent::RenderRequested`,
  `handle_server_event_with_render_impact`, `take_drained_writer_render`;
  rewrite step 6 and step 7 per 3.8, with `retry_refused_viewers`.
  `endpoint_requests.rs`: `handle_client_shell_endpoint_request` and
  `handle_client_shell_command` mark per 3.5 and stop returning a render
  `bool`. `client_views.rs`: `clipboard_viewers` becomes `pane_viewers`, used
  by clipboard writes, scroll invalidation and refusal retry.
- L2.3 `render.rs`: `RenderPlan`, `render_plan`, `surface_deliverable` with its
  per-plan memo, `workspace_surface_held`, `PassReport`, `render_pass`,
  `render_full`, `render_client_full` with `ClientPassOutcome`, the geometry
  step per 3.6 with out-of-pass invalidation; the `Changed` deferral owes
  without `request_generic`; `render_and_stream` and
  `has_pending_presentation_work` are deleted.
- L2.4 `retained_surface.rs`: `render_patches` and `PatchOutcome` per 3.7
  replace `render_retained_pane_surface_and_stream`; the `fallback!` macro
  becomes per-client and per-source promotion with the slot checked first;
  `success!` reasons that were return values become trace lines.
- L2.5 `clients.rs`: delete `render_pending` and its five methods;
  `surface_interest.rs` loses `clear_deferred_render` (its `request_repaint`
  already clears the debt) and marks per 3.5.
- L2.6 `app/mod.rs`, `app/events.rs`, `app/api.rs`: `RenderDemand` is deleted;
  the outcome types that carried it carry `view_changed: bool`, and the
  `handle_*_with_render_demand` and `*_with_render` entry points return it;
  the enum's test goes, and every reader in `server/` follows.
- L2.7 Delete `App::full_redraw_pending` and its reads and test writes.
- L2.8 Rewrite the L2 comments listed in 2.7 against the new code. No `notes/`
  document is edited by this spec's landings.
- L2.9 `AGENTS.md`, "Presentation is per client": append "What each client is
  owed (a projection, a surface, a patch) is likewise derived per client from
  its own location, baseline and render slot, with a server-wide view epoch
  only for changes every client depends on (`render_plan` in
  `crates/shepr-server/src/server/headless/render.rs`), so one client's slow
  link, resize or scroll never moves another client's render path."

Tests (new, in `server/headless/tests/surface_delta.rs` and
`server/render_stream.rs`):

- `a_drained_slow_client_is_rendered_alone` (two clients, slow client's slot
  occupied through a PTY change; the drain event; the next pass reports only the
  slow client in `full`, `surface_renders == 1`, and the responsive client's
  slot, queued frame and baseline are untouched)
- `a_pty_change_reaches_every_client_that_can_take_it_as_a_patch`
- `a_view_change_owes_every_presenting_client_one_pass_and_settles_them`
- `a_resize_renders_only_the_resized_client_when_no_workspace_resizes`
- `a_scroll_renders_only_the_viewers_of_the_scrolled_pane`
- `a_navigation_renders_only_the_client_that_moved`
- `a_presentation_sync_renders_nobody`
- `a_client_blocked_by_its_slot_is_out_of_the_plan_until_the_slot_frees`
- `a_blocked_client_costs_no_surface_render_in_a_view_change_pass`
- `a_synchronized_pane_defers_the_surface_without_a_retry_loop`
- `a_changed_deferral_owes_its_client_without_a_view_change`
- `an_oversized_first_surface_is_refused_without_a_retry_loop` (starts from a
  client with no baseline, the case that would spin; asserts the plan is empty
  on the next iteration)
- `a_refused_client_retries_on_pty_damage_to_a_pane_it_shows`
- `exhausted_surface_revisions_close_the_client`
- `a_patch_for_a_client_with_an_occupied_slot_owes_it_a_full_surface`
- `a_retained_check_failure_promotes_only_its_client`
- `a_failed_source_collection_promotes_only_clients_viewing_that_pane`
- `a_subset_pass_resizes_a_workspace_from_its_source_client`
- `a_subset_pass_resize_invalidates_viewers_outside_the_pass`
- `replies_follow_the_snapshot_when_a_pass_renders_a_subset`
- `with_no_client_attached_a_workspace_is_laid_out_once_per_epoch`
- `a_reaped_client_leaves_survivors_with_updated_projection_and_surface`

Existing tests adapted (calls to `render_and_stream` become a test helper
`render_now(&mut server)` that marks the view changed and runs one plan and
pass; `render_retained_pane_surface_and_stream` calls become a plan-and-pass
helper; tests asserting `handle_server_event`'s `bool` assert the epoch or the
plan instead): `a_reaped_client_joins_full_render_demand` becomes
`a_reaped_client_marks_the_view_changed`,
`backpressured_shell_does_not_disable_retained_patches_for_responsive_peer`,
`full_render_backpressure_does_not_disable_responsive_peer_patches`,
`writer_readiness_does_not_invalidate_application_or_input_sources` (now:
a drain event changes no epoch and moves no application state),
`retained_patches_only_reach_shells_viewing_the_dirty_workspace`,
`sibling_retained_output_waits_for_synchronized_pane_to_finish`,
`retained_snapshot_survives_a_writer_waiting_for_the_terminal_core`, and
`late_retained_fallback_leaves_all_client_baselines_unchanged`, which is
renamed `late_retained_fallback_promotes_its_client_and_commits_no_patch_for_it`:
client 7 now receives its patch, client 8 (the one with the stale hyperlink)
gets none, keeps its baseline until its full pass, and is the only client in
the promoted set. (All of these live in `server/headless/tests/mod.rs`.)

Failure demonstration for the central test. `brokkr check` cannot show that
`a_drained_slow_client_is_rendered_alone` would catch the defect, because the
old code it would run against has no `PassReport`. After L2 is complete, the
implementer reverts the production half narrowly by making `render_plan` put
every presenting client in `full` whenever any client is there for surface debt
alone (the old global join, rebuilt on the new types), runs

```
brokkr test -p shepr-server a_drained_slow_client_is_rendered_alone
```

and sees it fail on `surface_renders`, then removes the revert and runs the
landing gate. The revert is never committed.

Gate:

```
brokkr fmt
brokkr check
```

## 6. Stopping rule

In scope: `crates/shepr-server/src/server/` (outbox, transport, clients, the
headless loop, render, retained surface, surface interest, client views,
endpoint requests, lifecycle, worker, client commands doc, tests),
`render_stream.rs`, `crates/shepr-server/src/limits.rs`, the `RenderDemand`
enum and its users and the dead `full_redraw_pending` in
`crates/shepr-server/src/app/`, the comment rewrites in 2.7, one `AGENTS.md`
sentence. No `notes/` document is edited.

Out of scope, and why it is not deferral:

- The wire protocol, `ServerMessage`, framing, the client crate. Nothing the
  client reads changes: same messages, same per-client order on the control
  lane, same surface and patch semantics. (A held reply can now reach the
  client later than before, never out of order.) Item 2 of `notes/work.md`
  (the client's request ledger and surface baseline) is a separate work item.
- Items 1 and 3 of `notes/work.md`.
- The PTY size rule, geometry controllers and the shell projection cache and
  generations, which are already level-based.
- `RenderSignal` and the render cadence in `shepr-mux` and the app
  (`can_render_now`, `record_render_attempt`).
- Reply versus surface-frame ordering. The contract stays as stated in 2.3
  (reply after snapshot on the control FIFO, surface on its own lane); making
  the surface frame precede the reply is a wire-visible ordering change and a
  client-side concern (item 2).
- The reader-side event ordering for input, which is correct as is.

## 7. Standing references

- Contract this spec is written against:
  `reference/technical-implementation-spec.md`.
- Originating item: `notes/work.md`, item 4 ("Server loop: per-client render
  demand and one outbox per client"), with REJ-021 and REJ-022 in
  `notes/bugs-rejected-candidates.md`, and SLOOP-001 and SLOOP-003 recovered
  from `git show 9618aca^:notes/bugs-server-loop.md` (both entries were removed
  by 9618aca; section 2.6).
- Written contract of the project: `AGENTS.md`.
- Reviews folded into this revision:
  `notes/spec-server-loop-demand-outbox-r1.md`,
  `notes/spec-server-loop-demand-outbox-r2.md`.

## 8. Review findings not taken

Every other finding of both reviews is folded above.

- r1 B1, "SLOOP-001 and SLOOP-003 were deleted by the current HEAD commit
  e68d6e5": both entries were removed earlier, by 9618aca; e68d6e5 only removed
  the SLOOP-017 text that cited them. The substance (the IDs are recoverable and
  the spec must dispose of them) is taken in 2.6.
- r1 B1, "SLOOP-001 is left unfixed": it was adjudicated by 9618aca (comment in
  `send_control`, doc in `response_message`, a pinning test), not forgotten.
  The fix r1 proposes is still taken, because the adjudication's reason does
  not apply to holding replies loop-side (2.6, 3.3). r1's alternative (admit one
  item past the budget when the lane is otherwise empty) is not taken: it
  weakens the backlog bound the adjudication protected.
- r1 B2, "a pane entering synchronized output between the plan's check and the
  render keeps the client deliverable": the plan re-evaluates
  `workspace_surface_held` every iteration, so the next plan excludes that
  client; it does not spin. The `Changed` half of the same bullet is taken.
- r1 B3, option two (keep the global epoch and narrow the claims): not taken;
  option one (per-client derivation with an audit) is.
- r2 3, "define how foreground lookup, fallback selection and activity
  promotion exclude closed outboxes": not taken in that form. Reading
  `is_closed()` in those predicates is the cross-thread instability r1 S1
  describes. Foreground is handed over at the reap instead, one wake-driven
  iteration later, and the test r2 asks for pins that handover.
- r2 4, "completion can leave both buffers retained": the current code drops
  the refusal on completion, and 3.3 now says so; there is no retained-refusal
  state to test. The accounting r2 asks for is taken.
- r1 S1's statement that `panes_holding_focus` and `window_title_clients` filter
  on `is_active_shell_client` only was not verified: the design no longer
  depends on it, because no predicate reads closure.
