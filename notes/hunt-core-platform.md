# Defect hunt: shepr-core and shepr-platform

Scope: `crates/shepr-core` and `crates/shepr-platform`, with values followed into
their callers where the bug sits on the boundary. I read every source file in both
crates. Findings are ordered by severity. Each one names the claim it breaks.

## 1. The server's client writer has no stall bound in production (High)

**Claim broken.** `shepr_platform::write_client_stream` has a stall path: it
reports `TimedOut` "terminal observer stopped receiving output" and shuts the
stream down so the reader wakes. Its unit test
(`stalled_observer_write_times_out_when_the_injected_clock_passes_its_timeout`)
and the server test `observer_write_timeout_resets_when_sending_makes_progress`
both check this behaviour.

**What the code does.** `write_client_stream_with_clock` gets its stall budget
from `SO_SNDTIMEO` (`socket.write_timeout()`). When that option is unset it
falls back to a plain blocking `write_all`. The one production caller is
`client_writer_loop`, through `write_framed_bytes` in
`shepr-server/src/server/client_transport.rs`. It writes to a `try_clone` of the
handshake stream:

- `handle_client_handshake` puts that stream back into blocking mode with
  `set_nonblocking(false)`.
- Nothing in `shepr-server` sets a send timeout on it. The only
  `set_send_timeout` calls in `client_transport.rs` are in its tests.
- The accept loop in `client_accept.rs` does not set one either.

So in production the timeout branch never runs. The tests set up the socket in
a way production never does (nonblocking, with a send timeout).

