# Defect hunt: remote and launch

Scope: `crates/shepr-remote/src/`, the root binary's `src/` (CLI, `preflight.rs`,
`main.rs`, `autodetect.rs`) and `crates/shepr-platform/src/remote_bridge.rs`.
Followed values into `shepr-api` (`status.rs`, `server_stop.rs`, `server.rs`),
`shepr-config` (`address.rs`, `io.rs`), `shepr-platform` (`daemon.rs`,
`ssh_paths.rs`, `host.rs`) and the server's shutdown order
(`shepr-server/src/server/headless/lifecycle.rs`, `bootstrap.rs`).

Findings are ordered by how much they matter. Each names the claim it breaks.

---

## 1. A launch right after a successful conditional stop can be refused as "listening but not answering"

Severity: medium (narrow race, deterministic consequence, hits the designed
restart flow).

What happens:

- The server releases what a launcher looks at in this order
  (`HeadlessServer::release_sockets_after_save_observed`): data-directory lease,
  then the API socket (dropping `ServerHandle`, which unlinks the file and then
  joins the API listener thread, "bounded by one accept-failure backoff (at most
  a second)"), then the client socket.
- The conditional stop (`stop_socket_with_timeout` with `expected_boot_id`)
  succeeds as soon as the named boot stops answering on the API socket and the
  lease is free. It does not wait for the client socket. The unconditional stop
  does (`wait_until_stopped_until` over both sockets); the conditional one never
  consults `stopped_socket_paths` on success.
- So `stop_active_server(paths, Some(boot))` returns inside the window where the
  client socket is still live and the API socket file is already gone.
- `local_server::probe_server_at` checks the client socket first (`Live`), then
  calls `shepr_api::read_runtime_status_at` on the API socket. That function
  returns `Ok(None)` for an absent API socket file and for `ConnectionRefused`.
  `probe_server_at` maps `Ok(None)` to `Probed::Unresponsive`.
- `ensure_running` turns `Unresponsive` (first probe, or the probe under the
  lock) into a hard error: "a shepr server is listening at ..., but it is not
  answering status requests, so its build cannot be confirmed and no second
  server is started", with stop guidance for a server that is in fact gone.

The preflight path walks straight into this: `preflight::run` ->
`restart_local` -> `stop_active_server(paths, Some(boot_id))` returns `Ok` ->
notice "stopped the local server of a different build; one of this build starts
now" -> `auto_detect_launch` -> `ensure_running` probes within milliseconds.
With no machines configured the TUI launch then fails. The remote equivalent
(`stop_remote_server` then the bridge's `ensure_running` on the remote host) is
the same race, surfacing as a retryable bridge failure.

Claims broken:

- `local_server.rs` module doc, step 1: a server that is stopping "is treated as
  no server ... and the launch below outlasts it". A server in its last
  shutdown step is treated as an unresponsive live server and the launch gives
  up.
- `release_sockets_after_save` doc: the release order is "the one order that
  keeps [a launching shepr] from being misled". The launcher is misled exactly
  between the second and third step.
- `preflight::local_notice` for `Stopped`: "one of this build starts now".

Direction: either make the conditional stop's success criterion the same as
the launcher's notion of "no server" (wait for both sockets, as the
unconditional stop does), or have the probe treat "client socket live, API
socket absent or refusing" as `Stopping`, not `Unresponsive`. Structurally, the
launcher, the stop and the server's release order each encode their own idea of
"a server is there"; one shared predicate (or one socket) would remove the class
of race.

---

## 2. FIDO-touch keys are classified Offline, so the documented prompt never runs

Severity: medium (confidence moderate: depends on the authenticator's own
user-presence timeout, typically around 30 s, being longer than shepr's 15 s
round trip).

`remote/preflight.rs` module doc: "The TUI reaches machines with `BatchMode=yes`,
so any prompt (password, key passphrase, keyboard-interactive, FIDO touch) fails
its connection", and `preflight` then runs interactive ssh for the machines that
need authentication.

A key that needs a touch does not fail under BatchMode; ssh (or the agent)
blocks waiting for user presence. `RemoteSsh::command_timeout` caps the round
trip at `SSH_COMMAND_TIMEOUT` (15 s) and `wait_with_output_timeout` kills ssh
with `ErrorKind::TimedOut`. `SshFailureDiagnostic::from_error` maps `TimedOut`
to `SshFailure::Link`, and `classify_check` reports `MachineCheck::Offline`.
Offline machines get no prompt and no notice (`result_notices` deliberately
stays silent for them). The "signing failed" signature in
`classify_ssh_diagnostic` would recognise the authenticator's own timeout, but
only if ssh lived long enough to print it.

