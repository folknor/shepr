# Hunt: agent state

Scope read in full: every file of `crates/shepr-detect/src/` (lib, limits,
manifest and its tests, every bundled manifest, the whole `ownership` module
and its tests, `title_activity`), `crates/shepr-mux/src/pane/detect/*`,
`agent_detection.rs`, `detection_task.rs`, `agent_osc.rs`, `osc7.rs`,
`osc_debug.rs`, `crates/shepr-server/src/app/api/panes/reports.rs` and
`crates/shepr-server/src/app/agents.rs`. Followed into `shepr-agent`
(`lib.rs`, `report.rs`, `state.rs`), `pane/process_probe.rs`,
`pane/terminal.rs` and its backend/helpers (the detection snapshot), the
server's `app/events.rs`, `app/actions/events.rs`, `app/api/detect.rs`,
`app/api_helpers.rs`, `app/api/session.rs`, the API schema
(`schema/detection.rs`, `schema/panes.rs`), `persist/restore.rs`,
`shepr-vt/src/scan.rs` (progress spelling), `brokkr.toml` and `clippy.toml`.

What holds up well and needs no change: the detection snapshot really is the
bottom of the active screen and never the viewport (`terminal_detection_text`,
pinned by `detection_text_stays_at_bottom_when_viewport_is_scrolled`); the
report source gate is a closed type (`ReportOrigin::parse` over
`AgentSource`, with the API refusing anything else before dispatch); the
descriptor table and `IntegrationTarget` inverse are checked at compile time;
`bundled_manifest_source` is exhaustive; every bundled manifest is compiled
by a test.

---

## 1. Defects

### D1. `detect explain` reports the manifest verdict as the pane's state, which it often is not

AGENTS.md: "`shepr detect explain <pane>` says which rule decided its state".
In `App::handle_detect_explain`, every pane whose `state_owner()` is not a
hook goes down the screen path and returns
`DetectionExplanation::from(explain)`, whose `state` is
`explain.verdict.state()`, a fresh evaluation of the bundled manifest over
the current capture. That is not the state the pane holds or the sidebar
shows whenever any of the mux layer's gates is in play:

- the working-to-idle hold (`PendingIdleConfirmation`): the pane is still
  Working while explain says Idle and names the idle rule;
- a matched `skip_state_update` rule: the pane keeps its previous state,
  explain reports `state: unknown`;
- the startup grace (`AGENT_STARTUP_GRACE_WINDOW`) and the resume absence
  hold (`withhold_agent_absence`): nothing was published, explain shows a rule;
- `state_owner() == EffectiveStateSource::ProcessExit`: the state came from
  the exit, but explain evaluates the screen for
  `effective_agent().or(detected_agent())` and labels it
  `DetectionStateSource::Screen` (the schema has no process-exit source).

The hook path, by contrast, passes `terminal.ownership().state()`. Fix: always
report `ownership().state()` and `state_owner()` as the decided state and
source, and present the manifest evaluation as "what the screen says now"
beside it, plus which mux gate (hold, grace, skip, absence hold) is currently
withholding it. The detector already knows the gate; expose it through the
runtime. Enforceable by a test: an explain of a pane mid-hold must report the
held state.

### D2. A failed screen read is evaluated as an empty screen

`PaneTerminal::agent_detection_inputs` turns a poisoned core into
`AgentDetectionInputs::default()` and a `ReadError` from
`terminal_detection_text` into `unwrap_or_default()`. The detector then
matches an empty string, which for every manifest with the default `Idle`
fallback (most of them) publishes Idle, and for Codex and Letta publishes
Unknown. That is fabricated evidence, not "no evidence": after three ticks the
pending-idle hold lets a Working pane drop to Idle. The comment says a read
"stays silent" because the PTY actor will close the pane, but the detector
can publish in between. The tick should end without publishing
(`Option<AgentDetectionInputs>`, `ScreenTick::resume` returning a no-change
output). Enforceable by a test feeding a read failure.

