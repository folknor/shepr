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

## BUG-067 - A process-global teardown counter couples two servers in one process

`crates/shepr-mux/src/pane/teardown.rs` still counts in-flight pane teardowns in
a process-wide static, so one server's shutdown wait blocks on another server's
teardowns (the test suite runs several). A panicking teardown does not leak a
count (the RAII guard drops on unwind), and an unmatched decrement now warns
instead of saturating silently. Open: a server-owned tracker, which needs
`crates/shepr-mux/src/pane/runtime.rs` to hand it to teardown work and
`crates/shepr-server/src/server/headless.rs`'s shutdown to wait on it.

## BUG-008 - A pane can freeze silently with the child still alive

`crates/shepr-pty/src/actor.rs`: a hard poll failure and a wake-pipe drain
failure each log at `debug` and leave the IO loop, and the common exit reports
them as `Closed`. The mux ignores `Closed` and waits for the child watcher, so
with the child still alive nobody reads the PTY, the child blocks on a full
PTY, and the only trace is a debug line. `EINTR` is already retried. The fix
needs a distinct reason in `crates/shepr-platform/src/child_io.rs`'s
`ChildExitReason` (mapping these to an existing reason would misstate the
checkpoint policy), handled by the runtime. A comment at the actor records
this.

## BUG-064 - Git command-scope config variables are not modelled

The file reader now honours `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM`,
`GIT_CONFIG_NOSYSTEM` and `/etc/gitconfig` as git does. Open: git's
command-scope config (`GIT_CONFIG_COUNT` with `GIT_CONFIG_KEY_n` and
`GIT_CONFIG_VALUE_n`) overrides file values and is inherited by shepr's git
subprocesses but not modelled by `crates/shepr-mux/src/git/config.rs`, so the
two paths can still disagree when it is set. It is a dynamic variable family, so
the environment registry and `IsolatedEnv` need a way to cover it.

## BUG-095 - A relative Git override path silently falls back to the default files

`crates/shepr-mux/src/git/config.rs::git_user_config_paths` discards errors
from reading `GIT_CONFIG_GLOBAL` and `GIT_CONFIG_SYSTEM`. The registry refuses a
relative value, so a relative override silently falls back to the default
global and system files, where git itself would use the given path. The two
readers then disagree about which config applies.

## BUG-096 - A tab dropped late in restore may already have started shells and queued history

`crates/shepr-mux/src/persist/restore.rs::restore_tab`: a tab rejected late (all
panes pruned, or refused by `from_saved`) may already have queued
`history_carry` entries or started shells for panes that are then discarded.
The invalid-ratio rejection returns before any of that; the later rejections do
not. Also untested: the server wiring in `crates/shepr-server/src/app/mod.rs`
that turns a nonzero `dropped_tabs` into a backup of the original session file
on the first save (the `with_paths` construction path).

## BUG-094 - Two stale or misleading clipboard statements in the client

- `crates/shepr-client/src/shell/input/input.rs` says the platform clipboard
  reader has no timeout; it applies a two-second helper deadline.
- `crates/shepr-client/src/lib.rs` logs `data.len()` for a server-forwarded
  clipboard payload as if it were the clipboard size; it is the base64-encoded
  length, not the decoded byte count.
