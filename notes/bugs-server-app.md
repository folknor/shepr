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
for as long as the mount hangs. The
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
