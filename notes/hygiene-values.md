# Hygiene: values

Values defined in more than one place, and values nobody can find, change or
trust (tunables placed where nobody looks, no injection point, coupled values
tied together only in prose). Filed from the nine-scope hunt; each entry names
the hunts that reported it and says how the fixed form could be enforced.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.

---

## VAL-008 - The agent is stored twice in every saved session identity and every report

Reported by: restore-resume.

`resume.rs` `PersistedAgentSession { source, agent, session_ref }`: the source
names exactly one agent (`AgentSource::agent()`), and `is_valid_identity` exists
to check the second copy agrees. The saved file carries both `"source"` and
`"agent"`, so it can hold a disagreement decoding then refuses;
`ReportOrigin::owns` compares both again. On the wire a hook sends a `source` and
an agent label, and `ReportOrigin::parse` exists largely to refuse
`MismatchedAgent`. Fix: drop the `agent` field and the report label, derive the
agent from the source; the disagreement becomes unrepresentable. No migration is
owed. This also removes the label-acceptance split in POL.

## VAL-009 - Integration source strings restate their labels by hand

Reported by: restore-resume.

`shepr-agent/src/lib.rs` `IntegrationTarget::source()` hand-spells `"shepr:pi"`
through `"shepr:agy"`; each is `"shepr:" + label()`. Only round trips are tested.
Enforce with a test that `source() == format!("shepr:{}", label())` for every
target, or derive it.

## VAL-010 - Enum spellings are written twice: serde's and a hand-written `Display`

Reported by: restore-resume, agent-state.

`AgentState`, `HookRejection`, `FallbackReason`, `ScreenDetectionSkipReason` and
`ReportedStartSource` each derive `rename_all = "snake_case"` and also hand-write
`Display` with the same strings. Only `HookRejection` is tested
(`every_rejection_reason_displays_as_its_json_spelling`), and that test lists the
variants by hand, so a new variant is silently untested. `RegionSpec` has a
`parse` match and a `Display` match with only `whole_recent` round-tripped. Fix:
derive `Display` from the serde name (one helper), or one `[(variant, name)]`
table per enum used both ways. Enforceable by an exhaustive-match helper in tests.

## VAL-011 - `AgentSessionStartSource::ALL` restates the enum, unchecked

Reported by: restore-resume.

`parse` searches `ALL` and the round-trip test iterates `ALL`, so a variant left
out of `ALL` never parses and no test notices. Enforceable at compile time with
the exhaustive-match `const` block `lib.rs` already uses for `AGENTS`. The same
trick would replace `TARGETS_HAVE_INTEGRATIONS`'s "`Grok` is the last variant"
comment.

## VAL-014 - The resume timeline's tunables are spread across three crates with nothing naming the set

Reported by: restore-resume.

When and how a resume happens is decided by `[session] agent_resume_spacing_ms`
(config), `PENDING_AGENT_RESUME_THEME_WAIT` (750 ms, server limits),
`AGENT_ABSENCE_STARTUP_HOLD` (30 s, mux limits) and `LAUNCH_SETTLE_AFTER_PANE_END`
(mux limits). Each is named in its crate's limits, but nothing tells a reader these
four are the resume timeline. Add a section in `reference/` or a module doc in
`resume_schedule.rs` naming them. The theme wait has an injection point at
`ResumeSchedule::new` but none at the `App` (it always passes the constant).

## VAL-016 - The pane teardown step count is restated as a literal factor in another crate

Reported by: save-shutdown, pane-lifecycle.

`shepr-server` `PANE_TEARDOWN_WAIT = BUDGET.saturating_mul(4)`, explained as the
signal budget "plus three more of it for the /proc session scans between signal
rounds": the 3 is the length of mux's `PANE_TEARDOWN_STEPS`.
`PaneTeardownTracker::BUDGET`'s doc also says "Three signal grace periods".
Adding or removing a step leaves both stale. The scan time itself is unbounded
(two full `/proc` walks per round, a `ProcStat` read per pid) and unmeasured, so
the factor is a guess. Fix: mux exports the scan-inclusive wait (it owns the
steps), or `PANE_TEARDOWN_STEPS.len()`; reword the docs not to hard-code the count.

