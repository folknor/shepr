# Defects: shepr-client

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

Hunter coverage: `lib.rs`, `endpoint.rs`, `endpoint/supervisor.rs`, `endpoint/local_failure.rs`, `terminal_setup.rs`, `shell_runtime.rs`, plus `shepr-termio/src/host_term/modes.rs`. Not read: activation, registry, commands, selection, the `shell/` presentation code, the tab bar, the sidebar and keybinding help.

## CLT-001 - Direct attach leaves keyboard protocols set on the host terminal after exit

Breaks the promise in `lib.rs:9` ("Restores terminal on exit").
- In attach mode, `ServerMessage::DirectTerminalKeyboardProtocol` (`lib.rs:1397-1410`) calls `set_direct_host_keyboard_protocol`, which pushes kitty flags (`\x1b[>Nu`) and sets modifyOtherKeys (`\x1b[>4;Nm`).
- `setup_direct_attach_terminal` calls `setup_terminal_with_capabilities(false, ..)`, which leaves `reset_keyboard_enhancements` and `reset_modify_other_keys` false. So `restore_terminal_state` (`terminal_setup.rs:380`) never pops the kitty entry or resets modifyOtherKeys.
- `ClientState.direct_keyboard_protocol` is dropped with the loop, and nothing resets it on detach, error or panic.
- Result: detaching from a pane whose program enabled the kitty protocol or modifyOtherKeys (vim, most agent TUIs) leaves the user's shell receiving CSI-u or modifyOtherKeys sequences.
- Fix: make `TerminalGuard` own the direct keyboard state (for example a shared `Arc<Mutex<DirectHostKeyboardState>>`) and reset it in restore and in the panic hook.

Structural note from the hunter: host terminal mode ownership is split — mouse lives in `HostMouseMode`, report-all in `ClientState`, direct keyboard in `ClientState`, title in a flag. The teardown gaps in this entry and CLT-002 come directly from that split. A single `HostModes` owner that the guard restores would fix both. Related: TRM-023.

## CLT-002 - The window title shepr set is never reset when the client exits

Contradicts the stated intent at `shell_runtime.rs:111-113` ("Undoes an outer window title only if this client set one").
- `reset_window_title` only runs from `clear_endpoint_host_effects`, i.e. when the endpoint that owns the screen disconnects or is retired.
- A normal exit (detach, Ctrl-C, `ServerShutdown`, errors) goes through `TerminalGuard::restore`, which does not touch the title. `window_title_written` exists but is ignored on exit, so the host tab keeps the last agent or workspace title after shepr is gone.

## CLT-003 - A saved-machines file that fails to load is silently treated as empty

`lib.rs:163-170`, `unwrap_or_else(.. EndpointCatalog::default())`. Two things follow:
- The machines vanish from the sidebar with only a log warning.
- `LocalFailurePolicy` becomes `ExitClient`, so losing Local now ends the client. That breaks "with saved machines configured, losing the local server does not end the client".
- It is also a quiet fallback in a project whose stated posture is "no fallbacks". Failing soft is promised for unreachable machines, not for a corrupt catalog. The later catalog watcher keeps what was already loaded on a bad reload (`lib.rs:1618`), which is reasonable, but at launch this should fail visibly.

## CLT-004 - Removing the last saved machine while Local is down leaves a client with nothing to show

`LocalFailurePolicy` is only consulted when a transport failure arrives (`lib.rs:1552`, `1224`).
- If Local already failed while machines existed, it was handled as a reconnect. If the user then removes every machine, `follow_endpoint_catalog` recomputes the policy to `ExitClient` (`lib.rs:1616`), but no new Local failure arrives.
- The client stays open showing only a reconnecting Local. That contradicts the rule that losing Local ends the client when no machines are saved.
- The reverse case (a machine added after launch) is handled: `follow_endpoint_catalog` adds a Local supervisor (`shell_runtime.rs:679-689`).
- Fix: when the policy flips to `ExitClient`, check whether Local is currently connected and exit if it is not. Better, derive "end client" from state (no endpoints left and Local down) rather than from a per-event check.

## CLT-005 - Possible double handling of a retired endpoint's failure (unconfirmed)

The failure filter in `handle_timer` (`lib.rs:1541-1546`) only discards stale failures while a connection still exists for that endpoint.
- After `follow_endpoint_catalog` calls `endpoints.disconnect(id)`, a late failure for the same endpoint would pass the filter. `handle_endpoint_disconnect` would then run again with a stale generation.
- The supervisor ignores the stale generation, but if `active_id()` still names the removed endpoint, `present_handoff_unavailable` and `clear_endpoint_host_effects` fire again, and the screen could freeze again after a Local handoff was already scheduled.
- This depends on `EndpointRegistry::disconnect`/`take_failures` internals the hunter did not read. Check whether `disconnect` purges queued failures.

## CLT-006 - ClientLoop handoff state is spread across independent flags (structural)

Filed by the hunter as a structural observation, not a defect.
- `lib.rs`'s `ClientLoop` is a large hand-rolled state machine. Handoff state is spread across `pending_activation`, `deferred_local_activation`, `scheduled_activation`, `presentation_frozen`, `freeze_recovery_attempted` and the `selection` tracker. Fixes like `stale_freeze_recovery` and `correct_committed_surface_size` read as patches for states that combination allows.
- Collapsing these into one explicit presentation-ownership enum (Owned, Handoff, Unavailable, DeferredLocal) would make illegal combinations unrepresentable. That is the kind of rewrite worth doing here.
