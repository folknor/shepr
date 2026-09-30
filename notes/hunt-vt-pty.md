# Defect hunt: shepr-vt and shepr-pty

Scope: `crates/shepr-vt`, `crates/shepr-pty`, followed into their callers in
`crates/shepr-mux/src/pane` and `crates/shepr-server`. Pinned sources checked:
`alacritty_terminal-0.26.0`, `vte-0.15.0`.

Overall: the scanner is an exact mirror of vte's framing wherever it matters
(checked state by state against `vte-0.15.0/src/lib.rs`), the keyboard-stack
cap, the row-origin tracker, the PTY spawn path and the actor's reply
ordering held up under the cases I tried. The defects are below, most
important first.

---

## 1. OSC colour query answers are captured at the end of the parse, not at the query's position

**Claim broken.** `crates/shepr-vt/src/color.rs`, `ColorQuery` doc: "`core_color`
is what the terminal itself would report (...), captured at the query's
position in the stream." `ColorQuery::child_override`: "answered from the
child's own OSC 10/11 override *at the moment it was asked*". The lib.rs
module doc also promises query replies "in byte order".

**What the code does.** `Term::dynamic_color_sequence` only pushes
`Event::ColorRequest(index, fmt)` into the listener queue. The colour is
resolved later, in `Terminal::drain_events` (`crates/shepr-vt/src/lib.rs`),
which `with_handler` runs once after `parser.advance` has consumed the whole
segment. Every colour change later in that segment is already applied when
`core_query_color` and `default_color_override` run.

Concrete input, one read:

```
ESC ] 11 ; ? BEL   ESC ] 11 ; rgb:11/22/33 BEL
```

Contract: answer the host background (or no reply if unset), with
`child_override == false`. Actual: `core_color = 11/22/33`,
`child_override = true`, so the pane echoes the new colour back in the child's
own form (`crates/shepr-mux/src/pane/terminal/helpers.rs`,
`color_query_response`). The same thing happens for OSC 4 palette and OSC 12
queries, and to everything in a synchronized-update frame, because a frame is
replayed as one `advance` (`Processor::stop_sync_internal`), including when
`tick` flushes it. "Query the current background, then set a new one" in one
write is exactly what theme-switching tools do.

The existing tests only cover set-then-query (`tests.rs`, the
`]11;rgb:...` then `]11;?` cases), which is why nothing catches it.

**Fix.** Resolve the colour at dispatch. `CoreHandler::dynamic_color_sequence`
already runs at the right moment: give the handler the host fg/bg and default
palette (it already carries `cell`), compute `core_color` and
`child_override` there, and queue a typed adapter event. More broadly, the
adapter would be simpler if it stopped using alacritty's `Event` enum as its
reply queue: have `CoreHandler` push shepr-typed replies (bytes, resolved
colour query, title, clipboard) into its own `Vec`. Then every effect is
captured at its byte position by construction, and the
`Arc<Mutex<Vec<Event>>>` listener plus the `set_history_lines` truncation
trick shrink to the few events only `Term` can emit (title from
`set_options`/`pop_title`).

---

## 2. Pane clear on the alternate screen reports success but does nothing

**Claim broken.** `Terminal::clear_screen` returns the typed
`ClearScreenOutcome::AlternateScreenActive` so the caller can tell a no-op
from a clear ("A no-op returning `AlternateScreenActive` while the alternate
screen is active").

**What the code does.** `PaneTerminal::clear_screen`
(`crates/shepr-mux/src/pane/terminal/backend.rs`) does
`let _ = core.terminal.clear_screen(); Ok(())`. `PaneClearError`
(`crates/shepr-mux/src/pane/terminal.rs`) only has `TerminalLockPoisoned`, so
`App::handle_pane_clear` (`crates/shepr-server/src/app/api/panes/copy.rs`)
answers `Handled::done()` for a clear that did not happen.
`PaneRuntime::clear_screen` also bumps the detection content sequence anyway.

**Fix.** Carry the outcome through: an `AlternateScreenActive` variant (or
return the outcome itself) and have the endpoint reject with "the pane is on
the alternate screen". The vt side already does its part.

---

## 3. Clearing history does not bring a widened pane back under its scrollback budget

**Claim broken.** `Terminal::resize` comment
(`crates/shepr-vt/src/lib.rs`): "a widened pane holds more than its byte
budget until it narrows again (or its history is cleared)."

**What the code does.** The inflated line limit (`history_lines`) is only
recomputed inside `resize`. ED 3 (`CoreHandler::clear_screen`), RIS
(`CoreHandler::reset_state`) and the host clear (`Terminal::clear_screen`)
empty the history but leave `history_lines` (and alacritty's
`max_scroll_limit`) at the widened value. New output then fills history back
up to that limit at the wide column count, above the byte budget, until some
later resize happens to recompute it. So "or its history is cleared" is not
true.

**Fix.** After any purge that leaves `history_size() == 0` on the primary
screen, call `set_history_lines(scrollback_lines(max_scrollback, cols))`
(from `Terminal`, since the handler would need `max_scrollback`; or give the
handler a "history purged" flag that `with_handler` acts on). Otherwise drop
the parenthetical.

---

## 4. vte buffers an unterminated OSC without limit, so the scanner's per-pane bounds protect nothing (robustness)

No documented contract covers this directly, but it defeats the ones next to
it. `crates/shepr-vt/src/limits.rs` bounds every scanner buffer, "keeping
attacker supplied terminal input bounded per pane" (`MAX_OSC_BYTES`,
`MAX_XTGETTCAP_BYTES`), and `MAX_CLIPBOARD_BYTES` bounds "the payload that
the parser hands to its caller". alacritty depends on vte with `std`
(`alacritty_terminal-0.26.0/Cargo.toml`), and under `std` `Parser::osc_raw`
is a plain `Vec<u8>` that `action_osc_put` grows on every byte until a
terminator arrives. A child that prints `ESC ] 52 ; c ;` and then streams
base64 (or just forgets the terminator) grows the server's heap without
bound. When it does terminate, alacritty base64-decodes the whole thing
before shepr's 192 KiB check drops it. The server is shared by every pane on
the host, so one runaway pane takes all of them down.

