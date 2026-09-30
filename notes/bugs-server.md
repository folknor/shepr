# Defects: server application, serving and persistence

Filed from the defect hunt over `crates/shepr-server/src/app`,
`crates/shepr-server/src/server`, `crates/shepr-server/src/ui`,
`crates/shepr-daemon`, and the rest of `crates/shepr-mux` (persist, git,
workspace, events, render_signal, cwd).

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## SRV-001 - A "retryable" agent resume failure destroys the pane it meant to retry

Scope: server-app.

Where: `app/agent_resume.rs`, `start_pending_agent_resume`, the `try_send_bytes`
error branch; `shepr-mux/src/pane/runtime.rs` child watcher and
`Drop for PaneRuntime`; `app/events.rs` PaneDied handling.

**Claim broken.** `AttemptOutcome::Retryable` in `app/resume_schedule.rs`: "The
plan is kept and the attempt is repeated later (the resume command could not be
queued to the shell). Backs the schedule off". `PENDING_AGENT_RESUME_RETRY_INTERVAL`:
"Retry a restored agent launch when it has not consumed its plan".

**What happens.** The branch spawns a real shell (`PaneRuntime::spawn` with the
restored pane's own `pane_id`), fails to queue the resume command, then
`drop(runtime)`. Dropping runs `shutdown_pane_processes`, which signals the
shell's session but does not stop the child-watcher task spawned in
`PaneRuntime::spawn`. That task reaps the shell and sends
`AppEvent::PaneDied { pane_id, exit_reason }`. PaneDied is keyed only by
`PaneId`, and the pane is still in the layout, so `prepare_pane_removal_by_id`
finds it. The shell died of a signal, so `classify_child_exit` gives
`Interrupted`, `requires_session_checkpoint()` is true, the server holds the exit
for a checkpoint (`server/headless/internal_events.rs`) and then removes the pane
(and the workspace if it was the only pane). The pending plan and persisted agent
session go with the terminal, so the next autosave drops the agent from the saved
session, and the retry a second later has nothing to retry.

In practice `Closed` means the actor already died, so the pane was probably dying
anyway, and `Full` on a fresh queue is unlikely; either way the code does not do
what its own model says.

**Structural fix.** PaneDied (and every runtime-originated event) carries the
terminal id plus a runtime generation, and `App` drops events from a runtime that
is no longer the registered one for that terminal, so a discarded runtime (this
path, `handle_pane_split`'s commit failure) cannot act on the pane. Today only
process-wide unique `PaneId`s and "the pane is gone by then" keep stale events
harmless.

## SRV-002 - Every internal event forces a full render, a shell projection rebuild and a walk of every pane

Scopes: server-app and mux-terminal (both hunters found it independently).

Where: `app/events.rs`, `handle_internal_event_with_render_demand`; its callers
`server/headless/internal_events.rs` (`_ =>` arm) and the loop's step 2 in
`server/headless.rs`.

**Claims broken.** `RenderDemand`: "How much of the server view an app operation
requires the loop to render"; AGENTS.md "Hot paths multiply".

Apart from ClipboardWrite and GitStatusRefreshed, every event (StateChanged,
AgentProcessDetected, HookStateReported, AgentSessionReported,
TerminalCwdReported, PaneDied for an unknown pane) returns `RenderDemand::Full`
and calls `mark_shell_projection_dirty()` unconditionally. The `StateUpdate`
(`Unchanged` / `Changed` / `Released`) that `AppState::handle_app_event` computes
is thrown away. So a hook report that changes nothing, a duplicate cwd report, a
stale PaneDied, or a `StateChanged` TerminalState ignored (detection under
full-lifecycle authority) still bumps the projection revision, rebuilds the
session snapshot (one `/proc` cwd and foreground-cwd probe per pane,
`api/session.rs`) and renders every client. Each event also walks every pane of
every workspace in `sync_full_lifecycle_authority_detection_pauses`. Detection
publishes run per pane per tick, so the cost is panes x panes of atomic stores,
plus a full projection rebuild and fan-out to every client, whether or not
anything changed.

`TerminalCwdReported` also always calls `request_git_identity_refresh` (which
clears every non-Git cache entry in `GitRefreshScheduler::mark_due`) and requests
a render even when `handle_app_event` found the cwd unchanged.

**Fix.** Derive the demand from the `StateUpdate` (and whether the cwd changed);
mark the projection dirty only on a real change; resync the lifecycle pause only
for the terminal the event touched. Related: SRV-009, SRV-013, SRV-014.

## SRV-003 - `workspace.checkout_root` runs Git synchronously on the server loop

Scope: server-app.

Where: `app/api/checkout_root.rs`, reached from
`handle_endpoint_command_with_render` on the headless loop
(`server/headless/endpoint_requests.rs`).

**Claims broken.** AGENTS.md "Hot paths multiply", and the pattern the rest of
`app/` keeps: Git work runs on the `shepr-git-refresh` thread, and
`App::with_paths` says restored workspaces get their Git identity "from the first
background Git refresh, not from a synchronous walk here".

`checkout_root` does `std::fs::metadata` and then
`run_git(cwd, ["rev-parse", "--show-toplevel"])` (timeout `GIT_COMMAND_TIMEOUT`,
5 s) inline in the command handler; meanwhile the loop serves no client, fans out
no PTY output and drains no events. The `metadata` call has no timeout at all, so
a hung network mount blocks the loop indefinitely. The same unbounded `metadata`
runs on the loop in `start_pending_agent_resume`.

**Fix.** Answer from a worker (the endpoint protocol has request ids) or from the
Git refresh cache.

Smaller: the reply's `home` comes from `shepr_core::pathutil::home_dir()` (the
process environment), while every other app path uses `self.paths.home_dir()`,
the launch-resolved `AppPaths`; the two can disagree.

## SRV-004 - PaneDied publishes the agent exit twice, and again on replay

Scope: server-app.

`server/headless/internal_events.rs` calls
`state.publish_pane_process_exit_if_agent` and then
`app.handle_internal_event(ev)`, which calls it again (`app/events.rs`). A held
checkpointed exit goes through both again when replayed from
`pending_checkpointed_pane_exits`. Nothing claims idempotence. Today the second
call finds the agent already released and returns `Unchanged`, but it runs the
whole `update_terminal_state` path again with a later `clock_now`, and
correctness depends on a terminal-state detail far away in `shepr-mux`. One owner
(the App handler) should publish it, and the server should ask the App whether
the exit is held.

## SRV-005 - Detect capture and explain say "not found" for a pane that exists

Scope: server-app.

Both handlers in `app/api/detect.rs` return `pane_not_found(&target.pane_id)`
when `lookup_runtime` finds no runtime.

**Claims broken.** The error's own text ("pane w1:p2 not found") and
`shepr detect capture <pane>` / `detect explain <pane>` in AGENTS.md. A pane
waiting on agent resume, or a restored pane whose shell or resume failed
(documented in `runtime_for_pane_in_workspace`), exists and is in the sidebar,
but the CLI says it does not exist. It needs its own error ("pane has no running
terminal", with the restore error when there is one).

## SRV-006 - `AppState` does /proc I/O despite "pure data"

Scope: server-app.

`app/actions/events.rs` `AppState::apply_workspace_git_statuses` takes the
`PaneRuntimeRegistry` and calls `Workspace::resolved_identity_cwd_from`, which goes
through `PaneRuntime::follow_cwd` (/proc reads) for every result.
`AppState::runtime_for_pane_in_workspace` also takes the registry.

**Claims broken.** AGENTS.md "State is separated from runtime. `AppState` is pure
data, testable without PTYs or async", and the `AppState` doc ("state reaches a
runtime only through the registry it is handed"); handing it the registry is how
the rule is sidestepped. `App` should compute the resolved cwd and pass it in, so
`AppState` only compares.

## SRV-007 - `window_title_without_workspace` does not do what its doc says

Scope: server-app.

`app/window_title.rs`: the doc says "`None` when window titles are disabled or
every token resolved empty". It returns `Some("")` (or `Some` of just the
literals) when every token is empty; the test
`a_client_with_no_workspace_renders_no_workspace_or_pane_target` asserts
`Some("|||x")`. The server gets the claimed behaviour only because
`server/headless.rs` then runs `sanitize_window_title_text`. Fix the doc or move
the sanitising into this function.

## SRV-008 - `toggle_pane_zoom` reports "pane not found" after it moved focus

Scope: server-app.

`app/actions/pane.rs` focuses the pane first (marking the session dirty), then
returns `None` if `set_zoomed` did not take. The only caller
(`api/panes/geometry.rs` `handle_pane_zoom`) turns `None` into `pane_missing`, so
the requester is told the pane does not exist while its focus changed. `None` is
documented as "the pane is not in the workspace". A refused zoom should be a
separate outcome.

## SRV-009 - Render and projection invalidation happen even for failed requests

Scope: server-app. Related: SRV-002.

`app/api.rs` `handle_api_request_with_render` and
`handle_endpoint_command_with_render` decide `RenderDemand::Full` and bump the
shell projection from the method's static `mutates_ui` trait before dispatch, so
a rejected command (unknown pane, bad ratio, out-of-bounds move) or a hook report
for a pane that does not exist still forces a full render and a snapshot rebuild
on every client. The handlers know whether they changed anything.

## SRV-010 - A held pane exit can be checkpointed again and again under steady dirtying

Scope: server-app.

Where: `app/session.rs` `pane_exit_checkpoint_settled`
(`pane_exit_checkpoint_pending && !session_dirty`), `SessionSaver::schedule`
(clears `pane_exit_checkpoint_pending`), and the loop order in
`server/headless.rs` (`sync_session_save_schedule` in step 3,
`drain_server_events` in step 4, replay in step 5).

Nothing bounds this: `CHECKPOINT_MAX_FAILURES` counts only failed saves. If
anything dirties the session between the checkpoint's capture and the replay (a
bookmark move from an active client in step 4, a session-ref hook report, a cwd
change), the replay finds it unsettled, requests another full checkpoint (whole
session plus history) and holds the exit again. Under continuous dirtying the
dead pane stays in the layout and the session is rewritten back to back. The
checkpoint only has to prove the pre-exit layout is on disk; settle on "a
checkpoint captured after this exit was held succeeded", tracked by generation,
not on the global dirty flag.

