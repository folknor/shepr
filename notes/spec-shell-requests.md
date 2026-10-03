# Spec: shell request continuations and the endpoint move owner

Status: plan. Implements hunt entries STR-045 ("Requests: one typed continuation
per command") and STR-039 ("The client move protocol has no owner, and
ClientLoop is open") from `notes/hunt-structure.md`, both re-verified against
the tree at `7b4f30b2`.

Written against `reference/technical-implementation-spec.md`. Sibling spec C,
`notes/spec-shell-render.md`, is written at the same time and owns the shape of
`ClientShellState`, the copy session and mode state, and the overlays; this
spec only adds continuation tokens to those, and the last section lists what it
assumes of C and what it offers C. The two specs land in one interleaved order,
given in section 6a and recorded identically in C.

## 1. Goal, in the owner's words and in types

STR-045 removes a bug class: a stale answer applied, or a request dropped for
the wrong reason. Target: every `Work` variant carries the token its feature
checks; features compare that token instead of keeping their own copy of the
ledger's `RequestId`; and `Work::dropped` receives a context that cannot reach
`submit`, so "a rollback cannot open a request" becomes a type rule instead of
the runtime `opened_since` orphan guard.

STR-039 gives the endpoint move protocol one owner, closes `ClientLoop`'s
fields, and replaces `ClientLoop::new`'s ten positional arguments.

The endpoint layer's own request ids are decided in section 5.4: they stay out
of the ledger, with the reason.

## 2. Contracts inventoried

- `reference/`: only `technical-implementation-spec.md` (this spec's contract).
- `docs/`: does not exist.
- `AGENTS.md`: read. Nothing in it describes the client ledger, the move
  protocol, `ClientLoop` or the client crate's module layout below the crate
  line ("`shepr-client`: endpoint management and TUI presentation"). No
  binding document changes. `brokkr.toml` (config, not a contract) changes in
  landings 2, 4 and 5: two new textlint rules and two path edits.
- No wire, protocol or server change. The `shepr-server` dev-test
  `crates/shepr-server/src/server/netside_tests.rs` drives the client's public
  `endpoint::view` API and `ClientShellState::endpoint_choice(_mut)`; that API
  is kept (section 9, landing 4).

## 3. Survey: the shell ledger today

All paths below are under `crates/shepr-client/src/`.

### 3.1 `shell/ledger.rs`

- `Ledger { next: u64, entries: HashMap<RequestId, Entry> }`. `Entry` holds
  `issued_at` (a serial used only by the orphan guard), `boot_id`, `command:
  CommandKind`, `work: Work`.
- `Ledger::open` allocates `RequestId::allocate()` (a process-global counter in
  `shepr-protocol/src/identity.rs`, shared with the endpoint layer's ids), so a
  ledger id never equals a move or view-off id.
- `Ledger::work(id)` lets a caller read another request's `Work`; its only
  caller is `defer_copy_until_search_result` in `shell/input/copy_mode.rs`.
- `Ledger::mark` / `opened_since`: the orphan guard's serial window.
- `Work` variants: `Plain`, `SelectionCopy`, `WorkspaceLabel`, `PaneScroll {
  pane_id }`, `WordSelection { pane_id, row }`, `CopyMotion { pane_id, origin
  }`, `CopySearch { pane_id, origin, query, direction, repeat, generation }`.
  Only `CopySearch` carries a token (`generation`, documented "not a request
  guard").
- `Work::answered(self, shell, request: &RequestId, result, now, outcome)` and
  `Work::dropped(self, shell: &mut ClientShellState, request: &RequestId)`: both
  hand the `RequestId` to the feature, which compares it with its own copy.
  `dropped` receives the whole `ClientShellState`, so it can call `submit`.
- `ClientShellState::submit(command, work, outcome) -> Option<RequestId>`:
  takes `pending_workspace_highlight` when the command changes focus, refuses
  with a NotReady notice when the presented endpoint is not usable, returns
  `None` without a snapshot, else opens the entry and pushes
  `ClientShellAction::Endpoint`.
- `answer_request`: takes the entry; boot mismatch drops it as `WrongBoot`;
  checks `command.accepts_reply && work.accepts_reply`; on error calls
  `release_highlight(request_id)` then pushes the error notice; then
  `work.answered`.
- `dropped_entry(entry, request, reason)`: `release_highlight(request)`; the
  "Action interrupted" notice when `reason` is `Interrupted` and `work` is
  `Plain`; `work.dropped`; then the orphan guard: any entry opened since the
  mark is logged at error and dropped as `Unsent`.
- `DropReason::Interrupted` is documented as covering "its endpoint is no
  longer the active one when its answer or lane expiry arrives" (see finding
  F3: unreachable with entries).
- `ClientShellEndpointRequest { id: RequestId, command }` is defined in
  `shell/state.rs`, re-exported by `shell/mod.rs`.

### 3.2 Every staleness check today, and what replaces it

| # | Where | Today | After |
|---|---|---|---|
| 1 | `Ledger::take` in `answer_request` / `drop_request` | unknown or settled `RequestId` is ignored | unchanged (transport identity) |
| 2 | `answer_request` | entry boot and snapshot boot must equal the answer's boot, else `WrongBoot` drop | unchanged |
| 3 | `answer_request` | `command.accepts_reply && work.accepts_reply`, else typed internal error | unchanged; `Focus` accepts any reply like `Plain` |
| 4 | `complete_copy_motion`, `complete_copy_search`, `drop_copy_operation` (`shell/input/copy_mode.rs`) | `CopyPipeline::is_awaiting(request)` on entry, and again after apply (the apply can exit copy mode and reset the pipeline) | `CopyPipeline::holds(flight)` with the `flight: Ticket` from `Work`; same two places |
| 5 | `apply_copy_search_result`, `cancel_deferred_copy_after_search` | `copy_mode.operation_generation != generation` | `copy_mode.rows != rows` (`rows: Ticket` from `Work::CopySearch`) |
| 6 | `apply_copy_motion_target`, `apply_copy_search_result` | answered pane equals session pane, `copy_mode.cursor == origin` | unchanged (state checks, not identity) |
| 7 | `defer_copy_until_search_result` | reads `ledger.work(awaiting)` and matches `CopySearch` on pane and generation | `copy_pipeline.awaited_search_rows() == Some(copy_mode.rows)`; no ledger read |
| 8 | `ScrollLanes::answered`, `ScrollLanes::failed` (`shell/input/scroll_lanes.rs`) | lane exists and `flight.request == request` | `flight.ticket == flight` |
| 9 | `ScrollLanes::retain_panes` (called by `apply_active_snapshot`) | drops lanes of vanished panes; the ledgered answer then mismatches a missing flight | unchanged behaviour; doc says the ticket is gone with its lane |
| 10 | `answer_pane_scroll` (`shell/input/mouse.rs`) | reply pane equals `pane_id` | unchanged |
| 11 | `complete_word_selection_row`, `drop_word_selection` (`shell/input/word_selection.rs`) | `gesture.pending == Some(request)` | `gesture.pending == Some(read)` (`read: Ticket`) |
| 12 | `complete_word_selection_row` | pane still in snapshot, reply pane equals `pane_id` | unchanged |
| 13 | `complete_workspace_label_lookup` (`shell/overlays/overlay_input.rs`) | overlay is the new-workspace prompt and `label_lookup == Some(request)` | `label_lookup == Some(lookup)` (`lookup: Ticket`) |
| 14 | `release_highlight` in `answer_request` (error) and `dropped_entry` (every drop) | `pending_workspace_highlight.request_id == request` | moves into `Work::Focus { highlight }`: `answered` (error) and `dropped` release `highlight` |
| 15 | `submit` | any focus-changing command takes the highlight | unchanged |
| 16 | `dropped_entry` | interruption notice only for `Plain` | `Work::reports_interruption`: `Plain` and `Focus` |
| 17 | `dropped_entry` orphan guard | runtime: entries opened during a rollback are dropped as `Unsent` | deleted; `Work::dropped` takes `Rollback`, which has no path to `submit` |
| 18 | `ClientCopyModeState::operation_generation` bumps: `presented_surface_changed` (resize or screen switch), `CancelOrClear`, each search dispatch; restarts at 0 in `enter_copy_mode` | per-session `u64` | `rows: Ticket` re-issued by the ledger at the same points and at session entry |
| 19 | `abandon_copy_operation` | `reset_copy_pipeline` clears `awaiting`; "the request stays in the ledger; its answer ... is ignored" | same call; the flight ticket is revoked, doc says so |

Endpoint layer and crate root (all unchanged in meaning; section 5.4):

| # | Where | Check |
|---|---|---|
| 20 | `EndpointCommands::receive_response` (`endpoint/commands.rs`) | `RequestKey { generation, boot_id, request_id }` equals the lane's in-flight key |
| 21 | `EndpointCommands::send_next` | a queued command whose generation the registry no longer accepts is cancelled unsent |
| 22 | `ClientLoop::handle_server_message` (`dispatch.rs`) | `write_stream.accepts(endpoint, generation)`; `PresentationGate` by role and `move_response` |
| 23 | `Preparing::accepts_response` (`endpoint/choice/preparing.rs`) | lease (endpoint, generation, boot) and the on request or the focus lane's in-flight id |
| 24 | `Preparing::receive_snapshot / _surface / _patch` | lease, revision floor, surface size |
| 25 | `FocusLane::receive` (`endpoint/choice/focus_lane.rs`) | reply names the requested target |
| 26 | `dispatch.rs` command completion | `shell.endpoint_is_active(completed.endpoint_id)` else drop `Interrupted` (finding F3: never true with an entry) |
| 27 | `settle_expired_endpoint_commands` (`shell_runtime.rs`) | connection accepted and endpoint active, else drop `Interrupted` (the "not active" half: finding F3) |

### 3.3 Feature-local identities (STR-045's five, verified complete)

The only `RequestId` uses in `shell/` outside `ledger.rs` and the
`ClientShellEndpointRequest` struct are these five fields and the functions
that compare against them:

1. `CopyPipeline::awaiting: Option<RequestId>` (`shell/input/copy_mode.rs`):
   `in_flight`, `is_awaiting`, `awaiting`, `begin`, `finish`, `reset`.
2. `ScrollFlight::request: RequestId` (`shell/input/scroll_lanes.rs`): `sent`,
   `answered`, `failed`.
3. `ClientWordSelection::pending: Option<RequestId>`
   (`shell/input/word_selection.rs`).
4. `PendingWorkspaceHighlight::request_id: RequestId`
   (`shell/navigation/workspace_navigation.rs`), set by
   `keep_workspace_highlight_until_snapshot` from `focus_endpoint_target`
   (`shell/navigation/actions.rs`).
5. `ClientRenameTarget::NewWorkspace::label_lookup: Option<RequestId>`
   (`shell/state.rs`), set by `open_new_workspace_overlay`.

### 3.4 Endpoint-layer request ids

- `EndpointCommands` (`endpoint/commands.rs`): per-endpoint lane, one command
  in flight, keyed by `RequestKey { generation, boot_id, request_id }` where
  `request_id` is the ledger's. Cancellations (`retire_lane`, `disconnect`,
  `send_next`) and completions (`receive_response`, `expire`) all flow back to
  the ledger by `RequestId` through `cancel_endpoint_commands`,
  `settle_expired_endpoint_commands` and `dispatch.rs`.
- `Preparing::view_request` (`endpoint/choice/preparing.rs`), allocated in
  `view::start_move` and sent by `view::turn_on` directly on the registry (not
  through the lane).
- `FocusLane::in_flight: Option<(RequestId, ClientEndpointFocusTarget)>`,
  allocated in `FocusLane::request`, sent by `view::send_focus`.
- `EndpointRegistry::release_unwanted_views` (`endpoint/registry.rs`)
  allocates a view-off id per release and never records it (fire and forget;
  its answer is dropped by the gate as role `Other`, or ignored by the lane if
  the endpoint is shown again). STR-045 omits this one.

### 3.5 The move protocol today (STR-039)

| Step | Lives in |
|---|---|
| Choice state machine (`EndpointChoice`, `Move`, `MoveStage`, `select`, `connection_lost`, `abandon`, `begin_preparing`, `fail_move`, `commit`, `role`, `wants_view`) | `endpoint/choice.rs`, stored at `ClientShellState.endpoints.choice` (`shell/endpoints.rs`) |
| Evidence and readiness (`Preparing`, `ViewEvidence`, `MoveFailure`) | `endpoint/choice/preparing.rs` |
| Navigation lane | `endpoint/choice/focus_lane.rs` |
| I/O steps (`start_move`, `turn_on`, `send_focus`, `commit_move`, `release_unwanted`, `selection_wait_notice_needed`, `target_readiness`) | `endpoint/view.rs` (pub; used by the netside test) |
| Viewed flags, view-off, `send_viewed` | `endpoint/registry.rs` |
| Lane retire at commit, `send_next` | `endpoint/commands.rs`, called from `reconcile.rs` |
| Sequencing: failures, fail on rejection or deadline, start, focus, commit, retire, release | `reconcile.rs` (`ClientLoop::reconcile`, `fail_move`, `endpoint_lost`) |
| Inbound buffering of evidence (`receive_patch/_surface/_response/_snapshot`), `move_response` | `dispatch.rs` (`ClientLoop::handle_server_message`) |
| Pick (`choice.select`), waiting notice, input endpoint, resize into `Preparing::update_geometry` | `shell_runtime.rs` |
| Commit of the choice | `ClientShellState::activate_endpoint_projection` (`shell/endpoints.rs`), called from `view::commit_move` |
| Loss of the choice | `ClientShellState::transition_endpoint_status` (`shell/endpoints.rs`), called from `reconcile.rs` |

`activate_endpoint_projection` has a third branch (`None if
pending_start().is_none()` sets `EndpointChoice::showing`) that production never
reaches; about thirty shell tests use it as a shortcut to switch endpoints
(`shell/tests/endpoints.rs`, `workspace_navigation.rs`, `mouse_selection.rs`).

### 3.6 `ClientLoop` today (`client_loop.rs`)

Fields, all `pub(crate)`: `state`, `local_failure_policy`, `should_quit`,
`fatal`, `write_stream` (the `EndpointRegistry`), `supervisors`,
`endpoint_commands`, `reported_cell_size`, `event_tx`, `event_rx`,
`will_query_host_cell_size`. `ClientLoop::new` takes ten positional arguments
(everything but `endpoint_commands`). `reconcile.rs` and `dispatch.rs` are
sibling modules with `impl ClientLoop` blocks that destructure the fields.
Constructed by `launch.rs`, by `client_loop.rs`'s `client_timer_tests`, and by
`tests/endpoint_choice.rs`'s `Fixture` (which `endpoint/view.rs`'s tests also
use). `present_notice` lives in `reconcile.rs` and is used by `launch.rs` and
`client_loop.rs`.

## 4. Decisions

### 4.1 A ticket, issued by the ledger, never reissued

The token is `Ticket(u64)`, issued by `Ledger::ticket()` from one counter for
the life of the shell. Rejected alternative: a per-feature generation counter.
Three of the five owners are values that are dropped and rebuilt (a word
gesture per double click, a scroll lane per pane flight, the rename prompt per
open); a counter inside such a value restarts and can hand the rebuilt owner
the same token as the answer still in flight for the one it replaced. A
shell-lifetime source rules that out by construction. "Bump on reset" becomes
"discard the held ticket": the next one is always new.

A feature mints its ticket before `submit` (so `Work` can carry it) and records
it only when `submit` returns `Submitted::Opened`. A refused submit leaves no
held ticket, so nothing needs revoking; each caller keeps its existing refusal
branch.

### 4.2 Copy keeps two tokens: the flight and the rows

The copy pipeline asks two different questions of an answer. "Does it release
my pipeline?" (finish the flight, replay queued keys, dispatch the next queued
operation) must be answered yes for the current flight even after a resize.
"Does it still describe my rows?" (apply search matches, move the cursor to a
match, run the deferred copy) must be answered no after a resize, a
`CancelOrClear`, or a newer search dispatch. Today these are `awaiting` and
`operation_generation`. They stay two tokens: `flight: Ticket` on both copy
`Work` variants, and `rows: Ticket` on `CopySearch` and on
`ClientCopyModeState`.

