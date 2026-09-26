Review of the wire protocol scope: `src/protocol/*` and `src/render_signal.rs`. I read wire.rs up to line 1476 (the rest is tests), plus preamble.rs, endpoint.rs, codec.rs (header and API), surface_delta.rs, surface_delta/decode.rs, surface_reuse.rs, render_ansi.rs (first 220 lines) and render_signal.rs. The project rules forbid shell commands in subagents, so I did not grep call sites in `src/server/` and `src/client/`. Wherever I say how the two sides use the protocol, it comes from what these files show.

## 1. Axes that should be types

- **IDs are all `String`.** `pane_id`, `workspace_id`, `tab_id`, `boot_id`, `terminal_id` and `request_id` travel as `String` in `ClientShellWorkspace/Tab/Pane/Agent`, `PaneSurfacePane`, `ClientShellPaneInput` and `ClientShellEndpointRequest`. `ClientShellAgent` alone holds three sibling ID strings that can be swapped silently. Use newtypes (`WirePaneId`, `WorkspaceId`, `TabId`, `BootId`). `BootId` matters most: every baseline check compares boot strings (see section 2).
- **Revisions are bare `u64`s.** `projection_revision`, `surface_revision` and `base_surface_revision` sit side by side as plain `u64`s in `PaneSurfaceFrame`, `PaneSurfacePatch`, `SurfaceDelta` and `SurfaceReuse`. The decoder compares them by hand. `ProjectionRevision(u64)` and `SurfaceRevision(u64)`, with a `next()` method, would make it impossible to compare a projection revision with a surface revision.
- **`CellData` is untyped on three axes.**
  - `fg`/`bg: u32` use a private tag scheme (0x00 named, 0x01 indexed, 0x02 RGB). An unknown tag decodes silently to `Reset`, so the invalid values are constructible on purpose. It should be a `WireColor` enum; the codec already encodes enums compactly.
  - `modifier: u16` packs ratatui bits plus a 4-bit underline style in bits 12-15. render_ansi.rs hard-codes `REVERSED_MODIFIER = 1 << 6` rather than using `Modifier::REVERSED.bits()`. It should be a struct `{ flags, underline: UnderlineStyle }`.
  - The comment on `fg` says "0xAARRGGBB", which contradicts the actual encoding.
- **Other primitives:**
  - `CursorShapeParam = u8` is a type alias for DECSCUSR 0..=6. It should be an enum.
  - `CursorState` carries `#[serde(default)]`, which means nothing in a positional codec. Drop it.
  - `modifiers: u8` (crossterm bits truncated to u8) appears in `ClientPaneInputEvent::Key/Mouse`, `AttachScroll` and `AttachMouse`. It should be a `WireModifiers` bitflags type.
  - `PaneSurfaceSplit.path: Vec<bool>` should be a typed BSP path (`Vec<Branch>`).
  - `git_ahead_behind: Option<(usize, usize)>` is a bare tuple.
  - `tokens` and `state_labels` are `Vec<(String, String)>`.
  - `ClientHostThemeUpdate::PaletteColors(Vec<(u8, ClientHostColor)>)`.
- **Resize geometry is repeated as loose fields.** `ClientMessage::TerminalHello`, `Resize`, `ClientShellResize` and `EndpointClientHello` each spell out `cols/rows/cell_width_px/cell_height_px/pixel_mouse` by hand, with "0 means unavailable" as a sentinel. Make one `TerminalGeometry { size, cell_px: Option<CellPx>, pixel_mouse }`. Then pixel_mouse without pixel sizes cannot even be constructed.
- **`EndpointControl { kind: String, data: String }` is a stringly typed side channel inside a typed enum.** I found nine or more `kind` constants: endpoint.rs has eight, surface_delta.rs one, surface_reuse.rs one. The payloads are JSON in some cases and base64 of the positional codec in others. The endpoint hello and welcome, the snapshot, health ping/pong, presentation sync/ready, agent completions, surface delta and surface reuse should each be a real `ServerMessage`/`ClientMessage` variant. There are no wire compatibility obligations, so nothing justifies this indirection. It costs:
  - a JSON encode of `ClientShellSnapshot`, which is large;
  - for surface delta, a codec encode, then base64 (+33%), then a codec string;
  - a string compare on every dispatch;
  - "unknown kind" as a runtime condition.
