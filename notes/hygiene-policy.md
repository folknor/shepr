# Hygiene: policy invented per call site, and code that is no longer load-bearing

This file consolidates the findings of the nine-scope hygiene hunt for two of the
eight questions the hunters were asked: question 7 (one rule implemented
independently wherever it was needed, ambient dependencies reached from logic,
shared mutable state whose safety rests on call order, unbounded resources,
secrets in diagnostics, test-only shortcuts production can reach) and question 8
(modules, functions, flags and configuration keys that are no longer
load-bearing). Findings about duplicated or unfindable values, output channels
and error handling, and tests, guards and stale claims are filed in sibling
documents; live defects are in `notes/bugs.md`. This is a working document
assembled from reading, not from running anything: entries may be wrong, and a
later fix pass is expected to find phantoms. Where two hunters read the same
thing differently, or where a hunter marked a claim as an unverified inference,
the entry says so.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

---

## HYGP-066 - Clock seam residue

The clock seam (time passed in, each converted subsystem held by a scoped
textlint) now covers `shepr-server/src/app/`, the headless loop, mux
`persist/`, `shepr-remote` (with marked I/O timing exceptions), the platform
deadline helpers, the client endpoint and activation paths, the agent version
probe and the vt synchronized-update timeout. Open:

- Textlints hold the headless loop, `shepr-platform`, `shepr-agent`,
  `shepr-client` and the vt/pty timing paths. `ClientShellState` mixes explicit
  `now` parameters with an ambient `self.now` that only the input and event
  entry points refresh, so a test that bypasses them runs on the construction
  time; pass `now` explicitly or refresh it in one place.
- Other remaining reads: `shepr-config`'s `TerminalId::alloc` (HYGV-087),
  `shepr-server/src/server/client_transport.rs` and the `shepr-api` transport
  deadlines.
- Tests still sleeping on real time, each with a reason recorded at the site:
  platform process, clipboard helper, bridge and D-Bus tests; mux runtime (50 ms
  and 20 ms); client `handshake.rs` and `terminal_geometry.rs`; server
  `app/mod.rs`, `tab_bar_status.rs` and `client_transport.rs`.

## HYGP-067 - A failed backup prune now blocks every save of an unloaded session

`crates/shepr-mux/src/persist/writer.rs`: when an old recovery copy cannot be
pruned, the new backup is removed and the save fails, where it used to warn and
continue. That keeps the directory bounded, but a single undeletable old copy
now stops every save of that session. Decide which failure is worse; if the
save must go through, keep the new copy, warn, and cap retries some other way.

## HYGP-068 - Small leftovers from the clock and guard work

- `crates/shepr-remote/src/remote/bridge.rs`: `BridgeChildStartupGuard` uses
  `.expect()` in production code, backed by an invariant; make the invariant a
  type or return an error.

## HYGP-004 - The process id is reached from logic to build names

Residue. Platform, agent and remote names come from platform randomness now,
and the boot id has its own constructor. Open: `shepr-mux`'s recovery filename
format still embeds `std::process::id()`, so recovery filenames are not
reproducible in a test.

## HYGP-005 - The working directory is a silent dependency on two paths

**Decision (partial):** the owner adopted broadarrow's rule that every child
process gets a stated working directory (a `clippy.toml` seal on
`std::process::Command::new`, with tests spawning through one helper that sets a
scratch working directory; B6 in `notes/broadarrow-ports.md`). That settles the
direction here - a test's directory comes from a `ScratchDir`, never from where
the runner was invoked - but the seal covers spawned children only and catches
none of these sites: `std::env::current_dir()` read as an input and the
`Path::new(".")` fallback are other spellings (A5's dot-directory textlint needs
a name after the dot). Open: every site.

- `shepr-platform`: `ipc.rs` falls back to `Path::new(".")` for the staging
  parent when the socket path has no parent, so a security-relevant 0700
  directory is created relative to whatever cwd the process happens to have.
