# Defects: shepr-vt, shepr-pty, shepr-termio

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

Hunter coverage: shepr-vt findings were checked against the pinned `research/vte` and alacritty sources but not run. shepr-pty was read including the reap/teardown path in shepr-mux. shepr-termio: `keybind_help.rs` and `host_term/{cell_size,theme,title}.rs` were not read, and callers in shepr-server/client were not checked.

## TRM-001 - Absolute row ids can be reused after a large batch

`crates/shepr-vt/src/rows.rs`.
- `row_signature` hashes only `cell.c`, so every blank or space-only row has the same signature.
- `finish()` relies on that signature to reject "same address, recycled slot" matches.
- Scenario: history is at its limit and one batch pushes more lines than the ring holds. The anchor's buffer address then reappears on another blank line, and the signature matches.
- Result: `evict` is undercounted modulo the ring size. That breaks the module's and `history_origin()`'s claim that an id "is never reused for another one".
- This is realistic: a synchronized-update frame is replayed in a single batch (`stop_sync` runs inside one `with_handler`), and vte buffers up to 2 MB. Output full of blank or identical lines, or clear-heavy TUI redraws, will hit it.
- Fix direction: don't use pointer identity plus a weak hash. Count evictions directly. Either have the handler observe `linefeed`/`scroll_up` at the history limit, or take a real content hash including flags and zerowidth. Better still is a stable per-row sequence number that the tracker owns. It is worth rewriting.

## TRM-002 - modifyOtherKeys is applied out of order inside synchronized updates

`crates/shepr-vt/src/lib.rs`, `apply_scan_event`.
- `ScanEvent::ModifyOtherKeys` writes `self.modes.modify_other_keys` immediately.
- The vte-dispatched forms (`CSI > 4 ; 0..2 m`) and RIS go through `handler.rs` and only land when the frame is replayed.
- Example: `BSU … CSI>4;2m … CSI>m … ESU` ends at level 2 (All) when it should be Off. Likewise, a scanner-set level followed by RIS in the same frame gets reset or not depending on buffering.
- This breaks the ordering contract that `handler.rs`'s module doc and `scan.rs` both rely on.
- Fix: do what `EraseScrollback` already does and feed a spelling vte dispatches (`\x1b[>4;0m` / `\x1b[>4;2m`) through `advance`, so the change is queued in byte order.

Same class: `WorkingDirectory` and `Progress` scanner events inside a sync frame are applied as their bytes arrive, not at replay. That is harmless today. A single "queue scanner effects into the parser stream" mechanism would remove the whole class; only replies need to stay immediate, for the DA1-sentinel reason documented in `write`.

## TRM-003 - unicode_text_width / unicode_display_units do not follow the grid's width rules

`crates/shepr-vt/src/cell.rs`.
- Their docs claim "Width of text under the terminal grid's grapheme and voiced-mark rules". But alacritty sizes each char on its own (`c.width()`, with zero-width chars attached to the previous cell) and does no grapheme clustering.
- `unicode_grapheme_cell_width` uses `str::width()` over a whole grapheme, clamped to 2. Two mismatches:
  - `\u{263A}\u{FE0F}` (smiley plus VS16) is 1 column in the grid but reported as 2.
  - A ZWJ family emoji is 6 columns in the grid (2+0+2+0+2) but reported as 2.
- Consumers: `crates/shepr-termio/src/copy_mode.rs` (`first_non_blank_col`, `last_character_col`) measures row text that came from the grid, so copy-mode columns drift. `crates/shepr-client/src/shell/sidebar/agent_sidebar.rs` is also affected.
- Fix: sum per-char `unicode_codepoint_width`. Do not use grapheme width.

## TRM-004 - Terminal::mode_set reports success for modes it cannot write

`crates/shepr-vt/src/lib.rs`.
- `modes.rs` says "A number missing from the table is unsupported for both query and write".
- For an unlisted number, `mode_set` routes `PrivateMode::Unknown` to alacritty, which ignores it, and returns `Ok(())`.
- It should return `Err` when `modes::lookup` is `None`.

## TRM-005 - Long OSC 7 / 9;9 / 1337 working-directory reports are dropped silently

`crates/shepr-vt/src/scan.rs`, `MAX_OSC_BYTES = 4096`.
- A `file://host` + percent-encoded path near PATH_MAX (4096) goes over the cap. The report is discarded and the pane's cwd goes stale.
- The claim "tracks OSC 7" does not hold for long paths.
- The cap should be sized for PATH_MAX × 3 (percent-encoding) plus the prefix, or the scanner should only buffer up to the first `;` and then stream the payload for those commands.

