# Hunt: pane process lifecycle

Scope read in full: every file in `crates/shepr-pty/src/`; in
`crates/shepr-mux/src/pane/`: `launch.rs`, `launch_status.rs`,
`exit_arbiter.rs`, `child_watcher.rs`, `teardown.rs`, `runtime.rs`,
`runtime/{cwd,input,read,read_effects,spawn,theme}.rs`, `process_probe.rs`,
`runtime_registry.rs`, `logging.rs`; `crates/shepr-mux/src/limits.rs`.
Followed into `shepr-platform` (`process.rs`, `process_identity.rs`,
`proc_tree.rs`, `child_io.rs`, `lib.rs`, the `accept_peer` part of `ipc.rs`,
`host.rs::launch_executable`), `shepr-server` (`app/pane_launch.rs`,
`app/mod.rs`, `app/agent_resume.rs`, `app/api/panes.rs`, `limits.rs`),
`shepr-core/src/env.rs` and the test fixtures. `brokkr.toml` and
`clippy.toml` read first.

## 1. Defects

### D1. "Unconfirmed" launches whose child is alive and running never die and never launch

`LaunchOutcome::Unconfirmed` is documented (`launch_status.rs`) as "the
child is gone without a report, or the pane ended before the launch settled.
The pane's death follows and is an ordinary one", and the server relies on
that (`app/pane_launch.rs`: "The pane's death follows and is handled as any
other. The runtime stays"). Several `settle()` paths return `Unconfirmed`
while the child is alive and has exec'd the shell:

- the oneshot sender is dropped without delivering (see D3: `register` drops
  `deliver` on a parked connection from another pid), so `channel.ok()` is
  `None` at once;
- `AsyncFd::new(channel)` fails (epoll registration, ENOMEM);
- `channel.readable()` errors;
- `reader.read` returns a protocol error (wrong-size record, out of order).

In each case nothing records an ending, so no death follows. The pane keeps
a working PTY (the actor runs, the user can type into the shell), but
`ChildLiveness` is `Unconfirmed` forever: `live_process_id()` is `None`, so
detection never starts (`LaunchWatch::launched` returns false), `/proc` cwd
tracking is off, `child_pid()` is `None`, and an agent resume is abandoned
as `ShellLaunchUnconfirmed`. The pane is in a limbo the type system names
"gone". The fix is to split the outcome: "child gone / pane ended" stays
`Unconfirmed`, while "status unreadable, child alive" must either keep
waiting for the child's exit (then settle) or be a `Failed` that tears the
pane down. Enforceable by a test with a fault-injected channel (the
`settle` future takes the channel by value, so a test can hand it a socket
that delivers a bad record while a fixture child sleeps).

### D2. A dead launch status listener leaves every later launch unsettled, silently

`shepr_pty::launch::init` says "A service that cannot accept is an error:
launches would never settle while their children live." Only bind-time
failure is an error. At run time `Router::accept_loop` returns on
`Accepted::Fatal` (logs one `error!`), the `SERVICE` `OnceLock` still holds
`Ok`, and `spawn_pty` keeps forking and registering. Children connect into
the backlog, nobody accepts, the oneshot never fires, the child lives, so
`settle()` stays pending forever: no settlement, detection never starts,
agent resumes stay `Launching`, and no per-pane line says why. The listener
thread's death should poison the service (`service()` returning the cached
error) so spawns fail loudly, or the thread should be restarted. Testable by
making `Router` injectable (it is not today; see 3).

### D3. `register` and `route` disagree on a pid mismatch, and `register` loses the launch

