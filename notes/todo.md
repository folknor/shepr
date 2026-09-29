# Later

Things to do when the situation comes up or when there is time for a larger
change, not defects to hunt. Each redesign below touches several modules at
once, so it needs a wave of its own rather than parallel fixers.

## Resolve typescript question

What to do about crates/shepr-agent/src/integration/assets/shepr-agent-state.test.ts
(and the opencode `*.test.ts` files next to its assets).

## Moved out of the findings documents as new claims

Each of these would make shepr promise something it does not promise today,
so they wait for a decision rather than a fixer.

- **Log rotation as config keys.** `shepr-platform/src/logging.rs::init_file_logging_with_config`
  passes `DEFAULT_MAX_LOG_BYTES` (5 MiB) and `DEFAULT_RETAINED_LOG_FILES` (1)
  from `shepr-platform/src/limits.rs`, although `RotatingFileMakeWriter::new`
  takes both. Moving them into `shepr-config` as validated keys would add two
  config keys.
- **Environment variables in `shepr --help`.** Render the `shepr-core`
  environment registry into `print_help` (`src/cli.rs`), with a test that it
  covers every entry. `--help` currently documents only `SHEPR_CONFIG_PATH`.
- **Hook assets call the CLI.** About fifteen hook assets open the API socket and
  hand-build the JSON-RPC envelope (two request-id formats, two timeouts).
  Moving every asset to the CLI keeps socket framing in Rust only. A rewrite of every shipped
  asset rather than a defect fix.
- **A visible notice for a partially restored session.** A tab or workspace
  dropped during restore leaves only a server log line and a backup of the
  original `session.json`; pane-level restore errors draw inside the pane, but
  there is no session-level warning channel from server to client to say a tab
  was dropped.
- **Remote checkout root for the new-workspace label.** For a remote endpoint,
  ask the remote server for the cwd's checkout root instead of skipping the Git
  lookup (`open_new_workspace_overlay` only queues the lookup for the local
  endpoint). Needs a new API method.

## Residuals from the CLI reduction

Surfaced while landing `notes/cli-ux-spec.md`; none blocks anything.

- **Install integrations automatically.** The server should install or update
  hooks for configured agents at launch, after which the `integration` command
  group can go (`notes/cli-ux.md`).
- **Flatten workspaces and tabs.** The owner considers the two grouping levels
  one too many. Touches the data model, persistence, sidebar and tab bar.
- **Rename `shepr_api::session`.** It now holds local server stop, restart
  guidance and the stop-target build guard; `SessionError` and
  `ApiErrorCode::SessionStopFailed` are misnomers too.
- **Hook assets and the CLI.** The item above proposes moving hook assets to
  the CLI, but the CLI no longer has report commands; that direction now means
  adding them back.
- **Server shell state is always present.** `ClientConnection::shell_state()`
  and `shell_state_mut()` in shepr-server still return `Option` although every
  connection has shell state; dozens of call sites guard a `None` that cannot
  happen.
- **Unconsumed `revision`.** `PaneInfo.revision` and `AgentInfo.revision` have
  no consumer in shepr.
- **Pane copy and search handlers.** Check whether the TUI still reaches every
  handler in `crates/shepr-server/src/app/api/panes/copy.rs` (including
  `clear_screen`); remove what it does not.
- **Remote interactive branches.** `RemoteSsh`'s non-interactive flag and its
  interactive branches (stderr relay, `framed_user_shell_output`) are only
  reachable from tests, and `RemoteCliCommand::ServerStop` is unused outside
  tests.
- **Handshake leftovers.** `ServerMessage::Welcome` is only ever sent with an
  error; `do_handshake` takes geometry and surface size separately;
  `RawInputByteFramer` is public but only used inside `RawInputFramer`.
- **Stale names and comments.** The client's `graphics_scope` field and its
  comment in `shell/state.rs`; the `TERMINAL_ID_STAMP` comment in shepr-protocol
  (and whether `TerminalId` is still needed outside the server);
  `is_launch_fatal_setup_error`'s comment in shepr-remote; "saved machines"
  wording in `src/autodetect.rs` and client comments, now configured machines.