**Fix direction.** The scanner already tracks OSC framing exactly as vte
does. Have `Terminal::write_at` stop feeding OSC body bytes to vte once the
scanner's OSC buffer has overflowed: hand vte a CAN to end the OSC (vte
dispatches, then treats CAN as a control, which is harmless) and skip bytes
until the scanner leaves OSC state. That is the one unbounded buffer: vte's
DCS passthrough and SOS/PM/APC do not retain bytes, and its sync buffer is
capped at 2 MiB.

---

## 5. Stale doc: `ScreenTextCell` names a production caller that no longer exists

`crates/shepr-vt/src/cell.rs`, `ScreenTextCell` doc: "The one remaining
builder of whole screens, the alternate-screen history read, copies a single
viewport per poll step of an explicit API read, so the per-cell `Vec` is
kept". No production code calls `screen_text_rows`, `screen_text_rows_range`
or `screen_cell`. The only callers are `crates/shepr-vt/src/tests.rs`, the
`#[cfg(test)]` `PaneTerminal::screen_text_snapshot`, the `#[cfg(test)]`
`OwnedTextBuffer` in `shepr-mux/src/pane/terminal/text.rs`, and the tests
module of `shepr-termio/src/blit.rs`. The justification in the doc is gone;
see item 7 for what to do with the surface.

---

## 6. Lateral: panes do not export `SHEPR_CLIENT_SOCKET_PATH`, and an inherited one is overridden

**Claim broken.** AGENTS.md: "Every pane exports `SHEPR_SOCKET_PATH` and
`SHEPR_CLIENT_SOCKET_PATH`".

**What the code does.** `apply_pane_launch_env`
(`crates/shepr-mux/src/pane/launch.rs`) sets only `SHEPR_SOCKET_PATH` (from
`PaneLaunchEnv::api_socket_path`). `SHEPR_CLIENT_SOCKET_PATH` is `Allowed`,
so it only reaches the pane if the server's own environment had it. Path
resolution gives an API-socket override priority over a client-socket
override (test
`client_socket_path_api_override_takes_precedence_over_client_override` in
`crates/shepr-server/src/server/socket_paths.rs`), deriving the client socket
from the API one. So for a server started with only
`SHEPR_CLIENT_SOCKET_PATH=<custom>`, every pane gets
`SHEPR_SOCKET_PATH=<runtime>/shepr.sock` plus the inherited custom client
path. A `shepr` run inside such a pane then derives
`<runtime>/shepr-client.sock` and misses its own server. In the common case
(no overrides) the derivation lands on the right socket, which is why nobody
has noticed.

**Fix.** Export the resolved client socket explicitly next to the API socket
(pass it in `PaneLaunchEnv` as well), or fix the doc and scrub the variable.

---

## 7. Test-only surface kept in production crates (simplification)

AGENTS.md asks for the smallest code surface. None of these has a production
caller:

- `Terminal::mode_set` (tests in shepr-vt, and `shepr-mux` runtime and
  terminal tests). Its DECCOLM routing, the "refuse 2026" branch and the
  `handler::private_mode` / `Setter` table column exist for it alone.
