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

## SRV-003 - PTY construction stats the cwd again on the loop and falls back to HOME

Scope: server-app, pty.

`workspace.checkout_root` and the agent resume cwd check now run in a worker and
complete through the loop. PTY construction still stats the requested cwd again
and silently falls back to HOME (`crates/shepr-pty/src/command.rs`), on the loop,
so a hung network mount can still block it there, and a resume cwd that vanishes
between the worker's check and the spawn sends the resume command into HOME.
Give the launch an already-validated or strict cwd (fail the resume instead of
falling back) so the second stat and the silent fallback go.
