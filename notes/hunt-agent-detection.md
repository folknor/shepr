# Hunt: agent detection and agent state

Scope: `crates/shepr-agent/src/detect/` (engine, proc tree, manifests),
`crates/shepr-agent/src/agent/`, `crates/shepr-mux/src/pane/agent_detection.rs`
and the loop around it (`pane/process_probe.rs`, the detection task in
`pane/runtime.rs`), the point where detector output meets hook reports
(`crates/shepr-mux/src/terminal/state/`), the `detect` API/CLI, and
`src/autodetect.rs`.

Ordered by severity. Each finding names the claim it breaks.

---

## 1. A failed agent resume on a quiet pane never withdraws its seeded agent

**Claim broken.** `restored_terminal` (`persist/restore.rs`): "The seed does not
outlive a failed resume: at expiry the detector publishes a no-agent `Unknown`
update, which withdraws it." Same promise in the doc of `withhold_agent_absence`
("once it expires, after which a pane whose resume never produced the agent
reports the absence as usual and the seed goes").

**What happens.** `DetectorState::new` starts with `state: AgentState::Idle`
(while `reset()` uses `Unknown`). During the hold every read tick goes
read -> `detection_content` (records `last_screen_scan_detection_content_seq`)
-> `withhold_agent_absence` -> `continue`, so `state` stays `Idle` and the
agent stays `None`. `should_skip_idle_screen_scan` treats `Idle` as a stable
state for any agent, `None` included. So once the resume command has failed and
the shell has printed its prompt (well inside the 30 s hold), the screen stops
changing, every later tick is skipped at `should_read_screen`, and
`withhold_agent_absence` is never reached again. The expiry path runs only after
the next PTY byte, resize or clear. Until the user touches the pane, the sidebar
shows an idle agent on a plain shell, and the seed's persisted session stays
too.

No test covers expiry. The hold tests call `withhold_agent_absence` directly.

**Fix direction.** Start the detector in `Unknown`, as `reset` does, so a
no-agent pane is never "stable Idle". Or check hold expiry before the read-skip
decision. Better: make the skip decision a function of the last published
detection, not of an initial placeholder state.

---

## 2. Under full-lifecycle hook authority, agent loss is only seen if the foreground group changes

**Claims broken.** `PROCESS_RECHECK_IDENTIFIED` ("Recheck cadence for an already
identified process") and `AGENT_MISS_CONFIRMATION_ATTEMPTS` (misses are
confirmed, then the agent is dropped). The sidebar's promise to show every
agent's state.

**What happens.** Three pieces add up:

- `ProcessProbeScheduler::schedule` returns `Skip` via
  `lifecycle_authority_can_skip` whenever authority is active, a foreground
  group is observed, a probe has happened and the group has not changed. That
  return comes before the `elapsed_since_check >= PROCESS_RECHECK_IDENTIFIED`
  safety check, so an identified agent is never rechecked on a timer. The only
  test of the safety probe under authority
  (`scheduler_keeps_identified_safety_probes_without_a_foreground_group`) covers
  the no-foreground-group case.
- `may_scan_screen` refuses screen scans under authority unless
  `process_exited`.
- `set_detected_state_with_screen_signals_at`: while a live full-lifecycle
  authority holds and the report is not `process_exited`, a detector report of
  `agent: None` is ignored, because `hook_authority_conflicts_with_detected_agent(None)`
  is false. `detected_agent` stays set, and authority stays "live".

The result: an agent that exits without the foreground process group changing
is never noticed. Examples: pi under a wrapper script or `bash -c 'pi; ...'`
that outlives it, since both share the script's pgid. The same goes for an agent
that is replaced by a non-shell program: the group changes once, one miss is
counted, and every later probe is skipped, so the 6-miss confirmation never
completes. The pane keeps the last hook state indefinitely. If the agent crashed
mid-turn that state is `Working`, and nothing withdraws it.

A related path: `set_full_lifecycle_authority_active(true)` triggers
`DetectorState::reset()`, which drops the detector's agent. If the single
re-probe right after that fails to identify the agent (for example, argv is
unreadable because the leader is in `D` state and the agent is only
identifiable from argv), `has_probe` is now true and authority skips every
later probe. The detector then holds `agent = None` for good. When the agent
later exits to the shell, `foreground_shell_agent_action` sees
`previous_agent = None` and never reports a process exit.

**Fix direction.** Keep the `PROCESS_RECHECK_IDENTIFIED` probe under authority.
It costs one probe per 5 s and is the only safety net. Do not let the app
ignore an agent-absent report that has passed miss confirmation. Do not clear
the detector's agent on an authority reset, or re-probe until the agent is
reacquired.

---

## 3. The Working -> Idle debounce ignores presentation: `Unknown` flips publish at once

**Claims broken.** `AgentDetection::visible_idle`: "The pane's detection loop
uses it to publish a Working -> Idle change at once instead of waiting for the
idle to be confirmed over several ticks." `AGENT_PENDING_IDLE_CONFIRMATIONS`:
"filtering a single transient frame". AGENTS.md: "Unknown presents as Idle."

**What happens.** `PendingIdleConfirmation::should_hold_working_to_idle` holds
only when `next.state == AgentState::Idle`. `Unknown` presents as Idle, but a
`Working -> Unknown` flip is not held and publishes immediately. Two manifests
produce `Unknown` routinely:

- Codex: no match falls back to `Unknown` (`fallback_state`).
- Letta: `composer_input`, `profile_selector` and the priority-0 catch-all
  `no_live_state_evidence` all yield `Unknown`.

For these agents, a single transient frame during a turn (a status line redrawn
in place, a partial frame) shows as Working -> Idle -> Working in the sidebar,
which is exactly what the hold exists to filter. For Codex this matters when no
hook authority exists: before the first `UserPromptSubmit`, when python3 is
missing, or after the authority is cleared.

**Fix direction.** Key the hold on `presentation_state()`: hold any
Working -> presented-Idle transition that lacks `visible_idle`.

---

## 4. `detect capture` does not capture what the detector evaluates, so an offline explain disagrees with the live one

**Claims broken.** AGENTS.md: "`shepr detect capture <pane>` prints the text the
detector evaluates for a pane", and the manifest workflow ("capture the pane
... encode invariant controls"). The `src/cli/detect.rs` module doc says the
same.

**What happens.** The detector evaluates `(screen, osc_title, osc_progress)`.
`handle_detect_capture` returns only `detection_text()`. `detect explain <pane>`
reads all three (`handle_detect_explain`), but `detect explain --file` goes
through `explain_for_label` -> `explain()`, which hard-codes empty OSC strings.
Codex, Claude, Amp, Grok, Kiro, Qwen and Letta all have top-priority OSC rules.
Capturing such a pane and explaining the capture offline can therefore select a
different rule and state than the live explain, and the maintainer cannot
reproduce the live decision from a capture. For example, Codex `Action Required`
in the title, a Grok title spinner, or Claude's title spinner with its
dialog-aware `not` gate.

**Fix direction.** Have capture emit the OSC title and progress alongside the
screen, in a format `--file` reads back, and have `explain --file` feed them to
`explain_with_input`.

---

## 5. Agent-specific policy is hard-coded outside the manifests

`fallback_state` hard-codes `Agent::Codex` as the only agent whose no-match
result is `Unknown`. `should_skip_idle_screen_scan` hard-codes `Codex` (plus
"no screen manifest") as the only agents whose `Unknown` is stable. Letta's
manifest ends in a catch-all `Unknown` rule, so a Letta pane sitting on its
composer or the catch-all never qualifies for the idle skip. It copies and
evaluates the full screen every 300 ms while nothing changes. That is the hot
path AGENTS.md says multiplies per pane.

`should_skip_idle_screen_scan` also asks `agent.screen_manifest()` (the
descriptor flag), while detection uses the compiled manifest. If a bundled
manifest failed to compile (logged, then `None`), detection returns `Unknown`
forever and the skip logic treats that as transient, so it reads every tick.
`has_screen_manifest`, which answers the right question, has no production
caller. Its doc names consumers ("consumers that wait for a screen-derived
`Idle`") that do not exist.

**Fix direction.** Make the no-match fallback a manifest field
(`fallback = "unknown"`) and derive "Unknown is stable" from the compiled
manifest. That removes both `Codex` special cases.

---

## 6. Hot-path waste in the detection tick

- `DetectorState::detection_content`, for an identified agent, compares the new
  screen text with `last_detection_text` and then `clone_from`s it every scan.
  The resulting `changed` flag is only consumed by
  `ProcessProbeScheduler::content_changed`, which returns immediately when
  `agent.is_some()`. So every agent pane pays a full-screen string compare and
  copy per tick for nothing.
- The tick locks the terminal core three times (`detection_text`,
  `agent_osc_title`, `agent_osc_progress`) and allocates three Strings. One
  locked read returning all three would do.
- Detection is a pure function of `(screen, osc_title, osc_progress)`, and
  `detection_content_seq` covers all three (the OSC values arrive as bytes, and
  flushes and resizes bump the sequence). Re-evaluating an unchanged input in
  `Working` or `Blocked` can only return the same result. The only tick-driven
  needs, the pending-idle confirmations and the stable-blocker refresh, could
  reuse the last `AgentDetection` instead of re-reading. The "only stable states
  may skip" rule in `should_skip_idle_screen_scan` is broader than necessary.

---

## 7. Stale or inaccurate documentation in scope

- `AgentDetection::visible_working`: "forwards it only together with
  `state == Working`". It is not forwarded at all: `AppEvent::StateChanged` and
  `StateChangedUpdate` have no working flag. Only `visible_blocker` crosses.
- `AGENT_PENDING_IDLE_CONFIRMATIONS = 3` is documented as "Matching idle
  observations needed before publishing idle". The hold publishes on the fourth
  matching observation: the first starts the hold, and three more confirmations
  follow (`pending_idle_holds_working_to_plain_idle_until_confirmed` asserts four
  calls).
- `may_scan_screen`: when the startup grace has just expired, the branch clears
  it but still returns `false`, so the first scan waits one extra tick after
  `AGENT_STARTUP_GRACE_WINDOW`. This is harmless, but it is not what "grace
  window" says.
- `RegionSpec::AfterCurrentPromptBlockMarker` returns a slice that starts at
  the marker line, which is not "after" it. No manifest uses it (see 8).

---

## 8. Dead code in scope

- `ForegroundProcess::argv0` is never populated (`foreground_job_from_members`
  and `foreground_group_leader_job` both set `None`), yet
  `normalized_process_name` reads it first.
- Region kinds with no bundled user: `current_prompt_block_marker`,
  `after_current_prompt_block_marker`, `above_prompt_box` and `bottom_lines(N)`.
  With no local overrides (a detection change ships as a new build), these are
  unreachable.
- `IdleScreenScanSkipInput` and `DetectionScreenReadInput` are field-for-field
  identical. `decide_detection_screen_read` only rewraps one into the other.
  `DetectionTransitionDecision` and `DetectionPublishDecision` repeat the same
  split.
- `has_screen_manifest` is test-only (see 5).

---

## 9. Restored history is read as live agent chrome (risk, not reproduced)

`seed_history_ansi` writes the saved history into the fresh terminal, so the
previous session's last frame sits on the live screen. `detection_text` reads
the live screen rows. A resumed agent that draws inline (not on the alternate
screen) leaves that old frame above its own output, inside `whole_recent` and
other broad regions. Several blocker rules read `whole_recent` with
`visible_blocker = true`, for example Claude's `bash_permission_prompt` and
`legacy_no_prompt_blocker`, Amp's `approval_footer`, Cursor's `approval_prompt`
and Grok's `option_dialog_blocked`. Codex's `screen_working_fallback` accepts
arbitrary non-marker lines after a timer line up to the end of the region. A
saved frame that ended on a dialog or a live timer can therefore classify the
resumed agent as Blocked or Working until enough output scrolls it off. The
3 s startup grace only delays the first scan. I did not reproduce this. It is
worth one capture test with a restored Codex or Claude frame.

---

## 10. Identification ranking prefers a wrapped child over a real agent executable

In `identify_agent_in_job`, `ProcessPriority::NormalizedAlias` ranks above
`AgentExecutable`. When the group leader is unrecognised (an `npx`/npm leader or
a wrapper script), a node child whose argv names a different agent outranks a
process whose own name is an agent. Such a child could be an MCP server shipped
as `.../bin/codex`, while the other process is named `claude`. The alias rank
exists for Nix `.x-wrapped` and node-wrapped agents, but it applies to every
job member. This is heuristic and no doc promises otherwise. Flagged as a smell.

---

## Structural note

The per-pane detection state machine lives in three places:

- pure deciders with single-use input structs in `agent_detection.rs`
- `DetectorState` in `process_probe.rs`
- the orchestration loop in `runtime.rs`, which interleaves `spawn_blocking`
  probes, OSC clearing, theme restore and publishing

Findings 1, 2 and 3 are all interaction bugs between these pieces: an initial
state set in one file, a skip rule in another, an early `continue` in the
third. A single pure `DetectorState::tick(Observations) -> TickOutput` (where
`Observations` holds the foreground pgid, probe result, content seq, screen and
OSC, authority flag and `now`, and `TickOutput` holds events to publish and the
next wake) would put every transition in one testable function and leave the
runtime as I/O only.

The merge layer (`terminal/state/detection.rs`, `hooks.rs`, `lifecycle.rs`) has
the same shape at a larger scale. Effective state is decided by several
interacting predicates (`hook_authority_is_effective`,
`should_ignore_detected_state_under_full_lifecycle_hook`,
`visible_blocker_overrides_hook`, release suppression, stale-session memory).
They compare timestamps from two clocks: the detector task's `Instant::now()`
at observation, and the app's per-iteration `clock_now` for hook reports. An
explicit arbitration table (inputs: detector report, hook authority, agent
kind; output: effective agent and state) is worth considering over the current
predicate web.

## Checked and found sound

- The rule engine (`manifest.rs`): the priority-order fast path and `explain`
  agree on tie-breaks, regions are interned and extracted once, gate validation
  and limits hold, and case-insensitive `contains` is consistent between needle
  and text.
- The detection snapshot reads live screen rows, not the viewport (backed by the
  `detection_text_stays_at_bottom_when_viewport_is_scrolled` test).
  Synchronized-output flushes by timer bump `detection_content_seq`. The seq
  load and screen read order cannot miss an update.
- Process exit to the pane shell: `ReportProcessExit`, then an `Idle` publish
  with `process_exited`, then `ClearAgent`. This works under authority because
  the foreground group changes.
- `src/autodetect.rs` has nothing to do with agent detection (it is the launch
  flow). Its behaviour matches its doc.
