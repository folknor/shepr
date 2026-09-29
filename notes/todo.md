# Later

Things to do when the situation comes up or when there is time for a larger
change, not defects to hunt. Each redesign below touches several modules at
once, so it needs a wave of its own rather than parallel fixers.

## JSON API leftovers

- `session.snapshot` has no production caller; only tests use it.
- A thread per client-shell command forwards an already computed reply
  (`spawn_response_waiter`) so it arrives after the command's render. A
  post-render outbox in `headless.rs` would drop the thread, and with it
  `endpoint_command_in_flight` and `EndpointBusy`.
- `ApiRequestMessage` carries the full `Method`, so the app keeps an arm for
  socket-only methods routed to it by mistake; an app-only enum would drop it.
- Client shell code still names protocol types through `shepr_api::schema`
  re-exports; they could move to `shepr_protocol::command`.

## Delete ssh-agent forwarding

Decided: the owner does not use it. Remove the whole feature: the
`server.ssh_agent.register` API method and its capability flag, the lease
connection handling in `crates/shepr-api/src/server.rs`,
`crates/shepr-remote/src/remote/ssh_agent.rs` and its start in `host.rs`,
`crates/shepr-platform/src/ssh_agent.rs`, the `SSH_AGENT_*` limits, the
`ssh_agent_unavailable` error code and whatever `status` reports about it.

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
`scripts/upstream_baseline.txt` (the fork was 21d0ce6; the baseline advances
as upstream changes are ported or judged irrelevant). Run it periodically.
Upstream keeps agent descriptors and resume in `src/agent*`, and hook
authority and session handling in `src/terminal/state.rs`; the script watches
both. Upstream's hook-lifecycle tests live in `src/app/actions.rs`, which is not
watched.

## Confirm the opencode/Kilo permission-dialog labels

Do this the next time opencode or Kilo is in use.

- The `permission_required` rules in `crates/shepr-agent/src/detect/manifests/opencode.toml` and `kilo.toml` match "△ Permission required" only when one of the dialog's control labels is also on screen: "allow once", "allow always", "reject" or "enter confirm". Those labels were written from memory of opencode's TUI, not captured.
- If they are wrong, opencode/Kilo panes never show as blocked on a permission prompt; they read as working or idle while waiting on you.
- To check: in a shepr pane, get the agent to ask for a permission, run `shepr detect capture <pane>`, and compare the dialog's labels with the gate. Fix the manifests if they differ.

## Client presentation follow-ups

`ClientState::presentation` (Owned, Handoff, Unavailable) now owns the handoff
state. Two pieces remain:

- `EndpointRegistry::input_enabled` is a third freeze flag that could derive
  from `Presentation::Owned`; server netside tests call `unfreeze_input` and
  `active_surface_available`.
- `correct_committed_surface_size` (`shell_runtime.rs`) patches a handoff that
  sizes its surface request by the source's shell layout. The root fix is a
  shell API that gives the surface size under a cached, not-yet-active
  projection so the handoff can size by the target's layout.
- `ClientEndpointId::display_label` returns "Unknown endpoint" for every SSH
  endpoint.

## Finish typing the client wire messages

Surfaces, the snapshot and the shell handshake are typed `ServerMessage`/`ClientMessage` variants now (`SurfaceUpdate`, `EndpointSnapshot`, `EndpointHello`/`EndpointWelcome`). What remains:

- Composition round-trips the frame through a ratatui buffer for every overlay it draws (`crates/shepr-client/src/shell/presentation/composition.rs`): `FrameData::to_ratatui_buffer` and `replace_from_ratatui_buffer_preserving_effects` (`crates/shepr-protocol/src/ratatui_conversion.rs`) rebuild every cell's `String` plus two hyperlink `HashMap`s each time.

## Split large surfaces across frames

- Surface geometry is bounded to what one frame can carry (`MAX_SURFACE_CELLS = MAX_FRAME_SIZE / SURFACE_BYTES_PER_CELL`, `MAX_SURFACE_DIMENSION`, `MAX_CELL_SIZE_PX` in `crates/shepr-protocol/src/limits.rs`). Cells with long graphemes or many hyperlinks can still exceed the 16-byte budget (reported once to clients). Splitting a full surface across frames, and chunking OSC 52 clipboard data, would remove the limit.
- An endpoint response crosses in one frame too (`ServerMessage::ClientShellEndpointResponse`): a result larger than `MAX_FRAME_SIZE` is answered with `endpoint_response_too_large` (`crates/shepr-server/src/server/client_commands.rs`). In practice only a selection copy gets that big, so copying more than about 2 MiB of scrollback fails with that error. Streaming the selection text in parts would lift it.

## Per-client presentation state on the server

- `app.state.active` doubles as a request context: a client-shell command first makes the requesting client's tab the session's focus (`set_default_shell_target_from_client`), so app handlers that take no explicit target act on what that client views, and a new tab or workspace spawns at that tab's area. Passing the requesting client's target and area into the app handlers would leave `active` as the saved session focus only.
- Clipboard writes from panes (`AppEvent::ClipboardWrite`) carry no pane, so they go to the foreground client rather than to the clients viewing the writing pane.
- A pane in the focused set whose runtime is replaced (an agent resume starting its shell) is not told it has focus again; `sync_pane_focus` only reports changes to the set.

## Event-driven SSH agent registration on the bridge side

- The API server's end of `server.ssh_agent.register` blocks in `shepr_platform::ipc::wait_local_stream_hangup` and wakes on hang-up, API shutdown or the lease refresh interval. The bridge's end (`Registration` in `crates/shepr-remote/src/remote/ssh_agent.rs`) still polls its registration stream with `park_timeout(SSH_AGENT_STREAM_POLL_INTERVAL)` and reads the registration response in a 10 ms sleep loop. It could hold a `ShutdownTrigger` in place of its stop flag and wait on the stream with the same primitive.

## Persistence leftovers

- The agent resume schedule (`crates/shepr-server/src/app/agent_resume.rs`) still lives on `App`, apart from the session persister (`crates/shepr-mux/src/persist/actor.rs`). It spawns runtimes and needs each tab's layout area, so it stays on the loop; what could move is the decision of which restored panes wait for a resume.
- The loop still polls the persister for a finished save every `SESSION_SAVE_CHECK_INTERVAL`. The persister could wake the loop instead (a channel the headless `select!` waits on).
- A pane's history is held once between saves (the reader's cached chunks; the alternate-screen fallback is rebuilt from them and the last screen read), but a save that has to write still holds it up to three times at once on the persister thread: the chunks, the assembled snapshot text and the serialized JSON. Serializing straight from the chunks would drop the assembled copy; the file-cap trimming in `io.rs` works on assembled text, so it would have to move too.
- A single logical line longer than a chunk (a huge soft-wrapped line) is still formatted under one lock hold: chunks only end on logical line ends.