- **Preflight findings are dropped.** `src/preflight.rs` prints nothing for
  `MachineCheck::Incompatible`, and the client's connectors never call
  `check_saved_ssh`, so a remote server that is not a detached daemon is
  attached to anyway and its error never shown.
- **Discovery ignores the metadata cache.** `check_saved_ssh` runs full remote
  discovery for every machine at every launch and neither reads nor writes
  `SshMetadataCache`.
- **Metadata cache and remote discovery versus build profiles.** The cache sits
  in the shared client state directory, so dev and release clients overwrite
  each other's hint for a target; remote discovery only finds an installed
  `shepr` (PATH, `~/.cargo/bin`, `~/.local/bin`), so a dev client can never
  match a remote dev build.
- **Redundant build hash in the root package.** The root package appears to
  run the workspace `build.rs` too, hashing the tree and writing build id files
  nothing in `src/` includes.
- **Dead after the API pruning.** With `layout.apply` gone nothing passes an
  argv to a new tab or split (`Tab::new_argv_command`, `Tab::split_pane_argv`,
  the `argv` parameters of `create_tab_with_runtime` and
  `split_pane_with_runtime`, `PaneRuntime::spawn_argv_command`);
  `dispatch_to_app_result` in shepr-api `server.rs` keeps an unused no-timeout
  branch; `ApiErrorCode` lists variants nothing returns (`PaneClosed`,
  `UnsupportedMethod`, `ClientMissing`); the test
  `agent_state_sequences_track_transitions_for_waiters` is misnamed now that
  there are no waiters; `Start::Branch` in shepr-agent `resume.rs` is only used
  by a parse test.
- **More stale wording.** `current_process_is_detached_server_daemon`'s doc says
  remote attach restarts a non-detached server (it no longer does);
  `RunServerError::SessionDataHeld` talks about a session data directory.
- **Re-prompting for SSH authentication.** A machine that still needs
  authentication after a failed or skipped startup prompt is not prompted again
  until the next launch, and there is no TUI action to suspend the screen and
  authenticate. Add one if losing the shared connection mid-session proves
  annoying.

## Monitor upstream changes to integrations

We need to create a script we can run periodically that checks upstream
for changes and additions to the integration assets and detection manifests
(upstream's src/integration/assets/* and src/detect/manifests/*, ours under
crates/shepr-agent/src/integration/assets/ and
crates/shepr-agent/src/detect/manifests/) and anything else relevant.

Find out which commit we forked from first. Was it 21d0ce6?
https://github.com/herdrdev/herdr/commit/21d0ce60267ad947c081d3d3fba401c859f06dd2

## Confirm the opencode/Kilo permission-dialog labels

Do this the next time opencode or Kilo is in use.

- The `permission_required` rules in `crates/shepr-agent/src/detect/manifests/opencode.toml` and `kilo.toml` match "△ Permission required" only when one of the dialog's control labels is also on screen: "allow once", "allow always", "reject" or "enter confirm". Those labels were written from memory of opencode's TUI, not captured.
- If they are wrong, opencode/Kilo panes never show as blocked on a permission prompt; they read as working or idle while waiting on you.
- To check: in a shepr pane, get the agent to ask for a permission, run `shepr detect capture <pane>`, and compare the dialog's labels with the gate. Fix the manifests if they differ.

## Finish absolute rows in copy mode

Absolute rows are live: `PaneSurfaceScrollMetrics` carries `history_origin`, mouse selections (`shepr_vt::selection::Selection`) and the copy-mode selection anchor (`ClientCopySelection`) store `AbsRow`, and `pane.selection.read` takes `PaneSelectionPoint` with absolute rows. What remains:

- Copy-mode cursor and search matches still name screen rows. `ClientCopyModeState.cursor` and `search_matches` are `PaneTextPoint`/`PaneTextRange` (`crates/shepr-api/src/schema/panes.rs`), whose `row` is a `ScreenRow`; `pane.copy.motion` and `pane.copy.search` in `crates/shepr-server/src/app/api/panes/copy.rs` convert with the origin read at request time and lean on `content_revision` to catch drift (optional for motion). Move them to `AbsRow`, have the server call the absolute readers, and drop the `expect(dead_code)` on `word_motion_target_absolute` and `paragraph_motion_target_absolute` (`crates/shepr-mux/src/pane/terminal.rs`), which only tests use today.

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
