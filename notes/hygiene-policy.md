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

## POL-035 - A daemon that never redirected stderr grows the boot log without bound

Reported by: server-lifecycle.

- `launch_with` empties the boot log with `set_len(0)` once its daemon is up; a daemon
  from a different launch that never redirected stderr (its log file could not be
  opened, `ServerReady.log_file_unavailable`) keeps the boot log as stderr for life, and
  nothing caps it once `BOOT_LOG_MAX_BYTES` stops being checked, so its later stderr
  (panics included) grows a tmpfs file without bound. A launcher-side cap cannot
  work (the launcher exits; a pipe needs a draining owner; an rlimit hits unrelated
  writes). The fix is server-side: when file logging fails, the server redirects its
  own stderr to a bounded sink. The gap is commented at `launch_with`.

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

## POL-039 - Wave 8 laterals

Reported by: the wave review.

- `shepr-test-support` `hook_capture.rs` still sets `TMPDIR` for hooks and documents
  why, though no shipped hook stages files any more.
- `shepr-server` `agent_report_test_support.rs` validates `ServerConfig::default()`
  through the real `validate`, so it depends on the host `SHELL` / `PATH` resolving.
- The client log moved to the client state directory; existing installs keep an
  orphaned `shepr-client.log` and its rotations in the old data directory.
- `ApiClient::request_value_with_timeout` now starts the read budget before connect;
  check `ORDINARY_RESPONSE_TIMEOUT`'s margin over the server bound still holds when
  connect is slow under a full backlog.
- `Autosave::is_due` survives only as a `#[cfg(test)]` method on a production type.
- "Current implies a no-op install" is still unproven: nothing catches a target that
  reads Current while an install would rewrite different bytes (Cursor's inserted
  `version` was the example).
- A non-regular object at an integration asset path is now a `NotRegularFile` error
  instead of reading as not installed.
