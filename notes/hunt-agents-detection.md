# Defect hunt: agents and detection

Scope: `shepr-agent`, `shepr-detect`, `shepr-integration`, followed into the
mux detector (`shepr-mux/src/pane/detect/`), the server's report admission
(`shepr-server/src/app/api/panes/reports.rs`,
`app/state/actions/events.rs`) and resume on restore
(`shepr-mux/src/persist/restore.rs`, `shepr-server/src/app/agent_resume.rs`).
Reconnaissance only; nothing was edited or run.

Findings are ordered by how much they matter. Each names the claim it breaks.

## F1. A same-agent relaunch between two probes is never seen as a new process (medium)

**Claim broken.** `HookSessionPolicy::CLAUDE` in `crates/shepr-agent/src/lib.rs`:
"`startup` reports a new process, which has no live session in this pane to
replace." The same reasoning holds up every policy that leaves `Startup` out of
`replacement_starts` (Claude, Pi, Grok, OpenCode, and DEFAULT for Copilot,
Cursor, Devin, Droid): it only works if the old process's exit always reaches
ownership before the new process's start does.

**What the code does.** The detector has no process identity. It only knows
"agent X is in the foreground".
- `DetectorState::observe_process_probe` (`shepr-mux/src/pane/detect/probe.rs`)
  reports an exit only when a probe finds the pane shell in the foreground
  (`ForegroundShellAgentAction::ReportProcessExit`).
- A tick runs every `PROCESS_RECHECK_ACTIVE_AGENT` (300 ms). A foreground-group
  change triggers a probe, but if that probe already finds the next process of
  the same agent, `AgentDetectionPresence::observe_process_probe(Some(same))`
  returns "unchanged". No exit and no replacement are published, even though
  `tick.group_changed` is set in that same tick.
- `claude; claude`, `pi; pi`, a relaunch loop, or a wrapper script that runs the
  agent twice all hit this reliably. The shell holds the foreground for
  milliseconds only.

**Consequences in ownership** (`crates/shepr-detect/src/ownership/source/start.rs`,
`transition_start`):
- **Claude, Copilot, Cursor, Devin, Droid.** `conflicting_same_owner_session_ref`
  refuses the new process's `startup` (or omitted) start as `ReplacedSession`.
  The pane keeps the old process's session, saves it, and on restore resumes the
  wrong conversation.
- **Pi with live full-lifecycle authority.** It is worse. The new process reports
  a path, which `conflicting_same_owner_session_ref` ignores (it only compares
  ids). But `same_owner_full_lifecycle_hook_authority_session_ref` returns the
  old path, and `Startup` is not in `HookSessionPolicy::PI`, so the start is
  refused as `ReplacedSession`. Every state report of the new process then
  routes through `HookSourceState::report_route`. There
  `authority_session_ref != incoming`, so it is refused as `CrossTalk`. The
  authority still governs (`effective_row`: the detected agent is still Pi, with
  no exit recorded), so screen detection stays paused. The pane shows the dead
  process's last state for the whole life of the new one.
- **OMP.** OMP has `Startup` in its list, so it recovers. Codex does too, and
  also has `CODEX_THREAD_ID` guards.

**Direction.** The detector already sees the evidence: the foreground pgid
changed while the identified agent stayed the same. Treat a pgid change under an
unchanged identified agent as `ReportReplacementProcess` (publish the exit, then
the replacement). A smaller fix is to carry the pgid in `AgentProcessDetected` /
`StateChanged` and let ownership treat a new pgid as an exit plus a new
presence. Either way, the policies' comments then become true by construction
rather than by timing.

## F2. OMP's `PI_CONFIG_DIR` is refused when relative, which is the form OMP and shepr's own hook command treat as home-relative (medium-low)

**Claim broken.** `crates/shepr-integration/src/command.rs`, the
`directory_setup` doc: "It accepts a relative override, which
`AgentIntegrationPaths` refuses, and resolves it from the agent's cwd." The
`AgentIntegrationPaths` doc in `env.rs` gives the same reason: "relative config
paths would resolve against the server's cwd here and the pane's cwd in the
agent."

**What the code does.**
- For OMP, the shell arm in `directory_setup` (`Target::Omp`) resolves a relative
  `PI_CONFIG_DIR` under `$HOME`, not the cwd:
  `*) hook_dir="$HOME/$hook_dir"`.
- `env.rs` `omp_extension_dir` also has a branch that joins a relative value onto
  the home directory. That is evidently OMP's semantics, where the variable names
  a directory under home and defaults to `.omp`.
