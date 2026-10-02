# Design hunt: edges

Scope: `crates/shepr-remote`, the root `shepr` package (`src/`), and
`crates/shepr-daemon`. Every non-test source file in the three was read in full;
the parts of `shepr-api`, `shepr-config`, `shepr-protocol` and `shepr-client`
that this scope hands off to were read where a question led there.

## The short version

Five moves would change the shape of this scope. Each is argued in detail below.

1. **Replace `io::Error` as the error currency of the remote edge.** A typed
   `SshFailureDiagnostic` is smuggled inside `io::Error` and recovered by
   downcast, with an `ErrorKind` heuristic as the fallback. Every caller then
   re-derives the meaning from the error. `shepr-client` even picks
   `ErrorKind`s by hand so the heuristic lands where it wants it. One typed
   endpoint-failure enum, with a single `disposition()`, would remove about
   eight scattered classification sites. One of those sites already disagrees
   with another (finding 2.1).
2. **Split `shepr-remote` into the three things it is.** It holds an SSH
   client, the remote-host half of the bridge, and an 860-line local server
   launcher that has nothing to do with SSH. `shepr-server` links all of it
   only to reach a shell-quoting helper.
3. **One grammar type per executable, shared by producer and parser.** The
   `shepr` command line is produced in `shepr-remote` and parsed in `src/`, and
   the two are kept in step by a round-trip test. The `shepr-server` command
   line and its `--version` line are produced in one crate and parsed in
   another with nothing shared at all. The stop command is spelled three
   times.
4. **One restart-offer engine.** The local restart offer in `src/preflight.rs`
   and the remote one in `shepr-remote/src/remote/preflight.rs` are the same
   state machine written twice. Each has its own offer limit and its own
   outcome enum, and they already diverge.
5. **Carry the identities as types.** `BootId` exists but is parsed and then
   thrown back into a `String` at every hop. A build id has no type at all.
   `SshTarget`, `SshFailureDiagnostic`, `BootId` and `CliContext` each `Deref`
   to the primitive they wrap.

---

## 1. Axes that should be types

### 1.1 Remote failures travel as `io::Error` with a typed payload smuggled inside

`SshFailureDiagnostic` (`crates/shepr-remote/src/lib.rs`) is a real typed value
made of two enums, `SshFailure` (what was established) and `SshFailureOrigin`
(where it came from). It never travels as itself, though. Every producer wraps
it into an `io::Error` (`local_setup_error`, `remote_compatibility_error`,
`remote_candidate_mismatch_error`, `command_failed`, `ssh_bridge_exit_error`,
`classify_command_timeout`). Every consumer calls
`SshFailureDiagnostic::from_error`, which tries a downcast first. When the
downcast misses, it falls back to classifying by `io::ErrorKind`:

- `TimedOut`, `ConnectionRefused`, `AddrInUse` and the like become `Link`;
- `InvalidData` and `Unsupported` become `Compatibility` (needs attention);
- everything else becomes `Other` (silent retry).

The `ErrorKind` has therefore become a hidden type tag, and code far from this
crate is written against it:

- `shepr-client`'s `handshake_error` (`endpoint/supervisor.rs`) turns each
  `ClientError` variant into an `io::Error` whose kind is chosen for what
  `from_error` will make of it. `ConnectionLimit` and `ServerStarting` become
  `ConnectionAborted` "so it is retried". A rejected handshake becomes
  `Unsupported` "so it needs attention". An early close becomes
  `UnexpectedEof` "which must stay out of InvalidData, which the attention
  classifier treats as a compatibility problem".
- `connect_once` remaps a missing Local socket from `NotFound` to
  `ConnectionRefused` for the same reason.
- `is_launch_fatal_setup_error` (`remote/machine_ssh.rs`) treats any
  `InvalidInput` as permanently fatal.
- `StoredSetupError` captures `(kind, message)` and rebuilds an `io::Error`
  from them, which drops the typed source.

Any `io::Error` of kind `InvalidData` reaching `from_error` from anywhere is
read as a remote incompatibility. Any `io::Error::other` is read as a silent
retry. The bug in 2.1 comes straight from this.

**Proposed type.** An `EndpointFailure` enum, built at the point where the fact
is known, with the evidence as variants rather than as a `(class, origin)`
pair:

```text
EndpointFailure
  NoRemoteResult(NoResult)       // Link{..} | AuthWait | SshRefused(SshRefusal) | DeadlineSpent
  RemoteCommand(RemoteExit)      // see 1.2
  RemoteIncompatible(Incompat)   // NotInstalled{rejected_path}, ClientBuild{..}, SiblingBuild{..}, ServerBuild{..}, Unparsable{which}
  LocalSetup(LocalSetup)         // RuntimeDirPolicy, BridgePath, SshConfig, Spawn ...
  Handshake(HandshakeFailure)    // the ClientError cases, typed, instead of an ErrorKind pick
  Message(RemoteText)
```

