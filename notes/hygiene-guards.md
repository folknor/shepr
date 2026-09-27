# Hygiene: tests that prove nothing, guards and claims that stopped holding

This file consolidates the findings for questions 5 and 6 of the nine-scope
hygiene hunt over the shepr workspace: tests that depend on the environment they
run in rather than on anything this repository builds, tests that cannot fail,
checks that fail open when a name stops matching, and invariants asserted in
comments, documentation or commit messages that nothing in the build would
notice becoming false. It is a working document assembled from the raw hunter
reports; nothing here has been verified, and some entries may be phantoms. A fix
pass should expect that.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

---

## HYGG-001 - Tests depend on host-installed programs rather than on anything the workspace builds

Reported independently from six scopes. Nothing in the repository provides these
binaries, so the suite asserts against whatever the developer's machine happens
to have.

`shepr-platform` (`tests.rs`, `process.rs`): `/bin/sleep`, `/bin/sh`,
`/bin/cat`, and `sh`, `printf`, `yes`, `head` resolved from `PATH`. `printf` is
used as an *executable*, so on a host where it exists only as a shell builtin
the test fails. The generated fake clipboard helpers carry `#!/bin/sh`
shebangs.

`shepr-pty`: `/bin/sh`, `/bin/cat`, `sleep`, `printf`.

`shepr-mux`: `bash` in `pane/terminal/migration_tests.rs`; `/bin/sh` at three
sites in `pane/runtime.rs`.

`shepr-agent`: `bash` in the `detect` tests; `/bin/sh` in
`version_probe_deadline_includes_inherited_stdout`; `python3`, which is
skipped-if-absent (the same finding with a nicer failure mode).

`shepr-remote`: `local_server.rs::server_daemon_detach_creates_new_session`
shells out to `sh -c 'ps -o sid= -p $$ | tr -d " "'`, so it needs `ps` with
BSD-ish `-o sid=` support and `tr` - and it tests
`shepr_platform::detach_server_daemon_command`, another crate's function, from
this crate's test module. `attach.rs` (three sites) and `launch.rs` (one) spawn
`/bin/sh` to execute generated remote scripts; the hunter calls that defensible
since POSIX-shell behaviour is the thing under test, but says it should be
stated. `process.rs::timeout_kills_the_child` and
`a_stderr_pipe_held_by_a_background_process_does_not_block_the_result` spawn
`sh` and `sleep`.

`shepr-server`: `app/tab_bar_status.rs` uses `printf 'old\nfinal\n'`,
`head -c 5000 /dev/zero | tr '\0' x`, and `sleep 0.3` (fractional sleep is a
coreutils extension, not POSIX); hardcoded `/bin/sh` in `tab_bar_status.rs`,
`app/snapshot_tests.rs`, `app/agent_resume.rs` and
`server/headless/tests/mod.rs`.

Enforcement rules the hunters named: a tiny test helper binary built by the
workspace (a `[[bin]]` in `shepr-test-support` that can sleep, echo, exit with a
code and hold a pipe open) replaces almost all of it and makes the tests depend
only on what the repo builds - the `shepr-platform` hunter calls this a
structural fix worth the effort because the same shapes recur in `shepr-pty` and
`shepr-agent`. The `shepr-remote` hunter proposes a brokkr text rule that test
modules may not name `ps`, `tr`, `sleep`. Two hunters note that `/bin/sh` on a
Linux-only project is a defensible dependency, so there is partial disagreement
about whether the `/bin/sh` sites are findings at all.

## HYGG-002 - Tests shell out to the host `git`

`shepr-mux/src/git/test_support.rs::run_git` spawns `git` and
`.expect("test precondition")`s the spawn. `init_repo_with_commit`,
`create_repo_with_linked_worktree` and `create_bare_repo_with_linked_worktree`
all need `git worktree` and `git clone --bare`, i.e. a reasonably modern `git`
installed on the machine running the suite, and `live_git_space` exercises
production code that shells out again. The `shepr-mux` hunter calls this the
sharpest of the host dependencies: nothing this repository builds provides
`git`, its version governs reftable and worktree behaviour - which is exactly
what the tests check - and `brokkr check` is the gate, so a machine without
`git`, or with one old enough to lack `extensions.refstorage`, fails or silently
passes differently. The crate already has `write_fake_tracked_repo`, which
writes a fixture repo as plain files, so most of the fixtures need no binary;
the few that genuinely need a real `git` should assert the binary's presence and
version up front so a missing one is a failure with a subject rather than a
spawn panic.

`shepr-server/src/app/git_refresh.rs` shells out to `git init` via
`Command::new("git")` resolved from `PATH`, in a test that asserts cache-key
deduplication - the `git` dependency is incidental to what is being checked, and
the hunter notes the code under test only canonicalises paths, so writing a
`.git` directory by hand removes the dependency entirely.

## HYGG-003 - The only behavioural test of the logind protocol never runs

`shepr-platform/src/shutdown.rs`'s
`delay_lock_is_held_until_checkpoint_and_retaken_after_cancellation` is
`#[ignore = "requires dbus-daemon; ..."]`. It is the sole test of
`watch_connection`: inhibitor acquisition, the hold-until-checkpoint invariant,
retake-after-cancellation, and the `already_preparing` reconnect branch.
Everything `brokkr check` actually exercises in that module is the three pure
`Shared` tests. The mechanism `AGENTS.md` calls out as the reason `zbus` is a
dependency at all - logind's delay inhibitor letting the server save before host
shutdown kills panes - is unverified by the gate.

Enforcement rule named: `zbus` can serve the `LoginManager` interface over a
`UnixStream` pair or a `p2p` connection with no `dbus-daemon` at all, which
removes the external-binary dependency and lets the test run in the gate. The
hunter calls this worth doing.

## HYGG-004 - `failed_cli_registration_preserves_existing_config` re-executes the test binary through the host `bash`

`shepr-agent/src/integration/opencode_config.rs`. The test re-runs the test
binary through the host shell with `ulimit -f 0`, so it depends on the host
shell, on `trap '' XFSZ` semantics, on `std::env::current_exe`, and on the
literal test path string
`integration::opencode_config::tests::failed_cli_registration_preserves_existing_config`
duplicating the function's own name. It does fail closed - the stdout assertion
catches a filter that matches nothing - which is why the hunter files it as
hygiene rather than a defect. The environment variable it invents,
`SHEPR_TEST_3970_CONFIG_DIR`, carries an issue number nobody can look up in this
repository and is a test-only name in a production-visible namespace.

## HYGG-005 - Tests that assert on the wall clock

Reported from every scope that has timing at all. None can be made
deterministic while the timeouts are `Instant`-based constants with no injection
point.

`shepr-platform`: `ipc.rs::deadline_reader_cuts_off_a_trickling_peer` (300 ms
deadline, 50 ms trickle, 2 s bound, real thread sleeps); `tests.rs` 200 ms
deadlines with 5 s bounds; the wl-copy owner test polling a marker file for 2 s;
`remote_bridge_tests.rs` (300 ms `TIMEOUT`, 60 ms sleeps, 3 s waits, and
`legacy_bridge_has_no_idle_deadline` proving a negative by sleeping
`TIMEOUT * 2`); `shutdown.rs` (5 s timeouts, 5 ms polls).

`shepr-vt`: `synchronized_output_buffers_until_end_or_timeout` sleeps through
vte's 150 ms timeout, and asserts `!flush_expired...` right after a write, which
the hunter expects to flake under load. `shepr-mux`'s `runtime.rs` teardown
tests assert `< 500ms`, `>= delay/2`, `< 200ms`.

`shepr-agent`: `version_probe_deadline_includes_inherited_stdout` asserts
`elapsed < 250ms` after a real 300 ms sleep - a flake on a loaded machine or a
debug build, and on a fast one it proves the deadline only coincidentally.

`shepr-remote`: `ssh_agent::registration_retries_when_the_api_is_initially_missing`
sleeps in 10 ms increments against a 5 second wall-clock deadline
(`assert!(Instant::now() < deadline, "registration did not retry")`);
`wait_for_server_socket_succeeds_after_delay` sleeps 50 ms in a spawned thread
and allows 2 s.

`shepr-server`: `app/mod.rs` sleeps 30 ms, `tab_bar_status.rs` sleeps 1100 ms
and 400 ms, `client_transport.rs` sleeps 5 ms in loops - the hunter ties all of
them to there being no injection point for `Instant::now()` at roughly 25
production sites in a loop that already has a `now`.

`shepr-client`: `shell/input/input.rs` has one clipboard test that sleeps 400 ms
inside a fake clipboard reader and asserts `started.elapsed() < 300ms` (a coin
flip on a loaded machine, and the 400 ms is paid on every run of the suite), and
another that sleeps 10 ms. The bounded-read helper already takes a `Duration`
and a closure, so only the clock it measures against is missing.

Enforcement rules named: a `Clock` trait or a plain `fn now() -> Instant` passed
in, living in `shepr-core`, plus a `clippy.toml disallowed_methods` entry for
`std::time::Instant::now` / `SystemTime::now` outside designated modules. The
`shepr-platform` hunter calls this the root cause of most of its question-5
section and notes that `SshAgentLease::refresh_at(now)`, `Activity`,
`DeadlineReader`, `wait_child_until` and `wait_for_process_exits` already take
deadlines and could take a clock. `shepr-client`'s `endpoint/health.rs` and
`shepr-mux`'s `terminal/state/**` and `pane/process_probe.rs` are named as the
in-repo models that already thread `now` and need no sleeps.

## HYGG-006 - A persist test fabricates a file mtime a day in the future to walk past a gate

`shepr-mux/src/persist/writer.rs` reaches `SystemTime::now()` directly at four
sites (`preserve_snapshot_history`, `prepare_snapshot_history`,
`preserve_existing_in` twice), so the 15-minute snapshot gate cannot be tested
except by lying about file mtimes - which the test does:
`.set_modified(SystemTime::now() + Duration::from_secs(86400))`. The hunter's
reading: a test that sets a file's mtime a day in the future to get past a gate
is a report that the gate has no seam. By contrast `src/terminal/state/**` and
`src/pane/process_probe.rs` thread `now: Instant` through every entry point, so
the crate already knows the pattern and `persist` does not.

Enforcement rule named: take `now: SystemTime` as a parameter the way the state
layer takes `now: Instant`, delete the mtime fabrication, and hold it with a
text rule against `SystemTime::now()` / `Instant::now()` in `src/persist/`.

## HYGG-007 - Tests that skip silently, or return early, and report success

`shepr-remote/src/remote/local_server.rs::is_server_listening_returns_permission_errors_instead_of_false`
`return`s early when running as root, so it silently passes as a no-op in a root
container.

`process_cwd_does_not_require_traversing_the_directory_path` in
`shepr-mux/src/pane/runtime.rs` does `eprintln!("skipping untraversable cwd
assertion for privileged test process")` and passes. Reported by both the
`shepr-mux` and the `shepr-vt`/`shepr-pty` hunters. The notice goes to stderr,
which the harness hides on success, so nothing reports it. The `shepr-mux`
hunter adds that this is the crate's only `eprintln!` in a source file: the
project has no channel for "test was skipped", so one was invented. Rust's
harness has no skip state, so the mechanical answer is either to make the test
not need privilege separation (run the assertion in a subprocess that drops
privileges, or assert the platform behaviour rather than the effect) or to fail
when running privileged so the condition is loud.

`shepr-agent`'s `python3`-dependent tests skip when the interpreter is absent
(see HYGG-001).

Enforcement rule named by the `shepr-remote` hunter: make the root skip an
explicit failure or an `#[ignore]`, plus a rule that a `return` inside a
`#[test]` needs a comment.

## HYGG-008 - `bridge_child` is a `#[test]` that returns immediately and passes

`shepr-platform/src/remote_bridge_tests.rs`: if `SHEPR_BRIDGE_TEST_SOCKET` is
unset it `return`s. It is a subprocess entry point re-entered through
`current_exe --exact remote_bridge_tests::bridge_child`, not a test, and in
every normal run it appears in the pass list having executed four lines. The
hunter accepts the pattern as necessary - there is no other way to get a child
process running crate-private code - but says it should be named so nobody reads
it as coverage, and the guard should `panic!` when the variable is absent unless
the harness can be told to skip it. The same file hardcodes libtest CLI flags
(`--exact`, `--nocapture`), which couples the crate's tests to the harness.

## HYGG-009 - `config_metadata_preserves_ownership_and_acl_without_inheriting_extra_access` compares a value to itself on every normal run

`shepr-platform/src/tests.rs`:

```rust
if effective_uid() == 0 {
    assert_eq!(unsafe { libc::fchown(input.as_raw_fd(), 1001, 1002) }, 0);
}
...
assert_eq!(
    (actual.uid(), actual.gid(), actual.mode()),
    (original.uid(), original.gid(), original.mode())
);
```

Unless the suite runs as root the `fchown` never happens, so source and
destination were both created by the same uid/gid and the ownership half of the
assertion cannot fail no matter what `write_config_temporary` does to ownership.
`brokkr check` does not run as root. The ownership-preservation logic - `fchown`
plus the `EPERM` tolerance in `config_file.rs` - is therefore untested and the
test's name advertises it. The ACL half of the test is real.

Enforcement rule named: split the ownership case into a test that is
`#[ignore]`d with a stated reason when not root, so the suite stops reporting it
as covered, or drive `write_config_temporary` with injected metadata instead of
real `fchown`.

## HYGG-010 - Tests that take their inputs from the developer's directory layout

`shepr-agent/src/agent/resume.rs` tests build paths from
`std::env::current_dir()` (`absolute_test_path`). The test only needs "some
absolute path", so this is an ambient dependency taken for convenience; a fixed
absolute literal or a `ScratchDir` would be hermetic.

`shepr-mux`: `pane/runtime.rs`, eight sites in `persist/restore.rs`, and
`workspace.rs`'s `test_adversarial_identity_state` use
`std::env::current_dir()` as a test cwd, so the tests depend on where the runner
was invoked.

`shepr-server/src/app/snapshot_tests.rs` reads the real `$HOME` with no
`IsolatedEnv`:

```rust
cwd: PathBuf::from("/tmp/this-directory-does-not-exist-for-shepr-test"),
...
cwd: std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/tmp")),
```

The test means "one pane with a missing cwd, one with an existing cwd" and gets
the second from the developer's `$HOME`; if `$HOME` is unset the fallback
silently changes the test's meaning. `ScratchDir` gives both cases
deterministically. The hunter notes this breaks two of the project's own rules
at once (tests touching the environment hold an `IsolatedEnv`; never read or
write from `/tmp`) - see HYGG-012.

`shepr-api`/CLI: `terminal_and_agent_attach_reject_invalid_config_before_connecting`
in `src/cli.rs` binds the env guard as `let _env = ...::IsolatedEnv::new();` and
then calls `_env.set(..)`. It works, but the underscore prefix conventionally
means "held only for Drop", so a reader deleting the apparently unused binding
breaks the test's isolation.

The `shepr-protocol`/`shepr-config` hunter reports the contrasting positive:
in that scope no environment-dependent tests were found, path-touching tests use
`IsolatedEnv` / `ScratchDir` as `AGENTS.md` requires, `AppPaths::default()`
deliberately points at `/nonexistent/shepr-test-config` so a slip fails loudly,
and `wire_tests::framing_over_unix_socketpair` uses an in-process socket pair
rather than the network.

Enforcement rule named: a brokkr text rule forbidding `"/tmp` literals and
`env::var("HOME")` in `crates/*/src` outside `shepr-test-support`.

