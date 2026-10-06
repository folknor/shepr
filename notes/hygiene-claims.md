# Hygiene: claims

Things that look like checks and are not: tests that cannot fail or depend on the
host rather than the repository, guards that fail open when a name stops matching,
and invariants or documentation that nothing enforces (several of them false
today). Filed from the nine-scope hunt; each entry names the hunts that reported it
and says how the fixed form could be enforced.

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

## Tests

## CLAIM-005 - Tests depend on the host's `/bin/sh` through a fixture constant the textlint cannot see

Reported by: restore-resume, workspace-model.

`shepr-test-fixtures/src/config.rs` sets `FIXTURE_SHELL = "/bin/sh"` as the default
shell of every validated test server config, and validation checks it exists. Every
`App::new` in tests therefore depends on the host shell, and tests that build an
`App` without `set_test_shell` and spawn a pane run it: the `agent_resume.rs` tests
(`pending_agent_resume_waits_for_live_host_theme_before_launch`,
`pending_agent_resume_can_launch_after_theme_wait_expires`, the hidden, zoom-hidden,
background and first-resize tests, and `failed_deferred_restore_...` with
`missing_shell == false`), the marker test of which depends on the host shell
interpreting the typed command. The `no-borrowed-process-stand-ins` textlint matches
only a literal inside `PaneShellConfig::new(...)`, so the constant slips past and no
`host-program-ok` marker records an exception. For the marker test the shell is
arguably the subject; then say so, and give the others
`shepr_test_support::fixture::resolved_shell` or `idle_shell`. A textlint on
`"/bin/sh"` literals in fixture crates would close the gap.

## CLAIM-008 - The D-Bus monitor test borrows the host's `dbus-daemon` unflagged

Reported by: save-shutdown.

`delay_lock_is_held_until_checkpoint_and_retaken_after_cancellation` spawns the
host's `dbus-daemon`. It is `#[ignore]`, so `brokkr check` never runs it, while
`brokkr test -p shepr-server host_shutdown` runs it (`--include-ignored`) and fails on
a host without `dbus-daemon`. `no-borrowed-process-stand-ins` lists only shells and
coreutils, so it is not flagged. Add `dbus-daemon` with a `host-program-ok:` marker
(real D-Bus behaviour is its subject), or say so in the rule's preset. It also uses
5 s wall-clock timeouts.

## CLAIM-010 - Most bundled detection manifests have no behaviour test

Reported by: agent-state.

Only Claude (title stand-down, one blocker), OpenCode and Kilo (permission), Codex
(one server explain test) and the stable-Unknown flags of Gemini and Letta are
exercised. The rules of Amp, Antigravity, Cline, Copilot, Cursor, Devin, Droid,
Grok, Kimi, Kiro, Letta, Maki, Muse, Pi, Qodercli and Qwen are pinned by nothing,
and their comments' evidence claims ("Grok 1.0.34 live pane reads", "Muse Code 0.2.1
captures") are unverifiable. There is no capture corpus, though `detect capture`
produces exactly the JSON `detect explain --file` reads. Fix: commit captures per
agent state under the detect crate and a test running each through
`explain_with_input` against its expected rule. This is what makes BUG-034 and
VAL-027 safe to change.

## CLAIM-011 - Detection tests that reimplement the path, cannot fail, or name a configuration they do not run

Reported by: agent-state.

- `visible_working_does_not_override_hook_idle_for_same_agent` and
  `visible_working_does_not_override_full_lifecycle_hook_idle` pass no visible-working
  flag; ownership has no such input (BUG-031). They wait on that decision, and so does
  the clock-reading `set_detected_state` helper they use (CLAIM-012).

## CLAIM-012 - Detection tests that depend on wall-clock timing

Reported by: agent-state.

Every ownership seam now takes its instant except the no-time `set_detected_state`
helper, which still calls `Instant::now()` because the two pending visible-working
tests (CLAIM-011) use it; the exception is commented at the seam.

## CLAIM-013 - Integration tests that cannot fail or guard removed things

