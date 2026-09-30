# Defects: client, TUI shell and terminal input

Filed from the defect hunt over `crates/shepr-client` (endpoint, transport,
handshake, loop, input, and the `shell/` presentation) and `crates/shepr-termio`.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## CLIENT-001 - Shift+Tab reaches legacy panes as a plain Tab when the host speaks kitty

Hunter's severity: High. Scope: termio-root.

**Claim broken.** AGENTS.md: "Key encoding to pane children covers ... legacy
encoding, kitty disambiguate and the keys crossterm's `KeyCode` models, and
modifyOtherKeys for Enter, Esc, Tab and Backspace". Shift+Tab (BackTab) is a key
crossterm models, and legacy encoding has a form for it (`CSI Z`).

**Path.**

- The client always pushes kitty flags 7 (disambiguate, event types, alternate
  keys) to the host at startup (`crates/shepr-client/src/terminal_setup.rs`,
  `set_keyboard_enhancement_flags` with
  `ime_compatible_keyboard_enhancement_flags()`).
- A kitty-capable host (kitty, Ghostty, foot, Alacritty, ...) then reports
  Shift+Tab as `CSI 9;2u`.
- `parse_kitty_key_sequence` (`crates/shepr-termio/src/input/parse.rs`) turns that
  into `KeyCode::Tab` with `SHIFT`; only the legacy `CSI Z` is parsed as
  `KeyCode::BackTab`.
- Server side, `PaneTerminal::encode_terminal_key_once`
  (`crates/shepr-mux/src/pane/terminal/backend.rs`) sends non-Char keys through
  `encode_terminal_key_with_modes`. For a pane with no kitty flags and
  modifyOtherKeys off or at level 1: `encode_modify_other_keys` returns `None`
  (Tab is level 2 only), `encode_legacy` finds no `encode_modified_special` form
  for Tab, and `encode_legacy_inner` returns `\t`. The Shift is lost.
- The kitty-pane direction works (`BackTab` is rewritten to `Tab+SHIFT`, giving
  `CSI 9;2u`); the reverse normalization (`Tab+SHIFT` to `CSI Z` for a legacy
  pane) is missing.

**Impact.** Any legacy-mode child (bash/readline completion cycling, most TUI
agents that bind Shift+Tab, including mode toggles) gets Tab instead of Shift+Tab
whenever the outer terminal supports kitty. No test feeds a host `CSI 9;2u` to a
legacy pane; `terminal_backtab_preserves_shift_across_keyboard_protocols` in
`crates/shepr-mux/src/pane/terminal/tests.rs` starts from `KeyCode::BackTab`,
which a kitty host never produces.

**Fix direction.** Treat BackTab and Tab+Shift as one key: either the parser
normalizes one to the other, or `encode_legacy`/`encode_legacy_inner` emits
`CSI Z` for Tab with exactly Shift, and the modifyOtherKeys level-1 path does the
same. One canonical form, not two spellings every encoder must remember. See also
WIRE-016 on canonical key combos.

## CLIENT-002 - Raw-input logging writes typed bytes it promises not to log

Hunter's severity: Low. Scope: termio-root.

**Claim broken.** `raw_input.rs` comments: "Length and kind only: the bytes and
the parsed key are what the user typed, passwords included, and the log file
outlives the session" and "Buffer contents are user keystrokes; log lengths,
never bytes."

`extract_one_event` logs
`tracing::debug!(sequence = ?seq, "dropping unsupported escape sequence")` with
the full sequence. Unsupported sequences include key reports the parser rejects:
a kitty report-all CSI u carrying associated text with a codepoint the parser
refuses (`reject_malformed_kitty_associated_text` cases), or IME text in a form
the parser does not accept. The codepoints of typed text then land in the client
log. `flush_timeout` also logs `bytes = ?self.buffer` for timed-out SGR mouse
prefixes: harmless, same pattern. Log length and a classification only.

## CLIENT-003 - Termio structural notes

Scope: termio-root (filed by the hunter as structural notes, not defects in
themselves).

