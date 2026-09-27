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

## CLT-006 - ClientLoop handoff state is spread across independent flags (structural)

Filed by the hunter as a structural observation, not a defect.
- `lib.rs`'s `ClientLoop` is a large hand-rolled state machine. Handoff state is spread across `pending_activation`, `deferred_local_activation`, `scheduled_activation`, `presentation_frozen`, `freeze_recovery_attempted` and the `selection` tracker. Fixes like `stale_freeze_recovery` and `correct_committed_surface_size` read as patches for states that combination allows.
- Collapsing these into one explicit presentation-ownership enum (Owned, Handoff, Unavailable, DeferredLocal) would make illegal combinations unrepresentable. That is the kind of rewrite worth doing here.
