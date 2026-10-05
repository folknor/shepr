# Hygiene: policy

One rule implemented separately wherever it was needed (retry, timeouts, cleanup,
validation, error classification), ambient dependencies reached from logic that
should have been handed them, shared state whose safety rests on call order,
growth without bound, personal data on disk, and test-only shortcuts production
can reach. Filed from the nine-scope hunt; each entry names the hunts that
reported it and says how the fixed form could be enforced.

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

## POL-001 - Leftovers of the removed lease-only save mode

Reported by: persistence, save-shutdown.

The test-only "hold the lease, persist nothing" mode is gone. Remnants:
`complete_shutdown` still returns a `Result` behind `RunServerError::Shutdown` for a
phase guard the loop has already checked; and `TestApp::persist` /
`HeadlessServer::persist_for_test` (18 call sites) are now nearly no-ops, since
`App::new` already persists on the outputs' signal; they only restart the persister.

## POL-002 - The snapshot cadence mixes the injected clock with filesystem time and re-derives what the writer already knows

Reported by: persistence.

`persist-clock-is-injected` forbids `SystemTime::now()` in `persist/`, and `now` is
injected, but `snapshot_history_decision` compares it with
`std::fs::metadata(latest)?.modified()`, the filesystem's clock at copy time. Tests
reach the cadence only by reading real mtimes back
(`snapshot_interval_uses_supplied_clock` builds `now` from the file's mtime, so its
name is half true) or by `set_times`. The mtime is used deliberately, so a rolled-back
clock recovers after one copy (`snapshot_cadence_recovers_after_clock_rollback_and_restart`).

More broadly, `SnapshotFingerprintCache` stamps and re-parses files (full schema
validation, with logging side effects; BUG-009) to learn the fingerprint of the newest
copy and of the current file, though under the lease the writer is the only process
writing any of them. The four-state `SnapshotHistoryPlan` (`Skip`,
`PreserveBeforeWrite`, `PreserveAfterWrite`, `RetryAfterWrite`), the
optional-replacement decision and the stamp cache all exist to recover facts the
writer discarded. Structural fix: the writer holds `{ on_disk: SavedLayout,
latest_copy: Option<(when, SavedLayout)> }`, initialized once at open from disk and
updated on each publish and copy, with the file's mtime used only for the first
decision after startup. The decision becomes a pure function of that state and
`now`, testable without a filesystem. Also: `SnapshotHistoryPlan`,
`plan_snapshot_history`, `finish_snapshot_history` and `preserve_snapshot_history`
use "history" for the series of recovery snapshots, not pane history; rename (for
example `SnapshotPlan`) so they are not mistaken for leftovers of the removed feature.

## POL-003 - Staging and recovery files: unstated placement and leftovers that accumulate

Reported by: persistence, integrations.

- `snapshot_directory` and `backup_directory` are `path.with_file_name(..)` of the
  session path as configured (the symlink), while staging files and directory syncs
  use the resolved target, so with a symlinked session file recovery copies live in
  the data directory and staging in the target's directory. Consistent with the notice
  (which names the data directory) and possibly intended, but nothing states it, and
  `missing_directory_chain` / `create_private_directory_all` create directories on the
  target side. Write the rule down where `SessionPath` is defined.
- A crash mid-publish leaves a `.shepr-<token>-<seq>.tmp` staging file (up to
  `MAX_SESSION_FILE_BYTES`, 64 MiB) beside the session file (possibly in the user's
  dotfiles tree when the session file is symlinked) or in a recovery directory.
  `publish_private_file`'s doc says such a leftover "is never reused or removed here",
  and nothing removes it anywhere, so they accumulate across crashes. The lease makes a
  startup sweep of `.shepr-*.tmp` in the data and recovery directories safe.
- The integration install thread is detached, so a shutdown during install can leave
  the same kind of staging file in the user's agent directory (process exit does not
  run the `PreparedFile` destructor); nothing reclaims those either.