- **`blit.rs` trusts a `FrameData` invariant it cannot see.** `write_all_cells`,
  `write_changed_cells` and `blit_patch_to` index `frame.cells[idx]` directly and
  panic if `cells.len() != width * height`. The client-side composers check this
  before building frames (`wire_cells.rs`, `compose_pane_surface.rs`), so nothing
  is known to break, but the invariant lives in several callers instead of the
  type. A validated frame type (cells length proven at construction) removes the
  panics and the duplicated checks.
- **Two "text char of a key" helpers disagree.** `copy_mode_command_char` maps an
  unshifted char with Shift through `shifted_ascii_char` (`/` + Shift to `?`);
  `keybind_help_text_char` returns the unshifted char. For the same key (no
  alternate codepoint), copy mode and the help filter see different characters.
- **Misleading module doc.** `crates/shepr-termio/src/host_term/title.rs` opens
  with a module doc about clipboard bytes; the module is named for titles and
  holds both.

## CLIENT-004 - The stdin reader polls the fd while reading through `StdinLock`'s buffer, splitting escape sequences it already holds

Hunter's severity: highest in its report. Scope: client-endpoint.

Where: `src/input.rs`, `stdin_reader_loop` and `flush_idle_input` (a path that
tracks upstream herdr).

**Claim broken.** The reader promises "Reads host input, frames and parses it
once". The crate knows the hazard: `terminal_setup.rs`'s
`query_host_escape_disambiguation` says "Bypass StdinLock's shared buffer so poll
and read observe the same bytes."

`stdin_reader_loop` does `let mut reader = stdin.lock();` and
`reader.read(&mut scratch)` with a 4096-byte scratch
(`HOST_INPUT_READ_CHUNK_BYTES`). `StdinLock` is a `BufReader` with an 8 KiB
buffer, which only skips its buffer for reads at least as large as the buffer, so
it fills up to 8 KiB from the fd and hands back 4096; the rest sits in user
space. `flush_idle_input` then asks `poll_fd_readable(reader.as_raw_fd(), ...)`
whether more input is coming; the fd is empty, poll times out, and
`flush_timeout_framed` releases the pending prefix as a lone ESC, Alt+`[` or a
broken mouse report. Only then does the next `read` return the buffered tail,
framed as plain text.

**When.** Any time 4097 to 8192 bytes wait on the tty and the 4096-byte cut falls
inside an escape sequence. The stdin thread feeds the bounded `event_tx` channel
(capacity 256) with `blocking_send`, shared with every endpoint reader, so a busy
client loop blocks the stdin thread and input piles up in the kernel. A mouse
drag or wheel burst during heavy pane output is exactly this case: the tail of an
SGR report (`4;37M`, `<35;64;37M`) goes to the focused pane as keystrokes, into an
agent's prompt. Every split also adds the idle-flush waits
(`MOUSE_ACTIVE_ESCAPE_SEQUENCE_FLUSH_TIMEOUT_MS`, then
`held_input_flush_timeout_ms`). Bracketed pastes are safe: the framer holds an
unterminated paste with no deadline.

**Fix direction.** Read the raw fd in the loop, as the startup probe does
(`shepr_platform::read_fd` on `io::stdin().as_raw_fd()`), so readiness and data
come from the same place. Check whether upstream herdr has the same bug.

## CLIENT-005 - A pane surface patch the shell rejects is dropped silently, and nothing recovers

Scope: client-endpoint.

Where: `src/lib.rs`, `handle_server_message`, the `PaneSurfacePatch` arm
(`ClientPaneSurfacePatchOutcome::Rejected => false`), with
`shepr_protocol::surface_reuse::Decoder` running on the reader thread.

**Claim broken.** The connection's decoder "happens before activation and
presentation filtering, so switching endpoints cannot discard a baseline needed
by the next wire message" (`surface_reuse.rs`). The decoder keeps its baseline,
but the shell's displayed surface is a second baseline that nothing keeps in step
with it. When the reader sees a baseline mismatch it fails the connection; when
the shell sees one it does nothing (no compose, no repaint request since the
protocol has none, no connection failure).

How the baselines drift:

