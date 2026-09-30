# Defect hunt: shepr-client endpoint, transport, handshake, loop, input, terminal

Scope: `crates/shepr-client` minus `src/shell/` and `src/shell_runtime.rs`
(both read where a value crossed into them). Findings are ordered by
severity. Each names the claim it breaks.

---

## 1. The stdin reader polls the fd while reading through `StdinLock`'s buffer, so it splits escape sequences it already holds

**Where:** `src/input.rs`, `stdin_reader_loop` and `flush_idle_input`.

**Claim broken:** the reader promises "Reads host input, frames and parses it
once". The same crate knows the hazard: `terminal_setup.rs`'s
`query_host_escape_disambiguation` says "Bypass StdinLock's shared buffer so
poll and read observe the same bytes."

**What happens:** `stdin_reader_loop` does `let mut reader = stdin.lock();` and
`reader.read(&mut scratch)` with a 4096-byte scratch
(`HOST_INPUT_READ_CHUNK_BYTES`). `StdinLock` is a `BufReader` with an 8 KiB
buffer. `BufReader::read` only skips its buffer for reads at least as large as
the buffer, so it fills up to 8 KiB from the fd and hands back 4096 bytes. The
other bytes sit in user space. `flush_idle_input` then asks
`poll_fd_readable(reader.as_raw_fd(), ...)` whether more input is coming. The
fd is empty, so poll times out, and `flush_timeout_framed` releases the pending
prefix as a lone ESC, Alt+`[` or a broken mouse report. Only after that does the
next `read` return the buffered tail, and it gets framed as plain text.

**When it triggers:** any time 4097 to 8192 bytes are waiting on the tty and the
4096-byte cut falls inside an escape sequence. The stdin thread feeds the
bounded `event_tx` channel (capacity 256) with `blocking_send`, and that channel
is shared with every endpoint reader. So a busy client loop blocks the stdin
thread, and input piles up in the kernel. A mouse drag or wheel burst during
heavy pane output is exactly this case. The tail of an SGR report
(`4;37M`, `<35;64;37M`) then goes to the focused pane as keystrokes, which means
into an agent's prompt. Every split also adds the idle-flush waits
(`MOUSE_ACTIVE_ESCAPE_SEQUENCE_FLUSH_TIMEOUT_MS`, then
`held_input_flush_timeout_ms`) of delay. Bracketed pastes are safe because the
framer holds an unterminated paste with no deadline.

**Fix direction:** read the raw fd in the loop, the way the startup probe does
(`shepr_platform::read_fd` on `io::stdin().as_raw_fd()`), so readiness and data
come from the same place. `input.rs` tracks upstream herdr, so check whether
upstream has the same bug and send the fix both ways.

---

## 2. A pane surface patch the shell rejects is dropped silently, and nothing recovers

**Where:** `src/lib.rs`, `handle_server_message`, the `PaneSurfacePatch` arm
(`ClientPaneSurfacePatchOutcome::Rejected => false`). It depends on
`shepr_protocol::surface_reuse::Decoder` running on the reader thread.

**Claim broken:** the connection's decoder "happens before activation and
presentation filtering, so switching endpoints cannot discard a baseline needed
by the next wire message" (`surface_reuse.rs`). The decoder does keep its
baseline. The shell's displayed surface is a second baseline, and nothing keeps
it in step with the first. When the reader thread sees a baseline mismatch it
fails the connection. When the shell sees one it does nothing at all: no
compose, no repaint request (the protocol has no repaint request), no
connection failure.

**How the two baselines drift apart:**

- `PresentationGate::decide` drops `PaneSurfacePatch` whenever frames are
  frozen. The decoder has already applied those patches.
- During `ActivatingTarget` (frozen), a full `PaneSurface` goes into the
  handoff's evidence. Patches after it are dropped. `complete_at` then installs
  the older evidence surface.
- During `SynchronizingPresentation` (not frozen), patches for the new target
  are `Apply`'d against whatever the shell holds, while the full sync surface is
  `Buffer`ed into evidence. Evidence needs the snapshot too
  (`coherent_surface` wants `snapshot_revision == projection_revision`). So
  patches that arrive between the sync surface and its snapshot are rejected
  against the old surface. At commit the shell installs the sync surface, which
  is now several patches behind the decoder. After that, every patch fails
  `patch.base_surface_revision != current.surface_revision`.