## POL-004 - One platform publish primitive, three call styles and two cleanup functions

Reported by: persistence.

`shepr-integration` has its own `AtomicReplace` wrapper over
`shepr_platform::publish_file::PreparedFile` (and its own `NotRegularFile`, VAL-005);
`shepr-remote/src/machine/ssh_metadata.rs` builds `PublishOptions` inline;
persistence's `publish_private_file` is the cleanest of the three. A platform-level
`publish_private(target, bytes, mode, PublishTarget)` would serve all three.
Separately, `shepr_platform::publish_file::cleanup` (a warn with `path` and `error`,
no `event` or `subsystem`) and `persist::files::remove_after_failed_publish` (a warn
with `event = "persist.cleanup"`) implement the same "best-effort remove, NotFound is
fine, warn otherwise" policy. Not enforceable beyond review.

## POL-005 - Retry and backoff are spelled per site, with different growth and different reset rules

Reported by: save-shutdown.

- autosave backoff: `Backoff` over the `SESSION_SAVE_*` constants;
- checkpoint backoff: `checkpoint_retry_delay` with its own minimum and a count cap;
- logind reconnect: `Backoff`, with a pending-shutdown override that resets the
  count; on `Ok(())` from `watch_shutdown` (owner change or signal stream end) the loop
  reconnects with no delay and resets `failures`, so a logind or bus that accepts and
  then drops connections repeatedly spins the task with a debug line at most. Apply the
  backoff on both arms, resetting only after a connection has lived some minimum;
- the final save: no retry at all (BUG-014);
- `shepr-api/src/server/listener.rs` `AcceptBackoff` doubles with a literal `2` and
  cannot see the server's `Backoff`.

`BACKOFF_MULTIPLIER`'s "Growth factor of every retry backoff" is therefore false
workspace-wide. Move `Backoff` down a layer (core or platform) for the API listener to
share, or say why that one stays separate.

## POL-006 - The session dirty bit is consumed while saves are disallowed, and kept correct by writers who never meet

Reported by: save-shutdown.

`sync_session_save_schedule` evaluates `take_session_dirty()` before
`allows_saves()`, so while frozen or stopped every mutation's dirty bit is dropped.
`preserves_pane_exit_checkpoint` treats "not dirty" as "no mutation since the
preserved capture", which is false after a freeze. Correctness rests on three other
writers: `resume_session_saves_after_cancel` re-marks dirty on cancel, a frozen final
save writes nothing, and a restart's host checkpoint captures the live state and
discards the preserved layout. `finish_checkpointed_pane_exit_after_event` also writes
`self.state.session_dirty = false` directly (bypassing `take_session_dirty`) on a
comment's argument that no other mutation can interleave, and calls
`autosave.schedule` without checking the policy, unlike `note_mutation`. Structural
fix: a mutation epoch counter that `CapturedLayout` records at capture, so a preserved
layout is authoritative iff the epoch is unchanged; it replaces the dirty bit's double
duty and the direct field write.

## POL-008 - Session writer retirement blocks the runtime thread where the save path is async

Reported by: save-shutdown, persistence.

`retire_session_writer` calls `pending.wait()` and `persister.retire()` (a thread
join) synchronously. It is reached from async `run` (normally with nothing in flight)
and from `HeadlessServer::drop`, which runs inside `rt.block_on`'s future when the loop
errors or unwinds, possibly with a save in flight; the persister's own `Drop` doc warns
about exactly this. Meanwhile `save_session_before_teardown_async` uses
`spawn_blocking`. Today it stalls one worker of a multi-thread runtime while the
process exits, but it is one rule implemented once async and once blocking. Make
retirement async on the `run` path and leave `Drop` as the blocking backstop.

## POL-010 - Test-only shortcuts in production APIs, which nothing reports

Reported by: restore-resume, workspace-model, pane-lifecycle, agent-state, persistence, remote.