- `PresentationGate::decide` drops `PaneSurfacePatch` whenever frames are frozen;
  the decoder has already applied those patches.
- During `ActivatingTarget` (frozen), a full `PaneSurface` goes into the
  handoff's evidence and later patches are dropped; `complete_at` then installs
  the older evidence surface.
- During `SynchronizingPresentation` (not frozen), patches for the new target are
  `Apply`'d against whatever the shell holds, while the full sync surface is
  `Buffer`ed into evidence, which needs the snapshot too (`coherent_surface` wants
  `snapshot_revision == projection_revision`). Patches between the sync surface
  and its snapshot are rejected against the old surface; at commit the shell
  installs the sync surface, now several patches behind the decoder, and every
  later patch fails `patch.base_surface_revision != current.surface_revision`.

**Effect.** The pane stops updating until the server happens to send a full
surface, which only follows a projection change (snapshot change, resize, focus
change). In an idle shell pane where the user types, that can mean typing blind
indefinitely. Whether the sync window happens depends on the order the server
emits reply, snapshot and surface after `ClientShellSurfaceSet { active: true }`,
which nothing the client can see guarantees.

**Fix direction (structural).** One surface baseline per connection: the
decoder's `current_surface()` already holds the authoritative grid; present from
it, or take the shell's surface from it at commit. At least, a shell-side
rejection should fail the connection like a decoder mismatch, or trigger a server
repaint.

## CLIENT-006 - Endpoint health measures client-loop latency, not transport liveness

Scope: client-endpoint.

Where: `src/endpoint/health.rs`, `registry.rs` (`received`, `tick_health`), and
`lib.rs` `ClientLoop::run` (the `biased` select with the timer first).

**Claim broken.** `crosses_ssh` says machine connections get "heartbeats and a
silence deadline", and `HEARTBEAT_TIMEOUT` is documented as tolerating "missed
scheduling and transport jitter before marking the endpoint offline". In
practice the endpoint is marked offline for the client's own stall.

`received(now)` is stamped when the loop processes a message, not when it
arrives. When the loop wakes after a stall, the overdue timer wins the `biased`
select, so `handle_timer` calls `tick_health` first, which sees an outstanding
`ping_sent_at` older than `HEARTBEAT_TIMEOUT` (or a missing first snapshot past
`connected_at + HEARTBEAT_TIMEOUT`) and returns `Expired`, while the `HealthPong`
and snapshot frames sit unread in `event_rx`.

What stalls the loop: everything it does synchronously. `HostTerminalWriter`
writes stdout blocking, so a host terminal that stops reading (XOFF or scroll
lock, a slow forwarded tty, a suspended multiplexer above shepr) blocks the loop.
`forward_clipboard` and `ClipboardWrite` run the native clipboard helper inline,
up to `CLIPBOARD_HELPER_TIMEOUT`. Stalls longer than about 10 s disconnect every
machine at once, each with "endpoint health check timed out".

**Fix direction.** Stamp liveness on the reader thread (an atomic per connection,
updated as each frame arrives) and have `tick_health` read it; or drain
`event_rx` before judging health on a timer wake.

## CLIENT-007 - Host effects from the active endpoint are applied while nothing owns the presentation

Scope: client-endpoint.

Where: `src/endpoint/message_policy.rs` `PresentationGate::decide`, and the
`lib.rs` arms for `MouseCapture`, `ClientShellKeyboardReportAll`, `WindowTitle`
and `Clipboard`.

**Claim broken.** `shell_runtime::active_endpoint_owns_presentation`: "This is
also the pane input gate: input, endpoint commands and host effects flow only
while it holds". `finish_client_shell_input` says the same for the outgoing
direction.

The gate drops presentation effects only when `frozen && activation_pending`.
`Presentation::Unavailable` with no handoff has `activation_pending == false`, so
the gate falls through to `_ if self.endpoint_active => Apply`, and
`endpoint_active` looks only at the registry (active id plus `surface_active`),
never at `Presentation`. The documented `Unavailable` states in which the
connection keeps its surface (a rollback ending `Unavailable` out of
`RestoringSource` or `SynchronizingPresentation`, and an Attention status for the
active endpoint) let the endpoint's mouse mode, report-all, title and clipboard
writes reach the host. The later automatic re-proof replays the effects anyway,
since the server resets its dedupe on activation, so the early application buys
nothing and breaks the invariant.

