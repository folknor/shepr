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

Resolved: the platform ownership test is renamed for its ACL coverage with a
separately ignored root-only ownership test, the `local_server.rs` permission
test is an explicit `#[ignore]` with its non-root requirement, and the reftable
trio no longer returns silently. Open:

- `crates/shepr-platform/src/config_file.rs`'s `EPERM` fallback on `fchown` is
  untested in the normal (non-root) run; testing it needs an ownership seam in
  `config_file.rs`.
- `crates/shepr-mux/src/pane/runtime.rs::process_cwd_does_not_require_traversing_the_directory_path`
  still prints a skip notice to stderr and passes green when run as root.

## BUG-097 - Git config override read errors are still discarded, and git -c config is not modelled

`crates/shepr-mux/src/git/config.rs::git_config_override_path` discards read
errors from `read_path` (`.ok().flatten()`), so a refused `GIT_CONFIG_GLOBAL`
or `GIT_CONFIG_SYSTEM` still falls back to the default files without saying so.
`GIT_CONFIG_PARAMETERS` (the `git -c` form of command-scope config, inherited
by shepr's git subprocesses) is not modelled by the file reader.

## BUG-098 - A malformed inherited `SHELL` fails a config that sets its own shell

`crates/shepr-config/src/validated.rs::resolve_default_shell` reads and
validates `SHELL` even when `terminal.default_shell` is set, so a broken
inherited `SHELL` refuses a launch that would never use it.

## BUG-096 - A tab dropped late in restore may already have started shells and queued history

`crates/shepr-mux/src/persist/restore.rs::restore_tab`: a tab rejected late (all
panes pruned, or refused by `from_saved`) may already have queued
`history_carry` entries or started shells for panes that are then discarded.
The invalid-ratio rejection returns before any of that; the later rejections do
not. Also untested: the server wiring in `crates/shepr-server/src/app/mod.rs`
that turns a nonzero `dropped_tabs` into a backup of the original session file
on the first save (the `with_paths` construction path).
