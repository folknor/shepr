# Hunt: server serving and pure render

Scope: `crates/shepr-server/src/server`, `crates/shepr-server/src/ui`,
`crates/shepr-daemon`. Read-only pass. Findings are ordered roughly by impact.
Each one names the claim it breaks.

## 1. A valid `ui.mouse_scroll_lines` gets the client disconnected on every wheel tick

Claim broken: AGENTS.md "Config is read and validated once at launch ... Any
config problem fails the launch". A value the validator accepts must work.

- `shepr-config` accepts `ui.mouse_scroll_lines` from 1 to `u16::MAX`
  (`crates/shepr-config/src/limits.rs`, `MAX_MOUSE_SCROLL_LINES = u16::MAX`).
- The client puts that value into every wheel event it sends:
  `lines: self.config.mouse_scroll_lines` (`crates/shepr-client/src/shell/input/mouse.rs`).
- The server's read loop charges each wheel event `lines` "expanded events"
  (`pane_input_event_limit` in `server/client_transport.rs`) and treats more
  than `MAX_INPUT_EVENT_BATCH` (4096) as `TooManyEvents`: it logs "oversized
  targeted pane input batch, closing" and disconnects the client.

So `mouse_scroll_lines = 5000` passes validation and then every scroll over a
pane drops the connection. The client reconnects and is dropped again on the
next scroll. Smaller values fail the same way when several wheel events share
one batch (1366 events at the default of 3). The charge is also wrong on its
own terms: under `WheelRouting::MouseReport` and `AlternateScroll`,
`apply_scroll` sends one report per event and ignores `lines`. Only host
scrollback uses the count, and that is a scroll distance, not input work.
Key `repeat_count` has the same shape: it is a `u16` charged one for one.

Fix direction: stop charging `lines` as events. Count one per wheel event and
bound it where it is spent (the host-scroll distance). Or give the config and
the server limit one shared bound in `shepr-protocol`, so the validator
refuses what the server would refuse.

## 2. Every sent frame makes the loop redo the per-client work its dirty flags exist to avoid

