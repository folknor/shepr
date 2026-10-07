# Bugs: terminal core (shepr-term, shepr-vt, shepr-pty)

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

Filed from the terminal core hunt. The raw report, including its list of areas
checked and found sound, is in commit 6dc81572 (`notes/hunt-terminal-core.md`).

## TERM-001 - A paste larger than the actor inbox cap drops every terminal reply and refuses keystrokes while the child is reading normally

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

The design premise (only a stalled child builds backlog) does not hold once an
oversized item is admitted. Direction: account the admitted oversized item
separately from the reply budget (replies get their own small reservation that
a paste cannot consume), or chunk pastes into cap-sized items at the actor
boundary so the backlog measure means "unread" again. Either way the
`PaneInputDropped` wording should only fire when the child really is not
draining.

## TERM-002 - OSC 52 stores to the primary selection are silently discarded, but the docs say program copies are forwarded

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
or (more consistent with "copies it the same way", which sets both selections)
selection stores should be forwarded as copies too. The hunter thinks the code
is the side to change.

## TERM-003 - An OSC 52 store large enough to be cut by the parser bound is dropped without the size diagnostic its limit comment promises

Where: `crates/shepr-vt/src/limits.rs`, `MAX_OSC_RAW_BYTES`; cut performed by
`ScanEvent::AbortOversizedOsc` in `Terminal::write_at`
(`crates/shepr-vt/src/lib.rs`), which feeds CAN to vte so vte dispatches the
truncated OSC.

Claim broken: the `MAX_OSC_RAW_BYTES` doc: "an OSC 52 cut at this bound still
decodes to more than `MAX_CLIPBOARD_BYTES`, so it is dropped by size instead of
stored truncated."

What happens: the cut lands at a fixed raw-byte count (excluding `;`). For the
common `52;c;<base64>` form the truncated base64 is 524288 - 3 = 524285 bytes,
which is 1 mod 4. alacritty decodes with
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
far below the cut. Direction: have the scanner note an OSC 52 that it cut (it
already knows the OSC number from its buffer prefix) and emit the dropped-size
effect itself, rather than relying on alacritty decoding a truncated payload.

## TERM-004 - UiPalette does not check text contrast against `surface1`, a surface the client draws text on

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
