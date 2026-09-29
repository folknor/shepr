# Later

Things to do when the situation comes up or when there is time for a larger
change, not defects to hunt. Each redesign below touches several modules at
once, so it needs a wave of its own rather than parallel fixers.

## JSON API leftovers

- `shepr_api::schema::SessionSnapshot` is no longer an API type: it is only an
  internal step before `client_shell.rs` builds the shell snapshot. Move it
  into shepr-server without serde; its `version` is never read, and several
  `PaneInfo` fields (`restore_error`, `agent_session`, `scroll`,
  `terminal_id`, `agent`, `focused`) are computed for every pane on every
  rebuild but never projected.
- `ApiErrorCode` still carries codes from the JSON era of TUI commands
  (`copy_motion_unavailable`, `invalid_pane_swap`, `split_not_found`,
  `query_too_large` and kin), plus an `External` variant and a string parser
  whose main wire user was the deleted ssh-agent bridge; audit what still
  produces or parses each.
- `shepr_platform::ipc::poll_local_stream_read` has no callers, and
  `start_server_with_stop_control` is the API server's only entry point.
- `render_and_stream` (`headless/render.rs`) skips a client whose surface
  render is deferred (synchronized output) before its snapshot projection, so
  that client gets no snapshot that render and a held endpoint reply can
  arrive before the projection its command changed.
- In the loop's stop path a `ClientShellEndpointRequest` falls into the
  catch-all arm and is never answered; the client waits out its timeout.

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

- A handoff sizes each side by its own layout (`HandoffGeometry`), but if a
  cached snapshot changes the committing endpoint's tab count mid-handoff (a
  tab added or closed elsewhere), the requested size goes stale and nothing
  corrects it after commit. Comparing geometry in `lib.rs` before
  `receive_snapshot` would catch it.
- `ClientShellEndpoint.label` always equals `endpoint_id.display_label()`; the
  field and the lookup in `endpoint_label` can go.
- shepr-client depends on shepr-api for one call,
  `shepr_api::server_stop::restart_after_update_guidance_for` in
  `endpoint/supervisor.rs`. Moving it lower (shepr-config or shepr-remote)
  drops the dependency.

## Composition and config encoding

- Composition converts a frame with overlays to a ratatui buffer once and
  writes it back once (`crates/shepr-client/src/shell/presentation/composition.rs`),
  but the write-back (`FrameData::from_ratatui_buffer_with_hyperlinks` in
  `crates/shepr-protocol/src/ratatui_conversion.rs`) still rebuilds every
  cell's `String` and two hyperlink `HashMap`s, and underline shapes survive
  only through `Modifier` bits stashed across the round trip.
- `ClientShellSnapshot.resolved_config` is a codec-encoded blob inside a codec
  message.

## Split large surfaces across frames

- Surface geometry is bounded to what one frame can carry (`MAX_SURFACE_CELLS = MAX_FRAME_SIZE / SURFACE_BYTES_PER_CELL`, `MAX_SURFACE_DIMENSION`, `MAX_CELL_SIZE_PX` in `crates/shepr-protocol/src/limits.rs`). Cells with long graphemes or many hyperlinks can still exceed the 16-byte budget (reported once to clients). Splitting a full surface across frames, and chunking OSC 52 clipboard data, would remove the limit.
- An endpoint response crosses in one frame too (`ServerMessage::ClientShellEndpointResponse`): a result larger than `MAX_FRAME_SIZE` is answered with `endpoint_response_too_large` (`crates/shepr-server/src/server/client_commands.rs`). In practice only a selection copy gets that big, so copying more than about 2 MiB of scrollback fails with that error. Streaming the selection text in parts would lift it.

## Per-client presentation state on the server

- `app.state.active` doubles as a request context: a client-shell command first makes the requesting client's tab the session's focus (`set_default_shell_target_from_client`), so app handlers that take no explicit target act on what that client views, and a new tab or workspace spawns at that tab's area. Passing the requesting client's target and area into the app handlers would leave `active` as the saved session focus only.

## Persistence leftovers

- The agent resume schedule (`crates/shepr-server/src/app/agent_resume.rs`) still lives on `App`, apart from the session persister (`crates/shepr-mux/src/persist/actor.rs`). It spawns runtimes and needs each tab's layout area, so it stays on the loop; what could move is the decision of which restored panes wait for a resume.
- History chunks are cut only at a line end with visible text or at a soft wrap (`crates/shepr-mux/src/pane/terminal/history.rs`), so a run of thousands of blank, unwrapped lines in history is still formatted under one lock hold.
