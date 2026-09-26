Terminal-core structural review. I made no edits and ran no shell commands. I read the files below in full. I read only parts of `src/terminal/runtime.rs`, `src/ghostty/scan.rs` and `src/pty/actor.rs` (the tests past about line 1540). I did not open `src/ghostty/format.rs`, `pty/backend.rs`, `pty/fd.rs`, `terminal/state.rs`, `terminal/history_read.rs` or `terminal/runtime_registry.rs`.

## 1. Axes that should be types

- **Row spaces (the most important).** Rows use three coordinate systems: viewport rows, screen rows (0 is the oldest retained line) and absolute rows (`history_origin` plus a screen row). All three travel as bare `u16`, `u32`, `u64` or `usize`.
  - `Selection` (`src/selection.rs`) stores `anchor` and `cursor` as `(u64, u16)`. The module doc says "a selection must be built and read in one row space throughout", but nothing enforces it. Both constructor families (`anchor` vs `anchor_at`, `drag` vs `drag_at`, `contains` vs `contains_at`) write into the same fields.
  - `absolute_row_for_viewport` returns a screen row. Its own comment says "Despite the name, not an absolute row".
  - `ordered_cells` quietly saturates absolute rows down to `u32`.
  - In `Terminal` (`src/ghostty/mod.rs`), `screen_line(u64)`, `viewport_line(u64)`, `screen_cell(x, y: u32)`, `screen_row_for_absolute(u64) -> usize` and `read_*_{screen,viewport}(start: (u16, u32), ...)` all take bare numbers. The only thing marking the space is a `Coordinates` enum chosen by the method name.
  - Fix: add `ViewportRow`, `ScreenRow` and `AbsRow` newtypes plus a `Point<R>`. Make `Selection<R>` generic over the row space, or allow only `AbsRow`, and delete the `ScrollMetrics` family.
- **Cell pixel geometry.** `(cell_width_px, cell_height_px)` is a bare `u32` pair in `Terminal`, `CoreHandler`, `Terminal::resize`, `PtyIoActorHandle::resize`, `PtyResize` and `TerminalRuntime::resize`. `HostCellSize` in `terminal_cell_size.rs` already exists but is not used for any of these. The resize signature `(rows: u16, cols: u16, w: u32, h: u32)` also appears as `(cols, rows, ...)` in `Terminal::resize` (the order flips across the layer). Fix: one `PaneGeometry { cols, rows, cell: Option<CellPx> }`.
- **DEC private modes.** Modes are a bare `u16` (`mode_get(u16)`, `mode_set(u16)`, the `MODE_*` constants), and `PrivateMode::Unknown(9 | 1016 | 2031 | 2048)` literals are matched in several places. Fix: an enum with a total mapping.
- **Keyboard protocol levels.** The modifyOtherKeys level is a bare `u8` (0, 1 or 2) in `ExtraModes`, `ScanEvent::ModifyOtherKeys` and `set_direct_host_keyboard_protocol`. Kitty flags are `u8` in `Terminal::kitty_keyboard_flags` but `u16` in `DirectHostKeyboardState`. `terminal_modes.rs` also ORs in `0b0001_0000` by hand because crossterm has no associated-text flag.
- **Cell style.** `CellStyle.underline: u8` ("0 none, 1 single...") sits beside a redundant `underlined: bool`, and `blink` and `overline` are always false. Use an `UnderlineStyle` enum and drop the dead fields.
- **Payloads that are really domain values.**
  - OSC 7 working directory and OSC 9;4 progress come out as `Vec<u8>`.
  - The title update is `Option<Option<String>>`.
  - `clear_screen() -> bool` means "refused because the alternate screen is active".
  - `TerminalRuntime::clear_screen` returns `Result<(), String>`.
  - Each deserves a typed value or outcome.
- **Errors are prose.**
  - `ghostty::Error(&'static str)` can't be branched on.
  - Most `Result<_, Error>` returns (`cols`, `rows`, `scrollbar`, `default_palette`, `kitty_keyboard_flags`, `RenderState::new`, `RowIterator::new`, and others) can never fail. They are left over from libghostty.
  - PTY submission outcomes reach callers as an `io::Error` whose `ErrorKind` is overloaded: `TimedOut` means "withdrawn by caller", and `BrokenPipe` means "actor closed" (in `actor.rs`). A typed `SubmissionError` is needed.
- **The PTY actor's `pane_id: u32`** should be `PaneId`.

## 2. Decisions made in more than one place

1. **Is pixel geometry known?** Four sites answer this: `HostCellSize::is_known`, `Terminal::has_pixel_geometry`, `handler::in_band_size_report` and `handler::text_area_pixels_report`, each with its own `w > 0 && h > 0`. They agree today. The owner should be a `CellPx` type that can only be built non-zero.
2. **Colour-scheme report bytes and the appearance type.** `ghostty::ColorScheme::report` and `terminal_theme::HostAppearance::color_scheme_report` both hard-code `\x1b[?997;1n` / `\x1b[?997;2n`, over two identical Light/Dark enums. The same split applies to `ghostty::RgbColor` vs `terminal_theme::RgbColor` (two structs, with conversions somewhere in the pane layer) and to `ghostty::DefaultColor` vs `terminal_theme::DefaultColorKind`. One owner should hold all of them, most naturally the vt module.
3. **What is a cell's text?** Three sites answer it: `cell_graphemes`, `cell_text_into` and `RowCellIter::grapheme_text_into`. **They already disagree:** `cell_text_into` blanks `KITTY_UNICODE_PLACEHOLDER` cells, while `cell_graphemes` (used by `screen_cell` and `screen_text_rows`) and `grapheme_text_into` (the render path) do not. So the placeholder leaks through the history-read and render paths even though the constant's comment says it is filtered. Make one `cell_text(cell) -> CellText` classifier.
4. **Row wrap flags.** `visit_screen_row_text` and `screen_text_rows_range` each compute `soft_wrapped` and `wrap_continuation`, and `ScreenTextRow` re-declares the fields instead of embedding `RowWrap`.
5. **Mode numbers.** At least four sites each carry the number mapping:
   - `mode_get` (numbers mapped to `TermMode` bits),
   - `handler::private_mode` (numbers mapped to vte names; they disagree on 3, 12 and 1042, which `mode_get` answers false),
   - `CoreHandler::adapter_private_mode`, plus the set and unset arms,
   - `terminal_modes::DISABLE_HOST_MOUSE_REPORTING_SEQUENCE`, which includes 1015, a mode the core doesn't model at all.
   One mode table should own number, name, getter and setter.
