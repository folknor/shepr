# Hunt: server lifecycle, seen from outside and at startup

Scope read in full: `crates/shepr-launch/src/*`, `crates/shepr-paths/src/*` (and
its `build.rs`), `crates/shepr-api/src/**`, `crates/shepr-daemon/src/main.rs`,
`crates/shepr-server/src/server/headless.rs`, `headless/bootstrap.rs`,
`headless/api_dispatcher.rs`, and the CLI in `src/` (`main.rs`, `cli.rs`,
`cli/{status,stop,fleet,error,spec,matches}.rs`, `tui.rs`, `preflight.rs`,
`preflight/words.rs`, `limits.rs`, `test_support.rs`). Followed into
`shepr-platform` (`daemon.rs`, `ipc.rs` accept/lease), `shepr-core/src/env.rs`,
`shepr-remote` (`fleet.rs`, `host.rs`, `server_lifecycle.rs`, `ssh.rs`,
`limits.rs`), `shepr-mux` (`persist/lock.rs`, `pane/launch.rs`), `brokkr.toml`,
`clippy.toml`.

Findings are not ranked. Each names the claim it breaks (for defects) and
whether the fix can be enforced mechanically.

---

## 1. Defects

### D1. An unresponsive or slow remote server is reported as "needs an SSH login"

`src/limits.rs` `STATUS_ANSWER_TIMEOUT` is 20 s; the remote `shepr status
--json` and `shepr status server --json` wait that long for `ping` (and
`status --json` up to another 20 s for `server.summary`). They are run over SSH
by `shepr-remote` through `RemoteSsh::sh_output`, whose per-command cap is
`SSH_COMMAND_TIMEOUT` = 15 s (`crates/shepr-remote/src/limits.rs`). When the
command uses its full budget, `command_timeout` marks it
`authentication_candidate` and it is classified
`SshFailureClass::AuthenticationPending`.

- `status --all`: `cli/fleet.rs::failure_label` turns `PossibleAuthentication`
  into "needs an SSH login: run `shepr` or ssh to it". AGENTS.md promises
  `status --all` prints the server's state, including "not answering"; a
  remote server that listens but does not answer is instead reported as a
  login problem.
