# Defects: the root shepr binary (CLI)

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

Hunter coverage: `src/main.rs`, `src/cli.rs`, `src/cli/target.rs`, `src/cli/server.rs`, `src/cli/status.rs`, plus a grep over the agent, pane, workspace, tab, machine and integration handlers. Not read: `src/api/`, `build.rs`, `tests/`, `src/netside_tests.rs`, `src/test_support*`, `spec.rs`, and the bodies of the pane, workspace, tab, machine and integration commands. The hunter suggested `src/cli/agent.rs` (whether `explain --file` validates the file before any local-vs-remote handling), `src/cli/pane.rs`, and the unread areas as next reads.

## CMD-004 - Build-mismatch machinery contradicts "client and server are always the same build"

There are two separate notions of mismatch:
- `restart_needed` is driven by protocol compatibility.
- `server_binary_stale` is driven by the version string.

`session stop` and local `server stop` also deliberately skip the protocol check "to stop a server from another build". This is defensible for a binary replaced in place, but it adds code surface (the `protocol_guard` and `Compatibility::Unknown` paths, `restart_after_update_guidance`) that could be reduced to one exact build-id check. (Remote discovery now requires an exact version and protocol match.)

## CMD-005 - Duplicated error-printing paths

`send_ok_request` and `print_response` duplicate the "error, print to stderr, return 1" logic. `print_read_response` prints errors with `{response}`, but `print_response` uses `serde_json::to_string`. The output is the same; it is just an inconsistency.
