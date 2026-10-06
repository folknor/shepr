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

## POL-034 - "Server busy" is refused with two codes, and abandoned requests fill the channel

Reported by: server-lifecycle.

(The wire strictness half is done: seven routes refuse unknown params, with
`pane.report_agent` the one stated exception.) The same condition (the app loop saturated) is refused with
two codes: `EndpointBusy` when app-slot admission is full, and `ServerUnavailable`
("server is busy handling API requests; retry later") when the channel is full, so a
caller retrying on one and giving up on the other behaves differently for one cause. And
`dispatch_to_app_result` leaves a timed-out request in the app channel after releasing its
slot (the request "may still run"), so after timeouts slot admission no longer describes
what is queued: abandoned requests occupy channel capacity (the same 64) that live
requests then meet as `ServerUnavailable`. That is the only way the channel-full branch is
reachable, which no comment says.

## POL-035 - A daemon that never redirected stderr grows the boot log without bound

Reported by: server-lifecycle.

- `launch_with` empties the boot log with `set_len(0)` once its daemon is up; a daemon
  from a different launch that never redirected stderr (its log file could not be
  opened, `ServerReady.log_file_unavailable`) keeps the boot log as stderr for life, and
  nothing caps it once `BOOT_LOG_MAX_BYTES` stops being checked, so its later stderr
  (panics included) grows a tmpfs file without bound. Capping it needs a bounded
  stderr sink; the gap is commented at `launch_with`.

## POL-037 - The client log lives in the server's leased data directory

Reported by: the wave review.

The ssh metadata cache moved to a client-owned per-profile directory, but the client
log is still at `client_log_path(data_dir)`, inside the data directory the server
holds a lease on and sweeps at startup. Move it beside the metadata cache.

## POL-038 - Wave 7 laterals

Reported by: the wave review.

- `shepr_remote::preflight` and the `PreflightSsh` trait are exercised only by that
  crate's tests; production uses `MachineSshPreflight::run` (a public test-only seam).
  `RatioDelta::get` is likewise a new `pub` accessor only tests call, and
  `PtyIoActor::spawn` may now have no production caller.
- The OSC evidence flag is a process-global `OnceLock`: the first server started in a
  process fixes it, and a later one never validates its own value (test binaries).
- `remote-wait-for-server` now classifies every wait failure as Repair, including
  presence-probe IO errors, which is broader than "setup failure".
- With no machines configured, a local status failure in preflight is printed and
  then the launch prints the same refusal again.
- `api.connection.failed` moved from debug to warn; idle or slow clients that hit the
  first-line timeout now warn.
- `pane/launch.rs` tests still use `/run/user/1000/...` literals.
- `every_session_save_status_roundtrips_as_a_unit_enum` lists the variants by hand.
- `reader_exit_callback` in `pane/runtime/spawn.rs` carries two `clock-io-ok` markers
  for one read.
- `api.rs` and `events.rs` detect a projection change by `revision != before`, which a
  saturated revision hides (unreachable in practice; render uses
  `shell_projection_is_current`).
