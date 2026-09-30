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

## PLAT-001 - The server's client control queue has no depth bound

Hunter's severity: High (original); the residue is Medium. Scope: core-platform,
server serving.

The writer now takes an explicit stall limit and owns its nonblocking mode, so a
client that makes no progress times out and is disconnected. What remains:
`ClientWriterQueueState.control` (`shepr-server/src/server/client_transport.rs`)
is an unbounded `VecDeque`. Render frames coalesce, but control messages do not,
so a peer that drains slowly while never stalling long enough to trip the limit
(a throttled SSH bridge) lets the backlog grow in server memory without bound.

**Direction.** Bound the control queue by count or bytes, and disconnect the
client when it is exceeded, so the memory bound does not depend on the stall
timer.

Related, same file: the stall limit is 5 s of no progress. Over SSH a network
blip longer than that now forces a reconnect, while the ssh keepalive budget the
connection already tolerates is 15 s times 4. Decide whether the writer limit
should sit near that budget.

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

## PLAT-011 - Local and bridge connects ignore the per-attempt deadline

Hunter's severity: not given (lateral). Scope: core-platform.

`connect_once` in `shepr-client/src/endpoint/supervisor.rs` takes a per-attempt
`deadline` but connects to Local with `connect_trusted_local_stream`, which has a
fixed 5-second budget. `MachineSshConnector::attempt` does the same for the
bridge socket. `connect_trusted_local_stream_within(path, remaining)` exists and
would honour the deadline.

## PLAT-013 - The data directory itself is created by the lease with the umask mode

Hunter's severity: Low (lateral). Scope: mux persist.

The log writer, session persistence, recovery snapshots and backups, and the SSH
metadata store now create their directories private. The data directory itself is
still created first by `DataDirLease::acquire`
(`crates/shepr-mux/src/persist/lock.rs`) with plain `create_dir_all`, before any
save runs; the private-directory helper leaves an existing directory unchanged,
so the later sites cannot tighten it. Under a 022 umask the directory holding
history is world-listable. Use the private-directory helper at the lease.

## PLAT-014 - `shepr-platform` holds higher-level policy its own doc says belongs elsewhere

Hunter's severity: not given (lateral). Scope: core-platform.

`lib.rs` says "higher-level rules belong to the crates that consume these
primitives". The crate nonetheless holds the logind inhibitor protocol and its
warning-generation policy, clipboard helper selection, Git environment policy,
and multi-process log rotation. It also makes the `shepr` client binary depend
on `zbus` and `tokio` for a monitor only the server runs. Moving the shutdown
monitor to `shepr-server` and the Git runner to `shepr-mux` would match the doc.

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

## PLAT-024 - The connector cannot recover when `XDG_RUNTIME_DIR` itself disappears

Hunter's severity: Low. Scope: remote.

Where: `ConnectorState::ssh` in `machine_ssh.rs`, `RemoteSsh::new` and the
managed ssh config path policy in `crates/shepr-remote/src/remote/ssh.rs`.

The connector now rebuilds `RemoteSsh` when its managed config file is missing,
which covers a removed shepr config directory. It does not cover the runtime root
vanishing (logind removes `/run/user/<uid>` after the last login session ends
while the TUI keeps running under tmux): `RemoteSsh::new` validates that root and
fails, so every attempt fails and the endpoint shows Reconnecting forever. The
limitation is noted at the call site. Recovery needs a decision in the ssh config
path policy: fall back to a shepr-owned directory elsewhere, or surface the loss
as an Attention diagnostic instead of a link failure.

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

## PLAT-035 - A test leaves an orphaned `shepr-fixture` process behind

Scope: test support (lateral, source not pinned down).

After `brokkr test` runs of shepr-remote and shepr-mux, the next brokkr command
twice reported "SIGKILL sent to 1 orphaned test process (shepr-fixture) that
outlived the brokkr run". Some test spawns a fixture child and does not reap or
kill it. Find the test (run the two packages' tests one module at a time and
watch for the report) and make its fixture owned by a guard that kills and waits
on drop. It may predate the wave that noticed it.
