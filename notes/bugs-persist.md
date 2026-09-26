# Persistence and restore defects

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

## PER-003 - Pane history is still formatted on the event loop under the terminal lock

Surfaced in three scopes: persistence, terminal core, pane/terminal state.

- Capture is split (`persist::capture_pending_history` on the loop, `PendingHistory::resolve` on the save thread) and unchanged history isn't rewritten, but `live_history_read` (`src/persist/snapshot.rs`) still formats each pane's whole scrollback eagerly on the loop: a `TerminalRuntime` can't leave the loop and there is no `Send` handle to the terminal core.
- The terminal now has stable absolute rows (`Terminal::history_origin()`): an absolute id stays valid while it is at or above the origin, and rows below `origin + history size` are append-only. A `Send` reader could remember the last absolute row it saved and resume from the later of that and the current origin (full re-read if the origin passed it), re-reading the screen rows each time, in bounded chunks under short lock holds.

## PER-011 - A restored managed-agent name can stick to a plain shell

- The pending-resume path calls `TerminalState::restore_managed_agent`, which sets `ManagedAgentPhase::Active` before any process exists; if the typed resume command fails, `reconcile_managed_agent_at` never clears the name. Restore can't fix this alone (commented in `restored_terminal`).
- **Needs:** a phase such as `ManagedAgentPhase::AwaitingResume` set by `restore_managed_agent_for_resume(name, kind)` in `src/terminal/state.rs` (not counted by `managed_agent_launch_pending()`, a no-op in `reconcile_managed_agent_at`); in `start_pending_agent_resume` (`src/app/agent_resume.rs`), after the resume command is sent, move it to `Pending { ready_after, deadline, observed_expected: false }` so the existing deadline releases the name if the agent never appears. Then swap the call in `restored_terminal`'s `PendingResume` arm. Decide too whether a failed deferred resume (missing cwd, failed spawn) should move it back to Active or keep it waiting.

## PER-015 - Structural recommendation from the persistence hunter

- Persistence is spread out: capture, writing, the history pairing and the resume schedule sit in separate places with no single owner. The data directory is locked (`src/persist/lock.rs`). The hunter suggests one persistence actor that owns the lock, takes cheap snapshots on the loop and formats history off it (PER-003), and writes layout plus history as one bundle.
- Carried history (restored screens for runtime-less panes, and each live pane's last primary history for saves taken on the alternate screen) is owned per app as `HistoryCarry` (`src/persist/snapshot.rs`, `App.pane_history_carry`); an actor would own it.
- `persist::restore` takes one size for every pane in the session; restored panes start at that size, not their own layout size, until the first resize.

## PER-017 - Duplicate-session panes lose their saved screen on the first save

- Panes skipped as duplicate agent sessions get a runtime with history replay turned off, so their saved screen is overwritten by the (empty) live history on the first save. Possibly intentional; undecided.

## PER-020 - Agents resumed with no client start with an empty theme (decision)

- With no client attached, pending resumes start 750 ms after startup at the headless size with an empty host theme (asserted by `headless_scheduled_tasks_start_pending_agent_resume_without_foreground_client`). Some agents pick colours once at startup and won't pick up the real theme when a client attaches. Deliberate today; worth deciding whether to wait for a client's theme.
