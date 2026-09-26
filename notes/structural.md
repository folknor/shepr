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

# Moves, splits and rewrites

## STR-024 - Rename src/ghostty to vt and split it

The libghostty shims and infallible `Result`s are gone. Remaining: `mod.rs`
still holds the colour model, palette, cell/style types, `Terminal`, the
`RenderState` snapshot, row/cell views and text readers; proposed `color.rs`,
`cell.rs`, `render.rs`, `read.rs` around `Terminal`, and the planned rename of
`src/ghostty` to `vt`. Several accessors are now `#[cfg(test)]`-only.

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