## HYGG-011 - `clear_integration_path_env` is a hand-maintained duplicate of the agent env-var inventory

`shepr-agent/src/integration/tests.rs` lists fifteen variables to remove so
paths resolve against the fake `HOME`. `env.rs` defines fourteen `*_ENV_VAR`
constants plus the two XDG names. Add an agent env var and forget this list, and
every install test for that agent silently inherits the developer's real value:
the test passes on the author's machine, writes into the author's real agent
config, and means nothing. The hunter calls this the clearest example in the
crate of a test that depends on the environment it runs in.

Enforcement rule named: expose `pub(crate) const INTEGRATION_PATH_ENV_VARS:
&[&str]` in `env.rs`, have both the `resolve()`-adjacent code and the test
helper read it, and add a test asserting the list covers every variable the
resolvers consult - checkable by construction if the resolvers take their
variable name from the list. Related: HYGG-030 (the same shape one layer up, in
`IsolatedEnv`).

## HYGG-012 - Scratch directories and test fixtures live under `/tmp`, against a project rule

`shepr-test-support/src/lib.rs` builds its scratch root as
`std::env::temp_dir().join(format!("shepr-test-{pid}"))`, while `AGENTS.md` and
`CLAUDE.md` both state "Never read or write from `/tmp`. All data lives in the
project." The doc comment gives the reason - socket paths must fit `sun_path`,
and a deep checkout eats the budget; `target/` under this checkout is already
about 30 bytes deep before any scratch name - which the hunter accepts as a real
constraint. The finding is that this is a rule the build cannot enforce because
the code deliberately breaks it, and neither document records the exemption.
Either `AGENTS.md` should name the exemption or the scratch root should move
under a short symlink inside the project.

Related leak in the same place: `ScratchDir::keep_until_exit` relies on
`atexit`, so a test process killed with SIGKILL leaves the directory behind
indefinitely, and the only cleanup for a stale one is a later run reusing the
same pid - an unbounded `/tmp` growth path on a machine where tests get killed.
The fix named is a `shepr-test-*` sweep at scratch-root creation rather than an
exact-pid match.

Fixed `/tmp` paths inside test data elsewhere: `shepr-mux/src/persist/writer.rs`'s
`snapshot()` helper builds JSON containing
`"identity_cwd": "/tmp/shepr-writer-test"`, and
`src/workspace/aggregate.rs`'s tests use `"/tmp".into()` as a `TerminalState`
cwd. Nothing is written there, so the rule is not broken in effect - but these
are fixed shared paths standing in for a scratch directory, and if validation is
ever added at the snapshot boundary they become load-bearing on `/tmp` existing
and being a directory. `shepr-server/src/server/socket_paths.rs` also uses
`/tmp/...` path strings, only as env values never touched on disk - harmless,
though it defeats the same grep. `shepr-server/src/app/snapshot_tests.rs` has a
real one (HYGG-010).

Enforcement rules named: a gremlin-style text rule forbidding `temp_dir`
outside `shepr-test-support` plus a documented exemption; a text rule against
`/tmp` literals in `shepr-mux` and in `crates/*/src` generally.

## HYGG-013 - `capture_bounded_migration_observations` cannot fail

`shepr-mux/src/pane/terminal/migration_tests.rs`, reported by both the
`shepr-mux` and the `shepr-vt`/`shepr-pty` hunters. The test writes mixed
input, resizes through three geometries, writes control sequences, scrolls and
resets, pushing an observation after each step - and its only assertion is
`assert_eq!(observations.last().expect(...), &terminal.observe())`, comparing
the last observation against observing the same unchanged terminal again. Both
sides come from the same place; it passes for any behaviour the emulator could
have. It reads as coverage of eleven semantic dimensions across four geometries
and asserts none of them.

