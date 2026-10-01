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
The limitation is now documented at `App::prepare_pane_exit` (`app/events.rs`)
and in shepr-mux `terminal/state/detection.rs`. The detector's release
(`AppEvent::StateChanged` with `process_exited`) carries no `ChildExitReason`;
the reason arrives later in `PaneDied` from the child watcher. The fix
coordinates the detector process-exit path (shepr-mux `pane/process_probe.rs`)
with the child watcher (`pane/runtime.rs`) so an `Interrupted` pane exit keeps
the resume identity even when the detector release runs first, while an agent
exiting under a live pane shell still clears it.

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

## SAPP-012 - Structural: the pane-exit checkpoint as a typed state machine

The checkpoint in `session.rs` spreads one concept across loosely coupled
fields. The separate pending flag is gone (preservation is now the snapshot's
presence), but the requested generation, saved generation, failures, readiness,
`session_revision`, `critical_save_retry_deadline` and the host-shutdown trio
remain separate. Every finding in this area is an invariant between two of them
that some path forgot (a stale removal plan clearing the bookkeeping was one,
since fixed). A typed enum (no checkpoint / requested gen N / saved gen N with
snapshot / abandoned) would make that class unrepresentable; the hunter
recommends the rewrite.

## SAPP-013 - A production-compiled test harness

`crate::agent_report_test_support` is a public, production-compiled test harness
that builds its workspace with `Workspace::test_from_pane`, a production-visible
`test_`-named constructor in shepr-mux. The module doc owns the choice; noted
because AGENTS.md prefers seams over test surfaces in production crates.
