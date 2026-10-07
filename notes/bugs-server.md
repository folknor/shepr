# Bugs: server (shepr-server app, ui and serving, shepr-api, shepr-daemon)

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

Filed from the server app and render hunt and the server serving and API hunt,
with one entry (SRV-001) shared with the mux and Git hunt. The raw reports,
including each one's list of areas checked and found sound, are in commit
6dc81572 (`notes/hunt-server-app.md`, `notes/hunt-server-serving.md`,
`notes/hunt-mux-git.md`).

## SRV-001 - Git status and the `/proc` projection refresh stop on every host whose only client is not presenting, so non-shown machines in the sidebar go stale

Surfaced by two scopes, which read the code differently; both readings are
kept.

Serving hunt's reading. Claim broken: AGENTS.md, "A server always computes a
workspace's Git branch and ahead/behind, whatever any sidebar shows", together
with "The sidebar lists every machine expanded ... with its workspaces while it
is connected" and the kept feature "Git status in the sidebar (branch,
ahead/behind)". The `ClientShellWorkspace` the server projects to every client
carries `branch` and `git_ahead_behind`, and inactive shells "still receive
control projections" (`render_targets` doc in `server/clients.rs`).

What the code does: `HeadlessServer::handle_scheduled_tasks_headless` only
calls `App::start_git_status_refresh_if_due` when `has_app_client()`, and the
loop's wake deadline only includes the Git deadline under the same test
(`self.app.next_deadline(self.has_app_client())`). `has_app_client` counts
presenting connections (`ClientRegistry::app_client_count` =
`presenting().count()`). The client turns off the surface of every connection
it is not showing (`release_unwanted_views` in
`shepr-client/src/endpoint/view.rs` sends
`client_shell.surface.set {active: false}`). So on every machine the user is
not currently looking at, the server's only client is non-presenting and Git
refresh never runs: a commit, branch switch or push by an agent on that machine
leaves the sidebar's branch and ahead/behind frozen until the user switches to
it. A `TerminalCwdReported` calls `request_git_launch_refresh`, which only
marks it due; nothing starts it.

The same gate hits the 1 s `/proc` projection timer:
`shell_cwd_refresh_deadline` returns `None` unless `latest_shell_client()`
(presenting) exists, so `refresh_shell_projection_sources` never runs for a
host shown only in the sidebar. Its doc says the timer "also bounds how long
any missed invalidation can leave a client stale"; for these clients nothing
bounds it.

