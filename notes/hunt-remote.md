# Hunt: remote machines

Scope read in full: every file under `crates/shepr-remote/src/` (tests included),
`crates/shepr-client/src/endpoint.rs` and everything under
`crates/shepr-client/src/endpoint/`. Followed into `crates/shepr-client/src/handshake.rs`,
`crates/shepr-client/src/launch.rs`, `crates/shepr-client/src/limits.rs`,
`crates/shepr-client/src/shell/endpoints.rs` (machine entry), `crates/shepr-launch/src/failure.rs`,
`connection_health.rs`, `limits.rs`, `stop.rs`, `src/main.rs`, `src/preflight.rs`,
`src/preflight/words.rs`, `src/cli/fleet.rs`, `src/cli/error.rs`, and the platform IPC and
single-use socket code in `crates/shepr-platform/src/ipc.rs`.

Findings are grouped by the nine questions. Within a group they are not ranked.
Each says what it breaks or duplicates, the evidence, the fix, and whether the fixed
version can be enforced mechanically.

---

## 1. Defects

### D1. `connection_health` claims the client heartbeats every endpoint; Local has none

`crates/shepr-launch/src/connection_health.rs` (module doc): "The client probes every
endpoint, local or SSH, after `HEARTBEAT_INTERVAL` of silence, and the server answers."
The client does not: `EndpointPolicy::uses_ssh_heartbeat` is `Machine` only
(`crates/shepr-client/src/endpoint.rs`), `EndpointRegistry::insert_with_activity` gives Local
`health: None`, and the tests `a_local_slot_is_not_health_tracked` and
`recovered_local_uses_transport_failure_not_remote_health_probes` pin that Local is never
probed. A Local server that is alive but wedged (SIGSTOPped, deadlocked loop) is therefore
never detected; the client waits on a silent socket forever. Either the doc is false or the
code is; the code's own comments ("Local reports a dead server as a socket transport
error") say the doc is the stale one. Fix the doc, or (better, since a wedged server is
exactly what a heartbeat is for) probe Local too and delete `uses_ssh_heartbeat`.
Enforceable: a test in shepr-client asserting the heartbeat set, plus deleting the doc
sentence; nothing checks prose against behaviour.

### D2. The login entry tells a dev client to "run shepr again"

`crates/shepr-client/src/shell/endpoints.rs` `machine_entry`:
`format!("run shepr again, or {ssh}")`. AGENTS.md: "Guidance uses `shepr` for a release
build and the running executable path for a dev build". `src/cli/fleet.rs failure_label`
does it right with `shepr_launch::guidance::operator_entrypoint()`; the sidebar spells
`shepr` literally, so a dev TUI tells the operator to start the release build, which has a
different runtime directory and control socket and will not unlock the dev client's
machine. Fix: build the hint from `operator_entrypoint()` (ideally in `shepr_launch::guidance`,
which owns operator text). Enforceable: a textlint forbidding a backtick-free literal
`"run shepr` / `` `shepr` `` in `crates/shepr-client/src/shell/**`, or a test that renders the
entry under a dev build and asserts `operator_entrypoint()` appears.

### D3. `ClientEndpointId::display_label` documents a refusal that does not exist

`crates/shepr-client/src/endpoint.rs`: "The launch refuses a machine label that names the
local server, so the two never read alike." AGENTS.md and
`crates/shepr-config/src/validated.rs` (`is_local_entry`): such an entry is skipped, not
refused, and its palette becomes the local hue. The same doc lists "`local`" as one of the
names `display_label` returns; it never returns that (only `Display` writes `local`). Not
checkable; reword.

### D4. `failed_before_remote_result` documents callers it does not have

`crates/shepr-remote/src/failure.rs`: "The bridge and the machine check use it so no such
failure is read as a remote command's answer". Production calls it nowhere; every call is in
a test (`bridge/tests.rs`, `ssh/tests.rs`, `discovery/tests.rs`, `machine_ssh.rs` tests).
The bridge and the check use `FailureEvidence` instead. See also 9.2.

### D5. The known-locations script says it runs before `command -v`; it runs after

`crates/shepr-remote/src/discovery.rs known_remote_binary_candidate_script`: "These are
checked before falling back to `command -v`". `DiscoveryProgress::run_remaining` and
`installed_remote_shepr_candidates` both run `path_via_account_shell` (the `command -v`)
first and put its result first in probe order; `fleet.rs` correctly says "the account
shell's PATH first, then the known install directories". Reword.

### D6. `DiscoverySteps` claims a single sequencer; there are two

