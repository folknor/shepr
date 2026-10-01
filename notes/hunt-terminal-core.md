# Defect hunt: terminal core (shepr-vt, shepr-pty)

Scope read: every file under `crates/shepr-vt/src/` and `crates/shepr-pty/src/`
(tests skimmed, not audited), checked against the pinned
`alacritty_terminal-0.26.0` and `vte-0.15.0` sources, plus the consumers in
`crates/shepr-mux/src/pane/runtime.rs` and `crates/shepr-server/src/app/agent_resume.rs`
where a contract crosses the boundary.

Overall: the terminal core is in good shape. The scanner's framing matches vte's state
machine byte for byte on every path that can produce an event, the keyboard-stack
mirror is exact, the reply-ordering machinery in the PTY actor holds up under every
interleaving I could construct, and the libc spawn path is async-signal-safe. Two real
defects, one in each crate, plus doc drift.

---

## 1. A widened pane loses scrollback and every absolute row id when its height changes

Severity: medium. Location: `Terminal::resize`, `crates/shepr-vt/src/lib.rs`.

Claim broken: the comment in `resize` says that lowering the history limit "at most to
the history already held, never below it" means a zoom/unzoom cycle cannot "destroy
scrollback for good". `history_origin`'s doc says an absolute id names the same line for
as long as that line is retained.

Mechanism. After a widening resize, `history_lines` deliberately stays above the byte
budget for the new width (call the retained limit H, the budget B < H). The next resize
that changes only the height takes the non-rewrap path:

```rust
let history_lines = budget_lines.max(self.term.history_size().min(self.history_lines));
```

This is evaluated *after* `self.term.resize`. A height grow of k rows makes alacritty's
`Grid::grow_lines` pull k lines out of history into the screen, so `history_size()` is
now H - k, and the limit is lowered to H - k (when that is above B). Two things follow:

- **Row ids are wiped at the grow.** `rows.finish` runs with the lowered limit. History
  (H - k) now equals the limit, so the early return for "history below its limit" does
  not fire. The anchor `begin` picked was the newest history row (`Line(-1)`). It is now
  on the screen at `Line(k - 1)`, but the walk only looks from `Line(-1)` upward. It
  never finds the anchor and falls through to `self.evict(anchor.total)`, so every
  absolute id is invalidated. A copy-mode cursor, an open selection and search matches
  all jump or vanish, though no line was evicted.
- **Scrollback is lost at the shrink back.** `max_scroll_limit` is now H - k. When the
  height shrinks back by k, `Grid::shrink_lines` scrolls k lines up into history.
  `increase_scroll_limit` has no room (`min(k, (H-k) - (H-k)) = 0`), so the rotation
  recycles the k oldest history lines. Without the lowering they would have gone back
  into history.

This is reachable in normal use: a pane with more history than its byte budget at a
wider width (a wider client attaching, a zoom, closing a side split) followed by any
height change (closing a split below, a taller client). The test
`widening_resize_keeps_history_that_already_fit` only cycles the width, so it does not
catch this.

Fix direction: only lower the limit on a column change, or base the floor on lines
retained (history plus the screen rows that came out of history) rather than
`history_size()` alone. Lowering also has to happen before `rows.finish`, or `finish`
has to take the pre-resize limit, so the walk is not run against a limit the resize
itself just created. The cleaner rewrite is to give the limit one owner: a "retained
lines" floor that only RIS, ED 3 and the host clear may lower. Then `resize` never
lowers it at all.

## 2. Hard PTY read/write errors are reported as a normal close, so the pane can outlive its reader

Severity: low to medium (the trigger is rare on Linux, but the outcome is a stuck pane).
Location: `PtyIoActorRunner::read_chunk`, `write_next`, and the `pty_error` arm of
`run`, all in `crates/shepr-pty/src/actor.rs`.

Claims broken:
- `ReaderExit::Closed` is documented as "The child has gone or is being torn down; its
  own exit is reported by whoever reaps it".
- `ReaderExit::IoFailed` is documented as "The actor could no longer wait for or drain
  PTY readiness. The child may still be running, so the owner must remove the pane".
- The mux (`runtime.rs`, `on_reader_exit`) says: "A terminal-core panic or a hard
  reader IO failure can leave the child alive with no reader, so report those exits".
- The actor's own log lines say "PTY actor read failed; closing the pane" and "PTY
  actor write failed; closing the pane".

What happens. A read error that is not EIO, EAGAIN or EINTR returns
`ReadOutcome::Closed` with `exit_reason` still `Closed`. So does a write error that is
not EIO (via `handle_write_failure`), `Ok(0)` from write, and POLLERR. The mux maps
`ReaderExit::Closed => return` and waits for the child watcher. Nothing closes the pane.
The only remaining lever is the SIGHUP the kernel sends when the actor thread drops the
master. A child that ignores or handles SIGHUP (nohup'd, or an agent runtime that traps
it) keeps running with no reader. The pane sits frozen and is never removed, which is
exactly the case `IoFailed` exists for.

EIO is the only error that means the slave side closed
(`pty_master_error_means_child_closed`). Every other hard error, and POLLERR, should set
`exit_reason = ReaderExit::IoFailed`. The `Closed` doc should then drop "a PTY read
error".

## 3. `ChildIo::shutdown` drops writes queued before it, not only after

Severity: low (doc/behaviour mismatch). Location: `crates/shepr-pty/src/child_io.rs`
(doc) and `actor.rs` (`run`, `close_inbox`).

