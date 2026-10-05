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

## CLAIM-002 - Persistence tests that pass vacuously, use a developer's paths, or fight the scratch convention

Reported by: persistence.

- `files::tests::resolve_write_target_returns_a_stat_error_other_than_not_found` and
  `writer::tests::repeated_failed_saves_do_not_replace_a_completed_recovery_copy`
  return early and pass when the runner can read a 0o000 directory or write a 0o500
  one, so under root they assert nothing and report success. Fail loudly or ignore
  under a privileged runner (the repo has `brokkr test`'s `--include-ignored`
  convention for root-only tests).
- `open::tests::refusing_launcher` hard-codes
  `socket_path: "/run/user/1000/shepr-test.sock"`, one developer's uid; harmless only
  because the launcher refuses before a child sees it. Use a scratch path.
- `writer::tests::snapshot_survives_exit_bursts_clears_and_writer_restarts` varies
  only the workspace name across 100 saves; the name is not in the layout
  fingerprint and every save is within the interval, so it cannot tell interval
  suppression from fingerprint suppression. Its local is still called `shrinking`, a
  history-era leftover.
- `files::tests::an_unrelated_leftover_beside_the_session_is_not_touched_by_saves`
  guards a `session.json.tmp` staging name nothing has used since publication moved
  to `.shepr-<token>-<seq>.tmp`; a regression test for a removed behaviour.
- The writer tests end with `std::fs::remove_dir_all(writer.path.parent())` while
  `ScratchDir` is documented as "deliberately left in place afterwards"; half the
  file follows the convention and half fights it. The `writer()` helper drops its
  `ScratchDir` at once, harmless only because it has no `Drop`. `persist/` spells
  scratch directories both `crate::test_support::ScratchDir` and
  `shepr_test_support::ScratchDir`.

## CLAIM-003 - Restore tests that exercise a test-only copy or cannot fail

Reported by: restore-resume.

- `restore.rs` `take_restore_plan_for_snapshot` is a `#[cfg(test)]` copy of the
  duplicate rule, not production `pane_restore_startup`.
  `restore_plan_selection_suppresses_duplicates` and
  `restore_does_not_rehydrate_duplicate_agent_session_metadata` test that copy; the
  latter's last assertion is the function's first `if`. The production rule is
  covered by two other tests. Delete the copy and its two tests.
- `restore_rehydrates_agent_session_metadata`: `restored_terminal_agent_session`
  re-validates an already validated session through its own constructor, so the
  assertions compare a value with itself.
- `complete_restore_planning_needs_no_runtime_or_directory_access`: planning a
  missing directory succeeds whether or not planning stats it, so the test cannot
  observe the access its name rules out. Only a seam (an injected filesystem, or a
  path that hangs) makes it observable; otherwise rename it.
- `resume.rs` `ids_are_data_not_shell_text` asserts the argv vector, never the
  shell text that is typed.
- `test_support.rs` `test_codex_plan(identity, argv)` keeps only the text after the
  last NUL and always builds a Codex session; callers pass
  `"shepr:codex\0codex\0Id\0probe-session"`, a leftover of an older NUL-joined key, so
  `"shepr:pi\0pi\0Path\0..."` would still give a Codex plan. Take a session id only.
- `restore.rs` `failed_cold_restore_preserves_panes_and_saved_directories` writes
  `/tmp/shepr-restore-test-a` and `-b` into its JSON and overwrites them at once;
  the literals mean nothing and read as a `/tmp` use.

## CLAIM-004 - Tests assert positional tables zipped against a list that can grow

Reported by: restore-resume, agent-state, integrations.

`shepr-agent` `integration_classes_preserve_authority_for_every_agent` zips a
23-entry positional `expected` array with `AGENTS`; `zip` stops at the shorter side,
so an appended agent is silently untested, and reordering both hides a swap. Key it
by agent and assert lengths. `bundle.rs` `shell_hook_gates_follow_the_descriptor_events`
picks specs by index (`SPECS[2]`, `SPECS[5]`, `SPECS[3]`), so reordering the table
silently retargets the assertions. `every_rejection_reason_displays_as_its_json_spelling`
lists variants by hand (VAL-010).

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

## CLAIM-007 - Save tests use test-only twins of the production paths and mix two clocks

Reported by: save-shutdown.