**Fix direction.** Give the gate the `Presentation` (or `owned()`), and treat
`is_presentation_effect` plus `Clipboard` as `Drop` unless owned.

## CLIENT-008 - A writer failure is reported as "server closed connection", and the real error is lost

Scope: client-endpoint.

Where: `src/endpoint/writer.rs` and `src/transport.rs`.

The writer keeps its error so `EndpointRegistry::take_failures` can report it; it
never gets there. `start_endpoint_transport` / `Connected` give the reader thread
the writer's `stop_handle()`. On a write error the worker stores the error and
sets `worker_stop`, the same `Arc` the reader's `EndpointReader` checks, so the
reader returns `Ok(0)`, `read_message` becomes `UnexpectedEof`, and the reader
sends `ServerDisconnected` ("server closed connection"). If that is handled before
the next timer, `handle_server_disconnected` calls `fail`, and `record_failure`
removes the connection; `take_failures` only calls `take_error()` on connections
still in the map, so the stored `endpoint write timed out` or `EPIPE` is dropped.
The diagnostic and log say the peer closed when the client's write side failed,
which matters for SSH triage.

**Fix direction.** Give the reader its own stop flag, or have `record_failure`
drain `take_error()` from the connection it removes and prefer that error.

## CLIENT-009 - `complete_at` failures are not rolled back as the code says

Scope: client-endpoint.

Where: `src/endpoint/activation.rs` `complete_at` and
`shell_runtime::complete_endpoint_activation`.

**Claim broken.** The comment in `complete_at`: "if either does not, the handoff
is rolled back like any other activation failure rather than presenting a surface
under the wrong projection."

Before its checks, `complete_at` already calls `set_surface_active(lease, true)`
and possibly `set_active(lease)`. If `activate_endpoint_projection` or
`endpoint_is_active` then fails, it returns `Err`, and
`complete_endpoint_activation` only calls `receive_endpoint_unavailable(error)`
and returns `Ok(None)`; no rollback starts. The handoff stays in
`ActivatingTarget`/`RestoringSource`, with the registry's active id possibly
already moved to the target, frames frozen and input closed, until `handle_timer`
notices `expired()` (up to `ACTIVATION_TIMEOUT`, 5 s per the hunter) and rolls
back. A `RestoringSource` failure then ends `Unavailable` although the source was
fine.

**Fix direction.** On `Err` from `complete_at`, call
`rollback_endpoint_activation` at once; better, validate before mutating the
registry.

## CLIENT-010 - A failed first Local handshake with machines configured throws away its diagnostic

Scope: client-endpoint.

Where: `src/lib.rs`, `run_launched_client` (the `initial` match) and
`run_client_loop` (`local_unavailable` sets `Connecting`).

**Claim broken.** AGENTS.md: "A config that fails to decode fails that handshake
and shows as that endpoint's Attention diagnostic"; likewise a refusal or
malformed welcome on the launch attempt.

The error goes to `warn!` only. Local shows as `Connecting`, and
`add_local(.., None, ..)` schedules a fresh attempt right away; the Attention
shows only if the second attempt fails the same way. If the server has meanwhile
stopped (it answered the first hello with `ServerShutdown`, or crashed decoding),
the user sees "Local is unavailable; start its server to reconnect" and never the
first failure. It also costs a redundant connection.

**Fix direction.** Route the launch attempt's error through
`handshake_error(.., Some(mismatch_guidance))` and seed the Local status and
diagnostic with it (Attention when `needs_attention()`), as a supervisor `Status`
event would.

## CLIENT-011 - `replay_host_theme` runs before the commit it says it follows

Scope: client-endpoint.

**Claim broken.** `src/state.rs` `replay_host_theme`: "Replay the retained
physical-host baseline only after an endpoint owns the committed presentation."

