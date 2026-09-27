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
