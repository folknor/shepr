# Defects: pane runtime

Filed from the defect hunt over `crates/shepr-mux/src/` `pane.rs`, `pane/`,
`terminal/` and `render_signal.rs`.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## PRUN-011 - Structural: finish hook arbitration as one per-source state machine

`terminal/state/source.rs` now holds a per-source (generation, event) to
(generation, effects) table that centralises routing, and the exposed
arbitration helpers dropped from 50 to 36. That pass deliberately kept the
existing mutation algorithms in `hooks.rs`, `sessions.rs` and `detection.rs`
intact to keep upstream herdr fixes portable; that constraint no longer exists.
Residue: move the mutations themselves into the transitions, so the table's
effects are what change state and the remaining `pub(super)` predicates and
flag juggling (`hook_authority`, `persisted_agent_session`,
`recent_agent_process_exit`, `pending_start`, `pending_replacement_report`,
`stale_sessions`, sequence re-anchoring) collapse into the machine, with the
`HookSourceState` invariants checked in one place. Leave room for a
provisional process-exit release, the next change planned here.

## PRUN-018 - A new pane runtime test cannot fail

Lateral, `pane/runtime.rs`. `parser_writer_does_not_retain_cwd_state_after_runtime_drop`
builds its runtime through `with_child_io`, which has no read effects, and
`PaneOutputWriter` has no cwd field, so the test cannot fail and does not
exercise the production read-effects bundle. Rewrite it over a runtime with
read effects, or delete it.

## PRUN-016 - A PTY actor startup failure kills and waits for the child synchronously

Lateral, pre-existing, `pane/runtime.rs` (PTY startup in the pane spawn path).
When the PTY actor fails to start, the spawn path kills the child and waits for
it inline. If that runs on a Tokio worker, a child slow to die blocks the worker;
the rest of the runtime hands unreaped children to the detached reaper instead.
Use the same handoff here.

## PRUN-017 - Process evidence after a hook clear discards a parked start

Lateral, `terminal/state/source.rs` (the per-source hook state machine). When
process evidence arrives after a hook clear, a parked start is discarded without
being installed. The behaviour predates the state machine, which keeps it and
pins it with a test. Decide whether it is intended (the clear means the parked
start is stale) or whether the start should be installed; if intended, say why at
the transition.
