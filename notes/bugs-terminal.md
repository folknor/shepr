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

## TRM-006 - Scanner framing diverges from vte for XTGETTCAP and DCS

- **XTGETTCAP body:** vte's passthrough ignores DEL and bytes 0x80-0xFF other than 0x9C. The scanner buffers them, so a request containing them gets no reply.
- **DCS ignore vs passthrough:** the scanner merges vte's `DcsIgnore` (which ignores 0x9C) with `DcsPassthrough` (which ends on 0x9C). The hunter found no observable difference, because both only resync on ESC. The module doc's claim of "mirroring framing" is slightly overstated.

## TRM-009 - PTY input backpressure does not work; the actor's write queue is unbounded

`crates/shepr-pty/src/actor.rs`.
- The API promises backpressure: `ACTOR_COMMAND_BUFFER = 1024`, and `try_write_user_input` returns `TrySendError::Full` with the bytes handed back.
- In practice, `drain_data_commands` moves every queued command into `pending_writes` (an unbounded `VecDeque`) on every loop iteration, whether or not the PTY can take writes. The only time it holds back is while a submission is active.
- So `Full` is almost never returned. When a child stops reading stdin, pastes and keystrokes keep piling up in memory.
- Terminal responses are worse. `read_chunk` pushes each `on_read` result into `pending_writes` with no limit. A child that prints queries such as DA1/DSR/XTGETTCAP in a loop and never reads stdin grows server memory for as long as it runs. That is a server-wide memory exhaustion caused by one pane.
- Fix: make `pending_writes` the bounded queue. Stop draining `data_rx` once the queued bytes pass a limit, and cap or coalesce terminal responses. When a child is not reading, dropping or coalescing replies is the correct terminal behaviour.

Design note from the hunter: the actor has four synchronisation channels (tokio mpsc, std mpsc for control, a `Mutex<SharedPtyControls>`, a `response_order` mutex) plus a `UserWriteGate` mutex. A single mutex-protected inbox (bounded bytes, latest resize, shutdown flag) plus the wake pipe would remove the cross-channel ordering issues (this entry and TRM-014) and the unreachable `Full` case. That is a worthwhile rewrite.

## TRM-010 - One blocking-pool thread per pane for the child's whole life

`crates/shepr-mux/src/pane/runtime.rs`.
- `tokio::task::spawn_blocking(move || child.wait())` holds a thread from Tokio's blocking pool (512 by default) until the child exits.
- The same pool runs detection's `foreground_process_group_id` and `probe_foreground_process`, the synchronized-output flush, and the theme probe. With enough panes those tasks queue forever.
- Also likely: `Runtime` drop waits for blocking tasks unless `shutdown_timeout` or `shutdown_background` is used, so an exit path that leaves pane processes alive would hang the server on `wait()`. The hunter did not verify which runtime shutdown the server uses.
- Fix: reap from a pidfd. `ProcessHandle` already opens one: register it with `AsyncFd`, then `waitid(P_PIDFD)`. That avoids a dedicated thread per child.

## TRM-014 - Resize replies can be sent out of order relative to earlier replies

`crates/shepr-pty/src/actor.rs`, `apply_pending_controls`.
- A resize request's responses are queued before any `controls.terminal_responses` that were pushed earlier, for example an appearance report queued before the resize.
- `write_terminal_response` has a `response_order` lock precisely to keep replies ordered. Resize replies skip that ordering.

## TRM-015 - A failed resize is swallowed; PTY and emulator sizes can diverge

`crates/shepr-pty/src/actor.rs` `resize`, `fd.rs` `resize_pty_fd`.
- `clamp_pane_size` in runtime.rs says "the PTY and the emulator always agree on the size".
- `PaneRuntime::resize` resizes the emulator first, then asks the actor. If the ioctl fails, the actor logs at `debug!` and nothing else happens.
- Not exiting the process is correct. But the size contract then breaks silently, and `current_size` already holds the new value, so the next identical resize is skipped as a no-op and nothing retries.

## TRM-030 - passwd_field drops non-UTF-8 home and shell paths

`crates/shepr-pty/src/command.rs`, `passwd_field`. It converts the passwd entry with `CStr::to_str()`, so a non-UTF-8 home directory or login shell is discarded. `SHELL` and `HOME` from the environment now keep their raw bytes; the passwd path should too (`OsStr::from_bytes`).

## TRM-031 - The old-kernel descriptor-close fallback fails soft

`crates/shepr-pty/src/backend.rs`, pre-exec. When `close_range` is unavailable, descriptors are marked close-on-exec by walking `/proc/self/fd`. If procfs cannot be opened, the walk silently does nothing and the pane process inherits every server descriptor. Spawn should fail instead.

