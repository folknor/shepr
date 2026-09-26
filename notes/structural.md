# Structural

Shape and placement findings from the 2026-09-26 design hunt: axes that should
be types, and moves, splits and rewrites with the payoff each buys.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

# Axes that should be types

## STR-001 - Ids travel as bare strings and integers

- App: workspace, tab, pane and terminal ids as `String` in `TerminalTarget.terminal_id`, `TerminalTargetCandidate`, `PaneFocusTarget.workspace_id`, `TabViewer::Tab{workspace_id}`, `public_pane_id_aliases: HashMap<String, PaneId>` and every API response; `TabViewer::Tab{tab_number: usize}`, `Tab.number: usize`. `TerminalTarget.terminal_id` is a `TerminalId` flattened with `to_string()` and found again by scan (STR-058).
- Server: client ids as `u64` in `clients`, `foreground_client_id`, `tab_geometry_controllers`, `terminal_attach_owners`, `sent_window_title`; `tab_geometry_controllers: HashMap<String, u64>`, `terminal_attach_owners: HashMap<String, u64>`; `ClientConnectionMode::TerminalAttach { terminal_id: String }` while `AltScreenReadSpec` uses `TerminalId`; `ClientShellLocation`/`ClientShellTopology` are `String → String` maps; `next_activity_stamp`/`last_activity` bare `u64`; `client_shell_boot_id` a `pid-nanos` string built inline.
- Protocol: `pane_id`, `workspace_id`, `tab_id`, `boot_id`, `terminal_id`, `request_id` as `String` in `ClientShellWorkspace/Tab/Pane/Agent`, `PaneSurfacePane`, `ClientShellPaneInput`, `ClientShellEndpointRequest`; `ClientShellAgent` holds three sibling id strings.
- Client shell: `AgentRow.pane_id`, `hits.agents: Vec<(Rect, String)>`, `dragged_workspace_id: Option<&str>`.
- Remote: `boot_id`/`request_id` strings; `EndpointCommandLane.retired: VecDeque<(u64, String, String)>` (proposed `RequestKey { generation, boot_id, request_id }`, which `InFlightCommand` has inline).
- PTY actor `pane_id: u32` should be `PaneId`; `PaneLaunchIdentity::Managed` holds three `String` ids.

Proposed: `WorkspaceId`, `PublicTabId`, `PublicPaneId` (with `Display`/`FromStr`,
CON-027), `ClientId` minted only by the accept path, `ActivityStamp`, `BootId`,
`RequestId`, and `TerminalId` everywhere. `BootId` matters most on the wire:
baseline checks compare boot strings.

Reported by: app-state, server, protocol, ui, remote, terminal-core, pane-detection.

## STR-002 - Indexes stand in for identity

`ws_idx: usize`, `tab_idx: usize`, `active: Option<usize>`, `selected: usize`
flow through every API handler and `PaneStateUpdate.ws_idx`;
`public_workspace_id` returns `""` on a stale index; `ids.rs` comments on stale
indexes. `TabSurfaceTarget { workspace_index, tab_index }` (`ui/tab_surface.rs:10`)
and `resize_tab_surface(app, _, workspace_index, tab_index, …)`. Proposed: carry
typed ids resolved at the boundary, or a generational `WorkspaceKey`; at least
`WorkspaceIndex`/`TabIndex` newtypes. See BUG-009.

Reported by: app-state, ui.

## STR-003 - Three row coordinate spaces share bare integers

