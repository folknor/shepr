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

## SAPP-002 - A hung resume directory check leaves that pane's resume pending forever

The resume pass now skips candidates whose directory check has not landed and
resumes the checked ones in layout order, so one hung mount no longer blocks
every other agent. Residue: `worker::resume_cwd_check` (`std::fs::metadata` on a
worker) still has no deadline, so a pane whose saved cwd sits on a hung mount
keeps its resume pending for as long as the mount hangs. A deadline after which
the directory counts as unavailable (abandoning the resume with
`RestoreFailure::DirectoryUnavailable`) would end it.

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
`session_revision` and clears the checkpoint snapshot). Handlers pick one or
both arbitrarily (`handle_workspace_rename` only schedules,
`handle_pane_rename` only marks, `handle_pane_swap` does both). Automatic
workspace replacement no longer goes through the scheduling side channel, but
the two entry points remain; one mutation entry point would remove the class.

## SAPP-012 - Structural: the pane-exit checkpoint as a typed state machine

The checkpoint in `session.rs` spreads one concept across loosely coupled
fields. The separate pending flag is gone (preservation is now the snapshot's
presence), but the requested generation, saved generation, failures, readiness,
`session_revision`, `critical_save_retry_deadline` and the host-shutdown trio
remain separate. Every finding in this area is an invariant between two of them
that some path forgot. A typed enum (no checkpoint / requested gen N / saved
gen N with snapshot / abandoned) would make SAPP-010 unrepresentable; the
hunter recommends the rewrite.

## SAPP-013 - A production-compiled test harness

`crate::agent_report_test_support` is a public, production-compiled test harness
that builds its workspace with `Workspace::test_from_pane`, a production-visible
`test_`-named constructor in shepr-mux. The module doc owns the choice; noted
because AGENTS.md prefers seams over test surfaces in production crates.

## SAPP-014 - A gremlin arrow in a test message

`app/mod.rs`: the test message "Working→Idle ..." contains a U+2192 arrow.
