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

## BUG-005 - The sync-timeout timer breaks the documented reply ordering guarantee

`crates/shepr-mux/src/pane/runtime.rs` (`rt.spawn` -> `spawn_blocking`): the
timer calls `flush_expired_synchronized_output`, which produces replies outside
the actor's `response_order` lock, then queues each reply with its own
`write_terminal_response(|| Some(response))` call. A PTY read on the actor thread
can parse and queue newer replies in between, so older replies land behind newer
ones. `PtyIoActorHandle::response_order` documents exactly the guarantee this
breaks ("replies enter the inbox in the order the terminal produced them");
nothing enforces it because the closure-based API accepts an already-computed
value.

Fix suggested: make the only way to queue a reply
`write_terminal_responses(|| -> Vec<Bytes>)`, run the flush inside that closure,
and remove the single-value form.

## BUG-006 - Render and read paths mutate the terminal

`crates/shepr-mux/src/pane/terminal/backend.rs`: `render()`,
`collect_dirty_patch()`, `synchronized_output_active()` and
`synchronized_output_state()` all call `flush_expired_synchronized_output`, which
feeds the buffered frame through the parser, changes the grid, and queues replies
and clipboard writes. This violates AGENTS.md's "Render is pure". These flushes
do not bump `content_seq` or `detection_content_seq` - the timer path bumps
`detection_content_seq` deliberately - so a frame flushed by a render is
invisible to detection's change counter, and the replies wait until the next read
or timer.

Fix suggested: one `tick(now)` entry point on `Terminal` owned by the runtime,
with render paths given `&Terminal` only.

## BUG-007 - The event loop can block on `/proc` scans behind the reply-order lock

`read_chunk` in `crates/shepr-pty/src/actor.rs` holds `response_order` across the
whole `on_read` callback, which runs `apply_process_result`, which does
`resolve_default_color_owner` (a `/proc` scan) and `publish_reported_cwd` (a
readlink via `process_cwd`). `PaneRuntime::resize` and
`apply_host_terminal_appearance` take the same lock from the app side, so they
block behind another thread's `/proc` walk. The comment in `backend.rs` says the
caller releases the terminal and content locks before the scan; the reply-order
lock is still held.

Fix suggested: return the effects from `on_read` and run them after the lock is
released, and document the lock order (response_order > content_write_lock >
core) in one place.

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

## BUG-021 - `app_dir_name()`'s `cfg!(test)` guard does not hold outside its own crate

**Decision (partial):** piece 1 of the test-isolation work adopted from
broadarrow (the `shepr-core` environment registry, after broadarrow's
`core::env`) makes the protection that actually works complete by construction:
`IsolatedEnv` isolates from the registry, and raw reads are banned in
`clippy.toml`, so no variable that steers a path can be missed. The
`cfg!(debug_assertions)` arm is removed (owner's decision, with the
`debug_assert!` ban adopted from broadarrow), so dev and release builds resolve
the same directory; that also removes the separate dev runtime directory that
`AGENTS.md`'s recipe for testing a dev build inside a running shepr relies on.
Building the release profile in `brokkr check` is decided against. The text
below predates `brokkr.toml`'s `[test] debug = true`: `brokkr test` builds dev
unless `--release` is passed, so "builds release by default" no longer holds.
Open: the `cfg!(test)` guard and its false comment.

**Decision:** with the switch gone, a dev run is kept apart from the installed
server by an explicit session (`--session <name>`), not by a directory, and a
build mismatch must always be refused, with a message. Release builds also get
`overflow-checks = true` in the root `Cargo.toml` profile. Together with the
`debug_assert!` ban, that removes the dev/release behaviour difference without
building release in the gate (the gate still does not build it). A read-only
check of the mismatch paths found that every local path reaching a server of
another build refuses before any codec frame: the build-identity preamble on
the client socket, `validate_running_server_compatibility` in
`auto_detect_launch`, and `ensure_server_build_matches` on each API command.
With saved machines, though, the session-aware guidance is lost (BUG-025).
Open here, all created or exposed by the removal:

- `build.rs` fingerprints source files only (root `Cargo.toml`, `Cargo.lock`,
  `build.rs`, `src/`, `crates/`), never the profile. A dev and a release build
  of the same tree therefore share one `BUILD_ID`, and `brokkr run` straight
  after `brokkr install` attaches to the installed server without a word.
  Broadarrow's build cohort (`build-stamp`) also hashes the profile,
  `OPT_LEVEL`, `DEBUG`, the rustflags, the compiler version and `CARGO_CFG_*`.
  Once the three decisions above land, the two builds behave alike, so nothing
  breaks on the wire. But which server answers a dev run depends on whether
  the tree has moved since the install. Hashing `PROFILE` (or the cohort's
  profile inputs) as well makes the refusal unconditional.
- The refusal text (`shepr_api::session::restart_after_update_guidance`) offers
  only the destructive fix: stop the running server, which "exits pane
  processes". For a dev run in the default session, that server is the
  installed one with every live agent in it. The text should also offer running
  the build in its own session (`--session <name>`).
- `server stop` and `session stop` skip the build check on purpose
  (`src/cli/server.rs::server_stop`), so a dev build's `server stop` run
  without `--session` now stops the installed server, with no refusal.
- Without `--session`, a dev build shares the default session's saved layout
  and history with the installed one. If the installed server is down, a dev
  server restores from them and saves over them. Config and the saved-machine
  catalog (`state_dir/client/endpoints.json`) are shared whatever the session.
- `AGENTS.md`'s recipe for testing a dev build inside a running shepr
  (`env -u SHEPR_SOCKET_PATH -u SHEPR_CLIENT_SOCKET_PATH brokkr run --
  <command>`) now lands on the installed server's default-session socket and
  is refused. It needs `--session <name>`. Panes now export a non-empty
  `SHEPR_SOCKET_PATH`, so the empty-override refusal no longer bites. An inherited `SHEPR_SESSION` does
  not work as the switch: `ServerAddress::resolve_paths` lets only an explicit
  `--session` outrank a non-empty socket override.

`crates/shepr-config/src/io.rs`:
`if cfg!(test) { "shepr-test" } else if cfg!(debug_assertions) { "shepr-dev" } else { "shepr" }`,
with a comment claiming unit tests get a directory of their own in every profile.
`cfg!(test)` is per-crate: it is true only while compiling `shepr-config`'s own
unit tests. A test in `shepr-server`, `shepr-client` or `shepr-remote` that
reaches `AppPaths::resolve()` compiles `shepr-config` as a normal dependency, and
`brokkr test` builds release by default so `debug_assertions` is off too - the
directory name is `shepr`, the real `~/.config/shepr`. What prevents damage is
`IsolatedEnv` pointing `HOME`/`XDG_*` at scratch: discipline, not the `cfg!`. The
hunter called this the most dangerous false claim in their scope.

Fix suggested: a positive signal (for example a `SHEPR_TEST_DIR_NAME` set by
`shepr-test-support`) that `app_dir_name()` honours, so isolation is opted into
and a wrong name is loud.

## BUG-028 - `status`'s machine refusal message is wrong and malformed

`src/cli/status.rs` / `src/cli.rs`: `status` (overview) calls
`read_server_runtime_status`, which sends `ping` over the API, yet reports
`is_api_command() == false`. With `--machine`, the user gets "`status ` is not an
API-backed machine command" - with a dangling space, because
`Command::Overview.name()` returns `""`, the same value `Command::Invalid`
returns. Two distinct states share one name string and the message asserts
something untrue about the command. The `""`-for-two-states pattern repeats in
every `src/cli/*.rs` `name()`.

Related, from the same hunter: `is_api_command` has no single owner -
`src/cli.rs::CliCommand::is_api_command` hardcodes `false` for `Config`,
`Machine`, `Session` and `Integration` while seven other groups delegate to
per-module methods, so an `integration` or `machine` subcommand that ever becomes
API-backed is blanket-blocked with no test failing.

Fix suggested: classify "may this run against a remote machine" rather than "is
this API-backed", and make `name()` unable to return `""`.

## BUG-035 - The client protocol socket skips the startup lock its own contract requires

