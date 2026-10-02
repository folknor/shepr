# Technical implementation spec: the session checkpoints as typed state machines

Written against `reference/technical-implementation-spec.md` (the contract this
document must satisfy). Spawned from item 3 of `notes/work.md` ("Server app:
the pane-exit checkpoint as a typed state machine", formerly SAPP-012).

This is a full rewrite of `SessionSaver` and its `App` wiring in
`crates/shepr-server/src/app/session.rs`, not a local change: every field,
every save-outcome path and the deadline rule are replaced. The replay block in
`server/headless.rs` and one constant are local edits.

Contracts inventoried before the survey: `docs/` does not exist; `reference/`
holds only `technical-implementation-spec.md`; `AGENTS.md` and `CLAUDE.md` were
read. Nothing in them describes the session saver, the checkpoints or the
fields this spec replaces (grepped for `checkpoint`, `session_saver`,
`session_revision`; the only hits are the work item and `notes/todo.md`, which
carry no contract). AGENTS.md rules that bind the result: "State is separated
from runtime" (the machines are plain data, tested without PTYs or async), "No
unwrap() in production", the limits-module textlint (every numeric or Duration
bound lives in `crates/shepr-server/src/limits.rs`), and "No god objects". The
workspace lint table in `Cargo.toml` binds too: `variant_size_differences` and
`large_enum_variant` are denied (2.3, 2.4). No contract is changed by this
spec, so no `reference/` or `docs/` file needs an edit. The implementation
edits no notes document and not this spec.

## 1. The problem, as the code has it

`SessionSaver` in `crates/shepr-server/src/app/session.rs` has seventeen
fields. Five describe autosave, the in-flight save and its plumbing
(`session_save_deadline`, `failed_saves`, `in_flight`, `persister`,
`save_finished`); eleven describe two different checkpoints that were grown
into the same struct; one is shared by both:

- Pane-exit checkpoint (hold a signalled pane's removal until the pre-exit
  layout is durable, then keep that layout as the one the final save rewrites):
  `pane_exit_checkpoint_requested`, `_generation`, `_saved_generation`,
  `_snapshot`, `session_revision`, `pane_exit_checkpoint_failures`,
  `pane_exit_checkpoint_ready`.
- Host-shutdown checkpoint (save once on the logind warning, then freeze):
  `host_shutdown_checkpoint_generation`, `_requested`, `_failures`, `_result`.
- Shared by both: `critical_save_retry_deadline`.

What each concept is, read off the code:

- Pane exit: a generation counter (`requested` plus `generation` plus
  `saved_generation` is really one of: nothing held, generation N held,
  generation N durable); a preserved layout valid while no session mutation
  happened since it was captured (`snapshot` plus `session_revision` plus
  `AppState::session_dirty` read from three places); an abandonment (failures
  at or above `CHECKPOINT_MAX_FAILURES`, undone by any successful save of any
  kind); and a `ready` flag that tells the loop "some held exit may have
  become replayable".
- Host shutdown: requested, or finished with a result, or neither; a
  generation that only exists so a cancelled request's late completion is
  ignored.
- `finish_session_save_with_checkpoint` (about 110 lines, four levels of
  nesting) is the one place that reconciles `SessionSavePurpose` with all of
  the above after every save. Every line of it is an invariant between two
  fields. Its failure arm alone sets `critical_save_retry_deadline` to `None`,
  to `now + delay`, or leaves it, depending on a four-way test over (host
  generation matches, host requested, pane requested, pane abandoned).

Failure modes and inconsistencies the shape allows, found in the survey:

