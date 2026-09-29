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

## HYGP-001 - The clock is reached ambiently from logic, workspace-wide

**Decision (partial):** the clock seam is adopted from broadarrow, incrementally
as part of the hygiene work rather than wholesale: time is passed in instead of
`Instant::now()` / `SystemTime::now()` being read inside logic, and each
subsystem that gets its seam is held by a scoped textlint in the shape of
broadarrow's `control-loop-reads-the-clock-seam` (drafted for
`shepr-server/src/app/` as B7 in `notes/broadarrow-ports.md`). That settles the
enforcement named below as text rules, not a workspace `disallowed_methods`
entry. Open: every site, subsystem by subsystem.

Reported from every scope. `Instant::now()` / `SystemTime::now()` /
`clock_gettime` are called inside the logic that uses them rather than being
handed in, so behaviour that depends on time is untestable without sleeping.

- `shepr-platform`: `ipc.rs` (2 sites), `clipboard.rs` (4), `client_stream.rs`,
  `process.rs`, `ssh_agent.rs` (2, one already injectable via
  `SshAgentLease::refresh_at(now)`), `remote_bridge.rs` (`clock_gettime`
  directly). `Activity`, `DeadlineReader`, `wait_child_until` and
  `wait_for_process_exits` already take deadlines and could take a clock.
- `shepr-vt` / `shepr-mux` terminal layer: `flush_expired_synchronized_output`,
  `apply_due_resize`, `poll_timeout_ms`, `SubmissionState`. vte's
  `Processor<T: Timeout>` is generic but shepr uses the default
  `StdSyncHandler`, so the 150 ms sync-update timeout can only be waited out
  (`synchronized_output_buffers_until_end_or_timeout` sleeps through it and
  asserts `!flush_expired...` right after a write, which will flake under load).
  Suggested fix: a shepr-owned `Timeout` impl driven by an injected clock.
- `shepr-agent`: `Instant::now()` inside `run_version_probe`;
  `enforce_agent_version` hardcodes `VERSION_PROBE_TIMEOUT` (5 s) at the call
  while `run_version_probe` takes a timeout parameter, so only the inner
  function is reachable from a test; `version_probe_deadline_includes_inherited_stdout`
  sleeps 300 ms + 50 ms of real time as a result.
- `shepr-config`: `TerminalId::alloc()` reads `SystemTime::now()` (see
  HYGV-087). `tab_bar.rs` `Command` interval/timeout effects are equally
  untestable without waiting.
- `shepr-remote`: `RemoteSsh::noninteractive_timeout`,
  `SavedSshConnector::attempt`, `SshStdioBridge::reported_failure`,
  `TeardownRegistry::release_all`, `wait_for_remote_server_shutdown`,
  `wait_for_server_socket`, `wait_with_output_timeout`, `ssh_agent::connect`.
  `EndpointCatalogWatch::poll(now)` is the one place that takes the clock as a
  parameter, and the only one with a fast deterministic test. `bridge_connection`
  hardcodes a 250 ms post-EOF grace; `ssh_agent::Registration` hardcodes a 100 ms
  probe interval, a 500 ms connect timeout and a 10 ms read poll, so
  `registration_retries_when_the_api_is_initially_missing` polls against a
  5 second wall-clock deadline.
- `shepr-mux`: `persist/` takes `now` everywhere and the
  `persist-clock-is-injected` textlint holds it. `Instant::now()` twice in
  `git/status.rs::git_status_snapshot_for_cwd_with_demand`, and the two reads
  measure the retry deadline from after the subprocess ran and the cache check
  from before. `src/terminal/state/**` and `src/pane/process_probe.rs` thread
  `now: Instant` through every entry point and are cleanly testable, so the crate
  already knows the pattern.
- `shepr-server`: `app/` reads time through its clock seam, held by the
  `app-state-reads-the-clock-seam` textlint (two marked sites measure elapsed
  time during I/O). Open: `server/` (`headless.rs`, `render.rs`,
  `internal_events.rs`, `client_views.rs`) still reads the clock inside the
  loop iteration, and the remaining test sleeps outside `app/` go with it.
- `shepr-client` / `shepr-termio`: `endpoint/health.rs` is the model (every
  method takes `now: Instant`, no sleeps in its tests), but
  `EndpointRegistry::insert` does `EndpointHealth::new(Instant::now())` one level
  below that injection point, so `connected_at` and the initial-snapshot expiry
  boundary cannot be driven; `ActivationState` writes
  `self.deadline = Instant::now() + ACTIVATION_TIMEOUT` at five sites, so no test
  drives an activation to its 5 s timeout; `shell/input/mouse.rs` reads the clock
  at six sites inside event handling and `word_selection.rs` at one more, which
  is why `shell/tests/mouse_selection.rs` (1312 lines) never exercises a throttle
  or a double-click window; the clipboard bounded-read helper takes a `Duration`
  and a closure but not the clock it measures against, so its test sleeps 400 ms
  and asserts `elapsed < 300 ms`.

