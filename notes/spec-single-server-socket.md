# Technical implementation spec: one server socket

Written against `reference/technical-implementation-spec.md` (the contract this
document must satisfy). Spawned from item 5 of `notes/work.md` ("Remote and
launch: merge the client socket into the API socket", formerly RLAUNCH-023).
Revised after two reviews (`notes/spec-single-server-socket-r1.md` and
`notes/spec-single-server-socket-r2.md`); section 8 records which findings were
folded in and which were rejected, and why.

The server listens on two sockets today: the JSON API (`shepr.sock`) and the
TUI's binary client socket (`shepr-client.sock`). This spec removes the second.
The surviving socket keeps the API socket's path, accepts both kinds of peer,
and tells them apart by their first byte.

Nothing in the originating item is deferred. The two "while there" items
(simplify `ServerLifetime::in_resource_order`, and stop exporting
`read_runtime_status_at`) are bricks below.

## 1. Contracts inventoried

`docs/` does not exist in this repository. `reference/` holds only
`technical-implementation-spec.md`, which governs this document's shape and
changes nothing here. `AGENTS.md` is the written contract that this work
changes. Every statement in it that this spec contradicts is listed here and
rewritten in the same landing (brick L2.14); none is left behind.

AGENTS.md statements changed:

1. Scope, JSON API bullet: "typed client-socket commands
   (`shepr_protocol::command::EndpointCommand`)". Becomes "typed commands on the
   TUI's connection to the server socket".
2. "Running a dev build": "Every pane exports `SHEPR_SOCKET_PATH` and
   `SHEPR_CLIENT_SOCKET_PATH` as its server resolved them". Becomes
   `SHEPR_SOCKET_PATH` alone, with `SHEPR_BUILD_PROFILE`.
3. "A process whose own profile differs from that marker ignores both socket
   variables" becomes "ignores the socket variable".
4. "`SHEPR_SOCKET_PATH` normally selects the API socket and derives the client
   socket. When both variables are set and the API path is exactly the profile's
   runtime `shepr.sock`, `SHEPR_CLIENT_SOCKET_PATH` selects the client socket.
   That is the pair a pane of a server started with only a client socket
   override exports, so `server stop` run in such a pane waits for that server's
   client socket to close." Deleted whole. Replaced by: "`SHEPR_SOCKET_PATH`
   selects the server socket."
5. The line "A non-runtime API path still takes precedence, so a user can set
   `SHEPR_SOCKET_PATH` inside a pane to select another server" becomes "A
   non-runtime path in `SHEPR_SOCKET_PATH` selects another server, so a user can
   set it inside a pane". The next sentence, "The API variable stays exported
   because every agent integration reports through it", stays, minus "API".
6. "Socket variables with no marker ... and ones with a matching marker still
   win over the runtime directory": stays, singular ("The socket variable with
   no marker ... still wins").
7. "The saved layout is not affected by the overrides, only the sockets are"
   becomes "The saved layout is not affected by the override, only the socket
   is."
8. The `server stop` bullet's "at any point while the stop waits for the
   sockets and the lease to go" becomes "while the stop waits for the socket
   and the lease to go".
9. Cross-build JSON control surface bullet: gains the `starting` flag. See 5.3.
10. The `ServerLifetime` bullet is rewritten. Exact new text is in brick L2.14.
11. Workspace layout, the `shepr-api` line ("JSON API schema, client and server
    transport") becomes "JSON API schema and client, and the server socket: its
    listener tells JSON requests from TUI connections and hands the latter to
    the server's client protocol."

Deliberately unchanged: "a dev build uses sibling `shepr-dev` directories, so
it has its own sockets". The runtime directory still holds more than one socket
per profile (the server socket and the per-machine SSH bridge sockets that
`machine_bridge_path` places there), so the plural stays true.

Other written contracts that change with the code (module docs, item docs,
comments and messages that name the client socket or the two sockets):

- `crates/shepr-client/src/lib.rs` module doc ("Connects to
  `shepr-client.sock`").
- `crates/shepr-client/src/shell/overlays/preferences.rs` ("One file per local
  client socket").
- `crates/shepr-protocol/src/command.rs` module doc.
- `crates/shepr-protocol/src/preamble.rs` module doc (who writes first).
- `crates/shepr-remote/src/remote/local_server.rs`: module doc, the `Probed`
  variant docs, the `acquire_launch_lock` doc ("socket overrides move only the
  sockets"), the `launch_with` doc ("polled past until its sockets go", "before
  its API bind", "released its sockets"), and the comment in `launch_with`
  ("before its API bind ... released sockets before its lease").
- `crates/shepr-remote/src/remote/host.rs` (item doc, error text, the
  `ensure_remote_server_running` doc, which brick L2.10 rewrites).
- `crates/shepr-remote/src/remote/bridge.rs`: the log message "remote bridge
  failed to prepare client socket" names the local SSH bridge stream, not the
  server's client socket; it becomes "remote bridge failed to prepare the
  accepted stream".
- `crates/shepr-remote/src/limits.rs` `REMOTE_STOP_SSH_TIMEOUT` doc ("close its
  sockets").
- `crates/shepr-server/src/server/headless.rs` module doc, the
  `HeadlessServer::new` doc, and the `Drop for HeadlessServer` comment (names
  `release_sockets_after_save` and "the sockets").
- `crates/shepr-server/src/server/client_transport.rs` module doc ("Blocking
  client socket transport").
- `crates/shepr-server/src/server/headless/bootstrap.rs`: `ServerReady` and
  `run_server` docs ("once both sockets are bound").
- `crates/shepr-core/src/env.rs` variable docs.
- `src/cli/spec.rs` (the hidden `client` subcommand's `about`).
- `src/limits.rs` `SERVER_READY_TIMEOUT` doc ("the fresh local server's client
  socket").
- `crates/shepr-api/src/schema/response.rs` (`stopping` doc).
- `crates/shepr-api/src/schema/tests.rs` (a comment about the client socket).
- `crates/shepr-api/src/server.rs`: the comment in `start_server` ("while the
  client socket stays up the server looks alive to autodetection").
- `crates/shepr-api/src/status.rs`: the `ServerPresence` doc and the
  `read_runtime_status_until` doc ("observe the endpoint lifetime").
- `crates/shepr-api/src/limits.rs`: `STOP_WAIT_TIMEOUT` ("for both sockets to
  disappear"), `STOP_LEASE_WAIT_TIMEOUT` ("its sockets are gone", "before
  removing its sockets") and `STOP_WAIT_POLL` ("its sockets").
- `crates/shepr-api/src/server_stop.rs`: the `stop_active_server` doc ("both
  server sockets"), the `stop_socket_with_timeout` doc, the
  `ServerStopError::LeaseHeld` doc and its display text ("its sockets
  disappeared"), the comment on the lease wait ("before its sockets bind"), the
  "Boot identity guards an occupant" comment, and the
  `wait_until_sockets_stopped_or_new_boot` doc.
- `crates/shepr-api/src/daemon_exit.rs` (`AlreadyRunning` doc: "already holds
  the sockets or the data directory").

Brick L2.15 sweeps for anything this list missed, with an exact command and a
defined scope.

## 2. The change in one paragraph

One path, `shepr.sock`, owned by `shepr_api::ServerHandle`. A listener thread
accepts, checks the peer, and classifies the connection by its first byte
without consuming it. A first byte of `S` (the preamble magic `SHEPRBID`) is the
TUI protocol, served by a handler the server installs once it has restored its
panes. Anything else is a JSON request line, served exactly as today. Each kind
has its own admission limit; a connection whose first byte has not arrived yet
holds a slot of a third, small classification limit, and saturating that limit
never refuses a peer whose own kind has room. Refusals are spoken in the kind's
language and name the limit that was actually reached. Before the handler is
installed, the socket answers `ping` with `starting: true`; this replaces the
old "API socket live, client socket absent" state, and `Releasing` disappears,
because there is no second socket to outlive the first.

## 3. Survey of the ground

### 3.1 Two sockets, where each lives today

API socket:

- Bound by `shepr_api::start_server` (`crates/shepr-api/src/server.rs`) through
  `shepr_platform::ipc::bind_private_socket`, before panes are restored, so
  agent hooks, `status` and `server stop` work during restore.
- A blocking accept thread. Each connection gets a thread after
  `ConnectionAdmission::try_acquire` (cap `MAX_ACTIVE_CONNECTIONS` = 64). Over
  the cap, `hand_off_busy_connection` queues the stream to a refuser thread
  (`BUSY_REFUSAL_QUEUE` = 16) that reads the request id within 500 ms and
  answers `EndpointBusy`; with the queue full it answers at once without an id.
  Accept failures of every kind back off (`AcceptBackoff`).
- `handle_connection` reads the request line under its own
  `INITIAL_REQUEST_TIMEOUT` deadline, taken when the thread starts. A line that
  is not UTF-8 is an `InvalidData` read error and closes the connection without
  an answer; a UTF-8 line that is not a request is answered with an
  `invalid_request` JSON line.
- `handle_request` answers `ping`, `server.stop` and `server.stop_if_boot` on
  the connection thread, and forwards every other method to the app loop.
- `ServerHandle::drop` wakes the listener with a connection, removes the socket
  file if still owned, joins the thread, then releases the startup lock.

Client socket:

- Reserved before restore (`reserve_client_socket_startup_lock` in
  `crates/shepr-server/src/server/headless/bootstrap.rs`: startup lock plus a
  liveness check, error `ClientSocketAlreadyLive`), bound after restore in
  `HeadlessServer::new` (`bind_private_socket_with_lock`), made nonblocking, and
  watched by the tokio loop through `AsyncFd<ListenerFd>`. The loop stops
  accepting once a stop is requested; connections left in the backlog see EOF
  when the listener closes.
- `accept_client_connection` in `crates/shepr-server/src/server/client_accept.rs`
  runs on the loop: accept, with `accept_failed_for_one_connection`
  (`ECONNABORTED`, `EPROTO`, `EPERM`, `EINTR`: retry at once) and
  `accept_resources_exhausted` (`EMFILE`, `ENFILE`, `ENOBUFS`, `ENOMEM`: pause)
  classifying failures; `peer_is_same_user`; its own `ConnectionAdmission`
  (`MAX_ACTIVE_CLIENT_CONNECTIONS` = 64); `ClientRegistry::allocate_client_id`
  (a plain `u64` counter inside the registry); then a `shepr-client-transport`
  thread running `handle_client_handshake`
  (`crates/shepr-server/src/server/client_transport.rs`). Over the cap, a
  process-wide `OnceLock` refuser (`busy_client_refuser`, queue 16, 250 ms hello
  bound) writes the preamble, reads the client's preamble and hello, and writes a
  `ConnectionLimit` refusal; any preamble error ends it without a refusal.
- Resource exhaustion on accept pauses the listener for
  `CLIENT_ACCEPT_RETRY_DELAY` through `client_accept_paused_until` in the loop's
  `select!`.
- `handle_client_handshake` writes the server preamble first, then reads the
  client's preamble and hello under `HANDSHAKE_TIMEOUT` (4 s). The client
  (`crates/shepr-client/src/handshake.rs`) already writes preamble and hello
  together before reading anything. A handshake that sends its welcome and then
  finds the server event channel closed sends the client a shutdown notice
  (`send_shutdown_to_unregistered_client`).

### 3.2 Everything that names or derives the client socket

- `shepr-config`: `ServerAddress` holds `api_socket`, `client_socket` and an
  `AddressSource` of `Runtime | ApiOverride | ClientOverride`;
  `resolve_paths` takes two overrides; `derive_client_socket_from_api_socket`
  appends `-client` to the stem; `override_variable`, `command` and
  `apply_to_child_command` handle both variables. `io.rs`
  `resolve_paths_from_env` reads both variables and drops both when
  `SHEPR_BUILD_PROFILE` names another profile.
- `shepr-core` `EnvVar::SheprClientSocketPath`, its rows in
  `every_variable_has_its_documented_name_and_kind`, and the selector list in
  `an_empty_selector_is_refused_not_unset`.
- `shepr-mux`: `PaneLaunchEnv` carries `client_socket_path`
  (`from_extra`, `from_extra_with_socket_paths`, the export in
  `pane/launch.rs`, and the `pane_env_policy` arm); `workspace.rs` spawn
  context and `persist/restore.rs` runtime context carry it into every pane
  launch.
- `shepr-server`: `app/ids.rs` and `app/mod.rs` compute it;
  `server/socket_paths.rs` exists only for it; `HeadlessServer` fields
  `client_listener`, `client_socket_path`, `client_socket_identity`,
  `active_client_connections`, `_client_socket_startup_lock`; `bootstrap.rs`
  `ServerSocket`, `ServerReady::client_socket`, `ClientSocketAlreadyLive`;
  `lifecycle.rs` `release_sockets_after_save` and `cleanup_sockets`;
  `headless/tests/mod.rs` `test_headless_server` (takes a client startup lock)
  and `headless/tests/already_running.rs`.
- `shepr-platform` `ipc.rs`: `ServerLifetime::{reserve, release,
  in_resource_order, endpoint_is_live, liveness, observe}`, `ReservedServer`,
  generic `ServerPresence<T>` with `Releasing`, `bind_private_socket_with_lock`
  and `acquire_socket_startup_lock` as public API for the handoff, and
  `SocketStartupLock::socket_path`, whose only caller is `HeadlessServer::new`.
- `shepr-api`: `server_stop.rs` waits on a socket list
  (`[api, client]`), `wait_until_sockets_stopped_or_new_boot`,
  `reachable_socket_paths`, `server_sockets_are_stopped`, the pass-through
  `server_socket_is_live` and `is_running_at`, `active_api_socket_path`, and
  the cfg(test) `running_from_liveness`; `lib.rs` `socket_path` (a wrapper over
  `active_api_socket_path`) used by `ApiClient::local`, `start_server`,
  `src/cli/target.rs`, `shepr-server` `app/ids.rs`, `app/mod.rs`,
  `bootstrap.rs`, `already_running.rs` and `shepr-remote` `local_server.rs`;
  `status.rs` `read_server_presence_at(client, api, ...)` and a `pub`
  `read_runtime_status_at` re-exported from `lib.rs` although only this crate
  uses it.
- `shepr-remote`: `local_server.rs` probes with both paths, has
  `Probed::Releasing` and a transition wait over both sockets; `host.rs`
  connects the SSH bridge to the client socket; `local_server_tests.rs` builds
  fake servers with two sockets (`runtime_sockets`).
- `shepr-client`: `lib.rs` connects to the client socket in two places
  (`run_launched_client`, and the supervisor's `add_local`);
  `crates/shepr-client/src/endpoint/supervisor.rs` stores the Local socket;
  `shell/overlays/preferences.rs` keys its per-socket file on the socket path
  (it hashes the path, so the preferences file for the Local endpoint is now
  named for the merged path; nothing on disk exists to migrate).
- Binary: `src/autodetect.rs` logs the client socket; `src/cli/spec.rs`
  describes the hidden `client` subcommand; `src/preflight.rs` builds a
  `RuntimeStatus` literal; `src/cli.rs` `server_not_running_error` re-implements
  the liveness-to-bool mapping inverted through `shepr_platform::ipc::probe`.
- Daemon: `crates/shepr-daemon/src/main.rs` prints `ServerSocket` in its
  already-running message.

### 3.3 What other work this touches

`notes/work.md` has four sibling items and no sibling specs exist, so there is
no sibling survey to reconcile against. Overlap by ground:

- Item 4 (server loop: per-client outbox) rewrites `ClientWriterQueue` in
  `client_transport.rs` and the `HeadlessServer` loop. This spec leaves the
  writer queue and render path alone. It changes `handle_client_handshake`'s
  opening and signature, removes `client_accept.rs`, and removes the listener
  arm and fields from the loop and the `HeadlessServer` literal in
  `test_headless_server`. Whichever lands second rebases a struct literal and
  the loop `select!`; no design depends on the other.
- Item 1 (client endpoints) touches `endpoint/supervisor.rs`. This spec
  changes only the accessor that supplies the Local socket path and one arm of
  `handshake_error`.

## 4. Obstacles, resolved

1. **Who is first on the wire.** The server writes its preamble before reading
   anything, which would corrupt a JSON peer on a merged socket. Resolution: the
   TUI client already speaks first (preamble and hello in one write, then it
   reads), so the server reads first. It still always answers a well-formed
   preamble with its own, matched or not, so a client of another build learns
   which build it reached. That holds on every path that reads a preamble: the
   handshake, and both refusals (limit and starting). On a `DifferentBuild`
   preamble the hello is not decoded (its layout is another build's): the
   server writes its own preamble and closes. Landing 1 makes this change on
   the existing client socket, which proves it before the merge.
2. **Classification needs a read, and reading needs a thread.** Spawning after
   admission is how the API listener bounds threads today, but a kind-specific
   admission cannot run before the kind is known. Resolution: the accept thread
   first looks for a first byte without waiting; a peer whose byte is already
   there is classified on the spot and goes straight to its kind's admission.
   Only a peer with nothing sent yet takes a slot of a third, small admission,
   `MAX_UNCLASSIFIED_CONNECTIONS`, which bounds threads that exist only to wait
   for a first byte. Over it, the connection goes to the refuser, which waits a
   short bound for the first byte, classifies, and then tries the kind's
   admission once: a kind with room is served, and only a full kind is refused,
   in its own language and naming its own limit. So silent peers that saturate
   classification delay other peers by at most the refuser's bound and never
   cause a refusal that names a limit that was not reached.
3. **The first byte must stay readable.** Classification uses `MSG_PEEK`, so the
   chosen service reads the connection from byte zero with no replay shim.
   `UnixStream::peek` is unstable, so the platform crate gets a small `recv`
   based primitive on its existing poll helpers (brick L2.1).
4. **No "starting" state without a second socket.** The old launcher learned
   "restore is still running" from "API live, client absent". Resolution: a
   `starting` flag in the `ping` answer, true until the server installs the
   client handler. It is derived from the handler slot, not stored twice
   (brick L2.3). A client connection before that point is refused with a new
   transient `HandshakeRefusal::ServerStarting`, never left waiting.
5. **Layering.** The TUI handshake and `ServerEvent` live in `shepr-server`,
   above `shepr-api`, which owns the listener. Resolution: `shepr-api` defines a
   `ClientProtocolHandler` trait and a `ClientGate` slot. `shepr-server`
   implements the trait with a struct that closes over its event channel and
   installs it. `shepr-api` already depends on `shepr-platform` and
   `shepr-protocol`, so the preamble, the `HandshakeRefusal` welcome, the
   framing and `write_client_stream` it needs to refuse a peer are available;
   no `brokkr.toml` dependency rule changes.
6. **Accept pressure.** The tokio `AsyncFd` arm and its pause timer exist because
   accept ran on the loop. On the listener thread the accept-failure
   classification moves with it: a failure that belongs to the one pending
   connection is retried at once, and every other failure, resource exhaustion
   included, goes through the existing `AcceptBackoff`. The pause machinery is
   deleted, not ported.
7. **Client ids from threads.** Ids were allocated by `&mut ClientRegistry` on the
   loop. Resolution: `ClientIdAllocator`, an `Arc<AtomicU64>` newtype owned by
   the handler (brick L2.7). The registry loses its counter.
8. **A server of another build at the same path.** A new build probing a
   running server whose layout predates this change reaches its JSON socket at
   the same path: `ping` returns that build's identity (no `starting`, read as
   false), so the existing build-mismatch guidance and restart offer work, and
   `server.stop_if_boot` stops it. That server's `shepr-client.sock` outlives
   its JSON socket briefly; its lease is retired first, so a successor is safe,
   and the stray file is removed by that server itself.

   A TUI preamble sent to such a socket is not reliably answered with anything
   the client can type. The JSON listener reads up to a newline for 5 s: hello
   bytes without a `0x0A` time out and close, bytes that are not UTF-8 fail the
   read and close, and only a UTF-8 line gets an `invalid_request` line that the
   client reads as `NotShepr`. The first two reach the client as
   `UnexpectedEof`, which `handshake_error` classifies as transient, so the
   mismatch would be retried forever and never reported. The local TUI never
   gets there (it checks the build before attaching, `BuildCheck::BeforeAttach`,
   and the restart offer precedes it). The SSH bridge does
   (`BuildCheck::AtClientHandshake` accepts a running server of another build
   and leaves the report to the handshake). Resolution: the bridge answers that
   case itself (brick L2.10). When its probe finds a running server whose
   `build_id` is not this build, it writes a preamble naming that build id to
   its stdout and exits without connecting, so the client reads a typed
   `DifferentBuild` whatever socket layout the occupant has. The bridge does
   this for every server of another build, not only for one with the old
   layout: the result is the same typed mismatch, and one rule is simpler than
   two.
9. **Remote hosts are unaffected otherwise.** A machine's `remote-client-bridge`
   runs the remote host's own installed `shepr`, which connects to its own
   server socket by its own build's rules. The local client only sees a byte
   stream carrying the preamble, so local and remote builds need not be merged
   in lockstep, and a mismatch is reported by the preamble as today.
10. **A client of an older build meeting a server of this build.** An old TUI
    whose runtime directory holds a new server probes `shepr-client.sock`,
    finds it absent while `shepr.sock` is live, reads that as its `Starting`
    transition, waits out its whole transition budget, and fails with "did not
    release its sockets" and no mismatch guidance. Accepted: the work item
    requires only that a new build recognise and stop an old server, and a
    client binary of the old build stops existing once this build is
    installed; an old dev build checked out later meets the same timeout and
    its operator restarts the server by hand (`server stop` from the old build
    still works: it pings `shepr.sock` and waits for both of its paths, the
    absent one counting as gone).

## 5. Target

### 5.1 `shepr-platform`

`crates/shepr-platform/src/ipc.rs`. Add:

```rust
pub enum FirstByte { Byte(u8), Closed }

/// Waits until `deadline` for the peer's first byte and returns it without
/// consuming it. `Closed` means the peer hung up before sending anything.
/// A deadline at or before now polls once without waiting. A passed deadline
/// with nothing to read is `io::ErrorKind::TimedOut`.
pub fn peek_first_byte(stream: &LocalStream, deadline: Instant) -> io::Result<FirstByte>
```

Implementation: loop on `child_io::poll_fd_readable(fd, poll_timeout_until(..))`,
then `libc::recv(fd, buf, 1, MSG_PEEK | MSG_DONTWAIT)`; `EINTR` retries, `EAGAIN`
re-polls until the deadline, a zero return is `Closed`. The `unsafe` block
carries a safety comment (one byte buffer on the stack, fd borrowed for the
call). Deadline arithmetic takes the clock the way
`LocalStreamDeadlineReader::new_with_clock` does so the unit tests do not sleep.

Remove: `ServerLifetime` whole (`reserve`, `release`, `in_resource_order`,
`endpoint_is_live`, `liveness`, `observe`), `ReservedServer`, and the generic
`ServerPresence<T>`. `ServerLifetime::liveness` loses its last caller when
`server_stop.rs`'s cfg(test) `running_from_liveness` goes (5.8). Add the free
function that replaces `endpoint_is_live`:

```rust
/// Whether a listener answers at `path`. `Absent | Stale` is `false`, `Live`
/// is `true`, and `Unreachable` is the error: an inaccessible path never
/// proves absence or permits a successor.
pub fn socket_is_live(path: &Path) -> io::Result<bool>
```

`Liveness`, `probe`, `SocketBusy`, `SocketFileIdentity`, `SocketStartupLock`,
`bind_private_socket` and `bind_single_use_private_socket` stay.
`acquire_socket_startup_lock` and `bind_private_socket_with_lock` become
private: after the change only `bind_private_socket` calls them
(`bind_single_use_private_socket` has its own `acquire_single_use_socket_lock`
and binds directly). Their in-module tests
(`held_socket_reservation_binds_without_releasing_its_lock`,
`reserved_socket_bind_refuses_a_listener_that_ignores_the_lock`,
`socket_binding_rejects_relative_paths`) still compile and stay.
`SocketStartupLock::socket_path` (the method) is deleted: its only caller,
`HeadlessServer::new`, loses that code. The `socket_path` field stays (its
`Drop` logs it and `bind_private_socket_with_lock` reads it).

`crates/shepr-platform/src/remote_bridge_io.rs`. Add:

```rust
/// Writes `bytes` to the bridge's stdout and flushes, for a bridge that answers
/// the client itself instead of relaying a server.
pub fn answer_remote_bridge(bytes: &[u8]) -> io::Result<()>
```

It takes fd 1 over as `forward_remote_bridge_stdio` does (a duplicated owned
descriptor, not `std::io::stdout()` text writing), with the
`stdout-handoff-ok` marker the `library-crates-do-not-write-stdout` textlint
requires.

### 5.2 `shepr-config` (`crates/shepr-config/src/address.rs`, `io.rs`)

```rust
pub struct ServerAddress { socket: PathBuf, overridden: bool }

impl ServerAddress {
    pub(crate) fn resolve_paths(runtime_dir: &Path, socket_override: Option<&Path>) -> Self;
    pub fn socket(&self) -> &Path;
    pub fn is_runtime_address(&self) -> bool;
    pub fn attach_command(&self, entrypoint: &str) -> String;
    pub fn stop_command(&self, entrypoint: &str) -> String;
    pub fn build_mismatch_guidance(&self, entrypoint: &str) -> String;
    pub fn apply_to_child_command(&self, command: &mut Command); // env_remove(SheprSocketPath)
}
```

`resolve_paths`: an override equal to `runtime_dir.join("shepr.sock")` is not an
override (a pane exports the runtime path, and a pane of the same profile must
still count as the runtime address). `command()` prefixes
`SHEPR_SOCKET_PATH=<quoted path>` when overridden. `api_socket()` is renamed
`socket()`; `client_socket()`, `override_variable`, `AddressSource` and
`derive_client_socket_from_api_socket` are deleted (and removed from the
`lib.rs` re-export). `resolve_paths_from_env` reads one variable and clears it
when `SHEPR_BUILD_PROFILE` names another profile, exactly as it does for the
API variable now.

`shepr_api::socket_path` and `shepr_api::server_stop::active_api_socket_path`
are deleted: both are wrappers around this accessor. Every caller uses
`paths.server_address().socket()` (with `.to_path_buf()` where it needs an
owned path): `ApiClient::local`, `start_server`, `src/cli/target.rs`,
`shepr-server` `app/ids.rs`, `app/mod.rs`, `bootstrap.rs`,
`already_running.rs`, and `shepr-remote` `local_server.rs` and its tests.

`shepr-core` `EnvVar::SheprClientSocketPath` is deleted with its rows in
`every_variable_has_its_documented_name_and_kind` and its entry in the selector
list of `an_empty_selector_is_refused_not_unset`, which then checks
`SheprSocketPath` alone. The doc of `SheprSocketPath` becomes "the socket of the
server to target, written into every pane as the socket of the server that owns
it".

### 5.3 `shepr-api` wire and status

`ResponseResult::Pong` gains `#[serde(default)] starting: bool`, documented as:
"The server has bound its socket but has not finished restoring panes, and
does not yet accept TUI connections. Absent in a pong from a build that
predates it." (The `no-predeployment-older-peer-compatibility` textlint rejects
"older build" in a code comment; this is the `stopping` doc's phrasing.) The
`stopping` doc drops "attaching to a client socket that no longer accepts" for
"attaching to a server that no longer accepts TUI connections".

`RuntimeStatus` gains `pub starting: bool`, chosen on purpose over classifying
the pong elsewhere: `stopping` already travels this way, `status_until` is the
one pong mapping shared by presence and stop, and a second type for two bools
would add a layer without removing one. `ApiClient`'s pong mapping
(`client.rs` `runtime_status`) copies it. `PONG_RESPONSE` in
`schema/tests.rs` becomes
`{"id":"cross-build:ping","result":{"type":"pong","version":"0.1.2","build_id":"0123456789abcdef","boot_id":"17-23","stopping":false,"starting":false}}`;
the existing "from before the stopping flag" fixture stays, and a new one pins
a pong that carries `stopping` but no `starting` deserializing with
`starting == false`. The `RuntimeStatus` literals in
`shepr-remote/src/remote/local_server_tests.rs` (`this_build`, `other_build`,
and the `serve_pong_once` JSON, which gains a `starting` parameter) and in
`src/preflight.rs` get the field.

`status.rs`:

```rust
pub enum ServerPresence {
    Gone,                  // no live listener at the socket
    Starting,              // live, `starting: true`
    Running(RuntimeStatus),
    Stopping,              // live, `stopping: true` (wins over `starting`)
    Unresponsive,          // live before and after a status request that got no answer
}

pub fn read_server_presence_at(socket: &Path, timeout: Duration) -> io::Result<ServerPresence>;
```

Order of tests:

1. `socket_is_live(socket)?` false: `Gone`.
2. Ask for status (`read_runtime_status_at`). An answer with `stopping`:
   `Stopping`; with `starting`: `Starting`; otherwise `Running`.
3. No answer: probe liveness again. Not live: `Gone` (the server finished
   shutting down between the probe and the request, which must read as a
   transition, never as a failure). Still live: `Unresponsive`.

A liveness error at either probe is the error, worded as today ("cannot tell
whether a shepr server listens at ..."). `read_runtime_status_at` becomes
`pub(crate)` and leaves the `lib.rs` re-export (the second "while there" item).

### 5.4 `shepr-api` listener (`crates/shepr-api/src/server.rs` plus two new modules)

New files:

- `crates/shepr-api/src/server/listener.rs`: the accept loop, `AcceptBackoff`
  and the accept-failure classification, classification, the three admissions
  and the refuser.
- `crates/shepr-api/src/server/client_protocol.rs`: the handler seam, the gate,
  `ConnectionSlot` and the client refusal writer.

`client_protocol.rs`:

```rust
pub trait ClientProtocolHandler: Send + Sync + 'static {
    /// One TUI connection, on its own thread, from byte zero. The slot is the
    /// connection's admission; hold it until the connection ends. `accepted` is
    /// when the listener accepted the stream: the handshake deadline counts
    /// from it, so time spent classifying is part of the handshake budget.
    fn serve(&self, stream: LocalStream, slot: ConnectionSlot, accepted: Instant);
}

#[derive(Clone, Default)]
pub struct ClientGate { handler: Arc<OnceLock<Arc<dyn ClientProtocolHandler>>> }
impl ClientGate {
    pub fn open(&self, handler: Arc<dyn ClientProtocolHandler>); // a second open logs an error and keeps the first
    pub fn is_open(&self) -> bool;
}

pub struct ConnectionSlot { active: Arc<AtomicUsize> } // released on drop
impl ConnectionSlot {
    pub(crate) fn try_acquire(active: &Arc<AtomicUsize>, cap: usize) -> Option<Self>;
}
```

`ConnectionSlot` is the existing `ConnectionAdmission` made `pub` and
parameterised by its cap. Both private copies (`server.rs` and
`client_accept.rs`) are deleted.

`ServerHandle` fields become `thread`, `path`, `identity`, `running`, `gate:
ClientGate` and `_startup_lock` (still last). It gains `pub fn client_gate(&self)
-> ClientGate`. `start_server` keeps its signature (`api_tx`, `server_stop`,
`paths`) and creates the gate, the three counters and the refuser; the counters
and the refuser's sender live in the accept closure, so nothing about them is
process-wide and only the accept closure holds the refuser's sender.

Listener flow (`listener.rs`). The accept thread does only bounded,
non-panicking work: no reads that wait, no `unwrap`, no slicing that can panic
(it is now the one listener for the API and the TUI, so a panic there takes
down both).

Accept failures: `ECONNABORTED`, `EPROTO`, `EPERM` and `EINTR` belong to the
one pending connection and are retried at once without a backoff sleep (the
classification and its test move from `client_accept.rs`); every other
failure, `EMFILE`, `ENFILE`, `ENOBUFS` and `ENOMEM` included, goes through
`AcceptBackoff` as today.

Per accepted stream, on the accept thread, with `accepted = Instant::now()`
(marked `clock-io-ok`: it starts the deadlines of real socket reads):

1. `peer_is_same_user`: refuse by dropping, as today.
2. `peek_first_byte(&stream, accepted)` (polls once, no wait):
   - `Closed`: drop.
   - `Byte(b)`: the kind is known (`b == PREAMBLE_MAGIC[0]` is the client
     protocol, anything else the API); go to step 4.
   - `TimedOut`: step 3. Any other error: drop, logged at debug.
3. Acquire an unclassified slot (`MAX_UNCLASSIFIED_CONNECTIONS`). None: hand to
   the refuser (step 6) unclassified. Some: spawn a `shepr-conn` thread (a spawn
   failure drops the stream and feeds the accept backoff, as today) that calls
   `peek_first_byte(&stream, accepted + INITIAL_REQUEST_TIMEOUT)`, releases the
   unclassified slot, and then:
   - `Closed`, `TimedOut` or another error: close. This is also what a liveness
     probe (`probe` connects and drops) looks like, and costs a short-lived
     thread.
   - `Byte(b)`: dispatch as step 4 does, on this thread, refusing inline (step
     5) rather than through the refuser.
4. Dispatch by kind:
   - API: acquire an API slot (`MAX_ACTIVE_CONNECTIONS`), then run
     `handle_connection(stream, deadline = accepted + INITIAL_REQUEST_TIMEOUT,
     ..)` on a `shepr-conn` thread (on the accept thread, spawn one; on a
     classification thread, run in place).
   - Client protocol: if the gate is closed, refuse with `ServerStarting`.
     Else acquire a client slot (`MAX_ACTIVE_CLIENT_CONNECTIONS`) and call
     `handler.serve(stream, slot, accepted)` on a `shepr-conn` thread (spawned,
     or in place as above).
   - No slot, or the gate closed: on the accept thread, hand to the refuser
     (step 6) with the kind known; on a classification thread, refuse inline
     (step 5).
5. Refusals, as functions both callers share:
   - API (`refuse_api`): read the request line for its id within
     `BUSY_REQUEST_ID_TIMEOUT`, answer `EndpointBusy` naming
     `MAX_ACTIVE_CONNECTIONS` (the existing `reject_busy_connection`).
   - Client (`refuse_client(stream, reason)`): read the client's preamble within
     `BUSY_CLIENT_HANDSHAKE_TIMEOUT`. On `Ok`, read the hello under the same
     deadline, then write the server preamble and
     `EndpointWelcome::refused(reason)` in one write bounded by
     `STREAM_WRITE_TIMEOUT` (through `shepr_platform::write_client_stream`).
     On `DifferentBuild`, write the server preamble alone (same bound) and close
     without reading the hello. On `NotShepr`, `UnexpectedEof` or `Io`, close
     without writing. The reason is `ServerStarting` when the gate is closed,
     else `ConnectionLimit(MAX_ACTIVE_CLIENT_CONNECTIONS)`. Reading the hello
     before writing is what lets the refusal reach a client that sends both in
     one write.
6. Refuser: one thread per handle, queue `BUSY_REFUSAL_QUEUE` of `(stream,
   accepted, Option<kind>)`. It peeks the first byte within
   `BUSY_REQUEST_ID_TIMEOUT` when the kind is not known yet (a peer that sends
   nothing in that bound is closed), then tries the kind's admission once
   exactly as step 4 does: a free slot (and, for a client, an open gate) is
   served on a spawned `shepr-conn` thread; otherwise it refuses as in step 5.
   A full queue or a dead refuser: an API connection whose kind is known gets
   `EndpointBusy` without an id at once, as today; anything else is closed,
   because an unclassified peer may be a TUI that would misread a JSON line as
   a foreign preamble. The refuser is sequential, so silent peers queued ahead
   delay later ones by at most `BUSY_REQUEST_ID_TIMEOUT` each; that bounded
   delay under deliberate saturation is accepted.

`handle_connection` takes its request-line deadline as a parameter instead of
starting `INITIAL_REQUEST_TIMEOUT` itself, so a JSON peer gets 5 s from accept
in all, as today, and not classification time plus 5 s.

`handle_request` for `ping` fills `starting: !gate.is_open()`, so it takes the
gate alongside `server_stop`. Drop order, wake-up and join in
`ServerHandle::drop` are unchanged. The refuser ends when the accept closure,
the only holder of its sender, is dropped with the listener thread.

New constants, all in `crates/shepr-api/src/limits.rs` with doc comments (the
`numeric-consts-live-in-limits` textlint applies):

```rust
pub(crate) const MAX_ACTIVE_CLIENT_CONNECTIONS: usize = 64;
pub(crate) const MAX_UNCLASSIFIED_CONNECTIONS: usize = 64;
pub(crate) const BUSY_CLIENT_HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(250);
```

There is no separate classification timeout: a classification thread waits for
the first byte until `accepted + INITIAL_REQUEST_TIMEOUT`, the longest
first-byte budget of either kind (the TUI's `HANDSHAKE_TIMEOUT`, 4 s, counts
from the same `accepted` inside `serve`). `BUSY_REFUSAL_QUEUE` (16) now serves
both kinds. The refusal writes reuse `STREAM_WRITE_TIMEOUT`. Deadlines that
bound real socket reads carry `clock-io-ok:` as in the surrounding code (the
`api-clock-is-injected` textlint). `shepr-server/src/limits.rs` loses
`CLIENT_LIMIT_HANDSHAKE_TIMEOUT`, `CLIENT_ACCEPT_RETRY_DELAY`,
`MAX_ACTIVE_CLIENT_CONNECTIONS` and `CLIENT_HANDSHAKE_REFUSAL_QUEUE_CAPACITY`; it
keeps `HANDSHAKE_TIMEOUT` and `CLIENT_WRITE_STALL_TIMEOUT`.

The comment in `start_server` about the listener thread outliving any failure
is reworded: a dead listener now leaves a server that answers neither the CLI,
agent hooks nor TUI attaches, so it must outlive every accept and spawn
failure.

### 5.5 `shepr-protocol`

- `HandshakeRefusal::ServerStarting` with display text "the server is still
  starting; it accepts clients once its panes are restored". No other wire type
  changes; the codec rules (no `skip_serializing_if`, `flatten`, tagged enums)
  hold and there are no frozen fixtures to update beyond new variants in the
  existing wire tests.
- `pub fn preamble_for(build_id: &str) -> [u8; PREAMBLE_LEN]`: the existing
  private `encode`, made public under this name, for the SSH bridge's own
  answer (5.9). `local_preamble` calls it.
- `preamble.rs` module doc: the client writes its preamble and hello; the
  server reads the client's preamble and then writes its own, answering a
  recognisable preamble of any build, including on a refusal, so each side
  learns the other's identity; on another build's preamble the server writes
  its own and closes without decoding the hello.

### 5.6 `shepr-server`

Delete `server/client_accept.rs`, `server/socket_paths.rs` and the module
declarations for both.

`client_transport.rs`:

```rust
pub(crate) struct ClientTransportHandler {
    server_event_tx: mpsc::Sender<ServerEvent>,
    should_quit: Arc<shepr_api::ServerStopSignal>,
    ids: ClientIdAllocator,
}
impl shepr_api::ClientProtocolHandler for ClientTransportHandler {
    fn serve(&self, stream: LocalStream, slot: ConnectionSlot, accepted: Instant) {
        let _slot = slot;
        let client_id = self.ids.allocate();
        if let Err(err) = handle_client_handshake(
            stream,
            client_id,
            accepted + HANDSHAKE_TIMEOUT,
            &self.server_event_tx,
            &self.should_quit,
        ) {
            debug!(?client_id, error = %err, "client transport failed");
        }
    }
}
```

`handle_client_handshake` takes the handshake deadline as a parameter (it no
longer reads the clock for it) and changes its opening only: read the client
preamble under that deadline; on `Ok` or `DifferentBuild` write this build's
preamble (a failed write is the old "client left" debug and return); on
`DifferentBuild` also log the existing rejection warning and return without
reading the hello; on `NotShepr`, `UnexpectedEof` and `Io` close without
writing, as the matching arms do today. Then the hello (same deadline) and
everything after it are unchanged. Its doc comment states the new order.

During shutdown the listener keeps serving until the socket is removed, where
the loop used to stop accepting at the stop request. A TUI that connects after
the stop meets the existing `should_quit` check at the top of
`handle_client_handshake` and is closed without an answer, which its client
classifies as transient, the same class as the backlog EOF it saw before. A
handshake that passed that check and then finds the event channel closed is
already sent a shutdown notice (`send_shutdown_to_unregistered_client`). No new
refusal is added for this window (section 8 says why).

`ClientIdAllocator` is an `Arc<AtomicU64>` newtype starting at 1 with
`allocate(&self) -> ClientId` using `fetch_add(1, Ordering::Relaxed)`; no
saturation (2^64 connections cannot happen). It lives beside `ClientRegistry`
in `clients.rs`, and `ClientRegistry::{next_client_id, allocate_client_id}` are
removed. The tests in `clients.rs` and `headless/client_views.rs` that call
`allocate_client_id` use `ClientId::test_new` or a local allocator.

`HeadlessServer` loses `client_listener`, `client_socket_path`,
`client_socket_identity`, `active_client_connections` and
`_client_socket_startup_lock`. `HeadlessServer::new(app, api_request_rx,
api_server, stop_requested)` becomes infallible and returns `Self`: it builds
the event channel, constructs the handler, and calls
`api_server.client_gate().open(handler)` when it holds a handle (tests pass
`None`). Its doc comment says so (the old numbered list about preparing and
binding the client socket goes). In the loop, delete
`LoopEvent::ClientListenerReady`, `LoopEvent::ClientListenerError`,
`ListenerFd`, the `AsyncFd` setup, the `client_accept_paused_until` state and
its deadline mixing, the listener `select!` arm, both handlers for those events
(including the arm in the stop branch), and the `HeadlessServer::
accept_client_connection` method. The module doc line "Creates and listens on
the API and client sockets" becomes "Serves one socket for the JSON API and
TUI clients". `shutdown_unregistered_clients` and
`reject_late_client_connections` stay: a connection that completed its
handshake before the stop is still settled there.

`bootstrap.rs`:

- `ServerSocket` is deleted. `RunServerError::AlreadyRunning { path }` names the
  one socket ("another server listens on the socket (path)"). `ServerReady` has
  `socket` and drops `api_socket` and `client_socket`; its display prints
  `socket: <path>`. `ClientSocketAlreadyLive`,
  `reserve_client_socket_startup_lock` and the client branch of `startup_error`
  are deleted.
- `run_server` no longer uses `ServerLifetime`. Its order, in code and in a
  comment on the function, is: take the data-directory lease; start file
  logging, manifests and the integration installer; bind the socket
  (`shepr_api::start_server`, answering `ping` as `starting` from now on); build
  the runtime; restore panes (`App::with_paths` takes the lease); build
  `HeadlessServer` with the API handle (which opens the gate); report ready;
  run. From taking the lease until those hand-offs, the resources live in a
  private struct

  ```rust
  struct Reserved {
      lease: DataDirLease,
      api: shepr_api::ServerHandle,
      file_logging: shepr_platform::logging::FileLoggingOutcome,
  }
  ```

  whose field order is
  the release order: the lease first, so a startup that dies before the
  hand-off retires the lease before the socket disappears. The hand-off
  destructures it: `lease` moves into `App::with_paths`, `api` into
  `HeadlessServer::new`, and `file_logging.unavailable` into `ServerReady`, as
  today. Between `App::with_paths` and `HeadlessServer::new` the app is a local
  declared after the API handle, so an unwind drops the app (and its lease)
  first. `on_ready` runs after the gate opens, which is the moment a client
  spawned by the launcher can attach.

`lifecycle.rs`: `release_sockets_after_save` becomes `release_socket_after_save`
and `release_sockets_after_save_observed` becomes
`release_socket_after_save_observed`, whose hook is `before_socket_removal`.
Body: `app.retire_session_writer()`, then run the hook, then
`drop(self._api_server.take())` (`ServerHandle::drop` removes the file). The
old `cleanup_sockets` is deleted. Doc comment states the rule: the final save
is on disk, then the lease is retired, then the socket goes, so a launcher that
sees the socket vanish and starts a daemon meets a free lease. Every exit still
runs it, including `Drop`; each step is idempotent. The `Drop for
HeadlessServer` comment names the new function.

`app/ids.rs`, `app/mod.rs`: pane launch env takes the one socket path.

### 5.7 `shepr-mux`

`PaneLaunchEnv` keeps `api_socket_path`, renamed `socket_path`;
`from_extra_with_socket_paths` is deleted and `from_extra(extra, socket_path)` is
the one constructor; the export of `SHEPR_CLIENT_SOCKET_PATH` and its
`pane_env_policy` arm go from `pane/launch.rs`, and the comment above the export
becomes "The socket is exported as the server resolved it, replacing any
inherited value. Every agent integration reports through it, so it is always
set." The spawn context in `workspace.rs` and the runtime context in
`persist/restore.rs` drop their `client_socket_path` field and their
pass-through.

The two `launch.rs` tests that pin the pair
(`pane_launch_exports_a_resolved_socket_pair`,
`inherited_socket_variables_give_way_to_the_resolved_pair`) become one,
`an_inherited_socket_variable_gives_way_to_the_resolved_socket`, which sets an
inherited `SHEPR_SOCKET_PATH` and pins the resolved value. It does not assert
anything about `SHEPR_CLIENT_SOCKET_PATH`: once the variable is deleted nothing
reads it, and nothing in the tree names it any more, so a value inherited from
a server environment of another build passes through to panes unread. Keeping
a literal name only to scrub it would be code for a variable that does not
exist.

### 5.8 `shepr-api` stop (`server_stop.rs`)

One socket instead of a list.

- `stop_active_server_with_timeout` passes `address.socket()` once.
- `stop_socket_with_timeout(socket_path, lease, timeout, label, expected_boot_id)`
  drops the `stopped_socket_paths` parameter.
- `server_sockets_are_stopped(&[P])` becomes `server_socket_is_stopped(path)`,
  `wait_until_stopped_until` takes one path, and `reachable_socket_paths`
  becomes a single reachable check.
- `ServerStopError::TimedOut { reachable: Vec<PathBuf> }` becomes
  `TimedOut { socket: PathBuf, .. }`; its display reads "{label} did not stop
  within {ms}ms; the socket at {path} is still reachable".
- `ServerStopError::LeaseHeld`'s doc and display say "its socket disappeared".
- `wait_until_sockets_stopped_or_new_boot` becomes
  `wait_until_socket_stopped_or_new_boot` and keeps its job (a successor can
  bind the path while the lease and file waits run, and must be reported as
  `OccupantChanged`), now over one path. Its doc comment drops the sentence
  about the API endpoint going before the client endpoint.
- The "Boot identity guards an occupant, not an endpoint lifetime" comment is
  reworded: the boot probe says which process answers, the socket check says
  whether anything listens, and both must hold for a stop to be complete.
- `server_socket_is_live` and `is_running_at` (pass-throughs) are deleted;
  their callers, and `status.rs`, call `shepr_platform::ipc::socket_is_live`.
  The cfg(test) helper `running_from_liveness` goes. `active_api_socket_path`
  goes (5.2).
- The `stop_active_server` and `stop_socket_with_timeout` docs say "the
  socket" where they say "both server sockets" and "its sockets".

Because the socket is removed last, after the lease is retired, a server that
has stopped answering has already freed the lease; the lease wait remains as the
guard against a successor or a stuck retiring writer.

`src/cli.rs` `server_not_running_error` becomes
`shepr_platform::ipc::socket_is_live(path).map(|live| !live)` with the error
converted as today, leaving one liveness mapping in the tree.

### 5.9 `shepr-remote`

`local_server.rs`:

- `probe_server(paths)` calls
  `shepr_api::read_server_presence_at(paths.server_address().socket(), STATUS_REQUEST_TIMEOUT)`;
  `probe_server_at` takes one path.
- `Probed` drops `Releasing`; `Starting` is documented as "the socket answers
  that the server is still restoring". `Starting` and `Stopping` remain as
  transitions to wait out. The launch-loop and `running_server_status` arms
  lose the `Releasing` cases.
- `wait_for_server_sockets_to_settle_until` becomes
  `wait_for_server_socket_to_settle_until` and `server_sockets_are_stopped`
  becomes `server_socket_is_stopped`, both over one socket.
  `server_transition_timeout`'s message says "did not release its socket".
- `ensure_running` returns `io::Result<RuntimeStatus>`: the status of the
  server it accepted (the probed one, or the one `launch_daemon` verified).
  `accept_running` passes the status through. Callers that only need success
  ignore it.
- `launch_with` is unchanged in logic: it accepts only `Probed::Running` with
  this build's identity, so a daemon whose socket answers `starting` is polled
  until the restore finishes and the gate opens. Its doc and comment say
  "before its socket bind" and "released its socket". `SERVER_READY_TIMEOUT`
  already covers restore, because the client socket used to appear only after
  restore.
- The module doc's step 1 is rewritten for one socket and the presence states
  `Gone`, `Starting`, `Running`, `Stopping`, `Unresponsive`.
- `require_own_runtime_address` names `SHEPR_SOCKET_PATH` directly.
- The `acquire_launch_lock` doc says "socket overrides move only the socket".

`host.rs`:

```rust
pub fn run_remote_client_bridge(paths) -> io::Result<RemoteBridgeOutcome> {
    let status = ensure_remote_server_running(paths)?;
    if !shepr_protocol::is_this_build(&status.build_id) {
        shepr_platform::answer_remote_bridge(&shepr_protocol::preamble::preamble_for(&status.build_id))?;
        return Ok(RemoteBridgeOutcome::Closed);
    }
    // connect_trusted_local_stream(address.socket()), then relay as today
}
```

The bridge answers a running server of another build itself and never connects
to it: the client reads that build's preamble and reports a typed
`DifferentBuild` with guidance, whatever listener layout the occupant has
(obstacle 8). The doc on `ensure_remote_server_running` is rewritten to say
this, and the connect error text says "server socket".

`bridge.rs`: the log message rename listed in section 1.

`local_server_tests.rs`: `runtime_sockets` becomes `runtime_socket`; fake
servers bind one socket and answer `ping` JSON including `starting`
(`serve_pong_once` gains the parameter).

- Deleted (their states cannot exist):
  `a_client_socket_left_after_api_release_is_still_shutting_down`,
  `launch_waits_for_the_client_socket_after_api_release`.
- `an_api_socket_live_before_the_client_socket_is_a_startup_transition`
  becomes `a_socket_answering_starting_is_a_startup_transition`, with a fake
  that answers `starting: true` (a bare bind is `Unresponsive` now).
- `a_listener_without_a_status_answer_is_unresponsive` takes one path.
- `repeated_socket_transitions_share_one_wait_deadline`: both transitions are
  a fake that answers every ping with `starting: true` until dropped (a bare
  listener would read as `Unresponsive`, on which the wait returns at once,
  and the test would stop testing the shared deadline).
- `the_bridge_leaves_a_running_mismatch_to_the_handshake` becomes
  `ensure_running_hands_back_a_running_mismatch_for_the_bridge`: with a fake of
  another build, `ensure_running(.., AtClientHandshake)` returns that status
  and launches nothing.
- New `a_vanished_server_reads_as_gone_not_unresponsive` (5.3 step 3): a fake
  that accepts the status request and closes its listener and removes its
  socket before answering probes as `Probed::NoServer`.

### 5.10 `shepr-client`, binary, daemon

- `shepr-client/src/lib.rs`: both `client_socket()` uses become `socket()`; the
  module doc names "the server socket".
- `crates/shepr-client/src/endpoint/supervisor.rs`: the field doc and the
  `add_local` call use `socket()`.
- `handshake_error` in `supervisor.rs` maps `HandshakeRejected {
  ServerStarting }` to `ConnectionAborted`, the same transient class as
  `ConnectionLimit`, so Local retries instead of parking an incompatibility.
- `src/autodetect.rs`, `src/cli/spec.rs`, `src/preflight.rs`,
  `src/cli/target.rs`, `src/limits.rs`: accessor rename, `about` text
  ("Connect to a running server"), the `starting: false` field, and the
  `SERVER_READY_TIMEOUT` doc ("the fresh local server's readiness").
- `crates/shepr-daemon/src/main.rs`: `AlreadyRunning { path }` prints
  `socket: <path>`.

## 6. Landings

Two landings. Each is one coherent change that is kept or reverted on its gate
results, and `brokkr check` is green at both boundaries. No smaller cut exists
for the second: with two disagreeing sockets mid-change, any partial state is
the defect this work removes.

### Landing 1: the server reads first

A behavior-preserving change on the existing client socket that makes the merge
possible and proves the new opening against the real client.

- L1.1 `handle_client_handshake` (`client_transport.rs`) reads the client
  preamble under `HANDSHAKE_TIMEOUT`, then writes the server preamble if the
  client's was recognisable (`Ok` or `DifferentBuild`), then continues on `Ok`
  and returns on `DifferentBuild` without reading the hello. Matching arms for
  the other failures as in 5.6.
- L1.2 `reject_busy_client` (`client_accept.rs`) reads the client preamble
  first. On `Ok` it reads the hello and then writes the server preamble and the
  refusal in one bounded write; on `DifferentBuild` it writes the server
  preamble alone and returns; on any other preamble error it returns without
  writing.
- L1.3 `preamble.rs` module doc updated for the new order; `local_preamble` and
  `read_preamble` are untouched.

Tests (named; `brokkr check` runs them):

- `foreign_build_preamble_gets_the_server_identity_and_no_session`
  (`client_transport.rs`): unchanged. It and the helper `open_as_client`
  already write the client preamble and hello before reading, so they prove the
  new order without a rewrite.
- `limit_refusal_waits_for_hello_after_publishing_its_preamble` becomes
  `limit_refusal_reads_the_hello_before_it_answers` (`client_accept.rs`): the
  client sends preamble and hello, then reads preamble and refusal.
- `a_foreign_client_over_the_limit_still_learns_the_server_build`
  (`client_accept.rs`): a client writes another build's preamble (and a hello
  the refuser must not decode), then reads a preamble naming this build, then
  EOF.
- `silent_excess_client_cannot_hold_the_refuser` (same file): its body changes,
  because the server no longer writes first: the silent client reads nothing,
  and the test asserts the refuser returns within its bound and the client then
  sees EOF.
- every other existing handshake test in `client_transport.rs` and the
  client's `handshake.rs` tests pass unchanged.

Gate: `brokkr check`.

By hand (nothing in the test suite attaches the real TUI to the real server),
with the dev build only; nothing here installs or runs a release build:

```
brokkr run --debug shepr-server -- --version
brokkr run --debug -- status server
brokkr run --debug --
```

The first builds the sibling server. The second must print `status: not
running` and exit 0. The third must start the dev server, attach, and show the
Local endpoint connected. From a pane of that dev server (a dev CLI in a dev
pane is allowed; only the TUI is refused there), `brokkr run --debug -- server stop`
must end the TUI and leave the dev runtime directory
(`$XDG_RUNTIME_DIR/shepr-dev`) free of `shepr.sock` and `shepr-client.sock`.

Keep/revert: any failure to attach, or any test that cannot be rewritten for the
new order without weakening it, reverts the landing.

### Landing 2: one socket

Bricks, in the order the code must be edited so the tree compiles at the end;
none is a separate landing.

- L2.1 `peek_first_byte`, `FirstByte`, `socket_is_live` and
  `answer_remote_bridge` in `shepr-platform`, with the platform removals that
  the later bricks free (5.1).
- L2.2 `HandshakeRefusal::ServerStarting`, `preamble_for` (5.5) and the
  `ServerStarting` mapping in `handshake_error` (5.10).
- L2.3 `Pong.starting`, `RuntimeStatus.starting`, the new `ServerPresence` and
  `read_server_presence_at` with the re-probe, `read_runtime_status_at` made
  `pub(crate)` (5.3).
- L2.4 Listener, gate, handler trait, slots, refuser, refusal functions, the
  accept-failure classification and limits in `shepr-api`; `start_server`
  creates the gate; `ping` fills `starting`; `handle_connection` takes its
  deadline (5.4).
- L2.5 Single-socket stop in `server_stop.rs`, and `server_not_running_error`
  in `src/cli.rs` (5.8).
- L2.6 `ServerAddress`, the deleted `socket_path` wrappers and their callers,
  `EnvVar` and `resolve_paths_from_env` (5.2).
- L2.7 `shepr-mux` launch env and contexts (5.7); `ClientIdAllocator` and
  `ClientTransportHandler`, and `handle_client_handshake` taking its deadline
  (5.6).
- L2.8 `HeadlessServer` without the listener, infallible `new`, loop arm and
  state removal; delete `client_accept.rs` and `socket_paths.rs`; move the limit
  constants (5.6).
- L2.9 `bootstrap.rs`, `lifecycle.rs`, daemon `main.rs` (5.6, 5.10).
- L2.10 `shepr-remote` launcher (`ensure_running` returning the status), the
  bridge's own answer to a server of another build, the bridge log rename, and
  the test servers (5.9).
- L2.11 `shepr-client`, binary and preflight edits (5.10).
- L2.12 Tests (below).
- L2.13 The module docs, item docs, comments and messages listed at the end of
  section 1, rewritten for one socket.
- L2.14 AGENTS.md. The `ServerLifetime` bullet becomes:

  > Startup and shutdown follow one order, written in `run_server` and in
  > `HeadlessServer::release_socket_after_save`. Startup takes the data-directory
  > lease, binds the server socket, restores panes, then opens the client
  > protocol. The socket is live and answers `ping` from the moment it is bound,
  > with `starting: true` until the client protocol opens; a TUI connection
  > before that is refused as transient. Shutdown keeps the socket through the
  > final save, retires the lease, then removes the socket. A socket that is
  > absent or stale reads as gone, a live one answers by `ping` as starting,
  > running (the default) or stopping, and a live one that does not answer reads
  > as unresponsive unless it has gone by the time the answer is missed, which
  > reads as gone. A launcher waits through starting, treats a server answering
  > `stopping` as no server (it no longer accepts clients), and starts a
  > successor once the socket is gone. Socket absence only permits a launch
  > attempt: the lease decides which contender owns the data directory, even
  > before the socket exists.

  The cross-build bullet reads "`ping` response identity (`version`, `build_id`,
  `boot_id`), its `stopping` and `starting` flags (each read as false when an
  older build omits it)". The other AGENTS.md edits are items 1 to 9 and 11 in
  section 1.
- L2.15 Sweep, over production code, tests, binding documentation and the
  lint configuration (not `notes/`, whose documents describe the removed
  design on purpose). Run from the repository root:

  ```
  grep -rn -i -e "client socket" -e "client_socket" -e "ClientSocket" -e "CLIENT_SOCKET" -e "-client.sock" -e "both sockets" -e "two sockets" -e "its sockets" -e "release_sockets" src crates AGENTS.md reference brokkr.toml
  ```

  It must print nothing. ("client endpoint" is not in the pattern: it also
  matches "per-client endpoint reply queues" in `headless.rs`, which is about
  endpoint requests, not sockets. Its socket-related uses, in `server_stop.rs`
  and `local_server.rs`, are listed in section 1 and rewritten by L2.13.)
  Stray rustdoc links to deleted items are fixed.

Tests added or changed (names are the proof they exist):

`shepr-platform`:
- `peek_first_byte_returns_the_byte_without_consuming_it`
- `peek_first_byte_reports_a_peer_that_closed_silently`
- `peek_first_byte_times_out_on_a_silent_peer`
- `peek_first_byte_with_a_passed_deadline_polls_once`
- `socket_is_live_maps_absent_and_stale_to_false_and_unreachable_to_an_error`
  (replaces `server_stop.rs`'s `socket_liveness_maps_absent_and_stale_to_stopped`
  and `server_socket_presence_uses_the_shared_liveness_rule`)
- `bind_private_socket_is_owner_only_from_the_moment_it_is_reachable` (moved
  from `shepr-server`'s `client_socket_is_owner_only_from_the_moment_it_is_reachable`,
  which only exercises `bind_private_socket`)
- deleted with `ServerLifetime`:
  `server_lifetime_observes_all_endpoint_transitions_and_stopping_identity`,
  `server_lifetime_orders_reservation_restore_bind_and_release`,
  `failed_client_reservation_retires_lease_before_api`

`shepr-protocol`:
- `preamble_for_names_the_given_build` (and the existing preamble tests
  unchanged)
- `ServerStarting` added to the existing refusal round-trip wire test

`shepr-api`:
- `a_preamble_connection_reaches_the_client_handler_and_a_json_one_the_api`
  (a recording handler; both on one socket)
- `a_connection_that_sends_nothing_releases_its_classification_slot`
- `client_and_api_connections_are_admitted_separately` (saturating one kind's
  cap leaves the other kind served)
- `classification_saturation_still_serves_a_peer_whose_kind_has_room` (fill
  the unclassified cap with silent peers; a `ping` and a TUI hello are still
  served through the refuser)
- `an_excess_api_connection_is_refused_with_its_request_id` (moved from
  `a_full_connection_limit_sends_endpoint_busy`)
- `a_full_refusal_queue_refuses_at_once_without_reading` and
  `the_busy_refuser_thread_echoes_the_request_id`: kept, for API connections
  whose kind is known
- `an_unclassified_connection_with_a_full_refusal_queue_is_closed`
- `an_excess_client_connection_is_refused_after_its_hello` (moved from
  `limit_refusal_reads_the_hello_before_it_answers`)
- `a_client_connection_before_the_gate_opens_is_refused_as_starting`
- `a_foreign_client_is_answered_with_this_build_when_refused` (both before the
  gate opens and with client admission full: the client reads this build's
  preamble, then EOF)
- `silent_excess_client_cannot_hold_the_refuser` (moved from `client_accept.rs`)
- `ping_reports_starting_until_the_gate_opens`
- `a_json_peer_gets_one_request_deadline_from_accept` (a peer that sends its
  first byte late gets the remainder of `INITIAL_REQUEST_TIMEOUT`, not a fresh
  one; uses the deadline parameter, not sleeps)
- `accept_failures_are_classified_so_descriptor_pressure_never_stops_the_server`
  (moved from `client_accept.rs` to `listener.rs`)
- `connection_slots_cap_at_their_limit_and_release_on_drop` (from
  `connection_admission_caps_workers_and_releases_slots` and
  `client_connection_admission_caps_handshake_workers_and_releases_slots`) and
  `independent_counters_have_independent_slots` (from
  `independent_servers_have_independent_admission_slots`)
- `dropping_the_handle_stops_the_listener_thread`: rewritten. It takes the
  listener, lock and identity from `bind_private_socket(&path)`, builds the
  handle literal with `gate: ClientGate::default()`, proves a competing
  `bind_private_socket(&path)` is refused as `SocketBusy` naming the path while
  the handle lives and succeeds after the drop, and keeps the listener-exit and
  socket-removal assertions.
- deleted: `api_socket_is_bound_owner_only` (it duplicates `shepr-platform`'s
  `private_listener_socket_is_owner_only`), `socket_path_prefers_explicit_env_override`
  and `socket_path_defaults_to_runtime_dir` (server.rs), and
  `the_active_socket_is_the_build_runtime_socket` and
  `env_socket_override_selects_the_active_socket` (server_stop.rs): the
  wrappers they test are gone, and `shepr-config`'s address and `io.rs` tests
  cover resolution.
- `a_pong_from_a_build_without_starting_reads_as_not_starting` plus the updated
  literal `PONG_RESPONSE` fixture
- presence: `a_dead_socket_is_gone`, `a_starting_pong_is_starting`,
  `stopping_wins_over_starting`, `a_silent_listener_is_unresponsive`,
  `a_listener_that_vanishes_before_answering_is_gone`
- stop: `a_conditional_stop_waits_for_the_client_socket_to_disappear` becomes
  `a_conditional_stop_waits_for_the_socket_to_disappear`, and
  `a_conditional_stop_uses_the_lease_deadline_for_the_last_socket` becomes
  `a_conditional_stop_uses_the_lease_deadline_for_the_socket`;
  `stop_fails_when_socket_remains_reachable_after_timeout` asserts
  `TimedOut { socket, .. }` names the path; the stop-error display test covers
  the new `TimedOut` text

`shepr-config`:
- `address.rs`: `runtime_address_guidance_is_plain`,
  `an_override_is_not_the_runtime_address`,
  `build_mismatch_guidance_names_the_stop_and_attach_commands` and
  `build_mismatch_guidance_keeps_the_socket_override` stay;
  `pane_exported_runtime_sockets_are_not_overrides` is renamed
  `a_pane_exported_runtime_socket_is_not_an_override` and checks the one
  variable; `override_guidance_names_the_override` loses its client-override
  half. Deleted: `api_socket_override_takes_precedence_over_client_socket_override`,
  `client_socket_override_pairs_with_the_runtime_api_socket`.
- `io.rs`: `socket_path_overrides_resolve_independently` is deleted;
  `invalid_socket_environment_fails_resolution` checks the one variable;
  `socket_overrides_beat_the_profile_runtime_directory`,
  `socket_overrides_with_a_matching_marker_win` and
  `socket_overrides_with_another_profiles_marker_are_ignored` become singular
  (`a_socket_override_...`) and set one variable.

`shepr-core`: `an_empty_selector_is_refused_not_unset` over `SheprSocketPath`
alone; `every_variable_has_its_documented_name_and_kind` loses the row.

`shepr-server`:
- `the_lease_is_free_by_the_time_the_socket_goes` (replaces
  `the_lease_is_free_by_the_time_the_client_socket_goes`; the test installs a
  real `ServerHandle` from `start_server` on scratch paths, so the
  socket-present observation is real)
- `bootstrap_opens_the_gate_after_restore` (replaces
  `bootstrap_reservation_binds_the_resolved_client_socket`, the one test that
  drove `HeadlessServer::new` through bootstrap): through the same bootstrap
  path, `ping` answers `starting: true` before `HeadlessServer::new` and
  `starting: false` after it, and a TUI hello is served only after.
- `already_running.rs`: the busy sockets list becomes `["socket", "data_dir"]`
  and the assertion about the API socket released with a refused client bind
  goes
- `a_foreign_client_is_answered_with_the_server_identity_through_the_gate`
  (an end-to-end test through a real handle, gate and handler, in
  `client_transport.rs`)
- deleted: `client_listener_readiness_wakes_for_new_connection`; the bootstrap
  reservation tests
  (`client_socket_reservation_refuses_an_existing_server_before_restore`,
  `client_socket_reservation_refuses_a_live_listener_without_its_lock`,
  `client_socket_reservation_does_not_publish_its_listener_path`,
  `client_socket_reservation_can_be_consumed_by_the_platform_binder`); the
  `socket_paths.rs` tests; `client_socket_is_owner_only_from_the_moment_it_is_reachable`
  (moved to `shepr-platform`, above)
- `test_headless_server()` drops the listener fields and the client startup
  lock; the one test that used `client_socket_path` as a scratch directory
  takes it from a `ScratchDir`.

`shepr-remote`: the probe, launch and bridge tests listed in 5.9, with one
fake socket.

`shepr-mux`: `an_inherited_socket_variable_gives_way_to_the_resolved_socket`
(5.7).

Gate:

```
brokkr check
```

By hand, local leg (dev TUI against a dev server; nothing here installs or runs
a release build):

```
brokkr run --debug shepr-server -- --version
brokkr run --debug -- status server
brokkr run --debug --
```

Expected: `status server` prints `status: not running` and exits 0; the TUI
starts the dev server and attaches with the Local endpoint connected. Then,
from a pane of that dev server:

```
ls $XDG_RUNTIME_DIR/shepr-dev
brokkr run --debug -- status server
brokkr run --debug -- server stop
```

Expected: the runtime directory holds `shepr.sock` and no `shepr-client.sock`;
`status server` prints `status: running` with this build's `build_id` while
the TUI keeps rendering (the same-socket concurrency check); `server stop` ends
the TUI with the server shut down. Afterwards, from the starting terminal,
`brokkr run --debug -- status server` prints `status: not running` again and
`$XDG_RUNTIME_DIR/shepr-dev` holds no `shepr.sock`.

No gate runs `brokkr install`, touches the release server or connects over
SSH. `remote-client-bridge` against the merged socket, and the bridge's own
mismatch answer, are gated by their named tests and `brokkr check` only.

Keep/revert: revert the landing if the real TUI fails to attach to the merged
socket, if `server stop` leaves a socket or lease behind, or if any added test
had to be weakened.

## 7. Stopping rule

In scope: everything in section 3.2. Out of scope, deliberately:

- The writer queue, render demand and the server loop beyond the listener arm
  (work item 4).
- The endpoint supervisor's activation and retry design (work item 1).
- The preamble and welcome content, the build identity rules and the codec,
  except the one new refusal variant, `preamble_for`, and the changed order of
  who writes first.
- The data-directory lease and the saved layout.
- Reading, migrating or deleting a `shepr-client.sock` left by a server of the
  old layout: nothing in this project has run, and that server removes its own
  file.
- An old-build client meeting a server of this build (obstacle 10).
- The CLI surface: no subcommand, flag or output field is added or removed.
  `status` JSON keeps its existing fields; `starting` is a ping field the CLI
  does not print.

## 8. Review dispositions

Both reviews (`notes/spec-single-server-socket-r1.md`, r1, and
`notes/spec-single-server-socket-r2.md`, r2) were checked against the tree.
Findings both raised are merged.

Folded in:

- r1 H1 (the SSH bridge pipes the TUI protocol into a JSON-only socket of
  another build, where the likely outcome is a transient EOF retried forever):
  confirmed against `host.rs`, `local_server.rs` and `handshake_error`. One
  nuance: a hello line that is not UTF-8 closes without any answer, so EOF is
  even likelier than r1 says. Obstacle 8, 5.1 `answer_remote_bridge`, 5.5
  `preamble_for` and 5.9.
- r1 H2 and r2 3 (presence without the re-probe reads a just-finished
  shutdown as unresponsive): 5.3 step 3, AGENTS.md text, two tests.
- r1 M1 and r2 1 (refusals lose the build identity for a foreign client):
  obstacle 1, 5.4 step 5, L1.2, and tests both before the gate and over the
  limit.
- r2 2 (a shared classification cap couples the two kinds, against the work
  item's "keep separate admission") and r1 M6 (overflow refusals name a limit
  that was not reached; refusals on an existing connection thread need no
  queue): obstacle 2 and 5.4 steps 2 to 6.
- r1 M7 (deadlines stack after classification): one `accepted` instant,
  `handle_connection` and `handle_client_handshake` take their deadlines.
- r1 M2 (inventory gaps): section 1, except the "own sockets" sentence
  (rejected below); the inventory also gained sites neither review listed
  (`shepr-remote` limits, `daemon_exit.rs`, `server_stop.rs` messages,
  `client_transport.rs` and `preferences.rs` docs, `local_server.rs` docs).
- r1 M3 (the `starting` doc trips a textlint): 5.3.
- r1 M4 and r2 4 (tests that break or vanish unnamed): section 6 test lists.
- r1 M5 (unpinned choices): `SocketStartupLock::socket_path` goes; the
  owner-only test moves to `shepr-platform` and the duplicate goes; refusal
  writes use `STREAM_WRITE_TIMEOUT`; full slots are refused inline or by the
  refuser as stated; `server_socket_is_live`, `is_running_at`,
  `shepr_api::socket_path` and `active_api_socket_path` are deleted; the lease
  test is named; `ClientIdAllocator` uses `fetch_add` without saturation.
- r1 M8 (by-hand gates): real `status server` output, the "release server and
  dev client" wording dropped. The owner then ruled out any gate that installs,
  runs a release build or connects over SSH, so the by-hand legs use the dev
  build only.
- r1 L1 (`Reserved` hand-off described wrongly): 5.6.
- r1 L2 (old client, new server): obstacle 10 and the stopping rule.
- r1 L3 (inherited `SHEPR_CLIENT_SOCKET_PATH` no longer scrubbed): 5.7 drops
  the absence assertion on purpose.
- r1 L4 (keep the accept-failure classification): obstacle 6 and 5.4.
- r1 L5 and r2 5 (the sweep cannot pass as written): L2.15 has a scope and an
  exact command, and the `bridge.rs` message is renamed.
- r1 L7 (`starting` on `RuntimeStatus`): kept, with the reason in 5.3.
- r1 L8 (accuracy): the L1 test notes, the `supervisor.rs` path, when
  `ServerLifetime::liveness` loses its caller, and the accept thread's no-panic
  rule. A further inaccuracy found while checking: 5.1 said
  `bind_single_use_private_socket` used the startup-lock functions; it does
  not.
- r1 lateral findings: `server_not_running_error` uses `socket_is_live` (5.8);
  the `socket_path` wrapper chain is deleted (5.2).

Rejected:

- r1 M2, the sentence "a dev build uses sibling `shepr-dev` directories, so it
  has its own sockets". It stays true: the runtime directory still holds the
  server socket and the per-machine SSH bridge sockets (`machine_bridge_path`
  in `shepr-remote/src/remote/machine_ssh.rs`).
- r1 L6, first half (add `HandshakeRefusal::ServerStopping`). Declined: a TUI
  that connects after a stop was closed without an answer before this change
  too (it sat in the backlog until the listener closed), so the client's
  classification (transient, retried) does not change; the launcher already
  learns `stopping` from `ping` and never hands a stopping server to a TUI; a
  new wire variant would buy only a message. 5.6 states the window.
- r1 L6, second half (a handshake that meets the closed event channel ends
  with no shutdown notice). Incorrect: `handle_client_handshake` sends
  `send_shutdown_to_unregistered_client` when the `ClientShellConnected` send
  fails.
- r1 lateral, "reconcile r2 before revising": that is this revision, not a
  spec change.
