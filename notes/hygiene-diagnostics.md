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

## DIAG-010 - The product is spelled "Shepr" in operator-facing text, and the local server is called "Local"

Reported by: save-shutdown, server-lifecycle.

`take_inhibitor` registers who="Shepr", why="Save terminal workspace layout", shown
by `systemd-inhibit --list`; `bootstrap.rs` says "Shepr TUI"; `shepr-remote/src/host.rs`
says "remote Shepr server socket"; elsewhere the product is `shepr` (the window
title is `shepr: <label>`). `failure.rs`'s module doc says "the Local server" and
`tui.rs`'s doc "the Local endpoint", although AGENTS.md says the local server is
never named "Local". A textlint on `"Shepr` in string literals would hold it.

## DIAG-012 - Launch settlements and cwd fallbacks leave no trace

Reported by: pane-lifecycle.

A `Launched` settlement is not logged, so a launch that fell back from its
requested directory to `HOME`, the passwd home or `/` leaves no trace of which
candidate it entered or why candidate 0 failed (the child does not send that errno;
only total failure is reported). `pane_launch.rs` stores the fallback cwd and no
notice says so. `pane_spawn_started` logs rows, cols and the
scrollback budget (constant per server) but not the launch kind, cwd or shell.

## DIAG-014 - A screen read error loses its reason

Reported by: pane-lifecycle.

- The mux terminal read collapses a `shepr_vt::ReadError` into `None`, so the
  `detect capture` / `detect explain` read-failure error can say only that the screen
  read failed, not why.

## DIAG-015 - Thread names exceed the kernel's 15-byte limit and are truncated

Reported by: pane-lifecycle.

`shepr-pane-{id}-teardown`, `shepr-pty-{id}`, `shepr-launch-status` and
`shepr-launch-reaper` exceed or approach the limit; `pthread_setname_np` truncates,
so every teardown thread shows as `shepr-pane-NN-t` and the reaper as
`shepr-launch-re`.

## DIAG-016 - A panicking detection tick leaves the pane's last state on the sidebar

Reported by: agent-state.

The failure now logs at error with the pane id. Restarting the task was declined (a
panic may follow terminal mutations or poison its mutex; reasoning at the failure
site), so detection stops and the pane keeps its last published state forever
(Working stays Working). It needs a pane failure policy that keeps terminal integrity:
publish Unknown and mark the pane, or end it.

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