The trait doc says "Stop accepting writes; anything queued afterwards is dropped." That
reads as if writes queued before the call still go out. The actor checks `shutdown` at
the top of its loop before `pump` and then `close_inbox` clears `entries` and
`latest_resize`, so input and replies already queued are discarded too. Today the only
caller is `PaneRuntime`'s `Drop`, where discarding is right. The doc should say so
("everything still queued is dropped"), or a caller that relies on the stated
semantics will lose its last write.

## 4. Doc drift: `PtyCommand` has no argv launch

Severity: low (doc). Location: `AGENTS.md`, "Terminal core": "`command.rs`
(`PtyCommand`: argv or login shell, full env control, cwd) builds the launch". The
hunt brief repeats this.

`PtyCommand` has one constructor, `interactive_shell(program, login)`, and no way to
pass arguments. It runs the configured shell (login argv0 or plain) with no argv beyond
argv0. Agent resume also spawns the shell (`agent_resume.rs` uses
`PaneShellConfig::new(...).require_cwd()`). The doc should say "the pane shell, login or
not" rather than "argv or login shell".

---

## Lateral findings (outside the immediate scope or smaller)

- **Blocking filesystem work on the caller's thread.** This is acknowledged in
  `agent_resume.rs` but wider than that note says. `PtyCommand::to_std_command` stats
  the requested cwd (`usable_directory`) and walks `PATH` (`resolve_executable` with
  `classify_candidate`, one `stat` plus `access` per candidate) on the calling thread.
  The parent's `Command::spawn` then waits for the child's chdir and exec. Every pane
  spawn and restore runs this on the server's event loop, so a hung mount in the cwd,
  `HOME` or any `PATH` entry stalls the whole server, not just resume. This is a design
  observation, not a broken claim.
- **Empty `impl PtyReadResult {}`** in production code (`actor.rs`, right after the
  struct). It is a leftover next to the test-only `impl PtyReadResult { fn empty() }`.
- **The `mark_inherited_fds_cloexec` fallback** returns early on getdents or
  record-parse errors without closing `directory_fd`. This is harmless, because an
  error fails the spawn and the forked child exits. It is noted only because the rest
  of the function is careful about ownership.
- **Scanner over-counts OSC parser bytes.** `osc_parser_bytes` counts the `;`
  separators, which vte keeps out of `osc_raw` (`action_osc_put_param` does not push
  them). The cut therefore comes a few bytes early. The 2x headroom in
  `MAX_PARSER_OSC_BYTES` keeps the "every accepted OSC 52 store reaches the parser
  whole" claim true, so this is no defect, but the bound is not quite "body bytes
  handed to the parser" as named.
- **An oversized OSC cut by the adapter still reaches the parser.** The adapter feeds
  CAN, which makes vte's `osc_end` dispatch the truncated OSC. For OSC 52 this is
  covered: the truncated base64 either fails to decode or decodes over the cap. An OSC
  0/2 title gets dispatched at up to `MAX_PARSER_OSC_BYTES` (about 512 KiB). Whether
  the title path caps that is for the mux hunter to confirm.
- **Alternate-screen ids reuse primary ids.** On the 1049 switch the origin does not
  move, so `origin + y` names a primary line before the switch and an alt line after
  it. `history_origin`'s doc states this (alt rows are viewport rows, the origin stays
  put), and the client drops selections and copy-mode state when
  `alternate_screen_active` flips between two surfaces. A consumer that only compares
  origins would not notice the switch.

## Checked and sound

These areas were checked and hold up, so nobody needs to re-audit them:
- **Scanner against vte 0.15.** Escape, intermediate, CSI, OSC (BEL, ESC, CAN and SUB
  terminators, raw 0x9C as body), DCS entry, intermediate and param transitions, the
  passthrough and ignore states, and SOS/PM/APC all match. Partial UTF-8 at a chunk
  edge never swallows an ESC, and splitting parser input at event boundaries is safe.
  The modifyOtherKeys split between vte (`CSI > 4 ; 0..2 m`, where `next_param_or`
  treats 0 as the default) and the scanner (`CSI > m`, `CSI > 4 n`, Pv above 2) has no
  overlap and no gap.
- **Keyboard-mode stack mirror.** The cap equals alacritty's 4096. `swap_alt`, RIS and
  pop/push at the cap are all mirrored, and `set_options` never toggles
  `kitty_keyboard`.
- **Row accounting during parsing.** Every `Handler` call that can push into history
  reports a bound, and alacritty's `substitute` is a no-op. The walk arithmetic in
  `RowOrigin::finish` is correct whenever the anchor stays in history. Finding 1 is the
  one case where it does not.
- **Synchronized output.** BSU is dispatched before buffering, ESU and expiry replay in
  order, and `tick` clears the parser's timeout through `stop_sync`.
- **PTY actor inbox.** Byte and item reservations are released exactly once across
  partial writes, retire, resize replacement and close. Resize replies are inserted at
  their order and never ahead of an in-progress write. The wake-pipe drain-then-pump
  ordering cannot miss work. Lock order (response_order, then inbox, never inbox across
  a syscall) is consistent.
- **Spawn path.** O_CLOEXEC on ptmx, TIOCGPTPEER with O_CLOEXEC, a single parent master
  fd after spawn, the signal reset, setsid plus TIOCSCTTY, and the close_range fallback
  are all async-signal-safe and allocation-free.