- That branch is dead for any override, though. `agent_config_override` runs
  first and returns `config_shape("PI_CONFIG_DIR must be an absolute path")` for
  anything relative. Only the built-in default `.omp` ever reaches it.

**Result.** An OMP user with `PI_CONFIG_DIR=.omp-work` gets an install error at
every release launch and never gets the extension, so no session identity, no
resume and no hook state. `agent_present` errors the same way.

**Direction.** Resolve OMP's override the way OMP and `directory_setup` do
(relative means under `$HOME`), and drop the generic absolute check for this one
variable. Then fix the `command.rs` doc, which is wrong for OMP either way.

Worth checking against OMP's source: whether OMP joins even an absolute value
onto `$HOME`. Node's `path.join(home, "/abs")` does that. If OMP builds the path
that way, the absolute case is wrong too: shepr would install into `/abs` while
OMP reads `$HOME/abs`.

## F3. A detector exit refused as stale is never re-delivered, and the pane can then present a dead full-lifecycle agent indefinitely (low probability, sticky outcome)

**Claims broken.**
- `AgentOwnership::effective_row` ("Process exits withdraw detector identity").
- The comment in `ownership/mod.rs`: "Confirmed process-exit updates clear
  matching authority before recomputing state."

**What the code does.**
- `transition_detector_observation` (`ownership/source/detection.rs`) drops any
  observation with `now < fallback_observed_at`.
- `apply_source_effect` moves `fallback_observed_at` forward to the hook's
  `reported_at` each time full-lifecycle authority activates. Activation happens
  on the first report of each generation, including after every session
  replacement that cleared authority.
- So an exit observation stamped at the start of a probe that began before an
  activating report is dropped.

**Why the exit is never replayed.**
1. The detector treats the exit as delivered once it publishes it
   (`AgentExitPhase::report` turns `ReportOwed` into `ClearOwed`).
2. On the next probe it withdraws the identity with an agent-less,
   `process_exited = false` update.
3. Ownership ignores that withdrawal under live full-lifecycle authority
   (`should_ignore_detected_state_under_full_lifecycle_hook` with `agent = None`).
   The test `full_lifecycle_hook_authority_ignores_detected_agent_clear_without_process_exit`
   pins this.
4. The detector, now holding no agent, never reports an exit again.

The pane keeps presenting the agent, with screen detection paused, until another
agent is identified or the pane dies.

**Trigger.** An agent that exits within one probe tick of an activating report,
for example an OMP or Pi `/new` followed by a quick quit, or a very short-lived
session. Rare, but nothing ever clears the result.

**Direction.** Two options:
- Do not judge exits by the hook watermark. An exit is about the process, and
  the hook carries no process identity.
- Or have ownership acknowledge exits, so the detector keeps `ReportOwed` until
  the exit is applied.

## F4. Pi and OMP send source-less session refreshes that the server logs as integration contract violations (low)

**Claim broken.** `HookRejection::is_integration_fault` (`ownership/mod.rs`)
treats `UnrecognizedStart` as "a bundled shepr hook violated a report contract".
`admit_hook_outcome` logs those at WARN as "bundled agent integration report
violated its contract".

**What the code does.**
- Both bundled extensions send a session report with no start source on every
  `agent_start`: `templates/decoders/pi.ts` and `omp.ts` call `reportSession()`.
- Pi's `session_start` also forwards `event?.reason` as is, with no fallback,
  unlike OMP, Kimi and MastraCode, which default to `startup`. That includes
  `"reload"`, which shepr does not know, as the bun test in
  `assets/shepr-agent-state.test.ts` exercises.
- `transition_start` routes a full-lifecycle start to `ParkRecognizedStart`
  whenever the process is not present, or present but unanchored. Every such
  report is then refused `UnrecognizedStart`.
- So in any pane where Pi/OMP is not yet identified, or never is (for example
  run through a wrapper the probe cannot see through), each turn logs a
  contract-violation WARN. That WARN comes from behaviour the bundled asset
  produces by design.

**Direction.** Either make the extensions send a recognized source, or
nothing, when they have no new start to report. Or classify an omitted source on
a refresh as routine, and keep the WARN for a source the agent itself supplied
that shepr does not know.

## F5. The report API refuses a report carrying both id and path; `session_ref_for_agent_report` and the resume-key doc describe preferring the path (low, doc and dead code)

**What the code does.**
- `parse_origin_session_ref` (`shepr-server/src/app/api/panes/reports.rs`) and
  the `PaneReportAgentSessionParams` schema doc refuse a report with both
  `agent_session_id` and `agent_session_path` as `InvalidRequest`.