Enforcement named by hunters: a `Clock` trait (or simply passing `now` /
`deadline`, as several sites already do) in `shepr-core`, plus a
`clippy.toml disallowed_methods` entry or a `brokkr.toml` text rule for
`Instant::now` / `SystemTime::now` outside designated modules (the client loop
head, a `Clock` type, tests). Several hunters call this the root cause of most of
their wall-clock test findings.

## HYGP-002 - The process environment is read at the moment of use, from logic

**Decision (partial):** piece 1 of the test-isolation work adopted from
broadarrow (one environment reader and registry in `shepr-core`, after
broadarrow's `core::env`) supplies the enforcement named here: raw
`std::env::var`/`var_os`/`vars`/`vars_os` banned in `clippy.toml` with scoped
`#[expect]` escapes, every variable a registry entry read under one policy, and a
pure `resolve` beside `read` as the testable inner function. Open: moving each
read from the moment of use to launch; the reader bans raw reads, not late ones.

Reported from six scopes. The pattern that works is a pure inner function taking
the values plus one resolution at the edge; several sites have the inner function
and skip the caching or the launch-time resolution.

- `shepr-agent`: `env.rs::AgentIntegrationPaths::resolve()` captures the
  environment and documents the boundary ("install and status code receives this
  value and never consults the process environment"), but the `*_dir()`
  resolvers read `std::env::var_os` themselves and are `pub(crate)`, so
  tests must manipulate real variables through `IsolatedEnv` to steer paths.
- `shepr-mux`: `OscDebugTracker::default()` is `Self::from_env()`, reached from
  `GhosttyPaneCore` construction, so `SHEPR_DEBUG_OSC_EVIDENCE` is resolved once
  per pane rather than once at launch; a typo is a silent no-op rather than a
  launch failure, and the variable is documented nowhere.
  `git/config.rs::git_user_config_paths()` reads `XDG_CONFIG_HOME` and `HOME`
  directly (a fourth implementation of the XDG rule).
- `shepr-api` / root binary: `server.rs` reads `SSH_AUTH_SOCK` inline inside
  `start_server_inner`, so the SSH-agent registry cannot be constructed in a test
  without mutating the process environment - and `SshAgentRegistry::new` already
  takes it as an argument, only the caller hardcodes the lookup.
  `src/main.rs::should_block_nested` reads `SHEPR_ENV` inline (mitigated by the
  extracted `should_block_nested_for_env`). `CliContext::local` reads
  `SHEPR_PANE_ID` and `SHEPR_SOCKET_PATH` in the constructor, which the hunter
  records as the good pattern (captured once at the edge, `caller_pane_from` pure
  and tested).
- `shepr-remote`: `std::env::var("SSH_AUTH_SOCK")`, `std::env::args().next()` in
  `run_remote`. `paths.current_dir()` is threaded properly and the contrast is
  what makes the rest stand out.
- `shepr-client` / `shepr-termio`: `host_modify_other_keys_mode()` reads `TMUX`,
  `TERM_PROGRAM` and `WEZTERM_PANE` when `setup_terminal_with_capabilities`
  happens to run, not at launch; the values never reach `ClientSettings`, so
  nothing can report which host protocol was chosen and no terminal-setup test
  sees the decision. The three reads also use three resolution rules in one
  function (`var_os(..).is_some()`, `var(..).is_ok()`, case-insensitive compare),
  none stated. `ClientProcessRole::from_env` is the counter-example the hunter
  names as the model: it enumerates accepted values, treats absent as `Local` and
  refuses startup on anything else including non-UTF-8.

Enforcement named: resolve every variable once at launch into the validated
config or a settings value carried down, then a
`clippy.toml disallowed_methods` entry for `std::env::var`/`var_os` outside a
designated env or launch module. Two hunters note the client has only four
production `env::var` sites, so the rule is cheap there today.

## HYGP-004 - The process id is reached from logic to build names

Residue. `shepr-platform` now owns randomness in its own `random.rs`
(getrandom-or-fail, no pid fallback) and its generated names no longer embed
the pid; its remaining `std::process::id()` uses are two log fields and the
`/proc` session check. Open: `std::process::id()` embedded in names by
`shepr-agent` (both temp-name generators), `shepr-mux` (the recovery filename
format, so filenames are not reproducible in a test), `shepr-remote`
(`local_forward_socket_path`, `saved_bridge_path`, `SavedSshApiBridge::start`,
`store_private_json` and whatever else still does) and `shepr-server` (the
boot id, HYGV-087).

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
- Tests reach it for convenience: `shepr-agent`'s `resume.rs` builds paths from
  `std::env::current_dir()` (`absolute_test_path`) when it only needs "some
  absolute path"; `shepr-mux` uses `std::env::current_dir()` as a test cwd in
  `pane/runtime.rs`, eight sites in `persist/restore.rs` and
  `workspace.rs::test_adversarial_identity_state`.

## HYGP-006 - Retry and backoff are invented per call site, across four crates, with no shared vocabulary

- `shepr-platform` alone has five shapes: `shutdown.rs` (1 s initial, double,
  cap 60 s, reset while a shutdown is pending), `ssh_paths.rs` (16 immediate
  retries, no delay), `ipc.rs` (`STAGING_ATTEMPTS = 4`, immediate),
  `clipboard.rs::wait_child_until` (fixed 5 ms poll to a deadline),
  `process.rs::wait_for_process_exits` (poll with a 10 ms sleep on poll
  failure).
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

## HYGP-007 - "A deadline and the remaining time until it" is implemented five ways in the client, and is a type nowhere

**Decision (partial):** the `Instant::now()` reads are covered by the clock seam
adopted incrementally with the hygiene work (HYGP-001), and the inline
`Duration::from_secs(5)` in `attach.rs` by the per-crate `limits` modules
adopted the same way (HYGV-036); the attach flush budget now shares the endpoint
writer timeout, and `shepr-platform`'s two `DeadlineReader`s are one shared
reader in `child_io.rs`. Open: the client `Deadline` type.

From `shepr-client` / `shepr-termio`:

- `handshake.rs`: `Instant::now() + read_timeout` then `min` with an optional
  caller deadline. The hunter calls this one right, and the only one that
  composes.
- `endpoint/writer.rs`: `Instant::now() + WRITE_TIMEOUT` plus a poll loop with
  `thread::sleep(IO_POLL_INTERVAL)` checking `Instant::now() >= deadline`.
- `terminal_setup.rs`: `Instant::now() + HOST_KEYBOARD_QUERY_TIMEOUT` with its
  own `checked_duration_since` remaining-time computation and its own
  `i32::try_from(..).max(1)` millisecond conversion.
- `activation.rs`: five copies of `Instant::now() + ACTIVATION_TIMEOUT`
  (HYGP-001).

Enforcement named: one small `Deadline` type with `remaining()`,
`remaining_millis_i32()` and a `min` combinator, plus a text rule against
`Instant::now() + Duration::` outside it.

## HYGP-008 - Cleanup on the error path is hand-rolled, with RAII guards available and used for only some resources

- `shepr-remote`, six sites and three shapes: `bridge.rs::start_command` has
  three separate `if let Err(e) = .. { remove_socket_file_if_owned(..); return
  Err(e) }` blocks; `bridge.rs::bridge_connection` does `child.kill();
  child.wait();` at five places, sometimes with `stdout.finish()` /
  `stderr.finish()` and sometimes not, alongside `terminate_bridge_child` which
  is the same thing as a helper for three of them;
  `ssh.rs::write_managed_ssh_config` uses an inline closure plus `remove_dir_all`
  on error; `process.rs::wait_with_output_timeout` duplicates kill/wait/finish/
  finish in the error arm and the timeout arm;
  `catalog.rs::store_private_json` has two `remove_file(&temp_path)` error arms.
  `ManagedSshConfigDirectory` and `TeardownRegistration` show the crate already
  knows the guard pattern and applies it to two resources; the socket, the child
  process and the temp file are left manual.
- `shepr-agent`: `file_ops.rs::write_managed_asset` and
  `config_file.rs::Replacement` both implement "unique sibling temp name from pid
  plus an `AtomicU64`, up to 128 attempts, write, publish by rename, remove the
  temp on failure", with two counters (`NEXT_ASSET_TEMP`, `NEXT_TEMP`), two
  temp-name formats (`.{name}.shepr-{pid}-{seq}.tmp`,
  `.shepr-config-{pid}-{seq}.tmp`) and two failure cleanups (explicit in one,
  `Drop` in the other). The difference that matters (managed assets get fresh
  permissions, user configs preserve the original's) is real; the allocation loop
  is not. Suggested: one `AtomicReplace` helper parameterised by the permission
  policy.

Enforcement named: guard types make the leak unrepresentable; no lint catches the
manual form.

## HYGP-009 - Resources that can grow without bound when something upstream misbehaves

**Decision (partial):** the `shepr-test-support` kept-scratch leak goes with
piece 2 (scratch under the project's `target/` tree, adopting broadarrow's
per-process slot locks so a rerun takes the same slot and clears its trees in
place; nothing relies on `atexit`). Open: every other bullet.

Reported from seven scopes. Several crates are careful, which is what makes the
gaps visible; the hunters recorded the good cases too so the absence is on the
record.

- `shepr-vt` / `shepr-pty`: terminal replies that overflow the inbox are dropped
  with no counter and no log (documented as deliberate); resize replies refused
  by `reserve` in `replace_resize`; replies from the timer before the actor
  handle is set.
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
- `shepr-remote`: `ssh_agent::Registration`'s worker loops at 10 Hz for the
  entire life of the remote bridge process whenever the API socket never appears
  - bounded in memory, unbounded in wakeups, and nothing logs after the first
  `debug!`. `TeardownRegistry.pending` grows with every bridge and every managed
  config, with entries removed on `Drop`, so a leaked owner leaks a registry
  entry; no cap, no metric. `PipeCapture` is properly bounded (1 MiB stdout /
  16 KiB stderr). `ssh_agent::connect` caps the response at 4096 bytes and then
  falls out of the loop reporting `TimedOut` rather than "response too large" - a
  bound that misreports.
- `shepr-mux`: `io::load_history` reads `session-history.json` with
  `read_to_string` and no size cap and then parses the whole thing - that file
  holds every pane's full scrollback, so restore reads it entirely into memory
  twice; `git/discovery.rs` caps ref files at 64 KiB, so the crate knows the
  pattern. `session-snapshots` is pruned to `SNAPSHOT_LIMIT` only when pruning
  succeeds; `session-backups` is pruned to 3 with warn-on-failure, forever.
  `OscDebugTracker::pending` grows until `drain_pending`, which its only caller
  does immediately, so it is bounded in practice with nothing structural saying
  so. Of the seven per-source maps on `TerminalState` keyed by strings from hook
  reports, three are capped (`MAX_METADATA_SOURCES`, `MAX_SEQUENCE_SOURCES`,
  `MAX_STATE_LABELS_PER_SOURCE`) and the hunter found no cap on
  `hook_report_sequences`, `hook_report_accepted_at`,
  `suppressed_full_lifecycle_hook_reports` or
  `stale_full_lifecycle_hook_sessions`: a misbehaving hook reporting a fresh
  `source` string per invocation grows those four without bound, per pane, for
  the life of the server. Suggested: one `BoundedSourceMap<V>` for all seven with
  the cap as a construction parameter.
- `shepr-client`: unusually good - `MAX_NOTICES` (64),
  `MAX_PENDING_PASTE_BYTES` (16 MiB), `MAX_QUEUED_BATCHES` / `MAX_QUEUED_BYTES`,
  `MAX_RETIRED_REQUESTS_PER_ENDPOINT`, `MAX_ENDPOINT_RESPONSE_BYTES`,
  `MAX_BUFFERED_HOST_INPUT`, `MAX_ORPHANED_SGR_MOUSE_TAIL_BYTES`,
  `MAX_DISCARDED_CONTROL_TAIL_BYTES`. Two gaps:
  `EndpointRegistry::failures: Vec<EndpointTransportFailure>` has no cap and is
  bounded only by the loop cadence that drains it, so a transport producing
  failures faster than the loop drains grows it; and `MAX_NOTICES` is declared
  inside a function body, invisible to anyone auditing the crate's limits.
- `shepr-api`: `EventHub::MAX_EVENTS` (512) bounds retained history. Recorded as
  handled.

## HYGP-010 - Locks held across blocking work, and a lock order documented only in scattered comments

- `shepr-pty` / `shepr-mux`: `read_chunk` in `actor.rs` holds `response_order`
  across the whole `on_read` callback, which runs `apply_process_result` and so
  `resolve_default_color_owner` (a `/proc` scan) and `publish_reported_cwd` (a
  readlink). `PaneRuntime::resize` and `apply_host_terminal_appearance` take the
  same lock from the app side, so they block behind another thread's `/proc`
  walk. The comment in `backend.rs` says the caller releases the terminal and
  content locks before the scan; the reply-order lock is still held. The lock
  order itself (response_order > content_write_lock > terminal core, with the
  inbox lock never held across a syscall) has no single documented home - it is
  spread across comments in `actor.rs`, `runtime.rs` and `backend.rs`. Fix:
  return the effects from `on_read`, run them after the lock is released, and put
  the lock order in one place.
- `shepr-remote`: `SavedSshConnector::state` is a `Mutex<ConnectorState>` held
  across the whole 25-second attempt, including the SSH child spawn, discovery
  round trips and the caller's `establish` handshake. The comment says it
  contends with nothing because the supervisor serialises attempts - a claim
  about another crate. As the hunter puts it: if it is truly serialised the mutex
  is unnecessary; if it is not, this is a 25-second stall; either way one of the
  two is wrong. Fix: move the mutable state behind `&mut self` so the
  supervisor's exclusive ownership is the enforcement and concurrent attempts are
  a compile error.
- `shepr-protocol` / `shepr-config`: checked, no lock held across a suspension
  anywhere in either crate. `shepr-server`: no terminal-core lock held across an
  await in that scope.

## HYGP-011 - Process-global mutable state whose consistency rests on the order calls happen to be made in

**Decision (partial):** the `shepr-test-support` statics bullet goes with piece 2
(scratch under the project's `target/` tree, adopting broadarrow's scheme): the
`atexit`/`Drop` pair is replaced by a once-resolved base, a claim registry keyed
by resolved path and a lifetime slot lock. Open: every other bullet.

- `shepr-test-support`: `SCRATCH_ROOT_OWNER`, `SCRATCH_ROOT`,
  `KEPT_SCRATCH_DIRS` and `NEXT_SCRATCH` are four separate statics whose
  consistency rests on `ensure_exit_cleanup` being called before any of them are
  read. Two writers (the `atexit` handler and `Drop`) can both remove the same
  path; harmless because both ignore errors, which is exactly the "invariant
  maintained by two writers who have never been introduced" shape. Suggested:
  bundle them into one `OnceLock<ScratchState>` so the ordering is structural.
- `shepr-mux`: `static PANE_TEARDOWNS_IN_FLIGHT: Mutex<usize>` and
  `static PANE_TEARDOWNS_DONE: Condvar`. `wait_for_pane_session_teardowns` waits
  on a count global to the process, not scoped to a server, so two servers in one
  process (which the test suite does) share it: one server's shutdown wait blocks
  on the other's pane teardowns, and a leaked count from a panicking teardown
  thread makes every later wait time out. The `Drop` impl's `saturating_sub`
  absorbs an unbalanced decrement silently. Suggested: hang the counter off the
  thing that owns the panes (an `Arc<TeardownTracker>` handed to
  `shutdown_pane_processes`), holdable by a text rule against `static.*Mutex` in
  the crate.
- `shepr-remote`: `SSH_TEARDOWN` is a process-global `static TeardownRegistry`
  whose `release_all(grace)` drains everything, so any caller invoking it disarms
  every other owner's cleanup - safe only because exactly one call site exists,
  in the client's exit path. The registry and the individual `Drop` impls are two
  writers coordinated by a `Condvar` and a comment about field declaration order
  (`ManagedSshConfigDirectory`'s `_teardown` "declared after `path`").
  `bridge.rs`'s `failure_rx: Arc<Mutex<Receiver<io::Error>>>` is locked by both
  the accept thread (`discard_unclaimed_bridge_failure`) and `reported_failure`,
  and correctness rests on the accept thread discarding before accepting; the
  polling loop in `reported_failure` exists specifically to avoid starving the
  other side. Suggested: a single-slot `Mutex<Option<io::Error>>` with explicit
  generation numbering.
- Root binary / `shepr-api`: `CliContext::build_checked` is a `Cell<bool>` whose
  invariant is "one build check per target per process". The flag is set on the
  first successful status probe and never invalidated, and
  `src/cli/target.rs::api_client` can silently rebuild the SSH bridge
  (`target.bridge.take()`, then `start(..)` with `use_cached_metadata = false`)
  after the flag was set - for a machine whose remote binary was replaced between
  the two, subsequent requests skip the build check. In practice the rediscover
  path only runs inside `server_status`, before `mark_build_checked`, so it is
  safe today by ordering, not by structure. Suggested: make the checked state
  part of the bridge or target value it describes, so replacing the bridge
  necessarily clears it.
- Also in this class: `PaneId::NEXT_PANE_ID` and `NEXT_TERMINAL_ID` (HYGV-087).

## HYGP-014 - Clamp-or-reject, and overflow policy, are chosen by the call site

- `shepr-protocol`: the counter half is resolved (`checked_next()` is the one
  increment). `geometry.rs::ProtocolCellSize::from_host` clamps and `from_wire` rejects (both
  documented, reasoning sound); `input.rs::ClientSurfaceSize::clamped` clamps
  while `limits.rs::surface_grid_size` rejects, and the two express the same cell
  budget by different arithmetic (division vs multiplication) - that pair is tied
  by `wire_tests::client_surface_clamp_fits_server_geometry_limit`, which the
  hunter names as the pattern the other pairs lack.
- `shepr-pty` / `shepr-config`: `to_std_command` quietly substitutes home for a
  bad cwd (warn only) while the API validates `new_cwd` upstream - two policies
  for one value.

## HYGP-015 - The validate-on-deserialize shadow-struct idiom is hand-written four times

From `shepr-protocol` and `shepr-config`: `geometry.rs`
(`ReceivedTerminalGeometry`), `address.rs` (`ServerAddress`'s `Wire`), `io.rs`
(`AppPaths`'s `Wire`, ten fields), `validated.rs` (`ValidatedConfig`'s `Wire`).
Each repeats its type's full field list and then a field-by-field move. Adding a
field to the outer type is a compile error in the struct literal, so the copies
cannot silently drift - credit the hunter gave - but it is four independent
implementations of one rule.

Enforcement named: a small derive or macro
(`#[validated_deserialize(validate = "validate_resolved")]`), after which the
rule exists once.

## HYGP-017 - Launch-environment validation exists twice, with different rules and different operator text

From `shepr-server` and the root binary: `app/api/env.rs` validates a JSON map;
`src/cli.rs` parses `KEY=VALUE`. Divergences today: a key containing `=` is
rejected with `"env key {key} must not contain '='"` by the API and is impossible
by construction in the CLI (which splits on the first `=`); NUL in a key is
`"env key must not contain NUL bytes"` versus `"env must not contain NUL bytes"`;
NUL in a value is `"env value for {key} must not contain NUL bytes"` versus
`"env must not contain NUL bytes"`. So the same rejected request yields different
operator text depending on whether it arrived via `--env` or the JSON API, and
within `api/env.rs` alone two of the four messages name the key and two do not.

Enforcement named: one validator in `shepr-api` (both the CLI and the server
depend on it) returning one `ApiError`; the CLI's parser then only splits and
delegates.

## HYGP-018 - The environment handed to panes is inherited wholesale and scrubbed by a denylist split across crates

**Decision (partial):** piece 1 (the `shepr-core` environment registry, after
broadarrow's `core::env`) supplies the list of shepr's variables and bans
`set_var`/`remove_var` in `clippy.toml`, so the `unsafe remove_var` of
`SHEPR_STARTUP_CWD` in `bootstrap.rs` has to become something else. Open: the
wholesale `vars_os()` inheritance, the split denylist, and the test that every
registry entry is either scrubbed from or allowed into pane env.

From `shepr-pty` / `shepr-mux` / `shepr-agent`: `base_env` is
`std::env::vars_os()`, then scrubbed by a denylist split between
`pane/launch.rs` (host terminal keys) and `shepr-agent` (agent keys).
Server-only variables are removed ad hoc elsewhere - for example
`SHEPR_STARTUP_CWD` is removed with an `unsafe remove_var` in
`headless/bootstrap.rs`. Nothing lists which `SHEPR_*` variables a pane may
inherit.

Enforcement named: one list in `shepr-config`, plus a test that every
`*_ENV_VAR` constant is either scrubbed or explicitly allowed. Related: the
`shepr-mux` hunter notes `pane/launch.rs` goes to real trouble to scrub
inherited host and agent variables for pane children; the crate's own `git`
subprocesses now go through one runner that scrubs repository and askpass
overrides, so the split denylist is what remains.

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
- `shepr-agent`: `explain` includes `region_preview` (verbatim screen text from
  the user's pane) and `ManifestSource::Override(path)` (a home-directory path)
  in output that goes over the API and into CLI output. The hook assets
  deliberately keep payloads to ids and nothing writes agent transcript text to
  logs, which the hunter calls the right call; `agent_session_path` (a transcript
  path) does travel in reports and could reach logs through a future `tracing`
  call on that path.
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
- `shepr-server`: `render.rs` puts a terminal id into a
  `ServerShutdown` message text sent to the client (benign, but operator text
  assembled at the site). `app/api/workspaces.rs` does
  `let _ = std::fs::remove_dir_all(&source_cwd)` - a recursive delete whose
  failure is discarded; in test code it is fixture teardown, but a recursive
  delete of a path derived from workspace state is the one operation to log
  either way. Suggested guard: a text rule banning `remove_dir_all` outside
  `shepr-test-support`.
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

## HYGP-033 - Test-only helpers that cannot report what their production siblings report

- `shepr-api` `event_hub.rs`: the test-support `events_after` returns
  `Vec::new()` on a poisoned lock and has no `Lost` signal, while
  `events_after_checked` distinguishes both. Eleven call sites in `shepr-server`
  tests use it, and an assertion that a history is empty cannot distinguish "no
  events were emitted" from "the lock is poisoned". Suggested: delete
  `events_after`.

## HYGP-036 - `migration_tests.rs` and `SHEPR_MIGRATION_OBSERVATIONS`: scaffolding for a finished migration

**Decision (partial):** the droid pid gate no longer spawns the host `bash`: it
now launches a `shepr_test_support::fixture::stand_in` named `droid` under a
scratch `PATH`. The stale prose turns out to be partly lintable: the older-peer
compatibility textlint adopted from broadarrow (A3 in
`notes/broadarrow-ports.md`) matches "during the migration", and the
`terminal/state/mod.rs` and `pane/state.rs` prose are gone. Open: the
tautological test, the env var, and the module header.

Reported by the `shepr-vt`/`shepr-pty` and `shepr-mux` hunters. The file (461
lines) is headed "Bounded semantic migration gates ... Keep the same runner for
old/candidate captures". The "old" side is the pre-fork upstream terminal layer;
there is no build of it in this repository and AGENTS.md states there is no
compatibility with upstream.

What tells the hunters it is dead: no in-repo producer of the "old" captures, no
committed fixture to compare against, an env var
(`SHEPR_MIGRATION_OBSERVATIONS`) with one writer and no reader, and a
`capture_bounded_migration_observations` whose only assertion compares the last
observation against observing the same unchanged terminal again - so it passes
for any behaviour the emulator could have while reading as coverage of eleven
semantic dimensions across four geometries.

What is not dead, per the `shepr-mux` hunter: the file's other six tests
(`incremental_rows_reconstruct_full_render`,
`sparse_dirty_patches_preserve_coordinates_and_clipped_rows`,
`dirty_patch_fallback_keeps_previously_collected_rows_dirty`,
`complete_history_replay_supports_plain_append`, and the read-purity checks)
assert real invariants and should stay, under a name saying what they check
rather than what they were once migrated from.

Enforcement named: deletion of the tautological test and the env var, a rename of
the module, and a text rule against `SHEPR_MIGRATION_OBSERVATIONS`.

## HYGP-037 - `shepr-platform`'s `test-support` feature gates one nine-line function with one caller

**Decision:** piece 4 of the test-isolation work adopted from broadarrow (test-only
code leaves production crates' features for dev-only crates, held by
`never-ships` dependency rules and a shipped-feature-set gate check) removes the
feature; `signal_processes` moves to the test side.

Delete the feature, the function (`process.rs::signal_processes`) and the
`features = ["test-support"]` entry in `shepr-server/Cargo.toml`. Full context in
HYGP-031; recorded separately because the deletion is self-contained.

## HYGP-041 - One-line pass-through wrappers, aliases and identity functions

Each of these is a second name or a second hop for one thing; the evidence given
for each is the hunter's.

- `shepr-platform` `logging.rs::help_log_paths_summary(dir) -> String` is
  `log_paths_summary(dir)`. One external caller (`src/cli.rs`); the private
  `log_paths_summary` exists only so the test can call it under a different name.
  Two names, one body, one caller. Make one of them public.
- `shepr-api` `session.rs`: `data_dir_for`, `client_socket_path_for` and
  `api_socket_path_for` are `pub` one-line forwarders to `SessionId` methods
  (one, one and two callers). None is dead; all are redundant indirection that
  makes `shepr-api::session` look like the owner of path layout when
  `shepr-config::SessionId` is.
- `shepr-api` `restart_after_update_guidance` is `pub` with exactly one caller,
  `restart_after_update_guidance_for` in the same file.
- `shepr-agent`: `AgentSource::to_source_string`, `as_str` and `Display` are
  three ways to spell one projection (`to_source_string` is
  `as_str().to_owned()`), plus `PartialEq<&str>` for both `AgentSource` and
  `Agent`. Fine to keep, cheap to collapse. `integration/command.rs` is fifteen
  lines holding two functions, one of which (`shell_single_quote`) is imported
  separately by `targets.rs` to build the Grok command that bypasses the other;
  merging it into the module that owns hook command construction removes a file.

## HYGP-042 - Flags, parameters and constants that have had one value since they were added

- `shepr-protocol`: `read_message`'s `max_frame_size` parameter is passed
  `shepr_protocol::MAX_FRAME_SIZE` at roughly fifteen production call sites
  across `shepr-client` and `shepr-server`; only
  `wire_tests::oversized_input_rejected_custom_max` passes anything else. A
  parameter nobody varies is dead weight and a hazard - a call site can weaken
  the cap and nothing notices. Suggested: drop it from the public function and
  keep a `cfg(any(test, feature = "test-support"))` variant for the one test.
- `shepr-agent`: `AgentDescriptor::prompt_observation` is `true` for Codex alone,
  and `Agent::prompt_ready` hardcodes Codex's own prompt text
  (`"AskCodextodoanything"`, `"model:loading"`, `"Resumingsession"`) plus a
  Codex-specific KMP matcher inside a generic method on `Agent` - a switch with
  one value and Codex logic wearing a generic signature. The hunter's preferred
  route is the Codex manifest (the manifest engine already has `bottom_lines(N)`
  regions and gate semantics that express exactly this), which also deletes
  `contains_recent_non_whitespace`, the 32-char needle array and the 12-line
  constant. `title_activity_glyphs` is non-empty for Claude alone
  (`CLAUDE_ACTIVITY_GLYPHS`) and every other agent has `""` - fine as data, but
  the field reads as a general mechanism and is one agent's detail.

## HYGP-045 - Branches and checks that cannot run

- `shepr-server` `app/actions/focus.rs::commit_workspace_creation` reads the
  root pane through `workspace.tabs().first()`, an `Option` over a tab list
  that can no longer be empty; a non-optional `first_tab()` removes it. A
  `restore.rs` test and the adversarial-identity helper in `workspace.rs` index
  `tabs[active_tab_index()]` where `active_tab()` would do.


- `shepr-client/src/input_wire.rs` and `shepr-server/src/server/input_wire.rs`
  keep one-line forwarding helpers (`WireMouseKind`, `WireMouseButton`,
  `wire_modifiers`, `host_modifiers`) over the protocol wire-type methods the
  conversion moved to; callers can use the protocol methods directly. Its
  eight call sites are in client `attach.rs`, `shell/input/input.rs`,
  `shell/input/mouse.rs` and server `server/pane_input.rs`.
- `shepr-pty/src/command.rs` keeps its own `access_ok` beside
  `shepr_platform::has_execute_access`, because the layering keeps
  `shepr-pty` off `shepr-platform`; worth a comment naming the twin, or moving
  the helper below both.

- `shepr-server` `app/actions/events.rs`: the `AppEvent::GitStatusRefreshed` arm
  of `AppState::handle_app_event` discards both payload fields and returns
  `Vec::new()`. `App::handle_internal_event_with_updates_and_render` intercepts
  the variant before `state.handle_app_event` is ever called, routing it to
  `apply_workspace_git_statuses`, so in production the arm cannot run; in a test
  that calls `state.handle_app_event(GitStatusRefreshed { .. })` directly it
  silently does nothing, which reads as "git statuses were applied". Evidence:
  the only producer path matches the variant first, and the body explicitly
  discards both fields. Suggested: split the event enum so state-level and
  app-level events are different types, making the arm unwritable.

## HYGP-046 - Dead trait impls and duplicate flag constants the compiler will not flag

**Decision (partial):** `#[allow]` gives way to `#[expect(.., reason)]`
workspace-wide (B9 in `notes/broadarrow-ports.md`); the third bullet's two
`#[allow(dead_code)]`s become `#[cfg_attr(not(test), expect(dead_code, reason =
..))]`, since tests read the items. The "`#[allow]` needs a comment" textlint
is dropped in favour of that migration; `AGENTS.md`'s wording changes later.
Whether "documents the table" is reason enough to keep them is open, as are
the first two bullets.

- `shepr-vt`: `Setter::Vte(NamedPrivateMode)` and `ModeSpec::name` carry
  `#[allow(dead_code)]` with the justification "documents the table" and are read
  only by tests.

## HYGP-049 - Enum variants that are constructed but never discriminated

**Decision (partial):** for the `RefFileRead` bullet: the `Path::exists` seal is
adopted from broadarrow (`clippy.toml`; use `try_exists` or match `NotFound`, B4
in `notes/broadarrow-ports.md`), so the stat-error-preserving distinction
`RefFileRead` draws is now the house rule, not dead reasoning, and Git reads
now carry the distinction to the refresh task as typed errors, so the
`RefFileRead` bullet is resolved. Open: the other three bullets.

- Root binary `src/cli/error.rs`: `CliError::source()` matches
  `Stop(error) | Delete(error)` and falls through to `_ => None` for
  `SessionCliError::InvalidName`, though `InvalidName` wraps the same
  `SessionError` type. Either an oversight or an intentional distinction with no
  comment; either way the asymmetry is invisible.
- `shepr-mux` `git/discovery.rs`: `RefFileRead` goes to real trouble to
  distinguish `Absent` from `Unavailable` (with a careful comment about
  `Path::exists()` lying on metadata errors) and its one consumer,
  `read_git_ref_file`, immediately collapses both to `None`. Either the
  distinction should reach the caller - the hunter argues it should, since it is
  the difference between "no branch" and "Git is broken" - or forty lines of
  careful `symlink_metadata` reasoning are dead.

## HYGP-050 - Environment variables with no reader

- `SHEPR_MIGRATION_OBSERVATIONS` (HYGP-036) exists for a migration that is over.
  `shepr-agent/src/integration/config_file/tests.rs` still names two re-exec
  test probes in the `SHEPR_` namespace (`SHEPR_CONFIG_READ_ONLY_TEST`,
  `SHEPR_CONFIG_PARTIAL_WRITE_TEST`); the opencode probe was renamed out of it.

## HYGP-057 - Modules and items sitting in a crate that does not use them

- `shepr-api` `RenderDemand::join` has a dedicated test and one user,
  `shepr-server`. Not dead - flagged because `RenderDemand` lives in
  `shepr-api` while every consumer is in `shepr-server`, so it is in the wrong
  crate for its one client; moving it would tighten `shepr-server`'s use of the
  API crate to actual wire concerns.
- `shepr-api/src/schema/integrations.rs` is 47 bytes, a single re-export. Not a
  problem; noted because the module boundary buys nothing there.
- Related, filed in the sibling documents but pointed at from here because the
  fix is a move: `shepr-platform/src/logging.rs` holds 25 domain event functions
  named after concepts that crate knows nothing about.

## HYGP-058 - Dependencies that production code does not use

- `shepr-core` depends on `ratatui`: `layout.rs` uses
  `ratatui::layout::{Direction, Rect}` and `brokkr.toml` allows it, putting a TUI
  rendering crate at the bottom of the layering where `shepr-mux`,
  `shepr-protocol` and `shepr-config` all inherit it. `Rect` and `Direction` are
  four `u16`s and a two-variant enum; owning them in `shepr-core` alongside
  `GridSize` would drop `ratatui` from the bottom four crates' dependency closure
  and remove a re-export the wire types currently share with the renderer.

## HYGP-062 - `modes::lookup(DecMode)` returns `Option` for a table that holds every variant

`crates/shepr-vt/src/modes.rs`: now that modes are a `DecMode` enum and the
`MODES` table has a row for every variant, `lookup` cannot miss, so the
"unsupported DEC private mode" branch in `mode_set` is unreachable. Make the
lookup total (a `match` on `DecMode`, or a table indexed by the variant) and
delete the branch.