In `LaunchService::register`, a parked connection for this ticket from a
different pid hits `Some(_) => None`: the foreign channel is closed, but the
launch is neither inserted into `waiting` nor logged, and `deliver` is
dropped. The real child's connection then arrives, finds no waiter, is
parked and expires after `LAUNCH_PARKED_CONNECTION_TTL`; meanwhile the
dropped sender makes `settle()` return `Unconfirmed` immediately (D1). The
same condition in `Router::route` re-inserts the waiter and warns. One rule
("a connection from the wrong process is dropped and the launch keeps
waiting") should live in one place on `Routes`. Not mechanically
enforceable beyond a unit test of `Routes`, which has none today.

### D4. `SHEPR_BIN_PATH` names the server binary, and nothing reads it

`ChildEnv::SheprBinPath` is documented (`shepr-core/src/env.rs`) as "the
shepr executable, set for every pane so programs in it can call back into
shepr", and `pane/launch.rs::launch_executable` as "The path panes are told
to run shepr by". It is resolved from `std::env::current_exe()` in the
process that launches panes, which is `shepr-server` (the `shepr-daemon`
package), not `shepr`. Its argument grammar is the daemon's. Nothing in the
repository reads the variable (no integration asset, no Rust site besides
the setter). And "set for every pane" is false when resolution fails:
`shepr_platform::launch_executable().ok()` caches `None` with no log, and
the variable is then removed from every pane. See also 9 (dead) and 5
(swallowed).

### D5. The accept loop busy-spins on a persistent poll error

`Router::accept_loop` does `let ready = poll(..); prune(..); if ready <= 0
{ continue; }`. A `poll` that fails with anything other than EINTR (ENOMEM
under memory pressure) returns at once every iteration, so the thread spins
at full CPU taking the routes lock. The `Backoff` sleep only covers accept
errors. Same fix as everywhere else in platform: go through
`poll_fd_readable`/`Wait` and back off on error (see 8, poll policy).

### D6. A directory whose name ends in " (deleted)" is treated as deleted

`crate::workspace::process_cwd_is_deleted` decides by the byte suffix
` (deleted)`. `ProcessCwd::read` (process_probe.rs) and the workspace cwd
filter use it, so a shell sitting in a real directory named `x (deleted)`
reads as `Deleted`: its cwd is never used for saves, splits or the Git
identity. The comment owns the trade-off (no stat on the event loop) but no
documentation says such directories are unsupported. Lower severity;
recorded because the claim "reject it without statting" silently widens to
"reject this name".

### D7. Teardown's survivor report is from a stale scan

`terminate_pane_session` rescans membership at the top of each round, but
after the last round it builds `survivors` from that round's `members`, so
a process forked during the final SIGKILL round is neither signalled nor
listed in the "pane session still alive after forced shutdown" warning.
Minor; the fix is one more `session_members` call before reporting.

### D8. Inline teardown fallback stalls the event loop

`shutdown_pane_processes` documents "Returns at once ... closing a
workspace never stalls the caller". When the teardown thread cannot be
spawned it runs `run_pane_teardown` inline, which is up to
`PANE_TEARDOWN_BUDGET` (750 ms) of sleeps plus full `/proc` scans on the
caller, which is the event loop (the runtime is dropped from
`PaneRuntimeRegistry::remove`/`clear`). This breaks the scope's rule that a
pane never stalls the server loop. `child_watcher::reap_on_detached_thread`
makes the opposite choice for the same failure ("left ... rather than
waited on inline: the caller may be the event loop"). See 8.

### D9. A blocking-pool cancellation drops the child unreaped

`wait_for_child_exit_blocking` moves the `PaneChild` into
`spawn_blocking`. The comment says "A blocking task keeps running once
started even if this await is dropped"; a task not yet started when the
runtime shuts down is dropped with its closure, and `PaneChild` "neither
kills nor reaps". `UnreapedChild`'s promise ("it never stays a zombie for
the rest of the process") does not hold on that path. Low impact (the
process is exiting), but the guard type stops guarding as soon as the
child leaves it; the fix is to move the `UnreapedChild` itself into the
blocking closure.

## 2. One value, one owner

- **The pidfd-exit wait.** `child_watcher::spawn` and
  `launch_status::settle` each `try_clone_pidfd()` the same child, each
  register it with tokio, and each carry a fallback (blocking `waitpid` in
  the watcher, a `LAUNCH_EXIT_POLL_INTERVAL` poll loop in settle). The
  coordinator re-derives what the watcher already records
  (`mark_wait_completed`). One owner: the watcher sets the fact and
  `ChildLiveness` carries a `tokio::sync::watch`/`Notify` the coordinator
  awaits; the second dup, the poll loop and `LAUNCH_EXIT_POLL_INTERVAL`
  disappear. Enforceable structurally (no `try_clone_pidfd` outside the
  watcher; a textlint over `crates/shepr-mux/src/pane/` would hold it).
- **"Is there a process behind this pane?"** is answered twice:
  `ChildIo::child_backing()` (`ChildBacking::{Process, NoProcess}`) and
  `ChildLiveness`'s `ChildIdentity::{Process, Absent}`. Production
  `with_child_io` already sets `ChildLiveness::launched_without_child()`,
  and `shutdown_pane_processes` already returns early on an absent
  identity, so `ChildBacking` only matters when a test swaps a real
  `ProcessHandle` into a runtime whose IO is `ChannelChildIo` (the cwd tests
  in `runtime.rs` put the test process's own handle there; without the
  backing check, dropping that runtime would SIGHUP/SIGTERM/SIGKILL the test
  binary). Two writers of one invariant; delete `ChildBacking` and let tests
  use a fixture child. Enforced by deleting the type.
- **Exit codes 126/127.** `backend.rs` defines `EXIT_SETUP_FAILED` and
  `EXIT_LAUNCH_FAILED` privately; the mux test
  `a_missing_configured_shell_fails_in_the_child_not_at_the_fork` asserts the
  literal `127`. Make them `pub` and reference them, or the test drifts.
- **The kernel's " (deleted)" marker** is spelled in
  `shepr-mux/src/workspace.rs::process_cwd_is_deleted` and in
  `shepr-platform/src/host.rs::resolve_launch_executable`
  (`strip_suffix(b" (deleted)")`). Both describe the same kernel `d_path`
  convention; one platform helper. Textlint for the literal outside it.
- **`numeric_file_name`** is defined twice in platform (`process.rs` and
  `proc_tree.rs`), identical bodies.
- **The idle-poll period** is `ACTOR_IDLE_POLL` in `shepr-pty/src/limits.rs`
  and restated as "at least once a second" in `PtyIoActorConfig::core_broken`'s
  doc, and assumed by two tests (see 6). Copies have not diverged yet.
- **The teardown step count** is `PANE_TEARDOWN_STEPS` (3 entries), restated
  as "Three signal grace periods" on `PaneTeardownTracker::BUDGET` and as
  "plus three more of it" on `shepr-server`'s `PANE_TEARDOWN_WAIT`. Reword to
  not hard-code the count.
- **Server environment.** `PtyCommand::interactive_shell` copies
  `std::env::vars_os()` on every pane spawn, and `cwd_candidates` reads
  `HOME` from that copy, while `passwd_home` is read once at init. Same
  process, two resolution moments for "the user's home"; the snapshot should
  be taken once at `init_pane_launches` with the passwd home. No divergence
  today because production never writes the environment (clippy seal).

## 3. Values nobody can find, change, or trust

- **Pane lifecycle tunables are spread across three limits modules** with
  coupled values uncoupled. `shepr-pty/src/limits.rs` holds
  `LAUNCH_HELLO_TIMEOUT` (1 s), `LAUNCH_ACCEPT_RETRY_DELAY`,
  `LAUNCH_PARKED_CONNECTION_TTL`, `ACTOR_IDLE_POLL`; `shepr-mux/src/limits.rs`
  holds `LAUNCH_STATUS_AFTER_EXIT` (1 s), `LAUNCH_SETTLE_AFTER_PANE_END`,
  `LAUNCH_EXIT_POLL_INTERVAL`, `TERMINAL_CLOSED_EXIT_GRACE`,
  `PANE_TEARDOWN_STEPS`; `shepr-server/src/limits.rs` holds
  `PANE_TEARDOWN_WAIT`. `LAUNCH_STATUS_AFTER_EXIT`'s doc says a connected
  child "is already in the listener's queue and is routed at once". It is
  not: the single listener thread reads each hello serially with up to
  `LAUNCH_HELLO_TIMEOUT`, and may be sleeping `LAUNCH_ACCEPT_RETRY_DELAY`.
  One stray same-uid connection ahead of a failing child consumes the whole
  1 s window and the failure report is lost (the coordinator settles
  `Unconfirmed` and the placeholder never says why). The client crate already
  writes `const _: () = assert!(..)` for coupled bounds; a
  `LAUNCH_STATUS_AFTER_EXIT > LAUNCH_HELLO_TIMEOUT + LAUNCH_ACCEPT_RETRY_DELAY`
  assertion needs the pty values to be `pub`, which is fine.
- **`ACTOR_IDLE_POLL`** is documented as "only a fallback for a missed
  wake", but it is also the only cadence at which a terminal core poisoned
  off the reader thread is noticed (`core_broken`, checked per loop). The
  doc should say so. It has no injection point: `TestPtyIo` can replace
  `poll` but `run_loop` passes `Wait::After(ACTOR_IDLE_POLL)` itself, so
  `a_core_broken_elsewhere_ends_an_idle_pane` waits up to 3 s of wall clock.
- **`LAUNCH_SETTLE_AFTER_PANE_END`** has no injection point;
  `a_hung_launch_does_not_keep_a_failed_reader_from_ending_the_pane` waits it
  out in real time (the comment says so). `coordinate` could take the bound
  as a parameter, as `reader_exit_callback` already does for
  `TERMINAL_CLOSED_EXIT_GRACE`.
- **`PANE_TEARDOWN_STEPS`** are not injectable either;
  `pane_teardown_reaches_background_jobs_after_the_leader_is_reaped` waits
  through real grace periods.
- **Router timings** (`LAUNCH_HELLO_TIMEOUT`, `LAUNCH_PARKED_CONNECTION_TTL`)
  live inside a process-global `OnceLock` service with no seam at all, so
  parking, retirement and the pid-mismatch paths are untested (see D3).
- **`PANE_TEARDOWN_WAIT = BUDGET * 4`** is a guess: "/proc session scans ...
  which the signal budget does not count" are unbounded (two full `/proc`
  walks per round, each with a `ProcStat` read per pid, opened twice per
  candidate). Nothing measures or bounds them.

