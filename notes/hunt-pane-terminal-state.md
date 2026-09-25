I hunted defects in pane and terminal state: `src/pane.rs`, `src/pane/{terminal,osc,cursor,state}.rs`, `src/terminal/*`, `src/selection.rs` and `src/copy_mode.rs`. Where a value crossed into the adapter, the PTY layer, persistence or the client, I followed it. I stayed read-only and built nothing, so every item below comes from reading code plus the pinned alacritty and vte sources. None of it has been checked by running anything.

## High / medium severity

**1. Closing a pane after its shell exits leaves the rest of the session running.**
- The claim: `Drop for PaneRuntime` in `src/pane.rs:868` says it will "terminate the owned session".
- What happens: `shutdown_pane_processes` (pane.rs:924) calls `platform::session_processes(child_pid)`. That reads `/proc/<child_pid>/stat` to find the session id (`linux.rs:959`).
- On the common path the shell has already been reaped: `PaneDied` fires, the pane is removed, then the runtime is shut down. So the lookup fails, and only the dead pid gets signalled. Background jobs and servers an agent started in that session survive.
- The child is always a session leader (`setsid` at `pty/backend.rs:133`), so `sid == child_pid` by construction. Scan `/proc` for that sid directly, or better, hold a pidfd.
- Two related problems:
  - The same code signals a reused pid if one turns up.
  - The escalation loop blocks with `thread::sleep` for up to 750 ms per pane. It runs synchronously on the App/server loop via `app/runtime.rs:9` and `shutdown_detached_terminal_runtimes`.

**2. Selections and copy-mode coordinates drift once scrollback is full.**
- The claims:
  - `selection.rs:12`: "keeps selection stable while the pane scrolls".
  - `client/shell/state.rs:1136`: "Ordinary selections are live buffer ranges".
- Rows are stored as screen rows where 0 is the oldest retained line (`ghostty/mod.rs:1084`). When history is at its limit, every new output line evicts the oldest one, so row N now names a different line.
- The effects:
  - A mouse selection held or dragged during output highlights and copies the wrong text.
  - Mouse copy sends `content_revision: None`, so it is never rejected as stale.
  - `copy_mode.selection` survives a revision change and drifts too.
- Fix: have the adapter expose a monotonic count of evicted lines, so row ids are absolute and never shift.

**3. The OSC 7 cwd from standard shell integrations is always rejected.**
- `parse_file_uri_cwd` (`pane/osc.rs:639`) accepts only an empty host or `localhost`.
- bash, zsh and fish integrations (vte.sh and similar) send `file://$HOSTNAME/path`, and that is rejected.
- So the reported cwd only ever comes from hand-written `file:///…`, and "follow cwd" / workspace identity fall back to `/proc` guessing.
- The test at osc.rs:944 only covers a foreign host. Nothing tests the machine's own hostname.

**4. A dropped cwd report is never re-sent.**
- `publish_reported_cwd` (pane.rs:1055) stores the new cwd in its dedupe slot and then calls `events.try_send`.
- If the shared bounded AppEvent channel is full, the event is lost. Every later identical OSC 7 then hits `current == Some(&cwd)` and returns early.
- AppState keeps the old cwd until the directory changes again. Fix: update the dedupe state only after a successful send.

**5. After a session restore, the first new prompt is glued onto the last restored line.**
- `snapshot_history` goes through `format_range` with `trim = true` (`ghostty/format.rs:170`), so the saved ANSI has no trailing CRLF.
- `seed_history_ansi` (pane/terminal.rs:1352) writes it as-is, leaving the cursor at the end of the old prompt line. bash then prints its prompt on that same row.
- The restore test at `persist/restore.rs:881` uses the fixture `"RESTORED_HISTORY\r\n"`, so it passes for the wrong reason.

**6. Saving while a pane is on the alternate screen persists the wrong history.**
- `ghostty_recent_read_range` (pane/terminal.rs:2596) reads the active grid only. alacritty has no public accessor for the inactive grid.
- `capture_pane_history` (`persist/snapshot.rs:305`) therefore saves the alt-screen frame instead of the primary scrollback, and overwrites the previously good history.

**7. The OSC title/progress and colour trackers disagree with the real parser.**
- vte ends an OSC on any ESC, CAN or SUB (`research/vte/src/lib.rs:407-423`). `OscStreamCollector` and both `DefaultColor*Tracker`s (`pane/osc.rs`) keep collecting after `ESC <non-\>`.
  - Example: `ESC]0;foo ESC[m … ESC]0;bar BEL` produces the title "foo[m …]0;bar", and the later sequences are swallowed.
- The title stack (CSI 22/23 t) and RIS are ignored. After vim restores the title, shepr keeps showing vim's title.
- `DecscusrTracker` (pane/cursor.rs) also ignores RIS, so a reset cursor is still reported as an explicit shape.
- Titles feed the sidebar and window-title tokens, and OSC title/progress feed detection.
- Fix: take the title from alacritty's `Event::Title`/`ResetTitle`, which the `Listener` currently filters out (`ghostty/mod.rs:460`), and fold the remaining scanners into `scan.rs`. Today each PTY read is scanned five times besides the vte parser: two `DefaultColor*` trackers, `AgentOscStateTracker`, `OscDebugTracker` and `DecscusrTracker`.

