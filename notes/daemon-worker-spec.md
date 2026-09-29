# Client and daemon binaries

## Owner decisions

The owner settled the open questions below as recommended:

1. `brokkr run` builds every binary in the root package, in debug mode by
   default. Whoever implements this reads `brokkr man` first to learn how
   brokkr is configured.
2. A client spawns a server only for its profile's own runtime address, never
   for an address a socket override picked.
3. Mid-session the client keeps only reconnecting to Local; it never relaunches
   it.
4. The api `session` module is renamed in the stop landing.

## Starting point

The CLI reduction in `notes/cli-ux-spec.md` has landed in full. This spec
starts from that tree. What it left in place and this work builds on:

- One executable with both roles. `src/main.rs::launch` dispatches the TUI,
  the hidden `client` (attach without launching anything) and
  `remote-client-bridge` modes, the CLI commands, and bare `shepr server`,
  which calls `shepr_server::server::headless::run_server`. The TUI and the
  remote bridge both start a missing server through
  `shepr_remote::local_server::ensure_running`, whose
  `spawn_server_daemon` re-executes the running binary as `shepr server`.
- The CLI is `status` (overview, `status server`, `status client`),
  `server stop [--force]`, `detect` and `integration`, plus the hidden
  `client` and `remote-client-bridge`. `detect capture` and pane-backed
  `detect explain` go through the JSON API; `detect explain --file` and
  `integration` run in the CLI process.
- Named sessions are gone. `shepr_config::AppPaths` selects the runtime
  directory and a new `data_dir()` (saved layout, history, server log and
  the `DataDirLease`) by `BuildProfile`: release keeps the plain XDG
  `shepr` locations, dev builds use sibling `shepr-dev` directories. Config
  and the client state directory are shared by every profile.
- The `SHEPR_SOCKET_PATH` and `SHEPR_CLIENT_SOCKET_PATH` overrides still
  exist and redirect only the sockets, never the data directory. Separate
  work in progress adds a build-profile marker so a pane's exported
  overrides are ignored by a process of another profile; this spec takes
  whatever `AppPaths` resolves and does not depend on how that lands.
- Machines are `[[machines]]` entries in `config.toml`, validated at launch
  and keyed by `MachineLabel`; `SshTarget`, `MachineLabel` and
  `MachineConfig` live in shepr-config. There is no catalog and no add-time
  preparation.
- Before the TUI takes the terminal, `src/preflight.rs` runs
  `shepr_remote::preflight` (in `crates/shepr-remote/src/remote/preflight.rs`):
  every machine is checked concurrently through `check_saved_ssh`, results
  are classified (ready, needs authentication, offline, host key,
  incompatible, failed), and interactive ssh runs one machine at a time for
  those that need authentication. `check_saved_ssh` treats a stopped remote
  server as fine (the bridge starts one) and a running server of another
  build, or one that is not a detached daemon, as an error.
- `server stop` still refuses a server of another build without `--force`
  (`StopTargetBuild` and `guard_mismatched_stop`), and every mismatch
  guidance names `shepr server stop --force`. The API module holding stop,
  that guard and the guidance is still named `session`.
- The build id is stamped into shepr-protocol by the workspace `build.rs`
  and covers the source tree and the build profile. `shepr --version`
  prints `shepr <version>+<build id>`.

Keep the build identity and positional wire handshake: client and server
must be the same identifiable build, profile included. There is no
compatibility layer.

## Executables and commands

The root package produces `shepr` and `shepr-server`, the second as a bin
target under `src/bin/`. List both in `brokkr.toml`'s `[bin] install` so
`brokkr install` installs the pair together. Keep both binaries in the same
directory; neither a PATH lookup nor an environment override chooses the
local daemon. Resolve the running client with
`shepr_platform::launch_executable`, which already follows a replaced
executable that Linux marks `(deleted)`, then replace its file name with
`shepr-server`. Report a missing or nonexecutable sibling as an install
error with the path. An old client whose file was replaced resolves to the
new sibling; the readiness identity check below reports that as a different
build, which is correct. An upgrade installs both files on each host; a
client alone is not a usable installation.