1. A preserved snapshot coexists with a newer pending request, and what that
   combination means is left to whichever reader looks at it. It arises when
   two exits are held across one checkpoint: exit A's checkpoint is in flight,
   exit B requests generation 2, and A's save lands and installs its snapshot
   while generation 2 stays requested. The combination is load-bearing (A's
   replay finds the snapshot, so A's removal is not a mutation, and generation
   2's capture is not voided), but it is read two ways at once:
   `pane_exit_checkpoint_settled` treats the snapshot as settling any further
   exit C, while `pane_exit_checkpoint_generation_settled` keeps B held for its
   own save, although the snapshot already holds B's pane exactly as it holds
   C's. The snapshot is cleared only by the next `schedule()`.
2. `pane_exit_checkpoint_snapshot.generation` different from
   `pane_exit_checkpoint_saved_generation`. `capture_final_session_save_job`
   carries a runtime check and a warning for it.
3. A failed autosave that was in flight when a pane exit was requested sets
   `critical_save_retry_deadline`, so the exit's checkpoint waits out an
   autosave backoff of up to `SESSION_SAVE_RETRY_MAX` (30 s) that has nothing to
   do with it. A pane exit is held for that long.
4. Abandoned generations are released by `failures >= CHECKPOINT_MAX_FAILURES`,
   a predicate over a counter that any later successful save resets to zero. A
   held exit that was abandoned and not yet replayed becomes unreleased again
   if an autosave succeeds first. The loop currently replays in the same pass,
   which is what keeps this from showing; nothing in the types says so.
5. After a host checkpoint fails repeatedly (`result == Some((_, false))`),
   `start_background_session_save` refuses to start anything while
   `SessionSaver::deadline` still reports the autosave retry the failure armed.
   The freeze normally lands first (the lifecycle takes the result in the same
   pass, or on the next event), so this costs at most one redundant wake for a
   deadline that cannot start a save, not a spin. It is listed because the
   deadline and the start decision disagree, which a single rule removes.

## 2. The target

Three small plain-data machines, one in-flight record with typed tickets, and
no field that restates another. `SessionSaver` goes from seventeen fields to
six. Everything below is in `crates/shepr-server/src/app/`.

### 2.1 Layout

`session.rs` stays the home of `SessionSaver`, `InFlightSave` and the `App`
wiring (capture, submit, reap, final save). The machines move to a `session/`
directory next to it (the pattern `actions.rs` + `actions/` already uses):

- `session/autosave.rs`: `Autosave`.
- `session/exit_checkpoint.rs`: `PaneExitCheckpoint`, `PreservedLayout`.
- `session/host_checkpoint.rs`: `HostShutdownCheckpoint`.

Each machine is a type with `pub(super)` methods that take their inputs as
arguments (`now: Instant`, `session_dirty: bool`) and read nothing else, so each
is unit-tested with no `App`. `impl App` in `session.rs` lives in the module
that defines `SessionSaver`, so it reaches the saver's private machine fields
(`self.session_saver.autosave`, `.exit`, `.host`) directly; `SessionSaver`
methods exist only where a rule spans several machines or a caller outside
`session.rs` needs one.

### 2.2 `Autosave` (`session/autosave.rs`)

```rust
pub(super) struct Autosave {
    deadline: Option<Instant>,
    /// Consecutive failed saves of any kind, for the backoff.
    failures: u32,
}

impl Autosave {
    pub(super) fn new() -> Self;
    pub(super) fn deadline(&self) -> Option<Instant>;
    pub(super) fn is_due(&self, now: Instant) -> bool;
    /// A session mutation was observed: the save is due `SESSION_SAVE_DEBOUNCE` from now.
    pub(super) fn schedule(&mut self, now: Instant);
    pub(super) fn clear(&mut self);
    /// Counts the failure and arms the retry: the earlier of an existing future
    /// deadline and `now + SESSION_SAVE_RETRY_MIN * 2^(failures - 1)` capped at
    /// `SESSION_SAVE_RETRY_MAX`. Returns the failure count and the delay.
    pub(super) fn record_failure(&mut self, now: Instant) -> (u32, Duration);
    /// Resets the backoff; returns the failures it recovered from, if any.
    pub(super) fn record_success(&mut self) -> Option<u32>;
}
```

This is today's `schedule`, `retry_after_failure`, `retry_after`, `save_is_due`
and `failed_saves` bit for bit (the exponent clamp at 16 and the
"existing deadline only if still in the future" rule move verbatim). `schedule`
no longer also bumps a revision or clears a snapshot; those are
`SessionSaver::note_mutation` (2.5).

### 2.3 `PaneExitCheckpoint` (`session/exit_checkpoint.rs`)

```rust
/// The layout a pane-exit checkpoint made durable, kept so the final save can
/// rewrite it (with fresh history and cwds) instead of the layout after the
/// exited panes left.
pub(super) struct PreservedLayout {
    pub(super) snapshot: shepr_mux::persist::SessionSnapshot,
    pub(super) terminal_ids: HashMap<(usize, u32), shepr_protocol::TerminalId>,
}

/// Generations are issued 1, 2, 3, ... per held exit. `through` is the newest
/// generation released: a held exit with generation <= `through` may be removed.
pub(super) enum PaneExitCheckpoint {
    /// Nothing is held and there is no preserved layout.
    Idle { through: u64 },
    /// Exits wait for generation `generation`'s checkpoint. Generations up to
    /// `through` are already released. No layout is preserved.
    Requested {
        through: u64,
        generation: u64,
        /// Consecutive failed checkpoint saves of `generation`'s epoch.
        failures: u8,
        /// A failed attempt's retry; `None` means start at once.
        retry_at: Option<Instant>,
    },
    /// A checkpoint is durable, no session mutation has been observed since its
    /// capture, and its layout is preserved. Generations up to `generation` (the
    /// newest issued when it landed) are released.
    Saved { generation: u64, layout: Box<PreservedLayout> },
    /// Checkpoints failed `CHECKPOINT_MAX_FAILURES` times in a row. Every
    /// generation ever issued is released (`through`), and no new exit is held
    /// until any save succeeds.
    Abandoned { through: u64 },
}
```

`layout` is boxed: unboxed, `Saved` (a `SessionSnapshot` plus a `HashMap`) is
well over three times `Requested`, which the denied `variant_size_differences`
lint may reject depending on the tag encoding rustc picks. Boxed, `Saved` is
16 bytes against `Requested`'s 40, and the allocation happens once per landed
checkpoint. Should `brokkr check` still report the lint on this enum, the
resolution is `#[expect(variant_size_differences, reason = "one machine per
server; the largest variant is the retry state")]`, not a different shape.

The old `requested` / `generation` / `saved_generation` / `snapshot` /
`session_revision` / `failures` / `ready` seven-tuple is exactly these four
variants. `snapshot.generation != saved_generation` (failure mode 2) has no
encoding, and the snapshot-plus-newer-request combination (failure mode 1) is
not needed: a checkpoint that lands with its layout releases every exit held at
that moment (`saved`, below), so no exit is ever held while a layout is
preserved.

Why a landed layout releases every held exit, not only its own generation: a
held exit's pane stays in the layout until its replay, and no exit newer than
the released range has been replayed, so every pane whose exit is held was
present when the capture was taken, unless it was created after the capture.
Creating a pane is a session mutation, and a mutation observed after the capture
voids the ticket's layout (`note_mutation`, 2.5) or is filtered out at the
finish (`!dirty`, 2.6), so a layout that survives to `saved` holds every held
pane. This is the same reasoning by which today's code settles an exit that
arrives after a preserved layout without a checkpoint of its own
(`exits_after_a_pane_exit_checkpoint_keep_its_layout`); the machine applies it
to exits that arrived while the checkpoint was in flight as well.

Methods (all pure; `generation` is always the value `issued()` returned):

```rust
pub(super) fn new() -> Self;                        // Idle { through: 0 }
fn issued(&self) -> u64;                            // Idle/Abandoned: through; Requested/Saved: generation
pub(super) fn is_released(&self, generation: u64) -> bool;
    // Idle/Requested/Abandoned: generation <= through; Saved: generation <= its generation
pub(super) fn is_requested(&self) -> bool;
pub(super) fn pending_generation(&self) -> Option<u64>;   // Requested.generation
pub(super) fn retry_at(&self) -> Option<Instant>;         // Requested.retry_at
pub(super) fn preserved(&self) -> Option<&PreservedLayout>; // Saved.layout
/// Whether an exit arriving now must wait for a checkpoint: Idle, Requested
/// and a Saved whose session has since changed (`session_dirty`) hold;
/// a Saved with an unchanged session and Abandoned do not.
pub(super) fn would_hold(&self, session_dirty: bool) -> bool;
/// Holds an exit: returns its generation, or `None` when it does not hold.
/// Idle -> Requested { through, generation: through + 1, failures: 0, retry_at: None };
/// Requested -> same state, generation + 1 (failures and retry_at kept);
/// Saved with session_dirty -> Requested { through: generation, generation + 1, 0, None }.
pub(super) fn request(&mut self, session_dirty: bool) -> Option<u64>;
/// A checkpoint save of `generation` succeeded. `layout` is its preserved
/// layout, `None` when it could not be paired with identities or a mutation
/// has been seen since the capture.
///   Requested { generation: cur, .. } with layout Some (any generation <= cur): Saved { generation: cur, layout };
///   Requested with generation == cur and layout None: Idle { through: cur };
///   Requested with generation < cur and layout None: through = max(through, generation), failures = 0, retry_at = None (the newer request waits for its own);
///   any other state: unchanged.
pub(super) fn saved(&mut self, generation: u64, layout: Option<Box<PreservedLayout>>);
/// A checkpoint save of `generation` failed. Returns whether this abandoned
/// the checkpoint.
///   Requested with generation == cur: failures += 1; at CHECKPOINT_MAX_FAILURES -> Abandoned { through: cur } (true); else retry_at = now + checkpoint_retry_delay(failures before the increment) (false);
///   Requested with generation < cur: retry_at = now + checkpoint_retry_delay(failures), failures unchanged (false);
///   any other state: unchanged.
pub(super) fn failed(&mut self, generation: u64, now: Instant) -> bool;
/// Any save succeeded: Abandoned -> Idle { through }; Requested.failures = 0.
pub(super) fn save_succeeded(&mut self);
/// The preserved layout stopped being authoritative (a mutation, or a save of
/// the live layout, replaced it): Saved { generation, .. } -> Idle { through: generation }.
pub(super) fn discard_layout(&mut self);
/// A host-shutdown checkpoint was requested: Requested.retry_at = None, so the
/// combined save starts at once. Other states: unchanged.
pub(super) fn expedite_retry(&mut self);
/// A host-shutdown freeze: held exits are released; Requested -> Idle { through: generation }.
pub(super) fn release_for_freeze(&mut self);
```

`checkpoint_retry_delay(failures_before: u8) -> Duration` is one private free
function in `session.rs` (child modules see their parent's private items, so it
needs no visibility modifier), shared by both checkpoint machines:

```rust
fn checkpoint_retry_delay(failures_before: u8) -> Duration {
    let factor = 1_u32.checked_shl(u32::from(failures_before)).unwrap_or(u32::MAX);
    SESSION_SAVE_RETRY_MIN
        .saturating_mul(factor)
        .min(CHECKPOINT_RETRY_MAX_DELAY)
}
```

It is total for every `u8`, so raising `CHECKPOINT_MAX_FAILURES` can never make
it overflow. With today's bound of three failures it yields 250 ms, then 500 ms,
then the checkpoint is abandoned: today's host-shutdown formula, now used for
the pane-exit checkpoint too (section 4, change 2).

### 2.4 `HostShutdownCheckpoint` (`session/host_checkpoint.rs`)

```rust
pub(super) enum HostShutdownCheckpoint {
    Idle,
    Requested { failures: u8, retry_at: Option<Instant> },
    /// The checkpoint saved. Held until the lifecycle takes the result.
    Saved,
    /// `CHECKPOINT_MAX_FAILURES` attempts failed. Held until the lifecycle
    /// takes the result.
    Unsaved,
}

pub(super) fn new() -> Self;
pub(super) fn request(&mut self) -> bool;        // Idle -> Requested { 0, None } (true); otherwise no-op (false)
pub(super) fn is_requested(&self) -> bool;
pub(super) fn retry_at(&self) -> Option<Instant>;
pub(super) fn is_finished(&self) -> bool;        // Saved or Unsaved
pub(super) fn finished_unsaved(&self) -> bool;   // Unsaved
pub(super) fn saved(&mut self);                  // Requested -> Saved
pub(super) fn failed(&mut self, now: Instant) -> bool;
    // Requested: failures += 1; at CHECKPOINT_MAX_FAILURES -> Unsaved (true);
    // else retry_at = now + checkpoint_retry_delay(failures before the increment) (false). Other states: unchanged.
pub(super) fn take_result(&mut self) -> Option<bool>;   // Saved -> Idle (Some(true)); Unsaved -> Idle (Some(false)); else None
pub(super) fn cancel(&mut self);                 // any -> Idle
```

The two outcomes are unit variants rather than one `Finished { saved: bool }`:
with a `bool` payload the enum has the shape of the seven enums the tree already
carries `#[expect(variant_size_differences)]` for (a one-byte variant against a
24-byte one), and with no data outside `Requested` there is no second variant
for the lint to compare against. Should `brokkr check` still report it, the
resolution is `#[expect(variant_size_differences, reason = "one machine per
server; the largest variant is the retry state")]`.

The generation counter and the `(generation, saved)` tuple are gone. A
cancelled request's late completion is ignored because the saver voids the
host half of the in-flight ticket on cancel (2.5), not because a number
differs; a re-request after a cancel therefore cannot be satisfied by a save
captured before it.

### 2.5 `SessionSaver` and its in-flight record (`session.rs`)

```rust
pub(crate) struct SessionSaver {
    autosave: Autosave,
    exit: PaneExitCheckpoint,
    host: HostShutdownCheckpoint,
    /// At most one save is in flight: a due save waits for it, so every
    /// capture reaches the persister after the one before it finished.
    in_flight: Option<InFlightSave>,
    persister: shepr_mux::persist::SessionPersister,
    /// Fired by the persister each time a submitted save ends.
    save_finished: Arc<tokio::sync::Notify>,
}

struct InFlightSave { pending: shepr_mux::persist::PendingSave, kind: SaveKind }

enum SaveKind { Autosave, Checkpoint(CheckpointTicket) }

/// What a checkpoint save was asked to make durable.
struct CheckpointTicket {
    exit: Option<ExitTicket>,
    /// Whether this save answers the host-shutdown request.
    host: bool,
}

struct ExitTicket {
    generation: u64,
    /// The captured layout; `None` when it could not be paired with terminal
    /// identities, or a session mutation was observed after the capture.
    layout: Option<Box<PreservedLayout>>,
}

pub(super) enum NextSave {
    Autosave,
    Checkpoint { exit_generation: Option<u64>, host: bool },
}
```

`SessionSavePurpose` and `PaneExitCheckpointSnapshot` are deleted
(`SaveKind` / `ExitTicket` / `PreservedLayout` replace them; the revision
field is gone, see `note_mutation`).

Methods of `SessionSaver` (the `pub(crate)` ones keep their current callers):

```rust
pub(crate) fn new(persister, save_finished) -> Self;
pub(crate) fn save_finished(&self) -> &tokio::sync::Notify;
pub(crate) fn deadline(&self) -> Option<Instant>;
pub(crate) fn is_due(&self, now: Instant) -> bool;           // deadline().is_some_and(|d| now >= d)
pub(crate) fn freeze_session_saves(&mut self);
fn note_mutation(&mut self, now: Instant);
fn next_save(&self, now: Instant) -> Option<NextSave>;
fn request_host_checkpoint(&mut self) -> bool;
fn cancel_host_checkpoint(&mut self);
```

`deadline`:

```text
in_flight is some                                   -> None   (its end fires save_finished)
host.finished_unsaved() and !exit.is_requested()    -> None   (nothing may start; failure mode 5)
exit.is_requested() or host.is_requested()          -> the later of exit.retry_at() and host.retry_at(), None if neither (a checkpoint with no retry starts at once, from the call that requested it or from the reap)
otherwise                                           -> autosave.deadline()
```

`next_save`:

```text
in_flight is some                                   -> None
host.finished_unsaved() and !exit.is_requested()    -> None
exit.is_requested() or host.is_requested()          -> Checkpoint { exit_generation: exit.pending_generation(), host: host.is_requested() }
                                                       when every retry_at of the requested machines is None or <= now, else None
otherwise                                           -> Autosave when autosave.is_due(now), else None
```

The "later of" rule waits only when both machines carry a retry, which happens
after a combined save failed for both. A host request never waits for a
pane-exit retry: `request_host_checkpoint` expedites it (below), as today's
`request_host_shutdown_checkpoint` clears `critical_save_retry_deadline`. A
pane exit arriving while a host retry is pending waits for that retry, as today.

`request_host_checkpoint()`: `host.request()`; when that returns true,
`exit.expedite_retry()`; returns the `host.request()` result.

`note_mutation(now)` runs once per consumed `session_dirty`: from
`sync_session_save_schedule`, only when the policy persists the session, as
today, and from the checkpoint branch of `start_background_session_save`
(2.6). It does `autosave.schedule(now)`, `exit.discard_layout()`, and, if the
in-flight save is a checkpoint with an `ExitTicket`, `ticket.layout = None`.
That replaces `session_revision`: a mutation observed after a capture voids
that capture's layout in the ticket, so a stale layout is never installed, and
a mutation observed before the capture is part of it.

`freeze_session_saves()`: `autosave.clear()` and `exit.release_for_freeze()`
(replacing the `requested = false; ready = true` pair). It does not touch the
host machine: the lifecycle takes the host result before freezing. It does not
cancel a save already in flight: one can be (an autosave that became due and
started in the pass that reaped the host checkpoint, before the lifecycle took
its result), and it finishes and writes after the freeze. It captured the
pre-freeze session, so the file it writes is still a pre-freeze state; "nothing
may write the session once it is frozen" means nothing starts after the freeze.

`cancel_host_checkpoint()`: `host.cancel()`, and if the in-flight save is a
`Checkpoint`, set its ticket's `host = false`.

Test-only on `SessionSaver` (replacing field pokes), with the visibility each
caller needs:

- `pub(crate)`: `autosave_deadline() -> Option<Instant>` (the raw field, since
  `deadline()` is None while a save is in flight) and
  `set_autosave_deadline(Option<Instant>)`, called from `app/mod.rs`,
  `app/api/workspaces.rs` and `server/headless/tests/mod.rs`;
  `save_in_flight()`; `hold_test_save_in_flight() -> SaveCompletion` (an
  `Autosave`-kind `InFlightSave`, as today); and
  `hold_test_checkpoint_in_flight(generation: u64) -> SaveCompletion`, an
  `InFlightSave` of kind `Checkpoint(CheckpointTicket { exit: Some(ExitTicket {
  generation, layout: None }), host: false })` built on
  `PendingSave::channel()` the way `hold_test_save_in_flight` is, for the
  headless test that cannot reach the private types.
- private (only `session.rs` tests use them): `expedite_checkpoint_retries()`
  (clears both machines' `retry_at`) and `preserved_layout_mut() -> Option<&mut
  PreservedLayout>`. Private matters for the second: `PreservedLayout` is
  `pub(super)` in `session/exit_checkpoint.rs`, so a `pub(crate)` method
  returning it trips `private_interfaces`.

### 2.6 `App` wiring (`session.rs`)

Kept names and signatures (callers unchanged): `preserves_pane_exit_checkpoint`,
`sync_session_save_schedule`, `reap_finished_session_save`,
`start_background_session_save`, `pane_exit_checkpoint_settled`,
`request_pane_exit_checkpoint`, `pane_exit_checkpoint_generation_settled`,
`request_host_shutdown_checkpoint`, `host_shutdown_checkpoint_result_ready`,
`take_host_shutdown_checkpoint_result`, `cancel_host_shutdown_checkpoint`,
`finish_checkpointed_pane_exit`, `finish_checkpointed_pane_exit_after_event`,
`save_session_before_teardown_async`, `retire_session_writer`, and the
test-only `wait_for_session_save`, `save_session_now`,
`save_session_before_teardown` and `handle_internal_event_after_checkpoint`.

Deleted: `take_pane_exit_checkpoint_ready`, `pane_exit_checkpoint_requested`,
`record_session_save_result` and its two `_with_retry` variants,
`finish_session_save_with_checkpoint`, `record_failed_pane_exit_checkpoint`,
`record_pane_exit_checkpoint_saved`, `finish_final_session_save`,
`capture_pane_exit_checkpoint_snapshot` (replaced by
`capture_preserved_layout`), `SessionSaver::clear_deadline`,
`PaneExitCheckpointSnapshot`, `SessionSavePurpose`.

```rust
fn finish_session_save(&mut self, kind: SaveKind, result: std::io::Result<()>);
```

The one place a save's outcome is applied. It replaces
`finish_session_save_with_checkpoint`, `record_session_save_result*` and
`finish_final_session_save`'s bookkeeping. `now = self.clock.now`,
`dirty = self.state.session_dirty`.

```text
Ok:
  if let Some(n) = autosave.record_success(): tracing::info!(failures = n, "session save recovered after failures")
  exit.save_succeeded()
  match kind:
    Autosave                    -> exit.discard_layout()
    Checkpoint(ticket):
        match ticket.exit:
          Some(e) -> exit.saved(e.generation, e.layout.filter(|_| !dirty))
          None    -> exit.discard_layout()          // a save of the live layout supersedes the preserved one
        if ticket.host: host.saved()
  if exit.is_requested(): autosave.clear()          // the next checkpoint capture supersedes a scheduled autosave
Err(error):
  (n, delay) = autosave.record_failure(now)         // a failed save of any kind re-arms the normal retry
  tracing::warn!(error = %error, failures = n, retry_ms = delay.as_millis(), "session save failed")
  if Checkpoint(ticket):
     if let Some(e) = ticket.exit && exit.failed(e.generation, now):
         tracing::warn!(failures = CHECKPOINT_MAX_FAILURES, "pane exit checkpoint failed repeatedly; removing exited panes without persisting their exit")
     if ticket.host && host.failed(now):
         tracing::warn!("host shutdown checkpoint failed repeatedly")
```

`layout.filter(|_| !dirty)`: a mutation that happened after the capture but
that `sync_session_save_schedule` has not consumed yet (the flag is set, the
loop has not run `note_mutation`) must not preserve the layout; today that
case installs the snapshot and relies on `preserves_pane_exit_checkpoint`'s
`!session_dirty` and the next `schedule()` to undo it.

A successful `Autosave` discards the preserved layout in every state. Today's
code kept it while a pane-exit or host checkpoint was requested. The pane-exit
half has no counterpart (no layout is preserved while an exit is requested,
2.3). The host half differs only when the host checkpoint then fails
`CHECKPOINT_MAX_FAILURES` times: today the final save would then rewrite the
older preserved layout, now it writes the live one, which the autosave that
just succeeded made durable anyway. When the host checkpoint succeeds, both
discard the layout (a host-only checkpoint saves the live layout). Section 4,
change 8.

`reap_finished_session_save`, `wait_for_session_save` (test) and
`save_session_before_teardown_async` / `retire_session_writer` /
`save_session_now` (test) / `save_session_before_teardown` (test) call
`finish_session_save(save.kind, result)` with the taken in-flight record.
`retire_session_writer` previously applied only the counter half of the
outcome (`record_session_save_result`); applying all of it there is harmless
(it clears the autosave next).

`request_pane_exit_checkpoint`:

```rust
pub(crate) fn request_pane_exit_checkpoint(&mut self) -> Option<u64> {
    if !self.policy.persists_session() {
        return None;
    }
    let generation = self.session_saver.exit.request(self.state.session_dirty)?;
    self.start_background_session_save();
    Some(generation)
}

pub(crate) fn pane_exit_checkpoint_settled(&self) -> bool {
    !self.policy.persists_session() || !self.session_saver.exit.would_hold(self.state.session_dirty)
}

pub(crate) fn pane_exit_checkpoint_generation_settled(&self, generation: u64) -> bool {
    !self.policy.persists_session() || self.session_saver.exit.is_released(generation)
}

pub(super) fn preserves_pane_exit_checkpoint(&self) -> bool {
    self.session_saver.exit.preserved().is_some() && !self.state.session_dirty
}
```

(The old `&& !requested` on the non-persisting clauses is dropped: a persisting
policy is the only one under which `Requested` exists, because
`request_pane_exit_checkpoint` refuses otherwise and
`freeze_session_saves` releases `Requested` before the freeze lands.)

`start_background_session_save`:

```text
if !policy.persists_session(): saver.autosave.clear(); return
reap_finished_session_save()
match saver.next_save(clock.now):
  None -> return
  Some(Checkpoint { exit_generation, host }) ->
      if std::mem::take(&mut state.session_dirty): saver.note_mutation(clock.now)
      saver.autosave.clear()
      job = capture_session_save_job()
      ticket = CheckpointTicket { exit: exit_generation.map(|generation| ExitTicket { generation, layout: capture_preserved_layout(&job).map(Box::new) }), host }
      spawn_session_save(job, SaveKind::Checkpoint(ticket))
  Some(Autosave) ->
      saver.autosave.clear()
      spawn_session_save(capture_session_save_job(), SaveKind::Autosave)
```

(The old non-persisting-but-requested exception cannot occur, as above.)
The checkpoint branch consumes a pending `session_dirty` through
`note_mutation` instead of clearing the flag, which today's code does. The
capture includes that mutation, but a preserved layout from an earlier
checkpoint predates it and must go: today a host-only checkpoint that starts
over a pending mutation while a layout is preserved, and then fails, leaves the
layout preserved with the flag clear, so a final save taken before the
autosave retry rewrites the pre-mutation layout. `note_mutation`'s autosave
schedule is cleared on the next line, as today; no save is in flight at that
point, so it voids no ticket. Section 4, change 9.

`spawn_session_save(job, kind)` submits to the persister and stores
`InFlightSave { pending, kind }`.

`capture_preserved_layout(&self, job: &PersistJob) -> Option<PreservedLayout>`
is the old `capture_pane_exit_checkpoint_snapshot` without the generation and
revision: `None` unless `job` is a `Save`, every pane of every workspace has a
terminal id, and the id count equals the snapshot's pane count.

`request_host_shutdown_checkpoint`:

```rust
if !self.policy.persists_session() || !self.session_saver.request_host_checkpoint() { return; }
self.start_background_session_save();
```

`host_shutdown_checkpoint_result_ready` is `host.is_finished()`;
`take_host_shutdown_checkpoint_result` is `host.take_result()`;
`cancel_host_shutdown_checkpoint` is `saver.cancel_host_checkpoint()`.

Final save:

```rust
fn capture_final_session_save_job(&self) -> Option<PersistJob> {
    let Some(layout) = self.session_saver.exit.preserved().filter(|_| !self.state.session_dirty) else {
        return Some(self.capture_session_save_job());
    };
    let job = self.capture_save_job_from_preserved_layout(layout);
    if job.is_none() {
        tracing::warn!("could not pair fresh pane history with the saved pane-exit layout; keeping the durable checkpoint");
    }
    job
}
```

`capture_save_job_from_preserved_layout(&self, layout: &PreservedLayout)` is the
old `capture_save_job_from_pane_exit_checkpoint` over the new type. The
generation-mismatch check and its warning are deleted. Both final-save paths,
`save_session_before_teardown_async` and the test-only synchronous
`save_session_before_teardown`, replace `finish_final_session_save(result)`
with `finish_session_save(SaveKind::Autosave, result)` followed, on success, by
`saver.autosave.clear()`, and replace each `clear_deadline()` with
`saver.autosave.clear()`. `save_session_now` and `retire_session_writer` make
the same `clear_deadline()` replacement.

`finish_checkpointed_pane_exit_after_event(session_was_dirty)`:

```rust
if self.session_saver.exit.preserved().is_none() { return; }
if session_was_dirty {
    self.session_saver.exit.discard_layout();   // mutations after the capture need a current-state save
} else {
    self.state.session_dirty = false;           // removing the checkpointed pane is not a new mutation
}
self.session_saver.autosave.schedule(self.clock.now);
```

### 2.7 The replay loop (`server/headless.rs`)

`handle_scheduled_tasks_headless` today replays held exits only when
`take_pane_exit_checkpoint_ready()` returned true or the policy does not persist
and nothing is requested. With `released(generation)` as the only question, the
flag has no job. The block becomes:

```rust
for _ in 0..self.pending_checkpointed_pane_exits.len() {
    let Some(pending) = self.pending_checkpointed_pane_exits.pop_front() else { break };
    if self.app.pane_exit_checkpoint_generation_settled(pending.checkpoint_generation) {
        self.replaying_checkpointed_pane_exit = Some(pending.checkpoint_generation);
        changed |= self.handle_internal_event_with_forwarding(pending.event);
    } else {
        self.pending_checkpointed_pane_exits.push_back(pending);
    }
}
```

run on every pass (the queue is empty except while a checkpoint is pending,
and a pass already costs one `reap_finished_session_save`). This also removes
failure mode 4: release is a property of the generation, not a flag that must be
consumed in the right pass. `PendingCheckpointedPaneExit`,
`pending_checkpointed_pane_exits` and `replaying_checkpointed_pane_exit` stay as
they are (section 5).

### 2.8 Constants (`crates/shepr-server/src/limits.rs`)

Rename `HOST_SHUTDOWN_CHECKPOINT_RETRY_MAX_DELAY` to
`CHECKPOINT_RETRY_MAX_DELAY` and reword its doc: "Longest retry delay of a
failed pane-exit or host-shutdown checkpoint." The other four
(`SESSION_SAVE_DEBOUNCE`, `SESSION_SAVE_RETRY_MIN`, `SESSION_SAVE_RETRY_MAX`,
`CHECKPOINT_MAX_FAILURES`) are unchanged. No numeric literal is introduced
outside `limits.rs` (`1_u32` and `u32::MAX` in `checkpoint_retry_delay` are the
shift's identity and saturation, not bounds).

## 3. Survey of the ground

Every use of the replaced fields and methods, from a grep of `crates/` and
`src/`:

Production:
- `crates/shepr-server/src/app/session.rs`: the whole subject.
- `crates/shepr-server/src/app/runtime.rs`: `session_saver.deadline()` in
  `next_headless_loop_deadline_with_git_refresh`. Unchanged.
- `crates/shepr-server/src/app/mod.rs`: `SessionSaver::new` in the `App`
  constructor, and `preserves_pane_exit_checkpoint` /
  `finish_checkpointed_pane_exit` in `create_default_workspace`. Unchanged.
- `crates/shepr-server/src/app/events.rs`: `prepare_pane_exit` ->
  `request_pane_exit_checkpoint`, `pane_exit_checkpoint_settled` and
  `finish_checkpointed_pane_exit_after_event` in
  `handle_internal_event_inner`. Unchanged.
- `crates/shepr-server/src/server/headless/internal_events.rs`:
  `pane_exit_checkpoint_generation_settled` and `prepare_pane_exit`.
  Unchanged.
- `crates/shepr-server/src/server/headless.rs`: `sync_session_save_schedule`
  in the loop, `save_finished().notified()` in the wait,
  `save_session_before_teardown_async` at teardown, and in
  `handle_scheduled_tasks_headless` the reap, the start, the ready flag and the
  replay (the one edit, 2.7).
- `crates/shepr-server/src/server/headless/lifecycle.rs`: in
  `sync_host_shutdown_freeze` and `freeze_for_host_shutdown`,
  `cancel_host_shutdown_checkpoint`, `host_shutdown_checkpoint_result_ready`,
  `take_host_shutdown_checkpoint_result`, `request_host_shutdown_checkpoint`
  and `freeze_session_saves`; `retire_session_writer` at stop. Unchanged.
- `crates/shepr-server/src/limits.rs`: the rename (2.8).

Tests touching the replaced surface (each needs the edit named in brick 7):
- `app/session.rs` tests module: all checkpoint tests (rewritten or kept as
  listed in section 6), including the test helpers
  `handle_internal_event_after_checkpoint` (uses
  `take_pane_exit_checkpoint_ready` and `critical_save_retry_deadline`),
  `save_session_now` and `save_session_before_teardown` (both call
  `clear_deadline`; the latter also `finish_final_session_save`), and
  `a_held_pane_exit_settles_on_its_checkpoint_although_the_session_changed_meanwhile`,
  which asserts `!app.pane_exit_checkpoint_requested()`, a deleted method.
- `app/mod.rs` tests: `session_dirty_flag_schedules_debounced_save`,
  `headless_next_loop_deadline_ignores_resize_poll`,
  `headless_next_loop_deadline_returns_none_when_resize_poll_is_only_deadline`,
  `due_session_save_starts_background_writer`,
  `background_session_save_reschedules_when_writer_is_busy`,
  `normal_autosave_replaces_a_signaled_exit_checkpoint`: all poke
  `session_saver.session_save_deadline`; they use `autosave_deadline()` /
  `set_autosave_deadline()` instead. `failed_session_saves_back_off_and_recover`
  calls the deleted `record_session_save_result` and moves to `autosave.rs`.
  `normal_autosave_replaces_a_signaled_exit_checkpoint` and
  `durable_mutation_after_pane_exit_checkpoint_wins_on_shutdown` call the
  test-only `save_session_before_teardown`, which keeps its name (2.6). The
  other tests there only call kept methods.
- `app/api/workspaces.rs`: one test reads `session_save_deadline`; it uses
  `autosave_deadline()`.
- `server/headless/tests/mod.rs`: one test reads `session_save_deadline`; it
  uses `autosave_deadline()`. `test_headless_server` builds the headless struct
  and is untouched (the held-exit fields stay).
- `server/headless/tests/pane_exit.rs`: only `reap_finished_session_save` and
  `pane_exit_checkpoint_generation_settled`; unchanged.

Docs and lint: no `docs/` or `reference/` text mentions any of this;
`brokkr.toml` has no rule keyed on these names. The `numeric-consts-live-in-limits`
textlint is satisfied (2.8). The workspace lints `variant_size_differences` and
`large_enum_variant` are denied in `Cargo.toml`; 2.3 and 2.4 pin the shapes
that keep the new enums inside them. No gremlins in the new files.

Facts the survey established that the design depends on:
- `request_pane_exit_checkpoint` is the only creator of a pane-exit request and
  refuses non-persisting policies, so `Requested` implies persisting.
- The only writer of `AppPolicy::Suspended` is `freeze_for_host_shutdown`, which
  calls `freeze_session_saves` right after; the test that sets it
  directly (`host_shutdown_warning_freezes_saves_...`) has no checkpoint requested.
- `PendingSave` has no cancel; one save is in flight at a time, so the typed
  ticket on the single `in_flight` slot is complete bookkeeping.
- `capture_deferred` and `capture_pending_history` (shepr-mux) and
  `capture_pending_cwds_for_snapshot` / `capture_pending_history_for_snapshot`
  take the snapshot and the `(workspace_index, pane_raw) -> TerminalId` map
  by reference; `PreservedLayout` carries both unchanged, so shepr-mux is not
  touched.
- A held exit's pane stays in the workspace until its replay
  (`prepare_pane_exit` only plans the removal), and publishing an interrupted
  exit does not change the pane's persisted agent identity
  (`agent_release_before_shell_death_preserves_exit_checkpoint_identity`), so a
  capture taken before a held exit's publication holds that pane as the final
  save needs it (2.3).
- In `handle_scheduled_tasks_headless` the reap and start come before
  `sync_host_shutdown_freeze`, and `handle_internal_event_with_forwarding`
  (`server/headless/internal_events.rs`) runs `sync_host_shutdown_freeze`
  before each event it admits, so an unclaimed host result is
  taken within one pass or one event (failure mode 5).

## 4. Behavior changes, named

The rewrite keeps the observable save/checkpoint protocol and changes these
points on purpose. Each has a test (section 6).

1. A failed autosave no longer delays a requested checkpoint (failure mode 3).
   Before, an autosave that failed while a pane-exit checkpoint was waiting set
   the checkpoint's retry deadline to the autosave backoff, up to 30 s. After,
   the checkpoint starts as soon as the failed autosave is reaped; only a failure
   of the checkpoint itself delays it.
2. A failed pane-exit checkpoint retries after 250 ms, then 500 ms, counted from
   its own failures, then is abandoned on the third. Before, the delay was the
   shared autosave backoff, which a history of failed autosaves inflated. (The
   host-shutdown retry is unchanged: 250 ms, 500 ms.)
3. After a host checkpoint fails repeatedly, `deadline()` no longer reports the
   autosave retry that cannot start, which removes one redundant wake before
   the freeze lands (failure mode 5).
4. Abandonment is a variant with its own released range. A later successful save
   of any kind still re-enables checkpoints (`Abandoned -> Idle`), but no longer
   un-releases the generations abandoned before it (failure mode 4).
5. The replay of held exits is evaluated on every scheduled-tasks pass instead
   of behind a flag (2.7). A released exit is replayed in the first pass after
   its release, as before; one that previously could have been stranded by a
   flag consumed in a pass where it was not yet released no longer can.
6. The generation-mismatch warning on the final save and the revision field are
   gone; the states they policed are unrepresentable.
7. Exits held while a checkpoint is in flight are released by that checkpoint
   when it lands with its layout (failure mode 1). Before, exit B, held at
   generation 2 while generation 1's save ran, waited for a second checkpoint
   of its own after generation 1 landed, while an exit C arriving after the
   landing was settled against generation 1's snapshot without one. Now B and C
   are treated alike: generation 1's layout holds both panes (2.3), B is
   released when it lands, and no second save is taken. When generation 1
   lands without a layout (identities unpaired, or a mutation since the
   capture), B still waits for its own checkpoint, as before.
8. A successful autosave discards the preserved layout even while a host
   checkpoint is requested (2.6). The difference shows only if that host
   checkpoint then fails three times.
9. A mutation pending when a checkpoint starts is consumed through
   `note_mutation` (2.6), so it discards a preserved layout that predates it.
   Before, the checkpoint start cleared the flag without that, and a host-only
   checkpoint that then failed left the pre-mutation layout preserved for the
   final save.

Unchanged and pinned by tests: a signalled pane exit holds removal until its
pre-exit layout is durable; a burst of exits after a saved checkpoint settles on
that layout; a mutation after the capture lets the held exit settle but does not
preserve the layout; the final save rewrites the preserved layout with fresh
history and cwds, and keeps the durable file when identities cannot be paired;
a later mutation or autosave replaces the preserved layout; automatic workspace
creation after the last pane's exit does not disturb a preserved layout; a host
checkpoint request starts at once even while a failed pane-exit checkpoint
waits for its retry (today by clearing `critical_save_retry_deadline`, now by
`expedite_retry`; new test `a_host_request_starts_at_once_during_a_pane_exit_retry`).

The preserved layout protects a burst of exits only until the next autosave
lands. That is deliberate and unchanged (pinned by
`normal_autosave_replaces_a_signaled_exit_checkpoint`): the checkpoint exists
so that a crash or host shutdown within the debounce window after a signalled
exit restores the pane, and an autosave that lands after the exit records that
the pane is gone. A signalled exit that lands while that autosave is in flight
or scheduled is settled against the preserved layout and then dropped with it,
like every earlier exit of the burst; the rewrite does not hold such exits for
a checkpoint of their own.

## 5. Stopping rule

In scope: `app/session.rs` and its new `session/` submodules, the one replay
block in `server/headless.rs`, the constant rename in `limits.rs`, the tests
listed in section 3 and section 6.

Out of scope, and why:
- The held-exit queue in `HeadlessServer` (`pending_checkpointed_pane_exits`,
  `replaying_checkpointed_pane_exit`, and the re-entry of
  `handle_internal_event_with_forwarding` for replay). It is a separate shape
  (a flag that routes one handler to skip its preparation step) and belongs to a
  server-loop item, not to the checkpoint machines; this spec only removes the
  coupling to the `ready` flag. It is not deferred work from item 3, which
  names `session.rs`'s fields.
- `ShutdownLifecycle` and the host-shutdown monitor (`lifecycle.rs`): their
  phases are already a typed machine and call only kept methods.
- `shepr-mux`'s persister, `PendingSave`, `SessionBundle`, capture functions.
- `AppState::session_dirty` and `mark_session_dirty`: the pure-state mutation
  signal stays; `sync_session_save_schedule` remains its loop consumer, and the
  checkpoint start consumes it through the same `note_mutation` (2.6).
- The restore-time backup and notice behavior (`restore_notice`,
  `session-backups`): untouched; their tests in `session.rs` stay as they are.

## 6. Landing

One landing. The saver, the three machines, the in-flight ticket, the replay
block and the test edits are mutually dependent (the replay block needs
`is_released`; `finish_session_save` needs all three machines), so no partial
subset builds green. The landing is one commit, kept or reverted on its gate.

### Bricks, in the order they are laid

1. `limits.rs`: rename `HOST_SHUTDOWN_CHECKPOINT_RETRY_MAX_DELAY` to
   `CHECKPOINT_RETRY_MAX_DELAY`, reword its doc.
2. `session/autosave.rs`: `Autosave` (2.2) with its unit tests.
3. `session/exit_checkpoint.rs`: `PreservedLayout`, `PaneExitCheckpoint` (2.3,
   `Saved.layout` boxed) with its unit tests.
4. `session/host_checkpoint.rs`: `HostShutdownCheckpoint` (2.4, two unit
   outcome variants) with its unit tests.
5. `session.rs`: replace `SessionSaver` and its helper types (2.5), including
   `request_host_checkpoint` and the test-only seams with the visibility 2.5
   gives each; add `checkpoint_retry_delay`, `finish_session_save`, the
   rewritten `start_background_session_save`, `request_*`, `capture_*` and
   final-save functions (2.6), and the edits 2.6 lists for the test-only
   `wait_for_session_save`, `save_session_now` and
   `save_session_before_teardown`; delete everything 2.6 lists as deleted. The
   restore-notice and backup tests and their helpers in the `session.rs` tests
   module are kept verbatim.
6. `server/headless.rs`: the replay block (2.7).
7. Mechanical test edits from section 3: replace `session_save_deadline` field
   reads and writes with `autosave_deadline()` / `set_autosave_deadline()`;
   `handle_internal_event_after_checkpoint` drops `take_pane_exit_checkpoint_ready`
   (the settled check alone ends the loop) and replaces
   `critical_save_retry_deadline = None` with `expedite_checkpoint_retries()`;
   `a_final_save_with_missing_checkpoint_identities_keeps_the_durable_layout`
   uses `preserved_layout_mut()` and `layout.terminal_ids.clear()`;
   `a_held_pane_exit_settles_on_its_checkpoint_although_the_session_changed_meanwhile`
   replaces `!app.pane_exit_checkpoint_requested()` with
   `!app.session_saver.exit.is_requested()`.
8. Tests added or rewritten (the list below).

### Tests

Machines (pure, no `App`):

`session/autosave.rs`
- `schedule_sets_the_debounce_deadline`
- `failed_saves_back_off_cap_and_recover` (the logic of the current
  `failed_session_saves_back_off_and_recover`, which moves here)
- `a_retry_never_postpones_an_earlier_pending_deadline`

`session/exit_checkpoint.rs`
- `a_request_on_an_idle_machine_issues_the_next_generation`
- `a_request_while_requested_keeps_failures_and_retry`
- `exits_after_a_saved_layout_with_an_unchanged_session_hold_nothing`
- `a_dirty_session_after_a_saved_layout_requests_again`
- `saving_the_newest_generation_preserves_the_layout`
- `saving_without_a_layout_releases_without_preserving`
- `an_older_generation_saved_with_a_layout_releases_every_held_exit`: request
  twice (generations 1 and 2), `saved(1, Some(layout))`; the state is `Saved {
  generation: 2, .. }`, both generations are released, `would_hold(false)` is
  false and `request(false)` is `None`.
- `an_older_generation_saved_without_a_layout_keeps_the_newer_exit_held`:
  the same two requests, `saved(1, None)`; generation 1 is released, generation
  2 is not, and `request(false)` returns `Some(3)`.
- `a_mutation_discards_the_layout_and_keeps_its_generation_released`
- `three_failures_of_the_newest_generation_abandon_it_and_release_every_generation`
- `a_failure_of_an_older_generation_re_arms_the_retry_without_counting`
- `any_successful_save_ends_the_abandonment_without_unreleasing_abandoned_generations`
- `expediting_clears_the_retry_and_keeps_the_failures`
- `freezing_releases_the_held_generation`
- `issued_generations_never_repeat_through_any_transition`

`session/host_checkpoint.rs`
- `a_request_is_ignored_while_requested_or_unclaimed`
- `two_failures_retry_and_the_third_finishes_unsaved`
- `a_result_is_taken_once`
- `cancel_discards_a_request_and_an_unclaimed_result`

`SessionSaver` (in `session.rs`): each test builds `test_app()` (Test policy,
the persister `App::new` builds) and operates on `app.session_saver` directly;
in-flight records are installed with `hold_test_save_in_flight` /
`hold_test_checkpoint_in_flight` or the same `PendingSave::channel()`
construction, and nothing is submitted to the persister:
- `a_requested_checkpoint_is_chosen_over_a_due_autosave`
- `nothing_starts_while_a_save_is_in_flight_and_no_deadline_is_reported`
- `a_checkpoint_waits_for_the_later_of_the_two_retry_deadlines`: both machines
  requested and both failed once with different failure counts (a combined
  ticket completed with `Err` through `finish_session_save`); `deadline()` is
  the later `retry_at`, and `next_save` is `None` before it and a checkpoint
  with both halves at it.
- `a_host_request_expedites_a_pending_exit_retry`: the exit machine requested
  with a future `retry_at`; after `request_host_checkpoint()`, `next_save(now)`
  is `Checkpoint { exit_generation: Some(_), host: true }`.
- `a_failed_host_checkpoint_reports_no_deadline_and_starts_nothing`
  (failure mode 5)
- `cancelling_the_host_checkpoint_voids_the_host_half_of_the_save_in_flight`
- `a_mutation_after_the_capture_voids_the_in_flight_layout`
- `freezing_clears_the_autosave_and_releases_a_held_exit`

`App` (production policy, real persister over `ScratchDir` paths, the helpers
`two_pane_app` and `saved_pane_counts` already in `session.rs` tests). Where a
test needs a checkpoint in a given state without real saves, it sets it up with
`app.session_saver.exit.request(true)` and applies outcomes with
`app.finish_session_save(SaveKind::Checkpoint(CheckpointTicket { exit:
Some(ExitTicket { generation, layout: None }), host: false }), result)`:
- rewrite `repeated_pane_exit_checkpoint_failures_release_the_held_exit` over
  that setup: failures 1 and 2 keep the exit held with `retry_at` set and
  `pane_exit_checkpoint_generation_settled` false; failure 3 releases it, the
  next `request_pane_exit_checkpoint` is `None`, and a later
  `finish_session_save(SaveKind::Autosave, Ok(()))` makes the next exit hold
  again while the earlier generation stays settled.
- `a_failed_autosave_does_not_delay_a_requested_checkpoint` (change 1):
  hold an autosave in flight with `hold_test_save_in_flight`, request a
  checkpoint with `request_pane_exit_checkpoint` (which starts nothing: a save
  is in flight), complete the autosave with `Err` and reap it; assert that
  `start_background_session_save` leaves a save in flight and that the exit
  machine's `retry_at()` is `None`.
- `a_failed_pane_exit_checkpoint_retries_on_its_own_backoff` (change 2): apply
  six `finish_session_save(SaveKind::Autosave, Err(..))`, then set up the exit
  as above and apply one failed checkpoint outcome; the exit's `retry_at()` is
  `Some(now + SESSION_SAVE_RETRY_MIN)`, while `autosave_deadline()` carries the
  inflated autosave backoff.
- `a_host_request_starts_at_once_during_a_pane_exit_retry`: set up the exit as
  above and apply one failed checkpoint outcome (its `retry_at` is in the
  future); `request_host_shutdown_checkpoint` leaves a save in flight; waiting
  for it settles the exit generation and makes the host result `Some(true)`.
- `a_host_shutdown_checkpoint_saves_the_live_layout_and_finishes` (new
  coverage: the production-policy host path has none today): request, wait,
  `host_shutdown_checkpoint_result_ready`, `take_host_shutdown_checkpoint_result`
  is `Some(true)`, the session file holds the live layout.
- `a_host_checkpoint_supersedes_a_preserved_pane_exit_layout`: after a pane
  exit checkpoint (layout of 2 panes preserved, one pane then removed), a host
  checkpoint writes the live layout (1 pane) and `preserves_pane_exit_checkpoint`
  is false.
- `a_mutation_pending_when_a_checkpoint_starts_discards_the_preserved_layout`
  (change 9): after a pane exit checkpoint with its layout preserved and the
  exit replayed, `mark_session_dirty()`, then `request_host_shutdown_checkpoint`;
  right after the call, with the host save still in flight,
  `preserves_pane_exit_checkpoint()` is false and the exit machine preserves
  nothing.
- `a_cancelled_host_checkpoint_in_flight_does_not_answer_a_new_request`: request,
  cancel, request again while the first save is in flight; the first completion
  leaves the result unready and a second save is started.
- `two_exits_held_by_one_checkpoint_keep_the_pre_exit_layout_through_teardown`
  (change 7; the regression the first draft of this design had): a three-pane
  variant of `two_pane_app` (one more `test_split`). `prepare_pane_exit` for
  two panes, the second while the first's checkpoint is in flight;
  `wait_for_session_save`; both generations are settled. Apply both exits with
  `handle_prepared_pane_exit`, run `sync_session_save_schedule`, then
  `save_session_before_teardown_async` before any autosave; the session file
  holds 3 panes. App level is enough: with the release rule of 2.3 no second
  checkpoint is started, so the loop's reap/start/replay order, which the
  regression depended on, no longer decides the outcome.
- `a_released_exit_is_replayed_by_the_pass_after_the_autosave_that_followed`
  (headless, in `tests/pane_exit.rs`; failure mode 4). Built like
  `server_with_held_runtime_exit`, but `hold_test_save_in_flight()` is called
  before the exit is delivered, so the exit is held (its generation read from
  the queue) and no checkpoint starts. Complete that autosave with `Ok` and
  `reap_finished_session_save()`. Then three times:
  `hold_test_checkpoint_in_flight(generation)`, complete with `Err`,
  `reap_finished_session_save()`; the third abandons the checkpoint. Then
  `hold_test_save_in_flight()`, complete with `Ok`, reap: the autosave lands
  before any scheduled-tasks pass. Run `handle_scheduled_tasks_headless(now)`:
  the queue is empty and the pane is gone.

Kept as they are (they exercise the kept surface), apart from the brick 7 edits:
in `session.rs`
`a_held_pane_exit_settles_on_its_checkpoint_although_the_session_changed_meanwhile`
(gains an assertion that `!preserves_pane_exit_checkpoint()` afterwards),
`a_save_captured_before_a_pane_exit_does_not_settle_it`,
`exits_after_a_pane_exit_checkpoint_keep_its_layout`,
`the_final_save_rewrites_the_checkpoint_layout_instead_of_skipping_it`,
`a_final_save_with_missing_checkpoint_identities_keeps_the_durable_layout`, and
the restore backup and notice tests; in `app/mod.rs`
`pane_exit_checkpoint_survives_automatic_workspace_creation_on_shutdown`,
`detector_release_before_pane_exit_keeps_checkpoint_resume_identity`,
`normal_autosave_replaces_a_signaled_exit_checkpoint`,
`reader_panic_removes_the_pane_without_a_checkpoint`,
`durable_mutation_after_pane_exit_checkpoint_wins_on_shutdown`,
`final_session_save_joins_background_writer_before_returning`; in
`server/headless/tests/pane_exit.rs` both existing tests and in
`server/headless/tests/mod.rs`
`host_shutdown_warning_freezes_saves_before_applying_events_and_thaws_on_cancel`
and `host_shutdown_freeze_waits_for_monitor_cancellation`.

### Gate

Format, then the gate, once for the landing:

```
brokkr fmt
brokkr check
```

`brokkr check` runs the gremlins and textlint checks, clippy (which enforces
the denied `variant_size_differences` and `large_enum_variant`) and every test,
which names and proves every test above; none is `#[ignore]`d.

No test needs a separate run with its production half reverted. A revert run
cannot be made here (the old production half is the deleted surface, and the
new tests do not compile against it), so the evidence that each behavior test
bites is that its assertion is a concrete value the old rule would not
produce:

- `a_failed_autosave_does_not_delay_a_requested_checkpoint`: a save in flight
  after `start_background_session_save`. The old failure arm set
  `critical_save_retry_deadline` to the autosave backoff, and the start
  returned without starting.
- `a_failed_pane_exit_checkpoint_retries_on_its_own_backoff`: a `retry_at` of
  exactly `now + SESSION_SAVE_RETRY_MIN` after six autosave failures. The old
  shared backoff gave `now + 16 s` there.
- `a_failed_host_checkpoint_reports_no_deadline_and_starts_nothing`:
  `deadline()` is `None`. The old `deadline()` reported the autosave retry.
- `two_exits_held_by_one_checkpoint_keep_the_pre_exit_layout_through_teardown`:
  the second generation settled after one save. The old code kept it held for
  a second checkpoint.
- `a_mutation_pending_when_a_checkpoint_starts_discards_the_preserved_layout`:
  `preserves_pane_exit_checkpoint()` false while the host save is in flight.
  The old start cleared the flag and left the snapshot.
- `a_released_exit_is_replayed_by_the_pass_after_the_autosave_that_followed`:
  the queue is empty after the pass. The old autosave success reset the failure
  counter and the generation read as unreleased.

The landing is kept when `brokkr check` passes and reverted when it does not.

Acceptance read of the diff, not a gate: `SessionSaver` has the six fields
listed in 2.5; `grep` for `session_revision`, `critical_save_retry_deadline`,
`pane_exit_checkpoint_ready`, `host_shutdown_checkpoint_generation` and
`SessionSavePurpose` finds nothing under `crates/`.

## 7. Review disposition

Two reviews (r1, r2) were folded into this revision. Accepted and folded: the
overlapping-exits regression in the first draft's `saved` rule (r2 1, folded as
the release rule of 2.3 and change 7, which also resolves r1 2); the host
request waiting for a pane-exit retry (r1 1, r2 2: `expedite_retry`); the
deleted `pane_exit_checkpoint_requested` call in a kept test (r1 3a, r2 3); the
headless test's missing seam (r1 3b); the unnamed synchronous
`save_session_before_teardown` (r1 3c); test-helper visibility (r1 3d); the
`SessionSaver` test construction (r1 3e) and checkpoint setup (r1 3f); the
denied `variant_size_differences` lint (r1 4); failure mode 5 overstated and
failure mode 6 not a failure mode (r1 5); `notes/work.md` (r1 6); the
`schedule_autosave` inconsistency, `checkpoint_retry_delay`'s visibility and
totality, the field count, the autosave-discard difference, the source line
numbers and the full-rewrite label (r1 7); the gate's justification (r1 8);
the checkpoint start swallowing a pending mutation (r1 lateral 1, change 9);
the in-flight save at the freeze (r1 lateral 3, 2.5).

Rejected or resolved differently:
- r1 2 asked to name "a third exit during a newer pending generation is now
  held" as a change and pin it with `request(false) == Some(N+2)` after
  `saved(older, Some(..))`. The state it describes no longer exists: a landed
  layout releases every held exit (2.3), so the third exit is settled as it
  is today, and the change actually made is change 7.
- r2 1 proposed representing a newer pending request together with an older
  preserved layout. Not adopted: the older layout already holds every held
  pane, so the newer request has nothing left to wait for, and keeping it
  would reintroduce failure mode 1's double reading plus a newer capture that
  could lack panes replayed against the older layout.
- r1 lateral 2 (a burst exit during an in-flight autosave settles against a
  layout that autosave then discards) is not a defect of this design: every
  exit of a burst is dropped from the preserved layout once the next autosave
  lands, by design and pinned by an existing test. Holding only exits that
  arrive during the autosave's write would protect them and not the rest of
  the burst. Section 4 now states the rule.