- Tests: `shepr-server/src/app/actions/tests.rs` reads `current_dir()` twice,
  and `shepr-server/src/test_support.rs` derives a fixture cwd from
  `current_dir()` with a `/` fallback. The agent, mux and snapshot test sites
  now use `ScratchDir` or a synthetic absolute path. Also in tests: twelve fixed
  `/tmp` path literals in server fixtures (`app/mod.rs`, `app/actions/tests.rs`,
  `ui/panes.rs`) and two in mux (`persist/writer.rs`'s `snapshot()` helper,
  `workspace/aggregate.rs`), and `src/cli.rs` binding a used `IsolatedEnv` as
  `_env`.

## HYGP-006 - Retry and backoff are invented per call site, across four crates, with no shared vocabulary

- Across crates: `shepr-client/src/endpoint/supervisor.rs` owns
  `INITIAL_RETRY_DELAY`, `MAX_RETRY_DELAY`, `ATTENTION_RETRY_DELAY`,
  `ATTEMPT_BUDGET` and an exponential `retry_delay(attempt)` - the one properly
  single-owned backoff; `shepr-remote/src/remote/saved.rs` has its own retry
  semantics (drop the remembered executable, resume discovery) with no delays;
  `shepr-remote/src/remote/ssh_agent.rs` retries at a fixed 100 ms forever,
  unbounded; `shepr-api/src/server.rs` has "a bounded backoff" for the accept
  loop, a fourth policy.
- `shepr-api` plus the CLI: `src/cli/agent.rs` has one loop for
  `agent_pane_busy` (deadline `PANE_SHELL_READINESS_RETRY_TIMEOUT`, interval
  `AGENT_START_POLL_INTERVAL`, plus a pinned-terminal invariant check per turn)
  and a second in `wait_for_named_agent` (deadline from `timeout`, same interval,
  a five-way outcome decision). Neither shares anything with `shepr-api`'s wait
  machinery (`wait_for_agent` with `until` statuses) that polls the same server.
  `agent start` is effectively a client-side reimplementation of `agent.wait`
  with extra identity checks; it cannot be tested (its timing constants are
  module-private, see the sibling values document), and it issues three or more
  API requests per 100 ms turn (`PaneGet`, `PaneProcessInfo`, `AgentGet`) for up
  to 30 s. The hunter's structural suggestion is to move the readiness wait
  behind `agent.start` on the server.