### D3. `Detection::Working { visible }` claims a refresh that does not exist

The doc on `Detection::Working` says "Visible working chrome refreshes screen
evidence, but never overrides hooks", and 40-odd manifest rules set
`visible_working = true`. Nothing reads it for refresh: `publish_screen` sets
`last_visible_signal_refresh` for a visible blocker or a visible working
verdict, but the only reader, `stable_visible_signal_refresh_due`, requires
both the previous and next detection to be visible blockers. The server drops
the flag (`StateEvent::StateChanged` passes only `detection.state()` and
`visible_blocker()` to ownership). Its only observable effects are an extra
`StateChanged` event when visibility flips with the state unchanged, and a
field in `detect explain`. Either implement the documented refresh (and
decide what it is for) or delete `visible_working` from the manifest schema,
`Detection`, the API payload and every manifest. See also 9.

### D4. The API accepts a hook state no integration sends

`PaneReportAgentParams.state` is `PaneAgentState`, an alias of
`shepr_agent::AgentState`, so `pane.report_agent` accepts `"unknown"`. No
bundled hook sends it (grep of `shepr-integration/src/assets`). Accepted from
a full-lifecycle source it installs authority with state Unknown, which
pauses screen detection while presenting Idle. `IntegrationHookAction`
already models the real vocabulary (working, blocked, idle); the wire state
should be a three-variant type. Enforceable by the type.

### D5. Parked start lifetime rests on a cadence claim that is false

`PARKED_START_LIFETIME` (shepr-detect limits) justifies two minutes by "the
detector's slowest cadence (no foreground process group) rechecks only every
thirty seconds". In `ProcessProbeScheduler::schedule`, a pane with no
identified agent and a foreground group that does not change is not probed
on any timer at all once its acquisition window (8 s) is over: it probes only
on a group change, a content change that reopens acquisition, or while
lifecycle authority is active. So a parked start for a process the
acquisition window missed is promoted only if the screen happens to change
within the two minutes. The 30 s figure is `PROCESS_RECHECK_MISSING_FOREGROUND_GROUP`
in another crate; nothing ties the two (see 2.5).

### D6. `screen_unknown_is_stable` counts rules that never produce Unknown

`compile_manifest` sets `unknown_is_stable |= rule.state == Unknown`, which
includes `skip_state_update` rules (state must be unknown), though a skip
rule yields `AgentDetection::Skip`, not Unknown. So Claude (fallback Idle,
two skip rules, no Unknown rule) reads as "can report a stable Unknown". The
skip gate in `should_skip_idle_screen_scan` happens to be harmless for Skip
too (unchanged content gives the same Skip), but the field's documented
meaning ("Whether an unchanged input can produce `Unknown`") is wrong; rename
it to what it means (an unchanged screen gives the same non-publishing
answer) or exclude skip rules.

### D7. Brittle identity of the Letta and Kilo/OpenCode permission rules (manifest correctness)

Not a contract violation, but rules that the manifests' own comments promise
to hold do not:

- `opencode.toml` and `kilo.toml` say the header alone can linger, so they
  require "it AND one of the dialog's own reply controls". The controls are
  `contains = ["reject"]`, `["enter confirm"]` over `whole_recent`: a lingering
  "Permission required" header plus any later transcript text containing
  "reject" (or "rejected") is Blocked. Only "earlier text only" is tested.
- `pi.toml` `working_literal` is `contains = ["Working..."]` over the whole
  snapshot, so transcript text containing it holds Working (masked while the
  Pi hook governs).
- `claude.toml` `legacy_no_prompt_blocker` blocks on "do you want to" plus
  "yes" anywhere on the screen, with no visible-blocker flag and only an
  empty-prompt `not`.

AGENTS.md asks for "invariant controls as explicit AND/OR gates"; these gate
on words, not controls. Enforceable only by captured-screen tests (see 6.1).

### D8. Letta treats ConEmu state 3 as Blocked; Qwen and Kiro treat it as Working