`discovery.rs`: "Only [`DiscoveryProgress`] sequences them". `installed_remote_shepr_candidates`
(used by `fleet::read_status`) sequences `path_via_account_shell` then `known_locations` again,
with its own dedupe, outside `DiscoveryProgress`. Two orderings of one candidate list that
must agree (the fleet and the TUI must pick the same `shepr` on a host with two installs)
with nothing tying them. Fix: one `candidates(steps)` function both call; `run_remaining`
keeps only the resume state. Enforceable by structure (one function), not by a rule.

### D7. "Without connection sharing" premises in budget docs are false today

`DiscoveryProgress` doc ("without connection sharing each is a cold SSH connect") and
`crates/shepr-client/src/limits.rs ATTEMPT_BUDGET` ("a slow link without connection sharing")
reason about a configuration production never has: `write_managed_ssh_config` always sets
`control_path: Some(..)` and `apply_managed_ssh_options` always adds `-S` with
`ControlMaster=auto` and `ControlPersist`. The resume machinery is still useful for the
first, master-establishing attempt, but the docs justify it by a case that cannot occur.
Reword around the master-less first connect.

### D8. A Connect or Restart attempt is budgeted as if no server had to start

`ConnectMode::Start` runs under `ATTEMPT_BUDGET` (`SSH_COMMAND_TIMEOUT + SSH_ATTEMPT_SLACK`,
25 s) and Restart under `ATTEMPT_BUDGET + REMOTE_STOP_SSH_TIMEOUT`. Neither budget contains the
remote launch the mode exists for: the remote bridge's `ensure_running` may legitimately
spend `shepr_launch::local_server::SERVER_READY_TIMEOUT` (15 s, `pub`) plus
`LAUNCH_LOCK_WAIT_GRACE` (5 s) and the session restore before it relays a byte, while the
client's handshake read stops at the attempt deadline. On a cold master or a slow restore the
operator's Connect ends as `TimedOut`, which `MachineState::after_failure` maps to `Offline`
(the entry flips Starting... to Offline), the request is consumed, and only a later automatic
Attach finds the server. `SSH_RESTART_ATTEMPT_BUDGET`'s doc ("then an ordinary connection
attempt that starts this build's server") asserts that the start fits, and nothing checks it.
Fix: a start budget in `shepr-remote/src/limits.rs` built from `SERVER_READY_TIMEOUT` and the
lock grace, with a `const _: () = assert!(..)` beside it, used for `Start` and added to
`Restart`. Enforceable: const assertion.

### D9. Startup authentication ssh has no connect bound

`ssh.rs authentication_command_with_config` appends `BatchMode=no`, the prompt count and
the shepr options, but not `SSH_CONNECT_TIMEOUT_OPTION` or `SSH_CONNECTION_ATTEMPTS_OPTION`,
which every batch command gets. A machine reaches this prompt precisely when a full round
trip timed out (`AuthenticationPending`), which also happens for a host that accepts TCP and
then stalls; the foreground ssh then waits on the OS connect/kex with no bound, before the
TUI has the terminal, and blocks every later machine's prompt. Fix: one option-set builder
(see 8.1) where the connect bound is common to every mode. Enforceable by the builder type.

### D10. `ssh_config_quote` does not escape and drops non-UTF-8

`ssh.rs`: `format!("\"{path}\"")` over `path.to_string_lossy()`. A home directory containing
`"` produces a broken `Include` line (ssh then fails to parse the managed config, reported as
a Configuration failure of every machine); a non-UTF-8 home is silently replaced with `U+FFFD`
and the user's config is not included at all while the include line looks present. Refuse
both (return an `InvalidInput` local setup error naming the path) rather than emit them. The
unit test `ssh_config_quote_wraps_path_with_spaces` only covers the space case.

### D11. The user ssh config is resolved from `$HOME`, OpenSSH resolves everything else from the passwd entry

`ssh_paths.rs remote_ssh_config_paths(app_paths.home_dir())` includes
`$HOME/.ssh/config`; because shepr passes `-F`, OpenSSH no longer reads its own default, and
its `~` expansion for `IdentityFile`, `UserKnownHostsFile` and so on uses `pw_dir`. With
`HOME` differing from the passwd home (sudo -E, a test env leaking into a real run), shepr's
ssh reads config from one home and keys and known hosts from another. Low impact; say which
home is meant and use one.

---

## 2. One value, one owner

