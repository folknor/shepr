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

## DET-001 - The production detect path does the `explain` work on every tick

- `detect_with_osc` → `evaluate_loaded_manifest` (`manifest.rs:373-443`) builds a full `EvaluatedRule` for every rule. That includes `rule_evidence` (cloning the `contains`/`regex`/`line_regex` vectors) and `bounded_preview`, which runs `chars().count()` over the whole region text. `into_detection()` then throws all of it away.
- `load_manifest` (`manifest.rs:479-490`) returns `LoadedManifest` by value. Only `compiled_rules` is an `Arc`; the whole `AgentManifest` tree (rules, nested gates, strings) is deep-cloned on every call.
- For every rule, `region()` re-collects `content.lines()` into a `Vec`, and `compiled_rule_matches` calls `to_lowercase()` on the region even when the gate has no `contains`.
- This runs every 300 ms per identified pane (100 ms during pending idle), which is the multiplying hot path AGENTS.md warns about.
- **Suggested rewrite:** split evaluation from explanation, hold an `Arc<LoadedManifest>`, compute each distinct region and its lowercase form once per input, and build evidence only for `explain`.

## DET-002 - Agents without a manifest are reported as idle

- Omp and Mastracode are not in `SCREEN_MANIFEST_AGENTS`, so `load_manifest` returns `None` and `fallback_explain` gives them `AgentState::Idle` (`manifest.rs:445-477`).
- Without the hook installed, the sidebar therefore shows these agents as idle whatever they are doing. For them, `Unknown` is the honest value.
- More generally, "no rule matched" means Idle for every agent except Codex. The hunter notes that is a design choice, but for manifest-less agents no evidence exists at all.

## DET-003 - `stabilize_agent_detection` is an identity function, and the detector's visible-state signals are carried but ignored

Surfaced in three scopes: detection, app core, pane/terminal state.

- `stabilize_agent_detection` (`src/terminal/state.rs:2217`) is an identity function whose name promises stabilisation it does not do.
- `AppEvent::StateChanged.visible_working` (`events.rs:34`) is carried but never read. `actions.rs:1171-1179` passes it (plus a hard-coded `false` for visible_idle) into `TerminalState::set_detected_state_with_screen_signals_at`, where both parameters are `_visible_idle` / `_visible_working` and ignored (`terminal/state.rs:309-317`). The detector computes them and ships them through `AppEvent::StateChanged`.
- `AgentDetection::visible_working` is documented as "diagnostic metadata" (`detect/mod.rs:34-37`), but it drives publishing (`should_publish_detection_update`, the refresh timestamp).
- Either the detector's signal was lost in stripping, or the field and parameters should be removed.

## DET-004 - Relative argv paths are resolved against the server's cwd, with blocking IO in the detection task

- `resolved_agent_name_from_path_token` (`detect/mod.rs:665-674`) calls `std::fs::canonicalize` on argv tokens such as `./agent` or `bin/x` relative to the shepr server's cwd, not the target process's `/proc/<pid>/cwd`. That can misidentify or miss agents.
- These blocking `canonicalize` calls, plus all the `/proc` reads (`probe_foreground_process`, `process_agent_hint` reading `environ`), run synchronously inside the per-pane tokio detection task. They block runtime workers, multiplied by the number of panes.

## DET-005 - The `SHEPR_AGENT` hint is inherited but never stripped

- `process_agent_hint` (`platform/linux.rs:696`, `platform/mod.rs:227`) trusts `SHEPR_AGENT` in any foreground process's environment. Nothing in the tree sets it, and `apply_pane_launch_env` (`pane.rs:127-158`) does not remove it.
- If the server is launched with it set, every pane inherits it. Plain shells and editors are then identified as that agent, because the hint is checked before the name-based identification.
- This breaks the documented rule that panes strip inherited host and agent variables.

## DET-006 - Manifest tests change process-global state

- They mutate `XDG_CONFIG_HOME` and the global `MANIFEST_CACHE` (`manifest/tests.rs:27-47`). They are only safe under one-process-per-test (nextest); any other test in the same process that runs Codex detection concurrently would see synthetic manifests.

## DET-007 - Minor detection issues

- `bundled_manifest` never checks that a bundled file's `id` matches its registry key (overrides are checked).
- The Claude title-spinner rule (priority 1100) outranks every Claude blocker. A title that stays on a spinner frame while a permission prompt is up would hide the blocked state.
- The opencode `permission_required` rule is an ungated `contains` over `whole_recent`, contrary to the stated manifest rule (invariant controls as explicit AND/OR gates).

## DET-008 - Letta bypasses the protected config writer

- `config_file.rs` documents protected writes for user-owned config (hard-link rejection, symlink resolution, permission and xattr preservation, atomic replace).
- `uninstall_letta` writes `~/.letta/settings.json` with a plain `fs::write` (`targets.rs:1271`).
- `install_letta` uses its own rename staging (`targets.rs:908-1107`). `fs::rename(target, backup)` renames a symlinked settings file away and replaces it with a regular file, which breaks symlinked dotfiles.
- Neither function calls `check_config_targets`.
- The comment keeping Letta outside `IntegrationTarget` cites a "frozen client endpoint" enum (`cli/integration.rs:136-139`). That contradicts AGENTS.md ("No wire compatibility obligations"). The parallel experimental path (types, registry, CLI) is leftover structure; the hunter suggests folding Letta into `IntegrationTarget`.

## DET-009 - `integration status` can report "current" for an install that does nothing

- Status looks only at the hook file's version marker (`registry.rs:225-243`); only Grok and opencode also check registration.
- Installs write the hook script before parsing or editing the agent's config (e.g. `install_claude`, `targets.rs:107-123`). A malformed `settings.json` leaves an orphan hook that status reports as current.
- A user deleting the settings entry also leaves status at "current".

## DET-010 - Kimi config: a missing END marker deletes the rest of the file; Codex `[features]` handling

- `remove_kimi_config_block` (`config_edit.rs:783-817`) drops everything to EOF if the BEGIN marker is present but END is missing. Install calls it first, so user config after a damaged block is silently lost.
- `build_codex_config_with_hooks` appends a `[features]` table whenever it finds no header, which produces invalid TOML if the user wrote `features.hooks = …` dotted keys or `features = {…}`.

## DET-011 - Kilo plugin repeats the child-session bug that the opencode plugin fixes

- `assets/kilo/shepr-agent-state.js` does not track child (subagent) sessions, and Kilo is a full-lifecycle authority.
- Subagent `session.created`/`updated` reports replace the pane's resumable session.
- Subagent `session.idle` marks the pane idle while the root session is still working.
- `stateFromSessionStatus` accepts only string statuses, while opencode handles `{type: …}` objects.

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
- Windows leftovers in integration assets and helpers despite the Linux-only rule: `win32` pipe paths in the JS/TS assets, `os.name == "nt"` in the Hermes plugin, `powershell`/`~\\` handling in `config_edit.rs`/`env.rs`. (The no-op platform stubs are in PLAT-006; the Rust-side Windows key path is PLAT-003.)
