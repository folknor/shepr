# Defects: platform, remote machines and the root binary

Filed from the defect hunt over `crates/shepr-core`, `crates/shepr-platform`,
`crates/shepr-remote`, and the root `shepr` package (`src/main.rs`,
`src/cli.rs`, `src/cli/`, `src/preflight.rs`, `src/autodetect.rs`).

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## PLAT-001 - The server's client writer has no stall bound in production

Hunter's severity: High. Scope: core-platform.

**Claim broken.** `shepr_platform::write_client_stream` has a stall path: it
reports `TimedOut` "terminal observer stopped receiving output" and shuts the
stream down so the reader wakes. Its unit test
(`stalled_observer_write_times_out_when_the_injected_clock_passes_its_timeout`)
and the server test `observer_write_timeout_resets_when_sending_makes_progress`
both check it.

**What the code does.** `write_client_stream_with_clock` takes its stall budget
from `SO_SNDTIMEO` (`socket.write_timeout()`); with the option unset it falls
back to a plain blocking `write_all`. The one production caller is
`client_writer_loop`, through `write_framed_bytes` in
`shepr-server/src/server/client_transport.rs`, writing to a `try_clone` of the
handshake stream:

- `handle_client_handshake` puts that stream back into blocking mode with
  `set_nonblocking(false)`.
- Nothing in `shepr-server` sets a send timeout on it; the only
  `set_send_timeout` calls in `client_transport.rs` are in its tests.
- The accept loop in `client_accept.rs` does not set one either.

So the timeout branch never runs in production. The tests set the socket up in
a way production never does (nonblocking, with a send timeout).

