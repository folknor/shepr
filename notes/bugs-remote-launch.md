# Defects: remote and launch

Filed from the defect hunt over `crates/shepr-remote/src/`, the root binary's
`src/`, and `crates/shepr-platform/src/remote_bridge.rs`.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## RLAUNCH-001 - A launch right after a successful conditional stop can be refused as "listening but not answering"

Hunter's severity: medium (narrow race, deterministic consequence, hits the
designed restart flow).

- The server releases what a launcher looks at in this order
  (`HeadlessServer::release_sockets_after_save_observed`): data-directory lease,
  then the API socket (dropping `ServerHandle` unlinks the file and joins the API
  listener thread, "bounded by one accept-failure backoff (at most a second)"),
  then the client socket.
- The conditional stop (`stop_socket_with_timeout` with `expected_boot_id`)
  succeeds as soon as the named boot stops answering on the API socket and the
  lease is free. It does not wait for the client socket; the unconditional stop
  does (`wait_until_stopped_until` over both sockets).
- So `stop_active_server(paths, Some(boot))` returns inside the window where the
  client socket is live and the API socket file is already gone.
- `local_server::probe_server_at` checks the client socket first (`Live`), then
  `shepr_api::read_runtime_status_at` on the API socket, which returns `Ok(None)`
  for an absent API socket file and for `ConnectionRefused`; `probe_server_at`
  maps `Ok(None)` to `Probed::Unresponsive`.
- `ensure_running` turns `Unresponsive` into a hard error: "a shepr server is
  listening at ..., but it is not answering status requests, so its build cannot
  be confirmed and no second server is started", with stop guidance for a server
  that is gone.

