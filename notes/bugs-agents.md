# Bugs: agents and detection (shepr-agent, shepr-detect, shepr-integration)

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

Filed from the agents and detection hunt, which followed into the mux detector
(`shepr-mux/src/pane/detect/`), the server's report admission
(`shepr-server/src/app/api/panes/reports.rs`, `app/state/actions/events.rs`)
and resume on restore (`shepr-mux/src/persist/restore.rs`,
`shepr-server/src/app/agent_resume.rs`). The raw report, including its list of
areas checked and found sound, is in commit 6dc81572
(`notes/hunt-agents-detection.md`).

## AGT-001 - A relaunched agent's startup hook can still beat the probe that sees the new process

Residue of a larger entry. The detector now remembers the identified agent's
process group (`AgentDetectionPresence::identified_group` in
`crates/shepr-mux/src/pane/detect/probe.rs`), and a probe that finds the same
agent in a new group publishes an exit, then the replacement presence. So
`claude; claude` and `pi; pi` are now seen as a new process.

What is left is ordering. A probe runs at most every
`PROCESS_RECHECK_ACTIVE_AGENT` (300 ms), and a new agent's startup hook can
arrive sooner. For policies that leave `Startup` out of `replacement_starts`
(Claude, Pi, Grok, OpenCode, and `DEFAULT` for Copilot, Cursor, Devin, Droid),
that early start still meets the old session, and ownership refuses it as
`ReplacedSession` (`crates/shepr-detect/src/ownership/source/start.rs`,
`transition_start`). The `HookSessionPolicy::CLAUDE` comment now says this
openly ("Hooks arriving before that process evidence may still be refused").

What happens then, as a reviewer traced it:

- Claude, Copilot, Cursor, Droid, Grok (screen-owned session, `SessionStart`
  their only session hook). The refused start is the only report carrying the
  new session. The probe's exit clears the persisted session and the
  replacement presence discards the checkpoint candidate, so the pane ends up
  with no session: restore resumes nothing rather than the wrong
  conversation. A save or pane death inside the one-probe window still holds
  the old session.
- Pi (full lifecycle, `Startup` not a replacement). After the exit the source
  sits in `AwaitingProcess` with no pending start, and `observe_process` needs
  one to reopen the generation. Every later Pi report carries the new path and
  parks as `Pending`, waiting for a start that never comes. The session is
  never established and hook state is ignored for the life of the process;
  screen detection governs until `/new`, `/resume` or `/fork`.
- Devin recovers on its next `UserPromptSubmit`. Codex, OMP, Kimi, MastraCode
  and Kilo list `Startup` as a replacement and are unaffected.

Options, for an adjudicator:

