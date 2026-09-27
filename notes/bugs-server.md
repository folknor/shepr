# Defects: shepr-server

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

Hunter coverage: `src/app/` limited to `mod.rs`, `session.rs`, `runtime.rs`, `creation.rs`, `api.rs`, `api_helpers.rs`, `api/session.rs` and the first part of `api/layouts.rs`. Not read: the rest of `src/app/`, `clients.rs`, `pane_input.rs`, `alt_screen_read.rs`, `render_stream.rs`, `retained_surface.rs` and `bootstrap.rs`.

## SRV-001 - Every full render rebuilds the whole session snapshot, with /proc reads per pane

Hot path; breaks "Hot paths multiply".
- `render_and_stream` (`server/headless/render.rs`) calls `shell_session_snapshot`, which calls `App::session_snapshot()` (`app/api/session.rs`).
- That runs `pane_info` for every pane in every workspace. Its own comment (`app/creation.rs`) says it does "a few /proc reads" for `foreground_cwd`, plus `cwd_for_pane`.
- It also builds `pane_layout_snapshot` for every tab. `shell_session_snapshot` then throws the layouts away (`snapshot.layouts = Vec::new()`). Its doc says they are "dropped before the snapshot is copied", but they are still computed every time.
- `snapshot_from_session` (`server/client_shell.rs`) then, per shell client:
  - resolves `resolved_new_workspace_cwd_from_tab` for every workspace (more /proc cwd lookups);
  - clones the snapshot and compares it field by field with the last one sent.
- This runs on every `RenderDemand::Full`: any internal event, API request, server event or agent state change, capped at 60 Hz.
- Fix: make the shell projection event-driven. Bump a revision when topology, labels, agent state or metadata change, and rebuild only then. The config is encoded once at startup, but those bytes still ride in every `ClientShellSnapshot`: a copy per render per shell client, plus a byte comparison on the server and another on the client. Send them once per connection (or as an `Arc<[u8]>`, which needs serde's `rc` feature).

## SRV-018 - A pending-agent-resume test is order dependent

`headless_scheduled_tasks_start_pending_agent_resume_without_foreground_client` failed once in a full shepr-server test run and passed on rerun and alone. It looks timing or order dependent; find what it waits on and make the wait deterministic.

## SRV-013 - ApiDispatcher swap dance

The `ApiDispatcher` swap dance (`with_api_dispatcher` taking it out, then `with_server_dispatcher` swapping it back in during dispatch) works, but it is hard to follow. `HeadlessServer` and `ApiDispatcher` are really one owner, and splitting routing state out as a plain struct passed by `&mut` would remove the swaps.
