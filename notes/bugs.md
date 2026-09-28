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

## BUG-001 - A `?` inside a closure discards a good persisted agent session

`crates/shepr-mux/src/persist/snapshot.rs`, `capture_tab`'s `agent_session`
closure. The two `?` operators on `AgentSource::from_pair` and
`Agent::parse_canonical_label` return from the whole closure, not from the `if`
block, so when a live `hook_authority` carries a source or label that is not
recognised, the fallback to `terminal.persisted_agent_session` is never reached
and the pane is saved with no agent session at all. The pane then restores as a
plain shell with no resume, and nothing is logged. The intended shape lifts the
inner expression into its own `Option` and uses `.or_else(...)`.

Enforcement the hunter proposed: a test that captures a snapshot from a
`TerminalState` holding both an unrecognised `hook_authority` and a valid
`persisted_agent_session` and asserts the session survives.

## BUG-002 - `save_history`'s digest short-circuit trusts a claim the lease does not make

`crates/shepr-mux/src/persist/writer.rs::save_history` skips the write when the
SHA-256 of the new history JSON equals `self.written_history`, the digest of what
this writer last wrote, justified by the `DataDirLease`. The lease excludes
another server; it does not stop a user or a cleanup script from deleting
`session-history.json`. After such a delete the writer never rewrites history
again for the life of the process, and every subsequent restore loses scrollback
silently.

Fix suggested: key the skip on the file's own (mtime, len, digest), or add a test
that deletes the file between two saves with unchanged history and asserts it
comes back.

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

## BUG-009 - A race that can leak PTY master fds into other children

Both `openpty` in `crates/shepr-pty/src/backend.rs` and `create_wake_pipe` in
`crates/shepr-pty/src/fd.rs` set `FD_CLOEXEC` after creating the fds. Pane
children are covered by `mark_inherited_fds_cloexec`, but any other concurrent
`std::process::Command` spawn is not - git status, and especially ssh or a
ControlMaster. If one of those runs in the gap it can inherit a pane's master fd
and the PTY never hangs up.

Fix suggested: `pipe2(O_CLOEXEC|O_NONBLOCK)` for the wake pipe, and
`posix_openpt(O_CLOEXEC)` plus opening the slave with `O_CLOEXEC`; the later
`set_cloexec` on the master in `PtyIoActor::spawn_inner` is then redundant.

## BUG-010 - `default_shell` is never validated at launch

Stored as a raw `String` in `crates/shepr-config/src/validated.rs`. PATH lookup
and the executability check happen in `PtyCommand::to_std_command` at every
spawn, so a typo fails each pane rather than the launch, contradicting "Any
config problem fails the launch". `default.toml` says "Empty means $SHELL, then
/bin/sh", but a `$SHELL` that is set and invalid is an error in pane mode, not a
fall-through to `/bin/sh`. Related: `to_std_command` quietly substitutes home for
a bad cwd (warn only) while the API validates `new_cwd` upstream.

Fix suggested: a `ValidatedShell` produced at config load.

## BUG-011 - A flaky PTY fd test whose lock guards nothing

`pty_spawn_leaves_one_parent_pty_fd` in `crates/shepr-pty/src/backend.rs` counts
every `/dev/pts` fd in the process. The two sibling tests open PTYs in parallel
without taking `pty_fd_test_lock`, which only that one test takes.

Fix suggested: count only the fds this test created, or give every PTY test the
same guard.

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

## BUG-013 - `prepare_pty_child` ignores the return codes of `sigemptyset` and `sigprocmask`

`crates/shepr-pty` (child setup). A failure to reset the signal mask before
`exec` is discarded, so a pane child can start with an inherited blocked signal
set.

## BUG-014 - `shepr agent explain --file` silently ignores local manifest overrides

