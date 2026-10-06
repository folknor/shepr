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

## POL-041 - The structured log vocabulary is not constrained

Reported by: the wave 11 reviewer.

Every info, warn and error call now goes through `structured_log!`, and
`scripts/check_structured_logs.py` refuses direct ones. The names those calls carry
are free text:

- Failure outcomes are spelled `error` almost everywhere, but the final save's info
  logs `failed` (documented so in `reference/session-save-shutdown.md`), so a failed
  final save logs two `persist.save` events with different failure outcomes. Success
  is spelled both `ok` and `completed`. `$outcome:expr` accepts any expression, so
  nothing holds outcomes to a small set; an outcome enum or a closed list of idents
  would.
- Subsystems that mean two things: `terminal.*` is the host terminal in the client
  and a pane's terminal in mux; `client.*` is the client process in shepr-client and a
  server-side connection in shepr-server and platform; `client.connection` in
  `shepr-client` `launch.rs` overlaps `endpoint.connection` in the same crate;
  `endpoint.response_encode` in the server's `client_commands.rs` uses the client-side
  name. `shutdown.pane_teardown` and `shutdown.client_flush` sit under `shutdown`,
  otherwise the logind host shutdown. `blit.frame_encode` is a one-event subsystem.
- Failure fields keyed other than `error`: `%failure` (`pane.launch` in
  `launch_status.rs`), `%reason` (`surface.patch` in `render_stream.rs`,
  `client.resize` in `client_transport.rs`). The textlint for the `error` key only
  catches `err`.

## POL-040 - Wave 9 and 10 laterals

Reported by: the wave 9 and 10 fixers and reviewers.

- `shepr-agent` `PresentedAgentState::label` spells "idle", "working" and "blocked" by
  hand beside `AgentState`, which gets the same spellings from `named_enum!` in the
  same file; declaring it through `named_enum!` would drop the duplicate table (check
  the macro's serde and Display behaviour first).

- `shepr-mux` `pane/runtime.rs` still has a `/run/user/1000/shepr-test.sock` literal.
- `shepr-mux` `pane/terminal/backend.rs` `process_pty_bytes` is a clock-reading
  production helper that only tests call.
- `no-borrowed-process-stand-ins` now matches `command_in_scratch("dbus-daemon", ..)`
  only; other programs spawned through `command_in_scratch` (Git in
  `app/git_refresh.rs` tests, `/bin/sleep` in mux `pane/launch_status.rs` tests) are not
  matched. Broaden the rule to every listed program through that helper and audit the
  markers.
- A neighbouring API deadline test still runs on a 30 ms real deadline (it asserts the
  outcome, not elapsed time).
- Integration hook commands now resolve their hook directory at run time
  (`"${VAR:-$HOME/...}"`, `exec sh "$hook_dir/..."`), which assumes every agent runs
  the command through a POSIX shell; the old `sh '<path>' action` only needed word
  splitting. No test runs a registered command through a shell, so an agent that
  splits and execs the command itself (Codex, Kimi, Grok, MastraCode, Devin, Cursor
  are the ones to confirm) would silently stop reporting. Confirm per agent, or add a
  test running one target's command under `sh -c` with a stand-in hook.
- `command.rs` `directory_setup` repeats `shepr-core` `env.rs`'s directory rules in
  shell, held in step only by a comment; no parity test. Its Pi, OMP, OpenCode and Kilo
  arms exist only for exhaustiveness (those targets register no command).
- `hook_command` takes `managed_assets(target).next()...unwrap_or_default()`; the
  default is dead and would silently produce `exec sh "$hook_dir/"`. Read
  `spec_for(target).primary_asset.path` directly.
- Integration `tests.rs` `kimi_hook_command(_hook_path, action)` ignores its path
  argument.
- The integration `lib.rs` overview lost why the generated assets are committed (bun
  and server contract tests run them from disk) and the OpenCode TUI selection
  report's seq-unit id note.
- Install and status now share subset matching, so a shepr hook the user disabled with
  an extra field such as `"disabled": true` reads as Current and stays disabled.
- Nothing in shepr-launch tests a `server_stop_completed` answer carrying an error (the
  `FinalSaveFailed` path).
- `ServerStopError::is_boot_mismatch` is true for `FinalSaveFailed` wrapping a boot
  mismatch, so that combination exits 3 (replaced), not as a save failure.
- Downgrade only: an older client stopping a newer server reads `server_stop_completed`
  as a protocol error though the server stops; and a restart-offer stop returning
  `FinalSaveFailed` says "could not stop the local server" though it stopped.
- If `HeadlessServer::run` errors before the final save, `complete_final_save` never
  runs, and a waiting stop request gets an empty answer at process exit and counts as
  accepted. Completing it with an explicit error from `release_socket_after_save`
  would close this.
- `launch_with` empties the boot log (`set_len(0)`) once the server answers as running,
  usually erasing the ready notice a server writes there when its log file could not
  be opened, so the operator never learns there is no server log. Surface the boot log
  before emptying it, or keep it when the server reported no log.
- `shepr-core` `RatioDelta::get` was removed as test-only and then put back to
  compile `shepr-server` `app/api/panes/tests.rs`, its one remaining caller; that test
  can compare deltas without a production accessor.
- `ServerStopSignal::wait_for_final_save` has no timeout; a server that dies before
  its final save leaves the stop connection thread blocked until exit.
- `shepr-remote` `preflight.rs`: the test-only `check_concurrently` duplicates the join
  and panic-resume block of `MachineSshPreflight::check_round`.
- The endpoint choice is now guarded three times (a private access token,
  `pub(in crate::endpoint)` transitions and the textlint); one would do.
