# Defects: server app

Filed from the defect hunt over `crates/shepr-server/src/app/`, `lib.rs`,
`limits.rs` and `logging.rs`.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## SAPP-001 - The server loop spins hot while a resume cwd check is outstanding (forever on a hung mount)

Claims broken: `start_pending_agent_resume` (`agent_resume.rs`) says the
worker-side cwd check exists "so a persistently hung mount lookup leaves the loop
free while the resume waits". `ResumeSchedule::wakeup` says it returns `None`
"while nothing holds an eligible candidate back".

`pending_agent_resume_wakeup()` returns `pending.not_before.max(theme_wait)`
whenever candidates are eligible, and that instant stays in the past once the
theme wait or a launch/backoff barrier has elapsed.
`next_headless_loop_deadline_with_git_refresh` (`runtime.rs`) feeds it into the
loop deadline unfiltered, unlike the render deadline and
`default_workspace_retry_at`, which are filtered to `> now` in the same
function. Meanwhile `start_pending_agent_resumes` returns `false` without doing
anything while any candidate lacks a directory check, and nothing in the
schedule moves:

1. theme wait expired (or a barrier passed), candidate eligible, check
   dispatched to a worker;
2. loop computes deadline = past instant, `sleep_until_or_pending` fires at
   once, `handle_scheduled_tasks_headless` runs, `start_pending_agent_resumes`
   returns false, `schedule_resume_cwd_checks` dedups the in-flight check;
3. back to 2.

The spin lasts as long as the `metadata` call on the worker: milliseconds
normally, unbounded on a hung NFS/sshfs mount (the case the worker split was made
for). The same holds after every `Retryable` outcome (its check is consumed by
`take_directory_check`, so the next pass re-dispatches and spins until the new
check lands).

Direction: the schedule should know about outstanding checks: return no wakeup
while any candidate awaits a check (the completion already wakes the loop
through `worker_rx`), or filter the resume wakeup to `> now`. Structurally, make
"awaiting cwd check" a state the schedule owns rather than something `App`
checks after `is_due` said yes.

## SAPP-002 - One hung resume directory blocks every other agent resume, indefinitely

Claims broken: the same comment ("leaves the loop free while the resume waits")
implies the wait is per resume; AGENTS.md promises agent resume on restore.

`start_pending_agent_resumes` refuses the whole pass if any candidate lacks a
check:

```rust
if pending.iter().any(|candidate| {
    !self.resume_schedule.has_directory_check(&candidate.terminal_id, &candidate.cwd)
}) {
    return false;
}
```

`worker::resume_cwd_check` has no timeout. One pane whose saved cwd sits on a
hung mount keeps every other restored agent, in every workspace, from resuming
for as long as the mount hangs (and per SAPP-001 spins the loop meanwhile). The
comment justifies the all-or-nothing gate as keeping candidate order and
spacing.

Direction: attempt candidates that have a check in order and skip (not block on)
ones still waiting, or give the worker check a deadline after which the
directory counts as unavailable and the resume is abandoned with
`RestoreFailure::DirectoryUnavailable`.

## SAPP-003 - Automatic workspace replacement destroys the pane-exit checkpoint on the production shutdown path

Claim broken: `create_default_workspace` (`mod.rs`): "Automatic replacement is
part of pane removal, not a new user mutation", and it re-arms
`pane_exit_checkpoint_pending` so the checkpoint survives. The test
`pane_exit_checkpoint_survives_automatic_workspace_creation_on_shutdown` asserts
the saved session keeps both panes.

`create_default_workspace` calls `create_workspace`, which calls
`schedule_session_save()` -> `SessionSaver::schedule`, which sets
`pane_exit_checkpoint_pending = false` and `pane_exit_checkpoint_snapshot =
None`. `create_default_workspace` then sets `pending = true` again but cannot
restore the snapshot. On shutdown the production path is
`save_session_before_teardown_async`: `preserve_checkpoint` is true (pending,
not dirty); the snapshot is `None`, so `checkpoint_job` is `Some(None)`; it logs
"could not pair fresh pane history with the saved pane-exit layout" (wrong
reason) and captures the live session, the freshly created one-pane default
workspace, overwriting the checkpoint.

The test passes only because it calls the `#[cfg(test)]`
`save_session_before_teardown`, which when preserving skips the save and leaves
the file on disk. Production never runs that function.

