# Later

Things to do when the situation comes up or when there is time for a larger
change, not defects to hunt. Each redesign below touches several modules at
once, so it needs a wave of its own rather than parallel fixers.

## JSON API leftovers

- Most app-handler error codes (`copy_motion_unavailable`, `query_too_large`,
  `pane_layout_unavailable`, `layout_not_found`, `invalid_ratio`,
  `invalid_agent`, `pane_clear_failed`, the `*_create_failed` and
  `*_move_failed` codes) are only ever shown: the client treats every code but
  `endpoint_timeout`, `endpoint_cancelled`, `server_unavailable` and
  `endpoint_response_too_large` alike. `CopyMotionUnavailable` in the
  word-motion branch and `InvalidPaneSwap` for missing swap ids look
  unreachable through typed commands.

## Deferred

Parked until the situation comes up.

- **A visible notice for a partially restored session.** A workspace
  dropped during restore leaves only a server log line and a backup of the
  original `session.json`; pane-level restore errors draw inside the pane, but
  there is no session-level warning channel from server to client to say a
  workspace was dropped. Build it the first time one actually goes missing.

## Residuals from the CLI reduction

Surfaced while landing the CLI reduction and the client/daemon binary split;
none blocks anything.

- **Integration install reads the server's environment.** The launch-time
  install resolves agent config locations (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`,
  `XDG_*`) from the server process, which a client- or SSH-spawned server may
  not share with the user's interactive shells; an agent with a non-default
  config directory then reads as absent.
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
assets and their tests, the detection manifests (bundled and published), the
manifest tooling and the detection and hook wiring since the baseline in
`scripts/upstream_baseline.txt` (the fork was 21d0ce6; the baseline advances
as upstream changes are ported or judged irrelevant). Run it periodically;
its docstring lists what it watches. Test coverage of the agent plugins grows by
porting upstream's tests, not by writing our own.

## Confirm the opencode/Kilo permission-dialog labels

Do this the next time opencode or Kilo is in use.

- The `permission_required` rules in `crates/shepr-agent/src/detect/manifests/opencode.toml` and `kilo.toml` match "△ Permission required" only when one of the dialog's control labels is also on screen: "allow once", "allow always", "reject" or "enter confirm". Those labels were written from memory of opencode's TUI, not captured.
- If they are wrong, opencode/Kilo panes never show as blocked on a permission prompt; they read as working or idle while waiting on you.
- To check: in a shepr pane, get the agent to ask for a permission, run `shepr detect capture <pane>`, and compare the dialog's labels with the gate. Fix the manifests if they differ.

## Client presentation follow-ups


## Composition and config encoding

- Composition converts a frame with overlays to a ratatui buffer once and
  writes it back once (`crates/shepr-client/src/shell/presentation/composition.rs`).
  The write-back now rewrites only cells that differ from the buffer, but the
  forward conversion (`FrameData::to_ratatui_buffer` in
  `crates/shepr-protocol/src/ratatui_conversion.rs`) still copies every cell's
  symbol, and underline shapes still cross the buffer as `Modifier` bits
  (ratatui has no shape field, and touched cells still round-trip). Drawing
  the overlay stages straight onto the frame's cells, or converting only the
  dirty rectangles, would remove both.
- `ClientShellSnapshot.resolved_config` is a codec-encoded blob inside a codec
  message.

## Per-client presentation state on the server

- `app.state.active` doubles as a request context: a client-shell command first makes the requesting client's workspace the session's focus (`set_default_shell_target_from_client`), so app handlers that take no explicit target act on what that client views, and a new pane or workspace spawns at that workspace's area. Passing the requesting client's target and area into the app handlers would leave `active` as the saved session focus only.

## Persistence leftovers

- The agent resume schedule (`crates/shepr-server/src/app/agent_resume.rs`) still lives on `App`, apart from the session persister (`crates/shepr-mux/src/persist/actor.rs`). It spawns runtimes and needs each workspace's layout area, so it stays on the loop; what could move is the decision of which restored panes wait for a resume.
- History chunks are cut only at a line end with visible text or at a soft wrap (`crates/shepr-mux/src/pane/terminal/history.rs`), so a run of thousands of blank, unwrapped lines in history is still formatted under one lock hold.