## 4. One channel, one implementation

- **Spawn failure is logged twice at different levels.** `PtySetup::start`
  logs `error!("failed to spawn shell")` on a `spawn_pty` failure, then
  `agent_resume.rs` logs `warn!("failed to start shell for deferred agent
  resume")` for the same event; the split path (`api/panes.rs`) logs
  nothing and returns the text to the client. Meanwhile an actor-startup
  failure (the other `Err` from `PtySetup::start`) is not logged at the mux
  level at all. One site (the launcher) should log once with pane, kind,
  cwd and stage.
- **Reap failures** are `error!` in the watcher (`pane_exit_failed`) and
  `warn!` in `UnreapedChild::drop` and `reap_on_detached_thread`. Same class
  of event (a possible zombie), two levels.
- **Missing lines.** A `Launched` settlement is not logged, so a launch that
  fell back from its requested directory to `HOME`/passwd home/`/` leaves no
  trace of which candidate it entered or why candidate 0 failed (the child
  does not even send that errno: only total failure reports it). An
  `Unconfirmed` settlement is not logged either, which is the only signal of
  D1/D2. `pane_spawn_started` logs rows, cols and the scrollback budget
  (constant per server) but not the launch kind, cwd or shell.
- **Child setup failures are indistinguishable from the shell's own exit.**
  Every pre-exec step failure exits 126 and the watcher logs only "pane
  child exited" with the status; a shell that itself exits 126 reads the
  same. Failures after the status socket is connected (the `sigprocmask`
  reset) could send a record and do not.
