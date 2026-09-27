# Hygiene hunt: `crates/shepr-remote`

Scope: saved machines (catalog, profile ids, SSH metadata, targets, remote executable) and
remote connections (SSH argument building, attach, bridge, discovery, host, launch, local
server lifecycle, remote processes, saved-machine state, server lifecycle, SSH agent), plus
everything traced out of the crate into `shepr-platform`, `shepr-api`, `shepr-client`,
`shepr-mux` and the root binary.

Counts as read: every file under `crates/shepr-remote/src`, and the consuming sites in
`src/cli/machine.rs`, `src/cli/target.rs`, `src/cli/status.rs`, `src/cli/error.rs`,
`src/autodetect.rs`, `src/main.rs`, `crates/shepr-client/src/endpoint/*`,
`crates/shepr-client/src/handshake.rs`, `crates/shepr-api/src/server.rs`,
`crates/shepr-platform/src/ssh_paths.rs`, `crates/shepr-platform/src/remote_bridge.rs`.

Findings are numbered `R1..R46` and grouped by the eight questions. Each carries an
**Enforce** line: what could hold the fixed version mechanically. Findings marked **FACT**
are divergences or dead code verified today, not predictions.

A note on the crate overall: the module layout is good (a real seam exists for discovery
round trips, `DiscoverySteps`, and it is the only injected dependency in the crate). The
recurring shape of the findings below is that everything *else* the crate depends on - the
clock, the `ssh` program, the process id, the environment, the remote `shepr` CLI's
argument spellings and JSON shapes, the operator's terminal - is reached directly from
logic, with no owner and no seam.

---

## 1. One value, one owner

### R1 `SSH_AUTH_SOCK` is resolved by three sites with three different rules - FACT
- `crates/shepr-remote/src/remote/ssh_agent.rs`: `env::var("SSH_AUTH_SOCK").ok().filter(|p| !p.is_empty())` - empty means absent, and absent means *no registration worker at all*.
- `crates/shepr-api/src/server.rs`: `env::var_os("SSH_AUTH_SOCK").map(PathBuf::from)` - **no empty check**, so an empty variable becomes `Some("")` and is handed to `SshAgentRegistry` as a real agent path.
- `crates/shepr-mux/src/pane/launch.rs`: writes the name back into pane children.

Three spellings of the name, and the resolution rule (what an empty value means) is already
divergent between the client-side bridge and the server-side registry for the same
variable in the same process tree. Nothing names this variable once.

**Fix**: `shepr_platform::ssh_agent` owns `inherited_agent_socket() -> Option<PathBuf>`;
all three call it (mux already calls `ssh_agent::pane_agent_socket`, so the module is the
natural owner).
**Enforce**: a text rule in `brokkr.toml` forbidding the literal `"SSH_AUTH_SOCK"` outside
`shepr-platform/src/ssh_agent.rs` - the same shape as the existing
`alacritty-terminal-only-in-shepr-vt` rule, one level down.

### R2 The bridge idle timeout and the client heartbeat interval are coupled across two crates with nothing linking them
`shepr_platform::remote_bridge::IDLE_TIMEOUT` (60s) must exceed
`shepr_client::endpoint::health::HEARTBEAT_INTERVAL` (5s) or a healthy idle remote bridge
would be torn down under a live client. Neither constant mentions the other; neither crate
can see the other (`shepr-platform` is below `shepr-client`). `shepr-remote` sits between
them and passes only the `idle_timeout: bool` through.
**Fix**: both constants belong in `shepr-core` (which both crates may depend on), with the
relation stated at the definition.
**Enforce**: a test in whichever crate can see both (`shepr-client`, or the root binary)
asserting `IDLE_TIMEOUT >= HEARTBEAT_INTERVAL * 4`. Today nothing would notice either
number changing.

### R3 The private-socket bring-up sequence is spelled three times and has already diverged - FACT
`shepr_platform::ipc` documents the order ("Acquire this before `prepare_socket_path` and
keep it until the listener"). Three callers implement it:

| site | startup lock | prepare | bind | identity | extra chmod |
|---|---|---|---|---|---|
| `shepr-remote/src/remote/bridge.rs` | yes | yes | yes | yes | **yes** (`0o600`) |
| `shepr-api/src/server.rs` | yes | yes | yes | yes | no |
| `shepr-server/src/server/headless.rs` (client protocol socket) | **no** | yes | yes | yes | no |

The client protocol socket - the one the whole TUI attaches to - skips the startup lock
that the doc comment says to take, which is exactly the race the lock exists to close.
The bridge's extra `restrict_socket_permissions(0o600)` is redundant:
`bind_private_local_listener` already ends owner-only.
**Fix**: one `shepr_platform::ipc::bind_private_socket(path, busy_message) -> (Listener,
SocketStartupLock, SocketFileIdentity)` that makes the wrong order unrepresentable; the
three callers lose the choice.
**Enforce**: a type/signature (the lock is returned by the bind, so it cannot be skipped) -
this is enforceable and currently is not.

### R4 `BRIDGE_SOCKET_PERMISSION_MODE` duplicates `PRIVATE_SOCKET_MODE`, and a test uses it for an unrelated file - FACT
`bridge.rs` defines `BRIDGE_SOCKET_PERMISSION_MODE: u32 = 0o600`; `ipc.rs` defines
`PRIVATE_SOCKET_MODE: u32 = 0o600` (private). Worse,
`attach.rs:managed_ssh_config_includes_user_config_then_fallback` asserts the *ssh config
file's* mode against `BRIDGE_SOCKET_PERMISSION_MODE` with the message "keepalive config
must be user-only" - two unrelated policies now share one constant, so changing the bridge
socket mode breaks an ssh-config test.
**Fix**: export `PRIVATE_SOCKET_MODE` from `shepr-platform`; delete the bridge copy; the
config test asserts `0o600` or a `PRIVATE_FILE_MODE` owned by `private_file.rs`.
**Enforce**: a test is what exists; a shared constant makes the divergence unrepresentable.

### R5 SSH keepalive settings are spelled twice, in two syntaxes, in one file - FACT (agreeing today)
`ssh.rs::apply_noninteractive_ssh_options` emits `-o ServerAliveInterval=15 -o
ServerAliveCountMax=4`; `ssh.rs::write_managed_ssh_config` writes `ServerAliveInterval 15`
/ `ServerAliveCountMax 4` into the config text. Same tunable, two syntaxes, both literal.
A third copy of the values sits in the test assertions in `attach.rs`.
**Fix**: one `struct SshKeepalive { interval_secs: u64, count_max: u32 }` that can render
itself as `-o` args or config lines.
**Enforce**: a test comparing the two renderings; better, a type that produces both.

### R6 `ConnectTimeout=10`, `ConnectionAttempts=1`, `NumberOfPasswordPrompts=0/3`, `StrictHostKeyChecking=yes`, `ControlPersist=600`, `ControlMaster=auto` are literal strings in argument builders
All in `ssh.rs`, none named. `StrictHostKeyChecking=yes` appears in both
`apply_noninteractive_ssh_options` and `authentication_command_with_config` (a
security-relevant value with two sites). `ControlPersist=600` is additionally *cited in
prose* in `shepr-client/src/endpoint/supervisor.rs` ("persisting ten minutes") - see R10.
**Fix**: a single `ssh_options` module with named constants and one builder.
**Enforce**: a clippy-visible constant set plus the existing `attach.rs` argument
assertions. Not enforceable as long as the values are inline string literals.

### R7 The remote `shepr` CLI's argument spellings are re-spelled in `shepr-remote` with no shared constant - FACT
`shepr-remote` builds command lines for a remote `shepr` binary out of bare literals:
`"remote-client-bridge"`, `"--idle-timeout-v1"`, `"remote-api-bridge"`, `"--check"`,
`"status"`, `"client"`, `"server"`, `"--json"`, `"server stop"`, `"--session"`. Every one of
those is defined independently in `src/cli/spec.rs` / `src/cli.rs`. `--idle-timeout-v1`
alone is spelled in three files (`launch.rs`, `src/cli.rs`, `src/cli/spec.rs`).
The root binary depends on `shepr-remote`, so a shared constant module in `shepr-remote`
(or `shepr-api`) could be the single owner for both the producer and the parser.
**Fix**: `shepr-remote` owns the subcommand/flag name constants; `src/cli/spec.rs` builds
its clap spec from them.
**Enforce**: a test that round-trips each generated remote command string through
`cli::spec::command().try_get_matches_from` - the parser proves the producer. This is
cheap and absent today; the only current check is byte-for-byte golden strings in
`attach.rs`, which pin the producer to itself and say nothing about the parser.

### R8 `255` and `254` are magic numbers in a shell string, and the `254` remap has no decoder - FACT
`SSH_OWN_FAILURE_EXIT_CODE: i32 = 255` exists in `bridge.rs`, but
`posix_remote_output_command` writes `255` and `254` as literal text inside the remote shell
script rather than interpolating the constant. Nothing anywhere maps `254` back: a remote
`shepr` that exits 255 reaches the operator as "remote command failed (exit status 254)", a
number documented in no user-facing text. The encode side has no decode partner.
**Fix**: interpolate `SSH_OWN_FAILURE_EXIT_CODE`, add `REMAPPED_REMOTE_255_EXIT_CODE`, and
have `ssh_bridge_exit_error` say "remote command failed with 255 (reported as 254)".
**Enforce**: a test asserting the generated script contains the constants and that the
error text names 255 - the existing test only checks the shell behaviour, not the decode.

### R9 `wait_for_server_socket` has no owned timeout, and its two callers disagree in the wrong direction - FACT
- `src/autodetect.rs`: `SERVER_READY_TIMEOUT = 15s` for the *local* server.
- `crates/shepr-remote/src/remote/host.rs`: `Duration::from_secs(5)`, inline, for the
  server on the *remote* host reached over SSH - the slower case gets the shorter budget,
  and the 5 is not even named.
**Fix**: `local_server::SERVER_READY_TIMEOUT` owned next to `wait_for_server_socket`, with
the parameter removed unless a caller has a stated reason to differ.
**Enforce**: removing the parameter makes divergence unrepresentable.

### R10 SSH timing budgets are cited in another crate's prose instead of being read
`shepr-client/src/endpoint/supervisor.rs` reasons at length about "15 seconds" per
discovery command (that is `NONINTERACTIVE_SSH_COMMAND_TIMEOUT` in `shepr-remote`), "the
handshake 60", and "ControlMaster, persisting ten minutes" (`ControlPersist=600` in
`ssh.rs`), and derives `ATTEMPT_BUDGET = 25s` from them. None of those numbers is read; all
are restated. Changing `NONINTERACTIVE_SSH_COMMAND_TIMEOUT` to 30s silently invalidates the
25s budget and the documented argument for it, and no test fails.
**Fix**: export `shepr_remote::NONINTERACTIVE_SSH_COMMAND_TIMEOUT` (and the handshake
timeout) and define `ATTEMPT_BUDGET` in terms of them.
**Enforce**: a test asserting `ATTEMPT_BUDGET >= NONINTERACTIVE_SSH_COMMAND_TIMEOUT +
slack` and `ATTEMPT_BUDGET < MAX_RETRY_DELAY` (the second half already exists at
`supervisor.rs:638` - so the pattern is known here and only half applied).