`shepr` owns the TUI, CLI presentation, configured machine connection and
startup SSH authentication, local daemon rendezvous, the hidden `client`,
and the hidden `remote-client-bridge` that relays SSH stdio to the daemon
client socket. It keeps `status`, `server stop`, `detect` and
`integration`. The bridge uses the same daemon launcher as the local TUI,
keeps stdout exclusively for framed traffic and reports startup errors on
stderr.

`shepr-server` owns config validation for the serving host
(`AppPaths::resolve_for_server`, which reads the `SHEPR_STARTUP_CWD`
handoff), the headless loop, PTYs, persistence, the API and client socket
listeners, and bundled detection. Its public invocation is `shepr-server`
in the foreground, for diagnosis or service management, plus `--version`
printing the same `<version>+<build id>` identity. A private
`--client-spawned` argument marks the client launch path. No `shepr server`
subcommand remains, and `shepr-server` parses nothing else.

The root binary's production uses of the server library are narrow:
`run_server` in `main.rs` and the `RunServerError` rendering in
`src/cli/error.rs`. Both move into the server binary. Its declared
shepr-mux dependency is already unused by `src/`. So the client binary
drops shepr-server and shepr-mux without new shared crates; the only thing
the two sides must agree on is the small daemon exit classification below,
which fits in shepr-api next to the stop and status code. The server binary
links shepr-server and not shepr-client.

Both binaries take `BUILD_ID` from the one shepr-protocol they link, so a
pair from one cargo build has one identity by construction. A cheap
integration test still asserts that the two `--version` outputs agree. Do
not add protocol generations or broadarrow's content-hash cohort.

Open decision: `brokkr run` must build the sibling too, or a dev client
spawns nothing (or a stale `target/debug/shepr-server`). Recommendation:
make `brokkr run` build every bin of the root package before running
`shepr`, and have the launcher's install error name the missing sibling
path so a partial build is obvious.

### Remote pair check

Discovery already runs `shepr status client --json` on each candidate
remote `shepr` and rejects one of another build before a bridge starts.
The simplest pair check extends that one probe instead of adding an SSH
round trip: `status client` runs on the remote host, so it can resolve its
own sibling `shepr-server`, run its `--version`, and report the sibling's
path and build in `ClientStatusJson`. Discovery then accepts a candidate
only when both builds match this client. A stale or missing sibling is an
install error, never a reason to launch whatever is on PATH.

Keep the remote executable cache keyed by SSH target, dropped on a failed
candidate as `SavedSshConnector` does now. A connection that uses the
cached path does not need to repeat the pair check: the remote bridge's
launcher verifies the spawned sibling's identity, and the protocol
handshake checks the running server's.

## Launch and rendezvous

Keep the launch directory handoff through `SHEPR_STARTUP_CWD` and the
detached child that runs from home or `/`, as `spawn_server_daemon` does
today. It already calls `setsid`, so the child leads a new session and
process group with no controlling terminal; the guard below kills that
group. Keep stdin and stdout on `/dev/null`, but send boot stderr to a
private file in the runtime directory instead of `/dev/null`, which is
where boot failures go today. Direct foreground starts use the same
ownership checks but print their own errors and ready notice.

Use two locks with different lifetimes. The lifetime lock exists:
`run_server` takes `shepr_mux::persist::DataDirLease` on the profile's data
directory before logging or persistence opens anything there, and holds it
until shutdown; keep that ordering. `bind_private_socket` also holds a
per-socket startup lock for the listener's life. What is missing is a
bounded, nonblocking-poll client-side launch lock around probe, spawn and
readiness wait, so concurrent first launches do not each spawn. Probe again
after acquiring it. Do not use a pid file. Keep lock files after release so
contenders always lock the same inode.

