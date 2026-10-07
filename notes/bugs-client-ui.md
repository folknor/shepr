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
test. The patch fast path's uncoloured chrome roles are filed as SURF-001.

## CUI-001 - Copy mode loses track of the viewport on a pane that keeps printing

Hunter's label: likely, medium.

Claim broken: `CopySession` doc ("Copy-mode coordinates are absolute rows.
Output leaves retained points in place") and `ScrollLanes`' own doc ("the
target offset until a surface shows it").

`ScrollLanes::answered` sets the lane target to the offset the server
confirmed, and `ScrollLanes::shown` clears a target only when a surface shows
exactly `target.min(max)`. The server keeps alacritty's `display_offset`, and
alacritty raises the display offset by the number of new lines while the
viewport is scrolled back (so the view stays on the same content). On a pane
whose program keeps writing (an agent in Working, a build log), the next
surface after the answer carries `confirmed + k`, never `confirmed`, so the
target is never cleared. The lane lives until the pane leaves the snapshot or
the projection resets.

While a target exists, `copy::surface_presented` keeps the session's own
`offset_from_bottom` instead of the surface's
(`if lanes.target(&pane.pane_id).is_none() { scroll.offset_from_bottom } else { session.scroll.offset_from_bottom }`)
but takes the surface's new `max_offset_from_bottom`. `viewport_top_row` is
`history_origin + max - offset`, so with `max` grown by `k` and `offset` held,
the session's viewport top moves `k` rows below the rows actually on screen.
`keep_cursor_within`, the projected selection, the copy cursor cell
(`ShellView::copy_cursor`) and `offset_for_top` for the next page motion are
all computed from that wrong top, so the copy cursor and highlight drift away
from the text under them and the next page command scrolls to the wrong place.
The gap between a scroll send and the first surface showing it has the same
drift briefly even without the stuck target; the stuck target makes it
permanent.

Wider point: "target until shown, by exact equality" cannot work for a
bottom-relative offset on a terminal whose bottom moves. The lane should track
the requested viewport top as an absolute row (or the server should answer with
the absolute top), and `shown` should compare absolute tops.

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

## CUI-004 - A left-button release over a machine status badge is swallowed

Hunter's label: confirmed, low-medium.

Claim broken: the release handling in `handle_mouse_with_accounting` (chrome
drag settle, pane mouse gesture end, selection finish and copy-on-select, which
`docs/clipboard.md` promises: "the text is copied when the mouse button is
released").

`handle_raw_event` runs `handle_machine_badge_event` before any mouse routing,
and that function returns `true` (consumed) for `Up(Left)` whenever the pointer
is over a machine row's status badge of a machine with a diagnostic. The
release never reaches `handle_mouse_with_accounting`, so whatever gesture it
would have ended stays open:

- A mouse selection dragged out of a pane into the sidebar and released on the
  badge is never finished, so copy-on-select does not copy, and the highlight
  stays until the next press.
- A pane mouse gesture (left button forwarded to a mouse-reporting pane)
  released there stays recorded; the child never gets its button-up, and the
  next press is dropped by the "gesture active, other button event" early
  return.
- A workspace reorder drag released there sends no `WorkspaceMove`; a sidebar
  drag is only settled at the next press.

The badge handler should consume only the press and click it started, not an
unrelated release; a release should pass through whenever a drag, gesture or
selection is recorded.

## CUI-005 - Pixel-mode mouse events outside the ioctl extent are dropped, releases included

Hunter's label: confirmed mechanism, low.

`handle_host_input` drops a pixel report whenever `HostPixelExtent::cell`
returns `None` (`continue`), and `cell` returns `None` for a pixel past
`width_px`/`height_px` or at 0. The extent is the `TIOCGWINSZ` pixel size,
which `input/mouse.rs` itself says can differ from the terminal's reporting
space by padding. A drag that ends in the window padding, or a terminal that
reports drags past the window edge, loses its `Up`, with the same stuck-gesture
consequences as CUI-004. Clamping to the extent for Drag and Up (and dropping
only presses) would keep gestures closed. In cell mode the same edge case is an
SGR coordinate of 0, which `parse_sgr_mouse` rejects (`checked_sub(1)?`),
turning a release into `Unsupported`.

## CUI-006 - Copy-search pruning miscounts matches evicted outside the returned window

Hunter's label: confirmed, low.

Claim broken: `prune_evicted_search_matches` doc ("each dropped match was ahead
of the current one in the server's global count") and
`PaneCopySearchPosition::global_index` ("The match's index in the full result
set").

The reply carries a window of matches plus `total` and `global_index` for the
full set. Pruning only removes matches from the window and subtracts that count
from `total` and `global_index`. Matches in the full set that were older than
the window's first match are evicted too as history scrolls off, but they are
not in the window, so neither `total` nor `global_index` drops for them. The
"N of M" a long-running copy search shows on a busy pane then overstates both
numbers. The window cannot know about those matches; the counter needs either a
re-query on eviction or the server to send the oldest row of the full set so
the client can tell.

## CUI-007 - A stalled bracketed paste can carry a partial terminator into the pane

Hunter's label: confirmed, low.

`give_up_stalled_paste` (`raw_input.rs`) delivers
`take(buffer) + BRACKETED_PASTE_END`. `drain_available_chunks` keeps a split
terminator prefix in the buffer while it waits (that is what
`pending_paste_is_unterminated`'s overlap scan is for), so when the stall
timeout fires on a paste whose last bytes were `\x1b[20` (the terminator split
across reads, its tail lost), those bytes become part of the pasted text.
`cut_oversized_paste` already strips them with `partial_suffix_len`; the stall
path does not. Low: needs a terminal that drops the tail of its own
terminator.

Related documented behaviour worth a second look (not a defect, the limits doc
says it): the stall check only runs when input next arrives, so an
unterminated paste stays invisible until the user presses another key.

## CUI-009 - Frame write failure can leave the blitter's cursor-shape cache wrong

Hunter's label: confirmed, very low.

`write_frame` in `crates/shepr-client/src/state.rs` commits the encoder (and
its `last_cursor_shape`) only after the whole write succeeds. A write that
fails part way can still have delivered the `CSI Ps SP q` shape change. The
retry repaints every cell (`repaint_pending`), but `write_host_cursor_state`
emits the shape only when it differs from the cached value, so if the next
frame wants the old shape back the host keeps the shape the failed write set. A
forced repaint should also re-emit the cursor shape (and visibility), i.e.
reset `last_cursor_shape` to an "unknown" value on a refused frame.

## CUI-013 - The clipboard read path does not follow the write route over SSH

`docs/clipboard.md` says copy-mode search and overlay prompts read the
clipboard "through the local helpers"; the read path picks helpers from
`DISPLAY`/`WAYLAND_DISPLAY` regardless of the SSH variables the write route
uses, so over SSH with X forwarding it reads the remote X clipboard. The doc's
"usually inserts nothing" covers it loosely; worth saying explicitly.
