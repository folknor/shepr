# Design hunt: terminal (shepr-vt, shepr-termio)

Scope read in full: every non-test file of `crates/shepr-vt/src` and
`crates/shepr-termio/src`. Followed outward into the consumers that carry
the scope's decisions: `shepr-mux/src/pane/terminal/{backend,helpers,history,text}.rs`,
`shepr-mux/src/pane/{osc,runtime}.rs`, `shepr-server/src/server/{pane_input,client_shell}.rs`,
`shepr-server/src/server/headless/{render,retained_surface}.rs`,
`shepr-protocol/src/{input,surface}.rs`, and the client input and presentation
modules that use these types.

The report has three overall themes. Each comes up again under the four
questions below.

- **The adapter erases information and its consumers rebuild it.** shepr-vt
  knows which mouse protocol is active, which colours the child overrode,
  which OSC spelled a working directory, and what unit and base a coordinate
  has. Its public surface hands out booleans, flat RGB values, raw bytes and
  bare integers. mux, server and client then rebuild those facts by
  comparing values or re-reading modes, each in its own place.
- **shepr-termio is two crates in one.** One half encodes input for pane
  children (server side). The other half frames, parses and paints the host
  terminal (client side). They share almost nothing beyond `TerminalKey`.
  The split runs through `input/`, `host_term/` and `scroll.rs` instead of
  between crates.
- **Value types sit in the emulator crate.** `shepr-protocol`, `shepr-api`,
  `shepr-termio` and the `shepr` client binary all depend on `shepr-vt` (and
  so on `alacritty_terminal` and `vte`). They need only `AbsRow`, `Point`,
  `Selection`, `RgbColor`, `UnderlineStyle`, `ModifyOtherKeysLevel`,
  `ColorScheme` and the width functions.

---

## 1. Axes that should be types

### 1.1 Scrollback bytes and history lines are both `usize`
`Terminal::new(cols, rows, max_scrollback: usize)` takes a byte budget.
`Terminal.history_lines`, `CoreHandler.history_limit` and
`RowOrigin::note_pushed(.., history_limit)` hold line counts. `with_handler`
passes both side by side into `CoreHandler { history_limit, max_scrollback }`,
and `scrollback_lines(max_scrollback_bytes, columns)` converts one to the
other. Nothing stops a line count from going where bytes are expected. A
`ScrollbackBudget(bytes)` with `fn lines_at(columns) -> HistoryLines`, plus a
`HistoryLines` type for capacity, would make swapping them impossible. The
"capacity never shrinks below held content" rule in `resize` could then live
on `HistoryLines`.

### 1.2 Row spaces beyond the three typed ones
`ViewportRow`, `ScreenRow` and `AbsRow` exist, but more row spaces travel as
bare integers:
- `Terminal::cursor_y() -> u16` is a line of the live screen (0 = top of the
  active screen). It is neither a viewport row nor a screen row. mux turns
  it into a screen row by hand (`viewport_start + cursor_y` in
  `terminal_recent_read_range`).