Its real purpose - dumping to `SHEPR_MIGRATION_OBSERVATIONS` for a human to
diff two builds - belongs to a finished migration: there is no "old" build in
this repository, no committed fixture to compare against, and the env var has
one writer and no reader. The file header comment ("Keep the same runner for
old/candidate captures") describes a workflow that cannot be performed.

Enforcement named: either a committed golden fixture the test compares against,
or deletion of the test and the env var, holdable by a text rule against
`SHEPR_MIGRATION_OBSERVATIONS`. The `shepr-mux` hunter is explicit that the
file's other six tests are real and should stay.

## HYGG-014 - `primary_screen_replay_honors_ed3_for_droid_at_chunk_boundaries` has a setup step that does nothing

`shepr-mux/src/pane/terminal/migration_tests.rs`. The test spawns host `bash`
and polls `/proc` for a process named "droid" so it can pass a real pid, but
`GhosttyPaneTerminal::process_pty_bytes` ignores `_shell_pid`. The setup does
nothing: the test is pure environment dependence, as its own comment about "the
former process-specific filter" admits.

## HYGG-015 - `child_sees_resolved_shell_not_a_non_executable_shell_env` compares against the function under test

`shepr-pty/src/command.rs`. The expected value is computed by
`cmd.resolve_shell(...)`, which is the same function the test exercises, so only
its `assert_ne` does any work.

## HYGG-016 - The login-shell tests never check the `-sh` argv0 that makes a shell a login shell

`login_shell_execs_shell_env_without_arguments` in `shepr-pty`, and the
`shepr-mux` twin `login_shell_builder_uses_one_resolved_path...`.
`std::process::Command` does not expose `arg0`, so only a spawn test can check
it - which means the property the tests are named for is unverified.

## HYGG-017 - `pane_terminal_identity_allows_explicit_override` applies the override by a path production does not use

`shepr-mux`. The test applies the override with a raw `cmd.env` after
`apply_pane_terminal_env`, while the production override path is
`PaneLaunchEnv::extra` in `apply_pane_launch_env`. The test therefore cannot
fail.

## HYGG-018 - Two pane-terminal-identity tests restate the values and the list they are checking

`pane_terminal_identity_removes_outer_terminal_identity` restates the production
scrub list verbatim, so a key added to production is not tested. Enforcement
named: export the list and iterate it.

`pane_terminal_identity_overrides_outer_terminal_env` in
`shepr-mux/src/pane/runtime.rs` hard-codes `"xterm-256color\ntruecolor\n"`
instead of reading `PANE_TERM` and the colorterm constant.

## HYGG-019 - The PTY actor's tests use `UnixStream` socket pairs, so PTY-specific behaviour is never exercised

`shepr-pty`. EIO on slave close, POLLHUP semantics and TIOCSWINSZ are never
reached. The tests accept `BrokenPipe | ConnectionReset | WriteZero`, but a real
PTY master reports EIO. The hunter's fix: one end-to-end actor-on-`openpty`
test would close the gap.

## HYGG-020 - `MAX_CLIPBOARD_TEXT_BYTES` is restated as a magic number in its own test

`shepr-platform/src/clipboard.rs` declares
`const MAX_CLIPBOARD_TEXT_BYTES: usize = 1024 * 1024` inside a function body;
`tests.rs` asserts the limit with `yes x | head -c 1048578`. Change the constant
and the test still passes while testing nothing in particular. Enforcement
named: hoist the const to module scope and have the test compute
`MAX_CLIPBOARD_TEXT_BYTES + 2`.

## HYGG-021 - Four small `shepr-platform` assertions that cannot fail, or that hide a failure

- `logging.rs`: `assert!(!summary.contains("/shepr.log"))` - the asserted string
  is built by a `format!` that cannot produce it.
- `geometry.rs`: `assert!(size_of::<PaneGeometry>() <= size_of::<(u16, u16,
  u32, u32)>())` asserts a property of the compiler's layout choices, not of
  this code; it passes for any plausible field arrangement.
- `tests.rs::session_members_are_withheld_...` calls
  `member.signal(Signal::Kill)` and drops the `bool` in cleanup, so a failure to
  clean up the background `sleep 30` is invisible and the process leaks past the
  test.
- `ssh_agent.rs` asserts both `!stable.exists()` and
  `symlink_metadata(&stable).is_ok()` - correct and deliberate (a dangling
  symlink), but reads as a contradiction without the comment it does not have.

Enforcement: review only, per the hunter.

## HYGG-022 - `received_geometry_rejects_pixel_mouse_without_cells` runs under a format production never uses

`shepr-protocol/src/geometry.rs`. The test exercises `TerminalGeometry`'s
`try_from` guard through `serde_json::from_value`, but the type crosses the
shepr codec in production. The guard is format-independent so the test is not
wrong, but it proves the rejection for JSON only - and `serde_json` is a
dev-dependency of `shepr-protocol` used for nothing else except one line in
`wire_tests.rs`. Enforcement named: run it through `codec::from_slice_exact` and
drop the dev-dependency, after which `brokkr.toml`'s dependency rules can keep
it out.

## HYGG-023 - The only codec exercise of `ValidatedConfig` covers the empty case of every interesting field

`codec::to_vec(&ValidatedConfig::test_default())` in `shepr-client`
(`shell/tests/mod.rs`, `shell_runtime.rs`, `endpoint/activation_tests.rs`) is
the whole of it. `test_default()` has empty `tab_bar_right`, default sidebar
rows, no styled tokens, no rules and no `rows_by_agent`, so every interesting
branch of `shepr-config/src/wire.rs` - `WireTabBarRightEntry::Command`,
`WireAgentSidebarToken::Styled` with rules, `rows_by_agent` - is encoded by no
test. It reads as coverage of "config is codec-safe". `shepr-config` cannot test
this itself: it does not depend on `shepr-protocol`, correctly, by the layering
in `brokkr.toml`.

Enforcement named: one test in `shepr-client` (or a maximal fixture in
`shepr-test-support`) that round-trips a config exercising every `wire.rs`
variant and asserts equality. The hunter calls this the single highest-value
missing test in that scope, and the data half of HYGG-067.

## HYGG-024 - `ServerAddress`'s test fixture violates the invariant its own type enforces

`shepr-config/src/address.rs`: the `#[cfg(any(test, feature = "test-support"))]`
`Default` builds `api_socket: "shepr.sock"`, `client_socket:
"shepr-client.sock"` - relative paths, which `validate_paths` rejects and which
`Deserialize` would refuse. Every test using `ServerAddress::default()` is
therefore asserting against a value production cannot produce. Enforcement
named: route the fixture through `ServerAddress::resolve_paths` with an absolute
root, as `AppPaths::test_with_context` already does, or have `Default` call
`validate_paths().expect(...)` so the fixture cannot drift.

## HYGG-025 - `PublicTabId`/`PublicPaneId`'s test-support `From<&str>` silently produces nonsense

`shepr-protocol/src/ids.rs`:
`value.parse().unwrap_or_else(|_| Self { workspace_id: "", number: 0, encoded: value })`.
A typo'd id in a test becomes a valid-looking `PublicPaneId` with workspace `""`
and number 0 rather than a failure, and equality against `&str` still passes
because it compares `encoded`. Any test built on a malformed literal quietly
tests nothing. Enforcement named: make the helper panic on a parse failure - a
one-line change the compiler cannot express.

## HYGG-026 - A constant-pattern match that would silently become a catch-all on rename

`shepr-config/src/tab_bar.rs::tab_bar_entries_parse_with_command_defaults`:

```rust
assert!(matches!(&parsed.entries[4], TabBarRightEntryConfig::Command {
    interval_seconds: DEFAULT_TAB_BAR_COMMAND_INTERVAL_SECONDS,
    timeout_seconds: DEFAULT_TAB_BAR_COMMAND_TIMEOUT_SECONDS, .. }));
```

This works today because both names resolve to `const`s, so they are constant
patterns. Rename either to lower case, or move it out of scope, and each becomes
a fresh binding that matches anything - the assertion then passes for every
value and rustc warns about nothing useful. Enforcement named: `assert_eq!` on
the destructured fields instead, which cannot degrade.

## HYGG-027 - `every_minimum_agent_version_parses` asserts over a single hard-coded target

`shepr-agent`. It takes `IntegrationTarget::Kimi`, calls
`agent_version_requirement`, and checks the requirement parses. Since Kimi is
the only target with a requirement the test cannot fail for any other target and
will not start covering a second one when it is added.
`agent_version_requirement_only_set_for_kimi` beside it pins the "only Kimi"
fact, so the pair is coherent, but the parse test should iterate
`IntegrationTarget::all()`.

## HYGG-028 - Two agent-table tests have the pinning length assertion and two next to them do not

`every_agent_has_a_canonical_interactive_executable` restates a 24-entry literal
list of `(Agent, executable)` copied from `AGENTS`, and
`assert_eq!(expected.len(), Agent::all().len())` forces it to be extended, so it
is a real pin - a deliberate second copy that catches accidental edits. The
hunter says keep it. It is recorded here because `identify_known_agents` and
`parse_known_agent_labels` beside it are the same shape *without* the length
assertion, so they can silently stop covering new agents.

Related positive, recorded so it is not re-hunted:
`agent/mod.rs::descriptors_are_the_domain_source_for_agent_views` pins the
`#[repr(usize)]` index into `AGENTS`, and
`manifest/tests.rs::all_bundled_manifests_parse_validate_and_compile` pins
`AgentDescriptor::screen_manifest` against what the registry actually loads -
both are the right kind of enforcement and the hunter marks the second a
non-finding with an answer: the test is the enforcement, keep it. The residual
note is that `BUNDLED_MANIFESTS` is a third list keyed by label string
(`("agy", include_str!("manifests/antigravity.toml"))`), so a label rename
breaks the join - and the existing test catches that.

## HYGG-029 - `test_support.rs::symlink_file` returns `true` unconditionally

`shepr-agent`. The helper `expect("create symlink")`s and then returns `true`,
so the return value cannot be false; call sites presumably
`assert!(symlink_file(...))`, which asserts nothing.

## HYGG-030 - The three bun test files never run

`shepr-agent/src/integration/assets/shepr-agent-state.test.ts`,
`assets/opencode/shepr-agent-state.test.ts` and
`assets/opencode/shepr-tui-session.test.ts` import `bun:test`. There is no
`package.json`, no bun or vitest config, and `brokkr.toml` runs cargo only. They
read as coverage for the JavaScript and TypeScript hook assets (the Pi, OMP,
opencode and Kilo integrations) and provide none. They also write sockets into
the system temp directory and mutate `process.env` globally. `notes/todo.md` has
an open item ("Resolve typescript question"), so this is known, but the files sit
beside code as if live. The hunter's position: either wire a bun step into
`brokkr check` or delete them; a test that cannot run is worse than no test.

## HYGG-031 - `src/netside_tests.rs` has four unbounded `recv()` loops that hang instead of failing

Around the source-release ack, the presentation-sync ack, the sync snapshot, the
presentation-effects fence, and the final returning-activation loop, each
`loop { ... control.recv().expect(..) ... }` with `continue` arms and no
deadline. If the expected message never arrives the test blocks forever rather
than failing, and `brokkr check` has no per-test timeout to rescue it. The
contrast named is
`crates/shepr-api/src/server/subscription_socket_tests.rs`, which defines
`RESPONSE_TIMEOUT` and threads a deadline through every read.

Enforcement named: a deadline helper plus a text rule banning bare `.recv()` in
tests in favour of `recv_timeout`.

## HYGG-032 - One `netside_tests.rs` assertion discards the result it exists to check

`returning.receive_response(&target_id, 7, &request_id, &data, &mut endpoints);`
is a bare statement, while every other `receive_response` call in the test is
wrapped in `assert_eq!` against a `SurfaceActivationProgress`. The
returning-activation half of the test therefore asserts nothing about the
response it just fed in, and reads as coverage.

## HYGG-033 - `machine_commands_reject_real_local_commands` enumerates commands by hand

`src/cli/target.rs` lists 12 denied and 8 allowed invocations. It is a good test
of the ones listed; nothing makes a newly added subcommand appear in either
list. `every_cli_spec_root_has_typed_parser`, which does cross-check the spec
against the sample list and is named as the excellent enforceable example, only
covers command *groups*, not subcommands.

Enforcement named: walk the spec's full subcommand tree
(`collect_subcommand_paths` already exists in `src/cli/spec.rs`'s tests) and
assert every leaf has an explicit machine-allowed classification, turning an
enumeration into a rule. The same walk would make `matches::required`'s
`String::default()` fallback unreachable in fact as well as in intent
(HYGG-074).

## HYGG-034 - `EventHub::events_after` cannot report what its production sibling reports

`crates/shepr-api/src/event_hub.rs`. The test-support method returns
`Vec::new()` on a poisoned lock and has no `Lost` signal, while
`events_after_checked` distinguishes both. Eleven call sites in `shepr-server`
tests use it (for example `app/api/panes/tests.rs`,
`assert!(app.event_hub.events_after(0).is_empty())`), and an assertion that a
history is empty cannot distinguish "no events were emitted" from "the lock is
poisoned" - a test that can pass for the wrong reason. Enforcement named: delete
`events_after` and have tests use `events_after_checked(..).expect(..)`.

## HYGG-035 - `status_exposes_only_dynamic_server_capabilities` asserts the absence of fields no type has

`src/cli/status.rs`:
`assert!(value["capabilities"].get("surface_interest").is_none())` and
`..get("health_check").is_none()`. `ServerCapabilitiesJson` has exactly two
fields, so both assertions are true for any possible value of the struct - they
cannot fail. They read as a guard against re-adding removed capabilities, but
nothing connects them to that intent; the real guard is the struct definition.

## HYGG-036 - `random_nested_message_comes_from_known_set` asserts a tautology

`src/main.rs`. `random_nested_message()` returns `NESTED_SHEPR_MESSAGES[index]`
where `index` is `% len`, and the test asserts the result is in
`NESTED_SHEPR_MESSAGES`. The neighbouring
`nested_message_strings_no_longer_repeat_shepr_prefix` can fail and is a real,
if tiny, guard.

## HYGG-037 - `machine_session_attach_is_rejected_as_a_tui_launch` names a situation it does not exercise

`src/cli/target.rs`. The test parses `--machine mac session attach work`, asserts
it became a `Tui` launch, then calls `run_on_machine("mac", None, ..)` -
constructing the `None` by hand rather than deriving it from the invocation it
just parsed. The assertion that a TUI launch yields no command for
`run_on_machine` is therefore made by the test, not by the code. It is the same
setup as `machine_prefix_rejects_missing_target_and_conflicting_global_options`,
duplicated.

## HYGG-038 - `managed_ssh_config_includes_user_config_then_fallback` never runs its headline assertion

`shepr-remote/src/remote/attach.rs`:

```rust
if let Some(home) = paths.home_dir() {
    let user_config = home.join(".ssh").join("config");
    if user_config.is_file() { ...assert include_at < fallback_at... }
}
```

`paths` comes from `test_app_paths()`, which is
`AppPaths::test_with_context(&root, Some(&root), None)` where `root` is a fresh
`ScratchDir`. A fresh scratch directory never contains `.ssh/config`, so the
inner block is dead in every run. The test's name and its comment ("any user
config is Included (quoted) BEFORE it so first-value-wins keeps the user's own
settings") describe behaviour the test does not check. The hunter calls this the
"worse than no test" shape, because it reads as coverage for the one ordering
rule OpenSSH's first-value-wins semantics depend on, and marks the test's own
claim false today. Fix named: write a `config` file into the scratch home and
assert unconditionally, i.e. remove the `if`. Interacts with HYGG-052.

## HYGG-039 - `remote_executable_accepts_only_cacheable_absolute_paths` has no accepting case

`shepr-remote`. Every row of the table has `valid == false`
(`"/home/a b/shepr"`, `"$HOME/.local/bin/shepr"`, `".../mise/shims/shepr"`,
`"/bin/shepr\nmalformed"`), so `RemoteExecutable::parse` could `return Err(...)`
unconditionally and the test would pass, despite its name claiming it checks
what is accepted. A valid path is exercised incidentally elsewhere (the
`ssh_metadata.rs` tests and `attach.rs`), so the behaviour is covered - but not
by the test named for it, and the `valid` column is dead weight that reads as if
both directions were covered. Fix named: add `("/usr/bin/shepr", true)` and
friends.

## HYGG-040 - Three names for one remote-locate call make a test assert nothing about their agreement

`shepr-remote`: `locate_remote_shepr`, `prepare_remote_shepr` (wrapping it in a
one-field `PreparedRemoteShepr`) and `find_installed_remote_shepr` (identical
body to `locate_remote_shepr`) are the same call. `discovery_tests.rs` exercises
`DiscoveryProgress` directly, so nothing tests that the three entry points
agree - they agree by being copies.

## HYGG-041 - `attach.rs` is a 1112-line test file named after a subject it does not contain

`shepr-remote/src/lib.rs` declares
`#[cfg(test)] #[path = "remote/attach.rs"] mod attach;`. There is no attach
code; the file is `mod tests { ... }` holding tests for the bridge, the managed
ssh config, the teardown registry, the process pipes, path sanitising, the
output framing, the reattach command and remote discovery. Anyone looking for
attach logic reads a test file; anyone changing `bridge.rs` does not think to
look in `attach.rs`. The project's own convention is that unit tests live next
to the code, and `discovery_tests.rs` in the same directory already shows the
correct naming, so the crate contradicts itself.

Enforcement named: a brokkr rule that a `#[path]`-included module file matching
`*_tests.rs` is allowed and anything else must be non-test, or simply that
`mod X` where `X.rs` contains only `mod tests` is an error.

## HYGG-042 - `libc` is a normal dependency of `shepr-remote` used only by tests

`crates/shepr-remote/Cargo.toml` lists `libc` under `[dependencies]`; the only
uses are `fcntl` in `attach.rs` and `geteuid` in `local_server.rs`, both inside
`#[cfg(test)]` modules. `brokkr.toml`'s `shepr-remote-layer` rule allows `libc`
for `kinds = ["normal"]`, so the allowlist currently blesses a dependency
production code does not use. Fix named: move it to `[dev-dependencies]` and
drop `libc` from the allowlist, at which point the dependency rule that already
exists enforces it - a one-line tightening of a check somebody already paid for.
The hunter flags this as one of the two findings it would most want confirmed by
actually building.

## HYGG-043 - Tests that clean up by hand, so a failing assertion leaks the resource

`shepr-mux/src/git/test_support.rs::temp_test_dir` returns
`ScratchDir::new(name).keep_until_exit()` and the doc says callers that clean up
remove it themselves. Callers then do
`std::fs::remove_dir_all(base).expect("test precondition")` *after* their
assertions (`git/status.rs`, two sites in `workspace.rs`). Any failing assertion
skips the cleanup, and `keep_until_exit()` is being used to opt out of the
crate's own RAII scratch directory for no stated reason.

`shepr-client`'s `handshake.rs::socket_pair` does the same: a `ScratchDir` with
`.keep_until_exit()`, and each test removes the socket file by hand with
`let _ = std::fs::remove_file(path)` after `peer.join()`. A test that panics
before that line leaves the socket behind.

Enforcement named: hold the `ScratchDir` guard in a binding, delete
`keep_until_exit()` and every manual removal, and hold it with a text rule
against `remove_dir_all` in test modules.

## HYGG-044 - The gate may never compile the feature set that ships

Reported from three scopes, with the same mechanism: a package that appears in
`[dependencies]` without features and in `[dev-dependencies]` with a test
feature gets that feature unified on for any `--all-targets` build, which is
what a clippy-plus-tests gate runs.

`shepr-mux` / `shepr-server`: root `Cargo.toml` has `shepr-server` plain and
with `features = ["test-api"]` as a dev-dependency, and `shepr-server`'s
`test-api` pulls in `shepr-mux/test-api`. So `brokkr check` builds both crates
with `test-api` on, and the configuration `brokkr install` ships - `test-api`
off - is compiled by no gate step. A `#[cfg(not(feature = "test-api"))]` path,
or code that accidentally depends on a test-only item, would not be caught. This
matters more than usual because `test-api` is not cosmetic in `shepr-mux`: it
adds a whole variant to a production enum (`PaneRuntimeIo::TestChannel`), plus
`Workspace::clear_tabs_for_test`, `PaneRuntimeRegistry::drain`,
`TerminalState::set_detected_state` and nine `PaneRuntime::test_*`
constructors. The same hunter notes it also means the production shape of
`src/git/status.rs` (whose `GitStatusRefreshDemand::ALL` and two helpers are
`cfg`-gated) is never compiled by the gate.

`shepr-client`: root `Cargo.toml` depends on it plainly and as a dev-dependency
with `features = ["test-support"]`, so under `cargo test` /
`cargo build --tests` items gated `#[cfg(any(test, feature = "test-support"))]` -
including `ClientState::test_new()`, the activation test hooks and the shell
hooks in `endpoints.rs` - are reachable from `shepr-client`'s own production
modules in that build. Nothing prevents a production code path from calling
them; only the fact that none does today. The hunter marks the vt/pty sibling of
this (the root `Cargo.toml` enabling `shepr-server/test-api`) as inference, not
verified.

Enforcement named: add a `[[check]]` entry to `brokkr.toml` that builds the
workspace with default features and without `--all-targets`, so the shipped
feature set is compiled by the gate. The `shepr-client` hunter adds that real
isolation means moving the helpers into a separate crate, the
`shepr-test-support` pattern the workspace already uses, so the production
module cannot name them.

## HYGG-045 - `advertised_client_shell_methods_all_exist` cannot fail for most breakages

`shepr-server/src/server/client_commands.rs`:

```rust
let method = serde_json::json!({ "method": name, "params": {} });
if let Err(error) = serde_json::from_value::<Method>(method) {
    assert!(!error.to_string().contains("unknown variant"), ...);
}
```

The test passes whenever the error message does not contain the exact substring
`"unknown variant"`. Serde changing its wording, a `#[serde(tag)]` change, or a
params error arriving before the tag is resolved all turn this into a no-op that
still reads as coverage of the 26-entry method list - and it is the only thing
holding that list to the schema (HYGG-095).

Enforcement named: have `shepr-api` expose the set of method names
(`Method::ALL_NAMES` or an iterator over the schema table) and assert
`CLIENT_SHELL_METHODS` is a subset by set membership, with no string matching.

## HYGG-046 - The `clamp_terminal_size` tests assert against the constant they are testing

`shepr-server/src/server/client_transport.rs`:
`assert_eq!(clamp_terminal_size(MIN_CLIENT_COLS, MIN_CLIENT_ROWS),
(MIN_CLIENT_COLS, MIN_CLIENT_ROWS))`. Both sides come from the same constant, so
the assertion holds for any value as long as the clamp uses it as its lower
bound; it cannot detect a wrong minimum. The neighbouring
`assert!(cols >= MIN_CLIENT_COLS && rows >= MIN_CLIENT_ROWS)` is near-vacuous
with `MIN_* = 1`: a `u16` fails it only at zero. Fix named: assert the literal
`(1, 1)` and add a case asserting `clamp_terminal_size(0, 0) == (1, 1)`, which
is the behaviour actually at stake.

## HYGG-047 - Two `#[cfg(test)]` shortcuts make every agent-hosting assertion unfalsifiable

`shepr-server/src/app/agents.rs`:

```rust
fn available_shell_name(runtime: &PaneRuntime) -> Option<String> {
    #[cfg(test)]
    if runtime.child_pid().is_none() { return Some("sh".into()); }
    ...
}
pub(super) fn runtime_hosts_agent(runtime: &PaneRuntime, expected: Agent) -> bool {
    #[cfg(test)]
    if runtime.child_pid().is_none() { return true; }
    ...
}
```

Any test using a `PaneRuntime` without a live child - which is most of them,
including every `PaneRuntime::test_with_screen_bytes` fixture - gets
`runtime_hosts_agent == true` for every agent, so assertions that a pane hosts
the expected agent cannot fail. The shortcut is gated on `cfg(test)` only while
the rest of the crate gates test affordances on
`any(test, feature = "test-api")`, so the root binary's integration tests see
the production path and unit tests see the shortcut - the two suites test
different code.

Enforcement named: inject the probe (a `ProcessProbe` trait, or an `Option<fn>`
on the runtime) rather than branching on `cfg(test)`, so a test that wants
"hosts the agent" must say so.

## HYGG-048 - `#[cfg(test)]` changes where client frames go, so no test covers the production writer

`shepr-client/src/state.rs::try_present_frame` picks its sink by `cfg`
(`io::stdout()` in production, `io::sink()` under test) with a comment
explaining that a full-screen frame written to the test runner's real stdout
would scribble on the developer's terminal. The consequence for question 5: the
whole presentation test surface - `shell/tests/copy.rs` (2492 lines),
`mouse_selection.rs` (1312), `endpoints.rs` (1934) - runs against `io::sink()`
for full frames, no test asserts anything about what the production sink
receives, and the sibling `present_surface_patch` path is not even the same code
(it writes `io::stdout()` unconditionally, which the hunter files separately as
a live defect).

Enforcement named: an injected writer on `ClientState`, so the tests assert on a
`Vec<u8>` and run the same code production runs, and the `#[cfg(test)]`
divergence between what tests exercise and what production runs disappears. The
same hunter notes there is no owner of host-terminal output at all, which it
files under its question-3 section.

## HYGG-049 - `an_attempt_deadline_caps_a_silent_peer_below_the_read_timeout` asserts against a quarter of a 60-second constant

`shepr-client/src/handshake.rs`. The test sets a 200 ms deadline and asserts
`elapsed < REMOTE_HANDSHAKE_READ_TIMEOUT / 4`, i.e. under 15 seconds, so a
regression that made the deadline 10 seconds late passes. The assertion should
be against the deadline it set, not a fraction of the value it is trying to
prove is not used. It also binds two real sockets and a thread, so it is
environment-coupled in the mild sense.

## HYGG-050 - `should_enable_host_color_scheme_reports` is an identity function re-exported for tests

`shepr-client`:

```rust
pub(super) fn should_enable_host_color_scheme_reports(enable_client_protocols: bool) -> bool {
    enable_client_protocols
}
```

It is `#[cfg(test)]`-imported in `lib.rs` alongside real helpers, so any test
asserting on it asserts `x == x` - both sides come from the same place. The
function exists so a rule *could* live there; today it holds no rule. A test
cannot enforce this; only deletion, or giving it the rule it was created to
hold.

## HYGG-051 - `help_lists_every_default_pane_binding` names five entries out of roughly seventy

`shepr-client/src/keybind_help.rs`. The name claims "every default pane
binding"; the body checks `copy mode` plus the four `swap pane` directions. It
reads as coverage of the whole help screen and is coverage of five rows. The
hunter's position is that the fix is not to widen this test but to make the help
list structural - see HYGG-070.

## HYGG-052 - A test comment cites an unresolvable issue number and a second-precise timestamp

`shepr-client/src/input/raw_input.rs`: `// Issue #3911, 2026-09-13 07:02:14
UTC: this prefix timed out, then its tail arrived 33 ms later.` Nobody working
in this repository can look up issue #3911 (shepr is a personal fork with no
tracker), and the timestamp is precise to the second for no purpose. The
behavioural content - a prefix, a 33 ms gap, two idle flushes - is the valuable
part and survives without either citation. Per the documentation rule in
`AGENTS.md`, the drifting specific should be reworded away rather than updated.
The hunter says no text rule is worth writing for this (`#\d+` in comments would
be over-broad).

## HYGG-053 - `IsolatedEnv` guarantees isolation from a list it does not own

`shepr-test-support`'s doc comment claims the guard means "nothing under test
can reach the user's real config, state or agent directories, or the live shepr
server a test run was started from". That guarantee rests on three name lists
inside the crate: `XDG_BASE_DIR_VARS` (four names), the `SHEPR_` prefix, and
`HOME`/`XDG_RUNTIME_DIR`. `shepr-config/src/io.rs` reads `XDG_CONFIG_HOME`,
`XDG_STATE_HOME`, `XDG_RUNTIME_DIR`, `HOME`, `SHEPR_CONFIG_PATH`,
`SHEPR_SOCKET_PATH`, `SHEPR_CLIENT_SOCKET_PATH` and `SHEPR_SESSION`;
`shepr-agent/src/integration/env.rs` reads a per-agent set of `*_HOME`-style
variables (`GROK_HOME` among them, per its own tests). The day a variable is
added on the reading side and not here, every test keeps passing while reaching
into the developer's real `$HOME`-adjacent state, and reports nothing.

The `shepr-protocol`/`shepr-config` hunter reports the same thing from the other
end and adds that the duplication is *forced* by the layering:
`shepr-test-support`'s dependency allowlist in `brokkr.toml` is `["libc"]`, so
it cannot import the names from anywhere - and calls for it to be reported as a
duplication with a reason rather than dismissed. Its proposed fixes: either
widen the allowlist to `shepr-core` and export the name set from there, or add a
test in `shepr-config` asserting every variable it reads is in the isolation
list.

The `shepr-platform` hunter calls this the highest-value mechanical fix in its
report: if every environment variable name lives in one `shepr-core::env`
module, `IsolatedEnv` can iterate that module's full list instead of restating a
subset, and a `brokkr.toml` text rule forbidding env-name literals elsewhere
keeps the list complete by construction. Related: HYGG-011 (the same shape
inside `shepr-agent`'s own test helper) and HYGG-054.

## HYGG-054 - `app_dir_name()`'s `cfg!(test)` guard is false outside its own crate

`shepr-config/src/io.rs`:

```rust
// Unit tests get a directory name of their own in every profile. ...
if cfg!(test) { "shepr-test" } else if cfg!(debug_assertions) { "shepr-dev" } else { "shepr" }
```

`cfg!(test)` is per-crate: it is true only while compiling `shepr-config`'s own
unit tests. A test in `shepr-server`, `shepr-client` or `shepr-remote` that
reaches `AppPaths::resolve()` compiles `shepr-config` as a normal dependency,
and `brokkr test` builds release by default so `debug_assertions` is off too -
the directory name is `shepr`, the real `~/.config/shepr`. The comment says this
slip is kept out of both the release and dev directory; for the majority of the
workspace's tests it is not. What actually prevents damage is `IsolatedEnv`
pointing `HOME`/`XDG_*` at scratch, i.e. discipline, not the `cfg!`. The hunter
marks this **the most dangerous false claim it found**.

Enforcement named: make the guard positive rather than negative - have
`shepr-test-support` set a variable (for example `SHEPR_TEST_DIR_NAME`) that
`app_dir_name()` honours, so isolation is something a test opts into and the
name is wrong loudly rather than silently. Failing that, at minimum reword the
comment.

## HYGG-055 - `WSL_MARKER_ENV_VARS` is a two-entry list that goes quiet when the names change

`shepr-platform/src/host.rs`. When Microsoft renames or drops a variable the
check quietly stops contributing, and there is no log and no test that the list
is non-empty or current. Checkable only against a real WSL host; the hunter says
it is not false today as far as can be determined by reading.

## HYGG-056 - The clipboard detach behaviour hangs on the program name `wl-copy`

`shepr-platform/src/clipboard.rs`:
`clipboard_program_name(command.program) == "wl-copy"` is the entire trigger for
"detach the clipboard owner instead of waiting for it". Rename the helper, wrap
it, or point at `wl-copy-wrapper` - a case the test in `tests.rs` explicitly
demonstrates produces `"wl-copy-wrapper"` - and the write path silently reverts
to waiting for a process that never exits until the 2 s timeout kills it, taking
the user's clipboard content with it. The hunter marks it checkable and worth
fixing structurally: the "owns the selection after exit" property belongs on
`ClipboardCommand` as a field, not inferred from the program name. It also
lists this among its lateral live-defect findings, so a fix pass should expect
the bug document to carry an overlapping entry.

## HYGG-057 - `is_posix_acl_xattr` is keyed on the `system.posix_acl_` prefix

`shepr-platform/src/config_file.rs`. Correct today; a filesystem that expresses
access control under another prefix (a security label, richacl) falls into the
best-effort branch that ignores failures. The comment says as much, which the
hunter calls the honest version.

## HYGG-058 - The `SHEPR_` prefix scrub silently keeps a non-UTF-8 key

`shepr-test-support`:
`key.to_str().is_some_and(|k| k.starts_with("SHEPR_"))`. Not reachable in
practice, but it is the fail-open shape.

## HYGG-059 - `SHEPR_AGENT` guards are keyed on a name with no producer, and on a byte literal with the `=` baked in

`shepr-agent/src/agent/mod.rs` has `LAUNCH_ENV_TO_SCRUB = &["SHEPR_AGENT"]`,
`detect/proc_tree.rs::parse_agent_env_hint` matches the byte literal
`b"SHEPR_AGENT="` when reading `/proc/<pid>/environ`, and
`shepr-mux/src/pane/runtime.rs` passes `"SHEPR_AGENT"` when setting it. The
`shepr-agent` hunter's reading of the guard: `parse_agent_env_hint` fails open on
a renamed variable - the hint is simply never found, the pane falls back to
process-name detection, and nothing is logged.

The `shepr-vt`/`shepr-pty` hunter reads the same names differently and files
them as a guard keyed on a name nothing produces: "`SHEPR_AGENT` is scrubbed
from pane env (`LAUNCH_ENV_TO_SCRUB` in `shepr-agent`), but nothing in the repo
sets it. The guard is keyed on a name that nothing produces; it looks like a
herdr leftover." Both readings are recorded; they disagree about whether there
is a live setter.

Enforcement named by both: one shared `pub const`, with the probe deriving its
prefix from it, so a rename is a compile error.

## HYGG-060 - `is_pane_shell_process_name` fails open on a shell it does not know, and three shell-name lists have already diverged

`shepr-agent/src/detect/proc_tree.rs::is_pane_shell_process_name` knows twelve
shells (`sh bash dash zsh fish ksh mksh csh tcsh elvish xonsh nu`);
`detect/mod.rs::is_generic_runtime_or_shell` knows four plus `tmux node bun`;
`wrapped_agent_name_from_runtime_argv` matches four. The hunter states the
consequence as a fact, not a prediction: a pane shell that is `dash`, `nu`,
`ksh` or `xonsh` running an agent through `-c` is not unwrapped, so the agent is
not identified, so there is no detection for that pane.

Separately as a fail-open guard: any shell outside the twelve-name list, or a
wrapper like `nix-shell`, `toolbox`, or a user's `$SHELL` symlink named
something else, is treated as not-a-pane-shell, which changes
`available_pane_shell` and the whole child-groups path, reporting nothing.
Checkable against the configured default shell: the config already knows the
user's shell, so a launch-time check ("your configured shell is not one shepr
recognises as a pane shell") would turn a silent no-op into a warning at boot.
The structural fix named is one `ShellKind` table with per-use predicates
(`is_pane_shell`, `supports_dash_c`) derived from it.

## HYGG-061 - `hook_registration_is_current` fails open for five targets and the coupling is invisible

`shepr-agent/src/integration/registry.rs`:
`Target::Pi | Omp | Kilo | Grok | Opencode => true` is documented for Pi, Omp
and Kilo (directory-loaded plugins) and for Grok and Opencode (checked by their
own helpers earlier in `integration_status_at`). The coupling is invisible from
either site: if the Grok special case above it were deleted, this `true` would
report a broken Grok install as Current with nothing noticing.

Enforcement named: make the spec row carry a `RegistrationCheck` variant
(`SelfRegistering | Json { file, root, depth } | Custom(fn)`) so the match is
exhaustive over data rather than over a target list.

## HYGG-062 - `json_hook_commands_registered` verifies installation by substring search

`shepr-agent`. Install writes an exact shape - four different shapes across
`ensure_command_hook`, `ensure_flat_command_hook`, `ensure_direct_command_hook`
and `ensure_simple_command_hook` - and status verifies by walking the event's
value recursively for a matching command string anywhere inside it
(`json_contains_string`). So a command string sitting in a disabled block, in a
comment-like field, or in an unrelated nested entry counts as registered.
Enforcement named: have status reuse the install shape
(`is_matching_command_hook` already exists for one of the four) instead of a
generic search.

## HYGG-063 - Ancestor depths in `hook_registration_is_current` mirror the install paths by hand

`shepr-agent`: `json_in(2, "settings.json", ...)` for Claude because the hook
lives at `<dir>/hooks/<name>`, `json_in(1, ...)` for Codex because it lives at
`<dir>/<name>`, `ancestor(hook_path, 3)` for Hermes. Those numbers are
`spec.path.len() + 1` and nothing says so. Change a spec path and status quietly
reports Outdated forever - the hook is fine, the check is looking in the wrong
directory - with no log line. Enforcement named: derive the depth from
`spec.path.len()`, or better, keep the config path on the spec row and stop
walking upward from the hook path.

## HYGG-064 - `ProcessDetectionMode::ChildGroups` is a claim nothing exercises

`shepr-agent`. No test sets `SHEPR_PROCESS_DETECTION=child-groups` end to end -
the two `*_with` tests call the inner function directly - and on Linux
`foreground_process_group_id` always succeeds when `/proc` is readable, so the
mode's trigger condition (native detection returning `None`) is rare to
nonexistent. The comment justifies the mode for "environments that do not expose
terminal foreground groups", but shepr is Linux-only and `/proc/<pid>/stat`
always exposes `tpgid`; the hunter suspects a WSL-era leftover and says: if it
is a WSL accommodation it should say so, and if not it is dead code. It also
notes the mode has no injection point at all - the only way to reach it is the
process-wide `OnceLock`, so no test can flip it without leaking into other tests
in the process, and the two `*_with` seams exist precisely because the mode
itself is untestable. The `shepr-platform` hunter's note that
`shepr_platform::running_inside_wsl()` still exists and is called from three
places argues for asking before deleting.

## HYGG-065 - Sidebar and theme guards keyed on strings before the strings become enums

`shepr-config/src/sidebar.rs::RawSidebarToken::parts` does
`matches!(token.token.as_str(), "state_icon" | "git_status")` to reject styling
rules on non-text tokens, *before* the string is parsed into an enum. Rename a
token and rules silently become accepted on a token whose value is a glyph. It
also conflates two namespaces: `git_status` does not exist as an agent token and
`state_icon` exists in both. Fix named: move the check after parsing and match
on the enum, so exhaustiveness holds it.

`sidebar.rs::parse_sidebar_token`'s built-in tables (nine agent entries, five
space entries) are slices, not matches. Add an `AgentSidebarToken` variant and
`agent_token_name` fails to compile - good - but the parse table does not, so
the new token silently becomes unparseable and is reported as "unknown sidebar
token". Fix named: one table used by both directions, or a `const fn all()`-style
enumeration with an exhaustive match.

`theme_config.rs::CustomThemeColors::parse` - the `color!()` list is tied to
`ParsedThemeColors` by a struct literal, so a field added to *both* is caught,
but a field added only to `CustomThemeColors` compiles and is silently ignored.
Fix named: generate both structs from one field list.

A bare `16` with a named twin, in the same file:
`RawSidebarToken::parts` has `if token.rules.len() > 16 { return Err("sidebar
tokens may contain at most 16 rules") }` - the number appears twice on adjacent
lines, in a file that already has two named `= 16` constants for neighbouring
limits.

## HYGG-066 - `ConfigProvenance::from_config` drops provenance keys silently for any `skip_serializing_if` field

`shepr-config/src/validated.rs` enumerates config keys by
`serde_json::to_value(config)` and walking the tree, so any
`skip_serializing_if` in a config type drops keys - and `RawRule` has ten of
them today, so sidebar-rule fields that are `None` never appear in
`config check`'s enumeration. The completeness of the provenance surface depends
on an attribute nobody audits. Enforcement named: the codec text rule of
HYGG-067, extended to `shepr-config`'s TOML-facing types with the `RawRule`
exemption spelled out and justified in a comment.

## HYGG-067 - Claim: "Wire types must not use `skip_serializing_if`, `flatten`, `untagged` or tagged enums"

`AGENTS.md` and the `shepr-protocol/src/codec.rs` module doc. The hunter's
verdict: **true today for the types that actually cross the codec, not enforced
by anything, and the runtime backstop is data-dependent.**

- Nothing in `brokkr.toml`, `clippy.toml` or the workspace lint table mentions
  these attributes.
- The runtime backstop is incomplete and, for one of the four, conditional:
  `codec.rs`'s own test asserts `to_vec(&Skipping { value: None })` yields
  `CodecError::SkippedField` but `to_vec(&Skipping { value: Some(1) })` succeeds
  and returns `[1, 1]`. So a wire type carrying `skip_serializing_if` encodes
  fine for every value where the predicate is false and fails only in
  production, on the first message where the field happens to be absent - a
  guard keyed on data, not on shape.
- There is no test that `flatten`, `untagged` or an internally or adjacently
  tagged enum is rejected. `unsupported_shapes_are_rejected` covers a manual
  `serialize_seq(None)` and `IgnoredAny`, not the four named shapes. In practice
  `flatten` dies as `CodecError::UnknownLength` and `untagged` as
  `NotSelfDescribing` on decode, but only if a test happens to exercise that
  message.
- `shepr-config` contains four shapes the codec cannot encode, all `pub` or
  reachable from `pub` types: `BindingConfig` (`#[serde(untagged)]`,
  `keybinds.rs`); `TabBarRightEntryConfig` (`#[serde(tag = "type", ...)]`, an
  internally tagged enum, `tab_bar.rs`); `RawRule` behind `SidebarTokenRule`'s
  `#[serde(try_from, into)]` with ten `skip_serializing_if =
  "Option::is_none"` fields (`sidebar/rules.rs`); and
  `AgentSidebarToken`/`SpaceSidebarToken`, whose hand-written `Serialize` uses
  `serializer.serialize_map(None)` in `serialize_styled_token`, i.e.
  `CodecError::UnknownLength` (`sidebar.rs`). None reach the codec solely
  because `shepr-config/src/wire.rs` hand-mirrors each one.
- But `WireConfig` *does* reuse raw config types unmirrored: `ThemeConfig`,
  `SessionConfig`, `ServerConfig`, `AdvancedConfig`, `RemoteConfig`,
  `SidebarCollapsedModeConfig`, `PaneBordersConfig`, `TabBarPositionConfig`,
  `StatusIndicatorStyle`, `AgentPanelSortConfig`, `ConfigAgent`,
  `ImeCursorShape`. So the boundary is not "config types never cross the wire";
  it is "these twelve do and those four do not", with no marker, trait or naming
  rule distinguishing them. Adding a `skip_serializing_if` to `ServerConfig` for
  nicer TOML output would compile, pass every existing test, and break attach at
  runtime.

Enforcement named, both called cheap: (1) a gremlin-style text rule in
`brokkr.toml` forbidding `skip_serializing_if`, `flatten`, `untagged` and
`serde(tag` under `crates/shepr-protocol/src`, excluding `#[cfg(test)]` - the one
legitimate occurrence is `codec.rs`'s `Skipping` fixture, so either exempt that
file or move the fixture; (2) a marker trait (`trait WireSafe {}`) implemented
only by mirrored types, with `WireConfig`'s fields bounded on it, so reusing a
TOML-facing type in `WireConfig` stops compiling. Neither is possible today. See
HYGG-023 for the data half.

## HYGG-068 - Claim: "Config is read and validated once at launch. No reload, no fallbacks. Any config problem fails the launch"

`AGENTS.md`. Five scopes report this claim as **partly false today**, at
different sites.

*Across a host boundary* (`shepr-protocol`/`shepr-config`): the local path
holds - `Config::load_validated` returns `Err` if `diagnostics` is non-empty,
`main.rs::load_validated_config_or_exit` exits 1, and
`bootstrap.rs::encode_resolved_config` propagates an encode failure with `?` so
the server refuses to boot. But `ValidatedConfig`'s `Deserialize` re-runs
`from_resolution(..., CwdCheck::Received)`, so a remote endpoint's config is
validated at the moment of use, on the client, at attach time - not at that
client's launch, and a config the server accepted can be rejected by the client.
That duplication is forced (two hosts, two binaries, one config travelling
between them) and what keeps the two validations in step is the exact-build
preamble plus the shared crate - which the hunter says is worth saying out loud
in the `AGENTS.md` sentence, currently written as absolute.

*XDG path variables* (`shepr-config/src/io.rs`): the empty/relative rule is
invented three times inside `resolve_paths_from_env` / `platform_xdg_dir` /
`socket_path_override`, and the five variables get four rules.
`XDG_CONFIG_HOME` and `XDG_STATE_HOME` treat empty and relative as unset and
fall back to `HOME`, silently, with provenance reporting `Default`;
`XDG_RUNTIME_DIR` is a hard error; `SHEPR_SOCKET_PATH` and
`SHEPR_CLIENT_SOCKET_PATH` produce a diagnostic; `SHEPR_CONFIG_PATH` produces a
diagnostic when empty and is *joined to the cwd* when relative; `SHEPR_SESSION`
is parsed. The one that fails open is also the one that silently sends config
reads somewhere other than where the user pointed them:
`XDG_CONFIG_HOME=relative/path` is a config problem that produces a silent
fallback. Enforcement named: one `env_path(variable, policy)` helper with an
explicit policy enum used for all of them, plus a test table over the five
variables pinning the matrix.

*`SHEPR_LOG`* (`shepr-platform/src/logging.rs`):
`EnvFilter::try_from_env("SHEPR_LOG").unwrap_or_else(|_| EnvFilter::new("shepr=info"))`.
A typo in the filter (`shepr=inof`) produces no diagnostic of any kind - the
`Err` is discarded and the default installed. The hunter calls this the one
config input in the project that contradicts the stated rule, and notes it is in
the module that owns diagnostics, so the failure cannot even be reported through
itself. The next line has the same shape: `let _ = tracing_subscriber::fmt()...
try_init();`. Enforcement named: return `Result` from `init_file_logging`, fail
the launch on a bad filter, and add `clippy::let_underscore_must_use` (or just
make the signature `-> io::Result<()>` so `unused_must_use` fires).

*`SHEPR_PROCESS_DETECTION`* (`shepr-agent/src/detect/proc_tree.rs`):
`process_detection_mode` reads the variable behind a `OnceLock` on the first
foreground probe, and an unrecognised value produces `tracing::warn!` and native
mode. It is config in all but name and breaks both halves of the rule: a typo is
discovered hours in, on whichever pane probed first, and is tolerated rather
than refused. It is also invisible to `shepr config check`. Enforcement named:
move it into the config file, or validate it during launch and pass the mode
down; `parse_process_detection_mode` is already the right shape.

*`SHEPR_DEBUG_OSC_EVIDENCE`* (`shepr-mux/src/pane/osc.rs`):
`impl Default for OscDebugTracker { fn default() -> Self { Self::from_env() } }`,
reached from `GhosttyPaneCore` construction, so the process environment is read
once per pane rather than once at startup. This knob reloads on every new pane,
and a typo (`SHEPR_DEBUG_OSC_EVIDENCEE=1`, or `=yes please`) is not a launch
failure but a silent no-op discovered hours later. It is also documented nowhere
at all - no `docs/`, no `reference/`, no `brokkr man` page - and the flag puts
pane content (OSC 0/2/9/21337 payloads, truncated to 512 chars) into the log at
`debug`, which nothing tells the user.

*`terminal.default_shell`* (`shepr-config/src/validated.rs`): stored as a raw
`String`, with PATH lookup and the executability check happening in
`PtyCommand::to_std_command` at every spawn, so a typo fails each pane rather
than the launch. `default.toml` says "Empty means $SHELL, then /bin/sh", but a
`$SHELL` that is set and invalid is an error in pane mode, not a fall-through to
`/bin/sh`. Enforcement named: a `ValidatedShell` produced at config load.

*Manifest overrides* (`shepr-agent/src/detect/manifest.rs`): an override that
does not compile is collapsed into a `warning: String` attached to the fallback
manifest and surfaces only through `explain`/`reload` summaries, so a bad
override at server boot is visible only if someone asks. Given the rule, the
hunter argues it ought to fail `shepr config check`. Enforcement named: a typed
`ManifestOverrideError` plus a `config check` path that loads overrides.

*Sidebar chrome preferences* (`shepr-client/src/shell/presentation/config.rs`):
`persist_chrome_preferences` writes preferences and, on failure, raises a UI
banner hours into a session on whatever gesture happened to trigger a persist.
The preferences path is `Option` and a `None` silently skips persistence
entirely. Nothing at launch checks the path is writable, so the first sidebar
drag of the session is where an unwritable state directory is discovered.
Enforcement named: a startup probe on the preferences path, checkable by a test
that launches with a read-only state dir.

The `shepr-protocol`/`shepr-config` hunter also records the contrasting
positive: everything else in that crate is genuinely front-loaded, including the
strftime compile (`parse_tab_bar_datetime_format`), the window-title template
parse and the keybind parse. The `shepr-server` hunter verified the same for its
scope and reports no finding there.

## HYGG-069 - Claim: every CLI subcommand acting on a running server goes through the JSON API, and local-state commands cannot be sent with `--machine`

`AGENTS.md` and `src/cli/`. The hunter's verdict: the `--machine` gate is real
and tested (`validate_machine_command` plus
`machine_commands_reject_real_local_commands`, see HYGG-033), and the API-only
half leaks in named places. `session stop`, `session delete` and local
`server stop` reach a running server without going through `ApiClient` at all -
`shepr-api/src/session.rs` hand-rolls a second client transport. `status`
(overview) and `status client` also act on a running server through the API
while declaring `is_api_command() == false`.

`status`'s refusal message for the second case is both wrong and malformed: with
`--machine` the user gets "`status ` is not an API-backed machine command",
with a dangling space, because `Command::Overview.name()` returns `""` - the
same value `Command::Invalid` returns. Two distinct states share one name
string, and the message asserts something untrue about the command. The
`""`-for-two-states pattern repeats in every `src/cli/*.rs` `name()` (agent,
pane, tab, workspace, machine, integration, server, session, terminal, config) -
ten copies.

`is_api_command` also has no single owner: `src/cli.rs::CliCommand::is_api_command`
hardcodes `false` for `Config`, `Machine`, `Session` and `Integration`, while
`Status`, `Server`, `Workspace`, `Tab`, `Pane`, `Agent` and `Terminal` delegate
to per-module methods. If an `integration` or `machine` subcommand ever becomes
API-backed, the blanket `false` silently blocks it with no test failing.

Enforcement named: the classification should be "may this run against a remote
machine", not "is this API-backed"; `name()` should not be able to return `""`
(make it `Option<&str>`, or give `Overview` the name `"status"` and drop the
`Invalid` variant in favour of `Result`/`Option` at parse time); and every
group's `Command` should implement one trait method, so a missing
implementation is a compile error.

## HYGG-070 - Claim: API error codes, response shapes and operator guidance each have one owner

`AGENTS.md`-adjacent framing checked by the `shepr-api`/CLI hunter, verdict
**false in several places.** Nine wire error codes are minted as string literals
outside `ApiErrorCode` (`agent_explain_file_read_failed`, `agent_start_failed`,
`agent_start_transport_failed`, `agent_kind_mismatch`, `agent_name_not_found`,
`build_mismatch`, `server_not_running`, `invalid_session_name`,
`session_stop_failed`, `session_delete_failed`), two codes that *are* in the
enum are spelled as literals at the site that emits them, the response-encoding
path has three independent implementations with different failure behaviour, and
operator guidance is assembled at three sites. The guard that should catch this
is HYGG-071.

## HYGG-071 - `error_response_json`'s `impl Into<ApiErrorCode>` is a check that fails open on a name

`crates/shepr-api/src/server.rs` passes `"invalid_ssh_agent"` /
`"ssh_agent_unavailable"` to `error_response_json`, whose signature is
`code: impl Into<ApiErrorCode>`, so a typo silently becomes
`External("invald_ssh_agent")` with no compile error and no log -
`ApiErrorCode::InvalidSshAgent` and `SshAgentUnavailable` both exist. The
`From<&str>` mapping of an unknown string to `External(s)` is correct for
parsing responses off the wire; it is the emit path that gets a silent
pass-through instead of a rejection. `src/cli/agent.rs` additionally emits the
literal `"timeout"` and `agent_not_ready`, duplicating `ApiErrorCode::Timeout`
and `ApiErrorCode::AgentNotReady`, and `api_response_outcome` re-parses the JSON
it just serialized and matches `"timeout"` literally - a third spelling.

Enforcement named: change `error_response_json` to take `ApiErrorCode` directly
and delete the `From<&str>`-driven coercion from that path; `From<&str>` is
needed for wire parsing but need not be reachable from an emit site. Weaker
fallback: a text rule that no string literal is assigned to an `ErrorBody.code`
field outside `error.rs`.

## HYGG-072 - `server_not_running`'s test helpers string-match the code their own comment says they do not

`src/cli/server_not_running.rs`. `was_reported` / `reported_response` are
`#[cfg(test)]` helpers that `matches!` on `response.error.code ==
"server_not_running"`, while the doc comment at the call site in `src/cli.rs`'s
test says "The typed error preserves the response without string matching." If
the code is renamed, both helpers become silent no-ops and the test
`maps_dead_server_connect_failure_to_friendly_error` fails loudly - so this one
fails closed, which the hunter calls fine. **The claim in the comment is what is
false.**

## HYGG-073 - `startup_command` keys on path equality and falls back to a bare `shepr`

`src/cli/server_not_running.rs`. If the socket path is not exactly
`paths.server_address().api_socket()`, the guidance degrades to "run `shepr`" -
which for a `--session work` invocation or a `SHEPR_SOCKET_PATH` override is the
wrong command and will attach the wrong server. Nothing reports that the
fallback was taken. The `--machine` case never reaches here (it is routed to
`target::remote_error`), so the reachable wrong-advice cases are socket
overrides. Enforcement named: make the address the source of the command -
`ServerAddress::attach_command` already takes the session - rather than
comparing paths.

## HYGG-074 - `matches::required` returns `String::default()` when the spec and the handler disagree, and a comment claims a test would catch it

`src/cli/matches.rs`. The fallback is deliberate and documented ("clap has
already rejected argv without it"), and the hunter agrees the non-panicking
choice is right. But the failure mode is an empty-string pane id or agent target
sent to the server, which surfaces as `pane_not_found: pane  not found` rather
than as a CLI bug, and `src/cli/pane.rs` compounds it with
`selected_pane(..)?.unwrap_or_default()`. Separately, a comment in the same file
says a spec/handler mismatch "shows up as a missing value in tests" - **no test
asserts that.** Enforcement named: extend
`every_cli_spec_root_has_typed_parser` to required arguments per subcommand via
the spec tree walk of HYGG-033, which makes the fallback unreachable in fact as
well as in intent.

## HYGG-075 - `run_on_machine`'s comment names an exclusion a different file enforces

`src/cli/target.rs::validate_machine_command`'s comment says `--machine`
excludes "no local file evaluation (`agent explain --file`)". That is enforced by
`agent::Command::is_api_command` (`Self::Explain(args) => args.file.is_none()`)
in a different file, and nothing ties the comment to it. True today, and the
existing test covers the `--file` case, so the hunter marks this one held.

## HYGG-076 - `machine_mutation_commands_only_expose_add_and_remove` is a blocklist of three strings

`shepr-remote`:

```rust
for command in ["rename", "enable", "disable"] { assert!(spec.try_get_matches_from(...).is_err()) }
```

This is the only enforcement of `AGENTS.md`'s "Saved machines are add/remove
only" claim at the CLI surface. It checks that three specific historical
subcommand names are absent, so adding `machine update`, `machine set-label`,
`machine edit` or `machine relabel` passes. The test reads as an invariant and
is a blocklist. The hunter marks it as not holding the claim it is named for.

Enforcement named: assert the *set* of `machine` subcommands equals
`{list, status, reconnect, add, remove}` - clap can enumerate them via
`command.get_subcommands()`. Cheap, and turns a fail-open name check into a
closed set. Related and better, recorded so it is not re-hunted:
`EndpointCatalog::apply_profile_delta` *does* enforce add/remove structurally -
"updating saved endpoint {id} is not supported" - at the storage layer. So the
claim is enforced where it matters and guarded by a name list where it is
exposed; the hunter asks for that to be said at both sites.

## HYGG-077 - `REMOTE_MISE_SHIM_SUFFIX` is a fail-open name guard

`shepr-remote/src/machine/executable.rs` rejects a discovered path ending in
`/mise/shims/shepr`. mise's shim directory is relocatable (`MISE_DATA_DIR`), and
the equivalent problem exists for asdf, rtx's legacy layout,
`~/.local/share/pipx`, and any other shim dir. When the name stops matching the
guard becomes a silent no-op and shepr caches a shim path that re-execs
something else - precisely the failure the guard was written for, reported as
nothing. Not enforceable as a name rule. The structural answer named: test the
candidate rather than its spelling - the status probe already runs
`status client --json` on the candidate and compares `build_id`, so a shim that
resolves to the right binary is fine and one that does not already fails.
Consider deleting the guard in favour of the probe and keeping only a diagnostic
note when a rejected candidate looked like a shim.

## HYGG-078 - `ssh_config_include` fails open on `is_file()`

`shepr-remote/src/remote/ssh.rs`: `path.filter(|path| path.is_file())`. A
symlink to a file passes (fine), an absent file is silently dropped (fine), and
if OpenSSH on this host reads its system config from somewhere else entirely
(`/etc/ssh/ssh_config.d/*`, a distro override) the managed config silently omits
settings the user believes are active, with no line logged. Combined with
HYGG-038, the include ordering is neither tested nor observable. Fix named: log
at `debug` which includes were emitted and which paths were skipped; the path
list itself cannot be enforced against OpenSSH's actual search order.

## HYGG-079 - `discard_remote_output_preamble` and two siblings are keyed on version-suffixed markers for a compatibility story the project does not have

`REMOTE_OUTPUT_READY_MARKER = "shepr-remote-output-ready:1"`,
`STALE_API_METADATA = "shepr-machine-metadata-stale-v1"`, and the
`--idle-timeout-v1` flag. Three `v1`/`:1` suffixes encoding version negotiation
that nothing reads: per `AGENTS.md` there are no wire compatibility obligations
and client and server are always the same build. Fix named: drop the version
suffixes, or state in one place why they exist - they are the one thing that
could legitimately differ if a stale remote binary is somehow reached, but
`build_id` checking already covers that before the marker is read. Not
enforceable; a decision to record.

## HYGG-080 - The remote `status --json` contract is two independent structs with no shared type

`src/cli/status.rs` defines `ServerStatusJson`/`ClientStatusJson` (`Serialize`);
`shepr-remote` defines `RemoteServerStatusJson`/`RemoteClientStatusJson`
(`Deserialize`) and parses the same JSON over SSH. Field names (`running`,
`version`, `build_id`, `capabilities.detached_server_daemon`) are spelled
independently on both sides. Renaming `running` in `status.rs` breaks every
saved machine at runtime and the build says nothing. The only thing pinning them
is a hardcoded JSON literal in `attach.rs`, written by hand, which can drift
from `status.rs` freely. Additionally, `ServerStatusJson` carries both
`status: "running"|"not_running"` and `running: bool` - two representations of
one fact - and `shepr-remote` reads only `running`, so the `status` string is
unread by the only programmatic consumer.

Enforcement named: put the shape in `shepr-api::schema` and have both sides use
the one type; this is a cross-process contract inside one build, so no copy is
needed at all. Failing that, a test that serialises `ServerStatusJson` and
deserialises it as `RemoteServerStatusJson` - possible today and absent. A
related rule the same hunter proposes for the producer side: a test that
round-trips each generated remote command string
(`"remote-client-bridge"`, `"--idle-timeout-v1"`, `"remote-api-bridge"`,
`"--check"`, `"status"`, `"client"`, `"server"`, `"--json"`, `"server stop"`,
`"--session"`) through `cli::spec::command().try_get_matches_from`, so the
parser proves the producer - the only current check is byte-for-byte golden
strings in `attach.rs`, which pin the producer to itself and say nothing about
the parser.

## HYGG-081 - Five `shepr-remote` comment claims nothing checks, two of them false today

- `RemoteSsh` doc: "no noninteractive command runs past it [the attempt
  deadline]" - `sh_output` and `framed_user_shell_output` honour it;
  `SshStdioBridge::start` and the `establish` callback do not consult it (the
  supervisor holds them to it separately). Checkable with a fake clock.
- `bridge.rs`: "Each local API request has its own stream and therefore its own
  SSH stdio process. The streams are served serially" - true by construction (a
  single accept loop) but nothing asserts it; a future `thread::spawn` per stream
  would break the claim silently.