Production `pub` items whose only callers are tests:

- `shepr-agent`: `AgentResumePlan::for_command(session, program, args)` (used only by
  the server's `test_support`; it lets any production caller build a plan that types
  an arbitrary command into a restored shell, in a project that deliberately keeps no
  way to drive panes), `AgentResumePlan::args()`, `PersistedAgentSession::from_report`
  and `AgentSource::from_pair`.
- `shepr-mux`: `TerminalState::plan_agent_resume` (a second writer of
  `AgentResumeState::Planned` beside restore's `with_pending_agent_resume_plan`; see
  CLAIM-022) and `TerminalState::set_hook_report_at` (production uses
  `ownership_mut().set_hook_report_at`); `Workspace::test_from_pane` and
  `PaneId::from_raw` (by design, per their docs, but nothing stops a production call);
  `persist::capture` (eager cwd read, used only by server `snapshot_tests.rs`),
  `SessionLoad::into_snapshot`, `CapturedLayout::snapshot()` and
  `PendingSave::channel` (an acknowledged seam).
- pane runtime: `PaneOutputWriter::try_begin`, `PaneOutputWrite::write` (reads the
  clock and swallows a poisoned-core error with `.ok()`), `PaneRuntime::output_writer`
  and `PaneRuntime::with_child_io`. Seams are sanctioned over test features; the
  finding is that `write` and `try_begin` are wider than the seam needs (production's
  reader uses `begin` plus the private `process`).
- `shepr-detect`: `AgentOwnership::with_initial_hook_authority` (CLAIM-025).
- `shepr-remote`: `pub use failure::SshFailureDiagnostic` (for one test in
  `src/preflight.rs`), `pub use ssh_paths::validate_remote_bridge_endpoint_path` (for
  one test in `shepr-client/src/launch.rs`), and `SshFailureDiagnostic`'s
  `from_message`, `from_local_setup_error`, `is_ssh_process_failure`,
  `remote_exit_code`, `is_transient_network_failure`, `needs_attention` and
  `failed_before_remote_result` (DEAD).

`shepr-test-fixtures` exists for this; its layering rule does not yet allow
`shepr-agent` (a one-line change). `scripts/check_dead_test_helpers.py` catches the
inverse (test helpers nothing calls) but not production `pub` items only tests call;
extending it to report non-`cfg(test)` public items whose only callers are test code
would enforce this. A textlint banning `\btest_from_pane\b|\bPaneId::from_raw\b`
outside test files and `cfg(test)` regions is the cheaper half.

## POL-011 - Agent label acceptance differs by path

Reported by: restore-resume.

`ReportOrigin::parse` trims, lowercases and accepts aliases (`" Claude "`,
`"claude-code"`), while `PersistedAgentSession::from_report`, `AgentSource::from_pair`
and the saved `Agent` deserializer accept only the canonical label. With VAL-008 the
label leaves reports altogether and the question disappears.

## POL-012 - The two resume entry points disagree about the work after a pass

Reported by: restore-resume.

`server/headless.rs` (the loop) calls `start_pending_agent_resumes` and syncs pane
focus; `client_views.rs` `finish_shell_workspace_geometry_change` also requests a
recompute from every client. `start_pending_agent_resumes` already marks the shell
projection dirty. Either the recompute is needed on both paths or on neither; one
post-pass helper decides it once. Also, `start_pending_agent_resumes` marks the session
dirty when a pass only launched (the plan stays until settlement, which marks it dirty
again), costing an extra save per resumed agent.

## POL-015 - The pane spawn path reaches the environment and clock directly, and a timer handle is set by call order

Reported by: pane-lifecycle.

