# Client and daemon binaries

## Starting point and dependency

This work follows `notes/cli-ux-spec.md`. Its CLI reduction removes named
shepr sessions, direct remote attach, `--machine` and the API bridge, moves
machines into launch-time config, separates dev and release runtime and saved
state paths by build profile, and authenticates configured SSH hosts before
entering the TUI. This spec keeps that target surface except for the server
entry point and `server stop --force`, as described below. It does not depend
on the later, optional automatic integration install.

The current executable has both roles: `src/main.rs::launch` dispatches the
TUI and `shepr_server::server::headless::run_server`, while
`shepr_remote::local_server::spawn_server_daemon` re-executes that executable
as `shepr server`. The root package already has separate `shepr-client` and
`shepr-server` library crates. The root binary links both. Keep the current
build identity and positional wire handshake: client and server must be the
same identifiable build, including profile. There is no compatibility layer.

## Executables and commands

The root package produces `shepr` and `shepr-server`. List both in
`brokkr.toml`'s install set so `brokkr install` installs the pair together.
Keep both binaries in the same directory; neither a PATH lookup nor an
environment override chooses the local daemon. Resolve the running client
with `shepr_platform::launch_executable`, which handles a replaced executable
marked `(deleted)` by Linux, then replace its file name with `shepr-server`.
Report a missing or nonexecutable sibling as an install error with the path.
The same rule applies when a cached remote `shepr` path is used: the remote
client finds its own sibling on that host. An upgrade installs both files on
each host; a client alone is not a usable installation.

`shepr` owns the TUI, CLI presentation, configured machine connection and
SSH authentication, local daemon rendezvous, and the hidden
`remote-client-bridge` that relays SSH stdio to the daemon client socket.
It keeps `status` (including `status client` for remote executable discovery),
`server stop`, `detect`, and `integration` commands from the reduced CLI.
`detect capture` and pane-backed `detect explain` remain JSON API clients;
file-backed explain and integration management stay in the client process.
The bridge uses the same daemon launcher as the local TUI. It must preserve
stdout exclusively for framed traffic and report startup errors on stderr.

`shepr-server` owns config validation for the serving host, the headless
loop, PTYs, persistence, API and client socket listeners, and bundled
detection. Its public invocation is `shepr-server` in the foreground for
diagnosis or service management. A private `--client-spawned` argument marks
the client launch path. No `shepr server` subcommand remains. `shepr-server`
does not parse TUI, SSH, status, stop, detect or integration commands. The
root executable drops its production dependencies on `shepr-server` and
`shepr-mux`; split any shared launch types into an existing lower crate
without reversing the dependency rules. The server binary links the server
library and does not link `shepr-client`. The two binaries use the same
build-id recipe; an integration test checks their reported identities. Do
not add protocol generations or broadarrow's content-hash cohort.

Remote discovery still executes the discovered remote `shepr status client
--json` to learn its build and executable path. That result must match the
local client before a bridge starts. Add a cheap remote check that derives
and verifies the executable sibling `shepr-server` and reads its build id
without starting a server. `shepr-server --version` is sufficient if it
prints the same identity format. A stale or missing sibling is an install
error, not a reason to launch whatever is on PATH. Keep the remote executable
cache keyed by SSH target; invalidate or rediscover on a failed candidate,
and repeat the pair check when a connection starts so an install changed
since startup is detected. The bridge and the normal protocol handshake
still check identity at use time.

## Launch and rendezvous

Keep the profile-separated `AppPaths` from the CLI reduction. Socket
overrides, if still supported there, must select the matching lock and
readiness target too. Preserve the launch directory handoff through
`SHEPR_STARTUP_CWD` and run a detached child from home or `/`, as
`spawn_server_daemon` does today, so a daemon does not pin the caller's
directory. The child has no controlling terminal, has stdin and stdout on
`/dev/null`, and sends boot stderr to a private file in the runtime tree.
Use a new process group for child cleanup; ensure it survives the client
exiting after readiness. The private argv marker communicates that a client
owns startup. Direct foreground starts use the same ownership checks but
print their own errors and ready notice.

