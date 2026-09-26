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

- `stabilize_agent_detection` (`src/terminal/state.rs:2217`) is an identity function whose name promises stabilisation it does not do.
- `AppEvent::StateChanged.visible_working` (`events.rs:34`) is carried but never read. `actions.rs:1171-1179` passes it (plus a hard-coded `false` for visible_idle) into `TerminalState::set_detected_state_with_screen_signals_at`, where both parameters are `_visible_idle` / `_visible_working` and ignored (`terminal/state.rs:309-317`). The detector computes them and ships them through `AppEvent::StateChanged`.
- `AgentDetection::visible_working` is documented as "diagnostic metadata" (`detect/mod.rs:34-37`), but it drives publishing (`should_publish_detection_update`, the refresh timestamp).
- Either the detector's signal was lost in stripping, or the field and parameters should be removed.

## DET-004 - Relative argv paths are resolved against the server's cwd, with blocking IO in the detection task

- `resolved_agent_name_from_path_token` (`detect/mod.rs:665-674`) calls `std::fs::canonicalize` on argv tokens such as `./agent` or `bin/x` relative to the shepr server's cwd, not the target process's `/proc/<pid>/cwd`. That can misidentify or miss agents.
- These blocking `canonicalize` calls, plus all the `/proc` reads (`probe_foreground_process`, `process_agent_hint` reading `environ`), run synchronously inside the per-pane tokio detection task. They block runtime workers, multiplied by the number of panes.

## DET-006 - Manifest tests change process-global state

- They mutate `XDG_CONFIG_HOME` and the global `MANIFEST_CACHE` (`manifest/tests.rs`). They are only safe under one-process-per-test (nextest); any other test in the same process that runs Codex detection concurrently would see synthetic manifests.

## DET-008 - Letta sits outside `IntegrationTarget`

- Letta install/uninstall now go through the protected config writer. What remains is structural: Letta still has its own experimental path (types, registry, CLI) instead of being an `IntegrationTarget` variant. Folding it in touches `src/api/schema.rs`. The comments in `src/cli/integration.rs` and the `EXPERIMENTAL_INTEGRATION_TARGET_LABELS` doc in `src/integration/mod.rs` describe it as leftover structure awaiting the fold.

## DET-009 - `integration status` can report "current" for an install that does nothing

- Status looks only at the hook file's version marker (`registry.rs:225-243`); only Grok and opencode also check registration.
- Most installs (Claude, Codex, Qwen, Cursor and others) write the hook script before parsing or editing the agent's config (e.g. `install_claude`, `targets.rs`). A malformed `settings.json` leaves an orphan hook that status reports as current. Kimi and Letta now build the config first.
- A user deleting the settings entry also leaves status at "current".

## DET-012 - Hook ordering depends on interpreter startup time

- Shell hooks (Kimi, Mastracode, all the Python ones) take `seq = time.time_ns()` after the interpreter has started, in a separate process per event.
- Near-simultaneous events (for example PreToolUse followed by PermissionRequest) can arrive with inverted seqs, and the server drops the "older" one.
- A wall-clock step backwards drops reports until the clock catches up.
- Kimi, Kilo and Mastracode always send `session_start_source: "startup"`, even on resume.

## DET-013 - Minor integration issues

- Hook scripts are rewritten in place with `fs::write` (truncate then write) while agents may be running them through `bash`, which reads scripts incrementally.
- `uninstall_target` logs only the "ok" outcome (errors return before logging).
- The CLI usage line omits `antigravity-cli` (also in CMD-018).
- The `version.rs` gate uses `expect`.
- Windows leftovers in integration assets and helpers despite the Linux-only rule: `win32` pipe paths in the opencode JS/TS assets, `os.name == "nt"` in the Hermes plugin, `powershell`/`~\\` handling in `config_edit.rs`/`env.rs`. (The no-op platform stubs are in PLAT-006; the Rust-side Windows key path is PLAT-003.)

## DET-015 - Gate-level `region` is undocumented

- Manifest gates now take an optional `region` (for example a `not` gate reading `bottom_non_empty_lines(12)` inside an `osc_title` rule). Any manifest-format doc (`docs/`, `reference/`, or the brokkr man pages) should describe it.

## DET-016 - opencode/Kilo permission-dialog control labels are unconfirmed

- The `permission_required` rules in the opencode and Kilo manifests now also require one of "allow once", "allow always", "reject" or "enter confirm". Those labels were written from memory of opencode's TUI. Confirm them against a live dialog with `shepr agent read <pane> --source detection --format text`.