`letta.toml` `osc_progress_blocked` is `^4;3(?:;|$)` at the highest priority;
`qwen.toml` `osc_tool_progress_working` and `kiro.toml`
`osc_progress_working` read the same `4;3` (indeterminate) as Working. One of
these may be right for its agent, but nothing records why Letta's
indeterminate progress means a blocker. Flagging as surprising rather than a
proven defect; it needs a capture.

---

## 2. One value, one owner

### 2.1 The braille spinner class is spelled per manifest, and the copies diverged

`[\x{2800}-\x{28FF}]` (claude, amp, maki), the same range in backslash-u escape form (antigravity,
cursor, kimi, qodercli, droid), `[\x{2801}-\x{28FF}]` (cline, grok), the
literal U+2801 to U+28FF range (qwen, also excluding U+2800), and a ten-glyph
literal subset of the dots spinner (codex, pi, letta), plus
`TITLE_ACTIVITY_GLYPHS`' own range. U+2800 is the
blank braille cell, which renders as a space: the copies that include it let
a "blank" leading cell count as a spinner, the others do not. Fix: named
matcher classes in the manifest schema (`{spinner}` expanded at compile) or a
shared include. Enforceable by compile-time expansion plus a lint test that
no manifest spells a raw braille range.

### 2.2 Claude's activity-glyph class diverged inside one manifest

`live_turn_working` uses `[\x{002A}\x{00B7}\x{2722}\x{2733}\x{2736}\x{273B}\x{273D}]`;
`background_agents_working` and `background_mcp_task_working` drop
`\x{2733}`. `TitleActivityGlyphs::CLAUDE_ANIMATION_GLYPHS` is a third copy
with a different membership (adds the half circles, lacks `*`). If the
omission is deliberate (U+2733 is the idle title marker), nothing says so.

### 2.3 Duplicated rule bodies

`kilo.toml` `opencode_permission` and `opencode.toml` `permission_required`
are the same gate tree (the follow-up test does iterate both, which keeps
them in step today). Within manifests: Letta restates its `active_status` and
`running_tool` regexes verbatim as `not` gates of `composer_idle`; Muse
restates its two picker pairs three times; Devin restates its blocker pair as
a `not` gate five times. The schema has no way to reference another rule or
a named matcher. Fix: a `[matchers]` table or a `rule = "<id>"` gate kind.
Mechanizable at compile.

### 2.4 Display spellings written beside serde spellings

`HookRejection`, `FallbackReason`, `ScreenDetectionSkipReason`,
`ReportedStartSource` and `AgentState` each have `rename_all = "snake_case"`
and a hand-written `Display` match. Only `HookRejection` is tested
(`every_rejection_reason_displays_as_its_json_spelling`), and that test lists
the variants by hand, so a new variant is silently untested. `RegionSpec` has
a `parse` match and a `Display` match with only `whole_recent` round-tripped.
Fix: derive `Display` from the serde name (one helper over
`serde_json::to_value`, or a single const table per enum used by both
directions); for `RegionSpec`, one `[(spec, name)]` table. Enforceable by an
exhaustive-match helper in the tests.

### 2.5 Cross-crate coupled timing values

`PARKED_START_LIFETIME` (detect) is derived in prose from the mux probe
cadence (`PROCESS_RECHECK_MISSING_FOREGROUND_GROUP`, 30 s);
`HOOK_SEQUENCE_REANCHOR_AFTER`'s doc restates the hook assets' seq units
(nanoseconds for shell and Python, microseconds for JS); `AGENT_ABSENCE_STARTUP_HOLD`
is an alias of a private `AGENT_RESUME_DETECTION_HOLD` used nowhere else. The
first can be checked mechanically: mux sits above detect, so a
`const _: () = assert!(...)` in mux limits can tie the lifetime to its
cadence. The units claim needs a test in the integration crate that reads
each asset's seq expression.

### 2.6 Executable suffix list