## TRM-020 - Kitty keys that shepr parses cannot be encoded again, so they are dropped

`crates/shepr-termio/src/input/encode.rs`, `parse.rs`.
- `kitty_codepoint_to_keycode` produces F13-F35, `CapsLock`/`ScrollLock`/`NumLock`/`PrintScreen`/`Pause`/`Menu`, `KeypadBegin`, `Media(..)` and `Modifier(..)` keys.
- `encode_kitty_functional_key` handles only arrows, Home/End/Ins/Del/PgUp/PgDn and F1-F12. For anything else `try_encode_csi_u` returns `None`, and `encode_legacy_inner` returns `vec![]` (as does `encode_f_key` for n>12).
- So a pane that pushed REPORT_ALL_KEYS never receives modifier-key or lock-key events, and F13+ is lost under every protocol. This breaks `encode_terminal_key`'s own doc: "Encode a key event for a PTY child using the pane's negotiated keyboard protocol."
- Related: keypad codepoints 57399-57426 are collapsed into `Char('0')` / `Up` and so on, so a REPORT_ALL_KEYS child can never see keypad identity. `TerminalKey` has nowhere to carry it. Caps Lock and Num Lock bits of the kitty modifier field are likewise dropped by `key_modifiers_from_u8`.
- Fix: keep the kitty functional codepoint in `TerminalKey`, and emit `CSI <cp>;mods[:ev]u` for it when REPORT_ALL_KEYS (or DISAMBIGUATE, for the keys the spec lists) is active.

Structural suggestion from the hunter: `TerminalKey` built on crossterm's `KeyCode` is the root of this entry and TRM-021: it can't represent kitty functional codepoints, keypad identity or lock state. A shepr-owned key model (kitty codepoint, shifted and base-layout alternates, a keypad flag, full modifier and lock bits) would make a lossless round trip possible. It would also let one encoder handle kitty, modifyOtherKeys and legacy output from the same data, replacing today's three layered fallbacks in `encode_terminal_key`. The US shifted-ASCII table is currently duplicated between `input/encode.rs` and `copy_mode.rs` (a parity test guards it); the key model would own it once.

## TRM-021 - modifyOtherKeys encoding is only half implemented

`crates/shepr-termio/src/input/encode.rs`, `encode_modify_other_keys`.
- `KeyEncodeModes::modify_other_keys` is documented as "xterm modifyOtherKeys level (0, 1 or 2)", and AGENTS.md says shepr tracks it through shepr-vt. But the encoder only handles Enter, Esc, Tab and Backspace.
- At level 2, xterm encodes every modified key, for example Ctrl+Shift+a as `CSI 27;6;97~`, Ctrl+1, Ctrl+. and Alt+letter. shepr falls back to legacy instead, so Ctrl+Shift+a reaches the child as `^A`. A child that asked for level 2 (neovim does) can't tell those chords apart.
- Level 1 also misses the "no well-known meaning" chords such as Ctrl+digit and Ctrl+Shift+letter.

## TRM-022 - Text-key lease rule assumes the host never sends REPORT_ALL_KEYS

`crates/shepr-termio/src/input/lease.rs`.
- `complete_press` returns `Ignore` without taking a lease for any key with `generated_text`. Its comment justifies this with "Without kitty REPORT_ALL_KEYS on the host ... a key that committed text gets no release event."
- But `host_term/modes.rs::set_host_kitty_keyboard_report_all(true)` pushes flags 31 (report-all plus associated text). In that mode every text key arrives as CSI u with associated text, gets `generated_text`, and does get Release and Repeat events.
- Those releases and repeats have no lease. So they aren't routed to the pane that got the press, and `plan_repeat` falls into untracked reprocessing.
- Needs confirming against the server-side caller. The rule should depend on the host mode, not on whether `generated_text` is present.

## TRM-027 - Selection highlight can land on the wrong rows without scroll metrics

`crates/shepr-termio/src/selection_render.rs`. When `scroll_metrics` is `None`, the viewport row is used as the absolute row. A selection stored in absolute coordinates will highlight the wrong rows whenever there is scrollback. This is only correct if the caller guarantees metrics are always present.

## TRM-028 - resolve_indexed_action second pass relies on matched_index for modifier correctness

`crates/shepr-termio/src/input/keybindings.rs`. With `exact_modifiers == false`, it accepts bindings whose normalized modifiers differ from the key's. Correctness then rests entirely on `IndexedKeybind::matched_index` rejecting modifier mismatches. That is fragile, and the hunter did not verify it in shepr-config.