## TRM-006 - Scanner framing diverges from vte for XTGETTCAP and DCS

- **XTGETTCAP body:** vte's passthrough ignores DEL and bytes 0x80-0xFF other than 0x9C. The scanner buffers them, so a request containing them gets no reply.
- **DCS ignore vs passthrough:** the scanner merges vte's `DcsIgnore` (which ignores 0x9C) with `DcsPassthrough` (which ends on 0x9C). The hunter found no observable difference, because both only resync on ESC. The module doc's claim of "mirroring framing" is slightly overstated.

## TRM-007 - OSC 8 hyperlink ids are mangled on replay

`crates/shepr-vt/src/format.rs`: a child-supplied hyperlink id ending in `_alacritty` loses its id on replay. An id containing `:` or `;` is emitted raw, so it can corrupt the replayed OSC 8 params.

## TRM-008 - Mouse extended-encoding modes are modelled asymmetrically

Setting 1005 cancels 1016, but setting 1016 does not cancel alacritty's `UTF8_MOUSE`. xterm has one extended-encoding variable for these, so the modelling is lopsided.

## TRM-009 - PTY input backpressure does not work; the actor's write queue is unbounded

`crates/shepr-pty/src/actor.rs`.
- The API promises backpressure: `ACTOR_COMMAND_BUFFER = 1024`, and `try_write_user_input` returns `TrySendError::Full` with the bytes handed back.
- In practice, `drain_data_commands` moves every queued command into `pending_writes` (an unbounded `VecDeque`) on every loop iteration, whether or not the PTY can take writes. The only time it holds back is while a submission is active.
- So `Full` is almost never returned. When a child stops reading stdin, pastes and keystrokes keep piling up in memory.
- Terminal responses are worse. `read_chunk` pushes each `on_read` result into `pending_writes` with no limit. A child that prints queries such as DA1/DSR/XTGETTCAP in a loop and never reads stdin grows server memory for as long as it runs. That is a server-wide memory exhaustion caused by one pane.
- Fix: make `pending_writes` the bounded queue. Stop draining `data_rx` once the queued bytes pass a limit, and cap or coalesce terminal responses. When a child is not reading, dropping or coalescing replies is the correct terminal behaviour.

Design note from the hunter: the actor has four synchronisation channels (tokio mpsc, std mpsc for control, a `Mutex<SharedPtyControls>`, a `response_order` mutex) plus a `UserWriteGate` mutex. A single mutex-protected inbox (bounded bytes, latest resize, shutdown flag) plus the wake pipe would remove the cross-channel ordering issues (this entry and TRM-014) and the unreachable `Full` case. That is a worthwhile rewrite.

## TRM-010 - One blocking-pool thread per pane for the child's whole life

`crates/shepr-mux/src/pane/runtime.rs`, around lines 453-476.
- `tokio::task::spawn_blocking(move || child.wait())` holds a thread from Tokio's blocking pool (512 by default) until the child exits.
- The same pool runs detection's `foreground_process_group_id` and `probe_foreground_process`, the synchronized-output flush, and the theme probe. With enough panes those tasks queue forever.
- Also likely: `Runtime` drop waits for blocking tasks unless `shutdown_timeout` or `shutdown_background` is used. Any exit path that keeps pane processes alive (`preserve_processes_on_drop`) would then hang the server on `wait()`. That is exactly the "blocks waiting for the child" problem this crate claims to avoid, just moved elsewhere. The hunter did not verify which runtime shutdown the server uses.
- Fix: reap from a pidfd. `ProcessHandle` already opens one: register it with `AsyncFd`, then `waitid(P_PIDFD)`. That avoids a dedicated thread per child.

Related: MUX-004 reports that `preserve_processes_on_drop` is `false` in every production constructor.

## TRM-011 - PaneDied from a reader panic says ChildExitReason::Exited

`crates/shepr-mux/src/pane/runtime.rs`, around line 627-629. The child did not exit; it may still be alive. The type's own name is misused, and anything that branches on the exit reason (restore, UI) is told something false.

Surfaced in both the pty and mux scopes.

## TRM-012 - If PtyIoActor::spawn fails, the child is orphaned

`crates/shepr-mux/src/pane/runtime.rs`, around lines 453 and 639-648.
- The child watcher is started before the actor. If `PtyIoActor::spawn(...)?` fails (wake pipe, fcntl, or thread creation), `spawn_command_builder` returns `Err`.
- `master_fd` is dropped, so the child gets SIGHUP, but `shutdown_pane_processes` never runs. A child or session member that ignores SIGHUP keeps running, and the watcher later sends `PaneDied` for a pane that never existed.
- The partial-failure path skips the teardown contract. Fix: create the actor, or at least the wake pipe and fds, before spawning, or run teardown on the error path.

