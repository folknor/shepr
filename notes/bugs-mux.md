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

The pane runtime reap finding from the pty hunter (TRM-010) sits in bugs-terminal.md though the code is in this crate.

## MUX-008 - Workspace Deref/DerefMut expect a tab

`workspace.rs:166-180`. That is a production panic path, against the no-`unwrap` rule; the comment admits it. The fix is the crate-wide `active_tab()` pass the comment describes.

## MUX-012 - History pairing now rests on pane ids alone

`persist/snapshot.rs`. The layout fingerprint deliberately hashes only workspace/tab structure, split layouts and sorted pane ids. If pane ids can be renumbered between two saves of the same shape, a stale history file pairs with a different pane. Check how pane ids are assigned at capture time; if they are not stable across saves, add a per-pane identity to the pairing.