- `saved.rs::connect`: "Attempts for one endpoint never overlap (the supervisor
  keeps one in flight...) so holding the lock for the whole attempt contends with
  nothing" - a claim about a *different crate's* scheduling, asserted in a
  comment the supervisor does not reference back. If the supervisor ever runs two
  attempts, this becomes a lock held across a 25-second blocking SSH operation.
  Checkable only by a test in `shepr-client`. The hunter adds: if it is truly
  serialised the mutex is unnecessary; if it is not, this is a 25-second stall -
  either way one of the two is wrong, and `&mut self` would make concurrent
  attempts a compile error.
- `catalog.rs::EndpointCatalogWatch`: "an unreadable or invalid file is reported
  once per change; the caller keeps the profiles it has" - **false today** for
  stat errors: `catalog_fingerprint` does `std::fs::metadata(path).ok()?`, so
  `None` means both "the file is absent" and "we cannot stat it", and in the
  second case the watcher reports `Ok(Vec::new())` once, retiring every saved
  machine in the running client. The hunter calls this a likely live defect as
  well.
- `ipc.rs`: "Acquire this before `prepare_socket_path`" - **false at one of three
  call sites**, see HYGG-082.

## HYGG-082 - Claim: `SocketStartupLock` must be acquired before `prepare_socket_path` and held until the listener stops

