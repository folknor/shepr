# Endpoint and SSH defects

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

## EP-003 - Saved-machine changes do not reach open clients, although the CLI says they do

- **Claim broken:** `cli/machine.rs` says "Changes apply automatically to open local Shepr clients", "Open Shepr clients connect automatically", and "Open Shepr clients retry within 30 seconds".
- **Actual behaviour:**
  - The client loads `EndpointCatalog` once (`mod.rs`). `EndpointSupervisors::new` only uses the profiles from startup. `set_endpoint_catalog` is only called at startup, and nothing watches the file.
  - Added machines never connect, and removed or disabled ones keep reconnecting.
  - `federated` is also fixed at launch, so a Local-only client stays fatal on Local loss after a machine is added.
  - The `reconnect` message is only true for an endpoint already in `Attention`.

## EP-005 - Reconnects re-read config from disk, although config is read once at launch

- **Claim:** AGENTS.md, config is read once at launch.
- Each saved-SSH connect attempt runs `connect_saved_ssh`, then `validated_saved_ssh`, then `RemoteSsh::new_noninteractive`, which calls `Config::load()` (`attach.rs:358-366`, `saved.rs:155-160`).
- It also writes a fresh temporary SSH config directory each time.
- So a long-running client picks up edits to `remote.manage_ssh_config` in the middle of a session.

## EP-006 - The activation's surface geometry is computed from the source's layout

- **Where:** `shell_runtime.rs:244-251`.
- The surface height depends on `focused_tab_count()` when `hide_tab_bar_when_single_tab` is set (`shell/config.rs:151`).
- `complete()` (`activation.rs`) switches the projection to the target and commits a surface built at the source's geometry. No resize follows.
- `install_client_shell_snapshot` only compares sizes before and after its own install, so it never corrects this.
- Result: switching between a single-tab and a multi-tab workspace leaves pane geometry wrong by one row. The client no longer panics on the mismatch, but see UI-016 for what the oversized surface still does.

## EP-007 - The saved bridge command assumes a POSIX login shell

- `bridge_command` (`attach.rs:217-225`) is passed straight to sshd, which runs it in the user's login shell as `printf …\nexec …`.
- Discovery explicitly works around non-POSIX login shells (the xonsh comment at `attach.rs:660-662`), and the API bridge wraps its command in `/bin/sh -c` (`attach.rs:1013`).
- So discovery and the API bridge work for those users, but the client bridge would not.

## EP-010 - Client exit leaks bridge sockets and temp directories

- Bridge teardown runs on writer threads after `run_client_loop` returns, and the process can exit first. That leaks `shepr-ssh-<pid>-<id>.sock` files and `/tmp/shepr-ssh-<pid>-N` config directories.

## EP-011 - Bridge sockets use predictable paths in `$TMPDIR`

- Names are pid-derived (`unix_common.rs:329`), and the socket is bound before its mode is set to 0600 (`bind_private_local_listener`, called from `attach.rs`), so there is a small window.
- A socket another user leaves at that path can block a connect attempt.

## EP-012 - Every reconnect repeats full discovery

- That is at least four SSH round trips (login-shell `command -v`, `sh` `command -v`, the candidates script, a status probe per candidate). The cached metadata is written but never read on this path.

## EP-013 - Possible hang joining the SSH stderr thread (unverified)

- With `ControlPersist=600`, an OpenSSH master that keeps stderr open would make `wait_with_output_timeout` block joining the stderr thread even after the child has exited. The hunter marked this unverified.

## EP-015 - Presentation freeze handling looks inverted relative to its comments

Raised by the client UI hunter for this scope.

- `install_client_shell_snapshot` calls `present_frame` (which respects the freeze) when `projection_pending`, and otherwise `present_frozen_chrome` (which bypasses it) (`shell_runtime.rs:605-611`).
- `finish_client_shell_input` bypasses the freeze whenever no activation is pending, including after `present_handoff_unavailable` froze presentation with `pending = None`. That lets full frames with the stale pane surface through.

## EP-016 - The endpoint handshake misclassifies a server shutdown and loses write error kinds

- `src/client/handshake.rs`: in endpoint-shell mode, a `ServerShutdown` frame received in place of Welcome becomes `InvalidData` "expected endpoint welcome", which the supervisor classes as attention. It should map to `ClientError::ServerShutdown`.
- A failed hello write is wrapped as `io::Error::other(e.to_string())`, which loses the error kind the supervisor classifies on.

## EP-017 - Production `expect()` in the client endpoint code

- **Claim:** no `unwrap` in production code.
- `activation.rs`: `geometry()`, `send_latest_focus`. `shell_runtime.rs`: `handle_endpoint_attention`, `complete_endpoint_activation`. `client/mod.rs`: "checked shell mode", "checked pending activation".