Key the launch lock to the profile, not to the socket pair. Socket
overrides move only the sockets, so every server of one profile competes
for the same lease anyway: at most one can run per profile per host. The
runtime directory is not moved by overrides either and is the natural home
for the launch lock.

Open decision: whether a client spawns a server at all when a socket
override selects its address. Recommendation: spawn only for the profile's
own runtime address; under an override, report that no server is running
at the named socket. An override exists to reach a running server (a pane
of it, or a test), and a server spawned for it would only meet the lease
held by the profile's real server.

Classify the initial probe explicitly: absent or stale means no listener; a
live responsive server is usable only when its build matches; a live but
unresponsive, inaccessible or untrusted socket is a failure and must not
cause a second spawn. `shepr_platform::ipc::probe` already separates
`Absent`, `Stale`, `Live` and `Unreachable`; follow `Live` with a bounded
status request (`read_runtime_status_at`) rather than trusting connect
alone. Reuse the private lock files, staged private bind, stale socket
preparation, identity-based unlink and peer checks in `shepr_platform::ipc`.
The accept-side peer check stays. Add a client-side check before sending
status, stop or attach bytes, including the bridge's local connection;
`ipc::peer_is_same_user` works on the connecting end as well. Bound each
probe by one deadline covering connect and response. Do not unlink a live
or unclassified socket.

Hold the launch lock until the child answers a real status probe with this
build's identity. Poll under an overall deadline, checking
`Child::try_wait` on each pass. Keep the child in a guard that kills and
reaps its process group on every unsuccessful exit path, including a failed
probe and the timeout; disarm only after a successful identity check. A
daemon that exits during boot reports its status and the tail of the
bounded boot log. Open the boot log as an owner-only regular file without
following symlinks. The durable server log stays in the data directory for
normal operation; boot stderr is for failures before tracing is ready. A
small shared exit classification distinguishes already running (today's
`RunServerError::AlreadyRunning` and the lease refusal
`SessionDataHeld`), config refusal and other boot failure, but the printed
error remains authoritative. The client must not turn a failed launch into
indefinite readiness polling.

The server keeps `run_server`'s API and client socket setup, shutdown
drain, saved layout and fresh PTY restore. Its lease is released only after
that shutdown. A direct `shepr-server` competes for the same lease and
sockets and gets a named already-running error. It never holds the launch
lock for life, so a client attaches promptly to a healthy foreground
server; that server's lifetime is the operator's responsibility.

## Builds, upgrades and stopping

The build id is a fingerprint, not an ordered version. Say "different
build" in UI and errors; do not infer that a server is older. A running
mismatched server is never silently reused or stopped.

At TUI startup, before raw mode, offer a restart for each responsive
mismatched server whose installed pair has been verified as this build:
Local, and each configured machine that passed the preflight. Say clearly
that stopping exits pane processes and restore rebuilds fresh shells,
possibly resuming agents. Default to keeping it, and keep it without asking
when there is no terminal to prompt on (the preflight's `can_prompt`
gate). Declining leaves the endpoint unavailable with actionable
diagnostics while other endpoints continue. Offline, authentication-failed,
host-key, unknown-build or wrongly installed hosts are reported without a
restart prompt. A mismatch found later during reconnection is reported in
the TUI; restart the client to get the pre-TUI prompt.

The preflight is the place for the remote half, and it needs one change of
shape to carry it. Today `check_saved_ssh` returns a flat `io::Error`, a
different build lands in `MachineCheck::Incompatible` with no structure,
and `src/preflight.rs` prints nothing for that class. Give the check a
structured result that carries the remote status (build, and the boot
identity below) and the discovered executable, add a different-build
outcome next to the existing classes, and re-check the machines whose
interactive authentication succeeded, which today are not checked again.
Collect Local's mismatch from the launch probe and offer all restarts in
one pass after the authentication prompts, so prompts never interleave.

