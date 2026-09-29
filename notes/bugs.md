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

## BUG-101 - `git -c` command-scope config is not modelled

`crates/shepr-mux/src/git/config.rs` models `GIT_CONFIG_COUNT` and its indexed
pairs, and refused `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` reads now surface as
errors. Open: `GIT_CONFIG_PARAMETERS` (the `git -c` form inherited by shepr's
git subprocesses), which git 2.53.0 passes as quoted key/value pairs (with
`'\''` for an apostrophe) and which overrides a conflicting indexed pair. Its
reader belongs in the `shepr-core` environment registry, not a raw read in mux.

## BUG-098 - A malformed inherited `SHELL` fails a config that sets its own shell

`crates/shepr-config/src/validated.rs::resolve_default_shell` reads and
validates `SHELL` even when `terminal.default_shell` is set, so a broken
inherited `SHELL` refuses a launch that would never use it.
