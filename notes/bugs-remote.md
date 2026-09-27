# Defects: shepr-remote

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

Hunter coverage: all of `crates/shepr-remote` plus the platform code it lands in (`ssh_paths.rs`, `remote_bridge.rs`, `remote_bridge_io.rs`, `ssh_agent.rs`). The hunter found nothing in this crate that stops unreachable machines failing soft. The client-side "keep serving remotes when Local is lost" contract was not traced here (see bugs-client.md).

## RMT-001 - A managed-SSH setup failure surfaces at first connect, not at launch

Managed SSH config creation is a hard error and never falls back to plain ssh, and a saved connector now retries a failed setup on its next attempt. But `SavedSshConnector::new` is infallible, so a setup error that would fail on every attempt (for example an over-long `XDG_RUNTIME_DIR`) reaches the client only on the first connection attempt instead of failing the client launch. Making it launch-fatal needs the client's endpoint supervisor to surface it.

## RMT-006 - SSH failure classification is plain substring matching

`lib.rs::classify_ssh_diagnostic`.
- Any "permission denied" without an auth method, and any "not ready", becomes `Compatibility` (needs attention). That includes such text from remote command stderr or a login banner.
- Exit code 255 from the *remote command* is treated as an SSH link failure (`from_ssh_output`, `is_link_failure`). That keeps a stale remembered executable and discovery progress instead of rediscovering.
- Errors re-wrapped with `format!` in `bridge.rs` ("remote bridge upload failed: {err}") lose the typed diagnostic and get classified again from text.
- A structured failure type carried end to end would replace this.

## RMT-007 - prepare_saved_ssh could hang (unverified)

`prepare_saved_ssh` (`launch.rs`) runs `exec shepr remote-client-bridge </dev/null` over *interactive* `sh_output`, which has no timeout, with the idle watchdog off. It returns only if the remote server closes a client connection that half-closes before its handshake. If the server instead waits for a handshake with no timeout, `machine add`'s prepare step hangs. The hunter did not check the server side.

## RMT-008 - Bridge-level smells

`remote/bridge.rs`:
- The accept thread runs `bridge_connection` inline and then keeps accepting. Any later connect by the same user to that socket starts a fresh ssh.
- `reported_failure` always blocks up to 1s even when nothing failed.
- The failure channel is `sync_channel(1)` with `try_send`, so a failure left over from an earlier connection on the same bridge can be reported for a later one.

## RMT-009 - Remote-host server probing is loose

`autodetect.rs`, used by `host.rs::ensure_remote_server_running`.
- `is_server_listening_at` treats any unexpected connect error (e.g. EACCES) as "not listening" and starts a second daemon. (A second daemon is now refused cleanly by the data-dir lease and the socket startup lock, so this costs a wasted spawn, not a broken server.)
- Every probe opens and drops a real client connection, which the server has to time out.
- Structurally, `autodetect` (local server launch) does not belong in `shepr-remote`. It belongs with the client or binary.

## RMT-010 - Smaller remote items

- `remove_ssh` never deletes `state/client/ssh-metadata/<id>.json`, so those files pile up.
- `EndpointCatalogChanges` has retire-and-restart handling for a changed target/session, but the documented add/remove-only machine commands can never produce that change.
- `SavedSshApiBridge::start` writes a temporary ssh config dir even when it uses cached metadata.
- `wait_with_output_timeout` captures remote stdout/stderr with no size limit.
- `SavedSshConnector::connect` keeps an unreachable "saved SSH transport is unavailable" branch after the lazy setup; `xdg_runtime_dir()` in `remote/ssh.rs` is now a trivial `Ok` wrapper.

## RMT-017 - Two remote paths still skip the build check, and the flock checks are duplicated

- The API-forwarding discovery probe (`remote_api_forwarding_supported`, `remote-api-bridge --check`) does not compare the remote build identity; only the status probe does.
- Saved connector attempts through `remote/host.rs` check that the remote server socket is listening but do not preflight the running daemon's build, so a stale daemon is caught only by the bridge handshake.
- `machine/catalog.rs` (`acquire_catalog_update_lock`, blocking flock) repeats the open, ownership, mode and flock work of `shepr_platform::ipc::acquire_socket_startup_lock` (non-blocking). One platform helper taking a sidecar path and a blocking flag, returning a guard, could serve both, and the agent config-edit lock too.

## RMT-011 - Idle saved machines may drop every minute without a keepalive (unverified)

Saved bridges run with `--idle-timeout-v1`, and the remote watchdog calls `process::exit(1)` after 60s with no bytes either way (`shepr-platform/src/remote_bridge.rs`). The hunter did not confirm whether the client sends keepalives. Without one, idle saved machines would drop and reconnect every minute.