## SRV-011 - The final save is skipped whenever a pane-exit checkpoint is the latest save

Scope: server-app.

**Claim broken.** `app/session.rs` `save_session_before_teardown_async`: the
final save is "Captured while every pane runtime still exists, so the final save
holds each live pane's history".

`preserve_checkpoint` returns before the final capture, so when the latest save
was a pane-exit checkpoint and nothing has dirtied the session since, the final
save is skipped entirely. With `experimental.pane_history` on, the other panes
lose the output written between the checkpoint and shutdown, as does any cwd
known only from `/proc` (not dirtying). Keeping the checkpointed layout and
taking fresh history are separate decisions: the final save could reuse the
checkpoint's structure with current history.

## SRV-012 - Smaller server-app items

Scope: server-app.

- `app/api/detect.rs`: the hook-authority skip answer is a hand-written
  `serde_json::json!` object mirroring
  `shepr_agent::detect::manifest::explain_to_json_value`'s schema field by field,
  with nothing keeping them in step. It reports the raw `terminal.state` label,
  which can say `unknown`, although AGENTS.md says Unknown presents as Idle
  (diagnostic output, so low).
- `app/events.rs` handles `ClipboardWrite` and `GitStatusRefreshed` before
  `AppState::handle_app_event`, which keeps dead arms for them "for
  exhaustiveness", plus a PaneDied arm that only logs a warning.
  `AppState::handle_app_event` is `pub` and returns a `StateUpdate` no production
  caller reads. Splitting `AppEvent` into state events and App-level events would
  let the type system say this.
