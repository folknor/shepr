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

## MUX-008 - Workspace Deref/DerefMut expect a tab

`workspace.rs`, the `Deref`/`DerefMut` impls to the active tab. That is a production panic path, against the no-`unwrap` rule; the comment admits it. The fix is the crate-wide `active_tab()` pass the comment describes, which reaches callers in shepr-server too. The cheaper route, agreed with the owner: make "a workspace always has at least one tab" hold by type (a non-empty tab collection), or show it holds by construction and document it at the `expect`.

## MUX-013 - A persist comment sits above the wrong item

`persist/snapshot.rs`: the comment explaining why pane ids are stable across saves (and restore carries history across its id remap) sits above the `layout_fingerprint` field, which it does not describe. Move it to the pairing site it explains.
