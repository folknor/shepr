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

## BUG-099 - A layout that cannot be fingerprinted is no longer preserved as a snapshot

`crates/shepr-mux/src/persist/writer.rs`: when the snapshot-preservation paths
were collapsed into one decision, the case where `layout_fingerprint` returns
`None` (a fingerprint serialization failure) changed from "preserve" to "skip".
Only reachable on a serialization failure. Decide which is right; if a layout
that cannot be fingerprinted should still be preserved, restore that and pin it
with a test.