### R11 A third and fourth 15-second SSH budget - FACT
`src/cli/target.rs::server_status` uses `Duration::from_secs(15)` inline for the remote API
probe; `ssh.rs` uses 15 for every noninteractive command. Same physical quantity ("one cold
SSH round trip"), two unnamed literals in two crates.
**Enforce**: same as R10.

### R12 The client state subdirectory is spelled at three sites
`catalog.rs::catalog_path` -> `state_dir/client/endpoints.json`;
`catalog.rs::selection_path` -> `state_dir/client/endpoint-selection.json`;
`ssh_metadata.rs::new` -> `state_dir.join("client/ssh-metadata")` (note: a different join
style for the same directory). Nothing owns "the client's state directory".
**Fix**: `shepr_config::AppPaths::client_state_dir()`, like the existing
`server_address()`/`session_id()` accessors.
**Enforce**: a text rule forbidding the literal `"client"` as a path component outside that
accessor, or simply the accessor's existence plus review.

### R13 The temp-file prefix in `store_private_json` is wrong for two of its three users - FACT
`catalog.rs::store_private_json` names its staging file `.endpoints-<pid>-<seq>.tmp`
regardless of the `description` it was given. It is used for the endpoint catalog, the
endpoint *selection*, and the *SSH metadata* cache. A leftover `.endpoints-*.tmp` in the
`ssh-metadata` directory names the wrong subject.
**Fix**: derive the prefix from the destination file name.
**Enforce**: a test asserting the staged name derives from the target path.

### R14 The profile-id truncation length `16` is duplicated
`saved.rs` slices `&profile_id.as_str()[..16]` in `saved_bridge_path` and again in
`SavedSshApiBridge::start`, for the short socket name. `profile_id.rs` owns
`PROFILE_ID_BYTES = 16` (a different 16 - bytes, not hex chars) so the two constants would
also be easy to confuse.
**Fix**: `ProfileId::short() -> &str` on the type that owns the invariant.
**Enforce**: a type/signature (indexing disappears, so a shorter id cannot panic either -
today both sites would panic on a malformed id, which `parse` happens to prevent).

### R15 `"shepr"` as a program name appears with two different resolution rules - FACT
`launch.rs::run_remote` resolves the local program from `std::env::args().next()` with
`"shepr"` as fallback; `saved.rs::saved_ssh_bootstrap_command` hardcodes `"shepr"`
unconditionally, and that string is printed to the operator as a command to run
(`check_saved_ssh`'s error). `discovery.rs` additionally hardcodes `shepr` as the *remote*
binary name in `command -v shepr` and in the known-locations script.
**Fix**: one `PROGRAM_NAME` constant plus one `local_invocation_name()` helper; the remote
binary name is a separate constant with a comment saying it is the remote install's name.
**Enforce**: text rule against the bare `"shepr"` literal outside the owning module. Note
this one *does* legitimately need two values (local argv0 vs remote install name) - a
finding with an answer, not a non-finding.

### R16 The known remote install locations are a literal shell script with no owner
`discovery.rs::known_remote_binary_candidate_script` hardcodes `$HOME/.cargo/bin/shepr` and
`$HOME/.local/bin/shepr`. `brokkr install` decides where the binary actually lands, and
`AGENTS.md` claims "the owner installs the binary on each host manually". Those two lists -
where we install, where we look - are independent.
**Enforce**: not mechanically enforceable across the brokkr/shepr boundary. Best available:
a comment at each site naming the other, and a test that the script's paths are a superset
of `brokkr install`'s destination if brokkr exposes it.

---

## 2. Values nobody can find, change, or trust

### R17 Nothing answers "what are this crate's tunables"
The crate's knobs are scattered across seven files, each defined where it was first needed:
`NONINTERACTIVE_SSH_COMMAND_TIMEOUT` (ssh.rs), `PIPE_DRAIN_GRACE`, `POLL_INTERVAL`,
`SSH_STDOUT_CAPTURE_LIMIT`, `SSH_STDERR_CAPTURE_LIMIT` (process.rs), `BRIDGE_ACCEPT_POLL`,
`BRIDGE_IO_POLL`, `BRIDGE_FAILURE_REPORT_TIMEOUT`, `BRIDGE_FAILURE_REPORT_POLL_INTERVAL`,
`BRIDGE_SOCKET_PERMISSION_MODE` (bridge.rs), `REMOTE_SERVER_SHUTDOWN_CONFIRM_TIMEOUT`,
`REMOTE_SERVER_SHUTDOWN_POLL_INTERVAL` (server_lifecycle.rs), `SOCKET_POLL_INTERVAL`,
`STATUS_REQUEST_TIMEOUT` (local_server.rs), `CATALOG_POLL_INTERVAL`, `MAX_CATALOG_BYTES`,
`MAX_PROFILES`, `MAX_LABEL_BYTES` (catalog.rs), `MAX_METADATA_BYTES` (ssh_metadata.rs),
`MAX_SSH_TARGET_BYTES` (target.rs), `MAX_REMOTE_EXECUTABLE_BYTES` (executable.rs), plus the
unnamed literals in R6/R9/R11 and a bare `Duration::from_millis(250)` inside
`bridge_connection`'s wait loop and `Duration::from_millis(100)`/`500ms`/`10ms` inside
`ssh_agent.rs`. A person tuning the SSH behaviour has to read the whole crate.
**Fix**: one `tunables.rs` (or `limits.rs` + `timing.rs`) per crate, as the top of the file
of record. This is the structural version of R5, R6, R9, R11 and R18 together.
**Enforce**: a text rule that `Duration::from_` and `const MAX_` may appear only in the
tunables module (and tests) - mechanically checkable and would have caught every inline
literal above.

### R18 Several timings have no injection point, so tests must wait them out - FACT
- `bridge_connection` hardcodes a 250 ms post-EOF grace before killing the child; no test
  can shorten it.
- `ssh_agent::Registration` hardcodes a 100 ms probe interval, a 500 ms connect timeout and
  a 10 ms read poll. `registration_retries_when_the_api_is_initially_missing` consequently
  sleeps in 10 ms increments against a 5-second wall-clock deadline (`assert!(Instant::now()
  < deadline, "registration did not retry")`) - a real-time test.
- `local_server::wait_for_server_socket` takes its timeout but not its poll interval;
  `wait_for_server_socket_succeeds_after_delay` sleeps 50 ms in a spawned thread and allows
  2 s.
- `SshMetadataCache`, `SavedSshConnector::connect` and `RemoteSsh::noninteractive_timeout`
  read `Instant::now()` / `SystemTime::now()` directly (see R38).
**Fix**: pass intervals in (or a small `Timing` struct) the way `deadline` already is
passed into `connect`; `EndpointCatalogWatch::poll(now)` shows the crate already knows this
pattern and applies it in exactly one place.
**Enforce**: a signature change; plus the text rule from R17.

### R19 `RemoteExecutable` and `SavedSshEndpoint` limits are validated at every use, not once
Not a defect in itself (these validate deserialized data, which is right), but the
asymmetry is worth naming: `SavedSshSettings` is explicitly documented as "read once at
launch and handed in", while the catalog is re-read, re-validated and re-limited on every
`load`, `store`, watch poll and CLI invocation. Two policies for two kinds of on-disk
state, and only one of them is stated anywhere.
**Enforce**: not a rule, a doc statement in the crate's module comment - which is what the
project does elsewhere ("Config is read and validated once at launch").

### R20 `is_launch_fatal_setup_error` decides a launch-vs-retry policy from an `io::ErrorKind` - FACT, partly false
It treats *every* `InvalidInput` as launch-fatal. `InvalidInput` is produced by
`shepr_platform::remote_bridge_endpoint_path` for "socket path exceeds the Unix socket
length limit", by `validate_private_runtime_dir` for a relative runtime dir, and by
`shared_ssh_control_path`. It is also produced by `RemoteExecutable::parse` failures
arriving through other paths. Some of those are genuinely deterministic; the classification
is by kind, not by cause, and the typed-error mechanism the same function uses for
`UnsafeSshRuntimeDirectory` is the right one and is only used once.
**Fix**: a typed `DeterministicSetupError` marker on every deterministic producer, and drop
the `ErrorKind::InvalidInput` blanket.
**Enforce**: the existing test `only_typed_runtime_directory_policy_errors_are_launch_fatal`
already asserts the blanket - it pins the looser rule in place, so this needs the test
changed, not added.

---

## 3. One channel, one implementation

### R21 A library crate writes operator text to stderr - FACT, 13 sites
`shepr-remote` calls `eprintln!`/`eprint!` from `lib.rs` (3), `bridge.rs` (2) and
`server_lifecycle.rs` (8). These are on a *library* layer below the binary that owns
operator output (`src/cli/error.rs::CliError::print`). `bridge.rs` even branches on
`noninteractive` to pick between `tracing::warn!` and `eprintln!` for the same event -
*"saved SSH endpoint bridge failed"* versus *"shepr: remote bridge failed: {err}"* - which
is a channel decision made inside the transport.
**Fix**: the crate returns values (`SshFailureDiagnostic`, a `RemoteHint` enum); the binary
renders them. `print_saved_ssh_error_hint` / `print_remote_error_hint` become
`saved_ssh_error_hint(&err) -> Option<Hint>`.
**Enforce**: a text rule in `brokkr.toml` forbidding `println!`/`eprintln!`/`print!` in
`crates/**` outside test modules. This is the single highest-value rule this hunt found:
it is trivially checkable, and the crate violates it 13 times.

### R22 An interactive terminal prompt lives in the library - FACT
`server_lifecycle.rs::confirm_remote_server_stop` checks `io::stdin().is_terminal()`, prints
five lines to stderr, prints a `[y/N]` prompt, flushes, and reads from `stdin().lock()` -
all from a crate that also serves a headless client's background reconnect worker. It is
only reachable from `run_remote`/`prepare_saved_ssh` today, but nothing structural keeps a
supervisor thread out of it.
**Fix**: `ensure_remote_server_ready` takes an `&mut dyn Confirm` (or returns a
`RemoteServerNeedsRestart` value the caller decides on). `read_remote_confirmation` already
takes a `&mut impl BufRead`, so the seam is half-built and then bypassed by the caller.
**Enforce**: the same no-print rule (R21) plus removing `is_terminal`/`stdin` from the
crate; a dependency rule cannot express "no stdin", but the print rule catches the
symptom.

### R23 The `[y/N]` default is stated twice and the two copies can disagree - FACT
`confirm_remote_server_stop` prints `[y/N]` and then calls
`read_remote_confirmation(&mut stdin, false)`. The prompt text and the `default` argument
are independent; `"" => Ok(default)` is the only consumer. `default: true` is never passed,
so the parameter is also a switch with one value (see R43).
**Fix**: one `Confirmation { default }` that renders its own prompt.
**Enforce**: a type.

### R24 Log levels are inconsistent for one class of event - FACT
The same class - "a remote thing we depend on is unavailable" - is logged at three levels:
- `tracing::debug!` for "SSH agent refresh unavailable" (`ssh_agent.rs`), "could not cache
  SSH machine metadata", "could not invalidate SSH machine metadata"
  (`ssh_metadata.rs`), "saved SSH setup failed transiently" (`saved.rs`).
- `tracing::warn!` for "SSH agent refresh unavailable" (`shepr-api/src/server.rs` - the
  *same message* as the debug one above), "failed to check server socket"
  (`local_server.rs`), "saved SSH endpoint bridge failed" (`bridge.rs`).
- `tracing::error!` for "remote bridge failed to prepare client socket" (`bridge.rs`).

"SSH agent refresh unavailable" specifically exists at both `debug` and `warn` for the two
ends of one feature.
**Fix**: state the level policy in the crate module comment and apply it.
**Enforce**: not mechanically enforceable for level choice. A text rule could at least
require every `tracing::` call in this crate to carry the endpoint identifier (see R25).

### R25 Bridge and connector diagnostics omit the identifiers needed to act - FACT
`bridge.rs` logs "saved SSH endpoint bridge failed", "saved SSH endpoint listener failed",
"rejected remote bridge socket peer with different credentials" and "remote bridge failed
to prepare client socket" with **no** profile id, label, target or socket path - and the
bridge thread owns all of them (`target` is captured in the closure). With several saved
machines configured, these lines do not say which machine.
Likewise `saved.rs` logs "remembered remote Shepr did not connect; rediscovering" and "SSH
discovery stopped; the next attempt resumes it" without the profile id or target, though
`self.profile_id` and `self.target` are in hand.
**Fix**: add `profile = %id, target = %target` fields; better, build the connector's and
bridge's `tracing::Span` once at construction so every line inside inherits them.
**Enforce**: a span at construction makes omission impossible - that is the structural
version and it is cheap here.

### R26 Significant events that log nothing - FACT
- `SshStdioBridge::start` logs nothing: no line says a bridge came up, for which machine,
  at which socket, with which remote executable. `shepr-api`'s server logs `info!("api
  server listening")` and `shepr-server` logs `info!("client protocol socket listening")`
  for the equivalent event, so the pattern exists and this crate skips it.
- A successful reconnect after N failures logs nothing.
- `SshMetadataCache::store` on success logs nothing, so there is no record of which remote
  path we decided to remember - the exact fact you want when a machine starts failing.
- `EndpointCatalogWatch` reloading the catalog logs nothing (the *failure* warns).
**Enforce**: not a rule. Worth a checklist item in the crate comment.

### R27 The `[y/N]` block is a five-`eprintln!` wall - FACT
`confirm_remote_server_stop` emits six lines including one 130-character sentence and one
110-character sentence. `local_server::validate_running_server_compatibility` builds a
five-line multi-paragraph error *inside an `io::Error`* via `format!` with embedded `\n\n`.
An `io::Error` message is also what gets `escape_debug`'d into one line by
`src/cli/machine.rs::status` - so that carefully formatted multi-line text reaches the
operator as one very long line with `\n` escapes in it. **FACT**: the two sites disagree
about whether error strings may contain newlines, and one of them mangles the other.
**Fix**: errors carry structure (subject + cause + guidance as fields); the binary formats.
**Enforce**: a test that no error message produced by the crate contains `\n`, once the
guidance moves out of the message.

---

## 4. Errors

### R28 A corrupt saved-machine catalog silently means "no saved machines" - FACT, defect
`src/main.rs:210`:
```rust
let saved_federation =
    shepr_remote::machine::EndpointCatalog::load(paths).is_ok_and(|catalog| catalog.has_ssh());
```
`EndpointCatalog::load` returns a rich `Err(String)` ("stored endpoint catalog is invalid",
"endpoint catalog exceeds the storage limit", "duplicate endpoint profile id", "failed to
open endpoint catalog: permission denied"). All of it is discarded. The consequences are
not cosmetic: `saved_federation == false` turns a local-server startup failure from a
warning into a hard launch failure (`autodetect.rs`), and it silently switches the client's
`LocalFailurePolicy` from `Reconnect` to `ExitClient` - so a typo in `endpoints.json`
changes the client's lifetime rule with no message anywhere. The client then loads the
catalog *again* a moment later in `shepr_client::run_client`; the two loads can disagree.
**Fix**: propagate the error and refuse the launch (the project's stated posture: "Any
config problem fails the launch; no fallbacks" - the catalog is state rather than config,
but an *unreadable* catalog is not a soft condition), and load it once, passing the value.
**Enforce**: `#[must_use]` does not help; a test asserting a malformed catalog makes the
launch fail is what is needed. This is a real, current swallow.

### R29 `SshMetadataCache::store` and `invalidate` swallow every failure at `debug`
Both return `()`. A metadata cache that can never be written means every reconnect pays
full discovery forever, reported only at `debug`. `store` is also called from
`src/cli/machine.rs::add` *after* "Saved SSH machine {id}. Remote server is ready." is
printed, so the user is told setup succeeded even when the cache seeding silently failed.
**Fix**: return `io::Result<()>`; the CLI's `add` decides whether to mention it.
**Enforce**: `#[must_use]` on the result, or the signature itself.

### R30 `catalog_fingerprint` swallows every `stat` error into `None` - FACT
`std::fs::metadata(path).ok()?`. `None` means both "the file is absent" (correct - empty
catalog) and "we cannot stat it" (a permission change, an unmounted state dir). In the
second case the watcher will happily report `Ok(Vec::new())` as the current catalog once,
retiring every saved machine in the running client. `EndpointCatalogWatch::poll`'s doc even
promises "an unreadable or invalid file is reported once per change; the caller keeps the
profiles it has" - which is true for `load_from_path` errors and false for stat errors.
**Fix**: distinguish `NotFound` from other kinds, like `load_from_path` already does.
**Enforce**: a test that a stat failure does not retire machines. This one is a likely
current defect, not just a smell.

### R31 `SavedSshEndpoint`/`EndpointCatalog` errors are `String`, and context is added by string concatenation
Every validation and IO failure in `catalog.rs`, `target.rs`, `profile_id.rs` and
`executable.rs` is a `String`. `src/cli/machine.rs` then wraps them with
`std::io::Error::other(error)` and sometimes prefixes them (`"remote prepared, but machine
was not saved: {error}"`). Nothing downstream can branch on *why* the catalog was rejected -
compare `SshFailureDiagnostic`, which exists in the same crate and does this properly.
**Fix**: a `CatalogError` enum, mirroring `SshFailureDiagnostic`'s design.
**Enforce**: a type.

### R32 `print_saved_ssh_error_hint` reclassifies an error by re-parsing it
`is_remote_host_key_error` / `is_remote_auth_error` call
`SshFailureDiagnostic::from_error`, which for an error that is *not* already a diagnostic
falls back to classifying by `ErrorKind`. So a hint is chosen from a downgraded
classification whenever the typed diagnostic did not survive the journey. The crate's own
comment says "Text classification is reserved for the SSH process boundary" - and this is
the one place that re-derives it afterwards.
**Fix**: take `&SshFailureDiagnostic`, not `&io::Error`, so the typed value must be
threaded.
**Enforce**: a signature. Currently unenforced and the doc comment says the opposite of
what the code allows.

### R33 The crate does not abort the process, but its callers do it on its behalf
`shepr-remote` itself has no `process::exit`/`panic!` on operator input - good. But
`src/main.rs` does `std::process::exit(1)` on `run_remote` failure *after* printing, and
`load_validated_config_or_exit` exits during remote launch, both bypassing
`CliError::exit_code`'s 1/2 distinction. And `src/cli/machine.rs` returns bare `Ok(2)` at
five sites to mean "usage error" instead of `CliError::Usage`, which is the type that owns
exit code 2. **FACT**: `machine` is the only command family in the CLI that spells 2 by
hand.
**Fix**: `machine.rs` returns `CliError::Usage`; `main.rs` funnels remote launch failures
through `CliError`.
**Enforce**: a text rule forbidding integer literals as `Ok(..)` exit codes outside
`error.rs`, or - better - change `run_machine_command`'s return type to
`CliResult<()>` so the code cannot be spelled at all.

### R34 Panic-by-indexing on `ProfileId`
`&profile_id.as_str()[..16]` (twice, R14) panics if a `ProfileId` is ever shorter than 16
bytes. `ProfileId::parse` guarantees 32 today, so this is reachable only through the struct
literal path used in tests. Still: an invariant maintained by a constructor, relied on by
slicing two modules away.
**Fix**: `ProfileId::short()`.

---

## 5. Tests that prove nothing

### R35 `managed_ssh_config_includes_user_config_then_fallback` never runs its headline assertion - FACT
```rust
if let Some(home) = paths.home_dir() {
    let user_config = home.join(".ssh").join("config");
    if user_config.is_file() { ...assert include_at < fallback_at... }
}
```
`paths` comes from `test_app_paths()`, which is
`AppPaths::test_with_context(&root, Some(&root), None)` where `root` is a fresh
`ScratchDir`. A fresh scratch directory never contains `.ssh/config`, so the inner block is
**dead in every run**. The test's name and its comment ("any user config is Included
(quoted) BEFORE it so first-value-wins keeps the user's own settings") describe behaviour
the test does not check. This is the "worse than no test" shape: it reads as coverage for
the one ordering rule that OpenSSH's first-value-wins semantics depend on.
**Fix**: write a `config` file into the scratch home and assert unconditionally.
**Enforce**: a test that fails if the include is absent - i.e. removing the `if`.

### R36 `remote_executable_accepts_only_cacheable_absolute_paths` has no accepting case - FACT
```rust
for (path, valid) in [("/home/a b/shepr", false), ("$HOME/.local/bin/shepr", false),
                      (".../mise/shims/shepr", false), ("/bin/shepr\nmalformed", false)]
```
Every expected value is `false`. `RemoteExecutable::parse` could `return Err(...)`
unconditionally and this test would pass, despite its name claiming it checks what is
*accepted*. (A valid path is exercised incidentally elsewhere, in `ssh_metadata.rs` tests
and `attach.rs`, so the behaviour is covered - but not by the test named for it, and the
`valid` column is dead weight that reads as if both directions were covered.)
**Fix**: add `("/usr/bin/shepr", true)` and friends.

### R37 Tests depend on the host environment - FACT
- `local_server.rs::server_daemon_detach_creates_new_session` shells out to `sh -c 'ps -o
  sid= -p $$ | tr -d " "'`. It needs `ps` with BSD-ish `-o sid=` support and `tr`, and it
  tests `shepr_platform::detach_server_daemon_command` - another crate's function - from
  this crate's test module.
- `attach.rs` (three sites) and `launch.rs` (one site) spawn `/bin/sh` to execute generated
  remote scripts. Defensible (POSIX-shell behaviour is the thing under test) but it is a
  host dependency and should be stated.
- `ssh_agent.rs::registration_retries_when_the_api_is_initially_missing` is wall-clock
  bound (5 s deadline, 10 ms polls) and depends on thread scheduling.
- `local_server.rs::is_server_listening_returns_permission_errors_instead_of_false`
  `return`s early when running as root - i.e. it silently passes as a no-op in a root
  container. A skipped test that reports success.
- `process.rs::timeout_kills_the_child` and
  `a_stderr_pipe_held_by_a_background_process_does_not_block_the_result` spawn `sh` and
  `sleep` and assert elapsed-time bounds (`< 1s`, `< 3s`).
**Fix**: move the detach test to `shepr-platform` where the function lives; mark the
wall-clock ones as such; make the root skip an explicit failure or a `#[ignore]`.
**Enforce**: a text rule that test modules may not name `ps`, `tr`, `sleep` (the gremlin
scan shows brokkr can do content rules); a rule that `return` inside `#[test]` needs a
comment. Both cheap.

### R38 Three redundant names for one function make a test assert nothing - FACT
`locate_remote_shepr`, `prepare_remote_shepr` (wraps it in a one-field
`PreparedRemoteShepr`) and `find_installed_remote_shepr` (`locate_remote_shepr` verbatim)
are the same call. `discovery_tests.rs` exercises `DiscoveryProgress` directly, so nothing
tests that the three entry points agree - they agree by being copies.
**Fix**: one function, no `PreparedRemoteShepr` (see R42).

### R39 `attach.rs` is a 1112-line test file named after a thing it does not contain - FACT
`lib.rs` declares `#[cfg(test)] #[path = "remote/attach.rs"] mod attach;`. There is no
attach code; the file is `mod tests { ... }` holding tests for the bridge, the managed ssh
config, the teardown registry, the process pipes, path sanitising, the output framing, the
reattach command and remote discovery. The scope brief lists "attach" as a subject of this
crate - there is no such subject. Anyone looking for attach logic reads a test file; anyone
changing `bridge.rs` does not think to look in `attach.rs`.
**Fix**: split into `#[cfg(test)] mod tests` blocks next to the code they test, as the
project's own convention requires ("Unit tests live next to the code").
**Enforce**: a brokkr rule that a `#[path]`-included module file matching `*_tests.rs` is
allowed and anything else must be non-test - or simply that `mod X` where `X.rs` contains
only `mod tests` is an error. `discovery_tests.rs` shows the correct naming already in use
in the same directory, so the crate contradicts itself.

### R40 `libc` is a normal dependency used only by tests - FACT
`crates/shepr-remote/Cargo.toml` lists `libc` under `[dependencies]`; the only uses are
`attach.rs:214` (`fcntl`) and `local_server.rs:214` (`geteuid`), both inside `#[cfg(test)]`
modules. `brokkr.toml`'s `shepr-remote-layer` rule allows `libc` for `kinds = ["normal"]`,
so the allowlist currently blesses a dependency that production code does not use.
**Fix**: move to `[dev-dependencies]` and drop `libc` from the `shepr-remote-layer` allow
list.
**Enforce**: the dependency rule already exists and would then enforce it - this is a
one-line tightening of a check somebody already paid for.

---

## 6. Guards and claims that have stopped holding

### R41 `machine_mutation_commands_only_expose_add_and_remove` is keyed on three names - FACT, fails open
```rust
for command in ["rename", "enable", "disable"] { assert!(spec.try_get_matches_from(...).is_err()) }
```
This is the *only* enforcement of the "saved machines are add and remove only" claim in
`AGENTS.md`. It checks that three specific historical subcommand names are absent. Adding
`machine update`, `machine set-label`, `machine edit` or `machine relabel` passes. The test
reads as an invariant and is a blocklist of three strings.
**Fix**: assert the *set* of `machine` subcommands equals
`{list, status, reconnect, add, remove}` - clap can enumerate them
(`command.get_subcommands()`).
**Enforce**: yes, exactly as above. Cheap, and turns a fail-open name check into a
closed set.

Related and better: `EndpointCatalog::apply_profile_delta` *does* enforce add/remove
structurally - "updating saved endpoint {id} is not supported" - at the storage layer. So
the claim is enforced where it matters and guarded by a name list where it is exposed. Say
so at both sites.

### R42 `REMOTE_MISE_SHIM_SUFFIX` is a fail-open name guard - FACT
`executable.rs` rejects a discovered path ending in `/mise/shims/shepr`. mise's shim
directory is relocatable (`MISE_DATA_DIR`), and the equivalent problem exists for asdf,
rtx's legacy layout, `~/.local/share/pipx`, and any other shim dir. When the name stops
matching, the guard becomes a silent no-op and shepr caches a shim path that re-execs
something else - which is precisely the failure the guard was written for, reported as
nothing.
**Fix**: test the candidate rather than its spelling - the status probe already runs
`status client --json` on the candidate and compares `build_id`, so a shim that resolves to
the right binary is fine and one that does not already fails. Consider deleting the guard
in favour of the probe, and keeping only a diagnostic note when a rejected candidate looked
like a shim.
**Enforce**: not enforceable as a name rule. Deleting the name guard in favour of the
behavioural probe is the structural answer.

### R43 `ssh_config_include` fails open on `is_file()`
`path.filter(|path| path.is_file())` - if `/etc/ssh/ssh_config` or `~/.ssh/config` is a
symlink to a file it passes (fine), if it is absent the include is silently dropped (fine),
and if OpenSSH on this host reads its system config from somewhere else entirely
(`/etc/ssh/ssh_config.d/*`, a distro override) the managed config silently omits settings
the user believes are active - with no line logged. Combined with R35, the include ordering
is neither tested nor observable.
**Fix**: log at `debug` which includes were emitted and which paths were skipped.
**Enforce**: a test; the path list itself (`ssh_paths.rs`) cannot be enforced against
OpenSSH's actual search order.

### R44 `discard_remote_output_preamble` is keyed on a version-suffixed marker string
`REMOTE_OUTPUT_READY_MARKER = "shepr-remote-output-ready:1"`. The `:1` implies versioning
that nothing reads: there is no version negotiation and, per `AGENTS.md`, no wire
compatibility obligation (client and server are always the same build). Same for
`STALE_API_METADATA = "shepr-machine-metadata-stale-v1"` and the `--idle-timeout-v1` flag.
Three `v1`/`:1` suffixes that encode a compatibility story the project explicitly does not
have.
**Fix**: drop the version suffixes, or state in one place why they exist (they are the
one thing that *could* legitimately differ if a stale remote binary is somehow reached -
but `build_id` checking already covers that, before the marker is read).
**Enforce**: not enforceable; a decision to record.

### R45 The remote `status server --json` / `status client --json` contract is two independent structs with no shared type - FACT
`src/cli/status.rs` defines `ServerStatusJson`/`ClientStatusJson` (`Serialize`);
`shepr-remote` defines `RemoteServerStatusJson`/`RemoteClientStatusJson` (`Deserialize`)
and parses the *same* JSON over SSH. Field names (`running`, `version`, `build_id`,
`capabilities.detached_server_daemon`) are spelled independently on both sides. Renaming
`running` in `status.rs` breaks every saved machine at runtime and the build says nothing.
The only thing pinning them is a hardcoded JSON literal in `attach.rs:1029`, which was
written by hand and can drift from `status.rs` freely.
Additionally **FACT**: `ServerStatusJson` carries both `status: "running"|"not_running"`
and `running: bool` - two representations of one fact - and `shepr-remote` reads only
`running`, so the `status` string is unread by the only programmatic consumer.
**Fix**: put the shape in `shepr-api::schema` and have both sides use the one type. This is
a cross-process contract inside one build, so no copy is needed at all.
**Enforce**: a shared type makes the mismatch unrepresentable; failing that, a test that
serialises `ServerStatusJson` and deserialises it as `RemoteServerStatusJson` - which is
possible today and absent.

### R46 Claims in comments that nothing checks
Checkable but unchecked:
- `RemoteSsh` doc: "no noninteractive command runs past it [the attempt deadline]" -
  `sh_output` and `framed_user_shell_output` honour it; `SshStdioBridge::start` and the
  `establish` callback do not consult it (the supervisor holds them to it separately).
  Checkable with a fake clock.
- `bridge.rs`: "Each local API request has its own stream and therefore its own SSH stdio
  process. The streams are served serially" - true by construction (a single accept loop)
  but nothing asserts it; a future `thread::spawn` per stream would break the claim
  silently.
- `saved.rs::connect`: "Attempts for one endpoint never overlap (the supervisor keeps one
  in flight...) so holding the lock for the whole attempt contends with nothing" - a claim
  about a *different crate's* scheduling, asserted in a comment that the supervisor does
  not reference back. If the supervisor ever runs two attempts, this becomes a lock held
  across a 25-second blocking SSH operation. Checkable only by a test in `shepr-client`.
- `catalog.rs::EndpointCatalogWatch`: "an unreadable or invalid file is reported once per
  change" - false for stat errors (R30).
- `ipc.rs`: "Acquire this before `prepare_socket_path`" - false at one of three call sites
  (R3).

False today: R30, R3, R35 (the test's own claim), R41 (the guard does not hold the claim it
is named for).

---

## 7. Policy invented per call site

### R47 The `local`/`server` keybinding string table is implemented twice - FACT
`shepr-remote/src/remote/args.rs::RemoteKeybindings::parse`/`as_str` owns the mapping and
the env var name, but `parse` is `pub(super)` and therefore not exported. So
`shepr-client/src/handshake.rs::ClientProcessRole::from_env` re-implements it: its own
`"server"`/`"local"` literals, its own error text (`"{var} must be 'local' or 'server', got
{value:?}"` versus `"--remote-keybindings must be 'local' or 'server'"`), and its own
handling of the absent and non-UTF-8 cases. One value written by one crate and read by
another, with the round trip spelled twice.
**Fix**: `RemoteKeybindings` owns `to_env_value`/`from_env` and is exported;
`shepr-client` (which already depends on `shepr-remote`) calls it.
**Enforce**: a round-trip test `from_env(to_env_value(x)) == x` - impossible to write today
because the two halves live in crates that do not share the type.

### R48 Cleanup on the error path is hand-rolled at six sites with three different shapes - FACT
- `bridge.rs::start_command`: three separate `if let Err(e) = ... { remove_socket_file_if_owned(...); return Err(e) }` blocks.
- `bridge.rs::bridge_connection`: `child.kill(); child.wait();` at five places, sometimes
  with `stdout.finish()`/`stderr.finish()`, sometimes not; plus
  `terminate_bridge_child` which does the same thing as a helper for three of them.
- `ssh.rs::write_managed_ssh_config`: an inline closure plus `remove_dir_all` on error.
- `process.rs::wait_with_output_timeout`: `kill/wait/finish/finish` duplicated in the error
  arm and the timeout arm.
- `catalog.rs::store_private_json`: two `remove_file(&temp_path)` error arms.
**Fix**: RAII guards. `ManagedSshConfigDirectory` and `TeardownRegistration` show the crate
already knows the pattern and applies it to two of the resources; the socket, the child
process and the temp file are left manual.
**Enforce**: a guard type makes the leak unrepresentable. No lint can catch the manual
form.

### R49 Retry and backoff policy is split across three crates with no owner
- `shepr-client/src/endpoint/supervisor.rs`: `INITIAL_RETRY_DELAY`, `MAX_RETRY_DELAY`,
  `ATTENTION_RETRY_DELAY`, `ATTEMPT_BUDGET`, exponential `retry_delay(attempt)`.
- `shepr-remote/src/remote/saved.rs`: its own retry semantics (drop the remembered
  executable, resume discovery) with no delays.
- `shepr-remote/src/remote/ssh_agent.rs`: a fixed 100 ms retry forever, unbounded, its own
  policy.
- `shepr-api/src/server.rs`: "a bounded backoff" for the accept loop, a fourth policy.
**Fix**: one backoff type in `shepr-core` parameterised per use.
**Enforce**: not enforceable as a rule; a shared type is the only lever.

### R50 Ambient dependencies reached directly from logic - FACT
- **Clock**: `Instant::now()` in `RemoteSsh::noninteractive_timeout`,
  `SavedSshConnector::attempt`, `SshStdioBridge::reported_failure`,
  `TeardownRegistry::release_all`, `wait_for_remote_server_shutdown`,
  `wait_for_server_socket`, `wait_with_output_timeout`, `ssh_agent::connect`.
  `EndpointCatalogWatch::poll(now)` is the single place that takes the clock as a
  parameter - and it is also the only one of these with a fast, deterministic test.
- **Randomness / identifier generation**: `ProfileId::generate` reads `SystemTime::now()`,
  `std::process::id()` and a private `AtomicU64`; `shepr-platform::unpredictable_token`
  reads `getrandom`. Two independent id schemes.
- **Process id**: `std::process::id()` in `local_forward_socket_path`, `saved_bridge_path`,
  `SavedSshApiBridge::start`, `store_private_json`, `create_remote_ssh_config_dir`,
  `unpredictable_token` - six sites embedding the pid in a name, no owner.
- **Environment**: `std::env::var("SSH_AUTH_SOCK")` (R1), `std::env::args().next()` in
  `run_remote`.
- **Working directory**: `paths.current_dir()` is threaded properly - good, and the
  contrast makes the rest stand out.
**Fix**: a `Clock` trait (or just passing `now`/`deadline`, as `connect` already does) and
an id source handed in at construction.
**Enforce**: a text rule forbidding `Instant::now()` / `SystemTime::now()` /
`std::process::id()` / `std::env::var` outside a designated module - mechanically
checkable, and it would flag every site above.

### R51 `ProfileId::generate` is a hand-rolled id scheme beside an existing one - FACT
`sha2` of `"{pid}:{nanos}:{seq}"` truncated to 16 bytes, with the comment "not secrets;
practical uniqueness is enough". `shepr-platform::unpredictable_token` already exists and
*is* unpredictable (getrandom). The catalog id is used in socket file names in the shared
XDG runtime directory (`saved_bridge_path`, `SavedSshApiBridge`), so it is partly a
namespace another local user can enumerate - and the comment's premise ("not a secret")
was written for the catalog row, not for the socket name it later became.
**Fix**: generate from `unpredictable_token`; delete the `sha2` dependency from
`shepr-remote` if nothing else needs it (**check**: `sha2` is in the crate's dependency
allowlist and used only here).
**Enforce**: dropping `sha2` from the `shepr-remote-layer` allowlist in `brokkr.toml`
enforces it thereafter.

### R52 Shared mutable state whose safety rests on call order
- `SavedSshConnector::state` is a `Mutex<ConnectorState>` **held across the whole 25-second
  attempt**, including the SSH child spawn, discovery round trips and the caller's
  `establish` handshake. That is a lock held across blocking IO. The comment says it
  contends with nothing because the supervisor serialises attempts - a claim about another
  crate (R46). If it is truly serialised, the mutex is unnecessary; if it is not, this is a
  25-second stall. Either way one of the two is wrong.
- `SSH_TEARDOWN` is a process-global `static TeardownRegistry`. Its `release_all(grace)`
  drains everything, so *any* caller invoking it disarms every other owner's cleanup - safe
  only because exactly one call site exists, in the client's exit path. Two writers who
  have never been introduced: the registry and the individual `Drop` impls, coordinated by
  a `Condvar` and a comment about field declaration order
  (`ManagedSshConfigDirectory`'s `_teardown` "declared after `path`").
- `bridge.rs`'s `failure_rx: Arc<Mutex<Receiver<io::Error>>>` is locked by both the accept
  thread (`discard_unclaimed_bridge_failure`) and `reported_failure`; correctness rests on
  the accept thread discarding before accepting, i.e. on ordering, and the polling loop in
  `reported_failure` exists specifically to avoid starving the other side. A
  single-slot `Mutex<Option<io::Error>>` with explicit generation numbering would make the
  intent structural.
**Fix**: for the connector, move the mutable state behind `&mut self` and let the
supervisor's exclusive ownership be the enforcement (it is what the comment claims anyway).
**Enforce**: a type - `&mut self` makes concurrent attempts a compile error, which is
strictly better than the comment.

### R53 Unbounded growth and unbounded polling
- `ssh_agent::Registration`'s worker loops at 10 Hz for the *entire life of the remote
  bridge process* whenever the API socket never appears. Bounded in memory, unbounded in
  wakeups, and nothing logs after the first `debug!`.
- `TeardownRegistry.pending` grows with every bridge and every managed config; entries are
  removed on `Drop`, so a leaked owner leaks a registry entry too. No cap, no metric.
- `PipeCapture` is properly bounded (1 MiB stdout / 16 KiB stderr) - the good case.
- `ssh_agent::connect` caps the response at 4096 bytes but then *falls out of the loop* and
  reports `TimedOut` rather than "response too large" - a bound that misreports.

### R54 Secrets and personal data in diagnostics - FACT, mostly handled
Handled well: `SshTarget::parse` rejects embedded passwords; `copy_local_stream_to_writer`
carries an explicit comment that it never logs the bytes it copies; the catalog test
asserts no `password`/`private_key` fields are persisted; socket names use the profile id
rather than the target (`bridge_paths_use_profile_identity_not_target_or_session`).
Remaining leaks:
- `local_forward_socket_path` (the `--remote` path, not the saved-machine path) *does* put
  `sanitize_path_component(target)` - i.e. `user@host` - into a world-listable name in the
  XDG runtime directory. The saved-machine path deliberately does not; the two paths
  disagree about whether the target is sensitive.
- `command_failed` and `ssh_bridge_exit_error` fold raw remote stderr into error messages
  that reach `eprintln!` and the CLI's JSON output. Remote stderr can contain a login
  banner, hostnames, usernames and anything the login shell printed. It is bounded (16 KiB)
  but unredacted.
- `src/cli/machine.rs::status` prints `error.escape_debug()`, i.e. that same remote stderr,
  into `shepr machine status` output and its `--json` form.
**Enforce**: a test asserting a bridge socket name contains neither `@` nor any component
of the target (the saved-machine equivalent already exists - extend it to the `--remote`
path).

### R55 Test-only shortcuts reachable from production
- `bridge_upload_cancellation_for_test` is `pub` under `#[cfg(any(test, feature =
  "test-support"))]`, and it `expect()`s four times and `assert!`s once. If anything ever
  enables `test-support` in a non-test build, a panicking API is exported from a library
  that otherwise bans `unwrap`.
- `RemoteSsh::test_with_state` constructs a `RemoteSsh` bypassing `new`, so the "managed
  config was written" invariant can be violated - `#[cfg(test)]` only, which is correct.
- `UPLOAD_READ_ATTEMPTS` is a `thread_local!` counter checked *inside*
  `copy_local_stream_to_writer`'s hot loop under `#[cfg(test)]`. Correctly gated, but it
  means the hot path under test is not the hot path that ships.
**Enforce**: `brokkr.toml` could forbid the `test-support` feature appearing in
`[dependencies]` (as opposed to `[dev-dependencies]`) - `shepr-server`'s allowlist already
lists `shepr-test-support` as a normal dependency, which is the case worth checking.

---

## 8. Code that is no longer load-bearing

### R56 `PreparedRemoteShepr` is a one-field wrapper with two aliases - FACT
```rust
pub(super) struct PreparedRemoteShepr { pub(super) remote_shepr: RemoteExecutable }
pub(super) fn prepare_remote_shepr(ssh) -> io::Result<PreparedRemoteShepr>
pub(super) fn find_installed_remote_shepr(ssh) -> io::Result<RemoteExecutable>  // == locate_remote_shepr
pub(super) fn locate_remote_shepr(ssh) -> io::Result<RemoteExecutable>
```
Three names and a wrapper struct for one call. **Evidence it is dead weight**: the struct has
exactly one field, one constructor and two readers, both of which immediately project the
field; `find_installed_remote_shepr` has an identical body to `locate_remote_shepr`.
**Fix**: keep `locate_remote_shepr`; delete the other two names and the struct.

### R57 `pub` items unreachable outside the crate - FACT
`machine.rs` re-exports only `{EndpointCatalog, EndpointCatalogChanges, EndpointCatalogWatch,
SavedSshEndpoint, RemoteExecutable, ProfileId, SshMetadataCache, IntoSshTarget, SshTarget}`.
So these are `pub` in private modules and reachable by nobody:
- `catalog::catalog_path` (`pub fn`) - used internally three times; verified zero external references.
- `executable::REMOTE_EXECUTABLE_ROOT`, `executable::REMOTE_MISE_SHIM_SUFFIX` (`pub const`) - zero external references.
`REMOTE_EXECUTABLE_ROOT = "/"` is additionally a constant that names nothing: its only use
is `value.starts_with(REMOTE_EXECUTABLE_ROOT)`, i.e. "is absolute", which `Path::is_absolute`
already spells.
**Fix**: `pub(crate)`/`pub(super)`, and replace `REMOTE_EXECUTABLE_ROOT` with
`Path::new(value).is_absolute()`.
**Enforce**: clippy has no `unreachable_pub` on by default for this shape; the workspace
lint table in `Cargo.toml` could add `unreachable_pub = "deny"` - that is exactly this
finding, enforced.

### R58 `SshFailure` is exported and its variants are never named outside the crate - FACT
`pub enum SshFailure` with six variants is re-exported from `lib.rs`. Grepping the whole
workspace: no external site names any variant; consumers use only
`SshFailureDiagnostic::{requires_authentication, is_host_key, is_stale_metadata,
is_link_failure, needs_attention}`. The enum is public surface that exists to be matched on
and is never matched on.
**Fix**: make it `pub(crate)` and keep the predicates as the public surface - or make it
public *and* delete the five predicate wrappers on `SshFailureDiagnostic`, which currently
duplicate `SshFailure`'s own two predicates plus three `==` comparisons. Today both
interfaces exist and only one is used.

### R59 `version_label` is a one-line helper with two callers in one function - FACT
`server_lifecycle.rs::version_label(Option<&str>) -> &str` is `version.unwrap_or("unknown")`.
Both callers are in `confirm_remote_server_stop`. Meanwhile
`remote_server_compatibility_error` and `remote_compatibility_error` each define their own
`printable` closure that does `unwrap_or("unknown")` *plus* an ASCII-graphic filter - two
independent policies for rendering an untrusted version string, and the stricter one is not
the one used in the interactive prompt. **FACT**: `confirm_remote_server_stop` prints a
remote-controlled version string to the terminal with no control-character filtering, while
the error path filters it.
**Fix**: one `printable_remote_value` used everywhere; delete `version_label`.
**Enforce**: a test that a version string containing `\x1b[2J` is filtered by every path
that renders it. That is a real terminal-injection hole through the one unfiltered site.

### R60 `RemoteKeybindings::Server` is a switch with one interesting value, and `default: bool` has one - FACT
- `read_remote_confirmation(reader, default)` is called once, with `false`. The `default`
  parameter has had one value since it was added (R23).
- `CandidateVerification` is a genuine two-valued enum (both arms reachable) - good.
- `RemoteExecutable::needs_shell_quoting` exists only to produce a diagnostic for a path
  `parse` already rejected; it re-runs the same predicate from outside, so the rejection
  reason is computed twice by two functions that must agree.
**Fix**: have `parse` return a typed rejection reason instead of a `String`, and delete
`needs_shell_quoting`.

### R61 `shell_quote`'s safe-character set is duplicated verbatim - FACT
`launch.rs::shell_quote` and `executable.rs::has_only_shell_safe_characters` contain the
*identical* predicate:
```rust
ch.is_ascii_alphanumeric() || matches!(ch, '@'|'%'|'_'|'+'|'='|':'|','|'.'|'/'|'-')
```
Two modules, two copies, one rule about what a POSIX shell treats as a plain word. They
agree today. They serve different purposes (one decides whether to quote, the other decides
whether to *reject*), which is why the duplication was easy to introduce and will be easy
to let drift - `executable.rs` rejecting a character `shell_quote` would have quoted safely
is a silent discovery failure.
**Fix**: one `fn is_shell_plain_word(s: &str) -> bool` in one module; both call it.
**Enforce**: a test asserting `shell_quote(s) == s` exactly when
`has_only_shell_safe_characters(s)` - writeable today, and it would pin the two together
without merging them.

### R62 `host.rs` is 41 lines that duplicate `autodetect.rs`'s flow at a different timeout
`ensure_remote_server_running` is `is_server_listening` -> `spawn_server_daemon` ->
`wait_for_server_socket`, which is exactly `autodetect::auto_detect_launch`'s startup block
minus the build-compatibility check and with a 5 s rather than 15 s budget (R9). The
missing build check is deliberate and documented (a good comment). The duplicated *sequence*
is not.
**Fix**: `local_server::ensure_running(paths, ReadyTimeout, BuildCheck)` owns the sequence;
both callers pick the policy explicitly.
**Enforce**: not a rule; a shared function.

---

## Summary of what could be wired into the build today

Ordered by how little work each is relative to what it would catch. All are of the kind
`brokkr.toml`, `clippy.toml` or the workspace lint table already expresses.

1. **No `println!`/`eprintln!`/`print!` in `crates/**` outside tests** - catches R21 (13
   sites), and pushes R22, R23, R27 into the binary where they belong.
2. **`libc` out of `shepr-remote`'s normal dependencies** (R40) - one line in `Cargo.toml`,
   one line removed from an existing `brokkr.toml` allowlist.
3. **`unreachable_pub = "deny"`** in the workspace lint table - catches R57, probably
   elsewhere too.
4. **No `"SSH_AUTH_SOCK"` literal outside `shepr-platform::ssh_agent`** (R1), and no
   `Instant::now()`/`SystemTime::now()`/`std::process::id()`/`std::env::var` outside
   designated modules (R50).
5. **`Duration::from_*` and `const MAX_*` only in a per-crate tunables module** (R17) -
   catches R5, R6, R9, R11, R18 as a class.
6. **A closed-set assertion for `machine` subcommands** (R41) - replaces the only
   enforcement of a stated behavioural claim, which is currently a blocklist of three
   strings.
7. **A round-trip test from generated remote command strings through the CLI parser** (R7)
   and **a serialise/deserialise test across the two status JSON structs** (R45) - both
   turn a silent cross-process contract into a build failure.
8. **`bind_private_socket` returning its own startup lock** (R3) - makes the order
   unrepresentable, and fixes a live violation in the server's client socket.

## Things worth fixing that no rule can hold

- R28 (the swallowed catalog load in `main.rs`) - a one-line behaviour change plus a test.
  This is the most consequential live defect in the report: a typo in `endpoints.json`
  silently changes the client's lifetime policy.
- R30 (stat errors retire every saved machine) - likely a live defect.
- R35 (a test whose headline assertion has never executed).
- R59 (unfiltered remote version string printed to the terminal).
- R52 (a mutex held across 25 seconds of blocking SSH, justified by a comment about another
  crate's scheduling).
- R42 (the mise shim name guard) and R16 (the install-location list) - both are name-keyed
  and neither can be enforced; the answer for R42 is to delete the guard in favour of the
  build-id probe that already runs.

## Scope not covered

- I did not run `brokkr check`, `cargo` or any test; per the project rules validation is the
  orchestrator's. Every "FACT" above is from reading, not from a failing run. The two I would
  most want confirmed by execution are R35 (assert the `if user_config.is_file()` block never
  runs) and R40 (`libc` unused in a non-test build).
- `crates/shepr-client/src/endpoint/supervisor.rs` and `registry.rs` were read for the
  timing and policy relations only (R10, R49, R52), not audited as scope.
- `shepr-platform::remote_bridge*` and `ssh_agent` are another hunter's scope
  (`notes/hunt-core-platform.md` covers `IDLE_TIMEOUT` and `--idle-timeout-v1`); I traced
  into them only where the value crosses the boundary (R1, R2, R3, R44) and have noted
  where our findings overlap rather than restating theirs.