- `save_session_now`, `save_session_before_teardown` (sync) and
  `wait_for_session_save` are `#[cfg(test)]` re-spellings of
  `save_session_before_teardown_async` / `reap_finished_session_save`.
  `final_session_save_joins_background_writer_before_returning` is named for the
  final save but calls `save_session_now`, so the production final save's join is
  untested, and it uses a 30 ms sleep to order threads.
  `normal_autosave_replaces_a_signaled_exit_checkpoint` and
  `durable_mutation_after_pane_exit_checkpoint_wins_on_shutdown` end on the sync twin.
  Drive the async path and delete the twins.
- `due_session_save_starts_background_writer`,
  `background_session_save_reschedules_when_writer_is_busy` and
  `normal_autosave_replaces_a_signaled_exit_checkpoint` set the deadline to
  `Instant::now() - 1s` while the saver compares it with `app.clock.now`, sampled
  when the app was built; more than a second between the two (a loaded machine) and
  the save is not due. Use `Some(app.clock.now)`, or make the seam take no argument
  (`make_autosave_due()`).
- `background_session_save_reschedules_when_writer_is_busy` asserts a save is in
  flight and the deadline `is_some()`; both hold whether or not the busy writer
  deferred it. Assert `deadline()` is `None` while in flight and that the next pass
  after the reap starts the save.

## CLAIM-008 - The D-Bus monitor test borrows the host's `dbus-daemon` unflagged

Reported by: save-shutdown.

`delay_lock_is_held_until_checkpoint_and_retaken_after_cancellation` spawns the
host's `dbus-daemon`. It is `#[ignore]`, so `brokkr check` never runs it, while
`brokkr test -p shepr-server host_shutdown` runs it (`--include-ignored`) and fails on
a host without `dbus-daemon`. `no-borrowed-process-stand-ins` lists only shells and
coreutils, so it is not flagged. Add `dbus-daemon` with a `host-program-ok:` marker
(real D-Bus behaviour is its subject), or say so in the rule's preset. It also uses
5 s wall-clock timeouts.

## CLAIM-009 - Pane lifecycle tests that race, depend on the runner, or name a geometry they do not run

Reported by: pane-lifecycle.

- `pty_spawn_leaves_one_parent_pty_fd` counts `/dev/pts` and `/dev/ptmx` fds in the
  whole test process under a lock private to `backend.rs`'s tests, while `actor.rs`
  `actor_open_pty_handles_io_resize_and_slave_close` opens a PTY pair in the same
  binary without that lock, so in parallel the `before + 1` assertion can fail or
  pass for the wrong reason. Move the lock to a crate-level helper. The same test
  sets `SHEPR_ENV=in-pane`, unrelated to what it asserts.
- `actor_wakes_idle_poll_for_user_input` proves wake-driven writes by
  `elapsed < 500 ms` against a 1 s idle poll; with `ACTOR_IDLE_POLL` at 500 ms or
  less it passes whether or not the wake works. Derive the bound from the constant or
  have the poll observer report the wake. `a_core_broken_elsewhere_ends_an_idle_pane`
  hard-codes a 3 s budget from the same constant.
- `the_child_keeps_no_inherited_descriptor` does `dup(0)` and asserts `> 2` as a
  precondition; under a runner with stdin closed it fails for an environmental
  reason. Create the leaked fd itself (a pipe without `O_CLOEXEC`).
- The focus tests in `runtime.rs` build `current_size: cells_only(24, 80)` (24
  columns, 80 rows) around an 80x24 terminal.
- `PtyCommand::interactive_shell` reads the process environment, and most tests in
  `command.rs`, `backend.rs` and `runtime.rs` call it without the `IsolatedEnv` the
  repository rule requires (no wrong result today, since they override what they
  assert on).

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

## CLAIM-014 - The bun tests use `/tmp`, sleep on the wall clock, and leak environment between files

Reported by: integrations.

`shepr-agent-state.test.ts` binds sockets at `join(tmpdir(), ...)` and
`shepr-tui-session.test.ts` uses `mkdtemp(join(tmpdir(), ...))`; the repository's
`no-host-temp-dir` rule is enforced only on `.rs`, so it fails open by extension.
Negative assertions after `Bun.sleep(25)` pass if the send is merely slow; positive
ones wait out real 500 ms retries (VAL-046). `opencode/shepr-agent-state.test.ts`
and `shepr-tui-session.test.ts` set `SHEPR_*` in `beforeEach` and never restore
them, and three files `mock.module("node:net", ...)`, which bun keeps for the
process while `shepr-agent-state.test.ts` needs the real `createServer`:
order-dependent.

