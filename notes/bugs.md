# Defects found by the hygiene hunt

Defects turned up by the hygiene hunt over the nine workspace scopes
(`shepr-core`/`shepr-platform`, `shepr-vt`/`shepr-pty`, `shepr-agent`,
`shepr-protocol`/`shepr-config`, `shepr-api` and the root binary, `shepr-remote`,
`shepr-mux`, `shepr-server`, `shepr-termio`/`shepr-client`). This is a working
document and it may be wrong: no hunter ran a build or a test, so every finding
here comes from reading code, and some are explicitly predictions or inferences
rather than observed behaviour. Those caveats are kept inside each entry. A fix
pass should expect phantoms.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## BUG-003 - The one type that encodes the pane cwd rule is unreachable from every writer

**Decision (partial):** the `Path::exists` seal is extended to `Path::is_file`
and `Path::is_dir`, so `UsableCwd::new`'s `path.is_dir()` becomes a metadata
match that tells a missing directory apart from one that cannot be stat'ed.
Open: the defect, promoting `UsableCwd` and routing every writer through it.

`crates/shepr-mux/src/pane/cwd.rs` defines `UsableCwd` (absolute and `is_dir`),
and `pane/runtime.rs::usable_reported_cwd` throws the type away one line later
(`UsableCwd::new(cwd).map(UsableCwd::into_path_buf)`), so the guarantee never
propagates. `TerminalState::cwd` is a bare `pub PathBuf` written directly from
four `shepr-server` sites that do no checking at all: `app/api/workspaces.rs`
(two sites), `app/api/layouts.rs`, `app/api/tabs.rs`, `app/actions/events.rs`.
`UsableCwd` is `pub(super)` inside `pane`, so those callers cannot use it even if
they wanted to. `WorkspaceSnapshot::identity_cwd` and `PaneSnapshot::cwd` are
`PathBuf` with no validation either.

Fix suggested: promote `UsableCwd` to the crate root, make `TerminalState::cwd`
private behind `cwd()` / `set_cwd(PaneCwd)`, and deserialize `PaneSnapshot::cwd`
through it.

## BUG-004 - `impl Deref for Workspace` aborts the server on state the API can reach

`crates/shepr-mux/src/workspace.rs`: `deref` does
`self.tabs.get(self.active_tab).expect("workspace must have a tab when implicitly
dereferenced")`. Every `Tab` method is silently available on `Workspace`, and the
one-tab invariant is enforced by an `expect` in a `Deref` impl. `active_tab` is
`pub` and `tabs_mut()` hands out `&mut [Tab]` to any crate, so a server-side
caller can put `active_tab` out of range and the next `ws.panes` - which reads as
a field access - aborts the server.

Fix suggested: delete the `Deref`/`DerefMut` impls, make `active_tab` private,
and require the existing `active_tab()` / `active_tab_mut()` which return
`Option`. Related: the crate's one-tab invariant checker
`Workspace::assert_invariants_for_test` is called only from individual tests,
never after a production mutation, so the `Deref` panic plus those opt-in calls
are the whole enforcement today.

## BUG-008 - A pane can freeze silently with the child still alive

`ReaderExit::Closed` in `crates/shepr-pty/src/actor.rs` covers EOF but also poll
failure and wake-pipe drain failure, both logged at `debug!`. The mux ignores
`Closed` because it expects the child watcher to report. When the loop ends for
one of those two reasons nobody reads the PTY: the child blocks on a full PTY,
nothing is reported, and the only trace is a debug line.

Fix suggested: a third `ReaderExit` variant (for example `Failed(io::Error)`) the
owner must handle.

## BUG-012 - Poisoned-lock paths fabricate values and silently drop writes

- `synchronized_output_state` returns `(true, 0)` on a poisoned core - a made-up
  value rather than an error (`crates/shepr-vt`, reported from the vt/pty scope).