`shepr-platform/src/ipc.rs`'s doc comment. Reported from two scopes.
`SocketStartupLock` is a separate value from the listener; nothing in the type
system requires the ordering or the lifetime. The `shepr-remote` hunter
enumerates the three callers and marks the claim **false today at one of them**:

| site | startup lock | prepare | bind | identity | extra chmod |
|---|---|---|---|---|---|
| `shepr-remote/src/remote/bridge.rs` | yes | yes | yes | yes | yes (`0o600`) |
| `shepr-api/src/server.rs` | yes | yes | yes | yes | no |
| `shepr-server/src/server/headless.rs` (client protocol socket) | **no** | yes | yes | yes | no |

The client protocol socket - the one the whole TUI attaches to - skips the
startup lock the doc comment says to take, which is exactly the race the lock
exists to close. The bridge's extra `restrict_socket_permissions(0o600)` is
redundant, since `bind_private_local_listener` already ends owner-only.

Enforcement named by both hunters, structurally: one
`shepr_platform::ipc::bind_private_socket(path, busy_message) -> (Listener,
SocketStartupLock, SocketFileIdentity)` - or `prepare_socket_path` taking
`&SocketStartupLock` - so the wrong order is unrepresentable and the three
callers lose the choice.

## HYGG-083 - Claim: the client's five-second HealthPing renews the bridge's sixty-second byte-level watchdog