**8. The detector reads stale history above the screen.**
- `ghostty_detection_text` asks for `rows` lines ending at `max(last content row, cursor row)` (pane/terminal.rs:2585-2623).
- After ED2 or Ctrl-L, or an agent redrawing from the top, alacritty's `clear_viewport` has pushed the old screen into history. The detector then gets mostly the pre-clear frame, for example a stale "proceed?" blocker.
- This breaks the AGENTS.md claim that the detector reads a screen snapshot.
- Related: `finish_recent_snapshot` reports `truncated: total_rows > lines` even when nothing was cut, because trailing blank rows are counted. The correct test is `start > 0`.

**9. Colour changes and resize replay are injected into the child's byte stream.**
- `apply_host_terminal_theme`, `maybe_restore_host_terminal_theme` (detection tick), `apply_cached_host_default_color`, and the resize "recovery replay" (pane/terminal.rs:1418) all call `terminal.write(<escape sequence>)`.
- If the child's last read ended mid-CSI, mid-OSC or mid-UTF-8, the injected ESC aborts it and the remainder renders as literal text.
- Fix: set colours through `Term`'s handler methods inside the adapter.
- The resize replay is a leftover from the libghostty version and should be re-checked against alacritty's reflow. It moves the emulator cursor without the child knowing.

**10. Non-login shells get the wrong `$SHELL`.**
- `pane_shell_command_builder` (pane.rs:1041) sets `SHELL` only in Login mode.
- In Auto/NonLogin mode the configured `default_shell` runs, but `to_std_command` exports the server's `SHELL` (`pty/command.rs:144`).
- Anything in the pane that spawns `$SHELL`, including agents' shell tools, gets a different shell than the pane.

**11. `unwrapped_text` drops spaces at wrap boundaries.**
- `terminal/history_read.rs:193` trims soft-wrapped rows, so "hello world" wrapped at the space becomes "helloworld".
- The test at history_read.rs:541 asserts this result. It also disagrees with `format.rs`, which joins wrapped rows correctly.
- Fix: don't trim soft-wrapped rows, and skip SpacerHead cells instead.

## Performance traps (hot paths)

- **Every plain shell pane reads its whole screen twice a second.** Detection passes `current_detection_content_seq = None` when no agent is identified (pane.rs:1627), which disables the unchanged-content skip. It runs whether or not the pane is visible or idle.
- **Copy-mode search copies the whole scrollback under the lock.** `search_text_window` → `retained_text_buffer` builds the entire history as per-cell `Vec<u32>` while holding the core lock, on every request (pane/terminal.rs:396). This stalls that pane's PTY reader.
- **Autosave formats the full scrollback under the lock.** `snapshot_history` does this per pane on every save.
- **Detection text allocates row by row.** It calls `screen_text_rows_range` once per row, twice (range search, then read).
- **A `/proc` scan runs inside the terminal lock on the PTY thread.** `current_transient_default_color_owner` → `detect::foreground_job` runs on any OSC 10/11 set, while the core lock and content lock are held.
- **One tokio task per PTY read during a synchronized update.** Each read inside the update spawns a task (pane.rs:1324).

## Low severity / dead code left from the port

- **The mouse-encoder comment is wrong about cell positions.** `encode_mouse_event` sends cell coordinates as SGR "pixels" when mode 1016 is on and the client supplied a `Cell` position (pane/terminal.rs:1806), despite the comment "cells are converted here".
- **Spawn and resize clamp sizes differently.** `spawn` passes unclamped rows/cols to `spawn_pty` (0 rows is possible), while `resize` clamps to 2x4 and the emulator clamps to 1 row / 2 columns.
- **`first_non_blank_col` miscounts combining marks.** It counts zero-width characters as one column (`copy_mode.rs:30`), unlike `last_character_col`.
- **`TerminalState` ignores the screen signals it is handed.** `set_detected_state_with_screen_signals_at` takes `_visible_idle`/`_visible_working` and ignores them, although the detector computes them and ships them through `AppEvent::StateChanged`.
- **Dead plumbing:**
  - `response_tx` / `_response_rx` (pane.rs:1236) and the `_response_writer` parameters are unused.
  - The `kitty_keyboard_flags` `AtomicU16` is never written.
  - `ProcessBytesResult.terminal_bells` is never consumed.
  - `CURSOR_POSITION_SETTLE_ENABLED = false` leaves all of `CursorPositionSettleState` dead.
  - `stabilize_agent_detection` is an identity function.
  - The `#[allow(dead_code)] // wired in Stage C` notes in osc.rs are stale.
- **Metadata maps can grow without bound.** `agent_metadata` and `metadata_report_sequences` are keyed by source strings that any pane process can choose (`terminal/metadata.rs`).
- **The dirty-row patch path can drift.** `collect_dirty_patch` sets the global dirty state to Clean but leaves rows at or below `area_height` flagged dirty, and both `render()` and patch collection consume the same `RenderState` dirty set.