`src/cli/agent.rs` calls `manifest::explain_for_label`, which reaches
`manifest::registry()`. `registry()` initialises the process-wide `MANIFESTS`
`OnceLock` with `override_dir = None` when nothing has called
`reload_manifests(config_dir)` first, and only the server does that
(`shepr-server/src/server/headless/bootstrap.rs`, `app/api.rs`,
`api_dispatcher.rs`). So in the CLI process the bundled manifests are the only
ones ever loaded, and the documented workflow ("capture the pane, edit the
override, re-explain") explains against rules that are not the ones the server is
using. The `source: bundled` field is the only hint.

Same root cause, stated as a hazard rather than a bug: `registry()` and
`reload_manifests()` race for the same `OnceLock`, so the first caller fixes
whether overrides exist for the process lifetime. In the server the ordering
happens to be right today (bootstrap runs before any detection tick) and nothing
in the build would notice a future early call to `has_screen_manifest` or
`detect_with_osc` moving ahead of bootstrap: the result would be silently
bundled-only detection with no warning.

Fix suggested: make the override directory an argument of the explain entry
point, or have the CLI resolve config paths and call `reload_manifests` first;
better, remove the global and pass a `&ManifestRegistry` down.

## BUG-015 - Declared hook events disagree with the hooks actually written, for Grok

**Decision (partial):** Hermes support is removed entirely, so the Hermes half
of this entry is gone. The Grok half remains open.

Grok's descriptor carries `integration_hook_events: &[]` yet
`targets.rs::grok_hook_config` writes a real `SessionStart` hook with action
`session`, so any generic consumer of `IntegrationTarget::hook_events()` sees
Grok as hookless.

Fix suggested: a test asserting that a target with a config-registered hook has a
non-empty event list, and deriving the written config from the event list instead
of hand-writing it.

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

## BUG-017 - Three diverged shell-name lists mean some panes get no agent detection

`crates/shepr-agent/src/detect/proc_tree.rs::is_pane_shell_process_name` knows
twelve shells (`sh bash dash zsh fish ksh mksh csh tcsh elvish xonsh nu`);
`detect/mod.rs::is_generic_runtime_or_shell` knows four plus `tmux node bun`;
`wrapped_agent_name_from_runtime_argv` matches four. Consequence today, stated by
the hunter as a fact rather than a prediction: a pane shell that is `dash`, `nu`,
`ksh` or `xonsh` running an agent through `-c` is not unwrapped, so the agent is
not identified, so there is no detection for that pane. `is_pane_shell_process_name`
also fails open on any name outside its list (`nix-shell`, `toolbox`, a `$SHELL`
symlink under another name), which changes `available_pane_shell` and the whole
child-groups path while reporting nothing.

Fix suggested: one `ShellKind` table with per-use predicates (`is_pane_shell`,
`supports_dash_c`) derived from it, plus a launch-time check that the configured
shell is one shepr recognises.

## BUG-018 - The session-start-source vocabulary has three copies and they disagree

**Decision (partial):** Hermes support is removed entirely, so the hermes
asset's invented start sources go. The remaining disagreement (enum versus
`SESSION_START_MATCHER`, Grok's `load`) stays open.

`crates/shepr-agent/src/agent/resume.rs::AgentSessionStartSource::parse` accepts
eight values (`startup resume clear compact branch new fork select`);
`integration/claude_settings.rs::SESSION_START_MATCHER` is
`^(startup|resume|clear|compact|fork)$` (five); the kimi asset defaults to the
literal `"startup"`. The
comment above `SESSION_START_MATCHER` says Grok "uses new/load", and `load` is in
none of the three lists, so a grok-imported Claude hook firing with `load`
normalises to `None` and is treated as unrecognised
(`session_start_source_is_recognized` in `shepr-mux/.../hooks.rs`).

Fix suggested: derive the matcher regex from the enum's variant strings and give
the enum a single `as_str` the assets can be grepped against.

## BUG-019 - `json_hook_commands_registered` verifies a hook by substring search

`crates/shepr-agent/src/integration/registry.rs`. Install writes an exact shape
(four different shapes across `ensure_command_hook`,
`ensure_flat_command_hook`, `ensure_direct_command_hook`,
`ensure_simple_command_hook`), and status verifies by walking the event's value
recursively for a matching command string anywhere inside it
(`json_contains_string`). A command string sitting in a disabled block, in a
comment-like field, or in an unrelated nested entry counts as registered.

Fix suggested: have status reuse the install shape (`is_matching_command_hook`
already exists for one of the four).

## BUG-020 - Grok reimplements `hook_command` with a different interpreter and a literal action

`crates/shepr-agent/src/integration/command.rs::hook_command` produces
`bash '<path>' <action>`; `targets.rs::grok_hook_command` produces
`sh '<path>' session`. The grok asset is `#!/bin/sh`, so `sh` is probably
intentional, but the choice is invisible at the one place that owns how shepr
invokes hook scripts, and the action string `session` is spelled here rather than
taken from `IntegrationHookAction::as_str`.

Fix suggested: give `hook_command` an interpreter parameter (or read it off the
spec row) so every call site goes through one function.

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
  is refused. It needs `--session <name>`. The `env -u` must stay while
  BUG-077 stands. `resolve_paths_from_env` reads the socket overrides before
  the session picks the address, so an empty `SHEPR_SOCKET_PATH` is refused
  even alongside an explicit `--session`. An inherited `SHEPR_SESSION` does
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

## BUG-022 - A remote endpoint's config decode error is discarded and drops the cached config

`crates/shepr-client/src/shell/endpoints.rs::cache_endpoint_snapshot` does
`codec::from_slice_exact::<ValidatedConfig>(&snapshot.resolved_config).ok()`: the
decode error is discarded, nothing is logged, and `endpoint.resolved_config` is
set to `None`, discarding any previously good cached config.
`resolve_snapshot_config` then decodes the same bytes again and this time
propagates, so the config is decoded twice per new snapshot on the fanout path.
For a non-active endpoint the error is never surfaced at all
(`apply_cached_endpoint_snapshot` only reports when
`endpoint_id == self.active_endpoint_id`), so a remote machine shipping an
unreadable config looks healthy in the sidebar until you switch to it. The
message that does reach the operator names no subject: "invalid endpoint
configuration: unexpected end of input: needed 1 bytes, 0 remaining" - no host,
no endpoint, no session.

Context from the same hunter: this is also where AGENTS.md's "config is validated
once at launch" stops holding, because `ValidatedConfig`'s `Deserialize` re-runs
`from_resolution(..., CwdCheck::Received)` on the client at attach time.

Fix suggested: decode once, keep the `Result`, log at `warn` with the endpoint
id, and set the endpoint's status to the error immediately.

## BUG-023 - `skip_serializing_if` silently drops provenance keys

`crates/shepr-config/src/validated.rs::ConfigProvenance::from_config` enumerates
config keys by `serde_json::to_value(config)` and walking the tree. Any
`skip_serializing_if` in a config type therefore drops provenance keys silently,
and `RawRule` (behind `SidebarTokenRule`, `sidebar/rules.rs`) has ten of them
today, so sidebar-rule fields that are `None` never appear in `config check`'s
enumeration. The completeness of the provenance surface depends on an attribute
nobody audits.

## BUG-024 - `default.toml` ships one active setting

Every line in `crates/shepr-config`'s `default.toml` is commented out except
`pane_history = false` under `[experimental]`. The file is only printed
(`shepr --default-config`), never parsed, so a user who redirects it to
`~/.config/shepr/config.toml` gets a config that explicitly pins one experimental
flag while leaving everything else to defaults. The hunter judged it almost
certainly an editing slip.

Fix suggested: a test asserting every non-blank, non-`[section]` line in
`DEFAULT_CONFIG` starts with `#`.

## BUG-025 - `auto_detect_launch` swallows a local-server startup failure into a log line no subscriber receives

**Decision (partial):** the `app_dir_name` debug/release directory switch is
removed (BUG-021), so a dev build and the installed server share the
default-session sockets, and a build mismatch must always be refused with a
message. Checked against that rule, a local server of another build does take
this path. With saved machines configured:

- `validate_running_server_compatibility`'s error, the one that names the
  session's stop and attach commands, goes to this unsubscribed warn.
- `run_client` then attaches anyway. Its first handshake fails on the
  build-identity preamble, which is logged only to the client log file.
- The Local endpoint supervisor retries, lands in `Attention` and retries every
  30 s. Local is the endpoint active at startup, so the screen shows "Local:
  build mismatch: ... Install the same shepr build on both sides and restart
  the server".

So the mismatch is still refused and visible, not silent. But the message that
reaches the screen is the generic preamble text. It names neither the stop
command nor the session, and the one that does is discarded. Open: get
`validate_running_server_compatibility`'s guidance to the operator (print it
before the TUI takes the terminal, or use it as the Local endpoint's
diagnostic), plus the subscriber fix below. Without saved machines the launch
already fails with the full guidance on stderr.