`shepr-platform/src/remote_bridge.rs`'s module doc: "The client endpoint sends
HealthPing after five seconds without received data, and the server answers
HealthPong. Those protocol frames renew this byte-level watchdog." A 60-second
`IDLE_TIMEOUT` in `shepr-platform` whose safety depends on a five-second
interval defined in `shepr-client`/`shepr-protocol`
(`shepr_client::endpoint::health::HEARTBEAT_INTERVAL`). Nothing relates the two
numbers; halve the timeout or double the ping interval and healthy idle bridges
start dying. Reported from two scopes. `shepr-platform` sits below
`shepr-client`, and `shepr-remote` sits between them passing only
`idle_timeout: bool` through, so neither crate can see the other's constant.

Enforcement named: put both constants where both crates can read them
(`shepr-core`, or the interval in `shepr-protocol`) and add a test asserting
`IDLE_TIMEOUT > HEALTH_PING_INTERVAL * k` - the `shepr-remote` hunter proposes
`IDLE_TIMEOUT >= HEARTBEAT_INTERVAL * 4`. Today nothing would notice either
number changing.

## HYGG-084 - Claim: `ProcessIdentity::StartTime` has a pid-reuse race window of a few syscalls

`shepr-platform/src/process.rs` documents the window; no test reaches it and no
assertion guards it. The hunter's own answer is that the right move is deleting
the branch rather than testing it - the deletion is filed as dead code in a
sibling document - but the unenforced claim is recorded here.

## HYGG-085 - `shepr-platform`'s `lib.rs` says domain rules live in modules that do not exist there

`crates/shepr-platform/src/lib.rs`: "domain rules live with their consumers in
`detect`, `remote`, and the app". There is no `detect` or `remote` module in
this crate or at that path in this workspace - they are
`crates/shepr-agent/src/detect` and `crates/shepr-remote`. **False today**, and
false in a second way: 25 domain event functions (`workspace_created`,
`tab_renamed`, `pane_spawned`, `session_saved`, `api_request_started`,
`integration_action`, ...) live in `logging.rs` in this crate, named after
concepts the crate knows nothing about.

Enforcement named for the second half, and the hunter says `brokkr.toml`
already expresses this kind of rule: a text rule forbidding the identifiers
`workspace`/`tab`/`pane`/`api`/`session` in `shepr-platform` public item names,
or simply moving the functions so the existing dependency allowlists do the
work.

## HYGG-086 - `AGENTS.md` describes `reference/` and `docs/` as binding in-repo folders that do not exist

**False today**: neither directory exists in the tree; only `notes/` does.
Reported from `shepr-platform`, which adds why it matters there - several of the
durable claims the hunts want written down (the tunable inventory, the env-var
registry, the logging level policy) have nowhere to live that the document says
they must. `shepr-protocol`/`shepr-config` and `shepr-server` both note the same
absence in passing when saying that user-facing caps
(`MAX_TAB_BAR_COMMAND_INTERVAL_SECONDS = 31_536_000`, `MAX_SESSION_NAME_LEN`,
the sidebar 16s, the roughly forty `shepr-server` tunables) are documented
nowhere a user would look. `shepr-mux` and `shepr-agent` likewise propose
`reference/`-level inventories that currently have no home.

## HYGG-087 - `AGENTS.md`'s crate list restates responsibilities nothing checks

"`shepr-platform`: Linux process, filesystem, IPC and terminal plumbing" and its
siblings. `brokkr.toml`'s `[[dependency_rule]]` entries already encode the
layering mechanically; the prose adds the responsibilities, which nothing
checks - and `logging.rs`'s domain catalogue is already outside its stated
responsibility (HYGG-085). `shepr-agent` reports the same shape: `AGENTS.md`
restates the agent state vocabulary and the crate layering, the latter enforced
by `brokkr.toml` and the former not.

## HYGG-088 - `AGENTS.md`'s description of `shepr-vt`'s `scan.rs` names things that live elsewhere and omits the module list

`AGENTS.md` says `scan.rs` scans "modes 9/1016/2031/2048 ... and the halfwidth
katakana voiced marks". Those live in `handler.rs`. `scan.rs` explicitly forbids
them and has a test for it
(`modes_and_reset_are_left_to_the_parser_handler`). `AGENTS.md` also omits
OSC 9;4, 9;9, 1337 and `CSI ? 3 J`, and its module list for `shepr-vt` omits
`handler.rs`, `modes.rs`, `rows.rs`, `selection.rs`, `coords.rs` and `locks.rs`.
The hunter files this as documentation restating a list the code owns.

## HYGG-089 - `AGENTS.md` says the pane child is reaped by a blocking `wait()` in the pane runtime

It is reaped through a pidfd `waitid`; blocking wait is only the fallback, per
commit b7c14da. **False today.**

## HYGG-090 - `AGENTS.md` and `app/mod.rs` both say `src/app/` is split into state, actions and input

**False as written**, per the `shepr-server` hunter. There is no `input` module
under `src/app/` at all; `app/` holds 20 modules (`actions`, `agent_resume`,
`agents`, `api`, `api_helpers`, `creation`, `events`, `git_refresh`,
`host_theme`, `ids`, `runtime`, `session`, `state`, `tab_bar_status`,
`terminal_targets`, `terminal_titles`, `window_title`, `snapshot_tests`, plus
the `api/` and `actions/` subtrees), and input lives in
`server/pane_input.rs` and `server/input_wire.rs`. The module doc at
`app/mod.rs` restates the same stale claim, listing only `state.rs` and
`actions.rs` - a doc comment enumerating a list the directory generates. The
no-god-object intent is honoured in substance (nothing is a 3000-line
monolith), but `impl App` is spread over 25 blocks in 20 files and `impl
AppState` over 6, so the "three parts" framing no longer describes anything.

Enforcement named: restate the claim as what is true and enforceable - `AppState`
is pure data, `App` owns runtime, no single file over N lines - and add a
file-length rule to `brokkr.toml`. The prose itself is not lintable and should
stop naming a module count that drifts.

## HYGG-091 - `AGENTS.md`'s "No god objects" principle names only `shepr-server/src/app/`, so it does not reach the crate's actual largest types

The `shepr-mux` hunter's reading: `PaneRuntime` is 2960 lines with about 90
public methods, roughly forty of them one-line delegations, and the principle as
written does not cover it; `terminal/metadata.rs` (1438 lines) and
`persist/restore.rs` (2365) are in the same position. If the principle is meant
generally, the enforcement should be a per-file or per-impl size rule in
`brokkr.toml`. Overlaps with HYGG-090's proposed file-length rule.

## HYGG-092 - Claim: `PaneState` is separate from `PaneRuntime`

`AGENTS.md`. The `shepr-mux` hunter's verdict: technically yes, vacuously.
`PaneState` is two fields (`attached_terminal_id`, `right_click_passthrough`);
everything that was pane state now lives in `TerminalState`, itself a 30-field
struct with 14 public mutable fields including two derived ones (`state`,
`revision`) whose invariants rest on callers remembering to call
`recompute_effective_state` and to bump the counter. The separation that matters -
pure data testable without PTYs - holds for `TerminalState` (it is tested
extensively and takes `now` as a parameter, which is the real evidence) and does
not hold for `PaneRuntime`, which mixes a PTY actor handle, a tokio abort handle,
four `Arc`-shared atomics, three mutexes, a `Cell` and forty pure-read
delegations in one type. The claim is "true about the name and misleading about
the shape."

## HYGG-093 - Claim: `AppState` is pure data

`AGENTS.md`. The `shepr-server` hunter's verdict: **holds** - no channels, no
runtime, no `Arc`, and `test_new()` works. One leak recorded:
`app/state.rs`'s `terminal_runtime_shutdowns: Vec<TerminalId>` ("runtimes that
should be shut down by the app/runtime layer") is a pending-effects queue, not
state. It is data-shaped so the claim survives literally, but it is the seam
through which pure state schedules side effects, and nothing stops the next such
field from being a channel. Returning the shutdown list from the mutating call
instead of parking it on state removes the field.

## HYGG-094 - Claim: render is pure

`AGENTS.md`: "`compute_view()` updates only `AppState::view`; pane runtimes are
resized by explicit geometry paths, and surface drawing takes shared references
and only draws." The `shepr-server` hunter's verdict: **holds in behaviour, not
in structure.** `compute_view` does touch only `view`, but it takes
`&mut AppState` so nothing enforces it. `resize_pane_infos` (`ui/panes.rs`)
takes `app: &AppState` and a `&PaneRuntimeRegistry` and calls `rt.resize(...)`
through them, and so do `resize_tab_surface`, `resize_tab_surface_layout` and
`resize_all_tab_surfaces`; the draw path (`render_tab_surface`) and the resize
paths take *identical* signatures `(&AppState, &PaneRuntimeRegistry, ...)`.
Nothing in the type system distinguishes "only draws" from "resizes every PTY in
the tab"; the invariant rests entirely on which function name a caller types, and
in `render.rs` the two are interleaved in one function, which is where a mistake
would land.

Enforcement named: give the resize paths a distinct receiver - a
`PaneResizer<'a>` newtype over the registry, constructed only on the explicit
geometry paths - so a drawing function cannot reach `resize`; and make
`compute_view` return `ViewState` instead of taking `&mut AppState`, which makes
"touches only `view`" true by signature.

The `shepr-vt`/`shepr-pty` hunter reports a violation of the same claim in
`shepr-mux/src/pane/terminal/backend.rs` (`render()`,
`collect_dirty_patch()`, `synchronized_output_active()` and
`synchronized_output_state()` all call `flush_expired_synchronized_output`, which
feeds the buffered frame through the parser and mutates the grid), but files it
as a live defect, so a fix pass should expect that entry in the bug document
rather than here.

## HYGG-095 - `CLIENT_SHELL_METHODS` is a hand-maintained list of 26 method-name strings matched against generated names

`shepr-server/src/server/client_commands.rs`. `supports_client_shell_method`
compares `method.traits().name` - generated in `shepr-api`'s schema - against a
literal list in this crate. Rename or retire a method in the schema and the
entry here becomes a dead string: the method silently stops being reachable over
the client-shell lane (`client_transport.rs` and `endpoint_requests.rs` both
refuse it), and nothing reports the mismatch. The only test guarding it is
HYGG-045, which cannot fail reliably. The hunter states it is checkable and that
it could not rule out the list being stale today without enumerating the schema.

