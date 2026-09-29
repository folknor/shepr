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

## BUG-103 - `IsolatedEnv` no longer clears shepr names that are not registry entries

`crates/shepr-test-support/src/lib.rs`: isolation now clears the shepr-core
registry and `ChildEnv` names instead of scanning every `SHEPR_` key, so names
read only by hook assets or test probes (`SHEPR_ACTION`,
`SHEPR_HOOK_INPUT_FILE`, `SHEPR_HOOK_SEQ`, `SHEPR_OMP_IDLE_DEBOUNCE_MS`,
`SHEPR_OMP_RETRY_GRACE_MS`, `SHEPR_DEVIN_LIST_JSON`,
`SHEPR_INTEGRATION_ID`/`VERSION`, `SHEPR_MESSAGES`) now reach tests from the
developer's shell. Give those names an owned list (the asset literal test
already enumerates the hook-local ones) that isolation also clears. The
`!= SCRATCH_DIR_ENV` filter in that loop is dead.

## BUG-104 - A valueless `git -c` key reads as `true` for every key

`crates/shepr-core/src/env.rs`'s `GIT_CONFIG_PARAMETERS` parser turns an
implicit `'key'` (no `=`) into the value `"true"`, which is right for boolean
keys but git errors on it for string keys such as `branch.<name>.remote`.
Model it as "no value" and let the consumer decide per key, as git does.

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