- **The `error: ` prefix is a cross-process protocol spelled in two crates.**
  `src/cli/error.rs CliError::print` writes `eprintln!("error: {error}")` for `CliError::Io`;
  `crates/shepr-remote/src/bridge.rs classified_remote_bridge_failure` strips
  `line.strip_prefix("error: ")` to find `BRIDGE_FAILURE_MARKER`; `bridge/tests.rs` spells it a
  third time. Change the CLI's prefix (to `shepr: error:` as `Launch` uses) and every remote
  classification silently becomes "unclassified retry"; the test would still pass because it
  forges the stderr itself. Fix: drop the prefix dependence entirely (the marker search can
  look for the marker anywhere in a line), or export the prefix from `shepr_launch` and have
  both ends use it. Enforceable: a test that runs the real `CliError::print` path into a buffer
  and feeds it to `ssh_bridge_exit_error`.
- **Remote exit codes 125/126/127 are spelled twice** in `failure.rs` (`SshExit::from_code` arms
  and `RemoteExit::code`). One table (`const` per variant, `from_code` matching on
  `RemoteExit::X.code()`) removes the second copy. Enforceable by a round-trip test over all
  variants.
- **The "probe a candidate" script is written twice:** `discovery.rs remote_client_status` and
  `fleet.rs overview_of` both build `test -x {path} || exit {candidate_missing}; {command}`
  and both decode `SshExit::Remote(CandidateMissing)`. One `candidate_command(exe, args)`.
- **Three JSON-reading policies for one remote stdout shape:** `parse_client_status_json` and
  `fleet::parse_overview` take the last line that parses (tolerating noise after the marker);
  `server_lifecycle::parse_remote_server_status_json` requires the whole trimmed stdout to be
  one JSON document. Same wrapper, same noise sources (a remote `shepr` that prints a notice to
  stdout), different verdicts. One `last_json_record::<T>(stdout)` helper.
- **The control-socket name is derived at two sites:** `MachineSshConnector::validate_local_setup`
  calls `shared_ssh_control_path(runtime_dir, &paths.client_config_file(), key)` and
  `ssh.rs write_managed_ssh_config` calls `ssh_control_path_under(dir, &config_file, key)`.
  They agree today; a change of namespace at one site would validate one path and use another.
  Have the managed config return the path it validated.
- **Bridge socket names are restated in a test of another crate:**
  `machine_ssh.rs machine_bridge_names` (`shepr-bridge-<label>.sock`, `shepr-b.sock`) versus
  `crates/shepr-client/src/launch.rs impossible_connector_paths_fail_in_the_preterminal_phase`,
  which proves its precondition with `"bridge.sock"`/`"b.sock"`. Moot if 9.1 lands.
- **Executable names in operator text:** `discovery.rs` writes "shepr-server" and "shepr" in
  every mismatch message instead of `shepr_launch::invocation::SERVER_BINARY_NAME` and
  `REMOTE_INSTALL_NAME`.
- **Per-profile separation of the SSH metadata cache is a second implementation.**
  `machine/ssh_metadata.rs` puts the cache under the shared `client_state_dir()` with its own
  `ssh-metadata-{profile.marker()}` suffix, while `AppPaths::data_dir()` already is the
  per-profile directory. Two rules for "where does dev keep its own state". Move the cache to a
  per-profile directory `AppPaths` owns.
- **`other_build()` test helper is copied** into `discovery/tests.rs`, `server_lifecycle/tests.rs`
  and `src/preflight.rs` tests (and inline in `bridge` tests). One fixture in
  `shepr_test_fixtures`.
- **"shorten XDG_RUNTIME_DIR"** advice is formatted at two sites in `ssh_paths.rs`.
- **Retry interval pinned as a literal in tests:** `supervisor.rs
  ssh_recovery_rejects_stale_generations_and_rechecks_attention` asserts
  `now + Duration::from_secs(30)` where every sibling test uses `ATTENTION_RETRY_DELAY`;
  `a_reconnecting_machine_retries_within_thirty_seconds` spells 30 again. If 30 s is a promise,
  name it (`RETRY_PROMISE`) and assert `MAX_RETRY_DELAY <= RETRY_PROMISE` once.

---

## 3. Values nobody can find, change, or trust

- **SSH option values are strings with numbers in them**, so their couplings cannot be
  asserted: `SSH_CONNECT_TIMEOUT_OPTION = "ConnectTimeout=10"` must stay below
  `SSH_COMMAND_TIMEOUT` (15 s) or a dead host stops reading as `Link`/Offline and starts reading
  as `AuthenticationPending`/NeedsLogin (and gets a foreground prompt at startup);
  `ControlPersist=600` against `SERVER_WAIT_MAX` and the reconnect cadence; the
  `NumberOfPasswordPrompts` counts. `brokkr.toml`'s limits rule explicitly cannot see "a numeric
  value in a string (an ssh option)". Fix: `Duration`/integer consts in `limits.rs`, formatted
  into options by the builder (8.1), with `const _: () = assert!(SSH_CONNECT_TIMEOUT <
  SSH_COMMAND_TIMEOUT)`. Enforceable: const assertion plus a textlint banning `=[0-9]` inside
  string literals in `shepr-remote`.