## CLAIM-015 - Workspace model tests with dead setup or expectations that cannot fail

Reported by: workspace-model.

- `pane_split_request_focuses_the_new_pane_and_navigates_the_requester_only`,
  `pane_split_request_splits_in_half_and_keeps_default_input_routing` and
  `a_split_sizes_against_the_recorded_geometry_and_only_then_the_requesters` do
  `env.set("SHELL", ..)`, but the fixture config sets an explicit `default_shell` and
  `test_app()` then replaces the launcher's shell: the setup does nothing.
- `moved_cwd_without_osc7_rediscovers_the_label_identity` moves no cwd and asserts
  only that a refresh went in flight ("label identity" is from when Git named
  workspaces); `refreshed_status_is_applied_to_its_workspace` asserts a name that
  comes from construction, not the refresh; `cwd_identity_refresh_runs_once` never
  checks "once".
- `runtime_lookup_is_by_pane` (`state.rs`) builds an `AppState` with a workspace and
  never consults it; it tests `PaneRuntimeRegistry`.
- `workspace_rename_trims_defaults_and_renders_what_it_changed` computes the expected
  blank-rename name with the function production calls, so it cannot catch a change
  in the naming rule. Assert a literal.
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

## CLAIM-017 - Remote tests that cannot fail, test another crate, or depend on the host

Reported by: remote.

- `ssh/tests.rs` `shared_ssh_transport_survives_helper_config_drop` starts no master;
  it checks that two configs name one control path and that dropping one removes only
  its directory.
- `bridge/tests.rs` `remote_bridge_failures_need_attention_only_when_the_host_must_be_fixed`
  forges the remote stderr as `"error: {record}"` itself (VAL-060); its third case
  includes `"error: shepr-remote-daemon-boot-exit:11\n..."`, a record nothing produces
  any more, now only re-testing "unknown marker is unclassified".
- `machine/executable.rs` `shell_quote_uses_the_remote_executable_plain_word_predicate`
  tests shepr-core's internal consistency from shepr-remote;
  `remote_executable_accepts_shell_safe_absolute_paths` and
  `shell_command/tests.rs` `remote_executable_rejects_paths_that_need_shell_quoting`
  test the same parse twice.
- `relay/tests.rs` `bridge_preserves_one_way_progress_and_drains_after_stdin_eof`
  sleeps 60 ms twelve times against a real 300 ms idle timeout and asserts the child
  is alive (a 300 ms stall fails it); `bridge_upload_idle_waits_without_repeated_reads_and_cancels`
  asserts exact poll counts after real sleeps; preflight tests prove concurrency by
  `max_checks_active == 4` after a 100 ms sleep. Say why in each, or drive the boot
  clock through the existing `start_with_clock` seam.
- `shell_command/tests.rs` `remote_output_wrapper_accepts_newline_scripts_and_remaps_exit_255`
  runs `known_remote_binary_candidate_script()` under host `/bin/sh` with the
  developer's real `HOME` / `CARGO_HOME` and asserts only exit 0; a version with a
  scratch `HOME` holding a stand-in `.cargo/bin/shepr` would test what it emits.
- `ssh_metadata.rs` `metadata_is_disposable_fingerprinted_and_independent_per_target`
  asserts removed `version` and `os` fields stay absent (migration residue).
- `process.rs` `a_stderr_pipe_held_by_a_background_process_does_not_block_the_result`
  borrows `shepr_platform::detach_server_daemon_command` to make a process group, so
  a change to server daemon spawning changes this fixture.

## Guards that fail open

## CLAIM-018 - Exclude lists and markers that keep approving things that no longer exist

Reported by: persistence, pane-lifecycle, agent-state, server-lifecycle.

- `brokkr.toml` `disallowed-escapes-are-allowlisted` excludes
  `crates/shepr-mux/src/persist/writer.rs`, which has no `clippy::disallowed_*`
  escape any more, and `crates/shepr-mux/src/pane/terminal/migration_tests.rs`, which
  does not exist. Each stale entry pre-approves a future escape there without review.
  Checkable: a script check that every path in a textlint `exclude` list exists and,
  for this rule, contains an escape.