Use two locks with different lifetimes. A bounded, nonblocking-poll flock
around the client-side probe, spawn, and readiness wait serializes concurrent
first launches. Probe again after acquiring it. A separate server lifetime
flock, acquired before persistence or logging and held until shutdown,
prevents two daemons from restoring and writing the same state. The existing
`shepr_mux::persist::DataDirLease` is the natural lifetime guard; retain it
and ensure no persistent state is opened before it. Do not use a pid file.
Keep lock files after release so contenders always lock the same inode.

Classify the initial probe explicitly: absent or refused means a stale or
missing listener; a live responsive server is usable only when its build
matches; a live but unresponsive, inaccessible, or untrusted socket is a
failure and must not cause a second spawn. Use a bounded status or handshake
request for the responsive check rather than socket connect alone. Reuse
`shepr_platform::ipc` socket probing, private lock files, staged private
bind, stale socket preparation, identity-based unlink and peer credential
checks. The JSON API's accept-side peer check stays. Add a client-side uid
check before sending status, stop or attach bytes, including the bridge's
local connection. Bound each probe by one deadline covering connect and
response. Do not unlink a live or unclassified socket.

Hold the rendezvous lock until the child answers the real readiness probe
with this build's identity. Poll under an overall deadline, checking
`Child::try_wait` on each pass. Keep the child in a guard that kills and reaps
its process group on every unsuccessful exit path, including a failed probe
and timeout; disarm only after a successful identity check. A daemon that
exits during boot reports its status and the tail of the bounded boot log.
The boot log must be opened safely as an owner-only regular file without
following symlinks. Keep the durable server log in the state tree for normal
operation; boot stderr is for failures before tracing is ready. A small
shared exit classification may distinguish already running, config refusal,
and other boot failure, but the printed error remains authoritative. The
client must not translate a failed launch into indefinite readiness polling.

The server continues to use `run_server`'s API and client socket setup,
normal shutdown drain, saved layout and fresh PTY restore. Its lifetime
guard closes only after that shutdown. A direct `shepr-server` competes for
the same lifetime lease and sockets and gets a named already-running error.
The client rendezvous lock is only a launch lock; a direct server never holds
it for life. This lets a client attach promptly to a healthy foreground
server, while that server's foreground lifetime remains the operator's
responsibility.

## Builds, upgrades and stopping

The build id is a fingerprint, not an ordered version. Say "different build"
in UI and errors; do not infer that a server is older. A running mismatched
server is never silently reused or stopped. At TUI startup, before raw mode,
offer a restart for each responsive mismatched server whose installed client
and daemon pair has been verified as this build. This includes Local and
configured machines reached after the CLI reduction's startup SSH
authentication. Say clearly that stopping exits pane processes and restore
rebuilds fresh shells, possibly resuming agents. Default to keeping it.
Declining leaves the endpoint unavailable with actionable diagnostics while
other endpoints continue. Offline, authentication-failed, unknown-build or
wrongly installed hosts are reported without a restart prompt. A mismatch
found later during reconnection is reported in the TUI; restart the client
to get the pre-TUI prompt.

On acceptance, re-read the status immediately and stop only the same
responsive server instance observed for the prompt. A build id alone cannot
identify an instance; expose its boot identity in status and make the stop
request conditional on that identity, so replacement between status and
stop is refused by the server. If it changed, return to discovery. Send the
conditional `server.stop` JSON API request with a bounded wait for socket
disappearance and lifetime lease release, then run normal rendezvous and
verify the new daemon's build. Do not auto-start a server merely to stop it.
Remote restart uses the discovered absolute remote `shepr` with a private
expected-boot option on `server stop` over the managed SSH connection; that
command sends the same conditional API stop request. Then it lets
`remote-client-bridge` launch the remote sibling. Keep remote failure soft
and do not block the remaining machines.

Retain `shepr server stop` as a local, explicit shutdown command and remove
`--force`. The flag currently protects the installed server from a dev
client using the same paths; profile separation removes that collision.
Stop must be able to address a mismatched build, since it is the upgrade
remedy. It uses the local JSON API's narrow stop path, with peer uid and
status checks, and never launches a daemon. If no server is present it
reports that fact. Commands other than status and stop continue to refuse
a build mismatch. Remote operators can use `ssh <host> shepr server stop`,
as planned by the CLI reduction. Update status output and mismatch guidance
to show the active build and plain stop command, without sessions or
`--force`.