`Runtime::classify` (detect `lib.rs`) and `normalized_agent_lookup_name`
(shepr-agent) both spell `[".exe", ".js"]`; the comment admits the copy.
Same today. Export one const from shepr-agent. Enforceable by the type
(a shared slice).

### 2.7 Limit values copied into tests

`manifest_validation_rejects_excessive_rule_count` builds 129 rules and
`manifest_validation_rejects_excessive_matchers` 33 matchers, literal copies
of `MAX_RULES_PER_MANIFEST + 1` and `MAX_MATCHERS_PER_GATE + 1`; raising a
limit makes them pass without testing the bound. The detect `state.rs` test
`agent_detection_does_not_skip_before_first_published_report` asserts
`Duration::from_millis(500)` instead of `PROCESS_RECHECK_NO_AGENT`. A textlint
over test code for literals equal to a named limit is impractical; review
item.

### 2.8 The progress spelling manifests match

`Progress`' `Display` in shepr-vt owns `4;state[;percent]`, restated in the
docs of `DetectionInput`, `AgentDetectionInputs`, `agent_osc.rs` and grok's
comment. The manifests' `osc_progress` regexes (`^4;0`, `^4;0;0$`, `^4;1$`,
`^4;3;?$`, `^4;3(?:;|$)`) are never tested against `Display` output. A test
that every `osc_progress` regex matches at least one `Progress::to_string()`
over the five states with and without percent would catch a spelling change.

---

## 3. Values nobody can find, change or trust

### 3.1 The detection tunables are split three ways

Arbitration bounds live in `shepr-detect/src/limits.rs`, detector cadence and
holds in `shepr-mux/src/limits.rs`, and the region depths
(`bottom_non_empty_lines(12)`, `(20)`, `(30)`, `top_non_empty_lines(20)`) in
each manifest. Nothing answers "what are the detection tunables". At least
give mux limits a section header that points at the detect limits that
depend on it, or move `PARKED_START_LIFETIME` and
`AGENT_PROCESS_EXIT_RELEASE_GRACE` to a parameter mux passes in.

### 3.2 The detector state machine has no clock seam enforcement

`pane/detect/mod.rs` says the detector "performs none of the I/O itself" and
`DetectorState` "can be exercised with fake times". That holds today
(`detection_task.rs` samples `Instant::now()` and passes it in), but brokkr
has a clock textlint for shepr-detect, mux `persist`, the server and others,
not for `crates/shepr-mux/src/pane/detect/**` or `agent_detection.rs`.
Mechanizable: add them to a textlint like `agent-clock-is-injected`, with
`detection_task.rs` as the marked sampler.

### 3.3 `SHEPR_DEBUG_OSC_EVIDENCE` is read at the first pane, and its refusal only warns

`osc_debug::enabled` reads the variable lazily at first pane construction and
turns a refused value into a warning and "off". The comment says this is
deliberate (pane construction has no error path). It contradicts the env
policy's "refuse naming the variable" in spirit, and a typo in a debug
switch is found only by reading the log. Reading it once in server startup
and handing it to pane construction like the other server settings fixes
both.

### 3.4 `DEFAULT_DETECTION_ROWS` is not what its name and doc say

Doc: "Default screen depth sampled for agent detection when no caller
supplies one". Detection reads `terminal.rows()`; no caller supplies a depth.
The const is used only as the floor of the resize recovery probe. Rename to
what it is and reword.

### 3.5 Test helpers read the real clock and mix it with synthetic offsets

The ownership test seams `set_hook_authority_with_session_ref`,
`set_agent_session_ref`, `set_agent_session_ref_for_session_start`,
`set_detected_state*` call `Instant::now()` inside, while tests around them
use `Instant::now() + Duration::from_secs(1)` as "later". Results depend on
the test running in under a second (for example
`fresh_detected_process_keeps_old_session_suppressed_after_process_exit`,
`omp_reacquires_full_lifecycle_hook_after_process_exit_with_fresh_process_and_session_ref`).
Every seam should take its instant; delete the clock-reading variants.

