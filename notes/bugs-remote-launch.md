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

## RLAUNCH-019 - The client-socket override rule may have lost its last consumer

Lateral. `ServerAddress::resolve_paths` lets `SHEPR_CLIENT_SOCKET_PATH` select
the client socket when `SHEPR_SOCKET_PATH` is exactly the profile's runtime
`shepr.sock`; AGENTS.md says this "keeps a nested client on a server started
with only a client socket override". Nested launches in a same-profile pane are
now always refused, and a pane of the other profile ignores both socket
variables, so a nested client reaches that rule only if `SHEPR_ENV` was
removed by hand. Check whether anything else (a CLI command run in a pane)
still needs it; if not, delete the rule and its AGENTS.md sentence.
