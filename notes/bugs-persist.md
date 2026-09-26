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

## PER-001 - Two servers can share one session file, and nothing stops them

- **Where:** `src/session.rs`, `src/server/socket_paths.rs`, `src/persist/io.rs`.
- The only guard against two servers is the socket: `prepare_socket_path` refuses a socket that is already live.
- The persistence location is chosen differently. `data_dir()` depends only on `SHEPR_SESSION`, never on `SHEPR_SOCKET_PATH`.
- So a second server started with a socket override (a documented mode: "restart Shepr with the same socket override", and the AGENTS.md debug-build recipe) passes the socket check, then loads the same `session.json`.
- **Consequences:**
  - It restores the same layout.
  - It resumes the same native agent sessions, so two `claude --resume <id>` processes run on one conversation.
  - Both servers autosave to the same file. Each overwrites the other's layout, so "layout survives a restart" becomes last-writer-wins.
  - The temp file name is fixed (`json.tmp`). It is created exclusively, so two writers can no longer publish a mixed file, but one of them gets a failed save (logged) whenever they overlap. A comment in `io.rs` records the one-writer assumption.
- **Suggested fix:** take a lock (flock) on the data directory, or derive the data directory from the socket.

## PER-003 - The history save runs on the server event loop while holding every pane's terminal lock

Surfaced in three scopes: persistence, terminal core, pane/terminal state.

- **Claim:** the save is called a "background writer"; AGENTS.md says keep terminal-core locks short and treat these paths as hot.
- **Where:** `src/app/session.rs` into `PaneRuntime::snapshot_history` (`pane.rs`) into `ghostty_recent_ansi_snapshot`.
- Only file IO runs on the thread. `capture_session_save_job` runs synchronously on the event loop.
- With `experimental.pane_history` on, it calls `recent_unwrapped_ansi(usize::MAX)` for every pane. Each call formats that pane's entire scrollback (up to 1M lines) as VT while holding the terminal-core mutex, which blocks the PTY reader.
- This happens on every debounced save (5 s after any dirty change) and on every pane-exit checkpoint (`save_session_now`).
- It stalls PTY readers, rendering and client fanout.
- The background write also fsyncs `session-history.json` (can be many MB) on every debounced save; off the loop, but disk traffic every 5 s while things change.
- Small related bug: on thread-spawn failure, `start_background_session_save` captures a second time.

## PER-007 - When a workspace or tab is dropped during restore, the saved indices point at the wrong item

Surfaced in two scopes: persistence, app core.

- `restore_workspace` returns `None` for a workspace with no tabs, and `restore_tab` can drop a tab (including now for layout leaves without saved state).
- `snap.active`, `snap.selected` (in `App::new`) and `snap.active_tab` are only clamped, never remapped, so after a drop they select a different workspace or tab. The app-core hunter adds that `zoomed` can survive pruning down to a single pane.
- `generate_workspace_id()` (for a snapshot with no `id`) runs before `reserve_workspace_ids`. It can hand out an ID that a later saved workspace already owns, giving duplicate workspace IDs.

## PER-011 - A restored managed-agent name can stick to a plain shell

- In the pending-plan branch, `restore_managed_agent` marks the agent `Active` before any process exists.
- If the typed resume command fails (for example, binary not found), `reconcile_managed_agent_at` never clears the name, because `Active` with no known agent does not trigger a clear.

## PER-013 - Stale persistence and config docs

- `src/persist.rs` says the file lives at `~/.config/shepr/session.json`. Named sessions actually use `sessions/<name>/`.
- The `cjk_ime` docs in `src/config/model.rs` talk about macOS in a Linux-only fork.
- `src/layout.rs` repeats the doc line "Reconstruct a layout from a saved tree." on `from_saved`.

## PER-014 - Resume may start without a client's terminal context (to confirm)

- The test comment in `native_agent_restore_defers_runtime_launch` says resume waits "until client terminal context is known".
- With no client attached, `sync_runtime_view_geometry` appears to give the view a nonzero area (the headless size). If so, resumes start 750 ms after startup at the headless size with an empty theme (`allow_empty_theme` = due). The hunter flagged this as a possible contradiction to confirm.

## PER-015 - Structural recommendation from the persistence hunter

- Persistence is spread out: capture, writing, the history pairing and the resume schedule sit in separate places with no owner and no lock. The hunter suggests one persistence actor that:
  - owns a data-directory lock (PER-001);
  - takes cheap state snapshots on the loop and formats history off the loop, in bounded chunks under short locks (PER-003);
  - writes layout plus history as one bundle. Durable 0600 writes and the symlink-aware clear already go through `publish_private_file` / `clear_path` in `src/persist/io.rs`.
- History for panes without a runtime (pending resume, failed restore) is now carried forward through a process-wide map in `src/persist/snapshot.rs`, because a field on `TerminalState` plus a `terminals` argument to `capture_history` needed edits in other modules. The actor should own that state instead of a hidden global.
- Restore should carry every saved `PaneSnapshot` field forward whether it succeeds or fails, instead of rebuilding it per branch (PER-011).
- `persist::restore` takes one size for every pane in the session; restored panes start at that size, not their own layout size, until the first resize.

## PER-017 - Duplicate-session panes lose their saved screen on the first save

- Panes skipped as duplicate agent sessions get a runtime with history replay turned off, so their saved screen is overwritten by the (empty) live history on the first save. Possibly intentional; undecided.
