# Terminal core and pane state defects

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

## TERM-002 - Host-side state is injected through the child's parser mid-stream

Surfaced in two scopes: terminal core, pane/terminal state.

- These all call `Terminal::write` (`src/ghostty/mod.rs`):
  - `write_host_default_color` (`src/pane/osc.rs`), which writes OSC 10/11/110/111 for the host theme;
  - `apply_host_terminal_theme`, `maybe_restore_host_terminal_theme` (detection tick), `apply_cached_host_default_color`;
  - `Terminal::mode_set`;
  - the resize "recovery replay" in `GhosttyPaneTerminal::resize` (`src/pane/terminal.rs`).
- `Terminal::write` shares the vte parser state, the scanner state and vte's sync buffer with the child's byte stream. PTY reads are 8 KiB chunks and routinely split escape sequences.
- Triggers: `maybe_restore_host_terminal_theme` run from the detection task at any moment; `apply_host_terminal_theme` when the host theme changes; resizes.
- Effects:
  - If the child's CSI, OSC or UTF-8 sequence is incomplete, the injected ESC aborts it and the child's tail prints as literal text.
  - During a child sync update (2026), the theme change is buffered until ESU or the 150 ms timeout.
- The host fg/bg and the child's OSC 10/11 share one alacritty slot (`colors[Foreground/Background]`). That forces the whole "transient owner" and re-inject machinery in `osc.rs`.
- The resize replay is a leftover from the libghostty version and should be re-checked against alacritty's reflow. It moves the emulator cursor without the child knowing. It now reads through `ghostty_bottom_rows_text`.
- `restore_host_terminal_theme_reapplies_cached_colors` passes only because it writes into an idle terminal.
- Structural fix suggested: add `set_default_colors(fg, bg)` to the adapter, like `set_default_palette`; resolve child override → host default → built-in in `render_colors` / `core_query_color`, and never write bytes for host state (set colours through `CoreHandler` / `Term`'s handler methods). OSC 110/111 then fall back to the host default for free, and `apply_cached_host_default_color` plus most of the owner-pgid tracking can go.

## TERM-004 - Effects of a timed-out sync flush on non-read paths are stranded or dropped

- Render, `collect_dirty_patch` and `synchronized_output_state` call `flush_expired_synchronized_output` (`pane/terminal.rs`). This runs the buffered bytes through the parser, and:
  - **Replies** sit in `Terminal.responses` until the next PTY read. A child that sent a query inside an unterminated update and is waiting for the answer hangs.
  - **OSC 52 clipboard writes** from that frame are thrown away by the discard at the start of `process_pty_bytes`.
  - **Resize coalescing:** `resize` drains all pending replies into the actor's resize slot, which the next resize overwrites (`pty/actor.rs`).

## TERM-013 - Per-cell allocation and whole-scrollback copies under the terminal lock

Surfaced in two scopes: terminal core, pane/terminal state.

- `ScreenTextCell.graphemes: Vec<u32>` allocates once per cell.
- Copy-mode search: `search_text_window` → `retained_text_buffer` builds the entire history as per-cell `Vec<u32>` while holding the core lock, on every request (`pane/terminal.rs`). This stalls that pane's PTY reader.
- Detection text and `ghostty_screen_row` rebuild rows every tick per pane.

## TERM-014 - Pane teardown misses the session's processes and can signal a reused pid

Surfaced in two scopes: pane/terminal state, platform.

- **Claim:** `Drop for PaneRuntime` in `src/pane.rs` says it will "terminate the owned session".
- **What happens:**
  - `shutdown_pane_processes` (`pane.rs`) calls `platform::session_processes(child_pid)`, which reads `/proc/<child_pid>/stat` to find the session id (`platform/linux.rs`).
  - The child is reaped early by `child.wait()` in a blocking task (`pane.rs`). On the common path `PaneDied` fires, the pane is removed, then the runtime is shut down, so the lookup fails and only the dead pid gets signalled. Background jobs and servers an agent started in that session survive.
  - If the pid has been reused, the lookup returns the new owner's whole session and sends it SIGHUP, then SIGTERM, then SIGKILL. The fallback `pids.push(child_pid)` also signals a dead or reused pid.
- The child is always a session leader (`setsid` in `pty/backend.rs`), so `sid == child_pid` by construction. Scan `/proc` for that sid directly and never read the leader's stat, or hold a pidfd.
- **Cost:** the escalation loop blocks with `thread::sleep` for up to 750 ms per pane, plus a full `/proc` scan, synchronously on the App/server loop via `app/runtime.rs` and `shutdown_detached_terminal_runtimes`. Closing a workspace pays this once per pane, and every client stalls meanwhile.
- The same pid-reuse pattern exists in `unix_common.rs` `StatusCommandGuard::terminate`: it calls `kill(-pgid, SIGKILL)` on drop, even after tokio has reaped the leader.

## TERM-015 - Selections and copy-mode coordinates drift once scrollback is full

Surfaced in two scopes: pane/terminal state, client UI.

- **Claims:**
  - `selection.rs`: "keeps selection stable while the pane scrolls".
  - `client/shell/state.rs`: "Ordinary selections are live buffer ranges".
- Rows are stored as screen rows where 0 is the oldest retained line (`ghostty/mod.rs`). When history is at its limit, every new output line evicts the oldest one; alacritty bumps `display_offset`, so row N now names a different line.
- **Effects:**
  - A mouse selection held or dragged during output highlights and copies the wrong text. Live copies (`request_selection_copy(..., live=true)`) read different text than what was highlighted.
  - Mouse copy sends `content_revision: None`, so it is never rejected as stale.
  - `copy_mode.selection`, the copy-mode cursor and its anchor survive a revision change and drift too.
  - This hits exactly the long-running, busy agent panes the tool is for.
- Suggested fix: have the adapter expose a monotonic count of evicted lines (total lines ever scrolled plus the viewport row), so row ids are absolute and never shift.

## TERM-019 - Saving while a pane is on the alternate screen persists the wrong history

- `ghostty_recent_read_range` (`pane/terminal.rs`) reads the active grid only. alacritty has no public accessor for the inactive grid.
- `capture_pane_history` (`persist/snapshot.rs`) therefore saves the alt-screen frame instead of the primary scrollback, and overwrites the previously good history.

## TERM-020 - The OSC title/progress and colour trackers disagree with the real parser

Surfaced in two scopes: pane/terminal state, detection/integrations.

- vte ends an OSC on any ESC and aborts it on CAN or SUB (`research/vte/src/lib.rs`). `OscStreamCollector` (`src/pane/osc.rs`, used by `AgentOscStateTracker`) and both `DefaultColor*Tracker`s keep collecting after `ESC <non-\>`: the `BodyEscape` arm pushes ESC plus the next byte into the body and keeps collecting until BEL or ST, and ignores CAN/SUB.
  - Example: `ESC]0;foo ESC[m … ESC]0;bar BEL` produces the title "foo[m …]0;bar", and the later sequences are swallowed. Or the whole body is discarded at the 4096-byte cap.
  - `osc_title` detection rules (Claude `osc_title_working`, Codex `osc_title_blocked`/`osc_title_working`) can see a stale or corrupted title. Because this tracker also drives `terminal_title` (the "title changed" render path), the displayed title is affected too. Titles feed the sidebar and window-title tokens.
  - The test `osc_stream_collector_ignores_strings_and_preserves_escaped_bytes` locks the divergence in (it expects the body `"9;a\x1b"`).
- Every `OSC 9;<text>` is stored as "progress" (`osc.rs`). An iTerm2-style `OSC 9;message` notification overwrites real `9;4;…` progress evidence.
- The title stack (CSI 22/23 t) and RIS are ignored. After vim restores the title, shepr keeps showing vim's title.
- `DecscusrTracker` (`pane/cursor.rs`) also ignores RIS, so a reset cursor is still reported as an explicit shape.
- Each PTY read is scanned five times besides the vte parser: two `DefaultColor*` trackers, `AgentOscStateTracker`, `OscDebugTracker` and `DecscusrTracker`.
- Suggested fix: take the title from alacritty's `Event::Title`/`ResetTitle`, which the `Listener` currently filters out (`ghostty/mod.rs`), and fold the remaining scanners into `CoreHandler` / `scan.rs`.

## TERM-023 - Pane hot-path costs

- **Every plain shell pane reads its whole screen twice a second.** Detection passes `current_detection_content_seq = None` when no agent is identified (`pane.rs`), which disables the unchanged-content skip. It runs whether or not the pane is visible or idle.
- **Detection text allocates row by row.** It calls `screen_text_rows_range` once per row, twice (range search, then read).
- **A `/proc` scan runs inside the terminal lock on the PTY thread.** `current_transient_default_color_owner` → `detect::foreground_job` runs on any OSC 10/11 set, while the core lock and content lock are held.
- **One tokio task per PTY read during a synchronized update.** Each read inside the update spawns a task (`pane.rs`).

## TERM-024 - The mouse-encoder comment is wrong about cell positions

- `encode_mouse_event` sends cell coordinates as SGR "pixels" when mode 1016 is on and the client supplied a `Cell` position (`pane/terminal.rs`), despite the comment "cells are converted here".

## TERM-028 - Metadata maps can grow without bound

- `agent_metadata` and `metadata_report_sequences` are keyed by source strings that any pane process can choose (`terminal/metadata.rs`).

## TERM-029 - The dirty-row patch path can drift

- `collect_dirty_patch` sets the global dirty state to Clean but leaves rows at or below `area_height` flagged dirty, and both `render()` and patch collection consume the same `RenderState` dirty set.

## TERM-031 - `resolve_shell_for_login_mode` is now used for non-login shells

- `pane_shell_command_builder` (`src/pane.rs`) resolves the configured shell through `resolve_shell_for_login_mode` in Auto/NonLogin mode too, so the name no longer describes it.

## TERM-032 - `DECRQM ?2026` reports reset during an active sync update

- The pinned alacritty answers `DECRQM ?2026` as always reset, even while a synchronized update is active; `mode_get(2026)` disagrees with it. `CoreHandler::report_private_mode` could answer it from the sync state.

## TERM-033 - Unsetting 1000/1002/1003 does not clear X10 mode

- `CoreHandler` makes setting 1000/1002/1003 cancel mode 9, but unsetting them leaves X10 on. xterm keeps one shared mouse-mode variable, so unsetting any of them clears X10 too. Pre-existing behaviour, kept as-is.

## TERM-034 - The adapter's bell counter has no consumer

- `bell_count` / `take_bell_count` in `src/ghostty/mod.rs` are drained each PTY read but nothing uses the count since `ProcessBytesResult.terminal_bells` was removed. Remove it or wire bells to something.

## TERM-035 - A panic off the reader thread while holding the core lock freezes the pane silently

- Reader-thread panics are now caught and reported as `PaneDied`. A panic on another thread while it holds the terminal core lock (render, detection, API reads) still poisons the core; `process_pty_bytes` then logs "ghostty core lock poisoned in reader" on every read and the pane freezes, with nothing reporting it.
- After a reader panic, the child watcher's own `PaneDied` may follow and log "PaneDied for unknown pane"; harmless but noisy.
