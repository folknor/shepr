# Defect hunt: server app

Scope: `crates/shepr-server/src/app/` (state, actions, API and endpoint
handlers, agent resume and its schedule, session saving and checkpoints, Git
refresh, host theme, titles, events, `App::with_paths` restore), plus
`crates/shepr-server/src/lib.rs`, `limits.rs`, `logging.rs`. Callers in
`server/headless*` were read where a value or call crossed the boundary.

Findings are ordered by how much they hurt. Each names the claim it breaks.

---

## 1. The server loop spins hot while a resume cwd check is outstanding (forever on a hung mount)

**Claim broken.** `start_pending_agent_resume` (agent_resume.rs) says the
worker-side cwd check exists "so a persistently hung mount lookup leaves the
loop free while the resume waits". `ResumeSchedule::wakeup` says it returns
`None` "while nothing holds an eligible candidate back".

**What happens.** `pending_agent_resume_wakeup()` returns
`pending.not_before.max(theme_wait)` whenever candidates are eligible, and
that instant stays in the past once the theme wait or a launch/backoff barrier
has elapsed. `next_headless_loop_deadline_with_git_refresh` (runtime.rs)
feeds it into the loop deadline unfiltered - unlike the render deadline and
`default_workspace_retry_at`, which are both filtered to `> now` in the same
function. Meanwhile `start_pending_agent_resumes` returns `false` without
doing anything while any candidate lacks a directory check, and nothing in
the schedule moves. So:

1. theme wait expired (or a barrier passed), candidate eligible, check
   dispatched to a worker;
2. loop computes deadline = past instant, `sleep_until_or_pending` fires at
   once, `handle_scheduled_tasks_headless` runs, `start_pending_agent_resumes`
   returns false, `schedule_resume_cwd_checks` dedups the in-flight check;
3. back to 2.

The spin lasts as long as the `metadata` call on the worker. Normally that is
milliseconds; on a hung NFS/sshfs mount - the exact case the worker split was
made for - it is unbounded, and the server burns a core until the mount
recovers or the server stops. The same holds after every `Retryable` outcome
(its check is consumed by `take_directory_check`, so the next pass re-dispatches
and spins until the new check lands).

**Direction.** The schedule should know about outstanding checks: either
return no wakeup while any candidate awaits a check (the check completion
already wakes the loop through `worker_rx` and calls
`start_pending_agent_resumes`), or filter the resume wakeup to `> now` like
the other deadlines. The structural fix is to make "awaiting cwd check" a
state the schedule owns rather than something `App` checks after `is_due`
already said yes.

## 2. One hung resume directory blocks every other agent resume, indefinitely

