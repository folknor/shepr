# Defect hunt: client endpoints

Scope: `crates/shepr-client/src/` outside `shell/` (main loop, endpoint registry,
supervision, writer, handshake, activation, host terminal setup, input threads).
Findings are ordered by severity. Each names the claim it breaks.

## 1. A configured machine's (and a reconnected Local's) "session not fully restored" notice is always dropped

Claim broken: the server sends `ClientShellError { SessionRestoreIncomplete }`
to every client right after the snapshot ("Every client is told, not only the
first: whoever looks at this server is looking at a session with holes",
`shepr-server/src/server/headless.rs`), and the client shell's
`receive_server_notice` says of this kind: "this one arrives on connect from
whichever machine restored, so it names that machine". Commit c087ce5 is titled
"Tell every client when the saved session did not restore in full".

What happens: `PresentationGate::decide` (`endpoint/message_policy.rs`) applies
`ClientShellError` only through the final `_ if self.endpoint_active => Apply`
arm, and `endpoint_active` in `ClientLoop::handle_server_message` requires the
endpoint to be the registry's active one with `surface_active`. Every
supervisor connection (all configured machines, and Local after any reconnect)
handshakes with `surface_active = false` and is inserted with
`insert_native(.., false, ..)`, so at the moment the notice arrives the
endpoint is never active. The notice is dropped. Only the very first Local
connection at launch (handshaken with `surface_active = true`) can show it.
So after a remote host reboots and restores with holes, or after the Local
server is restarted under a client with machines configured, the user is
never told. The only test of this notice is at shell level
(`shell/tests/presentation_regressions.rs`), which bypasses the gate.

Fix direction: classify `ClientShellError` by kind in the gate. Restore notices
are endpoint metadata (like `EndpointSnapshot`) and should apply from any
accepted connection; the other three kinds answer actions on the active
endpoint and can keep the current rule. Better still, make the restore state a
field of `ClientShellSnapshot` rather than a one-shot message, so it cannot be
lost to ordering or gating at all and is re-delivered on every reconnect.

## 2. SIGTERM / SIGHUP does not wake the client loop; a Local-only client lingers indefinitely

Claim broken: `run_launched_client`: "ctrlc's "termination" feature also
catches SIGTERM/SIGHUP so direct termination signals still run the quit path
and TerminalGuard::Drop."

What happens: the handler only stores `should_quit`. `ClientLoop::run` checks
the flag only at the top of each iteration, and otherwise sits in
`wait_for_next_event`'s `select!`. Nothing wakes that select:

- the stdin thread is blocked in `read_fd`; on hangup it gets EOF/EIO and
  `stdin_reader_loop` just `break`s without sending any event;
- `resize_poll_loop` checks `should_quit` after its 100 ms sleep and exits
  silently when it is set, so on SIGHUP it usually exits before its ioctl can
  fail and send `TerminalUnavailable`;
- Local has no heartbeat (`EndpointRegistry::crosses_ssh`), so with no machines
  configured there is no timer deadline at all;
- `ClientLoop` itself holds an `event_tx`, so the channel never closes either.

With no machines, an idle Local server and no shell timers pending, the client
process keeps running after SIGTERM or after its terminal closed, until the
server happens to send a frame. While it lingers it stays a connected client
of the server with an active surface, so it still takes part in the server's
per-client decisions (PTY size rule, foreground client for the host theme).
With machines configured the 5 s SSH heartbeat bounds the delay.

