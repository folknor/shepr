# Defect hunt: lifecycle, config and CLI

Scope: `shepr-launch`, `shepr-paths`, `shepr-platform`, `shepr-config`, the root
`shepr` binary (`src/`), with `docs/config.md` and `docs/clipboard.md`. Findings
are ordered roughly by how much they mislead or break. Each names the claim it
breaks and which side (code or document) looks wrong.

## L1. `stop --all` run from a pane of the local server loses its own report and exit status

Where: `src/cli/stop.rs`, `stop_everywhere` and `local_stop`.

Claim broken: the `stop_everywhere` doc comment says "The local one stops last,
so a TUI attached to it is still up while the machines report", and AGENTS.md
says `stop --all` "prints a line per host, and exits 0 only when every host
ended with no server".

What the code does: every row, the remote ones included, is printed only after
`local_stop` has returned. Nothing reports "while the machines report"; the
order buys nothing for an attached TUI. Worse, the natural place to run
`shepr stop --all` is a shell inside a shepr pane. The local stop ends every pane
process of that server, including the shell and the `shepr stop --all` process
itself (the PTY closes and the process group gets SIGHUP), before a single row
is written. The operator sees nothing and the exit status is lost, for every
host, not just the local one. Same for `shepr stop` in a pane, but there the
outcome is self-evident; for `--all` the remote results are the point.

Fix direction: print each remote row as soon as the remote leg finishes (or at
least print all remote rows before starting the local stop), and either refuse
`stop --all` from a pane of the local server or detach the local leg (ignore
SIGHUP, write the summary before the local stop). The doc comment is wrong as
written either way.

## L2. Preflight notices tell the operator a refused login "keeps retrying"; it never does

Where: `crates/shepr-launch/src/guidance.rs`, `machine_preflight_notice`
(`AuthenticationFailed`, `AuthenticationRefused`) and `failure_client_action`;
used by `src/preflight/words.rs`, `result_notices`.

Claim broken: AGENTS.md ("after startup a refusal is never retried by itself,
since repeated refused logins can get the client's address banned"),
docs/config.md ("Needs SSH login ... The client does not retry by itself"), and
the client's own behaviour (`shepr-client/src/endpoint/supervisor.rs`,
`record_failure`, clears `next_attempt` for `FailureDisposition::Authentication`).

What the code says:
- `AuthenticationRefused`: "machine X still refuses the client's connection
  after ssh authenticated: ... The client keeps retrying it." This is exactly the
  Authentication disposition the supervisor stops retrying.
- `AuthenticationFailed`: "authentication for machine X failed: ... The client
  keeps retrying it." When the check was a refusal (not a timeout), the client
  will show a login entry and wait for the operator.
- `failure_client_action(FailureDisposition::Authentication)` (reachable as
  `FailureDisposition::client_action`) returns "... it keeps retrying it", which
  is false for that disposition.

The guidance is the side that is wrong. The text should depend on the
disposition: Authentication says "choose the machine's login entry once access
is fixed", PossibleAuthentication may say it is retried.

## L3. A launch that ends on another server prints a false "the local server started, but reported this" notice

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
  answers. `launch_with` returns that occupant as success. Its boot log holds our
  daemon's `error: shepr-server is already running` / `socket: ...` lines, so
  `ensure_running` prints "shepr: the local server started, but reported this
  before it was ready ... error: shepr-server is already running", and
  `launch_with` logs a WARN that the server "may have no server log".
- The respawn loop (`DAEMON_RESTART_INTERVAL`) starts a second daemon that boots
  fine and points its stderr at /dev/null; the first daemon's "already running"
  lines are still in the file, so the same false notice fires for a healthy
  server that has a log.