1. Park a `ReplacedSession`-refused `startup` from the same agent as a pending
   start and let the exit-then-presence path promote it. Caveat:
   `HookSourceState::process_exited` consumes a start parked before the exit
   (it takes that start's process to be the one that exited), so the parked
   start must be marked as awaiting a group-change exit.
2. Carry the pgid on presence and let `transition_start` treat a `startup`
   from an agent whose identified group changed within a short window as a
   replacement. Still loses when the hook beats the probe.
3. Accept the residue (the session is lost, never wrong), document it at the
   code sites and close.

## AGT-013 - A stray Pi `startup` never replaces a persisted path, even with no live process authority

Raised as a lateral by the wave reviewer.

`pi_startup_preserves_persisted_session_without_live_authority`
(`crates/shepr-detect/src/ownership/tests.rs`) now pins that a Pi `startup`
with a different path never replaces a persisted path. Restore after a server
restart resumes Pi with the same path, so that is unaffected. But a user who
quits a restored Pi before the detector has identified it, and starts a fresh
`pi`, relies on the exit being observed first; otherwise the fresh session is
refused and the pane keeps the restored path.

## AGT-002 - OMP's `PI_CONFIG_DIR` is refused when relative, which is the form OMP and shepr's own hook command treat as home-relative

Hunter's severity: medium-low.

Claim broken: `crates/shepr-integration/src/command.rs`, the `directory_setup`
doc: "It accepts a relative override, which `AgentIntegrationPaths` refuses,
and resolves it from the agent's cwd." The `AgentIntegrationPaths` doc in
`env.rs` gives the same reason: "relative config paths would resolve against
the server's cwd here and the pane's cwd in the agent."

What the code does.

- For OMP, the shell arm in `directory_setup` (`Target::Omp`) resolves a
  relative `PI_CONFIG_DIR` under `$HOME`, not the cwd:
  `*) hook_dir="$HOME/$hook_dir"`.
- `env.rs` `omp_extension_dir` also has a branch that joins a relative value
  onto the home directory. That is evidently OMP's semantics, where the
  variable names a directory under home and defaults to `.omp`.
- That branch is dead for any override, though. `agent_config_override` runs
  first and returns `config_shape("PI_CONFIG_DIR must be an absolute path")`
  for anything relative. Only the built-in default `.omp` ever reaches it.

Result. An OMP user with `PI_CONFIG_DIR=.omp-work` gets an install error at
every release launch and never gets the extension, so no session identity, no
resume and no hook state. `agent_present` errors the same way.

Direction. Resolve OMP's override the way OMP and `directory_setup` do
(relative means under `$HOME`), and drop the generic absolute check for this
one variable. Then fix the `command.rs` doc, which is wrong for OMP either way.

Worth checking against OMP's source: whether OMP joins even an absolute value
onto `$HOME`. Node's `path.join(home, "/abs")` does that. If OMP builds the
path that way, the absolute case is wrong too: shepr would install into `/abs`
while OMP reads `$HOME/abs`.

## AGT-003 - A detector exit refused as stale is never re-delivered, and the pane can then present a dead full-lifecycle agent indefinitely

Hunter's severity: low probability, sticky outcome.

Claims broken:

- `AgentOwnership::effective_row` ("Process exits withdraw detector
  identity").
- The comment in `ownership/mod.rs`: "Confirmed process-exit updates clear
  matching authority before recomputing state."

What the code does.

- `transition_detector_observation` (`ownership/source/detection.rs`) drops
  any observation with `now < fallback_observed_at`.
- `apply_source_effect` moves `fallback_observed_at` forward to the hook's
  `reported_at` each time full-lifecycle authority activates. Activation
  happens on the first report of each generation, including after every
  session replacement that cleared authority.
- So an exit observation stamped at the start of a probe that began before an
  activating report is dropped.

Why the exit is never replayed.

1. The detector treats the exit as delivered once it publishes it
   (`AgentExitPhase::report` turns `ReportOwed` into `ClearOwed`).
2. On the next probe it withdraws the identity with an agent-less,
   `process_exited = false` update.
3. Ownership ignores that withdrawal under live full-lifecycle authority
   (`should_ignore_detected_state_under_full_lifecycle_hook` with
   `agent = None`). The test
   `full_lifecycle_hook_authority_ignores_detected_agent_clear_without_process_exit`
   pins this.
4. The detector, now holding no agent, never reports an exit again.

The pane keeps presenting the agent, with screen detection paused, until
another agent is identified or the pane dies.

Trigger. An agent that exits within one probe tick of an activating report,
for example an OMP or Pi `/new` followed by a quick quit, or a very
short-lived session. Rare, but nothing ever clears the result.

Direction. Two options:

- Do not judge exits by the hook watermark. An exit is about the process, and
  the hook carries no process identity.
- Or have ownership acknowledge exits, so the detector keeps `ReportOwed` until
  the exit is applied.

## AGT-004 - Pi and OMP send source-less session refreshes that the server logs as integration contract violations

Hunter's severity: low.

Claim broken: `HookRejection::is_integration_fault` (`ownership/mod.rs`) treats
`UnrecognizedStart` as "a bundled shepr hook violated a report contract".
`admit_hook_outcome` logs those at WARN as "bundled agent integration report
violated its contract".

What the code does.

- Both bundled extensions send a session report with no start source on every
  `agent_start`: `templates/decoders/pi.ts` and `omp.ts` call
  `reportSession()`.
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

Direction. Either make the extensions send a recognized source, or nothing,
when they have no new start to report. Or classify an omitted source on a
refresh as routine, and keep the WARN for a source the agent itself supplied
that shepr does not know.

## AGT-005 - The report API refuses a report carrying both id and path; `session_ref_for_agent_report` and the resume-key doc describe preferring the path

Hunter's severity: low (doc and dead code).

What the code does.

- `parse_origin_session_ref` (`shepr-server/src/app/api/panes/reports.rs`) and
  the `PaneReportAgentSessionParams` schema doc refuse a report with both
  `agent_session_id` and `agent_session_path` as `InvalidRequest`.
- But `shepr_agent::resume::session_ref_for_agent_report` has a whole branch
  that prefers the path and falls back to the id when both are present. The
  test `report_ref_prefers_pi_and_omp_paths_and_validates_values` pins that
  branch.
- The `AgentResumeKey` doc in `resume.rs` states as current behaviour that "a
  report prefers the path when it carries both, so the mixed pair practically
  never arises".

Which side is wrong. The code path is unreachable in production and the doc is
false: such a report is refused.

Direction. The hunter thinks the API is right and the agent crate should match
it. Make `session_ref_for_agent_report` take one already-chosen reference, and
reword the `AgentResumeKey` doc to say the bundled extensions send exactly one
kind.

## AGT-010 - A rule reference with an explicit `region = "whole_recent"` passes silently

Raised as a lateral observation.

In `expand_rule_gate` (`manifest.rs`), a rule that combines `rule = "..."` with
an explicit `region = "whole_recent"` passes the "rule reference with inline
region" check, because it compares against the default string, and the region
is silently ignored. Bundled-only, so it bites only a manifest author.
