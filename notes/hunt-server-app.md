# Hunt: crates/shepr-server/src/app

Scope: `AppState` (`app/state.rs`), `App` and its behaviour modules under
`app/`, and the places where the server loop (`server/headless/*`) hands
state to them or calls into them. Findings are ordered by how much they
break a stated claim. Each names the claim it breaks.

## 1. A "retryable" agent resume failure destroys the pane it meant to retry

Where: `app/agent_resume.rs`, `start_pending_agent_resume`, the
`try_send_bytes` error branch; `shepr-mux/src/pane/runtime.rs` child
watcher and `Drop for PaneRuntime`; `app/events.rs` PaneDied handling.

Claim broken: `AttemptOutcome::Retryable` is documented in
`app/resume_schedule.rs` as "The plan is kept and the attempt is repeated
later (the resume command could not be queued to the shell). Backs the
schedule off", and `PENDING_AGENT_RESUME_RETRY_INTERVAL` says "Retry a
restored agent launch when it has not consumed its plan".

What happens: the branch spawns a real shell (`PaneRuntime::spawn` with the
restored pane's own `pane_id`), fails to queue the resume command, then
`drop(runtime)`. Dropping a runtime runs `shutdown_pane_processes`, which
signals the shell's session, but it does not stop the child-watcher task
spawned in `PaneRuntime::spawn`. That task reaps the shell and sends
`AppEvent::PaneDied { pane_id, exit_reason }`. PaneDied is keyed only by
`PaneId`, and the pane is still in the layout, so
`prepare_pane_removal_by_id` finds it. The shell died of a signal, so
`classify_child_exit` gives `Interrupted`, `requires_session_checkpoint()`
is true, the server holds the exit for a checkpoint
(`server/headless/internal_events.rs`) and then removes the pane (and the
workspace if it was the only pane). The pending plan and the persisted
agent session go with the terminal, so the next autosave drops the agent
from the saved session. The "retry" a second later has nothing to retry.

In practice `Closed` means the actor already died, so the pane was probably
dying anyway; `Full` on a fresh queue is unlikely. Either way the code's
own model (keep the plan, retry later) is not what it does.

Structural fix: PaneDied (and every runtime-originated event) should carry
the terminal id plus a runtime generation, and `App` should drop events from
a runtime that is no longer the registered one for that terminal. Then a
discarded runtime (this path, `handle_pane_split`'s commit failure) cannot
act on the pane. Today only process-wide unique `PaneId`s and "the pane is
gone by then" keep stale events harmless; this path is where that breaks.

## 2. Every internal event forces a full render and a shell projection rebuild

Where: `app/events.rs`, `handle_internal_event_with_render_demand`; its
callers `server/headless/internal_events.rs` (`_ =>` arm) and the loop's
step 2 in `server/headless.rs`.

Claim broken: `RenderDemand` is documented as "How much of the server view
an app operation requires the loop to render", and AGENTS.md "Hot paths
multiply" (detection and client frame fanout run per event, times panes,
times clients).

What happens: apart from ClipboardWrite and GitStatusRefreshed, every event
(StateChanged, AgentProcessDetected, HookStateReported,
AgentSessionReported, TerminalCwdReported, PaneDied for an unknown pane)
returns `RenderDemand::Full` and calls `mark_shell_projection_dirty()`
unconditionally. `AppState::handle_app_event` already computes a
`StateUpdate` (`Unchanged` / `Changed` / `Released`) and it is thrown away.
So a hook report that changes nothing, a duplicate cwd report, or a stale
PaneDied still bumps the projection revision, rebuilds the session snapshot
(one `/proc` cwd and foreground-cwd probe per pane, `api/session.rs`) and
renders every client. Each event also walks every pane of every workspace in
`sync_full_lifecycle_authority_detection_pauses`.

Fix: return the demand from the `StateUpdate` (and from whether the cwd
changed); only mark the projection dirty on a real change; resync the
lifecycle pause only for the terminal the event touched.

`TerminalCwdReported` also always calls `request_git_identity_refresh`
(which clears every non-Git cache entry in `GitRefreshScheduler::mark_due`)
and requests a render even when `handle_app_event` found the cwd unchanged.

## 3. `workspace.checkout_root` runs Git synchronously on the server loop

Where: `app/api/checkout_root.rs`, reached from
`handle_endpoint_command_with_render` on the headless loop
(`server/headless/endpoint_requests.rs`).

Claims broken: AGENTS.md "Hot paths multiply" and the pattern the rest of
`app/` keeps: Git work runs on the `shepr-git-refresh` thread, and
`App::with_paths` spells out that restored workspaces get their Git identity
"from the first background Git refresh, not from a synchronous walk here".

What happens: `checkout_root` does `std::fs::metadata` and then
`run_git(cwd, ["rev-parse", "--show-toplevel"])`, whose timeout is
`GIT_COMMAND_TIMEOUT` (5 s), inline in the command handler. For that time
the loop serves no client, fans out no PTY output and drains no events.
The `metadata` call has no timeout at all, so a hung network mount blocks the
loop indefinitely. The same unbounded `metadata` runs on the loop in
`start_pending_agent_resume`.

Fix: answer this command from a worker (the reply can arrive later; the
endpoint protocol already has request ids) or from the Git refresh cache.

Smaller: the reply's `home` comes from `shepr_core::pathutil::home_dir()`
(the process environment), while every other app path uses
`self.paths.home_dir()`, the launch-resolved `AppPaths`. The two can
disagree.

## 4. PaneDied publishes the agent exit twice, and again on replay

Where: `server/headless/internal_events.rs` calls
`state.publish_pane_process_exit_if_agent` and then
`app.handle_internal_event(ev)`, which calls it again (`app/events.rs`). A
held checkpointed exit goes through both again when it is replayed from
`pending_checkpointed_pane_exits`.

Nothing claims idempotence here. Today the second call finds the agent
already released and returns `Unchanged`, but it runs the whole
`update_terminal_state` path again with a later `clock_now`, and the
correctness depends on a terminal-state detail far away in `shepr-mux`.
One owner (the App handler) should publish it, and the server should ask
the App whether the exit is held.

## 5. Detect capture and explain say "not found" for a pane that exists

Where: `app/api/detect.rs`. Both handlers return
`pane_not_found(&target.pane_id)` when `lookup_runtime` finds no runtime.

Claim broken: the error's own text ("pane w1:p2 not found") and
`shepr detect capture <pane>` / `detect explain <pane>` in AGENTS.md. A pane
waiting on agent resume, or a restored pane whose shell or resume failed
(documented in `runtime_for_pane_in_workspace`), exists and is listed in the
sidebar, but the CLI says it does not exist. It needs its own error ("pane
has no running terminal", with the restore error when there is one).

## 6. `AppState` does /proc I/O despite "pure data"

Where: `app/actions/events.rs` `AppState::apply_workspace_git_statuses`
takes the `PaneRuntimeRegistry` and calls
`Workspace::resolved_identity_cwd_from`, which goes through
`PaneRuntime::follow_cwd` (/proc reads) for every result.
`AppState::runtime_for_pane_in_workspace` also takes the registry.

Claim broken: AGENTS.md "State is separated from runtime. `AppState` is
pure data, testable without PTYs or async", and the `AppState` doc comment
("state reaches a runtime only through the registry it is handed").
Handing it the registry is exactly how the rule is sidestepped. The resolved
cwd should be computed by `App` and passed in, so `AppState` only compares.

## 7. `window_title_for_target` does not do what its doc says

Where: `app/window_title.rs`. The doc for `window_title_without_workspace`
says "`None` when window titles are disabled or every token resolved
empty". It returns `Some("")` (or `Some` of just the literals) when every
token is empty; the test `a_client_with_no_workspace_renders_no_workspace_or_pane_target`
asserts `Some("|||x")`. The server gets the claimed behaviour only because
`server/headless.rs` then runs `sanitize_window_title_text`. Fix the doc or
move the sanitising into this function so the documented contract holds
where it is stated.

## 8. `toggle_pane_zoom` reports "pane not found" after it moved focus

Where: `app/actions/pane.rs`. It focuses the pane first (marking the
session dirty), then returns `None` if `set_zoomed` did not take. The only
caller (`api/panes/geometry.rs` `handle_pane_zoom`) turns `None` into
`pane_missing`, so the requester is told the pane does not exist while its
focus has changed. `None` is documented as "the pane is not in the
workspace". A refused zoom should be a separate outcome.

## 9. Render and projection invalidation happen even for failed requests

Where: `app/api.rs` `handle_api_request_with_render` and
`handle_endpoint_command_with_render` decide `RenderDemand::Full` and bump
the shell projection from the method's static `mutates_ui` trait before
dispatch, so a rejected command (unknown pane, bad ratio, out-of-bounds
move) or a hook report for a pane that does not exist still forces a full
render and a snapshot rebuild on every client. Same hot-path claim as
finding 2; the handlers know whether they changed anything.

## 10. A held pane exit can be checkpointed again and again under steady dirtying

Where: `app/session.rs` `pane_exit_checkpoint_settled`
(`pane_exit_checkpoint_pending && !session_dirty`), `SessionSaver::schedule`
(clears `pane_exit_checkpoint_pending`), and the loop order in
`server/headless.rs` (`sync_session_save_schedule` in step 3,
`drain_server_events` in step 4, replay in step 5).

Nothing bounds this: `CHECKPOINT_MAX_FAILURES` counts only failed saves.
If anything dirties the session between the checkpoint's capture and the
replay (a bookmark move from an active client in step 4, a session-ref hook
report, a cwd change), the replay finds it unsettled and requests another
full checkpoint (whole session plus history) and holds the exit again. Under
continuous dirtying the dead pane stays in the layout and the session is
rewritten back to back. The checkpoint only has to prove the pre-exit layout
is on disk; later dirt does not undo that. Settle on "a checkpoint captured
after this exit was held succeeded", tracked by generation, not on the
global dirty flag.

## 11. The final save is skipped whenever a pane-exit checkpoint is the latest save

Where: `app/session.rs` `save_session_before_teardown_async`
(`preserve_checkpoint` returns before the final capture).

Claim broken: the comment there says the final save is "Captured while
every pane runtime still exists, so the final save holds each live pane's
history". When the latest save was a pane-exit checkpoint and nothing has
dirtied the session since, the final save is skipped entirely. So with
`experimental.pane_history` on, the other panes lose the output they wrote
between the checkpoint and shutdown, and so does any cwd that is only known
from `/proc` (not dirtying). Keeping the checkpointed layout and taking
fresh history are separate decisions: the final save could reuse the
checkpoint's structure with current history.

## Smaller items and smells

- `app/api/detect.rs`: the hook-authority skip answer is a hand-written
  `serde_json::json!` object that mirrors
  `shepr_agent::detect::manifest::explain_to_json_value`'s schema field by
  field. Nothing keeps the two in step. It also reports the raw
  `terminal.state` label, which can say `unknown`, although AGENTS.md says
  Unknown presents as Idle (diagnostic output, so low).
- `app/events.rs` handles `ClipboardWrite` and `GitStatusRefreshed` before
  `AppState::handle_app_event`, which keeps dead arms for them "for
  exhaustiveness", plus a PaneDied arm that only logs a warning.
  `AppState::handle_app_event` is `pub` and returns a `StateUpdate` no
  production caller reads. Splitting `AppEvent` into state events and
  App-level events would let the type system say this.
- `create_default_workspace` failing (for example a shell that stops
  resolving after launch) is retried on every loop wake by
  `create_automatic_workspace(None)` while a client is attached, and logs an
  error each time, with no backoff.
- `start_pending_agent_resumes` and `pending_agent_resume_wakeup` redo the
  per-workspace layout walk (`has_pending_agent_resume_candidates`) on every
  loop iteration for as long as a plan stays pending but not eligible (a
  workspace recorded at 0x0, or one never laid out).
- `app/actions/pane.rs` test-only `resize_pane` hard-codes `0.05` instead of
  `DEFAULT_PANE_RESIZE_AMOUNT`, so the test helper and the endpoint can
  drift apart.
- `handle_workspace_rename` and `handle_workspace_create` store any label
  as given (`set_custom_name(String)`), including an empty one, which pins
  a blank name and stops the auto label for good. `handle_pane_rename` trims
  and treats empty as clear. Today the client trims first, so this is only
  an asymmetry in the server contract.
