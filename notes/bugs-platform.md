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

Findings raised in this scope but filed elsewhere: build identity (WIRE-001), pane teardown signalling (TERM-014), clipboard read freeze (UI-007).

## PLAT-001 - `--help` names a log file nobody writes

- `src/main.rs` prints `logging::help_log_paths_summary()` (`logging.rs`), which names `<data_dir>/shepr.log`. No process ever writes it: only `shepr-server.log` (`server/headless/bootstrap.rs`) and `shepr-client.log` (`client/mod.rs`) exist.

## PLAT-002 - `SHEPR_BIN_PATH` is built two different ways

- `platform::launch_executable()` (`linux.rs`) strips the " (deleted)" suffix Linux adds once a binary is replaced. Only `integration/env.rs` uses it.
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

## PLAT-006 - The platform layer doesn't match its stated structure

- **Contract:** "OS-specific code lives in linux.rs; mod.rs holds the shared interface."
- `platform/mod.rs` itself holds libc calls: setsid, getsid, and the SIGWINCH sigaction. Its `unsafe` blocks (`detach_server_daemon_command`, the SIGWINCH `sigaction`, the resize-signal test) have no SAFETY comments.
- `unix_common.rs` is a leftover unix-vs-windows layer.
- Several shims only exist for other platforms:
  - `check_config_write_target` does nothing.
  - `write_existing_config` always returns false; its caller branch in `integration/config_file.rs` is dead.
  - `monitor_host_shutdown` returns an `Option` that is always `Some`.
  - `noninteractive_process::command` is just `Command::new`.
  - `create_private_state_file` forwards to `create_remote_ssh_config_file`.
- `fits_unix_socket_path` uses 103 bytes, which is macOS's limit (Linux allows 107), so it rejects SSH control paths that would work (`unix_common.rs`, duplicated in `remote/attach.rs`).
- **Recommendation:** collapse everything into one flat `platform` module with no re-export layering.

## PLAT-010 - The host-shutdown monitor can't report a cancellation

- The server side is done: the shutdown warning writes a checkpoint and freezes saves (`sync_host_shutdown_freeze`), exits only on SIGTERM / `server stop`, and thaws when the flag clears. Until the monitor can report a cancellation, the server drops the monitor right after the checkpoint (releasing the delay lock) and thaws itself after `HOST_SHUTDOWN_CANCEL_GRACE` (60 s).
- The monitor (`src/platform/linux/shutdown.rs`) needs to: on `true`, set the flag and wake the loop; release the delay inhibitor once the server has checkpointed (needs a server→monitor channel; `HostShutdownMonitor::start` doesn't take one); keep watching instead of parking on `pending()`; on `false`, clear the flag, wake the loop and take a fresh inhibitor. Then remove the monitor drop in `freeze_for_host_shutdown` and the grace timeout. The comment at the `pending()` park describes the old server behaviour and is stale.

## PLAT-011 - Release-profile tests may touch the real config directory

- `config/io.rs` picks the directory name with `cfg!(debug_assertions)`, and `brokkr test` defaults to release. Any test that does not override `XDG_CONFIG_HOME` touches the real `~/.config/shepr`. The hunter did not audit which tests do.

## PLAT-012 - Tests write under `/tmp`

- `test_headless_server` (`src/server/headless/tests/`) uses `std::env::temp_dir()`, the `src/server/autodetect.rs` tests hard-code `/tmp/ha-*` paths, and the `logging.rs` and `platform/linux.rs` tests write under `temp_dir()`. Test scratch data should live under a per-test temp dir the harness owns, not fixed shared paths.

## PLAT-013 - Test environment locks are per-module

- Tests that change `PATH`, `HOME` or `XDG_*` each take their own module's lock (`server/autodetect.rs`, `api/server.rs`, `session.rs`, `client/tests/mod.rs`, `app/mod.rs`, `integration/env.rs`, `integration/mod.rs`, `remote/attach.rs`, `config.rs`). The locks don't exclude each other, so those tests can race. Needs one crate-wide lock, or no environment mutation in tests.

## PLAT-015 - Keystroke logging outside `raw_input.rs` is unchecked

- `raw_input.rs` now logs input lengths and content-free event kinds at debug level. Client and server input handling were not checked for debug lines that record typed text or paste contents.

## PLAT-016 - Clipboard helpers have no timeout

- The client now waits at most 500 ms for a clipboard read on its own thread and skips later pastes while an abandoned read is still running. But `read_clipboard_text_with_command` (`src/platform/linux.rs`) still waits on the helper with no deadline, so an abandoned `xclip`/`wl-paste` lives until it exits, possibly never. Kill the child after a deadline. Check `run_clipboard_command` (writes) for the same blocking wait.

## PLAT-017 - Unsafe blocks in `pty/backend.rs` lack SAFETY comments

- `open_pty`, `enable_utf8_input`, `prepare_pty_child` and `mark_inherited_fds_cloexec` in `src/pty/backend.rs`, and the raw `kill(-pgid)` in the `detect/mod.rs` test `foreground_job_detects_agent_behind_shell_wrapper`.