- `src/cli/target.rs::server_status` is the recorded standard: retry-and-
  rediscover exists in exactly one place with a comment forbidding its spread
  ("Only this read-only probe may rediscover and retry. Requests that follow the
  probe must never be replayed after an ambiguous SSH failure."). Nothing
  enforces it; a second retry loop elsewhere would violate it silently.
- `shepr-agent`: retry and debounce policy exists in exactly one asset. Only the
  OMP TypeScript asset has `SHEPR_OMP_IDLE_DEBOUNCE_MS` (250) and
  `SHEPR_OMP_RETRY_GRACE_MS` (2500); every other asset fires once and gives up,
  and whether that asymmetry is deliberate is recorded nowhere.
- `shepr-pty`: the resize retry and backoff machinery in `actor.rs`
  (`RESIZE_RETRY_*`, `RESIZE_HOLD_ATTEMPTS`, the reply-holding logic and three
  tests) may protect against an error retrying cannot fix. The hunter marks this
  as an unverified inference: TIOCSWINSZ on a live PTY master appears to have no
  transient failure mode (only EBADF, EFAULT, ENOTTY). Worth confirming before
  deleting; logging once and moving on would do the same job.

Enforcement named: one `retry` helper in `shepr-core` taking a policy value, so a
test can assert the policy and the call sites become data. One hunter notes no
rule can hold this - a shared type is the only lever.

## HYGP-009 - Resources that can grow without bound when something upstream misbehaves

**Decision (partial):** the `shepr-test-support` kept-scratch leak goes with
piece 2 (scratch under the project's `target/` tree, adopting broadarrow's
per-process slot locks so a rerun takes the same slot and clears its trees in
place; nothing relies on `atexit`). Open: every other bullet.

Reported from seven scopes. Several crates are careful, which is what makes the
gaps visible; the hunters recorded the good cases too so the absence is on the
record.

- `shepr-agent`: the `/proc` walk is carefully bounded (five named budgets,
  documented, round-robin frontiers) and `MAX_VERSION_PROBE_OUTPUT` bounds the
  probe; `explain_loaded_manifest` and `install_target` are bounded by the
  manifest limits. Recorded as no alarming case.
- `shepr-protocol` / `shepr-config`: nothing in scope grows without bound - every
  wire collection is capped (`MAX_COLLECTION_ITEMS` by default, tighter per field
  where declared), depth is bounded, `FramePayloadBuffer` counts excess bytes
  without retaining them. The one collection with no declared cap is
  `ConfigProvenance::values`, bounded by the config schema but shipped on the
  wire inside `resolved_config` on every first snapshot per connection.
- `shepr-api`: `EventHub::MAX_EVENTS` (512) bounds retained history. Recorded as
  handled.

## HYGP-018 - The environment handed to panes is inherited wholesale and scrubbed by a denylist split across crates

Residue. `shepr-mux/src/pane/launch.rs` now has an exhaustive per-variable pane
policy over the whole `shepr-core` registry, held by a test, and the unsafe
`remove_var` is gone. Open: panes still inherit the server environment
wholesale (`shepr-pty/src/command.rs::base_env`), and agent variables are still
scrubbed from a separate list owned by `shepr-agent`
(`launch_env_to_scrub`), so the denylist is still split in two.

## HYGP-030 - Secrets and personal data reaching logs, diagnostics and world-visible names

Reported from every scope. Several scopes found nothing and said so, which is
recorded here so the absence is not re-hunted.

- `shepr-platform`: `remote_bridge.rs` and `remote_bridge_io.rs` both carry an
  explicit module-level rule ("input content must stay out of logs and error
  messages here; byte counts and error kinds only") and honour it. No equivalent
  note exists on the clipboard path, which handles the same class of content (the
  user's selection, potentially a token pasted between panes) and spawns it
  through an argv-visible helper process. Nothing leaks today because no
  clipboard log line exists at all, which means the first person to add one is
  the person who will leak it. The whole fix is a module-level comment matching
  the bridge's.
- `shepr-config`: `ConfigProvenance` stringifies every config value into
  `ConfigValueOrigin.value` and ships it to every attached client, including
  `terminal.default_shell`, `terminal.new_cwd` (an absolute path),
  `ui.tab_bar_right` command lines, and `AppPaths`'s full home, state and runtime
  paths. Nothing is a credential today, but `tab_bar_right` `Command` entries are
  arbitrary user-controlled shell command strings landing in a structure designed
  to be displayed. Exposure surface noted; no fix proposed.
- `shepr-api` / CLI: no leak found. `src/cli/machine.rs` prints SSH targets
  (`user@host`) and `error.escape_debug()` from connection failures; the server
  logs socket paths, not credentials; the `SSH_AUTH_SOCK` path is passed around
  but its contents never logged; `pane.send_text` / `agent.prompt` payloads reach
  the app but no site logs request params - `api_request_started` takes only id,
  name and two bools, which the hunter calls a deliberate and correct choice,
  stated explicitly so a future change does not casually add `%params`.
- `shepr-remote`: handled well in several places - `SshTarget::parse` rejects
  embedded passwords, `copy_local_stream_to_writer` carries an explicit comment
  that it never logs the bytes it copies, a catalog test asserts no
  `password`/`private_key` fields are persisted, and
  `bridge_paths_use_profile_identity_not_target_or_session` pins socket naming.
  Remaining: `local_forward_socket_path` (the `--remote` path, not the
  saved-machine path) puts `sanitize_path_component(target)`, i.e. `user@host`,
  into a world-listable name in the XDG runtime directory, so the two paths
  disagree about whether the target is sensitive; `command_failed` and
  `ssh_bridge_exit_error` fold raw remote stderr (login banner, hostnames,
  usernames, anything the login shell printed) into error messages that reach
  `eprintln!` and the CLI's JSON output, bounded at 16 KiB but unredacted; and
  `src/cli/machine.rs::status` prints that same stderr through
  `error.escape_debug()` into `shepr machine status` and its `--json` form.
  Suggested: extend the existing socket-name test to the `--remote` path.
- `shepr-mux`: `src/pane/terminal/backend.rs` logs
  `osc_command` and `osc_payload` at `debug!` for each drained OSC debug event.
  OSC 0/2/9/21337 payloads are arbitrary child-controlled text - window titles
  and progress strings that routinely carry branch names, file paths and ticket
  numbers - truncated to 512 chars and not otherwise filtered. This has an
  answer: it is off unless `SHEPR_DEBUG_OSC_EVIDENCE` is set and its purpose is
  capturing that text for manifest authoring. Recorded so the decision is
  visible; what is missing is a line in the docs saying the flag puts pane
  content in the log, since the flag is documented nowhere (HYGP-002).
- `shepr-client` / `shepr-termio`: `host_term::title::write_window_title` is
  careful about control characters and says so (titles carry cwd and branch
  text). `workspace_label.rs` passes the cwd and `$HOME` into a label;
  `shell/input/word_bounds.rs` tests use `$HOME`-shaped fixtures. No secret
  reaching a log was found, recorded so it is not re-hunted. One note for
  whoever adds context to `clipboard_forwarding.rs`'s invalid-payload warning:
  add the length, not the data.

## HYGP-031 - Test-only code is compiled into production libraries through Cargo feature unification (`test-api`, `test-support`)

**Decision (partial):** piece 4 of the test-isolation work adopted from
broadarrow is landed: no production crate has a `[features]` table any more.
Shared test doubles live in `shepr-test-support` and the new dev-only
`shepr-test-fixtures` crate, server-only fixtures moved into `shepr-server`'s
own `#[cfg(test)]` module, and `brokkr.toml` forbids any normal or build edge to
either dev-only crate (`test-support-never-ships`,
`test-fixtures-never-ships`). An install feature check
(`install_feature_check = "always"`) compiles the shipped feature set the way
`cargo install` resolves it, closing the gate gap. The seam
`PaneRuntimeIo::TestChannel` needed is built: `shepr-pty::ChildIo` is a boxed
trait object `PaneRuntime` holds, with a `PaneOutputWriter` for the real PTY
read path and a `ChannelChildIo` test double in `shepr-test-fixtures`, so the
enum variant and its six `#[cfg]` match arms are gone. `shepr-agent`'s
`resume.rs::test_codex_plan`, `shepr-platform`'s `process.rs::signal_processes`,
the `ServerAddress` `Default` that validation would reject, and
`EventHub::events_after` are deleted outright rather than feature-gated.
`shepr-server`'s crate-wide `#[cfg_attr(feature = "test-api", allow(dead_code))]`
is gone along with the feature, and `dead_code` reports nothing in that crate
today. `#[allow]` gives way to `#[expect(.., reason)]` workspace-wide (B9 in
`notes/broadarrow-ports.md`). Open: `shepr-protocol`'s public id conversions
are test-only again (`PublicTabId`/`PublicPaneId`'s `From<&str>` are
`#[cfg(test)]` and panic on a non-canonical literal instead of the earlier
`unwrap_or_else` fallback), but `TerminalId::test_new`, `WorkspaceId::new` and
`WorkspaceId::from(&str)` remain `pub` and ungated, so any caller can still mint
an identity that is supposed to come from one place.

## HYGP-045 - Branches and checks that cannot run



- `shepr-client/src/input_wire.rs` and `shepr-server/src/server/input_wire.rs`
  keep one-line forwarding helpers (`WireMouseKind`, `WireMouseButton`,
  `wire_modifiers`, `host_modifiers`) over the protocol wire-type methods the
  conversion moved to; callers can use the protocol methods directly. Its
  eight call sites are in client `attach.rs`, `shell/input/input.rs`,
  `shell/input/mouse.rs` and server `server/pane_input.rs`.


## HYGP-058 - Dependencies that production code does not use

- `shepr-core` depends on `ratatui`: `layout.rs` uses
  `ratatui::layout::{Direction, Rect}` and `brokkr.toml` allows it, putting a TUI
  rendering crate at the bottom of the layering where `shepr-mux`,
  `shepr-protocol` and `shepr-config` all inherit it. `Rect` and `Direction` are
  four `u16`s and a two-variant enum; owning them in `shepr-core` alongside
  `GridSize` would drop `ratatui` from the bottom four crates' dependency closure
  and remove a re-export the wire types currently share with the renderer.