Net effect: a FIDO machine is silently shown offline at startup, every client
attempt then blocks on a touch request for its whole budget, and the
interactive authentication the module exists to provide is never offered.

---

## 3. The local restart offer promises a restart it cannot perform for a socket-override address

Severity: low-medium.

`preflight::run` offers the local restart for whatever
`running_server_status(paths)` finds, which follows the resolved address,
including a socket override. After consent and a successful stop,
`local_notice(Stopped)` says "one of this build starts now", and the offer text
says "The saved layout is restored with fresh shells". But
`ensure_running` -> `require_own_runtime_address` refuses to start a server for
any address that is not the profile's runtime address. The result is a stopped
server, no replacement, and (with no machines configured) a failed launch
reading "no shepr server is running at ..., which SHEPR_SOCKET_PATH selects".

Related false negative in `ServerAddress::resolve_paths` (`shepr-config`): when
both pane variables are set and name exactly the runtime paths (what every pane
exports), the source is `ClientOverride`, so `is_runtime_address()` is false
for what is in fact the runtime address. Guidance then gains a superfluous
`SHEPR_CLIENT_SOCKET_PATH=...` prefix and a launch refuses to start a server.
Reachable only with inherited pane environment (nesting allowed, or a process
such as tmux started in a pane that outlives the server), so low.

Claims broken: the local offer and `local_notice` text; AGENTS.md "the layout
is restored with fresh shells and agents resumed".

---

## 4. Local failures are filed as remote incompatibility

Severity: low (wrong class and unhelpful text; the machine still fails soft).

