# Technical implementation spec: one request ledger and an explicit surface baseline

Written against `reference/technical-implementation-spec.md` (the contract this
document must satisfy). Spawned from item 2 of `notes/work.md` ("Client shell: a
request ledger and an explicit surface baseline", formerly CSHELL-018 and
CSHELL-019). It resolves rejected candidate REJ-011 in
`notes/bugs-rejected-candidates.md`, which that item names as a consequence, and
it settles the question the landed spec for item 1 left open: whether the
command-lane tombstones survive (they do not; section 3.9).

The client shell tracks "what is in flight" in nine places and "what the server
last drew" in three. The in-flight state is a request map plus seven side maps,
queues and counters that each feature unwinds by hand on every exit path. The
surface state is a displayed surface, a parked "future" surface, a generation
tag and a third copy inside the reader, with revision rules written out three
times. Both shapes can represent states that are bugs: a queued scroll offset
with nothing in flight, a patch refused because the shell parked a surface the
reader had already accepted. This spec replaces them with two shapes that cannot.

Landing A: the shell models the surface as a server baseline that every patch
applies to (mirroring the reader) and, separately, a presented pair chosen by
matching the baseline to the snapshot. REJ-011 cannot occur, and
`Applied`/`Rejected` regain their meaning.

Landing B: one ledger whose entries own both completion and rollback. A request
leaves the ledger through exactly one function, whose rollback half has no way to
send anything. Every feature that kept its own counter, queue or in-flight flag
keeps a ledger request id instead, in one object that drops its queued work with
its in-flight marker.

Nothing in the originating item is deferred. What the survey added is bricks
below: the command-lane tombstone removal (a decision the previous spec handed to
this item), the typed patch rejection reason, and the removal of the
`ClientShellEndpointError::Cancelled` pseudo-result.

## 1. Contracts inventoried

`docs/` does not exist in this repository. `reference/` holds only
`technical-implementation-spec.md`, which governs this document's shape and
changes nothing here. `AGENTS.md` is the written contract the work touches. A
search of `AGENTS.md`, `LLM.md` and `reference/` for "ledger", "baseline",
"tombstone", "retired", "in flight" and the shell field names found one hit:
`AGENTS.md`'s "Presentation is per client" bullet uses "baseline" for the
server's per-client render baseline. That is the server's baseline, unchanged
here (section 7), so no `AGENTS.md` sentence goes stale.

Statements in `AGENTS.md` this spec relies on, none contradicted:

- "Hot paths multiply." A surface patch is applied per terminal output event,
  times panes, times clients. Section 3.3 removes the whole-grid clone the patch
  slow path makes today, adds none on a projection change (3.2: the one clone is
  made only when a patch arrives while the snapshot is past the presented
  surface), and validates each patch once. The fast path still builds the
  composed row vector and clones the changed cells into it, as today; it is not
  allocation-free, but it never copies the grid. The ledger and the
  scroll and copy objects are on the input path (one entry per user request),
  not the output path.
- "State is separated from runtime", "Render is pure": the shell stays plain
  data. Every type added here (`PaneSurfaces`, `Ledger`, `ScrollLanes`,
  `CopyPipeline`) is plain data with pure transitions, unit-testable without a
  transport, a clock or a terminal. `compose` still takes the state and only
  draws.
- "The client applies its own config to everything it draws and interprets":
  none of this state is config or crosses hosts.
- "No wire compatibility obligations", "Wire encoding is shepr's own": no wire
  type changes. The reader's decoder is untouched.
- Code conventions: no `unwrap()` in production code, `#[allow]` only with a
  reason, tests next to the code, no dependency added.
- The `shell/state.rs` header note that several state types derive `Debug`
  while carrying typed or pasted text stays true: `Work` carries a copy-mode
  search query and is `Debug`, so the note is extended to name it (brick B1).

No contract is changed. Notes documents (`notes/work.md`,
`notes/bugs-rejected-candidates.md`) and this spec are not edited by the
implementation.

## 2. Survey of the ground

Everything is in `crates/shepr-client/src/` unless a path says otherwise.

### 2.1 In-flight request state today

One request map and the per-feature state around it:

| State | Where | What it is |
|---|---|---|
| `pending_requests: HashMap<RequestId, PendingEndpointRequest>` | `shell/state.rs` | boot, method name, `PendingEndpointKind` |
| `next_request_id` | `shell/state.rs` | id counter, ids are `client-shell:{n}` |
| `pane_scroll_in_flight: HashMap<PaneId, u64>` | `shell/state.rs` | pane to serial of the outstanding scroll |
| `pane_scroll_queued: HashMap<PaneId, usize>` | `shell/state.rs` | newest offset wanted while one is outstanding |
| `pane_scroll_targets: HashMap<PaneId, usize>` | `shell/state.rs` | optimistic offset until a surface shows it |
| `next_scroll_serial` | `shell/state.rs` | serial counter that tags a scroll request |
| `copy_operation_in_flight: bool` | `shell/state.rs` | one copy motion or search outstanding |
| `copy_operation_queue: VecDeque<ClientCopyOperation>` | `shell/state.rs` | operations behind it |
| `copy_input_queue: VecDeque<TerminalKey>` | `shell/state.rs` | keys typed behind it |
| `copy_session_generation: u64` | `shell/state.rs` | bumped by `reset_copy_pipeline`; a stale answer carries the old value |
| `ClientWordSelection::pending_row` | `shell/input/word_selection.rs` | row read outstanding for a held double click |
| `word_selection_generation: u64` | `shell/state.rs` | guards a late answer against a newer gesture |
| `next_workspace_label_lookup_id: u64` and `ClientRenameTarget::NewWorkspace::label_lookup_id` | `shell/state.rs`, `shell/overlays/overlay_input.rs` | the new-workspace prompt's checkout-root lookup |
| `PendingWorkspaceHighlight::request_id` | `shell/navigation/workspace_navigation.rs` | display-only highlight kept until the focus request's snapshot |

`PendingEndpointKind` (`shell/state.rs`) is the discriminator. Completion is one
long `match` in `apply_endpoint_result` (`shell/navigation/actions.rs`);
rollback is a second `match` in `PendingEndpointKind::cancel`. The two are not
tied together: a result for a request whose boot no longer matches takes a third
path (the highlight check and `cancel`), and the highlight is released by a
`request_id` comparison written out twice in that function (the wrong-boot
branch and the `Err` branch). A cancel
path that dispatched work would be caught only by a runtime `tracing::error!`
(`cancel_endpoint_request_with_notice`). The endpoint notices are pushed, and a
success clears the method's timeout suppression, before the per-kind guards
run: an answer the feature then ignores (an abandoned copy request, a replaced
word gesture, a reopened label prompt) still reports its timeout or server error.

Who removes entries from `pending_requests`: `apply_endpoint_result` (the one
exit for answered and cancelled requests), one blanket clear that skips
rollback by design, `reset_endpoint_projection` (`pending_requests.clear()`
together with the three scroll maps, `reset_copy_pipeline()` and the gesture
reset), and `mark_endpoint_disconnected`, which cancels each id through the
rollback path and then clears the scroll in-flight and queued maps again.

The transport lane, `endpoint/commands.rs` (`EndpointCommands`), is a second
layer and stays one: per endpoint one FIFO lane, one in flight, a send
deadline, and a bounded tombstone list of retired request keys. It reports back
by request id string: `EndpointCommandResult` (answer or timeout) and
`EndpointCommandCancellation` (`unsent`, `possibly_sent`). The shell never sees
the lane; `shell_runtime.rs::cancel_endpoint_commands`, `lib.rs`
(`handle_server_message`, `handle_timer`) and `reconcile.rs` translate lane
reports into `cancel_unsent_endpoint_request`, `cancel_endpoint_request` and
`handle_endpoint_result_at`.

Each feature's in-flight protocol, as it must survive:

- Pane scroll (`shell/input/mouse.rs`): `push_pane_scroll_offset` records the
  target; with one outstanding it queues the newest offset (latest wins),
  otherwise dispatches. A good `PaneInfo` answer for the pane re-records the
  target as the confirmed offset, but only if the target still exists (a surface
  that already showed the offset removed it, and the answer must not bring it
  back); then it dispatches the queued offset. Every dispatch, that of a queued
  offset included, records its offset as the target, so while the queued
  request flies the target is the queued offset, not the confirmed one. A failed
  or unexpected answer
  drops queued and target and dispatches nothing; the unexpected-result error is
  set only after the serial matched. A send that fails to enter the
  queue removes target and in-flight; a dispatch with no snapshot returns before
  recording anything, leaving the target `push` recorded with no flight behind
  it. A cancel of the outstanding serial removes
  all three. The target is also removed by `install_pane_surface` and by the fast
  path of `apply_pane_surface_patch` when a surface shows
  `min(target, max_offset)`, is read by copy mode to avoid overwriting its offset
  with an in-between surface, and is dropped for panes missing from a new
  snapshot (`apply_active_snapshot`).
- Copy mode (`shell/input/copy_mode.rs`, `shell/input/input.rs`): one motion or
  search outstanding; keys typed meanwhile queue (bounded by
  `MAX_COPY_INPUT_QUEUE`) and replay in order when the answer lands; a full queue
  plus an interrupt key abandons the outstanding request (`abandon_copy_operation`
  bumps the session, the request stays in the ledger and its answer is ignored).
  `reset_copy_pipeline` is called on entry, exit, pane loss, projection reset and
  by the cancel path. `dispatch_next_copy_operation` pops one operation and, when
  the command cannot be pushed (endpoint not online, no snapshot), returns with
  the rest still queued and nothing in flight. `complete_copy_operation`
  re-checks the session generation after the result was applied, which also
  catches a reset during the apply (a search with `copy_after_search` exits copy
  mode). `defer_copy_until_search_result` scans `pending_requests`
  for a search of this pane and search generation, which also counts a request
  abandoned by an earlier session.
- Word selection (`shell/input/word_selection.rs`): one row read outstanding per
  gesture; a late answer is accepted only if the generation, pane and pending row
  all match; a cancel of the matching request cancels the gesture.
- New-workspace label lookup (`shell/overlays/overlay_input.rs`): the lookup id
  lives in the overlay; an answer for a closed or reopened overlay is ignored;
  a failed lookup keeps the path-based suggestion.
- Direct workspace focus (`focus_endpoint_target`): finds the request id of the
  command it just pushed by peeking `outcome.actions.first()`, then attaches the
  highlight to it. A failed, cancelled or wrong-boot answer releases the
  highlight if it still belongs to that request.

### 2.2 Surface state today

- `ClientShellState::pane_surface: Option<PaneSurfaceFrame>`: what is drawn, what
  input reads (copy-mode entry, split-drag identity, patch application) and the
  base of the next patch. Holds an exact snapshot pair, or a "retained future"
  surface (a full surface whose projection revision skipped past the snapshot).
- `pending_pane_surface`: a full surface whose projection revision is exactly one
  past the snapshot; parked until its snapshot arrives.
- `pane_surface_generation`: the connection generation of `pane_surface`, compared
  with `active_snapshot_generation` in three places.
- `set_pane_surface`, `install_pane_surface` (with a `retain_future` flag) and
  `apply_pane_surface_patch` each repeat "same boot, not older than the snapshot,
  not older than the current surface" in their own words.