- **`REMOTE_STOP_SSH_TIMEOUT` (45 s) is coupled to `STOP_WAIT_TIMEOUT` + `STOP_LEASE_WAIT_TIMEOUT`**
  in shepr-launch (15 + 10 s) plus one connect, as its doc says, but those are `pub(crate)` there,
  so no assertion can relate them. Export them (or a `REMOTE_STOP_BUDGET` built from them) and
  assert. Same for D8's start budget.
- **`REMOTE_HANDSHAKE_READ_TIMEOUT` (60 s) almost never binds.** Every machine handshake runs with
  `Some(deadline)` and `do_handshake_for_endpoint` takes the minimum; the attach deadline is at
  most 25 s from attempt start, so 60 s is reachable only in a Restart whose stop returned quickly.
  Its doc ("waits on a fresh SSH connection, including key exchange and authentication") describes
  a role the attempt deadline took over. Either delete it (machines take the attempt deadline
  only) or document it as the Restart cap it effectively is.
- **`limits.rs` mixes tunables with structure**, which defeats "what is the set of tunables for
  remote": `REMOTE_COMMAND_ARGS_INITIAL_CAPACITY` (a `Vec` capacity hint),
  `SSH_PIPE_DONE_CHANNEL_CAPACITY = 1` and `BRIDGE_FAILURE_CHANNEL_CAPACITY = 1` (protocol of a
  one-shot channel), `BRIDGE_IO_POLL = 1ms` sit beside the real knobs (budgets, wait cadences,
  keepalive). The lint forces them there; mark the structural ones `limits-exempt` at their use
  instead, so `limits.rs` reads as the operator-relevant list.
- **No injection point:** `SshStdioBridge::reported_failure` always waits up to
  `BRIDGE_FAILURE_REPORT_TIMEOUT` (1 s) of real time; `wait_with_output_timeout` polls at a fixed
  50 ms; `MachineSshPreflight::new` and `fleet_ssh` read the clock to build their deadline. Any test
  that reaches an EOF-before-welcome path pays the real second.
- **Relay idle expiry vs SSH keepalive:** `BRIDGE_IDLE_TIMEOUT` (60 s) equals
  `SSH_KEEPALIVE` (15 s x 4). The coincidence is harmless but undocumented; nothing says whether
  one is meant to fire before the other.

---

## 4. One channel, one implementation

- **Operator text assembled at the site, outside the owner of operator text.** The mismatch and
  install advice in `discovery.rs` (`client_build_mismatch`, `ensure_remote_sibling_build`,
  "install or update it there manually and retry"), the Local-socket-missing text in
  `supervisor.rs connect_once` ("the local server is unavailable; start it to reconnect"), the
  machine entry hint (D2) and `fleet.rs stop_plan`'s "stop it on that host" are each written where
  the failure is detected. `shepr_launch::guidance` exists for exactly this and already spells the
  entry point per profile. Not mechanically enforceable short of a textlint on imperative verbs;
  the structural fix is typed failure causes whose wording lives in one module.
- **The handshake success line has no identity.** `crates/shepr-client/src/handshake.rs`:
  `info!("endpoint handshake succeeded")` with no endpoint id, generation or boot; with several
  machines connecting at launch the log cannot say which succeeded. Pass the endpoint id in.
- **Routine events logged at warn, failures silent.** A remote bridge that exits with the
  `no-server` record (the normal state of a machine with no server, once per backoff cycle) logs
  `warn!("remote SSH bridge failed")` in `bridge.rs`; every lost connection, including a deliberate
  `shepr stop` on a machine, logs `warn!("endpoint transport failed")` in `hub.rs reconcile`;
  meanwhile a failed attempt that leaves the machine Offline or Reconnecting logs nothing at any
  level (`attempt_failed` warns only for Attention), the scheduling of a wait for a server, a wait
  ending, and an operator Connect or Restart being taken log nothing. Pick one level per class
  (state transitions at info, attention at warn) and log every supervisor transition with endpoint,
  generation and mode.
- **Remote host log lines without identifiers.** `relay.rs`: "SSH bridge upload failed", "SSH bridge
  failed to half-close the server socket" carry only the error; every client's bridge on that host
  writes to the same client log, so a line cannot be tied to a client or a bridge process (no pid,
  no peer). `process.rs PipeCapture::finish` "ssh pipe is still open..." names no command or pid.