`CopyMotion` gets no `rows`. A resize keeps the copy cursor (it is only
clamped by `retained_row`), so a motion target answered across it is exactly as
valid as the cursor the shell kept; refusing it would only lose the keystroke.
This is the current behaviour, now stated (finding F5).

### 4.3 `Work::Focus` owns the highlight

The workspace highlight is display continuity for one focus request; today the
ledger releases it generically by comparing the highlight's `RequestId` with
every answered or dropped id. `focus_endpoint_target` now submits `Work::Focus
{ highlight }`; that variant's `answered` (on error) and `dropped` release the
highlight holding `highlight`. `Focus` reports interruption like `Plain`
(today these commands are `Plain`).

### 4.4 The rollback context is a disjoint borrow

`Work::dropped(self, parts: Rollback<'_>) -> Repaint`. `Rollback` holds one
`&mut` to each feature-state owner a drop restores (copy pipeline, copy session
state, mouse selection, scroll lanes, overlay slot, highlight slot), built in
`ledger.rs` from disjoint field borrows of `ClientShellState`. None of those
types holds or reaches the `Ledger` or `ClientShellState`, and `submit` is a
method of `ClientShellState`, so a drop cannot open a request. The orphan
guard, `Ledger::mark`, `opened_since` and `Entry::issued_at` are deleted. The
interruption notice stays in `dropped_entry` (ledger code, not `Work`).

`answered` keeps `&mut ClientShellState` and the routed `outcome`: an answer
may submit (the queued scroll offset, the next copy operation, the replayed
keys), and its outcome is routed by the caller.

### 4.5 The endpoint layer's ids stay out of the ledger

- The command lane is transport correlation, not a continuation: it stores the
  ledger's id plus generation and boot, holds no feature state, and reports
  every outcome back to the ledger by id. It stays as is.
- The move's on request and focus requests are continuations whose owner is
  one value, the `Preparing`; its lifetime is the scope, so "stale" is "not
  this `Preparing`'s lease and id" (`accepts_response`). The ledger cannot hold
  them: it is scoped to the presented endpoint's boot and is emptied at every
  commit (the projection reset drops all entries as `Reset`); its `Work`
  completes shell features, while move answers are evidence for an
  endpoint-layer value, arrive from a connection that is not the shown one,
  and are routed by the presentation gate before the shell sees anything.
  They must remain `RequestId`s because they are matched on the wire with no
  ledger to map an id to a ticket. `FocusLane::in_flight` is already the
  ticket-and-work pair in this model.