- The reader (`transport.rs::server_reader_thread`) owns a
  `shepr_protocol::surface_reuse::Decoder` per connection. It applies each
  `SurfaceUpdate` to its own cell baseline, validates it, and emits either a full
  `PaneSurface` or a `PaneSurfacePatch` that follows the previous message
  exactly. It never judges snapshot pairing.
- Messages reach the shell through `handle_server_message` (`lib.rs`) only for the
  shown connection (role `Shown`); a target's messages are evidence in
  `Preparing` (`endpoint/choice/preparing.rs::ViewEvidence`), which applies them
  to its own surface in lockstep with the decoder, and every other connection's
  are dropped (`endpoint/message_policy.rs`). `commit_move` (`endpoint/view.rs`)
  installs the evidence surface with `set_pane_surface` right after
  `activate_endpoint_projection`.

Why REJ-011 happens. The decoder's baseline always advances: it is exactly "what
the server last said". The shell's `pane_surface` does not:

1. A full surface at snapshot revision plus one is parked in
   `pending_pane_surface`; `pane_surface` stays at N. A patch built on that parked
   surface is checked only against `pane_surface` and is `Rejected`, which fails
   the connection (`lib.rs`).
2. When the snapshot has moved past the visible surface, `fast_path_blocker`
   returns `projection_gap`, the patch goes through the slow path, and
   `set_pane_surface(next)` returns silently for a surface older than the snapshot.
   `apply_pane_surface_patch` still reports `Applied(None)`. The shell's surface
   revision did not advance, so the next patch is `Rejected`.

Both are the shell using one field for two roles: the base patches apply to and
the pair on screen.

Readers of `pane_surface` and `pending_pane_surface`, all in the shell: `compose`
(`presentation/composition.rs`: the presentability gate and the draw),
`apply_pane_surface_patch` and `fast_path_blocker`
(`presentation/surface_patch.rs`), `enter_copy_mode` (`input/copy_mode.rs`, two
reads), `pane_split_target_is_current`, `pane_split_topology_matches_hit` and
`split_child_panes` (`input/mouse.rs`), the workspace-navigation preview gate
(`navigation/workspace_navigation.rs`), `presentation_log_context`,
`reset_endpoint_projection`, `activate_endpoint_projection`
(`shell/endpoints.rs`), `apply_active_snapshot`, and
`commit_move` and `handle_server_message` as writers. Tests: about 245 sites in
`shell/tests/*.rs` and `tests/endpoint_choice.rs`, of which about a dozen read or
write the fields directly (listed in 2.5).

### 2.3 What `Preparing` guarantees at commit (verified, not changed)

`ViewEvidence` records every full surface of its lease that matches the client's
current surface size, applies every patch to it, and drops it on a geometry
change; `ready` requires it to pair with the recorded snapshot revision. The
size filter in `receive_surface` could in principle leave the evidence one full
surface behind the decoder. It cannot at commit: a size change drops the
recorded surface first (`update_geometry`), a surface of the old size that
arrives afterwards is filtered, and patches against it find no baseline, so
`ready` stays `None` until a full surface of the new size is recorded. So at
commit the evidence surface equals the decoder baseline, and installing it as
the shell's baseline is exact. This spec relies on that and adds a test that
pins it (brick A6).

### 2.4 Dependents of the request API outside the shell

- `lib.rs`: `handle_server_message` calls `handle_endpoint_result_at` (shown
  endpoint) or `cancel_endpoint_request` (an endpoint no longer active) for a
  completed command; `handle_timer` does the same for expired commands.
- `shell_runtime.rs`: `cancel_endpoint_commands` (both cancellation lists) and
  `dispatch_client_shell_actions` (`cancel_unsent_endpoint_request` for an action
  that never entered the lane).
- `reconcile.rs`: `endpoint_lost` and the commit path call
  `cancel_endpoint_commands` and `mark_endpoint_disconnected`.
- `endpoint/commands.rs` and `endpoint/message_policy.rs`: the tombstone list and
  its `command_response` flag in `PresentationGate`.