The spawn path reads the process environment (`base_env`) and the clock (read effects,
the reader-exit callback, `decide_after`) directly; the platform crate has the
clock-injection rule, mux's pane code does not (CLAIM-018). `PaneReadEffects::timer_writer`
is a `OnceLock` set after the actor spawns, and the timer path handles the gap by
dropping replies with a warning: documented, not structural. Creating the inbox and wake
pipe first, then spawning the actor with the effects already holding a handle, removes
the window.

## POL-016 - Blocking work off the loop is bounded per pane but not overall

Reported by: pane-lifecycle.

`flush_expired_synchronized_output` runs on `spawn_blocking` and, through the
deferred-effect ticket order, can wait behind the PTY actor's OSC 7 `stat` on a hung
mount, holding a tokio blocking-pool thread for as long as the mount hangs; the
watcher's blocking fallback also holds one pool thread per pane for the pane's life. The
pool is shared with the rest of the server. Bounded by pane count, not by anything the
server configures.

## POL-017 - Agent evidence is ordered across two clock samplers, and a derived flag is mirrored by two writers

Reported by: agent-state.

Hook reports are stamped with the loop's per-pass `AppClock` sample, detector
observations with the detector task's own `Instant::now()` before its probe;
`fallback_not_older_than_hook`, `hook_authority_not_newer_than` and
`detector_observation_allows` compare the two. The bias currently favours hooks, so no
wrong outcome was found, but the safety rests on where the loop calls
`refresh_app_clock`, which nothing ties to these comparisons. `lifecycle_authority` is
derived (`full_lifecycle_hook_authority_active()`) and mirrored into an `AtomicBool` by
two writers (`apply_lifecycle_authority_changes` and `install_runtime`), kept fresh by
every `update_terminal_state` marking the pane dirty: correct today, by call order.
Child-controlled data reaching logs (OSC evidence payloads, documented, opt-in,
truncated; the `/proc` comm in `info!("agent changed", process = ..)`) is acceptable and
noted for completeness.

## POL-018 - Agent state laterals: stale fallback after authority ends, and small probe costs

Reported by: agent-state.

- The detector's `reset()` (on authority activation) and the end of authority leave
  ownership's `fallback_state` at whatever the detector last published before the
  authority, possibly long ago. When a session-start replacement clears authority
  without an exit, that stale fallback is presented until the next publication (about
  one tick). Harmless in practice; a reset could also reset the fallback to Unknown.
- `osc7.rs` keeps a `?query` or `#fragment` of a `file://` URI as part of the path.
  Shells do not send them.
- `ProcessProbeResult::process_name` is computed and cloned per probe only for the
  "agent changed" log line; `foreground_group_leader_job` and then `foreground_job` read
  `/proc` twice per probe when the leader is unidentified. Fine at current cadences.

## POL-019 - Per-target integration behaviour is scattered over seven modules instead of carried by the spec

Reported by: integrations.

`targets::install` picks the artifact role by `match target` (`Claude | Copilot |
Devin => Settings`, `Cursor => UpdatedHooks`), inserts Cursor's `version` by
`target == Target::Cursor` (now a positional `cursor_version: bool` to the shared
`install_json`), hard-codes "mastracode hooks file" for any
`HooksRoot::Document` target, and checks OMP against Pi; `missing_agent_directory` has
its own name table; `registration::expected_events` has `matches!(target, Copilot |
Devin | Droid)`; `registry::action_label` special-cases Antigravity;
`registry::agent_directory` special-cases Pi and OMP; `JsonShape::NestedClaude` applies
the SessionStart matcher to every Claude event (correct only while Claude registers one
event). `DirectoryKey` is a second enum restating `IntegrationTarget`. Fix: the spec row
carries role, document root description, extra required keys, presence directory,
matcher source and the "decodes every event" flag; the per-site matches go. Once the rows
carry it, the exhaustive `spec_for` match is the check.

## POL-021 - Three hand-written delivery retry policies in the plugin kits

Reported by: integrations.