---

## 4. One channel, one implementation (logging)

- `DetectionTask::run`: `warn!(?error, "pane detection tick failed")` has no
  pane id, and a `JoinError` here means the blocking tick panicked, after
  which the pane's detection is gone for the rest of its life (see 5.1). It
  should be `error!` with `pane`.
- The same identifier is logged under two keys: `pane = %pane_id` (mux
  `publish.rs`, `detection_task.rs`, server `admit_hook_outcome`) and
  `pane_id = %params.pane_id` (`handle_pane_report_agent_session`, which also
  logs the public id string where the others log the internal id).
- Levels for the same class of event disagree: an unknown session start
  source from a bundled hook is `warn!`, while a bundled hook's report
  rejected as `InvalidSession`, `MissingSession`, `MissingSequence` or
  `UnrecognizedStart` (each an integration bug, since only shepr's own hooks
  can report) is `debug!` in `admit_hook_outcome`. Rejections that are
  routine races (`OutOfOrder`, `CrossTalk`, `RetiredSession`, `LifecycleGate`)
  belong at debug; contract violations by shepr's own assets belong at warn.
  A `HookRejection::is_integration_fault()` would make that one decision.
- The rejection log omits `seq` and the session ref, which are what one needs
  to match it to the hook that sent it.
- Silent significant events: a parked start expiring is dropped without a
  log; a hook authority withdrawn because the detector reports another agent
  (`transition_detection`'s clear) is not logged anywhere, though it changes
  the sidebar.
- OSC evidence capture needs both `SHEPR_DEBUG_OSC_EVIDENCE=1` and a
  `SHEPR_LOG` filter that admits debug for shepr_mux; with only the first,
  nothing is logged and nothing says why. Log at info, or say so in the
  variable's doc in `shepr_core::env`.

---

## 5. Errors

### 5.1 A panicking detection tick ends detection for the pane, silently to the user

`DetectionTask::run` returns on `Err(JoinError)`. The pane keeps its last
published state forever (Working stays Working on the sidebar) and its
process exit is only learned from the child watcher. A panic in a regex or a
`/proc` reader should either restart the task with a fresh `DetectorState` or
publish an Unknown and mark the pane, and log at error with the pane id.

### 5.2 Swallowed read errors become evidence

See D2.

### 5.3 Manifest compile failure degrades silently per agent

`bundled_manifest` logs `error!` once and the agent's panes report Unknown
forever. A test compiles every bundled manifest, so this cannot ship today,
but the runtime path still treats an impossible state as a soft degrade.
Given detection changes ship only as new builds, a failure here could be a
startup failure of the server (or a `expect` justified by the test).

---

## 6. Tests that prove nothing

### 6.1 Most bundled manifests have no behaviour test

Only Claude (title stand-down, one blocker), OpenCode and Kilo (permission),
Codex (one server explain test) and the stable-Unknown flags of Gemini and
Letta are exercised. Amp, Antigravity, Cline, Copilot, Cursor, Devin, Droid,
Grok, Kimi, Kiro, Letta, Maki, Muse, Pi, Qodercli and Qwen rules are pinned by
nothing, and their comments' evidence claims ("Grok 1.0.34 live pane reads",
"Muse Code 0.2.1 captures") are unverifiable. There is no capture corpus in
the repo although `detect capture` produces exactly the JSON `detect explain
--file` reads. Fix: commit captures per agent state under the detect crate
and a test that runs every capture through `explain_with_input` and checks
its expected rule. This is what makes D7 and 2.1 safe to change.

### 6.2 A test that reimplements the path it verifies

`priority_ordered_detection_agrees_with_full_explain` and
`gate_region_reads_a_different_input_than_its_rule` use a test-local
`detect_loaded`, a copy of `detect_with_manifest`'s loop, so the production
detection path is not what agrees with explain. Call `detect_with_manifest`.

### 6.3 Assertions that cannot fail