- But `shepr_agent::resume::session_ref_for_agent_report` has a whole branch
  that prefers the path and falls back to the id when both are present. The test
  `report_ref_prefers_pi_and_omp_paths_and_validates_values` pins that branch.
- The `AgentResumeKey` doc in `resume.rs` states as current behaviour that "a
  report prefers the path when it carries both, so the mixed pair practically
  never arises".

**Which side is wrong.** The code path is unreachable in production and the doc
is false: such a report is refused.

**Direction.** I think the API is right and the agent crate should match it.
Make `session_ref_for_agent_report` take one already-chosen reference, and
reword the `AgentResumeKey` doc to say the bundled extensions send exactly one
kind.

## F6. Stale fact in the integration crate's header comment (low, doc)

`crates/shepr-integration/src/lib.rs` says the server refuses a report "with an
empty agent label". Reports carry no agent label any more. The server refuses an
unsupported `source` (`ReportOrigin::parse`, answering `invalid_agent`), and
there is no label to be empty. Reword it to name the source refusal.

## Lateral observations (outside the scoped question or not quite defects)

- **Codex/partial-state reports lost after an exit.**
  `transition_report` refuses a partial-state report with `ProcessExited` while
  the recorded exit is that agent's. A new Codex started in the same pane is
  refused until the detector identifies it as a replacement process. A
  `UserPromptSubmit` sent in that window is lost, and the pane then shows Idle
  while Codex works. That lasts until the next report or a visible screen
  signal. Codex's screen manifest has a working fallback, but authority wins over
  screen Working. Narrow race; the same pgid-based replacement detection
  suggested in F1 would close it.
- **`conflicting_same_owner_session_ref` only guards id references.** Path
  references (Pi, OMP) skip it entirely (`current.session_ref().is_id() &&
  session_ref.is_id()`). For Pi, the `replaced_hook_session` check covers it
  while authority is live. With only a persisted path and no authority, any
  same-owner start replaces the session, whatever its source.
- **JS reporters seed `seq` once per module load (`Date.now() * 1000`) and never
  re-sample.** Suppose the host clock steps backwards and the extension then
  reloads in a live process, such as Pi `/reload`. The new instance's seqs sit
  below the old instance's, and the server's wall clock has not reversed relative
  to its last acceptance. So `HookSequence::supersedes` drops the new reports
  until wall time passes the old base. `seq_units.txt` and `limits.rs`
  (`HOOK_SEQUENCE_REANCHOR_AFTER`) say a backwards step re-anchors. That is true
  for reports arriving across the step, not for a reload after it. Low impact.
- **A rule reference with an explicit `region = "whole_recent"` passes
  silently.** In `expand_rule_gate` (`manifest.rs`), a rule that combines
  `rule = "..."` with an explicit `region = "whole_recent"` passes the "rule
  reference with inline region" check, because it compares against the default
  string, and the region is silently ignored. Bundled-only, so it bites only a
  manifest author.
- **`CompiledContains` final-sigma handling.** It lowercases the needle with
  `str::to_lowercase`, which applies final sigma in the needle's own context, but
  the text per character. So a needle ending in `Σ` cannot match it mid-word.
  English manifests never hit it.
- **Kimi and MastraCode have no nested-process guard.** Both put `Startup` in
  `replacement_starts`, and their decoders have none of the guards Codex
  (`CODEX_THREAD_ID`), OMP (`OMPCODE`) and Claude (`agent_id`, `CLAUDE_JOB_DIR`)
  have. A nested `kimi` or `mastracode` started from inside the pane's agent
  would replace the pane's root session. Speculative; it depends on whether
  those agents ever spawn themselves.

## Checked and found sound

- Descriptor table guards and the inverse checks (`TARGETS_HAVE_INTEGRATIONS`).
- Source parsing.
- Session id and path validation, including the leading-dash refusal.
- Resume argv construction and shell quoting.
- The manifest compile limits and the region extraction offsets (CRLF, the
  pointer-based offsets).
- Priority ordering shared by detect and explain.
- `unknown_is_stable`.
- Process identification for runtime wrappers.
- Installer publication order (assets before configs).
- User-config replacement through symlinks, with lock and snapshot conflict
  checks.
- Kimi managed-block editing.
- Claude matcher derivation from the policy.
- The generated-asset staleness test.
- Detection content sequence bumps on any PTY output, so OSC title changes do
  re-scan an idle pane.