- `stop --all`: `fleet::stop_plan` has an explicit `Unresponsive` arm ("the
  server there is not answering... stop it on that host") that is unreachable
  for exactly the case it was written for; the operator again sees "needs an
  SSH login".
- `server_lifecycle::parse_remote_server_status_json` documents "A server that
  listens but does not answer fails the check", but the SSH command dies first
  and the failure is a possible-authentication wait. In the preflight/TUI path
  AGENTS.md says such a result gets the foreground interactive ssh attempt, so
  a wedged remote server triggers an interactive SSH prompt.
- Same for a remote server of this build whose app loop is stalled: `ping`
  answers at once (connection thread), but `server.summary` waits for the
  server's 15 s `ORDINARY_REQUEST_TIMEOUT`, so `status --json` exceeds 15 s.

The local CLI budget was chosen with a false rationale (see C3: `ping` never
goes through the app loop). Fix: one owner for "how long a status probe waits"
(launch's `STATUS_REQUEST_TIMEOUT`, 2 s, is the right order) and make the
remote side's budgets derive from it, with `summary` given a short bound of
its own or skipped when the ping already took long. Enforceable with a
`const _: () = assert!(...)` in `shepr-remote/src/limits.rs` that the remote
status command's worst case (exported from launch/the binary) is below
`SSH_COMMAND_TIMEOUT`; today the CLI value lives in the binary crate where
remote cannot see it, which is itself the structural problem.

### D2. A listener that dies leaves a deaf server that defeats every launcher

`crates/shepr-api/src/server.rs::start_server` says "a dead one leaves a
server that answers neither the CLI, agent hooks nor TUI attaches: it must
outlive every accept and spawn failure". `listener.rs` breaks out of the
accept loop on `Accepted::Fatal` (EBADF, EINVAL, ENOTSOCK from
`classify_accept_failure`), logs one `error!`, and exits. The refuser thread's
`rx` then ends, the last `ApiRequestSender` clones drop, and
`HeadlessServer::next_loop_event` sees the API channel closed, logs "API
request channel closed; API requests are no longer served" and keeps running.
Nothing initiates shutdown.

Consequences: the socket file stays (owned, not removed) but nothing accepts,
so `socket_is_live` reads stale -> `Gone`; `shepr stop` reports "not running";
a new `shepr` takes the launch lock and starts a daemon that exits
`AlreadyRunning` on the data-directory lease every 500 ms until
`SERVER_READY_TIMEOUT`, ending in a misleading "found another server already
running, but that server did not answer" or boot-timeout message. Only SIGTERM
or SIGKILL by pid recovers, and pid is only discoverable through the server
log. The `api_request_open = false` comment ("which also means the socket is
dead") recognises the state and chooses to continue.

Fix: listener death (and API channel closure) must latch the stop signal
(`ServerStopSignal::request`) so the server saves and exits; the stop then
releases lease and socket in the documented order. Enforceable by a test that
closes the listener fd and asserts the server reaches `ShutdownPhase::Stopping`.

### D3. A starting different-build local server is never offered a restart

AGENTS.md: "A running local server of a different build is then offered a
restart". `preflight::local_server_status` -> `running_server_status` returns
`None` for `Starting` and `Stopping`. If the local server of another build is
still restoring when `shepr` runs (common right after a reboot or after
another client just started it), no offer is made; `ensure_running` then
waits through the transition, finds `Running(other build)` and fails with
`DifferentBuild` (or, with machines configured, prints the notice and keeps
going without a local server). The operator must stop it by hand, the case the
consent flow exists to avoid. Fix: let the preflight wait through `Starting`
(as the launcher does) before deciding, or have the offer accept a
`Starting` status (it already names build and boot). A test in
`preflight.rs` with a scripted `Starting` probe would pin it.

### D4. `ensure_running` requires the sibling executable before waiting out a starting server

`local_server.rs::ensure_running` resolves `server_executable()` (which fails
when `shepr-server` is missing or not executable) before it takes the lock and
before it waits through `Starting`/`Stopping`. A client whose sibling is
missing (a dev build after `brokkr run` without the server, or a half-finished
install) fails with an install error while another client's server is a
moment from ready, although AGENTS.md says "A launcher waits through
starting". Move the executable lookup to just before `launch_daemon`, after
the transition wait has returned `NoServer`.

### D5. `SHEPR_BIN_PATH` names `shepr-server`, not `shepr`

`shepr_core::env::ChildEnv::SheprBinPath` is documented as "the shepr
executable, set for every pane so programs in it can call back into shepr".
`shepr-mux/src/pane/launch.rs` sets it from `shepr_platform::launch_executable()`
inside the server process, which is the `shepr-server` binary since the
client/server split. Anything running `$SHEPR_BIN_PATH status` gets the
server's usage error. Nothing in the repository reads the variable (only
`env.rs` mentions it), so it is also dead (see N1). Either set it to
`launch_executable().with_file_name(PROGRAM_NAME)` or delete it.

### D6. A foreground server's ready notice names the wrong entry point in a dev build

`bootstrap.rs` `ServerReady`'s `Display` says "run `shepr`, which starts the
server itself". AGENTS.md: guidance "uses `shepr` for a release build and the
running executable path for a dev build". A dev `shepr-server` run in the
foreground (`brokkr run shepr-server`, which AGENTS.md documents) tells the
operator to run the installed release `shepr`, which talks to a different
server. Use `shepr_launch::guidance` (with the sibling client path for a dev
build); this also moves operator text out of shepr-server (see C-items under
4).

### D7. The CLI's response reader never checks the response id

`shepr-api/src/client.rs::read_json_line` decodes whatever line arrives;
`request_value*` never compare `SuccessResponse.id`/`ErrorResponse.id` with
the request's id. Today one request per connection makes a mismatch
unlikely, but the test fakes already prove nothing checks it:
`local_server_tests.rs::serve_pong_once` answers with
`"autodetect:server:status"`, an id no production code sends (the ping is
`"api-client:status"`), and every test passes. A typed `Request`/response
pairing (the client compares and refuses a mismatch as `InvalidData`) closes
it; a test with a wrong id then fails.

### D8. `ServerStopIfBootParams` alone among the control params accepts unknown keys

`schema/server.rs`: `ServerStopParams` and `ServerSummaryParams` carry
`deny_unknown_fields`; `ServerStopIfBootParams` and `PingParams` do not. The
`Request` decoder's comment and `api_service.rs::stop_server`'s comment both
claim a stop is conditional "only when its one guard is where this method
reads it", built on refusing stray keys. A `server.stop_if_boot` with a
misspelled extra key (for example `expected_boot` beside a correct
`expected_boot_id`) decodes silently. Not unsafe today, but it is the one
cross-build request and the policy is per struct. Put `deny_unknown_fields` on
every socket-route params type; a schema test that iterates the socket route
and sends an extra key enforces it.

### D9. Smaller defects

- `local_server.rs::LaunchError::remote_failure_class` comment: "`launch_with`
  waits out a daemon that gave way to another server rather than failing on
  it" is given as the reason `DaemonExit::Clean` is `Retry`. `launch_with`
  only exempts `AlreadyRunning`; a daemon that exits 0 during boot fails the
  launch at once. The classification may still be right, but its stated reason
  is false.
- `ServerAddress::for_runtime_dir` treats an override equal to the runtime
  socket as no override by lexical `Path` equality. A pane-exported socket that
  reaches the same file through a symlinked `XDG_RUNTIME_DIR` (or the logind
  fallback vs an exported `/run/user/<uid>` spelled differently) reads as an
  override: `status` prints "set by SHEPR_SOCKET_PATH", the TUI will not start
  a server there and the restart offer is skipped. Compare canonical paths, or
  compare the inode once the socket exists.
- `stop.rs`: the request id is always `"cli:stop"`, also when the TUI's
  restart offer or a remote `--expect-boot` stop sends it; server logs cannot
  tell an operator stop from the startup restart.
- `api_service.rs`: the same condition (the app loop is saturated) is refused
  with two different codes: `EndpointBusy` when the app-slot admission is full,
  `ServerUnavailable` ("server is busy handling API requests; retry later")
  when the channel is full. A caller that retries on one and gives up on the
  other behaves differently for one cause.
- `ApiErrorCode`: `BuildMismatch`, `ServerNotRunning`,
  `AgentExplainFileReadFailed` and `ServerStopFailed` are never produced by a
  server; the CLI fabricates `ErrorResponse` envelopes with them
  (`cli.rs::ensure_server_build_matches`, `map_server_not_running_or_io`,
  `cli/error.rs` for stops). The wire vocabulary carries CLI-local failures,
  and `shepr stop` prints a JSON envelope on stderr to a human operator.

---

## 2. One value, one owner

### C1. Process exit codes have three owners

- `shepr_launch::daemon_exit`: 0 (inline in `code()`), `FAILED_EXIT_CODE` 1,
  `ALREADY_RUNNING_EXIT_CODE` 10, `CONFIG_REFUSED_EXIT_CODE` 11.
- `shepr_launch::stop`: `NO_SERVER_EXIT_CODE` 4, `BOOT_MISMATCH_EXIT_CODE` 3
  (private consts behind `ServerStopExit`).
- `src/main.rs::ProcessExit`: literals 0, 1, 2 in both `code()` and
  `from_cli_code`; `cli.rs::parse_launch` returns a literal `Err(2)`;
  `crates/shepr-daemon/src/main.rs::ServerProcessExit::Usage` is a literal 2
  and its fallback a literal 1 (`unwrap_or(1)`, also in `ProcessExit`).

Not diverged yet, but usage 2 and failure 1 are spelled five times in two
binaries, and `DaemonExit`'s doc ("the server executable exits with one of
these codes") is false for the daemon's usage exit 2. One `ProcessStatus`
enum in `shepr-launch` covering every status either executable ends with
(and `u8` typed, removing the `unwrap_or(1)` fallbacks) fixes it. Enforceable
by a textlint banning `ExitCode::from(` and `Err(2)`-style numeric literals in
`src/main.rs`, `src/cli.rs` and `crates/shepr-daemon/src/main.rs`, or by the
type alone.

### C2. The executable names are re-spelled

`SERVER_BINARY_NAME` exists, yet `"shepr-server"` is literal in
`stop.rs::ServerStopError::TimedOut`'s message and in
`shepr-daemon/src/main.rs::report_server_error` (`"shepr-server is already
running"`). `"shepr"` is literal in `cli/error.rs` (`"run 'shepr --help' for
usage"`), `bootstrap.rs::ServerReady`, and `PROGRAM_NAME` and
`REMOTE_INSTALL_NAME` are two constants of one value (AGENTS.md: a host never
has more than one `shepr`). Fold `REMOTE_INSTALL_NAME` into `PROGRAM_NAME`
unless the owner wants them to diverge; route the rest through the constants
or, for operator commands, through `guidance::operator_entrypoint`.
Enforceable with a textlint for `"shepr-server` and `` `shepr `` inside string
literals outside `invocation.rs` and `guidance.rs` (region would need to be
literal text, as `no-relative-dot-directory-path` does).

### C3. Four budgets for one `ping`, and the copies have already diverged

The question "does a server answer at this socket, and as what" has these
timeouts: `STATUS_REQUEST_TIMEOUT` 2 s (launch, bridge, remote wait),
`STATUS_ANSWER_TIMEOUT` 20 s (`src/limits.rs`, `shepr status`),
`ORDINARY_RESPONSE_TIMEOUT` 20 s (`ApiClient::ping`, used by the CLI's
pre-request build check in `cli.rs`), `STOP_STATUS_PROBE_TIMEOUT` 250 ms
(stop). `STATUS_ANSWER_TIMEOUT`'s doc claims it "matches the API client's
ordinary response window ... so a slow but working loop still answers in
time"; `ping` is answered on the connection thread (`api_service.rs::
route_request`) and never waits for the loop, so the rationale is false and
the copy is a restatement of a `pub(crate)` value the binary cannot even
name. The divergence is what produces D1. One owner (launch) with named
derivations; a `const` assert in remote that its SSH command budget exceeds
the remote status worst case.

### C4. `REMOTE_STOP_SSH_TIMEOUT` restates launch's stop budgets

`shepr-remote/src/limits.rs` says the 45 s "covers" the remote `shepr stop`'s
own deadline plus the connection. That deadline is `STOP_WAIT_TIMEOUT` (15 s)
+ `STOP_LEASE_WAIT_TIMEOUT` (10 s) + a final probe, all `pub(crate)` in
`shepr-launch`. In step today (about 25 s + `ConnectTimeout=10`), but nothing
notices if either grows. Export a `STOP_WORST_CASE` from launch and assert in
remote, as `BRIDGE_IDLE_TIMEOUT` already does against `HEARTBEAT_INTERVAL`.

### C5. A remote Connect's budget is not checked against the launch it triggers

`SSH_CONNECTION_ATTEMPT_BUDGET` (25 s, discovery included) bounds a Connect,
whose remote bridge runs `ensure_running(paths, SERVER_READY_TIMEOUT)`: up to
`SERVER_READY_TIMEOUT` (15 s) for the daemon, plus up to
`SERVER_READY_TIMEOUT + LAUNCH_LOCK_WAIT_GRACE` (20 s) waiting for the launch
lock, plus transition waits. A slow restore on the remote host makes the
client give up while the remote launch is still legitimately running. Either
derive the client budget from launch's exported worst case or bound the
bridge launch by the remaining attempt budget handed down. Assertable.

### C6. The runtime directory's file names have no single owner

`shepr-paths` claims "runtime layout", but: `shepr.sock` is
`address.rs::SOCKET_FILE_NAME`; `launch.lock` and `server-boot.log` are
private consts in `local_server.rs`; the socket's `.lock` sibling is a naming
rule in `shepr_platform::ipc::socket_startup_lock_path`; SSH control sockets
are named in `shepr-remote/src/ssh_paths.rs`; the lease `session.lock` is in
`shepr-paths`, `server.log` in `shepr_platform::logging`; `"client"`,
`"client.toml"`, `"server.toml"` and `"shepr-dev"` are literals in
`app_paths.rs`/`profile.rs`. The test `assert_nothing_was_launched` re-lists
two of them. Nobody can answer "what files does a shepr profile own under
`$XDG_RUNTIME_DIR`". Move every runtime and data file name into `AppPaths`
accessors (`launch_lock_path()`, `boot_log_path()`, ...), with the platform
helpers taking paths. Enforceable by a textlint for `.join("` with a
`.lock`/`.log`/`.sock` literal outside `shepr-paths`.

### C7. Request ids and other wire strings spelled per site

`"api-client:status"`, `"api-client:summary"`, `"cli:stop"`,
`"cli:detect:capture"`, `"cli:detect:explain"` are literals at each call site;
test fakes spell `"autodetect:server:status"`. With D7 fixed, a request type
that mints its id is the natural owner.

### C8. Two spellings of "is this build"

`status.build_id.is_this_build()` (launch, preflight, remote) and
`BuildIdentity::for_this_build().matches(status.build_id)`
(`cli/status.rs::print_full_status`, `render_server`). Same answer today;
pick one.

---

## 3. Values nobody can find, change, or trust

- `STATUS_REQUEST_TIMEOUT`, `STOP_WAIT_TIMEOUT` and `SERVER_READY_TIMEOUT` have
  no injection point at the public entry points (`running_server_status`,
  `server_presence`, `stop_active_server`, `ensure_running`'s probes), so
  tests wait them out: `a_listener_without_a_status_answer_is_unresponsive`,
  `a_silent_listener_reads_as_the_launchs_unresponsive_error`,
  `a_listener_that_does_not_answer_is_never_replaced` each sit through the
  full 2 s. The only parameterised one is `stop_active_server_with_timeout`.
- `launch_with` and `acquire_launch_lock_with` take `now`/`sleep` seams, but
  every test (`launch_fixture`, `launch_after_a_refused_first_daemon`,
  `the_launch_lock_wait_is_bounded...`) passes `Instant::now` and
  `std::thread::sleep`. The seam is unused; the tests run on real time and
  `a_holder_that_never_leaves...` asserts a 2..=4 restart count from
  wall-clock pacing. Either drive them with a fake clock or drop the seam.
- `SOCKET_POLL_INTERVAL` is also the poll of a child process
  (`read_server_version_line`), named for something else.
- `MAX_LOCAL_OFFERS` and `STATUS_ANSWER_TIMEOUT` live in the binary's
  `src/limits.rs`; the restart policy they belong to lives in launch. A person
  tuning the restart offer has to know to look in the binary.
- The set of lifecycle tunables is split over `shepr-launch/src/limits.rs`,
  `shepr-api/src/limits.rs`, `shepr-remote/src/limits.rs`,
  `shepr-server/src/limits.rs` and `src/limits.rs`, with couplings (C3, C4,
  C5) stated only in prose. Nothing answers "which timeouts must stay ordered
  with which".
- `ApiClient` has two request paths with different policies:
  `request_value_with_timeout` (connect bound, write timeout, a read deadline
  that starts after the write) and `request_value_until` (one shared
  deadline). `request`/`ping` use the first; launch and stop use the second.

---

## 4. One channel, one implementation

- Operator text is built ad hoc in many places outside `guidance.rs`:
  `local_server.rs` (`unresponsive_error` appends its own "If that fails, stop
  the server process manually", `running_build_mismatch`,
  `no_server_at_override`, `boot_failure`...), `stop.rs` (`TimedOut` tells the
  operator to SIGKILL by name), `bootstrap.rs::ServerReady`, `cli.rs`
  (`ensure_server_build_matches`), `cli/error.rs` (`Usage`, `Nested`),
  `preflight/words.rs`, `tui.rs::local_startup_notice`. The two manual-kill
  instructions are worded differently for the same situation; D6 is the
  divergence this produces.
- `CliError::Nested` prints raw `\x1b[1m`/`\x1b[2m` escapes unconditionally,
  although `shepr man` honours `NO_COLOR` and a non-terminal stdout.
- `CliError::ServerStop` prints a JSON envelope (`{"error":{...}}`) to an
  operator's stderr for an ordinary "no server running" stop; the human
  `shepr stop` and the SSH caller share one rendering. The SSH caller reads
  only the exit code, so the human form is free.
- Logging: `stop_active_server` logs nothing, so the client log has no record
  of which boot the restart offer stopped, or of a stop that timed out. On the
  server, a successful `server.stop`/`server.stop_if_boot` is logged at
  `debug!` only (`MethodTraits` has `mutates_ui: false`, and
  `api_request_completed` raises to `info!` only on a non-ok outcome), and
  `lifecycle.rs` logs "server shutdown initiated" without the cause (API stop,
  conditional stop with which boot, or signal). Invalid API requests
  (`handle_connection`'s parse-error branch) are answered but not logged at
  all; oversized or non-UTF-8 request lines and read timeouts end at
  `debug!("api connection failed")` with no peer or request id.
- `local_server.rs` logs "server already running" / "server started by
  another client" without the socket path or the build and boot it found.
- Capitalisation of the product in operator text varies ("Shepr TUI" in
  `ServerReady`, "remote Shepr server socket" in `shepr-remote/src/host.rs`,
  "shepr" elsewhere); the `failure.rs` module doc says "the Local server",
  and `tui.rs`'s doc says "the Local endpoint", although AGENTS.md says the
  local server is never named "Local".

---

## 5. Errors

- D2 is the main one: listener death is logged and swallowed.
- `stop.rs`: `impl From<io::Error> for ServerStopError` attaches the context
  "server operation failed"; `send_stop_request` uses it
  (`Err(error.into())`) for a non-wait-able request IO error, so the operator
  sees "server operation failed: ..." instead of "could not send the stop
  request to server at <socket>".
- `stop_socket_io_error` drops the `socket_is_live` error (`Ok(true) | Err(_)`
  both become `Unreachable` with the original connect error).
- `cli.rs::map_server_not_running_or_io` swallows the liveness probe error with
  `unwrap_or(false)`.
- `preflight::local_server_status` reduces every probe failure to a `warn!`
  and no offer; the comment relies on the launch reporting it next, which
  holds only when no machines are configured (with machines, `tui.rs` prints a
  notice and continues).
- `read_runtime_status_at`: a pong that fails to decode (a far older build
  without `boot_id`) is an error, not "a different build", so the launch fails
  with "did not give a usable status answer" and no restart is offered. This
  matches the frozen cross-build surface, but the operator gets no stop
  guidance for that case (`unresponsive_error`'s guidance is only attached to
  `Probed::Unresponsive`).
- `local_server.rs::read_server_version_line` drops `child.kill()`/`wait()`
  errors on timeout; harmless, unlogged.

No site aborts the process on operator-controlled input in this scope; the
`assert!`s in `shepr_core::env::resolve_*` guard programmer errors only.

---

## 6. Tests that prove nothing

- `guidance.rs::the_default_entry_point_is_this_builds`: compares each public
  function with its `_with` form called with `operator_entrypoint()`, which is
  what the public function does. Cannot fail.
- `local_server_tests.rs::server_daemon_runs_in_home_not_the_launch_directory`:
  the expected working directory is computed with the function's own rule
  (`home_dir().or("/")`), then it asserts the builder set the directory it was
  handed, and that it differs from a path nothing could have set. Cannot fail
  for any implementation of `build_server_daemon_command` that calls
  `current_dir`.
- `tui.rs::the_local_startup_notice_carries_the_whole_refusal`: builds the
  error message including the guidance itself, then asserts the notice
  contains it; only `{error}` interpolation is under test.
- `stop.rs::stop_wait_timeout_allows_slow_graceful_shutdown` asserts the
  constant equals 15 s.
- `local_server_tests.rs::serve_pong_once` answers with an id production never
  sends; every launch test passes regardless (D7).
- `local_server_tests.rs::a_vanished_server_reads_as_gone_not_unresponsive`
  duplicates `status.rs::a_listener_that_vanishes_before_answering_is_gone`.
- Wall-clock tests: `repeated_socket_transitions_share_one_wait_deadline`
  (350 ms sleep, `< 225 ms` assertion),
  `api_service.rs::request_line_arriving_after_connect_is_read_without_a_poll_delay`
  (`< 100 ms`), `a_holder_that_never_leaves...` (2..=4 restarts),
  `listener.rs::classification_saturation_still_serves_a_peer_whose_kind_has_room`
  (a 50 ms sleep decides which path is exercised: under load the peer may be
  classified directly and the test passes without testing the overflow path
  its name claims).
- Banned-word assertions for removed features (`"--session"`, `"--force"`,
  `"SHEPR_SESSION"` in `guidance.rs` and `tui.rs`; `"capabilities"`,
  `"status"`, `"running"` keys in `cli/status.rs::
  server_status_json_reports_the_running_boot`): they assert the absence of
  strings no code produces; they pass for every implementation.
- Test data borrowed from one developer and `/tmp`:
  `/home/folk/.cargo/bin/shepr` in `cli/status.rs`, `/tmp/shepr-server-test`
  and `/tmp/shepr-test` in `local_server_tests.rs`, `/tmp/shepr-test.sock` in
  `client.rs`. Only compared, never touched, but the repo rule for compared
  paths is `/nonexistent/...`; a textlint like `no-relative-dot-directory-path`
  for `"/tmp/` and `"/home/` in test literals would enforce it.
- `main.rs::args_as_utf8_passes_through_valid_arguments` uses `pane get
  pane-1`, a removed command group (harmless data, stale).

---

## 7. Guards and claims that have stopped holding

False today:
- `shepr-api/src/server.rs`: the listener "must outlive every accept and spawn
  failure" (D2).
- `headless.rs::next_loop_event`: "Production keeps the sender in the listener
  until this loop ends" (false after a fatal accept).
- `src/limits.rs::STATUS_ANSWER_TIMEOUT` rationale (C3).
- `shepr-mux/src/persist/lock.rs`: "Config owns the lease filename" (it is
  `shepr-paths`).
- `local_server.rs::server_daemon_working_dir` names `new_terminal_cwd =
  "current"`; the setting is `terminal.new_cwd` (`docs/config.md`,
  `default-server.toml`).
- `local_server.rs::build_server_daemon_command` doc: the child "gets the
  already-resolved socket target"; it only removes `SHEPR_SOCKET_PATH`, and the
  daemon re-resolves from its own environment.
- `local_server.rs::LaunchError::remote_failure_class` comment about `Clean`
  (D9).
- `DaemonExit` doc: "The server executable exits with one of these codes"; its
  usage error exits 2.
- `shepr_core::env::EnvVar::SheprBuildProfile` doc speaks of "the socket
  variables" and "the socket overrides", plural; there is one.
- `shepr-paths/src/lib.rs` says both pane markers decide whether an inherited
  `SHEPR_SOCKET_PATH` applies; only `SHEPR_BUILD_PROFILE` does (`SHEPR_ENV`
  decides the TUI refusal).
- `cli/spec.rs` module doc: typed parsers read ids "spelled from
  shepr-launch's `COMMAND_` and `FLAG_` constants". The `detect` subcommands
  (`"capture"`, `"explain"`) and options (`"file"`, `"agent"`, `"verbose"`,
  `"pane"`, `"help"`, `"version"`) are literals.
- `headless.rs::dispatch_api_request` comment: "API handlers read each
  workspace's recorded layout area for directional focus, resize steps,
  layout snapshots and spawn sizes"; none of those API methods exists any more
  (the API is ping, stops, summary, detect, two reports).
- `headless.rs::handle_scheduled_tasks_headless`: "Similar to the former App
  scheduler", "No resize polling needed" (history, not behaviour).
- `cli/status.rs` line 662 comment contains a horizontal ellipsis (U+2026),
  a gremlin under the project rule; either `brokkr`'s gremlins check does not
  cover that character or the file is not being swept. Checkable: grep for
  non-ASCII.

Fail-open guards keyed on names:
- `cli.rs::parse_launch` reads `--start` with `matches::flag`, which turns a
  spec/handler mismatch into `false`. `matches.rs`' own doc reserves the
  default-on-error read for root help and version. A rename of `FLAG_START`
  in the spec only (or a typo in the id) silently makes every Connect and
  Restart attach-only. Use `try_flag` and refuse.
- `ServerAddress::for_runtime_dir`'s "override equal to runtime is not an
  override" is a lexical comparison (D9).

Checkable claims nothing enforces: the startup order (`bootstrap.rs` test pins
it, good); the shutdown order lease-then-socket is held by `Drop` order of
`Reserved` and `release_socket_after_save`, with a comment, and pinned only
for the startup failure path.

---

## 8. Policy invented per call site

- IO-error classification is implemented four times with different tables:
  `shepr_platform::ipc::classify_stream_error` (stop, status, disconnect
  notices), `failure.rs::is_link_error_kind` (which treats `ConnectionReset`
  as offline and deliberately differs), `LaunchError::remote_failure_class`
  (its own list of six kinds for Retry), and `stop.rs::
  stop_request_error_allows_wait`. Each is defensible alone; together a
  `ConnectionReset` is "peer gone, retry", "offline" and "retry" depending on
  the caller. Name the questions (link-level reachability, peer left, retry
  the launch) and give each one table.
- Probe budgets per call site (C3).
- `ensure_running`, `wait_for_overridden_server` and
  `wait_for_server_socket_to_settle_until` each compute deadlines from
  `Instant::now()` + timeout; stop computes its own three deadlines
  (`deadline`, `lease_deadline`, `socket_deadline`, then reuses the expired
  `deadline` in the final `wait_until_boot_stops`). The final wait in
  `stop_socket_with_timeout` is given `deadline`, which has usually passed by
  then, so that branch can only return `TimedOut` at once. Either the branch
  is dead or it needs `socket_deadline`.
- Ambient dependencies: `main.rs::random_nested_message` reads the wall clock
  and pid for randomness; `cli/status.rs` reads `SystemTime::now()` twice for
  one report (`overview.now` and the machines section); the CLI is outside the
  clock textlints, which is fine, but the double read means the local and
  machine uptimes can be computed against different instants.
- Wire validation: `deny_unknown_fields` decided per params struct (D8);
  `PaneReportAgentParams` deliberately accepts extra keys (tested); no single
  rule says which API types are strict.
- Unbounded growth: `ServerHandle` is fine; the refuser queue is bounded;
  `MAX_UNCLASSIFIED_CONNECTIONS` bounds threads. `dispatch_to_app_result`
  leaves a timed-out request in the app channel after releasing its app slot
  (the request "may still run"), so after timeouts the app-slot admission no
  longer describes what is queued: abandoned requests occupy channel capacity
  (the same number, 64) that live requests then meet as `ServerUnavailable`.
  This is the only way the channel-full branch (and its second busy code, D9)
  is reachable, which no comment says.

---

## 9. Code that is no longer load-bearing

- N1. `ChildEnv::SheprBinPath` (`SHEPR_BIN_PATH`): set in every pane, read by
  nothing in the repository, and wrong (D5). Also `init_pane_launches` resolves
  the path at startup solely to set it.
- `restart.rs::RestartFailure` has one variant, `Local`; the remote variant is
  gone. Replace with `ServerStopError` directly.
- `LaunchError` payloads never read after construction: `DifferentBuild.status`,
  `SiblingBuildMismatch.status`, `TransitionTimeout.timeout`,
  `BootTimeout.timeout` and `.occupant_only`, `DaemonFailed.status`
  (`ExitStatus`). Only the messages and `DaemonFailed.class` are consumed
  (grep of every `LaunchError::` use outside the crate: `tui.rs` test,
  `preflight.rs` match on `Unresponsive`, `shepr-remote/src/host.rs`).
- `wait_for_overridden_server`'s `Starting | Stopping` arm: the comment admits
  `wait_for_server_socket_to_settle_until` returns only settled states.
- `app_paths.rs::resolve_paths_from_env_with_marker`: the arm returning "paths
  could not be resolved; no path-specific error was reported" is unreachable
  (every `None` pushes a problem).
- `ServerAddress::apply_to_child_command` takes `&self` and ignores it; a free
  function (or a `ServerAddress`-independent child-env helper) says what it is.
- `ServerHandle::remove_socket_file_if_owned` is `pub` but only `Drop` calls
  it; `ApiClient::request_value_with_timeout` is `pub` with no caller outside
  the crate's own tests.
- `connection_health.rs` exists only to re-export `HEARTBEAT_INTERVAL` from
  `limits`.
- `stop.rs`: the `label` parameter of `stop_socket_with_timeout` and every
  `ServerStopError` variant has one value, `"server"`.
- The removed-method and removed-command test lists (`schema/tests.rs::
  removed_methods_are_rejected`, `removed_uncalled_methods_are_rejected`,
  `cli.rs::unknown_commands_and_launch_flags_are_rejected` with `--session`,
  `machine`, `integration`, `config`, `remote-api-bridge`, ...) assert that
  unknown strings are unknown. They protect against resurrection, which the
  owner may value, but they grow with every removal and fail only if
  someone deliberately re-adds a name.
- Cross-build compatibility shims that AGENTS.md keeps on purpose (the
  `#[serde(default)]` on `Pong.stopping`/`starting` and
  `StatusOverviewJson.summary`) are load-bearing for `status --all` against
  older hosts; listed only so they are not mistaken for leftovers.

---

## Lateral observations

- The client's restart flow and `stop --all`'s local stop are unconditional
  vs conditional in different places: `stop --all` stops the local server
  unconditionally while every remote one is stopped by boot. AGENTS.md states
  exactly this, so it is not a defect, but a server that replaced the local
  one between `status` and the stop is stopped without being named.
- `status_probe_has_no_answer` treats a `TimedOut` probe as "gone"; during a
  stop, a server whose 64 ingress slots are full answers no probe within
  250 ms and reads as gone. The later lease and socket waits catch it, so the
  stop does not report success falsely, but it reports `LeaseHeld` or
  `TimedOut` for a server that was merely busy.
- `launch_with` empties the boot log with `set_len(0)` once its daemon is up;
  a daemon from a different launch that never redirected stderr (its log file
  could not be opened, `ServerReady.log_file_unavailable`) keeps the boot log
  as stderr for life, and nothing caps it after `BOOT_LOG_MAX_BYTES` stops
  being checked. That daemon's later stderr (panics included) grows a tmpfs
  file unbounded.
- `STOP_WAIT_TIMEOUT` (15 s) is shorter than an unbounded final save
  (`headless.rs` documents that the final save has no deadline). A large
  session or a slow filesystem makes the restart offer report "did not stop
  within 15000ms ... kill with SIGKILL" while the save is healthy; following
  that advice loses the final save.
