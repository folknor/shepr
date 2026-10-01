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

## RLAUNCH-013 - Structural: two independent discovery and validation paths for one machine

`check_machine_ssh` (fresh discovery per round, verifies the disk cache, judges
the running server's build) and `MachineSshConnector` (resumable discovery,
trusts the remembered path, leaves the server build to the handshake) share only
the metadata cache file. A single machine-probe state machine used by preflight
and the connector would remove the duplicated policy and the places they
disagree (cache trust, progress retention, error classes).

## RLAUNCH-014 - Structural: server presence is decided in three places with three rules

The launcher and the stop now share one socket-pair absence predicate, and the
launcher waits through the API-first startup and shutdown transitions. Residue:
the server's own lease, API and client socket release order is still a third,
separately maintained rule, and boot identity is still a separate status check.
Folding the API into the client socket would leave one rule.

## RLAUNCH-017 - `is_remote_candidate_mismatch` matches every remote compatibility error

Lateral, `remote/discovery.rs`. The predicate now matches any remote
compatibility error (including "matching Shepr is not ready" and remote server
compatibility errors), not only a candidate mismatch. In
`resolve_remote_shepr` this matters only if verify can return one of the
others, which it appears not to today; the predicate is wider than its name.

## RLAUNCH-018 - An unwrapped local io error now retries silently instead of asking for attention

Lateral, `shepr-remote/src/lib.rs`. `SshFailureDiagnostic::from_error` no longer
treats `InvalidInput`, `NotFound` or `PermissionDenied` as needing attention;
local failures are recognised only when wrapped by `local_setup_error`. An io
error of those kinds that reaches the classifier unwrapped (from client
endpoint setup, say) now reads as `Other` and is retried as Reconnecting
instead of showing Attention.