`plugin_kit.js` makes one attempt; `extension_kit.ts` retries once in the same queue
slot; `tui_kit.js` and the TUI decoder retry every 500 ms indefinitely while the
selection is current. `bundle.rs` documents the split, but each policy is hand-written
per kit.

## POL-022 - Unbounded growth in long-running agent plugins, and unbounded config reads

Reported by: integrations.

The owner runs agents for days:

- `templates/opencode_family.js` `childSessions` gains an entry per subagent session and
  never drops one (`session.deleted` is a no-op).
- `decoders/opencode_tui.js` `tui()`: `ctx.events` accumulates every event while
  `!ctx.hydrated`. Hydration throws "incomplete session snapshot" whenever
  `session.status` returns a type outside `["busy", "retry", "idle"]`, so a new OpenCode
  status type means hydration never succeeds, `ctx.events` grows for the life of the TUI,
  and `state()` never returns idle. `ctx.deleted` also only grows.
- Config and asset reads are unbounded: `read_config_bytes`, and
  `registry::integration_state_for_path` / `file_matches_asset` use plain `fs::read`
  after an `is_file` check, a second read policy beside `read_config_bytes`' pinned
  regular-file open.

## POL-023 - Hook payloads, including prompt text, are staged in `/tmp`

Reported by: integrations.

