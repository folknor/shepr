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

The OSC tracker divergence from vte, raised in this scope, is filed as TERM-020.

## DET-002 - Agents without a manifest are reported as idle

- Omp and Mastracode have no screen manifest, so `fallback_state` in `src/detect/manifest.rs` gives them `AgentState::Idle`. Without the hook installed, the sidebar shows them idle whatever they are doing; `Unknown` is the honest value.
- Changing detection alone breaks two consumers, so the fix must change all three together:
  - `TerminalState::reconcile_managed_agent_at` (`src/terminal/state.rs`) only marks a managed launch ready on `Idle` (Codex excepted), so managed Omp/Mastracode launches would time out and lose their name.
  - `should_skip_idle_screen_scan` (`src/pane/agent_detection.rs`) treats only `Idle`, or Codex `Unknown`, as stable, so `Unknown` would force a screen read every tick.
- A comment at `fallback_state` records this.

## DET-003 - `stabilize_agent_detection` is an identity function, and the detector's visible-state signals are carried but ignored

Surfaced in three scopes: detection, app core, pane/terminal state.

- `stabilize_agent_detection` (`src/terminal/state.rs`) is an identity function whose name promises stabilisation it does not do.
- `AppEvent::StateChanged.visible_working` (`events.rs`) is carried but never read. `actions.rs` passes it (plus a hard-coded `false` for visible_idle) into `TerminalState::set_detected_state_with_screen_signals_at`, where both parameters are `_visible_idle` / `_visible_working` and ignored. The detector computes them and ships them through `AppEvent::StateChanged`.
- `AgentDetection::visible_working` is documented as "diagnostic metadata" (`detect/mod.rs`), but it drives publishing (`should_publish_detection_update`, the refresh timestamp).
- Either the detector's signal was lost in stripping, or the field and parameters should be removed.

## DET-008 - Letta sits outside `IntegrationTarget`

- Letta install/uninstall go through the protected config writer. What remains is structural: Letta still has its own experimental path (types, registry, CLI) instead of being an `IntegrationTarget` variant. Folding it in touches `src/api/schema.rs`. The comments in `src/cli/integration.rs` and the `EXPERIMENTAL_INTEGRATION_TARGET_LABELS` doc in `src/integration/mod.rs` describe it as leftover structure awaiting the fold.

## DET-012 - Hook report ordering is decided by wall-clock seqs from separate processes

- Kimi and Mastracode now take their seq first thing in the shell (`date +%s%N`), which shrinks but doesn't close the window: separate hook processes still race, and a wall clock stepping backwards still drops reports until it catches up.
- The real fix is server-side, in how `HookStateReported` / `AgentSessionReported` accept seqs (reached from `src/app/api/panes.rs`): tolerate small inversions or use a monotonic per-source clock.
- Seq units differ by integration: Kilo, opencode, pi and omp seed from `Date.now()*1000` (microseconds); the shell/python hooks use nanoseconds. Harmless only while seqs are compared per source.
- Kilo still sends `session_start_source: "startup"` always; its events carry no start source (commented in the asset).

## DET-016 - opencode/Kilo permission-dialog control labels are unconfirmed

- The `permission_required` rules in the opencode and Kilo manifests also require one of "allow once", "allow always", "reject" or "enter confirm". Those labels were written from memory of opencode's TUI. Confirm them against a live dialog with `shepr agent read <pane> --source detection --format text`.

## DET-017 - `expand_tilde_path` mis-expands `~user`

- `expand_tilde_path` in `src/integration/env.rs` turns `~bob/x` into `$HOME/bob/x`. It should only expand a bare `~` or `~/`.

## DET-018 - The Kimi hook assumes a JSON object payload

- The Kimi hook script calls `payload.get` without checking the payload is a dict, so a non-object JSON body raises (and the report is silently lost).