- `all_bundled_manifests_parse_validate_and_compile`: the `None` branch asserts
  `screen_manifest_agents().all(|c| c != agent)`, but `screen_manifest_agents`
  is defined as the filter of `bundled_manifest_source(..).is_some()`.
- `explain_for_label_evaluates_the_bundled_manifest_and_names_an_unknown_label`
  compares `explain_for_label` with `explain`, which both call
  `explain_with_input`.
- `agents_without_a_screen_manifest_are_unknown_not_idle` loops over Omp and
  Mastracode but calls `detect_with_manifest(.., None)`, which does not take
  the agent.
- `codex_no_match_is_unknown_without_changing_other_agents`: the "without
  changing other agents" half calls `fallback_explain(Pi, Some((&pi, vec![])))`,
  which cannot be changed by anything the test does. A leftover from local
  manifest overrides, which are gone.
- `title_activity_glyphs_cover_claude_animation` checks one glyph.

### 6.4 Tests named for a configuration they do not run

- `fallback_idle_does_not_override_other_agent_hook_working` uses Codex for
  both the detector and the hook.
- `visible_working_does_not_override_hook_idle_for_same_agent` and
  `visible_working_does_not_override_full_lifecycle_hook_idle` pass no
  visible-working flag; ownership has no such input at all (see D3).
- `set_detected_state_with_visible_blocker(.., _ignored_screen_idle, ..)`: a
  helper parameter that is ignored, and tests pass `true` to it as if it meant
  something.

### 6.5 Wall-clock and scheduling dependence

- `foreground_job_detects_sleep`, `foreground_job_detects_shell_running_command`,
  `foreground_job_detects_agent_behind_shell_wrapper` sleep 50 to 100 ms and
  hope the child became the foreground group; a loaded host flakes them. Poll
  `foreground_job` until it shows the expected process, with a deadline.
- `state_changed_event_waits_for_queue_space_instead_of_dropping` relies on
  20 ms and 50 ms `tokio::time::timeout`s; use paused tokio time.
- `core_contention_does_not_park_the_async_worker` measures real elapsed time.
- 3.5 above.

---

## 7. Guards and claims that have stopped holding

- False today: `Detection::Working`'s "refreshes screen evidence" (D3);
  `PARKED_START_LIFETIME`'s "slowest cadence ... thirty seconds" (D5);
  `DEFAULT_DETECTION_ROWS`' doc (3.4); `compile_manifest`'s
  `unknown_is_stable` doc (D6); `AGENT_ABSENCE_STARTUP_HOLD`'s "A restored pane
  holds absence" (only `LaunchKind::AgentResume` launches hold;
  `LaunchKind::Restored` does not); `with_initial_hook_authority`'s
  "Production code never calls it" (true, unenforced: it is a plain `pub fn`).
- Doc on `CompiledRule`/explain: "detect explain says which rule decided its
  state" (D1).
- Unenforced but true: the detector state machine is I/O-free (3.2); the
  ownership module "never runs the manifest engine" (checkable by a
  dependency-style textlint forbidding `manifest::` in `ownership/`).
- Fail-open by path: `[gremlins] exclude = ["crates/shepr-detect/src/manifests"]`
  exempts the manifests' comments too, not only the screen text they match.
  A narrower exception (matcher string values only) would keep the comments
  checked.
- The variant list in `every_rejection_reason_displays_as_its_json_spelling`
  (2.4) silently stops covering new variants.
- `integration_classes_preserve_authority_for_every_agent` pins capabilities
  by table position; reordering agents and the expectation together hides a
  swap. Key it by agent.

---

## 8. Policy invented per call site

- Ordering of evidence across two clock samplers: hook reports are stamped
  with the loop's per-pass `AppClock` sample, detector observations with the
  detector task's own `Instant::now()` before its probe. `fallback_not_older_than_hook`,
  `hook_authority_not_newer_than` and `detector_observation_allows` compare
  the two. The bias currently favours hooks (a report is stamped no earlier
  than it was queued), so I found no wrong outcome, but the safety rests on
  where the loop calls `refresh_app_clock`, which nothing ties to these
  comparisons.
