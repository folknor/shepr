# Defect hunt: terminal core (shepr-term, shepr-vt, shepr-pty)

Scope: `crates/shepr-term`, `crates/shepr-vt`, `crates/shepr-pty`, followed
across into `shepr-mux` (pane terminal, launch settlement), `shepr-server`
(pane input), `shepr-client` (palette use) and `shepr-git` where a value
crossed the boundary. Reconnaissance only; nothing was edited.

Overall: the core is in good shape. The scanner tracks vte 0.15's framing
faithfully, the absolute-row tracker and the keyboard-stack mirror match the
pinned alacritty 0.26 exactly, and the launch protocol and fork path hold up
under the failure orders I could construct. The findings below are the gaps
that remain, most significant first.

## F1. A paste larger than the actor inbox cap drops every terminal reply and refuses keystrokes while the child is reading normally

Where: `crates/shepr-pty/src/actor.rs`, `PtyIoInbox::reserve` and
`PtyIoInbox::push_terminal_response`; limit `ACTOR_INBOX_MAX_BYTES` (256 KiB)
in `crates/shepr-pty/src/limits.rs`.

Claim broken: the comment on `push_terminal_response` justifies dropping
replies with "Only a child that has stopped reading fills the inbox, and a
reply it will read late is worth little". The server-side notice for a
refused input (`NoticeKind::PaneInputDropped`, reached through
`ChildIoSendError::Full` -> `PaneInputError::Backpressure` in
`crates/shepr-server/src/server/pane_input.rs`) tells the user "the pane is not
reading its input".

What happens: `reserve` admits one item larger than the byte cap when nothing
is outstanding. A paste is queued as a single item
(`PaneRuntime::try_send_paste` in `crates/shepr-mux/src/pane/runtime/input.rs`),
and the protocol allows pastes up to `MAX_INPUT_PAYLOAD` = 1 MiB
(`crates/shepr-protocol/src/limits.rs`). For any paste between roughly 256 KiB
and 1 MiB, `pending_bytes` stays above the cap for as long as the child takes
to consume the excess at PTY speed, even though the child is reading. During
that window:

- every terminal reply (DA1, DSR/CPR, DECRQM, OSC colour answers, kitty
  `CSI ? u`, in-band resize reports, resize replies) fails `reserve` and is
  dropped. A program that queries the terminal while ingesting the paste (an
  editor or shell prompt hook asking for the cursor position or background)
  waits for an answer that never comes;
- every keystroke is refused with `Full`, and the user is told the pane is not
  reading its input, which is false.

The design premise (only a stalled child builds backlog) does not hold once
an oversized item is admitted. Direction: account the admitted oversized item
separately from the reply budget (replies get their own small reservation that
a paste cannot consume), or chunk pastes into cap-sized items at the actor
boundary so the backlog measure means "unread" again. Either way the
`PaneInputDropped` wording should only fire when the child really is not
draining.

## F2. OSC 52 stores to the primary selection are silently discarded, but the docs say program copies are forwarded

Where: `crates/shepr-vt/src/effects.rs`, `Effects::drain`, which keeps only
`ClipboardType::Clipboard`; alacritty maps targets `p` and `s` to
`ClipboardType::Selection` (`Term::clipboard_store` in alacritty_terminal
0.26).

