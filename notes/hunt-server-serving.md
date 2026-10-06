# Defect hunt: server serving and API

Scope: `crates/shepr-server/src/server/` (headless loop, startup and shutdown
order, client connections and per-client views, the PTY size rule, pane focus
sync, foreground client, render planning and fanout, endpoint commands), the
`shepr-api` crate (schema, client, listener, JSON service, ping, the stops,
`server.summary`, detect explain) and `crates/shepr-daemon`. Followed into
`shepr-server/src/app/`, `shepr-launch/src/stop.rs` and `shepr-client` where a
value crosses. Findings are ordered by how much they matter. Each names the
claim it breaks and says which side is wrong.

## S1. Git status and the `/proc` projection refresh stop on every host whose only client is not presenting, so non-shown machines in the sidebar go stale

Claim broken: AGENTS.md, "A server always computes a workspace's Git branch and
ahead/behind, whatever any sidebar shows", together with "The sidebar lists
every machine expanded ... with its workspaces while it is connected" and the
kept feature "Git status in the sidebar (branch, ahead/behind)". The
`ClientShellWorkspace` the server projects to every client carries `branch` and
`git_ahead_behind`, and inactive shells "still receive control projections"
(`render_targets` doc in `server/clients.rs`).

What the code does: `HeadlessServer::handle_scheduled_tasks_headless` only calls
`App::start_git_status_refresh_if_due` when `has_app_client()`, and the loop's
wake deadline only includes the Git deadline under the same test
(`self.app.next_deadline(self.has_app_client())`). `has_app_client` counts
*presenting* connections (`ClientRegistry::app_client_count` =
`presenting().count()`). The client turns off the surface of every connection
it is not showing (`release_unwanted_views` in
`shepr-client/src/endpoint/view.rs` sends `client_shell.surface.set {active:
false}`). So on every machine the user is not currently looking at, the
server's only client is non-presenting and Git refresh never runs: a commit,
branch switch or push by an agent on that machine leaves the sidebar's branch
and ahead/behind frozen until the user switches to it. A `TerminalCwdReported`
calls `request_git_launch_refresh`, which only marks it due; nothing starts it.

The same gate hits the 1 s `/proc` projection timer:
`shell_cwd_refresh_deadline` returns `None` unless `latest_shell_client()`
(presenting) exists, so `refresh_shell_projection_sources` never runs for a
host shown only in the sidebar. Its doc says the timer "also bounds how long
any missed invalidation can leave a client stale"; for these clients nothing
bounds it.