Reported by: integrations.

- `shell_hooks_reject_dev_panes_after_draining_input` feeds every shell hook
  `{"session_id":"dev-session"}` under `SHEPR_BUILD_PROFILE=dev` and asserts no
  request. Claude's decoder exits on a missing `hook_event_name`, Devin's on an
  event not in `EVENTS`, Antigravity's on a missing `conversationId`, so those three
  send nothing under `release` either; the "after draining input" half is not checked
  (`capture_hook` accepts a `BrokenPipe` on stdin). Give each hook its known-valid
  payload and assert it reports under release and not under dev.
- `bundled_integration_assets_report_the_descriptor_identity` asserts
  `contents.contains(agent.label())`; for `pi`, `omp`, `kilo`, `agy`, `grok` the
  substring is in almost any file. Match the generated `AGENT = "<label>"` line.
- `process_owned_integration_assets_do_not_report_release` guards against
  `pane.release_agent`, a method that exists nowhere any more; same for the bun
  helper `requestHasMessage`.
- `contract_traces.ts` `normalize` rewrites each request's `id` to `<source>:<rank>`
  before comparing, so the id the plugin sent is never checked against the trace.
- `every_target_reads_current_right_after_install` proves install implies Current,
  never that Current implies a no-op install (Cursor's inserted `version`, which
  status never checks, is the counterexample).
- The thirteen `install_*_errors_when_config_dir_missing` tests call `install_X`
  directly, bypassing the launch path's presence check, so they keep a race-only path
  looking load-bearing.
- Python hook tests require host `python3` and plugin tests host `bun`; both are
  sanctioned (`host-program-ok`, `check_agent_asset_tests.py`) and are the subject.

## CLAIM-015 - Workspace model tests with dead setup or expectations that cannot fail

Reported by: workspace-model.

- Many server tests seed `seed_bookmark_index(Some(n))` and name workspaces
  "active" / "background" although nothing they exercise reads the bookmark;
  leftovers of the "active workspace" model, misleading about dependencies.

## CLAIM-016 - Server lifecycle tests that cannot fail, duplicate each other, or use a developer's paths

Reported by: server-lifecycle, remote.

- `guidance.rs` `the_default_entry_point_is_this_builds` compares each public
  function with its `_with` form called with `operator_entrypoint()`, which is what
  the public function does.
- `local_server_tests.rs` `server_daemon_runs_in_home_not_the_launch_directory`
  computes the expected directory with the function's own rule, then asserts the
  builder set the directory it was handed.
- `tui.rs` `the_local_startup_notice_carries_the_whole_refusal` builds the message
  including the guidance, then asserts the notice contains it.
- `stop.rs` `stop_wait_timeout_allows_slow_graceful_shutdown` asserts the constant
  equals 15 s.
- `a_vanished_server_reads_as_gone_not_unresponsive` duplicates
  `status.rs` `a_listener_that_vanishes_before_answering_is_gone`.
- Wall-clock tests: `repeated_socket_transitions_share_one_wait_deadline` (350 ms
  sleep, `< 225 ms` assertion),
  `request_line_arriving_after_connect_is_read_without_a_poll_delay` (`< 100 ms`),
  `a_holder_that_never_leaves...` (2..=4 restarts),
  `classification_saturation_still_serves_a_peer_whose_kind_has_room` (a 50 ms sleep
  decides which path runs; under load the overflow path its name claims may not be
  exercised).
- Banned-word assertions for removed features (`"--session"`, `"--force"`,
  `"SHEPR_SESSION"` in `guidance.rs` and `tui.rs`; `"capabilities"`, `"status"`,
  `"running"` keys in `server_status_json_reports_the_running_boot`) assert the
  absence of strings no code produces.
- Test data from one developer and `/tmp`: `/home/folk/.cargo/bin/shepr` in
  `cli/status.rs`, `/tmp/shepr-server-test` and `/tmp/shepr-test` in
  `local_server_tests.rs`, `/tmp/shepr-test.sock` in `client.rs`. Only compared, but
  the repo rule for compared paths is `/nonexistent/...`; a textlint on `"/tmp/` and
  `"/home/` in test literals would hold it.
