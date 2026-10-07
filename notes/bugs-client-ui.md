# Bugs: client presentation and input (shepr-client shell and input, shepr-termio)

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

Filed from the client presentation and input hunt. The raw report, including
its list of areas checked and found consistent, is in commit 6dc81572
(`notes/hunt-client-presentation.md`). The hunter's labels: "confirmed" means
read end to end in code; "likely" means the code path is certain but one
external behaviour (terminal or emulator) was taken from memory rather than a
test.

## CUI-002 - A pane program can stall the client loop with OSC 52, and the comment says it cannot flood

Hunter's label: confirmed, medium.

Claim broken: the comment at the `DecodedWireServerMessage::Clipboard` arm in
`client_loop/dispatch.rs` ("Once per user copy, so a warn cannot flood"), and
the reasoning in `shell/input/mod.rs` `read_clipboard_text_bounded` that a
clipboard helper must not run on the event loop because a hung one "would
freeze rendering and input for every pane".

`ServerMessage::Clipboard` is produced by `handle_internal_event_with_origin`
in `shepr-server/src/server/headless/internal_events.rs` for every
`RuntimeEvent::ClipboardWrite`, which any program in any pane triggers by
writing OSC 52; it is sent even when no client views the pane (to the
foreground client). There is no rate limit on either side. The client handles
each one synchronously on its event loop through `forward_clipboard` ->
`write_clipboard_bytes` -> `ClipboardRoute::write_with_helpers`, which spawns
the clipboard helper and then the primary-selection helper and waits for them
under `CLIPBOARD_HELPER_TIMEOUT` (2 s). So:

- A pane program that writes OSC 52 in a loop (or a TUI that re-yanks on every
  redraw) makes the client spawn two helpers per write on the loop thread, and
  rendering and input for every machine stall behind them.
- With a hung selection owner (the exact case the modal-paste reader was
  bounded for) each write blocks the loop for up to 2 s.
- Each failure logs at WARN (the per-cause dedupe that `HostWriteFailure` gives
  frames is not used here), so the "cannot flood" comment is false.

The user-copy path in `finish_client_shell_input` has the same synchronous
helper call (bounded at 2 s); that one is at least driven by a user action. The
server-sourced path needs the same off-loop treatment the paste read got (a
worker with latest-wins coalescing), and the comment needs to stop saying "user
copy".

## CUI-003 - Input leases are taken for presses whose release the host never sends; focus loss then sends stale releases into panes

Hunter's label: confirmed, low-medium.

Claim broken: `press_takes_lease` doc in `shepr-termio/src/input/lease.rs`
("Whether a press can be followed by its repeats and release, and so holds a
lease ... Otherwise a key that committed text gets no release event, and a
lease for it would go stale").

The function takes a lease for every press without `generated_text`, whatever
the host keyboard mode. Presses that never get a release:

- Under the flags the client always pushes
  (`ime_compatible_keyboard_enhancement_flags`: disambiguate, event types,
  alternate keys, no report-all), kitty does not report releases for Enter, Tab
  and Backspace (the kitty spec keeps them legacy so `reset` can be typed);
  they arrive as `\r`, `\t`, `\x7f`, parse with no `generated_text`
  (`with_text_commit` only sets it for `Char`), and take a Forwarded lease.
- On a host with no kitty support (or a tmux/WezTerm host on modifyOtherKeys
  only), no key ever gets a release, so every arrow, Esc, F-key and control
  chord forwarded to a pane leaves a Forwarded lease.

These leases are only replaced by the next press of the same code.
`release_input_leases` (run on `OuterFocusLost`) turns every one of them into a
synthetic `KeyEventKind::Release` sent to the pane it was forwarded to. A pane
child with kitty REPORT_EVENT_TYPES then receives key-up events for keys that
were released long ago (for example `CSI 13;1:3u` for an Enter pressed minutes
earlier) on every focus switch, and the encoder in
`shepr-term/src/key/encode.rs` emits a CSI-u release for Enter even when the
child did not ask for report-all, which a real kitty terminal never does.

Fix direction: decide "a release will follow" from the host mode the input
arrived under (kitty event types active, and for Enter/Tab/Backspace also
report-all), not from `generated_text` alone.

Related smell: `host_reports_all_keys` is read from
`HostModes::keyboard_report_all_active()` when the batch is handled, which is
the mode last requested, not the mode the bytes were produced under. The doc on
`ClientShellState::host_reports_all_keys` and `handle_host_input` says "the
host keyboard mode the input arrived under". Keys typed between the prefix
press and the host applying the report-all push are handled as if
reported-all; a text press then takes a lease whose release never comes.
Narrow window, same mechanism as above.

## CUI-013 - The clipboard read path does not follow the write route over SSH

`docs/clipboard.md` says copy-mode search and overlay prompts read the
clipboard "through the local helpers"; the read path picks helpers from
`DISPLAY`/`WAYLAND_DISPLAY` regardless of the SSH variables the write route
uses, so over SSH with X forwarding it reads the remote X clipboard. The doc's
"usually inserts nothing" covers it loosely; worth saying explicitly.

## CUI-014 - Leaving copy mode restores a bottom-relative offset, so new output changes what it restores

Raised as a lateral by a wave 2 fixer.

Copy mode now tracks absolute viewport rows, but `exit_copy_mode`
(`crates/shepr-client/src/shell/copy/keys.rs`) still restores the saved
scroll position as a bottom-relative offset. On a pane that printed while copy
mode was open, that offset names different content than the user was reading
when they entered.
