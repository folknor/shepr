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

- These all call `Terminal::write` (`src/ghostty/mod.rs:561`):
  - `write_host_default_color` (`src/pane/osc.rs:739-750`), which writes OSC 10/11/110/111 for the host theme;
  - `apply_host_terminal_theme`, `maybe_restore_host_terminal_theme` (detection tick), `apply_cached_host_default_color`;
  - `Terminal::mode_set` (`mod.rs:1025`);
  - the resize "recovery replay" in `GhosttyPaneTerminal::resize` (`src/pane/terminal.rs:1418-1423`).
- `Terminal::write` shares the vte parser state, the scanner state, `held_utf8` and vte's sync buffer with the child's byte stream. PTY reads are 8 KiB chunks and routinely split escape sequences.
- Triggers: `maybe_restore_host_terminal_theme` run from the detection task at any moment; `apply_host_terminal_theme` when the host theme changes; resizes.
- Effects:
  - If the child's CSI, OSC or UTF-8 sequence is incomplete, the injected ESC aborts it and the child's tail prints as literal text.
  - A held `EF` / `EF BE` gets prepended to the injected ESC, printing U+FFFD (plus a second U+FFFD for the stray continuation bytes).
  - During a child sync update (2026), the theme change is buffered until ESU or the 150 ms timeout.