Two findings that compound, from the same hunter:

- `src/autodetect.rs`: when saved machines are configured, a failed
  `spawn_server_daemon` / `wait_for_server_socket` /
  `validate_running_server_compatibility` is downgraded to `tracing::warn!("Local
  startup failed; keeping saved machines available")` and the client starts
  anyway. The comment says the Local endpoint's handshake will report the
  problem.
- The CLI process installs no tracing subscriber at all: `init_file_logging` is
  called only from `crates/shepr-client/src/lib.rs` and
  `crates/shepr-server/src/server/headless/bootstrap.rs`, neither of which has
  run yet. So that warn, plus `"auto-detect launch starting"`, `"server already
  running, attaching as client"` and `"no server running, spawning server
  daemon"`, are all discarded.

The hunter called this the highest-severity item in their report: a swallowed
failure whose only diagnostic is provably discarded.

Fix suggested: move logging init into `main` ahead of the launch dispatch, plus a
test asserting a subscriber exists before `auto_detect_launch` runs.

## BUG-026 - `EventHub::push` silently drops an event when the mutex is poisoned

`crates/shepr-api/src/event_hub.rs`: `let Ok(mut state) = self.inner.lock() else
{ return; }`. The read path was deliberately hardened -
`events_after_checked` returns `EventHistoryError::Unavailable` and has a test
for the poisoned case - but the write path just returns. After a poison,
subscribers see a silent permanent gap rather than the `server_unavailable` they
were designed to receive, because `current_sequence` also stops advancing, so
`events_after_checked` sees a consistent-looking empty tail rather than `Lost`.

Fix suggested: make `push` infallible by construction (a lock-free ring, or
`PoisonError::into_inner`) or report.

## BUG-027 - The unbounded `ApiClient` path writes with no send timeout

`crates/shepr-api/src/client.rs`: in `request_value`'s
`response_timeout(request) == None` branch (`agent.prompt` without `wait`, and
waits sent without `timeout_ms`) the client connects and writes with no
`set_send_timeout`, unlike `request_value_with_timeout`. A server that accepts
the connection and never drains the socket buffer blocks the CLI in `write_all`
forever with no diagnostic. The unbounded read is deliberate and argued for in a
comment; the unbounded write looks incidental to it. The hunter called this the
closest thing to a live hang in their scope.

Fix suggested: a test using a listener that never reads (the file already has
three tests of that shape).

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

## BUG-029 - Two API paths turn an encode failure into a dead connection instead of an error response

`crate::serialize_response_or_error` in `crates/shepr-api/src/lib.rs` is the
owner: on a serde failure it logs and emits a valid `serialization_error` JSON
body preserving the request id, and it has a dedicated test.
`crates/shepr-api/src/wait.rs` bypasses it four times with
`serde_json::to_string(&ErrorResponse{..}).map_err(std::io::Error::other)?`, and
`subscriptions.rs` / `server.rs` use `write_json_line`, which maps an encode
failure to `io::Error::other("failed to encode json: ..")`. So the same class of
failure either produces a well-formed error response or kills the connection with
no response at all, and the fallback machinery is defeated on those paths.

## BUG-030 - Four unbounded `recv()` loops in `src/netside_tests.rs` hang instead of failing

The source-release ack, the presentation-sync ack, the sync snapshot, the
presentation-effects fence and the final returning-activation loop all do
`loop { ... control.recv().expect(..) ... }` with `continue` arms and no deadline.
If the expected message never arrives the test blocks forever rather than
failing, and `brokkr check` has no per-test timeout to rescue it. Contrast
`crates/shepr-api/src/server/subscription_socket_tests.rs`, which defines
`RESPONSE_TIMEOUT` and threads a deadline through every read.

## BUG-031 - `startup_command` gives the wrong command when the socket path was overridden