- `ClientShellEndpointError::Cancelled` is constructed only in the shell and has
  one `Display` text ("This server action was interrupted. Check its state before
  retrying.").
- Tests that name removed fields or functions: `shell/tests/endpoint_requests.rs`
  (19 field uses), `shell/tests/copy.rs` (31), `shell/tests/mouse_selection.rs`
  (4), `shell/tests/workspace_navigation.rs` (7, all
  `pending_workspace_highlight`, which stays), `shell/tests/text_editing.rs` and
  `shell/input/input.rs` tests (field and function names),
  `tests/endpoint_choice.rs::commit_retires_the_previous_command_lane`, and the
  `commands.rs` and `message_policy.rs` unit tests. Among them:
  - `shell/tests/endpoint_requests.rs::stale_queued_request_is_cancelled_without_blocking_the_current_generation`
    asserts through `commands.response_kind(...)` and
    `CommandResponseKind::{Untracked, Active}`, both deleted by B7.
  - The `#[cfg(test)]` wrapper `ClientShellState::handle_endpoint_result`
    (`shell/navigation/actions.rs`) has about 35 callers in `shell/tests/`.
  - Two `shell/input/input.rs` tests
    (`copy_prefix_replays_after_keys_queued_behind_an_operation`,
    `copy_escape_cancels_selection_started_by_prior_queued_key`) assign
    `copy_operation_in_flight = true` and call
    `complete_copy_operation(generation, true, ...)` directly.
  - The `shell/tests/copy.rs` test that abandons a session behind a full key
    queue and then calls `cancel_unsent_endpoint_request(&old_id)` asserts
    `copy_session_generation` is unchanged.
  - `ledger` (like every new field) is `pub(super)` in `crate::shell`, so
    `crate::tests` (`tests/endpoint_choice.rs`) cannot read it.

### 2.5 Direct field uses in tests that need a replacement

`chrome_context.rs:105` (`pane_surface.is_some()`), `input.rs:708`
(`pending_pane_surface.is_some()`), `endpoints.rs:670, 811` (`pane_surface.is_none()`),
`endpoints.rs:1666, 1682, 1770, 1820` (`pending_pane_surface`), `copy.rs:326, 339,
1023` (`pane_surface.clone()`), `presentation/surface_patch.rs:352` (assigns
`pane_surface`) and `cursor_patch` in the same test module (reads
`state.pane_surface.as_ref()`), `input/mouse.rs:1941, 1943` (assigns both
fields, in `split_drag_state`), `tests/endpoint_choice.rs:176`
(`set_pane_surface`). Reads split over two lines (`state` then `.pane_surface`)
that a one-line search misses: four in `shell/tests/mouse_selection.rs` (the
word-gesture tests that clone the presented surface to build a changed one),
five in `shell/tests/endpoints.rs` (the future-surface and reconnect tests next
to the `pending_pane_surface` uses above) and one in
`shell/tests/workspace_navigation.rs`.

### 2.6 Load-bearing behaviors that must survive

1. A displayed frame stays on screen, clipped, while the snapshot is ahead of it
   (`compose` returns `None`); the hit map stays live through that gap.
2. A surface from a reconnected generation never draws against a snapshot of the
   old generation, and the old pair stays visible (frozen) until the new pair
   exists (`reconnect_same_endpoint_accepts_new_generation_surface_revision`,
   `disconnected_active_endpoint_freezes_surface_and_marks_cached_ui_stale`).
3. Installing a presented surface invalidates a selection or word gesture whose
   pane changed size or screen, clamps copy-mode points to the retained rows,
   refreshes copy-mode scroll metrics unless a scroll target is pending, and
   removes a scroll target the surface now shows.
4. The fast patch path applies cells and per-pane metadata in place and presents
   only the patched rows; a blocker (mode, overlay, error, notice, selection,
   copy mode, overflow, missing pane hits) sends the update through full compose.
5. A cancelled request restores only the state it owns and never dispatches work;
   an answered request may dispatch its queued follow-up.
6. A late answer to a request that was cancelled, abandoned, replaced or reset is
   ignored without touching state a newer request owns. Endpoint notices are not
   such state: they report what the server answered, so an answer the feature
   ignores (abandoned, replaced) still shows its timeout or server error, and a
   success still clears the method's timeout suppression, as today (2.1). A
   request that left the ledger (cancelled, reset) has no answer to report.
7. "Action interrupted" is shown for a plain command whose connection was lost with
   the outcome unknown, not for a read-only request and not for one that never
   left the client; a timeout shows "Server timed out" once per method until a
   success clears it.

## 3. The target

### 3.1 Principles and invariants

Surface:

- S1. `PaneSurfaces::baseline()` is the shown connection's reader baseline: every
  full surface the shell receives replaces it, every patch the shell receives
  advances it, and nothing else changes it except losing the connection. The
  shell observes a lost connection as the active snapshot generation changing
  from one connection's to another's (`Some(a)` to anything else), or as a
  projection reset or endpoint switch. A first snapshot (`None` to `Some`) is
  not a loss: a surface the launch connection sent before its first snapshot is
  that connection's baseline and is kept. This does not rely on the server
  writing the snapshot before the surface.
- S2. What is presented is chosen from the baseline and the snapshot by one
  function (`pair`): the baseline is presented exactly when it has the snapshot's
  boot and projection revision. Otherwise the last presented pair is held
  unchanged.
- S3. `ClientPaneSurfacePatchOutcome::Rejected` means the patch does not follow the
  baseline (no baseline, wrong revision, a geometry change a patch cannot carry,
  rows outside the frame or outside a patched pane). Pairing never causes it.
  The reader's decoder checks revisions, pane presence and frame bounds but not
  pane geometry or that rows lie inside a patched pane, so a rejection means
  either a shell bug (shell and reader disagree about the baseline) or a server
  that sent a patch the decoder accepts and the shell does not. Both are bugs;
  only the first is ruled out by the shell mirroring the reader (section 8 has
  the follow-up that would rule out the second).

Ledger:

- L1. A request id is created by `Ledger::open` and leaves by `answer_request`
  (answered) or `drop_request` (everything else). `Ledger::take` is private to
  the ledger module. In production code there is no other removal; the
  `#[cfg(test)]` `Ledger::retain` helper (3.4) is the one test-only exception.
- L2. The rollback half of `Work` (`dropped`) receives no `ClientShellInput`, so
  anything it tried to send has nowhere to go; only `answered` can queue an
  action or merge input. The types do not make this airtight: `dropped` takes
  `&mut ClientShellState`, and a rollback that called `submit` with a throwaway
  `ClientShellInput` would open an orphan entry whose action is lost. So
  `drop_request` carries a `debug_assert!` that the ledger did not grow across
  `dropped`, and the rollback test asserts the same for every `Work` kind (B8).
- L3. No feature owns a counter or a serial to identify its request. A feature
  holds the `RequestId` the ledger issued, in the one object that also holds
  everything queued behind that request, so the queue cannot outlive the request.
- L4. A late answer is ignored by comparing its request id with the feature's
  current one. There is no generation to forget to bump.

### 3.2 `PaneSurfaces` (new `shell/presentation/surfaces.rs`)

```rust
/// What the shell knows of the shown connection's pane surface. Every variant is a
/// state that can occur, and every state that can occur has exactly one variant: there
/// is no "neither baseline nor presented" case to rule out by convention.
#[derive(Default)]
pub(super) enum PaneSurfaces {
    /// Nothing received on this connection and nothing presented.
    #[default]
    Empty,
    /// The baseline pairs with the snapshot. It is drawn, input reads it, and patches
    /// apply to it in place.
    Paired(PaneSurfaceFrame),
    /// The snapshot moved past the paired surface and nothing has arrived since. The one
    /// value is both the held presentation (on screen, frozen, what input reads) and the
    /// baseline. The first patch splits it (`Split`, the one grid copy); a full surface
    /// replaces the baseline without a copy.
    Passed(PaneSurfaceFrame),
    /// The connection was lost: the last presentation is held, frozen, and there is no
    /// baseline until the new connection's first surface.
    Frozen(PaneSurfaceFrame),
    /// The baseline and the held presentation are different surfaces. `held` is the last
    /// paired surface (on screen, frozen, what input reads), or `None` when nothing was
    /// presented on this projection yet.
    Split {
        baseline: PaneSurfaceFrame,
        held: Option<PaneSurfaceFrame>,
    },
}

pub(super) enum Pairing {
    /// Nothing on screen changed.
    Unchanged,
    /// The baseline is now the presented surface; `previous` is what was presented.
    Presented { previous: Option<PaneSurfaceFrame> },
    /// The snapshot moved past the presented surface: it is held while the baseline advances.
    Passed,
}

/// Why a patch does not follow the baseline. Crate-visible because
/// `ClientPaneSurfacePatchOutcome::Rejected` carries it to `lib.rs`, which logs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PatchRejection {
    NoBaseline,
    DoesNotFollow,   // boot, projection revision, base revision or next revision
    PaneGeometry,    // a patched pane is missing, or its rect, focus or pixel size differs
    RowOutsideFrame, // a row outside the frame, empty, or outside the patched panes
}
```

Methods (all pure):

| Method | Behavior |
|---|---|
| `presented(&self) -> Option<&PaneSurfaceFrame>` | `Paired(s)`, `Passed(s)` and `Frozen(s)` give `s`; `Split` gives `held`; `Empty` gives `None`. This is what input reads. |
| `paired(&self) -> Option<&PaneSurfaceFrame>` | `Paired(s)` only. This is what `compose` draws. |
| `is_paired(&self) -> bool` | |
| `baseline(&self) -> Option<&PaneSurfaceFrame>` | `Paired(s)` and `Passed(s)` give `s`; `Split` gives `baseline`; `Frozen` and `Empty` give `None`. |
| `waiting_baseline(&self) -> Option<&PaneSurfaceFrame>` | `Split`'s baseline only: a received surface that differs from what is presented and waits for its snapshot. (In `Passed` the baseline is the presented surface itself.) |
| `receive(&mut self, surface)` | Replace the baseline, all moves, no copy. `Empty` becomes `Split { baseline: surface, held: None }`; `Paired(p)`, `Passed(p)` and `Frozen(p)` become `Split { baseline: surface, held: Some(p) }`; `Split { held, .. }` keeps `held`. Never pairs by itself. |
| `pair(&mut self, boot: &BootId, revision: ProjectionRevision) -> Pairing` | A match is `boot_id == boot && projection_revision == revision`. `Paired(s)` that matches stays (`Unchanged`); `Paired(s)` that does not becomes `Passed(s)` (`Passed`, a move, no copy); `Split` whose baseline matches becomes `Paired(baseline)` (`Presented { previous: held }`); `Passed(s)` that matches becomes `Paired(s)` (`Unchanged`: the presented surface is the same value, so no effect has anything new to see; a snapshot never moves back within one boot, so this is for totality); anything else `Unchanged`. |
| `lose_baseline(&mut self)` | The connection changed: `Paired(s)` and `Passed(s)` become `Frozen(s)`; `Split { held: Some(h), .. }` becomes `Frozen(h)`; `Split { held: None, .. }` becomes `Empty`; `Frozen` and `Empty` stay. |
| `validate(&self, patch) -> Result<(), PatchRejection>` | Checks the patch against `baseline()`: present, same boot, same projection revision, `base_surface_revision` equals its revision, its revision's successor equals the patch's, every patched pane exists with matching geometry (`pane_geometry_matches`), every row fits the frame and lies inside a patched pane's inner rect or scrollbar rect. This is the validation `apply_pane_surface_patch` runs today, moved. It is the one validation per patch. |
| `apply_validated(&mut self, patch) -> Result<(), PatchRejection>` | Precondition: `validate` accepted this patch against the current baseline, with nothing changed in between (the patch flow in 3.3 is the only caller). Does not validate again: it calls `shepr_protocol::surface_reuse::apply_patch_to_surface` on the baseline, whose own cheap checks stay (that crate is not touched); a protocol error maps to `DoesNotFollow`. `Paired(s)` and `Split`'s baseline are patched in place; `Passed(s)` becomes `Split { baseline: patched copy of s, held: Some(s) }`, the one grid copy, made only when a patch arrives while the snapshot is past the presented surface (REJ-011 case 2). `Frozen` and `Empty` return `NoBaseline` (unreachable after `validate`). |

Pairing is evaluated in exactly two places (3.3): after a surface is received and
after a snapshot is applied. A patch never changes a projection revision, so it
never changes pairing.

Today a snapshot that moves past the presented surface copies nothing (the
surface simply stays). `Passed` keeps that: a projection change is normally
followed by a full surface, which replaces the baseline by a move, so the grid is
copied only in the gap case where a patch arrives first.

### 3.3 Shell flow (landing A)

Fields: delete `pane_surface`, `pending_pane_surface` and
`pane_surface_generation` from `ClientShellState`; add
`surfaces: PaneSurfaces` (`Default`). Add the accessor
`pub(super) fn pane_surface(&self) -> Option<&PaneSurfaceFrame>` returning
`surfaces.presented()`, so reads in input code and tests keep their shape.

Entry points:

- `pub(crate) fn receive_pane_surface(&mut self, surface: PaneSurfaceFrame)`
  (replaces `set_pane_surface`): `self.surfaces.receive(surface)` then
  `self.pair_surfaces()`. No monotonic or "not older than the snapshot" check:
  the reader already enforces order, and the shell mirrors it. A surface of
  another boot than the snapshot simply never pairs.
- `fn pair_surfaces(&mut self)`: return when there is no snapshot. Call
  `self.surfaces.pair(&snapshot.boot_id, snapshot.revision)` with the two
  borrows taken as disjoint fields (`self.snapshot`, `self.surfaces`), no clone
  of the boot id. On `Presented { previous }`: compute
  `before = self.pane_facts_before(previous.as_ref())`, move the surfaces out
  with `std::mem::take(&mut self.surfaces)`, call
  `self.presented_surface_changed(before, surface)` on the paired surface, and
  move the surfaces back. `Unchanged` and `Passed` do nothing else: nothing on
  screen changed.
- `apply_active_snapshot` (`shell/state.rs`): when the generation changes from
  one connection's to another's, `self.active_snapshot_generation.is_some() &&
  generation_changed` (S1), call `self.surfaces.lose_baseline()` (replaces
  `pending_pane_surface = None`). A first snapshot (`None` to `Some`) keeps the
  baseline: at launch the choice is `Showing(Local)` with no `Preparing`, so a
  surface that reached the shell before the launch connection's first snapshot
  is that connection's baseline, and losing it would make the next patch
  `Rejected(NoBaseline)`. Today that surface is dropped by `set_pane_surface`
  (no snapshot); the server's outbox writes control before the surface slot, so
  the order may never occur, but the shell does not rely on it. `lose_baseline`
  is not moved into `mark_endpoint_disconnected`: that would unpair a `Paired`
  surface at disconnect, `compose` would then return `None`, and
  `disconnected_active_endpoint_freezes_surface_and_marks_cached_ui_stale`
  (the reconnecting banner drawn over the frozen surface) would break. A boot
  change still goes through `reset_endpoint_projection`, which sets
  `self.surfaces = PaneSurfaces::default()`; the pending-surface block at the end
  becomes `self.pair_surfaces()` (after `reconcile_pending_workspace_highlight`).
- `activate_endpoint_projection` (`shell/endpoints.rs`): when switching endpoint,
  `self.surfaces = PaneSurfaces::default()` replaces the two assignments.
- `commit_move` (`endpoint/view.rs`): `shell.receive_pane_surface(surface.clone())`
  replaces `set_pane_surface`. It follows `activate_endpoint_projection`, which
  has applied the snapshot at the evidence's revision (`endpoint_snapshot_matches`
  checked it), so the received baseline pairs at once.
- `handle_server_message` (`lib.rs`): `ServerMessage::PaneSurface` calls
  `receive_pane_surface`; the compose that follows returns `None` while the
  surfaces are unpaired and something is presented, as it does today.
- Every caller that today relies on `set_pane_surface` dropping a surface (no
  snapshot yet, older than the snapshot, or older than the current surface) now
  keeps it as the baseline. In production that is only the pre-first-snapshot
  case above; tests that set a surface before their snapshot, or at a lower
  revision than it, need checking one by one (A6), not a blind rename.
- `invalidate_pane_surface` (test only): `self.surfaces = PaneSurfaces::default()`
  plus the hits and mouse-pixel resets it already does.

`presented_surface_changed(&mut self, before: PreviousPane, surface: &PaneSurfaceFrame)`
is the body of today's `install_pane_surface` from "the selection pane" onward,
unchanged in behavior, minus three things: the `retain_future` flag and every
monotonic guard (gone), the `hits.panes.clear()` for a future surface (gone: the
pane hits describe the held surface, which is what input now reads), and the final
assignment of `pane_surface` and `pane_surface_generation` (gone: the caller owns
the storage). It still: invalidates the selection or word gesture of the selected
pane, drops scroll targets the surface shows, refreshes copy-mode geometry,
history origin, offsets and cursor clamping (`sync_copy_selection` when clamped),
and clears a selection owned by an invalidated copy pane.

```rust
/// The selected pane as the previously presented surface showed it. The selection
/// invalidation needs only this, so no previous surface is cloned.
pub(super) enum PreviousPane {
    NoSurface,                // nothing was presented: invalidate
    Absent,                   // the pane is missing from the previous or the next surface: keep
    Present(PaneFacts),       // compare
}
pub(super) struct PaneFacts {
    inner_width: u16,
    inner_height: u16,
    alternate_screen_active: bool,
    content_revision: u64,
}
```

`pane_facts_before(previous: Option<&PaneSurfaceFrame>) -> PreviousPane` reads the
selection or word-gesture pane's entry (or `NoSurface` for `None`); when there is
no selection it returns `Absent` without looking. `presented_surface_changed`
compares it with the new surface with exactly the three branches of today's
`selection_invalidated` closure.

Patch flow, replacing `apply_pane_surface_patch` (`presentation/surface_patch.rs`):

```rust
pub(crate) enum ClientPaneSurfacePatchOutcome {
    /// The patch does not follow the baseline (S3).
    Rejected(PatchRejection),
    Applied(PatchPresentation),
}
pub(crate) enum PatchPresentation {
    /// The baseline advanced; the presented surface did not change. Present nothing.
    Held,
    /// The presented surface changed; present by full compose.
    Compose,
    /// The presented surface changed; present only these rows.
    Rows(ClientComposedSurfacePatch),
}
```

The patch is validated once, in step 1; steps 2 and 3 call `apply_validated`,
which does not repeat the O(rows x panes) row and pane checks.

1. `self.surfaces.validate(patch)`; on error return `Rejected(reason)`. Nothing
   has changed at this point.
2. Not paired (`!self.surfaces.is_paired()`): `self.surfaces.apply_validated(patch)`
   (it advances the baseline only; `held` is untouched; `Passed` makes its one
   copy here) and return `Applied(Held)`. A failure maps to `Rejected`.