## Removed machinery and retained behavior

Remove `shepr server` dispatch, self re-exec for the daemon, the
`detached_server_daemon` capability and `/proc` session-leader test, and
the remote check and restart prompt tied to that capability. Remove the
old `BuildCheck` split once the launch probe and handshake both enforce
identity. The pre-TUI build restart offer above replaces the old
foreground-server prompt; it is an explicit upgrade action, not a condition
for remote readiness. Remove stale session, force-stop and bootstrap hints.

Retain the headless server's signal, API stop, persistence and logind
shutdown paths, the SSH bridge and agent registration, status probes, build
preamble, JSON API, local reconnect and soft failure policy with configured
machines. A missing Local server should still be launched at startup and
after a restart; with configured machines, a Local launch failure does not
close the client. Config remains validated once per process launch, and
the client still validates the config snapshot it receives from a server.

## Testing and green landings

Use process tests with isolated paths and executable fixtures for the real
pair. Cover first client spawn, second client reuse of the same daemon,
simultaneous first clients, direct server versus client startup, a failed
boot with useful stderr, timeout cleanup without an orphan, stale socket
recovery, refusal of an unresponsive or foreign-owned socket, stop without
autospawn, and restart after death. Exercise different-build identity and
restart decisions with fake status/SSH seams, including consent, refusal,
changed occupant, offline and remote install mismatch. Assert the installed
client and daemon identities agree. Existing attach, restore, API stop,
configured-machine reconnection and SSH bridge tests remain part of the
full `brokkr check` gate.

Land this in reviewable commits, each building and passing the full gate:

1. Add the second root binary and joint install/build identity wiring. Move
   server dispatch into it while temporarily retaining the old client
   command as a thin compatibility entry point for a green transition.
2. Move client launch to the sibling executable and add the guarded,
   bounded rendezvous with boot diagnostics. Preserve the current wire and
   server lifecycle throughout.
3. Verify remote executable pairs, make the bridge use the new launcher,
   and remove the detached-daemon capability and its remote prompt.
4. Add the pre-TUI mismatch restart decision for Local and configured
   machines, then simplify `server stop`, status and guidance.
5. Remove the transitional `shepr server` path and remaining self re-exec
   dependencies; verify the client binary no longer links server or mux.

## Corrections to the source notes

- `notes/daemon-worker-inspiration.md` lists broadarrow's private staged
  socket bind as a technique to take. Shepr's
  `shepr_platform::ipc::bind_private_local_listener` already has a staged
  bind, using a hard link rather than broadarrow's rename, plus a
  bind-then-chmod fallback. The remaining work is to review the fallback
  and add the outgoing peer check, not to add a second socket binder.
- The inspiration note treats a daemon-side lifetime flock as wholly new.
  `shepr_mux::persist::DataDirLease` already guards the persisted data tree,
  and `bind_private_socket` holds per-socket startup locks through listener
  teardown. The missing piece is the client-side launch rendezvous and its
  failure cleanup.
- `notes/cli-ux-spec.md` leaves `shepr server`,
  `server stop --force`, and detached-daemon checks in its target. This work
  intentionally changes those three decisions after its landings; no CLI
  reduction landing needs to anticipate the split.

## Lateral findings

- `shepr_platform::ipc::bind_private_local_listener` documents a fallback
  that briefly binds at final path before chmod. It still performs an
  accept-side peer check. The implementation should decide whether the
  fallback is acceptable on the supported runtime filesystem rather than
  claim the socket is unconditionally private before it is reachable.
- `shepr_remote::local_server::wait_for_server_socket` currently tests only
  that a connection succeeds, and its timeout says the background server
  may still be starting. A successful connect can precede useful readiness;
  a timeout can leave that child running. The guarded handshake readiness
  above closes both gaps.
- `build.rs` fingerprints the whole `src/` and `crates/` trees plus profile
  inputs. Adding the second binary under `src/` therefore changes the build
  id for both; package-local stamping must remain identical in the pair.
