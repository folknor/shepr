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

## SRV-013 - ApiDispatcher swap dance

The `ApiDispatcher` swap dance (`with_api_dispatcher` taking it out, then `with_server_dispatcher` swapping it back in during dispatch) works, but it is hard to follow. `HeadlessServer` and `ApiDispatcher` are really one owner, and splitting routing state out as a plain struct passed by `&mut` would remove the swaps.

## SRV-014 - Pane-exit checkpoints save the session inline on the event loop

`save_session_now()` (`app/session.rs`), reached from pane-exit checkpoints in `app/events.rs`, joins any in-flight writer and does the filesystem work inline on the tokio loop. Ordinary saves now run on the writer thread and retry on failure, but this path still pauses every client for the length of a disk write, including history formatting.

## SRV-015 - A failing save during a host-shutdown warning holds the logind delay lock

When logind warns of a host shutdown, the server checkpoints and then releases the delay lock. If the save keeps failing, `freeze_for_host_shutdown` returns early and the lock is never released, so host shutdown waits until logind's `InhibitDelayMaxSec` runs out. Probably acceptable, but the lock should be released after a bounded number of failed attempts.

## SRV-016 - An attach client's pane keeps its size after it leaves with no shell connected

When a terminal-attach client disconnects and no shell client is connected, the headless resize is skipped (it now runs only when the departing client was an active shell, so that attach departures do not start pending agent resumes). The attached pane keeps the departed client's size until a shell connects. Resizing to headless geometry should be split from the agent-resume side effect so both departures restore the size.

## SRV-017 - ClientRegistry::is_empty is dead outside tests

`crates/shepr-server/src/server/clients.rs`, `ClientRegistry::is_empty`, is used only by tests. `brokkr clippy --lib` flags it as dead code; the all-targets run hides it. Gate it to tests or remove it.