`src/cli/server_not_running.rs`: if the socket path is not exactly
`paths.server_address().api_socket()`, the guidance degrades to "run `shepr`",
which for a `--session work` invocation or a `SHEPR_SOCKET_PATH` override is the
wrong command and will attach the wrong server. Nothing reports that the fallback
was taken. The `--machine` case never reaches here (it routes to
`target::remote_error`), so the reachable wrong-advice cases are socket
overrides.

Fix suggested: derive the command from the address, which already knows how
(`ServerAddress::attach_command` takes the session).

## BUG-032 - Wire-supplied regexes are compiled with no size or complexity limit

`crates/shepr-api/src/subscriptions.rs` (`Subscription::PaneOutputMatched`) and
`crates/shepr-api/src/wait.rs` (`wait_for_output`) both call `Regex::new(value)`
on a pattern that arrives in an API request. `regex` is not backtracking so there
is no catastrophic-backtracking risk, but `RegexBuilder::size_limit` defaults to
10 MiB of compiled program per pattern, `events.subscribe` accepts a list of
subscriptions on one connection each with its own pattern, and there is one
connection thread per subscription - so a client, or a misbehaving agent hook,
can allocate a large multiple of that per connection.

Fix suggested: one shared `compile_match_regex(&str) -> Result<Regex, ApiError>`
with `size_limit`/`dfa_size_limit` set, used by both sites.

## BUG-033 - A corrupt saved-machine catalog silently means "no saved machines"

`src/main.rs`:
`shepr_remote::machine::EndpointCatalog::load(paths).is_ok_and(|catalog| catalog.has_ssh())`.
`load` returns a rich `Err(String)` ("stored endpoint catalog is invalid",
"endpoint catalog exceeds the storage limit", "duplicate endpoint profile id",
"failed to open endpoint catalog: permission denied") and all of it is discarded.
`saved_federation == false` turns a local-server startup failure from a warning
into a hard launch failure (`autodetect.rs`) and silently switches the client's
`LocalFailurePolicy` from `Reconnect` to `ExitClient`, so a typo in
`endpoints.json` changes the client's lifetime rule with no message anywhere. The
client then loads the catalog again a moment later in `shepr_client::run_client`
and the two loads can disagree. The hunter called this the most consequential
live defect in their report.

Fix suggested: propagate the error and refuse the launch, and load the catalog
once, passing the value. Note: `notes/bugs-cli.md` CMD-007 records the same site
from the CLI side; both entries are left as they are.

## BUG-034 - A `stat` error on the catalog retires every saved machine

`crates/shepr-remote/src/machine/catalog.rs::catalog_fingerprint` does
`std::fs::metadata(path).ok()?`. `None` means both "the file is absent"
(correct - empty catalog) and "we cannot stat it" (a permission change, an
unmounted state dir). In the second case the watcher reports `Ok(Vec::new())` as
the current catalog once, retiring every saved machine in the running client.
`EndpointCatalogWatch::poll`'s doc promises "an unreadable or invalid file is
reported once per change; the caller keeps the profiles it has", which is true
for `load_from_path` errors and false for stat errors. The hunter called this a
likely live defect.

Fix suggested: distinguish `NotFound` from other kinds, as `load_from_path`
already does, plus a test that a stat failure does not retire machines.

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

## BUG-036 - A test's headline assertion has never executed

**Decision (partial):** the `Path::exists` seal is extended to `Path::is_file`
and `Path::is_dir`, and it reaches test code, so the `user_config.is_file()`
guard cannot stay. The suggested fix below removes it anyway. Open: the fix.

`crates/shepr-remote/src/remote/attach.rs::managed_ssh_config_includes_user_config_then_fallback`
guards its ordering assertion with `if let Some(home) = paths.home_dir() { let
user_config = home.join(".ssh").join("config"); if user_config.is_file() { ... }
}`. `paths` comes from `test_app_paths()`
(`AppPaths::test_with_context(&root, Some(&root), None)` over a fresh
`ScratchDir`), and a fresh scratch directory never contains `.ssh/config`, so the
inner block is dead in every run. The test's name and comment describe the one
ordering rule OpenSSH's first-value-wins semantics depend on, and check nothing.
The hunter listed this as one of two findings they would most want confirmed by
execution.

Fix suggested: write a `config` file into the scratch home and assert
unconditionally.

## BUG-037 - A mutex held across a 25-second blocking SSH attempt