- `src/main.rs` `args_as_utf8_*` tests use `["shepr", "pane", "get", "pane-1"]`, a
  removed command group.

## Guards that fail open

## CLAIM-019 - Guards keyed on names that become silent no-ops

Reported by: integrations, server-lifecycle, remote.

- `brokkr.toml` `endpoint-moves-are-driven-from-the-endpoint-module` matches
  `choice\s*\.\s*(select|...)`, keyed on the binding name; `let c = &mut
  shell.endpoints.choice; c.commit()` or a direct field assignment (which test code
  already does) passes. Make the transition methods `pub(in crate::endpoint)` and the
  field private behind an accessor, and the compiler is the guard.

## Claims nothing enforces

## CLAIM-023 - Save and shutdown doc comments that are false today, and invariants held by call order

Reported by: save-shutdown.

- `Shared::warning_generation`'s doc ("Whether the server has checkpointed for the
  warning now pending.") was copied from `checkpointed`; the function returns the
  pending warning's generation.
- `Autosave::record_failure`'s "re-capture and rewrite the whole session four times a
  second" restates `SESSION_SAVE_RETRY_MIN = 250ms`; reword to "on every retry
  minimum".
- `limits.rs` `BACKOFF_MULTIPLIER` says "Growth factor of every retry backoff", false
  workspace-wide (POL-005), and both it and `backoff.rs` restate the list of backoff
  users, which will drift.
- `SessionSaver::next_save` says it decides "by the same rule as `deadline`"; they are
  separate implementations (`.max()` versus `.any(now < d)` over the same two
  retries). And `deadline() == None` means both "nothing to do" and "a requested
  checkpoint is due now" (`is_due` reads `None` as not due), so correctness rests on
  every requester calling `start_background_session_save` itself. It holds today; one
  missed call would leave a held pane exit with no deadline. Return an enum (`Idle`,
  `At(Instant)`, `Now`) and derive `next_save` from it.
- The final save before the lease is ordered in `run` but no test runs `run()` with a
  persisting server and checks the file exists when the socket goes;
  `server_stop.rs`'s re-exec test could (write a mutation, stop, check the file).

## CLAIM-026 - Integration claims that are false today

Reported by: integrations.

- `shepr-agent/src/lib.rs` `IntegrationHookAction`: "The action word an installed
  hook passes to the shepr report command". There is no report command; hooks write
  to the socket.
- `shepr-core/src/env.rs` `SHEPR_ASSET_INTERNAL_NAMES`: "Header markers install and
  status code parse out of an asset's text". Only `SHEPR_INTEGRATION_VERSION=` is
  parsed; `SHEPR_INTEGRATION_ID` is read by nothing.
- `registration.rs` `JsonShape::expected_events`: "Copilot, Devin and Droid call their
  payload-decoding hooks for every event". Devin's and Droid's events all carry an
  action; only Copilot has an action-less event.
- `config_edit.rs`: "Copilot uses the flatter settings shape `{ type, matcher, bash }`"
  sits above `ensure_flat_command_hook`, which is MastraCode's; Copilot uses
  `ensure_direct_command_hook`.
- `registration.rs`: "Install merges these entries and status matches them; neither
  reconstructs a second interpretation of the descriptor." Claude's install is a
  second, CST implementation, and Cursor's install inserts `"version": 1` that status
  never checks.
- `bundle.rs` `EMPTY_OBJECT`: "every exit path emits an empty object" still fails for a
  `set -e` abort on an unguarded failing command, which exits without `finish`; the
  template guards most commands with `|| true`, not all.
- `lib.rs` and the `bundle.rs` module doc each describe the whole envelope including
  the 500 ms number: two copies of one paragraph no test reads.
- True and unenforced: `opencode.js`'s "it never runs alongside this server plugin"
  (rests on `ownsLocalLifecycle` and OpenCode's launch shapes).
