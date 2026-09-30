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

## CLIENT-012 - The handshake timeout is chosen by the hello flag, not the link kind

Scope: client-endpoint.

`do_handshake` (`crates/shepr-client/src/handshake.rs`) picks the local or
remote read timeout from `surface_active`, and a supervised Local reconnect
passes `false`, getting the 60 s SSH timeout, capped only by `ATTEMPT_BUDGET`,
while the limits docs describe the remote timeout as the machines'. Pass the
link kind from `endpoint/supervisor.rs` instead; a note in `handshake.rs`
records the gap.

Related, harmless: `EndpointRegistry::received` now does nothing for native
connections (their health comes from the reader's stamp), but the client loop
still calls it for every message. Remove the call or the method's native arm.

## CLIENT-023 - Idle wakeups and a lazy keyboard-mode resync

Scope: client-shell.

- **Timer wakes every 100 ms.** `timer_delay` (`shell/state.rs`) knows only the
  autoscroll and repaint deadlines; the notice, endpoint error, workspace
  highlight and selection-clear deadlines rely on `MAX_CLIENT_TIMER_DELAY` (100
  ms, `limits.rs`) polling in the client loop (`lib.rs`), so an idle client wakes
  10 times a second forever. Fold those deadlines into `timer_delay` and let the
  loop sleep until the earliest one.
- **Keyboard mode not re-synced on snapshot installs.** The keybinding-change mode
  reset in `apply_active_snapshot` is not followed by
  `sync_client_shell_keyboard_report_all`, and snapshot installs do not go
  through `finish_client_shell_input` (`shell_runtime.rs`), so the host stays in
  report-all mode until the next input.
