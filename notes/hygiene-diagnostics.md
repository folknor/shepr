# Hygiene: diagnostics

Logging and operator-facing text (one channel, one implementation; levels,
identifiers, silence where something happened) and errors (swallowed, stripped of
context, or aborting where a refusal was owed). Filed from the nine-scope hunt;
each entry names the hunts that reported it and says how the fixed form could be
enforced.

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

## DIAG-001 - The `event` / `subsystem` / `outcome` field convention is hand-written per crate and unevenly applied

Reported by: persistence, integrations, save-shutdown.

Each crate writes its own helper for the project's structured fields (persist,
pane, ipc, api, client, server, integration), and many lines skip them:

- `persist/capture.rs`: two `tracing::error!`s ("workspace focus or root has no pane
  record; not saved", "workspace layout and pane records disagree") have
  `workspace` but no `event` / `subsystem`.
- `persist/open.rs`: the partial-restore warn has `dropped_workspaces` and
  `restore_damage` but no `event`, `subsystem` or `path`; the `log_restore` info
  right after has all three.
- `writer.rs` states event literals "are the emitted log schema, so the names stay
  visible at the event site"; `recovery.rs` routes `persist.snapshot` /
  `persist.backup` through `RecoveryKind::event()` in two helpers and spells
  `"persist.snapshot"` inline three more times; `persist.restore` is spelled in
  `files.rs` and `open.rs`. No list of persistence events exists.

Fix: one shared macro or helper owning the field set, and one convention for event
names. Enforceable by a script check (multi-line, so not a single-line textlint)
requiring `event =` in every `tracing::(warn|error|info)!` under the persistence
and lifecycle modules.

## DIAG-004 - Resume outcomes have no channel; a resume that did not happen is mostly silent

Reported by: restore-resume.

Resume counts, suppressed duplicates and dispatches are now logged and reported.
Remaining: after the command is typed, success is never confirmed. When the absence
hold expires in an `AgentResume` pane without the agent appearing, nothing is logged;
the detector has no pane or session identity to log (a boundary comment in mux
`pane/detect/publish.rs` marks it), so the caller has to carry the session reference
and public pane id to that point.

## DIAG-005 - Pane start-failure guidance names nothing the operator can act on

Reported by: restore-resume, workspace-model.

The vague "restart this session" guidance is replaced. Remaining: a resume failure
still names neither the agent, the session reference nor the command, although the
plan was in hand when it was abandoned, so the operator cannot resume by hand; that
needs the plan carried into `PaneStartFailure` by its callers.

## DIAG-006 - A restored shell that fails before forking logs twice

Reported by: restore-resume.

`pane.spawn.failure` (error, mux `runtime/spawn.rs`) and "failed to restore pane"
(warn, `persist/restore.rs`) both log the same failure. Collapse to the launcher's
line once every `launcher.launch` error path is confirmed to log.

## DIAG-010 - "Shepr" is still spelled capitalised in operator-facing text

Reported by: save-shutdown, server-lifecycle.

The logind inhibitor, ready hint and several docs are fixed. Still capitalised:
`shepr-remote/src/machine/executable.rs` (four error texts),
`shepr-client/src/shell/input/mod.rs` and `shepr-protocol/src/message.rs`
("Shepr's limit"), `shell/overlays/context_menu.rs` ("Use Shepr right-click menu"),
`shepr-integration/src/config_edit.rs` (MastraCode hook description, Kimi refusal),
`shepr-integration/src/types.rs`, `config_file/tests.rs`, and comments in
`default-client.toml` / `default-server.toml`. A textlint on `"Shepr` in string
literals would hold it.

## DIAG-031 - Small model fallbacks that hide a broken invariant

Reported by: workspace-model.

`sole_pane_size` still falls back to `GridSize::clamped` rather than
`clamped_pane` (a different minimum). And `mark_shell_projection_dirty` now panics
on revision exhaustion: unreachable, but a panic on the event loop; a saturating
revision that forces a full rebuild would fail safe.

## DIAG-022 - Server lifecycle events that log nothing, log at debug, or omit the cause

Reported by: server-lifecycle.

- The final-save log reports `outcome = "completed"` when saves are stopped or blocked
  on backup and nothing was written; only a frozen final save is distinguished.
- `stop_active_server` logs nothing, so the client log has no record of which boot
  the restart offer stopped, or of a stop that timed out.
- A successful `server.stop` / `server.stop_if_boot` is logged at debug only
  (`MethodTraits` has `mutates_ui: false`; `api_request_completed` raises to info
  only on a non-ok outcome), and `lifecycle.rs` logs "server shutdown initiated"
  without the cause (API stop, conditional stop with which boot, or a signal).
- Invalid API requests (`handle_connection`'s parse-error branch) are answered but
  not logged; oversized or non-UTF-8 lines and read timeouts end at
  `debug!("api connection failed")` with no peer or request id.
- `local_server.rs` logs "server already running" / "server started by another
  client" without the socket path or the build and boot it found.
- The startup restart probe (`src/preflight.rs`) now waits through a starting local
  server for up to `SERVER_READY_TIMEOUT` before the TUI takes the terminal, and
  prints nothing while it waits; after a reboot with a large session the operator sees
  a silent pause.

## DIAG-023 - Server lifecycle errors swallowed or stripped of context

Reported by: server-lifecycle.

- `preflight::local_server_status` reduces every probe failure to a warn and no
  offer, relying on the launch to report it next, which holds only with no machines
  configured (with machines, `tui.rs` prints a notice and continues).
- `read_server_version_line` now logs a failed `kill()` on timeout, but then calls a
  blocking `wait()`, so its cleanup can run past the probe deadline.
- `SshStdioBridge::start_command` uses `thread::spawn`, which panics if the thread
  cannot start; `Builder::spawn` mapped to a local setup error would refuse instead.

## DIAG-025 - The same remote condition is classified three ways

Reported by: remote.

An unresponsive remote server is now a repair on every path. Remaining: the remote
wait's setup failures are plain `CliError::Io` (unclassified) where the bridge maps the same path
and logging setup failures to a `Repair` record (`src/main.rs`
`bridge_setup_failure`), so a host whose logging cannot start shows Unavailable
through the bridge and "Connecting..." through the wait.

## DIAG-026 - Remote failures swallowed or losing their diagnostic

Reported by: remote.

- `hub.rs` `dispatch` ignores `supervisors.request(..)`'s refusal for
  `ConnectMachine` and `RestartMachine`, after the shell has set the entry to
  Starting... or Restarting.... Latent today (the refusal cases do not offer the
  entry); the shell should set the entry only when the request was taken, which
  needs the shell reducers to take the outcome (commented at `hub.rs`).