Every shell hook copies the agent's payload to `mktemp
"${TMPDIR:-/tmp}/shepr-<agent>-hook.XXXXXX"`. For `UserPromptSubmit` (Codex, Kimi,
MastraCode) that is the user's prompt text. `mktemp` makes it 0600 and the exit trap
removes it, but a SIGKILL leaves it in `/tmp`. Pipe the payload straight into python3
instead of staging it.

## POL-024 - Remote discovery and its stdout parsing each have two or three implementations

Reported by: remote.

`installed_remote_shepr_candidates` (used by `fleet::read_status`) sequences
`path_via_account_shell` then `known_locations` with its own dedupe, outside
`DiscoveryProgress`: two orderings of one candidate list that must agree (the fleet and
the TUI must pick the same `shepr` on a host with two installs), tied by nothing. One
`candidates(steps)` function both call, with `run_remaining` keeping only resume state.
Separately, `parse_client_status_json` and `fleet::parse_overview` take the last line
that parses (tolerating noise after the marker), while
`server_lifecycle::parse_remote_server_status_json` requires the whole trimmed stdout
to be one JSON document: same wrapper, same noise sources, different verdicts. One
`last_json_record::<T>(stdout)` helper.

## POL-025 - SSH option sets are assembled at four sites

Reported by: remote.

`RemoteSsh::command`, `bridge_connection`, `MachineSshConnector::wait_for_server` and
`authentication_command_with_config` each call `ssh_command()` +
`apply_managed_ssh_options` + some of `apply_batch_ssh_options` /
`ssh_options::append_shepr_options` + `-T` + target. The interactive login command
once lacked the connect bound this way; a shared connection-bounds appender now
covers that one option. Fix: one `SshInvocation { mode: Batch | Interactive, .. }` builder
owning `-C`, `-F`, `-S`, the control and keepalive options, the connect bound, `-T` and
the target, formatting the numeric options from `limits.rs` (VAL-062). Enforceable by
making `ssh_command()` private to the builder.

## POL-026 - Remote ambient state: random socket names, a process-global teardown registry, and broad watches

Reported by: remote.

- `SSH_TEARDOWN` is a process-global registry (now only for the temporary SSH config
  directories) whose correctness rests on calling `release_ssh_resources_before_exit`
  once, after the loop, before exit (documented call order, not structural).
- `server_wait` watches the whole runtime directory; on a host that is also a client,
  every lock sidecar and managed config directory created there wakes
  the wait for a pointless presence check. Filter inotify events by the server socket's
  name.
- `MachineSshPreflight::check` holds a machine's probe mutex for the whole bounded SSH
  check (up to 25 s); fine because each machine has its own, but the map lock and the
  deadline lock are two more mutexes around what could be a `Vec<MachineProbe>` handed
  to scoped threads by `&mut`, removing all three.

## POL-027 - Shell-projection invalidation has three mechanisms and no owner, and reducers are bypassed

Reported by: workspace-model.

Some `AppState` reducers advance the projection revision themselves
(`TerminalCwdReported`, `update_terminal_state`, `sync_terminal_titles`); most do not
(`focus_pane`, `commit_pane_split`, `commit_workspace_creation`, `rename_*`,
`move_workspace`, `remove_pane`, `close_workspace`, `swap_panes`, `toggle_pane_zoom`,
`set_pane_input`). For those, the endpoint path relies on `ViewMutation` / `*Outcome` ->
`EndpointEffects::shell_projection_changed` and on
`handle_endpoint_app_command_with_render` marking afterwards; non-endpoint callers
remember by hand (`apply_pane_removal`, `create_default_workspace`,
`handle_git_status_refreshed`, `handle_pane_launch_settled`'s failure arm) or forget
(BUG-049). Every caller then diffs the revision as well. The 1 s timer rebuild in
`render.rs` masks any miss, so no test notices one. Fix: every reducer that changes
projected data advances the revision itself (session-dirty marking already works that
way), and `EndpointEffects` keeps only surface and topology facts.

Callers inside `app/` also bypass the reducers through `pub(super)` fields:
`handle_workspace_create` sets the name with `workspace.set_name` +
`logging::workspace_renamed` instead of `AppState::rename_workspace`; `pane_launch.rs`
writes the terminal cwd and resume state directly; `close_workspace` reimplements
`forget_removed_panes`. Each mutation's bookkeeping (dirty marks, logs, authority drain)
is re-decided per site. Making the fields private to `state.rs` forces every mutation
through a named reducer.

## POL-028 - Label validation is decided at different layers per command

Reported by: workspace-model.

Workspace rename takes a validated `Label` at the endpoint; pane rename passes
`normalized_user_label(...)` (a `Label` turned back into `String`) to
`AppState::rename_pane(Option<String>)`, which compares raw strings, and
`TerminalState::set_manual_label` re-validates with `Label::new`. The reducer should take
`Option<Label>`.

## POL-029 - Layout logic reaches process-global id allocators

Reported by: workspace-model.

`WorkspaceChrome::sole_pane_size` calls `TileLayout::new()`, which calls
`PaneId::alloc()` on the process-wide counter, just to measure a one-pane layout; every
workspace creation, every restore fallback and the `prepare_split` fallback burn an id,
and id sequences in tests depend on how many sizes were computed. `PaneId::alloc` is also
called in `TreePlan::build`, `prepare_split` and `prepare_workspace`, and as a deliberate
error-forcing value (`ids.get(&self.focus).copied().unwrap_or_else(PaneId::alloc)` in
`TreePlan::build` burns an id to make `from_saved` fail). `RuntimeGeneration::alloc` is a
second global counter. Compute the one-pane content directly, and make the
error-forcing trick an explicit `TreeRejection`.

## POL-030 - Every successful launch requests a Git identity refresh with rediscovery

Reported by: workspace-model.

`handle_pane_launch_settled` calls `request_git_identity_refresh` (which also forces
repository rediscovery) for every successful launch, including every pane of a restore,
so a restore of N panes queues N rediscovery requests (coalesced by the scheduler, but
each invalidates the worker cache via `mark_due`). Policy decided at the site rather than
by the scheduler.

## POL-031 - Workspace model laterals

Reported by: workspace-model.

- `right_click_passthrough` is per pane, projected and not saved, so it resets on every
  server restart; nothing documents either way (consistent with `set_pane_input` not
  marking the session dirty).
- `mark_focused_pane_cells` with gaps off marks the cell one past the pane's right and
  bottom edge even for an outermost pane; harmless only because the grid ignores
  out-of-area cells.
- `WorkspaceSet` looks workspaces up by linear scan and `projection_input` calls
  `workspace_info(&ws.id())` per workspace, re-scanning: quadratic in workspaces on every
  projection rebuild. Small numbers today.
- `close_workspace` reports removed panes in hash order while `remove_pane` reports
  layout order; callers only shut runtimes down, so no effect yet.
- The global `LayoutEpoch` starts at 0 for every tree, restore included; fine because a
  client's epoch is always from the current boot's projection, but a client reconnecting
  to a new boot with a cached epoch could match a different tree by accident.

## POL-032 - IO-error classification is implemented four times with different tables

Reported by: server-lifecycle.

`shepr_platform::ipc::classify_stream_error` (stop, status, disconnect notices),
`failure.rs` `is_link_error_kind` (which treats `ConnectionReset` as offline and
deliberately differs), `LaunchError::remote_failure_class` (its own six kinds for
Retry), and `stop.rs` `stop_request_error_allows_wait`. Each is defensible alone;
together a `ConnectionReset` is "peer gone, retry", "offline" and "retry" depending on
the caller. Name the questions (link-level reachability, peer left, retry the launch) and
give each one table.

## POL-033 - Deadlines and request paths are decided per call site

Reported by: server-lifecycle.

`ensure_running`, `wait_for_overridden_server` and
`wait_for_server_socket_to_settle_until` each compute deadlines from `Instant::now()` +
timeout; stop computes three of its own (BUG-059). `ApiClient` has two request paths with
different policies: `request_value_with_timeout` (connect bound, write timeout, a read
deadline that starts after the write) used by `request` / `ping`, and
`request_value_until` (one shared deadline) used by launch and stop. Ambient reads in the
CLI: `main.rs` `random_nested_message` reads the wall clock and pid for randomness, and
`cli/status.rs` reads `SystemTime::now()` twice for one report (`overview.now` and the
machines section), so local and machine uptimes can be computed against different instants.

## POL-034 - Wire strictness and "server busy" are decided per type and per branch

Reported by: server-lifecycle.

Every server-route params type now refuses unknown keys, while
`PaneReportAgentParams` deliberately accepts extra keys (tested); no single stated rule
says which API types are strict. The same condition (the app loop saturated) is refused with
two codes: `EndpointBusy` when app-slot admission is full, and `ServerUnavailable`
("server is busy handling API requests; retry later") when the channel is full, so a
caller retrying on one and giving up on the other behaves differently for one cause. And
`dispatch_to_app_result` leaves a timed-out request in the app channel after releasing its
slot (the request "may still run"), so after timeouts slot admission no longer describes
what is queued: abandoned requests occupy channel capacity (the same 64) that live
requests then meet as `ServerUnavailable`. That is the only way the channel-full branch is
reachable, which no comment says.

## POL-035 - Stop and launch edge cases: probe timeouts read as gone, and an uncapped boot log

Reported by: server-lifecycle.

- `status_probe_has_no_answer` treats a `TimedOut` probe as gone. During a stop, a
  server whose 64 ingress slots are full answers no probe within 250 ms and reads as
  gone; the later lease and socket waits catch it, so the stop does not report success
  falsely, but it reports `LeaseHeld` or `TimedOut` for a server that was merely busy.
- `launch_with` empties the boot log with `set_len(0)` once its daemon is up; a daemon
  from a different launch that never redirected stderr (its log file could not be
  opened, `ServerReady.log_file_unavailable`) keeps the boot log as stderr for life, and
  nothing caps it once `BOOT_LOG_MAX_BYTES` stops being checked, so its later stderr
  (panics included) grows a tmpfs file without bound.
- `stop --all` stops the local server unconditionally while every remote one is stopped
  by boot. AGENTS.md states exactly this, so it is not a defect, but a server that
  replaced the local one between `status` and the stop is stopped without being named.