## TRM-013 - POLLERR throws away the child's last output

`crates/shepr-pty/src/fd.rs`, `poll_pty_and_wake`.
- The actor itself claims that a child's last output is drained before the loop ends (`handle_write_failure` documents this).
- But `POLLERR` on the PTY fd returns `Err`, and `run()` then breaks with no drain.
- The hunter believes Linux pty masters usually report `POLLHUP`/`EIO` rather than `POLLERR`, so this may be rare. Still, it is the one exit path that breaks the drain guarantee. Drain here the same way as on a write failure.

## TRM-014 - Resize replies can be sent out of order relative to earlier replies

`crates/shepr-pty/src/actor.rs`, `apply_pending_controls`.
- A resize request's responses are queued before any `controls.terminal_responses` that were pushed earlier, for example an appearance report queued before the resize.
- `write_terminal_response` has a `response_order` lock precisely to keep replies ordered. Resize replies skip that ordering.

## TRM-015 - A failed resize is swallowed; PTY and emulator sizes can diverge

`crates/shepr-pty/src/actor.rs` `resize`, `fd.rs` `resize_pty_fd`.
- `clamp_pane_size` in runtime.rs says "the PTY and the emulator always agree on the size".
- `PaneRuntime::resize` resizes the emulator first, then asks the actor. If the ioctl fails, the actor logs at `debug!` and nothing else happens.
- Not exiting the process is correct. But the size contract then breaks silently, and `current_size` already holds the new value, so the next identical resize is skipped as a no-op and nothing retries.

## TRM-016 - Pre-exec signal reset list is narrow

`crates/shepr-pty/src/backend.rs` `prepare_pty_child`. It resets only SIGCHLD/HUP/INT/QUIT/TERM/ALRM/PIPE. An ignored SIGTSTP/SIGTTOU/SIGTTIN/SIGUSR* in the server (from a parent or a library) would leak into every pane. Resetting all signals 1..NSIG to `SIG_DFL` is cheap and matches the "clear inherited state" intent.

## TRM-017 - PtyCommand cwd and SHELL handling diverge from their docs

- **Doc wording on `PtyCommand::cwd`.** It says a missing or non-directory path "falls back to HOME". HOME itself is not checked: a relative or missing HOME makes `spawn()` fail with a bare chdir error. The check also runs in the parent (time-of-check/time-of-use gap, harmless).
- **Non-UTF-8 `SHELL` is silently ignored.** `resolve_shell` uses `OsStr::to_str`, so in pane mode a non-UTF-8 `SHELL` quietly becomes `/bin/sh`, which contradicts "reject an invalid selected shell".

## TRM-018 - Small pty smells

- **Missing SAFETY comments in `command.rs`.** The three `unsafe` blocks in `passwd_field` and `access_ok` have none, although every other unsafe block in the crate is documented.
- **A burst of wakes delays PTY IO.** When `wake_ready` fires, `run()` does `continue` even if the PTY was also readable or writable, so a steady stream of wakes (keystrokes, timer responses) postpones PTY work. There is no correctness bug, but reads could be serviced in the same iteration.
- **Missed wakes are only caught by the 1 s idle poll.** The fallback is documented, but wake writes that hit `EAGAIN` are treated as success, which is correct only because the pipe is non-empty at that point. The hunter says that holds as written.

## TRM-029 - The raw-input idle flush still ends in a catch-all buffer clear

`crates/shepr-termio/src/input/raw_input.rs`, `RawInputByteFramer::flush_timeout`. Malformed heads are now consumed one event at a time, so by the idle flush only a single incomplete trailing sequence should remain. The flush still finishes with `self.buffer.clear()`, which would silently eat anything else if that invariant ever slips. It should drop exactly the incomplete sequence, or assert the invariant in tests.

## TRM-020 - Kitty keys that shepr parses cannot be encoded again, so they are dropped

