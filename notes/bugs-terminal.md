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

## TERM-013 - Per-cell allocation and whole-scrollback copies under the terminal lock

Surfaced in two scopes: terminal core, pane/terminal state.

- `ScreenTextCell.graphemes: Vec<u32>` allocates once per cell.
- Copy-mode search: `search_text_window` → `retained_text_buffer` builds the entire history as per-cell `Vec<u32>` while holding the core lock, on every request (`pane/terminal.rs`). This stalls that pane's PTY reader.
- Detection text and `ghostty_screen_row` rebuild rows every tick per agent pane; detection text calls `screen_text_rows_range` once per row, twice (range search, then read).

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

## TERM-023 - A `/proc` scan still runs under the terminal lock

- `current_transient_default_color_owner` → `detect::foreground_job` runs under the core and content locks on the PTY thread. It now runs only when the child actually set OSC 10/11, but still under the lock. Record the need and resolve the owner after releasing the lock.

## TERM-024 - The mouse-encoder comment is wrong about cell positions

- `encode_mouse_event` sends cell coordinates as SGR "pixels" when mode 1016 is on and the client supplied a `Cell` position (`pane/terminal.rs`), despite the comment "cells are converted here".

## TERM-029 - The dirty-row patch path can drift

- `collect_dirty_patch` sets the global dirty state to Clean but leaves rows at or below `area_height` flagged dirty, and both `render()` and patch collection consume the same `RenderState` dirty set.

## TERM-035 - A panic off the reader thread while holding the core lock freezes the pane silently

- Reader-thread panics are caught and reported as `PaneDied`. A panic on another thread while it holds the terminal core lock (render, detection, API reads) still poisons the core; `process_pty_bytes` then logs "ghostty core lock poisoned in reader" on every read and the pane freezes, with nothing reporting it.
- After a reader panic, the child watcher's own `PaneDied` may follow and log "PaneDied for unknown pane"; harmless but noisy.

## TERM-036 - Colour overrides survive RIS

- alacritty keeps OSC 10/11/4 colour overrides across RIS; xterm resets them. `CoreHandler::reset_state` could clear them via `reset_default_color_overrides()` and the palette reset.

## TERM-038 - Pane teardown has no fallback without pidfds

- Session teardown signals every process through a pidfd (`ProcessHandle`, `session_member_handles` in `src/platform/linux.rs`). On a kernel without `pidfd_open`, or when opening one fails, session members are skipped with no reliable fallback, so background jobs survive teardown.
