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

Findings raised in this scope but filed elsewhere: build identity (WIRE-001), pane teardown signalling (TERM-014), `config reset-keys` (CMD-006), clipboard read freeze (UI-007), stale scrollback config text (TERM-005).

## PLAT-001 - `--help` names a log file nobody writes, and the unknown-command whitelist is dead

- `src/main.rs:523` prints `logging::help_log_paths_summary()` (`logging.rs:33`), which names `<data_dir>/shepr.log`. No process ever writes it: only `shepr-server.log` (`server/headless/bootstrap.rs:5`) and `shepr-client.log` (`client/mod.rs:98`) exist.
- The unknown-command whitelist at `main.rs:559-577` is effectively dead. `cli::maybe_run` has already consumed every listed command except bare `server` and `client`.

## PLAT-002 - `SHEPR_BIN_PATH` is built two different ways

- `platform::launch_executable()` (`linux.rs:89`) strips the " (deleted)" suffix Linux adds once a binary is replaced. Only `integration/env.rs:30` uses it.
- `app/tab_bar_status.rs:22` exports `SHEPR_BIN_PATH` from raw `current_exe()`. After an install, status commands get `/…/shepr (deleted)` and fail.
- `server/autodetect.rs:124` (`spawn_server_daemon`), `remote/attach.rs:1617` (`run_client_process`) and `cli/status.rs:334` also use raw `current_exe()`.

## PLAT-003 - Windows code is still in the tree, and tests run a path production never takes

Surfaced in five scopes: platform, wire protocol, CLI/config, headless server, detection/integrations.

- **Claim:** "Linux only. No `#[cfg(windows)]`, `#[cfg(target_os = "macos")]` or `cfg!` branches for other platforms."
- `src/input/model.rs:34-221` uses `#[cfg(any(windows, test))]` for `PhysicalKeyId`, `KeySource::WindowsConsole` and `with_windows_record`, and still has `WindowsKeyRecord` and `windows_dead_key`.
- `src/protocol/wire.rs:284` has `cfg!(any(windows, test))` in `to_raw_input_event`, and `:315-320` repeats the cfg split.
- `WindowsKeyRecord` / `windows_record` and `physical_key_id` still travel on the wire in `ClientPaneInputEvent::Key` (`wire.rs:135`); on Linux these fields are always `None`. `windows_dead_key` / `with_windows_composition_hint` still run in production. The held-input tracking carries `windows_record` / `WindowsKeyRecord` too (`server/clients.rs:251,267`).
- Under test the Windows branch is compiled in and exercised, so key-identity, release-tracking and lease tests (`src/input/lease.rs`), `client_shell_pane_input_roundtrips_semantic_and_windows_keys` and the dead-key tests pass on logic the shipped binary does not have. The "Windows dead key" guard at the top of `encode_terminal_key` can never fire on Linux.
- Smaller cfg leftovers: `#[cfg(unix)]` in the integration tests, `pane/terminal.rs:4024,4070` and `client/endpoint/ssh_metadata.rs:160,201`, plus `#[cfg(target_os = "linux")]` in `pane/terminal/migration_tests.rs:121`.
- Integration assets carry their own Windows leftovers (DET-013).
- **Recommendation:** delete the whole Windows key-record path, including its wire fields.

## PLAT-004 - Process environment is changed after threads exist

Surfaced in two scopes: platform, headless server.

- **Production:** `server/headless/bootstrap.rs:100` (`take_startup_cwd`) calls `unsafe { std::env::remove_var(SHEPR_STARTUP_CWD) }` inside `rt.block_on`. By then the multi-thread tokio workers, the API server thread (started at bootstrap.rs:13) and whatever `App::new` spawned during session restore are all running. That is exactly the precondition `remove_var`'s safety contract forbids (a getenv/unsetenv race, undefined behaviour in glibc). It has no SAFETY comment, which fits the mechanical `scripts/wrap_env_unsafe.py` pass.
  - Suggested fix: read the value in `main` before any thread starts, or don't mutate the environment at all.
- **Tests:** each module has its own `env_lock()`, so the locks do not exclude each other.
  - `platform/linux.rs:1667` sets `PATH` to only a temp dir.
  - Meanwhile the lock-free tests `finite_clipboard_commands_report_exit_status` ("sh") and `read_clipboard_text_with_command_reads_utf8` ("printf") depend on looking up `PATH`.
  - `clipboard_commands_prefer_wayland_when_available` (`:1456`) and `read_clipboard_text_commands_include_session_backends` (`:1710`) leave fake `DISPLAY`/`WAYLAND_DISPLAY` values set for the rest of the run.
  - Expect flaky tests, and tests that pass for the wrong reason.

## PLAT-005 - Host input framing loses or stalls input

- **Where:** `src/raw_input.rs`.
- A complete bracketed paste containing invalid UTF-8 makes `extract_one_event` return `None` (`:540`). The framer stalls, and at the idle flush the whole buffer is dropped (`:371`): the paste and any keys typed after it within that window are lost silently. Decoding lossily would avoid this.
- An unterminated `ESC[200~` is held forever with no size limit (`:253`). All later input queues behind it, so the client looks hung.

## PLAT-006 - The platform layer doesn't match its stated structure

- **Contract:** "OS-specific code lives in linux.rs; mod.rs holds the shared interface."
- `platform/mod.rs` itself holds libc calls: setsid, getsid, and the SIGWINCH sigaction.
- `unix_common.rs` is a leftover unix-vs-windows layer.
- Several shims only exist for other platforms:
  - `check_config_write_target` does nothing.
  - `write_existing_config` always returns false; its caller branch at `integration/config_file.rs:75` is dead.
  - `monitor_host_shutdown` returns an `Option` that is always `Some`.
  - `noninteractive_process::command` is just `Command::new`.
  - `create_private_state_file` forwards to `create_remote_ssh_config_file`.
  - `sync_parent_directory(path)` syncs `path` itself, not its parent.
- `fits_unix_socket_path` uses 103 bytes, which is macOS's limit (Linux allows 107), so it rejects SSH control paths that would work (`unix_common.rs:345`, duplicated in `remote/attach.rs:1654`).
- **Recommendation:** collapse everything into one flat `platform` module with no re-export layering.

## PLAT-007 - Concurrent clients share one rotating log file

- `logging.rs` keeps no rotated files and deletes the log on rotation. Concurrent clients share `shepr-client.log`, so one client's rotation leaves the others writing to an unlinked inode.

## PLAT-008 - Logs are world-readable and can record raw keystrokes

- Logs are created 0644, and with `SHEPR_LOG=debug` they record raw keystrokes (`raw_input.rs:94`).

## PLAT-009 - Daemon detection is true for any session leader

- `current_process_is_detached_server_daemon` (`mod.rs:93`) is true for any session leader, including a client started with `terminal -e shepr`. It feeds the remote `DaemonDetach` restart decision.

## PLAT-010 - A cancelled host shutdown still stops the server

- Once `PrepareForShutdown(true)` is seen, `linux/shutdown.rs:69` never looks at a later `false`, so the server exits even if the host shutdown is cancelled.

## PLAT-011 - Release-profile tests may touch the real config directory

- `config/io.rs:20` picks the directory name with `cfg!(debug_assertions)`, and `brokkr test` defaults to release. Any test that does not override `XDG_CONFIG_HOME` touches the real `~/.config/shepr`. The hunter did not audit which tests do.
