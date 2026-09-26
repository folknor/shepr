# Agent detection and integration defects

```
1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
```

## DET-003 - `stabilize_agent_detection` is an identity function, and the detector's visible-state signals are carried but ignored

Surfaced in three scopes: detection, app core, pane/terminal state.

- `stabilize_agent_detection` (`src/terminal/state.rs`) is an identity function whose name promises stabilisation it does not do.
- `AppEvent::StateChanged.visible_working` (`events.rs`) is carried but never read. `actions.rs` passes it (plus a hard-coded `false` for visible_idle) into `TerminalState::set_detected_state_with_screen_signals_at`, where both parameters are `_visible_idle` / `_visible_working` and ignored. The detector computes them and ships them through `AppEvent::StateChanged`.
- `AgentDetection::visible_working` is documented as "diagnostic metadata" (`detect/mod.rs`), but it drives publishing (`should_publish_detection_update`, the refresh timestamp).
- Either the detector's signal was lost in stripping, or the field and parameters should be removed.

## DET-012 - Hook report ordering is decided by wall-clock seqs from separate processes

- Kimi and Mastracode take their seq first thing in the shell (`date +%s%N`), which shrinks but doesn't close the window: separate hook processes still race, and a wall clock stepping backwards still drops reports until it catches up.
- The real fix is server-side, in how `HookStateReported` / `AgentSessionReported` accept seqs (reached from `src/app/api/panes.rs`): tolerate small inversions or use a monotonic per-source clock.
- Seq units differ by integration: Kilo, opencode, pi and omp seed from `Date.now()*1000` (microseconds); the shell/python hooks use nanoseconds. Harmless only while seqs are compared per source.
- Kilo still sends `session_start_source: "startup"` always; its events carry no start source (commented in the asset).

## DET-016 - opencode/Kilo permission-dialog control labels are unconfirmed

- The `permission_required` rules in the opencode and Kilo manifests also require one of "allow once", "allow always", "reject" or "enter confirm". Those labels were written from memory of opencode's TUI. Confirm them against a live dialog with `shepr agent read <pane> --source detection --format text`.

## DET-021 - Two hooks still depend on stderr suppression or crash loudly

- The Mastracode hook runs its python heredoc under `set -eu` without `2>/dev/null || true`: non-object payloads are handled, but any other python exception exits non-zero with a traceback.
- The Cursor hook only hides an AttributeError on non-object payloads because stderr is suppressed; add an `isinstance` guard at its next version bump.
