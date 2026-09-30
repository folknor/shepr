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

A fixer found why an in-place fix stalls: the worker's completion has to return
through the headless loop's ordered reply outbox, which means touching
`server/headless.rs`, the internal event handling and `endpoint_requests.rs`
together with `checkout_root.rs`. The resume `metadata` call is likewise tied to
PTY construction, which stats the requested cwd again and falls back to HOME, so
moving only the resume check off the loop would send the resume command into the
wrong directory. Comments at both blocking sites record this. Give one fixer the
loop, the internal events, the endpoint request handling and both call sites.
(The `home` in the reply now comes from the launch-resolved `AppPaths`.)

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

A fixer confirmed the `/proc` path and found the change needs
`App::handle_git_status_refreshed` in `app/events.rs` to resolve the current
cwds and pass them in, together with `app/actions/events.rs` and `app/state.rs`.
`runtime_for_pane_in_workspace` only borrows a runtime and does no I/O itself;
removing its registry parameter touches callers in `app/creation.rs` and
`app/api/panes/copy.rs`. Comments now mark both. Give one fixer all of these.

## SRV-009 - Render and projection invalidation happen even for failed requests

Scope: server-app. Related: SRV-002.

`app/api.rs` `handle_api_request_with_render` and
`handle_endpoint_command_with_render` decide `RenderDemand::Full` and bump the
shell projection from the method's static `mutates_ui` trait before dispatch, so
a rejected command (unknown pane, bad ratio, out-of-bounds move) or a hook report
for a pane that does not exist still forces a full render and a snapshot rebuild
on every client. The handlers know whether they changed anything.

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

## SRV-036 - A history recovery copy whose layout copy was never published is never pruned

Scope: mux-persist (lateral).

Layout backups and periodic snapshots now carry a `session-history-<ts>-<seq>.json`
sidecar, written before the layout copy, which is the commit marker. If the
server dies between the two, or the history cleanup fails, the sidecar has no
layout partner. Pruning walks layout files only, and `recovery_timestamp` does not
match history names, so the orphan stays forever. A small leak, only after a
crash. Prune history sidecars that have no layout partner and are older than the
newest layout copy.

## SRV-037 - Smaller restore and Git refresh notes

Scope: mux-persist, mux-git (lateral).

- **A pruned pane takes no backup.** Restore drops a pane whose saved cwd is
  relative (shepr only saves absolute cwds, so it is damage), but that pane does
  not count toward `dropped_workspaces`, so the first save overwrites the file
  without backing up the original. Layout leaves with no saved state were already
  handled the same way. Count any pruned pane as a restore defect that triggers
  the backup.
- **Stamp after query.** Git refresh stamps the config origin files after the
  `--show-origin` query returns, so an edit landing between the query and the
  stamp is cached as seen. Stamp before the query, or re-stamp and compare after.