**Consequence.** A client that stops reading (a TUI suspended with Ctrl-Z, a
wedged SSH bridge) keeps the writer thread blocked in `write_all` until the
kernel reports an error; the client is never reported disconnected. Render
frames coalesce in `ClientWriterQueueState.render`, but
`ClientWriterQueueState.control` is an unbounded `VecDeque`, so control messages
pile up in server memory for as long as the peer stays stalled. The comments in
`send_shutdown_to_unregistered_client` and `headless/lifecycle.rs` ("a writer
stuck on a client that stopped reading") treat the stuck writer as expected;
only the shutdown flush is bounded.

**Direction.** Make the stall timeout an explicit argument of
`write_client_stream` instead of a socket option it silently depends on, and let
the function own the nonblocking mode it needs, so the writer loop passes a named
limit and the tests exercise production's setup. Bounding the control queue, or
disconnecting past a depth, would stop the memory growth even if the socket
bound is lost again.

## PLAT-002 - The logind monitor requests its delay lock before checking for a pending shutdown, which logind refuses

Hunter's severity: Medium. Scope: core-platform.

**Claims broken.** `watch_connection` in `shutdown.rs`: "Taken before the state
is read: logind honours a delay lock taken while it is already waiting on
others, so a warning that is pending at (re)connect is still held up until the
server has checkpointed." The module and `Shared::refresh_warning` also promise
that a warning still pending after a reconnect is refreshed.

**What the code does.** `watch_connection` calls `take_inhibitor(&manager).await?`
before reading `PreparingForShutdown`. systemd-logind's `method_inhibit` refuses
a delay lock when the delayed action it would cover is already running, with
`org.freedesktop.login1.OperationInProgress` ("The operation inhibition has been
requested for is already running"). The `?` turns that into an `Err`, `monitor`
reads it as "logind unavailable" and retries, and every retry fails the same way
until the host goes down. `announce` and `refresh_warning` are never reached
against a real logind, so a server that connects during a shutdown (fresh start,
system bus restart) never checkpoints. The ignored D-Bus test cannot catch it:
its fake `LoginManager::inhibit` always grants the lock.

The hunter notes this rests on systemd's `logind-dbus.c` behaviour as they know
it, and should be checked against the installed systemd version.

**Direction.** Read `PreparingForShutdown` first. When a shutdown is already
pending, announce it without a lock and treat `OperationInProgress` as "no lock
available", not a connection failure. Make the fake manager refuse `Inhibit`
while `preparing` is true so the ordering is tested.

## PLAT-003 - `shell::resolve_executable` searches `PATH` for relative names that contain a slash

Hunter's severity: Low-Medium. Scope: core-platform.

**Claim broken.** The name and doc ("Resolve a program path using the pane's
`PATH` and working directory") imply exec-style lookup, and config validation
relies on it: `resolve_recognized_shell` in `shepr-config/src/validated.rs`
resolves `terminal.default_shell` and `SHELL` with it, as does PTY launch
(`shepr-pty/src/command.rs`, `search_path`).

**What the code does.** Any relative program whose first component is not `.` or
`..` goes to the `PATH` search, so `bin/zsh` resolves to `<PATH entry>/bin/zsh`.
`execvp` and every shell treat a name containing a slash as relative to the
working directory and never search `PATH` for it. For `SHELL=bin/zsh` or
`terminal.default_shell = "bin/zsh"`, validation either accepts a different
binary from the one the name means, or refuses a shell that exists at
`cwd/bin/zsh`.

**Direction.** Search `PATH` only when the program has no `/`; otherwise join it
to `cwd`. `shepr-pty` has its own copy of `is_cwd_relative_path` and suppresses
`PATH` for cwd-relative names, which `resolve_executable` already does; drop the
copy.

## PLAT-004 - `connect_trusted_local_stream` trusts a listener owned by root

Hunter's severity: Low. Scope: core-platform.

**Claim broken.** "Connects to a server socket and checks its owner before
returning the stream, so no request or attach byte is ever written to a socket
served by another user."

**What the code does.** It uses `peer_is_same_user`, which also returns `true`
for uid 0. The stated reason for admitting root ("root can connect through the
0600 mode anyway, and refusing it would only break `sudo`") applies to the
accept side, not to a client trusting a root-owned server. Either split the
check (accept side admits root, connect side requires the same euid) or reword
the doc.

## PLAT-005 - Cleanup sweeps scan the whole `$XDG_RUNTIME_DIR`, which shepr does not own

Hunter's severity: Low. Scope: core-platform.

`machine_bridge_path` and `shared_ssh_control_path` get
`AppPaths::xdg_runtime_dir()`, the raw `/run/user/<uid>`, so three sweeps scan
the user's shared runtime directory:

- `sweep_abandoned_single_use_sockets` opens and reads every regular file there
  ending in `.lock`, from any program, before the tag parse rejects it.
- `sweep_stale_socket_staging_dirs`, called by `bind_private_local_listener`
  with the socket's parent directory, scans for `.s<16 hex>` directories.
- `sweep_stale_remote_ssh_config_dirs` scans for `shepr-ssh-*` directories.

The strict tag format keeps the chance of deleting foreign files small, but
shepr reads other programs' lock files and relies on name patterns in a shared
directory. `validate_local_setup` also runs a full sweep, and uses up a random
token, only to check that a path would fit.

**Direction.** Put bridge sockets, staging and SSH config directories in a
shepr-owned subdirectory (the per-profile runtime directory already exists), so
the sweeps only see shepr's own files. The socket-path budget allows it: the
staged path is short, and `/run/user/<uid>/shepr/` adds only a few bytes.

## PLAT-006 - `write_config_temporary` reopens the temporary file by path

Hunter's severity: Low. Scope: core-platform.

In `atomic_replace.rs`, `AtomicReplace::prepare_with_policy` creates the
temporary with `create_new` (via `create_private_file` or
`create_config_temporary`). `PermissionPolicy::write` then drops that handle and
calls `shepr_platform::write_config_temporary(existing, temporary, contents)`,
which reopens the path with `O_TRUNC` and without `O_NOFOLLOW`, then chowns,
copies ACLs, chmods and writes secrets through the new descriptor. The handle
that proved the file fresh has been thrown away; anyone who can write to the
agent's config directory could swap in a symlink between the two opens and have
the contents written (and truncated) elsewhere. `File::open(source)` also
follows a symlink when reading the metadata to copy.

**Direction.** `write_config_temporary` should take the `File` returned by the
create call, not a path; the platform API forces the reopen as it stands.

## PLAT-007 - The Git runner's deadline stops at the child's exit; relative cwd doubles; stale module doc

Hunter's severity: Low. Scope: core-platform.

- **Deadline.** `git.rs` promises "a short read-only probe with a deadline". The
  deadline covers only `child.try_wait()`. After a normal exit, `join_drain`
  waits unbounded for the drain threads to reach EOF; if Git left a descendant
  holding stdout or stderr, the probe outlives its deadline. On timeout the
  drain threads are deliberately leaked instead, as the code says.
- **Relative `cwd`.** The runner sets `current_dir(cwd)` and also passes
  `-C cwd`, so with a relative `cwd` Git runs in `cwd/cwd`. Every current caller
  passes an absolute path; latent.
- **Stale doc.** The module doc says "The server's Git status and the client's
  workspace label both go through `run_git`". The client never runs Git; it asks
  the server through `EndpointCommand::WorkspaceCheckoutRoot`, and the server
  runs `run_git` in `app/api/checkout_root.rs`.

## PLAT-008 - `read_git_config_parameters` accepts empty keys that Git rejects

Hunter's severity: Low. Scope: core-platform.

The doc says the reader follows Git, rejecting malformed counts and missing
pairs. Git's `config_parse_pair` also rejects an empty indexed key
(`GIT_CONFIG_KEY_<n>=`) with "empty config key"; here the key is pushed as `""`.
The empty-key check in `parse_git_config_parameters` only covers the quoted
form.

## PLAT-009 - `connect_local_stream_within` can panic on a large timeout

Hunter's severity: Low, latent. Scope: core-platform.

`let deadline = Instant::now() + timeout;` panics if the addition overflows.
`shepr_api::client::request_value_with_timeout` guards the same timeout with
`checked_add` in `deadline_after`, but only after it calls
`self.connect(timeout)`, so the guard comes too late. Every current timeout is a
constant. Use `checked_add` here and return `InvalidInput`, as the API does.

## PLAT-010 - Smaller doc and signature mismatches in core and platform

Hunter's severity: Low. Scope: core-platform.

- `host::hostname` claims to return the name "as shown by tmux's `#h`". tmux's
  `#h` strips the domain (`#H` is the full name); this returns the full node
  name, so an FQDN hostname shows its domain in `{hostname}` window titles.
- `read_boot_log_tail` says it is "Opened as [`open_boot_log`] does", but skips
  the owner check `open_boot_log` applies.
- `SpawnedDaemon::try_wait` is documented as "Whether the daemon has exited".
  After reporting the exit once, it returns `Ok(None)` ("not exited") on every
  later call. The one caller (`local_server.rs`) caches the first exit, so this
  is latent. A sticky `Option<ExitStatus>` field or a separate `exited()` method
  would say what it means.
- `env::resolve_text` documents panics for "a flag, presence or handoff
  variable"; it also panics for `Raw` (`SHELL`, `PATH`, `GIT_CONFIG_*`).
  `read_os` and `resolve_os` have no `# Errors` section.

## PLAT-011 - Local and bridge connects ignore the per-attempt deadline

Hunter's severity: not given (lateral). Scope: core-platform.

`connect_once` in `shepr-client/src/endpoint/supervisor.rs` takes a per-attempt
`deadline` but connects to Local with `connect_trusted_local_stream`, which has a
fixed 5-second budget. `MachineSshConnector::attempt` does the same for the
bridge socket. `connect_trusted_local_stream_within(path, remaining)` exists and
would honour the deadline.

## PLAT-012 - Rotating log writer does a syscall storm per event and can duplicate partial lines

Hunter's severity: not given (lateral). Scope: core-platform.

Every write through `RotatingFileGuard` opens the log, takes `flock`, runs
`stat` on both the path and the descriptor, writes and closes. Fine at
`shepr=info`; with `SHEPR_LOG=debug` on the PTY or render paths the cost
multiplies with events, panes and clients. A cached descriptor, checked against
the path's inode only every N writes or on rotation, would keep
rotation-following without the storm. A failed `write_all` is retried whole, so a
partial first write duplicates part of a line.

## PLAT-013 - Data and metadata directories are created with the umask mode

Hunter's severity: Low (lateral). Scopes: core-platform, remote.

`RotatingFileMakeWriter::new`, `persist/io.rs` and `persist/writer.rs` all create
the data directory with plain `create_dir_all`. The log comment says the files
are "private to the user like the rest of the data directory's state". The files
are 0600, but the directory holding history is world-listable under a 022 umask.
The remote hunter found the same for the SSH metadata directory created by
`store_private_json` (`crates/shepr-remote/src/machine/ssh_metadata.rs`), unlike
the private file inside it.

## PLAT-014 - `shepr-platform` holds higher-level policy its own doc says belongs elsewhere

Hunter's severity: not given (lateral). Scope: core-platform.

`lib.rs` says "higher-level rules belong to the crates that consume these
primitives". The crate nonetheless holds the logind inhibitor protocol and its
warning-generation policy, clipboard helper selection, Git environment policy,
and multi-process log rotation. It also makes the `shepr` client binary depend
on `zbus` and `tokio` for a monitor only the server runs. Moving the shutdown
monitor to `shepr-server` and the Git runner to `shepr-mux` would match the doc.

## PLAT-015 - Unrecognised ssh exit 255 is filed as "the machine did not answer", hiding authentication and config failures at startup

Hunter's severity: highest in its report. Scope: remote.

Where: `SshFailureDiagnostic::from_ssh_output` and `classify_ssh_diagnostic`
(`crates/shepr-remote/src/lib.rs`), `classify_check`
(`crates/shepr-remote/src/remote/preflight.rs`), `result_notices`
(`src/preflight.rs`).

`from_ssh_output` maps exit 255 to `HostKey` or `Authentication` only when
stderr matches a few narrow signatures, everything else to `Link`.
`classify_check` files `Link` as `MachineCheck::Offline`, and `result_notices`
deliberately prints nothing for `Offline` ("Offline machines are left to the
client").

**Claims broken.** `MachineCheck::Offline`: "The machine did not answer:
timeout, refusal, no route". `is_ssh_link_failure`: "the remote end was never
reached or was lost". `result_notices`: "everything the operator can act on is
printed". AGENTS.md: shepr "runs interactive ssh for each one that needs
authentication".

Inputs that land in `Offline` with no notice and no prompt:

- `Received disconnect from H port 22:2: Too many authentication failures` (an
  ssh-agent holding more keys than the server's `MaxAuthTries`). An auth failure
  a prompt with the right key or `IdentitiesOnly` would fix; instead the machine
  is silently offline and shows Reconnecting forever.
- `ssh: Could not resolve hostname typo.example` (a typo in
  `[[machines]].ssh`).
- `/home/u/.ssh/config: line 12: Bad configuration option: ...` or
  `Bad owner or permissions on /home/u/.ssh/config`. The managed config
  `Include`s the user config, so a broken user config fails every command with
  exit 255 before anything is reached.
- `Connection closed by H port 22` after a server-side refusal (fail2ban,
  `AllowUsers`).

Related naming defect: `SshFailureDiagnostic::is_link_failure` returns true for
Authentication and HostKey diagnostics too, falling back to "origin was ssh exit
255", so an auth refusal (remote reached) answers "link failure".
`classify_check` only gets it right because it tests `requires_authentication`
and `is_host_key` first; the connector and discovery rely on the true answer
(they keep hints on an auth failure). The name and doc say something other than
what those callers need.

**Direction (structural).** Stop letting one boolean mean two things. Classify
once at the ssh boundary into what was learned (nothing about the remote: could
not reach, auth refused, host key refused, local ssh config broken; versus the
remote said something) and separately into who can fix it (transient network,
operator at the terminal, operator editing config). Preflight silence should key
on transient network only, and an unmatched exit 255 should default to a
reported class, not a hidden one. See also PLAT-024.

## PLAT-016 - Discovery swallows why a status probe failed and tells the operator to reinstall

Scope: remote.

Where: `remote_client_status` (`crates/shepr-remote/src/remote/discovery.rs`),
reached from `SshDiscovery::matches`, `verify_remote_shepr` and the connector.

When the remote `shepr status client --json` exits non-zero and ssh itself did
not fail, the function returns `Ok(None)` and drops stderr; the candidate counts
as "not a match". When every candidate drops this way, discovery ends with
`matching Shepr is not ready on H; install or update it there manually and
retry`.

Across the boundary: on the remote, `status client` goes through `cli::run`,
which calls `resolve_app_paths()` first (`src/cli.rs`). `AppPaths::resolve` fails
when `XDG_RUNTIME_DIR` is unset (`crates/shepr-config/src/io.rs`), as in ssh
sessions on hosts without pam_systemd or logind. `status client` needs no paths
(`client_status_json` reads none). So a correctly installed, correct-build shepr
on such a host is reported "not ready; install or update it", and the real
message (`application paths could not be resolved: XDG_RUNTIME_DIR must be
set...`) is discarded. The same holds for any remote probe failure other than
`test -x` failing. On the cached path, `resolve_remote_shepr` treats that
`Ok(false)` as "gone" and invalidates the metadata cache before rediscovering,
throwing the good hint away.

**Claims broken.** `verify_remote_shepr` documents "`Ok(false)` means it is
gone"; it also means "it ran and failed for any reason". The not-ready message
misdiagnoses.

**Direction.** Keep `Ok(None)` for the `test -x` leg (not there or not
executable) and return the `command_failed` diagnostic for a probe that ran and
failed. Separately, in the root binary, `status client` should not resolve
`AppPaths`.

## PLAT-017 - The "stop it yourself" hint for a remote server runs a bare `shepr` that may not be on the remote PATH

Hunter's severity: Medium (termio-root). Scopes: remote, termio-root.

Where: `remote_stop_command` in `src/preflight.rs`, used by `restart_notice` for
Declined, NoTerminal and Failed.

The hint is `ssh <target> shepr server stop`, which runs `shepr` through the
remote user's non-interactive shell. Discovery exists because that PATH often
lacks `~/.cargo/bin` and `~/.local/bin` (see the doc on
`known_remote_binary_candidate_script`: "which misses them when a
non-interactive SSH shell has a minimal PATH"; `discovery.rs` tries a
login-shell `command -v`, then `/bin/sh`, then known locations). On such a host
the printed command fails with `shepr: command not found`.

Every `restart_notice` branch that prints `left_running` has
`MachineCheck::DifferentBuild(server)` carrying the verified absolute
`server.executable`, and `stop_remote_server` itself runs exactly that path
(`server.executable.command(&args)`). The notice should render the same path,
quoted with `shell_quote`.

**Claim broken.** AGENTS.md: "on refusal, the server is left running and
unavailable and shepr says how to stop it". The notice text itself says "To
restart it, run `...`".

## PLAT-018 - The connector discards the remembered attempt's error and hint, and non-install failures pay full rediscovery on every retry

Scope: remote.

Where: `MachineSshConnector::connect`
(`crates/shepr-remote/src/remote/machine_ssh.rs`).

When the attempt with the remembered executable fails for any non-link reason:
the error is logged at debug and dropped, `remote_shepr` is set to `None`, and
full discovery runs within the same deadline, followed by a second bridge and
handshake.

- **Masked diagnosis, wrong status class.** A remembered attempt that should
  show Attention (for example a handshake `PreambleError::DifferentBuild` from a
  remote server the operator declined to restart) followed by a rediscovery that
  runs out of budget reports `SSH connection attempt ran out of time` (TimedOut,
  so link, so Reconnecting). Likely when the first attempt spent its budget
  waiting on a slow remote server boot inside `run_remote_client_bridge`.
- **Good hint lost.** Discovery ends on a link failure, `remote_shepr` stays
  `None`, and the next attempt starts from full discovery again.
- **Unbounded cost.** For failures unrelated to the install (a different-build
  server left running, a remote launch failure, the remote idle watchdog), every
  retry forever costs one bridge launch, the login-shell probe, the `/bin/sh`
  probe, the known-locations script, one status probe per candidate, and a
  second bridge launch. The struct doc promises "a moved, removed or upgraded
  remote install costs one extra bridge launch".
- `handshake_error` maps a close before Welcome with no bridge report to
  `UnexpectedEof`, which classifies as `Other`, so a dropped link the bridge
  thread had not reported within `BRIDGE_FAILURE_REPORT_TIMEOUT` also triggers
  rediscovery.

**Direction.** Rediscover only when the failure says something about the
executable (the bridge's remote command failed to exec, or the preamble says the
remote client is the wrong build), not for handshake-level server mismatches or
remote launch errors. When rediscovery runs and fails, return the more specific
of the two errors, preferring the remembered attempt's attention-class error
over a later timeout.

## PLAT-019 - User keepalive settings are overridden despite the comment that says they are preserved

Scope: remote.

Where: `write_managed_ssh_config` and `apply_batch_ssh_options`
(`crates/shepr-remote/src/remote/ssh.rs`).

The comment on `write_managed_ssh_config` says the user config is included first
"so OpenSSH's first-value-wins behavior preserves explicit user keepalives". But
`apply_batch_ssh_options` passes `-o ServerAliveInterval=15 -o
ServerAliveCountMax=4` on the command line to every BatchMode command and every
bridge, and command-line options beat every config file. If a BatchMode check or
bridge creates the ControlPersist master (the normal case when no prompt was
needed), the master carries shepr's keepalive for its whole life. `ssh_tests.rs`
asserts both halves, locking the contradiction in.

**Claims broken.** The doc comment on `write_managed_ssh_config`, and the
`limits` doc "OpenSSH keepalive settings shared by command arguments and managed
config". Either drop the command-line keepalive and rely on the config's
`Host *` block, or drop the comment.

## PLAT-020 - `sh_output_within` lets a stdin write error override ssh's typed failure

Scope: remote. Latent.

Where: `RemoteSsh::sh_output_within` (`ssh.rs`).

The order is `write_all(script)`, `wait_with_output_timeout`, `write_result?`,
normalize. If ssh exits before reading stdin (auth refused, host key refused,
connection refused), `write_all` can fail with `BrokenPipe`, and the function
returns that bare `BrokenPipe`, discarding the `Output` with exit 255 and the
classified stderr. The result classifies as `Other` (`MachineCheck::Failed`), not
`NeedsAuthentication`, `HostKey` or `Offline`. The script is small and written
microseconds after spawn, so the pipe buffer normally absorbs it. The precedence
is still backwards: the child's output should win, and the write error should
matter only when the command otherwise succeeded.

## PLAT-021 - Discovery stops at the first wrong-build candidate instead of probing the rest

Scope: remote.

Where: `SshDiscovery::matches` and `DiscoveryProgress::run_remaining`
(`discovery.rs`).

`matches` returns `Err` for a candidate of another build (and for a bad
sibling), and `run_remaining` propagates it with `?`, so later candidates are
never probed. The `DiscoveryProgress` doc says discovery runs "a status probe per
candidate until one matches". Under the one-install-per-host rule this rarely
bites, but it does when a PATH entry is a shim or wrapper resolving differently
from the real install (mise shims are explicitly supported). Cheap fix: remember
the first mismatch error, keep probing, and return it only when no candidate
matches; or change the doc.

## PLAT-022 - A remote server started by the bridge lives in the ssh session's cgroup

Scope: remote (lateral, unverified on a host).

`run_remote_client_bridge` calls `local_server::ensure_running`, then
`build_server_daemon_command` and `detach_server_daemon_command` (`setsid` only).
The daemon is detached by session id but stays in the ssh login session's
systemd scope. On a host with logind `KillUserProcesses=yes`, systemd kills the
scope when that ssh session ends (when the bridge disconnects), taking the remote
server and every pane with it, against the promise that workspaces and panes
live in a headless server that outlives clients. Depends on the host's logind
config. Fix if it matters: start the daemon in its own transient user scope
(`systemd-run --user --scope` or the D-Bus equivalent) when a user manager is
available.

## PLAT-023 - `launch_with` can blame and kill its own daemon for another server

Scope: remote (lateral). Rare.

Where: `launch_with` (`crates/shepr-remote/src/remote/local_server.rs`).

When the probe answers with a different build while our daemon has not exited,
the launch returns `sibling_build_mismatch` ("... is a different build than this
shepr and was stopped") and the guard kills our daemon. The launch lock only
orders clients; a `shepr-server` started directly (for example
`brokkr run shepr-server`) takes no launch lock and can bind between our second
probe and our daemon's bind. The answering server is then not ours: the message
blames the installed pair wrongly, and a daemon that was about to exit
`AlreadyRunning` is killed. Checking the answering server's pid or boot against
the spawned child would close it.

## PLAT-024 - The connector never rebuilds its managed ssh config once built

Hunter's severity: Low. Scope: remote.

Where: `ConnectorState::ssh` in `machine_ssh.rs`.

`RemoteSsh` and its temporary config directory under `XDG_RUNTIME_DIR` are built
once and kept for the client's life. If the directory disappears (logind removes
`/run/user/<uid>` after the last login session ends while the TUI keeps running
under tmux, or anything else deletes it), every later ssh fails with `Can't open
user config file` and exit 255, which per PLAT-015 classifies as a link failure:
the endpoint shows Reconnecting forever with no recovery. The code retries a
transient setup failure, but not a config that vanished after setup. Cheap
guard: check `config_path` exists before each attempt and rebuild `RemoteSsh`
when it does not.

## PLAT-025 - `SshMetadataCache::store` reports failure after it has succeeded

Hunter's severity: Low. Scope: remote.

Where: `store_private_json` (`crates/shepr-remote/src/machine/ssh_metadata.rs`).

The rename has already replaced the cache file when `sync_directory(parent)`
runs. If that fsync fails, `store` returns `Err`, and callers log "could not
cache SSH machine metadata; later connections rediscover the remote shepr",
which is false: the entry is there and will be used. (The directory-mode half of
this finding is filed under PLAT-013.)

## PLAT-026 - Build-mismatch and not-running guidance is profile-blind and can stop the wrong server

Hunter's severity: High. Scope: termio-root.

**Claims broken.** AGENTS.md: dev and release builds use separate runtime and
data directories, and "a dev and a release build never talk to each other's
server"; "on refusal, the server is left running and unavailable and shepr says
how to stop it".

**Path.** `ServerAddress::stop_command` and `attach_command`
(`crates/shepr-config/src/address.rs`) always spell the command as `shepr` or
`shepr server stop`, prefixed only with a socket override. The root binary
passes this text straight to the operator from:

- `autodetect.rs` through `local_server::ensure_running` and
  `running_build_mismatch`;
- `cli.rs` `ensure_server_build_matches` (`target::restart_guidance`);
- `cli/server_not_running.rs` (`attach_command`);
- `unresponsive_error` in `crates/shepr-remote/src/remote/local_server.rs`.

**Scenario.** A dev build (`brokkr run`) meets an older dev server and is told
"Run `shepr server stop`, then run `shepr` again". On a host with shepr
installed, `shepr` on `PATH` is the release binary: it resolves the release
runtime directory and stops the release server, ending every pane and agent the
operator actually works in, while the dev server is untouched. The follow-up
`shepr` attaches the release TUI. The not-running message has the same problem
for a dev CLI command ("run `shepr`" starts a release server).

**Direction.** The guidance must name this profile's own entry point: `shepr`
for release; for dev, `brokkr run -- server stop` or the absolute path of the
running executable, which `launch_executable` already resolves. The address type
knows sockets, not which binary resolves to them; move the command rendering to
the root binary, which knows its profile and path.

## PLAT-027 - The dev TUI cannot be launched from a release pane, contrary to AGENTS.md

Hunter's severity: Medium. Scope: termio-root.

**Claim broken.** AGENTS.md: "Run it with plain `brokkr run -- [<command>]`,
including from inside a pane of the installed server." With no command that is
the TUI.

**Path.** `main.rs` `refuse_if_nested_disabled` blocks the TUI and `client`
launches whenever `SHEPR_ENV == SHEPR_ENV_IN_PANE` and
`experimental.allow_nested` is false (the default). It ignores
`SHEPR_BUILD_PROFILE`. A release pane exports both, so the dev TUI launched from
it fails with "nested shepr is disabled by default". The profile marker already
decides that a dev process in a release pane is not talking to that pane's
server (the socket overrides are dropped); the nesting guard does not use the
same fact. Either pass the guard when the marker's profile differs from
`BuildProfile::current()`, or have AGENTS.md say the dev TUI needs
`allow_nested`.

## PLAT-028 - `matches::flag` and `matches::string` turn a spec/handler mismatch into a valid-looking value

Hunter's severity: Low, latent. Scope: termio-root.

**Claim broken.** Module doc of `src/cli/matches.rs`: "these helpers use clap's
non-panicking lookups so a spec/handler mismatch is rejected rather than turned
into a valid-looking empty value".

`flag` maps a lookup error to `false`; `string` maps it to `None`. For
`server stop`, a misnamed id would silently turn `--expect-boot X` into an
unconditional stop (`expected_boot: None`), the exact race the flag exists to
close. Today the ids derive from the same constant (`option_name_from_flag`) and
`server_stop_parses_the_expected_boot` covers it. Either return `Result` and
fail the parse (exit 2), or fix the doc.

## PLAT-029 - Preflight runs before the terminal usability check it depends on

Hunter's severity: Low. Scope: termio-root.

**Claim.** `autodetect.rs`: "The client requires terminal geometry before it can
attach. Reject an unusable terminal before socket lookup creates directories or
starts a daemon."

`main.rs` calls `preflight::run` before `auto_detect_launch`, so before the
terminal check a launch can probe the local socket, run the full SSH check round
against every machine (up to `PREFLIGHT_CHECK_BUDGET`), prompt for
authentication, and with consent stop the local server (`restart_local`, with
`can_prompt` needing only stdin and stderr to be terminals). Only then does
`terminal_grid_size` reject the terminal. Move the grid-size check ahead of
`preflight::run`, or into it.

## PLAT-030 - The restart offer does not say it can end the launching terminal

Hunter's severity: Low. Scope: termio-root.

**Claim.** AGENTS.md: the offer "says that the restart ends the server's pane
processes".

With `allow_nested` and a same-profile server whose pane runs this `shepr`,
`restart_local` offers to stop the server that owns the launching shell, which
ends the process asking the question. The text ("ends every pane process it
hosts") is literally true but does not tell the operator it includes the
terminal they are typing in. `SHEPR_ENV` and a matching `SHEPR_BUILD_PROFILE`
are enough to detect this and either refuse or say so.

## PLAT-031 - The cross-build status and stop surface is implicit

Scopes: termio-root and protocol-api (both filed it as a structural note, not a
defect in itself).

The preflight restart offer, `status` and `server stop --expect-boot` exist to
talk to a server of another build, over the JSON `ping` and `server.stop`
requests: `read_runtime_status_at` deserializes the full `SuccessResponse` /
`ResponseResult` enum, and the boot guard lives only in the receiving server's
handling of `expected_boot_id`. AGENTS.md says there are no wire compatibility
obligations, but these shapes are the exception the restart flow depends on. If
a future `Pong` shape changes, `local_server_status` logs a warning and returns
`None`, the offer silently disappears, and the launch fails with "did not give a
usable status answer" with no stop guidance. A tiny, explicitly frozen
identity/stop envelope (its own type, its own test fixture) would make the
dependency visible.

The protocol-api hunter adds the sharper half. The preamble has a fixed layout
so that "any two builds can read each other's preamble", but the restart flow
also relies on JSON between different builds: `ping`'s
`Pong { version, build_id, boot_id }` and `server.stop`'s `expected_boot_id`
param. The conditional stop is only safe if the other build understands the
parameter. `ServerStopParams` is a plain serde struct and serde ignores unknown
fields, so a build that did not know `expected_boot_id` (renamed, removed) would
stop unconditionally: exactly the "stop the replacement" outcome the flag exists
to prevent. Latent, since every build today has the field. That hunter proposes a
separate method name for the conditional stop (an unaware server answers
`invalid_request` and stops nothing), plus a frozen test fixture for `ping` and
`server.stop` as the one cross-build JSON surface. See also WIRE-024 on the
client status JSON.

## PLAT-032 - CLI subcommands never load or validate config

Scope: termio-root (structural note).

`status`, `server stop` and `detect` resolve paths only. Probably intended (a
broken config must not block `server stop`), but AGENTS.md says "Config is read
and validated once at launch ... Any config problem fails the launch" without
carving out the CLI. The doc or the behaviour should say which.
