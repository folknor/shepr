# Bugs: client endpoints and remote (shepr-client endpoint lifecycle, shepr-remote)

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

Filed from the client endpoints and remote hunt. The raw report is in commit
6dc81572 (`notes/hunt-client-endpoints.md`). The hunter's structural note
(RMT-014) ties RMT-001, RMT-002 and RMT-003 to one cause.

## RMT-001 - A stale remembered executable is never rediscovered by a running client

Claim broken: `MachineSshConnector::connect` doc ("The handshake result is
returned as-is unless the remote command failed to execute the remembered
path") and its retry arm ("The remote command proved the path stale after
probing. Resolve once more within the same deadline"); `limits.rs`
`ATTEMPT_BUDGET` doc ("a stale remembered path ... is handled by resuming");
`MachineProbe` doc ("only evidence about that executable invalidates it").

What happens: once a machine's executable is `ProbeExecutable::Verified`, every
attach runs the bridge at that path without re-checking it. If the binary is
removed or moved on the host while the client runs, the remote `/bin/sh -c`
exits 127, `ssh_bridge_exit_error` builds an `SshFailureDiagnostic` with origin
`SshOutput(Remote(NotFound))`, which is exactly the `InstallStale` evidence
`observe_failure` looks for. But that error never reaches `connect` intact:

- the handshake sees EOF before the preamble, so
  `handshake::classify_handshake_error` swaps in `bridge.reported_failure()`,
- and it rewraps it as
  `io::Error::new(kind, EndpointFailure::from_error(&failure).with_context(..))`.

`EndpointFailure::from_error` walks the source chain to the inner
`EndpointFailure` (cause `Unclassified`), so the `SshFailureDiagnostic` and its
origin are dropped. Back in `connect`, `failure_evidence` sees a plain
`EndpointFailure` (origin `Message`, evidence `RemoteFault`),
`invalidates_executable()` is false, and the rediscovery arm never runs. The
endpoint fails as `Retry` ("remote command failed (exit status 127)") on every
backoff, forever, until the client is restarted (a fresh client verifies the
disk hint with `test -x || exit 125` and rediscovers).

The asymmetry is visible in the same file: `wait_for_server` passes the raw
`ssh_bridge_exit_error` to `observe_failure`, so the wait path does invalidate;
only the attach path, the one that matters, cannot.

Direction: the bridge failure should stay typed until the connector has looked
at it. Either `classify_handshake_error` keeps the bridge's `io::Error` (adding
context through `SshFailureDiagnostic::with_context`, which keeps the origin),
or, better, `establish` returns the raw `HandshakeError` and the connector,
which owns the bridge, asks the bridge for its failure and classifies once.
Today the SSH-specific evidence is stripped by a client module that has no
business deciding it. No test drives a stale path through a real handshake; the
unit tests only feed `observe_failure` a raw diagnostic.

## RMT-002 - A remote binary replaced under a running client yields a silent retry loop, or a Restart loop that stops servers and starts the wrong build

Claims broken: AGENTS.md, Restart: "stops that server by the boot its status
named and starts this build's"; `host.rs` comment on the build check ("the
client must still read a typed mismatch rather than an EOF it would retry
forever"); `ensure_remote_server_running` doc ("so the client reports a typed
mismatch").