**Effect:** the pane stops updating until the server happens to send a full
surface, which only follows a projection change (a snapshot change, a resize, a
focus change). In an idle shell pane where the user is typing, that can mean
typing blind indefinitely. Whether the sync window happens in practice depends
on the order the server emits reply, snapshot and surface after a
`ClientShellSurfaceSet { active: true }`. That order is not guaranteed anywhere
the client can see. The handoff fences are the only thing that currently
re-establishes coherence.

**Fix direction (structural):** keep one surface baseline per connection. The
decoder's `current_surface()` already holds the authoritative grid. Present from
it, or have the shell's surface come from it at commit, instead of keeping an
independent copy that patches have to line up with. At the least, a shell-side
rejection should fail the connection like a decoder mismatch does, or trigger a
server repaint, rather than silently freezing the pane.

---

## 3. Endpoint health measures client-loop latency, not transport liveness, and after a stall it expires before draining queued pongs

**Where:** `src/endpoint/health.rs`, `registry.rs` (`received`,
`tick_health`), and `lib.rs` `ClientLoop::run` (the `biased` select with the
timer first).

**Claim broken:** `crosses_ssh` says machine connections get "heartbeats and a
silence deadline", and `HEARTBEAT_TIMEOUT` is documented as tolerating "missed
scheduling and transport jitter before marking the endpoint offline". In
practice the endpoint is marked offline because of the client's own stall.

**What happens:** `received(now)` is stamped when the client loop processes a
message, not when it arrives. When the loop wakes after a stall, the overdue
timer wins the `biased` select, so `handle_timer` calls `tick_health` first.
That sees an outstanding `ping_sent_at` older than `HEARTBEAT_TIMEOUT`, or a
missing first snapshot past `connected_at + HEARTBEAT_TIMEOUT`, and returns
`Expired`. The `HealthPong` and snapshot frames are sitting in `event_rx`, but
the loop has not read them yet.

**What stalls the loop:** everything the loop does synchronously.
`HostTerminalWriter` writes to stdout blocking, so a host terminal that stops
reading (XOFF or scroll lock, a slow forwarded tty, a suspended multiplexer
above shepr) blocks the loop. `forward_clipboard` and `ClipboardWrite` run the
native clipboard helper inline, up to `CLIPBOARD_HELPER_TIMEOUT`. Stalls longer
than about 10 s disconnect every machine at once, each with "endpoint health
check timed out".

**Fix direction:** stamp liveness on the reader thread (an atomic per
connection, updated as each frame arrives) and let `tick_health` read that.
Or drain `event_rx` before judging health on a timer wake.

---

## 4. Host effects from the active endpoint are applied while nothing owns the presentation

**Where:** `src/endpoint/message_policy.rs` `PresentationGate::decide`, and
`lib.rs` arms for `MouseCapture`, `ClientShellKeyboardReportAll`,
`WindowTitle` and `Clipboard`.

