Platform and process plumbing: findings

This was read-only. Apart from `ls` and a few `rg` searches, no shell commands were run, and nothing was built or edited. The findings are ordered by severity.

**1. The rule that client and server are always the same build is not enforced (high)**
- The claim is stated in `protocol/wire.rs:6` ("Client and server are always the same build") and in AGENTS.md ("no frozen fixtures").
- The only guard is `PROTOCOL_VERSION: u32 = 1` (`src/protocol/wire.rs:21`), a constant someone has to bump by hand. Nobody will, because the repo says to change the protocol freely.
- `src/build_info.rs` ("Build identity helpers") returns only `CARGO_PKG_VERSION`, which is `0.1.0` for every build.
- So after `brokkr install`, an old running server (or an old binary copied by hand to a remote host) passes every check:
  - `server/autodetect.rs:92`
  - `remote/restart_policy.rs:16`
  - `remote/host.rs:42`
  - the handshake at `wire.rs:1206`
- The positional codec is not self-describing, so a mismatch decodes silently into garbage instead of failing.
- `autodetect.rs:232-236` also skips the check entirely when any saved SSH machine is enabled (`saved_federation`).
- Fix: embed a real build fingerprint (a git sha plus a dirty/timestamp mark from a build script, or a hash of the wire schema) in `version()`, `ping` and the handshake, and compare that. Drop the manual constant.

**2. Pane teardown can signal an unrelated process session (high impact, low probability)**
- Sites: `pane.rs:924` `shutdown_pane_processes`, and `platform/linux.rs:708` `session_processes`, `:959`.
- The child is reaped early by `child.wait()` in a blocking task (`pane.rs:1268`). But `child_pid` is kept, and at pane close `session_processes(child_pid)` reads `/proc/<child_pid>/stat` to find the session id.
- If that pid has been reused, it returns the new owner's whole session and sends it SIGHUP, then SIGTERM, then SIGKILL. The fallback `pids.push(child_pid)` also signals a dead or reused pid.
- It also misses real leftovers: once the leader is gone, background jobs still in the pane's session are never found.
- The fix is simple: the child is always a session leader (`pty/backend.rs` calls setsid), so the session id is `child_pid` by construction. Scan for `sid == child_pid` and never read the leader's stat.
- Related cost: every pane close does a full `/proc` scan plus up to 750ms of `thread::sleep`, synchronously on the server event loop (`app/runtime.rs:15`). Closing a workspace pays this once per pane, and every client stalls meanwhile.
- The same pid-reuse pattern exists in `unix_common.rs:400` `StatusCommandGuard::terminate`. It calls `kill(-pgid, SIGKILL)` on drop, even after tokio has reaped the leader.

**3. Windows code is still in the tree, and tests run a path production never takes (breaks the Linux-only rule)**
- `src/input/model.rs:34-221` uses `#[cfg(any(windows, test))]` for `PhysicalKeyId`, `KeySource::WindowsConsole` and `with_windows_record`.
- `src/protocol/wire.rs:284` has `cfg!(any(windows, test))`, and `:315` has `#[cfg(any(windows, test))]`.
- `WindowsKeyRecord` / `windows_record` still travel on the wire in `ClientPaneInputEvent::Key`, and `windows_dead_key` / `with_windows_composition_hint` still run in production.
- In production `windows_record` is always `None`. Under test the Windows branch is compiled in and exercised, so key-identity and release-tracking tests pass on logic the shipped binary does not have.
- Smaller cfg leftovers: `#[cfg(unix)]` in the integration tests, `pane/terminal.rs:4024,4070` and `client/endpoint/ssh_metadata.rs:160,201`, plus `#[cfg(target_os = "linux")]` in `pane/terminal/migration_tests.rs:121`.
- Recommendation: delete the whole Windows key-record path, including its wire fields.

**4. `--help` describes things that do not exist (`src/main.rs`)**
- Line 480 advertises `shepr config reset-keys`, but `cli.rs:112` only implements `check`. The command prints config help and exits 2.
- Line 523 prints `logging::help_log_paths_summary()` (`logging.rs:33`), which names `<data_dir>/shepr.log`. No process ever writes it: only `shepr-server.log` (`server/headless/bootstrap.rs:5`) and `shepr-client.log` (`client/mod.rs:98`) exist.
- The unknown-command whitelist at `main.rs:559-577` is effectively dead. `cli::maybe_run` has already consumed every listed command except bare `server` and `client`.