- `create_default_workspace` failing (a shell that stops resolving after launch)
  is retried on every loop wake by `create_automatic_workspace(None)` while a
  client is attached, logging an error each time, with no backoff.
- `start_pending_agent_resumes` and `pending_agent_resume_wakeup` redo the
  per-workspace layout walk (`has_pending_agent_resume_candidates`) every loop
  iteration for as long as a plan stays pending but not eligible (a workspace
  recorded at 0x0, or never laid out).
- `app/actions/pane.rs` test-only `resize_pane` hard-codes `0.05` instead of
  `DEFAULT_PANE_RESIZE_AMOUNT`.
- `handle_workspace_rename` and `handle_workspace_create` store any label as given
  (`set_custom_name(String)`), including empty, which pins a blank name and stops
  the auto label for good; `handle_pane_rename` trims and treats empty as clear.
  The client trims first today, so this is an asymmetry in the server contract.

## SRV-013 - Every sent frame makes the loop redo the per-client work its dirty flags exist to avoid

Scope: server-serving-ui.

**Claims broken.** The `immediate_pty_sources_dirty` field doc in `headless.rs`
("Recomputing it on every loop wake walked every pane per PTY notify ... a PTY
render wake changes neither"), and AGENTS.md "Hot paths multiply ... times
clients".

- The writer thread sends `ServerEvent::ClientWriterDrained` for every render item
  it writes (`client_writer_loop`), so every frame to every client comes back as a
  server event.
- `apply_server_event` begins with `self.immediate_pty_sources_dirty = true` for
  every event, `ClientWriterDrained` and `ClientShellPaneInput` included.
- The next iteration runs `sync_immediate_pty_sources`, which walks every
  client's workspace layout and builds a `HashSet`, and sets
  `host_input_modes_dirty`, which runs `stream_host_mouse_capture_mode` and
  `stream_shell_keyboard_mode`, taking terminal-core reads on every client's
  focused pane.

The result is the per-wake recompute the flag was added to remove, now once per
frame per client and once per keystroke. The events that change the inputs
(connect, disconnect, surface set, endpoint commands, internal events) are a small
subset. See SRV-014 for the shared structural fix.

## SRV-014 - Transport-only events rebuild the shared session snapshot, `/proc` reads included

Scope: server-serving-ui.

**Claims broken.** `render_and_stream`: "Rebuild the shared session only when
application state that feeds it changed"; the `snapshot_pane` comment in
`app/api/session.rs` (runs "once per pane for every session snapshot").

`handle_server_event_with_render_impact` exempts only `ClientShellPaneInput` and
`ClientShellEndpointRequest` from `mark_shell_projection_dirty`. Every other
server event returning true bumps `shell_projection_revision`, and the next render
rebuilds `ShellSessionCache` (`app.session_snapshot()`, reading `/proc` for every
pane's cwd and foreground cwd) and re-projects every client. That covers:

- `ClientWriterDrained` with a deferred render. A backpressured client (slow SSH)
  defers most frames, so the session is rebuilt about once per frame.
- `ClientShellResize`, which always returns true for an active client: a window
  drag rebuilds per resize event.
- `ClientShellFocus`, `ClientShellHostTheme` and `ClientShellPresentationSync`.

None of these changes anything a `ClientShellSnapshot` carries (no field for
client size, focus or writer state).

**Structural fix for SRV-013 and SRV-014.** Each event handler returns what it
invalidated (projection, immediate PTY sources, host input modes, geometry,
surface) instead of blanket flags at the top of the dispatcher. Keep transport
signals (`ClientWriterDrained`) out of that path entirely: a drained writer only
needs `take_deferred_render`.

## SRV-015 - A client the server drops is never disconnected

Scope: server-serving-ui.

**Claims broken.** Comments in `render_and_stream` and
`set_client_shell_surface_active` ("drop the client: it reconnects with a fresh
counter").

`remove_client` only takes the `ClientConnection` out of the registry; nothing
shuts the socket:

- The read thread still holds `endpoint_control_writer`, a `ClientControlWriter`
  clone made in `handle_client_handshake`, so the writer queue's `senders` never
  reaches 0 and the writer thread waits in `recv` forever.
- The read thread keeps reading and forwarding events for a `ClientId` that is
  gone; they are dropped silently. `handle_client_shell_endpoint_request` returns
  without a reply for an unknown client, so every command waits out its
  client-side timeout.
- `HealthPing` is answered by the read thread itself, so the client's health check
  keeps passing.

The client sees a healthy, silent connection and never reconnects. The paths that
remove a client this way (exhausted projection or surface revisions, a snapshot
that fails to frame, a frame serialize error other than `Oversized`) are close to
unreachable today (`MAX_MESSAGE_SIZE` is 1 GiB), but the contract is false and any
future server-side drop inherits it.

**Fix direction.** Give `ClientConnection` ownership of the connection's lifetime
(a close handle calling `shutdown(Both)` on a cloned stream, or a close item the
writer handles by shutting the socket), so removing from the registry closes the
connection. Separately, the pong should not bypass the loop if it is meant to show
that the server, not only its reader thread, is alive.

## SRV-016 - A config too large for the welcome exits as "failed", not "config refused"

Scope: server-serving-ui. Related: WIRE-009.

**Claims broken.** `run_server`'s comment ("A config the welcome cannot carry
fails the launch here, like any other config problem") and the `daemon_exit`
classes (`CONFIG_REFUSED_EXIT_CODE`, "configuration or paths were refused").

`ensure_config_fits_welcome` returns `io::Error`, which becomes
`RunServerError::Io`; `shepr-daemon`'s `report_server_error` then exits with
`FAILED_EXIT_CODE`, and a launching client reads `DaemonExit::Failed` ("failed to
start") instead of `ConfigRefused` ("refused its configuration"). It also runs
after the data-directory lease is taken, so it is not a config check in the
`serve()` sense. It belongs next to `load_validated` in `shepr-daemon`, or needs a
`RunServerError::ConfigRefused` variant.

## SRV-017 - A readiness error on the client listener skips the final save and the shutdown notice

Scope: server-serving-ui.

**Claims broken.** `ctrlc_handler`'s rationale ("without it a signal kills the
server without the shutdown sequence that saves the session"), and
`HeadlessServer::run`'s documented shutdown sequence.

In `run`'s `select!`, the `client_listener_ready.readable()` arm does
`Err(err) => return Err(err)`, returning straight out of `run`: no
`initiate_shutdown` (clients get no `ServerShutdown`), no
`save_session_before_teardown_async`, no pane teardown wait. Only `Drop` runs,
releasing the lease and sockets. The `accept_client_connections` error path next
to it does `run_error = Some(err); self.initiate_shutdown()`; this arm should too.

## SRV-018 - The listener can strand pending connections after an accept error

Scope: server-serving-ui.

`run` clears AsyncFd readiness (`guard.clear_ready()`) before
`accept_pending_client_connections` drains the backlog. The drain loop breaks on
any accept error other than `WouldBlock` (for example `EMFILE`) with connections
still queued; readiness is edge-triggered and already cleared, so they wait until
a new connection arrives. Clear readiness only when `accept` reports `WouldBlock`
(tokio's `try_io` pattern), or re-arm on error. Related:
`accept_pending_client_connections` returns `io::Result` but never returns `Err`,
so the `run_error` branch handling it is dead.

## SRV-019 - The retained render path resolves string pane ids per source, recipient and pane

Scope: server-serving-ui.

**Claim broken.** AGENTS.md "Hot paths multiply ... use narrow accessors".

`render_retained_pane_surface_and_stream` runs for PTY output to visible panes,
the hottest serving path. For each PTY source and recipient it does
`surface.panes.iter().find(|pane| self.app.parse_pane_id(&pane.pane_id) ...)`;
`parse_pane_id` parses the public id string, scans workspaces linearly with
`resolve_workspace_id`, then maps the pane number. `has_synchronized_pane` does
the same parse for every pane of every recipient, twice per call. The geometry
loop in `render_and_stream` parses every pane of the last surface again to
compare alternate-screen flags. The per-client baseline should keep the internal
`(workspace index or id, PaneId)` next to each wire `PaneSurfacePane`.

## SRV-020 - Every client renders its surface from scratch, even when two clients show the same thing

Scope: server-serving-ui (structural opportunity, not a contract break).

`render_and_stream` calls `render_client_shell_pane_surface` per client (layout,
every pane's `render_into` under its core lock, borders) even when two clients
view the same workspace at the same size and cell size, the typical multi-client
case (one user, two terminals). Key the render on `(workspace id, surface size,
cell size)` and share the `FrameData`, leaving only the baseline diff per client.
`snapshot_from_session(cache.session.clone(), ...)` clones the whole
`SessionSnapshot` per client per projection; it could borrow. The timer path
`refresh_shell_projection_sources` projects every client once to detect a change,
then `render_and_stream` projects them all again.

## SRV-021 - A resize promotes a client to foreground and switches every pane's host theme

Scope: server-serving-ui. The hunter calls whether resize should count an intent
question for the owner.

**Claim at issue.** `ClientRegistry` doc "which one was active most recently (the
foreground client)", and AGENTS.md "the host theme by the foreground client (the
one last active)".

`ClientShellResize` calls `promote_client_to_foreground`, and
`sync_host_theme_from_foreground` then recolours every pane with that client's
theme. A resize is usually not user activity: a tiling window manager relayout or
a font change on a background monitor steals foreground and flips the theme (and
where pane-less clipboard writes go). The other triggers (input with interaction,
outer focus gained, endpoint command, surface activation) are real activity.

## SRV-022 - A new client's seed snapshot can be followed by an older one

Scope: server-serving-ui.

`ClientShellConnected` builds its seed snapshot from a fresh
`app.session_snapshot()`, not from `shell_session_cache`, then records
`session_generation = self.shell_session_generation`. If the cache is older, the
next projection from it can carry older `/proc` cwd values than the seed did, and
the client gets a snapshot that moves its cwd back until the cwd timer refresh.
Seed from the cache (rebuilding it first if its revision is stale).

## SRV-023 - Stale or wrong comments in serving

Scope: server-serving-ui.

- `headless.rs` module doc: "Renders to a virtual ratatui Buffer in memory". It
  renders straight to wire cells (`render_surface_virtual` into `FrameData`). It
  also says it "handles ... minimum terminal size"; no such handling exists in
  the crate.
- `send_to_all_clients` doc: "the only callers are the two shutdown notices".
  There is one caller (`initiate_shutdown`).
- `initiate_shutdown`: "Clear client-local host graphics, then send
  ServerShutdown". Nothing clears graphics.
- `client_shell.rs`: the `snapshot_from_session` doc says projection runs again
  "only when the shared cache generation moves"; it also runs when the client's
  own location generation moves (`needs_projection` in `render_and_stream`).

## SRV-024 - Smaller serving smells

Scope: server-serving-ui.

- `pane_border_title(label, pane_width, _focused)` takes an unused parameter.
- The retained path's `success!($reason)` macro drops its reason, so success
  reasons are documentation only. Fallback reasons are logged once per lifetime,
  hiding a fallback that starts recurring later.
- `accept_pending_client_connections` sets each accepted stream nonblocking, and
  the handshake thread immediately sets it back to blocking.
- `render_pane_borders` allocates a `HashMap<(u16, u16), LineCell>` per render per
  client, then checks every pane for each border cell (`line_touches_pane`). A
  grid-sized bitmap or per-edge pass would be cheaper on the full-render path.
- `ui/panes.rs` tests cover `shepr_termio::selection_render`
  (`automatic_selection_*`, `render_selection_highlight`), not code in this
  crate; they belong in `shepr-termio`.
- The writer thread sends `ClientWriterDrained` with a `blocking_send` on the
  bounded server-event channel before writing the frame, so a full channel (one
  client flooding pane input) stalls frame writes to every other client until the
  loop drains it. Sending after the write with `try_send` breaks the coupling; a
  missed signal only matters when a render was deferred, which could be tracked on
  the shared writer queue instead.

## SRV-025 - One bad pane field throws away the whole saved session, though the docs say restore drops only the bad workspace

Scope: mux-persist-git-workspace.

**Claim broken.** `persist/snapshot.rs`, `parse_snapshot`: "Deserializes the saved
shape only. Semantic checks stay in `restore`, so one invalid workspace can be
dropped while healthy ones survive". `restore.rs`: "An invalid saved split ratio
drops this one workspace, like every other per-workspace restore defect below,
rather than refusing the whole session".

Several semantic checks run inside serde and fail the whole `SessionSnapshot`
parse:

- `PaneSnapshot.cwd` and `WorkspaceSnapshot.identity_cwd` go through
  `path_bytes::deserialize_saved_cwd`, which returns a serde error for a relative
  path.
- `PaneSnapshot.agent_session` is a `PaneAgentSessionSnapshot` with a typed
  `shepr_agent::agent::Agent`, whose `Deserialize` (`shepr-agent/src/agent/mod.rs`)
  fails with "unknown agent label" for any label this build does not know.
  `AgentSource` and `AgentSessionRef` validate the same way.

On failure `io::load` logs `parse_error` and returns `None`, the server restores
nothing (`app/mod.rs` sets `protect_unloaded`), and every workspace is gone from
the live session. The file is copied to `session-backups` before the first save,
but nothing restores it automatically. Realistic: detection changes ship as new
builds and the owner rebuilds often; a build that renames or removes an `Agent`
variant, or changes what `AgentSource` accepts, fails the previous build's file as
soon as any pane had a hook-reported session. A hand edit making one cwd relative
does the same.

**Fix.** Parse permissively and validate in restore: take `agent_session` as
`Option<serde_json::Value>` (or a lenient newtype turning any error into `None`
plus a warning) and convert it in `restore_plan_for_snapshot` /
`restored_terminal_agent_session`; parse cwds as plain `PathBuf`s (keeping the
byte-sequence form) and check absoluteness in `restore_workspace`, where a bad
pane becomes `RestoredPaneStart::Unavailable` and a bad `identity_cwd` falls back
to the root pane's cwd. Only `SnapshotVersion` should reject the whole file.

## SRV-026 - Restore validates public pane numbers only after it has started every shell

Scope: mux-persist-git-workspace.

**Claim broken.** `restore_workspace`: "That happens before any pane starts, so
every shell starts at its size in the layout the workspace ends up with"; it also
describes per-workspace defects as dropping the workspace before anything runs.

Two checks run only after `PaneRuntime::spawn_with_initial_history` for every
surviving pane: the duplicate or zero public-number check (in
`Workspace::from_restored` via `valid_panes`) and the "restored pane has no public
number" check. When either drops the workspace, the code has already:

- forked a shell for every pane, each with a `SHEPR_PANE_ID` launch env
  (duplicated across panes in the collision case); the runtimes are dropped and
  `PaneRuntime::drop` tears down the sessions;
- inserted resume reservations into `resumed_agent_sessions` for panes of the
  dropped workspace, so a later, healthy pane with the same saved session is
  treated as a duplicate: `restored_terminal_agent_session` returns `None` for it,
  it loses its agent session for good, and the next save writes it without one;
- added `history_carry.carry_restored` entries (harmless; the first save prunes
  them).

**Fix.** Everything `valid_panes` checks is known before any spawn (public
numbers come from `assign_public_pane_numbers`; the rest is structural). Run it
right after pruning and reserve resume keys only once the workspace is known to
survive. Best: a two-phase restore that validates the whole snapshot into plain
restore plans with no side effects, then spawns.

## SRV-027 - A panic on the persister thread releases the data-directory lease while the server keeps running

Scope: mux-persist-git-workspace.

**Claim broken.** `persist/actor.rs` module docs: "The lease is released only when
the persister is retired, after every job submitted before has finished";
`persist.rs` says one server at a time owns a data directory.

The lease sits in `PersistState` on the `shepr-persist` thread. If a job panics
(history formatting or serialization run there), the thread unwinds and drops the
state and the lease; `retire` only logs it later. Meanwhile the server keeps
running with every later job answered `abandoned` and the save loop retrying with
backoff forever (no live persistence, no loud signal), and `session.lock` is free,
so a second server on the same profile can acquire it and restore the stale file
while the first still owns live panes.

**Fix.** Hold the `DataDirLease` in `SessionPersister` itself and release it only
in `retire` (the direct fix), or treat a dead persister as fatal: on `abandoned`,
log an error and shut down cleanly.

## SRV-028 - Git status passes unvalidated ref-file contents to `git rev-list` as argv

Scope: mux-persist-git-workspace.

**Claim broken.** The git reader avoids trusting repository files
(`GitReadError::FileRead`: "A repository file could not be read or was not safe to
trust"; `read_ref_oid_with_errors` refuses stale packed fallbacks), but the OIDs it
produces are never validated as object names.

- `read_ref_oid_with_errors` returns the trimmed content of a loose ref file, or
  the first token of a packed-refs line, verbatim; `read_head_identity_from_files`
  does the same for a detached `HEAD`.
- `git_ahead_behind_between` formats `"{head_oid}...{upstream_oid}"` and runs
  `git rev-list --left-right --count <that>` with no `--end-of-options` or `--`.

A loose ref whose content starts with `-` becomes a git option: `.git/refs/heads/main`
containing `--output=/path/x` makes git's revision parser handle the diff option
`--output=`, which opens that path for writing. More ways in: `full_ref` comes from
`HEAD` (`ref: refs/heads/...`) or the branch's `merge` config; `upstream_full_ref`
returns `merge_ref` unchanged for `remote = "."`, and `common_dir.join(full_ref)`
with an absolute or `..` path reads any file (up to the ref size cap) as an "oid".
A legitimate symbolic loose ref (`ref: refs/heads/other`, for branch aliases) is
also passed as an oid; rev-list fails and the refresh enters a permanent retry
loop.

**Threat model.** Needs someone else's `.git` on disk (an extracted tarball or
copied checkout; `git clone` never writes these files). Running `git` there
already carries config risk; the marginal risk is shepr turning plain ref files
Git itself would treat as broken into argv options.

**Fix.** Accept an oid only if it is 40 or 64 lowercase hex characters and put
`--end-of-options` before the range; reject `full_ref` values that are absolute,
contain `..`, or do not start with `refs/` (as Git's `check_refname_format` does);
follow `ref: ` indirection in loose refs, or report the ref unavailable.

## SRV-029 - A drop by partial restore backs up the layout but not the history that pairs with it

Scope: mux-persist-git-workspace.

**Claim broken.** `restore.rs` on a dropped workspace: "The workspace is not lost
on disk: a nonzero `RestoredSession::dropped_workspaces` makes the first save back
the original file up before overwriting it."

`SessionWriter::preserve_unloaded` copies only `session.json` into
`session-backups`. The first save also replaces `session-history.json`, whose
history belongs to the backed-up layout (`layout_fingerprint` pairs them), so
restoring from the backup brings the layout back without its screen history. The
periodic `session-snapshots` copies are layout-only too. A real loss only with
`experimental.pane_history` on. Copy the history file next to the layout backup
(same timestamp and sequence name), or narrow the doc to the layout.

## SRV-030 - `from_existing_pane`, `ExistingPane` and the "pane move" rationale are production surface with no production caller

Scope: mux-persist-git-workspace.

**Claims broken.** AGENTS.md: "the goal is the smallest code surface that does
what the owner uses"; the stale-doc rule.

- `Workspace::from_existing_pane` and `pane_tree::ExistingPane` are `pub`, but
  their only caller is `shepr-server/src/test_support.rs`.
- The comment on `NEXT_WORKSPACE_NUMBER` in `workspace.rs` justifies the
  process-global counter by saying an owned allocator "would have to be threaded
  into every workspace constructor, pane move and restore". No pane move exists.
- `detach_pane` returns a `DetachedPane` its only caller, `remove_pane`,
  discards.
- `commit_new_pane` sets the public number twice: in `commit_prepared_split` and
  again in `register_new_pane_with_number`.

Move `from_existing_pane` behind `#[cfg(test)]` or into test support as a seam,
and drop "pane move" from the comment.

## SRV-031 - `events.rs` module doc is stale

Scope: mux-persist-git-workspace.

It says background tasks include "future hook listeners". Hook state already
arrives through `AppEvent::HookStateReported` and `AgentSessionReported`. Reword
as "PTY child watchers, detectors, hook reports, the git refresh".

## SRV-032 - Overflow on hand-edited public pane numbers panics restore in the dev build

Scope: mux-persist-git-workspace.

**Claim broken.** `PaneSnapshot.public_number`: "Restore gives a pane with none, or
with zero ..., a fresh free number."

In `restore_workspace`, `next_public_pane_number` starts as
`...max(snap.next_public_pane_number)`. If the file says
`next_public_pane_number: 18446744073709551615` and some pane lacks a number,
`assign_public_pane_numbers` runs `*next_public_pane_number += 1`: an overflow
panic in the dev profile (the server dies on every start until the file is fixed)
and a wrap in release. `valid_panes` would drop the workspace, but only if
execution got that far. Live splits have the same pattern in
`register_new_pane_with_number` (`number + 1`), though a live counter cannot get
there. Use `checked_add` and treat exhaustion as a per-workspace defect.

## SRV-033 - The Git config reimplementation differs from Git where its docs claim Git's semantics

Scope: mux-persist-git-workspace.

`git/config.rs` and `git/discovery.rs` re-derive Git's config chain to find the
upstream without spawning git. Docs promise Git-equivalent behaviour
(`git_dir_is_bare`: "Git's effective `core.bare` ... comes from the whole config
chain ... each with its includes"). Differences that change the answer:

- `includeIf "gitdir:"`, `onbranch:` and `hasconfig:remote.*.url:` go through
  `wildcard_match`, where `*` matches across `/` and `?`, `[...]` and `\` escapes
  are literal. Git uses wildmatch with `WM_PATHNAME` (`*` stops at `/`, only `**`
  crosses). `gitdir:/work/*/.git` includes configs at any depth here but one level
  in Git, so the upstream (and ahead/behind) can come from a config Git would not
  read.
- `normalize_config_value` strips outer quotes only: no escapes (`\"`, `\\`,
  `\t`), no mid-value quoting (`a"b c"d`), no continuation lines ending in `\`. It
  also skips the deprecated `[branch.main]` subsection syntax.
- packed-refs parsing in `read_ref_oid_with_errors` uses `parts.next()?` in a
  loop, so one line with an oid but no name aborts the whole lookup instead of
  skipping the line. packed-refs is read with no size cap, although loose refs are
  capped at `MAX_GIT_REF_FILE_BYTES`.

**Structural recommendation.** Stop reimplementing Git's config resolution. Keep
stat-level change detection (stamp `HEAD`, the branch's loose ref, `packed-refs`,
and every config file Git would read), and on change ask Git with one spawn:
`git for-each-ref --format='%(refname) %(objectname) %(upstream) %(upstream:track,nobracket)' refs/heads/<branch>`,
or `git rev-parse --symbolic-full-name @{u}` followed by the existing `rev-list`.
That removes about 1200 lines of config grammar (`config.rs`), fixes the fidelity
gaps at once, and closes SRV-028's config-driven paths; Git status refreshes on an
interval off the loop, so one extra spawn per changed repository is cheap.

## SRV-034 - Smaller persistence notes

Scope: mux-persist-git-workspace (lateral).

- `aggregate_state` returns `AgentState` where every consumer wants
  `PresentedAgentState`. It picks between Idle and Unknown by HashMap order, which
  is harmless only because the one consumer maps through `presentation_state()`.
  Returning the presented state would make "Unknown presents as Idle" a type fact.
  Related: AGENT-003, AGENT-027.
- `io::load` reads `session.json` with no size cap, and
  `snapshot_history_decision` reads both the live file and the newest recovery copy
  in full on every save outside the snapshot interval. Only the history file has a
  cap.
- `resolve_write_target` returns the path reached after
  `MAX_SESSION_PATH_SYMLINK_HOPS` even if it is still a symlink; the rename then
  replaces that link with a regular file where it should report a loop.
- `SessionPersister::drop` joins the persister thread; if `App` is dropped on a
  tokio worker during a big history save, that worker blocks for the save. Fine
  at shutdown, worth knowing if `App` is ever dropped elsewhere.

## SRV-035 - `RestoreFailure` promises API presentation that does not exist

Scope: mux-terminal. Hunter's confidence: high.

`RestoreFailure`'s doc: "The pane surface and the API both present it". Only
`ui/panes.rs` reads `restore_error`; no API schema field carries it, and nothing
in `shepr-api` mentions restore. The `Display` impl has no caller, since
`restore.rs` logs it with `?reason` (Debug). Either expose it (pane info and
`status`) or correct the doc and delete `Display`. Related: SRV-005, which wants
the restore error in the detect "no running terminal" answer.
