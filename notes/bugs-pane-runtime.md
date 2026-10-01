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

## PRUN-003 - `PaneRuntime` says dropping it "aborts async tasks"; two kinds of task outlive it

Hunter's rating: low. The synchronized-output timer
(`PaneReadEffects::arm_sync_timeout`) now holds a `Weak` while asleep and the
`PaneRuntime` doc describes the timer and the child watcher. Residue: a timer
that upgraded its reference just before the runtime dropped still finishes its
flush afterwards, so it can request a render wake (and queue generation-stamped
events the app discards) for a pane that is gone. Aborting the timer on drop
(an `AbortHandle` in `SyncTimeoutRender`) would close it.

## PRUN-004 - Two definitions of "the pane's cwd": restore uses the one the runtime calls inferior

Hunter's rating: medium-low, certain about behaviour, intent unclear. Also
surfaced as a lateral in the mux and persistence hunt.

Claims: `ReportedCwd`: "OSC 7 carries what /proc cannot: a logical path through
symlinks, or the directory of a program the pane shell's /proc entry does not
describe (a nested shell, a root shell under sudo)." `PaneRuntime::cwd`: the
OSC 7 report wins while the shell's /proc cwd is unchanged.

Everything live follows that arbitration (`cwd()`, `follow_cwd()`,
`TerminalState::cwd` fed by `TerminalCwdReported`). The save does not:
`persist::snapshot::capture_workspace` takes `remembered_cwd()` and replaces it
with `PaneCwdProbe::read()`, the raw `/proc` readlink of the shell.

- A pane in `~/proj` through a symlink, in a nested shell, or in a `sudo -s`
  shell is restored into the physical `/proc` path of the outer shell, not the
  directory the user saw and that splits would have inherited.
- `remembered_cwd()` prefers `persistence_cwd` (the last `/proc` value a save
  saw) over the OSC 7 report, whatever their age, so after one save a newer
  OSC 7 report never reaches the save fallback again. The mux hunter adds: when
  a later probe fails (for example, the shell has just exited), the save writes
  the older probed cwd even though the shell reported a newer one.

Owner's decision: restore where splits would go. The owner works through
symlinked directories and wants the logical path back, so the save must use
the same arbitration as `cwd()` (`ReportedCwd::resolve(reported, proc_cwd)`),
falling back to `/proc` only when no report applies, and a newer OSC 7 report
must win over an older probed value in the save fallback.

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

## PRUN-013 - `PaneOutputWriter::try_begin` doc omits the poisoned core

Lateral, low. `try_begin` (`pane/runtime.rs`) returns `None` for a poisoned core
as well as a busy one; its doc names only "a snapshot or another mutation holds
the core". Only test support uses it today.