Enforcement named: a `client_shell: bool` in the schema's own `MethodTraits`,
which already carries `mutates_ui` and `name` - then the list disappears and a
new method must declare its lane. Failing that, the set-membership test in both
directions.

A related unheld prediction from the `shepr-api` hunter: every method's wire
name is spelled twice, once in `#[serde(rename)]` and once in `traits().name` -
72 methods, 144 string literals. All 72 pairs were compared mechanically and
agree today, but a single mismatch would silently mislabel every log line, every
`api_method_name` caller, and the `api_response_outcome` classification for that
method, with nothing failing. The test named: serialize each `Method` variant and
assert `json["method"] == traits().name`.

## HYGG-096 - `#![cfg_attr(feature = "test-api", allow(dead_code))]` silences dead-code detection for `shepr-server` in exactly the build that would run it

`crates/shepr-server/src/lib.rs`. The root package depends on `shepr-server`
normally and with `features = ["test-api"]` as a dev-dependency, so under
feature unification any build that includes dev-dependencies - `cargo test`,
`cargo clippy --all-targets`, i.e. what `brokkr check` runs - turns `test-api`
on and with it silences `dead_code` across all roughly 45 000 lines of the
crate. The gate cannot report an unused function, module, field or variant in
`shepr-server`. The hunter calls this a guard that fails open and says it is the
reason its own dead-code section is hand-found; it also declines to call three
candidates dead (`MIN_CLIENT_COLS`/`MIN_CLIENT_ROWS`, the
`AttachInputDelivery::Failed` variant, `ShutdownLifecycle::set_frozen_session_policy_for_test`)
until the lint is enabled, since a wrong deletion is the mistake nobody can undo
by reading. It is also the only crate-wide `allow(dead_code)` in the repo;
`shepr-vt` and `shepr-mux` use narrow per-item allows with justifying comments,
which is the house style.

Enforcement named: delete the blanket allow, gate the test fixtures on the items
themselves with `#[cfg(any(test, feature = "test-api"))]` as the crate already
does nearly everywhere, and let `dead_code` run. The hunter names this the one
finding to fix first in its scope, because it is the one that hides other
findings.

## HYGG-097 - Two `debug_assert_eq!` phase claims are not checked in the build that ships

`shepr-server/src/server/headless/lifecycle.rs` asserts the lifecycle phase at
two sites. `brokkr.toml` sets `[test] debug = true` so they do run under
`brokkr test`, but `brokkr test <name>` defaults to release per `AGENTS.md` and
the shipped binary is release. These are the only two invariant assertions in
the serving layer, and they cover the phase machine, which is the one piece of
state written by signal, API and logind threads. Enforcement named: make the
phase transitions total functions on `ShutdownPhase` returning `Result`, so an
illegal transition is unrepresentable rather than asserted.

## HYGG-098 - `unregister_moved_pane` is a guard that vanishes in release

`shepr-mux/src/workspace.rs`:

```rust
pub fn unregister_moved_pane(&mut self, _pane_id: PaneId) {
    // `take_pane_for_move` removes the pane record and its public number
    // together; the API still calls this to acknowledge that removal.
    debug_assert!(self.pane_state(_pane_id).is_none());
}
```

Called from production at `shepr-server/src/app/api/panes/geometry.rs`. In a
release build (`brokkr install`) `debug_assert!` compiles away and this is a
`&mut self` method that does nothing, so the shipped binary takes a mutable
borrow of the workspace to acknowledge something. It is either an invariant
check that should be a real `assert!` or return `Result`, or it is dead.

## HYGG-099 - The one-tab workspace invariant is enforced by opt-in test calls and one `Deref` panic

`shepr-mux/src/workspace.rs`. `Workspace::assert_invariants_for_test` (150
lines, `#[cfg(any(test, feature = "test-api"))]`) is the real statement of the
invariant - non-empty tabs, `active_tab` in range, unique public tab and pane
numbers, layout pane set exactly equal to the pane record set, focused pane in
layout, root pane present. It is called from 25 places, all of them individual
tests that chose to call it; nothing calls it after a mutation in production and
no mutating method calls it. The recent commit "Enforce the one-tab workspace
invariant" gets its enforcement from the `Deref` panic plus these opt-in calls.

Which are checkable: all of them, and they are already written down as
assertions - the gap is only *when* they run. Which are false today: none the
hunter could find by reading; the adversarial constructor
`test_adversarial_identity_state` exists precisely to exercise the divergences.

Enforcement named: either call the checker `debug_assert`-style from every
mutating method's exit, or restructure so the checks are unnecessary -
`tabs: Vec<Tab>` plus `active_tab: usize` is the whole problem and a
non-empty-vec type with a focused index is the structural answer. The hunter
prefers the second given the project's posture. The `Deref` half (an `expect` in
a `Deref` impl, with `active_tab` public and `tabs_mut()` handing out
`&mut [Tab]` to any crate, so a server-side caller can put `active_tab` out of
range and the next `ws.panes` aborts the server) is filed by the hunter under
errors, so a sibling document may carry it too.

## HYGG-100 - `io::load` and `load_history` claim a lease they do not require

`shepr-mux/src/persist/io.rs`: "Reads the saved layout for restore. The server
acquires a DataDirLease before calling this, so native agent sessions cannot be
restored twice", and "Removing it is safe because only one server writes a data
directory: `SessionWriter` owns the directory lease before any write."
`SessionWriter::new(lease, ...)` does take the lease by value, so that half is
enforced by the type. `load(data_dir: &Path)` and
`load_history(data_dir: &Path)` are `pub`, take a bare path and enforce nothing;
the claim is true today only because the one caller happens to do it.

Enforcement named, trivially: `load(lease: &DataDirLease)`. The claim then holds
by signature, the comment can be deleted, and `lock::LOCK_FILE_NAME` stops
needing to be reachable.

## HYGG-101 - A pruning-policy guard keyed on a directory-name string literal

`shepr-mux/src/persist/writer.rs::preserve_existing_in`:

```rust
if let Err(err) = prune_backups(&older, keep) {
    if directory_name == "session-snapshots" {
        std::fs::remove_file(&backup)?;   // snapshot copies: pruning failure is fatal
        return Err(err);
    }
    tracing::warn!(...);                  // session-backups: pruning failure is a warning
}
```

Two different error policies selected by string comparison against one of the
five spellings of `"session-snapshots"` in the same file. Rename the directory at
the four call sites and forget this one, and snapshot pruning failures silently
become warnings - the directory grows without bound and nothing says so.
Enforcement named: pass a `PrunePolicy { Fatal, Warn }` alongside `keep`, which
removes the string entirely and makes the bad spelling unrepresentable.

## HYGG-102 - Fifteen hook-authority guards key on `(source, agent_label)` strings that arrive from shipped shell scripts, with nothing keeping the two sides in step

Reported from `shepr-mux` and `shepr-agent`.
`shepr_agent::detect::full_lifecycle_hook_authority(source, agent_label)` is
`AgentSource::from_pair(source, agent_label).and_then(|s| s.agent()).is_some_and(...)`,
so an unrecognised pair yields `false`. `shepr-mux` calls it, or the sibling
`session_identity_only_integration`, at ten sites across
`terminal/state/hooks.rs`, `lifecycle.rs` and `sessions.rs`, and calls
`from_pair` directly at five more.

Both strings originate in the hook assets shipped into other agents' config
directories (`crates/shepr-agent/src/integration/assets/*/shepr-agent-state.sh`
and friends), which must carry the literals - the deployment constraint is real.
What is missing is anything keeping them in step: rename or typo a `source` in
one asset and every one of the fifteen guards downgrades that agent from
hook-authoritative to screen-detected, silently, with no log line at any site.
The failure mode is "the sidebar became less accurate", which is exactly the
kind of regression nobody bisects. `shepr-agent` adds the same point from the
producing side: `assets/claude/...sh` has `source = "shepr:claude"`, kimi has
`"shepr:kimi"`, hermes has `_SOURCE = "shepr:hermes"`, with the agent label
duplicated next to it, while the owner is
`AgentDescriptor::integration_source`; a typo makes the report arrive as
`AgentSource::Custom`, which silently loses `full_lifecycle_hook_authority` and
`session_identity_only_integration`. Only two assertions exist anywhere today
(for qwen and for letta), both incidental.

Enforcement named by both hunters: a test that iterates `INTEGRATION_SPECS` and
asserts each asset's text contains its target's `integration_source` and
canonical label - `shepr-agent` calls it "the single highest-value mechanical
check in the crate" and "the cheapest mechanical win"; `shepr-mux` pairs it with
the `SHEPR_*` asset-literal test and says together they are the whole mechanical
answer to the forced duplication. Neither test exists.

## HYGG-103 - The same shape one level down: `SHEPR_*` names spelled as literals in shipped assets with no membership test

`shepr-mux/src/pane/launch.rs` exports `SHEPR_PANE_ID_ENV_VAR` publicly, keeps
`SHEPR_TAB_ID_ENV_VAR` and `SHEPR_WORKSPACE_ID_ENV_VAR` private, and writes
`"SHEPR_BIN_PATH"` as a bare literal; `shepr-server/src/app/tab_bar_status.rs`
writes `"SHEPR_BIN_PATH"` as a bare literal too, so there are two independent
writers of one name with no shared definition. Roughly twenty shipped hook
assets read both names as literals, and `shepr-agent` reports that every asset
restates the whole contract - `SHEPR_ENV` must equal `"1"`,
`SHEPR_SOCKET_PATH` and `SHEPR_PANE_ID` must be non-empty, `SHEPR_BIN_PATH`
falls back to the bare name `shepr` - field by field, in four languages. The
assets are a genuinely forced copy; nothing keeps them in step today.

Enforcement named: one module (`shepr-config`) exporting every `SHEPR_*` name, a
`brokkr.toml` text rule forbidding `"SHEPR_` string literals elsewhere, and one
test that walks `crates/shepr-agent/src/integration/assets/**` extracting
`SHEPR_[A-Z_]+` and asserts set membership. The `shepr-mux` hunter says that
last test is the only thing that can ever keep the deployment-forced copies
honest, and it does not exist.

## HYGG-104 - The asset version parity test carries a hand-written list

`shepr-agent`'s `bundled_integration_asset_versions_match_expected_versions`
enumerates eighteen `(name, asset, version)` triples and omits
`OPENCODE_TUI_PLUGIN_ASSET`, `OPENCODE_V2_TUI_PLUGIN_ASSET` and
`HERMES_PLUGIN_MANIFEST_ASSET` (whose `version: "1.0"` in
`assets/hermes/plugin.yaml` is a fourth spelling of the Hermes version that
nothing reads). A new target added without extending the list is silently
uncovered. `registry::integration_asset(target)` already exists, so iterating
`INTEGRATION_SPECS` would make the test exhaustive by construction.

Related unchecked claim in the same area: `assets/hermes/plugin.yaml`'s `name:`
duplicates `HERMES_PLUGIN_INSTALL_NAME` - the install directory is
`<hermes>/plugins/shepr-agent-state` (Rust const) and the manifest inside it
declares `name: shepr-agent-state` (YAML asset). Divergence means the plugin is
installed under a directory Hermes will not associate with the manifest, and
status only checks that `plugin.yaml` exists, not what it says. Checkable with a
test that parses or greps the asset.

## HYGG-105 - Hermes's and Grok's declared hook events are fiction

`shepr-agent`. `HERMES_HOOK_EVENTS` in `agent/mod.rs` declares one event,
`SessionStart`, with action `Session`; the actual asset
(`assets/hermes/__init__.py`) registers `on_session_start`, `on_session_reset`
and `pre_llm_call`, and invents three start sources (`startup`, `new`,
`resume`). Nothing consumes the Hermes row, so nothing notices. Same shape for
Grok: its descriptor carries `integration_hook_events: &[]`, yet
`targets.rs::grok_hook_config` writes a real `SessionStart` hook with action
`session`, so any generic consumer of `IntegrationTarget::hook_events()` sees
Grok as hookless. Enforcement named: a test asserting that a target with a
config-registered hook has a non-empty event list, plus deriving the written
config from the event list instead of hand-writing it.

## HYGG-106 - The session-start-source vocabulary is spelled three times and the copies disagree

`shepr-agent`. `resume.rs::AgentSessionStartSource::parse` accepts eight values
(`startup resume clear compact branch new fork select`);
`claude_settings.rs::SESSION_START_MATCHER` is
`^(startup|resume|clear|compact|fork)$` (five); the kimi asset defaults to the
literal `"startup"`; the hermes asset emits `startup`, `new`, `resume`. The
comment above `SESSION_START_MATCHER` says Grok "uses new/load", and `load` is
in none of the three lists, so a grok-imported Claude hook firing with `load`
normalises to `None` and is treated as unrecognised by
`session_start_source_is_recognized` in `shepr-mux`'s `hooks.rs`. Enforcement
named: derive the matcher regex from the enum's variant strings, and give the
enum a single `as_str` so the assets can be grepped against it.

## HYGG-107 - `default.toml`'s theme list has already diverged from `THEME_NAMES`

`shepr-config/src/theme.rs`'s `THEME_NAMES` holds 18 themes; `default.toml`'s
comment lists 11. Every light variant - `catppuccin-latte`, `tokyo-night-day`,
`gruvbox-light`, `one-light`, `solarized-light`, `kanagawa-lotus`,
`rose-pine-dawn` - is implemented, accepted by `canonical_theme_name`, named in
the *error message* for an unknown theme, and absent from the printed default
config. **A fact, not a prediction.** Enforcement named: assert every
`THEME_NAMES` entry appears in `DEFAULT_CONFIG`, the same shape as the existing
keybinding test.

Theme names are also spelled three times in code (`THEME_NAMES`,
`canonical_theme_name`'s match, `Palette::from_name`'s match) plus 18
constructor functions. `built_in_theme_names_resolve` covers
`THEME_NAMES -> canonical -> from_name` in one direction only, so a palette
implemented but missing from `THEME_NAMES` is undetected - which is exactly the
failure mode that produced the divergence above.

## HYGG-108 - `default.toml` restates four more lists the code owns, none of them checked

`shepr-config`. Beyond the theme list (HYGG-107) and the keybinding list (which
*is* checked, by
`src/main.rs::default_config_documents_every_keybinding_with_its_default`),
`default.toml` restates the `cjk_ime_agents` accepted-name list (22 agent names,
hand-written, generated by `ConfigAgent::all()`), the sidebar built-in token
lists, and the `right_click_passthrough_modifier` alias list. None is checked.

The keybinding test that does exist covers `[keys]` only, and only
string-valued entries - it `continue`s past anything that is not a TOML string.
Unchecked defaults documented in the same file: `sidebar_width = 26`,
`sidebar_min_width = 18`, `sidebar_max_width = 36`, `mouse_scroll_lines = 3`,
`headless_cols = 120`, `headless_rows = 40`,
`scrollback_limit_bytes = 10000000`, `startup_per_agent_delay_ms = 100`,
`row_gap = 0`, `window_title = "{hostname}: {workspace}"`, every enum default,
and the whole `[theme]` and `[remote]` blocks. The hunter also flags
`ui.accent`: `default.toml` says "Unset uses the theme accent" and documents
`#89b4fa`, while the Rust `Default` is `"cyan"` (never applied, since
`resolve_palette` only uses `ui.accent` when the provenance is explicit) and a
user who reads the doc and writes `accent = ""` gets a refused launch.