- The view-off id is fire and forget and needs no owner.

So the ledger's sole ownership of request identity is a shell claim, and it is
enforced by a textlint (landing 2). The endpoint layer's discipline is that
all of its continuations are checked in one place, the hub (landing 4).

### 4.6 One owner for the move: `EndpointHub`; the choice stays in the shell

New `endpoint/hub.rs`: `EndpointHub` owns the registry, the command lanes, the
supervisors and the local failure policy, all private, and is the only
production code that transitions `EndpointChoice`. `EndpointChoice` stays at
`ClientShellState.endpoints.choice` (C keeps it there; section 12): the shell
reads `presented()` and `live()` everywhere, and the netside test reaches the
choice through `endpoint_choice(_mut)`. The shell stops transitioning it:
`activate_endpoint_projection` no longer commits and `transition_endpoint_status`
no longer reports the loss. The pure step functions in `endpoint/view.rs` stay
public (the netside test drives them); the hub sequences them. Crate-root
`reconcile.rs` disappears into the hub plus a short effect applier; the
crate-root `dispatch.rs` keeps only host-side effects.

### 4.7 `ClientLoop` closed by module privacy

`client_loop.rs` becomes `client_loop/mod.rs`, `dispatch.rs` becomes
`client_loop/dispatch.rs` (a child module sees the parent's private fields),
all fields become private, and `new` takes five typed parts.

## 5. Target types and APIs

### 5.1 `shell/ledger.rs`

```rust
/// The token a continuation is checked against. A feature holds the ticket of the
/// answer it waits for, and that answer's `Work` carries the same ticket back. One
/// counter issues tickets for the life of the shell and does not wrap in practice, so a
/// ticket held by a value that was dropped and rebuilt can never match an answer meant
/// for the value it replaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::shell) struct Ticket(u64);

#[cfg(test)]
impl Ticket {
    /// For tests that build feature state by hand. Counts down from the top of the
    /// range, which a ledger never reaches.
    pub(in crate::shell) const fn fixture(n: u64) -> Self {
        Self(u64::MAX - n)
    }
}

/// One command bound for the active endpoint, with the id its answer comes back
/// under. (Moved here from `shell/state.rs`, with its redacting `Debug`.)
pub(crate) struct ClientShellEndpointRequest {
    pub(crate) id: RequestId,
    pub(crate) command: EndpointCommand,
}

pub(in crate::shell) struct Ledger {
    next_ticket: u64, // starts at 1
    entries: HashMap<RequestId, Entry>,
}
struct Entry {
    boot_id: BootId,
    command: CommandKind,
    work: Work,
}
impl Ledger {
    pub(in crate::shell) fn ticket(&mut self) -> Ticket;
    fn open(&mut self, boot_id: BootId, command: CommandKind, work: Work) -> RequestId;
    fn take(&mut self, id: &RequestId) -> Option<Entry>;
    pub(in crate::shell) fn is_empty(&self) -> bool;
    fn ids(&self) -> Vec<RequestId>;
}
// cfg(test): `retain`, `contains`, `len` stay. `mark`, `opened_since`, `work` are gone.

#[derive(Debug)]
pub(in crate::shell) enum Work {
    /// A command whose answer needs no shell state.
    Plain,
    /// A focus command. A workspace highlight installed for it holds `highlight`.
    Focus { highlight: Ticket },
    SelectionCopy,
    WorkspaceLabel { lookup: Ticket },
    PaneScroll { pane_id: PublicPaneId, flight: Ticket },
    WordSelection { pane_id: PublicPaneId, row: AbsRow, read: Ticket },
    CopyMotion { pane_id: PublicPaneId, origin: PaneTextPoint, flight: Ticket },
    CopySearch {
        pane_id: PublicPaneId,
        origin: PaneTextPoint,
        query: TypedText,
        direction: PaneCopySearchDirection,
        repeat: bool,
        flight: Ticket,
        rows: Ticket,
    },
}
impl Work {
    fn accepts_reply(&self, reply: &EndpointReply) -> bool;   // Focus: true, like Plain
    /// Whether losing it unanswered shows the interruption notice: commands that change
    /// server state. Reads only supply presentation.
    fn reports_interruption(&self) -> bool;                    // Plain | Focus
    fn answered(self, shell: &mut ClientShellState,
                result: Result<EndpointReply, ClientShellEndpointError>,
                now: Instant, outcome: &mut ClientShellInput);
    fn dropped(self, parts: Rollback<'_>) -> Repaint;          // landing 3
}

/// Everything a request's drop may restore, as disjoint borrows of the shell's
/// feature state. It holds no path to the ledger or to `ClientShellState`, so a
/// drop cannot open a request.
/// (Shapes as of C's landings 5 and 7, which land before this spec's landing 3:
/// the copy session owns its pipeline, and the overlay type is `Overlay`.)
struct Rollback<'a> {
    copy: &'a mut Option<CopySession>,
    mouse_selection: &'a mut MouseSelection,
    scroll_lanes: &'a mut ScrollLanes,
    overlay: &'a mut Option<Overlay>,
    highlight: &'a mut Option<PendingWorkspaceHighlight>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::shell) enum Submitted {
    Opened,
    Refused,
}

impl ClientShellState {
    pub(in crate::shell) fn submit(&mut self, command: EndpointCommand, work: Work,
                                   outcome: &mut ClientShellInput) -> Submitted;
    pub(in crate::shell) fn push_endpoint_command(&mut self, command: EndpointCommand,
                                                  outcome: &mut ClientShellInput);
    pub(crate) fn answer_request(&mut self, boot_id: &BootId, request_id: &RequestId,
                                 result: Result<EndpointReply, ClientShellEndpointError>,
                                 now: Instant) -> ClientShellInput;
    pub(crate) fn drop_request(&mut self, request_id: &RequestId, reason: DropReason) -> Repaint;
    pub(in crate::shell) fn drop_all_requests(&mut self, reason: DropReason) -> Repaint;
    fn rollback(&mut self) -> Rollback<'_>;                    // landing 3
    fn dropped_entry(&mut self, entry: Entry, reason: DropReason) -> Repaint;
}
```

`Work::dropped` (landing 3 form):

| Variant | Rollback |
|---|---|
| `Plain`, `SelectionCopy` | `Repaint::Needed` |
| `Focus { highlight }` | `PendingWorkspaceHighlight::release(parts.highlight, highlight)`, then `Repaint::Needed` |
| `WorkspaceLabel { lookup }` | `drop_label_lookup(parts.overlay, lookup)` (returns `Unchanged`, as today) |
| `PaneScroll { pane_id, flight }` | `Needed` if `parts.scroll_lanes.failed(&pane_id, flight)` else `Unchanged` |
| `WordSelection { read, .. }` | `parts.mouse_selection.drop_word_read(read)` |
| `CopyMotion { flight, .. }`, `CopySearch { flight, .. }` | `drop_copy_flight(parts.copy, flight)` |

`Work::answered`: as today, minus the `request` argument, each feature
receiving its tickets from the variant; `Focus` is `Plain` plus, on error,
`PendingWorkspaceHighlight::release(&mut shell.pending_workspace_highlight,
highlight)`.

`DropReason::Interrupted` doc becomes: "The request may have reached the
server and its outcome is unknown: the connection was lost, or the lane
cancelled it as possibly sent. Only work that reports interruption shows the
interruption notice." (The endpoint-not-active clause goes; finding F3.)

### 5.2 Feature state

Paths below are as C's landings 5 and 7 leave them, since this spec's
landings 2 and 3 follow those (section 6a): `CopyPipeline` in
`shell/copy/pipeline.rs`, the copy session in `shell/copy/mod.rs`, the copy
key code in `shell/copy/keys.rs`, the rename overlay in
`shell/overlays/rename.rs`, and the overlay openers in `shell/overlays/mod.rs`.

```rust
// shell/copy/pipeline.rs (C5 moved it from shell/input/copy_mode.rs)
#[derive(Clone, Copy, Debug)]
struct CopyFlight { ticket: Ticket, search_rows: Option<Ticket> }

/// Operations only queue behind a flight, except during dispatch. Keys also exist
/// while a completed flight's input is replayed.
#[derive(Default)]
pub(in crate::shell) struct CopyPipeline {
    /// The one outstanding copy request. Only an answer carrying its ticket applies;
    /// a reset discards it, which is what makes a late answer stale.
    flight: Option<CopyFlight>,
    ops: VecDeque<ClientCopyOperation>,
    keys: VecDeque<TerminalKey>,
}
impl CopyPipeline {
    pub(in crate::shell) fn in_flight(&self) -> bool;
    pub(in crate::shell) fn holds(&self, flight: Ticket) -> bool;
    pub(in crate::shell) fn begin(&mut self, flight: Ticket, search_rows: Option<Ticket>);
    /// The rows a search in flight was dispatched against; `None` for a motion or none.
    pub(in crate::shell) fn awaited_search_rows(&self) -> Option<Ticket>;
    pub(in crate::shell) fn finish(&mut self);
    pub(in crate::shell) fn reset(&mut self);
    // ops and keys methods unchanged
}
/// Landing 3 (in shell/copy/keys.rs). A dropped flight's queued keys depend on a
/// result that will never be applied: discard them, and the deferred copy. A no-op
/// unless the session's pipeline holds `flight`.
pub(in crate::shell) fn drop_copy_flight(copy: &mut Option<CopySession>, flight: Ticket) -> Repaint;

// shell/copy/mod.rs, CopySession (C5): `operation_generation: u64` becomes
/// The rows this session's row-addressed answers are checked against. Re-issued at
/// session entry, when a resize or screen switch moves the rows, when the search is
/// cleared, and at each search dispatch. A search answer carrying another value is not
/// applied; its flight still completes, so the keys queued behind it replay.
rows: Ticket,
// and CopyEntry gains `pub(in crate::shell) rows: Ticket`, so `CopySession::start`
// (the only constructor) takes the session-entry ticket.

// shell/input/scroll_lanes.rs
struct ScrollFlight { ticket: Ticket, queued: Option<usize> }
impl ScrollLanes {
    pub(in crate::shell) fn sent(&mut self, pane: PublicPaneId, flight: Ticket, offset: usize);
    pub(in crate::shell) fn answered(&mut self, pane: &PublicPaneId, flight: Ticket,
                                     confirmed: Option<usize>) -> ScrollAnswer;
    pub(in crate::shell) fn failed(&mut self, pane: &PublicPaneId, flight: Ticket) -> bool;
    /// Drops lanes of panes missing from a new snapshot, flights included. An answer for
    /// a dropped flight finds no lane holding its ticket and is stale.
    pub(in crate::shell) fn retain_panes(&mut self, exists: impl FnMut(&PublicPaneId) -> bool);
}

// shell/input/word_selection.rs
// ClientWordSelection::pending: Option<Ticket>
impl MouseSelection {
    /// Landing 3. Ends the gesture whose row read holds `read`.
    pub(in crate::shell) fn drop_word_read(&mut self, read: Ticket) -> Repaint;
}

// shell/navigation/workspace_navigation.rs
pub(in crate::shell) struct PendingWorkspaceHighlight {
    pub(in crate::shell) target: PinnedLocation,
    ticket: Ticket,
    expires_at: Instant,
}
impl PendingWorkspaceHighlight {
    /// Clears `slot` when it holds `ticket`.
    pub(in crate::shell) fn release(slot: &mut Option<Self>, ticket: Ticket) -> Repaint;
}
// keep_workspace_highlight_until_snapshot(target, ticket: Ticket, now)

// shell/overlays/rename.rs (C7), RenameTarget::NewWorkspace::label_lookup: Option<Ticket>
// RenameOverlay::apply_checkout_root(&mut self, lookup: Ticket, ..) -> Repaint: a no-op
// unless the prompt holds `lookup`.
// shell/overlays/mod.rs
pub(in crate::shell) fn drop_label_lookup(overlay: &mut Option<Overlay>,
                                          lookup: Ticket) -> Repaint; // landing 3
```

Feature entry points after landing 2 (signature changes only; bodies as today
with the token comparison swapped):

- `complete_copy_motion(&mut self, flight: Ticket, pane_id, origin, result,
  outcome) -> Repaint`
- `complete_copy_search(&mut self, flight: Ticket, rows: Ticket, pane_id,
  origin, query, direction, repeat, result, outcome) -> Repaint`
- `apply_copy_search_result(..., rows: Ticket, result, outcome) -> bool`
- `cancel_deferred_copy_after_search(&mut self, rows: Ticket)`
- `answer_pane_scroll(&mut self, flight: Ticket, pane_id, result, outcome)`
- `complete_word_selection_row(&mut self, read: Ticket, pane_id, row, result,
  now, outcome)`
- `complete_workspace_label_lookup(&mut self, lookup: Ticket, result)`

### 5.3 Textlint (landing 2)

```toml
# Request identity in the client shell belongs to the ledger. A feature waiting for
# an answer holds the ticket its Work carries back, never the request id, so a reset
# of the feature cannot leave a matching id behind.
[[textlint]]
name = "shell-request-identity-is-the-ledgers"
pattern = '\bRequestId\b'
paths = ["crates/shepr-client/src/shell/**/*.rs"]
exclude = ["crates/shepr-client/src/shell/ledger.rs", "**/tests/**", "**/tests.rs"]
skip_after = '^[ \t]*#\[cfg\(test\)\]'
region = "code"
message = "shell features hold the ledger's Ticket, not a RequestId; only ledger.rs knows request ids"
```

### 5.4 `endpoint/hub.rs` (landing 4)

```rust
/// The client's side of every endpoint: connections, command lanes, reconnect
/// supervisors, and the move between presentations. The only production code that
/// transitions the endpoint choice; the shell reads what is presented and never moves it.
pub(crate) struct EndpointHub {
    registry: EndpointRegistry,
    commands: commands::EndpointCommands,
    supervisors: EndpointSupervisors,
    local_failure_policy: LocalFailurePolicy,
}

/// What a reconcile or a supervisor event asks the loop to do to the host, in order.
pub(crate) enum HubEffect {
    /// Show this endpoint notice and present the chrome.
    Notice(shell::EndpointNotice),
    /// Drop the host modes, title and report-all a lost or retired endpoint requested.
    ClearHostEffects,
    ChromeDirty,
    /// A move committed: discard the blit baseline and compose the pane.
    Committed,
}

/// How one inbound message relates to the presentation.
pub(crate) enum Admission {
    /// A stale generation, a role the gate drops, or move evidence the hub kept.
    Consumed,
    /// For the loop to apply. A snapshot from the move's target was also kept as evidence.
    Present { message: Box<DecodedClientServerMessage>, role: ConnectionRole },
}

pub(crate) struct Dispatched {
    pub(crate) repaint: shell::Repaint,
    /// Clipboard writes, in action order, for the loop to forward to the host.
    pub(crate) clipboard: Vec<Vec<u8>>,
}

impl EndpointHub {
    pub(crate) fn new(registry: EndpointRegistry, supervisors: EndpointSupervisors,
                      local_failure_policy: LocalFailurePolicy) -> Self;

    // Loop timing and connections
    pub(crate) fn next_deadline(&mut self, shell: &ClientShellState, now: Instant) -> Option<Instant>;
    pub(crate) fn spawn_due(&mut self, now: Instant, options: impl Fn() -> EndpointConnectOptions,
                            events: &tokio::sync::mpsc::Sender<ClientLoopEvent>);
    pub(crate) fn supervisor_event(&mut self, shell: &mut ClientShellState,
                                   event: EndpointSupervisorEvent, now: Instant) -> Vec<HubEffect>;
    pub(crate) fn disconnected(&mut self, endpoint_id: &ClientEndpointId, generation: u64,
                               error: &io::Error);
    pub(crate) fn fail(&mut self, endpoint_id: &ClientEndpointId, error: &io::Error);
    pub(crate) fn ends_client_for(&self, endpoint_id: &ClientEndpointId) -> bool;
    pub(crate) fn tick_health(&mut self, now: Instant);
    pub(crate) fn settle_expired(&mut self, shell: &mut ClientShellState, now: Instant) -> ClientShellInput;

    // The move
    pub(crate) fn reconcile<'t>(&mut self, shell: &mut ClientShellState,
                                baseline: impl FnOnce(&ClientShellState) -> HostBaseline<'t>,
                                now: Instant) -> Result<Vec<HubEffect>, LoopExit>;
    pub(crate) fn admit(&mut self, shell: &mut ClientShellState, endpoint_id: &ClientEndpointId,
                        generation: u64, message: Box<DecodedClientServerMessage>) -> Admission;
    pub(crate) fn complete_command(&mut self, endpoint_id: &ClientEndpointId, generation: u64,
                                   boot_id: &BootId, request_id: &RequestId,
                                   result: Result<EndpointReply, EndpointError>)
                                   -> Option<commands::EndpointCommandResult>;
    pub(crate) fn install_snapshot(&mut self, shell: &mut ClientShellState,
                                   endpoint_id: &ClientEndpointId,
                                   snapshot: Box<ClientShellSnapshot>, role: ConnectionRole)
                                   -> Option<SnapshotDirty>;  // Pane | Chrome; None: connection gone
    pub(crate) fn requests_lost(&mut self, shell: &mut ClientShellState,
                                endpoint_id: &ClientEndpointId,
                                status: EndpointFailureStatus) -> (Lost, shell::Repaint);

    // Shell output
    pub(crate) fn dispatch(&mut self, shell: &mut ClientShellState,
                           actions: Vec<ClientShellAction>, now: Instant) -> Dispatched;
    pub(crate) fn send_shown(&mut self, shell: &ClientShellState, message: &ClientMessage);
    pub(crate) fn send_viewed(&mut self, message: &ClientMessage);
    pub(crate) fn resize_views(&mut self, shell: &mut ClientShellState, geometry: TerminalGeometry);
    pub(crate) fn detach(&mut self, shell: &ClientShellState);
}
```

`admit` reads the choice for `role` and `accepts_response` and writes the
`Preparing` for buffering, hence `&mut ClientShellState`. It returns
`Consumed` for a message whose generation the registry does not accept, for a
gate `Drop`, and for a gate `Buffer` after feeding the patch, surface or move
response into `preparing_mut()`; for `ApplyAndBuffer` (a target snapshot) it
records the snapshot into `preparing_mut()` and returns `Present`.

`reconcile` runs today's sequence in today's order (failures, rejection or
deadline, `view::start_move`, `view::send_focus`, `view::commit_move` with
`retire_lane` and `send_next`, `view::release_unwanted`), pushing effects where
today's code presents a notice, clears host effects, marks the chrome dirty or
requests a repaint; a failure the local policy ends the client for returns
`Err(LoopExit::ConnectionLost(..))` at once, as today. Its private
`endpoint_lost` is: supervisors `record_status`; `shell.set_machine_diagnostic`;
`shell.endpoints.choice.connection_lost(id)`; `requests_lost`'s lane-first drop
(landing 1); then the `Lost` match into effects.

Changes in the shell for landing 4 (`shell/endpoints.rs`):

```rust
/// The presented endpoint the shell can switch to, read before the choice commits.
pub(crate) struct EndpointProjection {
    endpoint_id: ClientEndpointId,
    snapshot: Arc<ClientShellSnapshot>,
    generation: u64,
    switching: bool, // endpoint_id != presented() when read
}
impl ClientShellState {
    /// `None` unless the endpoint is usable and has a snapshot and generation.
    pub(crate) fn endpoint_projection(&self, endpoint_id: &ClientEndpointId) -> Option<EndpointProjection>;
    /// Everything `activate_endpoint_projection` does after the choice commit, as C's
    /// landing 6 (which lands before this spec's landing 4) leaves it: surfaces cleared
    /// on a switch, `apply_active_snapshot`, then `sidebar_scroll.endpoint_activated`.
    /// (C6 removed the agent-scroll save and restore; the switch keeps the agent start
    /// because `ActiveProjection::accept` reports `Replaced(EndpointSwitched)`.)
    pub(crate) fn present_projection(&mut self, projection: EndpointProjection);
    /// A live connection was lost: the endpoint takes its failure status and, when it is
    /// presented, every request still in the ledger is interrupted. The choice is the
    /// hub's to update.
    pub(crate) fn endpoint_failed(&mut self, endpoint_id: &ClientEndpointId, status: EndpointFailureStatus);
}
// cfg(test), placed after the file's first #[cfg(test)]: activate_endpoint_projection
// keeps its name and exact behaviour (commit a preparing move to `id`, or show `id` when
// no move is pending, else false; then present) for the shell tests that use it.
```

`view::commit_move` becomes: `ready()` surface; lease and viewed checks;
`endpoint_snapshot_matches` (else `LostPair`); `shell.endpoint_projection(target)`
(else `ProjectionUnavailable`); `shell.endpoints.choice.commit()`;
`shell.present_projection(projection)`; `receive_pane_surface_from`; send
focus baseline and `ReplayHostEffects`. The checks all precede the commit, as
today.

Textlint (landing 4):

```toml
# The endpoint move protocol has one driver, crate::endpoint. Elsewhere the choice is
# read, never transitioned.
[[textlint]]
name = "endpoint-moves-are-driven-from-the-endpoint-module"
pattern = '\bchoice\s*\.\s*(?:select|connection_lost|abandon|begin_preparing|preparing_mut|fail_move|commit)\s*\('
paths = ["crates/shepr-client/src/**/*.rs"]
exclude = ["crates/shepr-client/src/endpoint/**", "**/tests/**", "**/tests.rs"]
skip_after = '^[ \t]*#\[cfg\(test\)\]'
region = "code"
message = "only crate::endpoint (the hub and its view steps) transitions the endpoint choice"
```

### 5.5 `ClientLoop` (landing 5)

```rust
// client_loop/mod.rs
pub(crate) struct LoopSignals {
    pub(crate) should_quit: Arc<AtomicBool>,
    pub(crate) fatal: Arc<fatal_panic::FatalPanic>,
}
pub(crate) struct EventQueue {
    pub(crate) tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
    pub(crate) rx: tokio::sync::mpsc::Receiver<ClientLoopEvent>,
}
/// The host's cell size as it reports it, and whether launch asked it to.
pub(crate) struct HostCellReport {
    pub(crate) size: Arc<AtomicCellSize>,
    pub(crate) queried: bool,
}
pub(crate) struct ClientLoop {
    state: ClientState,
    hub: endpoint::EndpointHub,
    signals: LoopSignals,
    events: EventQueue,
    cell: HostCellReport,
}
impl ClientLoop {
    pub(crate) fn new(state: ClientState, hub: endpoint::EndpointHub, signals: LoopSignals,
                      events: EventQueue, cell: HostCellReport) -> Self;
    pub(crate) async fn run(&mut self) -> Result<(), LoopExit>;
    pub(crate) fn handle_event(&mut self, event: ClientLoopEvent, now: Instant)
        -> Result<ClientLoopAction, LoopExit>;
    pub(crate) fn reconcile(&mut self, now: Instant) -> Result<(), LoopExit>;
    fn apply_hub_effects(&mut self, effects: Vec<HubEffect>) -> Result<(), LoopExit>;
}
#[cfg(test)]
impl ClientLoop {
    pub(crate) fn state(&self) -> &ClientState;
    pub(crate) fn state_mut(&mut self) -> &mut ClientState;
    pub(crate) fn hub(&self) -> &endpoint::EndpointHub;
    pub(crate) fn hub_mut(&mut self) -> &mut endpoint::EndpointHub;
}
// client_loop/dispatch.rs: `impl ClientLoop { pub(crate) fn handle_server_message(..) }`
// state.rs: `impl ClientState { pub(super) fn present_notice(&mut self, notice: &EndpointNotice) }`
// endpoint/hub.rs, cfg(test): `for_registry(registry) -> Self` (no supervisors,
// Reconnect policy), `registry(&self)`, `registry_mut(&mut self)`, `commands_mut(&mut self)`.
```

## 6. Out of scope (stopping rule)

- `RequestId`'s text representation and string constructors (TYP-007).
- The command lane's semantics: one command in flight per endpoint, and
  abandoning a copy read does not free the lane (finding F6).
- `PresentationGate`'s classification table, the view and focus wire
  messages, and all of `shepr-server` (its netside test is not edited).
- Where `EndpointChoice` is stored, and every other field placement in
  `ClientShellState` (spec C, STR-042).
- `ClientState` and the presenter (declined in STR-039 with its reason).
- The test layout beyond the edits each landing needs (STR-049).
- Snapshot and surface ordering checks (`apply_active_snapshot`'s revision
  check, `receive_tagged_pane_surface`'s generation check): ordering, not
  request staleness (STR-042).

## 6a. Landing order with spec C

This spec's landings interleave with C's (`notes/spec-shell-render.md`) in
one order, recorded identically in both specs. D*n* is this spec's landing
*n*, C*n* is C's:

| step | landing | why here |
|---|---|---|
| 1 | D1 lost endpoint drops lane-first | bug fix; touches only the endpoint-loss path, nothing C moves |
| 2 | C1 sidebar resolution | bug fixes |
| 3 | C2 overlay layout and views | bug fixes |
| 4 | C3 three-phase compose | bug fix; `ShellView`, `Presentation` |
| 5 | C4 `ModeState` | no overlap with this spec's fields |
| 6 | C5 `CopySession`, selection reactions, `Pointer` | defines the copy shapes the tokens go into |
| 7 | C7 one module per overlay | defines `RenameTarget` and `RenameOverlay::apply_checkout_root` |
| 8 | D2 continuation tokens | written once against the C5 and C7 shapes |
| 9 | D3 rollback type rule | `Rollback` written once against those shapes |
| 10 | C6 `ActiveProjection` and transitions | written once against tickets and `Rollback` |
| 11 | C8 notices, config, text helpers | independent |
| 12 | D4 endpoint hub | `present_projection` written once against C6's activation |
| 13 | D5 `ClientLoop` closed | |
| 14 | C9 visibility pass | needs the final tree, including the hub and `client_loop/` |

This replaces this spec's earlier recommendation that landings 2 and 3 go
before C restructures the owners. That recommendation rested on landings 2
and 3 only changing field types, and they do more: landing 2 replaces
`Work`'s variants, changes `submit`'s return type and seven feature entry
points, moves the copy staleness check onto tickets and adds `Work::Focus`;
landing 3 adds a drop function per feature and the `Rollback` struct, whose
copy fields C5 would fold into one. Landed after C5 and C7, each of those
edits is written once. They must still precede C6: C6 turns
`presented_surface_changed` into free functions that need the ledger to
re-issue `rows`, and its reset-order argument is the `Rollback` field list.

What the order changes in this spec's bricks:

- Paths follow C5 and C7: `shell/input/copy_mode.rs` is `shell/copy/keys.rs`
  (key code, requests, completions) plus `shell/copy/pipeline.rs`
  (`CopyPipeline`); `ClientCopyModeState` is `CopySession` in
  `shell/copy/mod.rs`; `ClientRenameTarget` is `RenameTarget` in
  `shell/overlays/rename.rs`; `open_new_workspace_overlay` and
  `complete_workspace_label_lookup` (a forwarder to
  `RenameOverlay::apply_checkout_root`) are in `shell/overlays/mod.rs`;
  `ClientShellOverlay` is `Overlay`; `copy_mode`/`copy_pipeline` accesses are
  `self.copy` and `session.pipeline()`/`pipeline_mut()`.
- `presented_surface_changed` is still a `&mut self` method in `state.rs` when
  landing 2 lands (C6 makes it free functions later), so brick 2's
  `rows = self.ledger.ticket()` goes there, on the session.
- `submit` returns `Submitted` from landing 2 on; C7's openers and
  `apply_overlay_effect` already call it from `impl ClientShellState`, and
  landing 2 updates them like every other caller.
- Landing 4's `present_projection` has no agent-scroll restore to carry: C6
  removed it.

## 7. Landing 1: the lost endpoint's requests drop lane-first

The bug (finding F1): `ClientLoop::endpoint_lost` in `reconcile.rs` calls
`shell.transition_endpoint_status`, whose `drop_all_requests(Interrupted)`
drops every ledger entry, before `endpoint_commands.disconnect(id)`, whose
precise `unsent` / `possibly_sent` split then finds nothing. A command still
queued behind the in-flight one was never sent but is dropped as interrupted.

Bricks:

1. `shell_runtime.rs`: add

   ```rust
   /// Ends every request of a lost connection. The lane's account comes first: a command
   /// still queued behind the in-flight one was never sent and drops as unsent, the
   /// in-flight one as interrupted. Only then is the endpoint marked failed, whose
   /// blanket interruption covers anything the lane did not hold.
   pub(super) fn endpoint_requests_lost(
       shell: &mut shell::ClientShellState,
       endpoint_commands: &mut endpoint::commands::EndpointCommands,
       endpoint_id: &endpoint::ClientEndpointId,
       status: endpoint::EndpointFailureStatus,
   ) -> (endpoint::Lost, shell::Repaint)
   ```

   body: `disconnect`, `cancel_endpoint_commands`, then
   `shell.transition_endpoint_status`.
2. `reconcile.rs` `endpoint_lost`: replace the transition, disconnect and
   cancel lines with one call; mark the chrome dirty when the repaint is needed.
3. Test `a_lost_endpoint_drops_its_queued_commands_as_unsent` in
   `shell/tests/endpoint_requests.rs`: ready shell; `start_label` (checkout-root
   read) then `handle_input_bytes(b"\r")` (a `Plain` workspace create); enqueue
   both actions on an `EndpointCommands` with generation 1 and `send_next` on
   `EndpointRegistry::new(TestTransport { fail: false }, 1)` so the read is in
   flight and the create queued; call `endpoint_requests_lost(.., Local,
   Reconnecting)`; assert the ledger is empty and `notices.visible()` is
   `None`. Second half: a lone `Plain` rename sent and in flight
   (`pending_request`), the same call, assert the visible notice title is
   "Action interrupted".

Gates:

- Seen failing first, with brick 1's body in today's order (transition before
  disconnect): `brokkr test -p shepr-client a_lost_endpoint_drops_its_queued_commands_as_unsent`
- `brokkr check`

## 8. Landing 2: continuation tokens (STR-045, tokens)

One compile unit; bricks in edit order.

1. `shell/ledger.rs`: add `Ticket`, `Ticket::fixture` (cfg(test)),
   `Ledger::ticket` with its own `next_ticket` counter (the existing `next`
   serial, `issued_at`, `mark` and the orphan guard stay until landing 3); move
   `ClientShellEndpointRequest` and its `Debug` here from `shell/state.rs` and
   re-export it from `shell/mod.rs` via `ledger`; replace `Work` with section
   5.1's variants; `accepts_reply` and `reports_interruption`; `submit`
   returns `Submitted`; `push_endpoint_command` ignores it; `answer_request`
   loses its `release_highlight` call; `dropped_entry` loses its
   `release_highlight` call and uses `reports_interruption`; delete
   `release_highlight` and `Ledger::work`; `Work::answered` and
   `Work::dropped` lose their `request` parameter (`dropped` still takes
   `&mut ClientShellState` in this landing, and calls the feature drop
   functions below with tickets); rewrite the `DropReason::Interrupted` doc
   (section 5.1).
2. `shell/overlays/rename.rs`: `RenameTarget::NewWorkspace::label_lookup:
   Option<Ticket>`. `shell/copy/mod.rs`: `CopySession::rows: Ticket` replaces
   `operation_generation` with section 5.2's doc, and `CopyEntry` gains
   `rows`. `shell/state.rs`: in `presented_surface_changed` replace the
   generation increment with `session.rows = self.ledger.ticket();` (a field
   path, so it borrows beside `self.copy`); delete
   `ClientShellEndpointRequest`.
3. `shell/copy/pipeline.rs`: `CopyFlight` and the `CopyPipeline` API of
   section 5.2 (`is_awaiting` and `awaiting` deleted). `shell/copy/keys.rs`:
   `enter_copy_mode` passes `rows: self.ledger.ticket()` in its `CopyEntry`;
   `CancelOrClear` sets `copy_mode.rows =
   self.ledger.ticket()`; `dispatch_next_copy_operation` mints `let flight =
   self.ledger.ticket();` per operation, and for a search sets `copy_mode.rows
   = self.ledger.ticket()` and carries it; on `Submitted::Opened` calls
   `begin(flight, search_rows)` (`Some(rows)` for a search), on `Refused` keeps
   today's branch; `complete_copy_motion` and `complete_copy_search` take
   `flight` (and `rows`) and use `holds(flight)` at both checks;
   `apply_copy_search_result` and `cancel_deferred_copy_after_search` compare
   `rows`; `defer_copy_until_search_result` becomes
   `self.copy_pipeline.awaited_search_rows() == Some(copy_mode.rows) ||
   self.copy_pipeline.has_queued_search()` (the pane match today's code makes
   is implied: every session change resets the pipeline, so a held flight is
   always the current session's); `drop_copy_operation(flight)`;
   `abandon_copy_operation`'s doc: "The request stays in the ledger; its
   answer, if one ever comes, finds no flight holding its ticket."
4. `shell/input/scroll_lanes.rs`: `ScrollFlight::ticket`; `sent`, `answered`,
   `failed` take `Ticket`; `retain_panes` doc per section 5.2; unit tests use
   `Ticket::fixture`.
5. `shell/input/mouse.rs`: `dispatch_pane_scroll` mints `flight`, submits
   `Work::PaneScroll { pane_id, flight }`, `sent(pane_id, flight, offset)` on
   `Opened`, `send_failed` on `Refused`; `answer_pane_scroll(flight, ..)`;
   `drop_pane_scroll(pane, flight)`.
6. `shell/input/word_selection.rs`: `pending: Option<Ticket>`;
   `request_word_selection_row` mints `read`, submits `Work::WordSelection {
   pane_id, row, read }`, sets `pending = Some(read)` on `Opened`, cancels on
   `Refused`; `drop_word_selection(read)`; `complete_word_selection_row(read,
   ..)`.
7. `shell/overlays/mod.rs`: `open_new_workspace_overlay` mints `lookup` when
   it will submit, stores `Some(lookup)` on `Opened`;
   `complete_workspace_label_lookup(lookup, result)` forwards to
   `RenameOverlay::apply_checkout_root(lookup, ..)` in `rename.rs`, which
   compares the ticket.
8. `shell/navigation/workspace_navigation.rs`: `PendingWorkspaceHighlight {
   target, ticket, expires_at }`; `keep_workspace_highlight_until_snapshot(target,
   ticket, now)`; add `PendingWorkspaceHighlight::release(slot, ticket)`.
9. `shell/navigation/actions.rs` `focus_endpoint_target`: `let highlight =
   self.ledger.ticket();` submit `Work::Focus { highlight }`; on `Opened`, for
   a workspace target with a `navigation_target`, keep the highlight with
   `highlight`. `request_selection_copy` ignores `Submitted`.
10. `brokkr.toml`: add `shell-request-identity-is-the-ledgers` (section 5.3).
11. Tests, edits:
    - Every test `CopyEntry` sets `rows: Ticket::fixture(1)`. After C5 those
      are the four sites that once built `ClientCopyModeState` field by field
      (`shell/input/mod.rs` tests, `presentation/surface_patch.rs` tests,
      `view/draw.rs` tests, `shell/tests/copy.rs`), now built through
      `CopySession::start` and its test builders. (The `surface_patch.rs` and
      old `composition.rs` sites were missing from this list before; without
      them the landing does not compile.)
    - `shell/input/mod.rs` tests: both
      `begin("test-request".into())` on the session's pipeline become
      `begin(Ticket::fixture(2), None)`.
    - `shell/tests/copy.rs`: in `cancelling_an_old_copy_request_does_not_reset_a_new_session`
      delete the `is_awaiting(&current_id)` assertion (the next three assert
      the same).
    - `shell/tests/text_editing.rs`: `open_new_workspace` returns `(RequestId,
      Ticket, String)`.
    - `shell/tests/endpoint_requests.rs`: in
      `a_dropped_request_runs_its_rollback_and_sends_nothing`,
      `answering_a_request_twice_applies_it_once` and
      `only_a_plain_request_shows_the_interruption_notice_and_only_when_it_may_have_been_sent`,
      `submit(..)` is asserted `Opened` and the id comes from
      `request_id(&out.actions)`; in `an_ignored_answer_still_reports_its_server_error`
      replace each `is_awaiting(&current)` with `in_flight()` plus
      `ledger.contains(&current)`; `a_word_selection_answer_for_a_replaced_gesture_is_ignored`
      replaces its two `drop_word_selection(&id)` calls with `drop_request(&old,
      Unsent)` (gesture still present) then `drop_request(&current, Unsent)`
      (gesture gone, repaint needed); `a_workspace_label_answer_for_a_reopened_overlay_is_ignored`
      matches `label_lookup: Some(_)` and then answers `current` and asserts the
      lookup is `None`; `a_failed_focus_releases_only_its_own_highlight` asserts
      the highlight is still `Some` after the old focus times out, `None` after
      `current` drops. Rename
      `only_a_plain_request_shows_the_interruption_notice_and_only_when_it_may_have_been_sent`
      to `only_state_changing_requests_show_the_interruption_notice_and_only_when_they_may_have_been_sent`
      and run it over `Plain`, `Focus { highlight: ledger.ticket() }` and
      `SelectionCopy` (notice for the first two).
12. Tests, new:
    - `tickets_are_never_reissued` (`shell/ledger.rs` tests): a thousand
      tickets are pairwise distinct and increasing.
    - `a_scroll_answer_for_a_lane_rebuilt_after_its_pane_left_is_ignored`
      (`endpoint_requests.rs`): start a scroll (id A); `set_snapshot` without
      the pane (lane dropped by `retain_panes`); `set_snapshot` with it; start a
      scroll (id B); answer A with `scroll_reply(3)`: lane still in flight and
      no action; answer B: lane settles.
    - `an_in_flight_search_answered_after_a_resize_finishes_without_applying`
      (`endpoint_requests.rs`): `copy_shell`; `copy_search` (id); queue a key
      with `handle_input_bytes(b"l")`; present a surface whose copy pane
      `inner_rect.width` is one less; answer id with one match at another
      point: no match is stored, the cursor is unchanged, the pipeline is not
      in flight, and the queued key was replayed (the keys queue is empty).

Gate: `brokkr check`.

## 9. Landing 3: the rollback type rule (STR-045, drop context)

1. `shell/ledger.rs`: `Rollback` and `ClientShellState::rollback` (section
   5.1); `Work::dropped(self, parts: Rollback<'_>)` per the table in 5.1;
   `dropped_entry(entry, reason)`: interruption notice, then
   `entry.work.dropped(self.rollback())`; delete the orphan guard, `Ledger::mark`,
   `opened_since`, `Entry::issued_at` and the entry serial (the ledger keeps
   only `next_ticket` and `entries`).
2. Feature drops move off `ClientShellState` onto the state they restore:
   `drop_copy_flight(copy, flight)` (`copy/keys.rs`, replaces
   `drop_copy_operation`; the session pipeline's `reset` and the
   `copy_after_result` clear, as today); `MouseSelection::drop_word_read`
   (word_selection.rs, replaces `drop_word_selection`); `ScrollLanes::failed`
   used directly (delete `drop_pane_scroll`); `drop_label_lookup`
   (`overlays/mod.rs`, delegating to the rename overlay; clears a matching
   `label_lookup`, returns `Unchanged` as the `None` answer path does today);
   `PendingWorkspaceHighlight::release` (already added).
3. Tests: `a_dropped_request_runs_its_rollback_and_sends_nothing` drops its two
   `ledger.mark()` lines and comment; its `ledger.len() == count - 1`
   assertion now detects any orphan, since nothing removes one;
   `a_word_selection_answer_for_a_replaced_gesture_is_ignored` already goes
   through `drop_request` (landing 2).

Gate: `brokkr check`.

## 10. Landing 4: the move protocol's owner (STR-039, part 1)

1. `shell/endpoints.rs`: `EndpointProjection`, `endpoint_projection`,
   `present_projection`; `activate_endpoint_projection` moves into the
   existing trailing cfg(test) `impl ClientShellState` with its exact current
   behaviour, built on the two; `transition_endpoint_status` becomes
   `endpoint_failed(id, status)` without `connection_lost`.
2. `endpoint/view.rs`: `commit_move` commits the choice itself (section 5.4);
   doc comments say the hub sequences these steps.
3. `endpoint/hub.rs` (new, `mod hub; pub(crate) use hub::{EndpointHub,
   HubEffect, Admission, Dispatched};` in `endpoint.rs`): section 5.4. Moves
   in, as methods or private helpers: from `shell_runtime.rs`
   `cancel_endpoint_commands`, `settle_expired_endpoint_commands`,
   `input_endpoint`, `dispatch_client_shell_actions` (as `dispatch`,
   infallible, clipboard writes returned), `waiting_notice`, `resize_views`
   (geometry passed in), `install_client_shell_snapshot` (with the
   `mark_ready`), `endpoint_requests_lost` (as `requests_lost`, calling
   `choice.connection_lost` first, then the landing 1 order); from
   `reconcile.rs` the bodies of `reconcile`, `fail_move`, `endpoint_lost`;
   from `client_loop.rs` `update_endpoint_status_presentation` and the bodies
   of `handle_endpoint_supervisor` and `handle_server_disconnected`; from
   `dispatch.rs` the generation check, role, `move_response`, gate and every
   `preparing_mut()` buffering branch (into `admit`), and the lane completion
   (into `complete_command`). Drop the unreachable "endpoint not active"
   branches (finding F3): `dispatch.rs` always answers a completed command, and
   `settle_expired` drops as interrupted only when the registry no longer
   accepts the generation.
4. `client_loop.rs`: replace `local_failure_policy`, `write_stream`,
   `supervisors`, `endpoint_commands` with `hub: endpoint::EndpointHub`
   (`pub(crate)` until landing 5); `next_timer_deadline` is the shell's
   deadline and `hub.next_deadline`; `handle_timer` is `hub.tick_health`, then
   `shell.tick_timers`, then `hub.settle_expired`, in today's order;
   `reconcile.rs`'s `ClientLoop::reconcile` becomes the hub call plus
   `apply_hub_effects` (section 5.5) plus `present_pending`. `ClientLoop::new`
   takes the hub in place of its `local_failure_policy`, `write_stream` and
   `supervisors` arguments (eight arguments until landing 5).
5. `shell_runtime.rs`: `finish_client_shell_input(state, outcome, hub, now)`:
   detach via `hub.detach`, resize via `hub.resize_views`, actions via
   `hub.dispatch` then the returned clipboard writes (each through
   `write_clipboard_bytes`, warning on failure as today), theme via
   `hub.send_viewed`, shown requests via `hub.send_shown`.
6. `dispatch.rs`: `handle_server_message` starts with `match
   self.hub.admit(..)`; the `Present` arms are today's host-side arms, with
   `hub.fail`, `hub.ends_client_for`, `hub.complete_command` and
   `hub.install_snapshot` where they reached the registry or lanes.
7. `launch.rs`: builds `EndpointHub::new(write_stream, supervisors,
   local_failure_policy)` and passes it.
8. `brokkr.toml`: add `endpoint-moves-are-driven-from-the-endpoint-module`
   (section 5.4).
9. Tests: `tests/endpoint_choice.rs`'s `Fixture` builds the hub; its uses of
   `client.write_stream`, `client.endpoint_commands` and
   `dispatch_client_shell_actions(..)` become `client.hub` methods and the
   cfg(test) accessors; `endpoint/view.rs` tests follow the fixture;
   `shell/tests/endpoint_requests.rs` tests that built a registry and an
   `EndpointCommands` build `EndpointHub::for_registry` and use `dispatch`,
   `settle_expired`, `requests_lost` and `commands_mut()`; `client_loop.rs`'s
   `test_client_loop` passes a hub. New test
   `a_commit_whose_projection_is_unavailable_leaves_the_move_preparing`
   (`endpoint/view.rs` tests): `Fixture::start`, `evidence`, then mark the
   target failed with `set_endpoint_status(remote, Reconnecting)`;
   `commit_move` returns `Err(ProjectionUnavailable)` and
   `choice.preparing()` is still `Some` (pins "every check precedes the
   commit" now that the commit moved out of the shell).

Gate: `brokkr check` (it runs the unchanged netside test).

## 11. Landing 5: `ClientLoop` closed (STR-039, part 2)

1. Move `client_loop.rs` to `client_loop/mod.rs` and `dispatch.rs` to
   `client_loop/dispatch.rs` (`mod dispatch;` in `mod.rs`); delete
   `reconcile.rs`, moving `apply_hub_effects` into `client_loop/mod.rs` and
   `present_notice` into `state.rs` as `ClientState::present_notice`
   (`launch.rs` and the hub effect applier call it); update `lib.rs`'s `mod`
   list.
2. Section 5.5: private fields, `LoopSignals`, `EventQueue`, `HostCellReport`,
   five-argument `new`, cfg(test) accessors, `reconcile` `pub(crate)` (the
   fixture calls it).
3. `launch.rs` builds the parts.
4. `brokkr.toml`: in `client-clock-is-injected`'s `exclude` and in
   `client-loop-clock-is-sampled`'s `paths`, replace
   `crates/shepr-client/src/client_loop.rs` with
   `crates/shepr-client/src/client_loop/mod.rs` (`client_loop/dispatch.rs`
   reads no clock and stays under the injected rule).
5. Tests: `client_timer_tests` (a child module, so it may keep reading
   `signals.fatal` and `events.rx`); `tests/endpoint_choice.rs` and
   `endpoint/view.rs` tests read `client.state()`, `client.state_mut()`,
   `client.hub_mut()`.

Gate: `brokkr check`.

## 12. Test strategy

- Every landing is gated by `brokkr check`, which runs every named test above,
  the textlints, clippy and the shepr-server netside test that drives the
  public move steps against two in-process servers.
- The one fail-first run is landing 1's, because the bug's only observable is a
  notice the next step of the real loop overwrites (finding F1); the test
  therefore drives the extracted function, not the whole reconcile.
- The type rule of landing 3 has no runtime test; the compiler is the gate.
  The rollback test's exact ledger count catches any rollback that opens a
  request through some future route.
- The two textlints pin the ownership claims: no `RequestId` in shell feature
  code, no choice transition outside `crate::endpoint`.
- Optional, not a gate: before landing 2's code change,
  `brokkr check --textlint shell-request-identity-is-the-ledgers` shows the
  rule matching the five fields.

## 13. Risks

- Spec C edits the same files (`shell/state.rs`, `copy_mode.rs`,
  `overlay_input.rs`, `word_selection.rs`, `workspace_navigation.rs`,
  `mouse.rs`). Section 6a fixes the order: landings 2 and 3 follow C5 and C7
  and precede C6, so they are written against C's copy and rename shapes,
  and C6 is written against tickets and `Rollback`.
- A ticket minted but not recorded after `Refused` is harmless (never held);
  a ticket recorded on `Refused` would be a held flight no answer can settle.
  Every brick records on `Opened` only.
- Borrows: minting with `self.ledger.ticket()` while `self.copy_mode.as_mut()`
  is live works through field paths; a helper method on `self` would not.
- `Focus` changes which work releases the highlight: an error answer or any
  drop of the focus request, as today, but no longer any other request with an
  equal id (there was none).
- Effects returned by `EndpointHub::reconcile` are applied after the whole
  sequence, so a failing `clear_endpoint_host_effects` now surfaces after the
  start, focus, commit and release steps of the same turn ran instead of
  before; the error ends the client either way.
- `admit` must keep today's ordering for a target snapshot: evidence first,
  then the cache install by the loop. Restore and saves-stopped notices stay in
  the loop, before the install.
- The netside test depends on `view::commit_move` committing and presenting in
  one call and on `ClientShellState::endpoint_choice(_mut)`; both are kept.
- The choice-transition textlint is line based; a transition split across lines
  outside `crate::endpoint` would pass it. Reviewers own that gap.

## 14. Interface with spec C (`notes/spec-shell-render.md`)

Assumptions this spec makes of C:

1. Every feature-state owner a drop restores stays a separately borrowable
   field of `ClientShellState`, or of a C-owned struct that is itself such a
   field. After C5 and C7, which land before landing 3, those are `copy`
   (`Option<CopySession>`, owning the pipeline), `mouse_selection`,
   `scroll_lanes`, `overlay` (`Option<Overlay>`) and
   `pending_workspace_highlight`; `Rollback` borrows exactly these. None of
   them may contain or reach the `Ledger` or `ClientShellState`.
2. `ledger` stays a field of `ClientShellState`, reachable as `self.ledger`
   from shell code, so features can mint with a field path. Where C turns a
   method into a free function over disjoint fields that must re-issue a
   ticket (C6's `copy::surface_presented` and `transitions::surface_presented`),
   it takes `&mut Ledger` as one more parameter; it only mints.
3. C's reset transitions keep discarding the waiting state they discard today:
   `reset_endpoint_projection` (all of it, after `drop_all_requests(Reset)`),
   `apply_active_snapshot`'s copy reset for a vanished pane and
   `retain_panes`, the surface row invalidation (which must re-issue the
   session's `rows`), `CancelOrClear` (re-issue `rows`), `enter_copy_mode`
   and `exit_copy_mode` (a new or ended session, so a new or no pipeline).
   With tickets, the relative order of a reset and `drop_all_requests` no
   longer matters for correctness: a stale answer finds no held ticket either
   way. C6 still drops requests first, and this spec keeps that order; it
   does not depend on it.
4. `CopySession::start` is the only constructor, and `CopyEntry` carries the
   initial `rows: Ticket` (added by landing 2).
5. C keeps `Endpoints` and its `choice` field inside `ClientShellState`
   (landing 4 relies on `shell.endpoints.choice` and on
   `endpoint_choice(_mut)` for the netside test). If C splits
   `shell/endpoints.rs`, `endpoint_projection`, `present_projection` and
   `endpoint_failed` go with endpoint presentation.
6. C7 makes the rename overlay a module owning its state before landing 2;
   landing 2 gives `RenameOverlay::apply_checkout_root` the `lookup: Ticket`
   match and landing 3 adds the drop with `(lookup: Ticket)`, both no-ops
   unless the open prompt holds `lookup`.
7. `ClientShellEndpointRequest` leaves `shell/state.rs` for `shell/ledger.rs`
   in landing 2; C's `state.rs` no longer defines it.

API this spec offers C:

- `Ticket` (Copy, Eq, Debug; `Ticket::fixture(n)` in tests) and
  `Ledger::ticket()`.
- The rule for any new waiting state: hold the `Ticket` your `Work` carries,
  record it only on `Submitted::Opened`, discard it on reset, never store a
  `RequestId` (the textlint enforces the last).
- `CopyPipeline { in_flight, holds, begin, awaited_search_rows, finish, reset }`
  and `drop_copy_flight`; `CopySession::rows` and `CopyEntry::rows`.
- `ScrollLanes { sent, answered, failed, retain_panes, shown, target, clear }`
  with tickets.
- `MouseSelection::drop_word_read`; `ClientWordSelection::pending:
  Option<Ticket>`.
- `PendingWorkspaceHighlight::release(slot, ticket)` and
  `keep_workspace_highlight_until_snapshot(target, ticket, now)`.
- `Rollback`'s field list as the one place that names what a drop may touch.
- After landing 4, the shell's endpoint surface for presentation code:
  `endpoints.presented()`, `choice.live()`, `endpoint_projection`,
  `present_projection`, `endpoint_failed`; nothing in the shell transitions
  the choice.

## 15. Findings

- **F1 (bug, latent; fixed in landing 1).** A lost presented endpoint drops its
  queued, never-sent commands as `Interrupted` instead of `Unsent`
  (`reconcile.rs` `endpoint_lost` runs `transition_endpoint_status`'s blanket
  drop before `EndpointCommands::disconnect`, whose precise split then finds an
  empty ledger). Rollbacks are identical for both reasons, and the "Action
  interrupted" notice it can push is overwritten at once by the
  connection-lost notice `endpoint_lost` presents for `Lost::Shown`, so no user
  sees it today; the reason is still wrong, and the masking is accidental.
- **F2 (no stale answer is applied today).** Every feature's id comparison is
  correct; the five copies and the two id-mismatch tricks (`abandon_copy_operation`,
  `retain_panes`) work. The latent collision is
  `ClientCopyModeState::operation_generation` restarting at 0 in each
  `enter_copy_mode`, so two sessions reuse generations; it is masked because
  every session change also resets the pipeline's `awaiting`. Tickets remove
  the dependency.
- **F3 (dead branches, stale doc; removed in landing 4, doc in landing 2).** The
  presented endpoint changes only at a move's commit, which runs
  `apply_active_snapshot` with a new boot key and so `reset_endpoint_projection`,
  dropping every ledger entry as `Reset` before any later answer or expiry; the
  commit also retires the old lane. So `dispatch.rs`'s
  "`endpoint_is_active(completed.endpoint_id)` else drop `Interrupted`" and the
  not-active half of `settle_expired_endpoint_commands` never find an entry,
  and `DropReason::Interrupted`'s "its endpoint is no longer the active one"
  clause describes nothing reachable. (The ledger's boot check would reject an
  answer from another server in any case.)
- **F4 (smell; fixed in landing 4).** `activate_endpoint_projection` commits the
  move from inside the shell, and its `EndpointChoice::showing` branch is
  reached only by tests.
- **F5 (asymmetry, kept by decision 4.2).** `CopyMotion` has no row guard, so a
  motion answered across a resize or screen switch is applied; consistent with
  the cursor surviving the same change.
- **F6 (not a defect, noted).** `abandon_copy_operation` frees the copy
  pipeline but not the endpoint command lane: the abandoned read stays the
  lane's in-flight command until answered or `ENDPOINT_COMMAND_TIMEOUT` (60 s),
  and later commands to that endpoint queue behind it. The server handles a
  connection's requests in order, so a stuck read would delay them anyway.
- **F7 (smell; fixed in landing 4).** `dispatch_client_shell_actions` returns
  `Result<Repaint, LoopExit>` but never fails.
- **F8 (smell, noted).** `focus_endpoint_target` builds a throwaway
  `ClientShellInput` and returns only its actions, discarding the repaint
  `submit` recorded (the highlight it took, a NotReady notice). Its one caller
  sets `Repaint::Needed` regardless, so nothing is lost today.
- **F9 (smell, noted).** `ScrollLanes::sent` replaces the whole lane, flight
  included; it is correct only because callers send solely when no flight
  exists (`want` returned `Send`, or `answered` already took the flight).
- **F10 (smell, noted).** `ClientShellState::endpoint_choice_mut()` is `pub`
  only for the shepr-server netside test, and exposes every choice transition
  to any crate.
- **Hunt corrections.** STR-045 says `operation_generation` is a token "only
  for deferred copy-after-search"; it also decides whether a search answer is
  applied at all (`apply_copy_search_result`), and is bumped on resize or screen
  switch, `CancelOrClear` and each search dispatch. STR-045's list of
  endpoint-layer id holders omits `EndpointRegistry::release_unwanted_views`
  (fire-and-forget view-off ids). STR-039 is accurate as written (ten
  arguments, every field `pub(crate)`, the eight files).
