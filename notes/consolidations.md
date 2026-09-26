# Consolidations

Decisions answered in more than one place, from the 2026-09-26 design hunt. One
entry per question; every site answering it belongs to that entry.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## CON-004 - Is pixel/cell geometry known, and how is cell size clamped for the protocol?

Sites:
- "Known": `HostCellSize::is_known`, `Terminal::has_pixel_geometry`, `handler::in_band_size_report`, `handler::text_area_pixels_report`, each with its own `w > 0 && h > 0`.
- Clamp to `MAX_CELL_SIZE_PX` and "exact only if both sides ≤ MAX": `client/handshake.rs:109-113`, `client/shell_runtime.rs:64-70`, `client/mod.rs:550-554`. The direct-attach `ClientMessage::Resize` at `client/mod.rs:791-797` does not clamp at all (already disagrees).
- Wire: `TerminalHello`, `Resize`, `ClientShellResize`, `EndpointClientHello` each spell out `cols/rows/cell_width_px/cell_height_px/pixel_mouse` with 0 as the "unavailable" sentinel.

Proposed owner: a `CellPx` that can only be built non-zero, a
`ProtocolCellSize::from_host(...)` constructor in `protocol`, and one wire
`TerminalGeometry { size, cell_px: Option<CellPx>, pixel_mouse }`.

Reported by: terminal-core, client, protocol.

## CON-008 - Colour, appearance and default-colour types exist twice

Sites: `ghostty::ColorScheme::report` and
`terminal_theme::HostAppearance::color_scheme_report` both hard-code
`\x1b[?997;1n` / `\x1b[?997;2n` over two identical Light/Dark enums;
`ghostty::RgbColor` vs `terminal_theme::RgbColor`; `ghostty::DefaultColor` vs
`terminal_theme::DefaultColorKind`; `server/clients.rs:362-393`
`update_host_theme` maps protocol colour/appearance enums inline (a second
mapping if the client maps the same enums).

Proposed owner: the vt module owns one set of types; protocol conversions are
`From` impls next to the protocol types.

Reported by: terminal-core, server.

## CON-025 - Does this event or API method need a render?

Sites:
- `handle_internal_event_with_render_impact` special-cases GitStatus and TabBar; `handle_internal_event_with_pane_updates` re-dispatches the same two variants and discards the answer.
- `api::request_changes_ui` (`src/api/mod.rs:22`), a hand-maintained allowlist separate from dispatch, consulted at `api/server.rs:360` (logging) and `headless.rs:2230` (render). Missing `ClientWindowTitleSet`/`ClientWindowTitleClear`, which headless special-cases to `true` before consulting it. Guarded only by spot-check tests (`app/mod.rs:699-704`, `app/api/panes.rs:2427`).
- `DeferredRender { None, Full }` and `RenderImpact { None, Full }` are identical enums backed by `render_pending: bool`; the run loop keeps `needs_render` / `needs_full_render` as two bools.

Proposed owner: handlers and event dispatch return an outcome carrying render
impact, over one `RenderDemand` lattice (None < Partial < Full) with join.

Reported by: app-state, server.

## CON-040 - Which role is this client process?

Sites: `is_remote_client_process()` called in `run_client_with_mode`,
`run_client_loop`, `handshake_read_timeout`; the same env var read in
`errors.rs` for the reattach message and in `handshake.rs:30-39` for
`ClientShellKeybindingSource` (BUG-021).

Proposed owner: a typed `ClientProcessRole` resolved once at startup.

Reported by: client.

## CON-046 - Does this process own the data dir?

Sites: the lock claimed lazily in `persist::io::load` and (per comments) in
`SessionWriter`; `load_history` does not claim it; global `HELD` list with
idempotent re-claim.

Proposed owner: a `DataDirLease` acquired once at server start, the only way to
get a writable session path.

Reported by: config-cli.

## CON-048 - Where is home?

Sites: `pathutil::home_dir()` (rejects empty `HOME`); `config/io.rs`
`config_dir()` / `state_dir()` / `platform_config_dir()` (do not use it);
`platform::remote_ssh_config_paths()` reads `HOME` directly. See BUG-030.

Reported by: config-cli.

## CON-057 - Workspace and agent row presentation in two sidebars

Sites: `client/shell/sidebar.rs` (local) and `client/shell/endpoint_sidebar.rs`
(multi-endpoint) render collapsed/expanded rows independently; selection
background private in `sidebar.rs:7` and inlined at `endpoint_sidebar.rs:125`;
agent rows in `agent_sidebar.rs` vs `endpoint_agents.rs` (seen only via
references). Already disagree on numbering (BUG-026); stale styling only in the
endpoint path.

Proposed owner (ui hunter, called the biggest payoff in the client shell): local
is the one-endpoint case of the endpoint sidebar; delete `sidebar.rs`'s row
rendering.

Reported by: ui.
