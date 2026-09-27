I reviewed crates/shepr-termio read-only. I read copy_mode.rs, selection_render.rs, blit.rs (up to the tests), host_term/modes.rs, and these files under input/: encode.rs, parse.rs, model.rs, mouse.rs, lease.rs, keybindings.rs, plus the non-test half of raw_input.rs. I did not open keybind_help.rs or host_term/{cell_size,theme,title}.rs. I also did not check callers in shepr-server/client, so findings that depend on a caller say so.

Copy mode in this crate is only helpers (column math, page size, command-char mapping). The copy-mode state machine and mouse selection live outside the crate. Here there is only selection highlight rendering.

## Defects, most important first

**1. One invalid byte stalls the input framer, then deletes the valid input queued behind it** (`input/raw_input.rs`).
- `extract_one_event` returns `None` in these cases:
  - a stray continuation byte or a 0xF8+ lead byte (`first_complete_utf8_char_len` gives `None`);
  - ESC followed by such a byte (`complete_escape_sequence_len` gives `None` via `utf8_char_width`);
  - a complete CSI that isn't UTF-8 (the `from_utf8(...).ok()?` at about line 969).
- `drain_available_chunks` then `break`s and everything typed afterwards waits behind that byte. When `flush_timeout` finally runs, parsing fails and `starts_with_incomplete_utf8_char` is false (because `error_len` is `Some`). It reaches "dropping incomplete raw input buffer" and runs `self.buffer.clear()`, which deletes every keystroke and mouse report typed after the bad byte.
- If input never goes idle (for example a stream of mouse motion), the stall has no end.
- Fix: whenever the head of the buffer can't start a valid event, consume exactly that one byte as `Unsupported` so the rest keeps flowing. Never clear the whole buffer.

**2. Kitty keys that shepr parses cannot be encoded again, so they are silently dropped** (`input/encode.rs`, `parse.rs`).
- `kitty_codepoint_to_keycode` produces F13–F35, `CapsLock`/`ScrollLock`/`NumLock`/`PrintScreen`/`Pause`/`Menu`, `KeypadBegin`, `Media(..)` and `Modifier(..)` keys.
- `encode_kitty_functional_key` handles only arrows, Home/End/Ins/Del/PgUp/PgDn and F1–F12. For anything else `try_encode_csi_u` returns `None`, and `encode_legacy_inner` returns `vec![]` (as does `encode_f_key` for n>12).
- So a pane that pushed REPORT_ALL_KEYS never receives modifier-key or lock-key events, and F13+ is lost under every protocol. This breaks `encode_terminal_key`'s own doc: "Encode a key event for a PTY child using the pane's negotiated keyboard protocol."
- Related: keypad codepoints 57399–57426 are collapsed into `Char('0')` / `Up` and so on, so a REPORT_ALL_KEYS child can never see keypad identity. `TerminalKey` has nowhere to carry it.
- Fix: keep the kitty functional codepoint in `TerminalKey`, and emit `CSI <cp>;mods[:ev]u` for it when REPORT_ALL_KEYS (or DISAMBIGUATE, for the keys the spec lists) is active.

**3. modifyOtherKeys is only half implemented** (`encode.rs`, `encode_modify_other_keys`).
- `KeyEncodeModes::modify_other_keys` is documented as "xterm modifyOtherKeys level (0, 1 or 2)", and AGENTS.md says shepr tracks it through shepr-vt. But the encoder only handles Enter, Esc, Tab and Backspace.
- At level 2, xterm encodes every modified key, for example Ctrl+Shift+a as `CSI 27;6;97~`, Ctrl+1, Ctrl+. and Alt+letter. shepr falls back to legacy instead, so Ctrl+Shift+a reaches the child as `^A`. A child that asked for level 2 (neovim does) can't tell those chords apart.
- Level 1 also misses the "no well-known meaning" chords such as Ctrl+digit and Ctrl+Shift+letter.

**4. The text-key lease rule assumes the host never sends REPORT_ALL_KEYS, which shepr itself turns on** (`input/lease.rs`).
- `complete_press` returns `Ignore` without taking a lease for any key with `generated_text`. Its comment justifies this with "Without kitty REPORT_ALL_KEYS on the host … a key that committed text gets no release event."
- But `host_term/modes.rs::set_host_kitty_keyboard_report_all(true)` pushes flags 31 (report-all plus associated text). In that mode every text key arrives as CSI u with associated text, gets `generated_text`, and does get Release and Repeat events.
- Those releases and repeats have no lease. So they aren't routed to the pane that got the press, and `plan_repeat` falls into untracked reprocessing.
- This needs confirming against the server-side caller. The rule should depend on the host mode, not on whether `generated_text` is present.

**5. `set_host_kitty_keyboard_report_all` always pops before it pushes** (`host_term/modes.rs`).
- Its own test shows the very first call emitting `\x1b[<1u\x1b[>31u`. If nothing earlier in the client pushed a shepr entry, that pop removes the enclosing program's kitty stack entry, for example an outer shepr or tmux on a nested/SSH host.
- The test name says it "replaces the current shepr stack entry". That only holds if the caller always pushed first. I haven't verified the caller.
- `set_direct_host_keyboard_protocol` handles this properly by tracking `active` state. The two paths should be unified on that pattern.

**6. Legacy Ctrl encoding is incomplete** (`encode_legacy_inner`).
- Ctrl+`?` and Ctrl+`8` send the plain character instead of DEL (0x7f), unlike xterm.
- Shift plus a digit or punctuation key with no `shifted_codepoint` sends the unshifted character (`shifted_text_char` returns `None`, then `unwrap_or(ch)`). Meanwhile `copy_mode::copy_mode_command_char` maps the same key through a US table (`'1'` becomes `'!'`). The two layers disagree on what the key is.

**7. Smaller issues**
- **UTF-8 mouse encoding (1005) has no upper limit** (`encode_mouse_cb` Utf8 branch). xterm caps it at 2015. Past that it emits 3-byte code points, and in the surrogate range the event is silently dropped.
- **`parse_kitty_key_sequence` reads the modifier field as `u8`.** A value of 256 (every modifier and lock bit set) makes the whole key `Unsupported` instead of being clamped.
- **Selection highlight can land on the wrong rows** (`selection_render.rs`). When `scroll_metrics` is `None`, the viewport row is used as the absolute row. A selection stored in absolute coordinates will highlight the wrong rows whenever there is scrollback. This is only correct if the caller guarantees metrics are always present.
- **`resolve_indexed_action` second pass** (`keybindings.rs`). With `exact_modifiers == false`, it accepts bindings whose normalized modifiers differ from the key's. Correctness then rests entirely on `IndexedKeybind::matched_index` rejecting modifier mismatches. That is fragile, and I didn't verify it in shepr-config.

## Checked and correct
- Kitty CSI u modifier, event and associated-text composition, and release suppression.
- DECCKM rewrite and SGR release button codes.
- Mouse mode filtering (X10, 1000, 1002, 1003).
- Pixel-to-cell mapping in `mouse.rs`.
- Paste stall and size limits.
- Wide-cell invalidation in the blit diff.
- Hyperlink sanitising.

## Structural suggestion
`TerminalKey` built on crossterm's `KeyCode` is the root of findings 2 and 3: it can't represent kitty functional codepoints, keypad identity or lock state. A shepr-owned key model (kitty codepoint, shifted and base-layout alternates, a keypad flag, full modifier and lock bits) would make a lossless round trip possible. It would also let one encoder handle kitty, modifyOtherKeys and legacy output from the same data, replacing today's three layered fallbacks in `encode_terminal_key`.
