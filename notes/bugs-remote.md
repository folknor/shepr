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

## RMT-007 - prepare_saved_ssh could hang (unverified)

`prepare_saved_ssh` (`launch.rs`) runs `exec shepr remote-client-bridge </dev/null` over *interactive* `sh_output`, which has no timeout, with the idle watchdog off. It returns only if the remote server closes a client connection that half-closes before its handshake. If the server instead waits for a handshake with no timeout, `machine add`'s prepare step hangs. The hunter did not check the server side.

## RMT-009 - Remote-host server probing is loose

`autodetect.rs`, used by `host.rs::ensure_remote_server_running`.
- `is_server_listening_at` treats any unexpected connect error (e.g. EACCES) as "not listening" and starts a second daemon. (A second daemon is now refused cleanly by the data-dir lease and the socket startup lock, so this costs a wasted spawn, not a broken server.)
- Every probe opens and drops a real client connection, which the server has to time out.
- Structurally, `autodetect` (local server launch) does not belong in `shepr-remote`. It belongs with the client or binary.

## RMT-017 - Two remote paths skip the build-id check

Autodetect, the discovery status probe and remote daemon startup now compare the exact build id. Two paths still skip it:
- The API-forwarding discovery probe (`remote_api_forwarding_supported`, `remote-api-bridge --check`) does not compare build identity at all.
- Saved connector attempts through `remote/host.rs` check that the remote server socket is listening but do not preflight the running daemon's build, so a stale daemon is caught only by the bridge handshake.

## RMT-018 - Bridge and capture smells

- `remote/bridge.rs` `reported_failure` holds the failure-receiver mutex across its 1 s `recv_timeout`, so the accept thread can wait up to a second when it discards a stale failure. No deadlock; CLI use is serial.
- Remote SSH stdout capture is capped at 1 MiB keeping the first bytes, so a login banner over 1 MiB would push out the trailing status JSON; keep the tail instead, or cap only stderr.
- `machine/catalog.rs` `acquire_catalog_update_lock` binds an unused `_parent` only to validate the path.
- `PROTOCOL_VERSION` and the `protocol` fields in Pong, `RuntimeStatus`, session info and status output are now informational only; nothing compares them. Removing them is a schema-wide cleanup.

## RMT-011 - Idle saved machines may drop every minute without a keepalive (unverified)

Saved bridges run with `--idle-timeout-v1`, and the remote watchdog calls `process::exit(1)` after 60s with no bytes either way (`shepr-platform/src/remote_bridge.rs`). The hunter did not confirm whether the client sends keepalives. Without one, idle saved machines would drop and reconnect every minute.
