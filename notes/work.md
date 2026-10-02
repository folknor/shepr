# Structural rewrites

The structural findings of the defect hunt, consolidated into one item per
area. Each item replaces a class of patched invariants with a shape that cannot
represent the failures. An item is removed entirely once its rewrite lands.

## 3. Server app: the pane-exit checkpoint as a typed state machine

Formerly SAPP-012.

The checkpoint in `session.rs` spreads one concept across loosely coupled
fields. The separate pending flag is gone (preservation is now the snapshot's
presence), but the requested generation, saved generation, failures,
readiness, `session_revision`, `critical_save_retry_deadline` and the
host-shutdown trio remain separate. Every finding in this area was an
invariant between two of them that some path forgot (a stale removal plan
clearing the bookkeeping was one, since fixed).

Direction: a typed enum (no checkpoint / requested gen N / saved gen N with
snapshot / abandoned) that makes that class unrepresentable.
