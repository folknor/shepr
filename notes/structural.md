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

# Moves, splits and rewrites

## STR-024 - Rename src/ghostty to vt and split it

The libghostty shims and infallible `Result`s are gone. Remaining: `mod.rs`
still holds the colour model, palette, cell/style types, `Terminal`, the
`RenderState` snapshot, row/cell views and text readers; proposed `color.rs`,
`cell.rs`, `render.rs`, `read.rs` around `Terminal`, and the planned rename of
`src/ghostty` to `vt`. Several accessors are now `#[cfg(test)]`-only.

Reported by: terminal-core.

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