- **`attempt_failed`'s warn and the bridge's warn log the same failure twice** at two layers
  (bridge thread, then hub), with different fields.

---

## 5. Errors

- **Remote-server-unresponsive is classified three ways.** The attach bridge
  (`host.rs attached_server_status`) reports `RemoteFailureClass::Repair` (needs attention); the
  Restart path's `server_lifecycle::parse_remote_server_status_json` returns a bare
  `io::Error::other` (unclassified, so `Retry`, entry "Connecting..."); `fleet::stop_plan` returns
  another bare `io::Error::other`. Same fact, three operator outcomes. Build it once as
  `EndpointFailure::remote_repair(..)`.
- **`server_wait.rs` swallows its failures without a word.** `DirectoryWatch::new(dir).ok()` drops
  the inotify error (an exhausted `max_user_watches` is common on hosts running many agents), so the
  wait silently degrades to a 2 s poll for its hour; `Err(_) => ServerSeen::Settling` turns a
  `server_presence` IO error (EACCES on the runtime directory) into a 500 ms spin for an hour, also
  silent. Log once per spell, and treat a persistent presence error as a failure of the wait.
- **The remote wait's setup failures are not classified while the bridge's are.** `src/main.rs`
  maps the bridge's path and logging setup failures to a `Repair` record (`bridge_setup_failure`);
  `WaitForServer` returns the same failures as plain `CliError::Io`, which the client's
  `ssh_bridge_exit_error` reads as unclassified. A host whose logging cannot start shows
  Unavailable through the bridge and "Connecting..." through the wait.
- **The bridge accept loop drops a local setup error.** `bridge.rs`: a failing
  `prepare_remote_bridge_stream` is logged and `continue`d; the accepted stream is dropped, the
  client reads EOF, waits the full `BRIDGE_FAILURE_REPORT_TIMEOUT` on `reported_failure` (nothing was
  sent) and shows "connection closed before the endpoint finished connecting". Send it through the
  failure channel like every other bridge error.
- **`ssh_metadata.rs load_metadata` swallows everything**, EACCES and `ELOOP` included, via `.ok()?`
  on `symlink_metadata`, `open`, `read` and JSON parse, contrary to the clippy seal's own stance
  ("a permission problem reads as a missing file"). It is a disposable hint, but an unreadable cache
  should at least log once with its path.
- **`hub.rs dispatch` ignores `supervisors.request(..)`'s refusal** for `ConnectMachine` and
  `RestartMachine`, after the shell has already set the entry to Starting... or Restarting...
  (`workspace_navigation.rs`). Today the refusal cases (Local, or a machine past its handshake but
  before its snapshot) do not offer the entry, so it is latent; return the outcome so the entry is
  set only when the request was taken.
- **Post-handshake EOF on an SSH endpoint loses ssh's diagnostic.** Only
  `handshake::classify_handshake_error` consults `MachineSshBridge::reported_failure`; once connected,
  `connection_io.rs server_reader_thread` reports a bare "server closed connection" and the
  bridge's classified stderr (for example the remote relay's own failure, or ssh's
  `Connection reset`) never reaches the machine diagnostic.

---

## 6. Tests that prove nothing (or depend on the host)

- **`ssh/tests.rs shared_ssh_transport_survives_helper_config_drop`** names a property (the shared
  ssh transport survives) it never exercises: no master is started; it checks that two configs
  name the same control path and that dropping one removes only its directory.
- **`managed_ssh_config_includes_user_config_then_fallback`** builds its expected `Include` line with
  `ssh_config_quote`, the function under test, so a quoting bug cannot fail it (both sides from one
  place). Spell the expected line literally.
- **`bridge/tests.rs remote_bridge_failures_need_attention_only_when_the_host_must_be_fixed`** forges
  the remote stderr as `"error: {record}"` itself, so it cannot notice the CLI changing its prefix
  (see section 2).
- **`bridge/tests.rs`'s third case includes `"error: shepr-remote-daemon-boot-exit:11\n..."`**, a
  record format nothing in the tree produces any more (a leftover of a removed daemon-boot
  classification); it now only re-tests "unknown marker is unclassified", which the second case
  already covers.
- **`machine/executable.rs shell_quote_uses_the_remote_executable_plain_word_predicate`** compares
  `shepr_core::shell_quote::quote` with `shepr_core::shell_quote::is_plain_word` through a one-line
  forwarder: it tests shepr-core's internal consistency from shepr-remote.
  `remote_executable_accepts_shell_safe_absolute_paths` and
  `shell_command/tests.rs remote_executable_rejects_paths_that_need_shell_quoting` test the same
  parse twice.