6. **The X10 mouse exclusivity rule.** It is written twice: once in the `set_private_mode` arms and once in the `unset_private_mode` arms.
7. **The `CoreHandler` batch.** The same nine-field construction wrapped in `rows.begin`/`rows.finish` plus `drain_events` is repeated in `advance`, `flush_expired_synchronized_output` and `mode_set`. "How a batch against the term is opened and closed" should be one `with_handler(|h| ...)` method. As it stands, a new entry point could skip the row accounting.
8. **Submission lifecycle state.** Three representations must be kept in step: `SubmissionStage` (shared), `SubmissionPhase` (runner-local) and `PendingWrite.boundary`. Only `debug_assert!` holds them together. Collapse them into one state machine owned by the actor, with the canceller seeing a projection of it.
9. **SHELL fallback.** `base_env` fills `SHELL` from passwd, `PtyCommand::shell()` re-validates and falls back again, and `to_std_command` re-inserts it. Resolve it once at spawn.
10. **Executable checks.** In `search_path`, the checks for "is this candidate executable / a directory / missing" are duplicated between the cwd-relative branch and the absolute branch. Make one `classify_candidate` function.
11. **Agent glyph knowledge in the terminal layer.** `terminal/title.rs` hard-codes Claude's activity glyphs. That belongs to detection manifests, not the terminal layer; check whether the manifests also list them.
12. **Does the theme exist?** `TerminalTheme::is_empty` ignores `palette`, so a palette-only theme counts as empty. That is possibly a bug; worth checking the callers.

## 3. Structure

- **`src/ghostty/` should become `vt/` and be split.** `mod.rs` is about 2000 lines. It holds the colour model, the palette, the cell and style types, `Terminal`, the `RenderState` snapshot, the iterators and the text readers. Suggested split: `color.rs`, `cell.rs`, `render.rs` and `read.rs`, with `Terminal` as the core.
- **Delete the libghostty compatibility shims.** `RowIterator` and `RowCells` (no state), `populate_row_iterator` and `populate_cells` (which ignore their argument), `selection()` (always `None`), `content_bg_color()` (always `None`), the unused `bytes` scratch in `grapheme_text_into`, and the dead error-free `Result`s. Replace them with a plain `for row in state.dirty_rows()` iterator returning cell views. Removing them lets the module-wide `#![allow(dead_code)]` go.
- **`terminal_theme.rs`, `terminal_modes.rs`, `terminal_effects.rs` and `terminal_cell_size.rs` are about the host (outer) terminal, not the pane terminal.** They are loose top-level files whose names collide with the pane-terminal concepts in `ghostty/`. Group them as `host_term/{theme,modes,title,cell_size}.rs`. The OSC colour-response parsing in `terminal_theme` is the client's input-side parsing, and host colours and pane colours should share one `Rgb` from the vt module.
- **`src/terminal/` isn't terminal core.** It holds server-side terminal identity, state and registry, plus `TerminalRuntime`, a pass-through newtype over `crate::pane::PaneRuntime` ("still delegates to the legacy pane runtime while the migration proceeds"). That gives a `terminal -> pane` edge, while `pane` consumes `ghostty`, so the layering is circular in intent. Either finish the migration (move `PaneRuntime`'s body into `terminal`) or delete the wrapper. The three `spawn*` pass-throughs with `too_many_arguments` exist only because of the wrapper. `title.rs` belongs in detection.
- **`selection.rs` does two unrelated jobs.** One is selection geometry. The other is clipboard delivery: OSC 52, WSL detection that reads `/proc` and the environment, and `platform::write_clipboard`. The WSL and SSH environment sniffing belongs in `platform/`, and the OSC 52 encoding belongs in host-term. Selection also depends upward on `pane::ScrollMetrics` and `ratatui::Rect`. Once it is `AbsRow`-only, the `ScrollMetrics` dependency goes away.
- **`copy_mode.rs` is small and fine.** It does borrow `ghostty::unicode_codepoint_width` for cell counting while the core uses unicode-width plus the U+FF9E/U+FF9F special case. Copy-mode column maths therefore disagree with the grid for those two marks. The width rule should live in one place in the vt module and be used by both.
- **PTY layer.** The boundary is fine. `actor.rs` has three `PtyIoActorRunner` literal constructions in tests (a test builder would help), and the submission logic (about 400 lines) could move into its own `submission.rs` state machine, separate from the fd IO loop.

## Other things noticed

- **`Terminal::scroll_viewport_row` does unchecked subtraction.** It computes `history - row.min(history)`, which is safe, but uses bare `-` elsewhere with `i32` casts. It's fine, just fragile.
- **Poisoned event-queue locks are silently recovered.** `Listener` uses `unwrap_or_else(PoisonError::into_inner)` everywhere, while the actor treats a poisoned core as fatal. That's two policies for the same situation.
- **`set_history_lines` truncates events that were queued during `set_options`.** It is correct only because nothing else pushes concurrently.
