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
  HYGP-003). `tab_bar.rs` `Command` interval/timeout effects are equally
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
- `shepr-mux`: `SystemTime::now()` at four sites in `persist/writer.rs`, with no
  seam, so the 15-minute snapshot gate can only be tested by setting a file's
  mtime a day into the future (`writer.rs` test). `Instant::now()` twice in
  `git/status.rs::git_status_snapshot_for_cwd_with_demand`, and the two reads
  measure the retry deadline from after the subprocess ran and the cache check
  from before. `src/terminal/state/**` and `src/pane/process_probe.rs` thread
  `now: Instant` through every entry point and are cleanly testable, so the crate
  already knows the pattern.
- `shepr-server`: the main loop takes `let now = Instant::now()` per iteration
  and threads it into several handlers, yet roughly 25 production sites inside
  the same iteration call `Instant::now()` again: `server/headless.rs` (5),
  `render.rs`, `internal_events.rs`, `client_views.rs`, `app/session.rs` (8),
  `app/events.rs` (2), `app/agents.rs` (2), `app/agent_resume.rs` (4),
  `app/api.rs` (2), `app/api/agents.rs`, `app/api/panes/reports.rs`,
  `app/api/workspaces.rs`, `app/git_refresh.rs` (6), `app/tab_bar_status.rs`,
  `app/runtime.rs`, `app/mod.rs`. Deadlines set from a fresh `now` and deadlines
  compared against the threaded `now` disagree by the iteration's duration:
  harmless at 16 ms, load-bearing for the 250 ms save poll and the 300 ms prompt
  delay. Consequence: tests sleep (30 ms, 400 ms, 1100 ms, 5 ms loops).
  `rebuild_shell_session_cache` calls `Instant::now()` while every reader of the
  resulting `built_at` takes an injected `now`.
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

Reported from six scopes. The pattern that works is a pure inner function taking
the values plus one resolution at the edge; several sites have the inner function
and skip the caching or the launch-time resolution.

- `shepr-platform`: `std::env::var_os` in `pathutil.rs`, `host.rs`,
  `terminal_environment.rs`, `clipboard.rs::ClipboardSession::from_env`. Three of
  the four have a pure inner function; `host.rs::detect_running_inside_wsl` does
  not. `prefers_osc52_clipboard` re-reads `SSH_CONNECTION`, `SSH_TTY`,
  `VSCODE_IPC_HOOK_CLI` and stats `/proc/sys/fs/binfmt_misc/WSLInterop` on every
  call, from `shepr-termio/src/host_term/title.rs` on every clipboard write,
  while the WSL answer it combines them with is `OnceLock`-memoised in the same
  crate. What is missing is one startup-time resolution of "does this host have a
  local clipboard".
- `shepr-agent`: `env.rs::AgentIntegrationPaths::resolve()` captures the
  environment and documents the boundary ("install and status code receives this
  value and never consults the process environment"), but all fourteen `*_dir()`
  resolvers read `std::env::var_os` themselves and are `pub(crate)`, so
  `hermes_plugin_dir` calls `hermes_dir` (a second read behind `resolve`) and
  tests must manipulate real variables through `IsolatedEnv` to steer paths.
  `SHEPR_PROCESS_DETECTION` is read behind a `OnceLock` on the first foreground
  probe, and an unrecognised value warns and falls back to native mode: config in
  all but name, discovered hours in, on whichever pane probed first, and
  invisible to `shepr config check`.
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

## HYGP-003 - Identifier allocation reaches process-global counters, and there are two id schemes

- `shepr-core`: `layout.rs` `static NEXT_PANE_ID`; `PaneId::alloc()` reads it,
  and `alloc_from(&counter)` exists purely so the exhaustion test can inject one.
  A test wanting deterministic pane ids must use `from_raw`, which bypasses
  validation entirely (it accepts `0`, the documented placeholder, while
  `collect_validated_ids` rejects `0`). Suggested: `from_raw -> Option<PaneId>`
  is compiler-enforced at every site; removing the global needs an allocator
  value threaded through `Workspace`, which the hunter calls the larger and
  better fix.
- `shepr-protocol`: `ids.rs` `static NEXT_TERMINAL_ID: AtomicU64`,
  `Ordering::Relaxed`, combined with `SystemTime::now()` in the id string.
  Uniqueness rests on the clock being monotonic across the process or the counter
  never wrapping, and `duration_since(UNIX_EPOCH)` falls back to
  `.unwrap_or(0)`, at which point ids become `term_<counter>` only. Suggested:
  own the counter in a `TerminalIdSource` passed to callers.
- `shepr-remote`: `ProfileId::generate` is a hand-rolled scheme (`sha2` of
  `"{pid}:{nanos}:{seq}"` truncated to 16 bytes) beside the existing
  `shepr_platform::unpredictable_token` (getrandom). Its comment says "not
  secrets; practical uniqueness is enough", which was written for the catalog row
  and not for the socket file name in the shared XDG runtime directory that the
  id later became (`saved_bridge_path`, `SavedSshApiBridge`). Fix would also drop
  `sha2` from the crate and from its `brokkr.toml` allowlist.
- `shepr-server`: the client-shell boot id's format lives in a struct literal at
  its only call site: `format!("{}-{}", std::process::id(), SystemTime::now()...
  .as_nanos())`. `shepr_protocol::BootId` is a newtype over `String` with
  `From<String>` and no constructor owning the format, while the value is
  compared in `client_commands.rs`, `client_transport.rs`, `surface_reuse.rs` and
  four places in `shepr-client`. `unwrap_or_default()` means a pre-epoch clock
  collapses every boot id to `pid-0`, silently defeating the stale-boot rejection
  it exists for. Suggested: `BootId::for_this_process()` in `shepr-protocol`,
  with `From<String>` restricted to deserialization.

## HYGP-004 - The process id and the randomness source are reached from logic, with the randomness utility living in the SSH module

- `std::process::id()` appears in `shepr-platform` (`logging.rs` twice,
  `ssh_paths.rs`, `ssh_agent.rs`, `remote_bridge_tests.rs`), `shepr-test-support`
  (4 sites), `shepr-agent` (both temp-name generators), `shepr-mux`
  (inside the recovery filename format, so filenames are not reproducible in a
  test), `shepr-remote` (six sites embedding the pid in a name:
  `local_forward_socket_path`, `saved_bridge_path`, `SavedSshApiBridge::start`,
  `store_private_json`, `create_remote_ssh_config_dir`, `unpredictable_token`)
  and `shepr-server` (the boot id, HYGP-003).
- `ssh_paths.rs::unpredictable_token` is the single owner of randomness, which
  the hunter calls good, but `ipc.rs` reaches across module boundaries into
  `super::ssh_paths::unpredictable_token` for a socket staging name: a
  cross-cutting utility living in the SSH module because that is where it was
  first needed. It belongs in its own module.
- Related caveat from the same hunter, recorded so a future simplification does
  not remove it: on `getrandom` failure `unpredictable_token` hashes only
  `std::process::id()` with a `RandomState` hasher. The unpredictability comes
  from `RandomState`'s OS-seeded keys, so the result is fine today, but the doc
  comment is subtle enough that "why hash the pid at all?" could quietly turn it
  into a predictable value used for 0700 staging directory names and socket
  paths. Either a sharper comment or a getrandom-or-fail policy.

## HYGP-005 - The working directory is a silent dependency on two paths

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
  `process.rs::wait_for_process_exits` (poll with a 10 ms recheck floor and a
  10 ms sleep on poll failure).
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

From `shepr-client` / `shepr-termio`:

- `handshake.rs`: `Instant::now() + read_timeout` then `min` with an optional
  caller deadline. The hunter calls this one right, and the only one that
  composes.
- `endpoint/writer.rs`: `Instant::now() + WRITE_TIMEOUT` plus a poll loop with
  `thread::sleep(IO_POLL_INTERVAL)` checking `Instant::now() >= deadline`.
- `attach.rs`: `Instant::now() + Duration::from_secs(5)` inline, an unnamed flush
  budget that happens to equal `WRITE_TIMEOUT`.
- `terminal_setup.rs`: `Instant::now() + HOST_KEYBOARD_QUERY_TIMEOUT` with its
  own `checked_duration_since` remaining-time computation and its own
  `i32::try_from(..).max(1)` millisecond conversion.
- `activation.rs`: five copies of `Instant::now() + ACTIVATION_TIMEOUT`
  (HYGP-001).

Enforcement named: one small `Deadline` type with `remaining()`,
`remaining_millis_i32()` and a `min` combinator, plus a text rule against
`Instant::now() + Duration::` outside it.

Related, from `shepr-platform`: two distinct private types both named
`DeadlineReader` in one crate - `ipc.rs` (re-arms `SO_RCVTIMEO` per read on a
`LocalStream`, which exists only because `interprocess`'s `set_recv_timeout` is
the only knob on that stream) and `clipboard.rs` (generic
`R: Read + AsRawFd`, polls with `poll_timeout_until`). Same name, same crate,
same concept, two implementations; the clipboard one is the general shape. No
mechanical rule catches duplicate private type names - the enforcement is one
`Deadline<R>` in `child_io.rs` used by both.

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
- `shepr-termio`: `HostModes::restore` runs seven restores plus a title reset
  plus a title-stack pop as nine hand-written copies of
  `if restore_state & FLAG != 0 { let next = <write>; if result.is_ok() { result =
  next; } }`. Suggested: a `Vec<(flag, fn(&mut W) -> io::Result<()>)>` table
  iterated once with `first_error`, so the flag-to-action pairing becomes data.

Enforcement named: guard types make the leak unrepresentable; no lint catches the
manual form.

## HYGP-009 - Resources that can grow without bound when something upstream misbehaves

Reported from seven scopes. Several crates are careful, which is what makes the
gaps visible; the hunters recorded the good cases too so the absence is on the
record.

- `shepr-platform`: `ipc.rs::bind_via_private_staging` leaks a 0700 staging
  directory per failed `remove_dir` (`let _ =`, no log), one per bind attempt in
  the XDG runtime directory; `ssh_paths.rs::create_remote_ssh_config_dir` creates
  `shepr-ssh-<pid>-<token>` directories and never removes them (the doc says
  "ephemeral" and the caller is responsible; nothing sweeps stale ones from a
  killed process); `ssh_agent.rs::publish` leaves `<path>.<pid>.new` behind if
  `symlink` succeeds and the process dies before `rename`;
  `shepr-test-support` kept-scratch directories survive SIGKILL indefinitely
  (`keep_until_exit` relies on `atexit`, and the only cleanup for a stale one is
  a later run reusing the same pid). `read_limited_reader` is the good
  counter-example. Suggested: a startup sweep keyed on "own uid, no live pid".
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
- `shepr-server`: `pending_alt_screen_reads`, `deferred_alt_screen_reads` and
  `queued_agent_manifest_reloads` are uncapped `Vec`s;
  `queued_agent_manifest_reloads` accumulates one entry per
  `server.reload-agent-manifests` request that arrives while a reload runs, so a
  client looping on that method grows it without bound. `shutdown_flushes` is
  bounded by client count. `broken_clients.contains(&client_id)` is a linear scan
  inside the per-render loop, bounded by client count, so cosmetic. Suggested: a
  bounded queue type that rejects with `EndpointBusy` (the code already exists)
  past a cap.
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

- `shepr-platform`: `ssh_agent.rs::SshAgentRegistry::register` and
  `SshAgentLease::refresh_at` hold `Arc<Mutex<State>>` across `State::publish`,
  which does `symlink_metadata`, up to N `connect_sync()` calls through
  `live_socket`, `symlink`, `rename` and another `symlink_metadata`. Each
  `connect_sync` uses `ConnectWaitMode::Timeout(Duration::ZERO)`, so the window
  is short by construction - but the structure, not the timeout, is what keeps it
  short and nothing records that. Every attachment's refresh serialises behind
  it. `logging.rs::RotatingFileState` is the same shape and worse: the mutex is
  held across `flock(LOCK_EX)`, a blocking syscall that waits for another
  process, plus `rename`, `remove_file` and `write`, so every thread emitting a
  log line blocks behind a cross-process lock. The workspace already denies
  `await_holding_lock`; this is the sync analogue and no lint covers it. Fix:
  take the file handle, drop the guard, then write.
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

- `shepr-platform`: `logging.rs::init_file_logging` installs a process-global
  subscriber and discards a second call's error, so a second caller (two roles in
  one process, a test harness, a future embed) silently keeps the first
  subscriber while believing its writer is installed.
  `host.rs::watch_terminal_resize_signal` installs a process-global SIGWINCH
  handler and `TERMINAL_RESIZE_SIGNALLED` is a process-global atomic; the test
  `terminal_resize_signal_is_recorded_once_per_delivery` installs the handler and
  `raise`s SIGWINCH while every other test in the binary runs concurrently, and
  survives only because nothing else in the suite touches SIGWINCH; the handler
  persists for the rest of the process.
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
- Also in this class: `PaneId::NEXT_PANE_ID` and `NEXT_TERMINAL_ID` (HYGP-003).

