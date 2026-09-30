# Defect hunt: crates/shepr-remote (and where src/preflight.rs meets it)

Scope read: every file under `crates/shepr-remote/src/`, `src/preflight.rs`,
`src/autodetect.rs`, `src/main.rs`, the client's use of the connector
(`crates/shepr-client/src/endpoint/supervisor.rs`), and the far ends the crate
hands values to (`shepr-api::server_stop`, `src/cli/status.rs`,
`src/cli/server.rs`, `shepr-platform` ssh paths and bridge IO).

Findings are ordered by how much they hurt the owner in practice. Each names
the claim it breaks.

---

## 1. Every unrecognised ssh exit 255 is filed as "the machine did not answer", so real authentication and configuration failures go silent at startup

Where: `SshFailureDiagnostic::from_ssh_output` and `classify_ssh_diagnostic`
(`crates/shepr-remote/src/lib.rs`), `classify_check`
(`crates/shepr-remote/src/remote/preflight.rs`), `result_notices`
(`src/preflight.rs`).

`from_ssh_output` maps exit 255 to `HostKey` or `Authentication` only when the
stderr matches a few narrow signatures, and everything else to `Link`.
`classify_check` then files `Link` as `MachineCheck::Offline`, and
`result_notices` deliberately prints nothing for `Offline` ("Offline machines
are left to the client").

Claims broken:

- `MachineCheck::Offline` is documented as "The machine did not answer:
  timeout, refusal, no route". `is_ssh_link_failure` is documented as "the
  remote end was never reached or was lost".
- `result_notices` promises that "everything the operator can act on is
  printed".
- AGENTS.md: shepr "runs interactive ssh for each one that needs
  authentication".

Concrete inputs that land in `Offline`, with no notice and no prompt:

- `Received disconnect from H port 22:2: Too many authentication failures`.
  This is the common case of an ssh-agent holding more keys than the server's
  `MaxAuthTries`. It is an authentication failure, and a prompt with the right
  key (or `IdentitiesOnly`) would fix it. Instead the machine is silently
  "offline" at startup and shows as Reconnecting forever.
- `ssh: Could not resolve hostname typo.example`: a typo in `[[machines]].ssh`.
- `/home/u/.ssh/config: line 12: Bad configuration option: ...`, or
  `Bad owner or permissions on /home/u/.ssh/config`. The managed config
  `Include`s the user config, so a broken user config fails every command with
  exit 255 before anything is reached.
- `Connection closed by H port 22` after a server-side refusal (fail2ban,
  `AllowUsers`).

A related naming defect: `SshFailureDiagnostic::is_link_failure` returns true
for Authentication and HostKey diagnostics too, because it falls back to "origin
was ssh exit 255". So an authentication refusal, where the remote was reached,
answers "yes, link failure". `classify_check` only gets this right because it
tests `requires_authentication` and `is_host_key` first. The connector and
discovery rely on the true answer, since they want to keep hints on an auth
failure. The function's name and doc say something different from what those
callers need.

Recommendation (structural): stop letting one boolean mean two things. Classify
once at the ssh boundary into what was learned: *nothing about the remote*
(could not reach, auth refused, host key refused, local ssh config broken)
versus *the remote said something*. Classify separately into *who can fix it*
(transient network, operator at the terminal, operator editing config).
Preflight silence should key on "transient network" only. An unmatched exit 255
should default to a class that is reported, not one that is hidden.

---

## 2. Discovery swallows why a candidate's status probe failed, and tells the operator to reinstall

Where: `remote_client_status` (`crates/shepr-remote/src/remote/discovery.rs`).
It is reached from `SshDiscovery::matches`, `verify_remote_shepr` and the
connector.

When the remote `shepr status client --json` exits non-zero and ssh itself did
not fail, the function returns `Ok(None)` and drops stderr. The candidate then
counts as "not a match". When every candidate is dropped this way, discovery
ends with `matching Shepr is not ready on H; install or update it there manually
and retry`.

Follow the value across the boundary. On the remote, `status client` goes
through `cli::run`, which calls `resolve_app_paths()` first (`src/cli.rs`).
`AppPaths::resolve` fails the launch when `XDG_RUNTIME_DIR` is unset
(`crates/shepr-config/src/io.rs`), which happens in ssh sessions on hosts
without pam_systemd or logind. `status client` needs no paths at all, since
`client_status_json` reads none. So a correctly installed, correct-build shepr
on such a host is reported as "not ready; install or update it", and the real
message (`application paths could not be resolved: XDG_RUNTIME_DIR must be
set...`) is discarded. The same holds for any other remote failure of the probe
that is not `test -x` failing.

On the cached path, `resolve_remote_shepr` treats that `Ok(false)` as "gone"
and invalidates the metadata cache before rediscovering, so the good hint is
thrown away too.

Claims broken: `verify_remote_shepr` documents "`Ok(false)` means it is gone".
It also means "it ran and failed for any reason". The not-ready message
misdiagnoses the failure.

Fix direction: keep `Ok(None)` for "not executable / not there" (the `test -x`
leg), and return the `command_failed` diagnostic for a probe that ran and
failed. Separately (root binary, outside this crate), `status client` should
not resolve `AppPaths`.

---

## 3. The "how to stop it yourself" hint runs a bare `shepr` that discovery already proved may not be on PATH

Where: `remote_stop_command` in `src/preflight.rs`, used by `restart_notice`
for Declined, NoTerminal and Failed.

The hint is `ssh <target> shepr server stop`. That runs `shepr` through the
remote user's non-interactive shell. Discovery exists precisely because that
PATH often lacks `~/.cargo/bin` and `~/.local/bin`: see the doc on
`known_remote_binary_candidate_script` ("which misses them when a
non-interactive SSH shell has a minimal PATH"). On such a host the printed
command fails with `shepr: command not found`. The `DifferentBuildServer` in
the outcome already carries the verified absolute `executable`, and the hint
should use it (quoted with `shell_quote`).

Claim broken: AGENTS.md says "on refusal, the server is left running and
unavailable and shepr says how to stop it". The notice text itself says "To
restart it, run `...`".

---

## 4. The connector throws away the remembered attempt's error and the hint, and persistent non-link failures pay full rediscovery on every retry

Where: `MachineSshConnector::connect`
(`crates/shepr-remote/src/remote/machine_ssh.rs`).

When the attempt with the remembered executable fails for any non-link reason,
three things happen:

1. The error is logged at debug level and dropped.
2. `remote_shepr` is set to `None`.
3. Full discovery runs within the same deadline, then a second bridge and
   handshake.

Consequences:

- Masked diagnosis and wrong status class. Take a remembered attempt that
  failed in a way that should show Attention, for example a handshake
  `PreambleError::DifferentBuild` from a remote server the operator declined to
  restart. If the rediscovery that follows runs out of the attempt budget, the
  endpoint reports `SSH connection attempt ran out of time` (TimedOut, so link,
  so Reconnecting). The real, actionable diagnostic is lost. This is likely
  when the first attempt spent its budget waiting on a slow remote server boot
  inside `run_remote_client_bridge`.
- The good hint is lost even though the executable was fine. Discovery then
  ends on a link failure, `remote_shepr` stays `None`, and the next attempt
  starts from full discovery again.
- A cost that never stops. For a failure that is not about the install (a
  different-build server left running, a remote server launch failure, the
  remote idle watchdog), every retry, forever, costs: one bridge launch, the
  login-shell probe, the `/bin/sh` probe, the known-locations script, one
  status probe per candidate, and a second bridge launch. The struct doc
  promises "a moved, removed or upgraded remote install costs one extra bridge
  launch". It is one extra launch per attempt, indefinitely, for failures that
  have nothing to do with the install.
- `handshake_error` maps a close before Welcome with no bridge report to
  `UnexpectedEof`, which classifies as `Other`. So a dropped link that the
  bridge thread had not reported within `BRIDGE_FAILURE_REPORT_TIMEOUT` also
  triggers rediscovery.

Fix direction: only rediscover when the failure says something about the
executable. The bridge's remote command failed to exec, or the preamble says the
remote *client* is the wrong build. Do not rediscover for handshake-level server
mismatches or remote launch errors. When rediscovery does run and fails,
return the more specific of the two errors, preferring the remembered attempt's
attention-class error over a later timeout.

---

## 5. User keepalive settings are not preserved, despite the comment that says they are

Where: `write_managed_ssh_config` and `apply_batch_ssh_options`
(`crates/shepr-remote/src/remote/ssh.rs`).

The comment on `write_managed_ssh_config` says the user config is included
first "so OpenSSH's first-value-wins behavior preserves explicit user
keepalives". But `apply_batch_ssh_options` also passes
`-o ServerAliveInterval=15 -o ServerAliveCountMax=4` on the command line to
every BatchMode command and every bridge. Command-line options beat every
config file, so the user's `ServerAliveInterval` never applies.

It also matters for the shared master. If a BatchMode check or bridge creates
the ControlPersist master, which is the normal case when no prompt was needed,
the master carries shepr's keepalive for its whole life. `ssh_tests.rs`
asserts both halves (config lines present, command-line options present), so
the contradiction is locked in by tests.

Claim broken: the doc comment on `write_managed_ssh_config`, and the `limits`
doc "OpenSSH keepalive settings shared by command arguments and managed
config". Pick one: drop the command-line keepalive and rely on the config's
`Host *` block, or drop the comment.

---

## 6. `sh_output_within` lets a stdin write error override ssh's typed failure

Where: `RemoteSsh::sh_output_within` (`ssh.rs`).

The order is `write_all(script)`, then `wait_with_output_timeout`, then
`write_result?`, then normalize. If ssh exits before reading stdin (auth
refused, host key refused, connection refused), `write_all` can fail with
`BrokenPipe`. The function then returns that bare `BrokenPipe` and discards the
`Output` that carried exit 255 and the classified stderr. The result
classifies as `Other` (`MachineCheck::Failed`), not
`NeedsAuthentication`/`HostKey`/`Offline`. Today the script is small and is
written microseconds after spawn, so the pipe buffer normally absorbs it and
this is latent rather than frequent. Still, the precedence is backwards: the
child's output should win and the write error should matter only when the
command otherwise succeeded.

---

## 7. Discovery stops at the first wrong-build candidate instead of probing the rest

Where: `SshDiscovery::matches` and `DiscoveryProgress::run_remaining`
(`discovery.rs`).

`matches` returns `Err` for a candidate of another build (and for a bad
sibling), and `run_remaining` propagates it with `?`. So later candidates are
never probed. The `DiscoveryProgress` doc says discovery runs "a status probe
per candidate until one matches". Under the owner's one-install-per-host rule
this rarely bites, but it does when a PATH entry is a shim or wrapper that
resolves differently from the real install (the code explicitly supports mise
shims). The doc or the behaviour should change. The cheap fix is to remember
the first mismatch error, keep probing, and return that error only when no
candidate matches.

---

## 8. Lateral: a remote server started by the bridge lives in the ssh session's cgroup

Where: `run_remote_client_bridge` calls `local_server::ensure_running`, then
`build_server_daemon_command` and `detach_server_daemon_command` (`setsid`
only).

The daemon is detached by session id but stays in the ssh login session's
systemd scope. On a host with logind `KillUserProcesses=yes`, systemd kills
everything in the scope when that ssh session ends, which is when the bridge
disconnects. That would take the remote server and every pane with it, against
the core promise that workspaces and panes live in a headless server that
outlives clients. I did not verify this on a host. It depends on the host's
logind config. If it matters, the fix is to start the daemon in its own
transient user scope (`systemd-run --user --scope`, or the D-Bus equivalent)
when a user manager is available.

---

## 9. Lateral: `launch_with` can blame and kill its own daemon for someone else's server

Where: `launch_with` (`crates/shepr-remote/src/remote/local_server.rs`).

When the probe answers with a different build while our daemon has not
exited, the launch returns `sibling_build_mismatch` ("... is a different build
than this shepr and was stopped") and the guard kills our daemon. The launch
lock only orders clients. A `shepr-server` started directly (for example
`brokkr run shepr-server`) takes no launch lock, so it can bind between our
second probe and our daemon's bind. The answering server is then not ours: the
message blames the installed pair wrongly, and a daemon that was about to exit
`AlreadyRunning` gets killed. This is rare, but the message states something
the code has not established. Checking the answering server's pid or boot
against the spawned child would close it.

---

## 10. Low: the connector never rebuilds its managed ssh config once it exists

Where: `ConnectorState::ssh` in `machine_ssh.rs`.

`RemoteSsh` and its temporary config directory under `XDG_RUNTIME_DIR` are
built once and kept for the client's life. If the directory disappears later
(logind removes `/run/user/<uid>` after the last login session ends while the
TUI keeps running under tmux, or anything else deletes it), every later ssh
fails with `Can't open user config file` and exit 255. Per finding 1 that
classifies as a link failure, and the endpoint shows Reconnecting forever with
no recovery. The code comments take care to retry a transient *setup* failure.
The same care does not reach a config that vanished after setup. A cheap guard
is to check that `config_path` exists before each attempt and rebuild
`RemoteSsh` when it does not.

---

## 11. Low: `SshMetadataCache::store` reports failure after it has succeeded

Where: `store_private_json` (`crates/shepr-remote/src/machine/ssh_metadata.rs`).

The rename has already replaced the cache file when `sync_directory(parent)`
runs. If that fsync fails, `store` returns `Err`, and callers log "could not
cache SSH machine metadata; later connections rediscover the remote shepr",
which is false: the entry is there and will be used. The metadata directory
itself is created with `create_dir_all`, so its mode is the umask default,
unlike the private file inside it.

---

## Checked and found consistent

- Host keys: every ssh shepr builds, the prompt included, passes
  `StrictHostKeyChecking=yes`. A HostKey check result is never prompted for.
- BatchMode: the checks, the bridge and the remote stop all go through
  `apply_batch_ssh_options`.
- Prompts: they run one at a time, in configuration order, before any restart
  offer. The re-check happens only after a successful prompt, and each round
  gets a fresh deadline.
- The conditional stop: the remote stop maps `BOOT_MISMATCH_EXIT_CODE` and
  `NO_SERVER_EXIT_CODE` correctly through the 255-to-254 remap, and
  `server stop` skips the build check.
- Bridge teardown stays bounded under ControlMaster multiplexing: killing the
  mux client makes the master close the passed stdio descriptors, so the
  download and upload joins in `SshStdioBridge::drop` finish.
- Shell handling: the remote command wrapping is safe for non-POSIX login
  shells, because paths are restricted to plain words and scripts go to
  `/bin/sh`. `SshTarget` rejects a leading `-`, so no ssh option can be
  injected through the target.