It runs at the top of `shell_runtime::complete_endpoint_activation` whenever the
phase is `ActivatingTarget`/`RestoringSource`, before `complete_at` has validated
anything, also when `complete_at` then fails, and again on every later `Ready` in
that phase. The ordering actually relied on (the theme goes ahead of the
presentation-sync request on the same transport) still holds; the doc should say
that, or the call should move after a successful `complete_at`.

## CLIENT-012 - Smaller client endpoint defects, smells and stale docs

Scope: client-endpoint.

- **`ClientMessageSink` is a dead abstraction with an infallible `io::Result`**
  (`transport.rs`). Nothing uses the `LocalStream` impl or
  `write_to_local_server`. The registry impl always returns `Ok(())`, so both
  `write_to_server(..).map_err(ClientError::ConnectionLost)?` calls in
  `finish_client_shell_input` are dead error paths that look like they could end
  the client. Delete the trait and call `endpoints.send`.
- **`ClientError::ConnectionFailed` wraps non-connect failures with a misleading
  hint.** Its Display adds "Is the shepr server running? Running `shepr` starts
  one." It also wraps `set_nonblocking` and recv-timeout failures in
  `do_handshake` and thread-spawn failures in `spawn_endpoint_reader` /
  `start_endpoint_transport`. The launch path (`lib.rs`) flattens it to
  `io::Error::other(..to_string())`, losing the kind.
- **Stale "saved endpoint" wording.** `ClientError::EndpointSetup`'s doc and
  Display say "saved SSH endpoints"; machines are `[[machines]]` config entries,
  and nothing is saved.
- **`lib.rs` module doc**: "Forwards OSC 52 clipboard writes from server to its
  own stdout". `forward_clipboard` prefers the host's native clipboard tool and
  writes OSC 52 only as a fallback, or when `prefers_osc52_clipboard`.
- **`CLIENT_EVENT_QUEUE_CAPACITY` doc** says the queue is shared by "the resize
  and server-reader threads". The stdin thread uses it too, and that sharing is
  what triggers CLIENT-004.
- **Handshake timeout chosen by `surface_active`.** `do_handshake` picks
  `LOCAL_HANDSHAKE_READ_TIMEOUT` vs `REMOTE_HANDSHAKE_READ_TIMEOUT` from the
  `surface_active` flag. A supervised Local reconnect passes `false` and gets the
  60 s "fresh SSH connection" timeout, capped only by `ATTEMPT_BUDGET`, while the
  `limits.rs` docs describe the remote timeout as the machines'. Pass the link
  kind instead of reusing the hello flag.
