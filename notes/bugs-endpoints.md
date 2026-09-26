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
- A `SavedSshConnector` is built per enabled profile at startup; a live-catalog fix should create and drop connectors per profile.

## EP-006 - The activation's surface geometry is computed from the source's layout

- **Where:** `shell_runtime.rs`.
- The surface height depends on `focused_tab_count()` when `hide_tab_bar_when_single_tab` is set (`shell/config.rs`).
- `complete()` (`activation.rs`) switches the projection to the target and commits a surface built at the source's geometry. No resize follows.
- `install_client_shell_snapshot` only compares sizes before and after its own install, so it never corrects this.
- Result: switching between a single-tab and a multi-tab workspace leaves pane geometry wrong by one row. The client no longer panics on the mismatch, but see UI-016 for what the oversized surface still does.

## EP-011 - Bridge sockets use predictable paths in `$TMPDIR`

- Names are pid-derived (`unix_common.rs`), and the socket is bound before its mode is set to 0600 (`bind_private_local_listener`), so there is a small window. The headless server's socket now avoids this with `bind_owner_only_listener` (staging directory + hard link); that helper could move into `ipc.rs` and serve here too.
- A socket another user leaves at that path can block a connect attempt.

## EP-015 - Presentation freeze handling looks inverted relative to its comments

Raised by the client UI hunter for this scope.

- `install_client_shell_snapshot` calls `present_frame` (which respects the freeze) when `projection_pending`, and otherwise `present_frozen_chrome` (which bypasses it) (`shell_runtime.rs`).
- `finish_client_shell_input` bypasses the freeze whenever no activation is pending, including after `present_handoff_unavailable` froze presentation with `pending = None`. That lets full frames with the stale pane surface through. Endpoint-result input also goes through `finish_client_shell_input`.

## EP-017 - Production `expect()` in the client endpoint code

- **Claim:** no `unwrap` in production code.
- `activation.rs`: `geometry()`, `send_latest_focus`. `shell_runtime.rs`: `handle_endpoint_attention`, `complete_endpoint_activation`. `client/mod.rs`: "checked shell mode", "checked pending activation".
