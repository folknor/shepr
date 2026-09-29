# Daemon/client split: what broadarrow does

A distillation of how `research/broadarrow` runs a separately built daemon
that its client auto-launches, gathered to inform splitting shepr into a
`shepr` (TUI) binary and a `shepr-server` (daemon) binary. Paths below are
relative to `research/broadarrow/`.

## Where shepr is today

- One binary. The TUI starts the server by re-executing itself as `shepr
  server`, detached with `setsid`, stdio on `/dev/null`, working directory
  home (`spawn_server_daemon` in `crates/shepr-remote/src/remote/local_server.rs`),
  then polls for the client socket. On remote hosts the hidden
  `remote-client-bridge` entry point starts the daemon the same way.
- Client and server compare a build id (computed over the workspace in the
  root `build.rs`, profile included) in the handshake and refuse to talk when
  the ids differ.
- Bare `shepr server` runs the server in the foreground of whatever terminal
  typed it. Because such a server dies with its terminal, the server reports a
  `detached_server_daemon` capability, remote checks treat a server without it
  as not ready, and `machine add` offers to restart it.
- `shepr server stop` refuses a server of a different build unless `--force`
  is given, a guard against a dev build stopping the installed server. After
  every upgrade the client refuses the old server and tells the user to run
  `shepr server stop --force`.
- `crates/shepr-client` depends on neither `shepr-server` nor `shepr-mux`, so
  a TUI-only binary would not link the mux, the server, PTY hosting or Git
  state.

## Open questions this informs

Raised in the CLI review (`notes/cli-ux.md`), not yet decided:

- Split the binary into `shepr` and `shepr-server`, which takes the server
  out of the CLI entirely and removes the detached-daemon capability, check
  and restart prompt.
- Drop `--force` from `server stop`: once dev builds use their own runtime
  paths (landing 7 of `notes/cli-ux-spec.md`), a release client only ever
  meets an older release server, which is the upgrade case.
- Offer at TUI startup to restart an older-build server, local or on a
  configured machine, next to the planned startup SSH authentication.

## broadarrow's shape

Four binaries: `ba` (client), `ba-daemon`, `ba-worker` (one detached process
per trading account, spawned by the daemon) and `ba-ui`. The daemon-to-worker
link reuses the client-to-daemon patterns, so it is a second instance of the
same design.

- Shared wire types, framing, runtime paths, socket bind, peer check and the
  daemon exit-code enum live in one crate, `crates/daemon-protocol`. Build
  identity is `crates/build-stamp`; every flock claim is `crates/file-lock`.
- The client finds the daemon as a sibling of itself:
  `current_exe()?.with_file_name("ba-daemon")` (`resolve_daemon_binary` in
  `crates/ba/src/rendezvous/spawn.rs`). No PATH search. A test-only env
  override exists behind a cargo feature, and a script refuses any install
  graph that enables it. The daemon finds `ba-worker` the same way, and only
  warns at boot if it is missing, because installs may land after the daemon
  is up.
- `brokkr install` installs all four from `brokkr.toml`'s install list.
  Installing the client alone yields a client with nothing to spawn.
- It does not handle a running binary replaced on disk (`current_exe()`
  reporting `(deleted)`); shepr's `launch_executable()` already does.

## Auto-launch

In `crates/ba/src/rendezvous/spawn.rs` (`daemon_access`):

1. Ensure the runtime dir (`$XDG_RUNTIME_DIR/broadarrow`, with fallbacks;
   relative or empty values count as unset so two working directories cannot
   yield two trees).
2. Fast path: one ping with a 1s deadline, classified as live, dead,
   unresponsive, untrusted (peer uid mismatch) or unresolved (EACCES and the
   like). Only ECONNREFUSED and ENOENT prove absence; anything else refuses
   instead of spawning.
3. Take the rendezvous flock `daemon.lock`, polling for at most 3s, then
   refuse with an error rather than hang. Held across probe, spawn and
   readiness.
4. Probe again under the lock, closing the concurrent-first-run race.
5. If dead and the socket file exists, unlink it before spawning (measured:
   5s failure vs 0.06s success).
6. Open `daemon-boot.log` in the runtime dir (`O_NOFOLLOW`, 0600, truncated)
   as the daemon's stderr. A file, not a pipe, so the detached daemon can
   never block on a full pipe.
7. Spawn `ba-daemon --client-spawned`, stdin and stdout on `/dev/null`, in
   its own process group (`process_group(0)`, not `setsid`). The flag tells
   the daemon its parent holds the rendezvous lock; it is argv, not env, so a
   stray export cannot forge it.
8. Keep the `Child` in a `SpawnedDaemon` guard whose `Drop` kills the whole
   process group unless the spawn was explicitly resolved. This fixed a real
   bug where an early `?` return orphaned a half-started daemon.
9. Readiness: ping every 50ms up to a 5s ceiling, and `try_wait` the child
   on each poll so a daemon that dies during boot is noticed within one
   interval. Failure messages include the boot log and the exit status. Only
   absence or timeout retries; a refusal from a bound daemon is reported; an
   untrusted peer kills the child. A daemon alive but never binding is
   killed, since releasing the lock would let the next client spawn a
   second one.