**Claim broken:** `shell_runtime::active_endpoint_owns_presentation`: "This is
also the pane input gate: input, endpoint commands and host effects flow only
while it holds". `finish_client_shell_input` says the same for the outgoing
direction ("Pane input and host effects flow only to an endpoint that owns the
presentation ... or reaches an endpoint while nothing owns it").

**What happens:** the gate drops presentation effects only when
`frozen && activation_pending`. `Presentation::Unavailable` with no handoff has
`activation_pending == false`, so the gate falls through to
`_ if self.endpoint_active => Apply`. `endpoint_active` looks only at the
registry (the active id plus `surface_active`), never at `Presentation`. The
states documented as `Unavailable` while the connection keeps its surface are:
a rollback that ended `Unavailable` out of `RestoringSource` or
`SynchronizingPresentation`, and an Attention status for the active endpoint.
In those states the endpoint's mouse mode, report-all, title and clipboard
writes all reach the host. The later automatic re-proof replays the effects
anyway, since the server resets its dedupe on activation. So the early
application buys nothing and breaks the invariant.

**Fix direction:** give the gate the `Presentation` (or `owned()`), and treat
`is_presentation_effect` plus `Clipboard` as `Drop` unless owned.

---

## 5. A writer failure is reported as "server closed connection", and the real error is lost

**Where:** `src/endpoint/writer.rs` and `src/transport.rs`.

**Claim broken:** the writer keeps its error so that
`EndpointRegistry::take_failures` can report it. It never gets there.

**What happens:** `start_endpoint_transport` / `Connected` give the reader
thread the writer's `stop_handle()`. On a write error the worker stores the
error and sets `worker_stop`. That is the same `Arc` the reader's
`EndpointReader` checks, so the reader returns `Ok(0)`, `read_message` becomes
`UnexpectedEof`, and the reader sends `ServerDisconnected` ("server closed
connection"). If that event is handled before the next timer,
`handle_server_disconnected` calls `fail`, and `record_failure` removes the
connection. `take_failures` only calls `take_error()` on connections still in
the map, so the stored `endpoint write timed out` or `EPIPE` is dropped. The
diagnostic and log then say the peer closed when the client's write side
actually failed. This matters for SSH triage.

**Fix direction:** give the reader its own stop flag, or have `record_failure`
drain `take_error()` from the connection it removes and prefer that error.

---

## 6. `complete_at` failures are not rolled back as the code says; the half-applied registry state waits for the 5 s activation timeout

**Where:** `src/endpoint/activation.rs` `complete_at` and
`shell_runtime::complete_endpoint_activation`.

**Claim broken:** the comment in `complete_at`: "if either does not, the
handoff is rolled back like any other activation failure rather than presenting
a surface under the wrong projection."

**What happens:** before its checks, `complete_at` already calls
`set_surface_active(lease, true)` and possibly `set_active(lease)`. If
`activate_endpoint_projection` or `endpoint_is_active` then fails, it returns
`Err`. `complete_endpoint_activation` only calls
`receive_endpoint_unavailable(error)` and returns `Ok(None)`. No rollback
starts. The handoff stays in `ActivatingTarget`/`RestoringSource` with the
registry's active id possibly already moved to the target, frames frozen and
input closed, until `handle_timer` notices `expired()` (up to
`ACTIVATION_TIMEOUT`) and rolls back from there. A `RestoringSource` failure
then ends `Unavailable` even though the source was fine.

**Fix direction:** on `Err` from `complete_at`, call
`rollback_endpoint_activation` at once. Better, validate before mutating the
registry.

---

## 7. A failed first Local handshake with machines configured throws away its diagnostic

**Where:** `src/lib.rs`, `run_launched_client` (the `initial` match) and
`run_client_loop` (`local_unavailable` sets `Connecting`).

**Claim broken:** AGENTS.md: "A config that fails to decode fails that
handshake and shows as that endpoint's Attention diagnostic." The same goes for
a refusal or a malformed welcome on the launch attempt.

**What happens:** the error goes to `warn!` only. Local is shown as
`Connecting`, and `add_local(.., None, ..)` schedules a fresh attempt right
away. The Attention shows only if the second attempt fails the same way. If the
server has meanwhile stopped (it answered the first hello with
`ServerShutdown`, or it crashed decoding), the user sees "Local is unavailable;
start its server to reconnect" and never sees the first failure. It also costs a
redundant connection.

**Fix direction:** route the launch attempt's error through
`handshake_error(.., Some(mismatch_guidance))` and seed the Local status and
diagnostic with it (Attention when `needs_attention()`), as a supervisor
`Status` event would.

---

## 8. `replay_host_theme` runs before the commit it says it follows

**Where:** `src/state.rs` `replay_host_theme` and
`shell_runtime::complete_endpoint_activation`.

**Claim broken:** "Replay the retained physical-host baseline only after an
endpoint owns the committed presentation."

**What happens:** it runs at the top of `complete_endpoint_activation` whenever
the phase is `ActivatingTarget`/`RestoringSource`, before `complete_at` has
validated anything. It also runs when `complete_at` then fails, and again on
every later `Ready` in that phase. The ordering the code actually relies on (the
theme goes ahead of the presentation-sync request on the same transport) still
holds. The doc should say that instead, or the call should move after a
successful `complete_at`.

---

## 9. Smaller defects, smells and stale docs

- **`ClientMessageSink` is a dead abstraction with an infallible `io::Result`**
  (`transport.rs`). Nothing uses the `LocalStream` impl or
  `write_to_local_server`. The registry impl always returns `Ok(())`, so both
  `write_to_server(..).map_err(ClientError::ConnectionLost)?` calls in
  `finish_client_shell_input` are dead error paths that look like they could end
  the client. Delete the trait and call `endpoints.send`.
- **`ClientError::ConnectionFailed` wraps non-connect failures and adds a
  misleading hint.** Its Display adds "Is the shepr server running? Running
  `shepr` starts one." It also wraps `set_nonblocking` and recv-timeout
  failures in `do_handshake` and thread-spawn failures in
  `spawn_endpoint_reader` / `start_endpoint_transport`. The launch path
  (`lib.rs`) also flattens it to `io::Error::other(..to_string())`, which loses
  the kind.
- **Stale "saved endpoint" wording.** `ClientError::EndpointSetup`'s doc and
  Display say "saved SSH endpoints". Machines are `[[machines]]` config entries,
  and nothing is saved.
- **`lib.rs` module doc:** "Forwards OSC 52 clipboard writes from server to its
  own stdout". `forward_clipboard` prefers the host's native clipboard tool and
  writes OSC 52 only as a fallback, or when `prefers_osc52_clipboard`.
- **`CLIENT_EVENT_QUEUE_CAPACITY` doc** says the queue is shared by "the resize
  and server-reader threads". The stdin thread uses it too, and that sharing is
  what triggers finding 1.
- **Handshake timeout chosen by `surface_active`.** `do_handshake` picks
  `LOCAL_HANDSHAKE_READ_TIMEOUT` vs `REMOTE_HANDSHAKE_READ_TIMEOUT` from the
  `surface_active` flag. A supervised Local reconnect passes `false` and gets
  the 60 s "fresh SSH connection" timeout, capped only by `ATTEMPT_BUDGET`. The
  `limits.rs` docs describe the remote timeout as the machines' timeout. Pass
  the link kind instead of reusing the hello flag.
- **Double Detach.** On Ctrl+C (`ClientLoop::run`), on `outcome.detach` and on
  `TerminalUnavailable`, the active endpoint gets `Detach` and then another from
  `EndpointRegistry::drop`. The comments admit it. The server ignores it, but
  one owner (the registry's Drop) would be enough.
- **Expired commands for a non-active endpoint leave the shell's pending
  request uncancelled** (`handle_timer`: `if !shell.endpoint_is_active(..) {
  continue; }`). The response path cancels in the same situation
  (`cancel_endpoint_request`). Lane retirement at source-off makes this hard to
  reach, but the two paths disagree.
- **`start_endpoint_transport(.., lifetime: impl Send + 'static, ..)`** is only
  ever passed `()`.
- **Launch-time Local failure after connect ends the client even with
  machines.** In `run_client_loop`, `start_endpoint_transport(..)?`
  (`try_clone` or thread spawn) returns an error even when
  `local_failure_policy.reconnects_local()`. That breaks "With machines
  configured, losing the local server does not end the client" only for this
  narrow resource failure.

## Lateral (outside scope, noticed while following values)

- **Only keybindings from an endpoint's config are applied**
  (`shell/presentation/config.rs` `apply_endpoint_config` sets `keybinds` only;
  `shell/state.rs` applies it only when `same_keybinding_resolution` differs).
  AGENTS.md says the client "rebuilds its runtime values from" the server's
  config. Sidebar, palette, mouse, copy and confirm settings stay the client's
  launch config for every endpoint. Either the doc overclaims or the shell
  under-applies. The shell hunt should settle which.
- **`install_client_shell_snapshot` (shell_runtime) sends a surface-size resize
  to the endpoint whose snapshot arrived** (`endpoints.send_to(endpoint_id, ..)`),
  even when that endpoint is not active and the size change came from the
  shell's active projection. Worth checking whether a non-active snapshot can
  change `surface_size` at all. If it can, the resize goes to the wrong server.