The preflight path walks into this: `preflight::run` -> `restart_local` ->
`stop_active_server(paths, Some(boot_id))` returns `Ok` -> notice "stopped the
local server of a different build; one of this build starts now" ->
`auto_detect_launch` -> `ensure_running` probes within milliseconds. With no
machines configured the TUI launch then fails. The remote equivalent
(`stop_remote_server`, then the bridge's `ensure_running` on the remote host) is
the same race, surfacing as a retryable bridge failure.

Claims broken: `local_server.rs` module doc step 1 (a stopping server "is treated
as no server ... and the launch below outlasts it"); the
`release_sockets_after_save` doc (the release order is "the one order that keeps
[a launching shepr] from being misled"); `preflight::local_notice` for `Stopped`
("one of this build starts now").

Direction: make the conditional stop's success criterion the launcher's notion of
"no server" (wait for both sockets, as the unconditional stop does), or have the
probe treat "client socket live, API socket absent or refusing" as `Stopping`,
not `Unresponsive`. See RLAUNCH-014.

## RLAUNCH-002 - FIDO-touch keys are classified Offline, so the documented prompt never runs

Hunter's severity: medium; confidence moderate (depends on the authenticator's
user-presence timeout, typically around 30 s, exceeding shepr's 15 s round trip).

`remote/preflight.rs` module doc: "The TUI reaches machines with `BatchMode=yes`,
so any prompt (password, key passphrase, keyboard-interactive, FIDO touch) fails
its connection", and preflight then runs interactive ssh for machines that need
authentication. A key that needs a touch does not fail under BatchMode; ssh (or
the agent) blocks for user presence. `RemoteSsh::command_timeout` caps the round
trip at `SSH_COMMAND_TIMEOUT` (15 s) and `wait_with_output_timeout` kills ssh
with `ErrorKind::TimedOut`; `SshFailureDiagnostic::from_error` maps `TimedOut` to
`SshFailure::Link`, and `classify_check` reports `MachineCheck::Offline`.
Offline machines get no prompt and no notice. The "signing failed" signature in
`classify_ssh_diagnostic` would recognise the authenticator's own timeout only if
ssh lived long enough to print it. Net effect: a FIDO machine is silently shown
offline at startup, every client attempt blocks on a touch request for its
whole budget, and interactive authentication is never offered.

## RLAUNCH-003 - The local restart offer promises a restart it cannot perform for a socket-override address

Hunter's severity: low-medium. `preflight::run` offers the local restart for
whatever `running_server_status(paths)` finds, which follows the resolved
address, including a socket override. After consent and a successful stop,
`local_notice(Stopped)` says "one of this build starts now", and the offer says
"The saved layout is restored with fresh shells". But `ensure_running` ->
`require_own_runtime_address` refuses to start a server for any address that is
not the profile's runtime address: a stopped server, no replacement, and (with
no machines) a failed launch reading "no shepr server is running at ..., which
SHEPR_SOCKET_PATH selects". Claims broken: the offer and `local_notice` text;
AGENTS.md "the layout is restored with fresh shells and agents resumed". The
related `ServerAddress::resolve_paths` misclassification of the runtime address
is WIRECFG-002.

## RLAUNCH-004 - Local failures are filed as remote incompatibility

Hunter's severity: low. `SshFailureDiagnostic::from_error` maps `InvalidInput`,
`InvalidData`, `NotFound`, `PermissionDenied` and `Unsupported` to
`SshFailure::Compatibility` ("The remote end is not a usable shepr of this
build"), and `classify_check` turns that into `MachineCheck::Incompatible` ("The
machine answered but cannot be served"). Errors of those kinds that never
touched the machine: `ssh` not installed locally (`Command::spawn` fails with
`NotFound`; preflight prints "machine X cannot be used: No such file or directory
(os error 2)", naming neither ssh nor the missing program); a missing XDG runtime
root or unsafe runtime directory from `RemoteSsh::new` in `check_machine_ssh`
(`NotFound`, or `PermissionDenied` carrying `UnsafeSshRuntimeDirectory`); local
IO on the metadata cache path or managed config write.

Structural note: classification reverse-engineers provenance from `io::ErrorKind`
plus stderr signatures. A typed error at the source (local setup, ssh process,
remote command, install mismatch, server mismatch) carried through
`check_machine_ssh` and the connector would make these classes exact, and would
let RLAUNCH-002 tell "blocked on user presence" from "network timeout".

## RLAUNCH-005 - `DiscoveryProgress` keeps progress across ssh failures its doc says clear it

The struct doc says results survive only "a timeout, the attempt deadline, a
dropped or refused connection", and that "an ssh failure reported through a
command's output" clears them. `advance` keeps progress for any error where
`is_ssh_link_failure` is true, which is `failed_before_remote_result`: every ssh
exit 255, including authentication, host-key, local-configuration and
unrecognized failures. A host-key change (host reinstalled) resumes discovery
with the old host's candidate list and probe index. Behaviour is tested as kept;
one of the two should change, and the doc reads as the intended policy.

## RLAUNCH-006 - Discovery's "login shell" is not a login shell, and its doc says /bin/sh

- `RemoteSsh::posix_user_shell_output` says it "Runs `remote_command` under
  `/bin/sh` through the remote user's login shell". It hands the POSIX text
  straight to the account shell; there is no `/bin/sh`.
- `DiscoverySteps::path_via_login_shell`: "`command -v` through the remote login
  shell, which sets up the user's PATH". sshd runs `$SHELL -c <command>`, which
  is not a login shell, so `~/.profile`, `~/.bash_profile` and `~/.zprofile` are
  not read, and PATH additions made there (a common place for `~/.cargo/bin` and
  `~/.local/bin`) are invisible. The known-locations script covers the two
  default install paths, so the gap is installs elsewhere on a profile-only
  PATH.

## RLAUNCH-011 - User ssh config can break every shepr ssh invocation and the auth classification

Lateral, not a broken claim; noted because RLAUNCH-002 and this both undermine
the prompt-on-auth design. The managed ssh config includes the user's
`~/.ssh/config` first, and shepr overrides only a handful of options on the
command line. A host block with `RemoteCommand` makes every shepr ssh invocation
fail ("Cannot execute command-line and remote command"), and `LogLevel QUIET`
suppresses the stderr signatures `classify_ssh_diagnostic` depends on, so
authentication failures read as `Unrecognized` and are never prompted for.
Passing `-o RemoteCommand=none` and `-o LogLevel=ERROR` with the batch options
would make classification independent of user config.

## RLAUNCH-013 - Structural: two independent discovery and validation paths for one machine

`check_machine_ssh` (fresh discovery per round, verifies the disk cache, judges
the running server's build) and `MachineSshConnector` (resumable discovery,
trusts the remembered path, leaves the server build to the handshake) share only
the metadata cache file. A single machine-probe state machine used by preflight
and the connector would remove the duplicated policy and the places they
disagree (cache trust, progress retention, error classes).

## RLAUNCH-014 - Structural: server presence is decided in three places with three rules

The launcher (client socket liveness, then an API ping), the stop (API boot
answer, lease, or both sockets depending on mode) and the server's own release
order. RLAUNCH-001 is the visible symptom. Folding the API into the client socket,
or giving the launcher and the stop one shared predicate, would make the race
impossible rather than narrow.