- **Thread names** `shepr-pane-{id}-teardown`, `shepr-pty-{id}`,
  `shepr-launch-status`, `shepr-launch-reaper` exceed or approach Linux's
  15-byte thread-name limit: `pthread_setname_np` truncates, so every
  teardown thread shows as `shepr-pane-NN-t` and the reaper as
  `shepr-launch-re`.

## 5. Errors

- `launch_executable().ok()` (`pane/launch.rs`) swallows the resolution
  error permanently and silently (D4).
- `open_pty_with_geometry` returns bare `io::Error`s from five different
  steps (`open /dev/ptmx`, `grantpt`, `unlockpt`, `TIOCGPTPEER`,
  `TIOCSWINSZ`); the operator sees "failed to spawn shell: Inappropriate
  ioctl for device" with no stage. Wrap each with the step it was.
- `PtyIoActor::spawn` maps the thread-spawn error through
  `io::Error::other(err.to_string())`, dropping its kind.
- `PaneOutputWrite::write` discards a poisoned-core error with `.ok()`; it is
  a test seam (see 8) but public.
- The shepr-pty `PaneChild::kill` turns a failed `pidfd_send_signal` into
  `last_os_error()` read after `ProcessHandle::signal` returned, which is
  correct only because nothing runs in between; `signal` should return the
  `io::Result` itself.
