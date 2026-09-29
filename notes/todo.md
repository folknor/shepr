# Later

Things to do when the situation comes up or when there is time for a larger
change, not defects to hunt. Each redesign below touches several modules at
once, so it needs a wave of its own rather than parallel fixers.

## Deferred

Parked until the situation comes up.

- **TypeScript tests nothing runs.**
  `crates/shepr-agent/src/integration/assets/shepr-agent-state.test.ts` and the
  opencode `*.test.ts` next to its assets are not run by any gate. Running them
  would bring node or bun into the gate; the alternative is deleting them, with
  any coverage that matters moved to Rust tests over the installed asset.
- **A visible notice for a partially restored session.** A tab or workspace
  dropped during restore leaves only a server log line and a backup of the
  original `session.json`; pane-level restore errors draw inside the pane, but
  there is no session-level warning channel from server to client to say a tab
  was dropped. Build it the first time a tab actually goes missing.

## Residuals from the CLI reduction

Surfaced while landing the CLI reduction and the client/daemon binary split;
none blocks anything.

- **Integration install reads the server's environment.** The launch-time
  install resolves agent config locations (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`,
  `XDG_*`) from the server process, which a client- or SSH-spawned server may
  not share with the user's interactive shells; an agent with a non-default
  config directory then reads as absent.
- **Hook asset leftovers.** `shepr-agent-state.test.ts` still tests a Windows
  named-pipe path no asset handles; kilo and opencode agent-state use
  `socket.setTimeout(500)`, an idle timeout that does not bound a connect that
  never completes.
- **Flatten workspaces and tabs.** The owner considers the two grouping levels
  one too many. Touches the data model, persistence, sidebar and tab bar.
- **Saved-machine names.** The `Saved*` types (`SavedSshCheck`,
  `SavedSshSettings`, `SavedSshConnector`, `SavedSshPreflight`,
  `saved_ssh_error_hint`, the `saved.rs` module) and "saved machine" wording in
  shepr-config (`model.rs`, the user-visible `default.toml` comment),
  shepr-remote (`discovery*.rs`, `ssh.rs`, the host-key hint in `lib.rs`),
  `shepr-platform/src/ssh_paths.rs` and `shepr-api/src/schema/server.rs` should
  say configured machine. `RemoteSsh` is always non-interactive now, so
  `new_noninteractive_with` can become `new` and the "noninteractive" naming in
  `limits.rs` and `noninteractive_timeout` can go.
- **Boot stderr after boot.** The launcher bounds `server-boot.log` only while
  it waits for readiness; the daemon keeps the file as its stderr for life, so a
  later panic or stray write grows it without limit. Point the daemon's stderr
  at `/dev/null` once tracing is up.
- **Metadata cache and remote discovery versus build profiles.** The cache sits
  in the shared client state directory, so dev and release clients overwrite
  each other's hint for a target; remote discovery only finds an installed
  `shepr` (PATH, `~/.cargo/bin`, `~/.local/bin`), so a dev client can never
  match a remote dev build.
- **A stopped server whose lease outlives its sockets.** `server stop` now waits
  for the data-directory lease, so the launcher no longer does. A server stopped
  another way (a signal) can still drop its sockets before its lease, and a
  launch right then meets the new daemon's already-running refusal instead of
  waiting.
- **Copy-on-exit staleness.** When copy mode copies a search match on exit it
  sends the pane's latest surface `content_revision` to `pane.selection.read`,
  which checks against the last surface seen, not the one the match was found
  on, so under streaming output the copy can fail with no notice. With
  absolute rows the check may be unnecessary there.
- **Re-prompting for SSH authentication.** A machine that still needs
  authentication after a failed or skipped startup prompt is not prompted again
  until the next launch, and there is no TUI action to suspend the screen and
  authenticate. Add one if losing the shared connection mid-session proves
  annoying.

## Review upstream changes

`scripts/upstream_watch.py` reports upstream herdr changes to the integration
assets and detection manifests since the baseline in
`scripts/upstream_baseline.txt` (the fork commit, 21d0ce6). Pending at the
first run: 4b4705d "report codex turn completion through hooks". Its `LOOSE`
table guesses where upstream keeps the agent list and resume definitions;
check it against the clone.

## Confirm the opencode/Kilo permission-dialog labels

Do this the next time opencode or Kilo is in use.

- The `permission_required` rules in `crates/shepr-agent/src/detect/manifests/opencode.toml` and `kilo.toml` match "△ Permission required" only when one of the dialog's control labels is also on screen: "allow once", "allow always", "reject" or "enter confirm". Those labels were written from memory of opencode's TUI, not captured.
- If they are wrong, opencode/Kilo panes never show as blocked on a permission prompt; they read as working or idle while waiting on you.
- To check: in a shepr pane, get the agent to ask for a permission, run `shepr detect capture <pane>`, and compare the dialog's labels with the gate. Fix the manifests if they differ.

## Collapse the client's handoff state into one enum

Presentation ownership is spread across `ClientLoop` in `crates/shepr-client/src/lib.rs` (`pending_activation`, `scheduled_activation`, `freeze_recovery_attempted` and the endpoint selection tracker) and `ClientState` in `crates/shepr-client/src/state.rs` (`presentation_frozen`, `deferred_local_activation`). Fixes like `stale_freeze_recovery` and `correct_committed_surface_size` (`crates/shepr-client/src/shell_runtime.rs`) read as patches for combinations those flags allow. One presentation-ownership enum (Owned, Handoff, Unavailable, DeferredLocal) would make illegal combinations unrepresentable. No defect has been traced to it; do it if handoff bugs start appearing.

## Finish typing the client wire messages

Surfaces, the snapshot and the shell handshake are typed `ServerMessage`/`ClientMessage` variants now (`SurfaceUpdate`, `EndpointSnapshot`, `EndpointHello`/`EndpointWelcome`). What remains:

- Endpoint operations are still JSON inside the positional codec: `ClientMessage::ClientShellEndpointRequest` carries a serialized JSON-RPC request as a `String`, and `ServerMessage::ClientShellEndpointResponseChunk` carries JSON response bytes (`crates/shepr-protocol/src/input.rs`, `message.rs`). Typed request and response variants would drop the JSON pass on both ends.
- Composition round-trips the frame through a ratatui buffer for every overlay it draws (`crates/shepr-client/src/shell/presentation/composition.rs`): `FrameData::to_ratatui_buffer` and `replace_from_ratatui_buffer_preserving_effects` (`crates/shepr-protocol/src/ratatui_conversion.rs`) rebuild every cell's `String` plus two hyperlink `HashMap`s each time.

## Split large surfaces across frames

- Surface geometry is bounded to what one frame can carry (`MAX_SURFACE_CELLS = MAX_FRAME_SIZE / SURFACE_BYTES_PER_CELL`, `MAX_SURFACE_DIMENSION`, `MAX_CELL_SIZE_PX` in `crates/shepr-protocol/src/limits.rs`). Cells with long graphemes or many hyperlinks can still exceed the 16-byte budget (reported once to clients). Splitting a full surface across frames, and chunking OSC 52 clipboard data, would remove the limit.

## Per-client presentation state on the server

- The event loop mixes per-client presentation state (the client registry's `foreground_client_id`, `effective_size`, global `app.state.active`, `app.state.outer_terminal_focus`, `app.pixel_mouse_available`) with session state. Make each client's view the only source of presentation truth and drop the global "foreground client" projection into `AppState`.

## Event-driven API connection loop

- The API server is thread-per-connection with 100 ms polling (`CONNECTION_POLL_INTERVAL`). Streams carry the hub sequence (`SubscriptionStream`, `crates/shepr-api/src/subscriptions.rs`), but sampled subscriptions (scroll, agent-status fallback) still poll the app 10 times a second. One event-driven loop and a complete, sequenced model diff from the event hub would remove the polling.

## One owner for persistence, with history formatted off the loop

- Capture, writing, the history pairing and the resume schedule sit in separate places with no single owner. The data directory is locked (`crates/shepr-mux/src/persist/lock.rs`). One persistence actor could own the lock, take cheap snapshots on the loop, format history off it, and write layout plus history as one bundle. It would also own the carried history (`HistoryCarry`, `crates/shepr-mux/src/persist/snapshot.rs`, `App.pane_history_carry`).
- `live_history_read` (`crates/shepr-mux/src/persist/snapshot.rs`) still formats each pane's whole scrollback eagerly on the loop, because a `PaneRuntime` can't leave the loop and there is no `Send` handle to the terminal core. With absolute rows (`Terminal::history_origin()`), a `Send` reader could remember the last absolute row it saved and resume from the later of that and the current origin (full re-read if the origin passed it), re-reading the screen rows each time, in bounded chunks under short lock holds.
- `persist::restore` takes one size for every pane in the session; restored panes start at that size, not their own layout size, until the first resize.
