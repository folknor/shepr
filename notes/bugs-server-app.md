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

## SAPP-004 - A detector release applied while the pane shell lives still drops the resume identity

A checkpointed pane exit (`Interrupted` or `ReaderIoFailed`) now keeps the
agent's resume identity: once the pane child has exited, the server drops
queued detector updates for that pane, the exit reason travels with the pane
exit, the preservation rule lives in shepr-mux `terminal/state/detection.rs`,
and an agent exiting under a live pane shell still clears it. Residue: a
detector release applied while the pane shell is still alive clears the
identity, even when the shell then dies from the same cause. A signal sent to
the whole session or cgroup reaches the agent first (an interactive shell
ignores SIGTERM until the escalation to SIGKILL), and an OOM kill can take the
agent before the shell; either way the checkpoint saves no resume identity. The
rarer case without a pidfd, where death is known only once the wait completes,
is the same class. A fix would make the detector's process-exit release
provisional (held until the shell survives a short grace period or a later
tick confirms it).

## SAPP-007 - The test event drain skips the headless server's forwarding

The test-only pane removal, navigation, swap and resize helpers are gone, and
tests drive the production event handler, the typed endpoint dispatcher and the
scheduled resume pass. Residue: the test queue drain in `app/runtime.rs` still
calls App event handling without the headless server's checkpoint hold,
shutdown-signal drop and clipboard forwarding (documented there), and a test of
the per-tick drain limit belongs in `server/headless/tests/`.

## SAPP-012 - Structural: the pane-exit checkpoint as a typed state machine

Outside the resolution loop: the owner resolves this directly. Do not assign it
or related bugs to fixers.

The checkpoint in `session.rs` spreads one concept across loosely coupled
fields. The separate pending flag is gone (preservation is now the snapshot's
presence), but the requested generation, saved generation, failures, readiness,
`session_revision`, `critical_save_retry_deadline` and the host-shutdown trio
remain separate. Every finding in this area is an invariant between two of them
that some path forgot (a stale removal plan clearing the bookkeeping was one,
since fixed). A typed enum (no checkpoint / requested gen N / saved gen N with
snapshot / abandoned) would make that class unrepresentable; the hunter
recommends the rewrite.
