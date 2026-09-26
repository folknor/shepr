# Platform and process plumbing defects

```
1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
```

## PLAT-011 - Release-profile tests may touch the real config directory

- `config/io.rs` picks the directory name with `cfg!(debug_assertions)`, and `brokkr test` defaults to release. Any test that does not override `XDG_CONFIG_HOME` touches the real `~/.config/shepr`. The hunter did not audit which tests do.

## PLAT-012 - Tests write under `/tmp`

- `test_headless_server` (`src/server/headless/tests/`) uses `std::env::temp_dir()`, the `src/server/autodetect.rs` tests hard-code `/tmp/ha-*` paths, `client_transport.rs` tests use hard-coded `/tmp` (`unique_test_path`), the tab-bar status tests write to `/var/tmp`, and the `logging.rs` and platform tests write under `temp_dir()`. Test scratch data should live under a per-test temp dir the harness owns, not fixed shared paths. The `server/autodetect.rs` tests also set environment variables in `unsafe` blocks with no SAFETY comments.

## PLAT-013 - Test environment locks are per-module

- Tests that change `PATH`, `HOME` or `XDG_*` each take their own module's lock (`server/autodetect.rs`, `api/server.rs`, `session.rs`, `client/tests/mod.rs`, `app/mod.rs`, `integration/env.rs`, `integration/mod.rs`, `remote/attach.rs`, `config.rs`). The locks don't exclude each other, so those tests can race. Needs one crate-wide lock, or no environment mutation in tests.

## PLAT-015 - Two places not yet audited for keystroke logging

- Server input handling, the client loop, `client/shell/input.rs`, `actions.rs`, `text_editor.rs` and `src/remote/` log no typed text or paste contents (comments mark the audited sites). Not yet read: `crate::platform::forward_remote_bridge_stdio`, which carries every remote keystroke on the host side, and the other `src/client/shell/*` files (search them for `tracing::`).
