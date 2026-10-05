# Hunt: when the server saves, and how it shuts down

Scope read in full: `crates/shepr-server/src/app/session.rs`,
`app/session/{autosave,exit_checkpoint,host_checkpoint}.rs`,
`server/headless/lifecycle.rs`, `server/headless/lifecycle/host_shutdown.rs`,
`crates/shepr-server/src/limits.rs`. Followed into `server/headless.rs` (the
loop, `run`, `Drop`, `ctrlc_handler`), `headless/internal_events.rs`,
`headless/bootstrap.rs`, `headless/api_dispatcher.rs`, `headless/tests/{shutdown,
server_stop,pane_exit}.rs`, `app/mod.rs`, `app/outputs.rs`, `app/events.rs`,
`app/actions/events.rs`, `crates/shepr-server/src/backoff.rs`,
`crates/shepr-mux/src/persist/{actor,error}.rs`, `crates/shepr-mux/src/limits.rs`,
`crates/shepr-launch/src/limits.rs`, `crates/shepr-api/src/server/listener.rs`.

---

## 1. Defects

### D1. The host-shutdown freeze (and the logind delay-lock release) waits for an unrelated wake

Claim broken: `host_shutdown.rs` module doc ("the shutdown is held up exactly
as long as the checkpoint takes"), `ShutdownLifecycle::sync_host_shutdown_freeze`
("A warning checkpoints before freezing saves"), and `SessionSaver::blocked`
("the lifecycle freezes saves once it takes that result").

Trace (`server/headless.rs` `run`):

- `sync_host_shutdown_freeze` runs at step 2 of a pass (inside
  `drain_internal_events_with_forwarding_up_to`), and per single
  `LoopEvent::Internal`, and in `handle_scheduled_tasks_headless` only when a
  held pane exit is being replayed.
- The host checkpoint's result becomes visible (`host.is_finished()`) only when
  the save is reaped, which happens in step 5 (`service_session_saves`), after
  the sync of the same pass.
- The persister's completion `Notify` gives one wake. Pass N+1 (woken by it)
  syncs first (result not reaped yet, nothing happens), then reaps in step 5
  (host becomes `Saved`). The loop then computes its next deadline:
  `SessionSaver::deadline()` is `None` (the autosave deadline was cleared when
  the checkpoint started, no checkpoint is requested any more), and with no
  client attached there is no Git or shell-cwd deadline either. The loop sleeps
  with the result unclaimed.
- Consequence: the saver is never frozen and `release_delay_lock` is never
  called until some unrelated event (pane output, a client, an API call)
  arrives. On an idle server with no TUI attached (the overnight case) the
  shutdown is held for logind's full `InhibitDelayMaxSec`, then systemd
  SIGTERMs the server while the saver is still thawed, so the final save runs
  against panes being killed concurrently, which is the race the freeze exists
  to avoid.
- It only works when the save happens to finish inside the same pass it was
  requested in (the stored `Notify` permit then gives a second pass), which a
  real fsync almost never does.
- Deterministic variant: when the persister is `Stopped`,
  `request_host_checkpoint` makes the result `Unsaved` synchronously, but
  `freeze_for_host_shutdown` returns right after requesting, and nothing (no
  save, no notify) wakes the loop again. The freeze then always waits for an
  unrelated event.
- Why tests miss it: `a_frozen_persisting_server_runs_the_final_save_and_writes_nothing`
  drives `sync`, `reap_finished_session_save` and `sync` again by hand in a
  sleep loop, so it never runs the loop's ordering.

Fix: have the reap report "host result ready" and sync the lifecycle right
after `service_session_saves`, or move the lifecycle sync to the end of every
pass, or have `request_host_shutdown_checkpoint` return the immediate outcome
so `freeze_for_host_shutdown` can carry on in the same call. Structural option:
make the lifecycle own a "result ready" wake (notify `outbox_wake` when
`finish_session_save` settles the host machine). Enforceable by a test that
runs `run()` (re-exec like `server_stop.rs`) with the request flag set and
asserts the phase reaches `Frozen` with no other event.

### D2. A refreshed warning during `HostShutdownWarning` is answered by the older checkpoint

Claim broken: `Shared::refresh_warning` logs "host shutdown remains pending
after reconnect; refreshing session checkpoint", and the generation scheme
("only a release for the warning still pending counts").

- `sync_host_shutdown_freeze` only compares generations in `Frozen`. In
  `HostShutdownWarning`, `freeze_for_host_shutdown` reads
  `monitor.warning_generation()` again on the call that takes the result.
- If the monitor reconnects and calls `refresh_warning` while the first
  checkpoint is in flight, the loop releases the lock for the new generation
  with a checkpoint captured for the old one. No refreshed checkpoint is taken.
- The generation that freeze records is the one current at freeze time, not
  the one the checkpoint was requested for.
- Fix: record the generation in the request (`request_host_shutdown_checkpoint(generation)`)
  and restart when it no longer matches, as the `Frozen` branch already does.
- Testable as a unit test on the lifecycle with a fake monitor generation;
  today `monitor` cannot be faked (see V5).

### D3. Final-save failure is swallowed and mis-described

Claim broken: AGENTS.md "Shutdown keeps the socket through the final save,
retires the lease, then removes the socket". The ordering holds, but nothing
reports whether the save succeeded.

- `save_session_before_teardown_async` throws away the `bool`
  `finish_final_session_save` returns (and so does the test-only sync twin).
  The only trace of a failed final save is the generic
  `warn!("session save failed", retry_ms = ...)` from `finish_session_save`,
  which promises a retry that never happens because the process is exiting.
- `shepr stop` and `run()` report a clean stop either way.
- A `JoinError` from `wait_off_the_runtime` is mapped to
  `SaveError::Abandoned`. That is non-retryable, so it logs "session persister
  cannot accept further saves; disabling session persistence for this boot"
  and marks the shell projection dirty. Both are wrong for a final save.
- Fix: a `FinalSave` save kind with its own outcome: an error-level line naming
  the data directory and the error, no retry wording, and a
  `RunServerError`/exit-class signal so the operator learns of it.
- Mechanically: a type (a `FinalSaveOutcome` that is `#[must_use]`), and a test
  that injects a failing persister into the final save.

### D4. `CHECKPOINT_RETRY_MAX_DELAY` has no effect

Claim broken: its doc says it is the longest retry delay of a failed
checkpoint.

- With `SESSION_SAVE_RETRY_MIN = 250ms`, `BACKOFF_MULTIPLIER = 2` and
  `CHECKPOINT_MAX_FAILURES = 3`, the delays actually used are 250ms and 500ms.
  The third failure abandons before any delay is computed, so the 1s cap is
  never reached.
- The cap also has no relation to logind's `InhibitDelayMaxSec` (default 5s),
  which is the budget the host checkpoint actually lives in (see V3).
- Enforceable with a `const _: () = assert!(...)` in `limits.rs`: the
  uncapped delay after `CHECKPOINT_MAX_FAILURES - 1` failures must reach or
  exceed the cap, or else remove the cap.

### D5. `ShutdownLifecycle::freeze`'s stated reason is false

The field doc says folding the freeze into the phase "would make
`ShutdownPhase` carry data and lose the `Copy` that `UnexpectedPhase` relies
on". But `HostShutdownFreeze` holds only `Option<WarningGeneration>`, and
`WarningGeneration` is `Copy`, so `ShutdownPhase::Frozen { generation:
Option<WarningGeneration> }` stays `Copy`. The parallel `Option` field and its
"Some only in Frozen or Stopping, readers check the phase first" call-order
invariant exist on a false premise.

`cancel_host_shutdown` and `restart_host_shutdown_warning` return
`Option<HostShutdownFreeze>`, but both callers only use `.is_some()`, so the
payload is never read (see N3). This can be checked by making the bad state
unrepresentable: fold it into the phase.

### D6. `Shared::warning_generation` doc describes another function

The doc reads "Whether the server has checkpointed for the warning now
pending.", but the function returns the pending warning's generation (or
`None`). The text was copied from `checkpointed` right below it. This is false
today and nothing checks it.

