# Defects: shepr-mux

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

Hunter coverage: `git/status.rs`, `pane/runtime.rs` (first 1550 lines), `pane/agent_detection.rs`, `pane/teardown.rs`, all of `persist/` except the tests at the end of `restore.rs`, and the first 400 lines of `workspace.rs`. Not read: `events.rs`, `render_signal.rs`, `terminal/*`, `git/discovery.rs`, `git/config.rs`, the `pane/` files `state.rs`, `launch.rs`, `process_probe.rs`, `osc.rs`, `cwd.rs` and `cursor.rs`, and `workspace/tab.rs`, `workspace/aggregate.rs`, `workspace/geometry.rs`.

The pane runtime reap/spawn findings from the pty hunter (TRM-010, TRM-011, TRM-012) sit in bugs-terminal.md though the code is in this crate.

## MUX-001 - One pane with a non-UTF-8 cwd stops the whole session from saving

`persist/snapshot.rs`, `persist/io.rs:141`.
- `PaneSnapshot.cwd` and `WorkspaceSnapshot.identity_cwd` are `PathBuf`s encoded with `serde_json`, which refuses non-UTF-8 paths.
- `save_json_to_path` → `to_string_pretty` then fails, so every autosave and the shutdown save fail. `layout_fingerprint` also returns `None`, which means restore always throws the history away.
- This breaks the session-restore claim. The Git layer in the same crate explicitly supports non-UTF-8 checkout paths (test `cache_key_preserves_non_utf8_checkout_path`), and `capture_tab` takes the cwd straight from `/proc`.
- Fix: encode paths as bytes (for example with base64 or an escaped form), or at least skip or replace an unencodable pane instead of failing the file.

## MUX-002 - layout_fingerprint hashes the whole snapshot, not the layout

`persist/snapshot.rs:304`.
- It hashes `host_theme`, every pane's cwd, labels, agent names, `active` and `selected`.
- The writer's snapshot rotation (`writer.rs:179-188`, test named `..._does_not_rotate_identical_layouts`) therefore treats any `cd` or theme change as a new layout.
- History pairing only works because history is always written after the layout in the same save.
- Its comment says converting through `serde_json::Value` "sorts" the `HashMap<u32, …>` keys. That only holds while no crate in the build turns on serde_json's `preserve_order` feature. If one does (`jsonc-parser`'s `serde_json` feature is worth checking), the fingerprint made at save time and the one recomputed at restore come from different HashMap orders, and saved history is silently dropped on every restore. It should fingerprint a canonical projection built on purpose, with sorted keys and layout-only fields.

## MUX-003 - The "one publisher" guarantee for reported cwd is false

`pane/runtime.rs:235-238` vs `577-579`.
- `publish_reported_cwd` says "Only the PTY reader thread publishes for a pane, so check-then-store does not race."
- The synchronized-output timeout flush also calls it, from a `spawn_blocking` thread. It can race the reader, so the dedupe slot can end up holding a cwd that `AppState` saw out of order, or one that was sent twice.
- The flush path also ignores `result.core_poisoned`, which the reader path checks.

Related smell from the same hunter: `SyncTimeoutRender` plus the timer path copies about ten `Arc`s per armed timeout and duplicates the reader's post-processing: cwd, clipboard, title, render. Extract one shared "apply `ProcessResult`" function so the two paths cannot drift, as they already have with `core_poisoned` and the detection bump.

## MUX-004 - PaneRuntime::shutdown() does nothing different from dropping

`pane/runtime.rs:257, 941`.
- `preserve_processes_on_drop` is `false` in every production constructor. Only the test constructor sets it `true`.
- So `shutdown()` (which sets it `false`) is the same as a plain drop, and the "process-session policy" it claims to pick does not exist in production. The flag only exists to keep tests from killing pid 0.
- Fix: delete the flag and `shutdown()`, or make the test IO variant own the policy.

## MUX-005 - Restored panes start with the default theme

`persist/restore.rs:550-564`.
- They are spawned with `TerminalTheme::default()` and appearance `None`.
- `SessionSnapshot.host_theme` is documented as "retained for headless resumes" (`SavedHostTheme::to_theme`), but `restore()` never reads it.
- Unless the server re-applies it afterwards (the hunter did not check `shepr-server`), restored panes answer OSC 10/11/4 colour queries with defaults.

## MUX-007 - Ahead/behind never retries after a failed git rev-list

`git/status.rs`. Filed by the hunter as a weaker finding to check before acting.
- If `rev-list` fails (for example the upstream object is not fetched yet), `None` is cached under a valid fingerprint and is not retried until HEAD or upstream changes.
- Separately, when `fingerprint()` returns `None` (an empty or unborn-invalid HEAD), no cache entry is returned, so every refresh redoes full repo discovery.

## MUX-008 - Workspace Deref/DerefMut expect a tab

`workspace.rs:166-180`. That is a production panic path, against the no-`unwrap` rule; the comment admits it. The fix is the crate-wide `active_tab()` pass the comment describes.

## MUX-009 - tab_display_name shows position, not the public tab number

`tab_display_name` shows `tab_idx + 1`, not the tab's persisted public `number`. After closing a tab, the displayed name and the public ID `t<number>` differ.

Related: SRV-010 (`workspace_info` fallback builds `active_tab_id` from a position).

## MUX-010 - Session writer persistence smells

- `SessionWriter::save` calls `preserve_snapshot_history` twice, before and after the write (`writer.rs:63, 95`). Each call re-reads and re-parses the session and the latest snapshot. Only the 15-minute gate prevents a double rotation.
- `save_serialized_to_path` syncs only the immediate parent when `create_dir_all` created several directory levels, so the durability claim for a fresh data directory is partial.

Related: SRV-005 (failed saves are never retried).

## MUX-011 - PaneRuntime.cwd() prefers a stale OSC 7 report forever

`PaneRuntime.cwd()` keeps preferring the last OSC 7 report forever, even after the foreground process changes directory without emitting OSC 7.
