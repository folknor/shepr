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
- The host-shutdown lifecycle lines carry no `event`, `subsystem` or `generation`
  (DIAG-007).

Fix: one shared macro or helper owning the field set, and one convention for event
names. Enforceable by a script check (multi-line, so not a single-line textlint)
requiring `event =` in every `tracing::(warn|error|info)!` under the persistence
and lifecycle modules.

## DIAG-002 - A published-but-not-durable save logs as a failed save, twice

Reported by: persistence.

`finish_save` logs `NotDurable` through `session_save_failed` at error level with
"failed to save session", though the file was saved and only the directory sync
failed. The server then logs `warn!("session save failed")` for the same event
without the path. Every save failure produces two lines at two levels, one with the
path and one without. Give `NotDurable` its own outcome (`outcome = "not_durable"`,
"session saved but not confirmed durable") and let one layer log.

## DIAG-003 - A broken snapshot directory logs two warnings on every save, indefinitely

Reported by: persistence.

If `session-snapshots` is unreadable (a file in its place, as in
`snapshot_failure_does_not_block_primary_save_and_clear`), `plan_snapshot_history`
fails, returns `RetryAfterWrite`, and `preserve_snapshot_history` fails again after
the write: two `persist.snapshot` warnings per autosave for the life of the boot.
Rate-limit, or log once per state change in the writer.

## DIAG-004 - Resume outcomes have no channel; a resume that did not happen is mostly silent

Reported by: restore-resume.

What the operator learns today:

- resume disabled by config: nothing (expected, but no info line);
- a duplicate session suppressed (`pane_restore_startup`): no log, no notice; the
  second pane starts as a plain shell and loses its session;
- an invalid saved session dropped at decode: a warn with no workspace or pane
  (BUG-009);
- a resume that launched: nothing, so when an agent does not come back there is no
  log to compare against;
- the `persist.restore` summary (`open.rs` `log_restore`) reports a workspace count
  and outcome, not how many resumes were planned or suppressed.

And after the command is typed (`send_resume_command` `Ok`), success is never
confirmed: the plan is cleared, and the only trace is the seeded idle agent, which
the detector withdraws after `AGENT_ABSENCE_STARTUP_HOLD` (30 s) if no agent
appears. A shell rc that discards typeahead, an agent rejecting the session, or a
missing executable all end with the agent gone from the sidebar and nothing logged.

`SessionRestoreNotice` reaches every client but covers only layout loss. Fix: a
resume summary (planned, suppressed as duplicate, abandoned with reason) logged
once and, for failures, carried in the restore notice; and when the absence hold
expires in an `AgentResume` pane without the agent appearing, log it with the
session reference and the pane's public id. Not mechanically enforceable.

## DIAG-005 - Pane start-failure guidance names nothing the operator can act on

Reported by: restore-resume, workspace-model.

`terminal/state/mod.rs` `PaneStartFailure::guidance`: "Could not resume the saved
agent. Restart this session." and "Pane directory is unavailable. Restore the
directory and restart this session." "This session" names nothing restartable
(there is a server restart: `shepr stop`, then `shepr`). The resume failure names
neither the agent, the session reference, nor the command, although the plan and
`plan.to_shell_command()` were in hand when it was abandoned, so the operator
cannot resume by hand. The text is shown in placeholders and `detect` errors and is
assembled at the site, not by `shepr-launch`'s guidance module, which owns how
shepr names the commands that reach a server. See DIAG-020.

## DIAG-006 - Log lines in restore and resume missing the identifiers needed to act, or doubled

Reported by: restore-resume.

- `schema.rs` "ignoring invalid saved agent session": no workspace, pane or agent
  (BUG-009).
- `restore.rs` `restored_terminal`, "preserving unavailable restored pane": cwd and
  reason, no pane.
- A session path resolve error now carries the path in its text, and the restore
  warning also logs `path = ...`, so the line spells the path twice.
- `agent_resume.rs` "failed to start shell for deferred agent resume": pane
  (internal id) and agent, no workspace, public id or session.