### D7. "releasing the delay lock" is logged when no lock exists

`freeze_for_host_shutdown` warns "host shutdown checkpoint failed repeatedly;
releasing the delay lock". When the monitor connected after preparation began
(`take_inhibitor_if_idle` returned `None`), or no monitor runs, there is no
lock, so the message is false in those cases. It also duplicates session.rs's
own "host shutdown checkpoint failed repeatedly" warn (see C2).

---

## 2. One value, one owner

### O1. Server shutdown time vs the launcher's stop wait

`shepr-launch::limits::STOP_WAIT_TIMEOUT` (15s) and `STOP_LEASE_WAIT_TIMEOUT`
(10s) must outlast the server's worst stop:

- `SHUTDOWN_FLUSH_TIMEOUT` (1s)
- plus the final save (unbounded by design)
- plus `PANE_TEARDOWN_WAIT` (3s)

Nothing relates them. The server cannot see launch's limits (`shepr-server` does
not depend on `shepr-launch`), but `shepr-daemon` links both, so a
`const _: () = assert!(...)` there could hold the bounded part. The final-save
term cannot be bounded by design (see the "no forced stop" comment in `run`).
Say so beside the assert.

### O2. `PANE_TEARDOWN_WAIT = BUDGET.saturating_mul(4)`