Fix direction: truncate the boot log (through the launcher's handle) before each
respawn, and only produce `boot_notice` when the accepted status's boot id names
the daemon this call spawned (the `boot_id_process_id` check already exists).

## L4. Machine labels are unique only case-sensitively, but the client compares names case-insensitively

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
differ only in case are "the same server" when compared with the local label and
different servers when compared with each other. Any `ignore_case` sidebar rule
on `machine` then cannot tell them apart, which is exactly what `same_name`
says must not happen. Also, two entries that both match the local label in
different case ("desk" and "DESK") are both silently skipped and only the first
one's palette is used; that duplicate is never reported.

Fix: make the uniqueness check use `same_name` (and report a second local entry
as a duplicate).

## L5. The "spec and parser disagree fail loudly" guard is debug-only

Where: `src/cli/matches.rs`, `src/cli/spec.rs` module docs.

Claim broken: spec.rs says "`matches.rs` rejects an id the spec never declared,
so a spec and a parser that disagree fail loudly instead of drifting", and the
matches.rs test `missing_match_is_not_fabricated_as_an_empty_value` asserts it.

What happens: clap 4's `ArgMatches::try_get_one` checks the id only under
`#[cfg(debug_assertions)]` (`clap_builder` `verify_arg`). A release build (the
installed `shepr`) reads an undeclared id as `Ok(None)`, i.e. "absent", which is
the silent drift the comment says cannot happen. The tests only ever run the dev
profile, so they cannot notice. Either state that the guard is a dev-build
check backed by tests, or verify ids against `Command::get_arguments` at parse
time.

## L6. `detect explain --file --agent <label>` accepts any label and exits 0

Where: `src/cli/spec.rs` (`option("agent", "LABEL")` has no value parser),
`src/cli/detect.rs`, `explain_file`; `shepr_detect::manifest::explain_for_label`.

Claim broken: spec.rs module docs: "Value validation lives here as value
parsers, so a bad value is a usage error (exit 2) instead of a transport error."

What happens: a typo (`--agent claud`) is not a usage error; it prints
`agent: claud`, `state: unknown`, `fallback_reason: unknown_agent` and exits 0,
which reads like a real verdict over the capture. Give `--agent` a value parser
over the known manifest labels.

## L7. `status --all server` and `status --all client` silently drop `--all`

Where: `src/cli/status.rs`, `parse`.

`--all` is declared on the `status` root, so clap accepts `shepr status --all
server`. `parse` reads `all` and then, for the `server` and `client`
subcommands, builds `Command::Server { json }` / `ParsedCommand::Client` without
it: the flag the operator typed is ignored. Every other unknown or misplaced flag
is a usage error (`unknown_commands_flags_and_arguments_are_rejected`). Make
`--all` conflict with the subcommands, or move it so clap rejects the combination.

## L8. A mid-file BOM is silently repaired instead of failing the launch

Where: `crates/shepr-config/src/io.rs`, `normalize_utf8_bom`.

Claim broken: AGENTS.md "Any config problem fails the launch; no fallbacks";
docs/config.md "Problems include: TOML syntax errors ...".

A U+FEFF at the start of a non-first line is a TOML syntax error. The loader
deletes it and parses the edited text, so a file `toml` itself rejects is
accepted. (A single leading BOM is legal TOML and is fine to strip.) Either drop
the mid-file repair or document it as a deliberate exception; as written the
code contradicts the no-fallback rule.

## L9. Stale statements in the default config templates

Where: `crates/shepr-config/src/default-client.toml` (and docs/config.md, see
last bullet).

- Header: "Pane shells, sessions, and whether panes have borders belong in
  server.toml". Pane borders are always drawn (AGENTS.md, default-server.toml,
  docs/config.md); server.toml only chooses shared divider vs adjacent borders
  (`pane_gaps`). The client template is wrong.
- `[[machines]]` comment: "Entries of an unreachable machine lose the colour."
  AGENTS.md says a machine that is not connected shows a state entry "and no
  workspaces or agents of its last snapshot", so there are no entries to lose
  colour. Wrong as written.
- Navigate-mode comment: "a machine's Connect or Restart entry in place of its
  workspaces" omits the login entry, which docs/config.md and AGENTS.md list as
  a navigate stop.
- docs/config.md "Host colours": "The local server's entries lose the colour
  while it reconnects." Worth checking against the client: if the local server
  follows the same not-connected rule as machines, this sentence describes
  entries that are not shown either.

## L10. Profile and state-directory docs misdescribe where client state lives

Where: `crates/shepr-paths/src/profile.rs` (`BuildProfile` docs),
`crates/shepr-paths/src/app_paths.rs` (`state_dir` docs),
`crates/shepr-paths/src/layout.rs` (`ssh_metadata_directory` docs).

- profile.rs: "Both config files and the client-owned state stay shared by every
  profile." The client log and the SSH metadata cache live in a per-profile
  `shepr-client` / `shepr-dev-client` directory (the paths tests assert it), so
  most client-owned state is not shared.
