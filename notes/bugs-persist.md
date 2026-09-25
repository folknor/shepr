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
  - The temp file name is fixed (`with_extension("json.tmp")`). With two writers, `std::fs::write` (truncate + write) can race with the other server's `rename`, which can publish a truncated or mixed file.
- **Suggested fix:** take a lock (flock) on the data directory, or derive the data directory from the socket, and use unique temp names.

## PER-002 - The main session file is written less carefully than its backups

- **Where:** `save_json_to_path`, `src/persist/io.rs:47`.
- Recovery copies get careful handling in `copy_recovery`: mode 0600 via `create_config_temporary`, `sync_all`, and a sync of the parent directory. A test even asserts 0600.
- The live `session.json` and `session-history.json` go through `std::fs::write` with no fsync of the file or directory before `rename`, and default permissions (umask, usually 0644).
- **Durability:** `Cargo.toml` says the logind inhibitor exists "so the server saves before host shutdown kills panes". That save can still be lost, or leave a zero-length file, on power loss. On the next start that file hits `parse_error`, and restore silently yields nothing.
- **Privacy:** `session-history.json` holds full pane scrollback (up to `scrollback_limit_bytes`, default 10 MB per pane, which can include tokens and secrets). It ends up group- and world-readable while the backups are 0600.
- Related: PLAT-006 notes that `sync_parent_directory(path)` syncs `path` itself, not its parent.

## PER-003 - The history save runs on the server event loop while holding every pane's terminal lock

Surfaced in three scopes: persistence, terminal core, pane/terminal state.

- **Claim:** the save is called a "background writer"; AGENTS.md says keep terminal-core locks short and treat these paths as hot.
- **Where:** `src/app/session.rs:39-59` into `PaneRuntime::snapshot_history` (`pane.rs:1987`) into `ghostty_recent_ansi_snapshot`.
- Only file IO runs on the thread. `capture_session_save_job` runs synchronously on the event loop.
- With `experimental.pane_history` on, it calls `recent_unwrapped_ansi(usize::MAX)` for every pane. Each call formats that pane's entire scrollback (up to 1M lines) as VT while holding the terminal-core mutex, which blocks the PTY reader.
- This happens on every debounced save (5 s after any dirty change) and on every pane-exit checkpoint (`save_session_now`).
- It stalls PTY readers, rendering and client fanout.
- Small related bug: on thread-spawn failure, `start_background_session_save` captures a second time (line 84).

## PER-004 - `clear()` deletes the user's stow symlink instead of the file it points to

- `save` resolves symlinks on purpose (`resolve_write_target`; the comment names stow users).
- `clear_path` (used by `SessionWriter::clear` for both files) calls `remove_file` on the link itself. The test `dangling_symlink_allows_first_save_and_late_target_is_preserved` even asserts that the target survives the clear.
- After the last workspace closes, the symlink is gone and the stale target still holds the old session. The next save writes a plain file where the link was.

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

## PER-009 - The pending-resume deadline can make the server loop spin

- If `start_pending_agent_resume` keeps returning `false` (`pane_launch_env` is `None`, or `try_send_bytes` fails), candidates stay pending.
- `pending_agent_resume_deadline` then stays in the past, `next_headless_loop_deadline_with_git_refresh` returns it, and the headless loop wakes immediately, forever.
- Related: SRV-001 (the same deadline being reset by render activity).

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

- Persistence is spread out: capture, writing, the history pairing and the resume schedule sit in separate places with no owner, no lock and no durability policy. The hunter suggests one persistence actor that:
  - owns a data-directory lock (PER-001);
  - takes cheap state snapshots on the loop and formats history off the loop, in bounded chunks under short locks (PER-003);
  - writes one atomic, fsynced, 0600 bundle (layout plus history, plus a symlink-aware clear) through the same helper the backups already use (PER-002, PER-004, PER-006).
- Restore should carry every saved `PaneSnapshot` field forward whether it succeeds or fails, instead of rebuilding it per branch (PER-005, PER-011).