- **Failures are prose.**
  - `check_client_version -> Result<(), String>`.
  - `Welcome.error: Option<String>`.
  - `ClientShellError { message }`, `ServerShutdown.reason: Option<String>`, and `DirectTerminalNotice { message }`, whose doc lists three distinct causes (input dropped, paste rejected, frame oversized).
  - `EndpointHandshakeError.code: String`.
  - `surface_reuse::Decoder::decode -> Result<_, String>`, and every surface_delta function returns `String` errors.
  - `PreambleError` is the good model here: typed, with a `PeerBuild` payload.

  Replace these with enums: `HandshakeRefusal { VersionMismatch{peer}, ... }`, `NoticeKind { InputDropped, PasteRejected{len,max}, FrameOversized }`, `SurfaceDecodeError { NoBaseline, BaselineMismatch{..}, SpanOutOfGrid, NotNegotiated, ... }`. A client could then branch between resync and fatal without matching on strings.

## 2. Decisions made in more than one place

1. **"Does this update continue the baseline?"** Three sites in surface_reuse.rs answer it, with different rules. They already disagree:
   - The reuse check (l.82-88) ignores projection_revision entirely.
   - The patch check (l.122-125) requires `projection_revision == base`.
   - The delta check (l.157-162) requires `base_projection_revision == base` and `projection_revision >= base`.
   - A patch that arrives with no baseline is passed through silently (`if let Some(base)`). Reuse and delta, by contrast, error with "without a baseline".
   - On the sender side, `surface_delta::message` checks boot, width, height and cell count against `last`.

   The owner should be one `Baseline::accepts(kind, base_rev, next_rev, proj, boot)` method, with each allowed transition stated once.
2. **"Does a span fit the grid?"** Four sites answer it:
   - the patch pre-check in `Decoder::decode`;
   - `decode_rows` in surface_delta/decode.rs, which also requires sorted, non-overlapping spans with length > 0;
   - `apply_rows`, a backstop with a different formula;
   - `render_ansi::patch_row_fits` plus `patch_rows_overlap`.

   The rules differ: only the delta decoder and render_ansi reject overlaps, and plain patches accept overlapping or empty spans. `BlitEncoder::commit_patch` bounds-checks only against the flat slice, so a span with x + len > width would wrap into the next row, relying on callers to have checked first. Make a validated `Spans` type that can only be built through one checker, and have every consumer take it.
3. **Surface size limits exist twice, with different values.**
   - wire.rs: `MAX_SURFACE_DIMENSION = 4096`, `MAX_SURFACE_CELLS = MAX_FRAME_SIZE/16 = 131072`.
   - decode.rs: `MAX_GRID_DIMENSION = 4096`, `MAX_GRID_CELLS = 1_000_000`.

   The two cell limits disagree by about 7.6x. They should be one constant, held in wire.rs.
4. **The "delta metadata fits" rule is mirrored by hand.** `decode::metadata_fits` repeats the decoder's limits on the sender side, and the file says so ("Mirrors all decoder-side metadata limits"). It is kept in step by `sender_eligibility_matches_grid_and_metadata_limits`, which is a pairwise-agreement test. That is a finding by your own rule.
5. **The wire layout of `PaneSurfaceFrame`, `FrameData` and `PaneSurfaceSplit` is restated in decode.rs.** It is held in step by struct literals and one layout test. Bounded collection decoding should be a codec feature instead: for example a `BoundedVec<T, const MAX>` whose `Deserialize` checks the count. The ordinary serde path would then be allocation-safe, and the hand decoder could be deleted.
6. **"Which surface encoding is in use / was it negotiated?"**
   - `Decoder::new(surface_delta: bool)` treats delta as negotiated, and the error text says "surface delta was not negotiated".
   - endpoint.rs says "There is no capability or encoding negotiation".
   - `RenderEncoding` in `Welcome` is also a negotiation.

   These contradict each other. Delete the bool, or say where it gets decided.
7. **"Is the peer this build?"** It is decided three times:
   - the preamble (magic, version and build id);
   - `check_client_version` on `TerminalHello.version`;
   - `EndpointClientHello.version` / `EndpointServerWelcome.version`.

   Because the preamble already compares full byte equality, the later checks are pure redundancy, and they carry different error prose. The wire.rs module doc calls them "a second check", which is a defensible re-check. Still, the `version` fields in three message types are dead weight that could drift in wording. I recommend deleting them.