`disposition() -> {Ready, Retry, Prompt, Attention}`, `proves_about_install()`
(see 2.2) and `hint()` (today's `machine_ssh_error_hint`) would each be one
method on it. Functions return `Result<_, EndpointFailure>` rather than
`io::Result`. `from_error` survives only at the true IO boundary (spawn, socket
connect), and there it classifies a raw OS error once.

### 1.2 Exit statuses as bare `i32`, with in-band sentinels

The scope reads exit codes in many places, each as a raw integer:

- ssh's own failure, `255` (`SSH_OWN_FAILURE_EXIT_CODE`), checked in
  `lib.rs` (three times), `bridge.rs` and `discovery.rs`
  (`remote_client_status_failure` reads `output.status.code()` directly
  rather than through the diagnostic);
- the remote wrapper's remap of a remote 255 to `254`
  (`REMAPPED_REMOTE_255_EXIT_CODE`), which aliases a native 254 (the code
  admits this);
- `125`, a sentinel minted in `discovery.rs` (`CANDIDATE_NOT_EXECUTABLE`) to
  mean "this candidate vanished";
- `126 | 127` as literals in `remote_executable_must_be_rediscovered`;
- `3` and `4` (`BOOT_MISMATCH_EXIT_CODE`, `NO_SERVER_EXIT_CODE`) in
  `shepr-api`, matched by `stop_remote_server` and produced by
  `CliError::exit_code`;
- `10`, `11` and `1` (`daemon_exit`), produced by `crates/shepr-daemon/src/main.rs`
  and read by `local_server.rs`;
- `2` for usage, produced in two binaries (`CliError::Usage`, the daemon's
  `usage_error`).

**Proposed types.** Parse `ExitStatus` once at the SSH boundary into
`SshExit::{SshFailed, Remote(RemoteExit)}`, where `RemoteExit` covers
`NotExecutable`, `NotFound`, `CandidateMissing`, `Remapped255Or254` and
`Code(i32)`. Give `server stop` the same treatment `DaemonExit` already has: a
`ServerStopExit` enum in `shepr-api` with `code()` and `from_code()`, so
`stop_remote_server` matches variants and `CliError::exit_code` returns one.
`main` would then return an exit enum, not an `i32` passed through
`u8::try_from(code).unwrap_or(1)`.

### 1.3 Outcomes that reach the operator only as prose

- `RestartResult::Failed(String)` (remote) and `LocalRestart::Failed(String)`
  (local) flatten a `ServerStopError`, or an `io::Error` carrying an
  `SshFailureDiagnostic`, into text.
- `PreflightOutcome::authentication: Option<Result<(), String>>`. The
  `authenticate` implementation returns `io::Error::other(format!("ssh exited
  with {status}"))`, so "ssh could not be spawned" and "ssh exited 255" read
  the same. The proposed type is
  `AuthenticationOutcome::{Succeeded, SshExited(ExitStatus), CouldNotRun(...)}`.
- `local_server::ensure_running` has about a dozen distinct failure outcomes,
  every one an `io::Error` with a formatted message:
  - unresponsive;
  - different build (`BeforeAttach`);
  - override with no server;
  - transition timeout;
  - boot failure (with a `DaemonExit` class it already has);
  - boot log overflow;
  - boot timeout, with or without an occupant;
  - sibling build mismatch;
  - sibling missing, not a file, or not executable;
  - launch lock timeout.

  `autodetect.rs` can only print it, and the Local endpoint has to rediscover
  a build mismatch through its own handshake. A `LaunchError` enum would let
  autodetect hand the client an initial Local status (for example Attention
  with the mismatch guidance) rather than a stderr line that the TUI then
  paints over.
- The remote bridge host (`remote/host.rs`) states the problem in its own doc
  comment. A launch failure on the remote host "reach[es] the client only as
  this command's stderr and exit status, which it classifies as an ordinary
  retryable failure". A remote host whose `shepr-server` refuses its config
  is therefore retried silently forever. The bridge already answers a build
  mismatch with a typed preamble. It could answer a launch refusal the same
  way, as a refusal preamble carrying a `DaemonExit` class, so the client can
  show Attention.

### 1.4 Shell dialects as interchangeable `&str`

`RemoteSsh` has three ways to run text remotely: `sh_output(script)` (fed to
`/bin/sh -s`), `user_shell_output(remote_command)` (handed to the account
shell) and the bridge's `bridge_command()` (wrapped in `/bin/sh -c`). All take
or produce `&str`/`String`. `posix_remote_output_command` emits POSIX syntax
(`$?`, `if [ ... ]; then ...; fi`). It is applied to the account-shell path as
well, and that path exists precisely for shells that are not POSIX (see bug
B4). Typed `PosixScript` and `AccountShellCommand` values (with
`RemoteExecutable::command` returning one of them, and
`SshStdioBridge::start_command` taking one in place of a `String`) would make
"send POSIX to the account shell" unrepresentable.

### 1.5 Preflight pairs outcomes with machines by position

`preflight()` returns a `Vec<PreflightOutcome>` that "pair[s] with `machines`
by position". `restart_different_builds` and `src/preflight.rs::result_notices`
both re-zip `machines` with `outcomes` and trust that the caller passed the
same slice. `PreflightOutcome` also carries `label`, a copy of the
machine's label, so there are two ways to identify the machine. The outcome
should own (or borrow) its `MachineConfig`, and the restart pass should take
the outcomes alone.

Related: `can_prompt: bool` plus a callback "that must not be called when
false" is passed to both `preflight` and `restart_different_builds`. An
`Option<impl FnMut>` (or a `Prompter` that exists only with a terminal) makes
the forbidden call unrepresentable.

### 1.6 Smaller axes

- `MachineSshPreflight::probes: HashMap<String, ...>` is keyed by
  `machine.label.as_str().to_owned()`. Key it by `MachineLabel`.
- `forward_remote_bridge_stdio(stream, true)`: a bool parameter whose only
  production caller passes `true`.
- `local_server::ensure_running(paths, timeout, build_check)`: the timeout
  takes three hops to arrive. It starts as `shepr-remote` `limits`, is
  re-exported as `local_server::SERVER_READY_TIMEOUT`, is aliased again as
  `src/limits.rs::SERVER_READY_TIMEOUT`, and is passed by `main` into
  `autodetect` into `ensure_running`. Every caller passes the same constant.
  The launcher should own it and drop the parameter.
- `detect::ExplainArgs { pane: Option<String>, file: Option<String>, agent:
  Option<String>, .. }` encodes a sum type that clap enforces and the handler
  re-checks with `CliError::Usage` arms. The proposed shape is
  `ExplainSource::{Pane(PublicPaneId), File { path: PathBuf, agent: AgentLabel }}`.
- `cli::Invocation { launch, help: bool, version: bool }` admits
  `Launch::Cli + help`, which is why `main` has an unreachable
  `"launch was already handled"` arm. `Help` and `Version` belong in `Launch`.
- `ServerStatusJson { running: bool, version: Option, build_id: Option,
  boot_id: Option, compatible: Option<bool>, restart_needed: bool }`
  (`shepr-api/src/schema/server.rs`) is an enum written as a struct. The same
  goes for `SiblingServerJson` ("either the identity ... or `error`"), which
  has four `Option`s. `ClientStatusJson.version`/`build_id` are `Option`, but
  the only producer always fills them. Shepr has no cross-build obligation
  here beyond what discovery reads, so make these serde enums.
- `RuntimeStatus { stopping: bool, starting: bool, .. }` (`shepr-api`): two
  bools whose four combinations `ServerPresence` then folds into three states.
  `ServerPresence::Running(RuntimeStatus)` still carries both bools, which are
  false by construction.

---

## 2. Decisions made in more than one place

### 2.1 "What does this endpoint failure mean for the operator?" (attention / retry / prompt / incompatible)

Sites that answer it independently:

1. `SshFailure::needs_attention` (`shepr-remote/src/lib.rs`).
2. `SshFailureDiagnostic::from_error`'s `ErrorKind` mapping (same file).
3. `classify_check` (`shepr-remote/src/remote/preflight.rs`), a separate
   ladder of predicates: auth, host key, transient, ssh-process-or-local-setup
   to `Failed`, `is_remote_compatibility || needs_attention` to
   `Incompatible`, otherwise `Failed`.
4. `check_after_authentication` (same file), which downgrades an auth wait
   to `Failed`.
5. `shepr-client` `handshake_error`, which picks `ErrorKind`s so that site 2
   lands right.
6. `shepr-client` `connect_once`, which remaps `NotFound` to
   `ConnectionRefused`.
7. `shepr-client` `errors::endpoint_setup_failure`.
8. `shepr-client` `ClientEndpointStatus::after_failure`.
9. `src/preflight.rs::result_notices`. Its prose predicts what the client
   will do ("The client shows it as unavailable and needs attention" /
   "keeps retrying it"), using `needs_attention()` a second time.

**They already disagree.** Two remote status-JSON parse failures are
classified differently:

- `discovery::remote_client_status` reports unparsable `status client` output
  as `io::ErrorKind::InvalidData`. That becomes `Compatibility`: needs
  attention, preflight `Incompatible`.
- `server_lifecycle::parse_remote_server_status_json` reports unparsable
  `status server` output as `io::Error::other(..)`. That becomes `Other`:
  no attention, a silent retry, preflight `Failed` with "The client keeps
  retrying it".

The same fault (a remote shepr printing JSON this build cannot read) is
Attention through one command and a silent retry through the other. Nothing
catches it, because the decision is the `ErrorKind` that one author happened
to pick.

**Owner.** `EndpointFailure::disposition()` (1.1), defined in the crate that
owns the failure type (see 3.4). Preflight's `MachineCheck` becomes a
projection of `disposition()` plus the `Ready`/`DifferentBuild` success
states, not a ladder of its own. `result_notices` asks the same method and
words what it returns.

### 2.2 "Does this failure prove anything about the remote install?"

Sites:

- `DiscoveryProgress::advance` keeps progress only when the failure is
  `is_transient_network_failure() || is_authentication_wait_timeout()`.
- `DiscoveryProgress::run_remaining` ends the pass on
  `failed_before_remote_result`, and otherwise records the first candidate's
  rejection.
- `MachineProbe::resolve` keeps a cached hint on
  `failed_before_remote_result`, drops it on `is_remote_candidate_mismatch`,
  and keeps it on anything else.
- `MachineProbe::observe_failure` invalidates on remote exit `126 | 127`.
- `path_lookup_result_with_rejected_candidate` turns a nonzero lookup into
  "not found" unless `failed_before_remote_result`.

These give different answers to one question. After a host-key or
authentication refusal, `resolve` keeps the disk hint ("says nothing about the
cached executable"), yet `advance` wipes discovery progress ("the next attempt
starts from fresh discovery"). Both are documented, but they are two models of
what an SSH refusal proves, kept consistent by prose. One
`EndpointFailure::evidence() -> {NothingLearned{transient}, InstallStale,
CandidateMismatch, InstallChanged, RemoteFault}` would let progress retention,
hint retention and invalidation each read one answer.

### 2.3 The restart offer: local and remote are the same engine written twice

| | local (`src/preflight.rs`) | remote (`shepr-remote/src/remote/preflight.rs`) |
|---|---|---|
| loop | `restart_local` | `restart_different_builds` |
| limit | `MAX_LOCAL_OFFERS` (`src/limits.rs`) | `MAX_RESTART_OFFERS` (`shepr-remote` limits) |
| outcome enum | `LocalRestart` (`NotNeeded`, `NoTerminal`, `Declined`, `Stopped`, `OccupantChanged`, `Failed(String)`) | `RestartResult` (the same minus `NotNeeded`) |
| offer text | `local_offer` | `remote_offer` |

They already diverge:

- **"No server left to stop."** Local maps `ServerStopError::NotRunning`
  straight to `OccupantChanged` without probing again. Remote maps
  `NO_SERVER_EXIT_CODE` to `RemoteStop::BootChanged`, re-checks, and may offer
  again.
- **A kept or unasked server.** The remote side prints a "left running, run
  this to stop it" notice. The local side prints nothing and leaves it to the
  launch error.
- **Display of identities.** Local prints the raw `status.build_id`. Remote
  prints a sanitized one.

**Owner.** One `restart_offers(targets, prompter)` in the crate that owns
launching (see 3.1), over a trait such as `RestartTarget { fn observe(&self)
-> Option<DifferentBuild>; fn stop(&self, &DifferentBuild) -> StopOutcome; }`,
with a local and a machine implementation. `src/preflight.rs` keeps only the
wording.

### 2.4 The `server stop` command line is spelled three times

- `RemoteCliCommand::ServerStop` (`shepr-remote/src/remote/args.rs`) with
  `RemoteExecutable::command`, which shepr runs.
- `src/preflight.rs::remote_stop_command` hand-formats
  `"ssh {} {} server stop --expect-boot {}"`, which the operator is told to run.
- `shepr-config/src/address.rs::ServerAddress::stop_command` formats
  `"{entrypoint} server stop"`, used in every build-mismatch guidance.

If the subcommand or flag is renamed, the spec, the producer and the round-trip
test all move together, while the two printed copies keep telling the operator
the old spelling. **Owner:** the grammar type of 2.5. `remote_stop_command`
would become `interactive_shell_command(["ssh", target] ++
executable.argv(ServerStop{boot}))`, and `stop_command` would render the same
value.

### 2.5 The command-line grammars: producer and parser are separate copies

- **`shepr`.** `RemoteCliCommand::args` (`shepr-remote`) produces argv. The
  clap builder in `src/cli/spec.rs` plus the hand-written typed parsers
  (`status::parse`, `server::parse`, `detect::parse`) consume it. Shared name
  constants (`COMMAND_*`, `FLAG_*`) keep the spelling in step, but the shape
  (which words, in what order, under which group) is held only by the
  pairwise test `generated_remote_cli_arguments_parse_with_the_cli_spec`. The
  spec and the typed parsers are themselves two copies, held by
  `every_cli_spec_root_has_typed_parser` and
  `every_cli_spec_leaf_parses_to_a_typed_command`.
  `root_exit_flags_before_subcommand` (`src/cli.rs`) is a fourth copy of the
  subcommand list, with `"detect"` as a bare literal (as in
  `CliCommand::from_matches`).
- **`shepr-server`.** `local_server.rs` produces `--client-spawned` (shared
  constant) and `--version` (a string literal in `read_server_version_line`).
  `crates/shepr-daemon/src/main.rs` parses both with its own `VERSION_FLAG`
  constant and a slice match. No test ties them together.

**Owner.** One `SheprInvocation` enum and one `ServerInvocation` enum, each with
`argv()` and `parse()` in the same type. The natural home is next to
`daemon_exit` and `server_stop` in `shepr-api`, which already holds half of
this vocabulary; a small `shepr-cli-grammar` module also works. The clap spec
can then be generated from or checked against that enum. The other option is
`clap` derive, which makes the spec and the typed value one declaration. The
CLI is small (two `status` leaves, `server stop`, two `detect` leaves and two
hidden launches), so a hand-written `parse` over the enum is also reasonable
and would drop the `matches.rs` layer entirely.

### 2.6 The `--version` line format

- `crates/shepr-daemon/src/main.rs` prints `shepr-server {build_version()}`.
- `src/main.rs` prints `shepr {build_version()}`.
- `shepr_protocol::build_version` formats `"{CARGO_PKG_VERSION}+{BUILD_ID}"`.
- `local_server::parse_server_version_line` strips `SERVER_BINARY_NAME`,
  `rsplit_once('+')` and rejects whitespace.

The format is written in one crate and parsed in another, with no shared type.
**Owner:** a `BuildIdentity { version, build_id: BuildId }` in `shepr-protocol`
with `Display` and `FromStr`. `SiblingServerJson`, `ClientStatusJson` and
`RuntimeStatus` then carry it as one value instead of two strings.

### 2.7 Shell quoting: four implementations, two already disagree

- `shepr-remote/src/remote/launch.rs::shell_quote` quotes a leading `=`
  (zsh expands it) and uses `RemoteExecutable::is_shell_plain_word`.
- `shepr-config/src/address.rs::shell_quote` copies the same character set
  but does not quote a leading `=`. A dev entrypoint or socket override
  path starting with `=` is printed unquoted into guidance that the
  operator pastes into zsh.
- `shepr-agent/src/integration/command.rs` uses the `'"'"'` escape.
- `shepr-test-support/src/fixture.rs` has its own.

`shepr-server` depends on `shepr-remote` only to call
`interactive_shell_command` (in `app/agent_resume.rs`). **Owner:** a
`shepr_core::shell` module (`quote`, `join_argv`, `is_plain_word`) that every
crate uses.

### 2.8 "The application paths could not be resolved": four renderings, two classes

- `main::resolve_bridge_paths` gives `CliError::Io("...: a; b")`.
- `cli::resolve_app_paths` gives `CliError::Io("...:\n  a\n  b")`.
- `main::load_validated_config` gives `CliError::Config(vec)`.
- `cli::print_help` gives `"unavailable (a; b)"`.

The cause is the same, `AppPaths::resolve() -> Result<_, Vec<String>>`, but
the class differs (Io vs Config) and so does the layout. The root cause is
that `AppPaths::resolve` returns `Vec<String>` and every caller formats it.
Give it a typed `PathsError` with a `Display`, and have one `From<PathsError>
for CliError`.

### 2.9 "Is this pane owned by a server of my profile?"

- `src/main.rs::should_block_nested_for_env` compares the raw
  `SHEPR_BUILD_PROFILE` text with `BuildProfile::current().marker()`.
- `shepr-config/src/io.rs::resolve_paths_from_env` reads the same variable
  through `BuildProfile::from_marker` (private) and decides whether the
  socket override applies.

Today these agree only because an unknown marker makes `main` say "not
nested" and `resolve` then fails the launch. **Owner:** `shepr-config`
exposes `PaneOwner::from_env() -> {NotInPane, SameProfile, OtherProfile}`
(or `BuildProfile::from_marker` as public), and both read it.

### 2.10 Sanitizing remote text

- `server_lifecycle::printable_remote_text`, `printable_remote_value` and
  `printable_remote_token` are applied at `command_failed`,
  `ssh_bridge_exit_error` and the discovery messages. Control characters
  become `?`, tabs and newlines are kept, CR is dropped.
- `shepr-client` `MachineDiagnostics::insert_machine_diagnostic` filters
  again with `!c.is_control() || c == '\n'`. Controls are dropped rather
  than turned into `?`, and tabs are dropped.

The second filter exists because `SshFailureDiagnostic` cannot say whether
its text was already sanitized. `from_message` and `with_context` accept
anything. **Owner:** a `RemoteText` newtype minted once at the SSH output
boundary. Its renderer is the only way to show it.

### 2.11 Which commands need application paths

`cli::run` routes `status client` and `detect explain --file` away before
resolving paths. `detect::explain` re-checks `args.file.is_some()`, and
`status::run_status_command` still has a `Client` arm that `dispatch` can no
longer reach. With the grammar enum of 2.5 this becomes one match.

### 2.12 Derived fields on the wire that no consumer reads

`ServerStatusJson.compatible` and `.restart_needed`, and
`FullStatusJson.update.restart_needed` (a copy of
`server.restart_needed`), are computed in `src/cli/status.rs`. The only
machine consumer, `server_lifecycle::parse_remote_server_status_json`,
recomputes compatibility from `build_id` and ignores them. Each site calls the
one predicate `is_this_build`, so this is a re-check rather than a duplicated
decision. The fields themselves are dead weight, though: drop them, or make
the consumer read them.

---

## 3. Structure

### 3.1 `shepr-remote` is three crates

1. **The SSH client side.** It covers machine connectors (`machine_ssh.rs`),
   discovery, the bridge's local half (`bridge.rs`), the SSH process
   plumbing (`ssh.rs`, `process.rs`), startup preflight, and the metadata
   cache.
2. **The remote-host side of the bridge.** This is `host.rs`
   (`run_remote_client_bridge`), which runs on the other machine.
3. **The local server launcher.** This is `local_server.rs` (860 lines):
   probing, the launch lock, spawning `shepr-server`, the boot log, the
   sibling-server identity behind `status client`, and the different-build
   policy.

Part 3 contains no SSH at all. It lives here because part 2 calls it, and
part 2 was filed as "remote". It is used by the TUI launch (`autodetect.rs`),
by preflight's local restart offer, by `status client`, and by the bridge
host. Its natural neighbours are `shepr-api`'s `status.rs`
(`ServerPresence`), `server_stop.rs` and `daemon_exit.rs`, which already form
the vocabulary for local server rendezvous.

**Proposed layout.**

- `shepr-launch` (new, below `shepr-remote`): `local_server`,
  `sibling_server_status`, `run_remote_client_bridge` (the host side is just
  "ensure a local server and relay stdio"), the restart-offer engine of 2.3,
  `DaemonExit`, `ServerInvocation`, and the `--version` line parsing.
- `shepr-remote`: SSH only. It depends on `shepr-launch` only for the types it
  reads in status JSON, if those do not move to `shepr-api`.
- `shepr-api` keeps the JSON API. `server_stop` can stay or move with the
  launcher; both the launcher and the CLI use it.

### 3.2 `shepr-server` links the SSH crate for a quoting function

`brokkr.toml` allows `shepr-server -> shepr-remote`, and the only use is
`shepr_remote::interactive_shell_command` in `app/agent_resume.rs`. As a
result, the daemon executable links the bridge, discovery, preflight and the
local launcher. Move quoting to `shepr-core` (2.7), remove the edge, and
remove `shepr-remote` from `shepr-server-layer`'s allow list so it cannot
return.

### 3.3 `lib.rs` flattens a directory with `#[path]`, then globs it into one namespace

`crates/shepr-remote/src/lib.rs` declares eleven modules as
`#[path = "remote/x.rs"] mod x;` at the crate root. It then
`use bridge::*; use discovery::*; use launch::*; use server_lifecycle::*;
use ssh::*;`, and most modules begin with `use super::*;`. In practice the
crate is a single module. `launch.rs` holds `impl RemoteExecutable` methods
(`command`, `bridge_command`, `status_client_command`) for a type defined in
`machine/executable.rs`. `bridge.rs` defines `SSH_OWN_FAILURE_EXIT_CODE` and
`failed_before_remote_result`, which `lib.rs`, `discovery.rs` and
`machine_ssh.rs` reach through the globs. The `remote/` directory name
reflects an older module tree, not the design. Test files
(`*_tests.rs`) follow the same `#[path]` arrangement. Re-nest along the split
in 3.1 and use explicit imports. With that, the visibility of
`pub(super)`/`pub(crate)` means something again.

### 3.4 `SshFailureDiagnostic` is the client's endpoint-failure vocabulary, owned by the SSH crate

`shepr-client` uses `shepr_remote::SshFailureDiagnostic` for every endpoint,
the Local one included. Examples are `initial_local_failure:
Option<SshFailureDiagnostic>` in `lib.rs`, `endpoint_setup_failure`, the
handshake classification, and `ClientEndpointStatus::after_failure`. The name
and the crate are both wrong for that role. The OpenSSH stderr classifier
(`classify_ssh_diagnostic`) belongs in `shepr-remote`. The failure type and
its disposition (1.1, 2.1) belong in a crate both sides already depend on,
such as `shepr-protocol`'s `endpoint` module, or the new launch crate if the
client takes it. The layer rules also show a quirk: `shepr-client` may not
depend on `shepr-api` but reaches it transitively through `shepr-remote`, and
this coupling is part of the reason the client takes its error vocabulary from
the SSH crate.

### 3.5 A public seam that exists for another crate's test

`shepr_remote::BridgeUpload` and `BridgeUploadEnd` (with `pub` fields) are
exported only so that `shepr-client/src/transport.rs`'s test
`upload_cancellation_preserves_pending_endpoint_download` can drive them.
Production uses them only inside `bridge.rs`. Move that test into
`shepr-remote` (it tests the upload half's cancellation contract), or put it
behind the `shepr-test-fixtures` seam, and make the types crate-private.

### 3.6 The CLI grammar lives in the SSH crate

`PROGRAM_NAME`, `REMOTE_INSTALL_NAME`, `COMMAND_*`, `FLAG_*` and
`option_name_from_flag` are exported from `shepr-remote/src/remote/args.rs`,
and the binary's own clap spec imports its subcommand names from the SSH
crate. See 2.5 for the proposed owner.

### 3.7 `src/`: a launcher whose dispatch is split across three partial matches

- `main.rs::launch_with_args` dispatches in four steps: an
  `if help/version`, then `if let Some(cli_command)`, then
  `if matches!(.. ClientBridge)`, then a `match` with an unreachable
  `ClientBridge | Cli(_) => Err("launch was already handled")` arm. With
  `Launch` covering help and version (1.6), this becomes one exhaustive
  match.
- `cli/target.rs` (`CliContext`) is left over from the removed `--machine`
  targeting. `local` is its only constructor (`test_local` is identical). It
  `Deref`s to `AppPaths`. `restart_guidance`, `attach_command` and
  `socket_label` are one-line wrappers. `build_checked: Cell<bool>` serves
  `send_request` only.
- `cli/server_not_running.rs` is a 47-line file whose production content is
  one response builder plus a `cli_error` identity wrapper.
- `cli/matches.rs` keeps fallible and infallible versions of every helper.
  The infallible ones serve only the root `help`/`version` flags.
- `src/preflight.rs` mixes the operator wording, which belongs in the binary,
  with the local restart engine, which does not (2.3).

**Proposed layout:**

- `main.rs`: parse, then one match.
- `launch/{tui.rs, client.rs, bridge.rs}`: the TUI path absorbs
  `autodetect.rs`.
- `cli/{status.rs, server.rs, detect.rs}`.
- `notices.rs`: all operator wording for preflight and launch.

### 3.8 `shepr-daemon`'s `main` re-decides what `shepr-api` already defines

`DaemonExit::code()` exists and is used only in its own test.
`report_server_error` maps `RunServerError` variants to the raw constants by
hand, and `config_error` does the same for config refusals. Two changes fix
this. `RunServerError::exit_class() -> DaemonExit` should live beside the
error in `shepr-server`. `main` should parse a `ServerInvocation` (2.5) and
end with `ExitCode::from(class.code())`. The `exit_with(i32)` /
`u8::try_from` dance then goes away.

### 3.9 Preflight and the connectors resolve each machine twice

`MachineSshPreflight` keeps one `MachineProbe` per machine and verifies the
remote executable during startup. `MachineSshConnector::new` then builds a
fresh `MachineProbe`. The only state passed from preflight to the connectors
is the disk cache, so on its first connect every connector re-verifies the
cached hint, which costs one more SSH round trip per machine just after
preflight finished verifying it. The per-machine verified state could pass
directly from preflight to the connectors, for example with
`MachineSshPreflight::into_connectors()` or one per-machine object used by
both phases. `RemoteSsh::new` (a fresh temporary config directory) is also
rebuilt on every preflight check and every stop.

### 3.10 Status JSON: one wire contract, two private enums

`src/cli/status.rs` defines `ServerRuntimeStatus` to produce
`ServerStatusJson`. `shepr-remote/src/remote/server_lifecycle.rs` defines
`RemoteServerStatus` to consume it. `ClientStatusJson` is produced in
`status.rs` and consumed by `discovery.rs::parse_client_status_json`, which
falls back to "some line had `version` or `build_id`". With the serde enums
of 1.6 in `shepr-api`, both sides use one type and both private enums go
away.

---

## 4. Types that resolve to primitives

### 4.1 `SshFailureDiagnostic: Deref<Target = str>`

`shepr-client`'s `set_machine_diagnostic` calls `failure.chars()` through
the `Deref`, and then re-sanitizes the text (2.10). Everything else uses
`Display`. Remove the `Deref` and offer `fn text(&self) -> &RemoteText`
(or `display_lines()`), whose type guarantees the sanitization.

### 4.2 `BootId` is parsed, then discarded, at every hop

`shepr_protocol::BootId` is a validated newtype. In the stop path, however:

- `RuntimeStatus.boot_id: String` (`shepr-api`). A test in that file uses the
  non-canonical `"test"`, which the type allows.
- `ServerStatusJson.boot_id: Option<String>`.
- `judge_remote_server` parses the boot id with `.parse::<BootId>()` purely
  as a filter, then stores the `String` in
  `DifferentBuildServer.boot_id: String`.
- `spec.rs::boot_id` value parser parses `BootId`, then returns
  `value.to_owned()`, which becomes `server::Command::Stop { expected_boot:
  Option<String> }`.
- `RemoteCliCommand::ServerStop { expected_boot: &str }`.
- `stop_active_server(paths, expected_boot_id: Option<&str>)`, and
  `ServerStopError::{BootMismatch, OccupantChanged}` with `String` boot ids.
- `restart_local`'s `stop: impl FnMut(&str)`.
- `local_server::boot_id_process_id` parses again to get the pid.

`BootId` itself provides `Deref<str>`, `Borrow<str>`, `PartialEq<str>`,
`PartialEq<&str>`, `PartialEq<String>` and `PartialEq<BootId> for String`, so
a raw string compares equal to it with no parse. Carry `BootId` end to end.
Remove the `Deref` and the cross-type `PartialEq`s, keeping `as_str()` only
for rendering.

### 4.3 A build id has no type

`build_id: String` (or `Option<String>`) appears in `RuntimeStatus`,
`DifferentBuildServer`, `ClientStatusJson`, `SiblingServerJson`,
`ServerStatusJson` and `RemoteServerStatus`. Every judgment goes through
`shepr_protocol::is_this_build(&str)`, which is good: there is one predicate.
Still, the value is free text until someone asks. In
`server_lifecycle::printable_remote_token`, a build id is effectively parsed
by hand ("non-empty printable ASCII, no spaces") just so it can be shown.
Adding `BuildId` (sixteen lowercase hex, per `is_identifiable_build_id`) with
`is_this_build(&self)` and an `Unidentified` case for peers that sent garbage
removes the printable-token helper and the `"unknown"` sentinel (4.6).

### 4.4 `SshTarget: Deref<Target = str>`, routinely unwrapped

`as_str()` is used for the ssh argv (`.arg(target.as_str())` in four places).
It is also used in the following places:

- `SshMetadataCache`, which stores `target: String` and
  `StoredMetadata.target: String`;
- `RemoteSsh::target() -> &str` and `DiscoverySteps::target() -> &str`;
- every message builder taking `target: &str` (`judge_remote_server`,
  `remote_server_compatibility_error`, `ensure_remote_client_build`,
  `ensure_remote_sibling_build`);
- `machine_ssh_error_hint(err, target: &str)`;
- `shell_quote(machine.ssh.as_str())` in `src/preflight.rs`.

The type should offer `fn append_to(&self, &mut Command)`,
`fn cache_key(&self)`, `Display` (already present) for messages, and
`fn shell_word(&self)` for printed commands. With those, no caller needs the
`&str`.

### 4.5 `RemoteExecutable` and `MachineLabel`

- `push_if_new_remote_binary_candidate` compares `existing.as_str() ==
  candidate.as_str()` although `RemoteExecutable` derives `PartialEq`.
  `remote_stop_command` uses `shell_quote(server.executable.as_str())`, which
  re-quotes a value `RemoteExecutable` already guarantees is a plain word
  (`quoted()` exists but is `pub(super)`).
- `MachineLabel` is unwrapped to key the preflight probe map (1.6).

### 4.6 Sentinels standing in for absence

- `"unknown"` appears in `printable_remote_value` (so
  `DifferentBuildServer.version` is documented as "printable or `unknown`"),
  in `status.rs::option_label`, in `local_server`'s
  `version.as_deref().unwrap_or("unknown")` (twice), in
  `current_exe_label`'s `"unknown ({err})"`, and in `detect.rs`'s
  explain printer.
- `125` (`CANDIDATE_NOT_EXECUTABLE`) is an in-band exit code standing for
  "vanished".
- `254` aliases a remote 255 and a native 254.
- `main`'s `u8::try_from(code).unwrap_or(1)`.
- `ServerStatusJson { running: false, version: None, .. }` stands in for "not
  running".

### 4.7 String-typed enums

- `detect.rs::print_detect_error` compares
  `response["error"]["code"] == "pane_terminal_unavailable"`. `ApiErrorCode`
  exists and is used elsewhere in the same crate.
- `detect` explain output: `shepr-agent` produces a typed explanation, then
  `explain_to_json_value` turns it into `serde_json::Value`. The CLI prints
  it by indexing string keys (`"agent"`, `"state"`, `"matched_rule"`,
  `"evaluated_rules"`, `"evidence"`, ...) with `unwrap_or("-")` /
  `unwrap_or(0)` defaults. A renamed field prints `-` silently. Use a typed
  `Explain` struct in `shepr-api` for both the wire and the printer. The same
  applies to `send_request`, which returns `serde_json::Value` and makes every
  command probe `response.get("error")`.
- Request ids `"cli:detect:capture"`, `"cli:detect:explain"` and
  `"cli:server:stop"` are literals. `RequestId` exists in `shepr-protocol`.
- `should_block_nested_for_env` compares profile marker strings (2.9).
- `CliCommand::from_matches` and `root_exit_flags_before_subcommand` match on
  subcommand name strings (2.5).

### 4.8 `CliContext: Deref<Target = AppPaths>`

The wrapper poses as the paths it holds, so `ApiClient::local(context)`,
`context.server_address()` and the like work through auto-deref. See 3.7:
the wrapper should go, or be honest about what it adds.

---

## 5. Bugs, smells and surprises (lateral)

- **B1. A server that is starting at a socket override is refused, and the
  refusal names the wrong cause.** In `local_server::ensure_running`, the
  first probe treats `Starting` and `Stopping` like `NoServer` and falls
  through to `require_own_runtime_address`. With `SHEPR_SOCKET_PATH`
  naming a server that is still restoring panes, the TUI (or the remote
  bridge host) fails with "no shepr server is running at X, which
  SHEPR_SOCKET_PATH selects". The intended behaviour is to wait through
  `Starting` as it does for the runtime address. A `Stopping` server at an
  override gets the same misleading message.
- **B2. The two remote status JSON parse failures are classified
  differently** (2.1): `InvalidData` leads to Attention, while
  `io::Error::other` leads to a silent retry.
- **B3. "Gone" is reported as "replaced".** `stop_remote_server` folds
  `NO_SERVER_EXIT_CODE` into `RemoteStop::BootChanged`, and `restart_local`
  folds `ServerStopError::NotRunning` into `LocalRestart::OccupantChanged`.
  A server that simply exited before the stop landed is then reported as "the
  shepr server ... changed while it was being stopped; no stop was sent to a
  replacement", which is wrong. Keep `NoServer` as its own variant.
- **B4. The account-shell probe sends POSIX syntax to the account shell.**
  `RemoteSsh::user_shell_output` wraps `command -v shepr` in
  `posix_remote_output_command` (`echo; ...; shepr_exit_status=$?; if [ ... ];
  then exit 254; fi; ...`) and hands it to sshd's account shell. In fish,
  nushell or xonsh this wrapper is a syntax error, so the first discovery
  round trip always fails there. The nonzero exit is then read as "not
  found", and discovery falls through to `/bin/sh`. That fallback works, but
  for exactly the shells the comment on `path_via_sh` names, account-shell
  PATH discovery can never succeed, and each fresh discovery wastes one cold
  SSH round trip. (Comments on `bridge_command` show the authors knew the
  account shell may not be POSIX.)
- **B5. `remote_client_status` runs `test -x` twice.** The command is
  `test -x X || exit 125; test -x X && X status client --json`, because
  `status_client_command` brings its own `test -x ... &&`. If the file
  disappears between the two tests, the `&&` exits 1 and is reported as a
  probe failure, not as "vanished". `status_client_command`'s guard is
  redundant here, and its only other use is a test.
- **B6.** `DifferentBuildServer.version` is never read in production. It is
  populated and stored, and appears only in tests.
- **B7.** `DaemonExit::code()` is unused outside its own test (3.8).
- **B8. `MachineProbe::resolve` has a dead arm.**
  `Err(error) if failed_before_remote_result(&error) => return Err(error)`
  precedes `Err(error) if !is_remote_candidate_mismatch(&error) => return
  Err(error)`. A candidate mismatch can never be a failure before a remote
  result (their origins are disjoint), so the first arm adds nothing. It
  reads as if it were a distinct rule.
- **B9. The SSH runtime directory is validated twice per `RemoteSsh`, and
  the two paths can differ.** `SshControlDir::runtime` and
  `write_managed_ssh_config` each call `ensure_ssh_runtime_dir`.
  `write_managed_ssh_config` takes a `control_dir` for the control socket but
  re-derives `runtime_dir` from `app_paths` for the config directory, so the
  two can disagree (they do in tests via `SshControlDir::unchecked`). The
  type exists only to make that test seam possible.
- **B10. The launch-fatal setup error message doubles its context.**
  `StoredSetupError::capture` keeps `error.to_string()`, which already
  carries "could not prepare local SSH paths: ...". `to_io_error` then
  wraps it with `local_setup_error("machine SSH setup failed", ..)`, so the
  operator reads "machine SSH setup failed: could not prepare local SSH
  paths: ...".
- **B11. `shell_quote` divergence** (2.7): `ServerAddress::stop_command`
  guidance does not quote a leading `=`, which zsh expands.
- **B12. `local_offer` echoes the raw `status.build_id`/`boot_id` from the
  local socket, while `remote_offer` echoes sanitized ones.** The local peer
  is the same user, so this is low risk. It is still a second rule about
  what may reach the terminal (2.10).
- **B13. The remote bridge's launch failures reach the client as untyped
  stderr and are retried forever** (1.3), as the bridge host's own doc
  comment admits.
- **Smell.** `preflight::check_concurrently` turns a panicked check thread
  into `MachineCheck::Failed`. Everywhere else in the client, a panic ends
  the process (`fatal_panic`). This is the one place a panic is converted
  into a value.
- **Smell.** `SshFailure::Compatibility` can be produced from an
  `Io(InvalidData | Unsupported)` origin, so `is_remote_compatibility()`
  (origin based) and `failure == Compatibility` (class based) can disagree
  for the same diagnostic. `classify_check` checks only the first;
  `needs_attention` sees only the second. This is the two-axis encoding of 1.1
  showing through.
