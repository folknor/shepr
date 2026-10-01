# Technical implementation spec: one endpoint choice, no exclusive surface lease

Written against `reference/technical-implementation-spec.md` (the contract this
document must satisfy). Spawned from item 1 of `notes/work.md` ("Client
endpoints: one endpoint choice, no exclusive surface lease", formerly CEND-014
and CEND-015). It also resolves rejected candidate REJ-025 in
`notes/bugs-rejected-candidates.md`, which that item names as a consequence.
Two review rounds (`notes/spec-client-endpoint-choice-r1.md`,
`notes/spec-client-endpoint-choice-r2.md`) are folded in; section 9 records
what was rejected.

The client holds "which endpoint" in seven places and re-derives their
agreement every loop turn. Moving from one endpoint to another runs a
source-off-first, six-phase, rollback-capable transaction, because a
server-side surface is treated as an exclusive lease: at most one on, input
ordered across the switch. This spec replaces both. One enum owns the choice.
A server connection is only ever told "viewed" or "not viewed"; the client
decides which connection it draws and sends input to; and a connection that
should not be viewed is turned off by a rule derived each loop turn, not by a
rollback path that has to remember to do it.

Nothing in the originating item is deferred. What the item implies and the
survey turned up are bricks below: the presentation-effects fence (a protocol
message pair that exists only for the lease), the source command-lane
retirement, the Local "deferred" selection, and REJ-025.

## 1. Contracts inventoried

`docs/` does not exist in this repository. `reference/` holds only
`technical-implementation-spec.md`, which governs this document's shape and
changes nothing here. `AGENTS.md` is the written contract the work touches. It
contains no wording about handoffs, leases or source-off (checked by search), so
no `AGENTS.md` sentence goes stale.

Statements in `AGENTS.md` this spec relies on, none contradicted:

- "Presentation is per client": each server connection keeps its own surface
  size, outer focus, location and window title, and the PTY size rule, pane
  focus reports and host theme are decided from all views in one place each.
  "Connection or surface activation, outer focus gain, pane interaction, and
  endpoint commands count as activity". The server keeps exactly these rules.
  What changes is only how often a client may have more than one connection
  viewed at once (briefly, during a move: sections 3.5 and 4), which the rules
  already tolerate: they are written over "all the views".
- "The client applies its own config to everything it draws and interprets":
  the choice of endpoint is client state and is never written to disk or sent
  to a server. The selection stays in memory and every client starts on Local.
- "With machines configured, losing the local server does not end the client
  either: it keeps serving the remote machines and reconnects once the local
  server is restarted." Section 3.9 keeps this and states its new shape.
- "Hot paths multiply": the gate that classifies every inbound message and the
  per-turn reconcile (3.5) are on the client frame path. The gate is one role
  comparison and allocates nothing. The reconcile is O(connections) per turn
  and allocates nothing while no move is in progress and no connection is
  viewed and unwanted; the messages it sends to start, commit or release a view
  allocate their request ids and boot ids as every send does.
- "Wire encoding is shepr's own", "No wire compatibility obligations": the
  protocol change in 3.12 is free of fixtures; `skip_serializing_if`, `flatten`,
  `untagged` and tagged enums stay unused.
- "State is separated from runtime", "No god objects": the new choice type is
  plain data. Every `EndpointChoice` method is a pure transition or query: none
  takes the registry or sends anything, and all are unit-testable without a
  registry or transport. The I/O is in `endpoint/view.rs` (the four steps the
  reconcile and the end-to-end test share, 3.4) and the loop's `reconcile.rs`
  (presentation, notices and command lanes, 3.5).

The standing text the work changes, a brick in section 5: the stale
"handoff", "lease", "frozen" and "source-off" wording in comments and test
names in the client and server crates (inventory in 2.6). Notes documents
(`notes/work.md`, `notes/bugs-rejected-candidates.md`) and this spec are not
edited by the implementation.

## 2. Survey of the ground

Everything is in `crates/shepr-client/src/` unless a path says otherwise.

### 2.1 Who holds "which endpoint" today

1. `EndpointRegistry::active` (`endpoint/registry.rs`), with
   `EndpointConnection::surface_active` per connection, `set_active`,
   `set_surface_active`, `active_surface_available`, `send` (to the active one).
2. `Presentation` (`state.rs`): `Owned` / `Handoff(Box<PendingEndpointActivation>)`
   / `Unavailable`, held in `ClientState::presentation`.
3. `EndpointSelectionTracker` (`endpoint/selection.rs`): `selected`, `attempt`
   (with its restore point), `failed` (endpoint plus connection generation).
4. `ClientState::deferred_local` (`state.rs`): an explicit Local selection
   waiting for a Local connection with metadata.
5. `ClientLoop::scheduled_activation` (`lib.rs`): a queued
   `ClientLoopEvent::ActivateEndpoint` (a shell pick, a handoff successor, a
   ready deferred Local, an automatic activation), threaded as a `&mut Option`
   through `dispatch_client_shell_actions`, `finish_client_shell_input`,
   `begin_endpoint_activation` and every server-message arm.
6. `PendingEndpointActivation::successor` and `ActivationCompletion::
   RestoredSource { successor }` (`endpoint/activation/model.rs`).
7. `ClientShellState::active_endpoint_id` (`shell/state.rs`): the endpoint whose
   snapshot is projected. This one is not in the item's list. It is legitimate
   (the shell must know what it renders) but it is a seventh writer of the same
   fact. It is initialized to Local by the `ClientShellState` constructor and
   written afterwards only by `activate_endpoint_projection`. Section 3.1 makes
   it a derived projection target with a tested invariant.

### 2.2 The handoff transaction being deleted