- `PaneTerminal::seed_history_ansi` returns `()` and silently does nothing when
  the core lock is poisoned, so restored scrollback is lost with no line
  anywhere.
- `GhosttyPaneTerminal::resize`, `scroll_up`, `scroll_down`, `scroll_reset` and
  `set_scroll_offset_from_bottom` use `if let Ok(mut core) =
  lock_terminal_core(...)` and drop the operation on a poisoned lock. The doc
  comment on `GhosttyPaneTerminal::core` justifies this policy for readers
  ("readers answer empty or default values rather than error"); it says nothing
  about writers, and a dropped resize is not a stale read.

## BUG-016 - The three bun test files never run

**Decision:** deferred; tracked by the "Resolve typescript question" item in
`notes/todo.md`. Not handled in the hygiene fix pass.

`crates/shepr-agent/src/integration/assets/shepr-agent-state.test.ts`,
`assets/opencode/shepr-agent-state.test.ts` and
`assets/opencode/shepr-tui-session.test.ts` import `bun:test`. There is no
`package.json`, no bun or vitest config, and `brokkr.toml` runs cargo only. They
read as coverage for the JavaScript and TypeScript hook assets (Pi, OMP,
opencode, Kilo) and provide none. They also write sockets into the system temp
directory and mutate `process.env` globally. `notes/todo.md` has an open item
("Resolve typescript question"), so this is known.

Fix suggested: wire a bun step into `brokkr check` or delete the files.

## BUG-048 - Discarded cleanup failures leak private directories and temporary files

`crates/shepr-platform/src/ipc.rs::bind_via_private_staging` leaks a 0700 staging
directory per failed `remove_dir` (`let _ =`, no log, one per bind attempt, in
the XDG runtime directory); `ipc.rs` also discards the `remove_file` after a
failed restrict. `ssh_paths.rs::create_remote_ssh_config_dir` creates
`shepr-ssh-<pid>-<token>` directories and never removes them, with the doc
calling them "ephemeral" and the caller responsible; nothing sweeps stale ones
from a killed process. `ssh_agent.rs` discards the `remove_file` of the temporary
symlink and of the published path on drop, and if `symlink` succeeds and the
process dies before `rename` the temporary stays. `logging.rs`'s
`set_permissions` failure when tightening a world-readable log is also discarded -
the one place where failing quietly means the log stays readable by others.

Fix suggested: make the `let _ =` sites explicit, and a startup sweep keyed on
"own uid, no live pid".

## BUG-062 - Identity strings collapse to a constant on a before-epoch clock

- `crates/shepr-server/src/server/headless.rs`: `client_shell_boot_id:
  format!("{}-{}", std::process::id(),
  SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos())`.
  `unwrap_or_default()` means a clock before the epoch collapses every boot id to
  `pid-0`, silently defeating the stale-boot rejection it exists for. The format
  for the whole boot-generation mechanism (compared in `client_commands.rs`,
  `client_transport.rs`, `surface_reuse.rs` and four places in `shepr-client`)
  lives in a `format!` inside a struct literal, because `shepr_protocol::BootId`
  is a newtype over `String` with `From<String>` and no owning constructor.
- `crates/shepr-protocol/src/ids.rs`: `TerminalId::alloc()` combines a
  process-global `AtomicU64` (`Relaxed`) with `SystemTime::now()`, and
  `duration_since(UNIX_EPOCH)` falls back to `.unwrap_or(0)`, at which point ids
  become `term_<counter>` only. Uniqueness rests on the clock being monotonic
  across the process or the counter never wrapping.

Fixes suggested: `BootId::for_this_process()` in `shepr-protocol` with
`From<String>` restricted to deserialization; own the terminal id counter in a
struct passed to callers.

## BUG-073 - Tests that skip themselves when run as root and report success

