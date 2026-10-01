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

## PRUN-020 - A cancelled provisional process-exit release can freeze the detector

`terminal/state/` (`ProvisionalProcessExit`, `source/detection.rs`). A detector
process-exit release is held for `AGENT_PROCESS_EXIT_RELEASE_GRACE` and
cancelled by new process evidence or an ownership change. A cancelled marker is
cleared only by a later detector update that names an agent. Until then every
detector observation without an agent is dropped, the deferred withdrawal is
never applied, and a later exit or confirmation is ignored. If ownership was
replaced during the window (a custom hook commit, say) and no agent process
comes back, the pane's detector state stays frozen. Bound it: once the grace
has elapsed, clear a cancelled marker and apply its deferred withdrawal without
the release. Related, smaller: during the window the dying agent's late hook
reports are admitted as live because process evidence stays available, so the
sidebar can briefly show them before confirmation clears the authority; and
`DetectionTask::provisional_release` is not cleared on a detector reset
(harmless, since the terminal ignores a confirmation for an exit it already
resolved).
