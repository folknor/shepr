shepr-vt review (read only; no edits, no builds). I checked every finding below against the pinned `research/vte/src/{lib.rs,ansi.rs}` and alacritty `term/mod.rs`. None of them has been run as a test.

## Defects

**1. Absolute row ids can be reused after a large batch (`crates/shepr-vt/src/rows.rs`).**
- `row_signature` hashes only `cell.c`, so every blank or space-only row has the same signature.
- `finish()` relies on that signature to reject "same address, recycled slot" matches.
- Scenario: history is at its limit and one batch pushes more lines than the ring holds. The anchor's buffer address then reappears on another blank line, and the signature matches.
- Result: `evict` is undercounted modulo the ring size. That breaks the module's and `history_origin()`'s claim that an id "is never reused for another one".
- This is realistic: a synchronized-update frame is replayed in a single batch (`stop_sync` runs inside one `with_handler`), and vte buffers up to 2 MB. Output full of blank or identical lines, or clear-heavy TUI redraws, will hit it.
- Fix direction: don't use pointer identity plus a weak hash. Count evictions directly. Either have the handler observe `linefeed`/`scroll_up` at the history limit, or take a real content hash including flags and zerowidth. Better still is a stable per-row sequence number that the tracker owns. It is worth rewriting.

**2. modifyOtherKeys is applied out of order inside synchronized updates (`crates/shepr-vt/src/lib.rs`, `apply_scan_event`).**
- `ScanEvent::ModifyOtherKeys` writes `self.modes.modify_other_keys` immediately.
- The vte-dispatched forms (`CSI > 4 ; 0..2 m`) and RIS go through `handler.rs` and only land when the frame is replayed.
- Example: `BSU … CSI>4;2m … CSI>m … ESU` ends at level 2 (All) when it should be Off. Likewise, a scanner-set level followed by RIS in the same frame gets reset or not depending on buffering.
- This breaks the ordering contract that `handler.rs`'s module doc and `scan.rs` both rely on.
- Fix: do what `EraseScrollback` already does and feed a spelling vte dispatches (`\x1b[>4;0m` / `\x1b[>4;2m`) through `advance`, so the change is queued in byte order.

**3. `unicode_text_width` / `unicode_display_units` do not follow the grid's width rules (`crates/shepr-vt/src/cell.rs`).**
- Their docs claim "Width of text under the terminal grid's grapheme and voiced-mark rules". But alacritty sizes each char on its own (`c.width()`, with zero-width chars attached to the previous cell) and does no grapheme clustering.
- `unicode_grapheme_cell_width` uses `str::width()` over a whole grapheme, clamped to 2. Two mismatches:
  - `☺\u{FE0F}` is 1 column in the grid but reported as 2.
  - A ZWJ family emoji is 6 columns in the grid (2+0+2+0+2) but reported as 2.
- Consumers: `crates/shepr-termio/src/copy_mode.rs` (`first_non_blank_col`, `last_character_col`) measures row text that came from the grid, so copy-mode columns drift. `crates/shepr-client/src/shell/sidebar/agent_sidebar.rs` is also affected.
- Fix: sum per-char `unicode_codepoint_width`. Do not use grapheme width.

**4. `Terminal::mode_set` reports success for modes it cannot write (`crates/shepr-vt/src/lib.rs`).**
- `modes.rs` says "A number missing from the table is unsupported for both query and write".
- For an unlisted number, `mode_set` routes `PrivateMode::Unknown` to alacritty, which ignores it, and returns `Ok(())`.
- It should return `Err` when `modes::lookup` is `None`.

**5. Long OSC 7 / 9;9 / 1337 working-directory reports are dropped silently (`scan.rs`, `MAX_OSC_BYTES = 4096`).**
- A `file://host` + percent-encoded path near PATH_MAX (4096) goes over the cap. The report is discarded and the pane's cwd goes stale.
- The claim "tracks OSC 7" does not hold for long paths.
- The cap should be sized for PATH_MAX × 3 (percent-encoding) plus the prefix, or the scanner should only buffer up to the first `;` and then stream the payload for those commands.

## Minor mismatches between scanner and vte

- **XTGETTCAP body:** vte's passthrough ignores DEL and bytes 0x80–0xFF other than 0x9C. The scanner buffers them, so a request containing them gets no reply.
- **DCS ignore vs passthrough:** the scanner merges vte's `DcsIgnore` (which ignores 0x9C) with `DcsPassthrough` (which ends on 0x9C). I found no observable difference, because both only resync on ESC. The module doc's claim of "mirroring framing" is slightly overstated.

## Smells and other notes

- **`format.rs` OSC 8 id:** a child-supplied hyperlink id ending in `_alacritty` loses its id on replay. An id containing `:` or `;` is emitted raw, so it can corrupt the replayed OSC 8 params.
- **Mouse encoding modes:** setting 1005 cancels 1016, but setting 1016 does not cancel alacritty's `UTF8_MOUSE`. xterm has one extended-encoding variable for these, so the modelling is lopsided.
- **Scanner events inside a sync frame:** `WorkingDirectory` and `Progress` are applied as their bytes arrive, not at replay. That is harmless today, but it is the same class of problem as defect 2. A single "queue scanner effects into the parser stream" mechanism would remove the whole class. Only replies need to stay immediate, for the DA1-sentinel reason documented in `write`.
- **Checked and consistent, no action:** the `clear_screen` host action, the `resize` history-limit logic, `set_history_lines` title-event truncation, the kitty stack cap, the voiced-mark print path, and damage/`RenderState` generations.

Relevant files are all in `/home/folk/Programs/shepr/crates/shepr-vt/src/`: `rows.rs`, `lib.rs`, `cell.rs`, `scan.rs`, `modes.rs`, `format.rs` and `handler.rs`. The copy-mode consumer is `/home/folk/Programs/shepr/crates/shepr-termio/src/copy_mode.rs`.