- **Double Detach.** On Ctrl+C (`ClientLoop::run`), on `outcome.detach` and on
  `TerminalUnavailable`, the active endpoint gets `Detach` and then another from
  `EndpointRegistry::drop` (the comments admit it). The server ignores it; one
  owner (the registry's Drop) would do.
- **Expired commands for a non-active endpoint leave the shell's pending request
  uncancelled** (`handle_timer`: `if !shell.endpoint_is_active(..) { continue;
  }`), while the response path cancels in the same situation
  (`cancel_endpoint_request`). Lane retirement at source-off makes this hard to
  reach, but the paths disagree.
- **`start_endpoint_transport(.., lifetime: impl Send + 'static, ..)`** is only
  ever passed `()`.
- **Launch-time Local failure after connect ends the client even with
  machines.** In `run_client_loop`, `start_endpoint_transport(..)?` (`try_clone`
  or thread spawn) returns an error even when
  `local_failure_policy.reconnects_local()`, breaking "With machines configured,
  losing the local server does not end the client" for this narrow resource
  failure.

## CLIENT-013 - The snapshot-install resize in `install_client_shell_snapshot` is either dead or aimed at the wrong endpoint

Scopes: client-endpoint and client-shell, with differing readings.

`install_client_shell_snapshot` (`shell_runtime.rs`) compares `surface_size`
before and after installing a snapshot and sends a resize to the endpoint whose
snapshot arrived (`endpoints.send_to(endpoint_id, ..)`) if it changed.

- The client-endpoint hunter's reading: the resize goes to the snapshot's
  endpoint even when that endpoint is not active and the size change came from
  the shell's active projection; if a non-active snapshot can change
  `surface_size`, the resize goes to the wrong server. That hunter left open
  whether it can.
- The client-shell hunter's reading: `surface_size` depends only on sidebar
  state, which snapshot installation does not change (`apply_endpoint_config`
  swaps only keybinds), so the branch cannot fire; it is dead code suggesting a
  coupling that no longer exists.

## CLIENT-014 - An in-flight copy-mode request captures every key, and a failed one throws them away

Scope: client-shell. Related: WIRE-002 (the same unbounded `push_target_event`).

`input/input.rs` `handle_key` starts with:

```rust
if self.copy_operation_in_flight {
    self.copy_input_queue.push_back(key);
    return;
}
```

While a `PaneCopyMotion` or `PaneCopySearch` request is outstanding, every key
event goes into `copy_input_queue` (press, repeat and release, any mode, overlay
or not), including the prefix, Esc, `q` and the Detach binding. The queue is
replayed only when the response is `Ok` and matches
(`complete_copy_operation(.., continue_queue = true, ..)`). On any error,
including `Timeout` (`ENDPOINT_COMMAND_TIMEOUT` = 60 s) and `Cancelled`
(disconnect, lane retirement at a handoff, a dispatch refused because the
endpoint does not own the presentation), `complete_copy_operation` runs
`copy_input_queue.clear()`; `release_input_leases` (outer focus lost) clears it
too.

- A slow or stuck server, or a copy request queued behind a slow command in the
  same serialized lane, freezes the TUI keyboard for up to 60 s, Detach included,
  breaking the Detach keybinding.
- Mouse input is not queued. The user can click another pane; the snapshot drops
  the mode to `Terminal` (`apply_active_snapshot` does this when the copy pane
  loses focus), but `copy_operation_in_flight` stays set, so text typed into the
  new pane is queued behind an unrelated copy request and, on failure, discarded
  with no notice.
- The queue has no bound. On replay, `dispatch_queued_copy_input` runs every key
  through `handle_key` into one `ClientShellInput`, and `push_target_event`
  (`input/events.rs`) appends consecutive same-pane events to a single
  `ClientShellPaneInput` with no size or count cap; the server closes the whole
  connection when one message expands past `MAX_INPUT_EVENT_BATCH` (4096)
  (`shepr-server/src/server/client_transport.rs`, "oversized targeted pane input
  batch, closing"). A long stall followed by held keys or key repeat can end in a
  disconnect.

**Direction.** Gate the queue on copy mode actually owning the key (copy mode
active and the copy pane focused); let prefix and Detach bindings pass; bound the
queue; on failure replay queued non-copy keys instead of dropping them; cap
`push_target_event` batches at the server's limits as `push_focused_paste` does
for text.

## CLIENT-016 - An "Unavailable" notice shows once per boot of the active endpoint, then never again

Scope: client-shell.

`push_endpoint_notice` (actions.rs) deduplicates every non-`Rejected` notice with
`endpoint_notice_seen.insert(key)`, keyed `(active snapshot boot_id, kind,
code)`. The set is cleared only by `reset_endpoint_projection` (a boot or
endpoint change); the only other removal is a Timeout key, cleared after a later
success of the same method. Expiry or dismissal does not re-arm it.

- `receive_endpoint_unavailable(message)` uses the message itself as the code, so
  the second time the same activation fails with the same text the user sees
  nothing: they click the machine and nothing happens. Examples:
  `"{label}: {error}"` from a preflight failure, `"buildbox did not produce a
  coherent surface in time"` from rollback, `"{label} is not ready"`, `"Local is
  reconnecting; ..."`. `begin_endpoint_activation` (shell_runtime.rs) reports every
  preflight failure through this call, so that reporting is best-effort once.
- `ClientShellEndpointError::Cancelled` maps to kind `Unavailable`, code
  `"cancelled"`, so only the first interrupted action per boot is reported. The
  notice says "Check its state before retrying", so later interruptions (a
  workspace close lost in flight) are exactly the ones the user needs.
- The key's `boot_id` is documented as "the server boot the notice is about", but
  is always the active snapshot's boot; a failed switch to another machine is
  recorded against the boot of the machine being left.

Related: `dispatch_client_shell_actions` (shell_runtime.rs) cancels an endpoint
request that never left the client (endpoint not active, or not owning the
presentation, as during a handoff) and still shows "This server action was
interrupted. Check its state before retrying.", suggesting it may have been
partly applied. `Cancelled` does not distinguish "never sent" from "sent, outcome
unknown".

## CLIENT-017 - The active remote endpoint cannot be picked explicitly; only Local can

Scope: client-shell.

**Claims broken.** `begin_endpoint_activation`: "With no proven owner
(`Unavailable`) a pick of the active endpoint re-proves ownership through a
handoff." `automatic_activation`: a failed handoff "is not retried ... until a new
connection or an explicit pick".

The shell never emits that pick for an already-active remote. `activate_endpoint`
and `focus_or_activate` (`navigation/endpoint_navigation.rs`) push
`ActivateEndpoint` only when `endpoint_id != active`, or for Local
(`endpoint_id.is_local() && (multi_endpoint_active() || !online)`). For the active
remote:

- Clicking its machine row in `handle_endpoint_machine_click` only toggles
  collapse (Local also activates on the same click).
- Picking it in the navigator (`Machine` target) closes the overlay and does
  nothing.
- Picking one of its workspaces or panes produces a plain `WorkspaceFocus` or
  `PaneFocus`, which `dispatch_client_shell_actions` cancels because the
  presentation is not owned, triggering the misleading notice from CLIENT-016.

Once the automatic re-proof has failed, a remote active endpoint left
`Unavailable` cannot be recovered by the promised pick; the user must pick another
machine or wait for a reconnect. The Local special case is justified as "Local can
still be displayed while a remote activation is pending. Route explicit selections
through the runtime so they can cancel that handoff"; the same applies to a
displayed remote while a handoff to a third machine is pending, where clicking the
displayed remote produces a cancelled command instead of cancelling the handoff.

**Fix.** Route the pick through `ActivateEndpoint` whenever the presentation is
not owned, for any endpoint. The shell cannot see ownership today, so either the
runtime decides or the shell is told.

## CLIENT-019 - Two sort keys for the same agent list

Scope: client-shell.

In `Priority` mode:

- Sidebar rows come from `endpoint_agents::agent_rows` via
  `aggregate_navigation::aggregate_agent_rows`, ordered by
  `(stale, status, Reverse(client recency))`, where recency is a client-side
  counter assigned in `cache_endpoint_snapshot_at_generation`.
- In single-endpoint mode, `FocusAgent(n)`, `NextAgent` and `PreviousAgent`
  resolve through `agent_sidebar::ordered_agent_pane_ids`, ordered by
  `(status, Reverse(state_change_seq))` (actions.rs `endpoint_command_for_action`).
  The collapsed single-endpoint sidebar prints `index + 1` beside each row
  (`endpoint_agents::render_collapsed`).

They agree only while recency and `state_change_seq` stay monotone together. On a
reconnect to a restarted server, `state_change_seq` restarts; an agent whose new
seq happens to equal its old one is not seen as changed and keeps its old, lower
recency while its seq may now be the highest, so the printed numbers differ from
what `FocusAgent(n)` focuses.

`indexed_navigation_target_exists` (input.rs) validates `FocusAgent` against a
third list, `online_agent_targets`, which drops stale endpoints; in multi-endpoint
`Spaces` mode a stale endpoint's rows sit mid-list but are not counted, so indices
shift. `ordered_agent_pane_ids` also counts agents whose workspace is missing from
the snapshot, which `AgentRowIndex::agent_row` drops from the display.

**Direction.** One ordered agent list per compose, used for rendering, the hit
map, the indices and relative navigation. Related: AGENT-027 (the seq bumps on
Idle/Unknown flips).

## CLIENT-020 - `reveal_workspace` ignores the endpoint and uses the wrong scroll units

Scope: client-shell.

`state.rs` `reveal_workspace` returns early if any `hits.workspaces` entry has the
same `workspace_id`, whatever its `endpoint_id`; workspace IDs are per server, so a
same-ID workspace on another machine suppresses the reveal. Otherwise it sets
`workspace_scroll = position among the active snapshot's entries`, while in the
multi-endpoint expanded sidebar the scroll index counts `Row::Endpoint` header rows
and every other endpoint's workspaces (`endpoint_sidebar::render_expanded`), and
the collapsed sidebar counts rows the same way. `endpoint_command_for_action` calls
it for `SwitchWorkspace(n)` in multi-endpoint mode too (`handle_endpoint_navigation`
does not take `SwitchWorkspace`), so the list first jumps to the wrong row; the
`reveal_focused_workspace` pass on the next snapshot corrects it. A visible jump,
plus an early return that can skip the correction.

## CLIENT-023 - Smaller client shell issues

Scope: client-shell. (Its note on the endpoint config being applied only as a
keymap is filed under WIRE-011, and its dead-resize note under CLIENT-013.)

- **Endpoint error cleared by non-user input.** `begin_input_batch` clears
  `endpoint_error` on any host input batch, including `Moved` mouse reports
  (any-motion tracking is on, since menus use hover), focus in and out, and host
  colour replies. `set_endpoint_error` promises a lifetime
  (`ENDPOINT_ERROR_TIMEOUT`), but a mouse twitch or terminal reply ends it at
  once, so errors such as "failed to replace client shell state" (from
  `persist_chrome_preferences`) are easy to miss.
- **The unavailable view ignores the collapsed sidebar.** `compose_unavailable`
  always calls `endpoint_sidebar::render_expanded`, even when the sidebar is
  collapsed in Compact mode (`layout.sidebar` is 4 columns wide), squeezing the
  expanded machine list into 4 columns with hit rects to match. With a single
  endpoint and the sidebar collapsed it prints "Local: online. Select a connected
  machine." when there is no other machine.
- **A scrollbar click leaves copy mode without ending it.** `handle_mouse`'s
  pane-scrollbar branch sets `self.mode = ClientShellMode::Terminal`
  unconditionally (from Copy, Prefix, Navigate or Resize) and leaves `copy_mode` in
  place. The next snapshot flips the mode back to Copy (`apply_active_snapshot`),
  so keys typed in between go to the pane as terminal input while the copy
  highlight persists.
- **Workspace drag does nothing when collapsed.** `workspace_drop_target_at`
  bounds rows by `hits.new_workspace.y`, which only the expanded sidebar sets; in
  the collapsed sidebar it is `Rect::default()` (y = 0), so every drop target is
  rejected, although the collapsed sidebar registers workspace hits and presses.
- **Timer wakes every 100 ms.** `timer_delay` only knows the autoscroll and
  repaint deadlines; the notice, endpoint error, workspace highlight and
  selection-clear deadlines rely on `MAX_CLIENT_TIMER_DELAY` (100 ms) polling, so
  the client loop wakes 10 times a second forever while idle. Folding those
  deadlines into `timer_delay` would allow a long idle sleep.
- **Keyboard mode not re-synced on snapshot installs.** The keybinding-change mode
  reset in `apply_active_snapshot` (Prefix, Navigate or Resize back to Terminal)
  is not followed by `sync_client_shell_keyboard_report_all`, and snapshot
  installs do not go through `finish_client_shell_input`, so the host stays in
  report-all mode until the next input. Harmless today because the next batch
  re-syncs, but "host mode follows shell mode" is only restored lazily.

## CLIENT-024 - An automatic notice with a multi-line ssh error can cover most of the UI unasked

Scope: client-shell (lateral).

The notice card now renders every line of its body and grows up to the rows below
`top_offset`, so a machine diagnostic opened from its badge is readable. The same
card also carries automatic notices: an Unavailable notice whose text is a
multi-line ssh error pops up large, over the sidebar and panes, without any click,
until dismissed. Keep automatic notices to a bounded height (first line or a few
lines, with the badge leading to the full text), and let only an explicitly
opened diagnostic grow.
