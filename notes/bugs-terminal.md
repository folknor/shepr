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

## TERM-035 - A poisoned core is only noticed on the pane's next output

- A core poisoned off the reader thread now ends the reader loop and reports `PaneDied` on the next PTY read. An idle pane isn't noticed until it produces output, and render, detection text and API reads on a poisoned core still quietly return empty or default values.

## TERM-041 - Kitty report-all-keys sends text keys as raw text

- Under REPORT_ALL_KEYS (kitty flag 8), a key the client committed as text is still sent as raw text rather than CSI u. The removed Windows path was the only code that re-encoded it; pre-existing. A comment in `encode_terminal_key` (`src/input/encode.rs`) marks it open.