## VAL-019 - Save and shutdown tests restate the constants' current arithmetic

Reported by: save-shutdown.

`a_failed_pane_exit_checkpoint_retries_on_its_own_backoff` asserts the deadline is
`SESSION_SAVE_RETRY_MIN * 64` after seven failures, hard-coding the multiplier and
that 16 s is under `SESSION_SAVE_RETRY_MAX`; raising the minimum to 1 s breaks it
for no behavioural reason. `two_failures_retry_and_the_third_finishes_unsaved`
hard-codes MIN, MIN*2 and the count 3; `three_failures_of_the_newest_generation_..`
names 3 while looping on the constant. Compute expectations through `Backoff` /
`checkpoint_retry_delay`, and name tests by the constant, not its value.

## VAL-020 - The save and shutdown time budget is scattered and its real bound is undocumented

Reported by: save-shutdown.

The tunables are `SESSION_SAVE_DEBOUNCE`, `SESSION_SAVE_RETRY_MIN` / `MAX`,
`CHECKPOINT_RETRY_MAX_DELAY`, `CHECKPOINT_MAX_FAILURES`, `SHUTDOWN_FLUSH_TIMEOUT`,
`PANE_TEARDOWN_WAIT`, `SHUTDOWN_RECONNECT_*` (server), the teardown steps (mux),
the stop waits (launch), and logind's `InhibitDelayMaxSec`, which is external and
mentioned nowhere. The host checkpoint's retries (250 ms, 500 ms, give up) plus
write time must fit `InhibitDelayMaxSec` (default 5 s); a disk taking 3 s per
attempt exceeds it and logind proceeds without the checkpoint while the server
keeps retrying. No document lists the shutdown budget end to end; `reference/`
says nothing about saves or shutdown (not when the layout is saved, not the
pane-exit or host checkpoints, not the final save skipped during host shutdown),
though "my last few seconds of layout changes were not restored" is user-visible.

Fix: a shutdown and save section in `reference/`. Optionally have the monitor read
the manager's `InhibitDelayMaxUSec` and turn checkpoint retries into a deadline
rather than a count.

## VAL-021 - Debounce, retry and checkpoint constants have no injection point

Reported by: save-shutdown.

They are read inside `Autosave::schedule`, `RETRY_BACKOFF` and
`checkpoint_retry_delay`. Tests work around it with `set_autosave_deadline`, and
the persisting tests (`handle_test_runtime_exit_and_replay`, `wait_for_checkpoint`)
spin on a real writer thread with 5 s wall-clock timeouts; that helper is the base
of many app tests, so a slow disk under the harness times many out together. A
`SavePolicyConfig { debounce, retry, checkpoint }` handed to `SessionSaver::new`
would let tests drop the hand-set deadlines.

## VAL-025 - Small duplicated values in the pane lifecycle

Reported by: pane-lifecycle.

- `ACTOR_IDLE_POLL` is restated as "at least once a second" in
  `PtyIoActorConfig::core_broken`'s doc and assumed by two tests (see the claims document).
- "The user's home" has two resolution moments: `PtyCommand::interactive_shell`
  copies `std::env::vars_os()` on every spawn and `cwd_candidates` reads `HOME`
  from that copy, while `passwd_home` is read once at init. Take the environment
  snapshot once at `init_pane_launches`.

## VAL-026 - Pane lifecycle tunables have no injection points and couplings stated only in prose

Reported by: pane-lifecycle.