- Nothing in scope aborts on operator-controlled input; NUL bytes in the
  shell path, environment or cwd are refused with a named field.

## 6. Tests that prove nothing (or prove it only sometimes)

- **`pty_spawn_leaves_one_parent_pty_fd` is racy.** It counts `/dev/pts`
  and `/dev/ptmx` fds in the whole test process under a lock private to
  `backend.rs`'s test module. `actor.rs`'s
  `actor_open_pty_handles_io_resize_and_slave_close` opens a PTY pair (and
  clones the master) in the same test binary without taking that lock, so
  run in parallel the `before + 1` assertion can fail or pass for the wrong
  reason. Move the lock to a crate-level test helper and take it there too.
  The same test sets `SHEPR_ENV=in-pane` on the command, a setup step with
  no bearing on what it asserts.
- **`actor_wakes_idle_poll_for_user_input`** proves wake-driven writes by
  `elapsed < 500 ms` against a 1 s idle poll. If `ACTOR_IDLE_POLL` drops to
  500 ms or less, the test passes whether or not the wake pipe works. Derive
  the bound from the constant (`ACTOR_IDLE_POLL / 2`) or, better, have the
  poll observer report the wake's readiness.
- **`a_core_broken_elsewhere_ends_an_idle_pane`** hard-codes a 3 s budget
  derived from the same constant.
- **`the_child_keeps_no_inherited_descriptor`** does `dup(0)` and asserts
  `> 2` as a "test precondition": under a runner with stdin closed it fails
  for an environmental reason. Create the leaked fd itself (a pipe without
  `O_CLOEXEC`).
- **The focus tests** in `runtime.rs` build `current_size:
  cells_only(24, 80)` (24 columns, 80 rows) around an 80x24 terminal: the
  fixture names a geometry it is not running.
- **Environment reads without `IsolatedEnv`.** `PtyCommand::interactive_shell`
  reads the process environment; most tests in `command.rs`, `backend.rs` and
  `runtime.rs` call it without the guard the repository rule asks for. Today
  they override what they assert on, so it is a convention breach, not a
  wrong result.
- No test exercises `Router` (park, retire, pid mismatch, hello timeout) or
  `accept_hello`, so D2, D3 and D5 have no test that could fail.

## 7. Guards and claims that have stopped holding