`crates/shepr-termio/src/input/encode.rs`, `parse.rs`.
- `kitty_codepoint_to_keycode` produces F13-F35, `CapsLock`/`ScrollLock`/`NumLock`/`PrintScreen`/`Pause`/`Menu`, `KeypadBegin`, `Media(..)` and `Modifier(..)` keys.
- `encode_kitty_functional_key` handles only arrows, Home/End/Ins/Del/PgUp/PgDn and F1-F12. For anything else `try_encode_csi_u` returns `None`, and `encode_legacy_inner` returns `vec![]` (as does `encode_f_key` for n>12).
- So a pane that pushed REPORT_ALL_KEYS never receives modifier-key or lock-key events, and F13+ is lost under every protocol. This breaks `encode_terminal_key`'s own doc: "Encode a key event for a PTY child using the pane's negotiated keyboard protocol."
- Related: keypad codepoints 57399-57426 are collapsed into `Char('0')` / `Up` and so on, so a REPORT_ALL_KEYS child can never see keypad identity. `TerminalKey` has nowhere to carry it.
- Fix: keep the kitty functional codepoint in `TerminalKey`, and emit `CSI <cp>;mods[:ev]u` for it when REPORT_ALL_KEYS (or DISAMBIGUATE, for the keys the spec lists) is active.

Structural suggestion from the hunter: `TerminalKey` built on crossterm's `KeyCode` is the root of this entry and TRM-021: it can't represent kitty functional codepoints, keypad identity or lock state. A shepr-owned key model (kitty codepoint, shifted and base-layout alternates, a keypad flag, full modifier and lock bits) would make a lossless round trip possible. It would also let one encoder handle kitty, modifyOtherKeys and legacy output from the same data, replacing today's three layered fallbacks in `encode_terminal_key`.

## TRM-021 - modifyOtherKeys encoding is only half implemented

`crates/shepr-termio/src/input/encode.rs`, `encode_modify_other_keys`.
- `KeyEncodeModes::modify_other_keys` is documented as "xterm modifyOtherKeys level (0, 1 or 2)", and AGENTS.md says shepr tracks it through shepr-vt. But the encoder only handles Enter, Esc, Tab and Backspace.
- At level 2, xterm encodes every modified key, for example Ctrl+Shift+a as `CSI 27;6;97~`, Ctrl+1, Ctrl+. and Alt+letter. shepr falls back to legacy instead, so Ctrl+Shift+a reaches the child as `^A`. A child that asked for level 2 (neovim does) can't tell those chords apart.
- Level 1 also misses the "no well-known meaning" chords such as Ctrl+digit and Ctrl+Shift+letter.

## TRM-022 - Text-key lease rule assumes the host never sends REPORT_ALL_KEYS

`crates/shepr-termio/src/input/lease.rs`.
- `complete_press` returns `Ignore` without taking a lease for any key with `generated_text`. Its comment justifies this with "Without kitty REPORT_ALL_KEYS on the host … a key that committed text gets no release event."
- But `host_term/modes.rs::set_host_kitty_keyboard_report_all(true)` pushes flags 31 (report-all plus associated text). In that mode every text key arrives as CSI u with associated text, gets `generated_text`, and does get Release and Repeat events.
- Those releases and repeats have no lease. So they aren't routed to the pane that got the press, and `plan_repeat` falls into untracked reprocessing.
- Needs confirming against the server-side caller. The rule should depend on the host mode, not on whether `generated_text` is present.

## TRM-024 - Legacy Ctrl/Shift encoding is incomplete and disagrees with copy mode

`encode_legacy_inner`.
- Ctrl+`?` and Ctrl+`8` send the plain character instead of DEL (0x7f), unlike xterm.
- Shift plus a digit or punctuation key with no `shifted_codepoint` sends the unshifted character (`shifted_text_char` returns `None`, then `unwrap_or(ch)`). Meanwhile `copy_mode::copy_mode_command_char` maps the same key through a US table (`'1'` becomes `'!'`). The two layers disagree on what the key is.

## TRM-025 - UTF-8 mouse encoding (1005) has no upper limit

`encode_mouse_cb` Utf8 branch. xterm caps it at 2015. Past that it emits 3-byte code points, and in the surrogate range the event is silently dropped.

## TRM-026 - Kitty modifier field parsed as u8

`parse_kitty_key_sequence` reads the modifier field as `u8`. A value of 256 (every modifier and lock bit set) makes the whole key `Unsupported` instead of being clamped.

## TRM-027 - Selection highlight can land on the wrong rows without scroll metrics

`crates/shepr-termio/src/selection_render.rs`. When `scroll_metrics` is `None`, the viewport row is used as the absolute row. A selection stored in absolute coordinates will highlight the wrong rows whenever there is scrollback. This is only correct if the caller guarantees metrics are always present.

## TRM-028 - resolve_indexed_action second pass relies on matched_index for modifier correctness

`crates/shepr-termio/src/input/keybindings.rs`. With `exact_modifiers == false`, it accepts bindings whose normalized modifiers differ from the key's. Correctness then rests entirely on `IndexedKeybind::matched_index` rejecting modifier mismatches. That is fragile, and the hunter did not verify it in shepr-config.