`PendingEndpointActivation` (`endpoint/activation.rs`, plus
`activation/model.rs`, `activation/protocol.rs` and `activation_tests.rs`)
drives `ActivationPhase`: `ReleasingSource`, `ActivatingTarget`,
`ReleasingTargetForRollback`, `RestoringSource`, `SynchronizingPresentation`,
`AwaitingPresentationEffects`. Source-off is acknowledged before target-on
(except Local, which never waits on a remote); a failure rolls back through
target-off and source-on; a newer selection becomes a retained `successor`
applied after restoration; a disconnect at each phase has its own arm in
`endpoint_disconnected_at`; commit runs two coherent-surface rounds (one on
commit, one more under `AwaitingPresentationEffects` after a `PresentationSync`
token and the server's `PresentationReady`). Its callers are
`begin_endpoint_activation`, every call site of `complete_endpoint_activation`
and `rollback_endpoint_activation` (in `shell_runtime.rs` and `lib.rs`),
`handle_endpoint_disconnect`, `resize_handoff`, and the `handle_server_message`
arms for responses, snapshots, surfaces, patches and `PresentationReady`.

Every rollback path exists only because the source was turned off first.
REJ-025 (a rollback that ends `Unavailable` leaves surface-on sent and never
released) is a path that forgot to turn something off.

### 2.3 What the server does with "viewed" (kept as is)

`ClientShellSurfaceSet { active }` (`set_client_shell_surface_active` in
`crates/shepr-server/src/server/headless/surface_interest.rs`) is already
idempotent and last-write-wins per connection: every `active: true` raises the
projection floor (`projection_revision` plus one), clears the cached snapshot so
the next control snapshot carries the floor, requests a repaint, forgets the
presentation-effect memory, promotes the client to foreground and claims
workspace geometry unless another focused viewer of the same workspace already
owns it; `active: false` discards pending surface frames, releases held inputs,
drops geometry control and promotes the next client.
`surface_active` gates: surfaces and patches, pane input, endpoint commands (a
non-viewed connection answers `EndpointError::SurfaceInactive`), the PTY size
rule, pane focus reports, immediate PTY sources, host mouse capture and
keyboard modes, and the host theme. A non-viewed connection still receives
snapshots (metadata for the sidebar). The connection's hello carries the
initial `surface_active`. Frames and acknowledgements on one connection are
ordered: the render for an `on` can precede its acknowledgement, because
replies are released after the render the command needs. Pane focus is held
only by viewed clients whose outer terminal reported focus
(`outer_terminal_focus == Some(true)`); a connection that never sent
`ClientShellFocus` holds `None`, which counts as unfocused.

The server needs no change to support non-exclusive viewing, except the fence
message pair in 3.12.

### 2.4 Dependents outside the activation module

- `lib.rs`: `ClientLoop` fields `selection`, `scheduled_activation`,
  `next_surface_serial`; `ClientLoop::new` (its `selection` parameter); `run`
  (settle, automatic activation, deadline); `handle_activate_endpoint`;
  `handle_server_message` (PresentationGate inputs, the `PresentationReady` arm,
  the snapshot arm's `take_ready_local_activation`); `handle_resize`
  (`resize_handoff`); `handle_timer` (failure handling, handoff expiry,
  `handle_endpoint_disconnect`); `run_client_loop` (initial `Presentation`, the
  launch focus message sent with `registry.send`).
- `shell_runtime.rs`: `dispatch_client_shell_actions`,
  `finish_client_shell_input`, `install_client_shell_snapshot`,
  `active_endpoint_owns_presentation`, `automatic_activation`,
  `present_handoff_unavailable`, `begin/complete/rollback_endpoint_activation`,
  `handle_endpoint_disconnect`, `install_pending_activation`,
  `take_ready_local_activation`, `local_activation_*`, `resize_handoff`,
  `handoff_geometry`, `handoff_interrupted_notice`.
- `state.rs`: `Presentation`, `ClientState::{presentation, deferred_local,
  end_handoff, present_chrome, present_chrome_through_freeze, present_frame,
  present_surface_patch}`.
- `endpoint/message_policy.rs`: `PresentationGate` (six booleans) and its
  module tests.
- `endpoint/registry.rs`, `endpoint/commands.rs` (`retire_lane` is documented
  and used for source-off), `events.rs` (`ActivateEndpoint { force }`),
  `shell/navigation/endpoint_navigation.rs` (comments naming the runtime's
  handoff), `shell/tests/endpoint_requests.rs` (three tests constructing
  `Presentation` and `scheduled`), `limits.rs` (`ACTIVATION_TIMEOUT` and its
  doc).
- `endpoint/writer.rs` tests: six sites build `ClientMessage::PresentationSync`
  as a generic string-carrying message for the writer queue and ordering tests.
  The writer's production code does not use it.
- `crates/shepr-server/src/server/netside_tests.rs`: the only cross-crate user
  of the client handoff API (`PendingEndpointActivation`,
  `SurfaceActivationProgress`, `ActivationCompletion`), running two real
  `HeadlessServer`s. It is the end-to-end instrument and is rewritten, not
  dropped (brick 6).
- `crates/shepr-server`: `ClientMessage::PresentationSync` handling
  (`client_transport.rs`, `headless.rs`), `server/headless/tests/surface_delta.rs`
  (one `ClientShellPresentationSync` event), `surface_interest.rs` and its tests,
  `server/headless/tests/mod.rs` (`dispatch_lifecycle_messages`, which brick 6
  extends), wording in `app/api.rs` and `server/outbox.rs`, a comment in
  `Cargo.toml`.
- `crates/shepr-protocol`: `ClientMessage::PresentationSync(String)` in
  `input.rs`, `ServerMessage::PresentationReady(String)` in `message.rs`, and
  `wire_tests.rs`, which uses `PresentationSync` as its client-side
  string-payload vehicle in the framing tests (frame-size boundary, chunked
  reassembly, mixed stream, socketpair) and shares the `SYNC_ENVELOPE` constant
  with the `ServerMessage::Clipboard` split tests.

### 2.5 Behaviors that are load-bearing and must survive

The deletion drops no work only if each of these has a new home (section 3):

- Local is attached directly when healthy at launch; Local unreachable at launch
  or later keeps the client alive when machines are configured, and reconnects.
- A remote becoming viewed needs a coherent pair: a snapshot and a surface at
  the same projection revision, at or above the floor the acknowledgement
  carried, sized for the client's surface. Stale-generation, stale-boot and
  older-epoch frames are never evidence.
- A pick can carry a navigation target (workspace or pane) which must already be
  focused when the target is first drawn, with latest-pick-wins coalescing.
- A failed or timed-out switch is reported to the user and is not retried
  automatically on the same connection generation.
- Host mouse mode, keyboard report-all, title and clipboard writes belong to the
  endpoint on screen; a newly shown endpoint replays its modes and title.
- Pane input and endpoint commands reach only the endpoint on screen; queued
  commands of an endpoint that leaves the screen are cancelled, in-flight ones
  tombstoned.
- Chrome (machine list, notices, overlays) presents even when no endpoint owns
  the screen; pane cells then stay the last coherent ones and take no output.
- One surface geometry for every endpoint; a host resize reaches whichever
  connections are viewed, and a resize that leaves the geometry unchanged does
  not discard a move's evidence.
- The host theme reaches a newly viewed endpoint before its first presented
  frame. The host focus baseline no longer does: it is sent at commit, right
  after the first frame (3.8), a deliberate change listed in 3.13.
- When a transport failure and a move deadline fall due together, the failure
  is handled first, so the user is told the switch was interrupted rather than
  that it timed out.

### 2.6 Stale wording to fix in the same change

Search `crates/shepr-client` and `crates/shepr-server` for `handoff`,
`Handoff`, `lease` (only in the viewing sense; the data-directory lease in
`shepr-mux`, `shepr-server/bootstrap.rs` and `shepr-platform` is a different
thing and stays), `source-off`, `frozen target`, `presentation sync` and
`surface lease`. The `stdout-handoff-ok` marker (in `terminal_setup.rs` and
`shepr-platform/src/remote_bridge_io.rs`) is `brokkr.toml`'s textlint allow
marker and is not stale wording: rewording it breaks the gate, so it stays.
Known sites:
`state.rs`, `events.rs`, `shell_runtime.rs`, `lib.rs`,
`endpoint/commands.rs`, `shell/navigation/endpoint_navigation.rs`,
`limits.rs` (the `ACTIVATION_TIMEOUT` doc says the timeout avoids "leaving
input blocked"; a move never blocks input, so the renamed constant's doc says
"how long a move may stay Preparing before it fails and the shown endpoint
stays"),
`server/headless/surface_interest.rs` (the "surface lease" doc comment and the
comment block inside `set_client_shell_surface_active` about the client
dropping target effects "while its old source frame is frozen" and "a frozen
target activation"; reworded to say the client drops a target's effects until
it commits and then asks for `ReplayHostEffects`),
`server/outbox.rs` (the `forget_presentation` doc, "a surface activation or
presentation sync replays them", becomes "a surface activation or a host-effects
replay"),
`app/api.rs` (comment and the test name
`the_surface_lease_answered_by_the_loop_is_reported_as_misrouted`),
`server/headless/tests/surface_interest.rs`, `server/headless/tests/mod.rs`,
`shepr-server/Cargo.toml`.

## 3. The target

### 3.1 Principles and invariants

- **Viewing is a per-connection fact told to the server and nothing more.** The
  client keeps one boolean per connection, `viewed`: has this connection been
  told it is viewed. Turning a connection on may be repeated at any time
  (idempotent: the server answers each with a fresh floor). Turning it off is
  never awaited.
- **The client chooses what it draws and where input goes.** That is
  `EndpointChoice::shown()`, nothing else. No server acknowledgement is
  needed to stop drawing or typing into an endpoint.
- **A viewed connection nobody wants is turned off by rule.** Every loop turn,
  one pass over the connections sends focus-loss and a view-off request to every
  connection with `viewed == true` that is neither the shown endpoint nor the
  target of the move being prepared. No failure path calls turn-off; failure
  paths only change the choice. The one connection the pass may have to skip is
  one whose boot id the client does not know yet (the request names it, 3.4);
  it is skipped, the rest of the pass continues, and the next turn retries it.
  Apart from that bounded wait, no connection stays viewed and unwanted past the
  turn in which it became unwanted, which closes REJ-025's class: there is no
  code that could skip the step.
- **Moving is not freezing.** While the client prepares another endpoint, the
  endpoint on screen stays live: its frames present, its input and commands
  work, its snapshots project. Pane frames are held back only when nothing is
  shown (the shown endpoint's connection was lost, or Local was never
  reachable). That the source keeps taking input during a move is a deliberate
  behavior change (3.13).
- **Shown implies viewed**: if `shown() == Some(e)` then `viewed(e)` is true
  (or `e` has no connection, in which case the loss is being handled). Viewing
  lives in the registry, so a loop-level test pins it across every transition,
  including send failures while starting and committing.
- **The shell projects what is shown.** `ClientShellState::active_endpoint_id`
  is initialized to Local by the `ClientShellState` constructor and written
  afterwards only by `activate_endpoint_projection`, called from commit. It
  equals `shown()` whenever something is shown, and keeps the last shown
  endpoint while nothing is (the retained coherent cells and snapshot belong to
  it). A test pins it. Nothing else decides which endpoint "is active".
- **Evidence for a switch lives outside the shell** (3.3), exactly as it does
  today, so this item does not touch the shell's surface baseline, which item 2
  owns.

### 3.2 The choice type (`endpoint/choice.rs`, replacing `selection.rs`)

```rust
/// Which endpoint the client shows, and the move toward the one it wants.
/// The only owner of that fact: nothing else records a selection.
/// Plain data: no method takes the registry or sends anything.
pub enum EndpointChoice {
    /// `endpoint` is selected and on screen.
    Showing(ClientEndpointId),
    /// `to` is selected and not on screen yet.
    Moving(Move),
}

pub struct Move {
    /// On screen (live) until the move commits. `None` while nothing is: the
    /// shown endpoint's connection was lost, or Local was unreachable at launch.
    from: Option<ClientEndpointId>,
    to: ClientEndpointId,
    stage: MoveStage,
}

pub enum MoveStage {
    /// Nothing sent yet: `to` has no connection with metadata for its current
    /// generation. Replaces `deferred_local` and automatic activation.
    /// `focus` is the navigation the user asked for, handed to the focus lane
    /// when preparing starts.
    Waiting { focus: Option<ClientEndpointFocusTarget> },
    /// `to` has been turned on and the client is collecting a coherent pair.
    Preparing(Box<Preparing>),
    /// Only with `from == None`: preparing `to` failed on connection generation
    /// `generation`. Not retried until `to` has a connection of another
    /// generation with metadata, or the user selects again. Carries no focus: a
    /// restart on a new generation shows the endpoint without navigation, and
    /// an explicit pick supplies its own. (With a shown `from`, a failure
    /// returns to `Showing(from)`, so no failure memory is needed: automatic
    /// retry only exists when selected differs from shown.)
    Failed { generation: u64 },
}

/// How an inbound message from one connection relates to the choice.
/// `Target` only while the move to that endpoint is `Preparing`: a `Waiting` or
/// `Failed` target has nothing to collect evidence into, so it is `Other`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ConnectionRole { Shown, Target, Other }

/// What a shell pick did to the choice.
pub enum Selection {
    /// Already shown, nothing to move, nothing to navigate.
    Unchanged,
    /// Already shown, and the pick carried navigation: the caller applies it
    /// through the ordinary endpoint-command path (`focus_endpoint_target`).
    FocusShown(ClientEndpointFocusTarget),
    /// The choice is now (or still) a move; `reconcile` drives it.
    Moving,
}

/// What losing a connection did to the choice.
pub enum Lost { Shown, Target, Unrelated }

/// A move that may start: `Waiting`, or `Failed` (then `failed_generation` is
/// set and a start needs a connection of another generation).
pub struct PendingStart<'a> {
    pub to: &'a ClientEndpointId,
    pub from: Option<&'a ClientEndpointId>,
    pub failed_generation: Option<u64>,
}

pub struct FailedMove { pub to: ClientEndpointId, pub returned_to: Option<ClientEndpointId> }
pub struct Committed { pub previous: Option<ClientEndpointId>, pub shown: ClientEndpointId }
```

State diagram (every arrow is a pure method; none does I/O):

```
launch, Local connected          -> Showing(Local)
launch, Local not connected      -> Moving{from None, to Local, Waiting{None}}

Showing(e)  --select(e, None)--> Showing(e)                 Unchanged
Showing(e)  --select(e, t)-----> Showing(e)                 FocusShown(t)
Showing(e)  --select(x, t)-----> Moving{e -> x, Waiting{t}}
Moving{f->x, Waiting} --select(x, t)--> Moving{f->x, Waiting{t}}
Moving{f->x, Preparing} --select(x, t)--> same, the lane's desired target becomes t
Moving{None->x, Failed} --select(x, t)--> Moving{None->x, Waiting{t}}  (explicit re-proof)
Moving{f->x} --select(f, t)----> Showing(f)                 Unchanged or FocusShown(t)
                                 (the move is cancelled; x is turned off by rule)
Moving{f->x} --select(y, t)----> Moving{f -> y, Waiting{t}} (x is turned off by rule)

Waiting --begin_preparing------> Preparing (the Waiting focus goes to the lane)
Waiting --abandon--------------> Showing(f)   (only with f; reconcile adds the notice)
Failed(g) --begin_preparing----> Preparing    (reconcile calls it only for a
                                               connection of generation != g)
Preparing --commit-------------> Showing(x)
Preparing --fail_move----------> Showing(f) if f exists, else Failed{lease generation}

connection_lost(c):
  Showing(c)                    -> Moving{None -> c, Waiting{None}}  Lost::Shown
  Moving{c -> x}                -> Moving{None -> x, same stage}     Lost::Shown
  Moving{f -> c} (any stage)    -> Showing(f) if f exists, else
                                   Moving{None -> c, Waiting{None}}  Lost::Target
  otherwise                     -> unchanged                          Lost::Unrelated
```

`MoveStage::Preparing` after `Lost::Shown` keeps preparing (a healthy target
survives the loss of the source; the old `losing_local_during_handoff_does_not_revoke_the_healthy_target`
property).

Methods, all `pub` because the server crate's end-to-end test drives them (the
same arrangement `PendingEndpointActivation` has today; no test feature):

```rust
impl EndpointChoice {
    pub fn showing(endpoint: ClientEndpointId) -> Self;
    pub fn waiting_for(endpoint: ClientEndpointId) -> Self;       // launch, nothing shown
    pub fn shown(&self) -> Option<&ClientEndpointId>;
    pub fn frames_frozen(&self) -> bool;                           // shown().is_none()
    pub fn role(&self, endpoint: &ClientEndpointId) -> ConnectionRole;
    pub fn wants_view(&self, endpoint: &ClientEndpointId) -> bool; // shown, or the Preparing target
    pub fn select(&mut self, endpoint: ClientEndpointId,
                  focus: Option<ClientEndpointFocusTarget>) -> Selection;
    pub fn connection_lost(&mut self, endpoint: &ClientEndpointId) -> Lost;
    pub fn pending_start(&self) -> Option<PendingStart<'_>>;       // Waiting or Failed
    /// Waiting with a shown `from`: back to `Showing(from)`; returns `to`.
    /// Any other state: unchanged, `None`.
    pub fn abandon(&mut self) -> Option<ClientEndpointId>;
    /// Waiting or Failed: becomes Preparing, taking the Waiting focus.
    /// Any other state: unchanged.
    pub fn begin_preparing(&mut self, lease: ViewLease, view_request: RequestId,
                           geometry: TerminalGeometry, now: Instant);
    pub fn preparing(&self) -> Option<&Preparing>;
    pub fn preparing_mut(&mut self) -> Option<&mut Preparing>;
    pub fn deadline(&self) -> Option<Instant>;
    /// Preparing only, else `None` and unchanged (see 3.9).
    pub fn fail_move(&mut self) -> Option<FailedMove>;
    /// Preparing only: becomes `Showing(to)`; else `None` and unchanged.
    pub fn commit(&mut self) -> Option<Committed>;
}
```

A pick for an endpoint the shell does not list never arises (the tracker's
`machines` list and its "unknown machines cannot be selected" check are dropped
because the machine set is fixed at launch and the shell only emits picks from
it). A pick for a remote the shell does not report online is refused by the
shell (`activate_endpoint`, `focus_or_activate`: "X is not ready"). The shell's
online check (`endpoint_is_online`) accepts a snapshot of any generation, so a
remote that reconnected and has not yet sent its first snapshot on the new
connection passes it. Such a pick, a Local pick without metadata, and any pick
while nothing is shown, become `Waiting` (3.5 step 2). A remote `Waiting` while
something is shown ends either when its first snapshot arrives (a server sends
it on connect) or when the health first-snapshot deadline expires the
connection, which is `Lost::Target` and returns to `Showing(from)` with the
interrupted notice. The user is told it is waiting (3.11).

### 3.3 Preparing, evidence and the focus lane (`endpoint/choice/preparing.rs`, `focus_lane.rs`)

`Preparing` is the surviving core of `ActivationPhase::ActivatingTarget`, with
everything about the source removed:

```rust
pub struct Preparing {
    lease: ViewLease,            // { endpoint_id, generation, boot_id: BootId, minimum_revision }
    view_request: RequestId,     // the on request; its acknowledgement carries the floor
    floor: Option<u64>,          // projection revision of that acknowledgement
    geometry: TerminalGeometry,  // the geometry last sent to the target
    focus_lane: FocusLane,       // desired navigation, one coalescing request in flight
    evidence: ViewEvidence,      // exactly today's ActivationEvidence
    rejection: Option<String>,   // set by a rejected response; reconcile fails the move
    deadline: Instant,           // ENDPOINT_MOVE_TIMEOUT after start (rename of ACTIVATION_TIMEOUT)
}
```

All `Preparing` methods are pure, like the choice's.

- `ViewLease` is today's `EndpointLease` without the optional boot id: a target
  is only prepared when it is connected and has a snapshot for the generation,
  so no placeholder (generation 0, boot `None`) lease exists. The
  `disconnected_endpoint_lease` function and its tests go.
- `ViewEvidence` is `ActivationEvidence` unchanged in behavior: `record_snapshot`
  (monotonic, keeps focused workspace and pane ids), `record_surface` (replaces
  by `(projection_revision, surface_revision)` order), `record_patch` (applies
  through `surface_reuse::apply_patch_to_surface`; a failed apply drops the
  surface so only a new full surface can satisfy the pair), `invalidate_surface`,
  `coherent_surface(minimum_revision, size)`.
- `ready(&self, size: ClientSurfaceSize) -> Option<&PaneSurfaceFrame>`: returns
  the evidence surface when `floor` is known, `rejection` is `None`, the focus
  lane is settled (nothing in flight, last response matched the desired target,
  or no navigation was asked) and `evidence.coherent_surface(floor, size)`
  matches the requested navigation (today's `target_matches`). The size is
  passed in at call time (`shell.surface_size(cols, rows)`), not stored, so a
  resize cannot leave a stale copy.
- `accepts_response(endpoint, generation, boot_id, request_id)`: the lease
  matches and `request_id` is `view_request` or the focus lane's in-flight id.
- `receive_response(result) -> PrepareProgress` (`Pending` / `Rejected` /
  `Stale`): a view acknowledgement must be `ClientShellSurfaceSet { active: true,
  projection_revision }` (anything else stores the rejection "surface activation
  returned an invalid acknowledgement") and sets `floor`; a focus response goes
  to the lane (a mismatch stores the lane's rejection). An `Err` result stores
  `error.to_string()`. `Rejected` only reports that `rejection` is now set; the
  caller does nothing with it, reconcile step 1 fails the move. This keeps every
  failure transition in reconcile.
- `receive_snapshot`, `receive_surface`, `receive_patch`: boot and generation
  matched against the lease; snapshots below `minimum_revision` are `Stale`;
  a surface whose size differs from the client's current surface size is kept
  out of the evidence (today's `surface_matches_geometry` check, now against the
  size passed by the caller). No method ever returns "ready": the loop asks
  `ready()` in reconcile. This removes the five scattered "if Ready then
  complete" call sites.
- `update_geometry(geometry) -> bool`: called on every host resize and every
  shell `outcome.resize`. When `geometry` equals the recorded one it changes
  nothing and returns false (the server ignores an unchanged
  `ClientShellResize` and sends no new surface, so dropping the evidence would
  stall the move until its deadline). Otherwise it records the geometry, calls
  `evidence.invalidate_surface()` and returns true. It is not called for focus
  or theme changes: later changes to the target's cells reach the evidence as
  patches.
- `retarget_focus(focus)` sets the lane's desired target, and
  `focus_request(&mut self) -> Option<ClientMessage>` builds the next
  `PaneFocus`/`WorkspaceFocus` endpoint request (with the lease's boot id) when
  the lane has a desired target and nothing in flight, marking it in flight.
  Both are the existing `retarget`, `send_latest_focus`, `next_focus_request_id`
  and the focus arm of `receive_response_for_boot_at`, moved into `FocusLane`
  with the send taken out: at most one request in flight, newer picks only
  replace the desired target, the latest desired target is requested when the
  in-flight response resolves, a response must satisfy `focus_result_matches`,
  request ids are `client-shell-focus:{serial}:{n}`. The send is
  `view::send_focus` (3.4); a failed send is a connection failure (3.9), so no
  retarget error has to be carried anywhere.

### 3.4 Viewing messages and the ledger (`endpoint/view.rs`, replacing `activation/protocol.rs`)

The registry's per-connection `surface_active` becomes `viewed` (rename of the
field, of `set_surface_active` to `set_viewed`, and a new `viewed(&self, id) ->
bool`). `EndpointRegistry::active` and everything that reads it are removed
(`active_id`, `set_active`, `active_surface_available`, `send`).
`insert(.., surface_active, ..)` keeps its position and meaning for the hello:
Local at launch is inserted `viewed = true`, every supervisor-connected
connection `false`.

New registry methods:

```rust
/// Sends to every viewed connection (a resize, a theme update). Sends while
/// iterating; a failed send's endpoint id is collected (the only allocation,
/// on the failure path) and recorded through `record_failure` after the
/// traversal, because `record_failure` removes from the map being iterated.
pub(crate) fn send_viewed(&mut self, message: &ClientMessage);

/// One pass over the connections: every viewed connection for which `wanted`
/// is false and `boot_id_of` knows a boot id is sent `ClientShellFocus {
/// focused: false }` and then the view-off request (`ClientShellSurfaceSet {
/// active: false }`, request id `client-shell-view:{serial}:off` from
/// `*serial`, which advances once per request), and marked `viewed = false`
/// before the sends. A connection without a known boot id is skipped and stays
/// viewed; the pass continues. Failed sends are recorded after the traversal
/// as in `send_viewed`. Returns how many connections were released. O(n) in
/// connections; allocates nothing when no connection is viewed and unwanted.
pub(crate) fn release_unwanted_views(
    &mut self,
    wanted: impl Fn(&ClientEndpointId) -> bool,
    boot_id_of: impl Fn(&ClientEndpointId) -> Option<&BootId>,
    serial: &mut u64,
) -> usize;
```

`endpoint/view.rs` holds the message sequences, the request-id format
`client-shell-view:{serial}:on` / `:off` (the focus lane keeps its own ids), and
the four I/O steps of the reconcile. The steps are `pub` so the reconcile and
the server crate's end-to-end test (brick 6) run the same code; the reconcile
adds only presentation, notices and command lanes around them.

```rust
pub struct HostBaseline<'a> {
    pub geometry: TerminalGeometry,                 // the one surface geometry (handoff_geometry today)
    pub host_focused: bool,                         // shell.host_focus_baseline(), sent at commit
    pub theme: &'a [ClientHostThemeUpdate],         // ClientState::host_theme_updates
}

pub enum StartOutcome {
    /// No move may start (Showing, Preparing, or Failed without a new generation).
    Idle,
    /// The move stays Waiting: `to` has no connection and is Local or nothing is
    /// shown, or `to` has no metadata for its current generation.
    Waiting,
    /// A machine without a connection while something is shown: the choice is
    /// back on `from`; the caller shows "{label} is not ready".
    Abandoned(ClientEndpointId),
    /// `begin_preparing` ran, then `turn_on`. A failed send is already recorded
    /// as a connection failure and is handled as `Lost::Target` next turn.
    Started,
}

/// Reconcile step 2. Reads `choice.pending_start()`, the registry's connection
/// and generation for `to`, and `shell.endpoint_snapshot_identity(to,
/// generation)`. On a start it builds the lease from that identity and the
/// request id from `*serial`, calls `choice.begin_preparing(..)` first, and only
/// then `turn_on`, so every send happens with the Preparing installed (the
/// old `start_target` rule).
pub fn start_move(choice: &mut EndpointChoice, endpoints: &mut EndpointRegistry,
                  shell: &ClientShellState, baseline: &HostBaseline<'_>,
                  serial: &mut u64, now: Instant) -> StartOutcome;

/// Reconcile step 3. Sends `preparing.focus_request()` to the target, if any.
pub fn send_focus(choice: &mut EndpointChoice, endpoints: &mut EndpointRegistry);

/// Reconcile step 4 (3.8). `Ok(None)`: nothing ready. `Err(reason)`: a commit
/// precondition failed and nothing was changed.
pub fn commit_move(choice: &mut EndpointChoice, endpoints: &mut EndpointRegistry,
                   shell: &mut ClientShellState, host_focused: bool,
                   surface: ClientSurfaceSize) -> Result<Option<Committed>, String>;

/// Reconcile step 5: `endpoints.release_unwanted_views(|id| choice.wants_view(id),
/// |id| shell.endpoint_boot_id(id), serial)`.
pub fn release_unwanted(choice: &EndpointChoice, endpoints: &mut EndpointRegistry,
                        shell: &ClientShellState, serial: &mut u64) -> usize;

/// resize, then every recorded theme update, then the on request (viewed = true
/// is recorded first). No focus message: focus follows the commit (3.8). Always
/// sends a fresh request, also to a connection already viewed: that is what
/// makes turning on idempotent and gives the new epoch its own floor.
pub(crate) fn turn_on(endpoints: &mut EndpointRegistry, lease: &ViewLease,
                      request: &RequestId, baseline: &HostBaseline<'_>);
```

The off acknowledgement is not awaited and is matched by nothing: it arrives as
an untracked `ClientShellEndpointResponse` and the gate drops it (3.6). A failed
send of any of these messages is a connection failure, which the
`take_failures` path turns into `connection_lost` at the start of the next
reconcile (3.5 step 0); there is no "did the peer observe a partial write" state
left to model, because a connection that failed is gone and its server drops
the client's view with the connection.

### 3.5 Reconcile: the loop's one derivation (`reconcile.rs`)

`ClientLoop::reconcile(&mut self, now: Instant) -> Result<(), ClientError>`
replaces the block at the top of `run` (`selection.settle`,
`automatic_activation`, `scheduled_activation`), the
`ClientLoopEvent::ActivateEndpoint` event (deleted, with `force`), and the
failure handling and handoff expiry in `handle_timer` (which keeps
`tick_health`, command expiry and the shell ticks). `run` calls it once per
iteration before it waits and propagates its error, and tests call it directly
after `handle_event`. In this order:

0. **Failures.** For each `endpoints.take_failures()` entry (skipping one whose
   endpoint has a live connection of another generation, as `handle_timer` does
   today), log it, return `ClientError::ConnectionLost` when
   `local_failure_policy.ends_client_for` the endpoint, else run
   `endpoint_lost` (3.9). Failures come first so that a target lost at its
   deadline is reported as an interrupted switch, not a timeout (the property
   `target_loss_at_activation_deadline_restores_source_before_timeout` pinned).
   A failure queued later in this turn makes the registry's service deadline
   `now`, so the next turn handles it at once.
1. **Fail.** If a `Preparing` has a `rejection`, or `choice.deadline()` has
   passed, `fail_move` with the rejection as notice, or for a deadline
   `"{label} did not produce a coherent surface in time"` (3.9).
2. **Start.** `view::start_move`. On `Abandoned(to)`, the notice
   `"{label} is not ready"` through `present_notice`.
3. **Focus.** `view::send_focus`.
4. **Commit.** `view::commit_move`. On `Ok(Some(committed))` the glue in 3.8;
   on `Err(reason)`, `fail_move` with the reason as notice.
5. **Views.** `view::release_unwanted`.

Cost: with nothing moving and nothing to release, a turn is an empty failure
list, a stage match, a `Preparing` presence check and one pass over at most
machines-plus-one connections. It allocates nothing. The per-message gate
(3.6) is one `role()` comparison.

During `Preparing`, both servers render surfaces for this client for at most
`ENDPOINT_MOVE_TIMEOUT`. That duplicate stream is the price of not freezing the
source and is accepted: it ends by commit or by step 5.

### 3.6 The message gate (`endpoint/message_policy.rs`)

`PresentationGate` loses five of six booleans. It takes the sender's role, whether
the message answers a move request, and whether it answers an ordinary command:

```rust
PresentationGate::new(role: ConnectionRole, move_response: bool, command_response: bool)
```

`move_response` is `choice.preparing().is_some_and(|p| p.accepts_response(..))`
for a `ClientShellEndpointResponse`, else false. `command_response` is
`endpoint_commands.response_kind(..) != CommandResponseKind::Untracked` (the
in-flight command or a tombstoned one), so a late response to a retired or
timed-out command of the shown endpoint still reaches `receive_response` and
consumes its tombstone, as today. `decide` returns the same
`PresentationDecision { Apply, Drop, Buffer }`:

| message | Shown | Target | Other |
|---|---|---|---|
| `EndpointWelcome`, `EndpointSnapshot`, `HealthPong`, `ServerShutdown` | Apply | Apply | Apply |
| `MouseCapture`, `ClientShellKeyboardReportAll`, `WindowTitle`, `Clipboard` | Apply | Drop | Drop |
| `PaneSurface`, decoded patch | Apply | Buffer | Drop |
| `ClientShellEndpointResponse` | `command_response` ? Apply : Drop | `move_response` ? Buffer : Drop (a command of a Target cannot exist: it is not shown) | Drop |
| anything else (`ClientShellError`, ...) | Apply | Drop | Drop |

A non-shown connection has no in-flight command: the lane is retired at commit
(3.7) and removed on loss (3.9). Tombstones of a retired lane are dropped with
the Other row and age out of the bounded tombstone queue, as they do today.

There is no `frozen` input: frames cannot be Applied for a connection that is
not shown, and when nothing is shown there is no Shown connection.
`PresentationReady` no longer exists (3.12). `Target` exists only while
`Preparing` (3.2), so a Buffer always has evidence to go into. The snapshot arm
of `handle_server_message` branches on role itself: Shown projects
(`set_endpoint_snapshot_for_generation`), Target caches and records evidence,
Other caches.

Online status is owned by the supervisor: the `Connected` arm sets a connection
Online when its transport connects, as it does today. The snapshot arm no
longer withholds Online for a pending projection (today's
`waits_for_selected_surface`, which only skipped a re-set of a status the
`Connected` arm had already set), and commit sets Online again before its
projection check (3.8), as `complete_at` does today.

### 3.7 Roles, input and commands

- **Pane input and endpoint commands go to `choice.shown()` only**, and only
  while the shown connection exists and is viewed. The helper replacing
  `active_endpoint_owns_presentation(presentation, endpoints)` is
  `input_endpoint(choice, endpoints) -> Option<&ClientEndpointId>`. The shell
  `ClientShellAction::Endpoint` arm enqueues on that endpoint's lane; the
  command lane is drained for it. During a move this is the source, which stays
  live (3.13).
- `ClientShellFocus` requests from the shell go to the shown connection only
  (a failed send is recorded against it, as now). The target's focus baseline is
  sent at commit (3.8), the source's focus-loss is sent by the release (3.4).
- `ClientShellResize` and `ClientShellHostTheme` requests go through
  `send_viewed`, so a host resize or theme change reaches the shown endpoint and
  a target being prepared (`update_resize_at`, `update_host_theme_at`, the
  per-phase destination `match` blocks, and `resize_handoff` are deleted). A
  resize also calls `preparing.update_geometry(geometry)`, which drops the
  evidence surface only when the geometry changed (3.3). Host theme updates are
  recorded in `ClientState::host_theme_updates` as today and replayed by
  `turn_on`.
- `Detach` goes to the shown endpoint; `EndpointRegistry::drop` still sends the
  courtesy `Detach` to every other connection.
- At commit the previous endpoint's command lane is retired
  (`EndpointCommands::retire_lane(from)`, whose doc comment now says "when an
  endpoint stops being shown"): queued commands are cancelled unsent, the
  in-flight one is tombstoned, and the shell requests it returns are cancelled.
  This is the only point where it is needed; the old call at handoff start
  (`install_pending_activation`) goes.

### 3.8 Commit

`view::commit_move(choice, endpoints, shell, host_focused, surface_size)`, in
this order:

1. Checks, with nothing mutated: a `Preparing` exists and
   `preparing.ready(surface_size)` returns a surface (else `Ok(None)`);
   `shell.endpoint_snapshot_matches(lease.endpoint_id, lease.generation,
   lease.boot_id, surface.projection_revision)` holds (else `Err(reason)`).
2. `shell.set_endpoint_status(target, Online)` (before the projection check, as
   `complete_with_host_theme_at` does today; the connection is live, so Online
   is true whatever happens next). Then `shell.endpoint_projection_available
   (target)` and `shell.activate_endpoint_projection(target)` must hold, else
   `Err(reason)`. Both only need Online plus a snapshot, which step 1
   established, so this cannot fail in practice; if it does, the only mutation
   left behind is a correct Online status.
3. `shell.set_pane_surface(surface.clone())`; `choice.commit()`: the choice
   becomes `Showing(target)`. From here the switch is complete.
4. `endpoints.send_to(target, ClientShellFocus { focused: host_focused })` then
   `ClientMessage::ReplayHostEffects` (3.12). A failed send does not undo the
   switch: it is a connection failure, handled next turn as `Lost::Shown`
   (nothing shown, reconnect), like any loss of the shown endpoint.
5. Return `Ok(Some(Committed { previous, shown }))`.

The reconcile then (step 4 glue) clears the previous endpoint's host effects
(`clear_endpoint_host_effects`, which a lost shown endpoint already uses, and
whose error propagates out of `reconcile`), retires the previous command lane
(3.7), runs `send_next` for the new shown endpoint, requests a full repaint and
presents the composed frame. Step 5 then releases the previous endpoint in the
same turn.

The second coherent-surface round and the effects fence of today are gone.
Pane input is no longer withheld between the pair being on screen and the
target's mode replay arriving; that window is one round trip, input is already
parsed into semantic events before it reaches a server, and a host mode that
lags (mouse capture, report-all) degrades one gesture, not correctness. This and
the other user-visible changes are listed in 3.13.

### 3.9 Failure, timeout, loss

`fail_move(&mut self) -> Option<FailedMove>` is called only by the reconcile
(steps 1 and 4). It acts only on a `Preparing` (which always holds a lease, so
`Failed { generation }` always has a generation) and returns `FailedMove { to,
returned_to }`, setting the choice to `Showing(from)` when `from` exists, else
`Moving{None -> to, Failed{generation: lease.generation}}`. The reconcile shows
the reason as an endpoint notice (`shell.receive_endpoint_unavailable`) with the
endpoint label prefixed as `begin_endpoint_activation`'s preflight does today,
and presents chrome. The target needs no cleanup call: it is no longer wanted,
so step 5 releases it.

A send failure is never a move failure. Whether it happens while starting
(`turn_on`), while sending focus, while committing or while releasing, the
registry records it as a connection failure and the next turn's step 0 runs
`endpoint_lost`. While preparing with nothing shown that is `Lost::Target`, the
move returns to `Waiting` with the connection gone, and it restarts only when
the endpoint reconnects, necessarily as a new generation. So the rule "not
retried on the same generation" holds for send failures without a
`Failed` state.

`connection_lost` (3.2) is called from `endpoint_lost(...)` in `reconcile.rs`,
which replaces `handle_endpoint_disconnect` and does in this order what the old
function did minus its handoff arm: `supervisors.disconnected`, the machine
diagnostic, `choice.connection_lost`, `endpoint_commands.disconnect` plus
cancelling the shell requests it returns, `shell.mark_endpoint_disconnected`,
then by outcome:

- `Lost::Shown`: the notice `"{label} {disconnect notice}"` and chrome,
  `clear_endpoint_host_effects`. Nothing is shown now, so pane frames are held
  back and pane input and commands close, exactly the old `Unavailable`
  presentation.
- `Lost::Target`: the notice `"machine switch interrupted: {label} {notice}"`
  (the existing `handoff_interrupted_notice`, renamed `move_interrupted_notice`).
  The shown endpoint was never touched, so there is nothing to restore.
- `Lost::Unrelated`: chrome only (a machine going offline).

`present_handoff_unavailable` becomes `present_notice(state, message)`: it no
longer decides a presentation state (the choice already says nothing is shown);
it shows the notice and presents chrome. Its call sites in the supervisor Status
and Connected arms (Attention for the endpoint the shell projects) keep calling
it.

A status of Attention for the projected endpoint with a still-live connection
cannot arise (a failed attempt only follows a lost connection); the call stays a
notice, not a state change.

### 3.10 Launch

`run_client_loop` builds `EndpointChoice::showing(Local)` when the initial Local
stream connected (registry inserted `viewed = true`, the launch focus message
sent with `send_to(Local, ..)`), else `EndpointChoice::waiting_for(Local)`, with
the same chrome and status handling it has today. `ClientState::presentation`
and `deferred_local` are replaced by `ClientState::choice: EndpointChoice`;
the choice lives in `ClientState`, so `ClientLoop::new` keeps its shape minus
the `selection` parameter. `ClientLoop` loses `selection`,
`scheduled_activation` and `next_surface_serial`, and gains `next_view_serial`
(monotonic, only to make request ids unique, passed as `&mut u64` to the view
steps). `next_timer_deadline` replaces the handoff deadline entry with
`state.choice.deadline()`.

### 3.11 Shell actions and navigation

`ClientShellAction::ActivateEndpoint { endpoint_id, target }` is unchanged in
the shell. `dispatch_client_shell_actions` takes `&mut EndpointChoice` instead
of `&Presentation` and `&mut Option<ClientLoopEvent>`, and handles the action
immediately, without an event round trip: `choice.select(...)`;
`Selection::FocusShown(t)` pushes `shell.focus_endpoint_target(t)`'s actions onto
the work list being dispatched (the old `already_active` branch, which called
`dispatch_client_shell_actions` recursively) and sets repaint;
`Selection::Moving` whose target has no connection with metadata for its
current generation emits the waiting notice. That notice is today's
`local_activation_unavailable_notice`, generalized to take the endpoint label
(`"{label} is connecting; selection will resume when it is ready"`,
`"... is reconnecting; ..."`, `"{label} needs attention"`, `"{label} is waiting
for its workspace snapshot; selection will resume when it is ready"`), so a
remote pick that has to wait is no longer silent. `ClientState::present_chrome`
and `present_chrome_through_freeze` merge into `present_chrome` (chrome always
writes); `present_frame` and `present_surface_patch` keep their gate, now
`choice.frames_frozen()`.

`handle_endpoint_machine_click`'s comment ("the runtime suppresses this while
the endpoint owns the presentation, and uses it to cancel a handoff") is
reworded to "selecting the shown endpoint cancels a move in progress".

### 3.12 Protocol and server changes

The fence exists only because target effects were dropped while the target was
frozen, and a lease needed a barrier before input resumed. With roles, target
effects are dropped until commit and replayed after it:

- `ClientMessage::PresentationSync(String)` becomes the unit variant
  `ClientMessage::ReplayHostEffects`: "this connection's effects are now
  presented; send its current mouse capture, keyboard mode and title". Doc
  comment says so.
- `ServerMessage::PresentationReady(String)` is deleted.
- Server: `ServerEvent::ClientShellPresentationSync { client_id, token }` becomes
  `ClientShellReplayHostEffects { client_id }` (`client_transport.rs`); its
  handler in `headless.rs` is today's body (`forget_presentation`,
  `stream_host_mouse_capture_mode`, `stream_shell_keyboard_mode`,
  `sync_window_title`) without the final `send_to_client(PresentationReady)`.
  A non-viewed connection ignores it, as now.
- `crates/shepr-protocol/src/wire_tests.rs`: every client-side
  `PresentationSync(..)` vehicle (the frame-size boundary tests, the chunked
  reassembly, the mixed stream and the socketpair tests) moves to a
  `ClientShellPaneInput` whose single event is
  `ClientPaneInputEvent::Paste(text)`, built by one test helper `paste(text)`.
  `SYNC_ENVELOPE` splits in two, because the `ServerMessage::Clipboard` split
  and reassembly tests use it too and need Clipboard's own envelope:
  `CLIPBOARD_ENVELOPE = 4` (unchanged value, comment reworded to name only
  Clipboard) and `paste_envelope()`, computed from `codec::encoded_len(&paste(""))`
  plus the two bytes the payload length varint grows by for strings of these
  sizes (one byte at length 0, three below 2 MiB), with that reasoning in its
  comment, so no pane-id string is counted by hand. A new round-trip test
  `replay_host_effects_roundtrips` covers `ReplayHostEffects`.
- `crates/shepr-client/src/endpoint/writer.rs` tests: the six
  `PresentationSync(..)` payloads move to the same `Paste` vehicle (a local
  `paste(text)` helper); the assertions on ordering and queued text are
  unchanged.
- The server's `set_client_shell_surface_active` function and the
  `ClientShellSurfaceSet` / `surface_active` names are kept (renaming 30 test
  sites buys nothing); their doc comments say "viewed" instead of "lease" (2.6).
  `EndpointError::SurfaceInactive` keeps meaning "this connection is not viewed".

### 3.13 User-visible behavior changes

Each of these is a decision, not a surprise:

1. **No post-commit input fence.** Pane input is not withheld while the target's
   mode replay is in flight (3.8).
2. **The source stays live and takes input during a move.** Keys and commands
   typed after picking machine X still go to the endpoint on screen until X
   commits: normally one round trip plus a render, at most
   `ENDPOINT_MOVE_TIMEOUT` (5 s). Today pane input is withheld from the moment
   a switch starts. No pending-move indication is added. This is accepted
   because what is drawn is always what receives input; a user typing during
   the move sees the keystrokes land on the machine that is on screen. If the
   owner wants the old behavior, the alternatives are to withhold pane input
   (not frames) while `Moving` with a shown `from`, or to mark the pending
   target in the machine list; either is a small addition to 3.7 or 3.11.
3. **Latest pick wins over a pick that has to wait.** Selecting Local while it
   has no metadata (or a remote waiting for its first snapshot) during a move
   to another endpoint abandons that move; the waiting endpoint becomes the
   target and the other is released. Today the in-flight remote handoff
   continues and the deferred Local applies afterwards (the old
   `local_selection_waits_for_fresh_metadata_without_abandoning_remote`).
4. **The target learns of host focus at commit.** Today the target gets
   `ClientShellFocus` with the on request; here it holds outer focus `None`
   (unfocused) until commit. While preparing, the target server's geometry
   fallback (`workspace_geometry_source`, the geometry-controller fallback in
   `client_views.rs`) and the on handler prefer another focused viewer of the
   same workspace if one exists, and at commit the focus event runs
   `claim_shell_workspace_geometry`, which in that case can resize the
   workspace's PTYs right after the committed pair. A cancelled move never
   focuses the target's panes.
5. **Pane focus can overlap briefly across servers at commit.** The target's
   focus-gain and the source's focus-loss go out in the same turn but on two
   independent transports to two servers, so the target may process its focus
   before the source processes its release, and for that transport skew a pane
   on each machine reports focus. Today the source is released before the
   target is turned on, so there is no overlap. Removing it would need
   cross-connection synchronization (waiting for the source's off
   acknowledgement before focusing the target), which this design gives up on
   purpose. Brick 6 pins the overlap and its resolution.

## 4. Obstacles resolved inline

- **The render for an on request can arrive before its acknowledgement.**
  The floor is only known at the acknowledgement. Preparing buffers surfaces and
  patches into `ViewEvidence` from the moment the on request is sent (gate:
  Target + surface = Buffer) and `ready` requires `surface.projection_revision >=
  floor`, exactly the check `coherent_surface` makes today. A buffered surface
  from before the acknowledgement that is older than the floor simply fails
  `ready` until the server's newer surface replaces it.
- **Old-epoch frames after a switch back.** A connection turned off and on again
  can still deliver frames the server sent before it processed the off. They are
  Dropped while the connection is Other, and while it is the Target they carry a
  projection revision below the new floor, so they never become evidence.
- **Two connections viewed at once.** Only during `Preparing`, on different
  servers. Each server's rules (PTY size, foreground, pane focus, effects) are
  per server; the client draws and types into one. During preparing only the
  source's panes hold focus, because the target has no outer focus until
  commit. At commit the overlap described in 3.13 item 5 can occur for the
  transport skew between the two servers.
- **Turn-off needs a boot id the client may not have yet.** Only a hello-viewed
  Local connection can be viewed without the client having sent an on request
  (hence without a snapshot), and it can only be unwanted after another
  endpoint committed, by which time its first snapshot has long arrived (a
  server sends it on connect). The path is still handled: the release pass skips
  it, releases every other unwanted connection, and the next turn retries it.
- **Source commands in flight at the switch.** The old design retired the lane at
  source-off. Here the lane is retired at commit; responses to commands sent
  just before are tombstoned as they are today and the shell cancels requests
  whose endpoint is no longer projected (`endpoint_is_active` check in the
  response arm).
- **Navigation on the target.** A pick that names a pane or workspace on another
  endpoint must show that target focused on first draw. Kept in the focus lane
  inside `Preparing` (not applied after commit through the ordinary command path,
  which would flash the target's previous focus for a round trip).
- **A rapid A to B to A.** `select(A)` during `Moving{A -> B}` cancels the move
  to `Showing(A)`; B is released by rule. Nothing was released on A, so there is
  no restore, no fresh epoch to force (`ClientLoopEvent::ActivateEndpoint.force`
  existed only for this) and no successor.
- **Automatic activation without a retry loop.** The old tracker retried an
  unproven selection every turn unless it had failed on this generation. Here
  automatic starts exist only as `Waiting` (a connection that appears) and a
  failure with a shown source returns to the source (selected equals shown, so
  nothing retries); with nothing shown a move failure parks in
  `Failed{generation}` and a send failure removes the connection (3.9).
  `a_failed_move_with_nothing_shown_is_not_retried_on_the_same_generation`
  pins it.
- **A failure and a deadline in the same turn.** Reconcile step 0 handles
  queued failures before step 1 applies the deadline (3.5).
- **The shell's own `active_endpoint_id`.** Pinned by the invariant test in
  3.1 rather than removed: the shell needs it to render.
- **Wire test vehicle** (3.12) and **the cross-crate test** (brick 6) are bricks,
  not deferrals.

## 5. Bricks

One coherent landing (section 6). Bricks are ordered for an implementer; the
tree is red between them and green only at the landing boundary, which rule 7
permits for a single landing. No brick has its own gate.

**Brick 1: protocol and server fence removal.**
`shepr-protocol/src/input.rs` (`ReplayHostEffects`), `message.rs` (delete
`PresentationReady`), `wire_tests.rs` (the `paste` vehicle, the
`CLIPBOARD_ENVELOPE` / `paste_envelope()` split, `replay_host_effects_roundtrips`).
`shepr-client/src/endpoint/writer.rs` tests (the `paste` vehicle).
`shepr-server/src/server/client_transport.rs` and `headless.rs` (event rename,
handler without Ready), `server/headless/tests/surface_delta.rs` (its
`ClientShellPresentationSync` event becomes `ClientShellReplayHostEffects`),
`server/headless/tests/surface_interest.rs`: rename
`presentation_sync_epoch_replays_modes_and_title` to
`replay_host_effects_replays_modes_and_title`. Today the test calls
`stream_host_mouse_capture_mode`, `stream_shell_keyboard_mode` and
`sync_window_title` directly after `surface_set(true)`; the rewrite instead
sends `ServerEvent::ClientShellReplayHostEffects { client_id }` after the
acknowledgement, then asserts the three effect messages (mouse capture on,
report-all off, the configured title) arrive and nothing else follows them on
the control channel. Also a new test
`replay_host_effects_is_ignored_by_a_non_viewed_connection`. Doc comment
edits (2.6).

**Brick 2: registry.** `endpoint/registry.rs`: remove `active`, `active_id`,
`set_active`, `active_surface_available`, `send`; rename to `viewed` /
`set_viewed` / `viewed()`; add `send_viewed`, `release_unwanted_views`. Port
the four tests that use the removed API:
`endpoint_failures_do_not_remove_other_connections` and
`reconnecting_active_identity_does_not_count_as_an_active_surface` (which used
`set_active`; the second becomes `a_reconnected_connection_is_not_viewed_until_told`),
`a_queued_failure_makes_the_service_deadline_due` and
`an_interactive_detach_is_not_sent_again_on_drop` (which used `send`; both move
to `send_to(Local, ..)`). New tests:
`send_viewed_reaches_only_viewed_connections`,
`send_viewed_records_a_failed_connection_and_still_reaches_the_rest`,
`release_unwanted_views_releases_every_resolvable_one_in_one_pass` (three
viewed and unwanted, one of them without a boot id, plus one wanted: two are
sent focus-loss then off and become unviewed, the unresolved one stays viewed,
the wanted one is untouched, and the return is 2).

**Brick 3: choice, preparing, view.** New `endpoint/choice.rs`,
`endpoint/choice/preparing.rs`, `endpoint/choice/focus_lane.rs`,
`endpoint/view.rs`. Delete `endpoint/selection.rs`, `endpoint/activation.rs`,
`endpoint/activation/`, `endpoint/activation_tests.rs`. `endpoint.rs` module list
and exports updated (public: `EndpointChoice`, `Move`, `MoveStage`, `Preparing`,
`ViewLease`, `ConnectionRole`, `Selection`, `Lost`, `PendingStart`,
`FailedMove`, `Committed`, `PrepareProgress`, `HostBaseline`, `StartOutcome`,
and the module `view` with `start_move`, `send_focus`, `commit_move`,
`release_unwanted`). Rename `ACTIVATION_TIMEOUT` to `ENDPOINT_MOVE_TIMEOUT` in
`limits.rs` with the new doc (2.6).
Tests (unit, next to the code):
- `choice.rs`: `a_launch_with_local_connected_shows_local`,
  `an_unreachable_local_at_launch_waits_with_nothing_shown`,
  `selecting_the_shown_endpoint_changes_nothing`,
  `selecting_the_shown_endpoint_with_navigation_asks_to_focus_it`,
  `selecting_another_endpoint_keeps_the_shown_one_until_commit`,
  `a_newer_selection_replaces_the_target_and_only_the_new_one_is_wanted`
  (`from` stays the original shown endpoint),
  `selecting_the_shown_endpoint_during_a_move_cancels_it`,
  `begin_preparing_takes_the_waiting_focus`,
  `a_commit_shows_the_target_and_ends_the_move`,
  `a_failed_move_with_a_shown_source_returns_to_showing_it`,
  `a_failed_move_with_nothing_shown_is_not_retried_on_the_same_generation`
  (`pending_start` reports the failed generation),
  `an_explicit_selection_rearms_a_failed_move` (`Failed` to `Waiting{t}`),
  `abandon_returns_to_the_shown_source_only_from_waiting`,
  `commit_and_fail_move_do_nothing_outside_preparing`,
  `losing_the_shown_connection_leaves_nothing_shown_and_keeps_the_selection`,
  `losing_the_target_connection_returns_to_the_shown_endpoint`,
  `losing_the_shown_connection_keeps_a_healthy_target_preparing`,
  `losing_an_unrelated_connection_changes_nothing`,
  `wants_view_is_the_shown_endpoint_and_the_preparing_target`,
  `role_is_target_only_while_preparing`.
- `preparing.rs` (ported from the activation tests):
  `ready_needs_the_ack_the_focus_and_an_exact_snapshot_surface_pair`,
  `the_ack_floor_rejects_older_epoch_surfaces`,
  `stale_generation_and_boot_are_not_evidence`,
  `a_response_for_another_boot_is_not_consumed`,
  `an_invalid_view_acknowledgement_is_kept_as_the_rejection`,
  `a_rejected_preparing_is_never_ready`,
  `a_surface_of_another_size_is_not_evidence`,
  `a_patch_without_a_baseline_waits_for_a_full_surface`,
  `a_changed_geometry_drops_the_recorded_surface`,
  `an_unchanged_geometry_keeps_the_recorded_surface`.
- `focus_lane.rs`: `a_newer_focus_pick_replaces_the_desired_target_without_joining_the_request`
  (port of `same_target_retarget_is_latest_wins`),
  `a_focus_request_is_built_once_until_its_response`,
  `a_focus_response_for_another_target_is_rejected`.
- `view.rs`: `turn_on_sends_geometry_then_theme_then_the_request_and_no_focus`,
  `turn_on_to_an_already_viewed_connection_still_sends_a_fresh_request`,
  `start_move_installs_preparing_before_it_sends`,
  `start_move_abandons_a_machine_without_a_connection_while_something_is_shown`,
  `start_move_waits_for_metadata_of_the_current_generation`,
  `start_move_restarts_a_failed_move_only_on_another_generation`,
  `commit_move_changes_nothing_when_a_check_fails`,
  `commit_move_sends_the_focus_baseline_then_replay`,
  `a_failed_commit_send_still_completes_the_switch`,
  `release_unwanted_sends_focus_loss_then_the_release`.
- `message_policy.rs`: rewrite the module tests to the table in 3.6:
  `target_effects_are_dropped_and_shown_effects_apply`,
  `a_target_surface_is_buffered_and_an_other_surface_is_dropped`,
  `only_a_move_response_is_buffered_and_only_a_command_response_applies`,
  `a_tombstoned_response_of_the_shown_endpoint_applies`,
  `an_other_restore_snapshot_applies`.

**Brick 4: client state, loop, runtime.** `state.rs` (delete `Presentation`,
`deferred_local`, `end_handoff`; add `choice`; merge the chrome presenters; the
frames gate), `events.rs` (delete `ActivateEndpoint`), `lib.rs` (fields, `new`
without `selection`, `run` calling `reconcile`, `handle_server_message` roles
and arms, `handle_resize`, `handle_timer` without failure handling and expiry,
`run_client_loop` launch, delete `handle_activate_endpoint`, the
`PresentationReady` arm and `take_ready_local_activation` use),
`shell_runtime.rs` (delete the functions in 2.4; `dispatch_client_shell_actions`,
`finish_client_shell_input`, `install_client_shell_snapshot` re-based on roles;
the generalized waiting notice), new `reconcile.rs` (`ClientLoop::reconcile`,
the commit glue, `endpoint_lost`, `present_notice`), `endpoint/commands.rs` (doc
comment), `shell/navigation/endpoint_navigation.rs` (comments),
`shell/tests/endpoint_requests.rs` (the three tests re-expressed over
`EndpointChoice`: `local_selection_is_scheduled_ahead_of_a_full_event_queue`
becomes `a_pick_is_applied_at_once_without_an_event_round_trip`, since picks no
longer travel through the event queue;
`current_owned_targetless_pick_is_a_noop_but_unowned_pick_reproves` becomes
`selecting_the_shown_endpoint_is_a_noop_but_with_nothing_shown_it_reproves`;
`dispatcher_cancels_pending_requests_on_frozen_surface_or_failed_send` becomes
`dispatcher_cancels_pending_requests_on_an_unviewed_endpoint_or_failed_send`).
Wording fixes of 2.6 in the client.

Loop-level tests, new `crates/shepr-client/src/tests/endpoint_choice.rs`, driving
a `ClientLoop` (constructor exists: `ClientLoop::new`, with the initial choice
in the `ClientState` it takes) over recording transports (the `TimerTransport`
pattern in `lib.rs`'s `client_timer_tests`, generalized to a
`RecordingTransport` that can also be told to fail its next send, while the
test injects inbound events through `handle_event` and calls `reconcile`):
- `selecting_a_machine_turns_it_on_and_leaves_the_source_live_until_commit`
  (source frames still present, source input still sent, source not released)
- `the_source_is_released_after_the_commit_and_not_before`
- `commit_sends_the_focus_baseline_then_replay_to_the_target`
- `target_host_effects_are_dropped_until_commit_and_the_replay_applies_after`
- `a_failed_move_releases_the_target` (the REJ-025 pin: ack rejected, timeout,
  and focus mismatch each end with no viewed unwanted connection, and the target
  connection is kept)
- `a_move_timeout_returns_to_the_source_and_reports_it`
- `a_failure_queued_at_the_deadline_reports_the_interruption`
- `a_send_failure_while_preparing_with_nothing_shown_waits_for_a_new_connection`
- `a_failed_commit_send_completes_the_switch_and_then_reports_the_loss`
- `shown_implies_viewed_across_every_transition` (selection, start, commit,
  cancel, failure, loss, and send failures while starting and committing)
- `selecting_local_while_a_remote_prepares_releases_the_remote`
- `local_selection_waits_for_metadata_while_the_shown_endpoint_stays_live`
  (replaces the deferred-Local tests; the notice text is asserted)
- `a_remote_pick_without_metadata_waits_with_a_notice`
- `a_newer_selection_replaces_a_waiting_one`
- `pane_input_and_commands_go_only_to_the_shown_endpoint`
- `commit_retires_the_previous_command_lane`
- `a_resize_reaches_every_viewed_connection_and_drops_the_recorded_surface`
- `a_resize_with_an_unchanged_geometry_keeps_the_move_evidence`
- `host_theme_updates_reach_a_new_target_before_its_on_request`
- `every_viewed_connection_gets_the_one_surface_geometry`
- `losing_the_shown_endpoint_freezes_pane_frames_but_chrome_still_presents`
  (replaces `chrome_frames_pass_an_unavailable_freeze_but_not_a_handoff`)
- `a_reconnected_local_is_prepared_once_per_connection` (replaces
  `an_unavailable_owner_whose_connection_survived_is_reproved_once_per_connection`)
- `an_interactive_detach_goes_to_the_shown_endpoint`
- `the_shell_projects_the_shown_endpoint` (the 3.1 invariant, across a commit, a
  cancel and a loss)
- `local_selection_never_waits_for_a_remote` (replaces
  `local_activation_does_not_wait_for_a_disconnected_stalled_or_failed_remote`)

Old test disposition, so nothing is dropped silently. Obsolete because there is
no source-off, restore, successor, placeholder lease, effects fence or frozen
state:
`source_off_request_is_distinct_and_precedes_target_on_phase`,
`active_source_requires_metadata_from_its_current_connection_generation`,
`observed_begin_write_failure_returns_recoverable_partial_activation`,
`source_release_is_sent_and_acknowledged_before_target_activation`,
`host_focus_change_restarts_an_issued_presentation_effects_fence`,
`source_release_rejection_restores_the_source_coherently`,
`source_release_timeout_starts_an_acknowledged_source_restore`,
`rollback_keeps_the_latest_intent_even_when_it_returns_to_the_target`,
`unacknowledged_target_release_closes_target_before_restoring_source`,
`target_loss_without_a_connected_source_does_not_restore_a_placeholder_lease`,
`target_loss_while_synchronizing_the_restored_source_keeps_that_restore`,
`acknowledged_target_release_preserves_connection_when_source_is_unavailable`
(its surviving property, a failed move keeps the target connection, is asserted
in `a_failed_move_releases_the_target`),
`unknown_machines_cannot_be_selected` (selection.rs; the machine set is fixed
and the shell only picks from it, 3.2),
`frozen_activation_drops_effects_and_buffers_surface_patches` (message_policy.rs;
there is no frozen input, and patch buffering is covered by
`a_target_surface_is_buffered_and_an_other_surface_is_dropped`),
`tracked_command_responses_apply_outside_the_active_presentation`
(message_policy.rs; a non-shown connection has no in-flight command, 3.6).
Replaced as listed above, or:
`activation_requires_an_exact_snapshot_surface_revision_pair`,
`typed_target_ack_sets_a_floor_for_same_boot_activation_evidence`,
`stale_generation_and_boot_are_not_activation_evidence`,
`stale_response_boot_is_not_consumed`, `same_target_retarget_is_latest_wins`,
`latest_host_focus_is_replayed_to_the_eventual_target` (now host focus goes to
the shown endpoint only and the baseline at commit:
`commit_sends_the_focus_baseline_then_replay_to_the_target`),
`resize_invalidates_already_recorded_surface_evidence`,
`resize_during_activation_reaches_the_pending_target`,
`rapid_a_to_b_to_a_restores_source_before_a_fresh_latest_epoch`
(`selecting_the_shown_endpoint_during_a_move_cancels_it`),
`local_selection_abandons_every_unfinished_remote_handoff_phase`,
`local_selection_waits_for_fresh_metadata_without_abandoning_remote` (its
property is inverted on purpose, 3.13 item 3:
`selecting_local_while_a_remote_prepares_releases_the_remote`),
`newer_remote_selection_cancels_deferred_local_selection`
(`a_newer_selection_replaces_a_waiting_one`),
`picking_the_active_endpoint_reproves_it_only_while_nothing_owns_the_presentation`,
`target_loss_at_activation_deadline_restores_source_before_timeout`
(`a_failure_queued_at_the_deadline_reports_the_interruption`),
`losing_local_during_handoff_does_not_revoke_the_healthy_target`,
`both_sides_of_a_handoff_are_sized_by_the_one_geometry`
(`every_viewed_connection_gets_the_one_surface_geometry`).
The `selection.rs` tests map onto `choice.rs`:
`a_new_tracker_starts_on_local` to `a_launch_with_local_connected_shows_local`;
`failed_handoff_restores_previous_selection_and_suppresses_retry` to
`a_failed_move_with_a_shown_source_returns_to_showing_it` and
`a_failed_move_with_nothing_shown_is_not_retried_on_the_same_generation`;
`a_committed_handoff_keeps_the_selection` to
`a_commit_shows_the_target_and_ends_the_move`;
`a_failed_handoff_back_to_a_selected_machine_restores_it` to
`a_failed_move_with_a_shown_source_returns_to_showing_it`;
`superseded_request_keeps_the_original_restore_point` to
`a_newer_selection_replaces_the_target_and_only_the_new_one_is_wanted`;
`explicit_request_clears_failure_memory` to
`an_explicit_selection_rearms_a_failed_move`.
The other `message_policy.rs` tests map onto the rewritten ones:
`host_effects_need_an_owned_presentation_or_a_validated_sync` and
`inactive_endpoint_control_applies_but_presentation_effects_drop` to
`target_effects_are_dropped_and_shown_effects_apply`;
`inactive_restore_snapshot_applies_without_surface_activation` to
`an_other_restore_snapshot_applies`;
`activation_surfaces_and_responses_are_buffered` to
`a_target_surface_is_buffered_and_an_other_surface_is_dropped` and
`only_a_move_response_is_buffered_and_only_a_command_response_applies`.

**Brick 5: documentation.** No `AGENTS.md` edit (2.1: it has no affected
statement); if review decides a sentence stating "a client views one endpoint
at a time and tells every other connection it is not viewed" belongs there, it
is added here, as the only contract-bearing wording. No `notes/` document and
not this spec.

**Brick 6: the end-to-end instrument.** Rewrite
`crates/shepr-server/src/server/netside_tests.rs`'s
`two_headless_servers_drive_atomic_endpoint_handoff` as
`two_headless_servers_switch_endpoints_without_a_lease`, keeping its real
`HeadlessServer` pair, its `CapturingEndpointTransport` and its rule that the
test authors no acknowledgement, snapshot or surface.
`server/headless/tests/mod.rs`'s `dispatch_lifecycle_messages` gains arms for
`ClientShellHostTheme` (to `ServerEvent::ClientShellHostTheme`) and
`ReplayHostEffects` (to `ServerEvent::ClientShellReplayHostEffects`). The test
drives `EndpointChoice`, `EndpointRegistry` and `ClientShellState` through the
same `pub` steps the reconcile uses (`view::start_move`, `view::send_focus`,
`view::commit_move`, `view::release_unwanted`) and asserts:
(0) the source server is sent the launch `ClientShellFocus { focused: true }`
first, as `run_client_loop` does, so its outer focus is `Some(true)` (it starts
`None`);
(1) after `select(Remote)` and `start_move`, the target server received resize
then the on request and no focus, and the source server received nothing and
still reports the client viewed with outer focus true;
(2) the target server's typed acknowledgement, snapshot and surface (routed from
the real server) make `ready` true;
(3) `commit_move` sends focus and `ReplayHostEffects`; the target server's
messages are routed first, and at that point both servers report the client's
outer focus true (the overlap of 3.13 item 5); the target server replays mouse
capture, keyboard mode and title (as the old test asserted for the fence);
then `release_unwanted` and routing the source server's messages leave the
source with outer focus false and the surface not viewed;
(4) returning to Local repeats the sequence in the other direction, with the
remote released and Local's pane focus restored without a host focus event.
`shepr-server/Cargo.toml`'s comment on the client dev-dependency is reworded.
The test file imports only `pub` items listed in brick 3.

## 6. Landing, gate, keep or revert

One landing containing bricks 1 to 6 (the protocol change, the registry change
and the client rewrite cannot be green apart: the client uses the new wire
message and the registry shape, and the old handoff cannot run without the
fence). Every named test above is gated by `brokkr check` passing: it runs the
gremlins check, clippy and every test including the loop-level and end-to-end
tests, none of which is `#[ignore]`d. No separate `brokkr test` run is needed:
no test here is ignored and none needs its production half reverted to be seen
failing, because each new test names behavior (a released target, a dropped
effect, a retired lane) that has no production code left to revert to.

Commands, in order:

```
brokkr fmt
brokkr check
```

Keep when `brokkr check` is green. Revert the whole landing otherwise; there is
no half-state worth keeping because the old and new owners of "which endpoint"
cannot coexist. This spec commits nothing.

## 7. Stopping rule and what stays out

The rip stops at:

- The client's endpoint choice, the viewing ledger, the message gate, the
  reconcile and its tests; the one protocol message pair (3.12) and the tests
  that used it as a vehicle (`wire_tests.rs`, `writer.rs`); the server event
  that handled it; comments and test names that describe the removed concepts.
- **Not touched, owned elsewhere:** the shell's surface baseline and its
  revision rules (`set_pane_surface`, `install_pane_surface`,
  `apply_pane_surface_patch`, `pending_pane_surface`) and the reader's own
  baseline in `lib.rs`/`transport.rs`: item 2 ("a request ledger and an
  explicit surface baseline") replaces them, and REJ-011 stays open until then.
  This spec uses only `activate_endpoint_projection`, `set_pane_surface` and
  `apply_pane_surface_patch`, as today, and keeps its switch evidence outside
  the shell so item 2 can change the shell side without touching it. Item 2's
  request ledger will also absorb `EndpointCommands`; nothing here changes its
  shape beyond a doc comment.
- The server's naming (`surface_active`, `ClientShellSurfaceSet`,
  `EndpointError::SurfaceInactive`), its viewing rules and its per-client render
  and outbox design (item 4, already landed): unchanged apart from the doc
  wording in 2.6.
- The pty-size, foreground and pane-focus rules in `client_views.rs`: they already
  read every client's viewing state.
- Supervisor, health and transport production code (`endpoint/supervisor.rs`,
  `health.rs`, `writer.rs`, `local_failure.rs`): they report connection facts and
  never knew about presentation. Only `writer.rs`'s tests change (3.12).
- Item 3 (the pane-exit checkpoint).

## 8. Findings outside the item

- `ClientShellState::active_endpoint_id` is a seventh holder of the endpoint
  choice (2.1); this spec constrains it but does not remove it, because the shell
  renders from it. If item 2 gives the shell an explicit "projected endpoint"
  concept, the invariant test in 3.1 is the thing to carry over.
- `EndpointCommands::retire_lane`'s doc described source-off; the semantic it
  needs now ("an endpoint stopped being shown") is narrower and clearer.
- The command-lane tombstones are nearly redundant today: a late response to a
  retired request that found no tombstone would already be ignored, because
  request ids are unique and `receive_response` drops anything that is not the
  in-flight key. This spec keeps them working for the shown endpoint (3.6);
  item 2's request ledger should decide whether they survive.
- `begin_endpoint_activation` cleared `deferred_local` before deciding whether
  to keep it, so a rejected newer pick could drop a still-valid deferred Local
  selection. The enum removes the ordering question.
- The remote-without-connection case of a pick is handled by the shell
  (`is not ready`) and again by reconcile step 2; the second is a race guard only.
- `host_focus_baseline` documents that an unreporting host means "focused". The
  commit-time baseline in 3.8 keeps that meaning.
- Turning a target on promotes the client to foreground on the target server
  and claims workspace geometry there (`set_client_shell_surface_active`), so a
  cancelled move (A to B to A) still resizes B's PTYs and switches B's host
  theme source for a moment. That was already true under the lease; cancelling
  is cheaper now, so it happens more often. A server-side rule that a viewed but
  unfocused connection does not claim geometry would make cancelled moves free.
  Out of this item's scope.

## 9. Review disposition

Every finding of both reviews was checked against the tree and folded above,
except one:

- Rejected: r1's lateral note that `dispatch_lifecycle_messages` maps
  `geometry.width()` / `geometry.height()` to `cell_width_px` /
  `cell_height_px` and may confuse cell pixels with surface columns.
  `TerminalGeometry::width()` and `height()` return the cell pixel size
  (`cell.width`, `cell.height`), so the mapping is correct and nothing changes.