- **`clock-io-ok` markers in `shepr-mux/src/pane/` are decoration.** No
  textlint covers `shepr-mux` outside `persist/`, yet `spawn.rs`,
  `exit_arbiter.rs` and `child_watcher.rs` carry the marker as if one did,
  while unmarked reads sit beside them (`runtime/read_effects.rs` three
  times, `runtime.rs::PaneOutputWrite::write`, `terminal/backend.rs`).
  Checkable today: add a `mux-pane-clock-is-injected` rule like
  `terminal-core-clock-is-injected`.
- **"The only place this becomes poll(2)'s int milliseconds is
  `Wait::poll_millis`"** (`shepr-platform/src/child_io.rs`) is false today:
  `shepr-pty/src/launch.rs::accept_loop` computes `wake_ms` from
  `LAUNCH_PARKED_CONNECTION_TTL.as_millis()` and calls `libc::poll` directly;
  `process.rs::has_exited` and `stream_wake.rs` pass literals. Checkable: a
  textlint on `libc::poll\(` outside `child_io.rs` with a marker.
- **`run_child`'s contract** ("no allocation, no lock, no destructor, no
  panic") is unenforced and has one bounds-checked index,
  `plan.envps[index]`, that would panic (unwind in a forked child of a
  multithreaded process) if `dirs` and `envps` ever diverged. They cannot
  today (both come from `candidates`), but nothing holds it; use `get` and
  `child_exit`, or build one `Vec<(dir, envp)>`.
- **`coordinate`'s "nothing here indexes unchecked or unwraps"** is the
  safety argument for the pane's only publisher; a module-level
  `#![deny(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used,
  clippy::panic)]` on `launch_status.rs` (outside tests) would make it a
  build fact.
- **The inbox lock "is never held across a syscall"** (`PtyIoInbox` doc)
  holds today; checkable only by review.
- **`LAUNCH_STATUS_AFTER_EXIT` "routed at once"** is false (see 3).
- **`init_pane_launches` / `launch::init` "A service that cannot accept is
  an error"** is false after start-up (D2).
- **`SHEPR_BIN_PATH` "the shepr executable, set for every pane"** is false
  twice over (D4).
- **`LaunchOutcome::Unconfirmed` "The pane's death follows"** is false on
  four paths (D1); `app/pane_launch.rs` repeats the claim.

## 8. Policy invented per call site

- **Reaping is implemented three times.** `backend.rs` has its own
  `shepr-launch-reaper` thread with a raw `waitpid` EINTR loop for the
  `ProcessHandle::open` failure; `PaneChild::wait_with` has a second
  `waitpid` loop; `child_watcher::reap_on_detached_thread` spawns
  `shepr-pane-reaper` over `PaneChild::wait`. Kill-then-reap on a failed
  start is likewise written twice (`spawn_pty`'s handle failure and
  `PtySetup::start`'s actor failure, the latter also starting a full
  teardown and then sending SIGKILL itself). One `PaneChild::abandon()` in
  shepr-pty would own it.
- **Thread-spawn failure policy disagrees** between teardown (run inline on
  the caller, D8) and reaping (leave the zombie rather than block). Pick one
  rule: never block the caller.
- **Wrong-pid connection policy** differs between `register` and `route`
  (D3).
- **Poll timeout conversion** bypasses `Wait` in the launch accept loop (7).
- **Ambient dependencies.** The pane spawn path reads the process
  environment (`base_env`) and the clock (read effects, the reader-exit
  callback, `decide_after`) directly; the platform crate has the
  clock-injection rule, mux's pane code does not.
- **Shared state kept by call order.** `PaneReadEffects::timer_writer` is a
  `OnceLock` set after the actor spawns; the timer path handles the gap by
  dropping replies with a warning. The ordering is documented, not
  structural; spawning the actor with the effects already holding a handle
  (create the inbox and wake pipe first, then the thread) would remove the
  window.