- **Wall-clock dependence:** `relay/tests.rs bridge_preserves_one_way_progress_and_drains_after_stdin_eof`
  sleeps 60 ms twelve times against a real 300 ms idle timeout on the boot clock and asserts the
  child is still alive; a 300 ms scheduler stall on a loaded CI box fails it.
  `bridge_upload_idle_waits_without_repeated_reads_and_cancels` asserts exact poll counts (1, 3)
  after real sleeps. `preflight` tests prove concurrency by `max_checks_active == 4` after a 100 ms
  sleep. These are inherent to what they test; say so in each, or drive the boot clock through the
  `start_with_clock` seam that already exists.
- **`shell_command/tests.rs remote_output_wrapper_accepts_newline_scripts_and_remaps_exit_255`** runs
  `known_remote_binary_candidate_script()` under the host `/bin/sh` with the developer's real
  `HOME`/`CARGO_HOME` (unless `command_in_scratch` scrubs them) and asserts only exit 0: it proves the
  script parses, not what it emits. A version with a scratch `HOME` holding a stand-in
  `.cargo/bin/shepr` would test the emission.
- **`ssh_metadata.rs metadata_is_disposable_fingerprinted_and_independent_per_target`** asserts the
  absence of `version` and `os` fields: a check that removed fields stay removed, which is
  migration residue rather than behaviour.
- **`process.rs a_stderr_pipe_held_by_a_background_process_does_not_block_the_result`** borrows
  `shepr_platform::detach_server_daemon_command` (the server launcher's detach) to make a process
  group; a change to server daemon spawning changes this test's fixture.

---

## 7. Guards and claims that have stopped holding

- **The endpoint-move textlint is keyed on the binding name `choice`.**
  `brokkr.toml endpoint-moves-are-driven-from-the-endpoint-module` matches
  `choice\s*\.\s*(select|...)`. `let c = &mut shell.endpoints.choice; c.commit()` or a direct
  `shell.endpoints.choice = EndpointChoice::showing(..)` (which `shell/endpoints.rs` test code already
  does) passes. Today false-negatives exist only under `#[cfg(test)]`. Checkable by visibility
  instead: make the transition methods `pub(in crate::endpoint)` and the field private behind a
  read accessor; then the compiler is the guard.
- **"Bridge socket names must stay clear of the `shepr-ssh-` prefix"** (`ssh_paths.rs`,
  `machine_ssh.rs`): true today (`shepr-bridge-`, `shepr-b.`), checked by nothing. A test asserting
  `!name.starts_with(SSH_CONFIG_DIRECTORY prefix)` would hold it; 9.1 removes the need.
- **`PIPE_DRAIN_GRACE`'s premise is probably stale.** `limits.rs` and three call-site comments say a
  ControlPersist master forked by ssh keeps the command's stderr (or stdout) open. Current OpenSSH's
  `control_persist_detach` points the backgrounded master's stdio at `/dev/null` unless ssh runs with
  debug logging. Unverified here (no OpenSSH source in the tree); if the premise holds, every ssh
  command leaks one blocked `PipeCapture` reader thread for the master's life (bounded only by
  `ControlPersist=600`), which is an unbounded-ish thread growth under a reconnect loop; if it does
  not, the grace and its plumbing are dead. Either way the claim needs checking against the OpenSSH
  version the owner runs, and the comment should say which.
- **`release_ssh_resources_before_exit`'s doc** says "including through `std::process::exit`",
  which `exits-from-main` and the clippy seal make impossible outside `src/main.rs`.
- **`TeardownRegistry`'s doc** ("in a client those owners live on endpoint writer threads ...
  which leaked sockets and config directories") is history; reword as the invariant.
- **`ensure_remote_sibling_build`'s "a candidate whose status does not report a sibling at all is
  one that predates the report"**: a compatibility rationale for older remote builds, which
  `ensure_remote_client_build` has already rejected by build id before this runs. The `None` arm is
  reachable only if this build ever omits `server`; say that, or make `server` non-optional in
  `ClientStatusJson` for this build.
- **`connection_health.rs` (D1), `display_label` (D3), `failed_before_remote_result` (D4), the
  candidate-script order (D5), the single sequencer (D6) and the connection-sharing premise (D7)**
  are claims false today; none is checkable as prose.
