I read through `crates/shepr-remote` (lib.rs, remote/{saved,bridge,ssh,launch,discovery,server_lifecycle,host,autodetect,args,process,ssh_agent}.rs, machine/{catalog,ssh_metadata,executable}.rs) and the platform code it lands in (`ssh_paths.rs`, `remote_bridge.rs`, `remote_bridge_io.rs`, `ssh_agent.rs`). Per the brief I edited nothing and ran nothing, so none of this was checked against a build or a test run.

I found nothing that stops unreachable machines failing soft, as far as this crate goes. Timeouts and missed deadlines count as link failures, and link failures keep the discovery progress and the remembered executable. Most findings below are contract mismatches, and a few are real partial-failure gaps. Most important first:

1. **A `manage_ssh_config=true` setting can be silently dropped** (`remote/ssh.rs`, `RemoteSsh::new`). If `write_managed_ssh_config` fails, the code logs at debug and falls back to plain ssh ("using plain ssh"). One common cause: `shared_ssh_control_path` finds neither a root-owned sticky `/tmp` nor a valid `/run/user/<uid>`.
   - This breaks the "any config problem fails the launch; no fallbacks" rule.
   - It also defeats auth recovery. `ssh_authentication_command` authenticates through the managed ControlPath, but a `RemoteSsh` without a managed config never uses that master.
   - `SavedSshConnector` rebuilds the config on each attempt (`missing_managed_config`), so it recovers eventually. `SavedSshApiBridge::start` (`validated_saved_ssh`) and `run_remote` never retry.
   - Fix: make managed-config creation a hard error, and check it once at launch.

2. **Saved-machine catalog edits can be lost** (`machine/catalog.rs`). `add_ssh`/`remove_ssh` followed by `store_profiles` is load → modify → rename with no lock. Two `shepr machine add/remove` running at once silently lose one change. The code says catalog edits happen while clients run, so this is a real partial-failure hole. Fix: an `flock` on a lock file around the read-modify-write.

3. **"Matching Shepr" discovery does not check the build** (`remote/discovery.rs`). `StatusProbe` accepts any candidate whose `status client --json` has *some* `version` (`parse_client_status_json(...).find(|s| s.version.is_some())`). The not-ready message still says "matching Shepr is not ready".
   - The build is only checked later, by the handshake preamble (see `server_lifecycle.rs` and `host.rs`).
   - A mismatched remote install fails the handshake, which is not a link failure. `SavedSshConnector::connect` then drops its remembered executable and runs full discovery again, which finds the same binary. So every retry does the full set of SSH round trips, forever.
   - Fix: have the probe compare the build/protocol identity (`shepr_protocol::build_version()` / `PROTOCOL_VERSION`) so discovery fails once with a clear Compatibility error.

4. **The doc says the cache holds one kind of result, but the code stores two.** `discovery.rs` says "The two proofs stay separate because the metadata cache records only this one" (the API-forwarding check). But `saved.rs:181` (`metadata_cache.store(&discovered)`) writes the status-probe result into the same per-profile file. The API bridge then trusts it as `used_cached_metadata`. The stale marker in `cached_remote_api_command` covers this at runtime, so it is harmless, but the stated contract is false. Either key the two caches apart or fix the doc.

5. **Remote command quoting breaks for executable paths that need quoting** (`launch.rs`: `cached_remote_api_command`, `RemoteExecutable::bridge_command`).
   - The comments require the wrapped `/bin/sh -c '<script>'` to contain nothing to escape, so that non-POSIX login shells (xonsh, nushell) receive one plain word.
   - But `RemoteExecutable::parse` accepts paths with spaces (its own test marks `/home/a b/shepr` valid). `shell_quote` then puts `'\''` inside the outer quote, exactly what the comment rules out.
   - Fix: reject such paths in `RemoteExecutable::parse`, or pass the script on stdin/base64 so nothing is nested.

6. **Classification is plain substring matching** (`lib.rs::classify_ssh_diagnostic`).
   - Any "permission denied" without an auth method, and any "not ready", becomes `Compatibility` (needs attention). That includes such text from remote command stderr or a login banner.
   - Exit code 255 from the *remote command* is treated as an SSH link failure (`from_ssh_output`, `is_link_failure`). That keeps a stale remembered executable and discovery progress instead of rediscovering.
   - Errors re-wrapped with `format!` in `bridge.rs` ("remote bridge upload failed: {err}") lose the typed diagnostic and get classified again from text.
   - A structured failure type carried end to end would replace this.

7. **Unverified, flagged: `prepare_saved_ssh` could hang.** `prepare_saved_ssh` (`launch.rs`) runs `exec shepr remote-client-bridge </dev/null` over *interactive* `sh_output`, which has no timeout, with the idle watchdog off. It returns only if the remote server closes a client connection that half-closes before its handshake. If the server instead waits for a handshake with no timeout, `machine add`'s prepare step hangs. I did not check the server side.

8. **Bridge-level smells** (`remote/bridge.rs`):
   - The accept thread runs `bridge_connection` inline and then keeps accepting. Any later connect by the same user to that socket starts a fresh ssh.
   - `reported_failure` always blocks up to 1s even when nothing failed.
   - The failure channel is `sync_channel(1)` with `try_send`, so a failure left over from an earlier connection on the same bridge can be reported for a later one.

9. **Remote-host server probing** (`autodetect.rs`, used by `host.rs::ensure_remote_server_running`).
   - `is_server_listening_at` treats any unexpected connect error (e.g. EACCES) as "not listening" and starts a second daemon.
   - Two bridges arriving at once (client bridge plus API bridge) both start daemons. This relies on the server refusing the second bind.
   - Every probe opens and drops a real client connection, which the server has to time out.
   - Structurally, `autodetect` (local server launch) does not belong in `shepr-remote`. It belongs with the client or binary.

10. **Smaller items:**
    - `create_remote_ssh_config_dir` checks that the path fits a `ctl` control socket, but that socket is never created there (the control path is `shared_ssh_control_path`), so the check does nothing. It also caps live config dirs per process at 100 per base directory. With up to 64 saved machines plus API bridges, that can fail with `AlreadyExists`.
    - `remove_ssh` never deletes `state/client/ssh-metadata/<id>.json`, so those files pile up.
    - `EndpointCatalogChanges` has retire-and-restart handling for a changed target/session, but the documented add/remove-only machine commands can never produce that change.
    - `SavedSshApiBridge::start` writes a temporary ssh config dir even when it uses cached metadata.
    - `wait_with_output_timeout` captures remote stdout/stderr with no size limit.

**Checked and fine:** random socket-path tokens mean retries and replacement connectors don't collide on bridge socket paths. The teardown registry, the `ssh_agent` symlink registry, deadline propagation through `noninteractive_timeout`, and resuming discovery progress on link failures all look correct.

**Not checked:** the "client keeps serving remote machines when Local is lost, reconnects when it returns" contract lives in `shepr-client`, outside what I traced. Also unconfirmed: whether the client sends keepalives. Saved bridges run with `--idle-timeout-v1`, and the remote watchdog calls `process::exit(1)` after 60s with no bytes either way (`shepr-platform/src/remote_bridge.rs`). Without a keepalive, idle saved machines would drop and reconnect every minute.