- `TerminalScrollbar.offset` is the `ScreenRow` of the viewport top, typed as
  `usize`. mux iterates `viewport.offset..offset+len` and wraps each value in
  `ScreenRow(row)` (`resize`'s recovery probe).
- `RowView::y() -> u16` is a viewport row. mux wraps it back up
  (`ViewportRow(row.y())` before `viewport_hyperlink_uri`).
- `CursorViewport { x: u16, y: u16 }` is a `Point<ViewportRow>` in all but
  name.
- mux `terminal_recent_read_range -> Option<(usize, usize, u16)>`: two raw
  screen rows and a column count in a tuple.

The fix is a `LiveRow` type (or simply returning `ScreenRow` from the cursor
accessor), typed fields on `TerminalScrollbar`, `RowView::y() -> ViewportRow`,
and `RenderCursor.viewport: Option<Point<ViewportRow>>`.

### 1.3 Scroll direction is a sign
`Terminal::scroll_viewport_delta(isize)` means "negative = older history".
alacritty's `Scroll::Delta` means the opposite. mux `scroll_up(lines)` passes
`-lines`, and vt negates it again before clamping to `i32`. That is two sign
flips held in step by comments. mux `paragraph_motion_target(direction: i8)`
does the same thing. A `ScrollTowards::{Older(n), Newer(n)}` (or
`Direction::{Up, Down}` plus a count) removes the sign entirely.

### 1.4 Mouse coordinates: unit and base are not typed
- `encode_mouse_event(kind, x: u32, y: u32, ..)` takes "1-based cells, or
  pixels for SGR-pixels". The caller must pair pixel numbers with
  `MouseProtocolEncoding::SgrPixels` and add the `+1` itself; mux
  `encode_mouse_event` does both by hand for four position/mode combinations.
- `RawInputEvent::Mouse(crossterm::MouseEvent)`: when the host has 1016
  enabled, `column`/`row` hold pixels minus one, not cells. The framer cannot
  say which. The client (`classify_unix_input` in `shepr-client/src/input.rs`)
  decides afterwards by checking whether the raw bytes start with `ESC [ <`
  and reading an `AtomicBool` of the host mode, then adds the 1 back.

Proposal: `MouseReportPosition::{Cell(CellPos), Pixel(PixelPos)}` with an
explicit 1-based newtype at the encoder. Tell the framer the host mouse mode
so it emits `HostMouse { position: HostPosition::{Cell, Pixel} }`. termio's
`mouse::Position` and protocol's `ClientMousePosition` are near copies of
that type already.

### 1.5 Read failures are prose
`shepr_vt::Error(&'static str)` is the only failure type of `read_text_screen`,
`read_ansi_screen_carrying` and `viewport_hyperlink_uri`: "selection start
out of range", "viewport column out of range". Callers cannot branch on it
and in practice discard it (`.ok()` in `terminal_extract_selection`,
`terminal_detection_text`). A selection whose rows were evicted is a real,
user-visible case ("copy failed because the text scrolled out of history").
`ReadError::{RowNotRetained, ColumnOutOfRange}`, or `Option` where nobody
cares, would let callers branch. `ClearScreenOutcome` is the house example
of doing this right.

### 1.6 Colour provenance is erased
`RenderColors` gives the resolved foreground, background and 256-entry
palette as plain `RgbColor`. The resolution order is child OSC override,
then host default, then built-in. mux needs to know which tier answered, so
it rebuilds the answer by comparing values:
- `terminal_default_fg/bg` compare the resolved colour with
  `host_theme.foreground/background` and with `initial_default_*`, which is
  the built-in `DEFAULT_FOREGROUND`/`DEFAULT_BACKGROUND` captured at pane
  creation, as the `lib.rs` comment admits.
- `PaletteOverrides::new` compares the active palette with
  `default_palette()` entry by entry to rediscover which entries the child
  set with OSC 4.

vt knows the answer exactly (`term.colors()[i].is_some()`). Value comparison
gives wrong answers at the edges. With no host theme, a child that sets the
foreground to white is indistinguishable from "no override". A child that
sets OSC 4 to the host's own value reads as "not overridden". Proposal:
`ResolvedColor { rgb, source: ColorSource::{Child, Host, Builtin} }` in
`RenderColors`, and `RenderColors::palette_overrides()` yielding only
child-set entries.

### 1.7 Selection shape is lost
`Selection::line_range(pane, anchor_row, cursor_row, end_col)` encodes "whole
lines" as columns `0..end_col` taken at creation time (server API passes
`width.saturating_sub(1)`). The selection no longer knows it is a line
selection. The client keeps that fact elsewhere (`ClientCopySelection::Line`)
and rebuilds the selection. After a widening resize a line selection does
not cover the new columns. Add `SelectionShape::{Range, Lines}` on
`Selection`.