Viewport rows, screen rows (0 = oldest retained) and absolute rows
(`history_origin` + screen row) all travel as `u16`/`u32`/`u64`/`usize`.
`Selection` stores `anchor`/`cursor` as `(u64, u16)`; both constructor families
(`anchor` vs `anchor_at`, `drag` vs `drag_at`, `contains` vs `contains_at`) write
the same fields; `absolute_row_for_viewport` returns a screen row ("Despite the
name, not an absolute row"); `ordered_cells` saturates absolute rows to `u32`;
`Terminal::screen_line(u64)`, `viewport_line(u64)`, `screen_cell(x, y: u32)`,
`screen_row_for_absolute(u64) -> usize`, `read_*_{screen,viewport}(start: (u16,
u32), ...)`. Proposed: `ViewportRow`, `ScreenRow`, `AbsRow` newtypes plus
`Point<R>`; `Selection` generic over the space or `AbsRow`-only; delete the
`ScrollMetrics` family. The terminal-core hunter calls this the most important
item in its scope.

Reported by: terminal-core.

## STR-004 - Grid and pixel geometry travel as bare tuples

- Terminal core: `(cell_width_px, cell_height_px)` as bare `u32` pairs in `Terminal`, `CoreHandler`, `Terminal::resize`, `PtyIoActorHandle::resize`, `PtyResize`, `TerminalRuntime::resize`; the resize signature flips from `(rows, cols, w, h)` to `(cols, rows, ...)` in `Terminal::resize`; `HostCellSize` exists but is unused there.
- Pane: `current_size: Cell<(u16, u16, u32, u32)>` and the resize watch channel; `clamp_pane_size` returns `(u16, u16)` (the clamped size should be a type).
- App: `headless_size: (u16, u16)` next to `sole_pane_size()` returning `(rows, cols)`; `App::new` destructures `(headless_cols, headless_rows)` then `(restore_rows, restore_cols)`.
- Server: `headless_size`, `effective_size`, `ClientConnection::terminal_size` as `(u16, u16)`; `host_keyboard_protocol_active: Option<(u16, u8)>`.
- Client: `(u16,u16,u32,u32,bool)` in `TerminalGeometry` alias (`terminal_geometry.rs:59`), `ClientLoopEvent::Resize` (`events.rs:7`), `last_size` in `resize_poll_loop`; `reported_size: (u16,u16)`, `reported_cell_size: (u32,u32)`; positional `cols, rows, cell_width_px, cell_height_px, exact_cell_size` to `do_handshake`/`run_client_loop`; `input::mouse::HostGeometry` is another geometry type.
- Config: `headless_size() -> (u16, u16)` validated by a separate diagnostic; `validated_sidebar_bounds -> Option<(u16, u16)>`.

Proposed: one `PaneGeometry`/`HostGeometry { grid: GridSize, cell: Option<CellPx> }`
with "exact" part of the value, `GridSize` of non-zero values, `SidebarBounds`.
See CON-004.

Reported by: terminal-core, pane-detection, app-state, server, client, config-cli.

## STR-005 - Terminal modes, keyboard protocol levels, cursor shape and modifiers as integers

DEC private modes as bare `u16` (`mode_get`, `mode_set`, `MODE_*`, CON-011);
modifyOtherKeys level `u8` in `ExtraModes`, `ScanEvent::ModifyOtherKeys`,
`set_direct_host_keyboard_protocol`; kitty flags `u8` in
`Terminal::kitty_keyboard_flags` but `u16` in `DirectHostKeyboardState`;
`terminal_modes.rs` ORs `0b0001_0000` by hand (crossterm lacks the flag).
`CursorShapeParam = u8` alias for DECSCUSR 0..=6 on the wire and
`cjk_ime_cursor_shape: u8` in app. `modifiers: u8` (crossterm bits truncated) in
`ClientPaneInputEvent::Key/Mouse`, `AttachScroll`, `AttachMouse`. Proposed: mode
enum with total mapping, level/flag types, a DECSCUSR enum, `WireModifiers`
bitflags.

Reported by: terminal-core, protocol, app-state.

## STR-006 - Cell style and wire style are untyped

`CellStyle.underline: u8` ("0 none, 1 single...") beside a redundant
`underlined: bool`; `blink` and `overline` always false. On the wire,
`CellData.fg`/`bg: u32` use a private tag scheme (0x00 named, 0x01 indexed, 0x02
RGB) where an unknown tag decodes to `Reset`; `modifier: u16` packs ratatui bits
plus a 4-bit underline in bits 12-15. Proposed: `UnderlineStyle` enum, drop dead
fields, `WireColor` enum and a `WireStyle { flags, underline }` owning the layout
(CON-038).

Reported by: terminal-core, protocol.

## STR-007 - Revisions and generations as bare u64

`projection_revision`, `surface_revision`, `base_surface_revision` side by side in
`PaneSurfaceFrame`, `PaneSurfacePatch`, `SurfaceDelta`, `SurfaceReuse`, compared
by hand. Connection generation bare `u64` in supervisor, registry, commands and
the selection tracker. Proposed: `ProjectionRevision`, `SurfaceRevision` with
`next()`, `ConnectionGeneration`.

Reported by: protocol, remote.

## STR-008 - Terminal event payloads that are domain values

OSC 7 working directory and OSC 9;4 progress come out as `Vec<u8>`; the title
update is `Option<Option<String>>`; `clear_screen() -> bool` means "refused
because the alternate screen is active"; `TerminalRuntime::clear_screen` returns
`Result<(), String>`.

Reported by: terminal-core.

## STR-009 - Terminal-core and PTY errors

`ghostty::Error(&'static str)` cannot be branched on; most `Result<_, Error>`
returns (`cols`, `rows`, `scrollbar`, `default_palette`, `kitty_keyboard_flags`,
`RenderState::new`, `RowIterator::new`, ...) cannot fail and are libghostty
leftovers. PTY submission outcomes reach callers as `io::Error` with overloaded
`ErrorKind` (`TimedOut` = withdrawn by caller, `BrokenPipe` = actor closed).
Proposed: infallible signatures and a typed `SubmissionError`.

Reported by: terminal-core.

## STR-010 - API errors are prose plus string codes

`ReadRejection = (&'static str, String)`; `collect_panes_for_workspace ->
Result<_, (String, String)>`; `normalize_launch_env`; `close_pane(...) ->
Result<(), String>` whose `Err` is already-encoded JSON; handlers return `String`
from `encode_error(id, "pane_not_found", ...)`; `ApiRequestMessage::respond_to`
is `Sender<String>` (`src/api/mod.rs:67`); hand-written JSON literals
(`headless.rs:2186`); error-code literals at ~60 sites (`"server_unavailable"`,
`"internal_error"` ~10 times, `"agent_not_found"` in `app/agents.rs` and
`api/wait.rs`, `"invalid_request"` in `api/server.rs` and `client_transport.rs`);
the SSH-agent path (`api/server.rs:364-383`) maps `io::ErrorKind` to codes.
Proposed: `ApiErrorCode` enum and `ApiError` with typed payloads; handlers return
`Result<ResponseResult, ApiError>`, serialised once at the socket boundary; tests
assert typed results instead of parsing JSON back. See BUG-010.

Reported by: app-state, server.

## STR-011 - Protocol and client failures are prose

`check_client_version -> Result<(), String>`; `Welcome.error: Option<String>`;
`ClientShellError { message }`; `ServerShutdown.reason: Option<String>` with the
magic `"detached"` matched at `client/mod.rs:302-307` (exit code) and
`errors.rs:38` (message); `DirectTerminalNotice { message }` covering three
causes; `EndpointHandshakeError.code: String`; `surface_reuse::Decoder::decode ->
Result<_, String>` and every surface_delta function; `ClientError::Protocol(
FramingError::Io(io::Error::new(InvalidData, "expected endpoint welcome")))`
built in five places, making malformed welcome, missing preamble, surface decode
failure and endpoint-control failure indistinguishable; `ClientShellEndpointError.code:
Option<String>` with `"endpoint_timeout"` / `"endpoint_response_too_large"`.
`PreambleError` is named as the model. Proposed: `HandshakeRefusal`,
`NoticeKind`, `SurfaceDecodeError`, `ShutdownReason` enums. See BUG-018.

Reported by: protocol, client, remote.

## STR-012 - CLI and session errors

CLI failures to machine callers are ad hoc JSON with string codes
(`print_session_error("session_stop_failed", ...)`); `server_not_running` /
`protocol_mismatch` markers carried inside `io::Error` and recovered in
`main::finish_cli` via `was_reported` downcasts. `session.rs` returns
`Result<_, String>` for every failure ("not running", "timed out with sockets
still reachable", name mismatch on a case-insensitive FS, "is running, stop
first"). `SessionInfo` stores `socket_path`/`session_dir` as pre-formatted
`String`s and is serialised straight to CLI JSON. Proposed: `CliError` with one
exit-code mapping and printer, `SessionError`, `PathBuf` in the domain type.

Reported by: config-cli.

## STR-013 - EndpointControl is a stringly side channel inside the typed protocol

`EndpointControl { kind: String, data: String }` with nine or more `kind`
constants (eight in `endpoint.rs`, one each in `surface_delta.rs` and
`surface_reuse.rs`); payloads are JSON or base64 of the positional codec; the
client compares against `PRESENTATION_EFFECTS_READY_KIND` (`client/mod.rs:1244`)
and `ENDPOINT_WELCOME_KIND`. Costs listed: JSON encode of `ClientShellSnapshot`,
codec + base64 + codec string for surface delta, a string compare per dispatch,
"unknown kind" as a runtime condition. Proposed: real `ServerMessage` /
`ClientMessage` variants for hello, welcome, snapshot, health ping/pong,
presentation sync/ready, agent completions, surface delta, surface reuse; most
of `endpoint.rs` then disappears.

Reported by: protocol, client.

## STR-014 - Agent identity and session references in resume

`agent: &str` and `source: &str` beside `detect::Agent` in `AgentResumePlan`,
`PersistedAgentSession`, `plan`, `session_ref_from_report`,
`session_ref_from_snapshot`, `is_official_agent_source` (swappable pair);
`persisted_session_from_launch_args` builds `"shepr:codex"`/`"codex"` by hand.
`AgentSessionRef { kind, value: String }` has public fields, bypassed by the
letta test (`value: "default:--yolo"`, BUG-034) and trusted by `plan()`; the letta
`default:<agent_id>` sub-grammar is parsed at plan time.
`normalize_session_start_source` returns `Option<String>` over 8 values;
`dedupe_key` is a `format!`/`{:?}` string joined by NUL. Proposed:
`Source::Official(Agent) | Custom(String)`, `enum AgentSessionRef { Id(SessionId),
Path(AbsSessionPath), ... }` with private newtypes, a start-source enum, a derived
key struct.

Reported by: pane-detection.

## STR-015 - Pane runtime primitives

`publish_state_changed_event(visible_blocker: bool, process_exited: bool)`
positional; `apply_agent_detection_publish_update` takes 7 separate `&mut`
fields; `ProcessProbeInput` is 5 unnamed bools/Options;
`ProcessProbeResult { agent: Option<Agent>, process_name: Option<String> }` can
hold a name with no agent; `MANAGED_AGENT_RESUME_TIMEOUT` lives in `pane.rs`
and the resume hold travels as `agent_absence_startup_hold: bool` (proposed
`LaunchPurpose::{Fresh, AgentResume}`).

Reported by: pane-detection.

## STR-016 - EventEnvelope carries its discriminant twice

`EventEnvelope { event: EventKind::X, data: EventData::X{..} }` can disagree.
Derive the kind from `EventData`. Lives in `api/schema`, constructed across
`src/app/`.

Reported by: app-state.

## STR-017 - App-state flags and snapshots

`cjk_ime_agent_filter_configured: bool` plus `cjk_ime_agents: Vec<Agent>`
(proposed `AgentFilter { Any, Only(Vec<Agent>) }` validated at load, BUG-004);
`PaneStateUpdate` has 16 previous/current fields plus `agent_released`,
`agent_name_changed`, `suppress_completion` (proposed `Snapshot` pair plus a
cause enum); `AppPolicy{restore_session, persist_session}` two bools with one
production combination; `ahead_behind: Option<(u32, u32)>` (and
`git_ahead_behind: Option<(usize, usize)>` on the wire).

Reported by: app-state, protocol.

## STR-018 - Split ratios and paths

Split `ratio: f32` unvalidated; `SplitBorder.path: Vec<bool>` and wire
`PaneSurfaceSplit.path: Vec<bool>` (proposed `Vec<Side>`/`Vec<Branch>`); sidebar
`split_ratio: f32` clamped silently in `sidebar_section_heights` (proposed
`SectionSplit` validated at config and drag time).

Reported by: app-state, protocol, ui.

## STR-019 - ClientConnection state that only means something in one mode

`ClientConnection` is ~25 loose fields; `shell_surface_active`,
`shell_mouse_capture`, `shell_location`, `shell_snapshot`,
`shell_agent_completions`, `shell_projection_revision`,
`shell_endpoint_command_in_flight`, `shell_uses_endpoint_keybindings`,
`shell_held_inputs` only mean something for shell clients;
`host_keyboard_protocol_active` only for attach; the constructor forces
`shell_surface_active` to stay truthful for a terminal stream. `RenderTarget`
(`clients.rs:17`) is `(u64, (u16,u16), HostCellSize, bool, ClientConnectionMode)`.
`writer: Option<ClientWriter>` exists only for test fixtures, with
`writer.is_none()` checks in production. Proposed:
`ClientConnectionMode::Shell(ShellState) | TerminalPending |
TerminalAttach(AttachState)`, a `RenderTarget` struct, a non-optional writer with
a test channel.

Reported by: server.

## STR-020 - Client-side primitives

Cell size packed into `AtomicU64` as `width<<32 | height` with 0 = absent
(`pack_cell_size`/`unpack_cell_size`; proposed `AtomicCellSize`);
`HandshakeResult` is a unit struct callers ignore; pending request ids
classified by `request_id.starts_with("client-shell-surface:")`
(`client/mod.rs:1123`); `mouse_scroll_lines: usize` clamped to `u16` ad hoc
(`client/mod.rs:735`); the stdin framer outputs `Vec<Vec<u8>>` (CON-007);
`run_client_with_mode(attach_request: Option<(String,bool)>, attach_escape:
Option<AttachEscapeState>)` discards the passed state (proposed
`ClientMode::{Shell, Attach{terminal_id, takeover}}`).

Reported by: client.

## STR-021 - Sidebar and token primitives

`metadata_tokens::accept_sequence -> Result<bool, ()>` hides three outcomes
(proposed `SequenceOutcome { Accepted, Stale, TooManySources }`);
`resolved_token_spans` takes 5 positional `Style` parameters and call sites pass
`secondary` twice (proposed `TokenStyles`); `acknowledge_surface(…,
outer_focused: Option<bool>)` tri-state; `contains(rect, (u16, u16))`;
`state_labels` and `tokens` as `Vec<(String, String)>`, rebuilt into a `HashMap`
per row per frame (`agent_sidebar.rs:255-260`, `sidebar.rs:488`) and keyed by
status text (proposed: keyed by the status enum). Wire `tokens`/`state_labels`
and `ClientHostThemeUpdate::PaletteColors(Vec<(u8, ClientHostColor)>)` are the
same shape.

Reported by: ui, protocol.

## STR-022 - Session identity and server address are ambient

Session identity is `Option<&str>`/`Option<String>` with `None` = default,
through `data_dir_for`, `api_socket_path_for`, `client_socket_path_for`,
`stop_session`, `session_info`, `SessionInfo.name`; `session::configure` mutates
process env (`SHEPR_SESSION`) plus a global `AtomicBool`
(`EXPLICIT_SESSION_REQUESTED`), and `active_name()` re-reads and re-validates
env on every call. Socket choice is decided from an explicit flag,
`SHEPR_SOCKET_PATH`, `SHEPR_CLIENT_SOCKET_PATH` and the session. Proposed:
`SessionId { Default, Named(SessionName) }` and `ServerAddress { Session(SessionId),
Override { api, client } }` with `api_socket()`, `client_socket()`,
`data_dir()`, `stop_command()`, `attach_command()`, resolved once in `main`;
`SHEPR_SESSION` written only into child env. See CON-042, CON-047, STR-037.

Reported by: config-cli.

## STR-023 - Remote machine metadata

`SshMachineMetadata.os: String`, where only `"linux"` is valid, is an upstream
leftover in a Linux-only fork; proposed deletion. `SshTarget`,
`RemoteExecutable`, `ProfileId` and `SessionName` typing are covered by CON-047,
CON-050, CON-051, CON-052. `RemoteServerStatus::Running`'s capability bools and
`remote_server_restart_reason(protocol, bool, bool, bool, bool)` - the remote
hunter proposes deleting most of them (STR-053) rather than typing them.

Reported by: remote.

# Moves, splits and rewrites

## STR-024 - Rename src/ghostty to vt, split it, delete the libghostty shims

`mod.rs` (~2000 lines) holds the colour model, palette, cell/style types,
`Terminal`, the `RenderState` snapshot, iterators and text readers; proposed
`color.rs`, `cell.rs`, `render.rs`, `read.rs` around `Terminal`. Delete
`RowIterator`/`RowCells` (no state), `populate_row_iterator`/`populate_cells`
(ignore their argument), `selection()` (always `None`), `content_bg_color()`
(always `None`), the unused `bytes` scratch in `grapheme_text_into` and the
infallible `Result`s; replace with a `for row in state.dirty_rows()` iterator
returning cell views. Payoff: the module-wide `#![allow(dead_code)]` can go.

Reported by: terminal-core.

## STR-025 - Host-terminal files are loose top-level modules

`terminal_theme.rs`, `terminal_modes.rs`, `terminal_effects.rs`,
`terminal_cell_size.rs` concern the outer (host) terminal, and their names
collide with pane-terminal concepts in `ghostty/`. Proposed
`host_term/{theme,modes,title,cell_size}.rs`; the OSC colour-response parsing in
`terminal_theme` is client input-side parsing; host and pane colours share one
`Rgb` from vt (CON-008). Theme merging on `ClientConnection` belongs in
`terminal_theme`.

Reported by: terminal-core, server.

## STR-026 - src/terminal wraps PaneRuntime and inverts the layering

`src/terminal/` holds server-side terminal identity, state and registry plus
`TerminalRuntime`, a pass-through newtype over `crate::pane::PaneRuntime` ("still
delegates to the legacy pane runtime while the migration proceeds"), giving a
`terminal -> pane` edge while `pane` consumes `ghostty`. The three `spawn*`
pass-throughs with `too_many_arguments` exist only because of the wrapper.
Proposed: finish the migration (move `PaneRuntime`'s body into `terminal`) or
delete the wrapper; `title.rs` belongs in detection (CON-001).

Reported by: terminal-core.

## STR-027 - selection.rs does selection geometry and clipboard delivery

Clipboard delivery (OSC 52, WSL detection reading `/proc` and env,
`platform::write_clipboard`) sits beside selection geometry; selection depends
upward on `pane::ScrollMetrics` and `ratatui::Rect`. Proposed: WSL/SSH env
sniffing to `platform/`, OSC 52 encoding to host-term, selection `AbsRow`-only
(removes the `ScrollMetrics` dependency).

Reported by: terminal-core.

## STR-028 - PTY actor: submission logic as its own module

The ~400 lines of submission logic in `pty/actor.rs` could be a `submission.rs`
state machine separate from the fd IO loop (CON-013); three `PtyIoActorRunner`
literal constructions in tests want a builder. The hunter judges the PTY
boundary itself fine.

Reported by: terminal-core.

## STR-029 - Split pane.rs and give the detector its own state

`pane.rs` (4300 lines) holds launch env policy, shell resolution, the
process-probe / agent-presence state machine, event publishers, the Codex prompt
special case, session teardown (global `PANE_TEARDOWNS_IN_FLIGHT` counter plus
signal escalation), the sync-timeout render scheduler and `PaneRuntime`.
Proposed `pane/launch.rs`, `pane/teardown.rs`, `pane/process_probe.rs`,
`pane/runtime.rs`. The detector task's state lives in `&mut` locals; a
`DetectorState` struct with methods would let the detection loop be unit-tested
without a runtime. Agent-specific code moves to the per-agent descriptor
(CON-001).

Reported by: pane-detection.

## STR-030 - Integration layout and dependency direction

`integration/registry.rs` depends on `crate::api::schema::IntegrationTarget`, so
the wire schema owns the domain enum; proposed inversion. ~60 flat per-agent
constants in `integration/mod.rs` hand-assembled into `[..; 18]`; adding an agent
touches mod.rs, registry, targets, agent_resume and detect. Proposed: per-target
module or `IntegrationSpec` bundling asset, version, events and path. With a
descriptor table, `agent_resume.rs`'s large table tests become one loop over
`Agent::ALL`.

Reported by: pane-detection.

## STR-031 - Make every mutation an AppState command with a typed outcome

Production mutations live in `App` methods in `api/panes.rs` (4,747 lines) while
`actions.rs` holds `#[cfg(test)]` twins (`AppState::close_pane`, `close_tab`,
`toggle_zoom`, ...), so the pure-AppState tests partly exercise code production
never runs. Proposed rewrite: every mutation is an `AppState`/`Workspace` command
returning a typed outcome; `App` applies runtime side effects (spawn, shutdown,
events) from it; API handlers and keybindings translate into commands. The
app-state hunter says this dissolves CON-020, CON-021 and CON-024.

Reported by: app-state.

## STR-032 - API request handling has no single home

`app/api.rs` is mostly internal `AppEvent` handling (PaneDied checkpointing, git
refresh results, graceful release, detection pauses; proposed `app/events.rs`).
`api/panes.rs` does ~15 jobs (split, read, copy-mode, metadata, agent reports,
move, resize, zoom...; proposed split by domain). `src/api/` holds schema and
socket server while execution lives in `src/app/api/` and partly
`server/headless.rs`; `api::request_changes_ui` is tested from `app/`; `api`
knows `crate::session::active_api_socket_path`. Proposed: `api/` is pure schema
plus transport, with one `api::handle(&mut App, Request) -> Outcome`. The
`(Ansi, Detection)` fallback arm in `read_validated_terminal_snapshot` exists
only because validation and read are split (proposed `ValidatedRead`).

Reported by: app-state, server.

## STR-033 - Palette and theme catalogue live in app/state.rs

~600 lines of colour tables plus `palette_from_config` and `ui_accent_override`
(`app/mod.rs`) belong in `ui/theme` or `config`. `client/shell.rs` imports
`crate::app::state::Palette`, so the client depends on the server's app state
module.

Reported by: app-state, client.

## STR-034 - layout.rs depends on ratatui render types

`PaneInfo.borders: Borders` and `scrollbar_rect` are UI chrome in the BSP tree
module. Proposed: pure tree (rects, ratios), chrome added in
`workspace/geometry.rs` or `ui`.

Reported by: app-state.

## STR-035 - App carries ~35 flat scheduler fields

Git refresh flags, deadlines, the session save thread, tab bar runtimes.
Proposed `GitRefreshScheduler`, `SessionSaver`, `TabBarStatus`, each owning its
deadline logic.

Reported by: app-state.

## STR-036 - HeadlessServer is a god object

`HeadlessServer` owns the listener, client registry, foreground policy,
tab-geometry arbitration, attach ownership, window-title state, config
diagnostics, alt-screen read queues, dirty flags, shutdown/freeze state, signal
flags and event channels; `headless.rs` exceeds 2200 lines with 8 submodules
split by activity rather than owned state. Proposed rewrite: `ClientRegistry`
(ids, foreground, activity, geometry controllers, attach owners; pure, testable
like `AppState`), `ApiDispatcher` (routing, deferral, alt-screen reads, typed
responses), `ShutdownController`, and a thin loop. Held-input tracking
(`track_shell_input`) moves to `pane_input.rs`. Payoff named: most of the ≥3400
lines of whole-server tests in `headless/tests/mod.rs` become unit tests.

Reported by: server.

## STR-037 - session.rs and socket paths

`session.rs` does identity/selection, path layout, a raw-socket stop RPC client
with its own deadline arithmetic, and user-facing command text. Socket-path
ownership is split between `api::socket_path()` (delegates to `session`),
`server/socket_paths.rs` and `api::SOCKET_PATH_ENV_VAR`;
`session::stop_active_server` calls `crate::server::socket_paths` (CLI-side
module depending on the server layer). Proposed: identity and layout in a
`ServerAddress` module absorbing `socket_paths.rs` (STR-022); the stop RPC as
`ApiClient::stop_unchecked(deadline)`; guidance text to `cli`.

Reported by: config-cli, server.

## STR-038 - Parse the command line once into a typed Launch; drop CLI thread-locals

`Invocation::command_name()` is string-matched in `main`; `bridge_args()`
round-trips flags into `Vec<String>` for re-parsing; `CommandOutcome::NotCli`
and the two-pass `cli::run` → `main`. Proposed `enum Launch { Tui, Server,
Client, ApiBridge, ClientBridge, Cli(Command) }` with command locality on the
spec (CON-044). `cli/target.rs` keeps the `--machine` target and
`PROTOCOL_CHECKED` in `thread_local!` swapped by a Drop guard; proposed an
explicit `CliContext`/`ApiTarget` passed into `dispatch`, making `is_remote()`,
`caller_pane()`, `restart_guidance()` pure functions.

Reported by: config-cli, remote.

## STR-039 - Config: validate once, move DEFAULT_CONFIG, move profile TOML

`ValidatedConfig` built once at load with typed diagnostics (CON-031, CON-045).
`DEFAULT_CONFIG` (290-line literal in `main.rs`) to `config/default.toml` via
`include_str!`; help generated from clap; `main` becomes parse → resolve target
→ dispatch. `config.rs` mixes validation, TOML profile publishing
(`local_keybindings_profile_toml`, `keybindings_from_profile_toml`, a remote
keybinding wire concern) and diagnostic filtering; the profile code belongs with
the remote keybindings feature or `keybinds.rs`.

Reported by: config-cli.

## STR-040 - The config dir is also the data root

`session::data_dir_for(None)` equals `config_dir()`, so sockets, `session.json`,
history, locks and logs live in `~/.config/shepr`. `state_dir()` exists and is
exported but unused for session data. Proposed: sockets in `$XDG_RUNTIME_DIR`,
session files in the state dir - a breaking layout change.

Reported by: config-cli.

## STR-041 - Split platform/mod.rs along its own section headers

2182 lines with headers `Status commands`, `Foreground job detection`, `SSH
paths`, `Config file replacement`, `Remote bridge stdio`, `Local client
streams`. Proposed a flat directory (`proc_tree.rs`, `ssh_paths.rs`,
`client_stream.rs`, `private_file.rs`, ...); commit f3b5436's "flat platform
layer" read as "no per-OS tree", not "one file". Domain rules move out: the
foreground-job `/proc` walker (detection), `StatusCommandGuard` (tab-bar command
runner), `is_pane_shell_process_name` and `shell_quote`.
`persist::io::publish_private_file` calls `platform::create_config_temporary(pending,
true)`; the `private: bool` hides two policies (proposed
`create_private_temporary`). Remote's `store_private_json` (temp name
`.endpoints-`) is another private-file writer that belongs here.

Reported by: config-cli, remote.

## STR-042 - Collapse the three surface update encodings into one

Today: typed `PaneSurfacePatch` ("legacy" in tests), `surface_delta` (base64 of
codec in `EndpointControl`), `surface_reuse` (JSON in `EndpointControl`), with
three baseline rule sets (CON-032), a hand decoder (CON-035) and a pairwise
mirror (CON-034). Proposed single `ServerMessage::SurfaceUpdate { base, next,
projection, meta: Option<SurfaceMeta>, spans: Spans }`, empty spans meaning
reuse, full grids still `PaneSurface`. Removes most of `surface_delta.rs`,
`decode.rs` and `surface_reuse.rs`. The protocol hunter's headline
recommendation.

Reported by: protocol.

## STR-043 - Protocol modules should be leaves

`wire.rs` holds limits, input event types plus crossterm conversions (depending
on `crate::input`, `crate::raw_input`), ClientShell projection types (depending
on `crate::api::schema::AgentStatus`), frame/cell/ratatui conversion, framing and
the version check. Proposed split into `limits`, `input`, `projection`,
`surface`/`frame`, `style`, `framing`, with conversions living in client and
server. `render_ansi.rs` (`BlitEncoder`, a stateful ANSI diff renderer) belongs
in a `term_out`/`blit` module. `render_signal.rs` is a server render-scheduling
primitive keyed by `layout::PaneId` and belongs in `server/`. Hyperlinks are
passed as `&[((u16,u16), String, String)]`, matched by symbol equality, and the
position map is rebuilt per frame (two HashMaps, clones of every linked symbol
and URI) on the render hot path.

Reported by: protocol.

## STR-044 - Rewrite the client loop around a ClientLoop struct and SessionMode

`run_client_loop` is ~1000 lines with one `match`; loop state lives in locals
(`write_stream`, `pending_activation`, `scheduled_activation`,
`endpoint_commands`, `supervisors`, `selection`, `catalog_watch`, `federated`,
`next_surface_serial`, three atomics), so `shell_runtime.rs` functions take 7-10
`&mut` parameters (`begin_endpoint_activation` takes 10). Shell vs direct attach
is `state.shell.is_some()` / `state.attach_escape.is_some()` re-checked in ~15
branches. Proposed: `ClientLoop` with one method per event, `SessionMode {
Shell(ShellSession), DirectAttach(AttachSession) }`, and `ClientState` split into
`Presenter`, `HostModes` and the session type. A `Presenter<W: Write>` removes the
`#[cfg(test)]` `io::sink()` in production code (`state.rs:250-253`).

Reported by: client.

## STR-045 - Client input plumbing

`client/input.rs` module doc is stale ("the server handles semantic parsing…");
the initial-host-input block (lines 82-131) duplicates the loop's flush and
held-escape logic; `stdin_reader_loop` is a leftover wrapper around
`unix_stdin_reader_loop`. `raw_input.rs` is crate-root and mixes host-reply
tracking (awaited colour and cell-size replies, appearance-on-focus) into a
generic byte framer; proposed move to `src/input/` with host-reply accounting in
a client-side wrapper. `transport::start_endpoint_transport` spawns a named
reader thread while `client/mod.rs:882` spawns the same `server_reader_thread`
unnamed with duplicated arguments.

Reported by: client.

## STR-046 - Client error messages read env at format time

`client/errors.rs` depends on `crate::server::socket_paths` and
`crate::session`; `Display` reads env vars and the socket path when formatting,
making messages nondeterministic in tests. Proposed: build the message at
construction time.

Reported by: client.

## STR-047 - crate::ui does two unrelated jobs; metadata_tokens is not UI

`src/ui/` renders only the tab surface; `ui/sidebar.rs` and
`ui/sidebar/tokens.rs` are a token-layout kit only the client shell calls
(`sidebar_agent_rows`, `resolved_token_spans`, `expanded_sidebar_sections`).
Proposed: move the sidebar kit into `client/shell/` (removing the `client::shell
-> crate::ui` edge). `metadata_tokens.rs` is server-side report sequencing and
TTL state depending on `terminal::state`; proposed home next to
`terminal/state` or in `workspace/`. `compute_view_*` and
`compute_tab_surface(resize_panes: bool)` resize PTYs (including background
tabs) as a side effect; proposed: return the layout and apply sizes in a separate
`apply_pane_sizes(layout)`.

Reported by: ui.

## STR-048 - Restructure client/shell

`client/shell.rs` mixes pane-input batching, an inline FNV topology hash, the
status presentation table and frame blitting. `client/shell/` has ~27 flat
modules on overlapping axes (`endpoint_*` × {agents, sidebar, navigation,
notices, agent_state} beside `agent_sidebar`, `sidebar`,
`aggregate_navigation`); the local/endpoint split mirrors history. Proposed
`presentation/`, `sidebar/` (one implementation over N endpoints, CON-057),
`navigation/`, `input/`, `overlays/`. `pub(crate) use state::*` re-exports make
the boundary porous. The client hunter raised whether `client/shell/`
duplicates `app/` rendering and copy-mode logic; not yet reviewed.

Reported by: ui, client.

## STR-049 - Break the remote / client::endpoint cycle with a machine module

`remote` imports `client::endpoint::{SshMachineMetadata, SshMetadataCache,
EndpointCatalog}`; `client::endpoint` imports `remote::{SavedSshConnector,
SavedSshSettings, SavedSshBridge, saved_ssh_failure_needs_attention}`. The
catalog is used by the CLI and `remote::run_remote` too. Proposed: `ProfileId`,
`SavedSshEndpoint`, `EndpointCatalog` (profiles only), `EndpointCatalogWatch`,
`EndpointCatalogChanges`, `SshMetadataCache`, `SshMachineMetadata` into a neutral
`src/machine/`; `remote/` becomes SSH transport, discovery and bridge;
`client/endpoint/` becomes runtime supervision.

Reported by: remote.

## STR-050 - Split machine selection out of the catalog

`EndpointCatalog.selected_profile` is `#[serde(skip_serializing)]` + `default`,
ignored on load, set from a second file, and validated inside `validate()`, so
storing profiles fails on a stale in-memory selection. Proposed:
`MachineCatalog` (persisted, shared) and selection owned by
`EndpointSelectionTracker` (resolves CON-053).

Reported by: remote.

## STR-051 - Split remote/attach.rs

~2000 lines doing the `--remote` launcher, one-shot saved checks, remote
command construction, managed ssh config, the process-global
`TeardownRegistry`, the `RemoteSsh` runner, executable discovery and the API
discovery script, server status/restart prompt/stop-wait, the stdio bridge pump
and error types. Proposed `remote/ssh.rs`, `remote/discovery.rs`,
`remote/server_lifecycle.rs`, `remote/bridge.rs`, `remote/launch.rs`, leaving
`saved.rs` as the saved-machine connector.

Reported by: remote.

## STR-052 - Endpoint subsystems at the client root

`endpoint_commands.rs` and `endpoint_selection.rs` belong under
`client/endpoint/`.

Reported by: remote.

## STR-053 - Delete the upstream remote compatibility fossils

`restart_policy.rs` (SurfaceInterest, HealthCheck, DaemonDetach reasons for
servers "started by an older shepr build"), the `capabilities` JSON, the
`require_surface_interest` plumbing and catalog load in `run_remote`, the
`remote-api-bridge --check` string `shepr-api-bridge-v1`, herdr-era prompt text
("predates Shepr's stable endpoint protocol", "join saved SSH endpoint
federation"). Possibly also `StoredMetadata` version fields, `CATALOG_VERSION`,
`SELECTION_VERSION` - on-disk formats, which the hunter calls a judgment call.
See CON-037.

Reported by: remote.

## STR-054 - Vestiges of removed platforms and features

`should_query_host_terminal_theme()` / `should_query_host_cell_size()` return a
constant `true` and are consulted at `terminal_setup.rs:55,69`,
`client/mod.rs:416`; `displayed_workspace_status` (`sidebar.rs:477`) is an
identity function; `workspace_entries` builds `WorkspaceEntry { index }` for
0..n; `CursorState` carries `#[serde(default)]`, meaningless in a positional
codec; `set_mouse_capture`'s else-arm (`terminal_setup.rs:343-346`) is a no-op
`match`.

Reported by: client, ui, protocol.

## STR-055 - Test layouts that mirror accidents

`app/mod.rs` has ~1,300 lines of API handler tests round-tripping JSON strings
(belong beside handlers, against typed results); `headless/tests/mod.rs` ≥3400
lines of whole-server tests (STR-036); `agent_resume.rs` table tests restate
`plan()` (STR-030); `stop_wait_timeout_allows_slow_graceful_shutdown` asserts a
constant equals itself; `nested_message_strings_no_longer_repeat_shepr_prefix`
tests joke strings; pairwise tests `help_advertises_only_commands_the_parser_accepts`,
`default_config_documents_every_keybinding_with_its_default`,
`live_keybinds_matches_the_separate_accessor` mark CON-044/CON-045;
`client/input.rs` `stdin_input_event_carries_raw_bytes` tests enum construction
only; the `saved.rs` "rejects invalid profiles" test becomes moot with typed
`ProfileId` (CON-052).

Reported by: app-state, server, pane-detection, config-cli, client, remote.

## STR-056 - Accept loop polls every 250ms

`CLIENT_ACCEPT_POLL_INTERVAL` polls the listener because it is not wired into
`tokio::select!` - a steady idle wakeup. Proposed: tokio `UnixListener` or
`AsyncFd` on the listener fd.

Reported by: server.

## STR-057 - Per-frame linear lookups in the sidebar

`agent_row` finds agent, workspace, tab and pane by linear string search per row
per frame (O(n²), silent `?` drops); proposed a keyed snapshot by typed id.

Reported by: ui.

## STR-058 - Terminal target lookup scans and allocates

`terminal_targets.rs` finds terminals with
`self.state.terminals.values().find(|t| t.id.to_string() == candidate.terminal_id)`
at lines 56-63, 91-95, 108-112 - O(n) with an allocation per comparison, per
candidate. Keeping `TerminalId` allows `terminals.get(&id)` (STR-001).

Reported by: app-state.
