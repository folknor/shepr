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

## MUX-012 - History pairing now rests on pane ids alone

`persist/snapshot.rs`. The layout fingerprint deliberately hashes only workspace/tab structure, split layouts and sorted pane ids. If pane ids can be renumbered between two saves of the same shape, a stale history file pairs with a different pane. Check how pane ids are assigned at capture time; if they are not stable across saves, add a per-pane identity to the pairing.

## MUX-011 - PaneRuntime.cwd() prefers a stale OSC 7 report forever

`PaneRuntime.cwd()` keeps preferring the last OSC 7 report forever, even after the foreground process changes directory without emitting OSC 7.