**5. `SHEPR_BIN_PATH` is built two different ways**
- `platform::launch_executable()` (`linux.rs:89`) strips the " (deleted)" suffix Linux adds once a binary is replaced. Only `integration/env.rs:30` uses it.
- `app/tab_bar_status.rs:22` exports `SHEPR_BIN_PATH` from raw `current_exe()`. After an install, status commands get `/…/shepr (deleted)` and fail.
- `server/autodetect.rs:124` (`spawn_server_daemon`), `remote/attach.rs:1617` (`run_client_process`) and `cli/status.rs:334` also use raw `current_exe()`.

**6. Process environment is changed after threads exist**
- Production: `server/headless/bootstrap.rs:100` calls `unsafe { remove_var(SHEPR_STARTUP_CWD) }` inside `block_on`. The API listener thread and the tokio workers are already running, which is a getenv/unsetenv race (undefined behaviour in glibc). It has no SAFETY comment, which fits the mechanical `scripts/wrap_env_unsafe.py` pass.
- Tests: each module has its own `env_lock()`, so the locks do not exclude each other.
  - `platform/linux.rs:1667` sets `PATH` to *only* a temp dir.
  - Meanwhile the lock-free tests `finite_clipboard_commands_report_exit_status` ("sh") and `read_clipboard_text_with_command_reads_utf8` ("printf") depend on looking up `PATH`.
  - `clipboard_commands_prefer_wayland_when_available` (`:1456`) and `read_clipboard_text_commands_include_session_backends` (`:1710`) leave fake `DISPLAY`/`WAYLAND_DISPLAY` values set for the rest of the run.
  - Expect flaky tests, and tests that pass for the wrong reason.

**7. Host input framing (`src/raw_input.rs`)**
- A complete bracketed paste containing invalid UTF-8 makes `extract_one_event` return `None` (`:540`). The framer stalls, and at the idle flush the whole buffer is dropped (`:371`): the paste and any keys typed after it within that window are lost silently. Decoding lossily would avoid this.
- An unterminated `ESC[200~` is held forever with no size limit (`:253`). All later input queues behind it, so the client looks hung.

**8. Clipboard read can freeze the client**
- `platform::read_clipboard_text` (`linux.rs:838`) runs synchronously from the client's key routing (`client/shell/input.rs:383`).
- It has no timeout: `xclip -out` against an unresponsive selection owner blocks client input indefinitely.

**9. Platform layer structure (contract: "OS-specific code lives in linux.rs; mod.rs holds the shared interface")**
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
- Recommendation: collapse everything into one flat `platform` module with no re-export layering.

**10. Lower-severity items**
- **Shared client log:** `logging.rs` keeps no rotated files and deletes the log on rotation. Concurrent clients share `shepr-client.log`, so one client's rotation leaves the others writing to an unlinked inode.
- **Log permissions:** logs are created 0644, and with `SHEPR_LOG=debug` they record raw keystrokes (`raw_input.rs:94`).
- **Daemon detection:** `current_process_is_detached_server_daemon` (`mod.rs:93`) is true for any session leader, including a client started with `terminal -e shepr`. It feeds the remote `DaemonDetach` restart decision.
- **Cancelled host shutdown:** once `PrepareForShutdown(true)` is seen, `linux/shutdown.rs:69` never looks at a later `false`, so the server exits even if the host shutdown is cancelled.
- **Release-profile tests:** `config/io.rs:20` picks the directory name with `cfg!(debug_assertions)`, and `brokkr test` defaults to release. Any test that does not override `XDG_CONFIG_HOME` touches the real `~/.config/shepr`. I did not audit which tests do.
- **Stale config text:** `DEFAULT_CONFIG` still says the `[advanced]` scrollback limit "Matches Ghostty's default", which is stale since the switch to alacritty.

Main files: `/home/folk/Programs/shepr/src/platform/{mod.rs,linux.rs,unix_common.rs,client_state.rs,ssh_agent.rs,linux/shutdown.rs}`, `/home/folk/Programs/shepr/src/{main.rs,raw_input.rs,logging.rs,build_info.rs}`, `/home/folk/Programs/shepr/src/pane.rs`, `/home/folk/Programs/shepr/src/server/{autodetect.rs,headless/bootstrap.rs}`, `/home/folk/Programs/shepr/src/input/model.rs`, `/home/folk/Programs/shepr/src/protocol/wire.rs`, `/home/folk/Programs/shepr/src/app/tab_bar_status.rs`.
