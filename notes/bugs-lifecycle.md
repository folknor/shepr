# Bugs: lifecycle, config and CLI (shepr-launch, shepr-paths, shepr-platform, shepr-config, root binary)

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

Filed from the lifecycle, config and CLI hunt. The raw report is in commit
6dc81572 (`notes/hunt-lifecycle-cli.md`).

## LIFE-001 - `stop --all` run from a pane of the local server loses its local row and exit status

Residue. `src/cli/stop.rs` now prints the remote rows before the local stop
starts, so remote results survive. Run from a shell inside a pane of the local
server, the local stop still ends that shell and the `shepr stop --all`
process itself (the PTY closes, the group gets SIGHUP) before the local row is
written, so the local row and the exit status (AGENTS.md: "exits 0 only when
every host ended with no server") are lost.

Direction: refuse `stop --all` from a pane of the local server, or detach the
local leg (ignore SIGHUP and report to something that outlives the pane).

## LIFE-006 - `detect explain --file --agent <label>` accepts any label and exits 0

Where: `src/cli/spec.rs` (`option("agent", "LABEL")` has no value parser),
`src/cli/detect.rs`, `explain_file`;
`shepr_detect::manifest::explain_for_label`.

Claim broken: spec.rs module docs: "Value validation lives here as value
parsers, so a bad value is a usage error (exit 2) instead of a transport
error."

What happens: a typo (`--agent claud`) is not a usage error; it prints
`agent: claud`, `state: unknown`, `fallback_reason: unknown_agent` and exits 0,
which reads like a real verdict over the capture. Give `--agent` a value parser
over the known manifest labels.

## LIFE-014 - `shepr stop` (and status) fail on an unusable `XDG_CONFIG_HOME` they never read

Where: `crates/shepr-paths/src/app_paths.rs`, `resolve_paths_from_env`;
`src/cli.rs`, `run_with_paths`.

AGENTS.md: CLI subcommands read neither config file. `AppPaths::resolve` still
resolves and validates the config directory, so a relative or padded
`XDG_CONFIG_HOME` makes `shepr stop` fail with "application paths could not be
resolved" before it reaches the socket. `stop_does_not_load_a_broken_config`
covers a broken file but not a broken config location. Stopping a server is the
recovery path and should not depend on a directory it does not use; resolve
the config directory lazily or only where it is read.

## LIFE-022 - "May still be saving" after the server already confirmed its final save

Raised as a lateral by a wave 2 fixer.

`crates/shepr-launch/src/guidance.rs` says a timed-out stop's server "may still
be saving". The stop now gets its answer (carrying the final save result)
before it waits for the socket to go, so when the answer confirmed the save
and only that later wait timed out, the sentence is wrong.