- `crates/shepr-platform/src/tests.rs::config_metadata_preserves_ownership_and_acl_without_inheriting_extra_access`:
  the `fchown` only happens `if effective_uid() == 0`, so unless the suite runs
  as root source and destination were created by the same uid/gid and the
  ownership half of the assertion cannot fail. `brokkr check` does not run as
  root, so the `fchown` plus `EPERM` tolerance in `config_file.rs` is untested
  while the test's name advertises it. (The ACL half is real.)
- `crates/shepr-mux/src/pane/runtime.rs::process_cwd_does_not_require_traversing_the_directory_path`
  prints "skipping untraversable cwd assertion for privileged test process" to
  stderr and passes green when run as root. The notice goes to stderr, which the
  harness hides on success. The vt/pty hunter reported the same test from their
  side.
- `crates/shepr-remote/src/remote/local_server.rs::is_server_listening_returns_permission_errors_instead_of_false`
  returns early when running as root, so it silently passes as a no-op in a root
  container.
- `crates/shepr-mux/src/git/discovery.rs::git_rev_parse_verify_reads_reftable_refs`,
  `git/status.rs::branch_reads_unborn_symbolic_head_from_reftable_repo` and
  `git/status.rs::git_status_fingerprint_reads_reftable_branch_identity` each
  `return` right after `git init --ref-format=reftable` if that command fails,
  so a host `git` too old for `extensions.refstorage` (or built without
  reftable support) makes all three pass having exercised nothing.

Fixes suggested: `#[ignore]` with a stated reason, or fail loudly when the
privilege condition is not met, or restructure so privilege is not needed. For
the reftable trio: assert the `git init` succeeded, or `#[ignore]` with a
reason naming the required `git` version, rather than returning silently.

## BUG-074 - Tests that reach the developer's real environment