3. Paired: compute `area` and `fast_path_blocker` as today, with the
   `projection_gap` and `pending_pane_surface` branches deleted (the paired
   precondition replaces them) and the surface read through `presented()`.
   - No blocker: `apply_validated` in place (no clone), update the patched panes'
     hits exactly as today, and for each patched pane call
     `self.scroll_target_shown(pane_id, scroll)` (the cleanup both sites already
     do, now one function), then return `Applied(Rows(composed))`.
   - A blocker: capture `before = pane_facts_before(self.surfaces.paired())`,
     `apply_validated` in place, then (take, call, put back as in
     `pair_surfaces`) `presented_surface_changed(before, surface)` and return
     `Applied(Compose)`. This replaces the clone-patch-`set_pane_surface`
     sequence, which cloned the whole grid on every slow-path patch.

`lib.rs` maps the outcome: `Applied(Rows(c))` presents the rows (a write failure
requests a repaint, as today); `Applied(Compose)` composes and presents;
`Applied(Held)` does nothing; `Rejected(reason)` logs `reason` (its `Debug`
form, content-free) with the existing `tracing::error!`, then fails the
connection as today. The comment above that failure is rewritten: a rejection
now means either the shell and the reader disagree about a baseline both derive
from the same wire, or the server sent a patch the decoder accepts but the shell
does not (a pane geometry change, or a row outside every patched pane; the
decoder checks neither). Both are bugs, and reconnecting for a fresh baseline is
the right response (and the only one).

Visibility: `PatchRejection` is `pub(crate)` and derives `Debug` (3.2), because a
`pub(crate)` enum's variant carries it and `lib.rs` logs it; a `pub(super)` type
there is a private type in a crate-visible interface, which clippy rejects, and
cannot be re-exported at a wider visibility. `shell.rs` re-exports it next to
`PatchPresentation`: `pub(super) use surface_patch::{ClientComposedSurfacePatch,
ClientPaneSurfacePatchOutcome, PatchPresentation};` and
`pub(super) use surfaces::PatchRejection;`.

`compose` (`presentation/composition.rs`): `has_surface` is
`snapshot.is_some() && surfaces.presented().is_some()`; the early return is
`has_surface && !surfaces.is_paired()` (replacing the three-way test); the draw
reads `surfaces.paired()`.

Other readers switch to the accessor or the typed query:

- `fast_path_blocker`'s overflow check and `copy_mode_cursor_changed_on_owner`:
  `presented()`.
- `enter_copy_mode` (two reads), `split_child_panes`,
  `pane_split_target_is_current` (still returns `None` unless the snapshot
  revision equals the presented surface's), `presentation_log_context`:
  `pane_surface()`. The log context's `generation` becomes
  `active_snapshot_generation` unconditionally (the surface no longer carries
  its own).
- `pane_split_topology_matches_hit`: `current_matches` over `presented()` and
  `pending_matches` becomes `surfaces.waiting_baseline().is_none_or(matches_hit)`.
  Same meaning: a split ratio update advances the projection without changing the
  tree, so a hit must match every topology received for this boot, including the
  one waiting for its snapshot.
- Workspace-navigation preview gate (`workspace_navigation.rs`, `pane_surface.is_none()`):
  `pane_surface().is_none()`.

### 3.4 `Ledger` (new `shell/ledger.rs`)

```rust
pub(super) struct Ledger {
    next: u64,
    entries: HashMap<RequestId, Entry>,
}

pub(super) struct Entry {
    pub(super) boot_id: BootId,
    pub(super) method: String,
    pub(super) work: Work,
}

impl Ledger {
    /// Issues `client-shell:{n}` and records the entry.
    pub(super) fn open(&mut self, boot_id: BootId, method: String, work: Work) -> RequestId;
    pub(super) fn work(&self, id: &RequestId) -> Option<&Work>;
    pub(super) fn contains(&self, id: &str) -> bool;
    pub(super) fn len(&self) -> usize;
    pub(super) fn is_empty(&self) -> bool;
    pub(super) fn ids(&self) -> Vec<RequestId>;
    fn take(&mut self, id: &str) -> Option<Entry>;           // private: L1
    #[cfg(test)] pub(super) fn retain(&mut self, keep: impl FnMut(&RequestId) -> bool);
}
```

Ids keep the `client-shell:{n}` form (the transport lane and its tests use the
string only as an opaque key; nothing parses it). `next_request_id` moves into the
ledger.

```rust
pub(super) enum Work {
    /// A command whose answer needs no shell state.
    Plain,
    SelectionCopy,
    WorkspaceLabel,
    PaneScroll { pane_id: PublicPaneId },
    WordSelection { pane_id: PublicPaneId, row: shepr_vt::AbsRow },
    CopyMotion { pane_id: PublicPaneId, origin: PaneTextPoint },
    CopySearch {
        pane_id: PublicPaneId,
        origin: PaneTextPoint,
        query: String,
        direction: PaneCopySearchDirection,
        repeat: bool,
        generation: u64,      // the copy-mode search generation (not a request guard)
    },
}
```

`PendingEndpointKind` and `PendingEndpointRequest` (`shell/state.rs`) are deleted.
`Work` is `Debug` and carries a search query, so the file header note on typed text
names it.

```rust
pub(crate) enum DropReason {
    /// The request may have reached the server and its outcome is unknown: the
    /// connection was lost, the lane cancelled it as possibly sent, or its endpoint is
    /// no longer the active one when its answer or lane expiry arrives. (An expiry on
    /// the active endpoint is an answer, `Err(Timeout)` through `answer_request`.) A
    /// `Plain` request shows the interruption notice.
    Interrupted,
    /// The request never left the client (rejected before the lane sent it).
    Unsent,
    /// The answer came for another server boot than the one the request was made for.
    WrongBoot,
    /// The projection was reset (boot change or endpoint switch).
    Reset,
}

impl Work {
    /// The request was answered (a reply, a timeout or a server error). May queue input,
    /// actions and requests on `outcome`.
    fn answered(
        self,
        shell: &mut ClientShellState,
        request: &RequestId,
        result: Result<EndpointReply, ClientShellEndpointError>,
        now: Instant,
        outcome: &mut ClientShellInput,
    );
    /// The request ends without an answer. Restores exactly the state this request
    /// owns. Returns whether to repaint. No `outcome`: it cannot send (L2).
    fn dropped(self, shell: &mut ClientShellState, request: &RequestId) -> bool;
}
```

`ClientShellEndpointError` loses its `Cancelled` variant (and the `Display` arm for
it); it keeps `Timeout` and `Server`. The "interrupted" text moves into the notice
built for `DropReason::Interrupted` (below).

Shell methods in `shell/ledger.rs`, the only code that calls `take`:

```rust
impl ClientShellState {
    /// Opens a ledger entry for `command` at the current snapshot's boot and appends the
    /// endpoint action. `None` (and no entry) when the endpoint is not online or has no
    /// snapshot. Replaces `push_endpoint_command_with_kind`; the highlight-clearing
    /// preamble for focus-changing commands is kept.
    pub(super) fn submit(&mut self, command: EndpointCommand, work: Work,
        outcome: &mut ClientShellInput) -> Option<RequestId>;

    /// Plain command: `submit` with `Work::Plain`, id discarded (was `push_endpoint_command`).
    pub(super) fn push_endpoint_command(&mut self, command: EndpointCommand,
        outcome: &mut ClientShellInput);

    pub(crate) fn answer_request(&mut self, boot_id: &str, request_id: &str,
        result: Result<EndpointReply, ClientShellEndpointError>, now: Instant)
        -> ClientShellInput;                                   // was handle_endpoint_result_at

    pub(crate) fn drop_request(&mut self, request_id: &str, reason: DropReason) -> bool;
                                                               // was cancel_endpoint_request
                                                               // and cancel_unsent_endpoint_request
    pub(super) fn drop_all_requests(&mut self, reason: DropReason) -> bool;

    /// Whether the ledger holds `request_id`. For `crate::tests`, which cannot see the
    /// `pub(super)` `ledger` field.
    #[cfg(test)]
    pub(crate) fn has_request(&self, request_id: &str) -> bool;

    /// The existing test-only wrapper, kept by name over `answer_request` with
    /// `Instant::now()`, so its callers in `shell/tests/` do not change.
    #[cfg(test)]
    pub(crate) fn handle_endpoint_result(&mut self, boot_id: &str, request_id: &str,
        result: Result<EndpointReply, ClientShellEndpointError>) -> ClientShellInput;
}
```

`DropReason` is named as `shell::DropReason` by `lib.rs` and `shell_runtime.rs`,
and `ledger` is a private module of `shell`, so `shell.rs` adds
`pub(super) use ledger::DropReason;` (crate-visible, like the other
re-exports there).

`answer_request`:

1. `take(request_id)`; absent: return an empty outcome (a late answer; L4).
2. Boot mismatch: `entry.boot_id != boot_id`, or the shell has no snapshot, or its
   snapshot has another boot than `boot_id`: finish the entry as
   `DropReason::WrongBoot` (step D below) and return an outcome with its repaint.
3. `Ok`: remove the `Timeout` notice key for `(entry.boot_id, entry.method)` from
   `endpoint_notice_seen`, as today.
4. `Err`: release the workspace highlight if it belongs to this request
   (`release_highlight(request_id)`, below), then push the notice exactly as today:
   `Timeout` gives the `Timeout` notice keyed by the method; `Server(ShuttingDown)`
   gives `Unavailable` "Server unavailable"; any other server error gives `Rejected`
   "Action rejected" keyed `{method}:{message}`; boot recorded is the entry's.
5. `entry.work.answered(self, &request, result, now, &mut outcome)`; `outcome.repaint`
   accumulates.

Steps 3 and 4 run before the feature's own staleness check inside `answered`, as
today (2.1, 2.6.6). That is deliberate: an abandoned copy request, a replaced
word gesture's read or a reopened label prompt's lookup was sent and the server
answered it, so its timeout or server error is shown and its success clears the
method's timeout suppression. Only the feature state is guarded. A test pins it
(B8, `an_ignored_answer_still_reports_its_server_error`).

`drop_request`: `take`; absent returns false. Step D (shared with `answer_request`'s
wrong-boot path):

- D1. `release_highlight(request_id)`: if `pending_workspace_highlight` has this
  request id, clear it (the one comparison of that id, shared with step 4 of
  `answer_request`; replaces the two copies in `apply_endpoint_result`).
- D2. `DropReason::Interrupted` and `Work::Plain`: push the `Unavailable` notice
  "Action interrupted" with the body "This server action was interrupted. Check its
  state before retrying.", code `cancelled`, at the entry's boot. (Read-only
  requests, `Unsent` and the other reasons show nothing; same as today's
  `should_show_cancelled_notice` and `show_cancelled_notice`.)
- D3. `entry.work.dropped(self, &request)`. The result ORed with D1 and D2 is the
  repaint. Around this call, `debug_assert!(self.ledger.len() <= len_before)`,
  where `len_before` is the length after `take` (L2).

