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

## PLAT-002 - `SHEPR_BIN_PATH` is built two different ways

- `platform::launch_executable()` strips the " (deleted)" suffix Linux adds once a binary is replaced. Only `integration/env.rs` uses it.
- `app/tab_bar_status.rs` exports `SHEPR_BIN_PATH` from raw `current_exe()`. After an install, status commands get `/…/shepr (deleted)` and fail.
- `server/autodetect.rs` (`spawn_server_daemon`), `remote/attach.rs` (`run_client_process`) and `cli/status.rs` also use raw `current_exe()`.

## PLAT-003 - Windows code is still in the tree, and tests run a path production never takes

Surfaced in five scopes: platform, wire protocol, CLI/config, headless server, detection/integrations.

- **Claim:** "Linux only. No `#[cfg(windows)]`, `#[cfg(target_os = "macos")]` or `cfg!` branches for other platforms."
- `src/input/model.rs` uses `#[cfg(any(windows, test))]` for `PhysicalKeyId`, `KeySource::WindowsConsole` and `with_windows_record`, and still has `WindowsKeyRecord` and `windows_dead_key`.
- `src/protocol/wire.rs` has `cfg!(any(windows, test))` in `to_raw_input_event`, and repeats the cfg split just below.
- `WindowsKeyRecord` / `windows_record` and `physical_key_id` still travel on the wire in `ClientPaneInputEvent::Key`; on Linux these fields are always `None`. `windows_dead_key` / `with_windows_composition_hint` still run in production. The held-input tracking carries `windows_record` / `WindowsKeyRecord` too (`server/clients.rs`).
- Under test the Windows branch is compiled in and exercised, so key-identity, release-tracking and lease tests (`src/input/lease.rs`), `client_shell_pane_input_roundtrips_semantic_and_windows_keys` and the dead-key tests pass on logic the shipped binary does not have. The "Windows dead key" guard at the top of `encode_terminal_key` can never fire on Linux.
- Smaller cfg leftovers: `#[cfg(unix)]` in the integration tests, `pane/terminal.rs` and `client/endpoint/ssh_metadata.rs`, plus `#[cfg(target_os = "linux")]` in `pane/terminal/migration_tests.rs`.
- `reported_cwd_parses_file_uri_and_bare_paths` (`src/pane/osc.rs`) still checks Windows `C:\...` paths as bare cwd reports.
- **Recommendation:** delete the whole Windows key-record path, including its wire fields.

## PLAT-011 - Release-profile tests may touch the real config directory

- `config/io.rs` picks the directory name with `cfg!(debug_assertions)`, and `brokkr test` defaults to release. Any test that does not override `XDG_CONFIG_HOME` touches the real `~/.config/shepr`. The hunter did not audit which tests do.

## PLAT-012 - Tests write under `/tmp`

- `test_headless_server` (`src/server/headless/tests/`) uses `std::env::temp_dir()`, the `src/server/autodetect.rs` tests hard-code `/tmp/ha-*` paths, `client_transport.rs` tests use hard-coded `/tmp` (`unique_test_path`), and the `logging.rs` and platform tests write under `temp_dir()`. Test scratch data should live under a per-test temp dir the harness owns, not fixed shared paths.

## PLAT-013 - Test environment locks are per-module

- Tests that change `PATH`, `HOME` or `XDG_*` each take their own module's lock (`server/autodetect.rs`, `api/server.rs`, `session.rs`, `client/tests/mod.rs`, `app/mod.rs`, `integration/env.rs`, `integration/mod.rs`, `remote/attach.rs`, `config.rs`). The locks don't exclude each other, so those tests can race. Needs one crate-wide lock, or no environment mutation in tests.

## PLAT-015 - Keystroke logging outside `raw_input.rs` is unchecked

- `raw_input.rs` logs input lengths and content-free event kinds at debug level. Client and server input handling were not checked for debug lines that record typed text or paste contents.
