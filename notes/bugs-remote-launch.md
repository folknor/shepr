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

## RLAUNCH-023 - Structural: merge the client socket into the API socket

The server listens on two sockets, the JSON API (`shepr.sock`, agent hooks and
the CLI) and the client socket (the TUI's binary protocol). Presence is now one
rule (`shepr_platform::ipc::ServerLifetime`), but the second socket still costs
a second bind, lock and staging path, the client socket reservation and its
handoff, `SHEPR_CLIENT_SOCKET_PATH` with its override pairing rule in
`ServerAddress::resolve_paths`, and a two-socket wait in `server stop`. Many
findings in this hunt came from the two disagreeing.

Owner's direction: do whichever is better; the analysis favours merging. Keep
the API socket's path and drop the client socket. Dispatch on accept by the
first bytes: the TUI client opens with shepr's binary preamble, hooks and the
CLI send a JSON line. Keep separate admission for the two connection kinds (an
API connection carries one request; a TUI connection lives until disconnect),
and move the refusal worker, which is still process-wide, onto the server with
them. Constraint: a new build must still recognise and stop a server from an
older build, which works because the merged socket sits where the old API
socket lives, so `ping` (`version`, `build_id`, `boot_id`, `stopping`) and
`server.stop_if_boot` keep their literal JSON fixtures. Update AGENTS.md
(the socket variables, presence and stop) to match. While there:
`ServerLifetime::in_resource_order` is plain composition and adds indirection,
not enforcement, so simplify it once there is one socket; and
`read_runtime_status_at` is used only inside shepr-api but is still `pub` and
re-exported.