- The host fg/bg and the child's OSC 10/11 share one alacritty slot (`colors[Foreground/Background]`). That forces the whole "transient owner" and re-inject machinery in `osc.rs`.
- The resize replay is a leftover from the libghostty version and should be re-checked against alacritty's reflow. It moves the emulator cursor without the child knowing.
- `restore_host_terminal_theme_reapplies_cached_colors` passes only because it writes into an idle terminal.
- Structural fix suggested: add `set_default_colors(fg, bg)` to the adapter, like `set_default_palette`; resolve child override → host default → built-in in `render_colors` / `core_query_color`, and never write bytes for host state (set colours through `Term`'s handler methods). OSC 110/111 then fall back to the host default for free, and `apply_cached_host_default_color` plus most of the owner-pgid tracking can go.

## TERM-003 - Scanner effects ignore vte's synchronized-update buffering, breaking "ordered query replies"

- **Claim:** `mod.rs:16`, ordered query replies.
- During a 2026 update, `Processor::advance` only buffers (`research/vte/src/ansi.rs:298-387`). The scanner's events are still applied immediately (`mod.rs:582-593`), so:
  - **Reply order:** XTGETTCAP, `CSI 16 t`, `?996n` and 2048 reports go out before DA/DSR/DECRQM replies that came earlier in the frame.
  - **Mode order:** RIS resets `ExtraModes` before alacritty's RIS runs. Example: `BSU ?1000h ?9h ESU` leaves X10 on and 1000 set, so `encode_mouse_event` picks PressRelease.
- Structural fix: the wrapper now exists. `CoreHandler` (`src/ghostty/handler.rs`) sits between vte and `Term`, delegates every `Handler` method, and already intercepts `push_keyboard_mode` / `pop_keyboard_modes` / `reset_state` to bound the keyboard-mode stack. Still to move into it from the scanner, so they are dispatched in byte order and sync-aware:
  - `set_private_mode` / `unset_private_mode` for `Unknown(9|1016|2031|2048)`;
  - `report_private_mode`, which replaces the string-parsing `filter_core_reply`;
  - RIS handling of `ExtraModes` in `reset_state`;
  - `set_modify_other_keys` / `report_modify_other_keys`;
  - `input(c)` for U+FF9E/U+FF9F. This removes `held_utf8`, `voiced_mark_prefix_len` and `input_halfwidth_voiced_mark`, and fixes the documented "mark folds inside sync" gap.
- After that, the scanner is only needed for OSC 7/9;9/1337, `CSI ?996n`, `CSI 16 t`, XTGETTCAP and `CSI ?3J`.
- `input_halfwidth_voiced_mark` and `apply_private_mode` still call `self.term` directly, bypassing `CoreHandler`. Harmless today, but any future direct `reset_state` or `push_keyboard_mode` on `self.term` would desync the tracked keyboard depth; moving them into the handler removes the trap.

## TERM-004 - Effects of a timed-out sync flush on non-read paths are stranded or dropped

- Render, `collect_dirty_patch` and `synchronized_output_state` call `flush_expired_synchronized_output` (`pane/terminal.rs:2178`). This runs the buffered bytes through the parser, and:
  - **Replies** sit in `Terminal.responses` until the next PTY read. A child that sent a query inside an unterminated update and is waiting for the answer hangs.
  - **Bells and OSC 52 clipboard writes** from that frame are thrown away by the discard at the start of `process_pty_bytes` (`pane/terminal.rs:1259-1263`).
  - **Resize coalescing:** `resize` drains all pending replies into the actor's resize slot, which the next resize overwrites (`pty/actor.rs:196`).

## TERM-005 - Widening a pane permanently truncates its scrollback, and the scrollback byte limit is not a maximum

Surfaced in three scopes: terminal core, CLI/config, platform.

- `scrollback_lines` divides the byte budget by the column count. `resize` shrinks history via `update_history`, which drops lines (`mod.rs:956-967`; `grid/mod.rs:154`).
  - A zoom/unzoom cycle, or attaching from a wider client (which resizes every pane), throws away history that fit the budget at the old width, and it never comes back.
  - This contradicts the comment "so a resize never truncates content it can keep".
- `MIN_SCROLLBACK_LINES` (1000) breaks the `MAX_SCROLLBACK_LINES` comment's claim that "the byte budget already bounds memory". A small budget on a wide pane gets 1000 full-width lines.
- The config doc for `advanced.scrollback_limit_bytes` says "Maximum scrollback buffer size in bytes", but `scrollback_lines` (`src/ghostty/mod.rs:418-424`) enforces the 1000-line floor, so small budgets exceed the cap.
- The `DEFAULT_CONFIG` comment "Matches Ghostty's default scrollback-limit behavior" is stale since the switch to alacritty.

## TERM-006 - `CSI 18 t` is suppressed without pixel geometry

- `filter_core_reply` (`mod.rs:810`) suppresses it, but that report is in characters and needs no pixels.
- `window_size_reports_need_pixel_geometry` enshrines the wrong behaviour.

## TERM-007 - Raw C1 `0x90` desyncs the scanner from vte

- `scan.rs:255`.
- vte only handles 7-bit controls (`research/vte/src/lib.rs:24`): it executes `0x90` as a no-op and **prints** the payload. So an 8-bit XTGETTCAP gets a reply and also leaves `+q…` text on screen.
- Any stray `0x90` (binary or Latin-1 output) puts the scanner into DCS state until the next ESC.
- Both the scan.rs module-doc claim that the scanner agrees with the core about framing and the comment "the core ignores it" are false.

## TERM-009 - Possible u16 overflow in the `CSI 14 t` reply

- `TextAreaSizeRequest` saturates the cell sizes to u16 separately, but alacritty's closure then multiplies `num_lines * cell_height` in u16 (`term/mod.rs:2261`).
- With large client-reported cell sizes this wraps in release and panics in debug, under the core lock. Unlikely at real cell sizes.

## TERM-010 - `Terminal::clear_screen` leaves the saved cursor and uses the current pen

- `mod.rs:1265`.
- It shifts rows but leaves `saved_cursor` untouched, so a later DECRC lands on a blank row.
- It refills rows with the current pen background (`grid.scroll_up`/`reset_region` use `cursor.template`).

## TERM-011 - `write_window_title`'s "safe_title" lets control characters through

- `terminal_effects.rs`: it strips only ESC, BEL and U+009C.
- CAN/SUB, CR/LF and other UTF-8-encoded C1s (e.g. U+009B, U+0090) pass through to the host. Titles can include cwd/branch text.

## TERM-013 - Per-cell allocation and whole-scrollback copies under the terminal lock

Surfaced in two scopes: terminal core, pane/terminal state.

- `ScreenTextCell.graphemes: Vec<u32>` allocates once per cell.
- Copy-mode search: `search_text_window` → `retained_text_buffer` builds the entire history as per-cell `Vec<u32>` while holding the core lock, on every request (pane/terminal.rs:396). This stalls that pane's PTY reader.
- Detection text and `ghostty_screen_row` rebuild rows every tick per pane.

## TERM-014 - Pane teardown misses the session's processes and can signal a reused pid

Surfaced in two scopes: pane/terminal state, platform.

- **Claim:** `Drop for PaneRuntime` in `src/pane.rs:868` says it will "terminate the owned session".
- **What happens:**
  - `shutdown_pane_processes` (pane.rs:924) calls `platform::session_processes(child_pid)`, which reads `/proc/<child_pid>/stat` to find the session id (`platform/linux.rs:708`, `:959`).
  - The child is reaped early by `child.wait()` in a blocking task (`pane.rs:1268`). On the common path `PaneDied` fires, the pane is removed, then the runtime is shut down, so the lookup fails and only the dead pid gets signalled. Background jobs and servers an agent started in that session survive.
  - If the pid has been reused, the lookup returns the new owner's whole session and sends it SIGHUP, then SIGTERM, then SIGKILL. The fallback `pids.push(child_pid)` also signals a dead or reused pid.
- The child is always a session leader (`setsid` at `pty/backend.rs:133`), so `sid == child_pid` by construction. Scan `/proc` for that sid directly and never read the leader's stat, or hold a pidfd.
- **Cost:** the escalation loop blocks with `thread::sleep` for up to 750 ms per pane, plus a full `/proc` scan, synchronously on the App/server loop via `app/runtime.rs:9`/`:15` and `shutdown_detached_terminal_runtimes`. Closing a workspace pays this once per pane, and every client stalls meanwhile.
- The same pid-reuse pattern exists in `unix_common.rs:400` `StatusCommandGuard::terminate`: it calls `kill(-pgid, SIGKILL)` on drop, even after tokio has reaped the leader.

## TERM-015 - Selections and copy-mode coordinates drift once scrollback is full

Surfaced in two scopes: pane/terminal state, client UI.

- **Claims:**
  - `selection.rs:12`: "keeps selection stable while the pane scrolls".
  - `client/shell/state.rs:1136`: "Ordinary selections are live buffer ranges".
- Rows are stored as screen rows where 0 is the oldest retained line (`ghostty/mod.rs:1083-1084`). When history is at its limit, every new output line evicts the oldest one; alacritty bumps `display_offset` (`research/.../grid/mod.rs:267`), so row N now names a different line.
- **Effects:**
  - A mouse selection held or dragged during output highlights and copies the wrong text. Live copies (`request_selection_copy(..., live=true)`) read different text than what was highlighted.
  - Mouse copy sends `content_revision: None`, so it is never rejected as stale.
  - `copy_mode.selection`, the copy-mode cursor and its anchor survive a revision change and drift too.
  - This hits exactly the long-running, busy agent panes the tool is for.
- Suggested fix: have the adapter expose a monotonic count of evicted lines (total lines ever scrolled plus the viewport row), so row ids are absolute and never shift.

## TERM-018 - After a session restore, the first new prompt is glued onto the last restored line

- `snapshot_history` goes through `format_range` with `trim = true` (`ghostty/format.rs:170`), so the saved ANSI has no trailing CRLF.
- `seed_history_ansi` (pane/terminal.rs:1352) writes it as-is, leaving the cursor at the end of the old prompt line. bash then prints its prompt on that same row.
- The restore test at `persist/restore.rs:881` uses the fixture `"RESTORED_HISTORY\r\n"`, so it passes for the wrong reason.

## TERM-019 - Saving while a pane is on the alternate screen persists the wrong history

- `ghostty_recent_read_range` (pane/terminal.rs:2596) reads the active grid only. alacritty has no public accessor for the inactive grid.
- `capture_pane_history` (`persist/snapshot.rs:305`) therefore saves the alt-screen frame instead of the primary scrollback, and overwrites the previously good history.

## TERM-020 - The OSC title/progress and colour trackers disagree with the real parser

Surfaced in two scopes: pane/terminal state, detection/integrations.

- vte ends an OSC on any ESC and aborts it on CAN or SUB (`research/vte/src/lib.rs:406-435`). `OscStreamCollector` (`src/pane/osc.rs:331-458`, used by `AgentOscStateTracker`) and both `DefaultColor*Tracker`s keep collecting after `ESC <non-\>`: the `BodyEscape` arm pushes ESC plus the next byte into the body and keeps collecting until BEL or ST, and ignores CAN/SUB.
  - Example: `ESC]0;foo ESC[m … ESC]0;bar BEL` produces the title "foo[m …]0;bar", and the later sequences are swallowed. Or the whole body is discarded at the 4096-byte cap.
  - `osc_title` detection rules (Claude `osc_title_working` at 1100, Codex `osc_title_blocked`/`osc_title_working`) can see a stale or corrupted title. Because this tracker also drives `terminal_title` (the "title changed" render path), the displayed title is affected too. Titles feed the sidebar and window-title tokens.
  - The test `osc_stream_collector_ignores_strings_and_preserves_escaped_bytes` locks the divergence in (it expects the body `"9;a\x1b"`).
- Every `OSC 9;<text>` is stored as "progress" (`osc.rs:501-504`). An iTerm2-style `OSC 9;message` notification overwrites real `9;4;…` progress evidence.
- The title stack (CSI 22/23 t) and RIS are ignored. After vim restores the title, shepr keeps showing vim's title.
- `DecscusrTracker` (pane/cursor.rs) also ignores RIS, so a reset cursor is still reported as an explicit shape.
- Each PTY read is scanned five times besides the vte parser: two `DefaultColor*` trackers, `AgentOscStateTracker`, `OscDebugTracker` and `DecscusrTracker`.
- Suggested fix: take the title from alacritty's `Event::Title`/`ResetTitle`, which the `Listener` currently filters out (`ghostty/mod.rs:460`), and fold the remaining scanners into `scan.rs`.

## TERM-021 - The detector reads stale history above the screen

- `ghostty_detection_text` asks for `rows` lines ending at `max(last content row, cursor row)` (pane/terminal.rs:2585-2623).
- After ED2 or Ctrl-L, or an agent redrawing from the top, alacritty's `clear_viewport` has pushed the old screen into history. The detector then gets mostly the pre-clear frame, for example a stale "proceed?" blocker.
- This breaks the AGENTS.md claim that the detector reads a screen snapshot.
- Related: `finish_recent_snapshot` reports `truncated: total_rows > lines` even when nothing was cut, because trailing blank rows are counted. The correct test is `start > 0`.

## TERM-023 - Pane hot-path costs

- **Every plain shell pane reads its whole screen twice a second.** Detection passes `current_detection_content_seq = None` when no agent is identified (pane.rs:1627), which disables the unchanged-content skip. It runs whether or not the pane is visible or idle.
- **Detection text allocates row by row.** It calls `screen_text_rows_range` once per row, twice (range search, then read).
- **A `/proc` scan runs inside the terminal lock on the PTY thread.** `current_transient_default_color_owner` → `detect::foreground_job` runs on any OSC 10/11 set, while the core lock and content lock are held.
- **One tokio task per PTY read during a synchronized update.** Each read inside the update spawns a task (pane.rs:1324).

## TERM-024 - The mouse-encoder comment is wrong about cell positions

- `encode_mouse_event` sends cell coordinates as SGR "pixels" when mode 1016 is on and the client supplied a `Cell` position (pane/terminal.rs:1806), despite the comment "cells are converted here".

## TERM-025 - Spawn and resize clamp sizes differently

- `spawn` passes unclamped rows/cols to `spawn_pty` (0 rows is possible), while `resize` clamps to 2x4 and the emulator clamps to 1 row / 2 columns.

## TERM-026 - `first_non_blank_col` miscounts combining marks

- It counts zero-width characters as one column (`copy_mode.rs:30`), unlike `last_character_col`.

## TERM-027 - Dead plumbing left from the port in the pane layer

- `response_tx` / `_response_rx` (pane.rs:1236) and the `_response_writer` parameters are unused.
- The `kitty_keyboard_flags` `AtomicU16` is never written.
- `ProcessBytesResult.terminal_bells` is never consumed.
- `CURSOR_POSITION_SETTLE_ENABLED = false` leaves all of `CursorPositionSettleState` dead.
- The `#[allow(dead_code)] // wired in Stage C` notes in osc.rs are stale.

## TERM-028 - Metadata maps can grow without bound

- `agent_metadata` and `metadata_report_sequences` are keyed by source strings that any pane process can choose (`terminal/metadata.rs`).

## TERM-029 - The dirty-row patch path can drift

- `collect_dirty_patch` sets the global dirty state to Clean but leaves rows at or below `area_height` flagged dirty, and both `render()` and patch collection consume the same `RenderState` dirty set.

## TERM-030 - A panic on a pane's reader thread kills the pane silently

- Any panic inside the terminal core runs on the PTY actor thread while it holds the core, `content_write_lock` and `response_order` mutexes. The thread dies, the master fd drops (the child gets SIGHUP) and every lock is poisoned.
- `on_reader_exit` is `None` in `src/pane.rs`, so nothing reports the dead pane. The keyboard-stack overflow in the pinned alacritty is now bounded in the adapter, but other core panics take the same path.

## TERM-031 - `resolve_shell_for_login_mode` is now used for non-login shells

- `pane_shell_command_builder` (`src/pane.rs`) resolves the configured shell through `resolve_shell_for_login_mode` in Auto/NonLogin mode too, so the name no longer describes it.