The values sit in three limits modules (`shepr-pty`: `LAUNCH_HELLO_TIMEOUT`,
`LAUNCH_ACCEPT_RETRY_DELAY`, `LAUNCH_PARKED_CONNECTION_TTL`, `ACTOR_IDLE_POLL`;
`shepr-mux`: `LAUNCH_STATUS_AFTER_EXIT`, `LAUNCH_SETTLE_AFTER_PANE_END`,
`LAUNCH_EXIT_POLL_INTERVAL`, `TERMINAL_CLOSED_EXIT_GRACE`, `PANE_TEARDOWN_STEPS`;
`shepr-server`: `PANE_TEARDOWN_WAIT`). Injection gaps:

- `ACTOR_IDLE_POLL`: documented as only a fallback for a missed wake, but it is
  also the only cadence at which a core poisoned off the reader thread is noticed
  (`core_broken`, checked per loop); say so. `TestPtyIo` can replace `poll` but
  `run_loop` passes `Wait::After(ACTOR_IDLE_POLL)` itself, so
  `a_core_broken_elsewhere_ends_an_idle_pane` waits up to 3 s of wall clock.
- `LAUNCH_SETTLE_AFTER_PANE_END`: `coordinate` could take it as a parameter, as
  `reader_exit_callback` does for `TERMINAL_CLOSED_EXIT_GRACE`;
  `a_hung_launch_does_not_keep_a_failed_reader_from_ending_the_pane` waits it out.
- `PANE_TEARDOWN_STEPS`: `pane_teardown_reaches_background_jobs_after_the_leader_is_reaped`
  waits through real grace periods.

## VAL-029 - Manifest rule bodies are duplicated because the schema cannot reference a rule or matcher

Reported by: agent-state.

`kilo.toml` `opencode_permission` and `opencode.toml` `permission_required` are the
same gate tree (a test iterates both, keeping them in step today). Letta restates
its `active_status` and `running_tool` regexes as `not` gates of `composer_idle`;
Muse restates its two picker pairs three times; Devin restates its blocker pair as
a `not` gate five times. Fix: a `[matchers]` table or a `rule = "<id>"` gate kind,
resolved at compile.

## VAL-032 - Limits and cadences copied into tests as literals

Reported by: agent-state, workspace-model.

`manifest_validation_rejects_excessive_rule_count` builds 129 rules and
`manifest_validation_rejects_excessive_matchers` 33 matchers, copies of
`MAX_RULES_PER_MANIFEST + 1` and `MAX_MATCHERS_PER_GATE + 1`; raising a limit makes
them pass without testing the bound.
`agent_detection_does_not_skip_before_first_published_report` asserts 500 ms instead
of `PROCESS_RECHECK_NO_AGENT`. `copy_search_bounds_returned_matches_but_keeps_exact_total`
asserts `1024` (`MAX_RETURNED_MATCHES`); `pane_resize_changes_target_ratio_without_changing_focus_or_navigating`
asserts `0.55` (`EVEN_SPLIT + DEFAULT_PANE_RESIZE_AMOUNT`); several core layout tests
assert `0.45` / `0.55`. Reference the constants.

## VAL-035 - `SHEPR_DEBUG_OSC_EVIDENCE` is read at the first pane and a bad value only warns

Reported by: agent-state.

`osc_debug::enabled` reads the variable lazily at first pane construction and turns
a refused value into a warning and "off" (deliberately: pane construction has no
error path). That contradicts the env policy's refusal naming the variable, and a
typo in a debug switch is found only by reading the log. Read it once at server
startup and hand it to pane construction like the other settings. See DIAG for the
second half (it also needs a debug log filter to show anything).

## VAL-040 - The Antigravity target has four names

Reported by: integrations.

Label `agy` (source `shepr:agy`, `mktemp` name `shepr-agy-hook`),
`registry::action_label` `antigravity-cli`, serde id `antigravity_cli` (asset
header, `bundle::integration_id`), and the directory `antigravity_cli/`.
`install_present_integrations` logs `integration = "agy"` with a message saying
"antigravity-cli", so one line names it two ways. Drop `action_label`; one name
per target from the descriptor.