- `Terminal::read_text_viewport`, `read_ansi_viewport`, `read_ansi_screen`
  (callers are `#[cfg(test)]` helpers in `shepr-mux/src/pane/terminal/helpers.rs`
  and `osc.rs` tests), and so `read.rs`'s `Coordinates::Viewport` and
  `viewport_line` for reads, plus `format.rs`'s `rectangle` mode and its
  `unwrap: false` path. Production only uses `read_text_screen` (selection,
  plain, unwrapped, non-rectangular) and `read_ansi_screen_carrying`
  (history).
- `screen_text_rows*`, `screen_cell`, `ScreenTextRow`, `ScreenTextCell`
  (item 5).
- In shepr-pty: `PtyCommand::new`, `arg`, `args`, `Program::Argv`,
  `resolve_shell`, `passwd_shell`, `FALLBACK_SHELL`, and the passwd buffer
  limits. Production only builds `PtyCommand::interactive_shell`
  (`shepr-mux/src/pane/launch.rs`). `backend::open_pty` and the public
  `spawn_in_pty` exist for `shepr-agent` detect tests and the actor tests.
  `home_dir`'s passwd fallback and `passwd_field` are still used by the cwd
  fallback.

The test doubles could build what they need from `interactive_shell` or a
test-support constructor, and the vt tests can assert through the production
readers.

---

## 8. Smaller notes

- **Scanner ground-state search is not memchr.** `Scanner::scan`
  (`crates/shepr-vt/src/scan.rs`) finds the next ESC with
  `iter().position(|&b| b == 0x1b)`, while vte uses `memchr` on the same
  bytes. This runs on every byte of every pane's output, just before vte scans
  the same slice again. `memchr` is already in the tree through vte.
  Cheap win on the hot path.
- **`write_at` re-enters `with_handler` per scan segment.** Each segment
  opens and closes a row batch, locks the event mutex and folds damage. It is
  correct, and segments are rare. If item 1's adapter-owned reply queue lands,
  scanner replies can be pushed into the same queue from inside the one
  advance, and the segmenting exists only to feed the injected spellings
  (`CSI 3 J`, `CSI > 4 ; Pv m`).
- **`PaneTerminal::resize` puts replies back behind the resize's own.**
  `backend.rs` takes pending core replies, resizes, drains the resize's replies
  into the actor's resize slot, and then `restore_pty_responses` the earlier
  ones for "the next read". If anything was pending, the earlier replies go
  out after the later resize reply. Today every writer collects its replies
  straight away, so the queue should be empty (only the test-only `mode_set`
  leaves some behind). If that invariant holds, `restore_pty_responses` can be
  deleted. If it does not, pending replies should be queued ahead of the
  resize replies, not after.
- **`PtyCommand` hands the server's `PWD`/`OLDPWD` to every pane.**
  `base_env` copies the server environment whole, and nothing resets `PWD` to
  the pane's cwd. Login and interactive shells fix `PWD` themselves at
  startup, so this is harmless for the only production launch (a shell). I
  note it only because `base_env`'s doc lists what must not reach a pane,
  and a stale `PWD` fits that description.
- **`Terminal::tick` returns `true` for an empty expired frame.** The doc
  says it "Returns whether anything was flushed". The callers only use the
  result to bump an epoch and request a render, so this is harmless.

## Checked and found sound

- Scanner vs vte framing: ESC/CSI/OSC/DCS/SOS-PM-APC entry and exit, C0 and
  DEL handling, raw C1 bytes in ground, 0x9C in DCS states, partial UTF-8
  across writes (vte never swallows the ESC). Injected `CSI 3 J` and
  `CSI > 4 ; Pv m` land in ground state, and inside a synchronized update they
  land in vte's buffer at the right offset.
- modifyOtherKeys: the scanner reports exactly the spellings vte's
  `('m', [b'>'])` arm drops (including `next_param_or` treating 0 as the
  default).
- Keyboard-mode stack mirror: depth keyed by `ALT_SCREEN` stays exact through
  `swap_alt` and RIS, and the pop-then-push at 4096 never reaches alacritty's
  `title_stack.remove(0)`.
- `RowOrigin`: batch bounds, the purge accounting for ED 3, RIS and the host
  clear (history + shift), height-only resizes (growth pulls from history
  without eviction), and history-limit lowering in `resize` (never below
  `history_size`, so no silent truncation).
- PTY: `TIOCGPTPEER` with `O_CLOEXEC`, the first `TIOCSWINSZ` before spawn,
  `IUTF8` through the master (Linux applies master termios ioctls to the
  slave), setsid plus `TIOCSCTTY`, an async-signal-safe `pre_exec` with the
  close_range fallback, and the slave dropped in the parent.
- Actor: byte and item accounting across partial writes, resize coalescing
  and reply placement (`insert_resize_replies` / `resize_holds`), the
  `response_order` > content > core lock order, EIO/POLLHUP drain on child
  exit, and panic containment.
