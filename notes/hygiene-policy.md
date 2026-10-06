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

## POL-003 - Staging leftovers outside the data directory have no owner to reclaim them

Reported by: persistence, integrations.

The data and recovery directories are now swept of staging leftovers at startup
under the lease, and the session file placement rule is written down in `files.rs`.
Not swept, deliberately, with the reason at the sweep site and in
`atomic_replace.rs`: staging leftovers beside an external symlinked session target,
and leftovers of the detached integration installer in agent directories. Both use
the same staging names, and a server's data-directory lease does not own agent
directories or an external target (servers with different XDG state roots can share
them), so a sweep could delete a live installer's file. Reclaiming them needs a
shared ownership mechanism for those directories first.

## POL-004 - Integration's `AtomicReplace` keeps its own publish path

Reported by: persistence.

Mux and `ssh_metadata.rs` now use the platform's `prepare_private` /
`publish_private`. `shepr-integration`'s `AtomicReplace` still wraps the lower-level
`PreparedFile` itself, because its metadata, permission and deferred-commit policies
are not the private publisher's defaults; adopt the shared preparation where the
policies match. The two best-effort cleanup functions stay separate on purpose (each
logs with its own layer's fields), as commented at both.

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
- `shepr-detect`: `AgentOwnership::with_initial_hook_authority` (now held to its one
  caller by a textlint).
- `shepr-remote`: `pub use failure::SshFailureDiagnostic` (for one test in
  `src/preflight.rs`), and seven `SshFailureDiagnostic` queries (`from_message`,
  `from_local_setup_error`, `is_ssh_process_failure`, `remote_exit_code`,
  `is_transient_network_failure`, `needs_attention`, `failed_before_remote_result`),
  now `#[cfg(test)]` inside the production impl: test-only API on a production type.
  Delete them and assert on `disposition()` and evidence instead.

`shepr-test-fixtures` exists for this; its layering rule does not yet allow
`shepr-agent` (a one-line change). `scripts/check_dead_test_helpers.py` catches the
inverse (test helpers nothing calls) but not production `pub` items only tests call;
extending it to report non-`cfg(test)` public items whose only callers are test code
would enforce this. A textlint banning `\btest_from_pane\b|\bPaneId::from_raw\b`
outside test files and `cfg(test)` regions is the cheaper half.

## POL-015 - The pane spawn path reaches the environment and clock directly, and a timer handle is set by call order

Reported by: pane-lifecycle.

The spawn path reads the process environment (`base_env`) and the clock (read effects,
the reader-exit callback, `decide_after`) directly; the platform crate has the
clock-injection rule, mux's pane code does not (CLAIM-018). `PaneReadEffects::timer_writer`
is a `OnceLock` set after the actor spawns, and the timer path handles the gap by
dropping replies with a warning: documented, not structural. Creating the inbox and wake
pipe first, then spawning the actor with the effects already holding a handle, removes
the window.

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

## POL-026 - Preflight probe locks

Reported by: remote.

- `MachineSshPreflight::check` holds a machine's probe mutex for the whole bounded SSH
  check (up to 25 s); fine because each machine has its own, but the map lock and the
  deadline lock are two more mutexes around what could be a `Vec<MachineProbe>` handed
  to scoped threads by `&mut`, removing all three.

## POL-027 - Some projection invalidation is still manual, and `AppState` fields are still open

Reported by: workspace-model.

The `AppState` reducers now advance the projection revision themselves and
`EndpointEffects` no longer carries a projection flag. What remains: manual
invalidation in `app/mod.rs`, `terminal_titles.rs`, `agent_resume.rs` and
`app/session.rs`; and `AppState`'s `pub(super)` fields, which let callers in `app/`
mutate past the reducers (the reason they stay open is commented beside
`AppState.workspaces`). Making the fields private to `state.rs` forces every mutation
through a named reducer. The 1 s timer rebuild stays regardless, for `/proc` cwd
observations no event reports.

## POL-029 - `RuntimeGeneration::alloc` is a second process-global counter

Reported by: workspace-model.

Pane sizing no longer burns pane ids and `TreePlan` refuses a bad focus or root by
type; pane ids stay process-global on purpose (events and render sources carry no
workspace id), as commented in `layout.rs`. `RuntimeGeneration::alloc` is a separate
global counter with about ten call sites in mux; decide whether it needs to be
global or can be per pane.

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

## POL-036 - Detector ticks run on the shared blocking pool, unbounded overall

Reported by: the blocking-pool fix.

Synchronized-output flushes are now capped and the child watcher's fallback is async,
but `pane/detection_task.rs` still runs each detector tick through `spawn_blocking`.
A tick that stalls (a `/proc` read on a hung mount) holds a pool thread shared with
the rest of the server; bounded by pane count only. Cap it like the flushes, or give
detection its own bounded pool.

## POL-018 - A stale fallback state is presented after hook authority ends

Reported by: agent-state.

The detector's `reset()` (on authority activation) and the end of authority leave
ownership's `fallback_state` at whatever the detector last published before the
authority, possibly long ago. When a session-start replacement clears authority
without an exit, that stale fallback is presented until the next publication (about
one tick). A first attempt reset the fallback on every accepted full-lifecycle report
(`AuthorityEffect::Set`) rather than only when authority activates, and changed what
`hook_authority_overrides_fallback_for_same_agent`,
`omp_hook_authority_overrides_detected_fallback` and
`visible_blocker_does_not_override_full_lifecycle_hook_authority` assert; it was
reverted. Reset only on activation, and decide the tests' expectations deliberately.

## POL-037 - The client writes its ssh metadata cache into the server's leased data directory

Reported by: the wave review.

The ssh metadata cache moved to `<data>/client/ssh-metadata`, inside the data
directory the server holds a lease on and sweeps at startup. Harmless today (the
sweep removes only staging-named files), but the client now writes into a
server-owned tree; a client-owned per-profile directory would keep the ownership
clean.