- `clock-io-ok` markers in `shepr-mux/src/pane/` are decoration: no textlint covers
  mux outside `persist/`, yet `spawn.rs`, `exit_arbiter.rs` and `child_watcher.rs`
  carry the marker while unmarked clock reads sit beside them
  (`runtime/read_effects.rs` three times, `PaneOutputWrite::write`,
  `terminal/backend.rs`). The detector state machine (`pane/detect/**`,
  `agent_detection.rs`) is likewise I/O-free and fake-time-testable today with
  nothing holding it. Add a mux pane clock rule like `terminal-core-clock-is-injected`,
  with `detection_task.rs` as the marked sampler.
- `persist-clock-is-injected` forbids `SystemTime::now()` in `persist/`, but the
  snapshot cadence compares the injected `now` with a file's `modified()` (POL-002),
  so the guard fails open for filesystem time. Widen the pattern with an allow marker
  or say so beside the rule.
- `[gremlins] exclude = ["crates/shepr-detect/src/manifests"]` exempts the manifests'
  comments too, not only the screen text they match; a narrower exception keeps the
  comments checked. And `cli/status.rs` has a comment containing U+2026 (horizontal
  ellipsis), a gremlin under the project rule, so either the gremlins check does not
  cover that character or the file is not swept. Checkable by a non-ASCII grep.

## CLAIM-019 - Guards keyed on names that become silent no-ops

Reported by: integrations, server-lifecycle, remote.

- `regenerate_bundled_assets` is `#[ignore]` "so the gate never writes into the
  tree", which holds for `brokkr check`, but `brokkr test` always passes
  `--include-ignored`, so any `brokkr test -p shepr-integration <filter>` matching
  `bundle`, `bundled`, `assets` or `regenerate` silently rewrites every committed asset
  from the templates, reverting a hand edit under investigation. Make regeneration a
  script, or gate the writer on an explicit environment variable read through
  `shepr_core::env`.
- `hook_assets_share_one_envelope` forbids transport by name (`createConnection`,
  lowercase `settimeout`, `AF_UNIX`, `Math.random`, `import random`); a decoder using
  `net.connect`, Node's `setTimeout` or `socket.create_connection` passes.
- `cli.rs` `parse_launch` reads `--start` with `matches::flag`, which turns a
  spec/handler mismatch into `false`; `matches.rs` reserves that read for root help and
  version. A rename of `FLAG_START` in the spec only silently makes every Connect and
  Restart attach-only. Use `try_flag` and refuse.
- `brokkr.toml` `endpoint-moves-are-driven-from-the-endpoint-module` matches
  `choice\s*\.\s*(select|...)`, keyed on the binding name; `let c = &mut
  shell.endpoints.choice; c.commit()` or a direct field assignment (which test code
  already does) passes. Make the transition methods `pub(in crate::endpoint)` and the
  field private behind an accessor, and the compiler is the guard.

## Claims nothing enforces

## CLAIM-021 - Persistence doc comments that are false today

Reported by: persistence.

- `SessionBackupPolicy::NoBackupNeeded` is documented as "Restore used the file in
  full, or there is no source file to preserve", but `open_and_summarize` leaves a
  `Missing` load at `PreserveExisting`, so a file that appears later is still backed
  up (pinned by `first_clear_preserves_an_unloaded_file_even_after_an_earlier_missing_clear`).
  `SessionPersister::spawn`'s doc likewise names only two backup reasons. The code is
  the safer one; fix the docs.
- `recovery.rs`, three sites (`plan_snapshot_history`, `preserve_snapshot_history`,
  `preserve_existing_in`): "the platform's session helpers emit through tracing too
  but label save outcomes" and variants. `shepr-platform` has no session helpers and
  emits no persist events.