Which side is wrong (serving hunt): the code. The gate should be "any connected
shell" (every connection receives projections), not "a presenting shell". If
the intent is to save work while nobody is attached at all, the right predicate
is `!self.clients.is_empty()`. `first_app_client` in the `ShellConnected` arm
has the same presenting/connected confusion in the other direction (it counts
presenting clients before the insert, whatever the new client's activity).

Mux and Git hunt's reading. `GIT_REMOTE_STATUS_REFRESH_INTERVAL` in
`crates/shepr-server/src/limits.rs` says "Refresh Git ahead/behind status
periodically while clients are connected". That hunter read
`GitRefreshScheduler::deadline` as keying only on `has_workspaces`, and, with
AGENTS.md's "A server always computes a workspace's Git branch and
ahead/behind, whatever any sidebar shows", judged the comment the wrong side:
it should drop the client condition.

## SRV-002 - A stop that wakes an idle loop skips the pre-shutdown drain, so the final save misses queued pane events

Claim broken: the comment at the top-of-loop stop check in
`HeadlessServer::run` (`server/headless.rs`): "The drain applies queued state
and agent-session reports so the final save carries them; after a signal it
leaves pane deaths out".

What the code does: that drain (`drain_all_internal_events_with_forwarding`
before `initiate_shutdown`) only runs when the stop is noticed at the top of an
iteration. The common case, `shepr stop` (or `stop --all`, or the restart
offer) arriving while the loop sleeps in `next_loop_event`, takes the post-wait
branch instead:
`if self.lifecycle.stop_requested() { ...; self.initiate_shutdown(); match event {...}; continue; }`.
It applies at most the one dequeued `LoopEvent::Internal`, and `continue` lands
on the `ShutdownPhase::Stopping` arm, which goes straight to
`complete_shutdown` and the final save. `save_session_for_exit` does not drain
either. Any `AppEvent` still queued in `AppOutputs` is never applied:
`TerminalCwdReported` (the cwd a restored pane starts in),
`AgentProcessDetected` (which agent the pane holds, hence what is resumed),
`PaneLaunchSettled`, and a non-signal `PaneDied` (the dead pane is saved and
comes back on restore). `tokio::select!` picks randomly among ready branches,
so a stop racing a burst of pane events can win it.
`begin_request_dispatch`'s own `initiate_shutdown` has the same gap but every
caller of it is behind the drain loops' stop checks, so the post-wait branch is
the live one.

Which side is wrong: the code. Do the drain wherever `initiate_shutdown` is
first reached on a non-signal stop, or move it into the `Stopping` arm before
`complete_shutdown` so every path gets it.

## SRV-003 - `FINAL_SAVE_ANSWER_TIMEOUT` outlives the only client's budget, so the documented "still saving" answer never reaches anyone

Claim broken: the doc comment on `FINAL_SAVE_ANSWER_TIMEOUT`
(`crates/shepr-api/src/limits.rs`): "a stopping client gives up at its own stop
budget (`ORDINARY_REQUEST_TIMEOUT` for the request, which the launcher's stop
budget matches), so a thread that waits past the client's response window
answers nobody". Also `reference/session-save-shutdown.md`: "That wait is
bounded by `FINAL_SAVE_ANSWER_TIMEOUT`; past it the answer says the server has
not reported its final save and may still be saving".

What the code does: the constant is `ORDINARY_RESPONSE_TIMEOUT` (15 s + 5 s
grace = 20 s), not `ORDINARY_REQUEST_TIMEOUT` (15 s). The only caller of the
stops is `shepr_launch::stop` with one `STOP_WAIT_TIMEOUT` (15 s) deadline that
covers connect, request and response. So whenever the final save outlasts the
budget, the client always times out first (`TimedOut`, treated as "accepted,
keep waiting", then `ServerStopError::TimedOut` because the same deadline has
already passed), and the server's explicit `server_unavailable` answer at 20 s
is written to a closed socket. The connection thread also holds an API ingress
slot for those extra 5 s. The comment's own rationale is the thing violated.

Which side is wrong: the code. The server must give up before the client does:
`ORDINARY_REQUEST_TIMEOUT` as the comment says, or better derived from the
launcher's `STOP_WAIT_TIMEOUT` minus a margin (the launcher's limit is the one
that matters; `shepr-api` cannot see it, so the bound belongs in a shared place
or the comment's "matches" needs a check). See also LIFE-012 on the launch and
stop worst-case budgets.

## SRV-004 - A busy refusal without a request id is read by every client as a corrupt response

Claim broken: `ErrorResponse::id` is `Option<String>` precisely so a refused or
malformed request can be answered ("Absent when a refused or malformed request
supplied no unambiguous text ID"), and the listener promises "Refusals are
spoken in the kind's language and name the limit that was reached"
(`server/listener.rs` module doc).

What the code does: the listener answers with `id: null` when the refusal queue
is full (`hand_off` -> `send_busy_refusal(stream, None, ...)`) and when a busy
peer's line does not arrive within `BUSY_REQUEST_ID_TIMEOUT`
(`reject_busy_connection`). `shepr_api::client::read_response_value` rejects
any response whose `id` is not exactly the request's, before looking at
`error`: `ApiClientError::Io(InvalidData, "API response id mismatch ...
received None")`. So the caller never sees `endpoint_busy`. In
`shepr_launch::stop`, `send_stop_request` turns that into
`ServerStopError::Io { "could not send the stop request" }` and `probe_boot`
turns it into `status_probe_error`, aborting a conditional stop's wait outright
instead of polling again. (An echoed `endpoint_busy` to a status ping has the
same effect through `status_probe_has_no_answer` returning false for
`ErrorResponse`, which is a launch-side question but worth fixing together.)

Which side is wrong: the client. One request per connection means any line on
it answers that request; an `ErrorResponse` with `id: None` should be accepted
as the answer, and busy should read as "unanswered, retry" in the stop waits.

## SRV-005 - A stopping server closes TUI connections in three different ways depending on timing, two of them unanswered

Claim broken: `read_client_handshake` ("Every recognized build gets this
process's preamble") and the listener's "Refusals are spoken in the kind's
language".

What the code does: `handle_client_handshake` checks the stop latch three
times. Before the preamble: returns, closing the stream with no preamble at
all, so even a client of another build cannot learn it is one, and the client
classifies it as `PreambleError::UnexpectedEof` -> "retry". After the hello:
returns with no welcome, also `UnexpectedEof`. After the writer starts: sends a
proper `server_shutdown()` notice (`HandshakeError::ServerShutdown`, a typed
outcome). Only the last is spoken in the protocol. There is no
`HandshakeRefusal` for "stopping" although there is one for `ServerStarting`.

Which side is wrong: the code. Answer the preamble always, then refuse with a
typed reason (a `ServerStopping` refusal, or the shutdown notice in all three
places) so the client can show Stopping rather than a transient retry.

## SRV-010 - Title changes on panes with no agent bump the shared projection and the server-wide view epoch

The server app hunt filed this as F1 and the two stale comments as a separate
F6; they are one entry here.

Where: `AppState::sync_terminal_titles` (`app/state.rs`),
`App::sync_terminal_titles` (`app/terminal_titles.rs`), and its callers in
`server/headless.rs` (the render loop's `sync_terminal_title_sources` followed
by `mark_view_changed`, and `sync_pending_terminal_titles` in
`dispatch_api_request` and in `endpoint_requests.rs`).

What happens: `AppState::sync_terminal_titles` stores the new title for every
pane the PTY parser reported. When any raw or stripped title changed, it calls
`mark_shell_projection_dirty()`, whatever the pane is. The only titles that
reach a client are in `ClientShellAgent` (`shepr-protocol/src/projection.rs`),
which `App::agent_info` builds only for panes with an `effective_agent()`.
`ClientShellPane` and `ClientShellWorkspace` carry no title, and the outer
window title is the client's own `shepr: <label>`. So a title change on a plain
shell pane changes nothing any client can see. It still:

- advances `shell_projection_revision`, which makes
  `refresh_stale_shell_session_cache` rebuild `projection_input()`. That
  rebuild runs `snapshot_pane` for every pane of every workspace, including the
  `foreground_cwd_for_pane` `/proc` reads, and advances the session generation;
- returns `true` to the render loop, which calls `mark_view_changed()`. That
  moves the server-wide view epoch and sends every client through a full
  planning pass.

Claims broken:

- The comment on `App::sync_terminal_titles` says "Any changed title also
  changes the shell agent metadata". That is false for panes with no agent. The
  same claim appears on `HeadlessServer::sync_terminal_title_sources` ("a
  changed title updates the shell agent metadata, so it requires a
  projection").
- AGENTS.md, under "Presentation is per client", promises "a server-wide view
  epoch only for changes every client depends on".
- The "Hot paths multiply" principle. A shell whose prompt or preexec hook sets
  the title with OSC 0 or 2 (common in zsh and bash setups, and in vim and
  htop) sets this off on every command, in every pane, for every client.

Fix direction: only mark the projection dirty when a changed title belongs to a
pane that has an effective agent. Keep storing the title for every pane, so an
agent detected later has its title at once; the state change that makes the
agent effective already bumps the revision. The server's
`sync_terminal_title_sources` should then call `mark_view_changed` only when
the projection moved. The simplest way is to return
`observe_projection_change` instead of the raw `TerminalTitleChange` flags. Fix
both comments together with it.

## SRV-011 - The final save under stopped or blocked persistence is reported as an error, and the documented `stopped` / `blocked_on_backup` outcomes are never logged

Where: `App::save_session_before_teardown_async` (`app/session.rs`) and the
final-save logging in `server/headless.rs` (the `kind = "final"` block).

What happens: if the policy is `Stopped` or `BlockedOnBackup`,
`submit_final_session_save` returns `Ok(None)`. The
`Ok(None) if self.session_saver.policy.is_unavailable()` arm then turns that
into `Err("session persistence was blocked before the final save")`. The caller
logs every `Err` at error level with `outcome = Error`. Its info-level branch has arms for
`Outcome::Stopped` and `Outcome::BlockedOnBackup`, but those arms cannot run:
with either mode set, the app always returns `Err`. The only non-error outcomes
that can actually be logged are `ok` and `frozen`. The `Err` also becomes
`RunServerError::Runtime` (an unclean exit), and every accepted `server.stop`
gets it as a failed final save, so `shepr stop` fails.

Claims broken: `reference/session-save-shutdown.md` says the
`persist.save kind = "final"` log records "`ok`, `error`, `stopped`,
`blocked_on_backup` or `frozen`", with failures at error level and "any other
outcome at info level". The message also says "blocked" for the `Stopped`
mode, which is not what happened.

Which side is wrong: the server's dead arms and the document agree with each
other. That suggests the intent was to report these as their own outcomes,
which points at the app's `Err` as the wrong side. Whether a stop should fail
when persistence was already off for the boot is a policy question for the
owner. Either way, one side has to change, and so does the "blocked" wording
for `Stopped`. No test covers a final save under `Stopped` or
`BlockedOnBackup`. See also SRV-015 on how `run_server` labels a final-save
failure.

## SRV-013 - A workspace created with an explicit cwd silently starts elsewhere when that directory cannot be entered, and keeps the name of the directory it is not in

Where: `App::handle_workspace_create` (`app/api/workspaces.rs`) together with
`create_workspace_outcome` (`app/creation.rs`).

What happens: `WorkspaceCreateSource::Cwd(raw)` is only checked lexically
(`launch_cwd`) and then launched with `LaunchKind::Fresh`. Fresh launches fall
back to `HOME`, the passwd home or `/` when the chdir fails
(`LaunchKind::requires_cwd` is false for `Fresh`). `prepare_workspace(cwd)` has
already named the workspace after the requested path. If the user typed a wrong
or nonexistent path:

- the command answers `Done`;
- the workspace appears under the requested directory's name but runs in
  `$HOME`;
- the only trace is a WARN `pane.cwd outcome = Fallback` in the server log.

Claim broken: `WorkspaceCreateSource` documents itself as "Where a new
workspace's first pane starts", with `Cwd` as "An explicit working directory".
Its sibling, `Default`, resolves through the server's new-terminal-cwd policy,
where a fallback is reasonable. An explicit path the user named is a different
case.

Direction: either launch an explicit `Cwd` with a required cwd (a failure
becomes the existing placeholder, "Pane directory is unavailable"), or report
the fallback to the requester. The workspace name should follow the cwd the
launch settled in, or the refusal.

## SRV-014 - An agent resume whose launch status became unreadable is recorded and logged as "shell launch unconfirmed", without its cause

Where: `App::handle_pane_launch_settled`, the
`LaunchOutcome::StatusUnavailable(error)` arm with
`kind == LaunchKind::AgentResume` (`app/pane_launch.rs`).

What happens: it calls
`fail_agent_resume(pane_id, ResumeUnavailableReason::ShellLaunchUnconfirmed, None)`.
The same arm for a non-resume pane records
`PaneStartFailure::launch_unobservable(&error)`, which keeps the cause.

- The resume placeholder tells the user the launch was "unconfirmed", which is
  the reason for the timeout case (`Unconfirmed`). Here the real cause was the
  status channel failing.
- The `error` is dropped from the `fail_agent_resume` log line, which its own
  doc comment calls "the one log line for the failure". The mux coordinator did
  log the channel error, but as a separate error-level line under another event.

Claim broken: the `fail_agent_resume` doc ("`detail` carries the cause a caller
observed ... so the caller does not log it again"), and the distinction
`ResumeUnavailableReason` exists to draw.

Severity: low (a diagnostic only).

## SRV-015 - `run_server` logs a final-save failure as "the server event loop failed"

Raised as a lower-severity observation by the serving hunt.

`run_server` (`bootstrap.rs`): `run` returns `RunServerError::Runtime(final save
error)`, which is already logged at error level as `persist.save` and is not a
loop failure. The label misleads whoever reads the log.

## SRV-016 - Geometry claims on focus gain, interaction and connect advance every client's view epoch

Raised as a lower-severity observation by the serving hunt.

Geometry claims on outer focus gain, pane interaction and connect call
`mark_view_changed()`, which sends every client (including inactive shells and
viewers of other workspaces) through a full pass. `apply_shell_geometry`
already requests recompute of the affected workspace's viewers, and the
`ShellResize` arm says "a resize must not advance unrelated clients' epoch".
AGENTS.md's "one client's slow link, resize or scroll never moves another
client's render path" lists only those three, but the same reasoning covers
these claims; the epoch bump is wasted work (a re-render and diff per client
that ends `Unchanged`).

## SRV-017 - `handle_api_request_with_shutdown_check` re-runs reconcile, geometry and focus on every request

Raised as a lower-severity observation by the serving hunt.

It compares the workspace order before and after `dispatch_api_request` and
re-runs reconcile and geometry, and calls `sync_pane_focus` on every request.
Every topology change reachable from that path (a drained pane death, the
automatic workspace) already does its own reconcile, geometry and focus
settlement, and no API method changes topology; the block is dead weight on
every hook report.

## SRV-021 - Two terminal-core reads per pane where one would do on the render path

Raised as a lateral observation by the server app hunt.

`PaneSurface::cursor` (`ui/pane_surface.rs`) reads the runtime twice per
focused pane per render: once through `runtime.read().cursor(area)` and again
through `pane_is_scrolled_back(runtime)`, which calls
`read().scroll_metrics()`. `compute_pane_surfaces` likewise takes two reads per
pane (alternate-screen state, then scroll metrics). These are per pane, per
client, per pass. One read returning both would follow "keep terminal-core
locks short".

## SRV-026 - A released pane exit during a host-shutdown freeze is recorded as `CheckpointDecision::Settled`

Raised as a lateral observation by the server app hunt.

When saves are frozen or stopped, `request_pane_exit_checkpoint` returns `None`
and `decide_pane_exit` records the decision as `CheckpointDecision::Settled`
("checkpointed, and the checkpoint is already settled"). Nothing was
checkpointed. The removal behaves correctly, because `preserved()` decides what
`finish_checkpointed_pane_exit_after_event` does. Still, the name says
something false, and a future reader of `checkpointed()` could act on it. A
distinct `Released` decision would keep it honest.
