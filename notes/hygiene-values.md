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

## VAL-014 - The resume timeline's tunables are spread across three crates with nothing naming the set

Reported by: restore-resume.

When and how a resume happens is decided by `[session] agent_resume_spacing_ms`
(config), `PENDING_AGENT_RESUME_THEME_WAIT` (750 ms, server limits),
`AGENT_ABSENCE_STARTUP_HOLD` (30 s, mux limits) and `LAUNCH_SETTLE_AFTER_PANE_END`
(mux limits). Each is named in its crate's limits, but nothing tells a reader these
four are the resume timeline. Add a section in `reference/` or a module doc in
`resume_schedule.rs` naming them. The theme wait has an injection point at
`ResumeSchedule::new` but none at the `App` (it always passes the constant).

## VAL-026 - Pane lifecycle tunables have no injection points and couplings stated only in prose

Reported by: pane-lifecycle.

The values sit in three limits modules (`shepr-pty`: `LAUNCH_HELLO_TIMEOUT`,
`LAUNCH_ACCEPT_RETRY_DELAY`, `LAUNCH_PARKED_CONNECTION_TTL`, `ACTOR_IDLE_POLL`;
`shepr-mux`: `LAUNCH_STATUS_AFTER_EXIT`, `LAUNCH_SETTLE_AFTER_PANE_END`,
`LAUNCH_EXIT_POLL_INTERVAL`, `TERMINAL_CLOSED_EXIT_GRACE`, `PANE_TEARDOWN_STEPS`;
`shepr-server`: `PANE_TEARDOWN_WAIT`). Injection gaps:

- `LAUNCH_SETTLE_AFTER_PANE_END`: `coordinate` could take it as a parameter, as
  `reader_exit_callback` does for `TERMINAL_CLOSED_EXIT_GRACE`;
  `a_hung_launch_does_not_keep_a_failed_reader_from_ending_the_pane` waits it out.
- `PANE_TEARDOWN_STEPS`: `pane_teardown_reaches_background_jobs_after_the_leader_is_reaped`
  waits through real grace periods.

## VAL-025 - "The user's home" has two resolution moments

Reported by: pane-lifecycle.

`PtyCommand::interactive_shell` copies `std::env::vars_os()` on every spawn and
`cwd_candidates` reads `HOME` from that copy, while `passwd_home` is read once at
init. A process-global snapshot taken at `launch::init` was tried and reverted: in a
test binary the first `run_server` under its `IsolatedEnv` froze that test's variables
for every later spawn. Take the snapshot per `PaneLauncher` instead (the reason for the
per-command copy is now recorded on `base_env`).

## VAL-021 - Most save tests still hand-set deadlines

Reported by: save-shutdown.

`SessionSaver::with_config` now takes a `SavePolicyConfig`, but only one test injects
it; most save tests still use `set_autosave_deadline`. Move them onto the injected
config.

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
