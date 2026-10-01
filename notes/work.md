# Structural rewrites

The structural findings of the defect hunt, consolidated into one item per
area. Each item replaces a class of patched invariants with a shape that cannot
represent the failures. An item is removed entirely once its rewrite lands.

## 1. Client endpoints: one endpoint choice, no exclusive surface lease

Formerly CEND-014 and CEND-015.

"Which endpoint" is held in six places: the registry's `active`,
`Presentation` (`Owned` / `Handoff` / `Unavailable`), the selection tracker's
`selected` / `attempt` / `failed`, `ClientState::deferred_local`,
`ClientLoop::scheduled_activation`, and `PendingEndpointActivation::successor`.
`ClientLoop::run` re-derives agreement every turn (`settle`, then
`automatic_activation`), and `begin_endpoint_activation`,
`complete_endpoint_activation`, `rollback_endpoint_activation` and
`handle_endpoint_disconnect` each patch a subset. A rollback that tore down a
healthy target connection (since fixed) and REJ-025 (a rollback that ends
`Unavailable` leaves the source surface on) are consequences.

The handoff protocol behind it (source-off first, six phases, rollback through
target-off and source-on, successor intents, effects fence) exists to keep at
most one server-side surface on and pane input ordered. It treats a
server-side surface as an exclusive lease.

Direction:

- One owner for the endpoint choice: a single enum covering selected,
  deferred, handing off from/to, and failed-on-generation.
- Make surface activation idempotent per connection. The client chooses which
  connection's frames to draw and where to send input; the server is told only
  "viewing" or "not viewing", for its foreground and PTY size rules. The
  rollback paths in `activation.rs` and their failure modes then disappear
  rather than getting another round of patches.

## 2. Client shell: a request ledger and an explicit surface baseline

Formerly CSHELL-018 and CSHELL-019.

In-flight state is spread across `pending_requests`, `pane_scroll_in_flight`,
`pane_scroll_queued`, `pane_scroll_targets`, `copy_operation_in_flight`,
`copy_operation_queue`, `copy_input_queue`, `ClientWordSelection::pending_row`
and `PendingWorkspaceHighlight::request_id`. Cancellation runs a rollback owned
by each pending request kind, but ordinary completion still unwinds each
feature's own maps and queues by hand (a mismatched-boot result that skipped
its rollback was one path that forgot, since fixed).

The shell also keeps `pane_surface` and `pending_pane_surface` with revision
rules in three places (`set_pane_surface`, `install_pane_surface`,
`apply_pane_surface_patch`), and the reader in `lib.rs` keeps its own baseline.
REJ-011 (surface patches against a pending or dropped baseline fail the
connection) follows from this.

Direction:

- One ledger whose entries own both completion and rollback, so dropped
  follow-up work is impossible by construction.
- Model the surface explicitly: a server baseline that patches always apply to
  (mirroring the reader), and a separately chosen presentable pair (snapshot
  plus surface at the same revision). REJ-011 then cannot happen, and
  `Applied`/`Rejected` regain their meaning.

## 3. Server app: the pane-exit checkpoint as a typed state machine

Formerly SAPP-012.

The checkpoint in `session.rs` spreads one concept across loosely coupled
fields. The separate pending flag is gone (preservation is now the snapshot's
presence), but the requested generation, saved generation, failures,
readiness, `session_revision`, `critical_save_retry_deadline` and the
host-shutdown trio remain separate. Every finding in this area was an
invariant between two of them that some path forgot (a stale removal plan
clearing the bookkeeping was one, since fixed).

Direction: a typed enum (no checkpoint / requested gen N / saved gen N with
snapshot / abandoned) that makes that class unrepresentable.

## 4. Server loop: per-client render demand and one outbox per client

Formerly SLOOP-004, SLOOP-017 and SLOOP-018.

A slow client turns every drain of its render slot into a full render for
everyone. When a client's one-slot render lane is full, `render_and_stream`
and `render_retained_pane_surface_and_stream` call `defer_full_render()` on it.
When its writer drains the slot it posts `ClientWriterDrained`;
`handle_server_event_with_render_impact` returns the client's deferred
`RenderDemand::Full`, and the loop joins it into the one global
`render_demand`. The next render is `render_and_stream` for every client: every
responsive peer's surface is recomputed in full and the retained path is
skipped for all. A client behind a slow link (an SSH bridge, a paused
terminal) whose slot is usually full puts every peer on the full path at its
drain rate. This breaks the closing comment of `render_and_stream`
(`headless/render.rs`), "Full-frame recovery is tracked per connection", and
AGENTS.md "Hot paths multiply".

Render demand is stored, edge-triggered state that paths must remember to
update, while pane focus (`sync_pane_focus`) and geometry control are derived
level-based from the current views.

Outbound messages to a client take three routes with different ordering
rules: the control lane (FIFO, byte-capped, overflow closes the connection),
the one-slot render lane (drained after control), and the endpoint-reply outbox
in the loop (held until after the render). SLOOP-001, SLOOP-003 (both
residues) and REJ-021 (a failed health pong leaves a ghost client registered)
are each a place where two of these disagree. Endpoint replies in the outbox
(`headless.rs`) can also pile up behind one unresolved worker reply while a raw
local client keeps sending commands, with no bound on the held replies.

Direction:

- Derive render demand per client, level-based from the views, like pane
  focus. A drained client is rendered alone (full for it, nothing for the
  others), and a PTY change goes retained to every client that can take it.
- One per-client outbox type owning ordering, size policy, flush barriers and
  disconnect reporting, with a bound on held endpoint replies.