Claim broken: `docs/clipboard.md`, "Programs in panes. A program that copies
with OSC 52 (an editor's yank to the system clipboard, for example) has its
text forwarded through the server to the TUI, which copies it the same way."
The same document says every shepr copy sets both the clipboard and the
primary selection.

What happens: `OSC 52 ; p ; ...` and `OSC 52 ; s ; ...` are dropped with no
effect and no diagnostic (the code comment says so; the user doc does not).
Neovim's OSC 52 provider sends the `*` register as `p`, so a yank to `"*` in a
pane does nothing. Either the doc needs to say only the `c` target is honoured,
or (more consistent with "copies it the same way", which sets both
selections) selection stores should be forwarded as copies too. I think the
code is the side to change.

## F3. An OSC 52 store large enough to be cut by the parser bound is dropped without the size diagnostic its limit comment promises

Where: `crates/shepr-vt/src/limits.rs`, `MAX_OSC_RAW_BYTES`; cut performed by
`ScanEvent::AbortOversizedOsc` in `Terminal::write_at`
(`crates/shepr-vt/src/lib.rs`), which feeds CAN to vte so vte dispatches the
truncated OSC.

Claim broken: the `MAX_OSC_RAW_BYTES` doc: "an OSC 52 cut at this bound still
decodes to more than `MAX_CLIPBOARD_BYTES`, so it is dropped by size instead of
stored truncated."

What happens: the cut lands at a fixed raw-byte count (excluding `;`). For the
common `52;c;<base64>` form the truncated base64 is 524288 - 3 = 524285
bytes, which is 1 mod 4. alacritty decodes with
`base64::engine::general_purpose::STANDARD`, which rejects that length, so
`clipboard_store` sends no event at all: no `ClipboardStore`, therefore no
`dropped_clipboard_store_bytes`, therefore no
`report_oversized_clipboard_store` in
`crates/shepr-mux/src/pane/terminal/backend.rs`. The store is not stored
truncated (that part holds), but it vanishes silently, so the user-facing
oversize diagnostic only covers stores between 192 KiB and about 384 KiB
decoded; anything larger gets nothing. The only test,
`oversized_osc52_clipboard_store_reports_only_its_byte_count`
(`crates/shepr-vt/src/tests.rs`), uses a payload just over the clipboard limit,
far below the cut. Direction: have the scanner note an OSC 52 that it cut
(it already knows the OSC number from its buffer prefix) and emit the
dropped-size effect itself, rather than relying on alacritty decoding a
truncated payload.

## F4. UiPalette does not check text contrast against `surface1`, a surface the client draws text on

Where: `crates/shepr-term/src/host_tint.rs`, `UiPalette::derive_toward`: the
`surfaces` list that `on_surfaces` pushes text and hues against is
`[background, panel_bg, active_row_bg, selection_bg, surface0]`. `surface1`
(contrast target 1.8, the furthest neutral from the background) and
`surface_dim` are computed afterwards and never enter it.

Claim broken: the `UiPalette::derive` doc, "Text and the hues then keep their
contrast against every surface they are drawn on."

Evidence that text is drawn on it: copy-mode search matches use
`Style::default().fg(palette.text).bg(palette.surface1)` in
`crates/shepr-client/src/shell/view/draw.rs`. On a low-contrast theme where
`text` was pushed only just to 4.5:1 against the listed surfaces, its contrast
against `surface1` can fall well below that, and the `readable` flag that picks
the surface direction never sees it. Direction: include `surface1` in the
surface set (and anything else that is ever a text background), or narrow the
doc to the surfaces actually checked.

## F5. OSC body capture does not carry "every OSC the terminal saw"

Where: `crates/shepr-vt/src/scan.rs`, `Scanner::dispatch_osc` returns before
emitting `ScanEvent::OscBody` when `overflow` is set (body over
`MAX_OSC_BYTES` = 16 KiB), and a body cut at `MAX_OSC_RAW_BYTES` never reaches
`dispatch_osc` at all.

Claim broken: `Terminal::set_osc_body_capture` doc, "While on,
`TerminalEffects::osc_bodies` carries every OSC the terminal saw".

Effect is limited to the opt-in OSC debug log (`osc_debug::log` in mux), but
that log is exactly where someone looks when a large OSC (an OSC 52 copy, a
long OSC 8 link) misbehaves, and it silently omits those. Either log a
truncated body with a marker, or reword the doc to "every OSC up to 16 KiB".

## F6. A child killed between its `ChdirOk` report and `execve` can settle as `Launched`

Where: `crates/shepr-mux/src/pane/launch_status.rs`, `settle`, the
`CommitCandidate` arm, which decides `Launched` versus `Unconfirmed` with
`child_liveness.has_exited()`.

Claim: `LaunchStatusEvent` doc in `crates/shepr-pty/src/launch.rs`: "EOF is
only a commitment candidate: the caller must still establish that the child
lives at that instant."

What happens: the kernel closes a dying task's fds (`exit_files`) before it
becomes a zombie and before pidfd readiness is signalled (`exit_notify`). A
child killed after sending `ChdirOk` but before `execve` closes its status
socket first, so the reader can see EOF while `has_exited()` (pidfd
readiness) still reads false, and the launch settles as `Launched` in a
directory the shell never ran in, followed by an ordinary `PaneDied`. The
window is tiny (only the envp lookup sits between the report and `execve`),
and the observable effect is a launch logged and published as succeeded that
did not. Low severity; noted because the reader's contract names exactly this
check, and the check cannot tell the difference. A stronger test would be to
wait briefly for either pidfd readiness or `/proc/<pid>/exe` changing, or to
treat EOF as committed only once the child has produced PTY output or survived
a short grace.

## Lateral findings (outside the scope)

### L1. Git helpers spawned by std inherit every server fd until they exec, including the data-directory lease

Where: `crates/shepr-git/src/runner.rs`, `run_git_with_program_and_clock`,
built from `shepr_platform::child_command(program, cwd)`
(`crates/shepr-platform/src/host.rs`), which sets `current_dir(cwd)`. The cwd
is a workspace directory, which can be on a hung network mount.

std performs that chdir in the forked (or `posix_spawn`ed) child before
`execve`. Until exec, the child holds a copy of every fd the server has: all
PTY masters (O_CLOEXEC only helps at exec), the server socket and the
data-directory lease, which is an `flock` (`acquire_flock_lock` in
`crates/shepr-platform/src/data_directory_lease.rs`) and so stays held while
any duplicate of its open file description is open. A git child stuck in
uninterruptible chdir therefore:

- keeps the lease locked after the server exits, so no successor server can
  start until the mount recovers (AGENTS.md: "the lease decides which
  contender owns the data directory");
- keeps closed panes' PTY masters open, so their sessions never get the
  master-close hangup (teardown's signals still apply, which softens this).

`crates/shepr-pty/src/launch.rs` already names this hazard ("every fd the
server owns is inherited by whatever it forks, including helpers std spawns,
and a helper hung in its own chdir would keep a parent-made pipe open
indefinitely") and the pane fork path avoids it by closing fds before chdir;
the git runner does not. Direction: run git with `-C <dir>` (or open the
directory with `O_PATH` in a short-lived thread and use `fchdir` in a
`pre_exec` after `close_range`), so no inherited fd outlives a hung chdir; or
spawn helpers from a small fork-server process that holds none of the
server's fds.

### L2. `PtyIoActorConfig::with_idle_poll` documents a nonzero requirement it does not enforce

`crates/shepr-pty/src/actor.rs`: "It must be nonzero". A zero duration turns
the actor's `poll` into a busy loop. Only tests call it today; a
`NonZero`-style type or a clamp would make the contract real.

## Checked and found sound

So nobody repeats the work:

- `Scanner` versus vte 0.15 framing: CSI entry/param/ignore states, OSC
  termination on BEL/CAN/SUB/ESC (vte dispatches on all four), DCS
  ignore/passthrough merging (both resume on ESC; raw 0x9C only differs where
  no event depends on it), SOS/PM/APC. The modifyOtherKeys spellings the
  scanner injects are exactly the ones vte's `('m', [b'>'])` arm rejects
  (`next_param_or(1) == 4` makes `CSI > m` unhandled), and `CSI ? 3 J` and
  `CSI 16 t` are indeed undispatched by vte.
- Oversized-OSC skipping and the injected CAN, ED3 and XTMODKEYS spellings
  keep byte order inside synchronized updates (they go through vte's sync
  buffer at a sequence boundary).
- `RowOrigin` (`rows.rs`): every handler call that can push primary lines into
  history reports an upper bound (input, tab, LF/NL/IND/NEL via `linefeed`,
  SU, DL at the region top, ED 2); purges (ED 3, RIS, host clear, column
  change) are settled explicitly; the address walk cannot hit a recycled row
  because batches close before `pushed` reaches the limit, and the minimum
  history (1000 lines) keeps the per-call bound far below it.
- Keyboard-mode stack mirror: alacritty swaps stacks with `ALT_SCREEN` in
  `swap_alt`, clears both on RIS, and `set_options` only resets them when
  `kitty_keyboard` changes, which `term_config` never does.
- `HistoryCapacity::set` never shrinks below retained content on the width
  path (`at_least(held)`), so `Grid::update_history` does not evict.
- Damage: `scroll_display` and `scroll_up_relative` mark full damage, so a
  display-offset change always reaches `RenderState` as full.
- Fork path: signals blocked across `_Fork`, dispositions reset before the
  mask is cleared, fds closed (with a `/proc/self/fd` fallback) before any
  filesystem step, status socket created in the child, slave kept above stdio
  so `dup2` clears close-on-exec.
- Launch router: pid-checked routing, parked and retired tickets with TTLs,
  per-peer hello deadlines, listener failure poisoning, record validation and
  phase ordering in `LaunchStatusReader`.
- Actor inbox ordering apart from F1: resize replies keep their place,
  superseded resizes release their reservations, partial writes release bytes
  exactly once.
- Mouse encoding: X10/1000/1002/1003 filtering, legacy and UTF-8 coordinate
  limits, 1016 with and without a known extent.