`SshFailureDiagnostic::from_error` maps `InvalidInput`, `InvalidData`,
`NotFound`, `PermissionDenied` and `Unsupported` to `SshFailure::Compatibility`
("The remote end is not a usable shepr of this build"), and `classify_check`
turns that into `MachineCheck::Incompatible` ("The machine answered but cannot
be served"). Errors with those kinds that never touched the machine:

- `ssh` not installed locally: `Command::spawn` fails with `NotFound`. Preflight
  prints "machine X cannot be used: No such file or directory (os error 2)",
  naming neither ssh nor the missing program.
- A missing XDG runtime root or an unsafe runtime directory from
  `RemoteSsh::new` in `check_machine_ssh` (`NotFound`, or `PermissionDenied`
  carrying `UnsafeSshRuntimeDirectory`).
- Local IO on the metadata cache path or managed config write.

Structural note: classification reverse-engineers provenance from
`io::ErrorKind` plus stderr signatures. A typed error at the source (local
setup, ssh process, remote command, install mismatch, server mismatch) carried
through `check_machine_ssh` and the connector would make these classes exact
and remove the kind-based guessing. The same typing would let finding 2 tell
"blocked on user presence" apart from "network timeout".

---

## 5. `DiscoveryProgress` keeps progress across ssh failures its doc says clear it

Severity: low (doc/code disagreement; behaviour is tested as kept).

The struct doc says results survive only "a timeout, the attempt deadline, a
dropped or refused connection", and that "an ssh failure reported through a
command's output" clears them. `advance` keeps progress for any error where
`is_ssh_link_failure` is true, which is `failed_before_remote_result`: every ssh
exit 255, including authentication, host-key, local-configuration and
unrecognized failures. A host-key change (host reinstalled) therefore resumes
discovery with the old host's candidate list and probe index. One of the two
should change; the doc reads as the intended policy.

---

## 6. Discovery's "login shell" is not a login shell, and its doc says /bin/sh

Severity: low (doc inaccuracy with a discovery consequence).

- `RemoteSsh::posix_user_shell_output` says it "Runs `remote_command` under
  `/bin/sh` through the remote user's login shell". It hands the POSIX text
  straight to the account shell; there is no `/bin/sh`.
- `DiscoverySteps::path_via_login_shell`: "`command -v` through the remote login
  shell, which sets up the user's PATH". sshd runs `$SHELL -c <command>`, which
  is not a login shell, so `~/.profile`, `~/.bash_profile` and `~/.zprofile`
  are not read. PATH additions made there (a common place for `~/.cargo/bin`
  and `~/.local/bin`, and for anything else) are invisible to this probe. The
  known-locations script covers the two default install paths, so the
  practical gap is installs elsewhere on a profile-only PATH.

---

## 7. A remote server is judged restartable on a boot id the stop command rejects

Severity: low.

`judge_remote_server` accepts any non-empty printable token as the boot id
(`printable_remote_token`) and returns `DifferentBuild`. The stop then runs the
verified remote `shepr server stop --expect-boot <id>`, whose clap value parser
(`spec::boot_id`) requires a canonical `shepr_protocol::BootId`. A boot id in
any other form makes the remote CLI exit 2, which `stop_remote_server` reports
as `RestartResult::Failed` after the operator consented. `judge_remote_server`'s
doc says a server "that did not [report a usable boot identity] cannot be
stopped as a specific instance, so it is an error the operator has to act on";
it should parse with the same `BootId` rule the stop uses, so such a server is
classified up front instead of offered and then failed.

---

## 8. Root `--help` and `--version` do not always win

Severity: low.

`launch_with_args`: "Root-level `--help` and `--version` win over any
subcommand given with them." Clap validates the subcommand before the root flags
are read, so `shepr --help server`, `shepr --version detect` (both groups are
`subcommand_required` with `arg_required_else_help`) and
`shepr -V detect capture` (missing required PANE) end in a usage error, exit 2,
instead of the root help or version.

---

## 9. Exemptions that buy nothing, and a CLI path that needs more than it says

Severity: low (smell; no wrong output).

- `cli::run` exempts `status client` from path resolution because "A remote
  discovery probe runs this in an ssh session that may have no
  XDG_RUNTIME_DIR". The check's very next probe, `status server --json`
  (`remote_server_status`), and the bridge itself both resolve `AppPaths`,
  which refuses an unset `XDG_RUNTIME_DIR`. Such a machine is unusable anyway;
  the exemption only moves where it fails.
- `detect explain --file` is documented (spec help, AGENTS.md) as evaluating
  locally "without a server", but `cli::run` resolves `AppPaths` before
  dispatch, so it still fails without `XDG_RUNTIME_DIR` or with an invalid
  `SHEPR_BUILD_PROFILE` marker.

---

## 10. Dead branch in `restart_notice`

Severity: trivial.

`restart_notice`'s `left_running` closure has a `None` arm ("Its executable and
boot identity are not available for a safe stop command"). `left_running` is
only reached for `NoTerminal`, `Declined` and `Failed`, and
`restart_different_builds` sets those only while `outcome.check` is still
`DifferentBuild`. The arm cannot run.

---

## Lateral findings outside the scope

- `shepr_remote::shell_quote` (also behind `interactive_shell_command`, which
  `shepr-server/src/app/agent_resume.rs` uses to type a resume command into a
  pane's shell) leaves words beginning with `=` unquoted. zsh's default
  `EQUALS` option expands `=word` to the path of command `word`, or fails with
  "word not found". Any resume argv element starting with `=` breaks under zsh.
  Quoting a leading `=` (and `~`, which is already excluded) closes it.
- The managed ssh config includes the user's `~/.ssh/config` first, and shepr
  overrides only a handful of options on the command line. A host block with
  `RemoteCommand` makes every shepr ssh invocation fail ("Cannot execute
  command-line and remote command"), and `LogLevel QUIET` suppresses the stderr
  signatures `classify_ssh_diagnostic` depends on, so authentication failures
  read as `Unrecognized` and are never prompted for. Passing
  `-o RemoteCommand=none` and `-o LogLevel=ERROR` with the batch options would
  make the classification independent of user config. Not a broken claim;
  noted because finding 2 and this both undermine the prompt-on-auth design.
- Startup cost of unreachable machines: preflight blocks the TUI until every
  check of a round finishes, up to `PREFLIGHT_CHECK_BUDGET` (25 s), and a
  second round follows any successful prompt. A blackholed host (no RST) costs
  the 10 s `ConnectTimeout` at every launch. Documented as the phase bound, so
  not a defect, but "fail soft" still means a slow start.

## Structural observations

- Two independent discovery and validation paths exist for one machine:
  `check_machine_ssh` (fresh discovery per round, verifies the disk cache, judges
  the running server's build) and `MachineSshConnector` (resumable discovery,
  trusts the remembered path, leaves the server build to the handshake). They
  share only the metadata cache file. A single machine-probe state machine,
  used by preflight and by the connector, would remove the duplicated policy
  and the places where the two disagree (cache trust, progress retention,
  error classes).
- Server presence is decided in three places with three rules: the launcher
  (client socket liveness, then an API ping), the stop (API boot answer, lease,
  or both sockets depending on mode) and the server's own release order.
  Finding 1 is the visible symptom. Folding the API into the client socket, or
  giving the launcher and the stop one shared predicate, would make the race
  impossible rather than narrow.