- `lifecycle_authority` is a derived value (`full_lifecycle_hook_authority_active()`)
  mirrored into an `AtomicBool` by two writers (`apply_lifecycle_authority_changes`
  and `install_runtime`) and kept fresh by every `update_terminal_state` marking
  the pane dirty. Correct today, kept by call order.
- Test-only shortcut reachable from production: `AgentOwnership::with_initial_hook_authority`
  installs authority without arbitration (a seam AGENTS.md permits, but this
  one bypasses the invariants the module exists to keep). Its only caller is
  a mux test; it could be `#[cfg(test)]` behind a dev-only path or replaced by
  constructing the authority through a report.
- Child-controlled data reaching logs: OSC evidence payloads (documented,
  opt-in, truncated) and the `info!("agent changed", process = ..)` process
  name (from `/proc` comm). Acceptable; noted for completeness.
- The API admits `"unknown"` as a hook state (D4): validation of a wire value
  wider than the vocabulary.

---

## 9. Code that is no longer load-bearing

- `visible_working` end to end (D3): manifest key, `Detection::Working.visible`,
  `AgentDetection::visible_working`, API `visible_working`, the
  `last_visible_signal_refresh` write for working.
- `manifest::explain(agent, screen)`: production-dead, used only by a manifest
  test.
- `Runtime::Tmux`: classified only so `wrapped_agent_from_runtime_argv` can
  return `None` for it, which is what an unclassified name gets anyway.
- `identify_agent` and `agent_from_basename` are both `parse_agent_label`
  under another name.
- `TitleActivityGlyphs`: a unit struct plus a const instance for one
  `contains` function with one caller (`terminal/title.rs`).
- `detect_state_from_api` and `presented_agent_status` in `api_helpers.rs`
  are identity functions over type aliases.
- `SnapshotAgent.agent: Option<Agent>` is always `Some` (`agent_info` filters on
  `is_agent_terminal`, which is `effective_agent().is_some()`).
- `AGENT_RESUME_DETECTION_HOLD`: a private name only used to define
  `AGENT_ABSENCE_STARTUP_HOLD`.
- OSC 21337 ("agent status") in `osc_debug`: captured as manifest evidence,
  but no region, manifest or detector reads it; a herdr protocol leftover.
- `set_detected_state_with_visible_blocker`'s `_ignored_screen_idle` parameter,
  a leftover of a removed screen-idle signal.
- Pane history leftovers next to scope (in `pane/terminal/backend.rs` and
  `pane/runtime.rs`): `recent_text`, `recent_ansi`, `recent_unwrapped_text`,
  `visible_ansi`, `PaneRuntime::recent_unwrapped_text`. The comment calls them
  "Test-only reads", yet they are `pub(crate)`/`pub` production items with no
  production caller. `cfg(test)` them or delete them with their tests;
  `scripts/check_dead_test_helpers.py` does not catch them because they are
  not under a test cfg.
- `is_unsequenced_opencode_selection` is policy-driven
  (`HookSessionPolicy::unsequenced_selection`), not OpenCode-specific; the
  name is stale.

---

## Lateral

- The detector's `reset()` (on authority activation) and authority end leave
  ownership's `fallback_state` at whatever the detector last published before
  the authority, possibly long ago. When a session-start replacement clears
  authority without an exit, that stale fallback is presented until the next
  detector publication (about one tick). Harmless in practice; a reset could
  also reset ownership's fallback to Unknown.
- `osc7.rs` keeps a `?query` or `#fragment` of a `file://` URI as part of the
  path. Shells do not send them; noting only.
- `ProcessProbeResult::process_name` is computed and cloned per probe only for
  the "agent changed" log line.
- `foreground_group_leader_job` and then `foreground_job` read `/proc` twice
  per probe when the leader is unidentified; fine at current cadences.