8. **The wire color and modifier layout is decided in several places.** `color_to_u32`, `u32_to_color`, the underline shift and mask, `u16_to_modifier` and render_ansi's own `REVERSED_MODIFIER` each hold a piece of it. It belongs to one `WireStyle` type.
9. **The "is it too big to send?" decision is scattered.**
   - `encode_frame` checks it.
   - `surface_delta::message` checks it three times: `changed_rows` as a lower bound, the delta, and the final message.
   - `surface_reuse::message` checks it.
   - decode.rs checks `data.len() > MAX_FRAME_SIZE` on the base64 string, which is the wrong unit: those are base64 chars, not frame bytes. It is harmless only because the outer frame is already bounded.

## 3. Structure

- **The big rewrite I would make: collapse the three surface update encodings into one.** Today there are:
  - the typed `PaneSurfacePatch` (called "legacy" in tests);
  - `surface_delta`: base64 of codec, tunnelled in `EndpointControl`;
  - `surface_reuse`: JSON tunnelled in `EndpointControl`.

  They carry three baseline rule sets, a hand decoder and a pairwise mirror. Replace them with a single `ServerMessage::SurfaceUpdate { base: SurfaceRevision, next: SurfaceRevision, projection, meta: Option<SurfaceMeta>, spans: Spans }`, where empty spans means reuse and a full grid is still a separate `PaneSurface`. One decoder would own the baseline, one validator the spans, and bounded collections would live in the codec. That removes most of surface_delta.rs, decode.rs and surface_reuse.rs.
- **wire.rs does at least six jobs:**
  - constants and limits;
  - input event types plus the crossterm conversions (`from_crossterm`/`to_crossterm`, `to_raw_input_event`, which depends on `crate::input` and `crate::raw_input`);
  - ClientShell projection types, which depend on `crate::api::schema::AgentStatus`;
  - frame, cell and ratatui conversion (`from_ratatui_buffer_with_hyperlinks`, `to_ratatui_buffer`);
  - framing (`encode_frame`/`read_message`);
  - the version check.

  Split it into `limits`, `input`, `projection`, `surface`/`frame`, `style` and `framing`. The dependency edges point outward: protocol → input/raw_input/api/terminal_theme/ratatui. Protocol types should be leaves, with the conversions living in the client (input) and the server (buffer→frame). `replace_from_ratatui_buffer_preserving_effects` and `to_ratatui_buffer` are presentation logic, not protocol.
- **`render_ansi.rs` is client or terminal-output code, not protocol.** `BlitEncoder` is a stateful ANSI diff renderer. It is used by the server for `TerminalAnsi` clients and probably by the client too. It belongs in a `term_out`/`blit` module that depends on protocol, not inside it.
- **Hyperlinks are passed as `&[((u16,u16), String, String)]` and matched by symbol equality.** The position map is also rebuilt from the frame on every `replace_...`, so it round-trips through strings just to be re-indexed. That is a smell, and it sits on the render hot path: it runs per frame, allocates two HashMaps, and clones every linked symbol and URI.
- **`render_signal.rs` is not protocol.** It is a server render-scheduling primitive keyed by `layout::PaneId`, and belongs in `server/`. Minor points:
  - `request_pty` wakes an immediate source only when it is newly inserted. A pane that was queued while hidden and becomes visible (after `set_immediate_pty_sources`) does not wake until something else does. `has_immediate_work` covers this only if someone polls it.
  - `request_terminal_title` returns true for every new pane, unlike the coalescing done for hidden PTY work.
- **endpoint.rs belongs to the ClientShell projection.** Once the `EndpointControl` kinds become real variants, most of this file disappears.

## Bugs and oddities noticed

- A `PaneSurfacePatch` that arrives before any full surface is accepted and passed through with no error. Every other update kind fails in that case.
- The patch path allows overlapping or zero-length spans; the delta path rejects them.
- `MAX_GRID_CELLS` (1M) versus `MAX_SURFACE_CELLS` (131072) is a real limit mismatch.
- The `CellData.fg` doc comment ("0xAARRGGBB") is wrong.
- decode.rs compares the base64 string length against `MAX_FRAME_SIZE`, which is the wrong unit.