- **`retry_delay`'s doc** names "the SSH agent registration worker" as another retry loop in the
  tree; no such worker exists (no hit for it anywhere). Drop the enumeration ("other retry loops
  answer different failures") rather than list them.

---

## 8. Policy invented per call site

- **8.1 SSH option sets are assembled at four sites.** `RemoteSsh::command`, `bridge_connection`,
  `MachineSshConnector::wait_for_server` and `authentication_command_with_config` each call
  `ssh_command()` + `apply_managed_ssh_options` + some of `apply_batch_ssh_options` /
  `ssh_options::append_shepr_options` + `-T` + target. D9 is the divergence that already happened.
  Fix: one `SshInvocation { mode: Batch | Interactive, .. }` builder that owns `-C`, `-F`, `-S`,
  the control and keepalive options, the connect bound, `-T` and the target, so a mode cannot
  omit a common option. Enforceable: make `ssh_command()` private to that builder.
- **Unresponsive remote server**: three classifications (section 5).
- **Remote stdout JSON parsing**: two policies (section 2).
- **Candidate probe script**: two copies (section 2).
- **Remote setup failures**: classified by the bridge, unclassified by the wait (section 5).
- **Ambient randomness and global state.** `remote_bridge_endpoint_path` draws
  `unpredictable_token()` and sweeps the runtime directory on every connect attempt;
  `SSH_TEARDOWN` is a process-global registry whose correctness rests on "call
  `release_ssh_resources_before_exit` once, after the loop, before exit" (call order, documented,
  not structural). 9.1 removes the socket half of both.
- **Bridge download busy-polls.** `copy_reader_to_local_stream` sleeps `BRIDGE_IO_POLL` (1 ms) on
  every `WouldBlock` of the nonblocking local stream; a client whose reader is stalled makes this
  thread wake 1000 times a second for as long as it lasts. The upload side already uses
  `StreamWake`; use a poll on writability for the download too.
- **Bridge stdout download join is unbounded while stderr is bounded.** After ssh exits,
  `bridge_connection` joins the download thread, which blocks on `child_stdout` EOF with no grace,
  while stderr gets `PIPE_DRAIN_GRACE`. If the premise of `PIPE_DRAIN_GRACE` is true for stderr it
  is equally a hang risk for stdout here, and the bridge's `Drop` joins this thread.
- **Indefinite authentication retries.** A machine in `NeedsLogin` (Attention) is retried every
  `ATTENTION_RETRY_DELAY` (30 s) for the life of the client with a BatchMode ssh that offers every
  key and is refused each time. On a host with fail2ban or `MaxAuthTries` accounting this can get
  the client's address banned, turning an auth problem into Offline for every client on that
  address. Consider not retrying authentication refusals automatically (only on operator action or
  on a key-agent change), or a much longer interval.
- **`AuthenticationPending` at runtime reads as "Needs SSH login".** After startup no prompt can
  run, yet `MachineState::after_failure` maps `PossibleAuthentication` to `NeedsLogin`; a host that
  merely stalls for a full 15 s round trip (overloaded sshd, kex stall) is presented as needing a
  login. AGENTS.md describes the full-budget rule for the startup check; at runtime the entry
  should say what is known (no answer within the round trip).
- **Test-only shortcuts in the public API.** `pub use failure::SshFailureDiagnostic` exists for one
  test in `src/preflight.rs`; `pub use ssh_paths::validate_remote_bridge_endpoint_path` exists for
  one test in `crates/shepr-client/src/launch.rs`; `SshFailureDiagnostic`'s `from_message`,
  `from_local_setup_error`, `is_ssh_process_failure`, `remote_exit_code`,
  `is_transient_network_failure`, `needs_attention` and `failed_before_remote_result` are `pub` and
  called only by tests. Production can reach all of them. `scripts/check_dead_test_helpers.py` looks
  for `pub` helpers under a test cfg; these are not under one, so nothing reports them.

---

## 9. Code that is no longer load-bearing

### 9.1 The bridge's filesystem socket and multi-stream accept loop (structural)

