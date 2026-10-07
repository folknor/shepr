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

## LIFE-021 - `status --all server` and `status --all client` are refused with a generic parser message

Raised as a lateral finding while closing the silent drop of `--all`.

`status::parse` (`src/cli/status.rs`) now refuses `--all` together with the
`server` or `client` subcommand, and the exit is a usage error as it should
be. But the refusal surfaces through the typed-parser fallback in `src/cli.rs`,
so the operator reads the generic "does not match a typed parser" message
rather than one saying that `--all` cannot be combined with a subcommand.
Making it a clap conflict, or giving that refusal its own message, would fix
it.

## LIFE-003 - A launch that ends on another server prints a false "the local server started, but reported this" notice

Where: `crates/shepr-launch/src/local_server.rs`, `launch_with`,
`ready_boot_notice`, `ensure_running`; `crates/shepr-platform/src/daemon.rs`,
`open_boot_log`.

Claim broken: `ServerReady::boot_notice` is documented as "What a server this
call launched wrote to its boot log before it was ready ... `None` when this
call launched nothing", and the notice text says the local server "reported
this before it was ready", usually meaning it has no server log.

What happens: the boot log is emptied once, when `launch_with` opens it. Two
paths then leave another daemon's stderr in it:

- Our daemon exits `AlreadyRunning` and an external server of this build then
  answers. `launch_with` returns that occupant as success. Its boot log holds
  our daemon's `error: shepr-server is already running` / `socket: ...` lines,
  so `ensure_running` prints "shepr: the local server started, but reported
  this before it was ready ... error: shepr-server is already running", and
  `launch_with` logs a WARN that the server "may have no server log".
- The respawn loop (`DAEMON_RESTART_INTERVAL`) starts a second daemon that
  boots fine and points its stderr at /dev/null; the first daemon's "already
  running" lines are still in the file, so the same false notice fires for a
  healthy server that has a log.

Fix direction: truncate the boot log (through the launcher's handle) before
each respawn, and only produce `boot_notice` when the accepted status's boot id
names the daemon this call spawned (the `boot_id_process_id` check already
exists).

## LIFE-004 - Machine labels are unique only case-sensitively, but the client compares names case-insensitively

Where: `crates/shepr-config/src/validated.rs`, `machine_label_diagnostics`;
`crates/shepr-config/src/machine.rs`, `MachineLabel::same_name`.

Claim broken: `same_name` documents that labels "name the same server as the
client compares names: equal apart from ASCII case, so a sidebar rule matched
with `ignore_case` never confuses them"; docs/config.md says "Labels must be
unique".

What the code does: the duplicate check keys a `HashMap` on the exact
`MachineLabel`, so `[[machines]] label = "Build"` and `label = "build"` both
validate as distinct machines. The local-entry match uses `same_name` (ASCII
case-insensitive), so the file is internally inconsistent: two entries that
differ only in case are "the same server" when compared with the local label
and different servers when compared with each other. Any `ignore_case` sidebar
rule on `machine` then cannot tell them apart, which is exactly what
`same_name` says must not happen. Also, two entries that both match the local
label in different case ("desk" and "DESK") are both silently skipped and only
the first one's palette is used; that duplicate is never reported.

Fix: make the uniqueness check use `same_name` (and report a second local
entry as a duplicate).

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

## LIFE-012 - Launch and stop worst-case budgets leave out the liveness connect timeout

Where: `crates/shepr-launch/src/limits.rs` (`START_WORST_CASE`,
`STOP_WORST_CASE`), `crates/shepr-launch/src/status.rs`
(`read_server_presence_at`), `crates/shepr-platform/src/ipc.rs`
(`socket_is_live` -> `connect_local_stream`, `LOCAL_CONNECT_TIMEOUT` 5 s).

`START_WORST_CASE` budgets each phase-boundary probe as one
`STATUS_REQUEST_TIMEOUT` (2 s). A presence probe is a liveness connect (up to
5 s against a full backlog), the ping (2 s), and on no answer a second
liveness connect (up to 5 s), so one probe can take about 12 s. The same
applies to stop: `wait_until_socket_stopped_or_new_boot` and
`wait_until_stopped_until` call `socket_is_live` after their deadline checks,
each able to overshoot by the 5 s connect. shepr-remote derives its bridge and
remote-stop SSH timeouts from these exported "worst case" constants, so a
wedged local server with a full backlog can make the remote side give up
before the remote launcher or stop has finished. Either bound the liveness
connect by the remaining phase budget or count it in the worst cases. See also
SRV-003 and RMT-007, which are about other budgets on the same stop and
remote-command paths.

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
