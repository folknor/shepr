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
- That runs `pane_info` for every pane in every workspace. Its own comment (`app/creation.rs:312-318`) says it does "a few /proc reads" for `foreground_cwd`, plus `cwd_for_pane`.
- It also builds `pane_layout_snapshot` for every tab. `shell_session_snapshot` then throws the layouts away (`snapshot.layouts = Vec::new()`). Its doc says they are "dropped before the snapshot is copied", but they are still computed every time.
- `snapshot_from_session` (`server/client_shell.rs`) then, per shell client:
  - resolves `resolved_new_workspace_cwd_from_tab` for every workspace (more /proc cwd lookups);
  - re-encodes the whole `ValidatedConfig` with the codec (`client_shell.rs:234-236`);
  - clones the snapshot and compares it field by field with the last one sent.
- This runs on every `RenderDemand::Full`: any internal event, API request, server event or agent state change, capped at 60 Hz.
- Fix: make the shell projection event-driven. Bump a revision when topology, labels, agent state or metadata change, and rebuild only then. Keep the config bytes, which never change after launch, as encoded once.

## SRV-002 - expect() in production on the render path

- `client_shell.rs:236`: `.expect("validated configuration must encode for the client protocol")` breaks "No `unwrap()` in production code".
- If encoding ever fails, the server panics mid-render for all clients. Encoding once at startup (see SRV-001) turns this into a launch failure, which matches "any config problem fails the launch".

Related: CFG-005.

## SRV-004 - The exhaustive internal-event drain before each API request can starve it

- `handle_api_request_with_shutdown_check_inner` calls `drain_all_internal_events_with_forwarding` (`headless/internal_events.rs:93-104`). That loops until a batch finds no event.
- PTY reader and detector threads keep refilling `event_rx` (capacity 256). Under sustained output (StateChanged, title and hook events) the loop can keep going, and the API request, plus the whole event loop, stalls for as long as producers keep up.
- Bound the drain to a snapshot of the queue length on entry.

Related: API-001 (a request that times out still runs later).

## SRV-005 - A failed session save is never retried

- `start_background_session_save` / `save_session_now` (`app/session.rs`) clear `session_save_deadline` before the job runs. `SessionWriter::finish_save` (`shepr-mux/src/persist/writer.rs`) only logs errors.
- After a transient failure (ENOSPC, EIO) the on-disk session stays stale until the next mutation or shutdown. Layout changes can be lost across a crash, against the session-restore claim.
- The writer should report failure back so the saver can go back to `retry()`, which already exists for the thread-busy case.

## SRV-006 - render_pane_surface takes &mut App but only reads

`client_shell.rs:266`. The signature forces `render_and_stream` to hold `&mut self.app` per client and hides that rendering is pure (the "Render is pure" principle). Make it `&App`.

## SRV-007 - Blocking sleeps on the async runtime during shutdown

- `initiate_shutdown` and `complete_shutdown` (`headless/lifecycle.rs:271, 299`) call `std::thread::sleep(50ms)` inside the tokio loop. It's also a guess at flush timing, not a guarantee.
- The writer threads should report the flush, or the shutdown should await it.

## SRV-008 - Stale AGENTS.md UI description and duplicated compute_view wrappers

- AGENTS.md describes `compute_view()` and `render()` in `ui.rs`. Neither exists any more.
- `compute_view_with_runtime_registry` and `compute_view_without_resizing_panes` are identical wrappers around `compute_view_internal`. The first one's doc says "without resizing", so the names suggest a difference that isn't there.
- Collapse them into one and fix the AGENTS.md wording.

## SRV-009 - Panes may keep the last client's size after it disconnects (unconfirmed)

- The `effective_size` doc (`headless.rs:181-183`) says the pane runtime size falls back to the configured headless size when no clients are connected.
- When the last client leaves, `sync_foreground_client_state` only recomputes the view (`compute_view_without_resizing_panes`). `render_and_stream` with no targets resizes panes only when `view.pane_infos.is_empty()` (`render.rs:370`), which is false after a session has had a client.
- So panes appear to keep the departed client's geometry. The hunter did not read `resize_tabs_for_only_shell_client`; check it before acting.

## SRV-010 - API handlers still name a tab by position when the stable-number lookup fails

`workspace_info` no longer invents a tab id, but the same `position + 1` fallback remains in `app/api/tabs.rs` (~111), `app/api/agents.rs` (~213) and `app/api/panes.rs` (~314). Tabs use stable public numbers and a bare position is rejected elsewhere, so if the lookup ever fails these can report a different tab's id. They should report no id or an error.

Related: MUX-009.

## SRV-011 - ServerAgentManifests is a status read that mutates state

`ServerAgentManifests` is a status read but mutates state (`refresh_agent_manifest_summaries`).

## SRV-013 - ApiDispatcher swap dance

The `ApiDispatcher` swap dance (`with_api_dispatcher` taking it out, then `with_server_dispatcher` swapping it back in during dispatch) works, but it is hard to follow. `HeadlessServer` and `ApiDispatcher` are really one owner, and splitting routing state out as a plain struct passed by `&mut` would remove the swaps.