`crates/shepr-remote/src/remote/saved.rs`: `SavedSshConnector::state` is a
`Mutex<ConnectorState>` held across the whole attempt, including the SSH child
spawn, discovery round trips and the caller's `establish` handshake. The comment
says it contends with nothing because the supervisor serialises attempts - a
claim about another crate (`shepr-client`'s `endpoint/supervisor.rs`) that the
supervisor does not reference back. As the hunter put it: if attempts really are
serialised the mutex is unnecessary; if they are not, this is a 25-second stall.
Either way one of the two is wrong.

Fix suggested: move the mutable state behind `&mut self` and let the supervisor's
exclusive ownership be the enforcement.

## BUG-038 - A remote-controlled version string is printed to the terminal unfiltered

`crates/shepr-remote/src/remote/server_lifecycle.rs`: `version_label` is
`version.unwrap_or("unknown")`, while `remote_server_compatibility_error` and
`remote_compatibility_error` each define their own `printable` closure that does
the same plus an ASCII-graphic filter. `confirm_remote_server_stop` uses the
unfiltered one, so a version string containing `\x1b[2J` reaches the operator's
terminal - a real terminal-injection hole through the one unfiltered site, stated
by the hunter as a fact.

Related exposure noted by the same hunter: `command_failed` and
`ssh_bridge_exit_error` fold raw remote stderr (bounded at 16 KiB, unredacted)
into error messages that reach `eprintln!` and the CLI's JSON output, and
`src/cli/machine.rs::status` prints that same stderr via `error.escape_debug()`.
Also `local_forward_socket_path` (the `--remote` path) puts
`sanitize_path_component(target)`, i.e. `user@host`, into a world-listable name
in the XDG runtime directory, while the saved-machine path deliberately does not.

Fix suggested: one `printable_remote_value` used everywhere; delete
`version_label`; a test that `\x1b[2J` is filtered by every rendering path.

## BUG-039 - `is_launch_fatal_setup_error` classifies by `ErrorKind`, and the blanket is wrong

`crates/shepr-remote`: it treats every `io::ErrorKind::InvalidInput` as
launch-fatal. `InvalidInput` is produced by
`shepr_platform::remote_bridge_endpoint_path` for "socket path exceeds the Unix
socket length limit", by `validate_private_runtime_dir` for a relative runtime
dir, by `shared_ssh_control_path`, and by `RemoteExecutable::parse` failures
arriving through other paths. Some of those are genuinely deterministic; the
classification is by kind, not by cause. The hunter marked this "FACT, partly
false". Note the existing test
`only_typed_runtime_directory_policy_errors_are_launch_fatal` pins the looser
rule in place, so this needs the test changed, not added.

Fix suggested: a typed `DeterministicSetupError` marker on every deterministic
producer, and drop the blanket.

## BUG-040 - The SSH agent registration worker polls forever, and one bound misreports

`crates/shepr-remote/src/remote/ssh_agent.rs`: `Registration`'s worker loops at
10 Hz for the entire life of the remote bridge process whenever the API socket
never appears - bounded in memory, unbounded in wakeups, and nothing logs after
the first `debug!`. Separately, `ssh_agent::connect` caps the response at 4096
bytes but then falls out of the loop and reports `TimedOut` rather than "response
too large": a bound that misreports.

## BUG-041 - `~` expansion in Git config resolves against the cwd when `HOME` is unset

**Decision (partial):** piece 1 (the `shepr-core` environment registry) bans the
raw `std::env::var_os("HOME")` in `clippy.toml`, so both sites must be rewritten.
Open: the fix itself, routing them through `expand_tilde_path`.

`crates/shepr-mux/src/git/config.rs::normalize_gitdir_include_pattern` and
`::resolve_include_path` both do
`std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(rest)` for a
`~/` prefix. With `HOME` unset or empty that yields exactly the failure
`shepr-core/src/pathutil.rs` exists to prevent - its doc comment states the
policy: unset, empty or relative `HOME` is an error, never a fallback, because
"every caller builds a path under it, and an invalid home would make that path
relative to the current directory". `~/x` becomes the relative path `x`, resolved
against whatever cwd the server happens to have. Two sites, same file, both
reachable from Git config parsing. The hunter listed this first among their live
defects.

Fix: one call to `shepr_core::pathutil::expand_tilde_path`, which is already
public and returns the right error. (A third `HOME` policy, with two silent
fallbacks, lives in `crates/shepr-pty/src/command.rs::home_dir`: `HOME` must be
absolute and an existing directory, else the passwd entry, else `/`.)

## BUG-042 - The clipboard write path is keyed on the program name `wl-copy`

`crates/shepr-platform/src/clipboard.rs`: the entire "detach the clipboard owner
instead of waiting for it" behaviour hangs on
`clipboard_program_name(command.program) == "wl-copy"`. Rename the helper, wrap
it, or point at `wl-copy-wrapper` - a case the project's own test at
`tests.rs` explicitly demonstrates produces `"wl-copy-wrapper"` - and the write
path silently reverts to waiting for a process that never exits until the 2 s
timeout kills it, taking the user's clipboard content with it. The hunter listed
this second among their live defects.

Fix suggested: put the "owns the selection after exit" property on
`ClipboardCommand` as a field rather than inferring it from the program name.

## BUG-043 - The logging writer's poisoned-mutex branch turns the process into one that never logs again

`crates/shepr-platform/src/logging.rs`: `RotatingFileGuard::write` returns
`Ok(buf.len())` on a poisoned mutex and on any write failure, and `flush` returns
`Ok(())` on a poisoned mutex. The recovery design (remember the first error,
report it into the log when writing resumes) is sound and tested, but the
poisoned-mutex branch is not covered by it: it returns success and records
nothing in `lost_error`, so a panic inside the rotation path silently turns the
process into one that logs nothing, forever, with no "lines were lost" note -
because it never recovers.

Fix suggested: `PoisonError::into_inner` (as `shepr-test-support` already does
for its own mutexes) so the state is recovered; the existing
`writer_recovers_after_an_io_error_and_notes_the_gap` test shape extends to it.

## BUG-044 - An invalid `SHEPR_LOG` filter degrades silently, and a second logging init is discarded

**Decision (partial):** piece 1 (the `shepr-core` environment registry, after
broadarrow's `core::env`) makes `SHEPR_LOG` a registry entry, so surrounding
whitespace and non-UTF-8 are refused naming the variable. Open: a filter that
fails to parse (the owning site's text, as in broadarrow) and the discarded
second init.

`crates/shepr-platform/src/logging.rs`:
`EnvFilter::try_from_env("SHEPR_LOG").unwrap_or_else(|_| EnvFilter::new("shepr=info"))`.
A typo in the filter (`shepr=inof`) produces no diagnostic of any kind: the `Err`
is discarded and the default installed. Given AGENTS.md's "Any config problem
fails the launch; no fallbacks", this is the one config input in the project that
contradicts the stated rule, and it is in the module that owns diagnostics, so
the failure cannot be reported through itself. The next line has the same shape:
`let _ = tracing_subscriber::fmt()... try_init();` - a second `init_file_logging`
call silently keeps the first subscriber while the caller believes its writer is
installed.

Fix suggested: return `Result` from `init_file_logging` and fail the launch on a
bad filter.

## BUG-045 - Mutexes held across blocking cross-process work in `shepr-platform`

- `logging.rs::RotatingFileState`: the mutex is held across `flock(LOCK_EX)` - a
  blocking syscall that waits for another process - plus `rename`, `remove_file`
  and `write`. Every thread in the process that emits a log line blocks behind a
  cross-process lock. The workspace already denies `await_holding_lock`; this is
  the sync analogue and no lint covers it.
- `ssh_agent.rs`: `SshAgentRegistry::register` and `SshAgentLease::refresh_at`
  hold `Arc<Mutex<State>>` across `State::publish`, which does
  `symlink_metadata`, up to N `connect_sync()` calls through `live_socket`,
  `symlink`, `rename` and another `symlink_metadata`. Each `connect_sync` uses
  `ConnectWaitMode::Timeout(Duration::ZERO)`, so the window is short by
  construction - but the structure, not the timeout, is what keeps it short, and
  nothing records that. Every attachment's refresh serialises behind it.

Fix suggested for the logging case: take the file handle, drop the guard, then
write.

## BUG-046 - `Activity::record` turns a clock failure into a desynchronised SSH relay

`crates/shepr-platform/src/remote_bridge.rs`: `TrackedIo::{read,write}` call
`self.progressed(count)?`, which calls `now()?`, which fails if
`clock_gettime(CLOCK_BOOTTIME)` fails. A clock error is then reported to the
caller as an IO error on the relayed stream, after the bytes have already been
read or written - so the byte count is lost and the stream desynchronises. A
failed clock read should leave the watchdog stale (the watchdog already treats a
`now()` failure as expiry), not corrupt the relay. The hunter marked this a
prediction / latent desync rather than an observed failure.

Fix suggested: make `record` infallible.

## BUG-047 - The only behavioural test of the logind protocol never runs

`crates/shepr-platform/src/shutdown.rs::delay_lock_is_held_until_checkpoint_and_retaken_after_cancellation`
is `#[ignore = "requires dbus-daemon; ..."]`. It is the sole test of
`watch_connection` - inhibitor acquisition, the hold-until-checkpoint invariant,
retake-after-cancellation, and the `already_preparing` reconnect branch.
Everything `brokkr check` exercises in that module is the three pure `Shared`
tests. The mechanism AGENTS.md cites as the reason `zbus` is a dependency at all
is unverified by the gate.

Fix suggested: `zbus` can serve the `LoginManager` interface over a `UnixStream`
pair or a `p2p` connection with no `dbus-daemon`, which removes the
external-binary dependency and lets the test run in the gate.

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

## BUG-050 - `ctrlc::set_handler` failure is swallowed on the server's only signal path

`crates/shepr-server/src/server/headless.rs`: `let _ = ctrlc::set_handler(move ||
{ ... });`. If installation fails - a handler already registered, which `ctrlc`
reports as `MultipleHandlers` - the server runs with no SIGINT/SIGTERM/SIGHUP
handling: `systemctl stop`, a logout or a Ctrl-C kills it without the shutdown
sequence that saves the session. Nothing is logged and nothing is returned. The
hunter called it a live swallowed failure, not a style point.

Fix suggested: return `io::Result` from `ctrlc_handler` and propagate to
`run_server`, which already returns `io::Result<()>`.

## BUG-051 - The final session save's result is discarded on shutdown

`crates/shepr-server/src/app/session.rs::retire_session_writer`: `if let
Some(thread) = self.session_saver.session_save_thread.take() { let _ =
thread.join(); }`. The join drops both the thread's panic and the
`io::Result<()>` the save job returned, so on the one path where losing a save
matters most a failed write is invisible. Every other save result goes through
`record_session_save_result`, which logs and retries.

Fix suggested: feed `join()` into `record_session_save_result`.

## BUG-052 - Results discarded where the server could act

- `crates/shepr-server/src/app/events.rs`: `let _ =
  self.state.commit_pane_removal(&plan);` in the `AppEvent::PaneDied` handler. If
  the plan no longer matches state (the pane was closed by an API call between
  plan and commit), the event is consumed with nothing recorded.
- `crates/shepr-server/src/app/api/workspaces.rs`: `let _ =
  std::fs::remove_dir_all(&source_cwd);` - a recursive delete whose failure is
  discarded. The hunter flagged it as worth a second look: it may be fixture
  teardown in test code, but a recursive delete of a path derived from workspace
  state is the one operation you want logged either way.

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

## BUG-060 - A failing tab-bar status command warns every interval, forever, with the command line in the log

`crates/shepr-server/src/app/tab_bar_status.rs`:
`tracing::warn!(command = %runtime.command, error, "tab bar status command
failed")`. A status command that fails persistently emits a warn line every
`interval_seconds` (which can be 1) for the server's whole life - unbounded log
growth on a permanent condition - and the command line is user-authored shell, so
a `tab_bar_right` entry that curls an endpoint with a bearer token puts that
token in the log at warn level. The command's stderr is separately discarded
(`.stderr(Stdio::null())`), so a failing segment silently goes blank, and the
timeout message (`format!("timed out after {}s", timeout.as_secs())`) names no
command and renders any sub-second timeout as "after 0s".

Related unbounded growth in the same crate
(`crates/shepr-server/src/server/headless.rs`): `pending_alt_screen_reads`,
`deferred_alt_screen_reads` and `queued_agent_manifest_reloads` are uncapped
`Vec`s, and the last accumulates one entry per `server.reload-agent-manifests`
request that arrives while a reload runs, so a client looping on that method grows
it without bound.

## BUG-061 - Three tab-bar sanitizers with three rules, so one entry kind can inject bidi overrides

`crates/shepr-server/src/app/tab_bar_status.rs`: `sanitize_separator` and
`sanitize_literal_text` strip control characters only; `sanitize_status_text`
also trims, strips unicode format controls and caps at 80 chars. All three feed
`AppState::tab_bar_right` and render into the same row, so a configured `text`
entry can carry a bidi override (`U+202A`-`U+202E`) that an identical string from
a `command` entry cannot, and the separator is uncapped.

Fix suggested: one `TabBarText` newtype whose only constructor sanitizes.

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

## BUG-063 - Two Git config parsers in one module, and the naive one decides two real questions

**Decision:** shepr's own git discovery now honours `GIT_CEILING_DIRECTORIES`
with git's semantics - it was the one upward walk that ignored it - and
`IsolatedEnv` sets the ceiling to the scratch base, so a test's upward walk
cannot escape it. Unrelated to this entry's naive-parser defect, which stays
open.

`crates/shepr-mux/src/git/discovery.rs` has `read_git_config_value` /
`simple_git_config_section` / `strip_git_config_comment`: about forty lines that
read one key from one file, skip any `[section "subsection"]` header, and know
nothing about `include.path` or `includeIf`. `crates/shepr-mux/src/git/config.rs`
is 784 lines implementing the real thing. Two questions go through the naive
parser: `git_dir_is_bare` (`core.bare`) and `git_ref_storage_is_reftable`
(`extensions.refstorage`). Both are read from exactly one file, so `core.bare =
true` set via an `include.path` or in `~/.gitconfig` is missed, and a reftable
repo is then read with the loose-file path
(`read_head_identity_from_files`) and reports no branch. The hunter called this a
behavioural divergence between two parsers in the same directory.

Fix suggested: delete `read_git_config_value` and route both questions through
`config.rs`'s reader, with a test using an `include.path` and an `[extensions]`
block in an included file.

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

## BUG-069 - A `.pending` temporary is never cleaned up and burns a sequence slot

`crates/shepr-mux/src/persist`: `io.rs` publishes through
`target.with_extension("json.tmp")` while `writer.rs::copy_recovery` publishes
through `backup.with_extension("pending")`. Both go through the same
`publish_private_file`, but the crash-recovery reasoning in
`remove_stale_temporary` only knows about `json.tmp`. So a crash between create
and rename in `copy_recovery` leaves a `.pending` file that nothing ever cleans
up, and the next attempt at that exact name fails `AlreadyExists` and burns one
of the 128 sequence slots.

Fix suggested: one suffix const passed into `publish_private_file`, and extend
`remove_stale_temporary` to both.

## BUG-070 - `present_surface_patch` writes to the real stdout under `cfg(test)`

`crates/shepr-client/src/state.rs`: `try_present_frame` routes through
`frame_output::write_composed_frame` and picks its sink by `cfg`
(`io::stdout()` in production, `io::sink()` under test) with a comment explaining
that a full-screen frame written to the test runner's real stdout would scribble
on the developer's terminal. `present_surface_patch`, forty lines above, writes
`io::stdout().write_all(&encoded.bytes)` unconditionally, with no `cfg` and
bypassing `frame_output` entirely. The hunter marked this a live defect, not a
prediction: any unit test reaching the patch path writes escape sequences to the
test runner's terminal. It also means the presentation test surface
(`shell/tests/copy.rs`, `mouse_selection.rs`, `endpoints.rs`) never covers the
production writer, and the two paths are not the same code.

Fix suggested: an injected writer on `ClientState`, which removes the
`#[cfg(test)]` divergence as well.

## BUG-072 - The bridge idle watchdog still leaves no log line saying why it fired

**Decision (partial):** the suggested fix is adopted and landed: `process::exit`
is confined to `src/main.rs`, held by both a `clippy.toml disallowed-methods`
entry and broadarrow's `exits-from-main` textlint. `shepr-platform`'s remote
bridge watchdog (`remote_bridge.rs::Activity::start`) now takes an `expired`
callback instead of exiting, and `remote_bridge_io.rs` turns that into a
`RemoteBridgeOutcome::IdleExpired` that `src/main.rs::finish_bridge` maps to
`CliError::BridgeIdle`; `shepr-client`'s `run_client` returns a
`ClientRunError` instead of exiting; `shepr-server`'s bootstrap returns a typed
`RunServerError` on `AddrInUse` instead of exiting, so `logging::shutdown("server")`
runs either way. Open: the bridge still
ends with no log line saying the idle deadline fired - `CliError::BridgeIdle`
prints nothing (`CliError::print`'s `Self::BridgeIdle => {}` arm) - so the one
place a log would explain a mysterious remote disconnect still has none.

Fix suggested: log the idle expiry (with the elapsed idle duration) at the
point `RemoteBridgeOutcome::IdleExpired` is produced, or in `finish_bridge`
before returning the exit code.

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

## BUG-077 - Most panes are launched with an empty `SHEPR_SOCKET_PATH`

`crates/shepr-mux/src/pane/launch.rs`: `PaneLaunchEnv::from_extra` starts with
an empty `api_socket_path`, and `apply_pane_launch_env` always writes
`SHEPR_SOCKET_PATH` from it. The only call to `with_api_socket_path` is in
`crates/shepr-server/src/app/ids.rs::pane_launch_env`, which is reached from
agent resume. Every other pane - new workspaces (`creation.rs` ->
`Workspace::new_with_extra_env`), new tabs, API splits (`api/panes.rs` ->
`split_pane`), layout apply and session restore (`persist/restore.rs`) - is
built by `from_extra` alone, so the child sees `SHEPR_SOCKET_PATH=""`.

Consequences, if the trace holds:

- Every hook asset exits early when `SHEPR_SOCKET_PATH` is empty, so hook-based
  agent state and session reporting silently does nothing in ordinary panes.
- `pane_agent_socket(Path::new(""))` means those panes also never get shepr's
  forwarded `SSH_AUTH_SOCK`.

Found by reading during the `SHEPR_AGENT` removal; the field arrived in
253d332 threaded only through `ids.rs`. Not yet confirmed by execution:
`echo $SHEPR_SOCKET_PATH` in a fresh split settles it.

Fix suggested: make the socket path a required input of `PaneLaunchEnv`
construction rather than an optional builder step, so a pane cannot be launched
without it, plus a test that every pane creation path sets a non-empty value.

## BUG-078 - The agent sidebar's pane sort compares `PublicPaneId`'s encoded strings, not numbers

`crates/shepr-client/src/shell/sidebar/agent_sidebar.rs`'s `AgentRowIndex`
orders `AgentRowIndexItem::Pane` rows with `left.pane_id.cmp(&right.pane_id)`.
`PublicPaneId` (`crates/shepr-protocol/src/ids.rs`) derives no `Ord`; the call
compiles by deref-coercing through `impl Deref for PublicPaneId` to `&str` and
comparing the encoded id text. The encoded suffix is a base-32-style digit
string with no fixed width (`encode_public_number`), so it sorts
lexicographically rather than numerically: pane 32 sorts before pane 1, and
pane 60 before pane 2. `PublicTabId`, by contrast, derives `Ord` and sorts by
its `(workspace_id, number)` fields, so tabs order correctly while panes in the
same sidebar do not.

Fix suggested: derive or implement `Ord`/`PartialOrd` on `PublicPaneId` from
`(workspace_id, number)`, matching `PublicTabId`, and have the sidebar compare
`left.pane_id.cmp(&right.pane_id)` order by that rather than by `Deref`.

## BUG-079 - The new-workspace label runs a local `git` on a cwd that may be on a remote host, synchronously on the client's main thread

`crates/shepr-client/src/workspace_label.rs::derive_label_from_cwd` spawns
`git rev-parse --show-toplevel` in `cwd` to derive a suggested workspace name.
Its only caller, `crates/shepr-client/src/shell/overlays/overlay_input.rs::open_new_workspace_overlay`,
passes `new_workspace_cwd` read from `self.snapshot` - the *active endpoint's*
snapshot, which for a workspace on a remote machine names a path on that
remote host, not on the machine running the client. `derive_label_from_cwd`
spawns `git` locally against that path regardless, so it either inspects an
unrelated local directory that happens to share the remote path, or (more
often) fails to enter it and falls through to the non-Git fallbacks - silently
producing a wrong or generic label with no indication the lookup targeted the
wrong host. Separately, the call runs synchronously in the client's input
handling as the new-workspace overlay opens, blocking the UI thread on a
subprocess spawn (and, on a slow or unmounted local filesystem, its `git`
invocation) instead of asking the endpoint that owns the cwd.

Fix suggested: only derive the label locally for the `Local` endpoint, and for
a remote endpoint either skip the Git lookup or ask the remote server for the
checkout root; run the lookup off the input-handling path either way.

## BUG-080 - `PublicTabId::new` / `PublicPaneId::new` accept inputs their own parser rejects

`crates/shepr-protocol/src/ids.rs`: `PublicTabId::new` and `PublicPaneId::new`
perform no validation on `workspace_id` or `number`. An empty `workspace_id`
builds an encoded id like `:t3`, which `parse_public_child_id` explicitly
rejects (`if workspace_id.is_empty() { return Err(..) }`). A `number` of `0`
encodes to the empty string via `encode_public_number` (its loop runs zero
times), producing an id like `work:t`, which `parse_public_number` also
explicitly rejects (`if encoded.is_empty() { return Err(..) }`, and separately
`.filter(|number| *number > 0)`). Either input builds a `PublicTabId`/
`PublicPaneId` value that serializes to text nothing in the codebase can parse
back into the same type - a round-trip break reachable from any caller that
constructs one from a raw workspace id string or a zero-based index.

Fix suggested: make `new` fallible (or debug-assert / refuse in the same way
`parse_public_child_id` does) so the two constructors agree with the one
parser about what a valid id looks like.

## BUG-081 - A Git config read failure reads the same as "the key has no value"

`crates/shepr-mux/src/git/discovery.rs::read_git_config_value` does
`std::fs::read_to_string(path).ok()?` and returns `None` on any read error
(permission denied, a symlink loop, an `EIO`) exactly as it does when the file
parses cleanly but the key is absent. Callers cannot tell "this repository is
not bare" from "the config file's bareness could not be determined" (same for
`extensions.refstorage`, which decides whether the repository uses `reftable`
refs). An unreadable git config therefore silently reads as a normal,
non-bare, non-reftable repository rather than failing the git-status read or
logging the stat/read failure.

Fix suggested: return a `Result` distinguishing "key absent" from "file could
not be read", the same distinction `notes/bugs.md`'s BUG-034 asks for on the
saved-machine catalog's `stat` failures.