`drop_all_requests(reason)` drops every id from `ledger.ids()` through
`drop_request`. `mark_endpoint_disconnected` (`shell/endpoints.rs`), for the active
endpoint, calls `drop_all_requests(DropReason::Interrupted)` and nothing else (the
two scroll-map clears are gone: a scroll flight cannot exist without its entry,
3.5). `reset_endpoint_projection` calls
`drop_all_requests(DropReason::Reset)` in place of `pending_requests.clear()`, then
still resets the non-request feature state it resets today (selection, hits,
copy mode, gesture). Unlike the old `clear()`, this runs every entry's rollback.
Nothing visible changes: `Reset` shows no notice, and each rollback restores
state (scroll lanes, the copy pipeline, the word gesture, the label lookup, the
highlight) that the same reset then clears anyway.

The runtime `tracing::error!("a cancelled endpoint request produced actions or
requests")` is deleted. With no `outcome` the rollback has nowhere to put an
action; the one way left to misbehave (a rollback that opens a ledger entry with a
throwaway input) is caught by the D3 `debug_assert!` and the B8 test.

Per-kind behavior (the old completion `match` and `cancel`, one place each):

| `Work` | `answered` | `dropped` (returns repaint) |
|---|---|---|
| `Plain` | repaint when the result is `Err`. | true |
| `SelectionCopy` | as today: a non-empty `PaneSelection` pushes `ClipboardWrite` onto `outcome.actions`; an empty one shows "Nothing copied"; a wrong reply shows the endpoint error; `Err` repaints. | true |
| `WorkspaceLabel` | `complete_workspace_label_lookup(request, Some(reply))` on `Ok`, `None` on `Err`. | `complete_workspace_label_lookup(request, None)` |
| `PaneScroll { pane_id }` | `answer_pane_scroll` (3.5). | `drop_pane_scroll` (3.5) |
| `WordSelection { pane_id, row }` | `complete_word_selection_row` (3.5). | `drop_word_selection(request)` (3.5) |
| `CopyMotion` / `CopySearch` | `complete_copy_motion` / `complete_copy_search` (3.5). | `drop_copy_operation(request)` (3.5) |

### 3.5 Feature objects

All three replace separate maps, flags and counters by one value, so queued work
cannot be left behind.

**`ScrollLanes`** (new `shell/input/scroll_lanes.rs`). Replaces
`pane_scroll_in_flight`, `pane_scroll_queued`, `pane_scroll_targets` and
`next_scroll_serial`.

```rust
#[derive(Default)]
pub(super) struct ScrollLanes(HashMap<PublicPaneId, ScrollLane>);
struct ScrollLane {
    /// The offset the user last asked for, until a surface shows it.
    target: Option<usize>,
    flight: Option<ScrollFlight>,
}
struct ScrollFlight {
    request: RequestId,
    /// The newest offset wanted while this request is outstanding (latest wins). Exists
    /// only inside a flight: a queued offset cannot outlive the request it waits behind.
    queued: Option<usize>,
}
pub(super) enum ScrollWant { Send, Queued }
pub(super) enum ScrollAnswer { Stale, Next(Option<usize>) }
```

