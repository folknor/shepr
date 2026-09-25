## Terminal core defect hunt: src/ghostty, src/pty, terminal_* modules

I read everything in scope, then followed the values into `src/pane/terminal.rs`, `src/pane/osc.rs`, `src/pane.rs` and the pinned alacritty and vte sources. I built and ran nothing. Findings are ordered by severity.

### 1. HIGH: A child's output can panic the pane's reader thread (bug in pinned alacritty, reachable because shepr enables kitty keyboard)
`research/alacritty/alacritty_terminal/src/term/mod.rs:1295-1301`, in `push_keyboard_mode`:
```rust
if self.keyboard_mode_stack.len() >= KEYBOARD_MODE_STACK_MAX_DEPTH {
    let removed = self.title_stack.remove(0);
```
- It removes from the **title** stack, not the keyboard stack.
- `term_config` sets `kitty_keyboard: true` (`src/ghostty/mod.rs:426`).
- So 4096 × `CSI > 1 u` with no title pushed (about 20 KB, e.g. `cat` of a file or a buggy app) panics with "removal index 0 < len 0". If titles were pushed, the keyboard stack grows without bound instead.
- The panic happens inside `on_read` on the actor thread while it holds the core, `content_write_lock` and `response_order` mutexes. The thread dies, the master fd drops (the child gets SIGHUP) and every lock is poisoned. The pane goes silently dead; `on_reader_exit` is `None` in `src/pane.rs:1354`, so nothing reports it.
- Fix: intercept it in the adapter (see #3).

### 2. HIGH: Host-side state is injected through the child's parser mid-stream
These all call `Terminal::write` (`src/ghostty/mod.rs:561`):
- `write_host_default_color` (`src/pane/osc.rs:739-750`), which writes OSC 10/11/110/111 for the host theme;
- `Terminal::mode_set` (`mod.rs:1025`);
- the resize replay in `GhosttyPaneTerminal::resize` (`src/pane/terminal.rs:1418-1423`).

`Terminal::write` shares the vte parser state, the scanner state, `held_utf8` and vte's sync buffer with the child's byte stream. PTY reads are 8 KiB chunks and routinely split escape sequences.

Triggers:
- `maybe_restore_host_terminal_theme`, run from the detection task at any moment;
- `apply_host_terminal_theme` when the host theme changes;
- resizes.

Effects:
- If the child's CSI or OSC is incomplete, the injected ESC aborts it and the child's tail prints as text.
- A held `EF` / `EF BE` gets prepended to the injected ESC, printing U+FFFD (plus a second U+FFFD for the stray continuation bytes).
- During a child sync update (2026), the theme change is buffered until ESU or the 150 ms timeout.

The host fg/bg and the child's OSC 10/11 share one alacritty slot (`colors[Foreground/Background]`). That forces the whole "transient owner" and re-inject machinery in `osc.rs`.

Structural fix:
- Add `set_default_colors(fg, bg)` to the adapter, like `set_default_palette`.
- Resolve child override → host default → built-in in `render_colors` / `core_query_color`, and never write bytes for host state.
- OSC 110/111 then fall back to the host default for free, and `apply_cached_host_default_color` plus most of the owner-pgid tracking can go.

`restore_host_terminal_theme_reapplies_cached_colors` passes only because it writes into an idle terminal.

### 3. MEDIUM: Scanner effects ignore vte's synchronized-update buffering, breaking the "ordered query replies" claim (`mod.rs:16`)
During a 2026 update, `Processor::advance` only buffers (`research/vte/src/ansi.rs:298-387`). The scanner's events are still applied immediately (`mod.rs:582-593`), so:
- **Reply order:** XTGETTCAP, `CSI 16 t`, `?996n` and 2048 reports go out before DA/DSR/DECRQM replies that came earlier in the frame.
- **Mode order:** RIS resets `ExtraModes` before alacritty's RIS runs. Example: `BSU ?1000h ?9h ESU` leaves X10 on and 1000 set, so `encode_mouse_event` picks PressRelease.

Structural fix: wrap `Term` in a `Handler` that delegates everything and intercepts what vte already dispatches in byte order and sync-aware:
- `set_private_mode` / `unset_private_mode` for `Unknown(9|1016|2031|2048)`;
- `report_private_mode`, which replaces the string-parsing `filter_core_reply`;
- `reset_state`;
- `set_modify_other_keys` / `report_modify_other_keys`;
- `push_keyboard_mode` (fixes #1);
- `input(c)` for U+FF9E/U+FF9F. This removes `held_utf8`, `voiced_mark_prefix_len` and `input_halfwidth_voiced_mark`, and fixes the documented "mark folds inside sync" gap.

After that, the scanner is only needed for OSC 7/9;9/1337, `CSI ?996n`, `CSI 16 t`, XTGETTCAP and `CSI ?3J`.

### 4. MEDIUM: Effects of a timed-out sync flush on non-read paths are stranded or dropped
Render, `collect_dirty_patch` and `synchronized_output_state` call `flush_expired_synchronized_output` (`pane/terminal.rs:2178`). This runs the buffered bytes through the parser, and:
- **Replies** sit in `Terminal.responses` until the next PTY read. A child that sent a query inside an unterminated update and is waiting for the answer hangs.
- **Bells and OSC 52 clipboard writes** from that frame are thrown away by the discard at the start of `process_pty_bytes` (`pane/terminal.rs:1259-1263`).
- **Resize coalescing:** `resize` drains all pending replies into the actor's resize slot, which the next resize overwrites (`pty/actor.rs:196`).

### 5. MEDIUM: Widening a pane permanently truncates its scrollback
`scrollback_lines` divides the byte budget by the column count. `resize` shrinks history via `update_history`, which drops lines (`mod.rs:956-967`; `grid/mod.rs:154`).
- A zoom/unzoom cycle, or attaching from a wider client (which resizes every pane), throws away history that fit the budget at the old width, and it never comes back.
- This contradicts the comment "so a resize never truncates content it can keep".
- Separately, `MIN_SCROLLBACK_LINES` (1000) breaks the `MAX_SCROLLBACK_LINES` comment's claim that "the byte budget already bounds memory". A small budget on a wide pane gets 1000 full-width lines.

### 6. LOW: Other defects
- **`CSI 18 t` suppressed without pixel geometry** (`filter_core_reply`, `mod.rs:810`). That report is in characters and needs no pixels. `window_size_reports_need_pixel_geometry` enshrines the wrong behaviour.
- **Raw C1 `0x90`** (`scan.rs:255`):
  - vte only handles 7-bit controls (`research/vte/src/lib.rs:24`): it executes `0x90` as a no-op and **prints** the payload. So an 8-bit XTGETTCAP gets a reply and also leaves `+q…` text on screen.
  - Any stray `0x90` (binary or Latin-1 output) puts the scanner into DCS state until the next ESC.
  - Both the scan.rs module-doc claim that the scanner agrees with the core about framing and the comment "the core ignores it" are false.
- **`PtyCommand::to_std_command` overwrites the resolved `SHELL`** (`pty/command.rs:143-145`): `cmd.env("SHELL", shell)` is followed by `cmd.envs(&self.envs)`, and `envs` always contains the seeded `SHELL`. The child never sees the executable/passwd fallback that `shell()` computes.
- **Possible u16 overflow in the `CSI 14 t` reply:** `TextAreaSizeRequest` saturates the cell sizes to u16 separately, but alacritty's closure then multiplies `num_lines * cell_height` in u16 (`term/mod.rs:2261`). With large client-reported cell sizes this wraps in release and panics in debug, under the core lock. Unlikely at real cell sizes.
- **`Terminal::clear_screen`** (`mod.rs:1265`):
  - shifts rows but leaves `saved_cursor` untouched, so a later DECRC lands on a blank row;
  - refills rows with the current pen background (`grid.scroll_up`/`reset_region` use `cursor.template`).
- **`write_window_title`'s "safe_title"** (`terminal_effects.rs`) strips only ESC, BEL and U+009C. CAN/SUB, CR/LF and other UTF-8-encoded C1s (e.g. U+009B, U+0090) pass through to the host. Titles can include cwd/branch text.
- **Actor write errors lose output** (`pty/actor.rs:369-376`): pending writes are flushed before poll/read, and a write error breaks the loop immediately. If the child exits while replies or keystrokes are queued, its last buffered output is never read.

### 7. Performance traps (hot-path principle)
- **Per-cell allocation:** `ScreenTextCell.graphemes: Vec<u32>` allocates once per cell. Search builds it for the whole scrollback under the core lock on every query (`retained_text_buffer`). Detection text and `ghostty_screen_row` rebuild rows every tick per pane.
- **Whole-scrollback formatting under the lock:** `snapshot_history` (`pane.rs:1987`) formats the entire scrollback (up to 1M lines) as VT while holding the core mutex, which blocks the PTY reader.

### Checked and found correct
Arg order between `spawn_pty(rows, cols)` and `Terminal::new(cols, rows)`; stdio setup, `setsid` and `TIOCSCTTY` ordering; damage-to-viewport mapping via `TermDamageIterator` (display offset); the VT round-trip format; and scanner/vte framing for 7-bit CSI/OSC/DCS/APC.