**Claim broken.** Same comment as above ("leaves the loop free while the
resume waits") implies the wait is per resume; AGENTS.md promises "agent
resume on restore".

**What happens.** `start_pending_agent_resumes` refuses the whole pass if
*any* candidate lacks a check:

```rust
if pending.iter().any(|candidate| {
    !self.resume_schedule.has_directory_check(&candidate.terminal_id, &candidate.cwd)
}) {
    return false;
}
```

`worker::resume_cwd_check` has no timeout. One pane whose saved cwd sits on a
hung mount therefore keeps every other restored agent, in every workspace,
from resuming for as long as the mount hangs (and, per finding 1, spins the
loop meanwhile). The comment justifies the all-or-nothing gate as keeping
candidate order and spacing; ordering is not worth starving N-1 agents.

**Direction.** Attempt candidates that have a check in order and skip (not
block on) ones still waiting, or give the worker check a deadline after which
the directory counts as unavailable and the resume is abandoned with
`RestoreFailure::DirectoryUnavailable`.

## 3. Automatic workspace replacement destroys the pane-exit checkpoint on the production shutdown path

**Claim broken.** `create_default_workspace` (mod.rs): "Automatic replacement
is part of pane removal, not a new user mutation", and it deliberately
re-arms `pane_exit_checkpoint_pending` so the checkpoint survives. The test
`pane_exit_checkpoint_survives_automatic_workspace_creation_on_shutdown`
asserts the saved session keeps both panes.

**What happens.** `create_default_workspace` calls `create_workspace`, which
calls `schedule_session_save()` -> `SessionSaver::schedule`, which sets
`pane_exit_checkpoint_pending = false` *and*
`pane_exit_checkpoint_snapshot = None`. `create_default_workspace` then sets
`pending = true` again but cannot restore the snapshot. On shutdown the
production path is `save_session_before_teardown_async`:

- `preserve_checkpoint` is true (pending, not dirty);
- the snapshot is `None`, so `checkpoint_job` is `Some(None)`;
- it logs "could not pair fresh pane history with the saved pane-exit
  layout" (wrong reason) and captures the *live* session - the freshly created
  one-pane default workspace - overwriting the checkpoint.

The test passes only because it calls the `#[cfg(test)]`
`save_session_before_teardown`, which, when preserving, skips the save
entirely and leaves the file on disk. Production never runs that function.

Scenario: a client is attached, every shell dies from a signal (session
teardown, SIGHUP), all workspaces empty, `create_automatic_workspace` makes a
new one, then the server gets SIGTERM within the debounce window: the saved
layout is replaced by a single default workspace.

**Direction.** Two problems to fix together. (a) The checkpoint state should
not be resettable by a side channel: `schedule()` should not drop the
snapshot when the caller is about to declare the change non-durable (or
`create_default_workspace` should not go through `schedule_session_save` at
all). (b) Delete the test-only `save_session_before_teardown` and
`save_session_now` variants, or make them thin wrappers over the async path,
so tests exercise the code that runs. See also "Test-only reimplementations"
below.

## 4. A pane-exit checkpoint drops the dying pane's agent session before it captures

**Claim broken.** The pane-exit checkpoint exists to "keep the pre-exit
layout live until its checkpoint is durable" (internal_events.rs) so that a
signal-killed pane comes back on restore; AGENTS.md promises "agent resume on
restore".

**What happens.** `App::prepare_pane_exit` (events.rs) calls
`publish_pane_process_exit` *before* `request_pane_exit_checkpoint`.
`publish_pane_process_exit_if_agent` runs
`set_detected_state_with_screen_signals_at(agent, Idle, false, true, ..)`,
which for a matching agent sets `persisted_agent_session = None` and clears
the hook authority (shepr-mux `terminal/state/detection.rs`). The checkpoint
then captures that terminal with no agent session, so the restored pane gets
a plain shell and no resume - for exactly the panes the checkpoint was taken
to protect. (It also makes the session dirty, which forces a fresh checkpoint
rather than reusing a settled one.)

Caveat: the detector's own process scan can observe the agent's death first
and clear the session the same way, so the loss is racy even with the order
fixed. The underlying problem is that "agent process exited" is treated as
"user quit the agent" regardless of `ChildExitReason`; an `Interrupted` exit
should keep the resume identity.

**Direction.** Capture the checkpoint before publishing the exit, and/or
carry the exit reason into the release so a signal death keeps
`persisted_agent_session`.

## 5. The resume theme wait never applies after restoring a session that saved a theme

**Claim broken.** `PENDING_AGENT_RESUME_THEME_WAIT` (limits.rs): "Wait briefly
for restored agent theme reports"; `resume_schedule` module doc: the theme
wait is "how long a host theme is waited for".

**What happens.** `App::with_paths` seeds `host_terminal_theme` from the
snapshot (`restored_host_theme`). `host_theme_available()` is
`!host_terminal_theme.is_empty()`, so after any restore of a session saved
while a client was attached, the theme is "available" before any client has
reported one, and `ResumeSchedule::wakeup` skips the wait. Resumes are the
only consumer of the wait and only happen after a restore, so the wait is
effectively dead code whenever it could matter. Resumed agents answer their
startup OSC 10/11 queries with the theme of whichever client was foreground
when the session was last saved - possibly another machine's terminal with
the opposite light/dark scheme - and cache it.

**Direction.** Track "a live client reported a theme this boot" separately
from "a theme exists", and gate the wait on the former. If the saved theme is
meant to be the fallback, say so in `PENDING_AGENT_RESUME_THEME_WAIT` (whose
"before assigning a fallback" currently describes nothing the code does).

## 6. A preserved checkpoint that cannot be re-captured silently falls back to the live layout

Lower severity, related to 3. `capture_save_job_from_pane_exit_checkpoint`
returns `None` whenever `capture_pending_cwds_for_snapshot` or
`capture_pending_history_for_snapshot` cannot map a snapshot pane to a
terminal ID, and the caller then saves the live session with the misleading
"could not pair fresh pane history" warning. The `terminal_ids` map is built
by `capture_pane_exit_checkpoint_snapshot` keyed by
`(workspace_index, pane_id.raw())` and rejected wholesale if counts differ,
so in practice a stored snapshot always maps. The fallback branch is
reachable only through finding 3 (snapshot missing), where the message is
wrong. Worth making the two cases distinct in the log, and worth asking
whether falling back to the live layout is ever the right answer when the
whole point of `preserve_checkpoint` is that the live layout is the damaged
one.

---

## Lateral findings (outside this scope)

### L1. An appearance-only report from the foreground client wipes the saved host theme

`HeadlessServer::sync_host_theme_from_foreground` (server/headless.rs)
documents: "A client that has reported nothing yet leaves the current theme
(a live client's, or the one saved with the session) in place." It returns
early only when the theme is empty *and* the appearance is `None`. A client
that has reported its Mode 2031 appearance but not yet its OSC 10/11 colours
passes the guard, and `set_host_terminal_theme(empty)` replaces the current
theme with an empty one: every pane runtime is re-themed to defaults and
`schedule_session_save` persists the empty theme. Fix: apply the theme only
when the client's theme is non-empty, independently of the appearance.

### L2. Test-only reimplementations of production paths

Several `#[cfg(test)]` functions duplicate production logic with different
semantics, and tests assert on the duplicate:

- `App::save_session_before_teardown` and `save_session_now` (session.rs) vs
  production `save_session_before_teardown_async`. Different behaviour when a
  checkpoint is preserved; finding 3 hides behind this.
- `AppState::handle_pane_died` / `remove_pane` (actions) vs the App's
  `handle_internal_event_inner` removal.
- `AppState::navigate_pane`, `swap_pane`, `resize_pane` (actions/pane.rs) vs
  the endpoint handlers. `swap_pane` does not move focus; the production
  `handle_pane_swap` focuses the source pane.
- `App::drain_internal_events*` (runtime.rs) vs
  `drain_internal_events_with_forwarding*`, which also applies the
  checkpoint hold, the signal-quit drop and the clipboard forwarding.
- `App::start_pending_agent_resume_for_terminal` bypasses the schedule and
  the directory-check gate.

Given the "do not preserve abstractions" posture, the right move is to delete
these and drive tests through the production entry points (the headless loop
already has a test harness).

### L3. `handle_workspace_checkout_root` runs blocking Git on the loop if reached

`dispatch_endpoint_command` routes `WorkspaceCheckoutRoot` to a synchronous
handler that stats the directory and runs `git rev-parse` inline. Production
never reaches it: `endpoint_requests.rs` intercepts the command and runs
`checkout_root_for_worker` on a worker. The synchronous arm exists only for
the unit tests, and any future routing change silently puts a blocking Git
call on the event loop. Make the app arm reject it the way the
`ClientShellSurfaceSet` arm does, and test the worker path.

### L4. A corrupt or unreadable pane-history file is discarded with no notice and no backup

`App::with_paths` loads history with `load_history(..).flatten()`; a read or
parse failure is only a `warn!` in shepr-mux, `restore_notice` says nothing,
and `protect_unloaded` does not cover the history file, so the first save
overwrites it. The notice added in c087ce5 is scoped to the session file, so
this is a gap rather than a broken promise; worth deciding explicitly since
pane history is the bulk of what a user would want back.

---

## Smells and doc drift

- `limits.rs`: `GIT_REMOTE_STATUS_REFRESH_INTERVAL` says "while it is
  visible"; the server refreshes whenever a client is attached, whatever any
  sidebar shows (AGENTS.md). `PENDING_AGENT_RESUME_THEME_WAIT` says "before
  assigning a fallback"; nothing assigns one (see finding 5).
- `handle_internal_event_inner` calls
  `finish_checkpointed_pane_exit_after_event` even when the removal plan went
  `Stale` and nothing was removed, clearing `session_dirty` for a removal
  that did not happen.
- `toggle_pane_zoom` commits the focus change (and marks the session dirty)
  before it can return `None`; `handle_pane_zoom` then answers "pane not
  found" with no effects, so the committed focus change is not rendered. Only
  reachable if `set_zoomed` refuses an inconsistent pane tree.
- Session-dirtiness has two entry points with different side effects:
  `AppState::mark_session_dirty` (flag, converted by
  `sync_session_save_schedule` once per loop pass) and
  `App::schedule_session_save` (immediately bumps `session_revision`, clears
  the checkpoint snapshot and pending flag). Handlers pick one or both
  arbitrarily (`handle_workspace_rename` only schedules,
  `handle_pane_rename` only marks, `handle_pane_swap` does both). Finding 3
  is a direct consequence. One mutation entry point would remove the class.
- The checkpoint state machine in `session.rs` spreads one concept across ten
  loosely coupled fields (`pane_exit_checkpoint_pending`, `_requested`,
  `_generation`, `_saved_generation`, `_snapshot`, `_failures`, `_ready`,
  `session_revision`, `critical_save_retry_deadline`, the host-shutdown
  trio). Every finding above in this area is an invariant between two of
  them that some path forgot. A typed state enum (no checkpoint / requested
  gen N / saved gen N with snapshot / abandoned) would make 3 and the Stale
  case unrepresentable; recommend the rewrite.
- `crate::agent_report_test_support` is a public, production-compiled test
  harness that builds its workspace with `Workspace::test_from_pane`, a
  production-visible `test_`-named constructor in shepr-mux. The module doc
  owns the choice; noting it because AGENTS.md prefers seams over test
  surfaces in production crates.
- `app/mod.rs` test message "Working→Idle ..." contains a U+2192 arrow.