**Decision (partial):** the first bullet is piece 1 of the test-isolation work
adopted from broadarrow (the `shepr-core` environment registry): every agent
variable is an entry and `IsolatedEnv` isolates from the registry, replacing the
hand list. In the second, piece 1 bans the raw `std::env::var("HOME")` and
piece 2 (scratch under the project's `target/` tree) supplies the existing cwd.
Open: the fixed `/tmp/this-directory-does-not-exist-...` literal standing in for
a missing cwd.

- `crates/shepr-agent/src/integration/tests.rs::clear_integration_path_env` is a
  hand-maintained list of fifteen variables to remove so paths resolve against
  the fake `HOME`, while `integration/env.rs` defines fourteen `*_ENV_VAR`
  constants plus the two XDG names. Add an agent env var and forget this list and
  every install test for that agent silently inherits the developer's real value:
  the test passes on the author's machine, writes into the author's real agent
  config, and means nothing.
- `crates/shepr-server/src/app/snapshot_tests.rs` uses
  `PathBuf::from("/tmp/this-directory-does-not-exist-for-shepr-test")` and
  `std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/tmp"))`
  with no `IsolatedEnv` - the project's own rule broken twice over. The test
  means "one pane with a missing cwd, one with an existing cwd" and gets that
  from the developer's `$HOME`; if `$HOME` is unset the fallback silently changes
  the test's meaning.
- Git discovery now reads `~/.gitconfig` and the XDG git config (for
  `core.bare`). Every test under `crates/shepr-mux/src/git/` holds an
  `IsolatedEnv`, but tests elsewhere that reach discovery
  (`crates/shepr-mux/src/workspace.rs`, `crates/shepr-server/src/app/git_refresh.rs`)
  were not checked and may read the developer's real git config.
- `ValidatedConfig::from_values` now resolves the shell at launch and reads
  `SHELL` and `PATH`; test helpers that call it without an `IsolatedEnv` (for
  example `crates/shepr-test-fixtures/src/config.rs`) read the developer's real
  values.

Since the config directory name no longer changes under `cfg!(test)`, a test
that reaches `AppPaths::resolve()` without an `IsolatedEnv` resolves the real
`~/.config/shepr`, which is what makes this class dangerous rather than merely
untidy.

## BUG-075 - Tests whose assertions are races against the wall clock

**Decision (partial):** the clock seam is adopted from broadarrow, incrementally
as part of the hygiene work: time is passed in rather than read inside logic,
held per subsystem by scoped textlints in the shape of broadarrow's
`control-loop-reads-the-clock-seam` (HYGP-001). That is the injection point the
common cause below names. Open: each of the four tests, as its subsystem gets
the seam.

Called out as flaky (not merely slow) by their hunters:

- `crates/shepr-client/src/shell/input/input.rs`: one clipboard test sleeps
  400 ms inside a fake reader and asserts `started.elapsed() <
  Duration::from_millis(300)` - on a loaded machine a coin flip, and the 400 ms
  is paid on every run.
- `crates/shepr-agent/src/integration/version.rs::version_probe_deadline_includes_inherited_stdout`
  asserts `elapsed < 250ms` after a real 300 ms sleep and spawns `/bin/sh`.
- `crates/shepr-vt/src/tests.rs::synchronized_output_buffers_until_end_or_timeout`
  sleeps through vte's 150 ms timeout and asserts
  `!flush_expired_synchronized_output` immediately after a write, which the
  hunter said will flake under load.
- `crates/shepr-remote/src/remote/ssh_agent.rs::registration_retries_when_the_api_is_initially_missing`
  polls at 10 ms against a 5-second wall-clock deadline and depends on thread
  scheduling.

The common cause named across reports is that these timeouts are `Instant`-based
constants with no injection point; `SshAgentLease::refresh_at(now)`,
`EndpointCatalogWatch::poll(now)` and `shepr-mux`'s `terminal/state` are cited as
the pattern that works.

## BUG-010 - Launch validation of the configured shell checks mode bits, not access

The shell is now resolved at launch through `shepr-core/src/shell.rs`: config
rejects an invalid configured shell, an unusable or unrecognised inherited
`$SHELL` falls back to `/bin/sh`, and the resolved path travels to the pane
builder. Open: config validation checks executable mode bits, while the PTY
checks `access(2)` before exec, so a shell on a `noexec` mount passes launch
validation and still fails every spawn. Exact access validation at launch needs
a platform helper reachable from `shepr-config`, which means a dependency-rule
change in `brokkr.toml`.

## BUG-025 - The CLI process installs no tracing subscriber before the launch dispatch

`auto_detect_launch` has three info log sites (`"auto-detect launch starting"`,
`"server already running, attaching as client"`, `"no server running, spawning
server daemon"`) that run before the client callback installs the file logger,
so they reach no subscriber. Installing a logger early in `main` would make the
client's own later `try_init` fail, so the fix is to change the client logger
startup contract (`crates/shepr-client/src/lib.rs` and
`crates/shepr-platform/src/logging.rs`) so `main` installs once and the client
reuses it. A comment in `src/autodetect.rs` records the conflict.

## BUG-054 - `public_workspace_id` answers an invalid index with an empty string

`crates/shepr-server/src/app/ids.rs` warns and returns an empty `String` for a
missing index, and the `""` flows into public ids and API responses as a
valid-looking value. The sibling `public_tab_id` and `public_pane_id` return
`Option<String>`. Changing the return type touches about 50 call sites across
12 files in `shepr-server` (`app/events.rs`, `creation.rs`, `tab_bar_status.rs`,
`api/workspaces.rs`, `api/tabs.rs`, `api/panes.rs`, `api/panes/geometry.rs`,
`api.rs`, `api/layouts.rs`, and tests under `api/panes/tests.rs` and
`server/headless/tests/`), so it needs a fixer with the whole crate in scope.

## BUG-059 - Two `#[cfg(test)]` shortcuts make every agent-hosting assertion unfalsifiable

`crates/shepr-server/src/app/agents.rs`: under `#[cfg(test)]`,
`available_shell_name` returns `Some("sh")` and `runtime_hosts_agent` returns
`true` whenever `runtime.child_pid().is_none()`, so tests with a childless
`PaneRuntime` see every agent as hosted and unit tests exercise different code
from the crate's dependents. The fix needs a probe seam in `PaneRuntime`
(`crates/shepr-mux/src/pane/runtime.rs`) and changes to the tests that lean on
the shortcut: the OpenCode prompt, Copilot prompt and Pi key tests in
`app/api/agents.rs`, and the agent-start retry test in `app/mod.rs`. Production
call sites: two in `app/api/agents.rs`.

## BUG-064 - Shepr's git config reader ignores git's config-location variables

`crates/shepr-mux/src/git/config.rs::git_user_config_paths` reads only the XDG
and `~/.gitconfig` global files, while the `git` subprocesses honour
`GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM`, `GIT_CONFIG_NOSYSTEM` and read
`/etc/gitconfig`, so the two paths can report different upstreams and a
different `core.bare` for one repository. Honouring them needs registry entries
in `crates/shepr-core/src/env.rs` first (raw environment reads are sealed). A
comment at the reader records this.

## BUG-067 - A process-global teardown counter couples two servers in one process

`crates/shepr-mux/src/pane/teardown.rs` still counts in-flight pane teardowns in
a process-wide static, so one server's shutdown wait blocks on another server's
teardowns (the test suite runs several). A panicking teardown does not leak a
count (the RAII guard drops on unwind), and an unmatched decrement now warns
instead of saturating silently. Open: a server-owned tracker, which needs
`crates/shepr-mux/src/pane/runtime.rs` to hand it to teardown work and
`crates/shepr-server/src/server/headless.rs`'s shutdown to wait on it.

## BUG-087 - A queued manifest reload can install before the previous reload's summaries apply

`crates/shepr-server/src/server/headless/api_dispatcher.rs`: completing a
manifest reload starts the next queued reload before applying the completed
one's summaries. If the next worker installs its registry first, detection
briefly runs the newer rules while the server's reload summaries still describe
the older ones.

## BUG-088 - The API's busy refusal carries an empty request id

`crates/shepr-api/src/server.rs` refuses a connection over the admission cap
with an `endpoint_busy` response sent before the request is read, so the
response's `id` is empty. shepr's own client does not check it, but the
response does not correlate with the request that was refused.

## BUG-090 - `status` over `--machine` reports the local client's version as the client

Now that `status` overview may run against a saved machine, its "client"
section still prints the local binary's version and build, beside the remote
server's. Read over `--machine`, that reads as the remote host's client.
Either label it as the local client or omit the section for a remote target.

## BUG-091 - The timer's deferred cwd publish can race the reader's

The synchronized-output timeout task runs its deferred effects, including the
OSC 7 cwd publish, on a blocking thread, while the PTY reader publishes on the
actor thread, so an older cwd can land after a newer one. The same race existed
before the reply-lock work; it is recorded now that the effects are explicit
values that could carry an ordering.

## BUG-092 - The unchanged-history skip rarely fires for tabs with several panes

`crates/shepr-mux/src/persist/snapshot.rs`: `TabHistorySnapshot::panes` is a
`HashMap` rebuilt on every save, and each new map iterates in its own random
order, so identical history serialises to different bytes and a different
digest. The writer's skip-unchanged check therefore misses, and
`session-history.json` is rewritten and fsynced on every save for any tab with
more than one pane. A `BTreeMap` (or sorted serialisation) makes the bytes
deterministic.

## BUG-093 - `config check` stops at a shell error and hides a bad `new_cwd`

`crates/shepr-config/src/validated.rs`: `ValidatedTerminalConfig::parse`
returns at the first shell error, so a problem with `terminal.new_cwd` is only
reported after the shell is fixed. `config check` exists to list every problem
at once.