Which side is wrong: the code. The gate should be "any connected shell"
(every connection receives projections), not "a presenting shell". If the
intent is to save work while nobody is attached at all, the right predicate is
`!self.clients.is_empty()`. `first_app_client` in the `ShellConnected` arm has
the same presenting/connected confusion in the other direction (it counts
presenting clients before the insert, whatever the new client's activity).

## S2. A stop that wakes an idle loop skips the pre-shutdown drain, so the final save misses queued pane events

Claim broken: the comment at the top-of-loop stop check in
`HeadlessServer::run` (`server/headless.rs`): "The drain applies queued state
and agent-session reports so the final save carries them; after a signal it
leaves pane deaths out".

What the code does: that drain (`drain_all_internal_events_with_forwarding`
before `initiate_shutdown`) only runs when the stop is noticed at the top of an
iteration. The common case, `shepr stop` (or `stop --all`, or the restart offer)
arriving while the loop sleeps in `next_loop_event`, takes the post-wait branch
instead: `if self.lifecycle.stop_requested() { ...; self.initiate_shutdown();
match event {...}; continue; }`. It applies at most the one dequeued
`LoopEvent::Internal`, and `continue` lands on the `ShutdownPhase::Stopping`
arm, which goes straight to `complete_shutdown` and the final save.
`save_session_for_exit` does not drain either. Any `AppEvent` still queued in
`AppOutputs` is never applied: `TerminalCwdReported` (the cwd a restored pane
starts in), `AgentProcessDetected` (which agent the pane holds, hence what is
resumed), `PaneLaunchSettled`, and a non-signal `PaneDied` (the dead pane is
saved and comes back on restore). `tokio::select!` picks randomly among ready
branches, so a stop racing a burst of pane events can win it.
`begin_request_dispatch`'s own `initiate_shutdown` has the same gap but every
caller of it is behind the drain loops' stop checks, so the post-wait branch is
the live one.

Which side is wrong: the code. Do the drain wherever `initiate_shutdown` is
first reached on a non-signal stop, or move it into the `Stopping` arm before
`complete_shutdown` so every path gets it.

## S3. `FINAL_SAVE_ANSWER_TIMEOUT` outlives the only client's budget, so the documented "still saving" answer never reaches anyone

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

Which side is wrong: the code. The server must give up before the client
does: `ORDINARY_REQUEST_TIMEOUT` as the comment says, or better derived from
the launcher's `STOP_WAIT_TIMEOUT` minus a margin (the launcher's limit is the
one that matters; `shepr-api` cannot see it, so the bound belongs in a shared
place or the comment's "matches" needs a check).

## S4. A busy refusal without a request id is read by every client as a corrupt response

Claim broken: `ErrorResponse::id` is `Option<String>` precisely so a refused or
malformed request can be answered ("Absent when a refused or malformed request
supplied no unambiguous text ID"), and the listener promises "Refusals are
spoken in the kind's language and name the limit that was reached"
(`server/listener.rs` module doc).

What the code does: the listener answers with `id: null` when the refusal queue
is full (`hand_off` -> `send_busy_refusal(stream, None, ...)`) and when a busy
peer's line does not arrive within `BUSY_REQUEST_ID_TIMEOUT`
(`reject_busy_connection`). `shepr_api::client::read_response_value` rejects any
response whose `id` is not exactly the request's, before looking at `error`:
`ApiClientError::Io(InvalidData, "API response id mismatch ... received None")`.
So the caller never sees `endpoint_busy`. In `shepr_launch::stop`,
`send_stop_request` turns that into `ServerStopError::Io { "could not send the
stop request" }` and `probe_boot` turns it into `status_probe_error`, aborting a
conditional stop's wait outright instead of polling again. (An echoed
`endpoint_busy` to a status ping has the same effect through
`status_probe_has_no_answer` returning false for `ErrorResponse`, which is a
launch-side question but worth fixing together.)

Which side is wrong: the client. One request per connection means any line on
it answers that request; an `ErrorResponse` with `id: None` should be accepted
as the answer, and busy should read as "unanswered, retry" in the stop waits.

## S5. A stopping server closes TUI connections in three different ways depending on timing, two of them unanswered

Claim broken: `read_client_handshake` ("Every recognized build gets this
process's preamble") and the listener's "Refusals are spoken in the kind's
language".

What the code does: `handle_client_handshake` checks the stop latch three
times. Before the preamble: returns, closing the stream with no preamble at all,
so even a client of another build cannot learn it is one, and the client
classifies it as `PreambleError::UnexpectedEof` -> "retry". After the hello:
returns with no welcome, also `UnexpectedEof`. After the writer starts: sends a
proper `server_shutdown()` notice (`HandshakeError::ServerShutdown`, a typed
outcome). Only the last is spoken in the protocol. There is no
`HandshakeRefusal` for "stopping" although there is one for `ServerStarting`.

Which side is wrong: the code. Answer the preamble always, then refuse with a
typed reason (a `ServerStopping` refusal, or the shutdown notice in all three
places) so the client can show Stopping rather than a transient retry.

## S6. Two docs promise reply "focus flags" that no reply carries

Claim broken: `handle_client_shell_app_command` doc
(`server/headless/endpoint_requests.rs`), step 6: "fill the reply's focus flags
against the requester's location"; and `App::handle_endpoint_app_command_with_render`
(`app/api.rs`): the loop "owns ... the reply's focus flags that follow".

What the code does: nothing fills anything after the command; `outcome.result`
is returned unchanged. `EndpointReply` has no focus field, and
`shepr_protocol::command::PaneInfo` says "Focus is part of the
requester-specific shell snapshot". The docs are stale.

Which side is wrong: the docs. Drop step 6 and the clause in `app/api.rs`.
(`EndpointCommandTraits::changes_focus` is also unused by the server; check
whether the client still reads it.)

## S7. "A second surface changes no workspace's size" is false

Claim broken: the comment before `claim_client_geometry(.., Connect)` in the
`ShellConnected` arm (`server/headless.rs`): "A second surface changes no
workspace's size: controlled workspaces keep their controller and uncontrolled
ones keep theirs."

What the code does: while one client presents, the PTY size rule sizes every
workspace for it, but `reapply_controlled_shell_workspace_geometry` records a
controller only for workspaces that client views. A second client that lands
on a workspace the first is not viewing finds it uncontrolled,
`claim_unowned_geometry` succeeds, and `apply_shell_geometry` resizes that
workspace to the newcomer (which is also what the rule says once two clients
present: the only viewer wins). So a second surface does resize panes.

Which side is wrong: the comment. The behaviour follows the documented rule;
reword it to say a second surface takes only workspaces nobody else views.

## S8. A stop accepted during startup can get an empty answer if startup fails after the bind

Claim broken: `reference/session-save-shutdown.md`: "A server that exits without
reaching its final save (its run failed first) answers every waiting stop with
an explicit error rather than an empty answer"; `ServerStopSignal::complete_unfinished_final_save`
("The server calls this on every exit path").

What the code does: `complete_unfinished_final_save` is called only from
`HeadlessServer::release_socket_after_save_observed`, i.e. once a
`HeadlessServer` exists. `start_server` binds the socket (stops are accepted and
waiting from that moment) and can still fail before `HeadlessServer::new`: the
Tokio runtime build (`RunServerError::Runtime`). That path drops `Reserved`
and returns; nobody publishes a result, the process exits under the waiting
connection thread, and the stopper reads `EmptyResponse`, which
`send_stop_request` counts as an accepted stop with no save failure.

Which side is wrong: the code, though the window is tiny. Publish the
unfinished result from `start_server`'s error path too (or own the stop signal
in `Reserved` with a drop guard).

## S9. A late stop arriving after the server waited for answers can still read as a clean stop

Claim broken: `ServerStopSignal::wait_for_stop_answers` doc: "an exit under an
answer still being written would close the connection unanswered, and the
stopping client would read that as a stop with no save failure to report".

What the code does: `run` waits for owed answers once, after pane teardown and
writer retirement, then removes the socket and exits. A `server.stop` accepted
on a connection thread after that wait (the socket is still bound until
`release_socket_after_save` drops the handle, and the runtime then has
`TOKIO_RUNTIME_SHUTDOWN_TIMEOUT` before exit) registers itself as unanswered,
reads the already-published result at once and starts writing while the
process exits. If the final save failed, that client gets `EmptyResponse` and
reports success. Second stoppers are rare (two operators, a retry, `stop
--all` racing a local `shepr stop`), so this is low severity.

Which side is wrong: the code. Either refuse new stops once the result is
published and the answer wait has run (answer `server_unavailable` "already
stopped"), or wait for answers again immediately before the socket goes.

## Lower-severity observations and smells

- `run_server` logs a final-save failure as "the server event loop failed"
  (`bootstrap.rs`): `run` returns `RunServerError::Runtime(final save error)`,
  which is already logged at error level as `persist.save` and is not a loop
  failure. The label misleads whoever reads the log.
- Geometry claims on outer focus gain, pane interaction and connect call
  `mark_view_changed()`, which sends every client (including inactive shells
  and viewers of other workspaces) through a full pass. `apply_shell_geometry`
  already requests recompute of the affected workspace's viewers, and the
  `ShellResize` arm says "a resize must not advance unrelated clients' epoch".
  AGENTS.md's "one client's slow link, resize or scroll never moves another
  client's render path" lists only those three, but the same reasoning covers
  these claims; the epoch bump is wasted work (a re-render and diff per client
  that ends `Unchanged`).
- `handle_api_request_with_shutdown_check` compares the workspace order before
  and after `dispatch_api_request` and re-runs reconcile and geometry, and
  calls `sync_pane_focus` on every request. Every topology change reachable
  from that path (a drained pane death, the automatic workspace) already does
  its own reconcile, geometry and focus settlement, and no API method changes
  topology; the block is dead weight on every hook report.
- `detect explain` reports the raw `AgentState` (`"state": "unknown"` in its
  own test), while AGENTS.md says "Unknown presents as Idle". As a diagnostic
  the raw state may be intended; if so, AGENTS.md's description of `detect
  explain` ("shows the pane's state") could say it is the internal state.
- A `SurfaceRenderDeferred::Unrepresentable` render returns `Owed`, and an
  owed, deliverable client is planned `full` again on every pass, so a client
  whose area somehow escapes the clamp re-renders at the render cadence
  forever. The guard is unreachable today (sizes are clamped at the
  handshake and resize), but the loop it would cause is silent.
- `ShellPaneInput` resolves `pane_runtime` three times and the client twice in
  one arm; `refresh_client_view_keys` rebuilds a `HashMap` of every client on
  every call. Both are cold enough not to matter, noted only because the arm
  is on the input path.

## Checked and consistent

Startup order (lease, logging, bind, restore, gate) and its test; the shutdown
order (notice, final save, teardown, writer retirement, lease, socket) and
`Drop` idempotence; `ping` `starting`/`stopping`; the `server.stop_if_boot`
guard and the `Request` decode that refuses stray or duplicate keys; API
ingress versus app admission; the PTY size rule against its doc; level-based
pane focus sync and the resumed-runtime focus-in; foreground selection
(connect, activation, focus gain, interaction, endpoint command); clipboard
fallback to the foreground client; held endpoint replies versus the render
pass (released only once no projection is owed); outbox closure and reaping;
the retained-patch fanout (damage consumed for all, slot-busy clients owed,
refused clients retried on PTY damage).