Scenario: a client is attached, every shell dies from a signal (session
teardown, SIGHUP), all workspaces empty, `create_automatic_workspace` makes a new
one, then the server gets SIGTERM within the debounce window: the saved layout is
replaced by a single default workspace.

Related, lower severity: `capture_save_job_from_pane_exit_checkpoint` returns
`None` whenever `capture_pending_cwds_for_snapshot` or
`capture_pending_history_for_snapshot` cannot map a snapshot pane to a terminal
ID, and the caller saves the live session with the same misleading warning. The
`terminal_ids` map is built by `capture_pane_exit_checkpoint_snapshot` keyed by
`(workspace_index, pane_id.raw())` and rejected wholesale if counts differ, so
in practice a stored snapshot always maps; the fallback is reachable only
through the missing-snapshot case above, where the message is wrong. The two
cases should be distinct in the log, and it is worth asking whether falling back
to the live layout is ever right when `preserve_checkpoint` means the live layout
is the damaged one.

Direction: (a) the checkpoint state should not be resettable by a side channel:
`schedule()` should not drop the snapshot when the caller is about to declare
the change non-durable, or `create_default_workspace` should not go through
`schedule_session_save`. (b) Delete the test-only `save_session_before_teardown`
and `save_session_now`, or make them thin wrappers over the async path. See
SAPP-007 and SAPP-012.

## SAPP-004 - A pane-exit checkpoint drops the dying pane's agent session before it captures

Claim broken: the pane-exit checkpoint exists to "keep the pre-exit layout live
until its checkpoint is durable" (`internal_events.rs`) so a signal-killed pane
comes back on restore; AGENTS.md promises agent resume on restore.

`App::prepare_pane_exit` (`events.rs`) calls `publish_pane_process_exit` before
`request_pane_exit_checkpoint`. `publish_pane_process_exit_if_agent` runs
`set_detected_state_with_screen_signals_at(agent, Idle, false, true, ..)`,
which for a matching agent sets `persisted_agent_session = None` and clears the
hook authority (shepr-mux `terminal/state/detection.rs`). The checkpoint then
captures that terminal with no agent session, so the restored pane gets a plain
shell and no resume, for exactly the panes the checkpoint protects. It also
makes the session dirty, forcing a fresh checkpoint rather than reusing a
settled one.

Caveat: the detector's own process scan can observe the agent's death first and
clear the session the same way, so the loss is racy even with the order fixed.
The underlying problem: "agent process exited" is treated as "user quit the
agent" regardless of `ChildExitReason`; an `Interrupted` exit should keep the
resume identity.

Direction: capture the checkpoint before publishing the exit, and/or carry the
exit reason into the release so a signal death keeps `persisted_agent_session`.

## SAPP-005 - The resume theme wait never applies after restoring a session that saved a theme

Claims broken: `PENDING_AGENT_RESUME_THEME_WAIT` (`limits.rs`): "Wait briefly
for restored agent theme reports"; `resume_schedule` module doc: the theme wait
is "how long a host theme is waited for".

`App::with_paths` seeds `host_terminal_theme` from the snapshot
(`restored_host_theme`). `host_theme_available()` is
`!host_terminal_theme.is_empty()`, so after any restore of a session saved while
a client was attached, the theme is "available" before any client has reported
one, and `ResumeSchedule::wakeup` skips the wait. Resumes are the only consumer
of the wait and only happen after a restore, so the wait is effectively dead
whenever it could matter. Resumed agents answer their startup OSC 10/11 queries
with the theme of whichever client was foreground when the session was last
saved, possibly another machine's terminal with the opposite light/dark scheme,
and cache it.