## VAL-042 - The integration asset list is written three times

Reported by: integrations.

`bundle.rs` `SPECS` (path, decoder, version), `lib.rs` (`include_str!` per asset,
install-name constants), and the server's `SHELL_ASSETS` / `BUN_ASSETS` /
`bun_trace_name` (plus trace names in `contract_traces.toml`). Kept in step by
`every_asset_with_a_decoder_is_generated` and the server's `assert_asset_coverage`.
The `include_str!` copy is forced (it needs a literal); one `macro_rules!` table
could emit both the `SPECS` rows and the constants.

## VAL-046 - Integration tunables live as literals in the plugins, with no clock seam

Reported by: integrations.

`limits.rs` holds three values plus one `#[cfg(test)]` one; the rest are literals in
the JS/TS decoders (VAL-038, OMP's 250 ms and 2500 ms defaults, the extension's
one-retry policy) and the descriptor table. `plugin_kit.js`, `tui_kit.js`,
`extension_kit.ts` and the TUI decoder call `Date.now()` and `setTimeout` directly,
so their bun tests can only sleep (650 ms, 1.6 s, 2.5 s deadlines). Generate every
decoder timing from `limits`, and pass a clock and timer object into the kits; the
Rust clock textlints do not reach `.js` / `.ts`. Also,
`TOML_BASIC_STRING_DELIMITER_BYTES = 2` (the two quote characters, a `with_capacity`
hint) poses as a tunable; mark it `limits-exempt` at the use or write `len() + 2`.

## VAL-051 - `inner_rect` means two different rects

Reported by: workspace-model.

In `shepr_core::chrome`, `inner_rect` is inside the borders, before the scrollbar
gutter. In `ui::PaneSurface`, the wire `PaneSurfacePane` and every client consumer,
it is the content rect, after the gutter. `pane_resize::laid_out_pane_sizes` sizes
PTYs from `pane.inner_rect` and is right only because it reads the surface meaning.
Rename the surface and wire field `content_rect`.

## VAL-052 - Small model values with two spellings or two definitions

Reported by: workspace-model.

- `PanePublicNumber`'s `Display` is decimal (`10`) while `PublicPaneId` spells the
  number in bijective base 32 (`w1:pA`); logging a `TreeRejection` prints a number
  the user has never seen. Use the base-32 form or remove `Display`.
- `TerminalTitleChange` (mux) and `TerminalTitleChanges` (server) have the same two
  fields and are folded field by field. Keep the mux one.
- `AppSettings::headless_rect` rebuilds `Rect::new(0, 0, cols, rows)` where
  `GridSize::rect()` / `SpawnGeometry::for_grid` exist; `AppSettings::pane_geometry_in`
  and `AppState::chrome_in` are one function under two names.
- `App::json_pane_with_id`'s error spells the id grammar (`expected
  w<workspace>:p<pane>`) owned by `shepr-protocol/src/ids.rs`; let
  `PublicIdParseError` carry the expected form.

## VAL-053 - Tunables named as defaults with nothing to override them, and one constant with two meanings

Reported by: workspace-model.

`DEFAULT_PANE_RESIZE_AMOUNT` is the only keyboard resize step; "default" suggests a
setting that does not exist. Rename `PANE_RESIZE_STEP`. The lost-refresh check
cadence borrows `GIT_REMOTE_STATUS_REFRESH_INTERVAL` (`refresh_deadline_after` is
used for the next refresh and for `lost_refresh_check_at`): two meanings, one
constant.

## VAL-055 - Executable names are re-spelled in operator text

Reported by: server-lifecycle, remote.

`SERVER_BINARY_NAME` exists, yet `"shepr-server"` is literal in `stop.rs`
`ServerStopError::TimedOut`'s message, `shepr-daemon/src/main.rs`
`report_server_error`, and every mismatch message in `shepr-remote/src/discovery.rs`.
`"shepr"` is literal in `cli/error.rs` (`run 'shepr --help'`). `PROGRAM_NAME` and
`REMOTE_INSTALL_NAME` are two constants of one value (a host never has more than one
`shepr`); fold them unless they are meant to diverge. Route operator commands
through `guidance::operator_entrypoint`. Enforceable with a textlint on
`"shepr-server` and `` `shepr `` in string literals outside `invocation.rs` and
`guidance.rs`.

