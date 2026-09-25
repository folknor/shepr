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

## EP-001 - A single end-of-stream during the handshake stops Local from ever reconnecting

- **Claim broken:** the supervisor keeps Local supervised and reconnects it. Its own message says "start its server to reconnect".
- **How it happens:**
  - `supervisor.rs:310-321` (`handshake_error`) turns `ClientError::Protocol(FramingError::UnexpectedEof)` into `ErrorKind::InvalidData`.
  - `saved.rs:121-131` (`saved_ssh_failure_needs_attention`, also used for Local) treats `InvalidData` as `Attention`. Its text needle `"handshake"` also catches `"server shut down during handshake"`.
  - `supervisor.rs:182-187` gives `Attention` a 30 s retry only when the endpoint is not Local, so Local gets `next_attempt = None`.
  - Nothing in the client UI can re-arm it: `ClientShellAction` has no reconnect.
- **Triggers:** a Local server that accepts and then closes while restarting; the `Unsupported` result for a Local lacking surface interest; any `handle_endpoint_attention` on Local (`shell_runtime.rs:497-514`), which a single malformed snapshot control triggers in federated mode (`mod.rs:1197-1219`).
- **Effect:** Local stays dead until the client restarts.
- **Same classification on SSH:** an EOF before Welcome (a network drop between discovery and the bridge, or the remote `ensure_remote_server_running` failing) becomes a 30 s "attention" instead of backoff. The real SSH stderr is only logged by the bridge (`attach.rs:1121-1127`), so the shown diagnostic is "unexpected end of stream".

## EP-002 - A failed handoff leaves the selection pointing at the failed target, which starts an endless retry loop

- **Where:** `mod.rs:845-850` calls `select_endpoint` and `store_selection()` before the activation is even prepared, and a rollback never reverts it.
- **Loop:** after a `RestoredSource` rollback, the next snapshot from any endpoint (`mod.rs:1260-1286`) sees that the selected target has a snapshot, its connection is `!surface_active`, and no activation is pending. So it schedules `ActivateEndpoint(target)` again.
- **Effect:** every retry releases the source surface first, freezes input for up to the 5 s timeout, then rolls back. This repeats on every snapshot with no backoff and no memory of the failure. The persisted `endpoint-selection.json` also points at the failed machine for the next launch.

## EP-003 - Saved-machine changes do not reach open clients, although the CLI says they do

- **Claim broken:** `cli/machine.rs:18` says "Changes apply automatically to open local Shepr clients", `:314` says "Open Shepr clients connect automatically", and `:202` says "Open Shepr clients retry within 30 seconds".
- **Actual behaviour:**
  - The client loads `EndpointCatalog` once (`mod.rs:150-157`). `EndpointSupervisors::new` only uses the profiles from startup (`mod.rs:476-477`). `set_endpoint_catalog` is only called at startup, and nothing watches the file.
  - Added machines never connect, and removed or disabled ones keep reconnecting.
  - `federated` is also fixed at launch, so a Local-only client stays fatal on Local loss after a machine is added.
  - The `reconnect` message is only true for an endpoint already in `Attention`.

## EP-004 - `endpoint_disconnected` ignores `source_available` when the target is lost

- **Where:** `activation.rs:745-756`.
- **Setup:** the source was prepared as disconnected, via `disconnected_endpoint_lease` with generation 0 and a cached or empty boot id (`activation/protocol.rs:40-53`).
- **What happens when the target drops:** `start_source_restore` sends a resize, `surface.set(true)` and a focus baseline to whatever connection that source id has now, for example a Local that reconnected with a new generation.
  - The acknowledgement can never match generation 0, so the handoff waits the full 5 s timeout and ends as `Unavailable`.
  - The source server may by then hold a live surface and a focus-true state that the client never releases.
- **Compare:** the `ReleasingTargetForRollback` path (`activation.rs:448`, `848`) does check `source_available`.

## EP-005 - Reconnects re-read config from disk, although config is read once at launch

