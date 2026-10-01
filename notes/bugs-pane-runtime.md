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

## PRUN-009 - Structural: one shared struct instead of about ten Arcs

`PaneRuntime`, `PaneReadEffects` and `PaneOutputWriter` each hold separate
`Arc`s to the same items (`terminal`, `reported_cwd`, `child_liveness`,
`full_lifecycle_authority_active`, `persistence_cwd`, `detect_reset_notify` and
so on; the content revisions now live in the terminal core). One
`Arc<PaneShared>` would make clone sites and ownership readable and make the
drop story in PRUN-003 explicit (a `Weak<PaneShared>` in the timer).

## PRUN-010 - Structural: `spawn_with_initial_history` still builds the terminal, PTY and reader inline

The detection loop and child watching now live in `pane/detection_task.rs`
(`DetectionTask`, one whole tick per `spawn_blocking` job) and
`pane/child_watcher.rs`. Residue: terminal setup, PTY setup and the read and
reader-exit callbacks are still built inline in `spawn_with_initial_history`,
and `DetectionHandles` groups only the detector's inputs (see PRUN-009).

## PRUN-011 - Structural: hook arbitration in `terminal/state` as one per-source state machine

Many flags drive the logic in `hooks.rs`, `sessions.rs`, `source.rs`,
`detection.rs` and `lifecycle.rs`: `hook_authority`, `persisted_agent_session`,
`recent_agent_process_exit`; the per-source `HookGeneration` (`Open`,
`AwaitingProcess`, `Cleared`), `pending_start`, `pending_replacement_report`,
`stale_sessions`; sequence re-anchoring. The entry points
(`set_hook_report_at`, `set_agent_session_ref_*`,
`set_detected_state_with_screen_signals_at`) each reimplement parts of the
routing. No concrete defect found, but the hunter expects the next ones here. A
single explicit `(generation, event) -> (generation, effects)` table would
replace about a dozen `pub(super)` predicates and make the invariants in the
`HookSourceState` doc checkable.
