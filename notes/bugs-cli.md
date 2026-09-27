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

## CMD-001 - config check ignores --session / session attach

In `src/cli.rs`, `run()` returns `config_check()` before it resolves paths with the requested session. `config_check()` then calls `AppPaths::resolve()` with no session. So `shepr --session work config check` reports `paths.session_id`, `api_socket` and `client_socket` for the default session. That output is labelled "resolved sources", which should mean what this invocation resolved. It also disagrees with how the real launch resolves paths (`load_validated_config_or_exit` uses `resolve_with_session`).

## CMD-002 - The --machine rejection test checks less than it claims

`src/cli/target.rs`: `machine_commands_reject_local_side_effects_and_tui_attach` says `--machine` rejects local and TUI commands. Many of its cases are commands that don't exist, so they fail in the clap parse (`machine_command_allowed` returns false on parse error) and never reach `validate_machine_command`: `update`, `terminal session control`, `plugin install`, `api schema`, `api snapshot`. The comment on `api snapshot` ("used to pass the check") refers to a path that no longer exists. Only the real commands in the list actually test the filter.

## CMD-003 - status --json gives two answers for server_binary_stale on a remote target

In `src/cli/status.rs`, `server_status_json` sets it to `None` for `--machine` targets, but `update_status_json` still computes it from the version. The overview `--json` can show `server.server_binary_stale = null` and `update.server_binary_stale = true/false` together. In practice this only matters under `--machine`, and the overview is a local-only command, so it is mostly latent. The helper is still inconsistent.

## CMD-004 - Build-mismatch machinery contradicts "client and server are always the same build"

There are two separate notions of mismatch:
- `restart_needed` is driven by protocol compatibility.
- `server_binary_stale` is driven by the version string.

`session stop` and local `server stop` also deliberately skip the protocol check "to stop a server from another build". This is defensible for a binary replaced in place, but it adds code surface (the `protocol_guard` and `Compatibility::Unknown` paths, `restart_after_update_guidance`) that could be reduced to one exact build-id check.

Related: RMT-003 (remote discovery accepts any version).

## CMD-005 - Duplicated error-printing paths

`send_ok_request` and `print_response` duplicate the "error, print to stderr, return 1" logic. `print_read_response` prints errors with `{response}`, but `print_response` uses `serde_json::to_string`. The output is the same; it is just an inconsistency.

## CMD-006 - Unreachable unknown-subcommand branch hides spec/parser drift

When `Launch::Cli` has an unknown name, `parse_invocation` prints its own error and exits 2. That branch is unreachable, because every spec subcommand maps to a parser. It is dead code that hides spec/parser drift at runtime; a test would catch that drift better.
