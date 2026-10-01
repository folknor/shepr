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
