# Defects: agent detection, integrations and agent state arbitration

Filed from the defect hunt over `crates/shepr-agent/src/detect/`,
`crates/shepr-agent/src/agent/`, `crates/shepr-mux/src/pane/agent_detection.rs`,
`crates/shepr-agent/src/integration/` (with its assets), and
`crates/shepr-mux/src/terminal` (the `TerminalState` arbitration between
detection and hook reports).

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## AGENT-001 - A failed agent resume on a quiet pane never withdraws its seeded agent

Scope: agent-detection. The initial-state mismatch at its root was also noted by
the mux-pane hunter.

**Claim broken.** `restored_terminal` (`persist/restore.rs`): "The seed does not
outlive a failed resume: at expiry the detector publishes a no-agent `Unknown`
update, which withdraws it." Same promise in the doc of `withhold_agent_absence`
("once it expires, after which a pane whose resume never produced the agent
reports the absence as usual and the seed goes").

**What happens.** `DetectorState::new` starts with `state: AgentState::Idle`,
while `reset()` and every agent change use `Unknown`. During the hold every read
tick goes read -> `detection_content` (records
`last_screen_scan_detection_content_seq`) -> `withhold_agent_absence` ->
`continue`, so `state` stays `Idle` and the agent stays `None`.
`should_skip_idle_screen_scan` treats `Idle` as stable for any agent, `None`
included. Once the resume command has failed and the shell has printed its
prompt (well inside the 30 s hold), the screen stops changing, every later tick
is skipped at `should_read_screen`, and `withhold_agent_absence` is never reached
again. Expiry runs only after the next PTY byte, resize or clear. Until the user
touches the pane, the sidebar shows an idle agent on a plain shell, and the
seed's persisted session stays too. No test covers expiry; the hold tests call
`withhold_agent_absence` directly.

**Fix direction.** Start the detector in `Unknown`, as `reset` does, so a
no-agent pane is never "stable Idle"; or check hold expiry before the read-skip
decision. Better: make the skip decision a function of the last published
detection, not of an initial placeholder.

## AGENT-002 - Under full-lifecycle hook authority, agent loss is only seen if the foreground group changes

Scope: agent-detection.

**Claims broken.** `PROCESS_RECHECK_IDENTIFIED` ("Recheck cadence for an already
identified process") and `AGENT_MISS_CONFIRMATION_ATTEMPTS` (misses are
confirmed, then the agent is dropped); the sidebar's promise to show every
agent's state.

**What happens.**

- `ProcessProbeScheduler::schedule` returns `Skip` via
  `lifecycle_authority_can_skip` whenever authority is active, a foreground group
  is observed, a probe has happened and the group has not changed. That return
  comes before the `elapsed_since_check >= PROCESS_RECHECK_IDENTIFIED` safety
  check, so an identified agent is never rechecked on a timer. The only test of
  the safety probe under authority
  (`scheduler_keeps_identified_safety_probes_without_a_foreground_group`) covers
  the no-foreground-group case.
- `may_scan_screen` refuses screen scans under authority unless
  `process_exited`.
- `set_detected_state_with_screen_signals_at`: while a live full-lifecycle
  authority holds and the report is not `process_exited`, a detector report of
  `agent: None` is ignored, because
  `hook_authority_conflicts_with_detected_agent(None)` is false. `detected_agent`
  stays set and authority stays live.

An agent that exits without the foreground group changing is never noticed: pi
under a wrapper script or `bash -c 'pi; ...'` that outlives it (both share the
script's pgid). An agent replaced by a non-shell program changes the group once,
one miss is counted, and every later probe is skipped, so the 6-miss
confirmation never completes. The pane keeps the last hook state indefinitely; if
the agent crashed mid-turn that is `Working`, and nothing withdraws it.

Related path: `set_full_lifecycle_authority_active(true)` triggers
`DetectorState::reset()`, which drops the detector's agent. If the single
re-probe right after fails to identify the agent (argv unreadable because the
leader is in `D` state and the agent is only identifiable from argv),
`has_probe` is now true and authority skips every later probe, so the detector
holds `agent = None` for good. When the agent later exits to the shell,
`foreground_shell_agent_action` sees `previous_agent = None` and never reports a
process exit.

**Fix direction.** Keep the `PROCESS_RECHECK_IDENTIFIED` probe under authority
(one probe per 5 s, the only safety net). Do not let the app ignore an
agent-absent report that has passed miss confirmation. Do not clear the
detector's agent on an authority reset, or re-probe until it is reacquired.

## AGENT-003 - The Working -> Idle debounce ignores presentation: `Unknown` flips publish at once

Scope: agent-detection. Related: AGENT-027 (the same presentation rule on the
sort seq).

**Claims broken.** `AgentDetection::visible_idle`: "The pane's detection loop uses
it to publish a Working -> Idle change at once instead of waiting for the idle to
be confirmed over several ticks." `AGENT_PENDING_IDLE_CONFIRMATIONS`: "filtering
a single transient frame". AGENTS.md: "Unknown presents as Idle."

**What happens.** `PendingIdleConfirmation::should_hold_working_to_idle` holds
only when `next.state == AgentState::Idle`, so a `Working -> Unknown` flip is not
held and publishes immediately. Two manifests produce `Unknown` routinely:

- Codex: no match falls back to `Unknown` (`fallback_state`).
- Letta: `composer_input`, `profile_selector` and the priority-0 catch-all
  `no_live_state_evidence` all yield `Unknown`.

For these, a single transient frame during a turn (a status line redrawn in
place, a partial frame) shows as Working -> Idle -> Working in the sidebar. For
Codex this matters when no hook authority exists: before the first
`UserPromptSubmit`, when python3 is missing, or after authority is cleared.

**Fix direction.** Key the hold on `presentation_state()`: hold any Working ->
presented-Idle transition that lacks `visible_idle`.

## AGENT-004 - `detect capture` does not capture what the detector evaluates, so an offline explain disagrees with the live one

Scope: agent-detection.

**Claims broken.** AGENTS.md: "`shepr detect capture <pane>` prints the text the
detector evaluates for a pane", and the manifest workflow ("capture the pane ...
encode invariant controls"). The `src/cli/detect.rs` module doc says the same.

**What happens.** The detector evaluates `(screen, osc_title, osc_progress)`.
`handle_detect_capture` returns only `detection_text()`. `detect explain <pane>`
reads all three (`handle_detect_explain`), but `detect explain --file` goes
through `explain_for_label` -> `explain()`, which hard-codes empty OSC strings.
Codex, Claude, Amp, Grok, Kiro, Qwen and Letta all have top-priority OSC rules,
so explaining a capture offline can select a different rule and state than the
live explain (Codex `Action Required` in the title, a Grok title spinner,
Claude's title spinner with its dialog-aware `not` gate), and the maintainer
cannot reproduce the live decision from a capture.

**Fix direction.** Have capture emit the OSC title and progress alongside the
screen in a format `--file` reads back, and have `explain --file` feed them to
`explain_with_input`.

## AGENT-005 - Agent-specific detection policy is hard-coded outside the manifests

Scope: agent-detection.

`fallback_state` hard-codes `Agent::Codex` as the only agent whose no-match
result is `Unknown`. `should_skip_idle_screen_scan` hard-codes `Codex` (plus "no
screen manifest") as the only agents whose `Unknown` is stable. Letta's manifest
ends in a catch-all `Unknown` rule, so a Letta pane on its composer or the
catch-all never qualifies for the idle skip and copies and evaluates the full
screen every 300 ms while nothing changes: the hot path AGENTS.md says
multiplies per pane.

`should_skip_idle_screen_scan` also asks `agent.screen_manifest()` (the
descriptor flag), while detection uses the compiled manifest. If a bundled
manifest failed to compile (logged, then `None`), detection returns `Unknown`
forever and the skip logic treats that as transient, reading every tick.
`has_screen_manifest`, which answers the right question, has no production
caller, and its doc names consumers ("consumers that wait for a screen-derived
`Idle`") that do not exist.

**Fix direction.** Make the no-match fallback a manifest field
(`fallback = "unknown"`) and derive "Unknown is stable" from the compiled
manifest, removing both `Codex` special cases.

## AGENT-006 - Hot-path waste in the detection tick

Scope: agent-detection.

- `DetectorState::detection_content`, for an identified agent, compares the new
  screen text with `last_detection_text` and then `clone_from`s it every scan.
  The `changed` flag is only consumed by `ProcessProbeScheduler::content_changed`,
  which returns immediately when `agent.is_some()`, so every agent pane pays a
  full-screen string compare and copy per tick for nothing.
- The tick locks the terminal core three times (`detection_text`,
  `agent_osc_title`, `agent_osc_progress`) and allocates three Strings; one
  locked read returning all three would do.
- Detection is a pure function of `(screen, osc_title, osc_progress)`, and
  `detection_content_seq` covers all three (OSC values arrive as bytes; flushes
  and resizes bump the sequence). Re-evaluating an unchanged input in `Working`
  or `Blocked` can only return the same result. The only tick-driven needs (the
  pending-idle confirmations and the stable-blocker refresh) could reuse the last
  `AgentDetection` instead of re-reading. The "only stable states may skip" rule
  in `should_skip_idle_screen_scan` is broader than necessary.

## AGENT-007 - Stale or inaccurate documentation in detection

Scope: agent-detection.

- `AgentDetection::visible_working`: "forwards it only together with
  `state == Working`". It is not forwarded at all: `AppEvent::StateChanged` and
  `StateChangedUpdate` have no working flag; only `visible_blocker` crosses.
- `AGENT_PENDING_IDLE_CONFIRMATIONS = 3` is documented as "Matching idle
  observations needed before publishing idle". The hold publishes on the fourth
  matching observation: the first starts the hold and three confirmations follow
  (`pending_idle_holds_working_to_plain_idle_until_confirmed` asserts four
  calls).
- `may_scan_screen`: when the startup grace has just expired, the branch clears
  it but still returns `false`, so the first scan waits one extra tick after
  `AGENT_STARTUP_GRACE_WINDOW`. Harmless, but not what "grace window" says.
- `RegionSpec::AfterCurrentPromptBlockMarker` returns a slice starting at the
  marker line, which is not "after" it. No manifest uses it (AGENT-008).

## AGENT-008 - Dead code in detection

Scope: agent-detection.

- `ForegroundProcess::argv0` is never populated (`foreground_job_from_members`
  and `foreground_group_leader_job` both set `None`), yet
  `normalized_process_name` reads it first.
- Region kinds with no bundled user: `current_prompt_block_marker`,
  `after_current_prompt_block_marker`, `above_prompt_box` and `bottom_lines(N)`.
  With no local overrides (a detection change ships as a new build), these are
  unreachable.
- `IdleScreenScanSkipInput` and `DetectionScreenReadInput` are field-for-field
  identical; `decide_detection_screen_read` only rewraps one into the other.
  `DetectionTransitionDecision` and `DetectionPublishDecision` repeat the split.
- `has_screen_manifest` is test-only (AGENT-005).

## AGENT-009 - Restored history is read as live agent chrome

Scope: agent-detection. The hunter flags this as a risk, not reproduced.

`seed_history_ansi` writes saved history into the fresh terminal, so the previous
session's last frame sits on the live screen, and `detection_text` reads live
screen rows. A resumed agent that draws inline (not on the alternate screen)
leaves that old frame above its output, inside `whole_recent` and other broad
regions. Several blocker rules read `whole_recent` with `visible_blocker = true`
(Claude's `bash_permission_prompt` and `legacy_no_prompt_blocker`, Amp's
`approval_footer`, Cursor's `approval_prompt`, Grok's `option_dialog_blocked`).
Codex's `screen_working_fallback` accepts arbitrary non-marker lines after a
timer line up to the end of the region. A saved frame that ended on a dialog or a
live timer can therefore classify the resumed agent as Blocked or Working until
enough output scrolls it off. The 3 s startup grace only delays the first scan.
The hunter suggests one capture test with a restored Codex or Claude frame.

## AGENT-010 - Identification ranking prefers a wrapped child over a real agent executable

Scope: agent-detection. The hunter flags this as a smell; no doc promises
otherwise.

In `identify_agent_in_job`, `ProcessPriority::NormalizedAlias` ranks above
`AgentExecutable`. When the group leader is unrecognised (an `npx`/npm leader or
a wrapper script), a node child whose argv names a different agent outranks a
process whose own name is an agent: for example an MCP server shipped as
`.../bin/codex`, while the other process is named `claude`. The alias rank exists
for Nix `.x-wrapped` and node-wrapped agents but applies to every job member.

## AGENT-011 - The per-pane detection state machine is spread over three files

Scope: agent-detection (structural note).

The state machine lives in pure deciders with single-use input structs in
`agent_detection.rs`, in `DetectorState` in `process_probe.rs`, and in the
orchestration loop in `runtime.rs`, which interleaves `spawn_blocking` probes,
OSC clearing, theme restore and publishing. AGENT-001, AGENT-002 and AGENT-003
are all interaction bugs between these pieces: an initial state in one file, a
skip rule in another, an early `continue` in the third. A single pure
`DetectorState::tick(Observations) -> TickOutput` (observations: foreground pgid,
probe result, content seq, screen and OSC, authority flag, `now`; output: events
to publish and the next wake) would put every transition in one testable
function and leave the runtime as I/O only.

## AGENT-012 - The OpenCode server plugin (run and --mini) can never anchor, so all its reports are ignored

Scope: agent-integration.

**Claim broken.** The asset says "V1 local run/Mini retain their server hooks"
(`assets/opencode/shepr-agent-state.js`, default export comment). The descriptor
gives OpenCode `full_lifecycle_hook_authority: true`, and AGENTS.md says the
integrations "report state and session IDs back to shepr".

Background (the hunter's main structural observation): the server grants hook
authority to a full-lifecycle source (pi, omp, mastracode, opencode, kimi, kilo)
only once it is anchored, and a fresh pane can only be anchored by a
`pane.report_agent_session` carrying a seq and a recognized
`session_start_source` (the OpenCode TUI's unsequenced `select` report is the one
exception). Until then every state report is parked as a pending replacement and
ignored.

- `reportSession(sessionID)` sends `pane.report_agent_session` with a seq and no
  `session_start_source`. In
  `TerminalState::set_agent_session_ref_for_typed_start_source_at`, a
  full-lifecycle source not yet anchored (`!session_anchored`) reaches
  `if !Self::session_start_source_is_recognized(..) { return None; }`, dropping
  the report. It is not the unsequenced `select` path either, since it carries a
  seq and no `Select` source.
- Every `reportState(...)` goes to `route_full_lifecycle_hook_report`. Nothing is
  anchored, so it is parked in `pending_replacement_report` and returns `Ignore`.
  A parked report is only promoted in
  `clear_full_lifecycle_hook_suppression_for_detected_agent` when
  `replacement_session_ref` is set, and only a session report sets that.
- The `session.created` / `session.updated` branches read
  `properties.sessionID`, but these events carry the session under
  `properties.info` (shepr's own V1 TUI plugin reads
  `data.sessionID ?? data.info?.id` for them). If so, `session.created` sets
  `reportedRootSessionID = undefined` and `session.updated` never reports.

Net effect: in `opencode run` or `--mini` panes the hook integration is inert and
resume never gets an OpenCode session id from this path; it only looks as if it
works because screen detection carries the state. The hunter could not verify the
exact OpenCode event payloads from the repo; this assumes `{ info }` for
created/updated, as OpenCode defines them.

**Fix direction.** Send `session_start_source` (for example `startup`) on the
first root session, read `info.id` for created/updated, and cover it with the
harness in AGENT-024.

## AGENT-013 - The Kilo plugin reads a field its own comment says does not exist, so it never reports a session

Scope: agent-integration.

**Claim broken.** `reportSession` in `assets/kilo/shepr-agent-state.js` says
"`session.created`/`session.updated` carry only the session info". The handler
calls `reportSession(sessionID)` for exactly those events with
`sessionID = sessionIDFromProperties(properties)`, which reads
`properties.sessionID`, while reading `properties.info` a few lines earlier to
track child sessions.

- If the comment is right (it matches OpenCode's `Session.Event.Created` /
  `Updated`, which are `{ info }`), `reportSession(undefined)` is a no-op. Kilo's
  only other session report is `session.status` with a status not in
  `SESSION_STATE_BY_STATUS`; OpenCode's status types (`idle`, `busy`, `retry`) are
  all mapped, so that path never fires.
- Kilo is full-lifecycle, so with no session report none of its state reports is
  accepted (the parking in AGENT-012). Kilo has `resume_support`, but
  `persisted_agent_session` is never set for a Kilo pane, so Kilo panes never
  resume on restore.
- The TS fixture in `assets/opencode/shepr-agent-state.test.ts` builds
  `session.updated` as `{ properties: { sessionID } }` while its
  `session.created` child fixture uses `{ properties: { info } }`. The tests
  encode the unverified shape.

Verify the Kilo event schema once; if it is `{ info }`, use `info.id` (skipping
children, as the code already does).

## AGENT-014 - A state report without a session ref erases the resume identity, then freezes hook authority

Scope: agent-integration.

**Claims broken.** Resume on restore (AGENTS.md "Session restore ... and agent
resume on restore") and the asset comments that the full-lifecycle plugins are
the pane's authority.

- In `set_hook_authority_at`, an accepted report always does
  `self.persisted_agent_session = None` and stores `session_ref` from the report.
  `route_full_lifecycle_hook_report` treats an incoming `None` as anchored
  (`is_none_or`), so a session-less report sets `hook_authority.session_ref =
  None` and clears the persisted session: the pane loses its resume identity.
  Intended per `accepted_hook_report_without_session_ref_clears_previous_ref`.
- The next report has nothing to anchor against. A session-less report hits
  `let Some(session_ref) = session_ref.clone() else { return Ignore }`; a
  session-bearing report is parked. Hook authority stays frozen at that one
  session-less report's state until the next session report with a recognized
  start source. Kimi and MastraCode send one only on SessionStart; Kilo and
  OpenCode-run in practice never do (AGENT-012, AGENT-013).
- Assets that emit session-less state reports:
  - `session.error` in the opencode and kilo plugins calls
    `reportState("blocked", sessionID)`. OpenCode's error event has an optional
    `sessionID`, so a global error (provider or auth failure) produces a
    `blocked` report with no session, pinning the pane at Blocked.
  - The kimi and mastracode hooks send state reports without `agent_session_id`
    whenever the payload lacks `session_id`.
  - The pi and omp plugins (`withSessionRef`) do the same when the session
    manager has no file or id (for example a no-session run).

**Fix direction.** Decide the contract in one place: either assets never send a
full-lifecycle state report without a session ref, or the server keeps the
anchored ref when the incoming report has none. The current combination is the
worst of both.

## AGENT-015 - Kimi and Kilo have no session-replacement rule, so an in-process session switch freezes the pane

Scope: agent-integration.

**Claim broken.** The Kimi asset says it passes the start source through ("shepr
ignores values it does not know"); the integration is meant to report the current
session and state.

- `session_report_allows_session_replacement` has arms for Claude, Codex,
  MastraCode, OpenCode, Pi, Grok, OMP and Antigravity, none for Kimi or Kilo.
- After a Kimi session change in the same process (a new SessionStart with a new
  `session_id`): the session report is refused
  (`replaced_hook_session.is_some() && !session_replacement_allowed`); every
  later state report carries the new id, is not anchored and is parked; hook
  authority stays at the old session's last state, and resume targets the old
  session.
- The Kilo comment says this is deliberate for Kilo ("neither lets Kilo replace a
  session"); the frozen state that follows is not called out anywhere.

Depends on Kimi switching sessions in-process (`/clear`, `/new`, a resume
picker), which the hunter could not verify. Then either add an arm or have the
server release authority on a refused replacement.

## AGENT-016 - "Installs or updates" means updates only when a version constant is bumped

Scope: agent-integration.

**Claim broken.** AGENTS.md: the server "installs or updates them at launch".

- `integration_state_for_path` compares the `SHEPR_INTEGRATION_VERSION` marker in
  the installed file against the spec's constant with `>=`; the asset bytes are
  never compared. An asset edited without bumping its constant (nothing enforces
  the bump: `bundled_integration_assets_match_expected_versions` only checks that
  the marker equals the constant) stays old on every host forever. Same for a
  config registration whose shape changed without a bump where the status check
  does not look at the changed part (for example hook timeout values in the
  Codex, Devin, Droid or MastraCode entries).
- Because of `>=`, a file written by a build with a higher constant is "Current"
  for a build with a lower one. Dev and release builds share agent config dirs
  (only runtime and data dirs are per-profile), so whichever build bumped last
  owns the hook files for both.
- The assets are `include_str!` constants, so comparing installed bytes with
  bundled bytes is cheap, and it lets the per-target version constants and
  markers go entirely.

## AGENT-017 - The Kimi config edit can produce TOML Kimi cannot parse, and install reports success

Scope: agent-integration.

**Claim broken.** The install-order comment at the top of `targets.rs`: "A config
that cannot be edited then fails the install before anything is written".

- `build_kimi_config_with_hooks` strips shepr's marked block and appends
  `[[hooks]]` tables as text, never parsing input or output. A user config that
  already defines `hooks` as an inline array (`hooks = [...]`) or a `[hooks]`
  table gets a conflicting redefinition: invalid TOML, and Kimi refuses to start.
  An already-invalid config is silently accepted.
- `kimi_hooks_registered` then cannot parse the file, so the status is Outdated
  on every launch; each launch re-runs the `kimi --version` probe (up to 5 s) and
  rebuilds the same broken text, which is unchanged, so not rewritten, and no
  error is logged.
- The Claude editor re-parses with `verify_updated` and the Codex editor goes
  through `toml_edit`. Kimi should use `toml_edit` too, or at least parse the
  result and fail when it does not parse.

## AGENT-018 - The Devin hook can attribute another pane's session, and runs a subprocess on every tool call

Scope: agent-integration.

**Claim broken.** The report is for this pane (`SHEPR_PANE_ID`), and the resume
that follows should target this pane's conversation.

- `resolve_session_id` in `assets/devin/shepr-agent-state.sh` falls back to
  `devin list --format json` whenever the payload has no session id (every event
  except UserPromptSubmit and a `startup` SessionStart) and takes the first entry
  whose `working_directory` equals the project dir. With two Devin panes in one
  repository it reports whichever session the list shows first. Devin is not
  full-lifecycle, and the first accepted session for a pane is kept
  (`conflicting_same_owner_session_ref`), so a pane can hold another pane's
  conversation, and restore resumes the wrong one, or it is deduplicated away
  against the other pane.
- Every Devin event is registered, PreToolUse and PostToolUse included, and each
  starts python and possibly a `devin list` (2 s timeout) inside a synchronous
  hook: latency on every tool call for a session-identity-only report that
  changes nothing after the first.

Register SessionStart (and maybe UserPromptSubmit) only, and drop the
cwd-matching fallback or restrict it to a unique match.

## AGENT-019 - The Pi reporter does not serialize session and state reports

Scope: agent-integration.

- OMP routes every request through `requestQueue`. Pi's `reportSession` calls
  `sendRequest` directly, while states go through `drainStateQueue`. On
  `agent_start`, `void reportSession()` (seq N) and the `working` state (seq N+1)
  go out on two concurrent connections; if N+1 lands first, N is dropped as a
  straggler (`hook_seq_superseded`).
- `sendRequest` retries with the same seq. If the first attempt timed out but was
  delivered, the retry is dropped (harmless); if a newer report landed in
  between, the retried report is lost.
- Small impact today, since the state report carries the session ref too. Pi is
  the one JS reporter without an ordered queue; share OMP's.

## AGENT-020 - Hook seq ordering and the arbitration's clocks rest on assumptions that do not hold

Scopes: agent-integration, mux-terminal (its finding 12, filed there as a note
rather than a proven defect), agent-detection (structural note).

- **Stamp timing.** The kimi and mastracode hooks stamp `date +%s%N` at hook
  start, and their comments explain that stamping after python start lets startup
  jitter reorder near-simultaneous events. The codex hook, which also sends
  `working` and `idle`, stamps `time.time_ns()` after `cat` and python start; if
  Codex ever fires hooks concurrently, the same reordering applies.
- **Re-anchor rule.** `report_seq_superseded` accepts a non-increasing seq as a
  clock step when it arrives `HOOK_SEQUENCE_REANCHOR_AFTER` (5 s) or more after
  the last acceptance. The hooks' budget is `HOOK_TIMEOUT` (10 s); each python
  socket op may take 0.5 s (connect, send and recv each); the Devin list call
  alone is up to 2 s, and the API request waits up to `ORDINARY_REQUEST_TIMEOUT`.
  A report delayed but still within budget can arrive more than 5 s late, be
  taken as a clock step, and overwrite a newer state; a straggler and a clock
  step look the same to this rule. The mux-terminal hunter notes the rule is
  documented as intended, and that since the seqs are wall-clock nanoseconds
  (`time.time_ns()` in every shipped hook), the server could judge a straggler
  against its own wall clock instead of "5 s since the last acceptance".
- **Clock mixing.** Hook reports are stamped with the loop's per-iteration
  `clock_now`, sampled before the drain that handles them; detection events carry
  the runtime's tick `now`, sampled before its `/proc` probe and screen read.
  Every "newer than" decision (`newer_custom_authority`,
  `hook_authority_not_newer_than`, `fallback_not_older_than_hook`,
  `detected_state_observed_before_release_suppression`) compares the two. The
  windows are milliseconds wide, and the headless loop drains queued internal
  events before API requests, which removes the worst ordering; it is still an
  implicit, undocumented contract.

## AGENT-021 - Hook traps ignore SIGTERM

Scope: agent-integration.

The `set -eu` hooks install `trap 'rm -f "$hook_input_file"' EXIT HUP INT TERM`.
A trap on HUP/INT/TERM that does not `exit` resumes the script, so an agent that
kills a timed-out hook with SIGTERM gets a hook that deletes its input file and
carries on into python anyway. Use `trap '...; exit 0' HUP INT TERM`, or trap
EXIT only.

## AGENT-022 - Hooks are registered under bash although every sh asset is POSIX sh

Scope: agent-integration.

Every sh asset has a `#!/bin/sh` shebang and is POSIX sh, and install makes it
executable (0755), yet `hook_command` registers `bash '<path>'` for every target
except Grok. The Grok comment in `targets.rs` ("a POSIX `sh` script, so it runs
under `sh` rather than the `bash` the shared command formatter uses for the other
hooks") implies the others need bash; they do not. On a host without bash every
hook fails silently while install and status report Current. Use one interpreter
(`sh`), or invoke the executable path directly.

## AGENT-023 - Smaller integration contract drift

Scope: agent-integration.

- `AgentIntegrationPaths` doc: "Install and status code ... never consults the
  process environment while it is choosing files to read or write".
  `config_update_lock_path` reads `XDG_STATE_HOME` / `HOME` live, which the
  comment on `absolute_xdg_home` concedes. Capture them in
  `AgentIntegrationPaths` or reword the doc.
- The opencode server plugin and the kilo plugin map every `session.error` to
  `blocked`, including `MessageAbortedError` (the user pressing Esc). The V1 TUI
  plugin, reporting under the same `shepr:opencode` source, excludes aborts: one
  source, two meanings.
- The codex hook refuses a session report without `transcript_path` but never
  sends the path; the requirement gates nothing the server uses.
- `settle(false)` in the opencode and kilo `requestOnce` passes an argument
  `settle` ignores (copied from the TUI plugin, where it matters).
- `action_label` is a second spelling of the target label used only in log lines
  ("antigravity-cli" vs "agy"). `integration_target_label`,
  `mastracode_hook_command` and `antigravity_cli_hook_command` are pass-through
  wrappers.

## AGENT-024 - Nothing checks the integration assets against the server's acceptance contract

Scope: agent-integration (the hunter's main structural finding).

The TS tests assert which requests an asset emits, and the Rust state tests
anchor sessions by calling `set_persisted_agent_session` directly, so no test
feeds an asset's real request sequence into `TerminalState`. AGENT-012 through
AGENT-015 all fall through that gap.

Also, 14 assets each re-implement the envelope, socket, timeout and JSON code,
and the regex test `hook_assets_share_one_envelope` exists only to keep them in
line.

**Direction.** One cross-boundary harness that runs each asset (sh via a fake
socket, JS/TS via the existing bun tests) through a scripted agent session and
replays the captured JSON requests into a `TerminalState`, asserting on the
resulting hook authority and persisted session rather than request shapes. Pair
it with a server-side or explicit asset-side rule for a state report with no
session ref (AGENT-014). Generating the sh assets from one template at build time
would remove most of the duplicated surface and keep install byte-comparable
(AGENT-016). A hidden `shepr` reporter subcommand would do it more simply but
widens the CLI, which AGENTS.md keeps small on purpose; the hunter leaves that
choice to the owner.

The hunter lists as not verified, and not reported as defects: Copilot's hook
event spelling (`SessionStart` here, camelCase in Copilot's own hooks format) and
settings file name; whether Codex has an `Interrupt` hook event; OpenCode's
global plugin directory name (`plugins/` for OpenCode vs `plugin/` for Kilo).

## AGENT-025 - One stray different-session report freezes a live full-lifecycle agent

Scope: mux-terminal. Hunter's confidence: high (traced by hand; no test covers
the follow-up report).

**Claim broken.** `terminal/state/mod.rs` header: "Full lifecycle Shepr hook
integrations are hook-authoritative while live". A live pi, omp, kimi, kilo or
mastracode pane stops following its own hooks.

In `route_full_lifecycle_hook_report` (`hooks.rs`):

- Live authority for `shepr:pi` with session S1, pi process present.
- A `pane.report_agent` for `shepr:pi` arrives with a different session S2 and a
  seq (a child or sibling pi that inherited `SHEPR_PANE_ID`, or the "unexpected
  session" case `pi_non_replacement_reports_preserve_full_lifecycle_authority`
  already builds). `session_anchored` is false, so it falls to the "pending
  replacement" tail, which runs
  `suppressed_full_lifecycle_hook_reports.entry(source).or_insert_with(..)` with
  reason `ProcessExit`, stores S2 as pending, and returns `Ignore`.
- The next report for the live S1 fails the accept gate
  `process_present && session_anchored && !suppressed.contains_key(source)`
  because the entry exists; it falls to the same tail, replaces the pending
  report and is ignored, as is every later S1 report.
- The entry is only removed by
  `clear_full_lifecycle_hook_suppression_for_detected_agent` (when the detected
  agent changes) or the sequenced session-start branch in `sessions.rs`; neither
  happens while the same pi keeps running. Detection cannot reach that function
  either: `should_ignore_detected_state_under_full_lifecycle_hook` returns early
  while authority is live, and the pane runtime has stopped scanning the screen
  (`may_scan_screen` is false while `full_lifecycle_authority_active` is set).

The sidebar shows the last accepted state (often Working) until the agent exits
or starts a new session. Only opencode is protected, by the `opencode_cross_talk`
early `Ignore`, which returns before the suppression entry is inserted.

**Test sketch.** Take
`live_full_lifecycle_hook_rejects_different_session_ref_for_same_source`, then
send one more `shepr:pi` report for `one.jsonl` with seq 22 and state Idle.
Expect `Some(..)` and `state == Idle`; per the trace it returns `None` and the
state stays Working.

**Fix direction.** While a process is present and the source's authority is
anchored to a different session, a mismatching report is cross-talk: ignore it
(as opencode's is) and do not open a replacement generation. Open one only on
process-exit evidence or a sequenced session-start report.

## AGENT-026 - A suspended agent (Ctrl-Z) is treated as exited and loses its resume session for good

Scope: mux-terminal. Hunter's confidence: high on the TerminalState side; the
pane side is traced through `foreground_shell_agent_action`, which returns
`ReportProcessExit` whenever the pane shell is back in the foreground.

**Claims broken.** "Session restore ... and agent resume on restore" (AGENTS.md
Scope) and the hook-authority claim in AGENT-025.

When the agent is suspended the shell becomes the foreground job, so the detector
publishes `StateChanged { process_exited: true }`, and
`set_detected_state_with_screen_signals_at` (`detection.rs`) sets
`persisted_agent_session = None` for that agent, clears hook authority, and
inserts a `ProcessExit` suppression holding the session ref. On `fg` the probe
reports `ReportReplacementProcess` and `AgentProcessDetected` arrives;
`clear_full_lifecycle_hook_suppression_for_detected_agent` keeps the
`ProcessExit` entry because it has no `replacement_session_ref`. Same process,
same session, no new SessionStart:

- Claude: resume is gone. Claude's hook only sends SessionStart, so the pane has
  no persisted session until `/clear` or a restart; a server restart brings back
  a bare shell.
- Codex: comes back on its next turn report, which carries the session id.
- pi, omp, kimi, kilo, mastracode: every report for the old session falls to the
  pending tail (AGENT-025; nothing is anchored any more) and is ignored. Hook
  authority and the resumable session do not come back. omp and mastracode have
  `screen_manifest: false`, so there is no screen fallback either; they show the
  process-exit fallback (Idle) for the rest of the process's life.

TerminalState cannot tell a suspend from an exit because `process_exited` is a
bool. Either the probe tells them apart (the job's processes in state `T` in
`/proc/<pid>/stat`), or TerminalState gets a third outcome ("suspended": drop
live authority but keep the session and generation).

## AGENT-027 - Idle and Unknown flips reorder the sidebar although both present as Idle

Scope: mux-terminal. Hunter's confidence: high. Related: AGENT-003.

**Claim broken.** "Unknown presents as Idle" (AGENTS.md). The comment on
`record_agent_state_change_seq` says the seq exists "so endpoint agent sorting
can observe transitions".

`record_agent_state_change_seq` (`app/actions/events.rs`) compares raw
`AgentState`, so a change between Idle and Unknown bumps
`last_agent_state_change_seq`. The client uses that seq as the secondary sort key
under Priority sort (`agent_sidebar.rs`) and as the recency change trigger
(`shell/endpoints.rs`). A screen whose rules stop matching for a moment
(`manifest.rs` returns `Unknown` when no rule fires) moves the row to the top
with no visible state change; so does process re-detection
(`set_detected_agent_process_at` resets the fallback to Unknown). Compare
`presentation_state()` instead.

## AGENT-028 - `detect explain` credits the screen for a state that a hook decided

Scope: mux-terminal. Hunter's confidence: high.

**Claim broken.** AGENTS.md: "`shepr detect explain <pane>` says which rule
decided its state".

`handle_detect_explain` (`app/api/detect.rs`) special-cases only
`full_lifecycle_hook_authority_active()`. For any other effective hook authority
it evaluates the screen rules and returns them as the explanation, although
`terminal.state` comes from `hook_authority.state` (`recompute_effective_state`)
unless a visible blocker overrides it. That covers shepr's own Codex turn hooks
(`shepr:codex` is neither full-lifecycle nor reserved) and any custom source.
With Codex the output routinely contradicts the sidebar: the hook says Working,
the screen rule says Idle. The explain should name the hook source when
`hook_authority` is effective and no visible blocker applies.

## AGENT-029 - Custom hook reports are silently refused whenever the pane has a session identity

Scope: mux-terminal. Hunter's confidence: high (traced).

**Claim broken.** `warn_unrecognized_hook_identity`: "Custom reports remain
usable".

In `set_hook_authority_at`, `current_session_owner_conflicts` returns true for
any custom source once `current_session_identity_for_persistence()` is `Some`:
`AgentSource::parse(custom)` is `Custom`, which never equals the stored official
source. Takeover then needs `session_ref`, and `session_ref_from_report` always
returns `None` for a custom source, so the report is dropped. Any Claude, Codex
or other official session recorded on the pane disables every custom state report
for it. An unparseable custom label (for example `"myagent"`) hits the same wall
through the `parse_canonical_label` failure branch, which returns `true`. Either
apply the owner check only to reports that carry a session identity, or drop the
warning's promise.

## AGENT-030 - Mutating TerminalState paths that return `None` leave the caller unaware

Scope: mux-terminal. Hunter's confidence: medium; the paths are real, the
reachable impact small today.

`update_terminal_state` treats `None` as "nothing changed" (no dirty mark, no
state-change seq). Several paths mutate and then return `None`:

- `set_agent_session_ref_for_typed_start_source_at` (`sessions.rs`) can clear
  `hook_authority` (the Codex replacement branch, `replaced_hook_session`, the
  foreground takeover) and remember stale sessions, and only then run
  `PersistedAgentSession::from_report(..)?`. If `from_report` fails, the
  effective state has changed but the caller hears `None`. Latent today, since
  the API only builds refs through `session_ref_from_report`, which respects the
  agent's policy: an invariant held in a different crate.
- Both entry points record the report's seq (`accept_hook_report_at`) before the
  known-agent, owner and conflicting-session rejections, so a rejected report
  still advances the source's ordering: a partial mutation on a rejected input.
- `abandon_agent_resume` discards its own mutation (`let _ =`); the label
  disappears without `record_agent_state_change_seq`. Given AGENT-027, skipping
  the bump happens to be right here.

**Structural fix.** Each entry point validates first and mutates second, so a
`None` means untouched.

## AGENT-031 - The "full-lifecycle only" invariant on the suppression maps is not held

Scope: mux-terminal. Hunter's confidence: high; low impact today.

The field docs on `suppressed_full_lifecycle_hook_reports` and
`stale_full_lifecycle_hook_sessions` say only full-lifecycle source and label
pairs can enter them. The process-exit branch of
`set_detected_state_with_screen_signals_at` builds `official_session` with
`is_official_agent_source` (any official pair) and inserts a `ProcessExit`
suppression for Claude, Codex, Cursor and so on. `set_hook_authority_at` (the
`session_ref.is_some()` removal) then moves those entries into the stale-session
map. The `ProcessExit` entry is never cleared for those agents
(`clear_full_lifecycle_hook_suppression_for_detected_agent` keeps it, having no
replacement), and meanwhile it gates
`detected_state_observed_before_release_suppression` and occupies a protected
sequence slot. Either filter with `full_lifecycle_hook_authority` (matches
intent) or fix the docs.

## AGENT-032 - Dead or misleading data on the hook path

Scope: mux-terminal. Hunter's confidence: high; each states something untrue.
Related: TERM-006.

- `HookAuthority.message` is accepted from `pane.report_agent`, stored, cloned
  into pending reports, and never read.
- `HookAuthority` derives `Serialize` and `Deserialize`, with a comment about
  "Decoding needs a fresh local observation time", but is never serialized or
  decoded. The derives and `#[serde(skip, default = "Instant::now")]` are dead,
  and the comment describes a path that does not exist.
- `EffectiveStateChange` computes and allocates two labels and two known agents on
  every mutation; the only reader (`record_agent_state_change_seq`) uses
  `previous_state` and `state`. `unchanged_effective_state_change()` builds a full
  struct whose only use is `previous_state == state`, so the seq code returns
  immediately, just to make `update_terminal_state` return `Released`. A `bool`
  would carry the same.
- `TerminalReadSnapshot` and `truncated` (`read_snapshot.rs`) plus the four
  `recent_*_snapshot` methods have no production reader apart from history
  persistence, which ignores `truncated`: leftovers of the pane reads the project
  removed on purpose.

## AGENT-033 - Two definitions of "the session to persist"

Scope: mux-terminal. Hunter's confidence: medium (smell with divergence risk).

`TerminalState::current_session_identity_for_persistence` (`hooks.rs`) decides
`session_ref_changed`, and so when the session is marked dirty, using
`from_report`: `AgentSource::parse` plus the `accepted_for` check.
`persist/snapshot.rs` `capture_workspace` decides what is written, re-implementing
the logic from public fields with `AgentSource::from_pair` and no `accepted_for`
check. They agree only because `session_ref` is built solely for official pairs
with a policy-valid kind. Make the method public and have the snapshot call it,
so the value that marks a save dirty is the value the save writes.

## AGENT-034 - The TerminalState arbitration is a web of predicates and maps whose invariants live in prose

Scopes: mux-terminal (structural recommendation), agent-detection (structural
note on the merge layer).

`terminal/state/` is about 1,500 lines of arbitration over four maps
(`hook_report_sequences` and `hook_report_accepted_at`, the suppression map, the
stale-session map, plus `recent_agent_process_exit`) whose invariants live in
prose on the fields; AGENT-025, AGENT-026, AGENT-030 and AGENT-031 all break
them. Effective state is decided by interacting predicates
(`hook_authority_is_effective`,
`should_ignore_detected_state_under_full_lifecycle_hook`,
`visible_blocker_overrides_hook`, release suppression, stale-session memory)
comparing timestamps from two clocks (AGENT-020). It is string-typed at the
boundary: the API already canonicalizes labels, yet every report re-parses
`source` and `agent_label` many times (`parse_agent_label` allocates through
`normalized_agent_lookup_name`), and agent-specific rules appear as literal pairs
(`("shepr:codex", "codex")`, `("shepr:opencode", "opencode")`,
`("shepr:mastracode", "mastracode")`, `("shepr:grok", "grok")`).

The mux-terminal hunter's case for a rewrite:

- Parse once at the API edge into
  `enum ReportSource { Official(Agent), Custom { source, label } }`, and move the
  per-agent quirks into `AgentDescriptor` flags next to
  `full_lifecycle_hook_authority`.
- Model each official source as one explicit per-source generation state machine
  (`Live { session }`, `Suspended { session }`, `Exited { session, pending }`,
  `Cleared { session }`) instead of four maps whose keys must agree, giving "same
  process, different session" (AGENT-025) and "suspended, not exited"
  (AGENT-026) states of their own.
- Validate first and mutate second at every entry point (AGENT-030).

The detection hunter suggests an explicit arbitration table (inputs: detector
report, hook authority, agent kind; output: effective agent and state) over the
current predicate web. The cost is upstream tracking: `terminal/state/` is on the
upstream-watch list, and a rewrite turns future herdr fixes into manual
re-derivations; the mux-terminal hunter leaves that trade to the owner and notes
AGENT-025 through AGENT-029 can be fixed in place without it.