Enforcement named: generalise the existing `main.rs` test - serialise
`Config::default()` to a TOML table, walk every leaf, assert each appears as
`# key = <value>` in `DEFAULT_CONFIG` - plus one test per list in the same
shape. The mechanism already exists in `main.rs`; it is just scoped to one
table. The hunter ranks this third among the things a single mechanism buys, and
notes it would have caught HYGG-107.

## HYGG-109 - `default.toml` ships one active setting

Every line is commented out except `pane_history = false` under
`[experimental]`. Since the file is only printed (`shepr --default-config`) and
never parsed, the effect is that a user who redirects it to
`~/.config/shepr/config.toml` gets a config that explicitly pins one
experimental flag while leaving everything else to defaults. The hunter calls it
almost certainly an editing slip. Enforcement named: a test asserting every
non-blank, non-`[section]` line in `DEFAULT_CONFIG` starts with `#`.

## HYGG-110 - `shepr --help` documents one of five environment variables the CLI honours

The CLI's behaviour is changed by `SHEPR_CONFIG_PATH`, `SHEPR_SESSION`
(`shepr_config::SESSION_ENV_VAR`), `SHEPR_SOCKET_PATH` and
`SHEPR_CLIENT_SOCKET_PATH` (`shepr_config::address`), and `SHEPR_PANE_ID`
(`shepr_mux::pane`, read by `CliContext::local` to resolve `--current`).
`shepr --help` documents exactly one, and spells its name as a literal rather
than interpolating `shepr_config::CONFIG_PATH_ENV_VAR`. A user debugging why
`--current` says "belongs to a different server" has no way to discover from the
CLI that two socket variables are involved, even though the error text names one
of them.

Enforcement named: a slice of `(const, description)` in one place rendered by
`print_help`, with a test asserting the slice covers every `*_ENV_VAR` const the
binary's crates export. Without the test it is a list that drifts.

Related, from `shepr-platform`: `SHEPR_LOG` is a bare literal in `logging.rs`
with no constant at all, and it appears in no registry, no `--help` text and no
doc. And from `shepr-vt`/`shepr-pty`: nothing lists which `SHEPR_*` variables a
pane may inherit - the pane environment is inherited wholesale and then scrubbed
by a denylist split across `shepr-mux`'s `launch.rs` (host terminal keys) and
`shepr-agent` (agent keys), with server-only variables removed ad hoc elsewhere
(`SHEPR_STARTUP_CWD` via `unsafe remove_var` in `headless/bootstrap.rs`). The
test named there: every `*_ENV_VAR` constant is either scrubbed or explicitly
allowed.

## HYGG-111 - `manifest.rs`'s module doc enumerates region names, matcher keys, gate keys and limits in prose

`shepr-agent`, next to `RegionSpec::parse`, `ManifestRule`, `ManifestGate` and
the `MAX_*` constants - including "at most eight levels total". The hunter says
the doc-comment case is enforceable only by a doc test that parses the prose,
which is not worth it, and that the honest fix is to shorten the prose to the
concepts and point at the enum. Same entry carries `notes/todo.md`'s stale
paths (`src/integration/assets/...`, `src/detect/manifests/...`,
`src/ghostty/rows.rs`, `src/protocol/wire.rs`, `src/client/shell/state.rs`) -
none of which exist since the crate split, all now under `crates/`. `notes/`
carries no truth guarantee, but the paths are stale enough to send a reader
nowhere.

## HYGG-112 - `HeadlessServer`'s module doc names socket files the config owns

`shepr-server/src/server/headless.rs` claims the server listens on `shepr.sock`
and `shepr-client.sock`. Those names live in `shepr-config::address` and are
session-dependent: a named session listens on
`sessions/<name>/shepr-client.sock`, per `socket_paths.rs`. Documentation
restating a list the code generates. Fix named: delete the names from the
comment.

## HYGG-113 - Four module docs in the terminal core cite a dependency and a directory that are not there

- `shepr-vt/src/format.rs`'s module doc says the VT output is replayed "after
  some resizes"; `shepr-mux`'s `backend.rs::resize` says that replay was
  removed.
- `shepr-vt/src/lib.rs`'s doc for `DEFAULT_FOREGROUND` says it matches "what the
  libghostty-vt render state reported". That backend is gone.
- The root `Cargo.toml` comment cites a "reference checkout in
  `research/alacritty`". No `research/` directory exists.
- `shepr-mux/src/pane/terminal/backend.rs` says a live workaround exists because
  "the libghostty core loses rows on resize" - the stated reason for a workaround
  in live code cites a dependency that is not present, so nobody can now check
  whether `alacritty_terminal` has that behaviour and the workaround is
  unfalsifiable. `backend.rs` also emits an operator-facing log line naming a
  component that does not exist: `error!(pane = ..., "ghostty core lock poisoned
  in reader")`.

All marked stale today. The `shepr-mux` hunter adds that roughly 200 production
references to Ghostty naming remain (`GhosttyPaneTerminal`, `GhosttyPaneCore`,
`PaneTerminal { ghostty }`, about forty `ghostty_*` free functions) and proposes
the cheapest enforcement in its report: a `brokkr.toml` text rule forbidding
`ghostty`/`Ghostty` outside a comment that explains a historical decision, or
forbidding it outright after a rename - the same mechanism the gremlin scan
already uses. The two workaround comments need a human to decide whether the
workaround is still needed against the real emulator, which cannot be answered
by reading; `AGENTS.md` already directs the reader to the pinned
`alacritty_terminal` source in the cargo registry.

## HYGG-114 - `TerminalState`'s docs and a whole test module describe a migration that is over

`shepr-mux/src/terminal/state/mod.rs`: "Pure state for a server-owned terminal.
**During the migration** this is still one-to-one with a pane-backed PTY, but
pane/view state no longer owns terminal identity, cwd, labels, or agent
metadata." And `src/pane/state.rs`: "Viewport state for a pane. Terminal
identity, cwd, labels, and agent metadata live in TerminalState." The migration
is complete - `PaneState` now holds two fields - so three doc comments and a
test module named `migration_tests` describe a transition nobody can still
observe, and nothing in the build would notice them becoming false; they were
already false when `PaneState` shrank. Not mechanically enforceable; it is a
deletion.

## HYGG-115 - `handler.rs` claims every `Handler` method is listed explicitly, and vte is not pinned

`shepr-vt`. The hunter checked the impl against vte 0.15.0: it is complete
today. Two things could break the claim silently - vte is not pinned (only
`alacritty_terminal` is `=0.26.0`, and vte arrives transitively at `^0.15`), so
a `cargo update` can move it; and a new defaulted trait method would be silently
a no-op. Enforcement named: `#[warn(clippy::missing_trait_methods)]` on that
impl makes the claim mechanical. Separately, pinning vte with `=` would need
`vte` added to the `shepr-vt` allowlist in `brokkr.toml`.

## HYGG-116 - Claim: alacritty types never leak out of `shepr-vt`

The dependency rule enforces this at crate level, and the hunter found no
alacritty types in public signatures. Two soft spots recorded: the public
`impl From<Rgb> for RgbColor` and `From<RgbColor> for Rgb`; and `CellStyle`,
which is `pub` in a private module and reachable through `CellBasicData.style`
but not re-exported, so callers cannot name it.

## HYGG-117 - `PtyIoInbox` assumes an entry's `order` identifies one entry

`shepr-pty`. `insert_resize_replies` inserts several entries with the *same*
order, and `next_entry_index(current_order)` uses `position(order == order)`. It
is correct only because those replies are contiguous and written front to back.
Enforcement named: give each reply its own order.

## HYGG-118 - A mutation rule in `shepr-vt` that nothing checks

"Every parser-driven mutation must use `with_handler`", and every mutation must
end with `collect_damage()` (or `bump_full_damage`). Callers do this by hand:
`write`, `flush`, `mode_set`, `resize`, the scroll methods. Enforcement named,
structurally: call `collect_damage` inside `with_handler`.

## HYGG-119 - `EndpointTransport`'s default method bodies fail open

`shepr-client`:

```rust
fn disconnect(&mut self) {}
fn flush(&mut self, _deadline: Instant) -> io::Result<()> { Ok(()) }
fn take_error(&mut self) -> Option<io::Error> { None }
```

A transport that forgets `flush` reports every flush as succeeding; one that
forgets `take_error` reports itself permanently healthy to the registry, which
is exactly the signal `local_failure_policy` and the supervisor act on. Three
defaults, three silent no-ops keyed on a name the implementor did not write.
There are few implementors, so the defaults save almost nothing. Enforcement
named: remove the defaults, so the compiler requires each implementor to state
its answer.

## HYGG-120 - The keybinding help screen is a hand-maintained restatement of the `Keybinds` struct

`shepr-client/src/keybind_help.rs::keybind_help_groups` enumerates
`keybinds.<field>` by hand for every one of `Keybinds`' 47 fields plus
`NavigateKeybinds`' 6. The hunter checked: today every field does appear, so
this is a checkable claim that is true right now with nothing holding it. Adding
a config key and forgetting the help line compiles, ships, and is invisible -
the key simply has no help entry.

The same function also hard-codes six entries that come from nowhere:
`entry("esc", "back")`, `entry("tab / shift+tab", "cycle pane")`,
`entry("enter", "open workspace")`, `entry("1..9", "switch workspace")`. If any
of those is rebindable the help is lying; if none is, they are undocumented
fixed keys the config cannot reach. Worth deciding which.

Enforcement named, at compile time: destructure `Keybinds { navigate, help,
new_workspace, .. }` exhaustively (no `..`) at the top of `keybind_help_groups`
and build the groups from the bindings, so adding a field fails to compile until
it is placed. The hunter calls this the single highest-value mechanical fix in
its scope. It also supersedes widening HYGG-051.

The help screen is one of eight sites the `shepr-config` hunter counts for the
keybinding action list, of which only three are compiler-checked; the uncovered
one it singles out is the roughly 51
`apply_action!`/`apply_indexed!`/`apply_navigate!` lines in `keybinds.rs` - add a
field to `KeysConfig` and forget its `apply_action!` line and it compiles, the
user's binding parses, and the action simply never fires with no diagnostic. Same
for a missing `shepr-termio/src/input/keybindings.rs` dispatch row or a missing
`keybind_help.rs` entry. Its enforcement proposal is one declarative table naming
each action once with its kind, default binding and help text, generating
`KeysConfig`, its `Default`, `Keybinds`, the apply loop, the wire mapping and the
help entries, so every omission is a compile error. The hunter says it thinks
this rewrite is worth it and that it is the only way the eight sites stop
drifting. Adjacent: `KEY_BINDING_COUNT = 51` in `wire.rs` is a hand-maintained
count of a compile-time-known list, correct today, whose only job is a runtime
error that cannot trigger; derive it from the macro or delete it.

## HYGG-121 - `modes.rs`'s mouse-clear list and its test are maintained by hand, together

`shepr-termio`. `DISABLE_HOST_MOUSE_REPORTING_SEQUENCE` lists eight modes; the
test `clears_all_known_host_mouse_modes` loops over the same eight spelled again
as strings. The test restates the constant rather than deriving anything from it,
so adding a ninth mode to the constant and not to the test passes, and adding it
to the test and not the constant fails with a clear message - half a guard.
Meanwhile `\x1b[?1016h`, the enable for one of those eight, lives in
`shepr-client/src/terminal_setup.rs`, which is the copy the test cannot see at
all. Enforcement named: derive the list from a single `const MODES: [&str; N]`
used by both the sequence builder and the test.

## HYGG-122 - `MAX_RETRY_DELAY`'s doc asserts a user-visible promise nothing checks

`shepr-client/src/endpoint/supervisor.rs`'s doc comment states that
`shepr machine reconnect` tells the user open clients retry within 30 seconds,
and that `ATTEMPT_BUDGET` (25 s) plus the retry accounting keep that promise.
Three separate constants, a fourth in `shepr-remote` (the 15-second per-command
discovery budget the comment cites), and the CLI's user-facing wording all have
to agree, and nothing in the build would notice any of them drifting. The hunter
calls it a careful, correct comment about an unenforced invariant - which is the
finding.

Enforcement named, cheap: `const _: () = assert!(ATTEMPT_BUDGET.as_secs() <
MAX_RETRY_DELAY.as_secs());` plus a test asserting the CLI's reconnect message
quotes `MAX_RETRY_DELAY` rather than a literal `30`. The `shepr-remote` hunter
reports the same relation from the other side and adds that `supervisor.rs`
already applies half of this pattern (`ATTEMPT_BUDGET < MAX_RETRY_DELAY` is
asserted there), and that the fix is to export
`shepr_remote::NONINTERACTIVE_SSH_COMMAND_TIMEOUT` and the handshake timeout and
define `ATTEMPT_BUDGET` in terms of them, since changing the SSH timeout to 30 s
silently invalidates the 25 s budget and the documented argument for it with no
test failing.

## HYGG-123 - `ClientProcessRole::from_env` is the model for environment resolution and nothing holds the others to it

`shepr-client`. `from_env` enumerates the accepted values, treats absent as
`Local`, and refuses startup on anything else, including non-UTF-8. It is the
only environment read in `shepr-termio` plus `shepr-client` that follows that
pattern; the hunter records it here as the enforceable model rather than as a
defect.

The three that do not: `input/model.rs::host_modify_other_keys_mode()` reads
`TMUX`, `TERM_PROGRAM` and `WEZTERM_PANE` when
`setup_terminal_with_capabilities` happens to run, not at launch resolution, so
the values never reach `ClientSettings`, nothing in the client can report what
host protocol it decided on, and the decision is invisible to every test of
terminal setup. Within that one function there are three different resolution
rules, none stated: `WEZTERM_PANE` uses `var_os(...).is_some()` (empty counts as
set), `TMUX` uses `var(...).is_ok()` (empty also counts), and `TERM_PROGRAM`
compares case-insensitively.

Enforcement named: move all env resolution into one launch module and forbid
`env::var` elsewhere by text rule - cheap today, since the two crates have only
four production `env::var` call sites. The same rule shape recurs across the
hunt (HYGG-053, HYGG-068, HYGG-110).

## HYGG-124 - `HostModes::apply_mouse` records the restore flag only when a parameter that means something else is true

`shepr-client`/`shepr-termio`. `apply_mouse` records
`RESTORE_MOUSE_CAPTURE` only when called with `reassert == true`. Both setup
paths do pass `true`, so it holds today - but the flag that decides whether
mouse capture gets turned off at exit is set by a parameter that means
"re-send even if unchanged". A future caller with `reassert: false` that enables
capture leaves the user's terminal in mouse mode after shepr exits.

The surrounding structure is what makes it unverifiable: `HostModes` guards
`HostModesState` behind a `Mutex` and `restore_state` behind an `AtomicU8`, with
a comment explaining that the panic hook must restore without taking a lock the
panicking thread may hold - deliberate and sound for the panic case - but it
means the restore intent and the mode state are kept consistent only by each
setter remembering to call a recorder before and after its write, and
`set_keyboard_enhancement_flags`, `set_direct_keyboard_protocol` and
`set_modify_other_keys` each do that pairing slightly differently (the first
records `false` for modify-other-keys on success unconditionally; the second and
third record the computed value). Whether those three agree is not checkable
from the types. The hunter says it is not mechanically enforceable and the
lock-free restore path is worth keeping; the honest statement is that the
recorder pairing is a convention maintained by three call sites, and a single
`set_keyboard(...)` entry point that computes the flags itself would reduce it
to one.