The doc explains 4 as "their signal budget, plus three more of it for the /proc
session scans between signal rounds", so the 3 is the length of
`shepr_mux::limits::PANE_TEARDOWN_STEPS`, written as a literal in another
crate. Adding or removing a signal step leaves the factor stale. Fix: mux
exports the scan-inclusive wait (it owns the steps), or exports
`PANE_TEARDOWN_STEPS.len()`. A type or derivation enforces it.

### O3. `APP_EVENT_CHANNEL_CAPACITY` reused as the stop-path drain limit

`run` calls `drain_internal_events_with_forwarding_up_to(APP_EVENT_CHANNEL_CAPACITY)`
on stop. The channel's capacity is being used as "drain everything queued".
`drain_all_internal_events_with_forwarding` (`outputs.queued_events()`) already
expresses that. Two spellings of one rule; use the queued count. This could be
caught by a textlint, though a review is probably enough.

### O4. The list of backoff users is restated in two docs

`limits.rs` `BACKOFF_MULTIPLIER` ("session writes, checkpoints, default
workspace creation and logind reconnects") and `backoff.rs`'s struct doc both
restate the same list, and it will drift. The limits doc also says "Growth
factor of every retry backoff", which is false workspace-wide:
`shepr-api/src/server/listener.rs` `AcceptBackoff` doubles with a literal `2`,
and the API crate cannot see the server's `Backoff`. Reword to "every
`Backoff`" and either move `Backoff` down a layer (core or platform) for the
API listener to share, or say why that one stays separate. A textlint on
`saturating_mul\(2\)` with a "backoff" nearby is weak. Moving the type down is
the real fix.

### O5. Checkpoint retry minimum borrows the autosave minimum

`checkpoint_retry_delay` uses `SESSION_SAVE_RETRY_MIN` with
`CHECKPOINT_RETRY_MAX_DELAY`. A person tuning checkpoints finds a `MAX` and
`MAX_FAILURES` but no `MIN`, and changing the autosave minimum silently changes
the checkpoint schedule and its total budget against logind. Name a
`CHECKPOINT_RETRY_MIN` (it may equal the other, with a const assert if they
must match).

### O6. Tests restate the constants' current arithmetic

- `a_failed_pane_exit_checkpoint_retries_on_its_own_backoff` asserts the
  autosave deadline is `SESSION_SAVE_RETRY_MIN * 64` after seven failures.
  That hard-codes the multiplier 2 and that 16s is under `SESSION_SAVE_RETRY_MAX`
  (30s). Raising `SESSION_SAVE_RETRY_MIN` to 1s breaks it for no behavioural
  reason.
- `two_failures_retry_and_the_third_finishes_unsaved` hard-codes MIN and MIN*2
  and the count 3.
- `three_failures_of_the_newest_generation_...` names 3 while looping on the
  constant.

Compute expectations through `Backoff`/`checkpoint_retry_delay`, and name
tests by the constant ("max failures"), not its value.

### O7. Product-name spelling in the logind inhibitor

`take_inhibitor` registers who="Shepr", why="Save terminal workspace layout",
which shows in `systemd-inhibit --list`. Elsewhere the product is `shepr`
(AGENTS.md, the window title `shepr: <label>`), though bootstrap says "Shepr
TUI". This is operator-facing text with no owner. Minor; there is no
mechanical check short of a textlint on `"Shepr"`.

---

## 3. Values nobody can find, change, or trust

### V1. The save and shutdown tunables are a scattered set

The save and shutdown tunables are:

- server `limits.rs`: `SESSION_SAVE_DEBOUNCE`, `SESSION_SAVE_RETRY_MIN`/`MAX`,
  `CHECKPOINT_RETRY_MAX_DELAY`, `CHECKPOINT_MAX_FAILURES`,
  `SHUTDOWN_FLUSH_TIMEOUT`, `PANE_TEARDOWN_WAIT`, `SHUTDOWN_RECONNECT_*`
- mux limits: the teardown steps
- launch limits: the stop waits
- logind's `InhibitDelayMaxSec`, which is external and not mentioned anywhere

No document lists the shutdown time budget end to end, and `reference/` has
nothing on saves or shutdown at all. A short "shutdown budget" section in
`reference/` (or a doc block at the top of the server's "Startup and shutdown"
limits group) is the place a tuner would look. This is not mechanically
enforceable beyond the const asserts in O1/D4.

### V2. Debounce, retry and checkpoint constants have no injection point

They are read inside `Autosave::schedule`, `RETRY_BACKOFF` and
`checkpoint_retry_delay`. Tests work around this with
`set_autosave_deadline`, and the persisting tests (`handle_test_runtime_exit_and_replay`,
`wait_for_checkpoint`) spin on a real writer thread with 5s wall-clock
timeouts. Acceptable, since the clock is injected and only values are fixed,
but a `SavePolicyConfig { debounce, retry, checkpoint }` handed to
`SessionSaver::new` would let the tests drop the hand-set deadlines.

### V3. The host checkpoint's real budget is logind's, and nothing says so

The retry loop (250ms, 500ms, then give up) plus write times must fit
`InhibitDelayMaxSec`. A slow disk that takes 3s per attempt exceeds the default
5s, and logind then proceeds without the checkpoint while the server keeps
retrying. Nothing documents or reads that limit. The monitor could read the
`InhibitDelayMaxUSec` property from the manager and hand it to the lifecycle,
which would turn retries into a deadline rather than a count.

### V4. `SessionOpenPolicy::Never` is a test-only switch that production code carries

Production always opens with `Persist` (`bootstrap.rs` `start_server`). `Never`
(and with it `SavePolicy::Never`, `SaveMode::Never`, the lease-only persister in
mux, `session_persists()`'s false case, and the `!app.session_persists()`
branch of `sync_host_shutdown_freeze`) is reached only by `App::new` in tests
and `agent_report_test_support`. So:

- there is a production branch no production configuration takes
- `host_shutdown_freeze_waits_for_monitor_cancellation` and
  `host_shutdown_warning_freezes_saves_before_applying_events_and_thaws_on_cancel`
  test the host-shutdown path under a policy production never runs (see T1)

Either make tests open `Persist` on a scratch directory (most already do via
`persist()`), or keep `Never` and say in its doc that it is a test seam.
Per AGENTS.md's "No production crate has a test feature" spirit, prefer
removing it. Enforceable by deleting the variant.

### V5. The logind monitor has no injection point for the bus or the delays

`monitor()` hard-codes `zbus::Connection::system()` and
`Backoff::new(SHUTDOWN_RECONNECT_*)`. The reconnect loop, the refresh path,
the Ok-return-without-delay path (P4) and the backoff reset while pending are
untested. The only D-Bus test calls `watch_connection` directly with
`refresh_pending_warning = false`, and its own comment admits the refresh is
untested. Taking a connection factory and a `Backoff` as parameters would make
`monitor` testable against the private bus the test already starts.

---

## 4. One channel, one implementation

### C1. Three or four info lines per warning, only one carrying the generation

Per warning:

- the monitor logs `event = "host.shutdown.request"` with `generation`
  ("host shutdown requested; preserving session ...")
- `freeze_for_host_shutdown` logs "host shutdown announced; checkpointing the
  session and freezing saves" on its first call and again on the call that
  takes the result, because the same function is re-entered

The lifecycle lines carry no `event`, `subsystem` or `generation`, and when
the session does not persist the line still says "checkpointing". Cancellation
logs twice as well: the monitor's "host shutdown cancelled", then the
lifecycle's "host shutdown cancelled; resuming session saves". When the
cancellation lands in `HostShutdownWarning` the lifecycle logs nothing. Give
the lifecycle one line per transition (request, freeze with outcome, cancel,
restart), each with the generation and the same `event`/`subsystem` scheme as
the monitor. Mechanical: a textlint requiring `event =` in `lifecycle/**` info
and warn calls is possible but crude.

### C2. Two warns for one exhausted host checkpoint

`finish_session_save` warns "host shutdown checkpoint failed repeatedly", and
the lifecycle then warns "host shutdown checkpoint failed repeatedly;
releasing the delay lock". Keep one, in the lifecycle, which knows whether a
lock exists (D7).

### C3. `"session save failed"` omits what failed

The warn has `error`, `failures` and `retry_ms`, but not the save kind
(autosave, pane-exit checkpoint with its generation, host checkpoint, final
save), nor the data directory. A person reading the log cannot tell whether
an exited pane is now held or the shutdown checkpoint is retrying. Add
`kind` and `generation` fields.

### C4. No line says a final save happened

The success of the last save before exit is not logged. The log goes from
"completing server shutdown" to "headless server exiting", and only a failure
appears (with retry wording, D3). An info line with the outcome and duration
is what an operator investigating a lost layout needs.

### C5. The teardown warn names nothing

`"pane session teardown did not finish before server exit"` gives no count
and no pane ids. `PaneTeardownTracker::wait` returns only a bool. Return the
unfinished ids and log them.

### C6. The refusal message for a frozen or stopped saver

When the persister stops, `tracing::error!` says "disabling session persistence
for this boot", and the client learns through `session_saves_stopped` in the
snapshot (good). When a host-shutdown freeze is in force no client is told
anything. That is by design (the host is going down), but a cancelled shutdown
that never thaws (D1 delays the freeze and its cancel symmetrically) would be
invisible. Noted only as the surface where it would show.

---

## 5. Errors

### E1. Final-save outcome dropped

See D3. `finish_final_session_save`'s `bool` has no reader in production.

### E2. `wait_off_the_runtime` maps a cancelled blocking task to `Abandoned`

That disables persistence "for this boot" and dirties every client's
projection on a path that only runs at exit. The `JoinError` (panic vs
cancellation) is dropped entirely: it is not logged and not kept as a source.
Keep the `JoinError` in the error, and treat it as a final-save failure, not a
persister refusal.

### E3. `ShutdownLifecycle::shutdown_error` asserts the phase

`assert_eq!(self.phase, ShutdownPhase::Stopping)` panics the event loop on a
caller that rejects a request outside Stopping. All three callers are after
`initiate_shutdown` today (call-order safety). A `Stopping` proof token
returned by `begin_stopping`/`initiate_shutdown`, which
`reject_api_request_for_shutdown` requires, makes it structural.

### E4. Guards that can never fire

`RunServerError::Shutdown(UnexpectedPhase)` and `ShutdownStep::CompleteShutdown`
exist for `complete_shutdown`'s `require_phase`, which the loop only calls
after checking `phase() == Stopping`. `freeze_for_host_shutdown`'s
`require_phase` and `finish_host_shutdown_freeze`'s second `require_phase`
are likewise unreachable from their callers. They are harmless, but they are
error types, `Display` impls and tests guarding states the callers exclude.
With D5's fold and E3's token, transitions can take the phase by value and
these disappear.

---

## 6. Tests that prove nothing

### T1. Host-shutdown lifecycle tests run under the never-persisting policy

`host_shutdown_freeze_waits_for_monitor_cancellation` (lifecycle.rs) and
`host_shutdown_warning_freezes_saves_before_applying_events_and_thaws_on_cancel`
(tests/shutdown.rs) build `App::new`, whose saver is `Never`.

- The freeze takes the `!session_persists()` shortcut: no checkpoint is
  requested, so neither test exercises request, result, freeze or release.
- `assert!(!app.session_persists())` after the thaw cannot fail under `Never`.
- The first test's name says it "waits for monitor cancellation" with no
  monitor running. Its last assertion, `assert!(!lifecycle.has_monitor())`
  with the comment "none was started by the thaw", checks something no code
  path could do.
- Its second and third `sync` calls assert the same thing twice.

### T2. The one persisting freeze test drives the loop's steps by hand

`a_frozen_persisting_server_runs_the_final_save_and_writes_nothing` polls
`reap_finished_session_save` with `std::thread::sleep(1ms)` up to 5000 times,
then syncs again. That reorders exactly what D1 is about, so the test passes
while the loop stalls. It also depends on the wall clock.

### T3. Test-only twins of production paths

`save_session_now`, `save_session_before_teardown` (sync) and
`wait_for_session_save` are `#[cfg(test)]` re-spellings of
`save_session_before_teardown_async` / `reap_finished_session_save`.

- `final_session_save_joins_background_writer_before_returning` is named for
  the final save but calls `save_session_now`, a test helper, so the
  production final save's join is untested by it. It also uses a 30ms sleep to
  order threads.
- `normal_autosave_replaces_a_signaled_exit_checkpoint` and
  `durable_mutation_after_pane_exit_checkpoint_wins_on_shutdown` end with the
  sync twin rather than the async production path.

Drive the async path (most neighbouring tests already do with `#[tokio::test]`)
and delete the twins.

### T4. Tests that mix the wall clock with the app clock

`due_session_save_starts_background_writer`,
`background_session_save_reschedules_when_writer_is_busy` and
`normal_autosave_replaces_a_signaled_exit_checkpoint` set the deadline to
`Instant::now() - 1s` while the saver compares it with `app.clock.now`, which
was sampled when the app was built (`test_clock()`). If more than a second
passes between building the app and that line (a loaded machine; the persister
thread spawn and real saves sit in between), the save is not due and the test
fails. Use `Some(app.clock.now)` as `save_session_now` does. A textlint
banning `Instant::now()` next to `set_autosave_deadline` is possible; simpler
is making the seam take no argument (`make_autosave_due()`).

### T5. A test that cannot fail on its claim

`background_session_save_reschedules_when_writer_is_busy` asserts that a save
is in flight and the autosave deadline `is_some()`. Both hold whether or not
the busy writer is why the save was deferred: the held save is in flight
either way, and a deadline that was not due (T4) also stays `Some`. Assert
that `deadline()` is `None` while in flight and that the next pass after the
reap starts the save.

### T6. The D-Bus monitor test depends on the host

`delay_lock_is_held_until_checkpoint_and_retaken_after_cancellation`:

- It spawns the host's `dbus-daemon` (`command_in_scratch("dbus-daemon", ...)`).
  It is `#[ignore]`, so `brokkr check` never runs it, and `brokkr test -p
  shepr-server host_shutdown` runs it (`--include-ignored`) and fails on a host
  without `dbus-daemon`.
- The `no-borrowed-process-stand-ins` textlint lists only shells and
  coreutils, so this borrowed host program is not flagged. Either add
  `dbus-daemon` with a `host-program-ok:` marker (its subject is real D-Bus
  behaviour) or say so in the brokkr rule's preset.
- It also uses `tokio::time` sleeps with 5s wall-clock timeouts (stated as
  harness guards).

### T7. Lifecycle preconditions asserted by their own tests

`a_test_server_holds_the_data_directory_lease` exists only as the precondition
of `the_lease_is_free_by_the_time_the_socket_goes`. That is fine. Note that
the latter observes the lease with `DataDirLease::acquire(...).is_ok()` and
drops the acquired lease at once inside the closure, before the socket goes,
which is fine for the claim.

---

## 7. Guards and claims that have stopped holding

### G1. Comment claims the code does not keep

- `ShutdownLifecycle::freeze` "lose the Copy" (D5): false today.
- `Shared::warning_generation` doc (D6): false today.
- `sync_host_shutdown_freeze`'s inner comment "The monitor wakes the loop when
  this flag changes, so a later batch observes it" is true for the flag. The
  lifecycle's implied promise that the checkpoint result is acted on promptly
  is false (D1).
- `Autosave::record_failure` doc "re-capture and rewrite the whole session four
  times a second" restates `SESSION_SAVE_RETRY_MIN = 250ms`. Reword to "on
  every retry minimum".
- `limits.rs` `BACKOFF_MULTIPLIER` "every retry backoff" (O4): false
  workspace-wide.
- `CHECKPOINT_RETRY_MAX_DELAY` "Longest retry delay" (D4): never reached.

### G2. Claims nothing enforces, and whether they could be

- "Every exit runs this [`release_socket_after_save`], including error and
  unwind exits through `Drop`": true, and it holds structurally through
  `Drop`.
- The lease-before-socket order is tested
  (`the_lease_is_free_by_the_time_the_socket_goes`).
- The final save before the lease: `run` orders `save_session_for_exit` before
  `release_socket_after_save`. No test runs `run()` with a persisting server
  and checks the file exists at the moment the socket goes. `server_stop.rs`'s
  re-exec test could assert that cheaply: write a mutation, stop, check the
  session file.
- `SessionSaver::next_save` says it decides "by the same rule as `deadline`".
  The two are separate implementations of one rule (the `.max()` versus the
  `.any(now < d)` over the same two retries). A test or derivation
  (`next_save` from `deadline`) would enforce it. Also, `deadline() == None`
  means both "nothing to do" and "a requested checkpoint is due now"
  (`is_due` reads `None` as not due). Correctness rests on every requester
  calling `start_background_session_save` itself. It holds today
  (request, expedite, reap and thaw-then-request all do), but one missed call
  would leave a held pane exit stuck with no deadline. Return an enum
  (`Idle`, `At(Instant)`, `Now`) instead.

---

## 8. Policy invented per call site

### P1. The session dirty bit is consumed even when saves are disallowed

`sync_session_save_schedule` evaluates `take_session_dirty()` before
`allows_saves()`, so while frozen or stopped every mutation's dirty bit is
dropped. `preserves_pane_exit_checkpoint` treats "not dirty" as "no mutation
since the preserved capture", and that is false after a freeze. Correctness
today rests on three other writers:

- `resume_session_saves_after_cancel` re-marks dirty on cancel
- a frozen final save writes nothing
- a restart's host checkpoint captures the live state and discards the
  preserved layout

That is an invariant kept by writers who never meet. Also,
`finish_checkpointed_pane_exit_after_event` writes `self.state.session_dirty =
false` directly (bypassing `take_session_dirty`), resting on a comment's
argument that "no other mutation can interleave". It also calls
`autosave.schedule` without checking the policy, unlike `note_mutation`
(harmless while blocked, but the policy gate is per call site). Structural
fix: a mutation epoch counter that `CapturedLayout` records at capture, so a
preserved layout is authoritative iff the epoch is unchanged. That replaces
the dirty bit's double duty and the direct field write.

### P2. `SavePolicy` and `SaveMode` encode (mode, frozen) twice

`SavePolicy` has 4 variants, one being `Frozen { resume_to: SaveMode }`, and
`SaveMode` mirrors the other three, with hand-written mappings in `freeze`,
`thaw` and `stop` and four predicates over them. `struct SavePolicy { mode:
SaveMode, frozen: bool }` removes the mapping. If V4 removes `Never`, `mode`
becomes `Persisting | Stopped`, and `persists_this_boot` is always true and
goes away.

### P3. Retry and backoff spelled per site

Within this scope:

- autosave backoff: `Backoff` over the `SESSION_SAVE_*` constants
- checkpoint backoff: `checkpoint_retry_delay` with a count cap
- logind reconnect: `Backoff`, with a pending-shutdown override that resets
  the count
- the final save: no retry at all, so the policy is effectively different from
  every other save
- outside scope: `AcceptBackoff` in shepr-api (O4)

The final save's zero retries is the substantive one: a transient EIO on the
last save loses everything since the last autosave (up to the 5s debounce plus
any backoff), while a checkpoint would have retried twice.

### P4. The monitor reconnects immediately after logind drops a working connection

On `Ok(())` from `watch_shutdown` (owner change or signal stream end) the loop
reconnects with no delay and resets `failures`. A logind (or bus) that accepts
and then drops connections repeatedly spins the task, with a debug line at
most. Apply the backoff on both arms, resetting only after a connection has
lived for some minimum.

### P5. Blocking waits on the runtime thread

`retire_session_writer` calls `pending.wait()` and `persister.retire()`
(a thread join) synchronously. It is reached from async `run` (normally with
nothing in flight) and from `HeadlessServer::drop`, which runs inside
`rt.block_on`'s future when the loop errors or unwinds, possibly with a save in
flight. The persister's own `Drop` doc warns about exactly this. Today it only
stalls one worker of a multi-thread runtime while the process exits, but it is
the same rule as `wait_off_the_runtime`, implemented once async and once
blocking. Make retirement async (`spawn_blocking`) on the `run` path, and
leave `Drop` as the blocking backstop.

### P6. Ambient clock

There is none in production in this scope beyond the two marked sites (the
signal instant and the shutdown flush deadline). Good. Tests read `Instant::now`
freely (T4).

### P7. Growth

Nothing unbounded here. `pending_checkpointed_pane_exits` is bounded by panes
and `shutdown_flushes` by clients. No secrets or personal data reach these
logs.

---

## 9. Code that is no longer load-bearing

### N1. `SessionOpenPolicy::Never` and everything hanging off it

Covered in V4: `SavePolicy::Never`, `SaveMode::Never`, `persists_this_boot`'s
false case, `session_persists()` in the lifecycle,
`SessionPersister::lease_only` and `SaveRefusal::LeaseOnly` in mux. The
evidence is that `bootstrap.rs` is the only production `App::open` and passes
`Persist`.

### N2. `finish_final_session_save`'s return value and its `autosave.clear()`

Neither caller reads the `bool`. The clear on success duplicates
`retire_session_writer`'s unconditional `autosave.clear()` a few lines later.
On failure, `record_failure` arms a deadline that nothing will service.

### N3. `HostShutdownFreeze` as a returned value

`cancel_host_shutdown()` and `restart_host_shutdown_warning()` return
`Option<HostShutdownFreeze>`, but both call sites use only `.is_some()`, so
the struct is never read through them. `lifecycle.rs`'s
`HostShutdownFreeze` doc ("held ... until shutdown completes") describes a
token, not what it is (a generation record).

### N4. Unreachable phase guards

`RunServerError::Shutdown`, `ShutdownStep::CompleteShutdown` and the second
freeze `require_phase` (E4).

### N5. `Autosave::is_due` and `SessionSaver::is_due`

`Autosave::is_due` is used only by `next_save` and its tests.
`SessionSaver::is_due` is `deadline().is_some_and(now >= d)`, which, per G2,
is never true for a due checkpoint with no retry. That works only because
`service_session_saves` also starts on `save_reaped`.

### N6. No pane-history leftovers here

There are no leftovers in this scope from the removed settings or the
pane-history feature. Searching the scope files for history, scrollback,
`confirm_close`, `pane_borders`, `status_indicators` and
`show_agent_labels` found none. The session.rs tests' `two_pane_app` doc
calls it "A production-policy app", which with V4 is accurate only after
`persist()`.

---

## Lateral

- The `SaveCompletion` `Notify` permit model (one permit, coalescing) is what
  makes D1 depend on timing. Any future "act on a result after reaping"
  consumer will hit the same ordering trap. Consider having the loop run one
  extra pass whenever a reap changed anything, which `service_session_saves`
  already returns implicitly through `save_reaped`.
- `handle_test_runtime_exit_and_replay` busy-loops with `std::thread::yield_now`
  and a 5s wall-clock deadline against a real writer thread. It is the base of
  many app tests, so a slow disk under the test harness makes many tests time
  out together.
- `docs/` and `reference/` say nothing about when the layout is saved
  (debounce, pane-exit checkpoint, host-shutdown checkpoint, the final save
  skipped during a host shutdown). The behaviour is substantial and
  user-visible: "my last few seconds of layout changes were not restored". It
  belongs in `reference/` at least, which is currently a single spec file with
  no persistence section.
