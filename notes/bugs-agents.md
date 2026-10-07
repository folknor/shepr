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

## AGT-014 - A probe with no process group never yields relaunch evidence

Raised as a lateral by the wave 2 adjudicator.

The detector records `identified_group` only when a probe carries a
`process_group_id` (`crates/shepr-mux/src/pane/detect/probe.rs`). A pane whose
probes come back without one never sees a same-agent relaunch as a group
change, so a relaunched agent's startup, held as a possible replacement in
`AgentOwnership::replacement_start`, expires, and the pane keeps the old
session while the new process runs. For Pi every report of the new path is
then cross-talk for the life of that process. When does a probe lack a
process group in practice, and can that case be closed?

## AGT-015 - A nested Pi run can take over a restored pane before its agent is identified

Raised as a lateral by the wave 2 adjudicator.

Before the detector first identifies a restored agent, any recognised Pi
`startup` parks as `ParkRecognizedStart` and is promoted by the first
presence, nested runs included. A nested `pi` started in the first second or
so of a restored pane can therefore displace the restored session path.
