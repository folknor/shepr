# Structural rewrites

The structural findings of the defect hunt, consolidated into one item per
area. Each item replaces a class of patched invariants with a shape that cannot
represent the failures. An item is removed entirely once its rewrite lands.

## 2. Client shell: a request ledger and an explicit surface baseline

Formerly CSHELL-018 and CSHELL-019.

In-flight state is spread across `pending_requests`, `pane_scroll_in_flight`,
`pane_scroll_queued`, `pane_scroll_targets`, `copy_operation_in_flight`,
`copy_operation_queue`, `copy_input_queue`, `ClientWordSelection::pending_row`
and `PendingWorkspaceHighlight::request_id`. Cancellation runs a rollback owned
by each pending request kind, but ordinary completion still unwinds each
feature's own maps and queues by hand (a mismatched-boot result that skipped
its rollback was one path that forgot, since fixed).

The shell also keeps `pane_surface` and `pending_pane_surface` with revision
rules in three places (`set_pane_surface`, `install_pane_surface`,
`apply_pane_surface_patch`), and the reader in `lib.rs` keeps its own baseline.
REJ-011 (surface patches against a pending or dropped baseline fail the
connection) follows from this.

Direction:

- One ledger whose entries own both completion and rollback, so dropped
  follow-up work is impossible by construction.
- Model the surface explicitly: a server baseline that patches always apply to
  (mirroring the reader), and a separately chosen presentable pair (snapshot
  plus surface at the same revision). REJ-011 then cannot happen, and
  `Applied`/`Rejected` regain their meaning.

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