On acceptance, re-read the status and stop only the instance that was
observed. A build id alone cannot identify an instance, so status must
expose a boot identity and the `server.stop` request (today it takes empty
params) must carry the expected one; the server refuses a stop aimed at a
different boot. If the occupant changed, return to discovery. Wait,
bounded, for socket disappearance and lease release, then run normal
rendezvous and verify the new daemon's build. Never start a server merely
to stop it. Remote restart runs the discovered remote `shepr server stop`
with a private expected-boot option over the managed SSH connection, then
lets `remote-client-bridge` launch the remote sibling.
`RemoteCliCommand::ServerStop` already exists for building that command
line and has no production caller since add-time preparation went; replace
its `force` field with the expected boot. Remote failure stays soft and
does not block the other machines.

Keep `shepr server stop` as a local, explicit shutdown command and remove
`--force` with `StopTargetBuild`, `guard_mismatched_stop` and
`SessionError::BuildMismatch`. The guard's own guidance comment already
concedes that profile separation means a mismatch is another build of the
same profile, which is the upgrade case the stop exists for. Stop keeps its
peer-checked narrow API path, never launches a daemon, and reports when no
server is present. `detect`, the remaining API command, keeps refusing a
build mismatch. Remote operators use `ssh <host> shepr server stop`.
Update the guidance that names `--force` (the `LocalBuildMismatch` text in
shepr-api's guidance, the Local endpoint's mismatch guidance in the client
supervisor, the launch notice in `src/autodetect.rs`) to the plain stop
command.

The API module is still named `session`, which now means nothing. Rename it
in the landing that rewrites stop (for example to `server_stop`, since
`stop.rs` already holds `ServerStopSignal`), unless the in-progress cleanups
have done so.

## Removed machinery and retained behavior

Remove the `shepr server` dispatch (`Launch::HeadlessServer` and the bare
`server` form in the clap spec), the self re-exec in
`spawn_server_daemon`, and the `ServerReady` notice's advice about
`shepr server`. Remove the detached-daemon capability:
`ServerCapabilities::detached_server_daemon`,
`shepr_platform::current_process_is_detached_server_daemon` and its
`/proc` session-leader test, `RemoteServerStatus`'s flag, and
`remote_server_not_detached_error` with its branch in `check_saved_ssh`.
With a client-spawned daemon there is no foreground server to detect; a
foreground `shepr-server` is the operator's explicit choice. Remove the
`BuildCheck` split once the launch probe and handshake both enforce
identity; the bridge's reason for deferring (a typed handshake mismatch
reaches the client, an stderr failure does not) still holds, so the
bridge's launcher should not fail on a running mismatched server either.
Remove `FORCE_STOP_FLAG` and every `--force` hint.

Retain the headless server's signal, API stop, persistence and logind
shutdown paths, the SSH bridge and agent registration, status probes, build
preamble, JSON API, and soft failure with configured machines. Local is
launched at startup and in the accepted restart flow; with configured
machines a Local launch failure does not close the client. Mid-session,
the client only reconnects to Local once something restarts it, as today
(`LocalFailurePolicy`); keep that, since a stopped Local server is usually
one the operator stopped. Config is validated once per process launch, and
the client still validates the config snapshot it receives from a server.

## Testing and green landings

Use process tests with isolated paths and the real pair (integration tests
see both bins). Cover first client spawn, a second client reusing the same
daemon, simultaneous first clients, direct server versus client startup, a
failed boot with useful stderr, timeout cleanup without an orphan, stale
socket recovery, refusal of an unresponsive or foreign-owned socket, stop
without autospawn, and restart after death. Exercise different-build and
restart decisions through the existing `PreflightSsh` seam and fake status
answers: consent, refusal, no terminal, changed occupant, offline and a
remote pair mismatch. Existing attach, restore, API stop,
configured-machine reconnection, preflight and SSH bridge tests remain part
of the full `brokkr check` gate.

