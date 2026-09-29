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
- **Flatten workspaces and tabs.** The owner considers the two grouping levels
  one too many. Touches the data model, persistence, sidebar and tab bar.
- **Startup authentication needs the managed SSH config.**
  `ssh_authentication_command` refuses unless `remote.manage_ssh_config` is
  true, so with it off a machine that needs a password or passphrase can never
  be authenticated at startup. Either make authentication work on the user's
  own ssh config or drop the setting.
- **Hand-kept preflight budget.** shepr-remote's `PREFLIGHT_CHECK_BUDGET`
  mirrors the client's per-attempt connection budget by hand; give it one
  owner at a layer both reach.
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

- Composition round-trips the frame through a ratatui buffer for every overlay it draws (`crates/shepr-client/src/shell/presentation/composition.rs`): `FrameData::to_ratatui_buffer` and `replace_from_ratatui_buffer_preserving_effects` (`crates/shepr-protocol/src/ratatui_conversion.rs`) rebuild every cell's `String` plus two hyperlink `HashMap`s each time.

## Split large surfaces across frames

- Surface geometry is bounded to what one frame can carry (`MAX_SURFACE_CELLS = MAX_FRAME_SIZE / SURFACE_BYTES_PER_CELL`, `MAX_SURFACE_DIMENSION`, `MAX_CELL_SIZE_PX` in `crates/shepr-protocol/src/limits.rs`). Cells with long graphemes or many hyperlinks can still exceed the 16-byte budget (reported once to clients). Splitting a full surface across frames, and chunking OSC 52 clipboard data, would remove the limit.
- An endpoint response crosses in one frame too (`ServerMessage::ClientShellEndpointResponse`): a result larger than `MAX_FRAME_SIZE` is answered with `endpoint_response_too_large` (`crates/shepr-server/src/server/client_commands.rs`). In practice only a selection copy gets that big, so copying more than about 2 MiB of scrollback fails with that error. Streaming the selection text in parts would lift it.

## Per-client presentation state on the server

- The event loop mixes per-client presentation state (the client registry's `foreground_client_id`, `effective_size`, global `app.state.active`, `app.state.outer_terminal_focus`, `app.pixel_mouse_available`) with session state. Make each client's view the only source of presentation truth and drop the global "foreground client" projection into `AppState`.

## Event-driven SSH agent registration on the bridge side

- The API server's end of `server.ssh_agent.register` blocks in `shepr_platform::ipc::wait_local_stream_hangup` and wakes on hang-up, API shutdown or the lease refresh interval. The bridge's end (`Registration` in `crates/shepr-remote/src/remote/ssh_agent.rs`) still polls its registration stream with `park_timeout(SSH_AGENT_STREAM_POLL_INTERVAL)` and reads the registration response in a 10 ms sleep loop. It could hold a `ShutdownTrigger` in place of its stop flag and wait on the stream with the same primitive.

## Persistence leftovers

- The agent resume schedule (`crates/shepr-server/src/app/agent_resume.rs`) still lives on `App`, apart from the session persister (`crates/shepr-mux/src/persist/actor.rs`). It spawns runtimes and needs the view's geometry, so it stays on the loop; what could move is the decision of which restored panes wait for a resume.
- The loop still polls the persister for a finished save every `SESSION_SAVE_CHECK_INTERVAL`. The persister could wake the loop instead (a channel the headless `select!` waits on).
- A pane's history is held up to three times: the reader's formatted chunks, the carried `Live` copy (the alternate-screen fallback) and the snapshot being written. The carried copy could be rebuilt from the chunks plus the last screen read instead.
- A single logical line longer than a chunk (a huge soft-wrapped line) is still formatted under one lock hold: chunks only end on logical line ends.
- Pending agent resumes of visible panes take the view's `inner_rect`, which the view computes without the scrollbar gutter for a pane that has no runtime yet, so such a resume starts one column wider than its first resize.
