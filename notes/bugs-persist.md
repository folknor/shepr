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
  - The temp file name is fixed (`json.tmp`). It is now created exclusively, so two writers can no longer publish a mixed file, but one of them gets a failed save (logged) whenever they overlap. A comment in `io.rs` records the one-writer assumption.
- **Suggested fix:** take a lock (flock) on the data directory, or derive the data directory from the socket.

## PER-003 - The history save runs on the server event loop while holding every pane's terminal lock

Surfaced in three scopes: persistence, terminal core, pane/terminal state.

- **Claim:** the save is called a "background writer"; AGENTS.md says keep terminal-core locks short and treat these paths as hot.
- **Where:** `src/app/session.rs:39-59` into `PaneRuntime::snapshot_history` (`pane.rs`) into `ghostty_recent_ansi_snapshot`.
- Only file IO runs on the thread. `capture_session_save_job` runs synchronously on the event loop.
- With `experimental.pane_history` on, it calls `recent_unwrapped_ansi(usize::MAX)` for every pane. Each call formats that pane's entire scrollback (up to 1M lines) as VT while holding the terminal-core mutex, which blocks the PTY reader.
- This happens on every debounced save (5 s after any dirty change) and on every pane-exit checkpoint (`save_session_now`).
- It stalls PTY readers, rendering and client fanout.
- The background write now also fsyncs `session-history.json` (can be many MB) on every debounced save; off the loop, but disk traffic every 5 s while things change.
- Small related bug: on thread-spawn failure, `start_background_session_save` captures a second time.

## PER-005 - A pane's `launch_argv` is dropped on the normal restore path

- **Where:** `restore_tab`.
- `unavailable_restored_terminal` copies `pane.launch_argv`, but neither success branch does (neither the runtime spawn nor the pending-resume plan).
- The next autosave then writes the pane without it, so a successful restore quietly loses saved intent that a failed one keeps.

## PER-006 - Deferred agent resume can permanently lose that pane's history

- `pane_restore_startup` suppresses history replay whenever a resume plan exists; the reasoning is that native resume owns the conversation.
- Pending panes have no runtime, so `capture_pane_history` skips them. Any save before the resume succeeds writes a history file without those panes.
- Saves happen before resume in practice: no client attached yet, or the deferred launch failed on a missing cwd or shell (`start_pending_agent_resume` sets `restore_error`).
- In the failure case the conversation never resumed and the saved screen history is gone. That breaks "a failed or partial restore does not destroy saved intent".

## PER-007 - When a workspace or tab is dropped during restore, the saved indices point at the wrong item

Surfaced in two scopes: persistence, app core.

- `restore_workspace` returns `None` for a workspace with no tabs, and `restore_tab` can drop a tab.
- `snap.active`, `snap.selected` (in `App::new`) and `snap.active_tab` are only clamped, never remapped, so after a drop they select a different workspace or tab. The app-core hunter adds that `zoomed` can survive pruning down to a single pane.
- `generate_workspace_id()` (for a snapshot with no `id`) runs before `reserve_workspace_ids`. It can hand out an ID that a later saved workspace already owns, giving duplicate workspace IDs.

## PER-008 - Session IDs are not checked for a leading `-`

- `valid_session_id` accepts IDs starting with `-`, and `plan` puts them as a separate argument after `--resume`, `--session` and similar flags. They are also typed into an interactive shell.
- So a hook report or API call can turn an "ID" into agent flags. The test `ids_are_data_not_shell_text` claims IDs are data.
- `persisted_session_from_launch_args` already rejects a leading `-`; the report and snapshot paths don't.

## PER-010 - Restored layouts are not validated

- Saved split ratios skip `valid_split_ratio` (`TileLayout::from_saved`); only live splits and resizes clamp.
- A pane ID repeated in `LayoutSnapshot` makes `remap_inner` overwrite `id_map`, orphaning one pane.
- If a layout pane has no entry in the panes map, it falls back to the server's own current directory, and that directory then gets saved.

## PER-011 - A restored managed-agent name can stick to a plain shell

- In the pending-plan branch, `restore_managed_agent` marks the agent `Active` before any process exists.
- If the typed resume command fails (for example, binary not found), `reconcile_managed_agent_at` never clears the name, because `Active` with no known agent does not trigger a clear.

## PER-012 - Dead restore code and unread fields

- In `restore_tab`'s runtime branch, `initial_restore_agent` is always `None`, since a present plan takes the other branch. Lines 426-436 are dead.
- `PaneHistorySnapshot.lines` is written but never read.

## PER-013 - Stale persistence and config docs

- `src/persist.rs` says the file lives at `~/.config/shepr/session.json`. Named sessions actually use `sessions/<name>/`.
- The `cjk_ime` docs in `src/config/model.rs` talk about macOS in a Linux-only fork.

## PER-014 - Resume may start without a client's terminal context (to confirm)

- The test comment in `native_agent_restore_defers_runtime_launch` says resume waits "until client terminal context is known".
- With no client attached, `sync_runtime_view_geometry` appears to give the view a nonzero area (the headless size). If so, resumes start 750 ms after startup at 120x40 with an empty theme (`allow_empty_theme` = due). The hunter flagged this as a possible contradiction to confirm.

## PER-015 - Structural recommendation from the persistence hunter

- Persistence is spread out: capture, writing, the history pairing and the resume schedule sit in separate places with no owner and no lock. The hunter suggests one persistence actor that:
  - owns a data-directory lock (PER-001);
  - takes cheap state snapshots on the loop and formats history off the loop, in bounded chunks under short locks (PER-003);
  - writes layout plus history as one bundle, so pending-resume panes can't lose history between them (PER-006). Durable 0600 writes and the symlink-aware clear already go through `publish_private_file` / `clear_path` in `src/persist/io.rs`.
- Restore should carry every saved `PaneSnapshot` field forward whether it succeeds or fails, instead of rebuilding it per branch (PER-005, PER-011).

## PER-016 - A directory-fsync failure skips the history save

- In `SessionWriter::save`, if the directory fsync fails after `session.json` has already been replaced, `save` returns early. `session-history.json` is skipped for that round and `protect_unloaded` is not cleared. Rare (needs EIO on a directory fsync).
