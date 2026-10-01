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

## RLAUNCH-014 - Structural: server presence is decided in three places with three rules

The launcher and the stop now share one socket-pair absence predicate, and the
launcher waits through the API-first startup and shutdown transitions. Residue:
the server's own lease, API and client socket release order is still a third,
separately maintained rule, and boot identity is still a separate status check.
Folding the API into the client socket would leave one rule.

## RLAUNCH-013 - The unified machine probe adds ssh round trips to every connect

Preflight and the running client's connector now share one `MachineProbe`
(cache trust, invalidation, progress retention and server judgement agree).
Residue: the connector now queries `remote_server_status` and judges the build
on every connection attempt before starting the bridge, and its first attempt
also verifies the disk hint, so a first connect costs three round trips (verify,
status, bridge) where it cost one, and the connector re-verifies what preflight
already verified (separate probe instances). With ControlMaster sharing that is
cheap; on a cold link without it, status plus bridge plus handshake may not fit
the attempt budget the bridge alone used to fit. The handshake already rejects a
different build, so the connector could skip the judgement, or take its probe
state from the preflight result. `check_machine_ssh` in `machine_ssh.rs` is now
unused anywhere; delete it.

## RLAUNCH-021 - `apply_to_child_command` keeps two unreachable override arms

Lateral, `crates/shepr-config/src/address.rs`. The launcher spawns a daemon
only for the runtime address (`require_own_runtime_address` in shepr-remote
`local_server.rs`), so both override arms of
`ServerAddress::apply_to_child_command` are unreachable in production; the
`ApiOverride` arm still exports `SHEPR_SOCKET_PATH` and only a remote test
reaches it. Collapse the method to the runtime arm and drop that test.

## RLAUNCH-022 - `is_ssh_link_failure` is named for less than it covers

The predicate is true for every ssh exit 255, authentication, host-key and
local configuration failures included. Its callers want exactly that
(`MachineProbe::resolve` keeps a disk hint across an ssh rejection, and the
candidate loop stops on any ssh failure), and discovery progress retention uses
the narrower `is_transient_network_failure` and `is_authentication_wait_timeout`
instead, so nothing is wrong today. Rename it to say what it covers (any failure
before a remote result), so no later caller takes it for a link-only check.