- **Blocking work off the loop is bounded per pane, not overall.**
  `flush_expired_synchronized_output` runs on `spawn_blocking` and, through
  the deferred-effect ticket order, can wait behind the PTY actor's OSC 7
  `stat` on a hung mount, holding a tokio blocking-pool thread for as long as
  the mount hangs; the watcher's blocking fallback also holds one pool thread
  per pane for the pane's life. The pool is shared with the rest of the
  server. Bounded by pane count, but not by anything the server configures.
- **Test-only shortcuts production can reach.** `PaneOutputWriter::try_begin`,
  `PaneOutputWrite::write` (reads the clock, swallows errors),
  `PaneRuntime::output_writer` and `PaneRuntime::with_child_io` are `pub` and
  used only by tests (mux's own and `shepr-server/src/test_support.rs`). The
  repository sanctions seams over test features, so this is by design; the
  finding is that `write` and `try_begin` are wider than the seam needs
  (production's reader uses `begin` plus the private `process`).
- No secrets reach logs in scope; pane environment values are never logged.

## 9. Code that is no longer load-bearing

- **`SHEPR_BIN_PATH`** (D4): exported to every pane, read by nothing in the
  repository, and its resolution is the only reason `init_pane_launches`
  stats the server binary. Given that shepr deliberately offers panes no way
  to drive it, remove `ChildEnv::SheprBinPath`, `launch_executable()` in
  mux and the init step, or decide what it is for and point it at `shepr`.
- **`shepr_platform::session_member_handles`** is a one-line alias of
  `session_members`, exported, with one caller (a mux test). Delete it.
- **`ChildBacking`** exists only for `ChannelChildIo` (2).
- **`PaneLaunchEnv::pane_id: Option`**: production always calls
  `with_pane_id`; the `None` branch ("stays unset rather than inheriting")
  is exercised only by tests. Make the id a required constructor argument.
- **`PaneRuntimeRegistry`**: `new()` duplicates `Default`;
  `From<HashMap<..>>` has only test callers; `IntoIterator` has no
  production caller I found. Restore builds and returns its own
  `HashMap<PaneId, PaneRuntime>` (`OpenedSession::terminal_runtimes`,
  `restore.rs`) instead of the registry, so the "server-owned live pane
  runtimes" newtype is bypassed for exactly the runtimes created before the
  app exists.
- **`fd::set_cloexec` on the PTY master** in `PtyIoActor::spawn_inner`:
  every production master is opened `O_CLOEXEC`; the call only matters for
  test sockets.
- **`PaneTeardownInFlight::drop`'s "completion had no matching start"**
  branch cannot occur (the guard is only minted by `start`).
- **`ChildLiveness::launched_without_child`** carries two doc paragraphs
  for one constructor, the first describing "the public ChildIo constructor"
  in terms that predate `with_child_io`'s current doc.
- No leftovers of the removed pane-history feature or the recently removed
  settings were found in scope.

## Lateral findings

- **The launch listener serializes hellos.** `accept_hello` blocks the only
  accept thread for up to `LAUNCH_HELLO_TIMEOUT` per connection. Any
  same-uid local process can connect to the abstract socket (its name is
  discoverable in `/proc/net/unix`) and delay every pane's settlement by a
  second per connection, which (per 3) loses failure reports. Reading the
  hello non-blockingly per connection (poll the set) removes the coupling.
- **Fds per pane**: master, two wake-pipe ends, the status channel while
  settling, the `ProcessHandle` pidfd and two dups of it. The 2-dup is the
  structural duplication in 2.
- **The startup-failure kill path** in `PtySetup::start` starts a full
  three-step teardown thread (with `/proc` scans) and then SIGKILLs the
  child itself; the teardown's SIGHUP grace is moot.
- **`ProcessHandle::open` logs at `error!`** for every non-ESRCH failure; a
  teardown scan under fd exhaustion logs one error per process in `/proc`.
- **The cwd fallback is silent end to end**: the child reports only the
  index it entered, the coordinator returns `Launched { cwd }`, and
  `pane_launch.rs` stores it. A user whose pane opened in `HOME` because the
  requested directory was unreadable has no log line or notice saying so.