The connector verifies the remote `shepr` once (RMT-001: `Verified` is kept for
the client's life). The owner upgrades hosts by installing over the same path.
Say the client is build C and the host is reinstalled to build N while its
server still runs C. On the next reconnect (any network blip, a laptop
resume):

1. Bridge N runs `attached_server_status`, finds server C, and since C is not
   its own build it answers with `preamble_for(C)` and exits 0
   (`RemoteBridgeOutcome::Closed`).
2. The client reads preamble C, which equals the client's own build, so there
   is no mismatch. It then reads EOF where the welcome should be.
3. `classify_handshake_error` asks the bridge, which exited 0, so there is no
   reported failure; the result is `EndpointFailure::retry("connection closed
   before the endpoint finished connecting")`. The machine sits at
   "Connecting..." and retries on backoff forever with nothing telling the
   operator the install changed. This is precisely the "EOF it would retry
   forever" the `host.rs` comment says the preamble answer exists to prevent:
   the bridge compares the server against its own build, but the client
   compares against the client's build, and the two differ here.

If instead the host's server has already been restarted onto N:

1. Bridge N relays to server N, the client reads preamble N, `DifferentBuild`,
   and the machine shows "Restart (other build)".
2. Operator chooses Restart. `connect(Restart)` calls `resolve_remote`, which
   returns the cached `Verified` path without a round trip,
   `stop_server_of_another_build` stops server N (ending every pane on it), and
   the starting bridge (binary N) starts server N again.
3. The handshake reads preamble N again: `DifferentBuild`, Restart offered
   again. Each Restart kills the host's panes and fixes nothing. What it starts
   is "whatever is installed", not "this build's".

`DifferentBuild` and `Incompatible` from a machine both classify as
`InstallChanged` evidence, which by design does not invalidate the executable,
so nothing ever re-verifies.

Direction: the bridge must report its own build, not only the server's. One
shape: the client passes its build to `remote-client-bridge` (or the bridge
always writes its own preamble first and the server's second), so a bridge of
another build fails as an install mismatch before anything else.
Independently, `connect(Restart)` should re-verify the executable
(`verify_remote_shepr`, one round trip) before it stops anything, and a
`DifferentBuild` from a machine should drop `Verified` back to `Hint` so the
next attempt re-checks.

## RMT-003 - The endpoint reader joins the SSH bridge before the stream is closed, so a client-side disconnect keeps the old session alive

Claims broken: `connection_io.rs` comment in `server_reader_thread` ("EOF means
the bridge worker has closed its stream; joining now drains its bounded SSH
diagnostic"); `SshStdioBridge::reported_failure` contract ("Call it after the
endpoint stream closes ... Called earlier, it waits for the connection itself
to end").

`EndpointReader::read` returns `Ok(0)` when the transport's `stopped` flag is
set, which `read_message` turns into `UnexpectedEof`. The registry sets that
flag on every client-initiated loss: health expiry, a full writer queue
(backpressure), a writer timeout, a rejected surface patch, a `ServerShutdown`
the hub fails itself, and client exit. The reader then calls
`ssh_bridge.reported_failure()`, which joins the bridge worker. But:

- the reader still holds its own clone of the client end of the socketpair, so
  the bridge's upload half never sees `Closed`;
- the reader holds an `Arc<MachineSshBridge>`, so `SshStdioBridge::drop` (the
  only thing that sets `should_stop` and kills the ssh child) cannot run.

The join therefore lasts until the ssh session ends on its own: the remote
relay's idle watchdog (`BRIDGE_IDLE_TIMEOUT`, 60 s, but only if the server
stops sending), the server's own stall timeout once the socketpair buffer
fills, or OpenSSH keepalive on a dead link. Meanwhile the remote server still
has this connection as a live client: it counts against the connection limit,
and if it was the viewed connection the server still holds its surface
activation and geometry, which feed the PTY size rule
(`workspace_geometry_source`) and focus reporting. A reconnect therefore races
a ghost client of the same machine. At exit,
`release_ssh_resources_before_exit` waits its full grace for these stuck
owners.

Direction: the reader should only consult the bridge on a real peer EOF, or
should drop its stream (and its Arc) before joining. Cleaner still: the bridge
is a connection-scoped resource owned in one place, with an explicit `close()`
that sets `should_stop` and is called by whoever fails the connection, rather
than an Arc shared by reader and writer with teardown left to the last drop.

## RMT-004 - Link losses that OpenSSH logs below ERROR read as "needs attention", most visibly for the remote wait

Claim broken: AGENTS.md, Offline "(unreachable; the machine row's diagnostic
badge carries the reason, and it is retried with the reconnect backoff)";
`SshFailureClass::Link` doc ("The remote was never reached or the link
dropped: a retry can clear it").

Every ssh runs with `LogLevel=ERROR`. Several ways an established session ends
are logged by OpenSSH below that level; the clearest is the keepalive timeout,
`logit("Timeout, server %s not responding.")` followed by exit 255, which
`ServerAliveInterval`/`ServerAliveCountMax` from the managed config will
trigger on any dead link. The ssh then exits 255 with empty stderr. Both
`ssh_bridge_exit_error` and `command_failed` produce messages like "remote SSH
connection failed (exit status 255)" that match no signature in
`classify_ssh_diagnostic`, so they are `Unrecognized`: disposition `Repair`
(needs attention, `MachineState::Unavailable`, no action, 30 s attention
retry) and evidence `TargetUntrusted` (discovery progress thrown away).

For a connected machine the client heartbeat (15 s) usually fires before the
ssh keepalive (60 s) and masks this. The remote wait for a server
(`MachineSshConnector::wait_for_server`) has no heartbeat, so a NotRunning
machine whose link drops (suspend, network change) comes back as "Unavailable,
needs attention" instead of Offline. The same holds for a mux client whose
control master dies under it.

Direction: classify exit 255 with no recognised stderr on an established
session (bridge, wait) as `Link`, keeping `Unrecognized` for failures before
authentication; or raise the log level enough to see the link messages and add
their signatures. Worth verifying against the pinned OpenSSH which messages
each path prints under `LogLevel=ERROR`.

## RMT-005 - Authentication refusals outside a narrow signature set are retried automatically

Claim broken: AGENTS.md ("after startup a refusal is never retried by itself,
since repeated refused logins can get the client's address banned");
`record_failure` comment.

`classify_ssh_diagnostic` recognises a refusal only as "permission denied"
followed by `(publickey`, `(keyboard-interactive` or `(password`, or "too many
authentication failures", or an agent signing failure. sshd lists the methods
that can continue in its own order, so a host whose only methods are, for
example, `gssapi-with-mic` or `hostbased` prints
`Permission denied (gssapi-with-mic).` That is `Unrecognized` -> `Repair` ->
attention retry every `ATTENTION_RETRY_DELAY`, i.e. one refused login every
30 s, exactly what the no-retry rule exists to prevent. A bare
`permission denied (` from ssh's own exit 255 is already enough evidence of a
refusal. See also LIFE-002, where the preflight guidance claims the opposite
retry behaviour.

## RMT-006 - A remembered hint that fails verification for any non-stale reason is kept forever

Claim broken: `MachineProbe::resolve` comment ("Link, server and target-trust
failures leave it as an unverified hint; a later attempt checks it again") read
together with discovery, where the very same probe result rejects the
candidate and moves on.

`verify_remote_shepr` on a `Hint` that runs but fails (a non-zero exit from
`status client --json`, or output that does not parse) yields `RemoteFault` or
`InstallChanged`, neither of which `invalidates_executable`, so `resolve`
returns the error and never falls through to discovery. In
`DiscoveryProgress::run_remaining` the identical outcome `rejects_candidate()`
and the next candidate is tried. The one-install-per-host rule makes this rare,
but the two paths judge the same evidence differently; the hint path should
treat "the candidate ran and did not answer as this build" as rejection of the
hint, as discovery does.

## RMT-007 - `SSH_COMMAND_TIMEOUT` does not cover the commands it bounds

Claim broken: the const assertion comment in `shepr-remote/src/limits.rs` ("A
cold connection and the full remote status overview must complete before SSH's
command timeout can be mistaken for an authentication wait, with room to
spare").

`SSH_COMMAND_TIMEOUT` is `SSH_CONNECT_TIMEOUT + STATUS_OVERVIEW_TIMEOUT + 1 s`
= 15 s, with the overview budget being ping plus summary (4 s). But the
discovery probe is `status client --json`, which runs the sibling
`shepr-server --version` under `SIBLING_VERSION_TIMEOUT` (5 s), and the fleet
reads `status --json`, which does the sibling probe and the server overview
(9 s). A cold connect near its 10 s bound plus a slow sibling therefore hits
the command timeout; with the full budget available that is classified as
`authentication_wait_timeout`, so startup preflight offers a foreground login
to a machine whose real problem is its install, and `status --all` reports a
login need. The budget should be derived from the slowest remote command it
bounds. See also LIFE-012.

## RMT-008 - A reader blocked on the full event queue can expire a healthy endpoint

Claim broken: `limits.rs` `HEARTBEAT_TIMEOUT` doc and `insert_with_activity`
comment ("Readers timestamp complete frames before queueing them, so health
deadlines measure transport silence").

All readers share one bounded queue (`CLIENT_EVENT_QUEUE_CAPACITY`, 256) and
use `blocking_send`. When the loop stalls (a blocking host terminal write) and
a busy endpoint fills the queue, a quiet endpoint's reader blocks on its next
send and stops reading its socket, so its pong sits unread and unstamped. On
resume, `wait_for_next_event` is `biased` with the timer ahead of the queue, so
`tick_health` runs before the backlog drains and expires the quiet endpoint if
it had a ping outstanding when the stall began. Stamping happens before the
send, but reading does not happen while the send blocks, so the stamp measures
queue pressure, not transport silence. Per-endpoint queues, or draining the
event queue before the health tick, would keep the claim.

## RMT-009 - Launch-fatal SSH setup is detected after preflight has already acted

Claim broken: `Launched::prepare` comment ("A machine whose connector cannot be
built fails the launch here, before the Local handshake and before the terminal
is taken") and the principle that a config problem fails the launch with no
side effects.

`tui::launch` runs `preflight::run` first. A runtime path that can never hold
the SSH control socket (the "shorten XDG_RUNTIME_DIR" case) makes every check
`Failed`, prints notices, and then the local restart offer still runs and can
stop the local server with the operator's consent. Only afterwards does
`EndpointSupervisors::new` find `launch_fatal_setup_error` and fail the launch.
The operator restarted a server for a launch that was never going to happen.
The connector admission check belongs before preflight.

## RMT-010 - `spawn_due` consumes an operator request before it knows it can start it

`ReconnectState::due_operation` takes `requested` and only then does
`spawn_due` find `connector: None` and `continue`, dropping the request
silently. Today a connector is missing only after a panicked attempt, which
ends the client, so this is latent; but it is the kind of ordering that
`next_retry_deadline` explicitly guards against ("It skips the same states
`spawn_due` skips"), and the request should be taken only once the attempt is
certain to start.

## RMT-011 - Restart and the fleet stop disagree on what a stopping server needs

Raised as a smaller note.

`stop_server_of_another_build` (Restart) treats a `Stopping` server as no
server and goes straight to a starting bridge; `stop_plan` in `fleet.rs` stops
a `Stopping` server by its boot. Both are defensible, but one rule should own
it.

## RMT-012 - `EndpointHub::endpoint_lost` updates the shell for a stale failure

Raised as a smaller note.

`EndpointHub::endpoint_lost` ignores `record_failure`'s `None` (stale
generation) and still sets the machine state and diagnostic. The comment in
`reconcile` argues no stale failure can reach it, so this is harmless now, but
the result should gate the shell update as `attempt_failed` does.

## RMT-013 - Small inconsistencies in remote status and the server wait

Raised as smaller notes.

- `remote_server_status` (used by Restart) does not wrap its command in
  `candidate_command`, so a vanished executable there surfaces as exit 127
  `InstallStale` rather than the `CandidateMissing` 125 discovery uses; the
  evidence is still correct, just a second spelling of the same fact.
- `wait_for_server` keeps a dead inotify watch if the runtime directory is
  removed and recreated (it never re-creates `DirectoryWatch` once set), so the
  wait silently degrades to the 30 s recheck. Unlikely while an SSH session
  pins `XDG_RUNTIME_DIR`, but the comment "only covers an event the watch could
  miss" undersells it.
- `classify_handshake_error` only consults the bridge on `UnexpectedEof`. A
  bridge that fails after writing a partial preamble, or whose stdout carries
  junk, reports a generic preamble error rather than ssh's stderr. Moving
  bridge classification into the connector (see RMT-001) fixes this too.

## RMT-014 - The SSH bridge's lifetime and failure classification are split across modules

Raised as a structural note.

The SSH bridge's lifetime is spread over three owners (writer `_lifetime`,
reader `Arc`, `AcceptedEndpoint`) and its failure is read by two modules
outside `shepr-remote` (`handshake.rs`, `connection_io.rs`). RMT-001, RMT-002's
silent EOF and RMT-003 all come from that split. A single connection object in
`shepr-remote` that owns the stream, the bridge and the classification of its
end, handing the client a typed `EndpointFailure`, would remove all three.