- **Claim:** AGENTS.md, config is read once at launch.
- Each saved-SSH connect attempt runs `connect_saved_ssh`, then `validated_saved_ssh`, then `RemoteSsh::new_noninteractive`, which calls `Config::load()` (`attach.rs:358-366`, `saved.rs:155-160`).
- It also writes a fresh temporary SSH config directory each time.
- So a long-running client picks up edits to `remote.manage_ssh_config` in the middle of a session.

## EP-006 - The activation's surface geometry is computed from the source's layout

- **Where:** `shell_runtime.rs:244-251`.
- The surface height depends on `focused_tab_count()` when `hide_tab_bar_when_single_tab` is set (`shell/config.rs:151`).
- `complete()` (`activation.rs:880-981`) switches the projection to the target and commits a surface built at the source's geometry. No resize follows.
- `install_client_shell_snapshot` only compares sizes before and after its own install, so it never corrects this.
- Result: switching between a single-tab and a multi-tab workspace leaves pane geometry wrong by one row.
- Related: UI-002 (the same one-row mismatch can panic the client).

## EP-007 - The saved bridge command assumes a POSIX login shell

- `bridge_command` (`attach.rs:217-225`) is passed straight to sshd, which runs it in the user's login shell as `printf …\nexec …`.
- Discovery explicitly works around non-POSIX login shells (the xonsh comment at `attach.rs:660-662`), and the API bridge wraps its command in `/bin/sh -c` (`attach.rs:1013`).
- So discovery and the API bridge work for those users, but the client bridge would not.

## EP-008 - `store_profiles` writes the selection into `endpoints.json`

- It serializes the live `selected_profile` there (`catalog.rs:74-82`, `142`). A `machine add`/`rename` therefore copies the selection into the catalog, and it becomes the fallback if the selection file is missing. This contradicts the separation the file split implies.
- The `load_profiles` comment says live clients keep their own selection, yet every client writes the shared selection file on every activation, including automatic ones, with an fsync each time.

## EP-009 - Chunked endpoint command responses have no size cap

- They accumulate with no limit (`endpoint_commands.rs:228`); only the 60 s timeout bounds them.

## EP-010 - Client exit leaks bridge sockets and temp directories

- Bridge teardown runs on writer threads after `run_client_loop` returns, and the process can exit first. That leaks `shepr-ssh-<pid>-<id>.sock` files and `/tmp/shepr-ssh-<pid>-N` config directories.

## EP-011 - Bridge sockets use predictable paths in `$TMPDIR`

- Names are pid-derived (`unix_common.rs:329`), and permissions are set to 0600 only after `bind` (`attach.rs:1082-1089`), so there is a small window.
- A socket another user leaves at that path can block a connect attempt.

## EP-012 - Every reconnect repeats full discovery

- That is at least four SSH round trips (login-shell `command -v`, `sh` `command -v`, the candidates script, a status probe per candidate). The cached metadata is written but never read on this path.

## EP-013 - Possible hang joining the SSH stderr thread (unverified)

- With `ControlPersist=600`, an OpenSSH master that keeps stderr open would make `wait_with_output_timeout` block joining the stderr thread even after the child has exited. The hunter marked this unverified.

## EP-014 - Unreachable `ConnectionLost` returns after registry writes

- The `Err(ConnectionLost)` returns after `write_to_server(&mut registry, …)` in `mod.rs` are unreachable, because the registry sink always returns `Ok`. They read as if they do something. (The dead WouldBlock arm in `server_reader_thread` is filed under WIRE-010.)

## EP-015 - Presentation freeze handling looks inverted relative to its comments

Raised by the client UI hunter for this scope.

- `install_client_shell_snapshot` calls `present_frame` (which respects the freeze) when `projection_pending`, and otherwise `present_frozen_chrome` (which bypasses it) (`shell_runtime.rs:605-611`).
- `finish_client_shell_input` bypasses the freeze whenever no activation is pending, including after `present_handoff_unavailable` froze presentation with `pending = None`. That lets full frames with the stale pane surface through.
