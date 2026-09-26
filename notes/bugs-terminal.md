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

## TERM-015 - Absolute rows exist in the terminal but nothing above it uses them

Surfaced in two scopes: pane/terminal state, client UI.

- The terminal side is done: `RowOrigin` (`src/ghostty/rows.rs`) tracks evictions exactly; `Terminal::history_origin()`, `screen_row_for_absolute`, `absolute_row_for_screen`; `ScrollPosition` / `viewport_top_row()`; `_absolute` readers on `PaneTerminal` (in an `#[allow(dead_code)]` block); `selection.rs` stores `u64` rows with `_at` methods. Column changes, alt-screen resizes, RIS and over-long writes retire old ids.
- Still to wire (then drop the `allow(dead_code)`):
  - **Server:** add `history_origin: u64` to `PaneSurfaceScrollMetrics`, filled from `scroll_position()`; re-export `ScrollPosition` from `pane.rs`.
  - **Client:** UI-020.
  - **API:** API-024.
  - **Mouse copy:** once rows are absolute, `content_revision: None` is safe because evicted rows are refused.

## TERM-024 - The mouse-encoder comment is wrong about cell positions

- `encode_mouse_event` sends cell coordinates as SGR "pixels" when mode 1016 is on and the client supplied a `Cell` position (`pane/terminal.rs`), despite the comment "cells are converted here".

## TERM-035 - A panic off the reader thread while holding the core lock freezes the pane silently

- Reader-thread panics are caught and reported as `PaneDied`. A panic on another thread while it holds the terminal core lock (render, detection, API reads) still poisons the core; `process_pty_bytes` then logs "ghostty core lock poisoned in reader" on every read and the pane freezes, with nothing reporting it.
- After a reader panic, the child watcher's own `PaneDied` may follow and log "PaneDied for unknown pane"; harmless but noisy.

## TERM-040 - The alt-screen read copies the whole primary history for nothing

- `screen_text_snapshot` (reached through `pane.rs` from the alt-screen read) copies the whole history as owned rows while on the primary screen, only for the caller to fall back. `ScreenTextCell.graphemes: Vec<u32>` is still an allocation per cell there (`history_read.rs` builds it with `vec![]`).