## HYGP-012 - The manifest registry is a process-global whose first caller fixes the override policy for the process

From `shepr-agent`. `manifest::registry()` initialises the process-wide
`MANIFESTS` `OnceLock` with `override_dir = None` when nothing has called
`reload_manifests(config_dir)` first, and `registry()` and `reload_manifests()`
race for the same `OnceLock` (plus `MANIFEST_INIT_LOCK`, with each doing its own
double-checked init). Only the server calls `reload_manifests`
(`server/headless/bootstrap.rs`, `app/api.rs`, `api_dispatcher.rs`), and in the
server the ordering happens to be right today because bootstrap runs before any
detection tick. Nothing in the build would notice if a future early call to
`has_screen_manifest` or `detect_with_osc` moved ahead of bootstrap: the result
is silently bundled-only detection with no warning.

`reload_lock` correctly serialises reloads, and the test comment ("tests build
their own `ManifestRegistry` ... so they never touch this") shows the design is
understood.

Enforcement named: remove the global - pass a `&ManifestRegistry` (or an override
directory) down, which makes the bad spelling unrepresentable; weakly, a debug
assertion that `detect_with_osc` is never the initialiser. The hunter also
reported the CLI-side consequence of the same root cause as a live defect
(`shepr agent explain --file` explaining against bundled-only manifests), which
belongs in `notes/bugs.md`.

## HYGP-013 - Mutex poison policy is re-decided at every lock site, and one mutex has two policies

- `shepr-mux`: `render_signal.rs` repeats
  `.lock().unwrap_or_else(std::sync::PoisonError::into_inner)` at eight sites
  (`request_generic`, `request_pty`, `set_immediate_pty_sources`,
  `has_immediate_work`, `request_terminal_title`,
  `pending_terminal_title_sources`, `take`, plus the wait path in `teardown.rs`
  going through `shepr_vt::recover_auxiliary_poison`) - continue on poisoned
  state, using the half-built `RenderRequest` a panicking thread left behind.
  Meanwhile `shepr_vt::lock_terminal_core` treats poisoning as terminal for the
  pane and `teardown.rs` routes through `shepr_vt::lock_auxiliary` /
  `recover_auxiliary_poison`, which are shared helpers. Three policies, two with
  owners, and `render_signal.rs` writing its own eight times. Suggested: route
  `render_signal` through `shepr_vt::lock_auxiliary`, held by a text rule against
  `PoisonError::into_inner` outside `shepr-vt`.
- `shepr-server`: the same `Arc<Mutex<SessionWriter>>` has opposite rules about
  twenty lines apart - `app/session.rs` recovers with
  `unwrap_or_else(PoisonError::into_inner)` and retires it anyway at one site,
  and refuses, logs "session writer is poisoned; refusing to modify session" and
  returns an `io::Error` at the other. Elsewhere the crate is consistent
  (`client_transport.rs`, `tab_bar_status.rs` twice, all `into_inner`).
  Suggested: a `SessionWriterHandle` newtype owning the lock and the poison rule.
- `shepr-agent`: `manifest.rs` unwraps poisoned locks into inner values at five
  sites (`unwrap_or_else(PoisonError::into_inner)`,
  `Err(poisoned) => poisoned.into_inner()`). The hunter calls this the right call
  for a cache and consistent, but the choice is re-made at each site; a small
  `fn read_cache(&self)` / `write_cache(&self)` pair would make it one decision.

## HYGP-014 - Clamp-or-reject, and overflow policy, are chosen by the call site

- `shepr-protocol`: `revision.rs`'s `counter!` macro gives every counter both
  `next()` (saturating) and `checked_next()` (returns `None`), plus saturating
  `Add`/`AddAssign`. `surface_reuse::Baseline::accepts` relies on
  `checked_next()`; other callers use `next()`. A saturated `SurfaceRevision` at
  `u64::MAX` would silently stop advancing and every subsequent delta would be
  rejected as a baseline mismatch, forever, with nothing logged. Not reachable in
  practice, but the type offers two answers and lets the call site pick.
  Suggested: keep one; if saturation is never acceptable, delete `next()`.
  `geometry.rs::ProtocolCellSize::from_host` clamps and `from_wire` rejects (both
  documented, reasoning sound); `input.rs::ClientSurfaceSize::clamped` clamps
  while `limits.rs::surface_grid_size` rejects, and the two express the same cell
  budget by different arithmetic (division vs multiplication) - that pair is tied
  by `wire_tests::client_surface_clamp_fits_server_geometry_limit`, which the
  hunter names as the pattern the other pairs lack.
- `shepr-mux` / `shepr-core`: `TerminalState::revision` is bumped at four sites
  under two overflow policies -
  `src/terminal/state/detection.rs` uses `wrapping_add(1)` at one site and
  `saturating_add(1)` forty lines later, and
  `shepr-server/src/app/actions/workspace.rs` and
  `app/api/panes/reports.rs` both use `saturating_add(1)`. Already diverged at
  `u64::MAX`; neither is obviously right, which is the point - nobody chose. Fix:
  make the field private behind one `fn bump_revision(&mut self)`.
- `shepr-core` / `shepr-mux` restore path: `SplitRatio::clamped` never refuses
  and the validating `new` exists only under `#[cfg(test)]`, so
  `persist/restore.rs` silently clamps a corrupt saved ratio instead of refusing
  the layout, while `from_saved` already models an `InvalidSavedLayout`.
  Separately `parse_snapshot` is `pub` and returns a `SessionSnapshot` whose
  types encode no validation (`ratio: f32`, `active: Option<usize>`,
  `selected: usize`, `active_tab: usize`, `focused: Option<u32>`,
  `root_pane: Option<u32>`, `cwd: PathBuf`), and two consumers
  (`preserve_snapshot_history`, `prepare_snapshot_history`) already parse it
  without going through restore's sanitising. They only read
  `version` / `workspaces.len()` / `layout_fingerprint` today, so the harm is
  that the next consumer gets unvalidated data by default.
- `shepr-remote`: `is_launch_fatal_setup_error` decides a launch-versus-retry
  policy from an `io::ErrorKind`, treating every `InvalidInput` as launch-fatal.
  `InvalidInput` is produced by `remote_bridge_endpoint_path` (socket path too
  long), `validate_private_runtime_dir` (relative runtime dir),
  `shared_ssh_control_path`, and `RemoteExecutable::parse` failures arriving
  through other paths. Some are genuinely deterministic; the classification is by
  kind, not by cause. The typed mechanism the same function uses for
  `UnsafeSshRuntimeDirectory` is the right one and is used once. Note the
  existing test `only_typed_runtime_directory_policy_errors_are_launch_fatal`
  pins the looser rule in place, so this needs the test changed, not added.
- Decided: resolved by deleting the `--state-label` feature end to end
  (HYGV-021). `shepr-agent` / `shepr-server`: `IntegrationHookAction::as_str` owns
  `session working blocked idle` and `PaneAgentState` owns the wire spelling, but
  `shepr-server/src/app/api/panes.rs::normalize_state_labels` re-checks
  `matches!(status.as_str(), "idle" | "working" | "blocked")` against a fresh
  literal list. Suggested: parse into `PaneAgentState` and let serde be the
  validator.
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

## HYGP-016 - User-supplied regexes arrive off the wire and are compiled with no size limit, by two independently written call sites

From `shepr-api`: `subscriptions.rs` (`Subscription::PaneOutputMatched`) and
`wait.rs` (`wait_for_output`) both call `Regex::new(value)` on a pattern from an
API request. `regex` is not backtracking so there is no catastrophic-backtracking
risk, but `RegexBuilder::size_limit` defaults to 10 MiB of compiled program per
pattern, `events.subscribe` accepts a list of subscriptions on one connection
each with its own pattern, and there is one connection thread per subscription -
so a client (or a misbehaving agent hook, a local semi-untrusted caller) can
allocate a large multiple of that per connection. The hunter notes this is the
only place in that scope where a value coming off the wire is validated by two
independently written call sites.

Enforcement named: one shared constructor
`fn compile_match_regex(&str) -> Result<Regex, ApiError>` with `size_limit` and
`dfa_size_limit` set, used by both, plus a text rule banning `Regex::new`
outside it.

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
inherited host and agent variables for pane children while the crate's own
subprocesses get none of that care (HYGP-019).

## HYGP-019 - `git` is spawned from four production sites with four policies, no timeout budget and an inherited environment

- `shepr-mux/src/git/discovery.rs::git_trimmed_stdout` and
  `src/git/status.rs::git_ahead_behind_between` (plus
  `src/git/test_support.rs::run_git`) each build
  `Command::new("git").arg("-C").arg(dir).args(..)` independently.
  `shepr-client/src/workspace_label.rs` and
  `shepr-server/src/app/git_refresh.rs` add two more, and the client's copy is
  the least careful of the four: no timeout, no environment scrubbing, every
  failure dropped with `.ok()`.
- Consequences the `shepr-mux` hunter draws out: no deadline anywhere (there is a
  30-second retry delay and no timeout), so `git rev-list --left-right --count`
  on a repository whose objects live on a stalled network filesystem blocks the
  calling thread indefinitely; the environment is inherited whole, so `GIT_DIR`,
  `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_CONFIG_GLOBAL`,
  `GIT_CEILING_DIRECTORIES`, `GIT_ALTERNATE_OBJECT_DIRECTORIES`, `GIT_ASKPASS`
  and `GIT_TERMINAL_PROMPT` all reach the child (the owner is a heavy Git user
  running shepr from inside a repo, and an unset `GIT_TERMINAL_PROMPT` means a
  credential prompt can block the spawn); stdin is inherited, so an interactive
  credential helper has a terminal.

Enforcement named: one `fn run_git(dir, args) -> Result<String, GitReadError>`
with a scrubbed environment (`GIT_TERMINAL_PROMPT=0`, `GIT_OPTIONAL_LOCKS=0`,
`-c core.fsmonitor=false`, null stdin), a deadline and typed errors, held by a
text rule banning `Command::new("git")` outside it. The `shepr-client` hunter
places the shared runner below `shepr-mux`, in `shepr-platform`.

## HYGP-020 - "Flush the synchronized-output buffer if it has expired" is re-implemented at six call sites

From `shepr-vt` / `shepr-mux`: `Terminal::write`, `render`,
`collect_dirty_patch`, `synchronized_output_active`, `synchronized_output_state`
and the runtime timer each re-implement "flush if expired, bump epoch". The epoch
bump itself (`if before != after { epoch += 1 }`) is repeated four times across
`backend.rs` and `helpers.rs`. The seqlock protocol on `content_seq` is
hand-copied at six sites (`on_read`, the timer, `resize`, `clear_screen`,
`test_process_pty_bytes`, `test_contend_during_dirty_collection`), each locking
`content_write_lock`, doing `fetch_add(AcqRel)`, the mutation, then
`fetch_add(Release)`.

The hunter's structural suggestion, which gathers this with several of their
other findings: one terminal-core owner type in mux (merging `PaneTerminal` and
`GhosttyPaneTerminal`, see HYGP-035) that owns the seqlock guard (a
`ContentWriteGuard` whose constructor and `Drop` do the two increments), one
`tick(now)` flush entry point owned by the runtime with render paths taking
`&Terminal` only, the reply-producing closure contract, and effect application
outside the reply lock. "Most findings here are about there being no one place
where those rules live." The render-mutates-terminal and reply-ordering
consequences were filed by that hunter as live defects.

Also from the same hunter, an unenforced mutation rule in the same area: "every
parser-driven mutation must use `with_handler`" and every mutation must end with
`collect_damage()` (or `bump_full_damage`), which callers do by hand in `write`,
`flush`, `mode_set`, `resize` and the scroll methods. Suggested: call
`collect_damage` inside `with_handler`.

## HYGP-021 - The hook assets implement one report protocol twice, with two transports, two timeout budgets and two request-id formats

**Decision (partial):** Hermes support is removed entirely, so the hermes asset
drops out of the lists below.

From `shepr-agent`: the claude, codex, kimi, copilot, devin, droid, grok, cursor,
antigravity, mastracode and opencode assets open the API socket directly with a
0.5 s timeout and hand-build the JSON-RPC envelope
(`{"id": .., "method": .., "params": ..}` plus a newline, then a best-effort
`recv(4096)`). The hermes, qwen, qodercli and letta assets instead exec
`shepr pane report-agent-session` with a 1 s (hermes) or unspecified timeout.
Two implementations of one protocol in about fifteen copies, plus two request-id
formats (`f"{source}:{ms}:{rand:06d}"` in claude, `f"shepr:kimi:{seq}"` in kimi)
and two `seq` sources (`date +%s%N` in the shell prologue,
`time.time_ns()` in Python). Each asset also reaches for its own ambient
`time.time_ns()` / `random.randrange`.

The hunter's consolidation: every asset shells out to `shepr pane report-*` - the
CLI already exists, `SHEPR_BIN_PATH` is already exported, socket framing stays
owned solely by Rust, and the JSON-RPC client disappears from fifteen shipped
scripts. Called a real reduction rather than a tidy-up.

## HYGP-022 - A hand-rolled YAML editor exists for one agent's one config key

**Decision:** Hermes support is removed entirely, and the YAML editor with it.

From `shepr-agent`: `config_edit.rs` carries about twenty `yaml_*` helpers
(indent parsing, inline comments, flow sequences, scalar quoting, list-item
matching at indent) plus `hermes_yaml_layout_is_editable` and a "give up and tell
the user to edit it by hand" error, all to toggle one key in Hermes's
`config.yaml`. The hunter calls this the largest per-call-site policy in the
crate. The `shepr-agent` dependency allowlist has no YAML crate; adding one, or
shipping the Hermes enablement differently (a drop-in file if Hermes supports
one), would delete several hundred lines and the class of bug that comes with
hand-parsing an indentation-sensitive format.

## HYGP-023 - `input_wire` is one conversion layer written twice, with the shared half duplicated verbatim and opposite policies for unrecognised bits

From `shepr-server`: `server/input_wire.rs` and
`shepr-client/src/input_wire.rs` have the same file name, the same trait names
(`WireKeyKind`, `WireKeyCode`, `WireMouseButton`, `WireMouseKind`,
`WirePaneInput`) and mirror-image directions. Two concrete problems:

- `text_bytes` is byte-identical in both copies, doc comment included, and it is
  the function that computes the `MAX_INPUT_PAYLOAD` budget. The client uses it
  to decide what to batch; the server uses it to decide what to reject. If the
  copies drift the client sends batches the server refuses, or under-fills and
  loses throughput. Two writers of one accounting rule who have never been
  introduced.
- Opposite policies for unrecognised bits: the client does
  `WireModifiers::from_bits_retain(modifiers.bits())`, the server does
  `KeyModifiers::from_bits_truncate(modifiers.bits())`. One preserves unknown
  bits, the other drops them silently. Equivalent today only because both
  bitflag sets cover the same bits. Meanwhile `render.rs` uses
  `KittyKeyboardFlags::from_bits_retain` for a value going the other way, so the
  crate holds both policies for wire bitflags.

Neither crate depends on the other, but both depend on `shepr-protocol`, which
owns `ClientPaneInputEvent`, `WireModifiers` and `MAX_INPUT_PAYLOAD`. The hunter
calls this one module in the wrong crate, twice, with no forced duplication, and
names it the second-largest structural move in that scope: move both directions
into `shepr-protocol` (or `shepr-termio`, also a dependency of both) as inherent
impls on the wire types, with `text_bytes` a method on `ClientPaneInputEvent`
next to the constant it charges against.

## HYGP-024 - "An unknown reported cell size means the default" is implemented at five sites in the hottest function in the server

From `shepr-server`: `server/headless/render.rs` (three sites, all inside
`render_and_stream`) and `client_views.rs` spell
`if cell_size.is_known() { cell_size } else { HostCellSize::default() }`.
Enforcement named: `HostCellSize::or_default(self) -> Self` in `shepr-termio`,
after which the branch is unspellable at call sites.

## HYGP-025 - Three sanitizers decide what may appear in the same tab bar row, with three different rules

From `shepr-server` `app/tab_bar_status.rs`: `sanitize_separator` strips control
characters only; `sanitize_literal_text` strips control characters only;
`sanitize_status_text` trims, strips control characters, strips unicode format
controls and caps at 80 characters. All three feed `AppState::tab_bar_right`,
rendered into one row - so a configured `text` entry can carry a bidi override
(U+202A to U+202E) that an identical string from a `command` entry cannot, and
the separator is uncapped.

Enforcement named: one `TabBarText` newtype whose only constructor sanitizes, so
`tab_bar_right` and `tab_bar_right_separator` cannot hold anything else.

## HYGP-026 - A three-entry key-alias table lives away from the key parser

From `shepr-server` `app/api_helpers.rs::normalize_api_key_alias`: maps
`"C-c" | "c-c" => "ctrl+c"` and `"+" => "plus"`. Key-name parsing otherwise
belongs entirely to `shepr-config::parse_key_combo`, so a fourth alias will be
added here rather than there and the two will drift. Enforcement named: move the
aliases into `shepr-config` next to the parser.

## HYGP-027 - "Only send if enough time has passed since `last_sent_at`" is reimplemented three times in the client mouse layer

From `shepr-client` `shell/input/mouse.rs`: the scrollbar drag (33 ms), the split
drag (33 ms) and the selection repaint (`SELECTION_REPAINT_INTERVAL`) each carry
their own `last_sent_at` field, their own `is_none_or` comparison, and their own
re-borrow of `self.chrome_drag` to write the timestamp back. Reconnect backoff in
`supervisor.rs`, by contrast, is properly single-owned.

Enforcement named: a small `Throttle { interval, last: Option<Instant> }` with
`fn admit(&mut self, now) -> bool`. Three call sites collapse to three fields of
one type, the interval becomes a named construction argument, and the re-borrow
dance disappears.

## HYGP-028 - The host terminal's mode state and its restore intent are two pieces of shared state kept consistent by convention at three setters

From `shepr-termio`: `HostModes` guards `HostModesState` behind a `Mutex` and
`restore_state` behind an `AtomicU8`, with a comment explaining the split - the
panic hook must restore without taking a lock that may be held by the panicking
thread. The hunter calls that deliberate and sound, and worth keeping. The
consequence is that restore intent and mode state stay consistent only by each
setter remembering to call a recorder before and after its write, and
`set_keyboard_enhancement_flags`, `set_direct_keyboard_protocol` and
`set_modify_other_keys` each do the pairing slightly differently (the first
records `false` for modify-other-keys on success unconditionally; the other two
record the computed value). Whether the three agree is not checkable from the
types. The `restore_state` bitfield is itself maintained by four recorder methods
(`record_keyboard_restore_state`, `record_keyboard_entry`, `record_restore_flag`,
and `apply_mouse`'s conditional `record_restore_flag(RESTORE_MOUSE_CAPTURE)`
which only records when `reassert` is true).

Not mechanically enforceable. The hunter's honest statement: the recorder pairing
is a convention maintained by three call sites, and a single `set_keyboard(..)`
entry point that computes the flags itself would reduce it to one.

## HYGP-029 - Two sibling modules build the same wire message with the same guard, separately

From `shepr-protocol`: `surface_reuse::message` and `surface_delta::message` both
check `Baseline::accepts` with the same five arguments and then build the same
six-field `SurfaceUpdate`, differing only in whether `spans` is empty.
Enforcement named: one constructor taking the spans - a type-level fix.

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
- `shepr-server`: `app/tab_bar_status.rs` logs the full status command line at
  `warn` on every failure, at every interval (default as low as 1 s) for the
  server's whole life - unbounded log growth on a permanent condition, and a
  `tab_bar_right` entry that curls an endpoint with a bearer token puts that
  token in the log at warn level. `render.rs` puts a terminal id into a
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
- `shepr-remote` again, and adjacent to this entry:
  `confirm_remote_server_stop` prints a remote-controlled version string to the
  terminal with no control-character filtering while the error path filters it
  through a `printable` closure (see HYGP-046) - a terminal-injection hole
  through the one unfiltered site.

## HYGP-031 - Test-only code is compiled into production libraries through Cargo feature unification (`test-api`, `test-support`)

Reported from six scopes; several hunters marked the unification mechanics as an
inference they had not verified by building. Gathered here as one entry.

The mechanism: the root package (and `shepr-server`) depend on library crates
both normally and, as dev-dependencies, with `features = ["test-api"]` /
`["test-support"]`. Cargo unifies features across a single build graph, so any
build that includes dev-dependencies - `cargo test`, `cargo clippy
--all-targets`, i.e. what `brokkr check` runs - compiles those libraries once
with the test feature on, for every consumer in that invocation, including the
production binary. Two hunters also draw the reverse conclusion: the feature set
that `brokkr install` ships (`test-api` off) is compiled by no gate step, so a
`#[cfg(not(feature = "test-api"))]` path or accidental dependence on a test-only
item would not be caught.

What becomes reachable:

- `shepr-platform`: `process.rs::signal_processes`, documented as "Test-only:
  production code signals through `ProcessHandle`, which cannot hit a reused
  pid", gated `#[cfg(any(test, feature = "test-support"))]` with the feature
  enabled by `shepr-server`'s dev-dependency. Its single user is one line in
  `shepr-server/src/app/snapshot_tests.rs`. Suggested: move the nine lines of
  `libc::kill` into the test that needs it and delete the `test-support` feature
  from `shepr-platform` entirely, making the shortcut unrepresentable rather than
  gated (see also HYGP-037).
- `shepr-mux`: `test-api` is not cosmetic - it adds a whole variant to a
  production enum (`PaneRuntimeIo::TestChannel`, whose arms contain real
  behaviour including a thread that sleeps and sends, with six `#[cfg]` match
  arms across `shutdown`, `owns_child_process`, `resize`, `try_send_bytes`,
  `write_terminal_response`, `queue_user_input_submission`), plus
  `Workspace::clear_tabs_for_test`, `PaneRuntimeRegistry::drain`,
  `TerminalState::set_detected_state`, nine `PaneRuntime::test_*` constructors,
  `GitStatusRefreshDemand::ALL` and `Workspace::assert_invariants_for_test`.
  Suggested: `PaneRuntimeIo` is a four-method interface - making it a trait
  object or generic puts the test double in the test module and deletes all six
  `#[cfg]` arms.
- `shepr-pty`: the same `TestChannel` double reimplements the actor - submissions
  as a thread with `sleep`, `try_send`, no ordering,
  `SubmissionCancel::untracked()`, resize replies discarded. Also
  `SubmissionCancel::untracked()` / `never_started()` and
  `backend::open_pty` / `spawn_in_pty` are public solely for tests in other
  crates.
- `shepr-agent`: `resume.rs::test_codex_plan` is gated
  `any(test, feature = "test-support")`, and both the root `Cargo.toml` and
  `shepr-server`'s dev-dependencies enable `shepr-agent/test-support`. It only
  panics on its own bad input, so the risk is low, but it is a test-only shortcut
  production code can reach. (`opencode_config.rs`'s and `manifest.rs`'s
  `#[cfg(test)]` wrappers are fine as test seams.)
- `shepr-protocol`: `TerminalId::test_new` is `pub` and ungated, and
  `WorkspaceId::new`, `BootId::from(&str)` and `RequestId::from(&str)` let any
  caller mint an identity that is supposed to come from one place.
  `error.rs` exports `TestResponseJson`, `TestReply`, `test_json`,
  `test_success`, `test_error` behind `cfg(any(test, feature = "test-support"))`
  (`TestResponseJson` as a named bound appears only in its own file), which the
  `shepr-api` hunter calls more surface than the use justifies and
  production-reachable whenever the feature is on. `Config` being exported
  publicly only under `feature = "test-support"` is named as the right pattern
  the crate already knows.
- `shepr-remote`: `bridge_upload_cancellation_for_test` is `pub` under
  `cfg(any(test, feature = "test-support"))` and `expect()`s four times and
  `assert!`s once - a panicking API exported from a library that otherwise bans
  `unwrap`, if anything ever enables the feature in a non-test build.
  (`RemoteSsh::test_with_state` is `#[cfg(test)]` only, which the hunter calls
  correct.)
- `shepr-client`: `test-support` is enabled in the same build as production code,
  so `ClientState::test_new()`, the activation test hooks in `activation.rs` and
  the shell hooks in `endpoints.rs` are reachable from `shepr-client`'s own
  production modules in that build; nothing prevents a production path from
  calling them, only the fact that none does today. The hunter's note: real
  isolation means moving the helpers into a separate crate (the
  `shepr-test-support` pattern the workspace already uses), because keeping the
  feature and forbidding production callers is not mechanically checkable.
- `shepr-server`: `#![cfg_attr(feature = "test-api", allow(dead_code))]` at the
  crate root means the build that would run `dead_code` is exactly the build that
  silences it across roughly 45 000 lines. The `shepr-server` hunter flags this
  as the finding that hides other findings, and says their question-8 list is
  hand-verified by grep as a result (see HYGP-051). It is the only crate-wide
  `allow(dead_code)` in the repo; `shepr-vt` and `shepr-mux` use narrow per-item
  allows with justifying comments, which is the house style.

Enforcement named across hunters: add a `[[check]]` entry to `brokkr.toml` that
builds the workspace with default features and without `--all-targets`, so the
shipped feature set is compiled by the gate; forbid the `test-support` feature
appearing in `[dependencies]` as opposed to `[dev-dependencies]`
(`shepr-server`'s allowlist already lists `shepr-test-support` as a normal
dependency, which is the case worth checking); and prefer moving helpers into a
separate crate or a trait seam over gating them.

## HYGP-032 - `#[cfg(test)]` branches inside production functions change what production runs

- `shepr-server` `app/agents.rs`: `available_shell_name` returns `Some("sh")` and
  `runtime_hosts_agent` returns `true` when `runtime.child_pid().is_none()`,
  under `#[cfg(test)]`. Any test using a `PaneRuntime` without a live child -
  which is most of them, including every `PaneRuntime::test_with_screen_bytes`
  fixture - therefore gets `runtime_hosts_agent == true` for every agent, so
  assertions that a pane hosts the expected agent cannot fail. The shortcut is
  gated on `cfg(test)` only while the rest of the crate gates test affordances on
  `any(test, feature = "test-api")`, so the root binary's integration tests see
  the production path and unit tests see the shortcut: the two suites test
  different code. Suggested: inject the probe (a `ProcessProbe` trait or an
  `Option<fn>` on the runtime) so a test that wants "hosts the agent" must say
  so.
- `shepr-client` `state.rs`: `try_present_frame` selects its sink by `cfg` -
  `io::stdout()` in production, `io::sink()` under test - with a comment
  explaining that a full-screen frame written to the test runner's real stdout
  would scribble on the developer's terminal. The presentation test surface
  (`shell/tests/copy.rs` 2492 lines, `mouse_selection.rs` 1312,
  `endpoints.rs` 1934) therefore never asserts anything about what the production
  sink receives, and the sibling `present_surface_patch` has no `cfg` at all (the
  hunter filed that divergence as a live defect). Suggested: an injected writer
  on `ClientState`, which removes the `cfg` divergence and lets tests assert on a
  `Vec<u8>` while running the same code production runs.
- `shepr-remote`: `UPLOAD_READ_ATTEMPTS` is a `thread_local!` counter checked
  inside `copy_local_stream_to_writer`'s hot loop under `#[cfg(test)]`. Correctly
  gated, but it means the hot path under test is not the hot path that ships.

## HYGP-033 - Test-only helpers that cannot report what their production siblings report

- `shepr-api` `subscriptions.rs`: `ActiveSubscription::poll` and
  `ActiveAgentStatusChangedSubscription::poll` are `#[cfg(test)]`-only and
  implemented as `self.poll_for_wait(..).ok().flatten()`, while the module's own
  doc comment stresses that errors are final and must not be silently dropped
  ("reports that instead of going silent"). Tests written against the shims
  cannot observe a `pane_not_found` or `events_lost` at all; two of the module's
  tests were clearly written to compensate. Suggested: delete the shims and have
  tests call `poll_for_wait` and `.expect(..)`.
- `shepr-api` `event_hub.rs`: the test-support `events_after` returns
  `Vec::new()` on a poisoned lock and has no `Lost` signal, while
  `events_after_checked` distinguishes both. Eleven call sites in `shepr-server`
  tests use it, and an assertion that a history is empty cannot distinguish "no
  events were emitted" from "the lock is poisoned". Suggested: delete
  `events_after`.
- `shepr-agent` `env.rs`: `GROK_CONFIG_DIR` exists only as a test seam and says
  so in a comment ("a shepr-level override only (primarily a test seam); the grok
  CLI does not honor it") - a production environment variable whose only purpose
  is testing, which the injected-environment fix in HYGP-002 would remove.
- `shepr-agent` `test_support.rs::symlink_file` returns `true`
  unconditionally after `expect("create symlink")`, so call sites doing
  `assert!(symlink_file(..))` assert nothing.
- `shepr-client` `should_enable_host_color_scheme_reports(enable_client_protocols:
  bool) -> bool` returns its argument and is `#[cfg(test)]`-imported alongside
  real helpers; any test asserting on it asserts `x == x`. The hunter's note: the
  function exists so a rule could live there and today holds no rule - delete it
  and inline the boolean, or give it the rule it was created to hold.
  (Also listed as dead code in HYGP-041.)

## HYGP-034 - Snapshot-preservation policy is implemented three times in one file

From `shepr-mux` `src/persist/writer.rs`: `preserve_snapshot_history(path)` and
`prepare_snapshot_history(path)` both list `recovery_files`, tolerate `NotFound`,
read the newest copy's mtime, compare its age against `SNAPSHOT_INTERVAL`, read
and parse `session.json`, check `version != SNAPSHOT_VERSION ||
workspaces.is_empty()`, compute `layout_fingerprint`, compare it against the
newest copy's, and call `preserve_existing_in(path, "session-snapshots",
SNAPSHOT_LIMIT)`. They differ only in what they return and whether the
fingerprint is handed back. The second one's outcome is then re-checked a third
time in `finish_snapshot_history`, which repeats the version and emptiness checks
and the fingerprint comparison inline. One policy, three partial
implementations, each with its own error handling and its own `tracing::warn!`
wording.

Enforcement: collapse to one function returning a decision value the caller acts
on. Holdable only by a reviewer, but the duplication is large enough to be
obvious once named.

---

## HYGP-035 - The Ghostty naming layer, and the `PaneTerminal` hop that only renames

Reported by two hunters (`shepr-vt`/`shepr-pty` and `shepr-mux`) as one thing.
The terminal core is `alacritty_terminal`, pinned with `=`, with a
`brokkr.toml` dependency rule (`alacritty-terminal-only-in-shepr-vt`) making the
boundary structural. The pane terminal layer is nonetheless named after Ghostty
throughout: `GhosttyPaneTerminal`, `GhosttyPaneCore`, `PaneTerminal { ghostty }`
and roughly forty to fifty `ghostty_*` free functions
(`ghostty_visible_text`, `ghostty_cell_style`, `ghostty_collect_dirty_patch`,
`ghostty_default_bg`, ...). Counted matches: `src/pane/terminal.rs` 65,
`src/pane/terminal/helpers.rs` 76, `src/pane/terminal/backend.rs` 27,
`src/pane/runtime.rs` / `pane.rs` / `pane/osc.rs` / `migration_tests.rs` 19,
`src/pane/terminal/tests.rs` 187, `shepr-vt/src/lib.rs` and
`shepr-termio/src/input/*` 12.

Dead as a concept: only one backend exists. Three of the references are not
naming but claims about a dependency that is not present, which makes the code
they justify unfalsifiable by reading:

- `src/pane/terminal/backend.rs`: "a workaround for the libghostty core losing
  rows on resize" - the stated reason for a workaround in live code.
- `shepr-vt/src/lib.rs`: `DEFAULT_FOREGROUND` "match what the libghostty-vt
  render state reported".
- `backend.rs`: `error!(pane = .., "ghostty core lock poisoned in reader")`, an
  operator-facing log line naming a component that does not exist (and the actor
  logs the same event again as "terminal core is broken ... closing the pane").

The wrapper itself: `PaneTerminal` is a one-field newtype over
`GhosttyPaneTerminal` whose whole body is one-line forwards, with no state, no
invariant and no method that does anything but forward - its only non-trivial
member, `core_poisoned`, forwards to a `shepr-vt` free function. `PaneRuntime`
forwards again, so a read from the API traverses `PaneRuntime` ->
`PaneTerminal` -> `GhosttyPaneTerminal` -> `shepr_vt::Terminal`, two of the three
hops adding nothing but a name. Roughly forty of `PaneRuntime`'s ninety public
methods are those one-line delegations (`visible_text`, `visible_ansi`,
`detection_text`, `terminal_title`, `agent_osc_title`, `agent_osc_progress`,
`bracketed_paste_enabled`, `focus_reporting_enabled`, `mouse_reporting_enabled`,
`sgr_pixel_mouse_enabled`, `alternate_screen_active`,
`synchronized_output_active`, four `recent_*_snapshot`, three `encode_mouse_*`,
four `scroll_*`, `search_text_window`, `word_motion_target`,
`paragraph_motion_target`, ...).

Both hunters recommend collapsing `PaneTerminal` and `GhosttyPaneTerminal` into
one type under a non-Ghostty name, which is also the natural owner for HYGP-020's
rules. Enforcement named: a `brokkr.toml` text rule forbidding
`ghostty`/`Ghostty` (the same mechanism the gremlin scan uses), either outright
after the rename or outside a comment explaining a historical decision. The two
workaround comments need a human to decide whether the workaround is still needed
against the real emulator - a question that cannot be answered by reading and
needs the pinned `alacritty_terminal` source in the cargo registry.

Adjacent, from the `shepr-mux` hunter: thinning `PaneRuntime`'s forty
pass-throughs is not lintable, but `pane/runtime.rs` at 2960 lines with ninety
public methods is the crate's god object, and AGENTS.md's "No god objects"
principle names only `shepr-server/src/app/` so does not reach it. If the
principle is meant generally, the enforcement is a per-file or per-impl size rule
in `brokkr.toml`, which would also catch `terminal/metadata.rs` (1438 lines) and
`persist/restore.rs` (2365).

## HYGP-036 - `migration_tests.rs` and `SHEPR_MIGRATION_OBSERVATIONS`: scaffolding for a finished migration

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
semantic dimensions across four geometries. The droid pid gate in
`primary_screen_replay_honors_ed3_for_droid_at_chunk_boundaries` is the same
shape: it spawns host `bash` and polls `/proc` for a process named "droid" to
pass a real pid to `process_pty_bytes`, which ignores `_shell_pid`.

What is not dead, per the `shepr-mux` hunter: the file's other six tests
(`incremental_rows_reconstruct_full_render`,
`sparse_dirty_patches_preserve_coordinates_and_clipped_rows`,
`dirty_patch_fallback_keeps_previously_collected_rows_dirty`,
`complete_history_replay_supports_plain_append`, and the read-purity checks)
assert real invariants and should stay, under a name saying what they check
rather than what they were once migrated from.

Related stale prose describing the same finished migration, which the hunter
notes is a deletion and not lintable: `src/terminal/state/mod.rs` ("During the
migration this is still one-to-one with a pane-backed PTY, but pane/view state no
longer owns terminal identity, cwd, labels, or agent metadata"), the same
module's header, and `src/pane/state.rs`. `PaneState` now holds two fields
(`attached_terminal_id`, `right_click_passthrough`), so the transition described
is over and the comments were already false when `PaneState` shrank.

Enforcement named: deletion of the tautological test and the env var, a rename of
the module, and a text rule against `SHEPR_MIGRATION_OBSERVATIONS`.

## HYGP-037 - `shepr-platform`'s `test-support` feature gates one nine-line function with one caller

Delete the feature, the function (`process.rs::signal_processes`) and the
`features = ["test-support"]` entry in `shepr-server/Cargo.toml`. Full context in
HYGP-031; recorded separately because the deletion is self-contained.

## HYGP-038 - `ProcessIdentity::StartTime`: a compatibility path for a kernel nobody runs

From `shepr-platform`. What tells the hunter it is dead: `ProcessHandle::open`
uses it only when `pidfd_open` fails with something other than `ESRCH`/`EINVAL`,
i.e. `ENOSYS` (kernel older than 5.3, September 2019) or fd exhaustion;
`ProcessHandle::open_by_start_time` is `pub(super)` with no non-test caller; the
two tests that exercise the branch assert `pidfd.pidfd().is_some(), "this kernel
has pidfds"` in the same breath, and `both_handle_kinds` / `openers` exist only
to double every process test; shepr is Linux-only, `rust-version = "1.99"`, and
has never been deployed.

What it costs: roughly a third of `process.rs` - a second identity enum arm,
`state_and_start_time_from_stat`, `state_by_start_time`, a `/proc` read before
every signal, a `has_exited` implementation with different semantics, the
`START_TIME_EXIT_RECHECK` tunable and its poll-floor logic in
`wait_for_process_exits` - plus a documented pid-reuse race the pidfd path does
not have (a race no test reaches and no assertion guards), plus twice the test
matrix in two tests.

Recommendation: delete it. `ProcessHandle::open` returns `None` (or an error
naming fd exhaustion) when `pidfd_open` fails; fd exhaustion is a "refuse and say
so" case, not a "silently degrade to a racy identity" case. Cost of being wrong:
shepr stops managing pane process groups on pre-5.3 kernels, which the owner does
not run. After deletion the `#[cfg]`-free code has one identity and
`wait_for_process_exits` loses its "handles without a pidfd" branch entirely.

## HYGP-039 - Version suffixes and negotiation for a deployment that does not exist

**Decision:** delete the `--idle-timeout-v1` flag, its plumbing,
`SHEPR_BRIDGE_TEST_LEGACY` and `legacy_bridge_has_no_idle_deadline`; drop the
`:1` and `-v1` suffixes from the two markers. Note found while checking: the
interactive `--remote` path (`launch.rs::run_remote`) passes `false` today, so
after deletion the interactive client bridge also gets the idle watchdog. The
client heartbeat keeps a live bridge busy, so this should be harmless, but it
is a behaviour change to verify.

Reported by the `shepr-platform` and `shepr-remote` hunters. AGENTS.md: "no wire
compatibility obligations", "client and server are always the same build".

- `--idle-timeout-v1`: every production spawner passes it
  (`shepr-remote/src/remote/attach.rs`, `::launch.rs`); `src/cli/spec.rs`
  defines it, `src/cli.rs` parses it, `src/main.rs` forwards it. The `false` path
  is reachable only by invoking the hidden `remote-client-bridge` subcommand by
  hand, and its only exerciser is `legacy_bridge_has_no_idle_deadline` plus the
  `SHEPR_BRIDGE_TEST_LEGACY` variable that exists solely to reach it. To be
  precise, per the hunter: the `idle_timeout: bool` *parameter* of
  `forward_remote_bridge_stdio` is genuinely two-valued -
  `shepr-remote/src/lib.rs`'s API bridge passes `false` legitimately. It is the
  CLI flag, the legacy test path and the test variable that are dead. The flag is
  also spelled in three files (`launch.rs`, `src/cli.rs`, `src/cli/spec.rs`).
  Recommendation: delete the flag, the parse, the plumbing, the test variable and
  `legacy_bridge_has_no_idle_deadline`; the client bridge always gets the
  watchdog. After deletion the flag cannot be passed if `spec.rs` does not define
  it.
- `REMOTE_OUTPUT_READY_MARKER = "shepr-remote-output-ready:1"` and
  `STALE_API_METADATA = "shepr-machine-metadata-stale-v1"`: the `:1` and `-v1`
  imply versioning that nothing reads - there is no version negotiation. The
  `shepr-remote` hunter's caveat: these are the one thing that could legitimately
  differ if a stale remote binary is somehow reached, but `build_id` checking
  already covers that before the marker is read. Either drop the suffixes or
  state in one place why they exist. Not enforceable; a decision to record.

## HYGP-040 - The world-readable-log tightening path is for a build nobody ran

From `shepr-platform` `logging.rs`: a branch re-chmods an existing log "left
behind by an older build that created logs world-readable", with a dedicated
assertion in `log_files_are_private_to_the_user`. AGENTS.md: "shepr has never
been run: no config, catalog, session or other on-disk state exists anywhere."
There is no older build and no world-readable log anywhere, and the
`mode(0o600)` on the `OpenOptions` covers every file this code creates.
Recommendation: delete the tightening branch and the legacy half of the test.
Cost of being wrong: nil - `mode` still applies to created files, and the owner
can `chmod` once if a stray file ever appears.

## HYGP-041 - One-line pass-through wrappers, aliases and identity functions

Each of these is a second name or a second hop for one thing; the evidence given
for each is the hunter's.

- `shepr-core` `layout.rs`: the free function `valid_split_ratio(f32) ->
  SplitRatio` whose entire body is `SplitRatio::clamped(ratio)`. Both are used
  externally (`clamped` from 36 sites, `valid_split_ratio` from
  `shepr-mux/src/persist/restore.rs` and three internal layout sites), so one
  policy has two doors. Delete `valid_split_ratio`; the compiler enforces the
  rest. (Filed by that hunter under one-value-one-owner.)
- `shepr-platform` `logging.rs::help_log_paths_summary(dir) -> String` is
  `log_paths_summary(dir)`. One external caller (`src/cli.rs`); the private
  `log_paths_summary` exists only so the test can call it under a different name.
  Two names, one body, one caller. Make one of them public.
- `shepr-remote` `bridge.rs::fits_unix_socket_path` is a private shim over
  `shepr_platform::fits_unix_socket_path`, a public function from a crate
  `shepr-remote` already depends on directly, used by `attach.rs` at three sites.
  It makes the socket limit look like it has two owners. Delete it.
- `shepr-remote` discovery: `locate_remote_shepr`, `prepare_remote_shepr` (wraps
  it in a one-field `PreparedRemoteShepr`) and `find_installed_remote_shepr`
  (body identical to `locate_remote_shepr`) are the same call. Evidence: the
  struct has one field, one constructor and two readers that both immediately
  project the field; the third function's body is a copy. Because of this,
  `discovery_tests.rs` exercises `DiscoveryProgress` directly and nothing tests
  that the three entry points agree - they agree by being copies. Keep
  `locate_remote_shepr`; delete the other two names and the struct.
- `shepr-remote` `server_lifecycle.rs::version_label(Option<&str>) -> &str` is
  `version.unwrap_or("unknown")`, with both callers inside
  `confirm_remote_server_stop`. Meanwhile `remote_server_compatibility_error` and
  `remote_compatibility_error` each define their own `printable` closure doing
  `unwrap_or("unknown")` plus an ASCII-graphic filter - two policies for
  rendering an untrusted version string, and the stricter one is not the one used
  in the interactive prompt (see HYGP-030). Fix: one
  `printable_remote_value` everywhere; delete `version_label`.
- `shepr-api` `session.rs`: `data_dir_for`, `client_socket_path_for` and
  `api_socket_path_for` are `pub` one-line forwarders to `SessionId` methods
  (one, one and two callers). None is dead; all are redundant indirection that
  makes `shepr-api::session` look like the owner of path layout when
  `shepr-config::SessionId` is.
- `shepr-api` `restart_after_update_guidance` is `pub` with exactly one caller,
  `restart_after_update_guidance_for` in the same file.
- Root binary: `src/cli/runtime.rs::print_method_response` and
  `src/cli/pane.rs::print_request` are the same function with different names
  (both `(&CliContext, &'static str, Method) -> CliResult<i32>`, both
  `print_response(send_request(..))`), and `src/cli.rs::send_ok_request` is the
  same again with the id fixed and the success body dropped. Three spellings of
  one policy.
- `shepr-pty`: `fail_active_submission` and `read_once` are single-line aliases.
- `shepr-agent`: `AgentSource::to_source_string`, `as_str` and `Display` are
  three ways to spell one projection (`to_source_string` is
  `as_str().to_owned()`), plus `PartialEq<&str>` for both `AgentSource` and
  `Agent`. Fine to keep, cheap to collapse. `integration/command.rs` is fifteen
  lines holding two functions, one of which (`shell_single_quote`) is imported
  separately by `targets.rs` to build the Grok command that bypasses the other;
  merging it into the module that owns hook command construction removes a file.
- `shepr-client`: `should_enable_host_color_scheme_reports` returns its argument
  (also HYGP-033); `frame_output::write_composed_frame` is
  `writer.write_all(encoded)` with one caller, and its companion `ComposedFrame`
  is a newtype over `FrameData` with a `From` and a `Deref` whose own doc says
  "Shepr no longer forwards pane images to the outer terminal, so this is a thin
  wrapper around the plain text frame" - the residue of a removed feature, read
  by everyone after as a composition boundary. Evidence: the doc names the
  removed reason for its existence, the type adds no field and no invariant, the
  function adds no behaviour over `write_all`, and the sibling patch path
  bypasses both.

## HYGP-042 - Flags, parameters and constants that have had one value since they were added

- `shepr-client`/`shepr-termio` `blit.rs`: `REPEAT_IME_ANCHOR_AFTER_SYNC: bool =
  true`, whose own doc says "Production always repeats the IME anchor after the
  synchronized block; the parameter threaded through the blit functions exists so
  tests can check both output shapes." So the constant is `true` at all three
  production call sites and a `bool` parameter is threaded through the blit call
  chain to let tests exercise a shape production never produces - the tests
  asserting the `false` shape assert nothing about shipped behaviour. Evidence:
  the constant has one value, is never computed, and its comment states the
  parameter exists only for tests. Deletion also removes a branch from the hot
  blit path.
- `shepr-vt`/`shepr-mux`: `hide_kitty_placeholders = true` (twice) and the
  parameter it feeds; `PtyIoActorConfig.on_reader_exit` and `core_broken` are
  `Option` while production always passes `Some`.
- `shepr-remote`: `read_remote_confirmation(reader, default)` is called once,
  with `false`; the `default` parameter has had one value since it was added, and
  the `[y/N]` prompt text it should agree with is printed separately by
  `confirm_remote_server_stop`. (`CandidateVerification` is a genuine two-valued
  enum - both arms reachable - which the hunter records as the good case.)
- `shepr-protocol`: `read_message`'s `max_frame_size` parameter is passed
  `shepr_protocol::MAX_FRAME_SIZE` at roughly fifteen production call sites
  across `shepr-client` and `shepr-server`; only
  `wire_tests::oversized_input_rejected_custom_max` passes anything else. A
  parameter nobody varies is dead weight and a hazard - a call site can weaken
  the cap and nothing notices. Suggested: drop it from the public function and
  keep a `cfg(any(test, feature = "test-support"))` variant for the one test.
- `shepr-config`: `ConfigSource::CliFlag(String)`'s only construction is
  `ConfigSource::CliFlag("--session".to_owned())` in `io.rs::resolve_with_session`.
  Nothing mechanical; either a unit variant documented as the session flag, or
  accept the generality.
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
- `shepr-server`: `AppPolicy::PRODUCTION` and `AppPolicy::TEST` are associated
  consts that are literally `Self::Production` and `Self::Test`, with no const for
  the third variant `Suspended` - so `server/headless/lifecycle.rs` writes both
  conventions in one expression. Repo-wide: roughly 60 `AppPolicy::TEST` sites
  and 2 `AppPolicy::Test` sites. Dead abstractions rather than dead code, but
  counted as a site by every finding that touches policy. Deleting the consts
  makes the second spelling unrepresentable.

## HYGP-043 - One-variant enums and an `Option` field that cannot be `None`

- `shepr-api` `client.rs`: `enum ConnectionTarget { SocketPath(PathBuf) }` with a
  `socket_path()` method that matches one arm, wrapped by `ApiClient` whose
  `socket_path` forwards again. Evidence: no second variant anywhere in the
  workspace and no test constructs one; all three consumers construct
  `SocketPath`. Fix: `ApiClient { socket_path: PathBuf }`.
- `shepr-client` `shell/state.rs`: `enum ClientInputTarget { Pane(PublicPaneId) }`
  with nine construct-or-match sites, so every `match target { .. }` in
  `shell/input/events.rs` is a rename of a field. It reads as a policy point
  ("where does input go?") and holds no policy. Fix: replace with
  `PublicPaneId`. If it is anticipating a second target, nothing says so.
- `shepr-platform` `ssh_paths.rs`: `system_config: Some(PathBuf::from(
  "/etc/ssh/ssh_config"))` - the `Option` shape claims a case the code cannot
  produce, and the sole consumer (`shepr-remote/src/remote/ssh.rs`) must handle
  it anyway. Make the field a `PathBuf` and the type system does the rest.
- `shepr-agent`: `session_ref_policy: Option<SessionRefPolicy>` encodes two facts
  as three states - `None` means "no resume", and every agent with
  `resume_args: None` also has `session_ref_policy: None`; the two fields are
  never independently set. One `Option<ResumeSupport>` carrying both would make
  `session_ref_from_report`'s `_ => None` arm unnecessary. Adjacent, recorded as
  a question rather than a finding: `AgentState::Unknown` versus the "Idle"
  presentation - AGENTS.md says "Unknown presents as Idle" and `attention_rank`
  gives them the same rank, so four states exist where three are presentable,
  with the distinction carried by convention across several crates. Not dead, but
  worth asking whether `Option<AgentState>` with three variants would say it
  better.

## HYGP-044 - Parameters that are threaded and then discarded

- `shepr-server` `app/api/responses.rs`: `success(_id: String, result:
  ResponseResult)` and `failure(_id: String, ..)` both ignore `_id`, at roughly
  220 call sites across `app/api/*` (geometry 61, workspaces 33, panes 28,
  layouts 26, agents 21, tabs 19, reports 17, copy 14, plus tests). Every site
  threads an id, usually a `String` cloned or moved specifically to be dropped,
  and reads to a newcomer as though response correlation happens here - it does
  not; correlation is `shepr_api::error::encode_result(id, result)` at the
  dispatcher. Evidence: the parameters are `_`-prefixed and unread in
  one-expression bodies, `ApiResult` has no id field, and the dispatcher supplies
  the id separately. The hunter calls this the single largest volume of dead
  plumbing in that scope; deleting the parameter makes the compiler find all 220
  sites and the dead clones with them.
- `shepr-vt`/`shepr-mux`: `_shell_pid` in `process_pty_bytes`, and `_pane_id` /
  `_shell_pid` in `flush_expired_synchronized_output` - threaded from the runtime
  and ignored. One test's whole setup exists to supply a real pid to the first of
  these (HYGP-036).
- `shepr-client` `set_handshake_recv_timeout(stream, timeout, _context: &'static
  str)`: the parameter is unused, so the one caller's string
  ("failed to clear client handshake read timeout") is dead and the resulting
  `ClientError::ConnectionFailed` carries the bare socket error with no
  indication where it came from. The intent to attach context is visible in the
  source and does nothing. Either use it or drop it; clippy's unused-variable
  lint is silenced only by the leading underscore.

## HYGP-045 - Branches and checks that cannot run

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
- `shepr-mux`: the kitty placeholder filter is spelled five times and can never
  fire - `shepr-vt`'s `cell_text` already classifies U+10EEEE as `Empty`, so the
  checks in `helpers.rs` (`ghostty_cell_symbol`,
  `ghostty_buffer_symbol_into`), `pane/terminal/text.rs` and
  `terminal/history_read.rs` are unreachable. Suggested: stop making
  `KITTY_UNICODE_PLACEHOLDER` public.
- Decided: delete. `shepr-mux` `workspace.rs::unregister_moved_pane(&mut self, _pane_id)` has a
  body of one `debug_assert!`, and is called from production
  (`shepr-server/src/app/api/panes/geometry.rs`). In a release build
  (`brokkr install`) the assert compiles away and this is a `&mut self` method
  that does nothing, so the shipped binary takes a mutable borrow of the
  workspace to acknowledge something. The hunter reads it two ways and says so:
  either it is an invariant check that should be a real `assert!` or return
  `Result`, or it is dead. Both answers are enforceable; the code does not say
  which is intended.
- Root binary: ten `Invalid` variants (`ConfigCommand::Invalid`,
  `TerminalCommand::Invalid`, and eight more) are unreachable by construction per
  their own comments - each group's spec sets `.subcommand_required(true)`, so
  `missing_subcommand()` is documented as running "only if a handler and the spec
  disagree". Ten variants, ten `""` names, ten dispatch arms and one
  `missing_subcommand` exist to model a state the parser prevents. (The `""`
  names collide with `Command::Overview`'s, which produces the malformed refusal
  message the sibling documents cover.) Fix: have each `parse` return
  `Option<Command>` / `Result` and let `CliCommand::from_matches` propagate; the
  `Invalid` variants and the `""` names disappear together. The hunter calls this
  the single largest mechanical simplification available in `src/cli/`.

## HYGP-046 - Dead trait impls and duplicate flag constants the compiler will not flag

- `shepr-core` `geometry.rs`: `impl From<bool> for SplitBranch` has no callers.
  Evidence: the hunter grepped the workspace for `SplitBranch::from`, `.into()`
  producing a `SplitBranch`, and any `bool`-to-`SplitBranch` coercion - zero
  sites, including inside `shepr-core`. Trait impls are invisible to `dead_code`,
  so nothing in the build notices. It also encodes a claim (`true` means
  `Second`) that nothing depends on:
  `shepr-client/src/shell/presentation/topology.rs` and `::input/mouse.rs` (two
  sites) all write the comparison out by hand, in the opposite direction. The
  same hunter notes why it went unnoticed: `SplitBranch` lives in `geometry.rs`
  while `SplitRatio`/`SplitBorder`/`Node` live in `layout.rs`, and
  `shepr-protocol/src/geometry.rs` has a third set of conversions.
- `shepr-termio` `input`: `model.rs` exports
  `pub const KITTY_FLAG_REPORT_ALL_KEYS` (not re-exported from `input/mod.rs`)
  while `encode.rs` declares four private `KITTY_FLAG_*` constants of its own
  from the same bitflags type, including its own `KITTY_FLAG_DISAMBIGUATE` far
  from the other three. Five copies of "read a bit out of `KittyKeyboardFlags`"
  that the bitflags type already provides via `.contains()`, and
  `KeyboardProtocol::reports_event_types` writes the bit as a raw literal
  `0b0000_0010` while `reports_all_keys` in the adjacent method uses the named
  constant. Delete all five and call `contains`; the raw literal then becomes
  unrepresentable.
- `shepr-vt`: `Setter::Vte(NamedPrivateMode)` and `ModeSpec::name` carry
  `#[allow(dead_code)]` with the justification "documents the table" and are read
  only by tests.

## HYGP-047 - `ModifyOtherKeysMode` is a second enum over a `shepr-vt` concept, and its output is recovered by sniffing a byte string

From `shepr-termio` / `shepr-client`.
`shepr_termio::input::model::ModifyOtherKeysMode` (`Mode1`/`Mode2`) emits
`b"\x1b[>4;1m"` / `b"\x1b[>4;2m"` from `set_sequence()`;
`shepr_vt::ModifyOtherKeysLevel` is a second enum over the same concept, written
as `\x1b[>4;{level}m` by `host_term::modes::set_direct_host_keyboard_protocol`.
`terminal_setup::setup_terminal_with_capabilities` bridges them with
`let parameter = if mode.set_sequence().ends_with(b";1m") { 1 } else { 2 };`
followed by `ModifyOtherKeysLevel::from_parameter(parameter)`. So the
mode-to-parameter mapping is owned three times and `set_sequence()`'s only
remaining consumer is the sniff - the bytes it builds are never written. A
`Mode3` or a spelling change in `set_sequence` silently yields `2`.

Evidence it is dead: one caller, which discards the bytes. The hunter also
records that this is not a forced copy: `shepr-vt` sits below `shepr-termio` in
the documented layering and `shepr-termio` already depends on it. Fix: delete
`ModifyOtherKeysMode` and have `host_modify_other_keys_mode()` return
`shepr_vt::ModifyOtherKeysLevel` directly, which also removes the `;1m`/`;2m`
literals from `model.rs`.

## HYGP-048 - Public surface nobody outside the crate names

- `shepr-remote` `machine.rs` re-exports only
  `{EndpointCatalog, EndpointCatalogChanges, EndpointCatalogWatch,
  SavedSshEndpoint, RemoteExecutable, ProfileId, SshMetadataCache, IntoSshTarget,
  SshTarget}`, so these are `pub` in private modules and reachable by nobody:
  `catalog::catalog_path` (verified zero external references, three internal
  uses) and `executable::REMOTE_EXECUTABLE_ROOT` /
  `executable::REMOTE_MISE_SHIM_SUFFIX` (zero external references).
  `REMOTE_EXECUTABLE_ROOT = "/"` additionally names nothing: its only use is
  `value.starts_with(REMOTE_EXECUTABLE_ROOT)`, i.e. "is absolute", which
  `Path::is_absolute` already spells. Fix: `pub(crate)`/`pub(super)`, and
  `Path::new(value).is_absolute()`. Enforcement named: add
  `unreachable_pub = "deny"` to the workspace lint table, which is exactly this
  finding enforced and would probably catch more elsewhere.
- `shepr-remote` `pub enum SshFailure` with six variants is re-exported from
  `lib.rs`; grepping the workspace, no external site names any variant, and
  consumers use only
  `SshFailureDiagnostic::{requires_authentication, is_host_key,
  is_stale_metadata, is_link_failure, needs_attention}`. Public surface that
  exists to be matched on and is never matched on. Two answers given: make it
  `pub(crate)` and keep the predicates as the public surface, or make it public
  and delete the five predicate wrappers, which currently duplicate
  `SshFailure`'s own two predicates plus three `==` comparisons. Today both
  interfaces exist and only one is used.
- `shepr-protocol` `PublicIdParseError` is exported but named only inside the
  crate (grep: zero hits outside `ids.rs`); it reaches `pub` through a
  `pub use ids::*`-style re-export. Minor; listed as one more symbol read as API.

## HYGP-049 - Enum variants that are constructed but never discriminated

- `shepr-config` `ConfigDiagnostic`'s six variants (`Read`, `Parse`,
  `Provenance`, `Unknown`, `Validation`, `Path`) are never distinguished: every
  consumer in the workspace calls `.message()` or `Display`, and the only
  construction outside the crate is `src/cli.rs` mapping into
  `ConfigDiagnostic::Path`. Evidence: no `match` on the enum exists anywhere
  except `message()` itself, which collapses all six arms into one. The
  classification is paid for at roughly 40 construction sites and read nowhere.
  Two answers: make it load-bearing by moving the message prefix into `Display`
  per variant, or collapse it to a newtype. Not lintable.
- `shepr-api` `ApiClientError::EmptyResponse` and `UnexpectedResult` are produced
  but never distinguished: every consumer in scope funnels them through
  `api_client_error_to_io` or `io::Error::other(err)`, i.e. straight to a string,
  and only `ApiClientError::Io` is ever matched. Evidence: grep for either name
  outside its definition and `Display` impl returns nothing. Not dead (they carry
  message text) but they carry no decision, so the enum's shape overstates what
  callers can do.
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

- `SHEPR_MIGRATION_OBSERVATIONS` (HYGP-036), `SHEPR_BRIDGE_TEST_LEGACY`
  (HYGP-039) and `GROK_CONFIG_DIR` (HYGP-033) are named by hunters as existing
  for something that is over or for tests only. `shepr-agent`
  also reports `SHEPR_TEST_3970_CONFIG_DIR`, a test-only name in a
  production-visible namespace carrying an issue number nobody can look up in
  this repository.

## HYGP-051 - `shepr-server`'s question-8 candidates cannot be confirmed while `dead_code` is silenced

From `shepr-server`. Because the crate-wide
`#![cfg_attr(feature = "test-api", allow(dead_code))]` is active in exactly the
build that would run the lint (HYGP-031), the hunter declined to call the
following dead without the lint enabled: `MIN_CLIENT_COLS` / `MIN_CLIENT_ROWS`
(used, but only to clamp to 1 - constants that have had one value and whose only
effect is "not zero"), the `AttachInputDelivery::Failed` variant, and
`ShutdownLifecycle::set_frozen_session_policy_for_test`. Their stated order of
operations: fix the blanket allow first and let the build answer, since the cost
of a wrong deletion here is the one nobody can undo by reading.

## HYGP-052 - `shepr-agent`'s `types.rs` and `actions.rs` are thirty-six structs and eighteen match arms serving one shape

From `shepr-agent`, framed by the hunter as the aggregate of several smaller
findings. `types.rs` holds thirty-six structs of two shapes:
`<Agent>InstallPaths` (one to four `PathBuf` fields) and
`<Agent>UninstallResult` (the same paths plus two or three `bool`s). They exist
so `actions.rs` can format per-agent sentences - 700 lines of near-identical
formatting across eighteen match arms, with the phrasing drifting between them
("installed X integration to", "installed X integration hook to", "ensured X
settings at", "ensured X config at").

Both families collapse into one
`InstallOutcome { artifacts: Vec<(Role, PathBuf)> }` /
`UninstallOutcome { removed: Vec<(Role, PathBuf)>, updated: Vec<(Role, PathBuf)> }`,
which the hunter says deletes `types.rs`, most of `actions.rs` and a third of
`targets.rs`. The hunter's larger consolidation, which subsumes several
duplication findings filed in the sibling document, is to make
`INTEGRATION_SPECS` the only table: config file name, config path depth, hooks
root, registration check strategy, directory key as an enum, asset, version,
events and timeout on the row.

## HYGP-053 - `ProcessDetectionMode::ChildGroups` may be a WSL-era mode that nothing exercises

**Decision:** WSL support is removed entirely: delete the mode, its budget and
parser, `SHEPR_PROCESS_DETECTION`, `running_inside_wsl` and every WSL branch
(including the one in `process_allows_remote_memory_read`).

From `shepr-agent`, gathering three of that hunter's findings. What tells them it
may be dead: shepr is documented Linux-only; native `tpgid` reading via
`/proc/<pid>/stat` has no documented failure mode on Linux, so the mode's trigger
condition (native detection returning `None`) is rare to nonexistent; nothing in
the repository sets `SHEPR_PROCESS_DETECTION`; and no test drives the mode
through its public entry point (the two `*_with` tests call the inner function
directly, and those seams exist precisely because the process-wide `OnceLock`
makes the mode itself untestable without leaking into other tests).

What tells them to be careful: `shepr_platform::running_inside_wsl()` still
exists and is called from three places in the crate, so somebody deliberately
supported WSL at some point. The hunter says to ask the owner before deleting.

If the WSL story is over, the deletion covers `ProcessDetectionMode::ChildGroups`,
`CHILD_GROUPS_SCAN_LIMIT`, `child_groups_foreground_process_group*`,
`PROCESS_DETECTION_ENV_VAR`, `parse_process_detection_mode` and the
`running_inside_wsl` branch in `process_allows_remote_memory_read`. If it is not
over, the comment should say so.

## HYGP-054 - `agent_name_from_known_package_path` hardcodes six npm package layouts

From `shepr-agent`: `@earendil-works/pi-coding-agent`, `@oh-my-pi/...`,
`@moonshot-ai/kimi-code`, `@qwen-code/qwen-code`, `mastracode`,
`@letta-ai/letta-code`, two of them twice for a `dist/bundle` variant. These are
upstream-version-specific paths, so a package layout change makes the arm dead
code that still reads as live, and nothing in the build can tell. The hunter
calls this the compatibility-path case: each arm should carry the version or date
it was observed, and re-checking belongs with the existing "monitor upstream
changes" work item.

Adjacent, from the same scope: `assets/hermes/plugin.yaml`'s `version: "1.0"` is
read by nothing on shepr's side (status parses `__init__.py`'s marker). It may
matter to Hermes; if not, it is a third version number for one integration.

## HYGP-055 - `SNAPSHOT_VERSION` is checked four times and the version field carries no type

From `shepr-mux` `src/persist/snapshot.rs`: `pub const SNAPSHOT_VERSION: u32 = 1`
with `parse_snapshot` and `parse_history_snapshot` rejecting anything else, and
`preserve_snapshot_history`, `prepare_snapshot_history` and
`finish_snapshot_history` each re-checking it (HYGP-034).

The hunter's distinction, worth keeping in the entry: unlike the other items in
their question-8 list this is a forward guard, not a backward compatibility path
- its job is to refuse a file from a future build, which is worth keeping. What
is not worth keeping is the same check written four times and the `version` field
being a bare `u32` so every consumer has to remember to check it. Fix: a newtype
whose `Deserialize` rejects the wrong version, so `SessionSnapshot` cannot exist
with a bad version and the four checks collapse to zero.

## HYGP-056 - Nine `serde` attribute pairs that restate the default they claim to tighten

From `shepr-protocol` `projection.rs` (7 fields) and adjacent types: `Vec` fields
carry
`#[serde(serialize_with = "codec::serialize_bounded_vec::<{ codec::MAX_COLLECTION_ITEMS }, _, _>", deserialize_with = "..")]`
while the codec's own `serialize_seq` / `read_collection_len` already enforce
`MAX_COLLECTION_ITEMS` on every sequence unconditionally in both directions.
What tells the hunter they are dead: the caps are numerically identical and the
codec applies its cap unconditionally. Eighteen lines of attribute doing nothing,
contradicting `serialize_bounded_vec`'s own doc comment ("lets wire fields state
a *tighter* rule"), and teaching the reader that a `Vec` field without the
annotation is unbounded. The remaining annotations (`MAX_SURFACE_PANES`,
`MAX_SURFACE_SPLITS`, `MAX_SURFACE_HYPERLINKS`, `MAX_SURFACE_CELLS`,
`MAX_SURFACE_PATCH_SPANS`, `MAX_SURFACE_SPLIT_PATH`, `MAX_SURFACE_DIMENSION`)
are all genuinely tighter. Enforcement named: a text rule banning
`serialize_bounded_vec::<{ codec::MAX_COLLECTION_ITEMS }` specifically.

## HYGP-057 - Modules and items sitting in a crate that does not use them

- `shepr-protocol/src/scroll.rs` is not wire code: `ScrollMetrics` derives no
  `Serialize`/`Deserialize` at all and never crosses the wire, and the rest is
  scrollbar rendering and hit-testing - `render_scrollbar_buffer` writes into a
  `ratatui::buffer::Buffer` and hardcodes the track glyph while taking
  `thumb_symbol` as a parameter (one of two glyphs injected, the other not), and
  `scrollbar_thumb_grab_offset` / `scrollbar_offset_from_row` /
  `scrollbar_offset_from_drag_row` are client interaction logic. What tells the
  hunter it does not belong: no type in the file is serialisable, and `ratatui`'s
  `Buffer`/`Rect` are a presentation dependency the wire-protocol crate does not
  otherwise need for drawing. Move it to `shepr-termio` or `shepr-client`; the
  enforcement is the move plus a structural rule about `Buffer` use, not the
  dependency allowlist alone, since `ratatui` is still needed for
  `ratatui_conversion.rs`.
- `shepr-api` `RenderDemand::join` has a dedicated test and one user,
  `shepr-server`. Not dead - flagged because `RenderDemand` lives in
  `shepr-api` while every consumer is in `shepr-server`, so it is in the wrong
  crate for its one client; moving it would tighten `shepr-server`'s use of the
  API crate to actual wire concerns.
- `shepr-api/src/schema/integrations.rs` is 47 bytes, a single re-export. Not a
  problem; noted because the module boundary buys nothing there.
- Related, filed in the sibling documents but pointed at from here because the
  fix is a move: `shepr-platform/src/logging.rs` holds 25 domain event functions
  named after concepts that crate knows nothing about, and `server/input_wire.rs`
  belongs in `shepr-protocol` (HYGP-023).

## HYGP-058 - Dependencies that production code does not use

- `shepr-pty` depends on `tokio` for one import,
  `tokio::sync::mpsc::error::TrySendError`. The actor is a std thread; the error
  type exists to match the test double's `mpsc::Sender`. Fix: a shepr-owned error
  type, then remove `tokio` from the `shepr-pty-layer` allowlist in
  `brokkr.toml`.
- `shepr-remote` lists `libc` under `[dependencies]`; the only uses are `fcntl`
  in `attach.rs` and `geteuid` in `local_server.rs`, both inside `#[cfg(test)]`
  modules, and `brokkr.toml`'s `shepr-remote-layer` rule allows `libc` for
  `kinds = ["normal"]`, so the allowlist currently blesses a dependency
  production does not use. Fix: move to `[dev-dependencies]` and drop `libc` from
  the allowlist - a one-line tightening of a check somebody already paid for. The
  hunter lists this as one of the two findings in their scope they would most
  want confirmed by an actual build.
- `shepr-remote`'s `sha2` is used only by `ProfileId::generate`; replacing that
  with `unpredictable_token` (HYGP-003) lets `sha2` be dropped from the crate and
  from its allowlist, which then enforces the change thereafter.
- `shepr-core` depends on `ratatui`: `layout.rs` uses
  `ratatui::layout::{Direction, Rect}` and `brokkr.toml` allows it, putting a TUI
  rendering crate at the bottom of the layering where `shepr-mux`,
  `shepr-protocol` and `shepr-config` all inherit it. `Rect` and `Direction` are
  four `u16`s and a two-variant enum; owning them in `shepr-core` alongside
  `GridSize` would drop `ratatui` from the bottom four crates' dependency closure
  and remove a re-export the wire types currently share with the renderer.
- `shepr-protocol`'s `serde_json` dev-dependency is used for one validation test
  and one line in `wire_tests.rs`; running that test through
  `codec::from_slice_exact` instead would let the dependency go and let
  `brokkr.toml`'s dependency rules keep it out.

## HYGP-059 - Two id types of one shape, and two `blit` modules of two shapes

- `shepr-protocol`: `PublicTabId` and `PublicPaneId` are two roughly 120-line
  types differing only in a discriminator character (`'t'` vs `'p'`) - and that
  character is spelled twice per type, once in `format!("{}:t{}", ..)` and once
  in `from_str`'s `parse_public_child_id(value, 't')`, with no link between the
  two. Everything else (`as_str`, `workspace_id`, `number`, `Display`, `FromStr`,
  `Serialize`, `Deserialize`, `Deref`, the two test-only `From`s, four
  `PartialEq` impls) is duplicated verbatim. Fix: one
  `PublicChildId<const KIND: char>` or a macro, after which the discriminator
  exists once and the duplication is structurally impossible.
- `shepr-termio/src/blit.rs` and `shepr-client/src/shell/presentation/blit.rs`
  are two modules named `blit` doing different things - the termio one encodes a
  frame to terminal bytes, the client one copies cells between `FrameData`
  buffers (`blit_pane_surface`). Not dead, but the collision makes "the blit
  code" ambiguous in every conversation and every grep, and the client's copy is
  pane-surface composition rather than blitting. Worth a rename
  (`compose_pane_surface`), free pre-1.0. Holdable only by review.

## HYGP-060 - A duplicated startup sequence and a duplicated shell-word predicate in `shepr-remote`

- `host.rs::ensure_remote_server_running` (41 lines) is
  `is_server_listening` -> `spawn_server_daemon` -> `wait_for_server_socket`,
  which is exactly `autodetect::auto_detect_launch`'s startup block minus the
  build-compatibility check and with a 5 s rather than 15 s budget. The missing
  build check is deliberate and documented, which the hunter calls a good
  comment; the duplicated sequence is not. Fix:
  `local_server::ensure_running(paths, ReadyTimeout, BuildCheck)` owns the
  sequence and both callers pick the policy explicitly. Not a rule, a shared
  function.
- `launch.rs::shell_quote` and `executable.rs::has_only_shell_safe_characters`
  contain the identical predicate
  (`ch.is_ascii_alphanumeric() || matches!(ch, '@'|'%'|'_'|'+'|'='|':'|','|'.'|'/'|'-')`).
  They agree today and serve different purposes (one decides whether to quote,
  the other whether to reject), which is why the duplication was easy to
  introduce and will be easy to let drift: `executable.rs` rejecting a character
  `shell_quote` would have quoted safely is a silent discovery failure. Fix: one
  `fn is_shell_plain_word(s: &str) -> bool`. Enforcement named: a test asserting
  `shell_quote(s) == s` exactly when `has_only_shell_safe_characters(s)`, which
  is writeable today and would pin the two together without merging them.
- `RemoteExecutable::needs_shell_quoting` exists only to produce a diagnostic for
  a path `parse` already rejected, re-running the same predicate from outside, so
  the rejection reason is computed twice by two functions that must agree. Fix:
  have `parse` return a typed rejection reason and delete
  `needs_shell_quoting`.

## HYGP-061 - Vestigial section banners and a stray import in `shepr-server`

`server/headless.rs` has a `// Constants` banner containing no constants (only
`struct ListenerFd`), a `// Loop event enum` banner that is fine, and a
`#[cfg(test)] use std::fs;` sitting among the crate-level imports of a 2222-line
file. No mechanical hold; listed because it is free.