Direction: track "a live client reported a theme this boot" separately from "a
theme exists", and gate the wait on the former. If the saved theme is meant to be
the fallback, say so in `PENDING_AGENT_RESUME_THEME_WAIT` (whose "before
assigning a fallback" describes nothing the code does).

## SAPP-006 - An appearance-only report from the foreground client wipes the saved host theme

Lateral, in `HeadlessServer::sync_host_theme_from_foreground`
(`server/headless.rs`), which documents: "A client that has reported nothing yet
leaves the current theme (a live client's, or the one saved with the session) in
place." It returns early only when the theme is empty and the appearance is
`None`. A client that has reported its Mode 2031 appearance but not yet its OSC
10/11 colours passes the guard, and `set_host_terminal_theme(empty)` replaces the
current theme with an empty one: every pane runtime is re-themed to defaults and
`schedule_session_save` persists the empty theme. Fix: apply the theme only when
the client's theme is non-empty, independently of the appearance.

## SAPP-007 - Test-only reimplementations of production paths

Several `#[cfg(test)]` functions duplicate production logic with different
semantics, and tests assert on the duplicate:

- `App::save_session_before_teardown` and `save_session_now` (`session.rs`) vs
  production `save_session_before_teardown_async`. Different behaviour when a
  checkpoint is preserved; SAPP-003 hides behind this.
- `AppState::handle_pane_died` / `remove_pane` (actions) vs the App's
  `handle_internal_event_inner` removal.
- `AppState::navigate_pane`, `swap_pane`, `resize_pane` (`actions/pane.rs`) vs
  the endpoint handlers. `swap_pane` does not move focus; production
  `handle_pane_swap` focuses the source pane.
- `App::drain_internal_events*` (`runtime.rs`) vs
  `drain_internal_events_with_forwarding*`, which also applies the checkpoint
  hold, the signal-quit drop and the clipboard forwarding.
- `App::start_pending_agent_resume_for_terminal` bypasses the schedule and the
  directory-check gate.

The hunter recommends deleting these and driving tests through the production
entry points (the headless loop already has a test harness).

## SAPP-008 - A corrupt or unreadable pane-history file is discarded with no notice and no backup

`App::with_paths` loads history with `load_history(..)`; a read or parse failure
is only a `warn!` in shepr-mux, `restore_notice` says nothing, and
`protect_unloaded` does not cover the history file, so the first save overwrites
it. The restore notice is scoped to the session file, so the hunter calls this a
gap rather than a broken promise, worth deciding explicitly since pane history
is the bulk of what a user would want back.

## SAPP-009 - Limit docs describe behaviour the code does not have

`limits.rs`: `GIT_REMOTE_STATUS_REFRESH_INTERVAL` says "while it is visible";
the server refreshes whenever a client is attached, whatever any sidebar shows
(AGENTS.md). `PENDING_AGENT_RESUME_THEME_WAIT` says "before assigning a
fallback"; nothing assigns one (see SAPP-005).

## SAPP-010 - A `Stale` removal plan still clears `session_dirty`

`handle_internal_event_inner` calls `finish_checkpointed_pane_exit_after_event`
even when the removal plan went `Stale` and nothing was removed, clearing
`session_dirty` for a removal that did not happen.

## SAPP-011 - Session dirtiness has two entry points with different side effects

`AppState::mark_session_dirty` (flag, converted by `sync_session_save_schedule`
once per loop pass) and `App::schedule_session_save` (immediately bumps
`session_revision`, clears the checkpoint snapshot and pending flag). Handlers
pick one or both arbitrarily (`handle_workspace_rename` only schedules,
`handle_pane_rename` only marks, `handle_pane_swap` does both). SAPP-003 is a
direct consequence. One mutation entry point would remove the class.

## SAPP-012 - Structural: the pane-exit checkpoint as a typed state machine

The checkpoint in `session.rs` spreads one concept across ten loosely coupled
fields (`pane_exit_checkpoint_pending`, `_requested`, `_generation`,
`_saved_generation`, `_snapshot`, `_failures`, `_ready`, `session_revision`,
`critical_save_retry_deadline`, the host-shutdown trio). Every finding in this
area is an invariant between two of them that some path forgot. A typed enum (no
checkpoint / requested gen N / saved gen N with snapshot / abandoned) would make
SAPP-003 and SAPP-010 unrepresentable; the hunter recommends the rewrite.

## SAPP-013 - A production-compiled test harness

`crate::agent_report_test_support` is a public, production-compiled test harness
that builds its workspace with `Workspace::test_from_pane`, a production-visible
`test_`-named constructor in shepr-mux. The module doc owns the choice; noted
because AGENTS.md prefers seams over test surfaces in production crates.

## SAPP-014 - A gremlin arrow in a test message

`app/mod.rs`: the test message "Working→Idle ..." contains a U+2192 arrow.