## VAL-056 - `ApiClient::ping` still waits the ordinary 20 s response window

Reported by: server-lifecycle.

Status probes now use launch-owned 2 s windows and the stop keeps its documented
250 ms probe. `ApiClient::ping` (the CLI's pre-request build check) still waits
`ORDINARY_RESPONSE_TIMEOUT` (20 s), although `ping` is answered on the connection
thread and never waits for the app loop. Give it the launch status window.

## VAL-058 - Request ids, "is this build" and similar wire facts are spelled per call site

Reported by: server-lifecycle.

`shepr-api` now has a request id type with constructors (ping, summary, operator
stop, startup-restart stop, detect capture and explain), used by the API client.
Still spelled as literals: the request ids and a local error response id in
`src/cli/detect.rs`, and the stop request in `shepr-launch/src/stop.rs` (which
still sends one id for an operator stop and the startup restart) and its status
fake in `shepr-launch/src/status.rs`. Separately, "is this build" has two spellings:
`status.build_id.is_this_build()` (launch, preflight, remote) and
`BuildIdentity::for_this_build().matches(..)` (`cli/status.rs`). Pick one.

## VAL-059 - Server lifecycle tunables: unused seams, misnamed values, no injection points

Reported by: server-lifecycle.

- `STATUS_REQUEST_TIMEOUT`, `STOP_WAIT_TIMEOUT` and `SERVER_READY_TIMEOUT` have no
  injection point at the public entry points, so tests wait them out (three tests
  each sit through the full 2 s). Only `stop_active_server_with_timeout` is
  parameterised.
- `launch_with` and `acquire_launch_lock_with` take `now` / `sleep` seams that every
  test fills with `Instant::now` and `std::thread::sleep`, so the tests run on real
  time and `a_holder_that_never_leaves..` asserts a 2..=4 restart count from
  wall-clock pacing. Drive them with a fake clock or drop the seam.
- `SOCKET_POLL_INTERVAL` is also the poll of a child process
  (`read_server_version_line`), named for something else.
- `MAX_LOCAL_OFFERS` lives in the binary's `src/limits.rs` while the restart
  policy lives in launch.
- The lifecycle tunables are split over five limits modules (launch, api, remote,
  server, binary). The remote start and stop budgets are now tied to launch by
  `const` asserts, but nothing names which timeouts must stay ordered with which.

## VAL-061 - Small duplicated values in the remote layer

Reported by: remote.

- The SSH metadata cache is per-profile by its own `ssh-metadata-{profile.marker()}`
  suffix under the shared client state dir, while `AppPaths::data_dir()` already is
  per-profile: two rules for where dev keeps its own state.
- The `other_build()` test helper is copied into `discovery/tests.rs`,
  `server_lifecycle/tests.rs`, `src/preflight.rs` tests and inline in bridge tests;
  one fixture in `shepr_test_fixtures`.
- The "shorten XDG_RUNTIME_DIR" advice is formatted twice in `ssh_paths.rs`.
- `supervisor.rs` `ssh_recovery_rejects_stale_generations_and_rechecks_attention`
  asserts `now + Duration::from_secs(30)` where siblings use `ATTENTION_RETRY_DELAY`,
  and `a_reconnecting_machine_retries_within_thirty_seconds` spells 30 again. If
  30 s is a promise, name it and assert `MAX_RETRY_DELAY <= RETRY_PROMISE` once.