Claims broken: the `immediate_pty_sources_dirty` field doc in `headless.rs`
("Recomputing it on every loop wake walked every pane per PTY notify ... a PTY
render wake changes neither"), and AGENTS.md "Hot paths multiply ... times
clients".

- The writer thread sends `ServerEvent::ClientWriterDrained` for every render
  item it writes (`client_writer_loop`), so every frame to every client comes
  back as a server event.
- `apply_server_event` begins with `self.immediate_pty_sources_dirty = true`
  for every event, including `ClientWriterDrained` and `ClientShellPaneInput`.
- On the next iteration this runs `sync_immediate_pty_sources`, which walks
  every client's workspace layout and builds a `HashSet`. It also sets
  `host_input_modes_dirty`, which runs `stream_host_mouse_capture_mode` and
  `stream_shell_keyboard_mode`. Those take terminal-core reads on every
  client's focused pane.

The result is the per-wake recompute the flag was added to remove, now once
per frame per client and once per keystroke. The events that actually change
the inputs (connect, disconnect, surface set, endpoint commands, internal
events) are a small subset.

## 3. Transport-only events rebuild the shared session snapshot, `/proc` reads included

Claims broken: `render_and_stream` "Rebuild the shared session only when
application state that feeds it changed", and the `snapshot_pane` comment in
`app/api/session.rs` (runs "once per pane for every session snapshot").

`handle_server_event_with_render_impact` exempts only `ClientShellPaneInput`
and `ClientShellEndpointRequest` from `mark_shell_projection_dirty`. Every
other server event that returns true bumps `shell_projection_revision`, and
the next render then rebuilds `ShellSessionCache` (`app.session_snapshot()`,
which reads `/proc` for every pane's cwd and foreground cwd) and re-projects
every client. That covers:

- `ClientWriterDrained` with a deferred render. A backpressured client (slow
  SSH) defers most frames, so the session is rebuilt about once per frame.
- `ClientShellResize`, which always returns true for an active client. A
  window drag rebuilds per resize event.
- `ClientShellFocus`, `ClientShellHostTheme` and `ClientShellPresentationSync`.

None of these changes anything a `ClientShellSnapshot` carries. The snapshot
has no field for client size, focus or writer state.

Structural fix for 2 and 3: have each event handler return what it
invalidated (projection, immediate PTY sources, host input modes, geometry,
surface) instead of setting blanket flags at the top of the dispatcher. Keep
transport signals (`ClientWriterDrained`) out of that path entirely: a drained
writer only needs `take_deferred_render`.

## 4. A client the server drops is never disconnected

Claims broken: the comments in `render_and_stream` and
`set_client_shell_surface_active` ("drop the client: it reconnects with a
fresh counter").

`remove_client` only takes the `ClientConnection` out of the registry.
Nothing shuts the socket:

- The read thread still holds `endpoint_control_writer`, a
  `ClientControlWriter` clone made in `handle_client_handshake`. So the writer
  queue's `senders` never reaches 0 and the writer thread waits in `recv`
  forever.
- The read thread keeps reading and forwarding events for a `ClientId` that is
  gone. They are dropped silently. `handle_client_shell_endpoint_request`
  returns without a reply for an unknown client, so every command waits out
  its client-side timeout.
- `HealthPing` is answered by the read thread itself, so the client's health
  check keeps passing.

The client sees a healthy, silent connection and never reconnects. The paths
that remove a client this way are: exhausted projection or surface revisions,
a snapshot that fails to frame, and a frame serialize error other than
`Oversized`. They are close to unreachable today (`MAX_MESSAGE_SIZE` is 1
GiB), but the contract is still false, and any future server-side drop inherits
it.

Fix direction: give `ClientConnection` ownership of the connection's lifetime
(a close handle that calls `shutdown(Both)` on a cloned stream, or a close item
the writer handles by shutting the socket). Then removing from the registry
closes the connection. Separately, the pong should not bypass the loop if it
is meant to show that the server, and not only its reader thread, is alive.

## 5. A config too large for the welcome exits as "failed", not "config refused"

Claim broken: `run_server`'s own comment ("A config the welcome cannot carry
fails the launch here, like any other config problem") and the
`daemon_exit` classes (`CONFIG_REFUSED_EXIT_CODE`, "configuration or paths
were refused").

`ensure_config_fits_welcome` returns `io::Error`, which becomes
`RunServerError::Io`. `shepr-daemon`'s `report_server_error` then exits with
`FAILED_EXIT_CODE`. A launching client reads `DaemonExit::Failed` ("failed to
start") instead of `ConfigRefused` ("refused its configuration"). It also runs
after the data-directory lease is taken, so it is not a config check in the
`serve()` sense. It belongs next to `load_validated` in `shepr-daemon`, or it
should need a `RunServerError::ConfigRefused` variant.

## 6. A readiness error on the client listener skips the final save and the shutdown notice

Claim broken: `ctrlc_handler`'s rationale ("without it a signal kills the
server without the shutdown sequence that saves the session"), and
`HeadlessServer::run`'s documented shutdown sequence.

In `run`'s `select!`, the `client_listener_ready.readable()` arm does
`Err(err) => return Err(err)`. That returns straight out of `run`: no
`initiate_shutdown` (clients get no `ServerShutdown`), no
`save_session_before_teardown_async`, and no pane teardown wait. Only `Drop`
runs, and it only releases the lease and sockets. The `accept_client_connections`
error path next to it does the right thing (`run_error = Some(err);
self.initiate_shutdown()`). This arm should too.

## 7. The listener can strand pending connections after an accept error

`run` clears AsyncFd readiness (`guard.clear_ready()`) before
`accept_pending_client_connections` drains the backlog. The drain loop breaks
on any accept error other than `WouldBlock` (for example `EMFILE`) with
connections still queued. Readiness is edge-triggered and already cleared, so
those connections wait until some new connection arrives. Clear readiness only
when `accept` reports `WouldBlock` (tokio's `try_io` pattern), or re-arm on
error. Related: `accept_pending_client_connections` returns `io::Result`, but
no path ever returns `Err`, so the `run_error` branch that handles it is dead.

## 8. Endpoint replies do not leave in command order

Claim broken: `handle_client_shell_endpoint_request` doc: "Commands from one
client run in arrival order ... and the replies leave in the same order", and
`flush_endpoint_replies` "in the order the commands ran".

Normal replies wait in `endpoint_replies` for the next render. `StaleBoot` and
`SurfaceInactive` refusals and the `ClientShellSurfaceSet` acknowledgement go
out at once through `send_to_client`, ahead of any reply the same client's
earlier commands are still holding. `reject_endpoint_request_for_shutdown`
calls `flush_endpoint_replies()` first for exactly this reason. The other
immediate paths do not. Either flush first on those paths too, or drop the
ordering claim if the client matches replies only by request id.

## 9. The retained render path resolves string pane ids per source, recipient and pane

Claim broken: AGENTS.md "Hot paths multiply ... use narrow accessors".

`render_retained_pane_surface_and_stream` runs for PTY output to visible panes,
which is the hottest serving path. For each PTY source and each recipient it
does `surface.panes.iter().find(|pane| self.app.parse_pane_id(&pane.pane_id) ...)`.
`parse_pane_id` parses the public id string, scans workspaces linearly with
`resolve_workspace_id`, then maps the pane number. `has_synchronized_pane`
does the same parse for every pane of every recipient, twice per call. The
geometry loop in `render_and_stream` parses every pane of the last surface
again to compare alternate-screen flags. The per-client baseline should keep
the internal `(workspace index or id, PaneId)` next to each wire
`PaneSurfacePane`, so this path never goes back through strings.

## 10. Every client renders its surface from scratch, even when two clients show the same thing

Structural opportunity, not a contract break. `render_and_stream` calls
`render_client_shell_pane_surface` per client (layout, every pane's
`render_into` under its core lock, borders) even when two clients view the same
workspace at the same size and cell size. The typical multi-client case is the
same user on two terminals, so the surfaces are identical. Key the render on
`(workspace id, surface size, cell size)` and share the `FrameData`; per-client
work would then be only the baseline diff. Also,
`snapshot_from_session(cache.session.clone(), ...)` clones the whole
`SessionSnapshot` per client per projection. It could borrow it. The timer path
`refresh_shell_projection_sources` projects every client once to detect a
change, then `render_and_stream` projects them all again.

## 11. A resize promotes a client to foreground and switches every pane's host theme

Claim at issue: `ClientRegistry` doc "which one was active most recently (the
foreground client)", and AGENTS.md "the host theme by the foreground client
(the one last active)".

`ClientShellResize` calls `promote_client_to_foreground`, and
`sync_host_theme_from_foreground` then recolours every pane with that client's
theme. A resize is usually not user activity in that terminal. A tiling window
manager relayout, or a font change on a background monitor, will steal
foreground and flip the theme (and where pane-less clipboard writes go). The
other promotion triggers (input with interaction, outer focus gained, endpoint
command, surface activation) are real activity. Whether resize should count is
an intent question for the owner, but today it does count, and that
contradicts "last active".

## 12. A new client's seed snapshot can be followed by an older one

`ClientShellConnected` builds its seed snapshot from a fresh
`app.session_snapshot()`, not from `shell_session_cache`, and then records
`session_generation = self.shell_session_generation`. If the cache is older,
the next projection from the cache can carry older `/proc` cwd values than the
seed did. The client then gets a snapshot that moves its cwd back until the
cwd timer refresh. Seed from the cache (rebuilding it first if its revision is
stale) so there is one source.

## 13. Stale or wrong comments

These are the "must be true" kind, since they sit in code:

- `headless.rs` module doc: "Renders to a virtual ratatui Buffer in memory".
  It now renders straight to wire cells (`render_surface_virtual` into
  `FrameData`). It also says it "handles ... minimum terminal size", but no
  such handling exists anywhere in the crate.
- `send_to_all_clients` doc: "the only callers are the two shutdown notices".
  There is one caller (`initiate_shutdown`).
- `initiate_shutdown`: "Clear client-local host graphics, then send
  ServerShutdown". Nothing clears graphics.
- `client_shell.rs`: the `snapshot_from_session` doc says projection runs
  again "only when the shared cache generation moves". It also runs when the client's own location
  generation moves (`needs_projection` in `render_and_stream`).

## Smaller smells noticed along the way

- `pane_border_title(label, pane_width, _focused)` takes an unused parameter.
- The retained path's `success!($reason)` macro drops its reason, so the
  success reasons are documentation only. The fallback reasons are logged once
  per lifetime, which hides a fallback that starts recurring later.
- `accept_pending_client_connections` sets each accepted stream nonblocking,
  and the handshake thread immediately sets it back to blocking.
- `render_pane_borders` allocates a `HashMap<(u16, u16), LineCell>` per render
  per client, then checks every pane for each border cell (`line_touches_pane`).
  A grid-sized bitmap or a per-edge pass would be cheaper on the full-render
  path.
- `ui/panes.rs` tests cover `shepr_termio::selection_render` (the
  `automatic_selection_*` and `render_selection_highlight` tests), which is not
  code in this crate. They should live next to it in `shepr-termio`.
- The writer thread sends `ClientWriterDrained` with a `blocking_send` on the
  bounded server-event channel before it writes the frame. A full channel (one
  client flooding pane input) therefore stalls frame writes to every other
  client until the loop drains it. Sending after the write, with `try_send`,
  would break that coupling. A missed signal only matters when a render was
  deferred, and that could be tracked on the shared writer queue instead.
