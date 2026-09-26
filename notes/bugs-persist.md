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

- Capture is now split: `persist::capture_pending_history` runs on the loop, `PendingHistory::resolve` and the layout fingerprint run on the save thread, and unchanged history is not rewritten. But `live_history_read` (`src/persist/snapshot.rs`) still formats each pane's whole scrollback eagerly on the loop, because a `TerminalRuntime` can't leave the loop and there is no `Send` handle to the terminal core.
- **Needs:** a `Send` history reader on `TerminalRuntime`/`PaneRuntime` (e.g. a clone of `Arc<PaneTerminal>` behind a small type), ideally formatting in bounded chunks under short lock holds, which needs row addresses stable while scrollback grows or is trimmed (see TERM-015). Then `live_history_read` returns a closure over it.

## PER-007 - When a workspace or tab is dropped during restore, the saved indices point at the wrong item

Surfaced in two scopes: persistence, app core.

- `restore_workspace` returns `None` for a workspace with no tabs, and `restore_tab` can drop a tab (including for layout leaves without saved state).
- `snap.active`, `snap.selected` (in `App::new`) and `snap.active_tab` are only clamped, never remapped, so after a drop they select a different workspace or tab. `zoomed` can survive pruning down to a single pane.
- `generate_workspace_id()` (for a snapshot with no `id`) runs before `reserve_workspace_ids`. It can hand out an ID that a later saved workspace already owns, giving duplicate workspace IDs.

## PER-011 - A restored managed-agent name can stick to a plain shell

- In the pending-plan branch, `restore_managed_agent` marks the agent `Active` before any process exists.
- If the typed resume command fails (for example, binary not found), `reconcile_managed_agent_at` never clears the name, because `Active` with no known agent does not trigger a clear.

## PER-013 - The `cjk_ime` docs talk about macOS

- The `cjk_ime` docs in `src/config/model.rs` talk about macOS in a Linux-only fork.

## PER-015 - Structural recommendation from the persistence hunter

- Persistence is spread out: capture, writing, the history pairing and the resume schedule sit in separate places with no single owner. The data directory is now locked (`src/persist/lock.rs`). The hunter suggests one persistence actor that owns the lock, takes cheap snapshots on the loop and formats history off it (PER-003), and writes layout plus history as one bundle.
- History for panes without a runtime is carried through a process-wide map in `src/persist/snapshot.rs`; the actor should own that state instead of a hidden global.
- Restore should carry every saved `PaneSnapshot` field forward whether it succeeds or fails, instead of rebuilding it per branch (PER-011).
- `persist::restore` takes one size for every pane in the session; restored panes start at that size, not their own layout size, until the first resize.

## PER-017 - Duplicate-session panes lose their saved screen on the first save

- Panes skipped as duplicate agent sessions get a runtime with history replay turned off, so their saved screen is overwritten by the (empty) live history on the first save. Possibly intentional; undecided.

## PER-020 - Agents resumed with no client start with an empty theme (decision)

- With no client attached, pending resumes start 750 ms after startup at the headless size with an empty host theme (asserted by `headless_scheduled_tasks_start_pending_agent_resume_without_foreground_client`). Some agents pick colours once at startup and won't pick up the real theme when a client attaches. Deliberate today; worth deciding whether to wait for a client's theme.
