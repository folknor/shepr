I reviewed pane runtime and agent detection. This is a partial read. I read all of src/agent_resume.rs, lines 1-1520 of the 4301-line src/pane.rs, the top of src/detect/mod.rs, src/integration/mod.rs, and the top of src/integration/registry.rs. I did not open src/pane/*, the manifests, override loading or src/app/agent_resume.rs. No files were edited and no commands were run.

## 1. Axes that should be types

- **Agent identity as a string in resume.** In `src/agent_resume.rs`, `agent: &str` and `source: &str` sit beside a typed `detect::Agent` enum. `AgentResumePlan.agent`, `PersistedAgentSession.{source,agent}` and the `plan`, `session_ref_from_report`, `session_ref_from_snapshot` and `is_official_agent_source` functions all take `(&str, &str)`.
  - Nothing ties the pair together, and the two arguments can be swapped without complaint.
  - `persisted_session_from_launch_args` builds `"shepr:codex"`/`"codex"` by hand even though it already holds an `Agent`.
  - Fix: an `IntegrationSource` enum (or a `Source::Official(Agent) | Source::Custom(String)`) whose official variant carries the `Agent`. That makes the "official source" check a pattern match instead of a string table.
- **The session ref is a kind tag plus a public string.** `AgentSessionRef { kind, value: String }` has public fields, so the validating constructors can be bypassed; the letta test does exactly this with `value: "default:--yolo"`. `plan()` then trusts `value`.
  - Fix: `enum AgentSessionRef { Id(SessionId), Path(AbsSessionPath) }` with private newtypes.
  - The letta `default:<agent_id>` convention inside a session ID is a hidden sub-grammar. It should be its own variant, parsed once when the ref is taken in rather than when the plan is built.
- **`normalize_session_start_source` returns `Option<String>` over a closed set of 8 values.** It should be an enum.
- **`dedupe_key` is a string built with `format!` and `{:?}` of the kind, joined by NUL.** It should be a derived `Hash`/`Eq` key struct. The Debug format should not carry identity.
- **Hook event tables in `src/integration/mod.rs` hold states as strings.** Entries like `("Stop", None, "idle")` should map to `AgentState` or a `ReportedAction { Session, State(AgentState) }` enum. As strings they can be misspelled, and nothing checks them against what the server parses.
- **In `src/pane.rs`:**
  - `current_size: Cell<(u16, u16, u32, u32)>` and the resize watch channel use a bare 4-tuple where rows/cols and pixel width/height are easy to swap. It should be a `PaneGeometry { rows, cols, cell_px }`.
  - `clamp_pane_size` returns `(u16, u16)`; the clamped size should be a type, so an unclamped size can't reach the PTY.
  - `PaneLaunchIdentity::Managed` holds three `String` ids instead of the typed ids.
  - `publish_state_changed_event` takes `visible_blocker: bool, process_exited: bool` positionally.
  - `apply_agent_detection_publish_update` takes 7 separate `&mut` fields. That is state split from its behaviour (see section 3).
  - `ProcessProbeInput` is a bag of 5 bools/Options with no names on the states they encode.
  - `ProcessProbeResult { agent: Option<Agent>, process_name: Option<String> }` can hold a name with no agent. It should be `Option<(Agent, String)>`.
- **The resume timeout's home.** `MANAGED_AGENT_RESUME_TIMEOUT` lives in pane.rs but is a resume-policy constant, and the resume hold travels as `agent_absence_startup_hold: bool` on the launch env. It would read better as `LaunchPurpose::{Fresh, AgentResume}`.

## 2. Decisions made in more than one place

- **Which agents have official integrations / resume support.** This has at least six independent answers:
  - `is_official_agent_source` (18 pairs)
  - `is_reserved_native_state_source` (9 pairs)
  - the `plan()` match arms (18 again)
  - the `"pi" | "omp"` path-accepting special cases, which appear twice: `session_ref_from_report` and `session_ref_from_snapshot`
  - `integration_target_label` and the `integration_specs` array (18, keyed by `api::schema::IntegrationTarget`)
  - `detect::agent_label` / `interactive_agent_executable`

  They disagree in visible ways:
  - Antigravity is `"agy"` in `agent_label`, `"shepr:antigravity_cli"` as its source, and `"antigravity-cli"` as its integration label.
  - Cursor's resume argv hardcodes `"cursor-agent"` instead of calling `interactive_agent_executable(Agent::Cursor)`, and the same applies to every other argv0.
  - Nothing maps `IntegrationTarget` to `Agent`.

  Owner: a single per-agent descriptor table keyed by `Agent`, in `detect` or a new `agents` module. It would hold the label, executable, integration source, whether native state is reserved, the accepted ref kinds, a resume argv builder, and the integration spec. `IntegrationTarget` becomes a subset view of it.
- **Resume argv0 vs. the detection executable.** Covered above: `plan()` and `interactive_agent_executable` each decide what to exec.
- **Absolute, usable cwd.** `usable_process_cwd` and `usable_reported_cwd` in pane.rs each re-derive "absolute and a directory". They agree today; there should be one `UsableCwd` constructor.
- **Whether the pane's child is gone.** `child_wait_completed: AtomicBool`, `session_leader.has_exited()`/`is_unreaped()`, and `child_pid == 0` (the check in `shutdown_pane_processes`) each answer it. `terminate_pane_session` picks between them with `leader_reaped`. This should be one `ChildLiveness` owner on the runtime.
- **When to probe processes.** `should_probe_foreground_job`, `should_skip_process_probe_for_lifecycle_authority` and `sync_content_change_acquisition` each read the acquisition window and foreground-group change on their own. They should be one scheduler state machine.
- **Pane teardown.** `Drop` and `shutdown()` both run the abort-io-shutdown-teardown sequence, held in step by the `preserve_processes_on_drop` flag. `shutdown()` should just set the policy and let `Drop` own the work.
- **Hook event to state mapping.** Each integration's `*_HOOK_EVENTS` table plus its shell/JS asset decides the state for each event. The server then re-decides what it accepts. There is no shared vocabulary.

## 3. Structure

- **`src/pane.rs` does several jobs at once.** At 4300 lines it holds the pane launch environment policy, shell resolution (`resolve_shell_executable_on`, `pane_shell_from`), the process-probe / agent-presence state machine, the event publishers, the Codex prompt special case, the session teardown machinery (a global `PANE_TEARDOWNS_IN_FLIGHT` counter plus a signal escalation), the sync-timeout render scheduler, and the `PaneRuntime` itself. Suggested split:
  - `pane/launch.rs`: env and shell
  - `pane/teardown.rs`: session kill and in-flight registry
  - `pane/process_probe.rs`: probe decisions, `AgentDetectionPresence`, pending release
  - `pane/runtime.rs`
- **The detector task's state is loose locals.** It lives in `&mut` locals (see `apply_agent_detection_publish_update`) and should be a `DetectorState` struct with methods. That would let the whole detection loop be unit-tested without a runtime, which fits the "state separated from runtime" principle better than today.
- **Agent-specific logic leaks into generic pane code.** `publish_codex_prompt_observation` / `AppEvent::CodexPromptObserved` in pane.rs is one. So is the env-scrub list (`CODEX_THREAD_ID`, `OMPCODE`, `CLAUDECODE`...) in `apply_pane_launch_env`. Both belong in the per-agent descriptor or in integration.
- **Dependency direction.** integration's registry depends on `crate::api::schema::IntegrationTarget`, so the wire schema owns the domain enum of integrations. Invert it: the domain enum lives in integration or agents, and the API reuses it.
- **Integration constants and tests.** `src/integration/mod.rs` holds about 60 per-agent constants as flat globals. `registry.rs` hand-assembles `[..; 18]` from them, so adding an agent means touching mod.rs, registry, targets, agent_resume and detect. A per-target module or `IntegrationSpec` struct would bundle each agent's asset, version, events and path.
- **`agent_resume.rs` tests.** They are large table tests that restate the `plan()` table. With a descriptor table, a single loop-driven test over `Agent::ALL` would cover it.
- **Two hand-maintained agent lists.** `Agent::ALL` and `SCREEN_MANIFEST_AGENTS` are hardcoded arrays with length literals (24/22). The screen-manifest set should come from the bundled manifests, or be checked against them, rather than kept in step by hand.

## Smells and possible bugs

- **`_agent_session_path`** in `session_ref_from_report` has an underscore prefix yet is used.
- **Reserved native-state list.** `is_reserved_native_state_source` omits some official sources, e.g. kimi and opencode, deliberately according to its test. The rule behind "native state reserved" is nowhere stated in code; it should be a descriptor field with a reason.
- **Resume argv safety.** The resume argv is "typed into an interactive shell" (per the comment in `agent_resume.rs`). The only guard against flag injection is the leading-dash check. Shell metacharacters rely on quoting elsewhere that I did not verify, and the `ids_are_data_not_shell_text` test only checks the argv vector, not the typed text.
- **Two mutex styles.** `active_pending_release` silently returns `None` on a poisoned lock, while other sites use `unwrap_or_else(into_inner)`.
