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

## SRV-009 - Successful endpoint commands still invalidate by static trait

Scope: server-app.

API demand now comes from actual projection changes, and a rejected endpoint
command no longer invalidates anything. A successful mutating command still
decides `RenderDemand::Full` and bumps the shell projection from its method's
static `mutates_ui` trait. Precision needs each handler to report what it
changed: extend `HandlerResult` in `app/api/endpoint.rs` with its effects and set
them in the individual handlers, then derive the demand from those. A note in
`app/api.rs` marks the limitation.

## SRV-013 - Endpoint requests still mark immediate PTY sources dirty unconditionally

Scope: server-serving-ui.

Server events, internal events and API dispatch no longer set the per-client
dirty flags blanket-wise. `server/headless/endpoint_requests.rs` still sets
`immediate_pty_sources_dirty` for every endpoint request, so each command
(including copy-mode steps) walks every client's workspace layout on the next
iteration. Set it only when the command changed focus, layout or workspace
membership.

## SRV-019 - Client baselines keep string pane ids

Scope: server-serving-ui.

The retained render path now resolves each pane's id once per recipient surface.
Each update still resolves every pane's public id string once, and the geometry
loop in `render_and_stream` (`headless/render.rs`) still parses every pane of
the last surface to compare alternate-screen flags. Keep the internal
`(workspace id, PaneId)` next to each wire `PaneSurfacePane` in the per-client
baseline, and use it in both places.

## SRV-034 - `aggregate_state` returns the raw state

Scope: mux-persist (lateral).

`aggregate_state` (`crates/shepr-mux/src/workspace/aggregate.rs`) returns
`AgentState` where its one consumer (`app/creation.rs`) wants the presented
state, and picks between Idle and Unknown by HashMap order; it is harmless only
because the consumer maps through `presentation_state()`. Return the presented
state so "Unknown presents as Idle" is a type fact; a note at the function
records the boundary.

## SRV-037 - A pruned pane takes no backup

Scope: mux-persist.

Restore drops a pane whose saved cwd is relative (damage, since shepr only saves
absolute cwds), but that pane does not count toward `dropped_workspaces`, so the
first save overwrites the file without backing up the original. Add a separate
restore-damage signal that `crates/shepr-server/src/app/mod.rs` reads when
deciding the backup, rather than misreporting a surviving workspace as dropped;
a note in `restore.rs` records the gap.