Fix direction: give the signal handler a clone of the event sender and have it
`try_send` a quit event (or use `tokio::signal` inside the loop's `select!`).
Also have `stdin_reader_loop` report EOF/EIO as `TerminalUnavailable` instead
of breaking silently, and have `resize_poll_loop` not swallow a final failure
just because quit was requested.

## 3. A rollback whose target acknowledged target-off still kills the target's healthy connection

Claim broken: `rollback_at`'s `ReleasingTargetForRollback` arm closes the
target transport with "endpoint did not acknowledge surface revocation"; the
comment says closing is "the only safe local revocation when target-off is not
acknowledged". It is also reached when target-off was acknowledged.

Path (`endpoint/activation.rs`, `lib.rs`): source unavailable at begin (Local
down, presentation `Unavailable`, the user picks a machine), so
`source_available = false`. The target activation fails (focus target gone,
server error, timeout of a phase) and `rollback_at` from `ActivatingTarget`
calls `start_target_release`, entering `ReleasingTargetForRollback`. The target
answers target-off successfully. `receive_response_for_boot_at` handles that
success by `set_surface_active(target, false)` and, because
`!self.source_available`, returns `SurfaceActivationProgress::Rejected` without
changing the phase. The client loop's response arm treats every `Rejected` the
same: `rollback_endpoint_activation`, which calls `rollback_at` again; the
phase is still `ReleasingTargetForRollback`, so it calls `endpoints.fail(target,
TimedOut "endpoint did not acknowledge surface revocation")` and returns
`Unavailable("...; the target connection was closed because no presentation
owner could be proven")`.

Result: in exactly the situation the scope promises to handle (Local lost,
remotes still served), a failed machine switch tears down that machine's
healthy connection, logs a false transport failure, shows a false reason, and
forces a full SSH reconnect. No test covers a successful target-off with an
unavailable source (`activation_tests.rs` covers target loss and source loss,
not this).

Fix direction: an acknowledged target-off with no source to restore should end
the handoff `Unavailable` directly (a distinct progress variant, or have
`receive_response_for_boot_at` return a terminal outcome the loop maps to
`end_handoff(Unavailable)`), not route through `rollback_at`. More broadly,
`SurfaceActivationProgress::Rejected` conflates "the peer refused" with "the
handoff is finished and failed", and the loop cannot tell which.

## 4. The global panic hook restores the terminal for panics the client is designed to survive

Claims broken: `EndpointSupervisors::spawn_due` explicitly survives a panicking
connection attempt ("A panic loses it with the task; the join error below then
reports no connector, and `return_connector` rebuilds one from the machine so
the endpoint still retries"). `transport::report_disconnect` documents that a
reader's exit is always handed to the loop.

What happens: `run_launched_client` installs a process-wide panic hook that
calls `panic_restore()` on any thread's panic. Release builds unwind
(`overflow-checks = true` in the release profile makes arithmetic panics
reachable). A panic in the `spawn_blocking` connect task (shepr-remote code),
an endpoint reader thread (decoder), the writer thread, the stdin thread or the
resize thread therefore leaves raw mode, leaves the alternate screen and pops
the keyboard modes, then claims the one-shot restore, while the client loop
keeps running and keeps writing frames and mode sequences to a cooked,
main-screen terminal. The final `TerminalGuard::restore` is then a no-op
because the restore was already claimed, so modes the loop re-enabled after
the panic (mouse capture, report-all) are never undone.

A panicked endpoint reader additionally never sends `ServerDisconnected`; for
Local, which has no heartbeat, that endpoint is silently frozen until exit.

Fix direction: either make the hook restore only for the main/loop thread and
turn helper-thread panics into loop events (reader: report a disconnect from a
drop guard; connect task: already a `JoinError`), or set `panic = "abort"`
semantics deliberately and stop claiming survivability in the supervisor.

## 5. The disconnect notice text does not read as the documented sentence

Claim broken: `handoff_interrupted_notice`'s doc says `notice` is "the same
predicate the active-endpoint path shows after the label ("connection was
lost; reconnecting", "was removed or re-pointed"), so both read as one sentence
about the named machine", and its test asserts those strings.

What happens: the only production caller (`ClientLoop::handle_timer`) passes
`format!("{}; reconnecting", failure.message)`, where `failure.message` is a
raw `io::Error` string. From a reader that is `EndpointFramingError`, which
itself embeds the storage key, so the user sees "build endpoint ssh:build:
server closed connection; reconnecting", "Local endpoint local: server closed
connection; reconnecting", or "Local Broken pipe (os error 32); reconnecting".
"was removed or re-pointed" names a removed feature: machines are fixed at
launch and cannot be removed or re-pointed.

Fix direction: map failure kinds to fixed predicates for the UI (and keep the
raw error for the log and the machine diagnostic), and drop the
removed/re-pointed wording.

## 6. Smaller defects and contract drifts

- `lib.rs` module doc: "Handles ServerShutdown gracefully (clean exit, ...)".
  With no machines, `ServerShutdown` returns `Err(ClientError::ServerShutdown)`,
  which `run_launched_client` turns into `ClientRunError::Session`, so a normal
  `shepr server stop` makes the attached client exit nonzero.
- `src/cli/error.rs` `finish_client` doc: "its lines (forwarded notices, then
  the message that ended the session)". `ClientExit` holds one optional message
  and never any forwarded notices.
- `ClientError::Preamble(DifferentBuild)` displays as "server rejected
  handshake: ..."; the server did not reject anything, the client detected a
  different build from the preamble.
- `begin_endpoint_activation` defers a Local pick with the notice "Local is
  reconnecting; selection will resume when it is ready" whatever Local's state
  is, including Attention (build mismatch, permission problem), where it will
  not become ready without outside action.
- `handshake::do_handshake_for_link` calls `set_handshake_recv_timeout(None)`
  "to clear client handshake read timeout", but no receive timeout is ever
  set (reads go through `DeadlineReader`). Dead step with a misleading error
  context.
- `transport::server_reader_thread` names its parameter `should_quit`; it is
  the per-transport stop flag from `NativeEndpointTransport::stop_handle`, not
  the client quit flag.
- `ClientLoop::handle_server_message` checks `completed.generation ==
  generation` after `receive_response` matched the key on that generation; it
  is always true.
- `ClientLoop::handle_endpoint_supervisor` `Connected` composes and presents a
  frame before changing any shell state (status is set Online only when the
  snapshot arrives), so the "machine list" repaint it comments on shows
  nothing new.
- `EndpointSupervisors::record_status` resets Local's attempts on every
  `Online`, while SSH waits for `STABLE_CONNECTION_PERIOD` ("A brief
  maintenance wake can complete a handshake without restoring the link"). A
  Local server that accepts and then dies (crash loop, or a stopping server
  that welcomes then sends `ServerShutdown`) is retried every
  `INITIAL_RETRY_DELAY` forever, never backing off.
- `EndpointHealth`: a frame the reader stamped between `tick_health`'s sync and
  its `HealthPing` send is synced on the next tick as `received`, which clears
  `ping_sent_at`, so the probe counts as answered by a frame that predates it.
  Effect is only a detection delay of up to one interval.
- An endpoint left `Unavailable` by `rollback_at` from `RestoringSource` (or
  from synchronizing the source) has had surface-on sent and never released;
  its server keeps this client's surface active (and the client in its PTY
  size and foreground decisions) while the client drops everything it sends.
  `ActivationRollback::Unavailable` releases nothing on the way out.

## Structural observations

- "Which endpoint" is held in six places that must agree: the registry's
  `active`, `Presentation` (`Owned` / `Handoff` / `Unavailable`), the selection
  tracker's `selected` / `attempt` / `failed`, `ClientState::deferred_local`,
  `ClientLoop::scheduled_activation`, and `PendingEndpointActivation::successor`.
  `ClientLoop::run` re-derives agreement every turn (`settle`, then
  `automatic_activation`), and `begin_endpoint_activation`,
  `complete_endpoint_activation`, `rollback_endpoint_activation` and
  `handle_endpoint_disconnect` each patch a subset. Findings 3 and the last
  item of section 6 are both consequences of an outcome that one layer
  considers final and another layer re-interprets. One owner for the endpoint
  choice (a single enum covering selected, deferred, handing off from/to,
  failed-on-generation) would remove most of these interactions.
- The handoff protocol (source-off first, six phases, rollback through
  target-off and source-on, successor intents, effects fence) exists to keep
  at most one server-side surface on and pane input ordered. Most of its
  complexity comes from treating a server-side surface as an exclusive lease.
  If surface activation were idempotent per connection and the client simply
  chose which connection's frames to draw and which to send input to, with the
  server told only "viewing" vs "not viewing" for its foreground and PTY size
  rules, the rollback paths (and finding 3) would disappear. Worth weighing as
  a rewrite rather than another round of patches to `activation.rs`.
- The loop's wakeup sources are split (event channel, supervisor channel,
  timer from five deadline sources, a signal flag that wakes nothing). Folding
  the quit signal and every helper-thread exit into the one event channel would
  make finding 2 structurally impossible.
- `PresentationGate` decides by message type alone; `ClientShellError` carries
  kinds with different ownership (finding 1). Gating should be by what the
  message is about, not its envelope.