| Method | Behavior |
|---|---|
| `want(pane, offset) -> ScrollWant` | `target = Some(offset)`. With a flight: `queued = Some(offset)`, `Queued`. Otherwise `Send`. |
| `sent(pane, request, offset)` | `target = Some(offset)` and start a flight with no queued offset. Recording the target on every send mirrors today's dispatch, which writes it for a queued offset too: while the queued request flies, the target is the queued offset, so a surface still showing the confirmed (in-between) offset does not clear it and copy mode does not take that offset. |
| `send_failed(pane)` | Remove the lane (target and flight), as today's failed dispatch. |
| `answered(pane, request, confirmed: Option<usize>) -> ScrollAnswer` | A flight with another request, or none: `Stale`. Otherwise end the flight and take `queued`. With nothing queued, and only if `confirmed` is `Some` and `target` is still `Some`, set `target = confirmed` (a surface that already showed the target removed it, and the answer must not bring it back). With an offset queued, leave `target` alone: the dispatch that follows calls `sent`, which records the queued offset as the target, or `send_failed`, which removes the lane. Returns `Next(queued)`. |
| `failed(pane, request) -> bool` | A flight with this request: remove the whole lane (target, flight and queued), true. Otherwise false. Used by a failed or unexpected answer and by `dropped`. |
| `target(pane) -> Option<usize>` | The pending target (copy mode's "scroll in progress" check). |
| `shown(pane, offset, max)` | A surface shows `offset` for `pane`: when `offset == min(target, max)`, `target = None`; remove the lane if it is then empty. Called from `presented_surface_changed` and from the fast patch path (the single `scroll_target_shown` helper of 3.3). |
| `retain_panes(exists)` | Drop lanes of panes missing from a new snapshot, flight included (the entry stays in the ledger until its answer, which is then `Stale`). |
| `clear()` | Reset (projection reset). |
| `queued(pane) -> Option<usize>` (`#[cfg(test)]`) | The queued offset of the pane's flight. |
| `in_flight(pane) -> bool` (`#[cfg(test)]`) | The pane's lane has a flight. |
| `is_idle() -> bool` (`#[cfg(test)]`) | No lane at all (no target, no flight, nothing queued). |

`push_pane_scroll_offset` (`mouse.rs`) becomes: `want`; on `Queued` return; on
`Send` call `dispatch_pane_scroll(pane, offset, outcome)`, which `submit`s
`EndpointCommand::PaneScroll` with `Work::PaneScroll { pane_id }` and calls
`sent(pane, id, offset)`, or `send_failed` when `submit` returns `None` (endpoint
not online or no snapshot; today the no-snapshot case returned early and left
the target behind, 3.9). `answer_pane_scroll(request, pane_id, result, now,
outcome) -> bool`: on `Ok(PaneInfo)` for this pane, `answered(pane, request,
pane.scroll.map(offset))`; on a `PaneInfo` for another pane or any other `Ok`,
call `failed` first and set the unexpected-result error only when it returned
true (a stale answer of the wrong shape shows nothing, as today where the error
follows the serial match); on `Err`, `failed`; `Next(Some(offset))`
dispatches again; `Stale` returns false. `drop_pane_scroll(request, pane)` is
`failed(pane, request)`. `answered` gets a fresh doc comment for its
`target.is_some()` condition (the old `complete_pane_scroll` has none to keep).

The field is `scroll_lanes: ScrollLanes`, not `scroll`, so it does not read as
the existing `mod scroll` (`shell/navigation/scroll.rs`).

**`CopyPipeline`** (in `shell/input/copy_mode.rs`). Replaces
`copy_operation_in_flight`, `copy_operation_queue`, `copy_input_queue` and
`copy_session_generation`.

```rust
#[derive(Default)]
pub(super) struct CopyPipeline {
    /// The one outstanding copy request. Only its answer applies.
    awaiting: Option<RequestId>,
    ops: VecDeque<ClientCopyOperation>,
    keys: VecDeque<TerminalKey>,
}
```

Methods: `in_flight()`, `is_awaiting(&RequestId)`, `begin(RequestId)`,
`finish()` (clears `awaiting`), `reset()` (clears all three), `push_op`, `pop_op`,
`has_queued_search()`, `push_key`, `pop_key`, `keys_len()`, `take_keys()`,
`put_keys(VecDeque)`, `clear_keys()`, and test-only `keys_is_empty()` and
`ops_is_empty()`.

- `reset_copy_pipeline()` is `self.copy_pipeline.reset()`. Dropping `awaiting` is
  what makes a late answer stale (L4); the generation counter is gone.
- `abandon_copy_operation` is unchanged in meaning: reset the pipeline and clear
  `copy_after_search`. The request stays in the ledger; its answer finds
  `!is_awaiting` and is ignored.
- `dispatch_next_copy_operation`: return if `in_flight()`; pop one op (empty
  queries skipped as today); with no copy mode, `reset()` and return; build the
  command and `Work::CopyMotion`/`Work::CopySearch` without any session value;
  `submit`; on `Some(id)` `begin(id)`; on `None` (endpoint not online, or no
  snapshot) clear `ops` and `copy_after_search` (the keys stay; the caller's
  replay handles them with nothing in flight, as today); return after the first
  attempt either way. Today the rest stay queued with nothing in flight and are
  sent, stale, on the next copy key after the endpoint returns, and a
  `copy_after_search` set for a search that never left waits for any later
  search; both go (3.9). This makes the type's invariant hold outside the call:
  no `ops` without `awaiting`, except transiently inside one
  `dispatch_next_copy_operation` call (between the pop and `begin` or the clear).
- `complete_copy_motion(request, pane_id, origin, result, now, outcome) -> bool` and
  `complete_copy_search(request, pane_id, origin, query, direction, repeat,
  generation, result, now, outcome) -> bool` are today's two arms. Each starts with
  `if !self.copy_pipeline.is_awaiting(request) { return false; }` (replacing the
  `session_generation` comparison), applies the result as today, then checks
  `is_awaiting(request)` again and returns its repaint without finishing when it
  no longer holds: the apply reset the pipeline (a search with
  `copy_after_search` exits copy mode, and `exit_copy_mode` resets it). That is
  the request-identity form of today's second generation check in
  `complete_copy_operation`, so L4 holds inside the call as well as at entry.
  Otherwise it ends with `finish_copy_operation(continue, outcome)` (today's
  `complete_copy_operation` minus its generation parameter and check: it calls
  `finish()` first, then the existing continue-or-clear-and-replay logic).
- `drop_copy_operation(request) -> bool`: not awaiting, return false; otherwise
  `reset()`, clear `copy_after_search`, true. This is the old `cancel` arm.
- `defer_copy_until_search_result` asks the pipeline: pending when the awaiting
  request's `Work` is a `CopySearch` of this pane and search generation
  (`ledger.work(awaiting)`), or `has_queued_search()`. It no longer counts a
  request an earlier session abandoned.
- `handle_key` and `release_input_leases` (`input.rs`) use `in_flight()`,
  `keys_len()`, `push_key` and `clear_keys()`.

**Word selection** (`word_selection.rs`). `ClientWordSelection::pending_row:
Option<AbsRow>` becomes `pending: Option<RequestId>`. `request_word_selection_row`:
return when `pending.is_some()`; `submit` the read with `Work::WordSelection {
pane_id, row }`; on `Some(id)` set `gesture.pending = Some(id)`; on `None`
`cancel_word_selection()`. `complete_word_selection_row(request, row, result, now,
outcome)` proceeds only when the gesture exists and `gesture.pending ==
Some(request)`; the row for the cache is the entry's. `drop_word_selection(request)`
cancels the gesture under the same condition and returns true, otherwise false.
`word_selection_generation` is deleted: a newer gesture has a different pending
request, so the generation guard's job is done by L4. The gesture's `pane_id`
equality check goes too: a gesture is bound to one pane and request.

**New-workspace label lookup.** `ClientRenameTarget::NewWorkspace::label_lookup_id:
Option<u64>` becomes `label_lookup: Option<RequestId>`;
`next_workspace_label_lookup_id` is deleted. `open_new_workspace_overlay` stores the
id `submit` returned. `complete_workspace_label_lookup(request, reply:
Option<EndpointReply>) -> bool` applies only when the open overlay is the
new-workspace prompt with `label_lookup == Some(request)`, clears it, and applies a
`WorkspaceCheckoutRoot` reply as today; `None` or any other reply clears the lookup
and keeps the suggestion.

**Workspace highlight.** `PendingWorkspaceHighlight` keeps `request_id` (it is
display state with its own deadline, not part of any request's rollback). The two
`request_id` comparisons in `apply_endpoint_result` collapse into
`release_highlight(request_id)` (3.4, D1 and the `Err` branch of `answer_request`).
`focus_endpoint_target` uses the id `submit` returns instead of peeking
`outcome.actions.first()`.

### 3.6 Fields and types removed and added

Removed from `ClientShellState`: `pane_surface`, `pending_pane_surface`,
`pane_surface_generation`, `next_request_id`, `pending_requests`,
`next_scroll_serial`, `pane_scroll_in_flight`, `pane_scroll_queued`,
`pane_scroll_targets`, `word_selection_generation`, `copy_session_generation`,
`copy_operation_in_flight`, `copy_operation_queue`, `copy_input_queue`,
`next_workspace_label_lookup_id`. Added: `surfaces: PaneSurfaces` (landing A);
`ledger: Ledger`, `scroll_lanes: ScrollLanes`, `copy_pipeline: CopyPipeline` (landing B).
Removed types: `PendingEndpointKind`, `PendingEndpointRequest`, and the
`ClientShellEndpointError::Cancelled` variant. `pane_surface(&self)` is kept as an
accessor, not a field.

New module files, mounted in `shell.rs` with `#[path]` like their neighbors:
`shell/presentation/surfaces.rs`, `shell/ledger.rs`, `shell/input/scroll_lanes.rs`.
`CopyPipeline` lives in `copy_mode.rs` beside its only user.

### 3.7 The transport lane (landing B, `endpoint/commands.rs`)

`EndpointCommands` keeps being the per-endpoint wire lane: FIFO, one in flight, a
send deadline, `retire_lane` and `disconnect` reporting `EndpointCommandCancellation`.
It stops tracking request identity beyond the in-flight key. The tombstone list is
deleted:

- `EndpointCommandLane::retired`, `retire`, `consume_retired`,
  `CommandResponseKind`, `response_kind`, and
  `MAX_RETIRED_REQUESTS_PER_ENDPOINT` (`limits.rs`).
- `expire` and `retire_lane` free the in-flight slot without recording a key.
- `receive_response` is `lane.in_flight` key comparison only: a response to a
  command that expired or was retired has no in-flight match and returns `None`,
  which is the outcome the tombstone produced after one extra lookup. Request ids
  are unique and never reused, so an expired id can never match a newer in-flight
  key.
- `PresentationGate::new(role, move_response)` loses `command_response`. A
  `ClientShellEndpointResponse` from the `Shown` endpoint is always `Apply`
  (`receive_response` rejects what is not in flight); `Target` still buffers only
  the move's own responses; `Other` drops. In `handle_server_message` the
  `command_response` computation is deleted. The doc comments on `retire_lane`
  and on the gate say so.

### 3.8 Call sites (landing B)

- `lib.rs::handle_server_message`: `handle_endpoint_result_at` becomes
  `answer_request`; `cancel_endpoint_request(&id)` becomes
  `drop_request(&id, DropReason::Interrupted)`.
- `lib.rs::handle_timer`: same two renames.
- `shell_runtime.rs::cancel_endpoint_commands`: `unsent` ids go to
  `drop_request(id, DropReason::Unsent)`, `possibly_sent` ids to
  `drop_request(id, DropReason::Interrupted)`. `dispatch_client_shell_actions`'s
  rejected action uses `Unsent`.
- `reconcile.rs`: no change beyond the function it calls.
- `focus_endpoint_target` and `actions.rs`: `PendingEndpointKind`, its `cancel`
  impl, `apply_endpoint_result`, `handle_endpoint_result_at_with_cancel_notice` and
  both `cancel_*` wrappers are deleted from `actions.rs` (moved or rewritten in
  `ledger.rs`); `record_binding`, notices and the rest of the file stay.

### 3.9 User-visible behavior changes

1. REJ-011 is gone: a surface patch while the snapshot and surface revisions
   differ no longer drops the connection and the reconnect it forces.
2. Pane hits and copy-mode entry stay live while a surface that skipped ahead of
   its snapshot waits: today such a surface clears `hits.panes` and
   `hits.pane_splits` and clicks are dropped until the snapshot arrives.
3. A first surface after a reset that is ahead of its snapshot shows the
   no-surface placeholder frame until paired. Today a surface that skipped more
   than one revision made `has_surface` true and drew nothing, while one exactly
   one ahead drew the placeholder; both now draw the placeholder.
4. An abandoned copy search no longer makes a later "copy after search" wait for an
   answer that will be ignored.
5. The presentation log context reports the active snapshot generation always.
6. No change for tombstones: a late response is ignored as before.
7. A surface that reaches the shell before the launch connection's first snapshot
   is kept as the baseline and pairs when that snapshot arrives. Today it is
   dropped, and the next patch fails the connection.
8. A pane scroll requested with no snapshot no longer leaves a target behind.
   Today `push` recorded it and the dispatch returned early, so copy mode treated
   the pane as scrolling until a surface happened to show that offset.
9. A copy operation whose command cannot be pushed (endpoint not online, no
   snapshot) drops the operations queued behind it and any pending "copy after
   search". Today they stayed queued with nothing in flight and were sent, stale,
   on the next copy key once the endpoint returned.
10. Not a visible change: a projection reset now runs every request's rollback
    (`drop_all_requests(Reset)`) where `clear()` ran none. The reset clears the
    same feature state right after and `Reset` shows no notice (3.4).

## 4. Obstacles resolved inline

- **Effects need `&mut self` while the surface lives in `self`.** `pair_surfaces`
  and the slow patch path move the enum out with `std::mem::take`, run
  `presented_surface_changed(before, &surface)`, and move it back. The effects
  never read `self.surfaces` (they read the hit map and copy-mode state), so
  nothing observes the empty value. A panic in between would leave `Empty`, which is
  a valid state.
- **The selection invalidation compared the previous surface with the next one;
  the in-place patch has no previous surface.** `PreviousPane` captures the three
  facts the comparison reads for the one selected pane before the change, so no
  surface is cloned.
- **A scroll answer must not resurrect a target, nor overwrite a queued one.**
  `ScrollLanes::answered` sets `target` only when it is still `Some` and nothing
  is queued; a queued offset becomes the target through `sent`, as every dispatch
  records its target today. Tests pin both.
- **A projection change must not copy the grid.** `pair` turns `Paired(s)` into
  `Passed(s)` by a move; the copy is deferred to the first patch in the gap,
  which a full surface normally pre-empts.
- **The first snapshot is not a lost connection.** `lose_baseline` runs only when
  the generation changes from `Some` to something else (S1), so a launch
  surface that precedes its snapshot survives.
- **Replaying queued copy keys runs with nothing in flight.**
  `dispatch_queued_copy_input` takes the key queue, handles a key (which may begin
  a new request and queue later keys into `keys`), and puts the rest back. That
  needs `keys` to exist while `awaiting` is `None`, which is why the struct is flat
  and not an enum over "awaiting". The invariant that matters (no `ops` without
  `awaiting`, except inside one `dispatch_next_copy_operation` call) is stated on the
  type and checked in a test, including the failed-submit case, which clears
  `ops` so the invariant holds when the call returns (3.5).
- **`reset_endpoint_projection` is called from inside `apply_active_snapshot`.**
  `drop_all_requests(Reset)` needs `&mut self` only, shows no notice, and its
  repaint result is ignored by the caller as the clear's was.
- **`Cancelled` was a result and a reason at once.** It is split: only a drop has a
  reason (`DropReason`); an answer has `Timeout` or `Server`. The notice text for
  the interruption lives with `DropReason::Interrupted`.
- **Tests read and write private fields.** Section 5 lists the accessors that
  replace each field use. The `retain` test helper on `Ledger` exists because two
  tests keep only one pending request.
- **Evidence and decoder baseline could differ.** Verified not to at commit (2.3);
  a test pins it.
- **Landing order.** The two landings touch neighboring code (`apply_active_snapshot`,
  `reset_endpoint_projection`, `presented_surface_changed`). A goes first: it moves
  the scroll-target cleanup into the one helper `scroll_target_shown` that still
  reads `pane_scroll_targets`; B then replaces that helper's body with
  `scroll_lanes.shown`. So each landing is green on its own.

## 5. Bricks

Two landings (section 6). Bricks are ordered for an implementer; the tree is green
at the end of each landing.

### Landing A: the surface baseline

**A1. `surfaces.rs`.** New `shell/presentation/surfaces.rs` with 3.2, mounted in
`shell.rs`; `shell.rs`'s `pub(super) use surface_patch::...` line exports
`PatchPresentation`, and a `pub(super) use surfaces::PatchRejection;` line
exports the `pub(crate)`, `Debug` rejection reason (3.3). Unit tests next to the
code:
`lose_baseline_of_a_split_with_nothing_held_is_empty`,
`a_received_surface_becomes_the_baseline_and_pairs_on_an_exact_revision`,
`a_surface_ahead_of_the_snapshot_waits_while_the_last_pair_is_held`,
`a_surface_behind_the_snapshot_waits_for_a_newer_one`,
`a_snapshot_moving_past_a_pair_passes_it_without_a_copy`,
`the_first_patch_after_a_pass_splits_the_baseline_from_the_held_pair`,
`a_full_surface_after_a_pass_replaces_the_baseline_without_a_copy`,
`a_different_boot_never_pairs`, `lose_baseline_keeps_only_the_held_pair`,
`a_patch_without_a_baseline_is_rejected`,
`a_patch_that_does_not_follow_the_baseline_is_rejected_and_changes_nothing`,
`a_patch_on_a_waiting_baseline_leaves_the_held_pair_untouched`.

**A2. State and flow.** `shell/state.rs`: fields (3.3, 3.6), `pane_surface()`
accessor, `receive_pane_surface`, `pair_surfaces`, `presented_surface_changed`
(from `install_pane_surface`), `PreviousPane`/`PaneFacts`/`pane_facts_before`,
`apply_active_snapshot` (with the `Some`-to-other generation condition of S1 and
3.3), `reset_endpoint_projection`, `presentation_log_context`,
`invalidate_pane_surface`; `set_pane_surface` and `install_pane_surface` deleted.
`shell/endpoints.rs::activate_endpoint_projection`. The scroll-target cleanup in the
install effects and the patch fast path becomes one private helper
`scroll_target_shown(pane_id, scroll)` that still operates on
`pane_scroll_targets` (landing B changes its body).

**A3. Patch application.** Rewrite `presentation/surface_patch.rs` per 3.3:
the two outcome enums, one `validate` then `apply_validated`, the simplified
`fast_path_blocker` (drop the `projection_gap` branch; neither unit test in this
file covers it, so both stay as they are apart from the A6 field ports), the
in-place slow path. The gap behavior is covered by
`shell/tests/copy.rs::copy_mode_repeat_during_projection_gap_stays_active` and
`shell/input/mouse.rs::split_release_sends_final_ratio_during_projection_gap`
(A6 ports the latter's fixture), plus the new A6 tests.

**A4. Readers.** `presentation/composition.rs` gate and draw; `input/mouse.rs`
(`pane_split_target_is_current`, `pane_split_topology_matches_hit`,
`split_child_panes`); `input/copy_mode.rs` (two reads);
`navigation/workspace_navigation.rs` (the preview gate).

**A5. Callers.** `lib.rs` (`PaneSurface` arm, the patch outcome mapping and the
rewritten rejection comment and log); `endpoint/view.rs::commit_move`.

**A6. Tests.** `set_pane_surface(` in `shell/tests/*.rs`,
`tests/endpoint_choice.rs` and in-module tests becomes `receive_pane_surface(`,
but not blindly: `receive_pane_surface` keeps every surface as the baseline,
where `set_pane_surface` dropped one with no snapshot, one older than the
snapshot and one older than the current surface. Each site that sets a surface
before its snapshot or at a lower revision than the snapshot or the current
surface is checked and, where the test meant the old drop or meant a presented
surface, rebuilt through the real sequence (snapshot, then surface). Sites that
set the snapshot first at the surface's revision (the common `set_snapshot` then
`set_pane_surface(surface())` shape) are a plain rename.
Direct uses become the accessor, including the reads split over two lines
(`state` then `.pane_surface`, 2.5): `state.pane_surface.is_some()`
and `.is_none()` become `state.pane_surface().is_some()`/`.is_none()`;
`state.pane_surface.clone().expect(...)` becomes
`state.pane_surface().cloned().expect(...)`; `.pane_surface.as_ref()` (including
`cursor_patch` in `surface_patch.rs`'s test module) becomes `.pane_surface()`;
`shell/tests/input.rs:708` becomes `state.surfaces.waiting_baseline().is_some()`;
the assignment at `surface_patch.rs:352` (`state_with_copy_pane_focus`, snapshot
already at the surface's revision) becomes a `receive_pane_surface` call.

`split_drag_state` in `mouse.rs` (the assignments of both fields) is rebuilt
through the real sequence, because the old fixture (snapshot at revision 2, a
presented surface at revision 1) cannot be reached by receiving: a surface at
revision 1 received against snapshot 2 is a waiting baseline with nothing
presented, so `pane_split_topology_matches_hit` would fail and
`split_release_sends_final_ratio_during_projection_gap` would send nothing. The
fixture: snapshot at revision 1; receive the revision 1 surface (pairs); snapshot
at revision 2 (`Passed`, the gap the test name describes); for the `true` case
only, receive the revision 3 surface with the changed topology (a waiting
baseline, which `split_release_is_rejected_when_a_received_future_surface_changed_topology`
needs).

Ported assertions: `future_surface_waits_for_its_exact_snapshot_revision`
(presented revision 1 and `waiting_baseline` revision 2, then presented 2 and none),
`reconnect_same_endpoint_accepts_new_generation_surface_revision` (the held
revision 9 survives the generation change and `compose` still returns `None`).

New shell tests (`shell/tests/endpoints.rs`, beside the ported ones):
`a_patch_on_a_surface_ahead_of_the_snapshot_advances_that_baseline` (REJ-011 case
1: receive a surface at snapshot plus one, apply a patch built on it, `Applied(Held)`;
then the snapshot arrives and the presented surface carries the patched cells),
`a_patch_after_the_snapshot_passed_the_surface_advances_the_baseline` (case 2: the
snapshot moves ahead, two patches in a row are both `Applied(Held)`, then the
matching full surface pairs),
`a_surface_ahead_of_the_snapshot_keeps_the_pane_hits_live`,
`compose_holds_the_last_frame_while_unpaired_and_draws_the_placeholder_when_nothing_was_presented`,
`pairing_a_waiting_baseline_runs_the_selection_and_copy_mode_effects` (a word gesture
whose pane resized is invalidated when the baseline pairs; a copy cursor is clamped),
`a_slow_path_patch_applies_in_place_and_composes` (a blocker present; the outcome is
`Compose` and the presented cells changed),
`a_new_generation_loses_the_baseline_but_keeps_the_held_pair`,
`a_surface_before_the_first_snapshot_stays_the_baseline` (generation `None` to
`Some`: the surface pairs when the snapshot arrives, and a patch that follows it
is `Applied`, not `Rejected(NoBaseline)`),
`a_rejected_patch_reports_its_reason` (each `PatchRejection` variant from a
crafted patch).

New loop tests (new `tests/surface_baseline.rs`, registered in `tests/mod.rs`, using
`Fixture` from `tests/endpoint_choice.rs` plus a new `Fixture::inbound_patch` that
sends `DecodedServerMessage::PaneSurfacePatch` through `handle_event`):
`a_patch_on_a_surface_ahead_of_its_snapshot_does_not_fail_the_connection`
(REJ-011 case 1: a full surface at snapshot plus one, then a patch built on it),
`a_patch_during_a_projection_gap_does_not_fail_the_connection` (case 2: the
snapshot moves past the presented surface, then two patches in a row; today the
first is silently not kept and the second fails the connection),
`a_patch_that_does_not_follow_its_baseline_fails_the_connection`,
`the_commit_baseline_is_the_evidence_surface_and_the_next_patch_applies` (commit a
move, then deliver the patch that follows the evidence surface: not rejected; this
pins 2.3).

These loop tests drive only wire messages through `handle_event`, so with
`Fixture::inbound_patch` (test-only) they compile against today's code. REJ-011
is latent and has never been reproduced, so they are laid first, before A1 to
A5, and the two REJ-011 tests are run and seen to fail there (section 6). That
is the evidence that they reproduce the bug.

### Landing B: the request ledger

**B1. `ledger.rs`.** New `shell/ledger.rs` with `Ledger`, `Entry`, `Work`,
`DropReason`, `answered`/`dropped`, `answer_request`, `drop_request`,
`drop_all_requests`, `submit`, `release_highlight`, the test-only `has_request`
and the kept test-only `handle_endpoint_result` wrapper (moved from `actions.rs`,
now over `answer_request`); mounted in `shell.rs`, which also adds
`pub(super) use ledger::DropReason;`.
`state.rs`: delete `PendingEndpointKind`, `PendingEndpointRequest`,
`ClientShellEndpointError::Cancelled`; add the `ledger` field; extend the file
header's no-`{:?}` note to name `Work`. Unit tests in `ledger.rs`:
`ids_are_unique_and_never_reused`, `an_entry_is_taken_once`.

**B2. Scroll lanes.** New `shell/input/scroll_lanes.rs` with the table in 3.5 and
unit tests `a_queued_offset_exists_only_inside_a_flight`,
`want_while_in_flight_queues_the_latest_offset`,
`an_answer_for_another_request_is_stale`,
`an_answer_does_not_bring_back_a_target_a_surface_already_showed`,
`a_dispatched_queued_offset_is_the_target_until_a_surface_shows_it` (scroll to 3,
then 7 (queued); the answer confirms 3; the target is 7 while 7 flies, and a
surface showing 3 does not clear it),
`a_failure_removes_target_and_queue_together`,
`a_surface_showing_the_target_removes_it_and_an_empty_lane`. Add the
`scroll_lanes` field. Rewire
`mouse.rs` (`push_pane_scroll_offset`, `dispatch_pane_scroll`,
`answer_pane_scroll`, `drop_pane_scroll`), the body of `scroll_target_shown`,
`presented_surface_changed`'s copy-mode check (`scroll_lanes.target(pane)`), the
`retain_panes` call in `apply_active_snapshot` and `scroll_lanes.clear()` in the
reset.

**B3. Copy pipeline.** `CopyPipeline` and its rewiring in `copy_mode.rs`,
`input.rs` (`handle_key`, `release_input_leases`) per 3.5; the two tests in
`input.rs` (`copy_prefix_replays_after_keys_queued_behind_an_operation`,
`copy_escape_cancels_selection_started_by_prior_queued_key`) set the awaiting
request with `state.copy_pipeline.begin("test-request".into())` instead of
assigning the bool, and replace `let generation = state.copy_session_generation;
state.complete_copy_operation(generation, true, &mut outcome);` with
`state.finish_copy_operation(true, &mut outcome);`. Unit tests next to the type:
`reset_clears_the_request_and_everything_queued`,
`a_finished_request_keeps_its_queued_keys_for_the_replay`. Shell tests
(`shell/tests/copy.rs`):
`a_failed_submit_drops_the_queued_operations_and_keeps_the_invariant` (several
operations queued, the endpoint goes offline, the next dispatch fails: no `ops`
without `awaiting` after the call, `copy_after_search` cleared, the keys still
replayed),
`a_search_that_exits_copy_mode_does_not_replay_or_dispatch` (a search answer with
`copy_after_search` exits copy mode inside the apply; the completion returns
without finishing, and nothing queued is replayed or dispatched).

**B4. Word selection and label lookup.** Per 3.5: `word_selection.rs`,
`overlay_input.rs`, `state.rs` (`ClientRenameTarget`, counter removal).

**B5. Delete the old request code.** `shell/navigation/actions.rs` per 3.8;
`shell/endpoints.rs::mark_endpoint_disconnected`; `state.rs`
`reset_endpoint_projection`; every remaining reference to a removed field.

**B6. Call sites.** `lib.rs`, `shell_runtime.rs` per 3.8.

**B7. Lane tombstones.** `endpoint/commands.rs`, `endpoint/message_policy.rs`,
`lib.rs` (the `command_response` computation), `limits.rs` per 3.7. Tests:
delete `retired_request_tombstones_are_bounded`; rewrite
`response_kind_uses_tracked_identity_instead_of_id_text` as
`a_response_matches_only_the_in_flight_request` (an in-flight id matches; a
different id, generation or boot does not) and fix
`in_flight_endpoint_command_expires_and_releases_the_lane` to end on "the late
response is ignored" instead of "consumed its tombstone"; rewrite
`commit_retires_the_previous_command_lane` in `tests/endpoint_choice.rs` to assert
the previous lane has nothing in flight (`disconnect` returns the default
cancellation) and the shell no longer holds the request
(`!shell.has_request(id)`, the `#[cfg(test)] pub(crate)` accessor of 3.4, since
`crate::tests` cannot see the `ledger` field); in
`shell/tests/endpoint_requests.rs::stale_queued_request_is_cancelled_without_blocking_the_current_generation`
replace the two `response_kind` assertions with `receive_response` calls: the
stale id at generation 1 returns `None` and the current id at generation 2
returns `Some` (the last step, so the lane state the test checks is not needed
afterwards); update
the `PresentationGate::new` calls and the tombstone test in `message_policy.rs`
(`a_tombstoned_response_of_the_shown_endpoint_applies` becomes
`any_response_of_the_shown_endpoint_applies`; `only_a_move_response_is_buffered_and_only_a_command_response_applies`
becomes `only_a_move_response_is_buffered_and_a_shown_response_applies`).

**B8. Test ports.** Field uses become: `state.pending_requests.is_empty()` to
`state.ledger.is_empty()`; `.contains_key(id)` to `.contains(id)`; `.len()` to
`.len()`; the two `retain` calls in `mouse_selection.rs` to `ledger.retain(...)`;
`pane_scroll_queued.get(&pane)` to `scroll_lanes.queued(&pane)` (test-only, in
the 3.5 table); `pane_scroll_in_flight.is_empty()`, `pane_scroll_queued.is_empty()`
and `pane_scroll_targets.is_empty()` to one `scroll_lanes.is_idle()` (test-only);
`pane_scroll_in_flight.contains_key(&pane)` to `scroll_lanes.in_flight(&pane)`
(test-only);
`copy_input_queue` to `copy_pipeline.keys_len()`/`keys_is_empty()`;
`copy_operation_in_flight` to `copy_pipeline.in_flight()`;
`copy_operation_queue.is_empty()` to `copy_pipeline.ops_is_empty()`;
`handle_endpoint_result_at` to `answer_request`; `cancel_endpoint_request(id)` to
`drop_request(id, DropReason::Interrupted)`; `cancel_unsent_endpoint_request(id)` to
`drop_request(id, DropReason::Unsent)`; results written
`Err(ClientShellEndpointError::Cancelled)` become the matching `drop_request` call
(the two sites in `endpoint_requests.rs` at lines 47 and 347). Every existing test in
`shell/tests/endpoint_requests.rs` keeps its name and its meaning:
`cancelled_scroll_rolls_back_queued_target_even_without_a_presented_snapshot`,
`mismatched_boot_scroll_result_rolls_back_queued_scroll_state`,
`disconnecting_a_pending_scroll_does_not_show_an_interrupted_action_notice`,
`a_pick_is_applied_at_once_without_an_event_round_trip`,
`selecting_the_shown_endpoint_is_a_noop_but_with_nothing_shown_it_reproves`,
`dispatcher_cancels_pending_requests_on_an_unviewed_endpoint_or_failed_send`,
`stale_queued_request_is_cancelled_without_blocking_the_current_generation`,
`failed_selection_copy_does_not_send_terminal_input`,
`server_errors_become_unavailable_or_rejected_notices`. The copy tests that
exercised the session generation (`copy.rs` near 2241 to 2650) keep their names; a
stale answer is now produced by resetting the pipeline instead of reading the
generation. The one that abandons a session behind a full key queue and then
drops the old request as unsent replaces
`assert_eq!(state.copy_session_generation, generation)` with
`assert!(state.copy_pipeline.is_awaiting(&current_id))` (the drop of the
abandoned request left the current one in charge). Callers of the test-only
`handle_endpoint_result` wrapper keep calling it (B1).

New ledger tests (`shell/tests/endpoint_requests.rs`):
`a_dropped_request_runs_its_rollback_and_sends_nothing` (every `Work` kind:
`drop_request` returns a `bool`, so there is no input to inspect; the test
asserts the ledger did not grow across each `drop_request` and the owning object
is clear),
`an_ignored_answer_still_reports_its_server_error` (an abandoned copy search
answered with `Err(Timeout)` shows the timeout notice and leaves the newer copy
request awaiting; another abandoned one answered `Ok` clears the method's timeout
suppression and changes no copy state; 3.4),
`answering_a_request_twice_applies_it_once`,
`a_cancelled_scroll_takes_its_queued_offset_with_it`,
`a_scroll_answer_does_not_bring_back_a_target_a_surface_already_showed`,
`a_copy_answer_after_the_pipeline_was_reset_is_ignored`,
`an_abandoned_copy_search_does_not_defer_a_later_copy`,
`a_word_selection_answer_for_a_replaced_gesture_is_ignored`,
`a_workspace_label_answer_for_a_reopened_overlay_is_ignored`,
`a_projection_reset_drops_every_request_with_its_feature_state`,
`a_failed_focus_releases_only_its_own_highlight`,
`only_a_plain_request_shows_the_interruption_notice_and_only_when_it_may_have_been_sent`.

## 6. Landing, gate, keep or revert

Two landings, A then B, each independently green (B only swaps the body of the one
scroll-target helper A introduced, and never touches the baseline or pairing). Every named test is gated by
`brokkr check` passing: it runs the gremlins check, clippy and every test, none of
which is `#[ignore]`d. The shell unit tests need no production half reverted to be
seen failing, because the old shapes cannot express what they name (a parked
surface a patch may not apply to, a queued offset with no flight, a rollback that
sends). The loop tests in `tests/surface_baseline.rs` are different: they drive
only wire messages, so they compile against today's code, and the REJ-011 bug
they target is latent and was never reproduced. So landing A starts by laying
`Fixture::inbound_patch` and those tests on today's code and running the two
REJ-011 tests, which must fail there; if either passes, it does not reproduce the
bug and is rewritten before A1.

Commands for landing A, in order:

```
brokkr test -p shepr-client surface_baseline
```

(expected: `a_patch_on_a_surface_ahead_of_its_snapshot_does_not_fail_the_connection`
and `a_patch_during_a_projection_gap_does_not_fail_the_connection` fail; the
other two pass), then A1 to A6, then:

```
brokkr fmt
brokkr check
```

Commands for landing B, in order:

```
brokkr fmt
brokkr check
```

Keep a landing when `brokkr check` is green. Revert the whole landing otherwise;
there is no half-state worth keeping because the old and new owners of the same
fact cannot coexist. This spec commits nothing, and the implementer commits
nothing.

## 7. Stopping rule and what stays out

The rip stops at:

- The client shell's surface fields and the three functions that wrote them, the
  patch application path, and the readers of `pane_surface` (landing A); the
  request map, its kind enum, its completion and cancel code and the per-feature
  in-flight state of scroll, copy mode, word selection and the label lookup (landing
  B); the transport lane's tombstone list and the gate flag that read it; and the
  tests and comments naming any of these.
- **Not touched:** the reader and `shepr_protocol::surface_reuse::Decoder` (the
  shell now mirrors it; `apply_patch_to_surface` is reused as is); the server's
  per-client baseline, render plan and outbox; every wire type; the endpoint choice
  and `Preparing` (its evidence surface follows its own rules and is verified
  equal to the decoder baseline at commit, 2.3); `EndpointCommands`' FIFO, deadline
  and cancellation lists; the supervisor, health and writer; the highlight's display
  state and deadline; selection, copy-mode and mouse behavior other than how they
  hold in-flight state.
- Item 3 of `notes/work.md` (the pane-exit checkpoint).

## 8. Findings outside the item

- `ViewEvidence` in `Preparing` is a third copy of "apply the connection's surfaces
  and patches in order". It differs on purpose (it filters by size and replaces by
  order because it also collects evidence before the first surface), and 2.3 shows
  it equals the decoder at commit. If the shell's `PaneSurfaces` later gains a second
  user, `Preparing` could hold one and make 2.3's argument structural instead of
  verified.
- `presented_surface_changed` still runs on every slow-path patch while a selection
  or copy mode is active. It could skip work when the patch updated no pane the
  selection or copy mode owns; not done because the fast path already excludes the
  common case and the effects are idempotent.
- `Work::CopySearch` carries `generation` (the copy-mode search generation) only to
  cross-check the answer against `copy_mode.search_generation`. With
  `is_awaiting` doing the request-level guard, that cross-check guards a different
  thing (the search state moved while the request flew) and stays.
- The tombstone removal makes `PresentationGate` a pure function of the role and
  the move response. If the gate's remaining `Shown` rule for responses ever needs
  "tracked" again, the lane's in-flight key is the place to ask.
- The shell's patch validation is stricter than the reader's decoder: it also
  checks that patched panes keep their geometry and that every row lies inside a
  patched pane's inner or scrollbar rect, and the server's
  `prepare_pane_surface_patch` checks neither. Moving those two checks into
  `shepr_protocol::surface_reuse` (decoder and `apply_patch_to_surface`) would
  make the reader reject what the shell rejects, so a `Rejected` outcome could
  only be a shell bug and S3 would hold by construction. Not done here: section 7
  keeps the decoder untouched.

## 9. Review disposition

Round 1: two review reports (r1, r2), consolidated. r2's three findings and its
smaller note each coincide with or extend an r1 finding, so they are listed once.

Accepted and folded in:

- Queued scroll offset lost its target on dispatch (r1 F1). `sent` now records
  the offset as the target, as today's dispatch does; `answered` writes the
  confirmed offset only when nothing is queued (3.5, 4); new test in B2.
- `split_drag_state` fixture port was wrong (r1 F2). Rebuilt through the real
  snapshot and surface sequence (A6).
- `response_kind` use in `shell/tests/endpoint_requests.rs` was missing, and
  `ledger` is not visible to `crate::tests` (r1 F3). Replacement assertions and
  a `has_request` test accessor (2.4, 3.4, B7).
- `PatchRejection` and `DropReason` visibility and re-exports (r1 F4).
  `PatchRejection` is `pub(crate)`, `Debug`, re-exported; `DropReason` is
  re-exported (3.2, 3.3, 3.4, A1, B1).
- `lose_baseline` on the first snapshot contradicted S1 (r1 F5). The baseline is
  lost only on a `Some`-to-other generation change; S1 states it; the advice not
  to move it into `mark_endpoint_disconnected` is recorded (3.1, 3.3, 3.9, A6).
- `CopyPipeline` invariant contradicted by the failed-submit path (r1 F6, r2 1).
  A failed submit clears `ops` and `copy_after_search` (keys stay and replay); a
  behavior change in 3.9; test in B3.
- `finish_copy_operation` dropped the post-apply guard (r1 F7). The
  request-identity re-check after the apply is kept; test in B3.
- L2 overclaimed and its test asserted on a nonexistent return value; L1 ignored
  `retain` (r1 F8). L1 and L2 reworded, a `debug_assert!` in `drop_request`, the
  test asserts the ledger did not grow (3.1, 3.4, B8).
- Each patch validated twice on a hot path (r1 F9, r2 3). One `validate`, then
  `apply_validated` (3.2, 3.3).
- `pair` added a full-grid clone per projection change (r1 F10). Replaced by the
  `Passed` and `Split` shape, which copies only on the first patch in the gap and
  makes "never both `None`" a property of the type (3.2).
- The revert-run exemption did not cover the loop tests, and REJ-011 case 1 had
  no loop test (r1 F11). Loop tests are laid first and seen to fail; a case 1
  loop test is added (A6, 6).
- Abandoned answers update notices before the staleness guard (r2 2). Kept as
  today and stated: notices report the server's answer, only feature state is
  guarded; 2.6.6 reworded; test in B8 (2.1, 3.4).
- "Fast path stays allocation-free" was not supported (r2 smaller). Section 1
  now says it avoids any grid copy and still builds the row vector.
- Smaller r1 items: the kept `handle_endpoint_result` test wrapper; the
  `input.rs` direct `complete_copy_operation` calls; the `copy.rs`
  generation assertion; two highlight comparisons, not three; A3's gap-test
  claim; `cursor_patch` missing from 2.5; the nonexistent `complete_pane_scroll`
  comment; the unexpected-result error ordering in `answer_pane_scroll`; the
  `DropReason::Interrupted` expiry wording; `mark_endpoint_disconnected` does run
  rollback; `queued` and `is_idle` added to the `ScrollLanes` table (with
  `in_flight`); the field renamed `scroll_lanes`; missing 3.9 entries; A6's
  blind rename replaced by a per-site check; S3's "only a bug" qualified and
  the decoder follow-up recorded in section 8.
- Found while validating, not in either report: 2.5 missed ten `.pane_surface`
  reads split over two lines (`mouse_selection.rs`, `endpoints.rs`,
  `workspace_navigation.rs`); now listed and covered by A6.

Rejected or narrowed:

- r1's 3.9 entry "`drop_all_requests(Reset)` now runs every rollback" as a
  user-visible change: narrowed. It is a real difference in what runs, recorded
  in 3.4 and 3.9, but nothing visible changes, because the same reset clears the
  restored state immediately afterwards and `Reset` shows no notice.
- r2 2's alternative of ignoring obsolete answers entirely, notices included:
  not taken. The server did receive and answer those requests; hiding a timeout
  or server error because the client lost interest in the result would hide
  real server trouble, and today's behavior already reports them. The finding's
  substance (the spec was silent and 2.6.6 read as contradicting it) is accepted
  above.
- r1 F10's literal suggested shape (`Unpaired { held, baseline: Baseline::Held
  | Own(F) }`): replaced by an equivalent five-variant enum that also removes the
  `held: None, baseline: Held` combination the suggested shape would still allow.
  The finding itself is accepted.