Daemon side, `acquire_boot_ownership` in `crates/daemon/src/boot_ownership.rs`:
the daemon takes its own lifetime flocks (`daemon.instance.lock` in the
runtime tree and an adoption lock in the state tree) with `try_lock` before
opening its store or log or binding any socket, and exits 1 with a named
message if another daemon holds them.

Socket bind, `bind_private_socket` in
`crates/daemon-protocol/src/rendezvous/socket.rs`: probe an existing socket
and unlink only if dead; bind at a staging name, chmod, verify 0600, rename
into place, so the socket is never connectable with looser permissions.

No pid files anywhere. Liveness is flock plus a ping; the kernel releases
flocks on death, so a crash leaves nothing stale.

Daemon exit codes are a shared enum (`BaDaemonExit` in
`crates/daemon-protocol/src/daemon_exit.rs`: clean, transient, incomplete
teardown, config refusal, signal) so the client can classify a child that
died during readiness.

## Build identity and mismatch

- Two identities: a display id (semver plus commit, `-dirty`), shown and
  logged but never compared; and a comparison "cohort", a SHA-256 over the
  HEAD tree, staged, unstaged and untracked content, `Cargo.lock`, profile,
  target, rustc version and flags, and toolchain environment. Every binary's
  `build.rs` emits it into a byte-stable generated file (no timestamps, which
  forced relinks under LTO). If the tree moves while hashing, the result is
  an "unidentifiable" value that compares unequal to everything, itself
  included, so nothing can falsely claim "already current". Every binary
  prints its cohort in `--version`.
- Ordinary commands do not compare builds. They are protected by separate
  protocol generation numbers: requests declare a minimum generation and the
  client refuses before dispatch if the daemon is older; the daemon
  down-converts responses for older clients.
- Builds are compared only in the explicit `ba upgrade`
  (`crates/ba/src/upgrade.rs`): for a client-spawned daemon it preflights the
  new worker binary, asks the daemon to quiesce and exit (workers keep
  running), waits for the socket to die, re-runs the auto-launch, and checks
  the new daemon reports the client's cohort before rolling workers. A daemon
  launched directly by the operator is never bounced automatically, because
  a replacement would inherit the wrong environment.

The heavy cohort exists because broadarrow must prove convergence before
stopping live trading processes. shepr does not need it: its rule is that
client and server are always the same build, and its build id already
enforces that. Protocol generations are likewise unnecessary for shepr,
which has no wire compatibility obligations.

## Wire

- Unix stream socket in a 0700 runtime dir, mode 0600. Every connect is
  followed by a `SO_PEERCRED` uid check before any byte is written.
- 4-byte big-endian length plus JSON. The reader grows its buffer only on
  bytes actually received, not on the declared length. Decode errors are
  rendered without the offending value.
- One request and one response per connection; no separate handshake
  (ping is liveness, status carries identity). Streams use separate sockets.
- Every round trip has one deadline covering connect, peer check, write and
  read. Errors are typed (connect, untrusted, timed out, exchange) because
  each proves something different.
- No reconnection; every request is a fresh connection.

shepr's wire (length-prefixed positional codec, long-lived client
connections, build-id handshake) is already its own and is not in question;
the peer uid check and the single-deadline round trip are the parts worth
comparing.

## Lifecycle

- Idle exit after a grace period with no workers. Explicit shutdown request.
  SIGHUP, SIGINT and SIGTERM all mean "quiesce", installed at the top of boot,
  latched if they arrive during boot.
- One shared exit path stops accepting, drains in-flight connections under a
  budget so clients get their answers, unlinks the socket, closes the store.
- Persistent `accept()` failure exits on purpose so the next client respawns
  a healthy daemon.
- Runtime files (sockets, locks, boot log) in the runtime tree, normally
  tmpfs; the durable log in the state tree, opened only after ownership is
  proven.
- Configured by environment only, read once at boot.

## Testing

- An integration test runs the real client and daemon: first command
  spawns, second attaches to the same pid, shutdown works; missing binary is
  refused. It asserts both binaries share a cohort first, so a stale daemon
  binary fails loudly.
- A test starts two real daemons at once: exactly one survives, the loser
  exits with a named message, restart after death works.
- Unit tests cover the process-group kill, the drop guard, the bounded lock
  wait and readiness semantics.
- Gaps: no multi-client first-spawn race test, no real cross-build mismatch
  test.

## Weak spots

- A daemon older than the client is silently used by ordinary commands.
- `ba shutdown` with no daemon running can spawn one.
- A directly launched daemon holds `daemon.lock` for life, which is why
  clients must time out on it.
- Separate runtime and state trees cannot detect each other.
- Relies on same-directory install; installing only the client breaks it.

## Candidates for shepr

Worth taking:

- sibling-path lookup of `shepr-server` next to the resolved launch
  executable;
- flock rendezvous with bounded wait, re-probe under the lock, stale socket
  unlink, and a daemon-side lifetime flock taken before binding;
- a `--client-spawned` style argv marker;
- the drop guard that kills a half-started daemon, and `try_wait` in the
  readiness loop;
- boot stderr to a file, shown when startup fails;
- a shared exit-code enum so the client can explain a failed start;
- the private socket bind (stage, 0600, rename) and a peer uid check;
- no pid files;
- tests that run the real pair and race two daemons.

Not worth taking: the content-hash cohort, protocol generation numbers and
down-conversion, per-request connections.
