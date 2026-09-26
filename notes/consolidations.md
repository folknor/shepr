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

## CON-040 - Which role is this client process?

Sites: `is_remote_client_process()` called in `run_client_with_mode`,
`run_client_loop`, `handshake_read_timeout`; the same env var read in
`errors.rs` for the reattach message and in `handshake.rs:30-39` for
`ClientShellKeybindingSource` (BUG-021).

Proposed owner: a typed `ClientProcessRole` resolved once at startup.

Reported by: client.
