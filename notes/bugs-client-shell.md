# Defects: client shell

Filed from the defect hunt over `crates/shepr-client/src/shell.rs` and
`crates/shepr-client/src/shell/`.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## CSHELL-018 - Structural: a request ledger instead of per-feature in-flight bookkeeping

Outside the resolution loop: the owner resolves this directly. Do not assign it
or related bugs to fixers.

In-flight state is spread across `pending_requests`, `pane_scroll_in_flight`,
`pane_scroll_queued`, `pane_scroll_targets`, `copy_operation_in_flight`,
`copy_operation_queue`, `copy_input_queue`, `ClientWordSelection::pending_row`
and `PendingWorkspaceHighlight::request_id`. Cancellation now runs a rollback
owned by each pending request kind, but ordinary completion still unwinds each
feature's own maps and queues by hand (a mismatched-boot result that skipped
its rollback was one path that forgot, since fixed). One ledger whose entries
own both completion and rollback would make dropped follow-up work impossible
by construction.

## CSHELL-019 - Structural: surface baseline versus presentable surface

Outside the resolution loop: the owner resolves this directly. Do not assign it
or related bugs to fixers.

The shell keeps `pane_surface` and `pending_pane_surface` with revision rules in
three places (`set_pane_surface`, `install_pane_surface`,
`apply_pane_surface_patch`), and the reader in `lib.rs` keeps its own baseline.
Model them explicitly: a server baseline that patches always apply to (mirroring
the reader), and a separately chosen presentable pair (snapshot plus surface at
the same revision). The patch-rejection finding filed in
`notes/bugs-rejected-candidates.md` then cannot happen, and `Applied`/`Rejected`
regain their meaning.
