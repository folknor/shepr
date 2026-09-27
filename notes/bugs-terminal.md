# Defects: shepr-vt, shepr-pty, shepr-termio

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

Hunter coverage: shepr-vt findings were checked against the pinned vte and alacritty sources but not run. shepr-pty was read including the reap/teardown path in shepr-mux. shepr-termio: `keybind_help.rs` and `host_term/{cell_size,theme,title}.rs` were not read, and callers in shepr-server/client were not checked.

## TRM-001 - Absolute row ids can be reused after a large batch

`crates/shepr-vt/src/rows.rs`.
- `row_signature` hashes only `cell.c`, so every blank or space-only row has the same signature.
- `finish()` relies on that signature to reject "same address, recycled slot" matches.
- Scenario: history is at its limit and one batch pushes more lines than the ring holds. The anchor's buffer address then reappears on another blank line, and the signature matches.
- Result: `evict` is undercounted modulo the ring size. That breaks the module's and `history_origin()`'s claim that an id "is never reused for another one".
- This is realistic: a synchronized-update frame is replayed in a single batch (`stop_sync` runs inside one `with_handler`), and vte buffers up to 2 MB. Output full of blank or identical lines, or clear-heavy TUI redraws, will hit it.
- Fix direction: don't use pointer identity plus a weak hash. Count evictions directly. Either have the handler observe `linefeed`/`scroll_up` at the history limit, or take a real content hash including flags and zerowidth. Better still is a stable per-row sequence number that the tracker owns. It is worth rewriting.

## TRM-010 - One blocking-pool thread per pane for the child's whole life

`crates/shepr-mux/src/pane/runtime.rs`, child watcher.
- `tokio::task::spawn_blocking(move || child.wait())` holds a thread from Tokio's blocking pool (512 by default) until the child exits.
- The same pool runs detection's `foreground_process_group_id` and `probe_foreground_process`, the synchronized-output flush, and the theme probe. With enough panes those tasks queue forever.
- The server runtime already shuts down with a 100 ms timeout (`server/headless/bootstrap.rs`), so a live waiter cannot hang shutdown; the cost is the pool thread per pane.
- Fix: reap from a pidfd. `ProcessHandle` already opens one, but `ProcessHandle::pidfd()` is private to `shepr-platform/src/process.rs`. Add an owned pidfd readiness API there (with a fallback for handles without pidfds), register it with `AsyncFd`, then `waitid(P_PIDFD)` in the watcher (a comment at the watcher records this). The fixer needs both files.

## TRM-020 - Kitty keys that shepr parses cannot be encoded again, so they are dropped

`crates/shepr-termio/src/input/encode.rs`, `parse.rs`.
- `kitty_codepoint_to_keycode` produces F13-F35, `CapsLock`/`ScrollLock`/`NumLock`/`PrintScreen`/`Pause`/`Menu`, `KeypadBegin`, `Media(..)` and `Modifier(..)` keys.
- `encode_kitty_functional_key` handles only arrows, Home/End/Ins/Del/PgUp/PgDn and F1-F12. For anything else `try_encode_csi_u` returns `None`, and `encode_legacy_inner` returns `vec![]` (as does `encode_f_key` for n>12).
- So a pane that pushed REPORT_ALL_KEYS never receives modifier-key or lock-key events, and F13+ is lost under every protocol. This breaks `encode_terminal_key`'s own doc: "Encode a key event for a PTY child using the pane's negotiated keyboard protocol."
- Related: keypad codepoints 57399-57426 are collapsed into `Char('0')` / `Up` and so on, so a REPORT_ALL_KEYS child can never see keypad identity. `TerminalKey` has nowhere to carry it. Caps Lock and Num Lock bits of the kitty modifier field are likewise dropped by `key_modifiers_from_u8`.
- Fix: keep the kitty functional codepoint in `TerminalKey`, and emit `CSI <cp>;mods[:ev]u` for it when REPORT_ALL_KEYS (or DISAMBIGUATE, for the keys the spec lists) is active.

Structural suggestion from the hunter: `TerminalKey` built on crossterm's `KeyCode` is the root of this entry and TRM-021: it can't represent kitty functional codepoints, keypad identity or lock state. A shepr-owned key model (kitty codepoint, shifted and base-layout alternates, a keypad flag, full modifier and lock bits) would make a lossless round trip possible. It would also let one encoder handle kitty, modifyOtherKeys and legacy output from the same data, replacing today's three layered fallbacks in `encode_terminal_key`.

## TRM-021 - modifyOtherKeys encoding is only half implemented

`crates/shepr-termio/src/input/encode.rs`, `encode_modify_other_keys`.
- `KeyEncodeModes::modify_other_keys` is documented as "xterm modifyOtherKeys level (0, 1 or 2)", and AGENTS.md says shepr tracks it through shepr-vt. But the encoder only handles Enter, Esc, Tab and Backspace.
- At level 2, xterm encodes every modified key, for example Ctrl+Shift+a as `CSI 27;6;97~`, Ctrl+1, Ctrl+. and Alt+letter. shepr falls back to legacy instead, so Ctrl+Shift+a reaches the child as `^A`. A child that asked for level 2 (neovim does) can't tell those chords apart.
- Level 1 also misses the "no well-known meaning" chords such as Ctrl+digit and Ctrl+Shift+letter.
- When this is reworked: in a pane that asked for report-all, raw IME text arrives with generated text and takes a forwarded lease, so losing focus sends that pane one extra (matched) release event. Harmless today; the key model should decide it explicitly.