**Consequence.** A client that stops reading keeps the writer thread blocked in
`write_all` until the kernel reports an error. Examples: a TUI suspended with
Ctrl-Z, or a wedged SSH bridge. The client is never reported disconnected.
Render frames coalesce in `ClientWriterQueueState.render`, but
`ClientWriterQueueState.control` is an unbounded `VecDeque`, so control messages
pile up in server memory for as long as the peer stays stalled. The comments in
`send_shutdown_to_unregistered_client` and in `headless/lifecycle.rs` ("a
writer stuck on a client that stopped reading") treat the stuck writer as
expected. Only the shutdown flush is bounded.

**Direction.** Stop passing the stall budget through a socket option that the
function silently depends on. Make the stall timeout an explicit argument of
`write_client_stream`, and make the function own the nonblocking mode it needs.
Then the writer loop passes a named limit, and the tests exercise the same setup
production uses. Bounding the control queue, or disconnecting past a depth,
would stop the memory growth even if the socket bound is lost again.

## 2. The logind monitor asks for its delay lock before checking for a pending shutdown, which logind refuses (Medium)

**Claim broken.** Two claims:

- `watch_connection` in `shutdown.rs` says: "Taken before the state is read:
  logind honours a delay lock taken while it is already waiting on others, so a
  warning that is pending at (re)connect is still held up until the server has
  checkpointed."
- The module and `Shared::refresh_warning` promise that a warning still pending
  after a reconnect is refreshed.

**What the code does.** `watch_connection` calls `take_inhibitor(&manager).await?`
before it reads `PreparingForShutdown`. systemd-logind's `method_inhibit`
refuses a delay lock when the delayed action it would cover is already running.
It returns `org.freedesktop.login1.OperationInProgress` ("The operation
inhibition has been requested for is already running"). The `?` turns that
into an `Err`, and `monitor` reads it as "logind unavailable" and retries. Every
retry fails the same way until the host goes down. The two branches that would
act on the pending warning, `announce` and `refresh_warning`, are never reached
against a real logind. That means a server that connects during a shutdown
(fresh start, or a system bus restart) never checkpoints. The ignored D-Bus test
cannot catch this: its fake `LoginManager::inhibit` always grants the lock.

This finding rests on systemd's `logind-dbus.c` behaviour as I know it. Check it
against the installed systemd version before acting on it.

**Direction.** Read `PreparingForShutdown` first. When a shutdown is already
pending, announce it without a lock and treat `OperationInProgress` as "no lock
available", not as a connection failure. Make the fake manager refuse `Inhibit`
while `preparing` is true, so the ordering is tested.

## 3. `shell::resolve_executable` searches `PATH` for relative names that contain a slash (Low-Medium)

**Claim broken.** The function name and its doc ("Resolve a program path using
the pane's `PATH` and working directory") imply exec-style lookup. Config
validation relies on that: `resolve_recognized_shell` in
`shepr-config/src/validated.rs` resolves `terminal.default_shell` and `SHELL`
with it, and so does PTY launch (`shepr-pty/src/command.rs`, `search_path`).

**What the code does.** Any relative program whose first component is not `.`
or `..` goes to the `PATH` search. So `bin/zsh` resolves to
`<PATH entry>/bin/zsh`. `execvp` and every shell treat a name that contains a
slash as a path relative to the working directory and never search `PATH` for
it. For `SHELL=bin/zsh` or `terminal.default_shell = "bin/zsh"`, validation
either accepts a different binary from the one the name means, or refuses a
shell that exists at `cwd/bin/zsh`.

**Direction.** Search `PATH` only when the program has no `/`. Otherwise join it
to `cwd`. `shepr-pty` also has its own copy of `is_cwd_relative_path` and
suppresses `PATH` for cwd-relative names, which `resolve_executable` already
does. Drop the copy.

## 4. `connect_trusted_local_stream` trusts a listener owned by root (Low)

**Claim broken.** "Connects to a server socket and checks its owner before
returning the stream, so no request or attach byte is ever written to a socket
served by another user."

**What the code does.** It uses `peer_is_same_user`, which also returns `true`
for uid 0. The reason given for admitting root applies to the accept side:
"root can connect through the 0600 mode anyway, and refusing it would only break
`sudo`". It does not carry over to the client trusting a root-owned server. As
written, the doc and the behaviour disagree. Either split the check (accept
side admits root, connect side requires the same euid) or reword the doc.

## 5. The cleanup sweeps run over the whole `$XDG_RUNTIME_DIR`, which shepr does not own (Low)

`machine_bridge_path` and `shared_ssh_control_path` get
`AppPaths::xdg_runtime_dir()`, the raw `/run/user/<uid>`. Three sweeps
therefore scan the user's shared runtime directory:

- `sweep_abandoned_single_use_sockets` opens and reads every regular file there
  whose name ends in `.lock`, from any program, before the tag parse rejects it.
- `sweep_stale_socket_staging_dirs`, called by `bind_private_local_listener`
  with the socket's parent directory, scans for `.s<16 hex>` directories.
- `sweep_stale_remote_ssh_config_dirs` scans for `shepr-ssh-*` directories.

The strict tag format keeps the chance of deleting foreign files small. Still,
shepr reads other programs' lock files and relies on name patterns in a
directory it shares with them. `validate_local_setup` also runs a full sweep,
and uses up a random token, only to check that a path would fit.

**Direction.** Put bridge sockets, staging and SSH config directories in a
shepr-owned subdirectory. The per-profile runtime directory already exists, so
the sweeps only ever see shepr's own files. The socket-path budget allows it:
the staged path is short, and `/run/user/<uid>/shepr/` adds only a few bytes.

## 6. `write_config_temporary` reopens the temporary file by path (Low)

In `atomic_replace.rs`, `AtomicReplace::prepare_with_policy` creates the
temporary with `create_new` (via `create_private_file` or
`create_config_temporary`). `PermissionPolicy::write` then drops that handle
and calls `shepr_platform::write_config_temporary(existing, temporary, contents)`.
That call reopens the path with `O_TRUNC` and without `O_NOFOLLOW`, then chowns,
copies ACLs, chmods and writes secrets through the new descriptor. The
exclusive create is what proved the file is fresh, but the handle that proved
it has been thrown away. Anyone who can write to the agent's config directory
could swap in a symlink between the two opens and have the contents written
(and truncated) elsewhere. `File::open(source)` also follows a symlink when it
reads the metadata to copy.

**Direction.** `write_config_temporary` should take the `File` returned by the
create call, not a path. The platform API forces the reopen as it stands.

## 7. The Git runner's deadline stops at the child's exit, and some of its docs are wrong (Low)

- **Deadline.** `git.rs` promises "a short read-only probe with a deadline".
  The deadline covers only `child.try_wait()`. After a normal exit, `join_drain`
  waits with no bound for the drain threads to reach EOF. If Git left a
  descendant holding stdout or stderr, the probe outlives its deadline. On
  timeout the drain threads are deliberately leaked instead, as the code says.
- **Relative `cwd`.** The runner sets `current_dir(cwd)` and also passes
  `-C cwd`. With a relative `cwd`, Git runs in `cwd/cwd`. Every current caller
  passes an absolute path, so this is latent.
- **Stale doc.** The module doc says "The server's Git status and the client's
  workspace label both go through `run_git`". The client never runs Git. It
  asks the server through `EndpointCommand::WorkspaceCheckoutRoot`, and the
  server runs `run_git` in `app/api/checkout_root.rs`.

## 8. `read_git_config_parameters` accepts empty keys that Git rejects (Low)

The doc says the reader follows Git: it rejects malformed counts and missing
pairs. Git's `config_parse_pair` also rejects an empty indexed key
(`GIT_CONFIG_KEY_<n>=`) with "empty config key". Here the key is pushed as `""`.
The empty-key check in `parse_git_config_parameters` only covers the quoted
form.

## 9. `connect_local_stream_within` can panic on a large timeout (Low, latent)

`let deadline = Instant::now() + timeout;` panics if the addition overflows.
`shepr_api::client::request_value_with_timeout` guards the same timeout with
`checked_add` in `deadline_after`, but only after it calls `self.connect(timeout)`,
so the guard comes too late. Every current timeout is a constant. Use
`checked_add` here and return `InvalidInput`, as the API does.

## 10. Smaller doc and signature mismatches

- `host::hostname` claims it returns the name "as shown by tmux's `#h`". tmux's
  `#h` strips the domain (`#H` is the full name). This returns the full node
  name, so an FQDN hostname shows its domain in `{hostname}` window titles.
- `read_boot_log_tail` says it is "Opened as [`open_boot_log`] does", but it
  skips the owner check that `open_boot_log` applies.
- `SpawnedDaemon::try_wait` is documented as "Whether the daemon has exited".
  After it has reported the exit once, it returns `Ok(None)` ("not exited") on
  every later call. The one caller (`local_server.rs`) caches the first exit,
  so this is latent. `Option<ExitStatus>` with a sticky field, or a separate
  `exited()` method, would say what it means.
- `env::resolve_text` documents panics for "a flag, presence or handoff
  variable". It also panics for `Raw` (`SHELL`, `PATH`, `GIT_CONFIG_*`).
  `read_os` and `resolve_os` have no `# Errors` section.

## Lateral findings outside the scope

- **Local connects ignore the attempt deadline.** `connect_once` in
  `shepr-client/src/endpoint/supervisor.rs` takes a per-attempt `deadline` but
  connects to Local with `connect_trusted_local_stream`, which has a fixed
  5-second budget. `MachineSshConnector::attempt` does the same for the bridge
  socket. `connect_trusted_local_stream_within(path, remaining)` exists and
  would honour the deadline.
- **Logging cost per event.** Every write through `RotatingFileGuard` opens the
  log, takes `flock`, runs `stat` on both the path and the descriptor, writes
  and closes. That is fine at `shepr=info`. With `SHEPR_LOG=debug` on the PTY
  or render paths, the cost multiplies with events, panes and clients. A cached
  descriptor, checked against the path's inode only every N writes or on
  rotation, would keep rotation-following without a syscall storm. A failed
  `write_all` is retried whole, so a partial first write duplicates part of a
  line.
- **Data directory mode.** `RotatingFileMakeWriter::new`, `persist/io.rs` and
  `persist/writer.rs` all create the data directory with plain
  `create_dir_all` (umask mode). The log comment says the files are "private to
  the user like the rest of the data directory's state". The files are 0600,
  but the directory that holds history is world-listable under a 022 umask.
- **Platform crate scope.** `lib.rs` says "higher-level rules belong to the
  crates that consume these primitives". The crate nonetheless holds the logind
  inhibitor protocol and its warning-generation policy, clipboard helper
  selection, Git environment policy, and multi-process log rotation. It also
  makes the `shepr` client binary depend on `zbus` and `tokio` for a monitor
  only the server runs. If the owner wants the layering to match the doc,
  moving the shutdown monitor to `shepr-server` and the Git runner to
  `shepr-mux` would do it.

## Checked and found sound

The env policy and its Git count and quote parsers (against Git's `strtoul`
and `sq_dequote_step` behaviour), the BSP layout operations and focus history,
the geometry clamps, `PaneId` allocation, the socket startup lock, staging and
hard-link bind, the probe classification, single-use sweep ordering against
live owners, `ProcessHandle` and `session_member_handles` pid-reuse reasoning,
`reap_pidfd` status mapping, the clipboard deadline and selection-owner
handling, `SpawnedDaemon` group kill, the SSH control-path length arithmetic
(`%C` = 40 bytes, staging suffix = 17), the remote bridge relay and watchdog,
and `begin_cli_output` ordering (every CLI command makes its socket requests
before it prints).
