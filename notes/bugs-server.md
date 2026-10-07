# Bugs: server (shepr-server app, ui and serving, shepr-api, shepr-daemon)

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

Filed from the server app and render hunt and the server serving and API hunt.
The raw reports,
including each one's list of areas checked and found sound, are in commit
6dc81572 (`notes/hunt-server-app.md`, `notes/hunt-server-serving.md`,
`notes/hunt-mux-git.md`).

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

## SRV-027 - `request_pane_exit_checkpoint`'s doc says `None` means the exit is settled

Raised as a lateral by the wave reviewer.

Its doc says `None` means "the exit is already settled". It also returns
`None` when the save policy disallows saves, a case `decide_pane_exit`
(`crates/shepr-server/src/app/events.rs`) now checks first and records as
`CheckpointDecision::Released`. Reword the doc so `None` covers both.

## SRV-028 - The delta planner falls back to a full surface silently when its own admission check fails

Raised as a lateral by the wave 2 reviewer.

`delta::message` (`crates/shepr-surface/src/delta.rs`) now runs the full patch
admission (spans times panes, plus hyperlink validation) on every planned
update, per client, and quietly sends a full surface when the check fails. A
producer bug would then never be logged, and the check costs work on the
render path. Either log the fallback (debug or warn) or keep the check as a
test assertion only.

## SRV-029 - The shutdown drain runs after clients are told the server is stopping

Raised as a lateral by the wave 2 reviewer.

The pre-save drain now sits in the `Stopping` arm of `HeadlessServer::run`
(`crates/shepr-server/src/server/headless.rs`), after `initiate_shutdown`. A
non-signal pane death drained there runs `reconcile_client_shell_locations`,
`sync_pane_focus` and the geometry reapply while clients are being told the
server is stopping. Harmless as far as the reviewer could see; draining before
`initiate_shutdown` on the post-wait branch would avoid it.