### 1.8 Working-directory report source is discarded
`scan.rs` knows whether a report came from OSC 7 (a URI), OSC 9;9 (a path) or
OSC 1337 CurrentDir (a path), and emits `WorkingDirectoryReport(pub Vec<u8>)`.
mux `parse_reported_cwd` guesses again from a `file://` prefix. An OSC 7 URI
with another scheme (kitty's `kitty-shell-cwd://`) is taken as a literal
path. Proposal: `WorkingDirectoryReport::{Uri(Vec<u8>), Path(Vec<u8>)}`.

### 1.9 Progress reports are a closed set carried as bytes
`ProgressReport(pub Vec<u8>)` holds the ConEmu `4;state;percent` payload. mux
turns it into `latest_progress: String` (empty string means none) and hands
detection a string. The state is a closed set (0 to 4) and the percent is an
optional 0..=100. The scanner could parse it once into `Progress { state:
ProgressState, percent: Option<u8> }`. Detection would then match variants,
not text.

### 1.10 Boolean pairs and flags at API boundaries
- `restore_host_keyboard_protocol(writer, modify_other_keys_active: bool,
  kitty_entry_active: bool)` is only ever called as `(true, false)` or
  `(false, true)` (`restore_modify_other_keys`, `restore_kitty_keyboard_entry`
  in client `terminal_setup.rs`). That is two functions sharing one name and
  a swappable pair of bools.
- `copy_mode_page_lines(height, half_page: bool)`,
  `write_clipboard_bytes(bytes, prefers_osc52_clipboard: bool, w)`.
- `RepeatPlan::Reprocess { tracked: bool }` and `reprocess_allowed(..,
  tracked: bool)`. "Tracked" is a lease state and wants an enum.

### 1.11 Durations as `i32` milliseconds
`termio/limits.rs` mixes `*_TIMEOUT_MS: i32` (for `poll`) with
`PASTE_STALL_TIMEOUT: Duration`. `held_input_flush_timeout_ms() -> i32` leaks
the poll unit into the framer API. Use `Duration` throughout and convert at
the `poll` call.

### 1.12 Synchronized-update deadline state
`SyncUpdateTimeout` keeps `pending: bool` and `deadline: Option<Instant>` for
one fact; `mode_get(SynchronizedOutput)` reads `deadline`, vte reads
`pending`. See L1 for the consequence. A single
`enum SyncState { Idle, Buffering { deadline: Instant } }` cannot disagree
with itself.

---

## 2. Decisions made in more than one place

### 2.1 Which mouse protocol a pane speaks
Question: given the pane's DEC modes, what mouse mode and encoding apply?
- vt handler: models the X10/1000/1002/1003 exclusivity and the 1005/1016
  exclusivity (`set_private_mode`, `unset_private_mode`).
- vt `mouse_tracking_enabled()`: `MOUSE_MODE` bits or x10.
- mux `PaneTerminal::encode_mouse_event` (production): precedence AnyMotion >
  ButtonMotion > PressRelease > X10, then SGR > UTF-8 > default. SGR-pixels is
  chosen separately per position.
- mux `input_state` (test snapshot): a second copy of the same ladder, with a
  different encoding precedence (SgrPixels > Sgr > Utf8 unconditionally).
- server `PaneRuntime::wheel_routing` and `plain_page_keys_use_host_scrollback`
  read the same modes again.

They agree today apart from the deliberate pixel fallback. The owner should
be vt: `Terminal::mouse_protocol() -> Option<MouseProtocol { mode, encoding,
pixels_requested }>`. The termio enums move with it, and `mouse_tracking_enabled`
becomes `mouse_protocol().is_some()`.

### 2.2 Whether a mouse report uses pixels or cells
- client `mouse.rs`: sends pixels when `hit.sgr_pixel_mouse && hit.pixel_width > 0`.
- server `pane_input::downgrade_ineligible_pixel_mouse`: downgrades unless the
  client geometry equals the runtime grid and pixel extent.
- server `apply_client_pane_input_event`: maps `Pixels` to `Position::Pixels`
  only if `runtime.sgr_pixel_mouse_enabled()`, else to `Cell`.
- mux `encode_mouse_event`: under 1016 maps cells to pixels through
  `cell_pitch` (width_px / cols), and maps pixels back to cells when 1016 is
  off.

That is four sites. They agree only because each re-checks the same mode
bit under a separate lock (see L4). The owner should be one function next to
the mouse protocol type that takes the pane's `PixelExtent` and the report.

### 2.3 How big a pane is in pixels
- vt `PaneGeometry::text_area_px()` (cell x cols): drives `CSI 14 t`, the 2048
  in-band report and `width_px()/height_px()`.
- server `client_shell.rs`: `inner_rect.width * HostCellSize.width_px`, with
  `(0, 0)` meaning unknown, published as `PaneSurfacePane.pixel_width/height`.
- mux `cell_pitch`: `width_px() / cols`, at least 1.

They agree only while the pane's `PaneGeometry.cell` equals the client's
`HostCellSize` and the grid equals `inner_rect`. With several clients of
different cell sizes, the server publishes per-client extents while the
child was told the geometry-source client's extent. Owner: `PaneGeometry`,
with the wire carrying `Option<PixelExtent>` taken from it.

### 2.4 Scroll position from the bottom
- vt `scrollbar()` returns from-top numbers (`total`, `offset`, `len`).
- mux `terminal_scroll_metrics`, `terminal_set_scroll_offset_from_bottom` and
  the inline copy in `PaneTerminal::resize` each compute
  `total - (offset + len)` and `total - len`.
- termio `ScrollMetrics::viewport_top_row` computes the inverse
  (`max - offset` plus origin).
- four field-by-field conversions between `shepr_termio::ScrollMetrics`
  (`usize`) and `shepr_protocol::PaneSurfaceScrollMetrics` (`u64`): server
  `client_shell.rs` and `retained_surface.rs` (`as u64`), client
  `composition.rs` and `surface_patch.rs` (`try_from(..).unwrap_or(usize::MAX)`).

They agree today. The owner should be vt, returning one `ScrollMetrics`
(with `offset_from_bottom <= max` enforced by construction). The same type
should be the wire type.

### 2.5 Whether an absolute row is retained
- `AbsRow::screen_row(origin) -> Option<ScreenRow>` checks only the lower bound.
- `Terminal::screen_row_for_absolute(row)` checks both bounds.

They already disagree for rows past the end. mux
`terminal_extract_selection` uses the lower-bound-only form and relies on
`read_text_screen`'s prose error to catch the upper bound. Owner: `Terminal`.
Remove `AbsRow::screen_row`.

### 2.6 Is this colour light or dark
- vt `RgbColor::inferred_appearance`: BT.601 luma on gamma-encoded channels,
  threshold 128/255. Feeds the `ColorScheme` reported to children (DSR 997)
  and the server's host appearance (`clients.rs`).
- termio `selection_render`: WCAG relative luminance (linearised) `< 0.5`
  picks the direction the highlight mixes toward. A third rule, the contrast
  ratio, picks the selection foreground.

They already disagree. A grey of 150 is Light to vt (luma 150) and dark to the
selection code (relative luminance about 0.30). For any background with luma
between 128 and roughly 188 the host is reported as light to children while
the selection treats it as dark. Owner: one `RgbColor::appearance()` (and
`contrast_with`) in the shared value crate.

### 2.7 What RGB a named colour is
vt `default_palette()` says ANSI red is `#cc6666`. termio
`selection_render::color_to_rgb` says `Color::Red` is `(128, 0, 0)` (VGA). The
two disagree by construction. The termio table should resolve through the
palette in force (host theme or `default_palette`).

### 2.8 SGR spelling of styles and colours
- `UnderlineStyle` to SGR (`4`, `4:2`..`4:5`): `format.rs` `UNDERLINE_SGR` and
  `blit.rs` `style_to_sgr_parts`.
- colour numbering (`30+i`, `90+i-8`, `38;5;n`, `38;2;r;g;b` and the
  background forms): `format.rs` `push_color` and `blit.rs`
  `color_to_sgr_fg/bg`.

They agree. Owner: `UnderlineStyle::sgr_param()` next to the enum. The
numbering would need a small shared SGR writer, which both an alacritty
`Color` and a `WireColor` can feed.

### 2.9 OSC colour reply encoding
- vt `color_query_format`: `rgb:` with each byte printed twice as `{:02x}{:02x}`.
- mux `osc_rgb_response`: the same thing as `{:04x}` of `x * 257`, and its own
  `ColorQueryTarget` to OSC number mapping (`10`, `11`, `12`, `4;i`).
- termio `host_term/theme.rs` parses the same `rgb:` form from the host.

They agree. Owner: `ColorQuery::reply(color, ReplyForm::{AsAsked, Canonical})`
in vt. mux then only picks the form.

### 2.10 The VT vocabulary is spelled separately on each side
- Focus `CSI I`/`CSI O`: vt `encode_focus` and termio `raw_input` matching.
- DSR 997 reports: vt `ColorScheme::report()` and termio
  `GHOSTTY_COLOR_SCHEME_{DARK,LIGHT}_REPORT`. DSR 996 query: `scan.rs` and
  `HOST_COLOR_SCHEME_QUERY_SEQUENCE`.
- modifyOtherKeys `CSI > 4 ; n m`: vt `apply_scan_event` (one literal per
  level), handler `report_modify_other_keys` (through `Display`), host
  `set_host_keyboard_protocol` and `HOST_MODIFY_OTHER_KEYS_RESET_SEQUENCE`.
- Kitty flag bits: vt `kitty_keyboard_flags` (inline 1/2/4/8/16),
  `shepr_protocol::KittyKeyboardFlags` constants, and crossterm
  `KeyboardEnhancementFlags` (bridged with `from_bits_retain`).
- DEC mode numbers: `blit.rs` writes `?2026h`, `?25l`; `host_term/modes.rs`
  writes `?1006l` and the rest as literals, although `DecMode::number()` exists.
- Bracketed paste markers: termio `BRACKETED_PASTE_START/END` (and the pane
  side wraps pastes in mux).

All of these agree today. Owner: a `shepr_vt::seq` (or value-crate) module of
typed builders and matchers (`Focus::sequence()`, `ColorScheme::report()`,
`ModifyOtherKeysLevel::set_sequence()`, `DecMode::set()/reset()`) used by both
the pane side and the host side.

### 2.11 Key spellings: encode and parse are mirrored tables
- Functional keys: encode has `encode_kitty_functional_key`,
  `encode_modified_special`, `encode_legacy_inner`, `encode_f_key`,
  `apply_application_cursor` and the special-key list inside
  `try_encode_csi_u`. Parse has `parse_legacy_special_sequence`,
  `parse_xterm_modified_special_sequence` and `kitty_codepoint_to_keycode`.
- Modifier bits: `xterm_modifier`/`kitty_modifier` vs `key_modifiers_from_u8`.
- Legacy control bytes: `encode_legacy_inner`'s Ctrl table vs
  `parse_legacy_ctrl_char`.
- Mouse button byte: `encode_mouse_cb`/`encode_mouse_event` vs `parse_mouse_cb`.

Round-trip tests keep the pairs in step (for example
`parse_legacy_alt_shift_letter_preserves_shift` encoding what it parsed).
That is a pairwise-agreement arrangement and catches only the pairs it
exercises. Owner: one table of
`FunctionalKey { code, legacy: (number, final), kitty_codepoint }` and one
`Modifiers <-> bits` mapping, each read in both directions.

### 2.12 What character a key produces
- `copy_mode_command_char` (used by `keybind_help_text_char` too).
- encode `text_char_for_key` / `shifted_text_char` / `is_shifted_ascii_punctuation`.
- `TerminalKey::with_text_commit` (uppercase means Shift).
- parse `parse_legacy_key_sequence` (uppercase sets Shift).
- keybindings `generated_character_key`.
- `shepr_config::BindingKey::canonical_key`.

Two pairwise tests keep some of these in step
(`legacy_shift_ascii_punctuation_matches_copy_mode_mapping`,
`help_filter_and_copy_mode_agree_on_shifted_ascii_keys`). The rules
legitimately differ by context: copy mode applies the US shift table to
letters too, the encoder only when there is no `shifted_codepoint`. But the
base question "Shift plus this base key gives which char" should be one
method, `TerminalKey::produced_char()`, with each context adding its own
policy on top.

### 2.13 How wide text is
- vt `unicode_codepoint_width` / `unicode_text_width`: grid widths per code
  point, with the voiced-mark override.
- termio `blit::text_width` (and its aliases `symbol_width` and `cell_width`):
  grapheme width from unicode-width, plus one per U+FF9E/U+FF9F, with the two
  codepoints hard-coded again instead of using vt's
  `is_halfwidth_voiced_mark_codepoint`.
- mux `terminal_buffer_symbol_into`: measures `symbol.width()` against
  `CellWide`, with special cases built from vt's voiced-mark predicates.
- client `word_bounds` re-measures row text with `unicode_codepoint_width`.

The grid rule and the grapheme rule differ on purpose (ZWJ emoji: 6 versus 2).
The voiced-mark rule, though, is written twice and the predicates three times.
Owner: one width module in the value crate exposing both rules by name
(`grid_width`, `display_width`).

### 2.14 Blit: three row painters
`write_all_cells`, `write_changed_cells` and `blit_patch_to` each decide which
cells to repaint (`invalidated`, `to_skip`), when to reposition the cursor
(`next_inline_col`) and how wide a cell is. Two equality rules exist:
`cells_equal` compares hyperlink indices and `cells_visually_equal` compares
sanitised URIs. They agree only because `patch_rows_fit` refuses any patch
that touches a hyperlink. The patch walker also repaints a wide glyph's
omitted successor, which the diff walker handles through `invalidated`. A
test asserts that patch bytes equal diff bytes (a pairwise check). Owner: one
row painter parameterised by a "previous cell at (x, y)" source, used by all
three.

### 2.15 What effects the terminal queues
vt keeps seven separate queues (`responses`, `pwd_changes`,
`clipboard_writes`, `dropped_clipboard_store_bytes`, `title_update`,
`progress_update`, `default_color_set`). mux lists them three times:
`collect_core_effects` (takes some of them; `AgentOscStateTracker` takes title
and progress), and two identical drop lists, `discard_core_effects`
(helpers.rs) and `discard_initial_terminal_effects` (runtime.rs). A new queue
must be added to all of them. Owner: `Terminal::take_effects() ->
TerminalEffects` (one struct, `#[must_use]`). Discarding is then dropping it.

### 2.16 History capacity after a purge, and the title-event hack
`Terminal::restore_scrollback_budget_after_history_purge` plus
`set_history_lines` (lib.rs), and
`CoreHandler::restore_scrollback_budget_after_history_purge` (handler.rs),
which inlines its own copy of `set_history_lines`, including the
"truncate the synthetic title event `set_options` emits" trick. They agree.
Owner: one `HistoryCapacity` struct that the handler borrows (see 3.3).

### 2.17 Is the alternate screen active
`rows.rs::primary_active`, `CoreHandler::primary_screen_active`, the
`active_keyboard_depth` test, and four raw `mode().contains(ALT_SCREEN)`
checks in `lib.rs` (`resize`, `clear_screen`, the purge restore,
`active_screen`). mux compares `active_screen() == Alternate` in many more
places. The expression is trivial, but `RowOrigin` and the keyboard-depth
mirror depend on it meaning exactly alacritty's grid swap. One accessor
should be the only reader.

### 2.18 Where an OSC ends
vte decides. `scan.rs::Scanner` mirrors vte's framing to find working
directory, progress and oversized OSCs. mux `osc.rs::OscStreamCollector`
mirrors vte a third time for the opt-in OSC debug log, and says so in its
doc comment. Owner: the scanner. It could emit an `OscBody` event (when the
debug log is on) and the collector could go.

### 2.19 Is the kitty protocol on
`matches!(protocol, Kitty { flags } if flags != 0)` in `encode_terminal_key`,
`modes.kitty_flags != 0` in `encode_terminal_key_with_modes`, and
`KeyboardProtocol::from_kitty_flags`. `KeyboardProtocol::Kitty { flags: 0 }`
can be built through the public variant, which is why the first site checks
again. Owner: `KeyboardProtocol` with a private payload of typed flags and
only a `from_flags` constructor.

### 2.20 Two notions of "synchronized update active" (deliberate)
DECRQM ?2026 answers from `ExtraModes.synchronized_update` (replay order), and
`mode_get(SynchronizedOutput)` from the parser deadline. This is documented
and correct (they answer different questions). It is still worth naming the
two as separate methods (`sync_update_in_replay`, `sync_update_buffering`) so
nobody unifies them by accident.

### 2.21 Lateral: title policy and word classes
- Title: vt cuts at `MAX_TITLE_BYTES` (bytes), mux `sanitize_agent_osc_string`
  filters controls and caps at `AGENT_OSC_MAX_CHARS` (chars), and termio
  `write_window_title` strips controls again for the host. Each has a reason,
  but "what is a displayable title" has no single owner.
- Word classes: mux `text_class` (`COPY_MODE_WORD_SEPARATORS`) for copy-mode
  `w`/`b`/`e` motion, and client `word_bounds::is_word_separator` (including
  CJK punctuation) for double-click. They differ. Whether that is intended is
  not written anywhere.

---

## 3. Structure

### 3.1 Split value types out of the emulator crate
`shepr-protocol`, `shepr-api`, `shepr-termio` and `shepr-client` depend on
`shepr-vt` only for value types: `AbsRow`/`ScreenRow`/`ViewportRow`/`Point`,
`selection::Selection`, `RgbColor`, `ColorScheme`, `DefaultColor`,
`UnderlineStyle`, `ModifyOtherKeysLevel`, `FocusEvent` and the width functions.
That puts `alacritty_terminal` and `vte` in the client binary's build graph,
the same kind of edge AGENTS.md keeps `shepr-mux` and `shepr-server` out of
the client for. Move these into a small crate (`shepr-term-types`, or
`shepr-core` modules) that vt re-exports. Protocol and the client then stop
depending on the emulator. `KittyKeyboardFlags` should move down into the
same crate from `shepr-protocol`, so vt can return it (see 4.4).

### 3.2 Split shepr-termio by side
termio today holds:
- **child-facing (server):** `input/encode.rs`, `MouseProtocolMode/Encoding`,
  `KeyEncodeModes`, `mouse::Position`, `TerminalKey`.
- **host-facing (client):** `input/raw_input.rs`, `input/parse.rs` (used by the
  framer), `input/keybindings.rs`, `input/keybind_help.rs`, `input/lease.rs`,
  `host_term/*`, `blit.rs`, `selection_render.rs`, `copy_mode.rs`,
  `input/mouse.rs` (`HostPixelExtent`, `HostPixels`).
- **shared values:** `ScrollMetrics`, `TerminalTheme`, `HostCellSize`,
  `text_width`, and the scrollbar geometry and drawing that the server chrome
  draws and the client hit-tests.

The server therefore links keybinding help, the host input framer and the
blitter. The client links pane key encoding. Proposal:
- Move pane input encoding next to the emulator (`shepr_vt::input`, or a
  `shepr-pane-input` crate). It should consume one `InputModes` snapshot that
  vt produces (kitty flags, modifyOtherKeys level, DECCKM, bracketed paste,
  focus reporting, mouse protocol, alternate scroll, alternate screen, cell
  pixels) under one lock. That removes the mode-reading ladders in mux
  (2.1), the per-accessor locking (L4) and `as_u8()` (4.3).
- Move the host-terminal half into `shepr-client` (or a `shepr-hostterm`
  crate that only the client takes).
- Collapse the parallel wire types into the shared ones. Protocol already
  depends on vt and `RgbColor` already derives serde, yet the wire has
  `ClientHostColor`, `ClientHostAppearance` and `ClientHostDefaultColorKind`
  next to `RgbColor`, `ColorScheme` and `DefaultColor`, joined by
  `theme_conversion.rs`. The same goes for `ClientMousePosition` and
  `ClientMouseGeometry` next to termio's `Position` and `HostPixelExtent`,
  `ClientPaneInputEvent::Key` field-for-field with `TerminalKey`, and
  `PaneSurfaceScrollMetrics` next to `ScrollMetrics`.

The description "copy mode" in AGENTS.md's crate list overstates what is
here: `copy_mode.rs` is four helpers. Copy mode lives in the client
(`shell/input/copy_mode.rs`) and mux (`text.rs` search and motions).

### 3.3 `Terminal` is a god struct; the handler borrows 12 of its fields
`Terminal` has 25 fields across five concerns: emulator plus parser, history
accounting (`rows`, `history_lines`, `max_scrollback`, `keyboard_depth`), host
colours (`default_palette`, `host_*`, `color_scheme`, `cell`), the effects
outbox (seven queues), and damage tracking (three counters). `with_handler`
destructures twelve of them into `CoreHandler`, field by field. That is why
2.16 exists: the handler cannot call `Terminal` methods, so it copies them.
Regroup into `Emulator`, `HistoryCapacity`, `HostDefaults`, `Effects` and
`Damage`, and have `CoreHandler` borrow `&mut HistoryCapacity`,
`&HostDefaults` and so on. The duplicated methods then live on those types
once.

### 3.4 Dirty-row state in vt, clearing policy in mux
`RenderState` owns `Dirty` and per-row `Cell<bool>` dirty bits. The clearing
protocol lives in mux `terminal_collect_dirty_patch`: it clears rows through
shared references (`RowView::clear_dirty`, interior mutability), computes
`rows_left`, and calls `set_dirty`. State and behaviour are split. Any caller
can `set_dirty(Clean)` without clearing rows. Move it into
`RenderState::take_dirty_rows(max_rows) -> DirtyRows`, which commits on drop
or on an explicit `commit()`, so the fallback path can leave it unchanged.
Then delete `set_dirty` and `clear_dirty` from the public API.

### 3.5 Lock policy lives in the emulator crate
`locks.rs` (`lock_auxiliary`, `try_lock_auxiliary`,
`recover_auxiliary_poison`, `lock_terminal_core`, `terminal_core_is_poisoned`)
is general poison policy. mux uses `shepr_vt::lock_auxiliary` for unrelated
mutexes (`render_signal.rs`). `lock_terminal_core<T>` accepts any mutex, so
"this is the terminal core" is a naming convention only. Move the auxiliary
policy to `shepr-core`. Make the core a `TerminalCore(Mutex<..>)` newtype
whose only lock method returns `Result<Guard, TerminalCorePoisoned>`.

### 3.6 The mode table stores one fact twice
`modes::ModeSpec` has `get: Getter::Extra(extra)` and `extra: Option<ExtraMode>`.
The `extra()` constructor sets both, but the struct-literal rows could set
them to different modes. `mode_get` reads `get` and `extra_mode(number)` reads
`extra`. Derive `extra` from `get` (a method).

### 3.7 Smaller boundary notes
- `host_term/title.rs` also does clipboard writing (OSC 52 plus native via
  `shepr_platform`). That deserves its own module (`host_term/clipboard.rs`).
- `input::raw_input::HostReplyPolicy` is a 13-method trait with default
  no-ops and two impls, one of them empty (`NoHostReplies`), apparently only
  for tests and "ordinary framing". A concrete `HostReplies` with an
  "inactive" state would do.
- `KeybindMatch` has a single variant `Action(KeybindAction)`. It is a
  leftover wrapper.
- `scroll.rs` mixes a value type (`ScrollMetrics`), scrollbar geometry and
  ratatui drawing. The geometry is shared (server draws it, client hit-tests
  it). The drawing is server chrome.
- `Terminal::new(cols: u16, rows: u16, ..)` clamps raw numbers while
  `resize(PaneGeometry)` takes the validated type. Construct from
  `PaneGeometry` too.

---

## 4. Types that resolve to primitives

### 4.1 `AbsRow(pub u64)`
Escape hatches: the public field, `From<u64>`, `saturating_add(u64)` and
`saturating_sub(u64)`, plus `.0` arithmetic across mux (`history.rs`,
`text.rs`) and the client. Sentinel: the client uses `AbsRow(0)` for "no
scroll metrics" (`mouse.rs` drag anchor). `viewport_row(top)` saturates rows
above the viewport to 0 and far below to `u16::MAX`, and the client clamps
the result again. Offer `AbsRow::checked_offset_from(origin)`, a
`ViewportPosition::{Above, At(ViewportRow), Below}` result instead of the
saturating conversion, an `AbsRange` for selections and reads, a private
field, and no `From<u64>`.

### 4.2 `ScreenRow(pub usize)`, `ViewportRow(pub u16)`
Built from raw loop counters everywhere (`ScreenRow(row)` in mux scans,
`ViewportRow(row - pane.y)` and `ViewportRow(cursor.y - inner.y)` in the
client). Offer iterators (`Terminal::screen_rows()`, `ScrollMetrics::viewport_rows()`)
and a `Rect::viewport_row_at(screen_y) -> Option<ViewportRow>` so nobody
subtracts by hand.

### 4.3 `ModifyOtherKeysLevel` collapses to `u8`
`as_u8()` is called in mux twice (`PaneTerminal::modify_other_keys_level`
and `encode_terminal_key_once`, which fills
`KeyEncodeModes.modify_other_keys: u8`). The encoder then compares `>= 2`,
`> 0` and `level < 2`, and server `render.rs` compares
`modify_other_keys_level() > 0`. The test input-state snapshot collapses it a
third way, to a bool (`== All`). `Display` prints the number so it can be
spliced into escapes. `from_parameter` is used only by tests. Offer: take the
enum in `KeyEncodeModes`, add `ModifyOtherKeysLevel::encodes(KeyCode) -> bool`
and `set_sequence()`, and drop `as_u8` and `Display`.

### 4.4 Kitty keyboard flags as `u16`
`Terminal::kitty_keyboard_flags() -> u16` (inline bit literals),
`KeyboardProtocol::Kitty { flags: u16 }`, `KeyEncodeModes.kitty_flags: u16`,
`HostKeyboardProbeResponses.flags: Option<u16>`. The protocol's
`KittyKeyboardFlags` newtype has `bits()` and no `contains`, so every test is
`flags & KittyKeyboardFlags::X.bits() != 0` (about ten sites in `encode.rs`
and `model.rs`). `set_host_kitty_keyboard_report_all` round-trips through
crossterm's `u8` flags with `u8::try_from(..).unwrap_or_default()`. Offer a
flags type with `contains`, `is_empty` and `insert`, owned by the value crate
so vt can return it (3.1).

### 4.5 `HostCellSize { width_px: u32, height_px: u32 }`
`Default` (zeros) means unknown. `is_known()` re-validates through
`CellPx::new`, and `or_default()` normalises invalid sizes to zeros. The
framer's `parse_host_cell_size_report` validates into a `CellPx` and then
destructures it back to `(u32, u32)` for `RawInputEvent::HostCellSizeReport`.
The server multiplies the raw fields (`client_shell.rs`). Offer
`Option<CellPx>` end to end.

### 4.6 `HostPixelExtent` fields
The grid is private but `width_px`/`height_px` are `pub` on a `Copy` type, so
the `> 0` invariant `new` checks can be undone by assignment. Make them
private and add accessors.

### 4.7 Zero as "no pixel geometry"
`Terminal::width_px()/height_px()` return 0 when the cell size is unknown.
`PaneSurfacePane.pixel_width/pixel_height` carry 0 on the wire for the same
meaning, and `retained_surface.rs` builds 0s directly. The client tests
`pixel_width > 0`, and mux `cell_pitch` tests `> 0`. Offer
`Option<PixelExtent>` from `PaneGeometry` to the wire.

### 4.8 Cursor shape and position in blit
`BlitEncoder.last_cursor_shape: u8` (0 = terminal default),
`HostCursorState.shape: u8` filled from `cursor.shape as u8` (an enum cast to
a DECSCUSR parameter), and positions as `(u16, u16)` tuples
(`last_visible_cursor`, `clamp_cursor_position`). `CursorShapeParam` already
exists. Keep it, with `Option` for "never set", and give it a
`decscusr()` method.

### 4.9 Codepoints as `u32`
`TerminalKey.shifted_codepoint: Option<u32>` (and the matching wire field and
`BindingKey::shifted_codepoint`). Every reader re-validates with
`char::from_u32` (copy mode, encode, parse's Shift inference).
`unicode_codepoint_width(codepoint: u32)` is called as `ch as u32` by the
client. Use `char`. The kitty associated-text and alternate-key encoders can
print `u32::from(ch)` at the edge.

### 4.10 `Selection` escape hatches
`ordered_cells() -> ((AbsRow, u16), (AbsRow, u16))` exists so callers can drop
`Point`. The client's `word_selection.rs` then stores `(AbsRow, u16)` tuples
(`anchor`, `cursor`). `pub pane_id: P` is compared field-wise in
`render_selection_highlight`. Offer `Selection::range() -> AbsRange`,
`belongs_to(&P)`, and drop the tuple form.

### 4.11 Sentinels in constants
`PANE_TRUECOLOR_BITS_PER_CHANNEL: Option<&[u8]>` makes `PANE_COLORTERM` a
`match` that yields `""` for "no truecolor". It is a compile-time switch with
one value ever used, and the `""` is a sentinel. Make it a plain constant.
Truecolor is part of the pane identity, and the XTGETTCAP `Tc`/`RGB` answers
can be plain constants too.

### 4.12 String-typed enums in keybind help
`keybind_help_groups` keys groups by `&'static str` names ("global",
"navigation", "workspaces", "panes") looked up with
`groups.iter().position(|(name, _)| *name == group)`. Insert-after and alias
merging find rows by comparing label strings (`existing.1 == label`). Entries
are `(String, Cow<'static, str>)` tuples. A `HelpGroup` enum and
`HelpRow { keys, label }` would make a typo in the keybinding table a compile
error instead of a silently appended group.

### 4.13 `Dirty` plus public setters
`RenderState::set_dirty(Dirty)` and `RowView::clear_dirty(&self)` let a
caller write any state through shared references (3.4).

### 4.14 Mouse modifiers as `u8`
server `apply_scroll(.., modifiers: u8)` gets `modifiers.bits()` and turns it
back with `KeyModifiers::from_bits_truncate`. Pass the modifier type.

---

## Lateral findings

- **L1: synchronized-update expiry can be lost.** `SyncUpdateTimeout::set_timeout`
  stores `deadline = now.and_then(|now| now.checked_add(duration))` but sets
  `pending = true` unconditionally. If `now` is unset or the add overflows,
  vte keeps buffering (`pending_timeout()` is true) while `deadline` is `None`.
  `tick()` then never ends the frame, and `mode_get(SynchronizedOutput)`
  reports false while output is withheld. This cannot happen today because
  every `advance` sets `now` first, but nothing forces that ordering. See 1.12.
- **L2: `AbsRow::viewport_row` clamps silently.** It is used by the client to
  place a drag anchor. An anchor scrolled above the viewport compares equal
  to viewport row 0.
- **L3: dead branch on the hot path.** In mux `terminal_cell_paint`,
  `cells.fg_color()` is `None` exactly when `basic.style.fg_color` is `None`,
  so the `.or_else(|| cells.fg_color()..)` arms never contribute. Yet
  `cell_color` is computed twice per cell (once in `basic_data`, once in
  `fg_color()`/`bg_color()`). `terminal_buffer_symbol_into` also re-measures
  `symbol.width()` for every cell of every dirty row. Both run per cell per
  patch collection.
- **L4: one lock per accessor, inconsistent snapshots.** `PaneTerminal`'s
  `mode_enabled`, `bracketed_paste_enabled`, `focus_reporting_enabled`,
  `sgr_pixel_mouse_enabled`, `mouse_reporting_enabled`,
  `modify_other_keys_level` and `negotiated_keyboard_protocol` each take the
  terminal-core lock. The server input path calls several in a row for one
  event (`sgr_pixel_mouse_enabled`, then `wheel_routing`, then
  `encode_mouse_wheel`, which reads the modes again). That is several
  acquisitions per event, and the child can change modes between them. An
  `InputModes` snapshot (3.2) fixes both.
- **L5:** `selection_render::selection_palette_background` and
  `panel_contrast_fg` have identical bodies.
- **L6:** `parse_reported_cwd` takes any non-`file://` OSC 7 payload as a
  literal path (1.8).
- **L7:** `InputLeaseTable::normalize_press` returns its input unchanged. The
  name suggests a transformation. It only drops an old lease.
- **L8:** the mux `OscStreamCollector` is a third OSC framer (2.18). If it
  drifts from vte, the debug log shows sequences the terminal did not see.
- **L9:** `Terminal::drain_events` silently drops empty clipboard stores and
  any `ClipboardType::Selection` store, with no counter like the oversized
  case has. That is probably fine, but it is the only effect with no trace.
- **L10:** mux `content_revision.wrapping_add(2)` and the
  `after.is_multiple_of(2)` checks in `client_shell.rs` encode a flag in the
  parity of a counter. That is outside this scope, but it is a sentinel of
  the kind 4 asks about.