`shepr_platform::ipc` documents the order ("Acquire this before
`prepare_socket_path` and keep it until the listener"). Three callers implement
it: `crates/shepr-remote/src/remote/bridge.rs` (lock, prepare, bind, identity,
plus a redundant `restrict_socket_permissions(0o600)`),
`crates/shepr-api/src/server.rs` (lock, prepare, bind, identity), and
`crates/shepr-server/src/server/headless.rs` for the client protocol socket -
which takes no startup lock. That is the socket the whole TUI attaches to, and
the omission is exactly the race the lock exists to close.

Fix suggested: one
`shepr_platform::ipc::bind_private_socket(path, busy_message) -> (Listener,
SocketStartupLock, SocketFileIdentity)` so the wrong order is unrepresentable.

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

## BUG-054 - `public_workspace_id` answers an invalid index with an empty string

`crates/shepr-server/src/app/ids.rs` documents this as deliberate ("a stale one
is a caller bug, reported and answered with an empty id rather than a panic") and
it does warn, but the empty `String` then flows into public ids and API responses
as a valid-looking value: a `""` workspace id in a response is
indistinguishable from a real one to the client. The sibling functions
`public_tab_id` and `public_pane_id` return `Option<String>`.

## BUG-055 - One not-found condition answered with two different API error codes

`crates/shepr-server/src/app/api/layouts.rs` answers a missing workspace with
`ApiErrorCode::WorkspaceNotFound` in one place and, in the apply path, with
`ApiErrorCode::LayoutApplyFailed` plus the message `"workspace not found"`. A
client switching on the code sees two classes for one fact. The hunter marked
this a divergence today, not a prediction.

Fix suggested: a single `resolve_workspace(...) -> Result<usize, ApiError>` that
owns the code, plus a table test over the layout handlers.

## BUG-059 - Two `#[cfg(test)]` shortcuts make every agent-hosting assertion unfalsifiable

`crates/shepr-server/src/app/agents.rs`: `available_shell_name` returns
`Some("sh")` and `runtime_hosts_agent` returns `true` whenever
`runtime.child_pid().is_none()`, under `#[cfg(test)]`. Any test using a
`PaneRuntime` without a live child - which is most of them, including every
`PaneRuntime::test_with_screen_bytes` fixture - gets `runtime_hosts_agent == true`
for every agent, so assertions that a pane hosts the expected agent cannot fail.
The shortcut is gated on `cfg(test)` only while the rest of the crate gates test
affordances on `any(test, feature = "test-api")`, so the root binary's
integration tests see the production path and unit tests see the shortcut: the
two suites test different code.

Fix suggested: inject the probe (a `ProcessProbe` trait or an `Option<fn>` on the
runtime) rather than branching on `cfg(test)`.

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

## BUG-064 - The file-reading Git path and the subprocess Git path can disagree about one repository

**Decision:** shepr's own git discovery now honours `GIT_CEILING_DIRECTORIES`
with git's semantics - it was the one upward walk that ignored it - and
`IsolatedEnv` sets the ceiling to the scratch base. This entry's divergence
(`GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM`, `GIT_CONFIG_NOSYSTEM`, `/etc/gitconfig`)
is unrelated and stays open.

`crates/shepr-mux/src/git/config.rs::git_user_config_paths` reads
`XDG_CONFIG_HOME` directly, filters on `is_absolute()`, and falls back to
`~/.config/git/config`. It does not honour `GIT_CONFIG_GLOBAL`,
`GIT_CONFIG_SYSTEM` or `GIT_CONFIG_NOSYSTEM`, and never reads `/etc/gitconfig`,
while the `git` subprocess invoked from `git/discovery.rs` and `git/status.rs`
honours all of them. So for one repository the two paths can report different
upstreams and a different `core.bare`. The hunter called this a divergence in
fact, not a prediction.

Fix suggested: a test comparing `read_config`'s answer against `git config --get`
for a repo with a global config and a `GIT_CONFIG_GLOBAL` override - "worth
writing; it will fail today".

Related, checked against git 2.53: git resolves `core.bare` through includes and
the global config (shepr now matches), but ignores it when a work tree exists.
`GitWorktreeInfo::is_bare` can therefore read true for a normal checkout whose
config includes `core.bare = true`. Only tests read the field today; either give
it git's work-tree rule or delete it.

## BUG-065 - Every Git failure is `None`, so a missing `git` binary is retried forever in silence

**Decision (partial):** the `Path::exists` seal is adopted from broadarrow
(`clippy.toml`; `try_exists` or a match on `NotFound`), which makes
`RefFileRead`'s `Absent` / `Unavailable` split the house rule rather than
reasoning the caller may discard. Open: the defect - carrying the distinction
to the refresh task, and the client's copy.

`crates/shepr-mux/src/git`: every Git read returns `Option`. Spawn failure,
non-zero exit, non-UTF-8 output, a permission error on `.git/config` and a
64 KiB-exceeding ref file all become `None`, and `None` means "this repo has no
branch". `discovery.rs`'s `RefFileRead` enum goes to real trouble to distinguish
`Absent` from `Unavailable` (with a careful comment about `Path::exists()` lying
on metadata errors) and its single caller `read_git_ref_file` immediately
collapses both to `None`. `git_status_snapshot_for_cwd_with_demand` then applies
the same 30-second retry to "not a repo" and to "git is broken", so if `git` is
not on the server's `PATH` the sidebar shows no branch forever, with no
diagnostic, retried once per workspace per 30 s.

The client has a fourth copy of the same shape:
`crates/shepr-client/src/workspace_label.rs::derive_label_from_cwd` does
`.output().ok().filter(|o| o.status.success()).and_then(|o|
String::from_utf8(o.stdout).ok())`, so "git missing", "git failed", "non-UTF-8"
and "not a repo" all become the same answer, which changes the label offered to
the user, with nothing logged.

Fix suggested: a `GitReadError` travelling to the refresh task, logged once per
distinct cause rather than per attempt, with `Absent` staying `None`.

## BUG-066 - `git` is spawned with the environment and stdin inherited whole, and no deadline

**Decision (partial):** `git/test_support.rs::run_git` is gone; fixtures write
plain files or, where only Git can write the fixture (reftable stores, linked
worktrees), go through `git_written_fixture`, which is exempt as a
host-program-ok site since the fixture itself is what the test exercises. The
child working-directory seal is adopted from broadarrow (`clippy.toml` on
`std::process::Command::new`), so the production spawns must state a working
directory, which the suggested `run_git(dir, ..)` can set from `dir`. The two
`Instant::now()` reads fall under the clock seam, adopted incrementally with
the hygiene work (HYGP-001). Open: the four production sites, which are the
defect.

`crates/shepr-mux/src/git/discovery.rs::git_trimmed_stdout`,
`git/status.rs::git_ahead_behind_between` and
`crates/shepr-client/src/workspace_label.rs` each build
`Command::new("git").arg("-C")...` independently. Consequently:

- No timeout budget. `git rev-list --left-right --count` on a repository whose
  objects live on a stalled network filesystem blocks the calling thread
  indefinitely; there is a 30-second retry delay and no deadline.
- The environment is inherited whole: `GIT_DIR`, `GIT_WORK_TREE`,
  `GIT_INDEX_FILE`, `GIT_CONFIG_GLOBAL`, `GIT_CEILING_DIRECTORIES`,
  `GIT_ALTERNATE_OBJECT_DIRECTORIES`, `GIT_ASKPASS` and `GIT_TERMINAL_PROMPT` all
  reach the child, and the launching shell may well have set several of them.
  `GIT_TERMINAL_PROMPT` unset means a credential prompt can block the spawn.
  Compare `pane/launch.rs`, which scrubs inherited host and agent variables for
  pane children.
- stdin is inherited, so an interactive credential helper has a terminal.

Also noted at that site:
`git/status.rs::git_status_snapshot_for_cwd_with_demand` calls `Instant::now()`
twice, so the retry deadline is measured from after the subprocess ran while the
cache checks are from before.

Fix suggested: one `run_git(dir, args) -> Result<String, GitReadError>` with a
scrubbed environment (`GIT_TERMINAL_PROMPT=0`, `GIT_OPTIONAL_LOCKS=0`,
`-c core.fsmonitor=false`, null stdin), a deadline and typed errors.

## BUG-067 - A process-global teardown counter couples two servers in one process

`crates/shepr-mux/src/pane/teardown.rs`: `static PANE_TEARDOWNS_IN_FLIGHT:
Mutex<usize>` plus a `Condvar`. `wait_for_pane_session_teardowns` waits on a
count global to the process, not scoped to a server - which is exactly what the
test suite runs - so one server's shutdown wait blocks on the other's pane
teardowns, and a leaked count from a panicking teardown thread makes every later
wait time out. The `Drop` impl uses `saturating_sub`, so an unbalanced decrement
is silently absorbed rather than caught.

Fix suggested: hang the counter off the thing that owns the panes (an
`Arc<TeardownTracker>` handed to `shutdown_pane_processes`).

## BUG-068 - Uncapped per-source maps and an uncapped history read

`crates/shepr-mux/src/terminal/state`: of the seven per-source maps on
`TerminalState` keyed by strings from hook reports, three are capped
(`MAX_METADATA_SOURCES`, `MAX_SEQUENCE_SOURCES`,
`MAX_STATE_LABELS_PER_SOURCE`); the hunter found no caps on
`hook_report_sequences`, `hook_report_accepted_at`,
`suppressed_full_lifecycle_hook_reports` or
`stale_full_lifecycle_hook_sessions`. A misbehaving hook that reports a fresh
`source` string per invocation grows those four without bound, per pane, for the
life of the server.

`crates/shepr-mux/src/persist/io.rs::load_history` does `fs::read_to_string` on
`session-history.json` with no size cap and then `serde_json` on the whole thing.
That file holds every pane's full scrollback, so restore reads it entirely into
memory twice (string, then parsed tree). `git/discovery.rs` caps ref files at
64 KiB, so the crate knows the pattern.

Fix suggested: one `BoundedSourceMap<V>` with the cap as a construction
parameter, and a cap plus test on the history read.

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

See also BUG-021, which is what makes this class dangerous rather than merely
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

## BUG-025 - The CLI process installs no tracing subscriber before the launch dispatch

The operator-guidance half is resolved: `validate_running_server_compatibility`'s
session-aware guidance is printed before the TUI takes the terminal when saved
machines keep the client running, and the Local endpoint carries session-aware
mismatch guidance.

Open: the CLI process installs no tracing subscriber before `auto_detect_launch`
runs (`init_file_logging` is called from `crates/shepr-client/src/lib.rs` and
`crates/shepr-server/src/server/headless/bootstrap.rs` only), so the
`auto_detect_launch` log lines (`"auto-detect launch starting"`, `"server
already running, attaching as client"`, `"no server running, spawning server
daemon"`, the Local-startup-failed warn) reach no subscriber. The early
`SHEPR_LOG` check in `src/autodetect.rs` (`FileLoggingConfig::from_environment`)
only validates the filter; it installs nothing.

Fix suggested: move logging init into `main` ahead of the launch dispatch, plus
a test asserting a subscriber exists before `auto_detect_launch` runs.

## BUG-037 - A mutex held across a 25-second blocking SSH attempt

`crates/shepr-remote/src/remote/saved.rs`: `SavedSshConnector::state` is a
`Mutex<ConnectorState>` held across the whole attempt (SSH spawn, discovery,
the caller's `establish` handshake). Verified in wave 1: the client's
`spawn_due` permits one in-flight attempt per endpoint and replacements wait for
retired attempts, so production does not contend on it today; the comment at the
mutex now says so. A concurrent direct caller would still wait out an active
attempt.

Open: the structural fix - move the mutable state behind `&mut self` so the
supervisor's exclusive ownership is the enforcement. That means transferring
connector ownership through the blocking task and returning it with the attempt
event, which crosses into `crates/shepr-client/src/lib.rs` as well as
`saved.rs` and `crates/shepr-client/src/endpoint/`.

## BUG-060 - Uncapped queues in the headless server

The tab-bar half is resolved (the warning logs once per failure streak without
the command line, timeouts render subsecond precision, and stderr stays
discarded with the reason at the code site).

Open: `crates/shepr-server/src/server/headless.rs`'s `pending_alt_screen_reads`,
`deferred_alt_screen_reads` and `queued_agent_manifest_reloads` are uncapped
`Vec`s, and the last accumulates one entry per `server.reload-agent-manifests`
request that arrives while a reload runs, so a client looping on that method
grows it without bound.

## BUG-082 - Nothing bounds concurrent API connections, so per-connection limits multiply

`crates/shepr-api/src/server.rs` spawns one thread per API connection with no
admission cap. Wire-supplied regexes are now compiled with size and DFA limits
and capped at 32 per subscription stream, but those limits are per connection,
so a client (or a misbehaving hook) opening many subscription connections still
multiplies the allocation. A global connection admission limit is the control
that makes the per-connection budgets a bound.

## BUG-010 - The configured shell is validated per spawn, not at launch

`crates/shepr-config/src/validated.rs` keeps `terminal.default_shell` as a raw
`String`; PATH lookup and the executability check happen in
`crates/shepr-pty/src/command.rs::to_std_command` at every spawn, and
`crates/shepr-mux/src/pane/launch.rs::pane_shell_command_builder` passes the
raw string through. A typo fails each pane rather than the launch. A `$SHELL`
that is set and invalid is an error in pane mode, not the fall-through to
`/bin/sh` that `default.toml` describes. Related: `to_std_command` quietly
substitutes home for a bad cwd (warn only) while the API validates `new_cwd`
upstream.

The layering question is the real blocker: `brokkr.toml` forbids a normal
`shepr-config` to `shepr-pty` edge, so the resolver cannot simply move into
config validation. Options: a shared shell resolver in a lower allowed layer
(`shepr-core` or `shepr-platform`) that both config validation and the PTY
call; or validation in the launch path in `src/main.rs` right after
`load_validated_config`, before starting the server or client. Either way the
pane builder should consume the validated value.

The same launch-time check should refuse a configured shell whose process name
shepr's detection does not recognise (`shepr-agent/src/detect/proc_tree.rs`'s
shell table): an unrecognised pane shell makes `available_pane_shell` return
`None`, and agent start then reports the target as busy.

## BUG-014 - The manifest registry is a process-wide global

`agent explain --file` now loads manifests from the CLI's resolved config
directory, so its explanation honours local overrides. Open: `registry()` still
serves a process-wide cache that a later `reload_manifests` replaces, so a
detection read before the headless bootstrap reload would briefly use bundled
rules with no warning. The structural fix passes a `&ManifestRegistry` down
instead. Consumers: one detector call in `crates/shepr-agent/src/detect/mod.rs`,
two readiness checks in `crates/shepr-mux/src/terminal/state/managed.rs`, one
explain call in `crates/shepr-server/src/app/api/agents.rs`; reloads in
`crates/shepr-server/src/server/headless/bootstrap.rs`,
`headless/api_dispatcher.rs` and `app/api.rs` (test code).

## BUG-079 - The Local new-workspace label blocks input handling on a git spawn

Remote endpoints now get a lexical cwd label with no local Git lookup. Open: for
the Local endpoint, `derive_label_from_cwd` still spawns `git rev-parse
--show-toplevel` synchronously in the new-workspace overlay's input handler.
Moving it off that path needs pending-label state and a client-loop completion
event, in `crates/shepr-client/src/shell/state.rs` and `shell_runtime.rs` or
`lib.rs` as well as `shell/overlays/overlay_input.rs`.

## BUG-084 - A failed stat after publishing the SSH agent link leaves the link behind

`crates/shepr-platform/src/ssh_agent.rs`: publication renames the temporary
symlink into place and then calls `symlink_metadata` on it to record its
identity. If that call fails, publication returns an error before the shared
state records the link, so later cleanup does not know the link is shepr's and
can leave the published path behind.

## BUG-085 - A shepr-client test writes a real OSC 52 sequence to the test runner's stdout

A full test run prints `]52;c;dGVzdA==` into the runner's output, so some
`shepr-client` test reaches a clipboard write path that still writes to the real
`io::stdout()` rather than an injected or test sink (candidates:
`shepr-termio`'s `host_term::title::write_clipboard_bytes`, which HYGC-003 names
as the lower crate's only direct stdout writer). Any test hitting it scribbles
on the developer's terminal clipboard when run interactively.

## BUG-086 - The API bridge logs its idle expiry to no subscriber

The client bridge launch now initialises the client file logger, so its idle
expiry is logged with the idle duration. The API bridge path (`remote-api-bridge`
in `src/main.rs`) logs the same expiry but installs no logger, so that line goes
nowhere.