Land this in reviewable commits, each building and passing the full gate:

1. Add the `shepr-server` binary with `--version`, the install set, and the
   identity test. `shepr server` keeps working and the launcher still uses
   it.
2. Move the launcher (TUI and bridge alike) to the sibling executable with
   the launch lock, identity readiness, child guard, boot log and
   client-side peer check. Server lifecycle and wire stay as they are.
3. Remove `shepr server` and the self re-exec, and drop the root binary's
   shepr-server and shepr-mux dependencies.
4. Extend `status client` with the sibling's identity, make discovery check
   the pair, and remove the detached-daemon capability and its preflight
   error.
5. Add the boot identity to status and the conditional stop, remove
   `--force` and its guard, update guidance, and rename the `session` API
   module.
6. Add the pre-TUI restart offer: the structured preflight result, the
   re-check after authentication, Local's mismatch, and the remote
   conditional stop through `RemoteCliCommand::ServerStop`.

## Corrections to the source notes

- `notes/daemon-worker-inspiration.md` describes "where shepr is today"
  from before the CLI reduction: `machine add` and its restart prompt no
  longer exist, and a non-daemon remote server is now an error from the
  startup preflight rather than a prompt.
- It lists broadarrow's private staged socket bind as a technique to take.
  `shepr_platform::ipc::bind_private_local_listener` already stages the
  bind, with a hard link rather than broadarrow's rename, plus a
  bind-then-chmod fallback. The remaining work is to review the fallback
  and add the outgoing peer check, not a second binder.
- It treats a daemon-side lifetime flock as wholly new. `DataDirLease`
  already guards the data directory, and `bind_private_socket` holds
  per-socket startup locks through listener teardown. The missing piece is
  the client-side launch lock and its failure cleanup.
- `notes/cli-ux-spec.md` kept `shepr server`, `server stop --force` and the
  detached-daemon check, and landed that way. This work changes those three
  decisions.

## Lateral findings

- `bind_private_local_listener`'s fallback briefly binds at the final path
  before chmod. The accept-side peer check still applies, and
  `XDG_RUNTIME_DIR` is normally owner-only, but the implementation should
  decide whether the
  fallback is acceptable rather than claim the socket is private before it
  is reachable.
- `wait_for_server_socket` only tests that a connection succeeds, and its
  timeout says the background server may still be starting. A connect can
  precede useful readiness, a timeout leaves the child running, and boot
  stderr goes to `/dev/null`. The guarded launch above closes all three.
- The startup preflight drops what it learns about incompatible machines:
  `result_notices` prints nothing for `MachineCheck::Incompatible`, and the
  client's connectors never call `check_saved_ssh`. So a remote server of
  another build is only reported later by the handshake, and a remote
  server that is not a detached daemon is attached to anyway, its error
  never shown. Landing 4 removes the second case; the first is what the
  structured preflight result fixes.
- `check_saved_ssh` runs full discovery for every machine at every launch
  and neither reads nor updates `SshMetadataCache`, so the preflight pays
  several SSH round trips the connector then repeats or skips via the
  cache.
- The SSH metadata cache lives in the client state directory, shared by
  every profile. A dev and a release client for the same target overwrite
  each other's remembered executable, and remote discovery only looks for
  an installed `shepr` (PATH, `~/.cargo/bin`, `~/.local/bin`), so a dev
  client can never find a matching remote dev build. Testing the remote
  pair flow from a dev build needs an answer to that.
- The root package auto-detects the workspace `build.rs` and so hashes the
  whole tree and writes build id files that nothing in `src/` includes;
  only shepr-protocol's copy is used. It looks like a redundant full-tree
  hash per root build.
- Stale wording after the reduction: the doc comment on
  `current_process_is_detached_server_daemon` says remote attach restarts a
  non-detached server; `RunServerError::SessionDataHeld` and its message
  still speak of a "session data directory".