- `shepr-paths/src/app_paths.rs` (`state_dir`: "the saved layout and history live in
  data_dir") and `shepr-paths/src/profile.rs` ("its saved layout and history"): pane
  history is gone.
- `persist.rs`'s module doc lists the files "by job" and omits `restore`, `error`,
  `actor` and `lock` from the list: a hand-restated file list nothing checks. Drop it
  in favour of each file's own module doc.
- `files.rs` `load`'s parse-error log says "failed to parse session file, ignoring";
  the file is protected and backed up, not ignored.
- `SNAPSHOT_LIMIT`'s "covers an overnight failure" holds only if a copy is made every
  interval; copies are made only on saves whose fingerprint changed, so the 48 copies
  span much longer than 12 hours in practice.
- `publish_private_file`'s doc and the recovery directories resolved from the link
  path while staging uses the target's directory: see POL-003 for the unstated rule.

## CLAIM-022 - Restore and resume doc comments that are false today

Reported by: restore-resume, agent-state, workspace-model.

- `AGENT_ABSENCE_STARTUP_HOLD` (mux limits): "A restored pane holds absence for the
  same interval as agent resume". `DetectorState::new` gives `LaunchKind::Fresh |
  LaunchKind::Restored => None`; only `AgentResume` holds.
- `restore.rs` `restored_terminal` doc: "cwd, label and launch argv: always kept".
  `PaneSnapshot` has no launch argv.
- `PaneStartFailure::ResumeUnavailable` doc: "(no command to run, the pane gone from
  under the attempt)". A plan always has a command, and the reasons that occur
  (`ShellLaunchUnconfirmed`, `CommandSendFailed`) are not named.
- `resume_schedule.rs` `AttemptOutcome::Abandoned` doc: "(PTY could not be opened,
  missing launch env)". The code abandons on a `launch_pane` error and an unreachable
  pane-gone branch; "missing launch env" names nothing.
- `app/events.rs` `decide_pane_exit`: "since history capture leaves an unreadable
  terminal's cached history as it was"; there is no history capture (BUG-027).
- `App::open`: "Restored workspaces get their Git identity (label and status) from
  the first background Git refresh"; a refresh never sets the name.
- `ResumeSchedule.retired`: "plans are minted only by session restore, before the
  first pass. Once set, nothing scans." Nothing enforces it:
  `TerminalState::plan_agent_resume` is a `pub` production method only tests call,
  and a production caller after retirement would plan a resume that never runs.
  Delete it (restore uses `with_pending_agent_resume_plan`) or textlint
  `plan_agent_resume(` outside tests.

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

## CLAIM-024 - Pane lifecycle claims that are false today or held only by review

Reported by: pane-lifecycle.

- `shepr-platform/src/child_io.rs`: "The only place this becomes poll(2)'s int
  milliseconds is `Wait::poll_millis`". False: `shepr-pty/src/launch.rs`
  `accept_loop` computes `wake_ms` from `LAUNCH_PARKED_CONNECTION_TTL.as_millis()` and
  calls `libc::poll` directly; `process.rs` `has_exited` and `stream_wake.rs` pass
  literals. Checkable by a textlint on `libc::poll\(` outside `child_io.rs`.
- `run_child`'s contract ("no allocation, no lock, no destructor, no panic") has one
  bounds-checked index, `plan.envps[index]`, which would panic (unwinding in a forked
  child of a multithreaded process) if `dirs` and `envps` diverged. They cannot today;
  use `get` and `child_exit`, or one `Vec<(dir, envp)>`.
- `coordinate`'s "nothing here indexes unchecked or unwraps" is the safety argument
  for the pane's only publisher; a module-level `#![deny(clippy::indexing_slicing,
  clippy::unwrap_used, clippy::expect_used, clippy::panic)]` on `launch_status.rs`
  outside tests makes it a build fact.
- `PtyIoInbox`'s "never held across a syscall" holds today, by review only.
- `SHEPR_BIN_PATH`'s "set for every pane" (BUG-022) is false today.

## CLAIM-025 - Agent detection claims that are false today or unenforced

Reported by: agent-state.

- `compile_manifest` sets `unknown_is_stable |= rule.state == Unknown`, which counts
  `skip_state_update` rules (state must be unknown) although a skip rule yields
  `AgentDetection::Skip`, not Unknown. So Claude (fallback Idle, two skip rules, no
  Unknown rule) reads as "can report a stable Unknown". Harmless today (unchanged
  content gives the same Skip), but the field's documented meaning ("Whether an
  unchanged input can produce `Unknown`") is wrong; rename or exclude skip rules.
- `AgentOwnership::with_initial_hook_authority`'s "Production code never calls it" is
  true and unenforced (a plain `pub fn` that bypasses arbitration); its only caller
  is a mux test. Make it test-only or construct the authority through a report.
- The ownership module "never runs the manifest engine": true, checkable by a
  textlint forbidding `manifest::` in `ownership/`.

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

## CLAIM-029 - The environment registry claims every variable a shepr process interprets

Reported by: workspace-model.

`EnvVar` is documented as "every environment variable a shepr process interprets",
with a declared kind; `SHEPR_PANE_ID` is only written (`pane/launch.rs`) and read by
hook assets, so its `EnvKind::Text` is a claim nothing exercises; it belongs in
`ChildEnv`. A test that every `EnvVar` variant has a production `env::read*` site
would catch it. `RegisteredEnv` "contains each name once" by convention only: its
variants are public, so `RegisteredEnv::Child(ChildEnv::Shell)` is constructible and
`pane_policy` carries arms for that second spelling. Make the variants private behind
the `From` impls.

## CLAIM-030 - Server lifecycle comments that are false today

Reported by: server-lifecycle.

- `api_service.rs`'s conditional-stop comment names `ServerStopParams` where the
  validation it explains is `ServerStopIfBootParams`'.
- `local_server.rs` `server_daemon_working_dir` names `new_terminal_cwd = "current"`;
  the setting is `terminal.new_cwd`.
- `local_server.rs` `build_server_daemon_command`: the child "gets the
  already-resolved socket target"; it only removes `SHEPR_SOCKET_PATH`, and the daemon
  re-resolves from its own environment.
- `LaunchError::remote_failure_class`: "`launch_with` waits out a daemon that gave way
  to another server rather than failing on it" is given as why `DaemonExit::Clean` is
  `Retry`; `launch_with` only exempts `AlreadyRunning`, and a daemon exiting 0 during
  boot fails the launch at once. The classification may still be right; its stated
  reason is false.
- `shepr_core::env::EnvVar::SheprBuildProfile` speaks of "the socket variables" and
  "the socket overrides", plural; there is one.
- `shepr-paths/src/lib.rs` says both pane markers decide whether an inherited
  `SHEPR_SOCKET_PATH` applies; only `SHEPR_BUILD_PROFILE` does (`SHEPR_ENV` decides the
  TUI refusal).
- `cli/spec.rs` module doc: typed parsers read ids "spelled from shepr-launch's
  `COMMAND_` and `FLAG_` constants"; the `detect` subcommands and options are
  literals.
- `headless.rs` `dispatch_api_request`: "API handlers read each workspace's recorded
  layout area for directional focus, resize steps, layout snapshots and spawn sizes";
  none of those API methods exists (the API is ping, stops, summary, detect, two
  reports).
- `headless.rs` `handle_scheduled_tasks_headless`: "Similar to the former App
  scheduler", "No resize polling needed" (history, not behaviour).

## CLAIM-031 - Remote layer comments that are false today

Reported by: remote.

- `ClientEndpointId::display_label`: "The launch refuses a machine label that names
  the local server, so the two never read alike." Such an entry is skipped, not
  refused (`validated.rs` `is_local_entry`), and its palette becomes the local hue. The
  same doc lists "`local`" as a name `display_label` returns; it never does (only
  `Display` writes `local`).
- `failure.rs` `failed_before_remote_result`: "The bridge and the machine check use
  it"; production calls it nowhere (DEAD).
- `discovery.rs` `known_remote_binary_candidate_script`: "These are checked before
  falling back to `command -v`"; both orderings run `command -v` first (`fleet.rs`
  says so correctly).
- `DiscoverySteps`: "Only `DiscoveryProgress` sequences them"; there is a second
  sequencer (POL-024).
- `DiscoveryProgress` ("without connection sharing each is a cold SSH connect") and
  the client's `ATTEMPT_BUDGET` ("a slow link without connection sharing") reason about
  a configuration production never has (`write_managed_ssh_config` always sets a
  control path and `ControlMaster=auto`). Reword around the master-less first connect.
- `PIPE_DRAIN_GRACE`'s premise, that a ControlPersist master forked by ssh keeps the
  command's stderr or stdout open, is probably stale: current OpenSSH points the
  backgrounded master's stdio at `/dev/null` unless ssh runs with debug logging. If the
  premise holds, every ssh command leaks one blocked `PipeCapture` reader for the
  master's life (up to `ControlPersist=600`), unbounded-ish under a reconnect loop; if
  not, the grace and its plumbing are dead. Check against the OpenSSH the owner runs
  and say which.
- `release_ssh_resources_before_exit`'s doc says "including through
  `std::process::exit`", which `exits-from-main` and the clippy seal make impossible
  outside `src/main.rs`.
- `TeardownRegistry`'s doc ("in a client those owners live on endpoint writer threads
  ... which leaked sockets and config directories") is history; state the invariant.
- `ensure_remote_sibling_build`: "a candidate whose status does not report a sibling at
  all is one that predates the report", a compatibility rationale for older builds that
  `ensure_remote_client_build` has already rejected by build id. Say the `None` arm is
  reachable only if this build omits `server`, or make `server` non-optional.
- `retry_delay`'s doc names "the SSH agent registration worker" as another retry loop;
  no such worker exists. Drop the enumeration.