- A restored shell whose launch fails before forking logs `error!("failed to
  restore pane")` in `SessionRestorePlan::launch` and then `warn!("preserving
  unavailable restored pane")` for the same pane; the same failure for a resume
  launch is one `warn!`, and a child-reported launch failure (`launch_status.rs`) is
  a `warn!`. Pick one level for "a pane could not start" and log it once (see
  DIAG-012 for the pane-lifecycle side of the same rule).

## DIAG-007 - Host-shutdown transitions log three or four untagged lines and some none

Reported by: save-shutdown.

Per warning, the monitor logs `event = "host.shutdown.request"` with `generation`;
`freeze_for_host_shutdown` logs "host shutdown announced; checkpointing the session
and freezing saves" on its first call and again on the call that takes the result,
because the function is re-entered. The lifecycle lines carry no `event`,
`subsystem` or `generation`, and say "checkpointing" even when the session does not
persist. Cancellation logs twice (the monitor's "host shutdown cancelled", then the
lifecycle's "...; resuming session saves"); a cancellation landing in
`HostShutdownWarning` logs nothing from the lifecycle. An exhausted host checkpoint
warns twice: `finish_session_save`'s "host shutdown checkpoint failed repeatedly"
and the lifecycle's "...; releasing the delay lock", and the second is false when
no lock exists (the monitor connected after preparation began, or no monitor runs).
Fix: one lifecycle line per transition (request, freeze with outcome, cancel,
restart), each with the generation and the monitor's `event` / `subsystem` scheme;
the exhausted-checkpoint warn kept only in the lifecycle, which knows whether a
lock exists.

## DIAG-008 - Save failure and shutdown lines omit what failed and what remains

Reported by: save-shutdown.

- `"session save failed"` has `error`, `failures`, `retry_ms`, but not the save
  kind (autosave, pane-exit checkpoint and its generation, host checkpoint, final
  save) or the data directory; a reader cannot tell whether an exited pane is held
  or the shutdown checkpoint is retrying. Add `kind` and `generation`.
- No line says a final save happened: the log goes from "completing server
  shutdown" to "headless server exiting"; only a failure appears, with retry
  wording (BUG-014). Add an info line with outcome and duration.
- `"pane session teardown did not finish before server exit"` gives no count and
  no pane ids; `PaneTeardownTracker::wait` returns only a bool. Return the
  unfinished ids and log them.
- While a host-shutdown freeze is in force no client is told anything; by design,
  but a cancelled shutdown that never thaws (BUG-012 delays freeze and cancel
  alike) would be invisible.

## DIAG-010 - The product is spelled "Shepr" in operator-facing text, and the local server is called "Local"

Reported by: save-shutdown, server-lifecycle.

`take_inhibitor` registers who="Shepr", why="Save terminal workspace layout", shown
by `systemd-inhibit --list`; `bootstrap.rs` says "Shepr TUI"; `shepr-remote/src/host.rs`
says "remote Shepr server socket"; elsewhere the product is `shepr` (the window
title is `shepr: <label>`). `failure.rs`'s module doc says "the Local server" and
`tui.rs`'s doc "the Local endpoint", although AGENTS.md says the local server is
never named "Local". A textlint on `"Shepr` in string literals would hold it.

## DIAG-011 - Pane spawn and reap failures are logged twice, at different levels, or not at all

Reported by: pane-lifecycle.

- `PtySetup::start` logs `error!("failed to spawn shell")` on a `spawn_pty` failure,
  then `agent_resume.rs` logs `warn!("failed to start shell for deferred agent
  resume")` for the same event; the split path (`api/panes.rs`) logs nothing and
  returns the text to the client; an actor-startup failure (the other `Err` from
  `PtySetup::start`) is not logged at the mux level at all. One site, the launcher,
  should log once with pane, kind, cwd and stage.
- Reap failures are `error!` in the watcher (`pane_exit_failed`) and `warn!` in
  `UnreapedChild::drop` and `reap_on_detached_thread`: one class of event (a
  possible zombie), two levels.
- `ProcessHandle::open` logs at `error!` for every non-ESRCH failure, so a teardown
  scan under fd exhaustion logs one error per process in `/proc`.

## DIAG-012 - Launch settlements and cwd fallbacks leave no trace

Reported by: pane-lifecycle.

A `Launched` settlement is not logged, so a launch that fell back from its
requested directory to `HOME`, the passwd home or `/` leaves no trace of which
candidate it entered or why candidate 0 failed (the child does not send that errno;
only total failure is reported). `pane_launch.rs` stores the fallback cwd and no
notice says so. `pane_spawn_started` logs rows, cols and the
scrollback budget (constant per server) but not the launch kind, cwd or shell.

## DIAG-013 - Child setup failures are indistinguishable from the shell's own exit

Reported by: pane-lifecycle.

Every pre-exec step failure exits 126 and the watcher logs only "pane child exited"
with the status; a shell that itself exits 126 reads the same. Failures after the
status socket is connected (the `sigprocmask` reset) could send a record and do
not.

## DIAG-014 - PTY and actor errors lose their stage and kind

Reported by: pane-lifecycle.

- `open_pty_with_geometry` returns bare `io::Error`s from five steps (open
  `/dev/ptmx`, `grantpt`, `unlockpt`, `TIOCGPTPEER`, `TIOCSWINSZ`); the operator sees
  "failed to spawn shell: Inappropriate ioctl for device" with no stage. Wrap each
  with its step.
- `PtyIoActor::spawn` maps the thread-spawn error through
  `io::Error::other(err.to_string())`, dropping its kind.
- `PaneChild::kill` turns a failed `pidfd_send_signal` into `last_os_error()` read
  after `ProcessHandle::signal` returned, correct only because nothing runs between;
  `signal` should return the `io::Result`.
- The mux terminal read collapses a `shepr_vt::ReadError` into `None`, so the
  `detect capture` / `detect explain` read-failure error can say only that the screen
  read failed, not why.

## DIAG-015 - Thread names exceed the kernel's 15-byte limit and are truncated

Reported by: pane-lifecycle.

`shepr-pane-{id}-teardown`, `shepr-pty-{id}`, `shepr-launch-status` and
`shepr-launch-reaper` exceed or approach the limit; `pthread_setname_np` truncates,
so every teardown thread shows as `shepr-pane-NN-t` and the reaper as
`shepr-launch-re`.

## DIAG-016 - A panicking detection tick ends detection for the pane, silently

Reported by: agent-state.

`DetectionTask::run` returns on `Err(JoinError)` after `warn!(?error, "pane
detection tick failed")`, which has no pane id. The pane keeps its last published
state forever (Working stays Working on the sidebar) and its process exit is only
learned from the child watcher. A panic in a regex or a `/proc` reader should
restart the task with a fresh `DetectorState`, or publish Unknown and mark the pane,
and log at error with the pane id.

## DIAG-017 - Agent-state log lines: two keys for one id, levels that disagree, missing fields, silent changes

Reported by: agent-state, restore-resume.

- The `pane` field means two identifiers: `restore.rs` logs `pane = %public_id,
  pane_id = %pane_id`, while `agent_resume.rs`, `pane_launch.rs`,
  `launch_status.rs`, mux `publish.rs`, `detection_task.rs` and the server's
  `admit_hook_outcome` log `pane = %pane_id` (internal), and
  `handle_pane_report_agent_session` logs `pane_id = %params.pane_id` (the public id
  string). Give the identifiers distinct field names; a textlint on `pane = %`
  followed by a `pane_id` binding would hold it.
- An unknown session start source from a bundled hook is `warn!`, but a bundled
  hook's report rejected as `InvalidSession`, `MissingSession`, `MissingSequence` or
  `UnrecognizedStart` (each an integration bug, since only shepr's own hooks can
  report) is `debug!`. Routine races (`OutOfOrder`, `CrossTalk`, `RetiredSession`,
  `LifecycleGate`) belong at debug, contract violations by shepr's own assets at
  warn; a `HookRejection::is_integration_fault()` makes that one decision.
- The rejection log omits `seq` and the session ref, which are what match it to the
  hook that sent it.
- A parked start expiring is dropped without a log; a hook authority withdrawn
  because the detector reports another agent (`transition_detection`'s clear) is not
  logged, though it changes the sidebar.
- OSC evidence capture needs both `SHEPR_DEBUG_OSC_EVIDENCE=1` and a `SHEPR_LOG`
  filter admitting debug for shepr_mux; with only the first, nothing is logged and
  nothing says why. Log at info, or say so in the variable's doc.

## DIAG-018 - A manifest that fails to compile degrades its agent to Unknown forever

Reported by: agent-state.

`bundled_manifest` logs `error!` once and the agent's panes report Unknown forever.
A test compiles every bundled manifest, so this cannot ship today, but the runtime
path treats an impossible state as a soft degrade. Since detection changes ship only
as new builds, it could be a server startup failure (or an `expect` justified by the
test).

## DIAG-019 - Integration install logging: free text with baked-in paths, failures logged twice, noise, and silence

Reported by: integrations.

- `ArtifactRole::install_message` builds "installed claude integration hook to
  /home/.../shepr-agent-state.sh" and `install_present_integrations` logs it as
  `info!(integration = label, "{message}")`; path, role and verb are not fields, so
  no query can select "which files did shepr rewrite". Log `role`, `path`,
  `integration` as fields with a fixed message.
- A status or install failure is logged by `logging::integration_action` at info
  with `outcome = Failed` and no error text, and again by
  `install_present_integrations` at warn with the error.
- An info "integration action finished" status line is logged per present agent per
  launch even when nothing is done.
- `targets.rs` names a binary spelling inline in the OpenCode V2 notice ("start
  opencode2 once and the next shepr server launch registers it").

## DIAG-020 - Operator text is assembled where failures are detected, outside the guidance module

Reported by: server-lifecycle, remote, restore-resume.

`shepr_launch::guidance` owns operator text and spells the entry point per profile,
yet: `local_server.rs` (`unresponsive_error` appends its own "If that fails, stop
the server process manually"; `running_build_mismatch`, `no_server_at_override`,
`boot_failure`), `stop.rs` (`TimedOut` tells the operator to SIGKILL by name, worded
differently from the other manual-kill instruction), `cli.rs` `ensure_server_build_matches`, `cli/error.rs` (`Usage`,
`Nested`), `preflight/words.rs`, `tui.rs` `local_startup_notice`, `discovery.rs`
(`client_build_mismatch`, `ensure_remote_sibling_build`, "install or update it there
manually and retry"), `supervisor.rs` `connect_once` ("the local server is
unavailable; start it to reconnect"), `shell/endpoints.rs` `machine_entry` (now
build-aware, but still worded at the site), `fleet.rs` `stop_plan` ("stop it on that
host"), and `PaneStartFailure` (DIAG-005). A dev server ready notice and a dev
client login entry that both named the release `shepr` were what this produced. Structural
fix: typed failure causes whose wording lives in one module. Not mechanically
enforceable short of a textlint on imperative operator verbs.

## DIAG-021 - CLI output: raw escapes regardless of `NO_COLOR`, and JSON envelopes printed to a human

Reported by: server-lifecycle.

- `CliError::Nested` prints raw `\x1b[1m` / `\x1b[2m` escapes unconditionally,
  although `shepr man` honours `NO_COLOR` and a non-terminal stdout.
- `CliError::ServerStop` prints a JSON envelope (`{"error":{...}}`) to an operator's
  stderr for an ordinary "no server running" stop; the human `shepr stop` and the
  SSH caller share one rendering, though the SSH caller reads only the exit code.
- `ApiErrorCode`'s `BuildMismatch`, `ServerNotRunning`, `AgentExplainFileReadFailed`
  and `ServerStopFailed` are never produced by a server; the CLI fabricates
  `ErrorResponse` envelopes with them (`cli.rs` `ensure_server_build_matches`,
  `map_server_not_running_or_io`, `cli/error.rs` for stops). The wire vocabulary
  carries CLI-local failures.

## DIAG-022 - Server lifecycle events that log nothing, log at debug, or omit the cause

Reported by: server-lifecycle.

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

## DIAG-024 - Remote supervision logs routine events at warn and leaves failures silent

Reported by: remote.

- A remote bridge exiting with the `no-server` record (the normal state of a
  machine with no server, once per backoff cycle) logs `warn!("remote SSH bridge
  failed")` in `bridge.rs`; every lost connection, including a deliberate
  `shepr stop` on a machine, logs `warn!("endpoint transport failed")` in `hub.rs`
  `reconcile`. Meanwhile a failed attempt leaving the machine Offline or Reconnecting
  logs nothing (`attempt_failed` warns only for Attention), and so do scheduling a
  wait for a server, a wait ending, and an operator Connect or Restart being taken.
- `attempt_failed`'s warn and the bridge's warn log the same failure twice at two
  layers with different fields.
- `handshake.rs`: `info!("endpoint handshake succeeded")` has no endpoint id,
  generation or boot, so with several machines connecting the log cannot say which.
- Remote-host lines (`relay.rs` "SSH bridge upload failed", "SSH bridge failed to
  half-close the server socket"; `process.rs` `PipeCapture::finish` "ssh pipe is
  still open...") carry only the error; every client's bridge on that host writes
  to the same log, so a line cannot be tied to a client, bridge process or command.

Fix: one level per class (state transitions at info, attention at warn) and every
supervisor transition logged with endpoint, generation and mode.

## DIAG-025 - The same remote condition is classified three ways

Reported by: remote.

A remote server that does not answer: the attach bridge (`host.rs`
`attached_server_status`) reports `RemoteFailureClass::Repair` (needs attention);
the Restart path's `parse_remote_server_status_json` returns a bare
`io::Error::other` (unclassified, so `Retry`, "Connecting..."); `fleet::stop_plan`
returns another bare `io::Error::other`. Same fact, three operator outcomes. Build
it once as `EndpointFailure::remote_repair(..)`. Related: the remote wait's setup
failures are plain `CliError::Io` (unclassified) where the bridge maps the same path
and logging setup failures to a `Repair` record (`src/main.rs`
`bridge_setup_failure`), so a host whose logging cannot start shows Unavailable
through the bridge and "Connecting..." through the wait.

## DIAG-026 - Remote failures swallowed or losing their diagnostic

Reported by: remote.

- `server_wait.rs`: `DirectoryWatch::new(dir).ok()` drops the inotify error (an
  exhausted `max_user_watches` is common on hosts running many agents), so the wait
  degrades to a 2 s poll for its hour, silently; `Err(_) => ServerSeen::Settling`
  turns a `server_presence` IO error (EACCES on the runtime directory) into a 500 ms
  spin for an hour, also silent. Log once per spell, and treat a persistent presence
  error as a failure of the wait.
- `bridge.rs`: a failing `prepare_remote_bridge_stream` is logged and `continue`d;
  the stream is dropped, the client reads EOF, waits the full
  `BRIDGE_FAILURE_REPORT_TIMEOUT` on `reported_failure` (nothing was sent), and shows
  "connection closed before the endpoint finished connecting". Send it through the
  failure channel like every other bridge error.
- `ssh_metadata.rs` `load_metadata` swallows everything (EACCES and ELOOP included)
  via `.ok()?` on `symlink_metadata`, `open`, `read` and the JSON parse, contrary to
  the clippy seal's stance. It is a disposable hint, but an unreadable cache should
  log once with its path.
- `hub.rs` `dispatch` ignores `supervisors.request(..)`'s refusal for
  `ConnectMachine` and `RestartMachine`, after the shell has set the entry to
  Starting... or Restarting.... Latent today (the refusal cases do not offer the
  entry); return the outcome so the entry is set only when the request was taken.
- Post-handshake EOF on an SSH endpoint loses ssh's diagnostic: only
  `classify_handshake_error` consults `MachineSshBridge::reported_failure`; once
  connected, `server_reader_thread` reports "server closed connection" and the
  bridge's classified stderr never reaches the machine diagnostic. After a remote
  bridge idles out (`IdleExpired`) the bridge prints nothing and exits 1, so the
  diagnostic gives no hint that the relay timed out; a `retry` record on stderr would
  show it.
- `is_link_error_kind` counts `AddrInUse` as a link failure, so a local bridge bind
  collision reads as the machine Offline; `FailureCause::Io(InvalidData |
  Unsupported)` maps to `Incompatible`, so an untyped local IO error of those kinds
  (a non-UTF-8 path, a refused `set_nonblocking`) is shown as the machine running an
  incompatible shepr.

## DIAG-027 - Integration errors travel as downcast payloads inside `io::Error`

Reported by: integrations.

`InstallIssue` (kind + message), `file_ops::NotRegularFile` and
`config_file::ConfigChanged` are three payload types that `InstallError::from`
downcasts. "Not a regular file" has two encodings (`InstallIssue` with
`InstallErrorKind::NotRegularFile` from `resolve_target`, and the struct from
`read_config_bytes`), and so does "config changed" (the `ConfigChanged` struct, and
an `InstallErrorKind::ConfigChanged` arm in `InstallIssue::io_error` that nothing
constructs). Fix: a typed error enum through the crate, turned into text once at the
log boundary. The test `install_failures_keep_their_category_at_the_log_boundary`
restates the `io::ErrorKind` mapping table rather than testing behaviour.

## DIAG-028 - Integration error paths that wait forever or allow an impossible state

Reported by: integrations.

- `config_file::lock_config_for_update` with `LockWait::UntilFree` waits forever on a
  held lock with nothing logged, so a stuck holder silently stalls every later
  target in the detached install thread.
- `PluginConfigEdit::write` can fail with "OpenCode config edit is missing its update
  lock", a state the type allows (contents `Some`, lock `None`). Use
  `Option<(String, ConfigUpdateLock)>`.
- A status error stops a repair the install could have made (BUG-038).

## DIAG-029 - Workspace and pane lifecycle is barely logged, and with the wrong identifier

Reported by: workspace-model.

`logging.rs` has structured `workspace.create/focus/close/rename` events, but a pane
split, a pane close, a pane exit removing a pane, a zoom, a swap and a workspace
move log nothing. `workspace_created` logs the root pane as the process-local raw
`PaneId`, which the operator cannot match to anything (`shepr detect explain` takes
`w1:p1`), and logs neither cwd nor name; `workspace_renamed` does not log the new
name. `commit_workspace_creation` and `commit_pane_split` log refusals ad hoc outside
the module that owns these events. `create_default_workspace` logs "failed to create
default workspace" without the cwd it tried, which decides most failures. Fix: pane
events in `logging.rs`, keyed by `PublicPaneId`.

## DIAG-030 - `UsableCwd::new` traces a permission error at `trace`, which no default filter shows

Reported by: workspace-model.

Its doc says the stat error "is traced with its error, so an unreadable directory is
not mistaken for a missing one", but `trace` is off in every default filter, so in
practice it is mistaken. Use debug or warn, or drop the claim.

## DIAG-031 - Workspace model refusals that hide their reason or a broken invariant

Reported by: workspace-model.

- `WorkspaceSet::insert` returns `Err(Box<Workspace>)` for both a repeated workspace
  id and a shared pane id; callers log "refused to add a new workspace" and "the new
  workspace was refused by the session". Return a reason enum like `RemoveRefusal`.
- `WorkspaceSet::remove_pane` swallows `RemoveRefusal` with `.ok()?`, so a
  disagreement between records and layout reads as "no workspace holds the pane";
  the `NotHere`-after-`contains` path signals a broken invariant that should be
  logged.
- Silent fallbacks that would hide a broken invariant: `TileLayout::resize_pane`
  reads `get_ratio_at(..).unwrap_or(SplitRatio::EVEN)` although the `SplitBorder` it
  just found carries `ratio`; `prepare_split` falls back to `sole_pane_spawn_geometry`
  when the new pane is not visible in its own planned layout; `sole_pane_size` falls
  back to `GridSize::clamped` (not `clamped_pane`, a different minimum). All
  unreachable by construction; if reached they give a wrong size silently. Prefer
  `split.ratio` and an internal error.
- `mark_shell_projection_dirty` stops advancing at `u64::MAX` "so bookkeeping never
  panics", after which no client sees a change. Unreachable; the chosen failure is
  silent staleness.