- app_paths.rs: `state_dir` "holds client-owned state". For a release build
  `state_dir` is the server's leased data directory itself (`data_dir ==
  state_dir`), so the client's sidebar preferences
  (`shepr-client` `preferences::path_for_local_endpoint`, `client-shell/local-*.json`)
  are written inside the release server's leased data tree. layout.rs gives the
  SSH cache a separate directory precisely to stay "outside the server's leased
  data tree"; the preferences file breaks that rule for release only. Either move
  the preferences under the client state directory or correct both docs.

## L11. Operator guidance that is wrong for a socket override

Where: `crates/shepr-launch/src/guidance.rs`, `server_not_running`,
`cli_build_mismatch`; callers in `src/cli.rs` (`map_server_not_running_or_io`,
`ensure_server_build_matches`).

- `server_not_running`: "run `SHEPR_SOCKET_PATH=... shepr` to start or attach
  it". AGENTS.md: a socket override "names an existing server: the TUI attaches
  to it but never starts a server there". For an override the advice cannot
  start anything; `no_server_at_override` already has the right wording.
- `cli_build_mismatch`: "restart the server with this build before using this
  command", followed by `build_mismatch_guidance`, which for an override says
  "This shepr cannot start a server at the selected socket override, so it
  cannot restart this address". The two sentences contradict each other in one
  message.

## L12. Launch and stop worst-case budgets leave out the liveness connect timeout

Where: `crates/shepr-launch/src/limits.rs` (`START_WORST_CASE`,
`STOP_WORST_CASE`), `crates/shepr-launch/src/status.rs`
(`read_server_presence_at`), `crates/shepr-platform/src/ipc.rs`
(`socket_is_live` -> `connect_local_stream`, `LOCAL_CONNECT_TIMEOUT` 5s).

`START_WORST_CASE` budgets each phase-boundary probe as one
`STATUS_REQUEST_TIMEOUT` (2s). A presence probe is a liveness connect (up to
5s against a full backlog), the ping (2s), and on no answer a second liveness
connect (up to 5s), so one probe can take about 12s. The same applies to stop:
`wait_until_socket_stopped_or_new_boot` and `wait_until_stopped_until` call
`socket_is_live` after their deadline checks, each able to overshoot by the
5s connect. shepr-remote derives its bridge and remote-stop SSH timeouts from
these exported "worst case" constants, so a wedged local server with a full
backlog can make the remote side give up before the remote launcher or stop has
finished. Either bound the liveness connect by the remaining phase budget or
count it in the worst cases.

## L13. `--help` says the server and client logs are "the only two any process writes"

Where: `crates/shepr-platform/src/logging.rs`, `help_log_paths_summary`; used by
`src/cli.rs`, `print_help`.

The client-spawned server's boot log (`server-boot.log` in the runtime
directory) is a log file too, and for a server whose own log could not be opened
it is the only record (local_server.rs and shepr-daemon say so). `shepr --help`
lists config and log paths for exactly this kind of diagnosis and omits it. Name
the boot log, or reword the doc comment.

## L14. `shepr stop` (and status) fail on an unusable `XDG_CONFIG_HOME` they never read

Where: `crates/shepr-paths/src/app_paths.rs`, `resolve_paths_from_env`;
`src/cli.rs`, `run_with_paths`.

AGENTS.md: CLI subcommands read neither config file. `AppPaths::resolve` still
resolves and validates the config directory, so a relative or padded
`XDG_CONFIG_HOME` makes `shepr stop` fail with "application paths could not be
resolved" before it reaches the socket. `stop_does_not_load_a_broken_config`
covers a broken file but not a broken config location. Stopping a server is the
recovery path and should not depend on a directory it does not use; resolve the
config directory lazily or only where it is read.

## L15. `stop --all` labels a stopped local server whose final save failed as "stop failed"

Where: `src/cli/stop.rs`, `local_stop`.

`ServerStopError::FinalSaveFailed { stop_error: None }` means the server
stopped and its layout save failed. `local_stop` turns every non-NotRunning
error into `HostStop::Failed(format!("stop failed: {error}"))`. The host did end
with no server; the row should say "stopped; final save failed: ...", as
`guidance::local_notice` already does for the restart path. Exit status 1 is
defensible, the wording is not.

## Lateral notes (outside the strict scope, or smaller)

- `src/cli/stop.rs` / AGENTS.md: the local leg of `stop --all` is
  unconditional (`stop_active_server(paths, None)`), which matches "whatever
  answers", but because remote rows are not printed first (L1), a hung local
  stop (up to 15s plus 10s lease wait) also delays every remote result.
- `crates/shepr-launch/src/stop.rs`, unconditional stop: success is "the socket
  stopped accepting connections". A successor that binds the socket within the
  stop's wait (any client that auto-launches) turns a successful stop into
  `TimedOut` with "the server may still be saving its layout" wording. Low
  likelihood today because only operator actions start servers, but the error
  text would be misleading.
- `crates/shepr-platform/src/logging.rs`, rotation: the size check uses the
  cached `current.size` between path rechecks, while several processes (TUI,
  every remote bridge, `remote-wait-for-server`) append to the same client log.
  The cap can overshoot by up to `PATH_RECHECK_AFTER_WRITES` writes per
  process. Probably acceptable for a soft cap; noting it because the doc
  comment presents the flock coordination as making rotation exact.
- `crates/shepr-config/src/keybinds.rs`, `parse_binding_string`: the `prefix+`
  marker is matched case-sensitively, while docs/config.md says key "Names are
  case-insensitive". `Prefix+z` fails as an unknown key rather than being read as
  the prefix. Either accept it or say `prefix` is lowercase-only.
- `crates/shepr-config/src/machine.rs`, `SshTarget::parse`: the password check
  looks for `:` in everything before the last `@`, after stripping only a
  leading `ssh://`. An `ssh://host/path@x` style string or an alias containing
  `@` and `:` is classified on that heuristic; harmless in practice, noted for
  completeness.
- `src/main.rs`, `ProcessExit::from_cli_code`: `AlreadyRunning` and
  `ConfigRefused` codes are logged as invalid CLI statuses and mapped to
  `Failed`; correct today because nothing in the CLI returns them, but the
  mapping relies on that by convention only.