Each connection attempt binds a fresh, randomly named, single-use Unix socket in the runtime
directory (`SshStdioBridge::start_command`), and the same thread immediately connects to it
(`MachineSshConnector::attempt` -> `connect_trusted_local_stream_within`). Nothing else ever
connects: the path is random, `0600`, and handed to no other process. Yet the bridge carries an
accept loop that serves stream after stream, a failure channel with "discard unclaimed failures
of an earlier stream" logic (`discard_unclaimed_bridge_failure`, the comment about a generation
slot), `PeerAdmission::OwnerOrRoot`, and a comment ("Each local API request has its own stream
and SSH stdio process") inherited from upstream, where the bridge carried API requests. A
`UnixStream::pair()` (still a `LocalStream`) given to `bridge_connection` directly deletes:
the socket file, its lock sidecar and `release_single_use_socket_lock`, the per-attempt
dead-owner sweep and random token, `remote_bridge_endpoint_path`,
`validate_remote_bridge_endpoint_path` and its export, `validate_machine_bridge_path`, the
"bridge socket path does not fit" launch-fatal setup error (a whole class of
`is_launch_fatal_setup_error`), `BRIDGE_NAME_LABEL_CHARS` and `bridge_name_fragment`,
`TeardownResource::Socket`, `BridgeSocketStartupCleanup`, `BRIDGE_ACCEPT_POLL`'s accept role,
the failure-channel generation problem, and the prefix-collision claim in section 7. It also
removes the 1 s `reported_failure` wait in favour of joining the one connection's worker.

### 9.2 `SshFailureDiagnostic` methods with no production caller

`failed_before_remote_result`, `is_ssh_process_failure`, `remote_exit_code`,
`is_transient_network_failure`, `needs_attention`, `from_message`, `from_local_setup_error`
(section 8). Move them under `#[cfg(test)]` or delete them; the diagnostic's production
interface is `from_error`, `from_ssh_output`, `with_context`, `evidence` and `disposition`.

### 9.3 Options with one production value

- `ManagedSshOptions::control_path: Option<PathBuf>`: always `Some` in production
  (`write_managed_ssh_config`); `None` only reachable from tests.
- `apply_managed_ssh_options(_, None)` and `SshStdioBridge::start(.., ssh_options: None)`: `None`
  only from tests (`bridge/tests.rs`, `machine_ssh.rs` tests).
- `RemoteSsh::attempt_deadline: Option<Instant>`: every production `RemoteSsh` gets
  `Some` (machine probe, connector, fleet) before running a command; the `None` branch of
  `command_timeout` (full timeout, authentication candidate) is exercised only by
  `an_attempt_deadline_shortens_and_then_refuses_commands`. Make the deadline a constructor
  argument.

### 9.4 The control-socket namespace hash

`ssh_control_path_under` hashes `client_config_file()` into the control socket name, and the
comment in `apply_managed_ssh_options` justifies it as "User ControlPaths may be shared across
isolated Shepr configs". There is no config path override (AGENTS.md), and the socket already
lives in the per-profile runtime directory, so the namespace distinguishes nothing in
production except two `XDG_CONFIG_HOME` values sharing one `XDG_RUNTIME_DIR` (a test setup).
Likely a leftover of the removed config override. Hash the target alone, or say what the
namespace is for.

### 9.5 Dead exports of `shepr-remote`

`pub use shell_command::shell_quote` and `pub use preflight::classify_check` have no user outside
the crate; `pub mod machine` exports `RemoteExecutable`, `RemoteExecutableError` and
`SshMetadataCache`, none used outside the crate. `SshRuntimeError` and
`UnsafeSshRuntimeDirectory` are `pub` (with a `pub fn new`) only because the test-only export
above returns them.

### 9.6 Smaller leftovers

- `bridge/tests.rs`'s `shepr-remote-daemon-boot-exit` record (section 6).
- `ssh_metadata.rs` test asserting removed `version`/`os` fields stay absent (section 6).
- `src/main.rs` tests `args_as_utf8_*` use `["shepr", "pane", "get", "pane-1"]`: the `pane` CLI
  group is gone; harmless data, but it reads as a command the CLI has.
- `EndpointSupervisorEvent::Status { message: EndpointFailure }`: the field is a failure named
  `message`, a remnant of a string-typed status.

---

## Lateral notes

- `MachineSshPreflight::check` holds a machine's probe mutex for the whole bounded SSH check
  (up to 25 s); fine because each machine has its own, but the map lock and the deadline lock are
  two more mutexes around what could be a `Vec<MachineProbe>` handed to scoped threads by
  `&mut`, removing all three.
- `is_link_error_kind` counts `AddrInUse` as a link failure (so a local bridge bind collision
  reads as the machine being Offline); with 9.1 the only producer disappears.
- `FailureCause::Io(InvalidData | Unsupported)` maps to `Incompatible`, so any untyped local IO
  error of those kinds that escapes typing (a non-UTF-8 path, a refused `set_nonblocking`) is
  shown as the machine running an incompatible shepr.
- After a remote bridge idles out (`IdleExpired`), the bridge process prints nothing and exits 1;
  the client sees EOF and reconnects, and the diagnostic says "server closed connection" with no
  hint that the remote relay timed out. A record (`retry`) on stderr would make that visible.
- `server_wait` watches the whole runtime directory; on a host that is also a client, every bridge
  socket, lock sidecar and managed config directory created there wakes the wait for a pointless
  presence check. Filtering inotify events by the server socket's name would stop it.
